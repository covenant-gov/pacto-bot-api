use crate::config::DaemonConfig;
use crate::dispatch::Dispatch;
use crate::errors::DaemonError;
use std::sync::Arc;

pub mod http;
pub mod protocol;
pub mod unix;

/// Combined transport layer exposing JSON-RPC over Unix socket and localhost HTTP.
#[derive(Debug)]
pub struct TransportLayer;

impl TransportLayer {
    pub fn new(_config: &DaemonConfig) -> Self {
        Self
    }

    pub async fn run(&self, _dispatch: Arc<Dispatch>) -> Result<(), DaemonError> {
        // Placeholder: real transport listeners will be started in U5/U6.
        Ok(())
    }
}
