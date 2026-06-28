use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use nostr::event::tag::Tag;
use nostr::key::Keys;
use nostr::secp256k1::schnorr::Signature;
use nostr::{Event, Kind, Timestamp, ToBech32, UnsignedEvent};
use nostr_sdk::Client;
use pacto_bot_api::config::{BotConfig, DaemonConfig, SigningConfig};
use pacto_bot_api::errors::DaemonError;
use pacto_bot_api::signer::{Signer, SignerBackend};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::process;
use std::str::FromStr;

const DAEMON_LOCK_FILE: &str = "daemon.lock";
const BOT_SECRET_TOKEN_FILE: &str = "bot_secret_token";
const AGENT_DB_FILE: &str = "agent.db";
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// `pacto-bot-admin` command-line interface.
#[derive(Parser, Debug)]
#[command(name = "pacto-bot-admin")]
#[command(about = "Pacto bot admin CLI")]
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
        Command::Diagnose { format } => cmd_diagnose(&cli.config, cli.data_dir, &format),
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
        _ => {
            SignerBackend::from_config(&bot.signing, &bot.npub)?;
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
            daemon_version: VERSION.to_string(),
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

fn cmd_diagnose(
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

    let mut errors = Vec::new();
    if let Some(err) = config_error {
        errors.push(err);
    }

    let lock_held = data_dir
        .as_ref()
        .map(|p| is_daemon_lock_held(p))
        .unwrap_or(false);

    let bots: Vec<BotDiagnosis> = config
        .as_ref()
        .map(|c| {
            c.bots
                .iter()
                .map(|b| BotDiagnosis {
                    id: b.id.clone(),
                    npub: b.npub.clone(),
                    signing_backend: signing_backend_label(&b.signing),
                    relay_count: b.relays.len(),
                })
                .collect()
        })
        .unwrap_or_default();

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

    let report = DiagnoseReport {
        config_valid,
        lock_held,
        data_dir: data_dir
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default(),
        bots,
        db_cursor_count,
        errors,
    };

    match format {
        "json" => println!("{}", serde_json::to_string_pretty(&report)?),
        _ => print_diagnose_text(&report)?,
    }

    Ok(())
}

fn find_bot<'a>(bots: &'a [BotConfig], bot_id: &str) -> Result<&'a BotConfig, DaemonError> {
    bots.iter()
        .find(|b| b.id == bot_id)
        .ok_or_else(|| DaemonError::UnknownBot(bot_id.to_string()))
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
    data_dir: String,
    bots: Vec<BotDiagnosis>,
    db_cursor_count: i64,
    errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct BotDiagnosis {
    id: String,
    npub: String,
    signing_backend: String,
    relay_count: usize,
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
    writeln!(out, "data_dir: {}", report.data_dir).map_err(DaemonError::Io)?;
    writeln!(out, "bots:").map_err(DaemonError::Io)?;
    for bot in &report.bots {
        writeln!(
            out,
            "  - id: {}, npub: {}, signing_backend: {}, relays: {}",
            bot.id, bot.npub, bot.signing_backend, bot.relay_count
        )
        .map_err(DaemonError::Io)?;
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
                nsec: nsec.to_string(),
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
                nsec: "x".to_string()
            }),
            "nsec"
        );
        assert_eq!(
            signing_backend_label(&SigningConfig::BunkerLocal {
                uri: "x".to_string()
            }),
            "bunker_local"
        );
        assert_eq!(
            signing_backend_label(&SigningConfig::BunkerRemote {
                uri: "x".to_string()
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
}
