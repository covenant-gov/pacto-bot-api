#![allow(clippy::panic, clippy::expect_used, clippy::unwrap_used)]

mod support;

/// req(R34)
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;
use std::time::Duration;

use assert_cmd::cargo::CommandCargoExt;
use pacto_bot_api::config::{BotConfig, DaemonConfig, GlobalDaemonConfig, SigningConfig};
use pacto_bot_api::errors::DaemonError;
use pacto_bot_api::signer::{BunkerConnection, BunkerKind, LocalKey};
use pacto_bot_api::transport::http::HttpTransport;
use pacto_bot_api::transport::message_handler;
use pacto_bot_api::transport::protocol::{JsonRpcMessage, serialize_message};
use support::secret_scan::{
    SensitiveFixture, assert_no_leak, assert_no_leak_bytes, capture_logs_during, strings_output,
    write_config_file,
};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// Path to a release build of `pacto-bot-api`, built on first use into a
/// separate target directory to avoid locking the default `target` tree while
/// tests run under `cargo test`.
fn release_binary_path() -> Result<&'static Path, &'static String> {
    static PATH: LazyLock<Result<PathBuf, String>> = LazyLock::new(|| {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let target_dir = manifest.join("target/secret-scan-release");
        let binary = target_dir.join("release/pacto-bot-api");

        if !binary.exists() {
            let status = Command::new("cargo")
                .args([
                    "build",
                    "--release",
                    "--bin",
                    "pacto-bot-api",
                    "--target-dir",
                ])
                .arg(&target_dir)
                .status()
                .map_err(|e| format!("failed to spawn cargo build: {e}"))?;
            if !status.success() {
                return Err("release build of pacto-bot-api failed".into());
            }
        }

        Ok(binary)
    });
    PATH.as_ref().map(|p| p.as_path())
}

#[test]
fn capture_logs_during_helper_records_events_without_leaks() {
    let fixture = SensitiveFixture::new();
    let (_, logs) = capture_logs_during(|| {
        tracing::warn!(target: "secret_scan_test", "synthetic warning event");
    });
    assert!(logs.contains("synthetic warning event"));
    assert_no_leak(&logs, &fixture);
}

#[test]
fn startup_logs_warn_about_nsec_without_leaking_marker() {
    let fixture = SensitiveFixture::new();
    let dir = TempDir::new().unwrap();
    let config = format!(
        r#"
[daemon]
data_dir = "{}"

[[bots]]
id = "leak-test-bot"
npub = "npub1leaktest"
signing = {{ backend = "nsec", nsec = "{}" }}
"#,
        dir.path().join("data").to_string_lossy(),
        fixture.nsec_marker
    );
    let config_path = write_config_file(dir.path(), &config).unwrap();

    let output = Command::new(release_binary_path().unwrap())
        .env("RUST_LOG", "debug")
        .arg("--config")
        .arg(&config_path)
        .arg("--data-dir")
        .arg(dir.path().join("data"))
        .output()
        .expect("failed to run daemon binary");

    let logs = String::from_utf8_lossy(&output.stdout);
    assert!(
        logs.contains("local test key (nsec) in use"),
        "expected nsec warning in logs: {logs}"
    );
    assert_no_leak(&logs, &fixture);
}

#[test]
fn bunker_connection_error_does_not_leak_uri_marker() {
    let fixture = SensitiveFixture::new();
    let expected_keys = nostr::Keys::generate();

    // A syntactically valid bunker_remote URI that uses ws:// instead of the
    // required wss://. The URI embeds the synthetic marker so that any echo of
    // the URI in the error would be detected. (Static pubkey checks are gone;
    // live verification happens during daemon startup.)
    let uri = format!(
        "bunker://{}?relay=ws://relay-{}.example.com",
        expected_keys.public_key().to_hex(),
        fixture.bunker_uri_marker
    );

    let err = BunkerConnection::connect(&uri, &expected_keys.public_key(), BunkerKind::Remote)
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("wss"),
        "expected wss requirement error, got: {msg}"
    );
    assert_no_leak(&msg, &fixture);
}

