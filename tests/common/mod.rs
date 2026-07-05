#![allow(dead_code)]

use chrono::Utc;
use fs2::FileExt;
use futures::{SinkExt, StreamExt};
use nostr::nips::nip44;
use nostr::{
    EventBuilder, JsonUtil, Keys, Kind, PublicKey, Tag, Timestamp, ToBech32, UnsignedEvent,
};
use pacto_bot_api::config::{BotConfig, SigningConfig};
use pacto_bot_api::db::Database;
use pacto_bot_api::events::EventType;
use pacto_bot_api::handlers::{ConnectionHandle, HandlerRef};
use pacto_bot_api::transport::protocol::{JsonRpcMessage, serialize_message};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufStream};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::{Duration, timeout};

/// Return a [`tempfile::TempDir`] created under a project-local directory
/// (`target/test-temp`) rather than the system temp directory.
///
/// The project-local root is created with `0o700` permissions so the daemon's
/// config-file permission checks pass even when the host's `/tmp` is
/// group/world-writable.
pub fn tempdir() -> io::Result<tempfile::TempDir> {
    let root = project_temp_root();
    fs::create_dir_all(&root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&root)?.permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&root, permissions)?;
    }
    let dir = tempfile::tempdir_in(root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = dir.path().metadata()?.permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(dir.path(), permissions)?;
    }
    Ok(dir)
}

fn project_temp_root() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    manifest.join("target").join("test-temp")
}

pub fn generate_nsec_bot(id: &str) -> Result<(BotConfig, String), Box<dyn Error>> {
    let keys = nostr::Keys::generate();
    let nsec = keys.secret_key().to_bech32()?;
    let npub = keys.public_key().to_bech32()?;
    let bot = BotConfig {
        id: id.to_string(),
        npub,
        signing: SigningConfig::Nsec {
            nsec: SecretString::new(nsec.clone().into()),
        },
        relays: vec!["wss://127.0.0.1:65535".to_string()],
        capabilities: vec!["ReadMessages".to_string()],
        ..Default::default()
    };
    Ok((bot, nsec))
}

/// Generate a bot config backed by a bunker_local URI.
///
/// `match_npub` controls whether the bunker URI declares the configured bot
/// pubkey (`true`) or a different one (`false`).
pub fn generate_bunker_bot(id: &str, match_npub: bool) -> Result<BotConfig, Box<dyn Error>> {
    let keys = nostr::Keys::generate();
    let npub = keys.public_key().to_bech32()?;
    let remote_keys = if match_npub {
        keys
    } else {
        nostr::Keys::generate()
    };
    let uri = format!(
        "bunker://{}?relay=ws://127.0.0.1:4848",
        remote_keys.public_key().to_hex()
    );
    Ok(BotConfig {
        id: id.to_string(),
        npub,
        signing: SigningConfig::BunkerLocal {
            uri: SecretString::new(uri.into()),
        },
        relays: vec![],
        capabilities: vec![],
        ..Default::default()
    })
}

/// Generate a bot config backed by a bunker_local URI, returning the bunker
/// keys so a test can start a live mock bunker.
///
/// `match_npub` controls whether the configured bot npub matches the bunker
/// keys (`true`) or a different generated key (`false`).
pub fn generate_bunker_bot_with_keys(
    id: &str,
    match_npub: bool,
) -> Result<(BotConfig, nostr::Keys), Box<dyn Error>> {
    let keys = nostr::Keys::generate();
    let npub = keys.public_key().to_bech32()?;
    let remote_keys = if match_npub {
        keys.clone()
    } else {
        nostr::Keys::generate()
    };
    let uri = format!(
        "bunker://{}?relay=ws://127.0.0.1:4848",
        remote_keys.public_key().to_hex()
    );
    let bot = BotConfig {
        id: id.to_string(),
        npub,
        signing: SigningConfig::BunkerLocal {
            uri: SecretString::new(uri.into()),
        },
        relays: vec![],
        capabilities: vec![],
        ..Default::default()
    };
    Ok((bot, remote_keys))
}

