use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use nostr::event::tag::Tag;
use nostr::key::Keys;
use nostr::secp256k1::schnorr::Signature;
use nostr::{Event, Kind, PublicKey, Timestamp, ToBech32, UnsignedEvent};
use nostr_sdk::Client;
use pacto_bot_api::config::{BotConfig, DaemonConfig, SigningConfig};
use pacto_bot_api::diagnostics::{
    BunkerCheck, DaemonStatus, HealthSnapshot, RelayCheck, check_bunker_connectivity,
    check_relay_connectivity,
};
use pacto_bot_api::errors::DaemonError;
use pacto_bot_api::nip46;
use pacto_bot_api::secrecy::ExposeSecret;
use pacto_bot_api::signer::{Signer, SignerBackend};
#[cfg(not(unix))]
use pacto_bot_api::transport::protocol::MetricsResponse;
#[cfg(unix)]
use pacto_bot_api::transport::protocol::{
    JsonRpcMessage, MetricsResponse, parse_message, serialize_message,
};
use rusqlite::Connection;

#[cfg(test)]
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::AsyncReadExt;
#[cfg(not(unix))]
use tokio::io::AsyncWriteExt;
#[cfg(unix)]
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixStream;

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::process;
use std::str::FromStr;
use std::time::Duration;

const DAEMON_LOCK_FILE: &str = "daemon.lock";
const BOT_SECRET_TOKEN_FILE: &str = "bot_secret_token";
const AGENT_DB_FILE: &str = "agent.db";

/// `pacto-bot-admin` command-line interface.
#[derive(Parser, Debug)]
#[command(name = "pacto-bot-admin")]
#[command(about = "Pacto bot admin CLI")]
#[command(version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_COMMIT_SHORT"), ")"))]
struct Cli {
    /// Path to the bot configuration file.
    #[arg(
        short,
        long,
        value_name = "PATH",
        default_value = "pacto-bot-api.toml",
        global = true
    )]
    config: PathBuf,

    /// Directory for runtime data (database, socket, token).
    #[arg(short, long, value_name = "DIR", global = true)]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
