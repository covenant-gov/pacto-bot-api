//! Relay client for the `create-mls-group` tool.
//!
//! Provides a thin wrapper around [`nostr_sdk::Client`] for the two operations
//! the tool needs: waiting for a bot's `kind:443` KeyPackage and publishing a
//! Nostr event to a single relay.

use std::time::Duration;

use nostr::{Event, Kind, PublicKey};
use nostr_sdk::{Client, RelayPoolNotification, client::Error as NostrSdkError};
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, error};

/// Errors that can occur when talking to a Nostr relay.
#[derive(Debug, Error)]
pub enum RelayError {
    /// The client could not add or connect to the requested relay URL.
    #[error("failed to connect to relay at {url}: {source}")]
    Connect {
        url: String,
        #[source]
        source: NostrSdkError,
    },

    /// No matching KeyPackage arrived before the timeout.
    #[error("timed out after {0:?} waiting for KeyPackage from {1}")]
    Timeout(Duration, PublicKey),

    /// The subscription request failed.
    #[error("failed to subscribe to relay: {source}")]
    Subscribe {
        #[source]
        source: NostrSdkError,
    },

    /// Publishing an event failed.
    #[error("failed to publish event: {source}")]
    Publish {
        #[source]
        source: NostrSdkError,
    },
}

/// A short-lived client connected to a single Nostr relay.
#[derive(Debug, Clone)]
pub struct RelayClient {
    client: Client,
}

impl RelayClient {
    /// Connect to the given relay URL.
    ///
    /// The connection is started asynchronously; subscription and publish calls
    /// will queue until the relay is reachable.
    pub async fn new(relay_url: &str) -> Result<Self, RelayError> {
        let client = Client::default();
        client
            .add_relay(relay_url)
            .await
            .map_err(|e| RelayError::Connect {
                url: relay_url.to_string(),
                source: e,
            })?;
        client.connect().await;
        debug!(relay_url, "connected to relay");
        Ok(Self { client })
    }

    /// Subscribe to `kind:443` KeyPackage events authored by `bot` and return
    /// the first matching event, or [`RelayError::Timeout`] if none arrives
    /// within `timeout`.
    pub async fn fetch_key_package(
        &self,
        bot: &PublicKey,
        timeout: Duration,
    ) -> Result<Event, RelayError> {
        let filter = nostr::Filter::new().kind(Kind::MlsKeyPackage).author(*bot);

        let subscription_id = self
            .client
            .subscribe(filter, None)
            .await
            .map_err(|e| RelayError::Subscribe { source: e })?
            .val;

        let (tx, mut rx) = mpsc::channel::<Event>(1);
        let recv = self.client.handle_notifications(|notification| {
            let tx = tx.clone();
            async move {
                if let RelayPoolNotification::Event { event, .. } = notification {
                    let event = *event;
                    if event.kind == Kind::MlsKeyPackage && event.pubkey == *bot {
                        let _ = tx.try_send(event);
                        return Ok(true);
                    }
                }
                Ok(false)
            }
        });

        let result = tokio::time::timeout(timeout, recv).await;
        self.client.unsubscribe(&subscription_id).await;

        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                error!(error = %e, "relay notification handler failed");
                return Err(RelayError::Subscribe { source: e });
            }
            Err(_) => {
                debug!(bot = %bot, ?timeout, "KeyPackage fetch timed out");
                return Err(RelayError::Timeout(timeout, *bot));
            }
        }

        rx.recv().await.ok_or(RelayError::Timeout(timeout, *bot))
    }

    /// Publish the given event to the relay.
    pub async fn publish(&self, event: &Event) -> Result<(), RelayError> {
        self.client
            .send_event(event)
            .await
            .map(|_| ())
            .map_err(|e| RelayError::Publish { source: e })
    }
}

#[cfg(test)]
#[path = "../../../tests/support/mock_relay.rs"]
mod mock_relay;

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{EventBuilder, Keys};

    #[tokio::test]
    async fn fetches_key_package_from_relay() -> Result<(), Box<dyn std::error::Error>> {
        let relay = mock_relay::MockRelay::start().await?;
        let bot = Keys::generate();
        let event = EventBuilder::new(Kind::MlsKeyPackage, "test-key-package")
            .sign(&bot)
            .await?;
        relay.inject_event(event.clone()).await;

        let client = RelayClient::new(&relay.url()).await?;
        let fetched = client
            .fetch_key_package(&bot.public_key(), Duration::from_secs(5))
            .await?;

        assert_eq!(fetched.id, event.id);
        assert_eq!(fetched.kind, Kind::MlsKeyPackage);
        assert_eq!(fetched.pubkey, bot.public_key());

        relay.stop().await;
        Ok(())
    }

    #[tokio::test]
    async fn fetch_key_package_times_out() -> Result<(), Box<dyn std::error::Error>> {
        let relay = mock_relay::MockRelay::start().await?;
        let bot = Keys::generate();

        let client = RelayClient::new(&relay.url()).await?;
        let result = client
            .fetch_key_package(&bot.public_key(), Duration::from_millis(100))
            .await;

        assert!(
            matches!(result, Err(RelayError::Timeout(_, _))),
            "expected timeout error, got {result:?}"
        );

        relay.stop().await;
        Ok(())
    }

    #[tokio::test]
    async fn publish_event_to_relay() -> Result<(), Box<dyn std::error::Error>> {
        let relay = mock_relay::MockRelay::start().await?;
        let keys = Keys::generate();
        let event = EventBuilder::text_note("hello from relay client")
            .sign(&keys)
            .await?;

        let client = RelayClient::new(&relay.url()).await?;
        client.publish(&event).await?;

        let stored = relay
            .wait_for_event(
                |e| e.kind == Kind::TextNote && e.content == "hello from relay client",
                Duration::from_secs(5),
            )
            .await?;
        assert!(stored.iter().any(|e| e.id == event.id));

        relay.stop().await;
        Ok(())
    }

    #[tokio::test]
    async fn new_rejects_invalid_relay_url() {
        // A malformed URL must fail at connection setup rather than being
        // silently accepted.
        let result = RelayClient::new("not a valid url").await;
        assert!(
            matches!(result, Err(RelayError::Connect { .. })),
            "expected connection error for invalid URL, got {result:?}"
        );
    }
}
