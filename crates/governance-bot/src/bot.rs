//! Snapshot bot orchestration: cadence timer, governance reads, formatting,
//! and daemon dispatch with retry/backoff.

use std::time::Duration;

use alloy::providers::{Provider, ProviderBuilder};
use tokio::time::{Interval, MissedTickBehavior, interval};
use tracing::{error, info, warn};

use crate::config::BotConfig;
use crate::daemon_client::{DaemonClient, DaemonClientError};
use crate::evm::addresses::{hats_address, registry_address};
use crate::evm::reader::GovernanceReader;
use crate::snapshot::format::format_snapshot;

/// Errors that can occur during a single snapshot posting attempt.
#[derive(Debug, thiserror::Error)]
pub enum BotError {
    /// Governance read failed.
    #[error("governance read failed: {0}")]
    Governance(String),
    /// Daemon call failed.
    #[error("daemon call failed: {0}")]
    Daemon(#[from] DaemonClientError),
    /// Other operational error.
    #[error("operational error: {0}")]
    Other(String),
}

/// Backoff state for a single failing operation.
#[derive(Debug, Clone, Default)]
pub struct Backoff {
    step: u32,
}

impl Backoff {
    /// Return the next sleep duration and advance the step.
    pub fn next_delay(&mut self) -> Duration {
        const BASE: Duration = Duration::from_secs(5);
        const MAX: Duration = Duration::from_secs(300);
        let duration = (BASE * 2u32.pow(self.step.min(6))).min(MAX);
        self.step = self.step.saturating_add(1);
        duration
    }

    /// Reset after a successful operation.
    pub fn reset(&mut self) {
        self.step = 0;
    }
}

/// The running snapshot bot.
#[derive(Debug, Clone)]
pub struct SnapshotBot {
    config: BotConfig,
    daemon: DaemonClient,
    backoff: Backoff,
}

impl SnapshotBot {
    /// Create a bot from configuration without yet connecting.
    pub fn new(config: BotConfig) -> Result<Self, BotError> {
        let daemon = config_to_daemon_client(&config)?;
        Ok(Self {
            config,
            daemon,
            backoff: Backoff::default(),
        })
    }

    /// Register the handler with the daemon and publish the bot's KeyPackage.
    ///
    /// The KeyPackage is only published once at startup; repeated calls will
    /// produce duplicate key packages on the relay, but the daemon does not
    /// prevent this. The caller should run this once per process lifetime.
    pub async fn setup(&self) -> Result<(), BotError> {
        let registration = self
            .daemon
            .handler_register(&self.config.bot_id, &["SendGroupMessages"])
            .await?;
        info!(
            handler_id = %registration.handler_id,
            "registered handler with daemon"
        );

        match self.daemon.publish_key_package(&self.config.bot_id).await {
            Ok(event_id) => {
                info!(event_id, "published key package");
            }
            Err(DaemonClientError::JsonRpc { code, message }) => {
                warn!(
                    code,
                    message, "key package publish returned error; continuing"
                );
            }
            Err(e) => return Err(e.into()),
        }

        Ok(())
    }

    /// Build a governance reader from the configured RPC endpoint.
    pub fn reader(&self) -> Result<GovernanceReader<impl Provider>, BotError> {
        let provider = ProviderBuilder::new().connect_http(
            self.config
                .rpc_url
                .parse()
                .map_err(|e| BotError::Other(format!("invalid RPC URL: {e}")))?,
        );
        Ok(
            GovernanceReader::new(provider, registry_address(), hats_address())
                .with_known_tokens(self.config.known_tokens.clone()),
        )
    }

    /// Read governance state, format it, and post it to the Squad channel.
    ///
    /// Returns the published event id on success.
    pub async fn post_snapshot(&self) -> Result<String, BotError> {
        let reader = self.reader()?;

        let snapshot = reader
            .snapshot(
                self.config.squad_index,
                self.config.captain,
                &self.config.crew_candidates,
                &self.config.proposer_candidates,
            )
            .await
            .map_err(|e| BotError::Governance(e.to_string()))?;

        let markdown = format_snapshot(snapshot);
        let event_id = self
            .daemon
            .send_group_message(&self.config.bot_id, &self.config.group_id, &markdown)
            .await?;
        Ok(event_id)
    }

    /// Run the cadence loop until the process is interrupted.
    ///
    /// On failure, the bot sleeps with exponential backoff and then retries
    /// on the next cadence tick. There is no human-paste fallback.
    pub async fn run(&mut self) -> Result<(), BotError> {
        let mut timer = cadence_timer(self.config.cadence_seconds);

        loop {
            timer.tick().await;
            info!("cadence tick: posting snapshot");

            match self.post_snapshot().await {
                Ok(event_id) => {
                    info!(event_id, "posted snapshot");
                    self.backoff.reset();
                }
                Err(e) => {
                    let wait = self.backoff.next_delay();
                    error!(error = %e, "failed to post snapshot; retrying in {wait:?}");
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }
}

fn config_to_daemon_client(config: &BotConfig) -> Result<DaemonClient, BotError> {
    if let Some(socket) = &config.daemon_socket {
        #[cfg(unix)]
        {
            return Ok(DaemonClient::unix(socket));
        }
        #[cfg(not(unix))]
        {
            return Err(BotError::Other(
                "Unix socket transport is not supported on this platform".into(),
            ));
        }
    }

    match (&config.daemon_http, &config.http_secret) {
        (Some(url), Some(secret)) => Ok(DaemonClient::http(url, secret)),
        _ => Err(BotError::Other(
            "HTTP transport requires both URL and secret".into(),
        )),
    }
}

fn cadence_timer(seconds: u64) -> Interval {
    let mut timer = interval(Duration::from_secs(seconds));
    timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    timer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_then_caps() {
        let mut b = Backoff::default();
        assert_eq!(b.next_delay(), Duration::from_secs(5));
        assert_eq!(b.next_delay(), Duration::from_secs(10));
        assert_eq!(b.next_delay(), Duration::from_secs(20));
        assert_eq!(b.next_delay(), Duration::from_secs(40));
        assert_eq!(b.next_delay(), Duration::from_secs(80));
        assert_eq!(b.next_delay(), Duration::from_secs(160));
        assert_eq!(b.next_delay(), Duration::from_secs(300));
        assert_eq!(b.next_delay(), Duration::from_secs(300));
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_secs(5));
    }

    #[tokio::test]
    async fn cadence_timer_respects_configured_seconds() {
        let timer = cadence_timer(123);
        assert_eq!(timer.period(), Duration::from_secs(123));
    }
}