#[allow(clippy::enum_variant_names)]
enum Command {
    /// Create a new bot identity config snippet.
    New {
        #[arg(value_name = "BOT_ID")]
        bot_id: String,

        /// Signing backend for the new bot.
        #[arg(short, long, value_name = "BACKEND", default_value = "nsec")]
        backend: String,

        /// Relay URLs for the new bot.
        #[arg(short, long, value_name = "RELAY")]
        relays: Vec<String>,

        /// Capabilities granted to handlers for the new bot.
        #[arg(long, value_name = "CAPABILITY")]
        capabilities: Vec<String>,

        /// Bunker URI (required for bunker backends; omit to prompt).
        #[arg(short, long, value_name = "URI")]
        uri: Option<String>,
    },
    /// Publish a bot profile (kind:0) event.
    PublishProfile {
        #[arg(value_name = "BOT_ID")]
        bot_id: String,
    },
    /// Test a NIP-46 bunker connection and pubkey match.
    TestBunker {
        #[arg(value_name = "BOT_ID")]
        bot_id: String,
    },
    /// Export bot daemon-local state to JSON.
    Export {
        #[arg(value_name = "BOT_ID")]
        bot_id: String,
    },
    /// Import bot daemon-local state from JSON.
    Import {
        #[arg(value_name = "BOT_ID")]
        bot_id: String,

        #[arg(value_name = "STATE_FILE")]
        state_file: String,
    },
    /// Validate the daemon configuration file.
    ValidateConfig,
    /// Rotate the HTTP secret token.
    RotateHttpToken,
    /// Emit structured daemon diagnostics.
    Diagnose {
        #[arg(short, long, value_name = "FORMAT", default_value = "text")]
        format: String,
    },
    /// Show daemon status, connectivity, and registered handlers.
    Status {
        #[arg(short, long, value_name = "FORMAT", default_value = "text")]
        format: String,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("error: {e}");
        process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), DaemonError> {
    match cli.command {
        Command::New {
            bot_id,
            backend,
            relays,
            capabilities,
            uri,
        } => cmd_new(&bot_id, &backend, &relays, &capabilities, uri),
        Command::PublishProfile { bot_id } => cmd_publish_profile(&cli.config, &bot_id).await,
        Command::TestBunker { bot_id } => cmd_test_bunker(&cli.config, &bot_id).await,
        Command::Export { bot_id } => cmd_export(&cli.config, cli.data_dir, &bot_id),
        Command::Import { bot_id, state_file } => {
            cmd_import(&cli.config, cli.data_dir, &bot_id, &state_file)
        }
        Command::ValidateConfig => cmd_validate_config(&cli.config, cli.data_dir),
        Command::RotateHttpToken => cmd_rotate_http_token(&cli.config, cli.data_dir),
        Command::Diagnose { format } => cmd_diagnose(&cli.config, cli.data_dir, &format).await,
        Command::Status { format } => cmd_status(&cli.config, cli.data_dir, &format).await,
    }
}

fn cmd_new(
    bot_id: &str,
    backend: &str,
    relays: &[String],
    capabilities: &[String],
    uri: Option<String>,
) -> Result<(), DaemonError> {
    if bot_id.is_empty() {
        return Err(DaemonError::Config("bot_id must not be empty".into()));
    }

    let keys = Keys::generate();
    let npub = keys
        .public_key()
        .to_bech32()
        .map_err(|e| DaemonError::Nostr(format!("failed to encode npub: {e}")))?;
    let nsec = keys
        .secret_key()
        .to_bech32()
        .map_err(|e| DaemonError::Nostr(format!("failed to encode nsec: {e}")))?;

    let relays_toml = format_toml_array(relays);
    let caps_toml = format_toml_array(capabilities);

    match backend {
        "nsec" => {
            println!("[[bots]]");
            println!("id = {bot_id:?}");
            println!("npub = {npub:?}");
            println!("signing = {{ backend = \"nsec\", nsec = {nsec:?} }}");
            println!("relays = {relays_toml}");
            println!("capabilities = {caps_toml}");
        }
        "bunker_local" | "bunker_remote" => {
            let uri = uri.map_or_else(prompt_uri, Ok)?;
            println!("[[bots]]");
            println!("id = {bot_id:?}");
            println!("npub = {npub:?}");
            println!("signing = {{ backend = {backend:?}, uri = {uri:?} }}");
            println!("relays = {relays_toml}");
            println!("capabilities = {caps_toml}");
        }
        _ => {
            return Err(DaemonError::Config(format!("unknown backend: {backend}")));
        }
    }

    Ok(())
}

async fn cmd_publish_profile(config_path: &Path, bot_id: &str) -> Result<(), DaemonError> {
    let config = DaemonConfig::load(config_path)?;
    let bot = find_bot(&config.bots, bot_id)?;
    let event = build_profile_event(bot).await?;

    let relays: Vec<String> = bot
        .relays
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if relays.is_empty() {
        eprintln!("warning: no relays configured; event signed but not published");
        println!("{}", event.id.to_hex());
        return Ok(());
    }

    let client = Client::default();
    for relay in &relays {
        client
            .add_relay(relay)
            .await
            .map_err(|e| DaemonError::Nostr(format!("failed to add relay {relay}: {e}")))?;
    }
    client.connect().await;

    let output = client
        .send_event(&event)
        .await
        .map_err(|e| DaemonError::Nostr(format!("failed to publish event: {e}")))?;
    println!("{}", output.id().to_hex());

    Ok(())
}

async fn cmd_test_bunker(config_path: &Path, bot_id: &str) -> Result<(), DaemonError> {
    let config = DaemonConfig::load(config_path)?;
    let bot = find_bot(&config.bots, bot_id)?;

    match &bot.signing {
        SigningConfig::Nsec { .. } => Err(DaemonError::Config(
            "test-bunker requires a bunker backend".into(),
        )),
        SigningConfig::BunkerLocal { uri } | SigningConfig::BunkerRemote { uri } => {
            let expected_pubkey = PublicKey::parse(&bot.npub)
                .map_err(|e| DaemonError::Config(format!("invalid npub for bot: {e}")))?;
            let uri = uri.expose_secret();
            nip46::verify_bunker_public_key(uri, &expected_pubkey, Duration::from_secs(30)).await?;
            println!("bunker public key matches npub for {bot_id}");
            Ok(())
        }
    }
}

fn cmd_export(
    config_path: &Path,
    data_dir_override: Option<PathBuf>,
    bot_id: &str,
) -> Result<(), DaemonError> {
    let config = DaemonConfig::load(config_path)?;
    let data_dir = resolve_data_dir(&config, data_dir_override);
    check_no_daemon_lock(&data_dir)?;

    let db_path = data_dir.join(AGENT_DB_FILE);
    let conn = open_agent_db(&db_path)?;

    let mut cursors = Vec::new();
    if let Some(cursor) = load_bot_cursor(&conn, bot_id)? {
        cursors.push(cursor);
    }

    let handlers = load_bot_handlers(&conn, bot_id)?;

    let state = ExportState {
        metadata: ExportMetadata {
            daemon_version: pacto_bot_api::version::VERSION.to_string(),
            exported_at: Utc::now().to_rfc3339(),
            source_data_dir: data_dir.to_string_lossy().to_string(),
        },
        cursors,
        handlers,
        split_brain_warning: true,
    };

    println!("{}", serde_json::to_string_pretty(&state)?);
    Ok(())
}

fn cmd_import(
    config_path: &Path,
    data_dir_override: Option<PathBuf>,
    bot_id: &str,
    state_file: &str,
) -> Result<(), DaemonError> {
    let config = DaemonConfig::load(config_path)?;
    let _bot = find_bot(&config.bots, bot_id)?;
    let data_dir = resolve_data_dir(&config, data_dir_override);
    check_no_daemon_lock(&data_dir)?;

    let state_json = fs::read_to_string(state_file).map_err(DaemonError::Io)?;
    let state: ExportState = serde_json::from_str(&state_json)?;

    let db_path = data_dir.join(AGENT_DB_FILE);
    let conn = open_agent_db(&db_path)?;

    for cursor in &state.cursors {
        if cursor.bot_id == bot_id {
            save_bot_cursor(&conn, cursor)?;
        }
    }

    for handler in &state.handlers {
        if handler.bot_ids.contains(&bot_id.to_string()) {
            save_handler_export(&conn, handler)?;
        }
    }

    println!("imported state for {bot_id}");
    Ok(())
}

fn cmd_validate_config(
    config_path: &Path,
    data_dir_override: Option<PathBuf>,
) -> Result<(), DaemonError> {
    let mut errors = Vec::new();

    let config = match DaemonConfig::load(config_path) {
        Ok(c) => c,
        Err(e) => {
            errors.push(e.to_string());
            print_validate_report(&errors);
            return Err(DaemonError::Config("config validation failed".into()));
        }
    };

    let data_dir = resolve_data_dir(&config, data_dir_override);
    let db_path = data_dir.join(AGENT_DB_FILE);
    if db_path.exists() {
        match open_agent_db(&db_path) {
            Ok(conn) => {
                for bot in &config.bots {
                    match load_bot_cursor(&conn, &bot.id) {
                        Ok(Some(cursor)) => {
                            if cursor.npub != bot.npub {
                                errors.push(format!(
                                    "bot {}: DB npub {} does not match config npub {}",
                                    bot.id, cursor.npub, bot.npub
                                ));
                            }
                        }
                        Ok(None) => {}
                        Err(e) => errors.push(format!("bot {}: DB cursor error: {e}", bot.id)),
                    }
                }
            }
            Err(e) => errors.push(format!("failed to open DB at {}: {e}", db_path.display())),
        }
    }

    print_validate_report(&errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(DaemonError::Config("config validation failed".into()))
    }
}

fn cmd_rotate_http_token(
    config_path: &Path,
    data_dir_override: Option<PathBuf>,
) -> Result<(), DaemonError> {
    let config = DaemonConfig::load(config_path)?;
    let data_dir = resolve_data_dir(&config, data_dir_override);
    check_no_daemon_lock(&data_dir)?;
    ensure_data_dir(&data_dir)?;

    let token = generate_hex_token()?;
    write_token_atomic(&data_dir, &token)?;

    println!(
        "rotated HTTP token at {}",
        data_dir.join(BOT_SECRET_TOKEN_FILE).display()
    );
    Ok(())
}

async fn cmd_diagnose(
    config_path: &Path,
    data_dir_override: Option<PathBuf>,
    format: &str,
) -> Result<(), DaemonError> {
    let (config_valid, config, config_error) = match DaemonConfig::load(config_path) {
        Ok(c) => (true, Some(c), None),
        Err(e) => (false, None, Some(e.to_string())),
    };

    let data_dir = config
        .as_ref()
        .map(|c| resolve_data_dir(c, data_dir_override.clone()))
        .or_else(|| data_dir_override.as_deref().map(expand_path_buf));

    let socket_path: Option<PathBuf> = config
        .as_ref()
        .map(|c| PathBuf::from(c.socket_path()))
        .or_else(|| data_dir.as_ref().map(|d| d.join("pacto-bot-api.sock")));

    let mut errors = Vec::new();
    if let Some(err) = config_error {
        errors.push(err);
    }

    let lock_held = data_dir
        .as_ref()
        .map(|p| is_daemon_lock_held(p))
        .unwrap_or(false);

    let socket = socket_path
        .as_deref()
        .map(inspect_socket)
        .unwrap_or_default();

    let live_snapshot = match (&socket_path, &data_dir) {
        (Some(socket), Some(dir)) => probe_live_metrics(socket, dir).await,
        _ => None,
    };

    let bots: Vec<BotDiagnosis> = config
        .as_ref()
        .map(|c| {
            c.bots
                .iter()
                .map(|b| {
                    let live_bunker_connected = live_snapshot.as_ref().and_then(|s| {
                        s.bots
                            .iter()
                            .find(|bh| bh.bot_id == b.id)
                            .map(|bh| bh.bunker_connected)
                    });
                    BotDiagnosis {
                        id: b.id.clone(),
                        npub: b.npub.clone(),
                        signing_backend: signing_backend_label(&b.signing),
                        relay_count: b.relays.len(),
                        live_bunker_connected,
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    let mut relay_connectivity = Vec::new();
    let mut bunker_connectivity = Vec::new();
    if let Some(ref cfg) = config {
        for bot in &cfg.bots {
            relay_connectivity.extend(check_relay_connectivity(bot).await);
            if let Some(check) = check_bunker_connectivity(bot).await {
                bunker_connectivity.push(check);
            }
        }
    }

    let service_versions = if let Some(ref cfg) = config {
        probe_service_versions(&cfg.bots).await
    } else {
        ServiceVersions::default()
    };

    let db_cursor_count = if let Some(ref dir) = data_dir {
        let db_path = dir.join(AGENT_DB_FILE);
        if db_path.exists() {
            match open_agent_db(&db_path) {
                Ok(conn) => count_cursors(&conn).unwrap_or_else(|e| {
                    errors.push(format!("db error: {e}"));
                    0
                }),
                Err(e) => {
                    errors.push(format!("failed to open db: {e}"));
                    0
                }
            }
        } else {
            0
        }
    } else {
        0
    };

    let daemon_status = live_snapshot.as_ref().map(|s| daemon_status_str(s.status));

    let report = DiagnoseReport {
        config_valid,
        lock_held,
        daemon_status,
        data_dir: data_dir
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default(),
        socket,
        bots,
        relay_connectivity,
        bunker_connectivity,
        service_versions,
        db_cursor_count,
        errors,
    };

    match format {
        "json" => println!("{}", serde_json::to_string_pretty(&report)?),
        _ => print_diagnose_text(&report)?,
    }

    Ok(())
}

async fn cmd_status(
    config_path: &Path,
    data_dir_override: Option<PathBuf>,
    format: &str,
) -> Result<(), DaemonError> {
    let config = match DaemonConfig::load(config_path) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("warning: failed to load config: {e}");
            None
        }
    };

    let data_dir = config
        .as_ref()
        .map(|c| resolve_data_dir(c, data_dir_override.clone()))
        .or_else(|| data_dir_override.as_deref().map(expand_path_buf));

    let socket_path: Option<PathBuf> = config
        .as_ref()
        .map(|c| PathBuf::from(c.socket_path()))
        .or_else(|| data_dir.as_ref().map(|d| d.join("pacto-bot-api.sock")));

    let (daemon_running, live_metrics, live_snapshot) = match (&socket_path, &data_dir) {
        #[cfg(unix)]
        (Some(socket), Some(dir)) => match call_agent_metrics(socket).await {
            Ok(metrics) => (true, Some(metrics), read_latest_report(dir)),
            Err(_) => (false, None, read_latest_report(dir)),
        },
        #[cfg(not(unix))]
        (_, Some(dir)) => (false, None::<MetricsResponse>, read_latest_report(dir)),
        _ => (false, None, None),
    };

    let daemon_status = live_snapshot.as_ref().map(|s| daemon_status_str(s.status));
    let uptime_seconds = live_metrics
        .as_ref()
        .and_then(|m| m.uptime_seconds)
        .unwrap_or(0);
    let handlers_registered = live_metrics
        .as_ref()
        .and_then(|m| m.handlers_registered)
        .unwrap_or(0);

    let mut bot_statuses = Vec::new();
    if let Some(cfg) = &config {
        for bot in &cfg.bots {
            let relays = check_relay_connectivity(bot).await;
            let bunker = check_bunker_connectivity(bot).await;
            bot_statuses.push(BotStatus {
                id: bot.id.clone(),
                npub: bot.npub.clone(),
                relays,
                bunker,
            });
        }
    }

    let report = StatusReport {
        daemon_running,
        daemon_status,
        uptime_seconds,
        handlers_registered,
        bots: bot_statuses,
    };

    match format {
        "json" => println!("{}", serde_json::to_string_pretty(&report)?),
        _ => print_status_text(&report)?,
    }

    Ok(())
}

fn read_latest_report(data_dir: &Path) -> Option<HealthSnapshot> {
    let path = data_dir.join("reports").join("latest.json");
    if let Ok(contents) = std::fs::read_to_string(&path) {
        if let Ok(snapshot) = serde_json::from_str::<HealthSnapshot>(&contents) {
            return Some(snapshot);
        }
    }
    None
}

fn find_bot<'a>(bots: &'a [BotConfig], bot_id: &str) -> Result<&'a BotConfig, DaemonError> {
    bots.iter()
        .find(|b| b.id == bot_id)
        .ok_or_else(|| DaemonError::UnknownBot(bot_id.to_string()))
}

fn inspect_socket(path: &Path) -> SocketHealth {
    let exists = path.exists();
    #[cfg(unix)]
    let mut mode = None;
    #[cfg(not(unix))]
    let mode: Option<u32> = None;
    let mut owner_readable = false;
    let mut owner_writable = false;
    if exists {
        if let Ok(meta) = std::fs::metadata(path) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let m = meta.permissions().mode();
                mode = Some(m & 0o777);
                owner_readable = m & 0o400 != 0;
                owner_writable = m & 0o200 != 0;
            }
            #[cfg(not(unix))]
            {
                owner_readable = true;
                owner_writable = !meta.permissions().readonly();
            }
        }
    }
    SocketHealth {
        path: path.to_string_lossy().to_string(),
        exists,
        mode,
        owner_readable,
        owner_writable,
    }
}