/// Replace the bunker URI on a bot config.
pub fn set_bunker_uri(bot: &mut BotConfig, new_uri: &str) {
    match &mut bot.signing {
        SigningConfig::BunkerLocal { uri } | SigningConfig::BunkerRemote { uri } => {
            *uri = SecretString::new(new_uri.into());
        }
        SigningConfig::Nsec { .. } => {}
    }
}

/// Write a `pacto-bot-api.toml` into `dir` and return its path.
pub fn make_config(
    dir: &tempfile::TempDir,
    bots: Vec<BotConfig>,
) -> Result<PathBuf, Box<dyn Error>> {
    let data_dir = dir.path().to_string_lossy();
    let socket_path = dir.path().join("pacto-bot-api.sock");
    let mut content = format!(
        "[daemon]\ndata_dir = {:?}\nsocket_path = {:?}\n\n",
        data_dir, socket_path
    );

    for bot in bots {
        content.push_str("[[bots]]\n");
        content.push_str(&format!("id = {:?}\n", bot.id));
        content.push_str(&format!("npub = {:?}\n", bot.npub));
        match &bot.signing {
            SigningConfig::Nsec { nsec } => {
                content.push_str(&format!(
                    "signing = {{ backend = \"nsec\", nsec = {:?} }}\n",
                    nsec.expose_secret()
                ));
            }
            SigningConfig::BunkerLocal { uri } => {
                content.push_str(&format!(
                    "signing = {{ backend = \"bunker_local\", uri = {:?} }}\n",
                    uri.expose_secret()
                ));
            }
            SigningConfig::BunkerRemote { uri } => {
                content.push_str(&format!(
                    "signing = {{ backend = \"bunker_remote\", uri = {:?} }}\n",
                    uri.expose_secret()
                ));
            }
        }
        if !bot.relays.is_empty() {
            content.push_str(&format!("relays = {:?}\n", bot.relays));
        }
        if !bot.capabilities.is_empty() {
            content.push_str(&format!("capabilities = {:?}\n", bot.capabilities));
        }
        content.push('\n');
    }

    let path = dir.path().join("pacto-bot-api.toml");
    fs::write(&path, content)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&path, perms)?;
    }

    Ok(path)
}

/// Create a daemon lock file in `dir` that the admin CLI will treat as held.
///
/// Writes the current process's PID into the file and acquires an exclusive
/// advisory lock. The returned `File` must be kept alive for the duration of
/// the test so the lock remains held.
pub fn hold_daemon_lock(dir: &tempfile::TempDir) -> Result<std::fs::File, Box<dyn Error>> {
    let path = dir.path().join("daemon.lock");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)?;
    file.lock_exclusive()?;
    writeln!(file, "{}", std::process::id())?;
    file.flush()?;
    Ok(file)
}

/// Create a disconnected handler reference for tests.
pub fn handler_ref(
    id: &str,
    bot_ids: &[&str],
    event_types: &[EventType],
    capabilities: &[&str],
) -> HandlerRef {
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let now = Utc::now();
    HandlerRef {
        id: id.to_string(),
        connection: Some(ConnectionHandle::new(tx)),
        bot_ids: bot_ids.iter().map(|s| s.to_string()).collect(),
        event_types: event_types.to_vec(),
        capabilities: capabilities.iter().map(|s| s.to_string()).collect(),
        reconnect_token: SecretString::new(format!("reconnect-{id}").into()),
        registered_at: now,
        last_seen: now,
        transport: "unknown".to_string(),
    }
}

/// Populate `agent.db` in `dir` with a cursor and handlers for `bot_id`.
pub fn populate_db(
    dir: &tempfile::TempDir,
    bot_id: &str,
    npub: &str,
    cursor: i64,
    handlers: Vec<HandlerRef>,
) -> Result<(), Box<dyn Error>> {
    let db_path = dir.path().join("agent.db");
    let db = Database::open(&db_path)?;
    db.save_cursor(bot_id, npub, cursor)?;
    for handler in handlers {
        db.save_handler(&handler)?;
    }
    Ok(())
}

/// Write an invalid config file with loose permissions for negative tests.
pub fn write_loose_config(path: &Path, content: &str) -> Result<(), Box<dyn Error>> {
    fs::write(path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o644);
        fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// Path to the daemon binary for integration tests.
pub fn daemon_bin_path() -> Result<PathBuf, Box<dyn Error>> {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_pacto-bot-api") {
        return Ok(PathBuf::from(path));
    }
    let cmd = assert_cmd::Command::cargo_bin("pacto-bot-api")?;
    Ok(cmd.get_program().into())
}

