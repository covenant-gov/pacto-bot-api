use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use colored::Colorize;
use fs2::FileExt;
#[cfg(unix)]
use nix::errno::Errno;
#[cfg(unix)]
use nix::sys::signal::{Signal, kill};
#[cfg(unix)]
use nix::unistd::Pid;
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
    AdminSendTestDmResponse, AgentListHandlersResponse, AgentUnregisterHandlerResponse,
    HandlerRegisterResponse, JsonRpcMessage, MetricsResponse, parse_message, serialize_message,
};
use rusqlite::Connection;

use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use serde_json::Value;
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

use pacto_bot_api::guide;

mod scaffold;

const DAEMON_LOCK_FILE: &str = "daemon.lock";
const BOT_SECRET_TOKEN_FILE: &str = "bot_secret_token";
const AGENT_DB_FILE: &str = "agent.db";

/// `pacto-bot-admin` command-line interface.
const TOP_LEVEL_AFTER_HELP: &str = r#"Examples:
  # Create a new dev bot with the nsec backend
  pacto-bot-admin new echo-bot --backend nsec --relays ws://localhost:7000

  # Create a bot and scaffold a Python handler project
  pacto-bot-admin new --scaffold echo-bot --backend nsec --relays ws://localhost:7000 --commands echo

  # Publish the bot's kind:0 profile
  pacto-bot-admin publish-profile echo-bot

  # Test a bunker connection
  pacto-bot-admin test-bunker echo-bot

  # Show daemon status
  pacto-bot-admin status

For a complete operator's guide formatted for LLMs, run:
  pacto-bot-admin --llm-help
"#;

const NEW_AFTER_HELP: &str = r#"Examples:
  # Interactive wizard (prompts for backend, relays, capabilities, and optional profile fields)
  pacto-bot-admin new

  # Dev-only nsec backend (not for production)
  pacto-bot-admin new echo-bot --backend nsec --relays ws://localhost:7000

  # Local bunker backend
  pacto-bot-admin new echo-bot --backend bunker_local --uri bunker://<key>@127.0.0.1:4848

  # Remote bunker backend
  pacto-bot-admin new echo-bot --backend bunker_remote --uri bunker://<key>?relay=wss://relay.nsec.app

  # Create a bot identity and scaffold a Python handler project.
  # The project directory defaults to "echo-bot-project/" and the bot lives at
  # "echo-bot-project/bots/echo-bot/".
  pacto-bot-admin new --scaffold echo-bot --backend nsec --relays ws://localhost:7000 --commands echo

  # Use a custom project directory name (creates "my-project/bots/echo-bot/")
  pacto-bot-admin new --scaffold echo-bot --backend nsec --relays ws://localhost:7000 --commands echo --project-name my-project

  # Use a full project directory path
  pacto-bot-admin new --scaffold echo-bot --backend nsec --relays ws://localhost:7000 --commands echo --project-dir /path/to/my-project

Valid capabilities:
  ReadMessages   Receive decrypted DMs and group messages
  SendMessages   Send replies as the bot
  ManageProfile  Update the bot's kind:0 profile
"#;

const PUBLISH_PROFILE_AFTER_HELP: &str = r#"Examples:
  pacto-bot-admin publish-profile echo-bot
"#;

const TEST_BUNKER_AFTER_HELP: &str = r#"Examples:
  pacto-bot-admin test-bunker echo-bot
"#;

const EXPORT_AFTER_HELP: &str = r#"Examples:
  pacto-bot-admin export echo-bot > echo-bot-state.json
"#;

const IMPORT_AFTER_HELP: &str = r#"Examples:
  pacto-bot-admin import echo-bot echo-bot-state.json
"#;

const VALIDATE_CONFIG_AFTER_HELP: &str = r#"Examples:
  pacto-bot-admin validate-config
"#;

const ROTATE_HTTP_TOKEN_AFTER_HELP: &str = r#"Examples:
  pacto-bot-admin rotate-http-token

Note: The daemon must be restarted or sent SIGHUP to reload the token.
"#;

const DIAGNOSE_AFTER_HELP: &str = r#"Examples:
  pacto-bot-admin diagnose
  pacto-bot-admin diagnose --format json
"#;

const DOCTOR_AFTER_HELP: &str = r#"Examples:
  pacto-bot-admin doctor
"#;

const LOGS_AFTER_HELP: &str = r#"Examples:
  pacto-bot-admin logs
  pacto-bot-admin logs --follow
"#;

const SEND_TEST_DM_AFTER_HELP: &str = r#"Examples:
  pacto-bot-admin send-test-dm echo-bot npub1recipient... "hello"
"#;

const TRACE_EVENTS_AFTER_HELP: &str = r#"Examples:
  pacto-bot-admin trace-events echo-bot
  pacto-bot-admin trace-events echo-bot --since 30 --limit 50
"#;

const STATUS_AFTER_HELP: &str = r#"Examples:
  pacto-bot-admin status
  pacto-bot-admin status --format json
"#;

const HANDLERS_AFTER_HELP: &str = r#"Examples:
  # List all registered handlers
  pacto-bot-admin handlers list

  # Show a single handler
  pacto-bot-admin handlers show <handler_id>

  # Forcibly unregister a stale handler
  pacto-bot-admin handlers unregister <handler_id>
"#;

const SCAFFOLD_AFTER_HELP: &str = r#"Examples:
  # Create a new bot identity and scaffold a Python handler project.
  # The project directory defaults to "echo-bot-project/" and the bot lives at
  # "echo-bot-project/bots/echo-bot/".
  pacto-bot-admin new --scaffold echo-bot --backend nsec --relays ws://localhost:7000 --commands echo

  # Scaffold a project for an existing bot identity (adds to current directory)
  pacto-bot-admin scaffold echo-bot --commands echo

  # Scaffold a bot that calls external HTTP APIs
  pacto-bot-admin scaffold price-bot --commands price

  # Add a second bot to an existing multi-bot project
  pacto-bot-admin scaffold price-bot --commands price
"#;

const UPDATE_AFTER_HELP: &str = r#"Examples:
  # Update an existing bot project from its lock file
  pacto-bot-admin update echo-bot

  # Force overwrite protected files
  pacto-bot-admin update echo-bot --force

  # Refresh cached artifacts from remote sources
  pacto-bot-admin update echo-bot --refresh

  # Update a bot in a specific project directory
  pacto-bot-admin update echo-bot --project-dir /path/to/my-project
"#;

#[derive(Parser, Debug)]
#[command(name = "pacto-bot-admin")]
#[command(about = "Pacto bot admin CLI")]
#[command(version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_COMMIT_SHORT"), ")"))]
#[command(after_help = TOP_LEVEL_AFTER_HELP)]
#[command(before_help = concat!("pacto-bot-admin ", env!("CARGO_PKG_VERSION"), " (", env!("GIT_COMMIT_SHORT"), ")"))]
#[command(arg_required_else_help = true)]
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

    /// Print the LLM-readable operator's guide and exit.
    #[arg(long, global = true)]
    llm_help: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