#[tokio::test]
async fn http_401_response_body_does_not_echo_token_marker()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = SensitiveFixture::new();
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().to_path_buf();
    write_token_file(&data_dir, &fixture.http_token_marker).await?;

    let (port, shutdown_tx, _handle) = start_http_server(&data_dir, echo_handler()).await?;

    let response = raw_http_post(port, Some("wrong-token"), "{}").await?;
    assert!(
        response.starts_with("HTTP/1.1 401"),
        "expected 401, got: {response}"
    );
    assert_no_leak(&response, &fixture);

    let _ = shutdown_tx.send(());
    Ok(())
}

#[tokio::test]
async fn json_rpc_error_response_does_not_contain_secret_markers()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = SensitiveFixture::new();
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().to_path_buf();
    let token = "known-test-token";
    write_token_file(&data_dir, token).await?;

    let error_handler = message_handler(|_, _connection, _handler_id| async move {
        Err::<Option<JsonRpcMessage>, DaemonError>(DaemonError::MethodNotFound)
    });

    let (port, shutdown_tx, _handle) = start_http_server(&data_dir, error_handler).await?;

    // Embed every synthetic marker in the request parameters so that a naive
    // echo of the input would be caught, while the legitimate error path should
    // never return them.
    let params = serde_json::json!({
        "nsec": fixture.nsec_marker,
        "bunker_uri": fixture.bunker_uri_marker,
        "http_token": fixture.http_token_marker,
    });
    let body = serialize_message(&JsonRpcMessage::request(
        7.into(),
        "agent.metrics",
        Some(params),
    ))?;
    let response = raw_http_post(port, Some(token), &body).await?;

    assert!(
        response.starts_with("HTTP/1.1 200"),
        "expected 200, got: {response}"
    );
    assert!(
        response.contains("error"),
        "expected JSON-RPC error: {response}"
    );
    assert_no_leak(&response, &fixture);

    let _ = shutdown_tx.send(());
    Ok(())
}

#[test]
fn release_binary_strings_contain_no_synthetic_markers() {
    let fixture = SensitiveFixture::new();
    let binary = release_binary_path().unwrap();
    let Some(strings) = strings_output(binary) else {
        // `strings(1)` is unavailable on this platform; skip.
        return;
    };
    assert_no_leak(&strings, &fixture);
}

#[test]
fn config_parse_error_nsec_backend_does_not_leak_marker() {
    let fixture = SensitiveFixture::new();
    let dir = TempDir::new().unwrap();
    let config = format!(
        r#"
[[bots]]
id = "leak-test-bot"
npub = "{}"
signing = {{ backend = "nsec", nsec = "" }}
"#,
        fixture.nsec_marker
    );
    let path = write_config_file(dir.path(), &config).unwrap();

    let err = DaemonConfig::load(&path).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("non-empty nsec"),
        "expected nsec validation error, got: {msg}"
    );
    assert_no_leak(&msg, &fixture);
}

#[test]
fn simulated_core_dump_after_nsec_load_does_not_leak_marker() {
    let fixture = SensitiveFixture::new();

    // Load the synthetic nsec into the local signer, then drop it. The secret
    // bytes are held in a Zeroizing container and cleared on drop, so no copy
    // of the marker or raw secret bytes should remain in writable memory.
    let signer = LocalKey::parse(&fixture.nsec_marker).unwrap();
    drop(signer);

    let Some(memory) = fixture.scan_memory() else {
        // Core-dump simulation is only implemented on Linux.
        return;
    };
    assert_no_leak_bytes(&memory, &fixture);
}

async fn write_token_file(data_dir: &Path, token: &str) -> std::io::Result<()> {
    let path = data_dir.join("bot_secret_token");
    tokio::fs::write(&path, token).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        tokio::fs::set_permissions(&path, perms).await?;
    }
    Ok(())
}

fn echo_handler() -> pacto_bot_api::transport::MessageHandler {
    message_handler(|msg, _connection, _handler_id| async move {
        let id = msg.id().cloned().unwrap_or(serde_json::Value::Null);
        Ok(Some(JsonRpcMessage::response(
            id,
            Some(serde_json::Value::String("pong".into())),
        )))
    })
}