/// Spawn the daemon with `config` and wait until its Unix socket appears,
/// which indicates the transport layer is bound and the daemon is ready.
///
/// The caller is responsible for killing the returned child.
pub async fn spawn_daemon_until_ready(config: &Path) -> Result<Child, Box<dyn std::error::Error>> {
    spawn_daemon_until_ready_with_log(config, None).await
}

/// Spawn the daemon, optionally redirecting stderr to `log_path`, and wait
/// until its Unix socket appears.
pub async fn spawn_daemon_until_ready_with_log(
    config: &Path,
    log_path: Option<&Path>,
) -> Result<Child, Box<dyn std::error::Error>> {
    let socket_path = config
        .parent()
        .ok_or("config has no parent directory")?
        .join("pacto-bot-api.sock");

    let (stdout, stderr) = match log_path {
        Some(path) => {
            let file = std::fs::File::create(path)?;
            let stderr = file.try_clone()?;
            (Stdio::from(file), Stdio::from(stderr))
        }
        None => (Stdio::null(), Stdio::null()),
    };

    let mut child = std::process::Command::new(daemon_bin_path()?)
        .arg("--config")
        .arg(config)
        .stdout(stdout)
        .stderr(stderr)
        .env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "debug".into()),
        )
        .spawn()?;

    let start = tokio::time::Instant::now();
    loop {
        if start.elapsed() > Duration::from_secs(15) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("daemon did not become ready".into());
        }

        if socket_path.exists() {
            return Ok(child);
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Send SIGINT to a daemon child and wait for it to exit cleanly.
///
/// This allows coverage data and shutdown hooks to flush, unlike SIGKILL.
pub async fn shutdown_daemon(mut child: Child) -> Result<(), Box<dyn std::error::Error>> {
    let pid = child.id();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGINT,
    )?;

    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = child.wait();
    })
    .await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Dev-env integration gate
// ---------------------------------------------------------------------------

/// True when the `PACTO_DEV_ENV` environment variable is set to `1`.
pub fn dev_env_enabled() -> bool {
    std::env::var("PACTO_DEV_ENV").as_deref() == Ok("1")
}

/// Return `true` when the dev-env gate is enabled, printing a skip message
/// and returning `false` otherwise.
///
/// Use at the start of a gated test to make the body a no-op when the gate
/// is closed:
///
/// ```ignore
/// if !common::skip_unless_dev_env() { return Ok(()); }
/// ```
pub fn skip_unless_dev_env() -> bool {
    if !dev_env_enabled() {
        eprintln!("PACTO_DEV_ENV=1 is required; skipping dev-env test");
        false
    } else {
        true
    }
}

/// Default Nostr relay URL provided by `pacto-dev-env`.
pub const fn dev_relay_url() -> &'static str {
    "ws://localhost:7000"
}

/// Default EVM RPC URL provided by `pacto-dev-env`.
pub const fn dev_evm_url() -> &'static str {
    "http://localhost:8545"
}

// ---------------------------------------------------------------------------
// Handler client for Unix-socket JSON-RPC tests
// ---------------------------------------------------------------------------

