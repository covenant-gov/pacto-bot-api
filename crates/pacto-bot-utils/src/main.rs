//! Headless utility for creating or re-opening an MLS group and inviting a bot.

use crate::cli::Cli;
use crate::mls::MlsManager;
use crate::relay::RelayClient;
use crate::state::GroupState;
use crate::welcome::gift_wrap_welcome;
use anyhow::{Context, Result};
use clap::Parser;
use nostr::ToBech32;
use std::time::Duration;
use tracing::{error, info};

mod cli;
mod mls;
mod relay;
mod state;
mod welcome;

#[tokio::main]
async fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    if let Err(e) = run().await {
        error!(error = %e, "create-mls-group failed");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let creator_keys = cli
        .load_creator_keys()
        .context("failed to load creator nsec")?;
    let creator_npub = creator_keys
        .public_key()
        .to_bech32()
        .context("failed to encode creator npub")?;
    let bot_npub = cli
        .bot_npub
        .to_bech32()
        .context("failed to encode bot npub")?;

    let mut state = state::load(&cli.state_file)
        .with_context(|| format!("failed to load state file {:?}", cli.state_file))?;

    // If the group exists and the bot is already invited, just print the wire
    // id without touching the relay or MLS engine.
    if let Some(group) = state.groups.get(&cli.group_name) {
        if group.creator_npub != creator_npub {
            return Err(anyhow::anyhow!(
                "group '{}' was created by {}; supplied nsec derives {}",
                cli.group_name,
                group.creator_npub,
                creator_npub
            ));
        }

        if group.invited_bots.contains(&bot_npub) {
            println!("{}", group.group_id);
            return Ok(());
        }

        info!(
            group_name = %cli.group_name,
            wire_id = %group.group_id,
            "re-opening existing group to invite new bot"
        );
        let manager = MlsManager::new(&cli.mls_db)
            .with_context(|| format!("failed to open MLS database {:?}", cli.mls_db))?;
        let relay = RelayClient::new(&cli.relay)
            .await
            .with_context(|| format!("failed to connect to relay {}", cli.relay))?;

        let key_package = relay
            .fetch_key_package(&cli.bot_npub, Duration::from_secs(cli.key_package_timeout))
            .await
            .context("failed to fetch bot KeyPackage")?;

        let addition = manager
            .add_member(&group.group_id, &key_package)
            .with_context(|| {
                format!("failed to add bot {} to group {}", bot_npub, group.group_id)
            })?;

        if let Some(evolution) = addition.evolution_event {
            relay
                .publish(&evolution)
                .await
                .context("failed to publish group evolution event")?;
        }

        let welcome = gift_wrap_welcome(&creator_keys, &cli.bot_npub, addition.welcome_rumor)
            .await
            .context("failed to gift-wrap welcome")?;
        relay
            .publish(&welcome)
            .await
            .context("failed to publish welcome gift-wrap")?;

        let mut updated = group.clone();
        updated.invited_bots.push(bot_npub);
        updated.relay = cli.relay.clone();
        state.groups.insert(cli.group_name.clone(), updated);
    } else {
        info!(
            group_name = %cli.group_name,
            relay = %cli.relay,
            "creating new MLS group"
        );
        let manager = MlsManager::new(&cli.mls_db)
            .with_context(|| format!("failed to open MLS database {:?}", cli.mls_db))?;
        let relay = RelayClient::new(&cli.relay)
            .await
            .with_context(|| format!("failed to connect to relay {}", cli.relay))?;

        let key_package = relay
            .fetch_key_package(&cli.bot_npub, Duration::from_secs(cli.key_package_timeout))
            .await
            .context("failed to fetch bot KeyPackage")?;

        let created = manager
            .create_group(&creator_keys, &key_package, &cli.group_name)
            .context("failed to create MLS group")?;

        let welcome = gift_wrap_welcome(&creator_keys, &cli.bot_npub, created.welcome_rumor)
            .await
            .context("failed to gift-wrap welcome")?;
        relay
            .publish(&welcome)
            .await
            .context("failed to publish welcome gift-wrap")?;

        state.groups.insert(
            cli.group_name.clone(),
            GroupState {
                group_id: created.wire_id,
                creator_npub,
                relay: cli.relay.clone(),
                invited_bots: vec![bot_npub],
            },
        );
    }

    state::save(&cli.state_file, &state)
        .with_context(|| format!("failed to save state file {:?}", cli.state_file))?;

    let wire_id = state
        .groups
        .get(&cli.group_name)
        .map(|g| g.group_id.clone())
        .context("group missing from state after update")?;
    println!("{}", wire_id);
    Ok(())
}
