#![allow(dead_code, reason = "support utilities used by future integration tests")]

use nostr::Keys;

use nostr_connect::prelude::*;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, broadcast};
use tokio::task::JoinHandle;

/// A lightweight in-process NIP-46 bunker for integration tests.
///
/// Wraps `nostr-connect`'s `NostrConnectRemoteSigner` and serves all
/// approved requests over an in-memory relay. The bunker auto-approves every
/// request, making it suitable for happy-path tests.
#[derive(Clone)]
pub struct MockBunker {
    keys: Keys,
    requests: Arc<Mutex<Vec<BunkerRequest>>>,
    request_tx: broadcast::Sender<BunkerRequest>,
    handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

#[derive(Debug, Clone)]
pub struct BunkerRequest {
    pub method: String,
    pub params: serde_json::Value,
}

/// Action handler that auto-approves every NIP-46 request and records it.
#[derive(Clone)]
struct AutoApprove {
    requests: Arc<Mutex<Vec<BunkerRequest>>>,
    request_tx: broadcast::Sender<BunkerRequest>,
}

impl NostrConnectSignerActions for AutoApprove {
    fn approve(&self,
        _public_key: &nostr::PublicKey,
        req: &nostr::nips::nip46::NostrConnectRequest,
    ) -> bool {
        let method = req.method().to_string();
        let params = serde_json::to_value(req.params()).unwrap_or(serde_json::Value::Null);
        let request = BunkerRequest { method, params };

        // Fire-and-forget recording: tests that need ordering can subscribe
        // to the broadcast channel instead of polling `requests()`.
        if let Ok(mut guard) = self.requests.try_lock() {
            guard.push(request.clone());
        }
        let _ = self.request_tx.send(request);
        true
    }
}

impl MockBunker {
    /// Create a new mock bunker backed by the given keys and start serving
    /// requests on the supplied relay URLs.
    pub async fn new(keys: Keys, relays: Vec<String>) -> Result<Self, Box<dyn std::error::Error>> {
        let connect_keys = NostrConnectKeys {
            signer: keys.clone(),
            user: keys.clone(),
        };
        let signer = NostrConnectRemoteSigner::new(connect_keys, relays, None, None)?;
        let (request_tx, _request_rx) = broadcast::channel(64);

        let requests = Arc::new(Mutex::new(Vec::new()));
        let actions = AutoApprove {
            requests: Arc::clone(&requests),
            request_tx: request_tx.clone(),
        };

        let handle = tokio::spawn(async move {
            let _ = signer.serve(actions).await;
        });

        Ok(Self {
            keys,
            requests,
            request_tx,
            handle: Arc::new(Mutex::new(Some(handle))),
        })
    }

    /// Return the bunker URI that clients can use to connect.
    pub fn uri(&self, relay_url: &str) -> String {
        format!(
            "bunker://{}?relay={}",
            self.keys.public_key().to_hex(),
            relay_url
        )
    }

    /// Return the bunker URI using the first relay in `relays`.
    pub fn uri_from_relays(&self,
        relays: &[impl AsRef<str>],
    ) -> Option<String> {
        relays.first().map(|r| self.uri(r.as_ref()))
    }

    /// Return the bunker's long-term public key.
    pub fn public_key(&self) -> nostr::PublicKey {
        self.keys.public_key()
    }

    /// Return a copy of all recorded requests.
    pub async fn requests(&self) -> Vec<BunkerRequest> {
        self.requests.lock().await.clone()
    }

    /// Wait until a request matching `predicate` is recorded, or timeout.
    pub async fn wait_for_request<F>(
        &self,
        predicate: F,
        timeout: Duration,
    ) -> Result<BunkerRequest, Box<dyn std::error::Error>>
    where
        F: Fn(&BunkerRequest) -> bool,
    {
        let mut rx = self.request_tx.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            {
                let guard = self.requests.lock().await;
                if let Some(req) = guard.iter().find(|r| predicate(r)) {
                    return Ok(req.clone());
                }
            }

            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err("timeout waiting for bunker request".into());
            }

            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(_)) => continue,
                Ok(Err(_)) => return Err("bunker request channel closed".into()),
                Err(_) => return Err("timeout waiting for bunker request".into()),
            }
        }
    }

    /// Stop the mock bunker and wait for the serve task to finish.
    pub async fn stop(self) {
        if let Some(handle) = self.handle.lock().await.take() {
            handle.abort();
            let _ = handle.await;
        }
    }

    /// Produce a bunker response event for `get_public_key`.
    ///
    /// Kept for backwards compatibility with tests that manually drive the
    /// mock relay; the primary path now uses `NostrConnectRemoteSigner::serve`.
    pub async fn public_key_response(
        &self,
        _client_pubkey: &nostr::PublicKey,
    ) -> Result<nostr::Event, Box<dyn std::error::Error>> {
        let content = serde_json::json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "result": self.keys.public_key().to_hex(),
        });
        self.sign_response(content).await
    }

    async fn sign_response(
        &self,
        content: serde_json::Value,
    ) -> Result<nostr::Event, Box<dyn std::error::Error>> {
        // The mock response is a placeholder kind:24133 event. A full
        // implementation would NIP-44 encrypt `content` to the client.
        let event = nostr::EventBuilder::new(nostr::Kind::NostrConnect, content.to_string())
            .sign(&self.keys)
            .await?;
        Ok(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn bunker_can_be_constructed() {
        let keys = Keys::generate();
        let bunker = MockBunker::new(keys.clone(), vec![]).await.unwrap();
        assert_eq!(bunker.public_key(), keys.public_key());
    }

    #[tokio::test]
    async fn bunker_responds_to_get_public_key() {
        let keys = Keys::generate();
        let relay = crate::support::mock_relay::MockRelay::start().await.unwrap();
        let bunker = MockBunker::new(keys.clone(), vec![relay.url()]).await.unwrap();

        // Give the signer a moment to bootstrap and subscribe.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let uri = bunker.uri(&relay.url());
        let app_keys = Keys::generate();
        let parsed = nostr::nips::nip46::NostrConnectURI::parse(&uri).unwrap();
        let connect = NostrConnect::new(parsed, app_keys, Duration::from_secs(5), None).unwrap();

        let live_uri = connect.bunker_uri().await.unwrap();
        connect.shutdown().await;
        relay.stop().await;
        bunker.stop().await;

        let live_pubkey = live_uri.remote_signer_public_key().unwrap();
        assert_eq!(*live_pubkey, keys.public_key());
    }
}