#[allow(clippy::enum_variant_names)]
enum Command {
    /// Create a new bot identity config snippet.
    #[command(after_help = NEW_AFTER_HELP)]
    New {
        /// Bot identity name (omit to run the interactive wizard).
        #[arg(value_name = "BOT_ID")]
        bot_id: Option<String>,

        /// Signing backend for the new bot.
        /// Valid values: nsec (dev-only), bunker_local, bunker_remote.
        #[arg(short, long, value_name = "BACKEND", default_value = "nsec")]
        backend: String,

        /// Relay URLs for the new bot.
        #[arg(short, long, value_name = "RELAY")]
        relays: Vec<String>,

        /// Capabilities granted to handlers for the new bot.
        /// Valid values: ReadMessages, SendMessages, ManageProfile.
        /// Defaults to ReadMessages,SendMessages when omitted.
        #[arg(long, value_name = "CAPABILITY")]
        capabilities: Vec<String>,

        /// Bunker URI (required for bunker backends; omit to prompt).
        #[arg(short, long, value_name = "URI")]
        uri: Option<String>,

        /// Also scaffold a handler project for the new bot.
        #[arg(long)]
        scaffold: bool,

        /// Language for the generated handler project.
        #[arg(short, long, value_name = "LANG", default_value = "python")]
        language: String,

        /// Slash-command stubs to generate (comma-separated or repeated).
        #[arg(short = 'C', long, value_name = "COMMAND", value_delimiter = ',')]
        commands: Vec<String>,

        /// Skip generating pytest files for `new --scaffold`.
        #[arg(long)]
        no_tests: bool,

        /// Generate the handler with HTTP client dependencies and tests.
        #[arg(long)]
        http: bool,

        /// Overwrite existing files without prompting.
        #[arg(long)]
        force: bool,

        /// Project directory for the scaffolded project.
        ///
        /// This is the outer directory that contains `bots/`, `pacto-bot-api.toml`,
        /// `docker-compose.yml`, and the vendored SDK. The bot itself lives at
        /// `<project-dir>/bots/<bot-id>/`.
        ///
        /// For `new --scaffold`, defaults to `<bot-id>-project/`.
        /// For `scaffold` (existing bot), defaults to the current directory.
        #[arg(long, value_name = "DIR")]
        project_dir: Option<PathBuf>,

        /// Name of the outer project directory (convenience alias for `--project-dir`).
        ///
        /// Only used by `new --scaffold`. Defaults to `<bot-id>-project`.
        /// Ignored when `--project-dir` is also supplied.
        #[arg(long, value_name = "NAME")]
        project_name: Option<String>,

        /// Bot kind (selects a template directory in the template repository).
        #[arg(short, long, value_name = "KIND", default_value = "llm")]
        kind: String,

        /// Template repository URL or local path.
        #[arg(
            long,
            value_name = "URL",
            env = "PACTO_TEMPLATE_REPO",
            default_value = "https://github.com/covenant-gov/pacto-bot-templates"
        )]
        template_repo: String,

        /// Pin a template git ref (tag, branch, or commit).
        #[arg(long, value_name = "REF")]
        template_ref: Option<String>,

        /// Re-fetch cached artifacts from remote sources before resolving.
        #[arg(long)]
        refresh: bool,

        /// Prune stale cache entries before resolving.
        #[arg(long)]
        prune_cache: bool,

        /// Allow `cargo-generate` to execute pre/post-generation hooks.
        #[arg(long)]
        allow_hooks: bool,
    },
    /// Publish a bot profile (kind:0) event.
    #[command(after_help = PUBLISH_PROFILE_AFTER_HELP)]
    PublishProfile {
        #[arg(value_name = "BOT_ID")]
        bot_id: String,
    },
    /// Test a NIP-46 bunker connection and pubkey match.
    #[command(after_help = TEST_BUNKER_AFTER_HELP)]
    TestBunker {
        #[arg(value_name = "BOT_ID")]
        bot_id: String,
    },
    /// Export bot daemon-local state to JSON.
    #[command(after_help = EXPORT_AFTER_HELP)]
    Export {
        #[arg(value_name = "BOT_ID")]
        bot_id: String,
    },
    /// Import bot daemon-local state from JSON.
    #[command(after_help = IMPORT_AFTER_HELP)]
    Import {
        #[arg(value_name = "BOT_ID")]
        bot_id: String,

        #[arg(value_name = "STATE_FILE")]
        state_file: String,
    },
    /// Validate the daemon configuration file.
    #[command(after_help = VALIDATE_CONFIG_AFTER_HELP)]
    ValidateConfig,
    /// Rotate the HTTP secret token.
    #[command(after_help = ROTATE_HTTP_TOKEN_AFTER_HELP)]
    RotateHttpToken,
    /// Emit structured daemon diagnostics.
    #[command(after_help = DIAGNOSE_AFTER_HELP)]
    Diagnose {
        /// Output format. Valid values: text, json.
        #[arg(short, long, value_name = "FORMAT", default_value = "text")]
        format: String,
    },
    /// Run a quick health check and print PASS/FAIL results.
    #[command(after_help = DOCTOR_AFTER_HELP)]
    Doctor,
    /// Tail the daemon log file.
    #[command(after_help = LOGS_AFTER_HELP)]
    Logs {
        /// Follow the log file as it grows.
        #[arg(short, long)]
        follow: bool,
    },
    /// Send a test DM as a bot (admin-only).
    #[command(after_help = SEND_TEST_DM_AFTER_HELP)]
    SendTestDm {
        #[arg(value_name = "BOT_ID")]
        bot_id: String,

        #[arg(value_name = "RECIPIENT")]
        recipient: String,

        #[arg(value_name = "CONTENT")]
        content: String,
    },
    /// Read recent event trace rows from the daemon database.
    #[command(after_help = TRACE_EVENTS_AFTER_HELP)]
    TraceEvents {
        #[arg(value_name = "BOT_ID")]
        bot_id: String,

        /// Only include rows newer than this many minutes ago.
        #[arg(short, long, value_name = "MINUTES", default_value = "10")]
        since: i64,

        /// Maximum rows to return.
        #[arg(short, long, value_name = "LIMIT", default_value = "100")]
        limit: usize,
    },
    /// Show daemon status, connectivity, and registered handlers.
    #[command(after_help = STATUS_AFTER_HELP)]
    Status {
        /// Output format. Valid values: text, json.
        #[arg(short, long, value_name = "FORMAT", default_value = "text")]
        format: String,
    },
    /// Manage registered handler connections.
    #[command(subcommand)]
    Handlers(HandlersCommand),
    /// Scaffold a handler project for an existing bot identity.
    #[command(after_help = SCAFFOLD_AFTER_HELP)]
    Scaffold {
        /// Bot identity name from the daemon config.
        #[arg(value_name = "BOT_ID")]
        bot_id: String,

        /// Language for the generated handler project.
        #[arg(short, long, value_name = "LANG", default_value = "python")]
        language: String,

        /// Slash-command stubs to generate (comma-separated or repeated).
        #[arg(short = 'C', long, value_name = "COMMAND", value_delimiter = ',')]
        commands: Vec<String>,

        /// Generate pytest files even when retrofitting an existing project.
        #[arg(long)]
        with_tests: bool,

        /// Generate the handler with HTTP client dependencies and tests.
        #[arg(long)]
        http: bool,

        /// Overwrite existing files without prompting.
        #[arg(long)]
        force: bool,

        /// Project directory (default: current directory).
        #[arg(long, value_name = "DIR")]
        project_dir: Option<PathBuf>,

        /// Bot kind (selects a template directory in the template repository).
        #[arg(short, long, value_name = "KIND", default_value = "llm")]
        kind: String,

        /// Template repository URL or local path.
        #[arg(
            long,
            value_name = "URL",
            env = "PACTO_TEMPLATE_REPO",
            default_value = "https://github.com/covenant-gov/pacto-bot-templates"
        )]
        template_repo: String,

        /// Pin a template git ref (tag, branch, or commit).
        #[arg(long, value_name = "REF")]
        template_ref: Option<String>,

        /// Re-fetch cached artifacts from remote sources before resolving.
        #[arg(long)]
        refresh: bool,

        /// Allow `cargo-generate` to execute pre/post-generation hooks.
        #[arg(long)]
        allow_hooks: bool,
    },
    /// Update an existing bot project from its lock file.
    #[command(after_help = UPDATE_AFTER_HELP)]
    Update {
        /// Bot identity name to update.
        #[arg(value_name = "BOT_ID")]
        bot_id: String,

        /// Overwrite protected files without prompting.
        #[arg(long)]
        force: bool,

        /// Re-fetch cached artifacts from remote sources before resolving.
        #[arg(long)]
        refresh: bool,

        /// Prune stale cache entries before resolving.
        #[arg(long)]
        prune_cache: bool,

        /// Allow `cargo-generate` to execute pre/post-generation hooks.
        #[arg(long)]
        allow_hooks: bool,

        /// Project directory containing the bot (default: current directory).
        #[arg(long, value_name = "DIR")]
        project_dir: Option<PathBuf>,
    },
    /// Print documentation in the requested format.
    Docs {
        /// Output format. Valid values: llm.
        #[arg(short, long, value_name = "FORMAT", default_value = "llm")]
        format: String,
    },
    /// Show the CLI version.
    Version,
}