async fn probe_live_metrics(socket_path: &Path, data_dir: &Path) -> Option<HealthSnapshot> {
    #[cfg(unix)]
    // A successful `agent.metrics` response proves the daemon is running, but
    // the response shape only contains counters. The full snapshot (status,
    // per-bot health, recent errors) lives in the flushed report file.
    let _ = call_agent_metrics(socket_path).await.ok();
    #[cfg(not(unix))]
    let _ = socket_path;
    read_latest_report(data_dir)
}

#[cfg(unix)]
async fn call_agent_metrics(socket_path: &Path) -> Result<MetricsResponse, DaemonError> {
    let stream = tokio::time::timeout(Duration::from_secs(2), UnixStream::connect(socket_path))
        .await
        .map_err(|_| DaemonError::Config("unix socket connect timed out".into()))??;
    let (reader, mut writer) = stream.into_split();
    let request = JsonRpcMessage::request(1.into(), "agent.metrics", None);
    let line = format!("{}\n", serialize_message(&request)?);
    writer.write_all(line.as_bytes()).await?;

    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    let n = tokio::time::timeout(Duration::from_secs(2), reader.read_until(b'\n', &mut buf))
        .await
        .map_err(|_| DaemonError::Config("unix socket read timed out".into()))??;
    if n == 0 {
        return Err(DaemonError::Config("unix socket closed".into()));
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    let line = String::from_utf8(buf)
        .map_err(|_| DaemonError::Config("metrics response is not valid UTF-8".into()))?;

    match parse_message(&line)? {
        JsonRpcMessage::Response {
            result: Some(value),
            ..
        } => {
            let metrics: MetricsResponse = serde_json::from_value(value)?;
            Ok(metrics)
        }
        JsonRpcMessage::Response { result: None, .. } => {
            Err(DaemonError::Config("empty metrics result".into()))
        }
        JsonRpcMessage::Error { error, .. } => Err(DaemonError::Config(format!(
            "metrics error: {}",
            error.message
        ))),
        _ => Err(DaemonError::Config("unexpected metrics response".into())),
    }
}

fn daemon_status_str(status: DaemonStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| format!("{status:?}").to_lowercase())
}