async fn start_http_server(
    data_dir: &Path,
    handler: pacto_bot_api::transport::MessageHandler,
) -> Result<(u16, oneshot::Sender<()>, tempfile::TempDir), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

    let transport = HttpTransport::new("127.0.0.1:0", data_dir).with_max_frame_size(1024);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (disconnect_tx, _disconnect_rx) = tokio::sync::mpsc::channel::<Option<String>>(1);
    tokio::spawn(async move {
        let _ = transport
            .run_with_listener(listener, handler, disconnect_tx, shutdown_rx)
            .await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    Ok((port, shutdown_tx, dir))
}

async fn raw_http_post(
    port: u16,
    secret: Option<&str>,
    body: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}")).await?;

    let secret_header = secret
        .map(|s| format!("X-Pacto-Bot-Secret: {s}\r\n"))
        .unwrap_or_default();

    let request = format!(
        "POST / HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         {secret_header}\
         \r\n\
         {body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;

    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
        .await
        .map_err(|_| "timed out reading HTTP response")??;
    buf.truncate(n);
    Ok(String::from_utf8_lossy(&buf).to_string())
}

// ---------------------------------------------------------------------------
// MLS group admin secret-redaction tests
// ---------------------------------------------------------------------------

mod common;

use std::sync::Arc;

use nostr::{Keys, Timestamp, ToBech32};
use pacto_bot_api::client_manager::ClientManager;
use pacto_bot_api::db::Db;
use pacto_bot_api::diagnostics::Diagnostics;
use pacto_bot_api::dispatch::Dispatch;
use pacto_bot_api::nostr::NostrClient;
use secrecy::{ExposeSecret, SecretString};
use serde_json::json;
use support::mock_mls_peer::MockMlsPeer;
use support::mock_relay::MockRelay;
use tokio::sync::RwLock;

fn mls_bot_config(id: &str, keys: &Keys, mls_db_path: Option<&str>) -> BotConfig {
    BotConfig {
        id: id.to_string(),
        npub: keys.public_key().to_bech32().unwrap(),
        signing: SigningConfig::Nsec {
            nsec: SecretString::new(keys.secret_key().to_bech32().unwrap().into()),
        },
        relays: vec![],
        capabilities: vec!["Admin".into()],
        mls_dedup_window_secs: None,
        mls_db_path: mls_db_path.map(PathBuf::from),
        mls_key_package_freshness_secs: Some(300),
        ..Default::default()
    }
}

fn write_mls_config(dir: &tempfile::TempDir, bots: &[BotConfig]) -> PathBuf {
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
            _ => panic!("unsupported signing config in test"),
        }
        if !bot.relays.is_empty() {
            content.push_str(&format!("relays = {:?}\n", bot.relays));
        }
        if !bot.capabilities.is_empty() {
            content.push_str(&format!("capabilities = {:?}\n", bot.capabilities));
        }
        if let Some(path) = &bot.mls_db_path {
            content.push_str(&format!("mls_db_path = {:?}\n", path));
        }
        if let Some(secs) = bot.mls_key_package_freshness_secs {
            content.push_str(&format!("mls_key_package_freshness_secs = {}\n", secs));
        }
        content.push('\n');
    }
    let path = dir.path().join("pacto-bot-api.toml");
    std::fs::write(&path, content).expect("write config");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&path, perms).unwrap();
    }
    path
}

async fn setup_mls_dispatch(
    bots: Vec<BotConfig>,
    relay_url: &str,
) -> (Arc<Dispatch>, Arc<RwLock<ClientManager>>) {
    let config = DaemonConfig {
        daemon: GlobalDaemonConfig::default(),
        bots,
    };
    let client = NostrClient::new(vec![relay_url.to_owned()])
        .await
        .expect("nostr client connects to mock relay");
    let dir = common::tempdir().expect("tempdir");
    let db = Db::open(dir.path().join("agent.db").as_path())
        .await
        .expect("db opens");
    let cm = Arc::new(RwLock::new(
        ClientManager::new(dir.path(), config, client, &db)
            .await
            .expect("client manager initializes"),
    ));
    let dispatch = Dispatch::new(cm.clone(), db, Diagnostics::new());
    (Arc::new(dispatch), cm)
}

