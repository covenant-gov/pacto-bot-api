use clap::Parser;
use pacto_bot_api::config::DaemonConfig;
use std::path::PathBuf;
use std::process;
use tracing::{info, warn};

#[derive(Parser, Debug)]
#[command(name = "pacto-bot-api")]
#[command(about = "Pacto bot API daemon")]
struct Cli {
    /// Path to the bot configuration file.
    #[arg(short, long, value_name = "PATH", default_value = "pacto-bot-api.toml")]
    config: PathBuf,

    /// Directory for runtime data (database, socket, reports).
    #[arg(short, long, value_name = "DIR")]
    data_dir: Option<PathBuf>,

    /// Enable the optional localhost HTTP transport.
    #[arg(long)]
    enable_http: bool,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    info!(
        config = %cli.config.display(),
        enable_http = cli.enable_http,
        "starting pacto-bot-api daemon"
    );

    let config = match DaemonConfig::load(&cli.config) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("failed to load config: {}", e);
            process::exit(1);
        }
    };

    let data_dir = cli
        .data_dir
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| config.data_dir().to_string());

    if cli.enable_http {
        warn!("localhost HTTP transport is enabled; ensure the secret token is protected");
    }

    if config
        .bots
        .iter()
        .any(|b| matches!(b.signing, pacto_bot_api::config::SigningConfig::Nsec { .. }))
    {
        warn!("local test key (nsec) in use — not for production");
    }

    info!(
        data_dir = %data_dir,
        socket_path = %config.socket_path(),
        bots = config.bots.len(),
        "pacto-bot-api daemon initialized (placeholder); exiting cleanly"
    );

    // Placeholder: real daemon event loop will run here in later implementation units.
}