fn is_pacto_dev_env() -> bool {
    env::var("PACTO_DEV_ENV").map(|v| v == "1").unwrap_or(false)
}

async fn probe_service_versions(bots: &[BotConfig]) -> ServiceVersions {
    if !is_pacto_dev_env() {
        return ServiceVersions::default();
    }
    let relay = Some(probe_http_service("http://localhost:7000", "/", 2).await);
    let evm_node = Some(probe_evm_node().await);
    let bunker_port = match find_bunker_port(bots) {
        Some(port) => Some(probe_tcp_service(&format!("127.0.0.1:{port}")).await),
        None => None,
    };
    ServiceVersions {
        relay,
        evm_node,
        bunker_port,
    }
}

async fn probe_http_service(base_url: &str, path: &str, timeout_secs: u64) -> ServiceInfo {
    let url = format!("{}{}", base_url.trim_end_matches('/'), path);
    match tokio::time::timeout(Duration::from_secs(timeout_secs), raw_http_get(&url)).await {
        Ok(Ok((status, body))) => {
            let reachable = status == 200;
            let version = if reachable {
                extract_version(&body)
            } else {
                None
            };
            ServiceInfo {
                url: url.clone(),
                reachable,
                version,
                error: if reachable {
                    None
                } else {
                    Some(format!("HTTP {status}"))
                },
            }
        }
        Ok(Err(e)) => ServiceInfo {
            url: url.clone(),
            reachable: false,
            version: None,
            error: Some(e.to_string()),
        },
        Err(_) => ServiceInfo {
            url: url.clone(),
            reachable: false,
            version: None,
            error: Some("request timed out".to_string()),
        },
    }
}

async fn raw_http_get(url: &str) -> Result<(u16, String), DaemonError> {
    let (host, port, path) = parse_http_url(url)?;
    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&addr).await?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(DaemonError::Io)?;
    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .await
        .map_err(DaemonError::Io)?;
    parse_http_response(&buf)
}

async fn raw_http_post(url: &str, body: &str) -> Result<(u16, String), DaemonError> {
    let (host, port, path) = parse_http_url(url)?;
    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&addr).await?;
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(DaemonError::Io)?;
    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .await
        .map_err(DaemonError::Io)?;
    parse_http_response(&buf)
}

fn parse_http_url(url: &str) -> Result<(String, u16, String), DaemonError> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(|| DaemonError::Config(format!("not an http url: {url}")))?;
    let (host_port, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    let (host, port) = match host_port.rfind(':') {
        Some(idx) => {
            let host = &host_port[..idx];
            let port: u16 = host_port[idx + 1..]
                .parse()
                .map_err(|_| DaemonError::Config("invalid port".into()))?;
            (host, port)
        }
        None => (host_port, 80),
    };
    Ok((host.to_string(), port, path.to_string()))
}

fn parse_http_response(buf: &[u8]) -> Result<(u16, String), DaemonError> {
    let text = String::from_utf8_lossy(buf);
    let status_line = text
        .lines()
        .next()
        .ok_or_else(|| DaemonError::Config("empty http response".into()))?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| DaemonError::Config("invalid http status line".into()))?;
    let status: u16 = status
        .parse()
        .map_err(|_| DaemonError::Config("invalid http status code".into()))?;
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    Ok((status, body))
}

fn extract_version(body: &str) -> Option<String> {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(v) = value.get("version").and_then(|v| v.as_str()) {
            return Some(v.to_string());
        }
        if let Some(v) = value.get("name").and_then(|v| v.as_str()) {
            return Some(v.to_string());
        }
    }
    body.lines().next().map(|s| s.to_string())
}

async fn probe_evm_node() -> ServiceInfo {
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "net_version",
        "params": []
    })
    .to_string();
    let url = "http://localhost:8545";
    match tokio::time::timeout(Duration::from_secs(2), raw_http_post(url, &payload)).await {
        Ok(Ok((status, body))) => {
            let reachable = status == 200;
            let version = if reachable {
                serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| {
                        v.get("result")
                            .and_then(|r| r.as_str())
                            .map(|s| s.to_string())
                    })
            } else {
                None
            };
            ServiceInfo {
                url: url.to_string(),
                reachable,
                version,
                error: if reachable {
                    None
                } else {
                    Some(format!("HTTP {status}"))
                },
            }
        }
        Ok(Err(e)) => ServiceInfo {
            url: url.to_string(),
            reachable: false,
            version: None,
            error: Some(e.to_string()),
        },
        Err(_) => ServiceInfo {
            url: url.to_string(),
            reachable: false,
            version: None,
            error: Some("request timed out".to_string()),
        },
    }
}

async fn probe_tcp_service(addr: &str) -> ServiceInfo {
    let result = tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(addr)).await;
    let reachable = matches!(result, Ok(Ok(_)));
    ServiceInfo {
        url: format!("tcp://{addr}"),
        reachable,
        version: None,
        error: if reachable {
            None
        } else {
            Some("connection refused or timed out".to_string())
        },
    }
}