/// Connect to `path` and retry until the socket accepts or `deadline` passes.
pub async fn wait_for_socket(
    path: &Path,
    deadline: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let end = tokio::time::Instant::now() + deadline;
    while tokio::time::Instant::now() < end {
        if UnixStream::connect(path).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err("daemon socket did not become available".into())
}

/// A simple handler client connected to the daemon Unix socket.
///
/// Maintains a persistent connection, matches responses by JSON-RPC id, and
/// delivers daemon notifications on a dedicated channel.
pub struct HandlerClient {
    outgoing_tx: mpsc::UnboundedSender<JsonRpcMessage>,
    notification_rx: mpsc::UnboundedReceiver<JsonRpcMessage>,
    pending: Arc<Mutex<HashMap<Value, oneshot::Sender<JsonRpcMessage>>>>,
    handler_id: String,
    reconnect_token: String,
}

impl HandlerClient {
    async fn connect(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        wait_for_socket(path, Duration::from_secs(10)).await?;
        let stream = UnixStream::connect(path).await?;
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();
        let (notification_tx, notification_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();
        let pending: Arc<Mutex<HashMap<Value, oneshot::Sender<JsonRpcMessage>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let pending_for_io = Arc::clone(&pending);
        tokio::spawn(async move {
            let mut stream = BufStream::new(stream);

            async fn read_frame(
                stream: &mut BufStream<UnixStream>,
            ) -> Result<String, std::io::Error> {
                let mut line = String::new();
                stream.read_line(&mut line).await?;
                Ok(line)
            }

            loop {
                tokio::select! {
                    Some(msg) = outgoing_rx.recv() => {
                        let line = match serialize_message(&msg) {
                            Ok(l) => l,
                            Err(_) => continue,
                        };
                        if stream.write_all(line.as_bytes()).await.is_err() {
                            break;
                        }
                        if stream.write_all(b"\n").await.is_err() {
                            break;
                        }
                        if stream.flush().await.is_err() {
                            break;
                        }
                    }
                    result = read_frame(&mut stream) => {
                        let line = match result {
                            Ok(l) => l,
                            Err(_) => break,
                        };
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let msg: JsonRpcMessage = match serde_json::from_str(trimmed) {
                            Ok(m) => m,
                            Err(_) => continue,
                        };

                        if let Some(id) = msg.id().cloned()
                            && let Some(tx) = pending_for_io.lock().await.remove(&id)
                        {
                            let _ = tx.send(msg);
                            continue;
                        }
                        let _ = notification_tx.send(msg);
                    }
                }
            }
        });

        Ok(Self {
            outgoing_tx,
            notification_rx,
            pending,
            handler_id: String::new(),
            reconnect_token: String::new(),
        })
    }

    /// Register a new handler over the Unix socket at `path`.
    pub async fn register(
        path: &Path,
        bot_ids: &[&str],
        event_types: &[&str],
        capabilities: &[&str],
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut client = Self::connect(path).await?;
        let params = serde_json::json!({
            "bot_ids": bot_ids,
            "event_types": event_types,
            "capabilities": capabilities,
        });

        let resp = client
            .call(JsonRpcMessage::request(
                1.into(),
                "handler.register",
                Some(params),
            ))
            .await?;

        let (handler_id, reconnect_token) = match resp {
            JsonRpcMessage::Response { result, .. } => {
                let result = result.ok_or("handler.register response missing result")?;
                let handler_id = result
                    .get("handler_id")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .ok_or("handler.register response missing handler_id")?;
                let reconnect_token = result
                    .get("reconnect_token")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .ok_or("handler.register response missing reconnect_token")?;
                (handler_id, reconnect_token)
            }
            JsonRpcMessage::Error { error, .. } => {
                return Err(format!("handler.register failed: {}", error.message).into());
            }
            _ => return Err("unexpected handler.register response".into()),
        };

        client.handler_id = handler_id;
        client.reconnect_token = reconnect_token;
        Ok(client)
    }

    /// Reconnect a previously registered handler using its secret token.
    pub async fn reconnect(
        path: &Path,
        handler_id: &str,
        reconnect_token: &str,
        bot_ids: &[&str],
        event_types: &[&str],
        capabilities: &[&str],
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut client = Self::connect(path).await?;
        let params = serde_json::json!({
            "handler_id": handler_id,
            "reconnect_token": reconnect_token,
            "bot_ids": bot_ids,
            "event_types": event_types,
            "capabilities": capabilities,
        });

        let resp = client
            .call(JsonRpcMessage::request(
                1.into(),
                "handler.reconnect",
                Some(params),
            ))
            .await?;

        let returned_handler_id = match resp {
            JsonRpcMessage::Response { result, .. } => {
                let result = result.ok_or("handler.reconnect response missing result")?;
                result
                    .get("handler_id")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .ok_or("handler.reconnect response missing handler_id")?
            }
            JsonRpcMessage::Error { error, .. } => {
                return Err(format!("handler.reconnect failed: {}", error.message).into());
            }
            _ => return Err("unexpected handler.reconnect response".into()),
        };

        if returned_handler_id != handler_id {
            return Err(format!(
                "handler.reconnect returned different handler_id: expected {handler_id}, got {returned_handler_id}"
            )
            .into());
        }

        client.handler_id = returned_handler_id;
        client.reconnect_token = reconnect_token.to_string();
        Ok(client)
    }

    /// Send a JSON-RPC request and await its response.
    pub async fn call(
        &self,
        msg: JsonRpcMessage,
    ) -> Result<JsonRpcMessage, Box<dyn std::error::Error>> {
        let id = msg.id().cloned().ok_or("request missing id")?;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        self.outgoing_tx
            .send(msg)
            .map_err(|_| "request channel closed")?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        match timeout(deadline.duration_since(tokio::time::Instant::now()), rx).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(_)) => Err("response channel closed".into()),
            Err(_) => Err("request timed out".into()),
        }
    }

    /// Wait for the next daemon notification.
    pub async fn next_notification(
        &mut self,
        deadline: Duration,
    ) -> Result<JsonRpcMessage, Box<dyn std::error::Error>> {
        match timeout(deadline, self.notification_rx.recv()).await {
            Ok(Some(msg)) => Ok(msg),
            Ok(None) => Err("notification channel closed".into()),
            Err(_) => Err("timed out waiting for notification".into()),
        }
    }

    /// Return the server-assigned handler id.
    pub fn handler_id(&self) -> &str {
        &self.handler_id
    }

    /// Return the secret reconnect token returned at registration.
    pub fn reconnect_token(&self) -> &str {
        &self.reconnect_token
    }

    /// Unregister this handler using the authorized agent.unregister_handler RPC.
    pub async fn unregister(&self) -> Result<(), Box<dyn std::error::Error>> {
        let params = serde_json::json!({
            "handler_id": self.handler_id,
        });
        let resp = self
            .call(JsonRpcMessage::request(
                uuid::Uuid::new_v4().to_string().into(),
                "agent.unregister_handler",
                Some(params),
            ))
            .await?;
        match resp {
            JsonRpcMessage::Response {
                result: Some(result),
                ..
            } => {
                if result.get("unregistered").and_then(Value::as_bool) != Some(true) {
                    return Err(format!("unexpected unregister response: {result}").into());
                }
            }
            JsonRpcMessage::Error { error, .. } => {
                return Err(format!("unregister failed: {} {}", error.code, error.message).into());
            }
            _ => return Err("unexpected unregister response shape".into()),
        }
        Ok(())
    }

    /// Send a `handler.response` notification.
    pub async fn send_response(
        &self,
        event_id: &str,
        action: &str,
        content: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut params = serde_json::json!({
            "event_id": event_id,
            "action": action,
        });
        if let Some(c) = content {
            params["content"] = Value::String(c.into());
        }
        let msg = JsonRpcMessage::notification("handler.response", Some(params));
        self.outgoing_tx
            .send(msg)
            .map_err(|_| "request channel closed")?;
        Ok(())
    }
}
/// Build a kind:1059 gift wrap from `sender` to `recipient_npub`.
pub async fn build_gift_wrap(
    sender: &Keys,
    recipient_npub: &str,
    content: &str,
) -> Result<nostr::Event, Box<dyn std::error::Error>> {
    let recipient = PublicKey::parse(recipient_npub)?;
    let event = EventBuilder::private_msg(sender, recipient, content, Vec::new()).await?;
    Ok(event)
}

