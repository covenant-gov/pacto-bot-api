use crate::errors::DaemonError;

/// Placeholder Nostr relay client wrapper.
#[derive(Debug, Clone)]
pub struct NostrClient {
    relays: Vec<String>,
}

impl NostrClient {
    pub fn new(relays: Vec<String>) -> Result<Self, DaemonError> {
        Ok(Self { relays })
    }

    pub fn relays(&self) -> &[String] {
        &self.relays
    }
}