fn find_bunker_port(bots: &[BotConfig]) -> Option<u16> {
    for bot in bots {
        if let SigningConfig::BunkerLocal { uri } | SigningConfig::BunkerRemote { uri } =
            &bot.signing
        {
            if let Some(port) = extract_port_from_url(uri.expose_secret()) {
                return Some(port);
            }
        }
    }
    None
}

fn extract_port_from_url(url: &str) -> Option<u16> {
    let trimmed = url
        .strip_prefix("ws://")
        .or_else(|| url.strip_prefix("wss://"))?;
    let host_port = trimmed.split('/').next()?;
    let parts: Vec<&str> = host_port.split(':').collect();
    if parts.len() == 2 {
        parts[1].parse().ok()
    } else {
        None
    }
}

fn resolve_data_dir(config: &DaemonConfig, override_path: Option<PathBuf>) -> PathBuf {
    override_path
        .as_deref()
        .map(expand_path_buf)
        .unwrap_or_else(|| PathBuf::from(config.data_dir()))
}

fn expand_path_buf(path: &Path) -> PathBuf {
    expand_path(&path.to_string_lossy())
}

fn expand_path(input: &str) -> PathBuf {
    if let Some(rest) = input.strip_prefix("~/") {
        if let Ok(home) = env::var("HOME") {
            return PathBuf::from(format!("{home}/{rest}"));
        }
    }
    PathBuf::from(input)
}

fn is_daemon_lock_held(data_dir: &Path) -> bool {
    data_dir.join(DAEMON_LOCK_FILE).exists()
}

fn check_no_daemon_lock(data_dir: &Path) -> Result<(), DaemonError> {
    if is_daemon_lock_held(data_dir) {
        return Err(DaemonError::Config(format!(
            "daemon lock is held at {}",
            data_dir.join(DAEMON_LOCK_FILE).display()
        )));
    }
    Ok(())
}

fn ensure_data_dir(data_dir: &Path) -> Result<(), DaemonError> {
    fs::create_dir_all(data_dir).map_err(DaemonError::Io)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = fs::metadata(data_dir).map_err(DaemonError::Io)?;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            let mut perms = metadata.permissions();
            perms.set_mode(0o700);
            fs::set_permissions(data_dir, perms).map_err(DaemonError::Io)?;
        }
    }

    Ok(())
}

fn generate_hex_token() -> Result<String, DaemonError> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| DaemonError::Io(std::io::Error::other(e)))?;
    Ok(hex::encode(bytes))
}

fn write_token_atomic(dir: &Path, token: &str) -> Result<(), DaemonError> {
    let tmp = dir.join(format!("{}.tmp", BOT_SECRET_TOKEN_FILE));
    let dest = dir.join(BOT_SECRET_TOKEN_FILE);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(DaemonError::Io)?;
        file.write_all(token.as_bytes()).map_err(DaemonError::Io)?;
        drop(file);
    }

    #[cfg(not(unix))]
    {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .map_err(DaemonError::Io)?;
        file.write_all(token.as_bytes()).map_err(DaemonError::Io)?;
        drop(file);
    }

    fs::rename(&tmp, &dest).map_err(DaemonError::Io)?;
    Ok(())
}

fn prompt_uri() -> Result<String, DaemonError> {
    print!("Enter bunker URI: ");
    io::stdout().flush().map_err(DaemonError::Io)?;
    let mut buf = String::new();
    io::stdin().read_line(&mut buf).map_err(DaemonError::Io)?;
    let uri = buf.trim().to_string();
    if uri.is_empty() {
        return Err(DaemonError::Config("bunker URI is required".into()));
    }
    Ok(uri)
}

fn format_toml_array(items: &[String]) -> String {
    let parts: Vec<String> = items.iter().map(|s| format!("{s:?}")).collect();
    format!("[{}]", parts.join(", "))
}

fn signing_backend_label(signing: &SigningConfig) -> String {
    match signing {
        SigningConfig::Nsec { .. } => "nsec".to_string(),
        SigningConfig::BunkerLocal { .. } => "bunker_local".to_string(),
        SigningConfig::BunkerRemote { .. } => "bunker_remote".to_string(),
    }
}

async fn build_profile_event(bot: &BotConfig) -> Result<Event, DaemonError> {
    let signer = SignerBackend::from_config(&bot.signing, &bot.npub)?;
    build_profile_event_with_signer(bot, &signer).await
}

async fn build_profile_event_with_signer(
    bot: &BotConfig,
    signer: &dyn Signer,
) -> Result<Event, DaemonError> {
    let content = serde_json::to_string(&json!({
        "name": bot.id,
        "bot": true,
        "capabilities": bot.capabilities,
    }))?;

    let pubkey = signer.public_key();
    let created_at = Timestamp::now();
    let kind = Kind::Metadata;
    let tags: Vec<Tag> = Vec::new();

    let mut unsigned = UnsignedEvent::new(pubkey, created_at, kind, tags.clone(), content.clone());
    unsigned.ensure_id();
    let event_id = unsigned
        .id
        .ok_or_else(|| DaemonError::Nostr("failed to compute event id".into()))?;

    let payload = event_signing_bytes(&unsigned)?;
    let sig_hex = signer.sign_event(&payload).await?;
    let signature = Signature::from_str(&sig_hex)
        .map_err(|e| DaemonError::Nostr(format!("invalid signature: {e}")))?;

    Ok(Event::new(
        event_id, pubkey, created_at, kind, tags, content, signature,
    ))
}