/// Build a kind:1059 gift wrap with an explicit `created_at` timestamp.
///
/// The normal [`EventBuilder::private_msg`] randomizes the gift-wrap
/// timestamp for privacy; this helper is useful when a test needs a
/// deterministic timestamp for `since` filter assertions.
pub async fn build_gift_wrap_with_timestamp(
    sender: &Keys,
    recipient_npub: &str,
    content: &str,
    created_at: Timestamp,
) -> Result<nostr::Event, Box<dyn std::error::Error>> {
    let recipient = PublicKey::parse(recipient_npub)?;
    let rumor = EventBuilder::private_msg_rumor(recipient, content).build(sender.public_key());
    let seal = EventBuilder::seal(sender, &recipient, rumor)
        .await?
        .sign(sender)
        .await?;

    let ephemeral = Keys::generate();
    let gift_content = nip44::encrypt(
        ephemeral.secret_key(),
        &recipient,
        seal.as_json(),
        nip44::Version::default(),
    )?;
    let gift = UnsignedEvent::new(
        ephemeral.public_key(),
        created_at,
        Kind::GiftWrap,
        [Tag::public_key(recipient)],
        gift_content,
    );
    Ok(gift.sign_with_keys(&ephemeral)?)
}

// ---------------------------------------------------------------------------
// Real-relay helpers for dev-env integration tests
// ---------------------------------------------------------------------------