#[derive(Subcommand, Debug)]
#[command(after_help = HANDLERS_AFTER_HELP)]
enum HandlersCommand {
    /// List all registered handlers.
    List,
    /// Show a single handler by id.
    Show {
        #[arg(value_name = "HANDLER_ID")]
        handler_id: String,
    },
    /// Forcibly unregister a handler from the routing table.
    Unregister {
        #[arg(value_name = "HANDLER_ID")]
        handler_id: String,
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
    if cli.llm_help {
        print_llm_guide();
        return Ok(());
    }

    let Some(command) = cli.command else {
        return Err(DaemonError::Config(
            "a subcommand is required; use --help for usage".into(),
        ));
    };

    match command {
        Command::New {
            bot_id,
            backend,
            relays,
            capabilities,
            uri,
            scaffold,
            language,
            commands,
            no_tests,
            http,
            force,
            project_dir,
            project_name,
            kind,
            template_repo,
            template_ref,
            refresh,
            prune_cache,
            allow_hooks,
        } => {
            cmd_new(
                bot_id.as_deref(),
                &backend,
                &relays,
                &capabilities,
                uri,
                scaffold,
                &language,
                &kind,
                &commands,
                !no_tests,
                http,
                force,
                project_dir.as_deref(),
                project_name.as_deref(),
                &template_repo,
                template_ref.as_deref(),
                refresh,
                prune_cache,
                allow_hooks,
            )
            .await
        }
        Command::Scaffold {
            bot_id,
            language,
            commands,
            with_tests,
            http,
            force,
            project_dir,
            kind,
            template_repo,
            template_ref,
            refresh,
            allow_hooks,
        } => {
            cmd_scaffold(
                &cli.config,
                &bot_id,
                &language,
                &kind,
                &commands,
                with_tests,
                http,
                force,
                project_dir.as_deref(),
                &template_repo,
                template_ref.as_deref(),
                refresh,
                allow_hooks,
            )
            .await
        }
        Command::Update {
            bot_id,
            force,
            refresh,
            prune_cache,
            allow_hooks,
            project_dir,
        } => {
            cmd_update(
                &bot_id,
                force,
                refresh,
                prune_cache,
                allow_hooks,
                project_dir.as_deref(),
            )
            .await
        }
        Command::PublishProfile { bot_id } => cmd_publish_profile(&cli.config, &bot_id).await,
        Command::TestBunker { bot_id } => cmd_test_bunker(&cli.config, &bot_id).await,
        Command::Export { bot_id } => cmd_export(&cli.config, cli.data_dir, &bot_id),
        Command::Import { bot_id, state_file } => {
            cmd_import(&cli.config, cli.data_dir, &bot_id, &state_file)
        }
        Command::ValidateConfig => cmd_validate_config(&cli.config, cli.data_dir),
        Command::RotateHttpToken => cmd_rotate_http_token(&cli.config, cli.data_dir),
        Command::Diagnose { format } => cmd_diagnose(&cli.config, cli.data_dir, &format).await,
        Command::Doctor => cmd_doctor(&cli.config, cli.data_dir).await,
        Command::Logs { follow } => cmd_logs(&cli.config, cli.data_dir, follow).await,
        Command::SendTestDm {
            bot_id,
            recipient,
            content,
        } => cmd_send_test_dm(&cli.config, cli.data_dir, &bot_id, &recipient, &content).await,
        Command::TraceEvents {
            bot_id,
            since,
            limit,
        } => cmd_trace_events(&cli.config, cli.data_dir, &bot_id, since, limit).await,
        Command::Status { format } => cmd_status(&cli.config, cli.data_dir, &format).await,
        Command::Handlers(sub) => cmd_handlers(&cli.config, cli.data_dir, &sub).await,
        Command::Docs { format } => cmd_docs(&format),
        Command::Version => {
            println!(
                "pacto-bot-admin {} ({})",
                pacto_bot_api::version::VERSION,
                pacto_bot_api::version::GIT_COMMIT_SHORT
            );
            Ok(())
        }
    }
}

fn print_llm_guide() {
    print!("{}", guide::render_llm_guide());
}

fn cmd_docs(format: &str) -> Result<(), DaemonError> {
    match format {
        "llm" => {
            print_llm_guide();
            Ok(())
        }
        other => Err(DaemonError::Config(format!(
            "unsupported docs format: {other}; expected 'llm'"
        ))),
    }
}

#[allow(clippy::too_many_arguments)]
async fn cmd_new(
    bot_id: Option<&str>,
    backend: &str,
    relays: &[String],
    capabilities: &[String],
    uri: Option<String>,
    scaffold: bool,
    language: &str,
    kind: &str,
    commands: &[String],
    with_tests: bool,
    http: bool,
    force: bool,
    project_dir: Option<&Path>,
    project_name: Option<&str>,
    template_repo: &str,
    template_ref: Option<&str>,
    refresh: bool,
    prune_cache: bool,
    allow_hooks: bool,
) -> Result<(), DaemonError> {
    let interactive = bot_id.is_none();

    let params = if interactive {
        run_interactive_new()?
    } else {
        let bot_id = bot_id.unwrap_or_default();
        validate_bot_id(bot_id)?;

        let uri = if matches!(backend, "bunker_local" | "bunker_remote") && uri.is_none() {
            Some(SecretString::new(prompt_uri_with_label(backend)?.into()))
        } else {
            uri.map(|s| SecretString::new(s.into()))
        };

        NewBotParams {
            bot_id: bot_id.to_string(),
            backend: backend.to_string(),
            relays: relays.to_vec(),
            capabilities: if capabilities.is_empty() {
                vec!["ReadMessages".to_string(), "SendMessages".to_string()]
            } else {
                capabilities.to_vec()
            },
            uri,
            display_name: None,
            about: None,
            picture: None,
            scaffold: false,
            http: false,
            project_dir: None,
        }
    };

    let scaffold = if interactive {
        params.scaffold
    } else {
        scaffold
    };
    let project_dir: Option<&Path> = if interactive {
        params.project_dir.as_deref()
    } else {
        project_dir
    };

    validate_backend(&params.backend)?;
    for relay in &params.relays {
        validate_relay_url(relay)?;
    }
    for cap in &params.capabilities {
        validate_capability(cap)?;
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

    let snippet = build_bot_snippet(&params, &npub, &nsec);

    if prune_cache {
        let cache = crate::scaffold::cache::Cache::new()?;
        let removed = cache.prune()?;
        println!("Pruned {removed} stale cache entries");
    }

    if scaffold {
        let language = if interactive {
            prompt_language()?
        } else {
            validate_language(language)?;
            language.to_string()
        };
        let kind = if interactive {
            "llm".to_string()
        } else {
            validate_kind(kind)?;
            kind.to_string()
        };
        let commands = if interactive {
            prompt_commands()?
        } else {
            normalize_commands(commands)
        };
        let with_tests = if interactive {
            prompt_yes_no("Generate pytest files?")?
        } else {
            with_tests
        };
        let http = if interactive { params.http } else { http };
        let project_dir = project_dir
            .map(Path::to_path_buf)
            .or_else(|| project_name.map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(format!("{}-project", params.bot_id)));
        let project_dir_display = project_dir.display().to_string();

        // Help the user see the distinction between project dir and bot dir.
        println!(
            "Project directory: {} (bot will be at bots/{}/)",
            project_dir_display, params.bot_id
        );

        scaffold::generate::run_scaffold(scaffold::generate::ScaffoldRequest {
            bot_id: params.bot_id.clone(),
            language,
            kind,
            commands,
            with_tests,
            http,
            force,
            allow_hooks,
            project_dir,
            template_repo: template_repo.to_string(),
            template_ref: template_ref.map(|s| s.to_string()),
            refresh,
            mode: scaffold::generate::ScaffoldMode::NewProject { snippet },
        })
        .await?;
        println!(
            "Created scaffolded project for {} in {}",
            params.bot_id, project_dir_display
        );
        return Ok(());
    }

    if interactive {
        println!("\nPreview of the config snippet that will be generated:\n");
        println!("{snippet}");
        if !prompt_yes_no("Create this bot identity?")? {
            println!("Cancelled.");
            return Ok(());
        }
    }

    println!("{snippet}");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_scaffold(
    config_path: &Path,
    bot_id: &str,
    language: &str,
    kind: &str,
    commands: &[String],
    with_tests: bool,
    http: bool,
    force: bool,
    project_dir: Option<&Path>,
    template_repo: &str,
    template_ref: Option<&str>,
    refresh: bool,
    allow_hooks: bool,
) -> Result<(), DaemonError> {
    validate_bot_id(bot_id)?;
    validate_language(language)?;
    validate_kind(kind)?;

    let project_dir = project_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let project_dir_display = project_dir.display().to_string();

    // The global --config default is relative to CWD. For the scaffold
    // subcommand the natural default is the config inside --project-dir.
    let config_path = if config_path == Path::new("pacto-bot-api.toml") {
        project_dir.join("pacto-bot-api.toml")
    } else {
        config_path.to_path_buf()
    };
    let config = DaemonConfig::load(&config_path)?;
    let bot = find_bot(&config.bots, bot_id)?;

    scaffold::generate::run_scaffold(scaffold::generate::ScaffoldRequest {
        bot_id: bot_id.to_string(),
        language: language.to_string(),
        kind: kind.to_string(),
        commands: normalize_commands(commands),
        with_tests,
        http,
        force,
        allow_hooks,
        project_dir,
        template_repo: template_repo.to_string(),
        template_ref: template_ref.map(|s| s.to_string()),
        refresh,
        mode: scaffold::generate::ScaffoldMode::ExistingProject {
            bot_config: bot.clone(),
        },
    })
    .await?;
    println!("Updated scaffold for {bot_id} in {}", project_dir_display);
    Ok(())
}

async fn cmd_update(
    bot_id: &str,
    force: bool,
    refresh: bool,
    prune_cache: bool,
    allow_hooks: bool,
    project_dir: Option<&Path>,
) -> Result<(), DaemonError> {
    validate_bot_id(bot_id)?;

    if prune_cache {
        let cache = crate::scaffold::cache::Cache::new()?;
        let removed = cache.prune()?;
        println!("Pruned {removed} stale cache entries");
    }

    let project_dir = project_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    crate::scaffold::update::run_update(&project_dir, bot_id, force, refresh, allow_hooks).await?;
    println!("Updated scaffold for {bot_id} in {}", project_dir.display());
    Ok(())
}

fn validate_language(language: &str) -> Result<(), DaemonError> {
    match language {
        "python" => Ok(()),
        other => Err(DaemonError::Config(format!(
            "unsupported scaffold language: {other}; supported languages: python"
        ))),
    }
}

fn validate_kind(kind: &str) -> Result<(), DaemonError> {
    match kind {
        "llm" | "governance" => Ok(()),
        other => Err(DaemonError::Config(format!(
            "unsupported scaffold kind: {other}; supported kinds: llm, governance"
        ))),
    }
}

fn normalize_commands(commands: &[String]) -> Vec<String> {
    commands
        .iter()
        .flat_map(|s| s.split(','))
        .map(|s| s.trim().trim_start_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn prompt_language() -> Result<String, DaemonError> {
    println!("\nHandler language:");
    println!("  1) python (default)");
    loop {
        let input = prompt_line("Choose language [1]: ")?;
        let choice = if input.trim().is_empty() {
            "1"
        } else {
            input.trim()
        };
        match choice {
            "1" | "python" => return Ok("python".to_string()),
            _ => println!("Invalid choice; enter 1 or 'python'."),
        }
    }
}

fn prompt_commands() -> Result<Vec<String>, DaemonError> {
    println!("\nSlash commands to scaffold (e.g. echo,help). Leave blank for none.");
    let input = prompt_line("Commands: ")?;
    let raw: Vec<String> = input
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    Ok(normalize_commands(&raw))
}

/// Parameters collected for a new bot identity.
#[derive(Debug, Clone)]
struct NewBotParams {
    bot_id: String,
    backend: String,
    relays: Vec<String>,
    capabilities: Vec<String>,
    uri: Option<SecretString>,
    display_name: Option<String>,
    about: Option<String>,
    picture: Option<String>,
    scaffold: bool,
    http: bool,
    project_dir: Option<PathBuf>,
}

fn run_interactive_new() -> Result<NewBotParams, DaemonError> {
    println!("\nCreate a new Pacto bot identity\n");

    let bot_id = prompt_bot_id()?;

    let backend = prompt_backend()?;

    let uri = if matches!(backend.as_str(), "bunker_local" | "bunker_remote") {
        Some(SecretString::new(prompt_uri_with_label(&backend)?.into()))
    } else {
        None
    };

    let relays = prompt_relays()?;
    let capabilities = prompt_capabilities()?;

    println!("\nOptional profile fields (press Enter to skip):");
    let display_name = prompt_optional("Display name (defaults to bot id): ")?;
    let about = prompt_optional("About text: ")?;
    let picture = prompt_optional("Picture URL: ")?;

    let scaffold = prompt_yes_no("Scaffold a handler project?")?;
    let http = if scaffold {
        prompt_yes_no("Will this bot call external HTTP APIs (adds httpx + respx)?")?
    } else {
        false
    };
    let project_dir = if scaffold {
        let default = PathBuf::from(format!("{}-project", bot_id));
        let input = prompt_line(&format!("Project directory [{}]: ", default.display()))?;
        let dir = if input.trim().is_empty() {
            default
        } else {
            PathBuf::from(input.trim())
        };
        Some(dir)
    } else {
        None
    };

    Ok(NewBotParams {
        bot_id,
        backend,
        relays,
        capabilities,
        uri,
        display_name,
        about,
        picture,
        scaffold,
        http,
        project_dir,
    })
}

fn build_bot_snippet(params: &NewBotParams, npub: &str, nsec: &str) -> String {
    let mut lines = Vec::new();
    lines.push("[[bots]]".to_string());
    lines.push(format!("id = {:?}", params.bot_id));
    lines.push(format!("npub = {npub:?}"));

    match params.backend.as_str() {
        "nsec" => {
            lines.push(format!(
                "signing = {{ backend = \"nsec\", nsec = {nsec:?} }}"
            ));
        }
        backend => {
            let uri = params.uri.as_ref().map(|s| s.expose_secret()).unwrap_or("");
            lines.push(format!(
                "signing = {{ backend = {backend:?}, uri = \"${{PACTO_BUNKER_URI:-{uri}}}\" }}"
            ));
        }
    }

    match params.relays.len() {
        0 => lines.push("relays = [\"${PACTO_RELAY_URL:-ws://localhost:7000}\"]".to_string()),
        1 => lines.push(format!(
            "relays = [\"${{PACTO_RELAY_URL:-{}}}\"]",
            params.relays[0]
        )),
        _ => lines.push(format!("relays = {}", format_toml_array(&params.relays))),
    }
    lines.push(format!(
        "capabilities = {}",
        format_toml_array(&params.capabilities)
    ));

    if let Some(display_name) = &params.display_name {
        lines.push(format!("display_name = {display_name:?}"));
    }
    if let Some(about) = &params.about {
        lines.push(format!("about = {about:?}"));
    }
    if let Some(picture) = &params.picture {
        lines.push(format!("picture = {picture:?}"));
    }

    lines.join("\n") + "\n"
}

fn validate_bot_id(bot_id: &str) -> Result<(), DaemonError> {
    if bot_id.is_empty() {
        return Err(DaemonError::Config("bot_id must not be empty".into()));
    }
    if bot_id.len() > 64 {
        return Err(DaemonError::Config(
            "bot_id must be 64 characters or fewer".into(),
        ));
    }
    if bot_id.contains(|c: char| c.is_whitespace() || c == '/' || c == '\\') {
        return Err(DaemonError::Config(
            "bot_id must not contain whitespace, '/', or '\\\\'".into(),
        ));
    }
    Ok(())
}

fn validate_backend(backend: &str) -> Result<(), DaemonError> {
    match backend {
        "nsec" | "bunker_local" | "bunker_remote" => Ok(()),
        _ => Err(DaemonError::Config(format!("unknown backend: {backend}"))),
    }
}

fn validate_relay_url(url: &str) -> Result<(), DaemonError> {
    if url.is_empty() {
        return Err(DaemonError::Config("relay URL must not be empty".into()));
    }
    if !(url.starts_with("ws://") || url.starts_with("wss://")) {
        return Err(DaemonError::Config(format!(
            "relay URL must start with ws:// or wss://: {url}"
        )));
    }
    Ok(())
}

fn validate_capability(cap: &str) -> Result<(), DaemonError> {
    match cap {
        "ReadMessages" | "SendMessages" | "ManageProfile" => Ok(()),
        _ => Err(DaemonError::Config(format!(
            "unknown capability: {cap}; expected ReadMessages, SendMessages, or ManageProfile"
        ))),
    }
}

fn prompt_bot_id() -> Result<String, DaemonError> {
    loop {
        let input = prompt_nonempty("Bot identity name: ")?;
        if let Err(e) = validate_bot_id(&input) {
            println!("Invalid name: {e}");
            continue;
        }
        return Ok(input);
    }
}

fn prompt_backend() -> Result<String, DaemonError> {
    println!("Signing backend:");
    println!("  1) nsec         - local dev key (prints nsec; do not use in production)");
    println!("  2) bunker_local - NIP-46 bunker on the same machine");
    println!("  3) bunker_remote - NIP-46 bunker reachable over wss://");

    loop {
        let input = prompt_line("Choose backend [1]: ")?;
        let choice = if input.trim().is_empty() {
            "1"
        } else {
            input.trim()
        };
        match choice {
            "1" => return Ok("nsec".to_string()),
            "2" => return Ok("bunker_local".to_string()),
            "3" => return Ok("bunker_remote".to_string()),
            _ => println!("Invalid choice; enter 1, 2, or 3."),
        }
    }
}

fn prompt_uri_with_label(backend: &str) -> Result<String, DaemonError> {
    let label = match backend {
        "bunker_local" => "local bunker URI (e.g. bunker://<pubkey>@127.0.0.1:4848)",
        "bunker_remote" => "remote bunker URI (e.g. bunker://<pubkey>?relay=wss://relay.nsec.app)",
        _ => "bunker URI",
    };
    loop {
        let uri = prompt_nonempty(&format!("Enter {label}: "))?;
        if uri.is_empty() {
            println!("A bunker URI is required for this backend.");
            continue;
        }
        if backend == "bunker_remote" && uri.contains("ws://") {
            println!("Remote bunker must use wss://, not ws://.");
            continue;
        }
        return Ok(uri);
    }
}

fn prompt_relays() -> Result<Vec<String>, DaemonError> {
    println!("\nRelay URLs for this bot (ws:// or wss://).");
    println!("Leave blank and press Enter to finish. If none are entered,");
    println!("the default dev relay ws://localhost:7000 will be used.");

    let mut relays = Vec::new();
    loop {
        let prompt = if relays.is_empty() {
            "Relay URL [ws://localhost:7000]: ".to_string()
        } else {
            "Relay URL (blank to finish): ".to_string()
        };
        let input = prompt_line(&prompt)?;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            if relays.is_empty() {
                relays.push("ws://localhost:7000".to_string());
            }
            return Ok(relays);
        }
        if let Err(e) = validate_relay_url(trimmed) {
            println!("Invalid relay: {e}");
            continue;
        }
        relays.push(trimmed.to_string());
    }
}

fn prompt_capabilities() -> Result<Vec<String>, DaemonError> {
    println!("\nCapabilities grant handlers permission to act on behalf of this bot.");
    println!("  ReadMessages   - receive decrypted DMs and group messages");
    println!("  SendMessages   - send replies as the bot");
    println!("  ManageProfile  - update the bot's kind:0 profile");

    loop {
        let input = prompt_line("Capabilities (comma-separated) [ReadMessages, SendMessages]: ")?;
        let raw = if input.trim().is_empty() {
            "ReadMessages, SendMessages".to_string()
        } else {
            input.trim().to_string()
        };
        let caps: Vec<String> = raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let mut valid = true;
        for cap in &caps {
            if let Err(e) = validate_capability(cap) {
                println!("{e}");
                valid = false;
                break;
            }
        }
        if valid && !caps.is_empty() {
            return Ok(caps);
        }
        if caps.is_empty() {
            println!("Select at least one capability.");
        }
    }
}

fn prompt_optional(prompt: &str) -> Result<Option<String>, DaemonError> {
    let input = prompt_line(prompt)?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

fn prompt_yes_no(prompt: &str) -> Result<bool, DaemonError> {
    let input = prompt_line(&format!("{prompt} [y/N]: "))?;
    Ok(input.trim().eq_ignore_ascii_case("y"))
}

fn prompt_nonempty(prompt: &str) -> Result<String, DaemonError> {
    loop {
        let input = prompt_line(prompt)?;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            println!("A value is required.");
            continue;
        }
        return Ok(trimmed.to_string());
    }
}

fn prompt_line(prompt: &str) -> Result<String, DaemonError> {
    print!("{prompt}");
    io::stdout().flush().map_err(DaemonError::Io)?;
    let mut buf = String::new();
    io::stdin().read_line(&mut buf).map_err(DaemonError::Io)?;
    Ok(buf)
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
    if let Some(cfg) = &config {
        for bot in &cfg.bots {
            relay_connectivity.extend(check_relay_connectivity(bot).await);
            if let Some(check) = check_bunker_connectivity(bot).await {
                bunker_connectivity.push(check);
            }
        }
    }

    let service_versions = if let Some(cfg) = &config {
        probe_service_versions(&cfg.bots).await
    } else {
        ServiceVersions::default()
    };

    let db_cursor_count = if let Some(dir) = &data_dir {
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

    let recent_counts = live_snapshot.as_ref().map(|s| RecentCountsReport {
        events_received: s.recent_counts.events_received,
        events_dispatched: s.recent_counts.events_dispatched,
        replies: s.recent_counts.replies,
        reply_send_failed: s.recent_counts.reply_send_failed,
        window_minutes: s.recent_counts.window_seconds / 60,
    });

    let mut bot_cursors = Vec::new();
    let mut handler_count = 0i64;
    if let Some(dir) = &data_dir {
        let db_path = dir.join(AGENT_DB_FILE);
        if db_path.exists() {
            match open_agent_db(&db_path) {
                Ok(conn) => {
                    if let Some(cfg) = &config {
                        for bot in &cfg.bots {
                            match load_bot_cursor(&conn, &bot.id) {
                                Ok(Some(cursor)) => bot_cursors.push(BotCursorDiagnosis {
                                    bot_id: bot.id.clone(),
                                    cursor: cursor.cursor,
                                }),
                                Ok(None) => {}
                                Err(e) => {
                                    errors.push(format!("bot {}: cursor load error: {e}", bot.id))
                                }
                            }
                        }
                    }
                    match count_handlers(&conn) {
                        Ok(n) => handler_count = n,
                        Err(e) => errors.push(format!("handler count error: {e}")),
                    }
                }
                Err(e) => errors.push(format!("failed to open db: {e}")),
            }
        }
    }

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
        recent_counts,
        bot_cursors,
        handler_count,
        errors,
    };

    match format {
        "json" => println!("{}", serde_json::to_string_pretty(&report)?),
        _ => print_diagnose_text(&report)?,
    }

    Ok(())
}

async fn cmd_doctor(
    config_path: &Path,
    data_dir_override: Option<PathBuf>,
) -> Result<(), DaemonError> {
    let mut checks = Vec::new();

    let config_result = DaemonConfig::load(config_path);
    let config = match config_result {
        Ok(c) => {
            checks.push(doctor_pass("config", "configuration file is valid"));
            Some(c)
        }
        Err(e) => {
            checks.push(doctor_fail(
                "config",
                &format!("failed to parse configuration: {e}"),
                "fix the TOML syntax and required fields, then re-run doctor",
            ));
            None
        }
    };

    let data_dir = config
        .as_ref()
        .map(|c| resolve_data_dir(c, data_dir_override.clone()))
        .or_else(|| data_dir_override.as_deref().map(expand_path_buf));

    let mut has_fatal = false;
    for check in &checks {
        if !check.pass {
            has_fatal = true;
            break;
        }
    }
    if has_fatal {
        print_doctor_report(&checks);
        return Err(DaemonError::Config(
            "doctor found fatal configuration errors".into(),
        ));
    }

    let data_dir = match data_dir {
        Some(dir) => {
            if let Err(e) = std::fs::create_dir_all(&dir) {
                checks.push(doctor_fail(
                    "data_dir",
                    &format!("data directory is not writable: {e}"),
                    "ensure the parent directory exists and is writable by this user",
                ));
            } else if !dir.is_dir() {
                checks.push(doctor_fail(
                    "data_dir",
                    "data_dir path is not a directory",
                    "set a directory path for --data-dir or data_dir in config",
                ));
            } else {
                checks.push(doctor_pass(
                    "data_dir",
                    "data directory exists and is writable",
                ));
            }
            dir
        }
        None => {
            checks.push(doctor_fail(
                "data_dir",
                "data directory cannot be determined",
                "set --data-dir or data_dir in the config file",
            ));
            PathBuf::new()
        }
    };

    let lock_held = if data_dir.as_os_str().is_empty() {
        false
    } else {
        is_daemon_lock_held(&data_dir)
    };
    if lock_held {
        checks.push(doctor_pass(
            "daemon_lock",
            "daemon is running and holds the lock",
        ));
    } else {
        checks.push(doctor_fail(
            "daemon_lock",
            "daemon is not running or lock is stale",
            "start the daemon with `pacto-bot-api`",
        ));
    }

    let bots = config.as_ref().map(|c| c.bots.as_slice()).unwrap_or(&[]);
    if bots.is_empty() {
        checks.push(doctor_fail(
            "bots",
            "no bots configured",
            "add at least one [[bots]] entry to the config file",
        ));
    } else {
        checks.push(doctor_pass(
            "bots",
            &format!("{} bot(s) configured", bots.len()),
        ));
    }

    let mut all_relays_reachable = true;
    for bot in bots {
        let relay_checks = check_relay_connectivity(bot).await;
        for check in relay_checks {
            if !check.reachable {
                all_relays_reachable = false;
                checks.push(doctor_fail(
                    "relay_reachability",
                    &format!(
                        "bot {} relay {} is unreachable: {}",
                        check.bot_id,
                        check.relay,
                        check.error.as_deref().unwrap_or("unknown error")
                    ),
                    "check the relay URL and network connectivity",
                ));
            }
        }
    }
    if all_relays_reachable && !bots.is_empty() {
        checks.push(doctor_pass(
            "relay_reachability",
            "all configured relays are reachable",
        ));
    }

    let handler_count = if !data_dir.as_os_str().is_empty() {
        let db_path = data_dir.join(AGENT_DB_FILE);
        if db_path.exists() {
            match open_agent_db(&db_path) {
                Ok(conn) => count_handlers(&conn).unwrap_or(0),
                Err(_) => 0,
            }
        } else {
            0
        }
    } else {
        0
    };
    if handler_count > 0 || bots.is_empty() {
        checks.push(doctor_pass(
            "handlers",
            &format!("{handler_count} handler(s) registered"),
        ));
    } else {
        checks.push(doctor_fail(
            "handlers",
            "no handlers registered",
            "start a handler and connect to the daemon socket",
        ));
    }

    let token_path = data_dir.join(BOT_SECRET_TOKEN_FILE);
    if token_path.exists() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&token_path)
                .map(|m| m.permissions().mode() & 0o777)
                .unwrap_or(0o777);
            if mode == 0o600 {
                checks.push(doctor_pass(
                    "token_permissions",
                    "HTTP secret token has strict permissions (0o600)",
                ));
            } else {
                checks.push(doctor_fail(
                    "token_permissions",
                    &format!("HTTP secret token permissions are 0o{mode:o}"),
                    "run `chmod 0600` on the token file",
                ));
            }
        }
        #[cfg(not(unix))]
        {
            checks.push(doctor_pass(
                "token_permissions",
                "HTTP secret token exists (permissions not checked on this platform)",
            ));
        }
    } else {
        checks.push(doctor_pass(
            "token_permissions",
            "no HTTP secret token present (HTTP transport disabled)",
        ));
    }

    print_doctor_report(&checks);

    let failures = checks.iter().filter(|c| !c.pass).count();
    if failures > 0 {
        Err(DaemonError::Config(format!(
            "doctor found {failures} failing check(s)"
        )))
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct DoctorCheck {
    name: &'static str,
    pass: bool,
    message: String,
    suggestion: Option<String>,
}

fn doctor_pass(name: &'static str, message: &str) -> DoctorCheck {
    DoctorCheck {
        name,
        pass: true,
        message: message.to_string(),
        suggestion: None,
    }
}

fn doctor_fail(name: &'static str, message: &str, suggestion: &str) -> DoctorCheck {
    DoctorCheck {
        name,
        pass: false,
        message: message.to_string(),
        suggestion: Some(suggestion.to_string()),
    }
}

fn print_doctor_report(checks: &[DoctorCheck]) {
    for check in checks {
        if check.pass {
            println!("[{}] {}: {}", "PASS".green(), check.name, check.message);
        } else {
            println!("[{}] {}: {}", "FAIL".red(), check.name, check.message);
            if let Some(suggestion) = &check.suggestion {
                println!("        suggestion: {}", suggestion);
            }
        }
    }

    let total = checks.len();
    let passed = checks.iter().filter(|c| c.pass).count();
    let failed = total - passed;
    println!();
    if failed == 0 {
        println!("{}", format!("{passed}/{total} checks passed").green());
    } else {
        println!(
            "{}",
            format!("{passed}/{total} checks passed, {failed} failed").red()
        );
    }
}

async fn cmd_logs(
    config_path: &Path,
    data_dir_override: Option<PathBuf>,
    follow: bool,
) -> Result<(), DaemonError> {
    let config = DaemonConfig::load(config_path)?;
    let data_dir = resolve_data_dir(&config, data_dir_override);
    let log_path = data_dir.join("daemon.log");

    if is_daemon_lock_held(&data_dir) {
        eprintln!(
            "warning: the daemon is running; the log file may be incomplete or rotated while tailing"
        );
    }

    if !log_path.exists() {
        eprintln!("warning: log file does not exist: {}", log_path.display());
        return Ok(());
    }

    let file = tokio::fs::File::open(&log_path)
        .await
        .map_err(DaemonError::Io)?;
    let reader = tokio::io::BufReader::new(file);
    let mut lines = reader.lines();

    while let Some(line) = lines.next_line().await.map_err(DaemonError::Io)? {
        println!("{line}");
    }

    if follow {
        loop {
            match lines.next_line().await.map_err(DaemonError::Io)? {
                Some(line) => println!("{line}"),
                None => tokio::time::sleep(Duration::from_millis(250)).await,
            }
        }
    }

    Ok(())
}

async fn cmd_send_test_dm(
    config_path: &Path,
    data_dir_override: Option<PathBuf>,
    bot_id: &str,
    recipient: &str,
    content: &str,
) -> Result<(), DaemonError> {
    #[cfg(not(unix))]
    {
        let _ = (config_path, data_dir_override, bot_id, recipient, content);
        return Err(DaemonError::Config(
            "send-test-dm is only available on Unix platforms".into(),
        ));
    }

    #[cfg(unix)]
    {
        let config = DaemonConfig::load(config_path)?;
        let _bot = find_bot(&config.bots, bot_id)?;
        let data_dir = resolve_data_dir(&config, data_dir_override);
        let socket_path = data_dir.join("pacto-bot-api.sock");

        let request = JsonRpcMessage::request(
            1.into(),
            "admin.send_test_dm",
            Some(serde_json::json!({
                "bot_id": bot_id,
                "recipient": recipient,
                "content": content,
            })),
        );
        let value = with_admin_session(&socket_path, bot_id, request).await?;
        let response: AdminSendTestDmResponse = serde_json::from_value(value)?;
        println!("{}", response.event_id);
        Ok(())
    }
}

async fn cmd_trace_events(
    config_path: &Path,
    data_dir_override: Option<PathBuf>,
    bot_id: &str,
    since_minutes: i64,
    limit: usize,
) -> Result<(), DaemonError> {
    let config = DaemonConfig::load(config_path)?;
    let _bot = find_bot(&config.bots, bot_id)?;
    let data_dir = resolve_data_dir(&config, data_dir_override);
    let db_path = data_dir.join(AGENT_DB_FILE);
    if !db_path.exists() {
        return Err(DaemonError::Config(format!(
            "daemon database not found at {}",
            db_path.display()
        )));
    }

    let conn = open_agent_db(&db_path)?;
    let since = Utc::now() - chrono::Duration::minutes(since_minutes);
    let mut stmt = conn.prepare(
        "SELECT event_id, author, content_preview, action, reply_event_id, created_at
         FROM event_trace
         WHERE bot_id = ?1 AND created_at >= ?2
         ORDER BY created_at DESC, rowid DESC
         LIMIT ?3",
    )?;
    let rows = stmt.query_map((bot_id, since.timestamp(), limit as i64), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, i64>(5)?,
        ))
    })?;