fn event_signing_bytes(unsigned: &UnsignedEvent) -> Result<Vec<u8>, DaemonError> {
    serde_json::to_vec(&json!([
        0,
        unsigned.pubkey,
        unsigned.created_at,
        unsigned.kind,
        unsigned.tags,
        unsigned.content
    ]))
    .map_err(DaemonError::Json)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExportState {
    metadata: ExportMetadata,
    cursors: Vec<CursorExport>,
    handlers: Vec<HandlerExport>,
    split_brain_warning: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExportMetadata {
    daemon_version: String,
    exported_at: String,
    source_data_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CursorExport {
    bot_id: String,
    npub: String,
    cursor: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HandlerExport {
    handler_id: String,
    bot_ids: Vec<String>,
    event_types: Vec<String>,
    capabilities: Vec<String>,
    registered_at: String,
}

#[derive(Debug, Clone, Serialize)]
struct DiagnoseReport {
    config_valid: bool,
    lock_held: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    daemon_status: Option<String>,
    data_dir: String,
    socket: SocketHealth,
    bots: Vec<BotDiagnosis>,
    relay_connectivity: Vec<RelayCheck>,
    bunker_connectivity: Vec<BunkerCheck>,
    service_versions: ServiceVersions,
    db_cursor_count: i64,
    errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct StatusReport {
    daemon_running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    daemon_status: Option<String>,
    uptime_seconds: u64,
    handlers_registered: u64,
    bots: Vec<BotStatus>,
}

#[derive(Debug, Clone, Serialize)]
struct BotStatus {
    id: String,
    npub: String,
    relays: Vec<RelayCheck>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bunker: Option<BunkerCheck>,
}

#[derive(Debug, Clone, Serialize)]
struct BotDiagnosis {
    id: String,
    npub: String,
    signing_backend: String,
    relay_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    live_bunker_connected: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Default)]
struct SocketHealth {
    path: String,
    exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<u32>,
    owner_readable: bool,
    owner_writable: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
struct ServiceVersions {
    #[serde(skip_serializing_if = "Option::is_none")]
    relay: Option<ServiceInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    evm_node: Option<ServiceInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bunker_port: Option<ServiceInfo>,
}

#[derive(Debug, Clone, Serialize)]
struct ServiceInfo {
    url: String,
    reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn open_agent_db(path: &Path) -> Result<Connection, DaemonError> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;",
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS cursors (
            bot_id TEXT PRIMARY KEY,
            npub TEXT NOT NULL,
            last_event_id TEXT,
            updated_at INTEGER
        );
        CREATE TABLE IF NOT EXISTS handlers (
            handler_id TEXT PRIMARY KEY,
            bot_ids TEXT NOT NULL,
            event_types TEXT NOT NULL,
            capabilities TEXT NOT NULL,
            registered_at INTEGER
        );",
    )?;
    Ok(conn)
}

fn load_bot_cursor(conn: &Connection, bot_id: &str) -> Result<Option<CursorExport>, DaemonError> {
    let mut stmt = conn.prepare("SELECT npub, last_event_id FROM cursors WHERE bot_id = ?1")?;
    let mut rows = stmt.query([bot_id])?;

    if let Some(row) = rows.next()? {
        let npub: String = row.get(0)?;
        let last: Option<String> = row.get(1)?;
        let cursor = last
            .as_ref()
            .map(|s| s.parse::<i64>())
            .transpose()
            .map_err(|e| DaemonError::Config(format!("invalid cursor in database: {e}")))?;
        Ok(Some(CursorExport {
            bot_id: bot_id.to_string(),
            npub,
            cursor: cursor.unwrap_or(0),
        }))
    } else {
        Ok(None)
    }
}

fn load_bot_handlers(conn: &Connection, bot_id: &str) -> Result<Vec<HandlerExport>, DaemonError> {
    let mut stmt = conn.prepare(
        "SELECT handler_id, bot_ids, event_types, capabilities, registered_at FROM handlers",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
        ))
    })?;

    let mut handlers = Vec::new();
    for row in rows {
        let (id, bot_ids_json, event_types_json, capabilities_json, registered_at_ts) = row?;
        let bot_ids: Vec<String> = serde_json::from_str(&bot_ids_json)?;
        if bot_ids.contains(&bot_id.to_string()) {
            let event_types: Vec<String> = serde_json::from_str(&event_types_json)?;
            let capabilities: Vec<String> = serde_json::from_str(&capabilities_json)?;
            let registered_at = DateTime::from_timestamp(registered_at_ts, 0)
                .unwrap_or_else(Utc::now)
                .to_rfc3339();
            handlers.push(HandlerExport {
                handler_id: id,
                bot_ids,
                event_types,
                capabilities,
                registered_at,
            });
        }
    }

    Ok(handlers)
}

fn save_bot_cursor(conn: &Connection, cursor: &CursorExport) -> Result<(), DaemonError> {
    let now = Utc::now().timestamp();
    let last_event_id = cursor.cursor.to_string();
    conn.execute(
        "INSERT INTO cursors (bot_id, npub, last_event_id, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(bot_id) DO UPDATE SET
            npub = excluded.npub,
            last_event_id = excluded.last_event_id,
            updated_at = excluded.updated_at",
        (&cursor.bot_id, &cursor.npub, last_event_id, now),
    )?;
    Ok(())
}

fn save_handler_export(conn: &Connection, handler: &HandlerExport) -> Result<(), DaemonError> {
    let registered_at = DateTime::parse_from_rfc3339(&handler.registered_at)
        .map_err(|e| DaemonError::Config(format!("invalid registered_at: {e}")))?
        .timestamp();
    let bot_ids = serde_json::to_string(&handler.bot_ids)?;
    let event_types = serde_json::to_string(&handler.event_types)?;
    let capabilities = serde_json::to_string(&handler.capabilities)?;
    conn.execute(
        "INSERT INTO handlers (handler_id, bot_ids, event_types, capabilities, registered_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(handler_id) DO UPDATE SET
            bot_ids = excluded.bot_ids,
            event_types = excluded.event_types,
            capabilities = excluded.capabilities,
            registered_at = excluded.registered_at",
        (
            &handler.handler_id,
            bot_ids,
            event_types,
            capabilities,
            registered_at,
        ),
    )?;
    Ok(())
}

fn count_cursors(conn: &Connection) -> Result<i64, DaemonError> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM cursors", [], |row| row.get(0))?;
    Ok(count)
}

fn print_validate_report(errors: &[String]) {
    if errors.is_empty() {
        println!("config is valid");
    } else {
        println!("config validation failed:");
        for err in errors {
            println!("  - {err}");
        }
    }
}

