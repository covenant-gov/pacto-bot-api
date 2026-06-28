#![allow(dead_code, reason = "support utilities used by future integration tests")]

use nostr::{Event, Keys, PublicKey};
use std::sync::Arc;
use tokio::sync::{Mutex, broadcast};

/// A lightweight in-process NIP-46 bunker for integration tests.
///
/// Currently this is a structural placeholder: the daemon's bunker signing path
/// is not fully implemented, so a live mock bunker cannot yet exercise the full
/// `sign_event` / `nip44_encrypt` flow. The type is exposed so that future
/// tests can plug it in once the daemon side is ready.
#[derive(Clone)]
pub struct MockBunker {
    keys: Keys,
    requests: Arc<Mutex<Vec<BunkerRequest>>>,
    request_tx: broadcast::Sender<BunkerRequest>,
}

#[derive(Debug, Clone)]
pub struct BunkerRequest {
    pub method: String,
    pub params: serde_json::Value,
}

impl MockBunker {
    /// Create a new mock bunker backed by the given keys.
    pub fn new(keys: Keys) -> Self {
        let (request_tx, _request_rx) = broadcast::channel(64);
        Self {
            keys,
            requests: Arc::new(Mutex::new(Vec::new())),
            request_tx,
        }
    }

    /// Return the bunker URI that clients can use to connect.
    pub fn uri(&self, relay_url: &str) -> String {
        format!(
            "bunker://{}?relay={}",
            self.keys.public_key().to_hex(),
            relay_url
        )
    }

    /// Return the bunker's long-term public key.
    pub fn public_key(&self) -> PublicKey {
        self.keys.public_key()
    }

    /// Record an incoming NIP-46 request and broadcast it to subscribers.
    pub async fn record_request(&self, method: String, params: serde_json::Value) {
        let req = BunkerRequest { method, params };
        self.requests.lock().await.push(req.clone());
        let _ = self.request_tx.send(req);
    }

    /// Return a copy of all recorded requests.
    pub async fn requests(&self) -> Vec<BunkerRequest> {
        self.requests.lock().await.clone()
    }

    /// Produce a bunker response event for `get_public_key`.
    pub async fn public_key_response(
        &self,
        _client_pubkey: &PublicKey,
    ) -> Result<Event, Box<dyn std::error::Error>> {
        let content = serde_json::json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "result": self.keys.public_key().to_hex(),
        });
        self.sign_response(content).await
    }

    async fn sign_response(
        &self,
        content: serde_json::Value,
    ) -> Result<Event, Box<dyn std::error::Error>> {
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

    #[tokio::test]
    async fn bunker_can_be_constructed() {
        let keys = Keys::generate();
        let bunker = MockBunker::new(keys.clone());
        assert_eq!(bunker.public_key(), keys.public_key());
    }
}
