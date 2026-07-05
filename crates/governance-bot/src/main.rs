//! Example governance snapshot bot for `pacto-bot-api`.
//!
//! Reads on-chain governance state from a Pacto squad and posts encrypted MLS
//! group-message snapshots to a Squad channel on a configurable cadence.
//!
//! # Quick start
//!
//! 1. Create a bot identity with `pacto-bot-admin new --scaffold`.
//! 2. Configure the environment variables listed in [`config::BotConfig`].
//! 3. Run the daemon.
//! 4. Run this binary.
//!
//! The handler connects to the daemon over Unix socket or HTTP, registers with
//! the `SendGroupMessages` capability, and posts snapshots autonomously.

use governance_bot::bot::SnapshotBot;
use governance_bot::config::BotConfig;
use tracing::{error, info};

#[tokio::main]
async fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let config = match BotConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "failed to load configuration");
            std::process::exit(1);
        }
    };

    info!(
        bot_id = %config.bot_id,
        group_id = %config.group_id,
        cadence_seconds = config.cadence_seconds,
        "starting governance snapshot bot"
    );

    let mut bot = match SnapshotBot::new(config) {
        Ok(b) => b,
        Err(e) => {
            error!(error = %e, "failed to initialize bot");
            std::process::exit(1);
        }
    };

    if let Err(e) = bot.setup().await {
        error!(error = %e, "bot setup failed");
        std::process::exit(1);
    }

    if let Err(e) = bot.run().await {
        error!(error = %e, "bot run loop exited");
        std::process::exit(1);
    }
}