fn print_diagnose_text(report: &DiagnoseReport) -> Result<(), DaemonError> {
    let mut out = io::stdout().lock();
    writeln!(out, "config_valid: {}", report.config_valid).map_err(DaemonError::Io)?;
    writeln!(out, "lock_held: {}", report.lock_held).map_err(DaemonError::Io)?;
    if let Some(status) = &report.daemon_status {
        writeln!(out, "daemon_status: {status}").map_err(DaemonError::Io)?;
    }
    writeln!(out, "data_dir: {}", report.data_dir).map_err(DaemonError::Io)?;
    writeln!(out, "socket:").map_err(DaemonError::Io)?;
    writeln!(
        out,
        "  path: {} exists: {} owner_readable: {} owner_writable: {}",
        report.socket.path,
        report.socket.exists,
        report.socket.owner_readable,
        report.socket.owner_writable
    )
    .map_err(DaemonError::Io)?;
    if let Some(mode) = report.socket.mode {
        writeln!(out, "  mode: 0o{mode:o}").map_err(DaemonError::Io)?;
    }
    writeln!(out, "bots:").map_err(DaemonError::Io)?;
    for bot in &report.bots {
        write!(
            out,
            "  - id: {}, npub: {}, signing_backend: {}, relays: {}",
            bot.id, bot.npub, bot.signing_backend, bot.relay_count
        )
        .map_err(DaemonError::Io)?;
        if let Some(connected) = bot.live_bunker_connected {
            writeln!(out, ", live_bunker_connected: {connected}").map_err(DaemonError::Io)?;
        } else {
            writeln!(out).map_err(DaemonError::Io)?;
        }
    }
    writeln!(out, "relay_connectivity:").map_err(DaemonError::Io)?;
    for check in &report.relay_connectivity {
        writeln!(
            out,
            "  - bot_id: {}, relay: {}, reachable: {}",
            check.bot_id, check.relay, check.reachable
        )
        .map_err(DaemonError::Io)?;
    }
    writeln!(out, "bunker_connectivity:").map_err(DaemonError::Io)?;
    for check in &report.bunker_connectivity {
        writeln!(
            out,
            "  - bot_id: {}, reachable: {}",
            check.bot_id, check.reachable
        )
        .map_err(DaemonError::Io)?;
    }
    if is_pacto_dev_env() {
        writeln!(out, "service_versions:").map_err(DaemonError::Io)?;
        if let Some(relay) = &report.service_versions.relay {
            writeln!(out, "  relay: {} reachable: {}", relay.url, relay.reachable)
                .map_err(DaemonError::Io)?;
        }
        if let Some(evm) = &report.service_versions.evm_node {
            writeln!(out, "  evm_node: {} reachable: {}", evm.url, evm.reachable)
                .map_err(DaemonError::Io)?;
        }
        if let Some(bunker) = &report.service_versions.bunker_port {
            writeln!(
                out,
                "  bunker_port: {} reachable: {}",
                bunker.url, bunker.reachable
            )
            .map_err(DaemonError::Io)?;
        }
    }
    writeln!(out, "db_cursor_count: {}", report.db_cursor_count).map_err(DaemonError::Io)?;
    if !report.errors.is_empty() {
        writeln!(out, "errors:").map_err(DaemonError::Io)?;
        for err in &report.errors {
            writeln!(out, "  - {err}").map_err(DaemonError::Io)?;
        }
    }
    Ok(())
}