async fn register_admin_handler(dispatch: &Dispatch, bot_ids: &[&str]) -> String {
    let req = JsonRpcMessage::request(
        1.into(),
        "handler.register",
        Some(json!({
            "bot_ids": bot_ids,
            "event_types": Vec::<String>::new(),
            "capabilities": ["Admin"],
        })),
    );
    let resp = dispatch
        .handle_message(req, None, None)
        .await
        .unwrap()
        .unwrap();
    let JsonRpcMessage::Response { result, .. } = resp else {
        panic!("expected handler.register response");
    };
    let result = result.unwrap();
    result
        .get("handler_id")
        .and_then(serde_json::Value::as_str)
        .unwrap()
        .to_string()
}

fn collect_error_messages(messages: &[JsonRpcMessage]) -> String {
    messages
        .iter()
        .map(|m| match m {
            JsonRpcMessage::Error { error, .. } => format!("{} {}", error.code, error.message),
            _ => format!("{m:?}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// req(R34)
#[tokio::test(flavor = "multi_thread")]
async fn mls_group_json_rpc_errors_redact_synthetic_secrets() {
    let fixture = SensitiveFixture::new();
    let relay = MockRelay::start().await.expect("relay starts");
    let keys = Keys::generate();
    let no_mls_keys = Keys::generate();
    let mls_db_path = format!("{}-mls.db", fixture.http_token_marker);
    let mls_bot = mls_bot_config("mls-bot", &keys, Some(&mls_db_path));
    let no_mls_bot = mls_bot_config("no-mls-bot", &no_mls_keys, None);
    let (dispatch, _cm) = setup_mls_dispatch(vec![mls_bot, no_mls_bot], &relay.url()).await;
    let handler_id = register_admin_handler(&dispatch, &["mls-bot", "no-mls-bot"]).await;

    let mut errors = Vec::new();

    // Recipient with a fresh key package (valid MLS content) so the first
    // create succeeds, allowing the MlsGroupAlreadyExists error later.
    let fresh_recipient = MockMlsPeer::new();
    let fresh_kp = fresh_recipient
        .create_key_package_event(vec![relay.url()])
        .await;
    relay.inject_event(fresh_kp).await;

    // Create a group successfully so we can trigger MlsGroupAlreadyExists.
    let create_req = JsonRpcMessage::request(
        2.into(),
        "admin.create_mls_group",
        Some(json!({
            "bot_id": "mls-bot",
            "group_name": "exists",
            "recipient": fresh_recipient.public_key().to_bech32().unwrap(),
        })),
    );
    let resp = dispatch
        .handle_message(create_req, Some(&handler_id), None)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(resp, JsonRpcMessage::Response { .. }),
        "expected successful create: {resp:?}"
    );

    // MlsGroupAlreadyExists
    let duplicate_req = JsonRpcMessage::request(
        3.into(),
        "admin.create_mls_group",
        Some(json!({
            "bot_id": "mls-bot",
            "group_name": "exists",
            "recipient": fresh_recipient.public_key().to_bech32().unwrap(),
        })),
    );
    let resp = dispatch
        .handle_message(duplicate_req, Some(&handler_id), None)
        .await
        .unwrap()
        .unwrap();
    assert_jsonrpc_error(resp, -32014, &mut errors);

    // MlsGroupNotFound
    let missing_recipient = MockMlsPeer::new();
    let missing_req = JsonRpcMessage::request(
        4.into(),
        "admin.invite_to_mls_group",
        Some(json!({
            "bot_id": "mls-bot",
            "group_name": "missing",
            "recipient": missing_recipient.public_key().to_bech32().unwrap(),
        })),
    );
    let resp = dispatch
        .handle_message(missing_req, Some(&handler_id), None)
        .await
        .unwrap()
        .unwrap();
    assert_jsonrpc_error(resp, -32015, &mut errors);

    // MlsEngineNotConfigured
    let no_mls_recipient = MockMlsPeer::new();
    let no_mls_req = JsonRpcMessage::request(
        5.into(),
        "admin.create_mls_group",
        Some(json!({
            "bot_id": "no-mls-bot",
            "group_name": "any",
            "recipient": no_mls_recipient.public_key().to_bech32().unwrap(),
        })),
    );
    let resp = dispatch
        .handle_message(no_mls_req, Some(&handler_id), None)
        .await
        .unwrap()
        .unwrap();
    assert_jsonrpc_error(resp, -32013, &mut errors);

    // StaleKeyPackage
    let stale_recipient = MockMlsPeer::new();
    let stale_kp = stale_recipient
        .create_key_package_event_with_content(
            vec![relay.url()],
            fixture.bunker_uri_marker.clone(),
            Timestamp::now() - 3600,
        )
        .await;
    relay.inject_event(stale_kp).await;
    let stale_req = JsonRpcMessage::request(
        6.into(),
        "admin.create_mls_group",
        Some(json!({
            "bot_id": "mls-bot",
            "group_name": "stale",
            "recipient": stale_recipient.public_key().to_bech32().unwrap(),
        })),
    );
    let resp = dispatch
        .handle_message(stale_req, Some(&handler_id), None)
        .await
        .unwrap()
        .unwrap();
    assert_jsonrpc_error(resp, -32016, &mut errors);

    let haystack = collect_error_messages(&errors);
    assert_no_leak(&haystack, &fixture);

    relay.stop().await;
}

fn assert_jsonrpc_error(
    resp: JsonRpcMessage,
    expected_code: i32,
    errors: &mut Vec<JsonRpcMessage>,
) {
    let JsonRpcMessage::Error { ref error, .. } = resp else {
        panic!("expected JSON-RPC error, got {resp:?}");
    };
    assert_eq!(
        error.code, expected_code,
        "unexpected error message: {}",
        error.message
    );
    errors.push(resp);
}

/// req(R34)
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn mls_group_admin_cli_errors_redact_synthetic_secrets() {
    let fixture = SensitiveFixture::new();
    let dir = common::tempdir().expect("tempdir");
    let relay = MockRelay::start().await.expect("relay starts");
    let log_path = dir.path().join("daemon.log");

    let (mls_keys, _mls_nsec) = common::generate_nsec_bot("mls-bot").unwrap();
    let (no_mls_keys, _no_mls_nsec) = common::generate_nsec_bot("no-mls-bot").unwrap();

    let mls_db_path = format!("{}-mls.db", fixture.http_token_marker);
    let mut mls_bot = mls_keys;
    mls_bot.relays = vec![relay.url()];
    mls_bot.capabilities = vec!["Admin".into()];
    mls_bot.mls_db_path = Some(PathBuf::from(&mls_db_path));
    mls_bot.mls_key_package_freshness_secs = Some(300);

    let mut no_mls_bot = no_mls_keys;
    no_mls_bot.relays = vec![relay.url()];
    no_mls_bot.capabilities = vec!["Admin".into()];
    // Intentionally no mls_db_path.

    let config = write_mls_config(&dir, &[mls_bot, no_mls_bot]);

    let mut daemon = std::process::Command::new(common::daemon_bin_path().unwrap())
        .arg("--config")
        .arg(&config)
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&log_path).expect("create log"))
        .env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "debug".into()),
        )
        .spawn()
        .expect("spawn daemon");

    common::wait_for_socket(
        &dir.path().join("pacto-bot-api.sock"),
        Duration::from_secs(15),
    )
    .await
    .expect("daemon ready");

    let mut cli_output = String::new();

    // Fresh key package for the bot that has MLS configured.
    let fresh_recipient = MockMlsPeer::new();
    let fresh_npub = fresh_recipient.public_key().to_bech32().unwrap();
    let fresh_kp = fresh_recipient
        .create_key_package_event(vec![relay.url()])
        .await;
    relay.inject_event(fresh_kp).await;

    // Successful create, so we can later trigger MlsGroupAlreadyExists.
    let out = run_mls_admin_cli(
        &config,
        &[
            "create",
            "--bot",
            "mls-bot",
            "--group",
            "exists",
            "--recipient",
            &fresh_npub,
        ],
    )
    .await;
    assert!(out.success, "first create should succeed: {}", out.stderr);
    cli_output.push_str(&out.stdout);
    cli_output.push_str(&out.stderr);

    // MlsGroupAlreadyExists
    let out = run_mls_admin_cli(
        &config,
        &[
            "create",
            "--bot",
            "mls-bot",
            "--group",
            "exists",
            "--recipient",
            &fresh_npub,
        ],
    )
    .await;
    assert!(!out.success, "expected duplicate create to fail");
    cli_output.push_str(&out.stdout);
    cli_output.push_str(&out.stderr);

    // MlsGroupNotFound
    let missing_recipient = MockMlsPeer::new();
    let missing_npub = missing_recipient.public_key().to_bech32().unwrap();
    let missing_kp = missing_recipient
        .create_key_package_event_with_content(
            vec![relay.url()],
            fixture.bunker_uri_marker.clone(),
            Timestamp::now(),
        )
        .await;
    relay.inject_event(missing_kp).await;
    let out = run_mls_admin_cli(
        &config,
        &[
            "invite",
            "--bot",
            "mls-bot",
            "--group",
            "missing",
            "--recipient",
            &missing_npub,
        ],
    )
    .await;
    assert!(!out.success, "expected invite to missing group to fail");
    cli_output.push_str(&out.stdout);
    cli_output.push_str(&out.stderr);

    // MlsEngineNotConfigured
    let no_mls_recipient = MockMlsPeer::new();
    let no_mls_npub = no_mls_recipient.public_key().to_bech32().unwrap();
    let no_mls_kp = no_mls_recipient
        .create_key_package_event_with_content(
            vec![relay.url()],
            fixture.bunker_uri_marker.clone(),
            Timestamp::now(),
        )
        .await;
    relay.inject_event(no_mls_kp).await;
    let out = run_mls_admin_cli(
        &config,
        &[
            "create",
            "--bot",
            "no-mls-bot",
            "--group",
            "any",
            "--recipient",
            &no_mls_npub,
        ],
    )
    .await;
    assert!(!out.success, "expected no-mls create to fail");
    cli_output.push_str(&out.stdout);
    cli_output.push_str(&out.stderr);

    // StaleKeyPackage
    let stale_recipient = MockMlsPeer::new();
    let stale_npub = stale_recipient.public_key().to_bech32().unwrap();
    let stale_kp = stale_recipient
        .create_key_package_event_with_content(
            vec![relay.url()],
            fixture.bunker_uri_marker.clone(),
            Timestamp::now() - 3600,
        )
        .await;
    relay.inject_event(stale_kp).await;
    let out = run_mls_admin_cli(
        &config,
        &[
            "create",
            "--bot",
            "mls-bot",
            "--group",
            "stale",
            "--recipient",
            &stale_npub,
        ],
    )
    .await;
    assert!(!out.success, "expected stale key package create to fail");
    cli_output.push_str(&out.stdout);
    cli_output.push_str(&out.stderr);

    let _ = daemon.kill();
    let _ = daemon.wait();
    relay.stop().await;

    let daemon_logs = std::fs::read_to_string(&log_path).expect("read daemon log");
    assert_no_leak(&cli_output, &fixture);
    assert_no_leak(&daemon_logs, &fixture);
}

struct CliOutput {
    stdout: String,
    stderr: String,
    success: bool,
}

async fn run_mls_admin_cli(config: &std::path::Path, args: &[&str]) -> CliOutput {
    let config = config.to_path_buf();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let output = tokio::task::spawn_blocking(move || {
        let mut cmd = Command::cargo_bin("pacto-bot-admin").expect("cargo bin");
        cmd.arg("--config").arg(config).arg("mls-group");
        for a in args {
            cmd.arg(a);
        }
        cmd.output().expect("run CLI")
    })
    .await
    .expect("blocking task");
    CliOutput {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        success: output.status.success(),
    }
}