/// Connected Nostr relay client used to publish events and wait for replies.
///
/// Implemented directly over `tokio_tungstenite` so dev-env tests have full
/// control over REQ/EVENT timing and can subscribe before a fast reply is
/// published by the daemon.
pub struct DevRelayClient {
    write: tokio::sync::mpsc::UnboundedSender<nostr::Event>,
    notifications: tokio::sync::mpsc::UnboundedReceiver<nostr::Event>,
    sub_id: String,
}

impl DevRelayClient {
    /// Connect to `relay_url`, start a reader/writer loop, and subscribe to
    /// kind:1059 gift wraps addressed to `recipient`.
    pub async fn new(
        relay_url: &str,
        recipient: &PublicKey,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let (ws, _) = tokio_tungstenite::connect_async(relay_url).await?;
        let (mut write_half, mut read_half) = ws.split();

        let sub_id = format!("dev-relay-{}", uuid::Uuid::new_v4());
        let req = serde_json::json!([
            "REQ",
            sub_id,
            {
                "kinds": [1059],
                "#p": [recipient.to_hex()]
            }
        ]);
        let req_line = serde_json::to_string(&req)?;
        write_half
            .send(tokio_tungstenite::tungstenite::Message::Text(
                req_line.into(),
            ))
            .await?;

        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<nostr::Event>();
        let (publish_tx, mut publish_rx) = tokio::sync::mpsc::unbounded_channel::<nostr::Event>();

        let sub_id_clone = sub_id.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(event) = publish_rx.recv() => {
                        let msg = serde_json::json!(["EVENT", event]);
                        let text = serde_json::to_string(&msg).unwrap_or_default();
                        if write_half
                            .send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    msg = read_half.next() => {
                        match msg {
                            Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                                if let Ok(parts) = serde_json::from_str::<Vec<serde_json::Value>>(&text)
                                    && parts.len() >= 3
                                    && parts[0] == "EVENT"
                                    && parts[1].as_str() == Some(&sub_id_clone)
                                    && let Ok(event) =
                                        serde_json::from_value::<nostr::Event>(parts[2].clone())
                                {
                                    let _ = event_tx.send(event);
                                }
                            }
                            Some(Err(_)) => {
                                break;
                            }
                            None => break,
                            _ => {}
                        }
                    }
                }
            }
        });

        Ok(Self {
            write: publish_tx,
            notifications: event_rx,
            sub_id,
        })
    }

    /// Publish `event` to the connected relay.
    pub async fn publish(&self, event: &nostr::Event) -> Result<(), Box<dyn std::error::Error>> {
        self.write
            .send(event.clone())
            .map_err(|_| "relay writer channel closed")?;
        Ok(())
    }

    /// Wait for a kind:1059 gift wrap addressed to `recipient` within
    /// `timeout_duration`.
    pub async fn wait_for_reply(
        &mut self,
        recipient: &PublicKey,
        timeout_duration: Duration,
    ) -> Result<nostr::Event, Box<dyn std::error::Error>> {
        let recipient = *recipient;
        let deadline = tokio::time::Instant::now() + timeout_duration;

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, self.notifications.recv()).await {
                Ok(Some(event))
                    if event.kind == Kind::GiftWrap
                        && event.tags.public_keys().any(|p| p == &recipient) =>
                {
                    return Ok(event);
                }
                Ok(Some(_)) => continue,
                Ok(None) => return Err("relay notification channel closed".into()),
                Err(_) => return Err("timed out waiting for reply gift wrap on relay".into()),
            }
        }
    }
}