fn print_status_text(report: &StatusReport) -> Result<(), DaemonError> {
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "daemon: {}",
        if report.daemon_running {
            "running"
        } else {
            "stopped"
        }
    )
    .map_err(DaemonError::Io)?;
    if let Some(status) = &report.daemon_status {
        writeln!(out, "status: {status}").map_err(DaemonError::Io)?;
    }
    writeln!(out, "uptime: {}s", report.uptime_seconds).map_err(DaemonError::Io)?;
    writeln!(out, "handlers: {}", report.handlers_registered).map_err(DaemonError::Io)?;

    if !report.bots.is_empty() {
        writeln!(out, "\nbots:").map_err(DaemonError::Io)?;
        for bot in &report.bots {
            writeln!(out, "  - id: {}", bot.id).map_err(DaemonError::Io)?;
            writeln!(out, "    npub: {}", bot.npub).map_err(DaemonError::Io)?;
            writeln!(out, "    relays:").map_err(DaemonError::Io)?;
            if bot.relays.is_empty() {
                writeln!(out, "      (none)").map_err(DaemonError::Io)?;
            } else {
                for check in &bot.relays {
                    write!(out, "      - {}: ", check.relay).map_err(DaemonError::Io)?;
                    if check.reachable {
                        writeln!(out, "reachable").map_err(DaemonError::Io)?;
                    } else if let Some(error) = &check.error {
                        writeln!(out, "unreachable ({error})").map_err(DaemonError::Io)?;
                    } else {
                        writeln!(out, "unreachable").map_err(DaemonError::Io)?;
                    }
                }
            }
            match &bot.bunker {
                Some(check) if check.reachable => {
                    writeln!(out, "    bunker: connected").map_err(DaemonError::Io)?;
                }
                Some(check) => {
                    write!(out, "    bunker: disconnected").map_err(DaemonError::Io)?;
                    if let Some(error) = &check.error {
                        writeln!(out, " ({error})").map_err(DaemonError::Io)?;
                    } else {
                        writeln!(out).map_err(DaemonError::Io)?;
                    }
                }
                None => {
                    writeln!(out, "    bunker: not configured").map_err(DaemonError::Io)?;
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pacto_bot_api::signer::LocalKey;

    fn nsec_signer() -> Result<(LocalKey, String, String), DaemonError> {
        let keys = Keys::generate();
        let nsec = keys
            .secret_key()
            .to_bech32()
            .map_err(|e| DaemonError::Nostr(format!("bech32: {e}")))?;
        let npub = keys
            .public_key()
            .to_bech32()
            .map_err(|e| DaemonError::Nostr(format!("bech32: {e}")))?;
        let signer = LocalKey::parse(&nsec)?;
        Ok((signer, nsec, npub))
    }

    fn dummy_bot(id: &str, npub: &str, nsec: &str) -> BotConfig {
        BotConfig {
            id: id.to_string(),
            npub: npub.to_string(),
            signing: SigningConfig::Nsec {
                nsec: SecretString::new(nsec.to_string().into()),
            },
            relays: vec!["wss://relay.example.com".to_string()],
            capabilities: vec!["ReadMessages".to_string()],
        }
    }

    #[test]
    fn format_toml_array_handles_empty_and_items() {
        assert_eq!(format_toml_array(&[]), "[]");
        assert_eq!(
            format_toml_array(&["a".into(), "b c".into()]),
            "[\"a\", \"b c\"]"
        );
    }

    #[test]
    fn expand_path_expands_tilde() -> Result<(), DaemonError> {
        let home =
            env::var("HOME").map_err(|e| DaemonError::Config(format!("HOME not set: {e}")))?;
        assert_eq!(
            expand_path("~/foo/bar"),
            PathBuf::from(format!("{home}/foo/bar"))
        );
        assert_eq!(expand_path("/abs/path"), PathBuf::from("/abs/path"));
        Ok(())
    }

    #[test]
    fn find_bot_returns_matching_bot() -> Result<(), DaemonError> {
        let bots = vec![dummy_bot("a", "npub1a", "nsec1a")];
        let bot = find_bot(&bots, "a")?;
        assert_eq!(bot.id, "a");
        Ok(())
    }

    #[test]
    fn find_bot_errors_for_unknown() {
        let bots = vec![dummy_bot("a", "npub1a", "nsec1a")];
        let err = find_bot(&bots, "b").unwrap_err();
        assert!(matches!(err, DaemonError::UnknownBot(_)));
    }

    #[test]
    fn signing_backend_label_values() {
        assert_eq!(
            signing_backend_label(&SigningConfig::Nsec {
                nsec: SecretString::new("x".into())
            }),
            "nsec"
        );
        assert_eq!(
            signing_backend_label(&SigningConfig::BunkerLocal {
                uri: SecretString::new("x".into())
            }),
            "bunker_local"
        );
        assert_eq!(
            signing_backend_label(&SigningConfig::BunkerRemote {
                uri: SecretString::new("x".into())
            }),
            "bunker_remote"
        );
    }

    #[test]
    fn generate_hex_token_is_64_hex_chars() -> Result<(), DaemonError> {
        let token = generate_hex_token()?;
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        Ok(())
    }

    #[test]
    fn daemon_lock_detected_by_file_existence() -> Result<(), DaemonError> {
        let dir = tempfile::tempdir().map_err(DaemonError::Io)?;
        assert!(!is_daemon_lock_held(dir.path()));
        fs::write(dir.path().join(DAEMON_LOCK_FILE), b"locked").map_err(DaemonError::Io)?;
        assert!(is_daemon_lock_held(dir.path()));
        Ok(())
    }

    #[test]
    fn write_token_atomic_creates_restricted_file() -> Result<(), DaemonError> {
        let dir = tempfile::tempdir().map_err(DaemonError::Io)?;
        write_token_atomic(dir.path(), "deadbeef0123456789")?;
        let token =
            fs::read_to_string(dir.path().join(BOT_SECRET_TOKEN_FILE)).map_err(DaemonError::Io)?;
        assert_eq!(token, "deadbeef0123456789");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join(BOT_SECRET_TOKEN_FILE))
                .map_err(DaemonError::Io)?
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        Ok(())
    }

    #[test]
    fn open_agent_db_creates_tables() -> Result<(), DaemonError> {
        let dir = tempfile::tempdir().map_err(DaemonError::Io)?;
        let conn = open_agent_db(&dir.path().join(AGENT_DB_FILE))?;
        let count: i32 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name IN ('cursors', 'handlers')",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(count, 2);
        Ok(())
    }

    #[test]
    fn cursor_roundtrip() -> Result<(), DaemonError> {
        let dir = tempfile::tempdir().map_err(DaemonError::Io)?;
        let conn = open_agent_db(&dir.path().join(AGENT_DB_FILE))?;
        let cursor = CursorExport {
            bot_id: "bot-1".to_string(),
            npub: "npub1".to_string(),
            cursor: 42,
        };
        save_bot_cursor(&conn, &cursor)?;
        let loaded = load_bot_cursor(&conn, "bot-1")?
            .ok_or_else(|| DaemonError::Config("expected cursor to be present".to_string()))?;
        assert_eq!(loaded.bot_id, "bot-1");
        assert_eq!(loaded.npub, "npub1");
        assert_eq!(loaded.cursor, 42);
        Ok(())
    }

    #[test]
    fn handler_roundtrip() -> Result<(), DaemonError> {
        let dir = tempfile::tempdir().map_err(DaemonError::Io)?;
        let conn = open_agent_db(&dir.path().join(AGENT_DB_FILE))?;
        let handler = HandlerExport {
            handler_id: "h1".to_string(),
            bot_ids: vec!["bot-1".to_string()],
            event_types: vec!["dm_received".to_string()],
            capabilities: vec!["ReadMessages".to_string()],
            registered_at: Utc::now().to_rfc3339(),
        };
        save_handler_export(&conn, &handler)?;
        let loaded = load_bot_handlers(&conn, "bot-1")?;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].handler_id, "h1");
        Ok(())
    }

    #[test]
    fn count_cursors_counts_saved_rows() -> Result<(), DaemonError> {
        let dir = tempfile::tempdir().map_err(DaemonError::Io)?;
        let conn = open_agent_db(&dir.path().join(AGENT_DB_FILE))?;
        assert_eq!(count_cursors(&conn)?, 0);
        save_bot_cursor(
            &conn,
            &CursorExport {
                bot_id: "b".to_string(),
                npub: "npub1".to_string(),
                cursor: 1,
            },
        )?;
        assert_eq!(count_cursors(&conn)?, 1);
        Ok(())
    }

    #[tokio::test]
    async fn build_profile_event_is_kind_metadata() -> Result<(), DaemonError> {
        let (signer, nsec, npub) = nsec_signer()?;
        let bot = dummy_bot("profile-bot", &npub, &nsec);
        let event = build_profile_event_with_signer(&bot, &signer).await?;

        assert_eq!(event.kind, Kind::Metadata);
        assert!(event.verify_signature());
        assert_eq!(event.id.to_hex().len(), 64);

        let parsed: serde_json::Value = serde_json::from_str(&event.content)?;
        assert_eq!(parsed["name"], "profile-bot");
        assert_eq!(parsed["bot"], true);
        let caps = parsed["capabilities"]
            .as_array()
            .ok_or_else(|| DaemonError::Config("missing capabilities array".into()))?;
        assert!(caps.iter().any(|v| v == "ReadMessages"));
        Ok(())
    }

    #[test]
    fn new_rejects_empty_bot_id() {
        let err = cmd_new("", "nsec", &[], &[], None).unwrap_err();
        assert!(err.to_string().contains("bot_id"));
    }

    #[test]
    fn new_rejects_unknown_backend() {
        let err = cmd_new("x", "invalid", &[], &[], None).unwrap_err();
        assert!(err.to_string().contains("unknown backend"));
    }

    #[test]
    fn inspect_socket_reports_missing_path() {
        let path = Path::new("/nonexistent/pacto-bot-api.sock");
        let health = inspect_socket(path);
        assert_eq!(health.path, path.to_string_lossy());
        assert!(!health.exists);
        assert!(!health.owner_readable);
        assert!(!health.owner_writable);
        assert!(health.mode.is_none());
    }

    #[test]
    fn inspect_socket_reports_temp_file_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pacto-bot-api.sock");
        fs::write(&path, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o600);
            fs::set_permissions(&path, perms).unwrap();
        }
        let health = inspect_socket(&path);
        assert!(health.exists);
        assert!(health.owner_readable);
        assert!(health.owner_writable);
    }

    #[test]
    fn extract_port_from_url_parses_ws_port() {
        assert_eq!(extract_port_from_url("ws://127.0.0.1:4848"), Some(4848));
        assert_eq!(
            extract_port_from_url("wss://relay.example:443/path"),
            Some(443)
        );
        assert_eq!(extract_port_from_url("ws://relay.example"), None);
    }

    #[test]
    fn daemon_status_str_uses_snake_case() {
        assert_eq!(daemon_status_str(DaemonStatus::Ready), "ready");
        assert_eq!(
            daemon_status_str(DaemonStatus::ShuttingDown),
            "shutting_down"
        );
    }
}