    for row in rows {
        let (event_id, author, preview, action, reply_event_id, created_at) = row?;
        let when = DateTime::from_timestamp(created_at, 0)
            .unwrap_or_else(Utc::now)
            .to_rfc3339();
        let reply = reply_event_id.as_deref().unwrap_or("-");
        println!("{when} {event_id} {author} {action} reply_to={reply} preview={preview}");
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

    let lock_held = data_dir
        .as_ref()
        .map(|p| is_daemon_lock_held(p))
        .unwrap_or(false);

    let live_metrics: Option<MetricsResponse> = if let Some(socket) = &socket_path {
        #[cfg(unix)]
        {
            call_agent_metrics(socket).await.ok()
        }
        #[cfg(not(unix))]
        {
            None
        }
    } else {
        None
    };

    let live_handlers: Option<AgentListHandlersResponse> = if let (Some(socket), Some(bot_id)) = (
        &socket_path,
        config
            .as_ref()
            .and_then(|c| c.bots.first())
            .map(|b| b.id.as_str()),
    ) {
        #[cfg(unix)]
        {
            call_agent_list_handlers(socket, bot_id).await.ok()
        }
        #[cfg(not(unix))]
        {
            None
        }
    } else {
        None
    };

    let live_snapshot = data_dir.as_deref().and_then(read_latest_report);

    // `daemon_running` is derived from the daemon's lock file, which is the
    // same ground truth used by `pacto-bot-admin diagnose`. A live `agent.metrics`
    // response only proves the Unix socket is reachable, so we no longer use it
    // as the liveness signal; this avoids reporting `daemon: stopped` when the
    // socket is stale or inaccessible while the daemon is healthy and holding
    // the data-directory lock.
    let daemon_running = lock_held;
    let daemon_status = live_snapshot.as_ref().map(|s| daemon_status_str(s.status));
    let uptime_seconds = live_metrics
        .as_ref()
        .and_then(|m| m.uptime_seconds)
        .or_else(|| live_snapshot.as_ref().map(|s| s.uptime_seconds))
        .unwrap_or(0);
    let handlers_registered = live_metrics
        .as_ref()
        .and_then(|m| m.handlers_registered)
        .or_else(|| live_snapshot.as_ref().map(|s| s.handlers_registered))
        .unwrap_or(0);

    let handlers: Vec<HandlerStatus> = live_handlers
        .map(|r| {
            r.handlers
                .into_iter()
                .map(|h| HandlerStatus {
                    handler_id: h.handler_id,
                    bot_ids: h.bot_ids,
                    event_types: h.event_types,
                    capabilities: h.capabilities,
                    connected: h.connected,
                    state: h.state,
                    transport: h.transport,
                    last_seen: h.last_seen,
                    registered_at: h.registered_at,
                })
                .collect()
        })
        .unwrap_or_default();

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
        handlers,
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
    if let Ok(contents) = std::fs::read_to_string(&path)
        && let Ok(snapshot) = serde_json::from_str::<HealthSnapshot>(&contents)
    {
        return Some(snapshot);
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
    if exists && let Ok(meta) = std::fs::metadata(path) {
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

#[cfg(unix)]
async fn call_agent_list_handlers(
    socket_path: &Path,
    bot_id: &str,
) -> Result<AgentListHandlersResponse, DaemonError> {
    let request = JsonRpcMessage::request(
        2.into(),
        "agent.list_handlers",
        Some(Value::Object(Default::default())),
    );
    let value = with_admin_session(socket_path, bot_id, request).await?;
    let response = serde_json::from_value(value)?;
    Ok(response)
}

#[cfg(unix)]
async fn call_agent_unregister_handler(
    socket_path: &Path,
    bot_id: &str,
    handler_id: &str,
) -> Result<AgentUnregisterHandlerResponse, DaemonError> {
    let request = JsonRpcMessage::request(
        2.into(),
        "agent.unregister_handler",
        Some(serde_json::json!({"handler_id": handler_id})),
    );
    let value = with_admin_session(socket_path, bot_id, request).await?;
    let response = serde_json::from_value(value)?;
    Ok(response)
}

/// Open a Unix socket connection, register an admin handler, send the provided
/// request, and clean up the temporary handler. The same connection is used
/// for all messages so the Unix transport associates the admin identity with
/// the subsequent admin request.
#[cfg(unix)]
async fn with_admin_session(
    socket_path: &Path,
    bot_id: &str,
    request: JsonRpcMessage,
) -> Result<Value, DaemonError> {
    let stream = tokio::time::timeout(Duration::from_secs(15), UnixStream::connect(socket_path))
        .await
        .map_err(|_| DaemonError::Config("unix socket connect timed out".into()))??;
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Register a temporary handler with Admin capability.
    let register = JsonRpcMessage::request(
        1.into(),
        "handler.register",
        Some(serde_json::json!({
            "bot_ids": [bot_id],
            "event_types": [],
            "capabilities": ["Admin"],
        })),
    );
    write_request(&mut writer, &register).await?;
    let response = read_response(&mut reader, Duration::from_secs(5)).await?;
    let admin_handler_id = match response {
        JsonRpcMessage::Response {
            result: Some(r), ..
        } => {
            let parsed: HandlerRegisterResponse = serde_json::from_value(r)?;
            parsed.handler_id
        }
        JsonRpcMessage::Error { error, .. } => {
            return Err(DaemonError::Config(format!(
                "admin handler registration failed: {}",
                error.message
            )));
        }
        _ => return Err(DaemonError::Config("unexpected daemon response".into())),
    };

    // Send the actual admin request on the same authenticated connection.
    write_request(&mut writer, &request).await?;
    let result = read_response(&mut reader, Duration::from_secs(15)).await?;

    // Best-effort cleanup of the temporary admin handler.
    let unregister = JsonRpcMessage::request(
        3.into(),
        "handler.unregister",
        Some(Value::Object(Default::default())),
    );
    let _ = write_request(&mut writer, &unregister).await;
    let _ = read_response(&mut reader, Duration::from_secs(2)).await;

    match result {
        JsonRpcMessage::Response {
            result: Some(value),
            ..
        } => Ok(value),
        JsonRpcMessage::Response { result: None, .. } => {
            Err(DaemonError::Config("empty daemon response".into()))
        }
        JsonRpcMessage::Error { error, .. } => {
            // If the call was unauthorized, surface a clearer message.
            if error.code == -32006 || error.code == -32007 || error.code == -32008 {
                Err(DaemonError::Config(format!(
                    "admin handler {} is not authorized for this operation",
                    admin_handler_id
                )))
            } else {
                Err(DaemonError::Config(format!(
                    "daemon error: {}",
                    error.message
                )))
            }
        }
        _ => Err(DaemonError::Config("unexpected daemon response".into())),
    }
}

#[cfg(unix)]
async fn write_request(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    request: &JsonRpcMessage,
) -> Result<(), DaemonError> {
    let line = format!("{}\n", serialize_message(request)?);
    writer.write_all(line.as_bytes()).await?;
    Ok(())
}

#[cfg(unix)]
async fn read_response(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    timeout_duration: Duration,
) -> Result<JsonRpcMessage, DaemonError> {
    let mut buf = Vec::new();
    let n = tokio::time::timeout(timeout_duration, reader.read_until(b'\n', &mut buf))
        .await
        .map_err(|_| DaemonError::Config("unix socket read timed out".into()))??;
    if n == 0 {
        return Err(DaemonError::Config("unix socket closed".into()));
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    let line = String::from_utf8(buf)
        .map_err(|_| DaemonError::Config("daemon response is not valid UTF-8".into()))?;
    parse_message(&line)
}

#[cfg(unix)]
async fn cmd_handlers(
    config_path: &Path,
    data_dir_override: Option<PathBuf>,
    sub: &HandlersCommand,
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

    let socket_path = socket_path
        .ok_or_else(|| DaemonError::Config("unable to determine daemon socket path".into()))?;

    let bot_id = config
        .as_ref()
        .and_then(|c| c.bots.first())
        .map(|b| b.id.clone())
        .ok_or_else(|| {
            DaemonError::Config("no bots configured; cannot create admin session".into())
        })?;

    let response = call_agent_list_handlers(&socket_path, &bot_id).await?;

    match sub {
        HandlersCommand::List => {
            println!("{}", serde_json::to_string_pretty(&response)?);
        }
        HandlersCommand::Show { handler_id } => {
            let found = response
                .handlers
                .into_iter()
                .find(|h| h.handler_id == *handler_id)
                .ok_or_else(|| DaemonError::HandlerNotRegistered)?;
            println!("{}", serde_json::to_string_pretty(&found)?);
        }
        HandlersCommand::Unregister { handler_id } => {
            let result = call_agent_unregister_handler(&socket_path, &bot_id, handler_id).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
    }

    Ok(())
}

#[cfg(not(unix))]
async fn cmd_handlers(
    _config_path: &Path,
    _data_dir_override: Option<PathBuf>,
    _sub: &HandlersCommand,
) -> Result<(), DaemonError> {
    Err(DaemonError::Config(
        "handler management is only available on Unix platforms".into(),
    ))
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
            && let Some(port) = extract_port_from_url(uri.expose_secret())
        {
            return Some(port);
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
    if let Some(rest) = input.strip_prefix("~/")
        && let Ok(home) = env::var("HOME")
    {
        return PathBuf::from(format!("{home}/{rest}"));
    }
    PathBuf::from(input)
}

fn is_daemon_lock_held(data_dir: &Path) -> bool {
    let path = data_dir.join(DAEMON_LOCK_FILE);
    let pid = match fs::read_to_string(&path)
        .ok()
        .and_then(|contents| contents.trim().parse::<u32>().ok())
    {
        Some(pid) => pid,
        None => return false,
    };

    if !process_exists(pid) {
        return false;
    }

    // Confirm the lock file is still exclusively locked. A stale PID from a
    // crashed daemon will not hold the lock.
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(false)
        .open(&path)
    {
        Ok(f) => f,
        Err(_) => return false,
    };
    file.try_lock_exclusive().is_err()
}

/// Best-effort check that a process with the given PID is still running.
#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    // Send signal 0 (no signal) to test liveness. `kill` returns ESRCH when the
    // PID does not exist and EPERM when it exists but we lack permission; both
    // cases mean a daemon is using the lock, so EPERM is treated as alive.
    match kill(Pid::from_raw(pid as i32), None::<Signal>) {
        Ok(()) => true,
        Err(e) => matches!(e as Errno, Errno::EPERM),
    }
}

#[cfg(not(unix))]
fn process_exists(_pid: u32) -> bool {
    // No portable process-liveness check on this platform; rely on the lock.
    true
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
    let name = bot.display_name.as_deref().unwrap_or(&bot.id);
    let mut profile = json!({
        "name": name,
        "bot": true,
        "capabilities": bot.capabilities,
    });
    if let Some(about) = &bot.about {
        profile["about"] = about.clone().into();
    }
    if let Some(picture) = &bot.picture {
        profile["picture"] = picture.clone().into();
    }
    let content = serde_json::to_string(&profile)?;

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
    reconnect_token: String,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    recent_counts: Option<RecentCountsReport>,
    bot_cursors: Vec<BotCursorDiagnosis>,
    handler_count: i64,
    errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct BotCursorDiagnosis {
    bot_id: String,
    cursor: i64,
}

#[derive(Debug, Clone, Serialize)]
struct RecentCountsReport {
    events_received: u64,
    events_dispatched: u64,
    replies: u64,
    reply_send_failed: u64,
    window_minutes: u64,
}

#[derive(Debug, Clone, Serialize)]
struct StatusReport {
    daemon_running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    daemon_status: Option<String>,
    uptime_seconds: u64,
    handlers_registered: u64,
    handlers: Vec<HandlerStatus>,
    bots: Vec<BotStatus>,
}

#[derive(Debug, Clone, Serialize)]
struct HandlerStatus {
    handler_id: String,
    bot_ids: Vec<String>,
    event_types: Vec<String>,
    capabilities: Vec<String>,
    connected: bool,
    state: String,
    transport: String,
    last_seen: String,
    registered_at: String,
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
            reconnect_token TEXT NOT NULL,
            registered_at INTEGER
        );
        CREATE TABLE IF NOT EXISTS event_trace (
            bot_id TEXT NOT NULL,
            event_id TEXT NOT NULL,
            author TEXT,
            content_preview TEXT,
            action TEXT NOT NULL,
            reply_event_id TEXT,
            created_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_event_trace_bot_created
            ON event_trace (bot_id, created_at DESC);",
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
        "SELECT handler_id, bot_ids, event_types, capabilities, reconnect_token, registered_at FROM handlers",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, i64>(5)?,
        ))
    })?;

    let mut handlers = Vec::new();
    for row in rows {
        let (
            id,
            bot_ids_json,
            event_types_json,
            capabilities_json,
            reconnect_token,
            registered_at_ts,
        ) = row?;
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
                reconnect_token,
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
        "INSERT INTO handlers (handler_id, bot_ids, event_types, capabilities, reconnect_token, registered_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(handler_id) DO UPDATE SET
            bot_ids = excluded.bot_ids,
            event_types = excluded.event_types,
            capabilities = excluded.capabilities,
            reconnect_token = excluded.reconnect_token,
            registered_at = excluded.registered_at",
        (
            &handler.handler_id,
            bot_ids,
            event_types,
            capabilities,
            &handler.reconnect_token,
            registered_at,
        ),
    )?;
    Ok(())
}

fn count_cursors(conn: &Connection) -> Result<i64, DaemonError> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM cursors", [], |row| row.get(0))?;
    Ok(count)
}

fn count_handlers(conn: &Connection) -> Result<i64, DaemonError> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM handlers", [], |row| row.get(0))?;
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
    let mut write = |s: &str| writeln!(out, "{s}").map_err(DaemonError::Io);

    write(&format!("config_valid: {}", report.config_valid))?;
    write(&format!("lock_held: {}", report.lock_held))?;
    if let Some(status) = &report.daemon_status {
        write(&format!("daemon_status: {status}"))?;
    }
    write(&format!("data_dir: {}", report.data_dir))?;
    write("")?;

    write("socket:")?;
    write(&format!("  path: {}", report.socket.path))?;
    write(&format!("  exists: {}", report.socket.exists))?;
    write(&format!(
        "  owner_readable: {}",
        report.socket.owner_readable
    ))?;
    write(&format!(
        "  owner_writable: {}",
        report.socket.owner_writable
    ))?;
    if let Some(mode) = report.socket.mode {
        write(&format!("  mode: 0o{mode:o}"))?;
    }
    write("")?;

    write("bots:")?;
    if report.bots.is_empty() {
        write("  (none)")?;
    } else {
        for bot in &report.bots {
            write(&format!("  - id: {}", bot.id))?;
            write(&format!("    npub: {}", bot.npub))?;
            write(&format!("    signing_backend: {}", bot.signing_backend))?;
            write(&format!("    relays: {}", bot.relay_count))?;
            if let Some(connected) = bot.live_bunker_connected {
                write(&format!("    live_bunker_connected: {connected}"))?;
            }
        }
    }
    write("")?;

    write("relay_connectivity:")?;
    if report.relay_connectivity.is_empty() {
        write("  (none)")?;
    } else {
        for check in &report.relay_connectivity {
            write(&format!("  - bot_id: {}", check.bot_id))?;
            write(&format!("    relay: {}", check.relay))?;
            write(&format!("    reachable: {}", check.reachable))?;
        }
    }
    write("")?;

    write("bunker_connectivity:")?;
    if report.bunker_connectivity.is_empty() {
        write("  (none)")?;
    } else {
        for check in &report.bunker_connectivity {
            write(&format!("  - bot_id: {}", check.bot_id))?;
            write(&format!("    reachable: {}", check.reachable))?;
        }
    }
    write("")?;

    if is_pacto_dev_env() {
        write("service_versions:")?;
        if let Some(relay) = &report.service_versions.relay {
            write("  relay:")?;
            write(&format!("    url: {}", relay.url))?;
            write(&format!("    reachable: {}", relay.reachable))?;
            if let Some(version) = &relay.version {
                write(&format!("    version: {version}"))?;
            }
            if let Some(error) = &relay.error {
                write(&format!("    error: {error}"))?;
            }
        }
        if let Some(evm) = &report.service_versions.evm_node {
            write("  evm_node:")?;
            write(&format!("    url: {}", evm.url))?;
            write(&format!("    reachable: {}", evm.reachable))?;
            if let Some(version) = &evm.version {
                write(&format!("    version: {version}"))?;
            }
            if let Some(error) = &evm.error {
                write(&format!("    error: {error}"))?;
            }
        }
        if let Some(bunker) = &report.service_versions.bunker_port {
            write("  bunker_port:")?;
            write(&format!("    url: {}", bunker.url))?;
            write(&format!("    reachable: {}", bunker.reachable))?;
            if let Some(version) = &bunker.version {
                write(&format!("    version: {version}"))?;
            }
            if let Some(error) = &bunker.error {
                write(&format!("    error: {error}"))?;
            }
        }
        write("")?;
    }

    write(&format!("db_cursor_count: {}", report.db_cursor_count))?;
    write(&format!("handler_count: {}", report.handler_count))?;

    write("")?;
    write("bot_cursors:")?;
    if report.bot_cursors.is_empty() {
        write("  (none)")?;
    } else {
        for cursor in &report.bot_cursors {
            write(&format!("  - bot_id: {}", cursor.bot_id))?;
            write(&format!("    cursor: {}", cursor.cursor))?;
        }
    }
    write("")?;

    write("recent_counts:")?;
    if let Some(counts) = &report.recent_counts {
        write(&format!("  window_minutes: {}", counts.window_minutes))?;
        write(&format!("  events_received: {}", counts.events_received))?;
        write(&format!(
            "  events_dispatched: {}",
            counts.events_dispatched
        ))?;
        write(&format!("  replies: {}", counts.replies))?;
        write(&format!(
            "  reply_send_failed: {}",
            counts.reply_send_failed
        ))?;
    } else {
        write("  (no live daemon metrics)")?;
    }
    write("")?;

    write("relay_reachability:")?;
    if report.relay_connectivity.is_empty() {
        write("  (none)")?;
    } else {
        for check in &report.relay_connectivity {
            write(&format!("  - bot_id: {}", check.bot_id))?;
            write(&format!("    relay: {}", check.relay))?;
            if check.reachable {
                write("    status: reachable")?;
            } else if let Some(error) = &check.error {
                write(&format!("    status: unreachable ({error})"))?;
            } else {
                write("    status: unreachable")?;
            }
        }
    }
    write("")?;

    if !report.errors.is_empty() {
        write("errors:")?;
        for err in &report.errors {
            write(&format!("  - {err}"))?;
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

    if !report.handlers.is_empty() {
        writeln!(out, "\nhandlers:").map_err(DaemonError::Io)?;
        for h in &report.handlers {
            writeln!(out, "  - handler_id: {}", h.handler_id).map_err(DaemonError::Io)?;
            writeln!(out, "    state: {}", h.state).map_err(DaemonError::Io)?;
            writeln!(out, "    transport: {}", h.transport).map_err(DaemonError::Io)?;
            writeln!(out, "    bot_ids: {:?}", h.bot_ids).map_err(DaemonError::Io)?;
            writeln!(out, "    event_types: {:?}", h.event_types).map_err(DaemonError::Io)?;
            writeln!(out, "    capabilities: {:?}", h.capabilities).map_err(DaemonError::Io)?;
            writeln!(out, "    last_seen: {}", h.last_seen).map_err(DaemonError::Io)?;
        }
    }

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
            display_name: None,
            about: None,
            picture: None,
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
    fn daemon_lock_detected_by_live_pid_and_lock() -> Result<(), DaemonError> {
        let dir = tempfile::tempdir().map_err(DaemonError::Io)?;
        assert!(!is_daemon_lock_held(dir.path()));

        let path = dir.path().join(DAEMON_LOCK_FILE);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(DaemonError::Io)?;
        file.lock_exclusive().map_err(DaemonError::Io)?;
        file.write_all(format!("{}\n", std::process::id()).as_bytes())
            .map_err(DaemonError::Io)?;
        file.flush().map_err(DaemonError::Io)?;
        assert!(is_daemon_lock_held(dir.path()));

        drop(file);
        assert!(!is_daemon_lock_held(dir.path()));

        // A stale PID with no lock should also report not held.
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(DaemonError::Io)?;
        file.write_all(b"9999999\n").map_err(DaemonError::Io)?;
        file.flush().map_err(DaemonError::Io)?;
        assert!(!is_daemon_lock_held(dir.path()));
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
            reconnect_token: "deadbeef".to_string(),
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

    #[tokio::test]
    async fn build_profile_event_uses_optional_fields() -> Result<(), DaemonError> {
        let (signer, nsec, npub) = nsec_signer()?;
        let mut bot = dummy_bot("profile-bot", &npub, &nsec);
        bot.display_name = Some("Profile Bot".to_string());
        bot.about = Some("A test bot".to_string());
        bot.picture = Some("https://example.com/bot.png".to_string());
        let event = build_profile_event_with_signer(&bot, &signer).await?;

        let parsed: serde_json::Value = serde_json::from_str(&event.content)?;
        assert_eq!(parsed["name"], "Profile Bot");
        assert_eq!(parsed["about"], "A test bot");
        assert_eq!(parsed["picture"], "https://example.com/bot.png");
        Ok(())
    }

    #[tokio::test]
    async fn new_rejects_empty_bot_id() {
        let err = cmd_new(
            Some(""),
            "nsec",
            &[],
            &[],
            None,
            false,
            "python",
            "llm",
            &[],
            false,
            false,
            false,
            None,
            None,
            "",
            None,
            false,
            false,
            false,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("bot_id"));
    }

    #[tokio::test]
    async fn new_rejects_unknown_backend() {
        let err = cmd_new(
            Some("x"),
            "invalid",
            &[],
            &[],
            None,
            false,
            "python",
            "llm",
            &[],
            false,
            false,
            false,
            None,
            None,
            "",
            None,
            false,
            false,
            false,
        )
        .await
        .unwrap_err();
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
