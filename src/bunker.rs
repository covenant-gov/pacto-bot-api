use crate::errors::DaemonError;

/// Placeholder NIP-46 bunker connection.
#[derive(Debug, Clone)]
pub struct BunkerConnection {
    pub uri: String,
}

impl BunkerConnection {
    pub fn connect(uri: &str) -> Result<Self, DaemonError> {
        if uri.is_empty() {
            return Err(DaemonError::Bunker("empty bunker URI".into()));
        }
        Ok(Self {
            uri: uri.to_string(),
        })
    }
}
