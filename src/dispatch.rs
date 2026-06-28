use crate::errors::{DaemonError, JsonRpcError};
use crate::events::AgentEvent;
use crate::transport::protocol::{JsonRpcMessage, parse_method};

/// Placeholder event dispatch router.
#[derive(Debug, Default)]
pub struct Dispatch;

impl Dispatch {
    /// Create a new dispatch sink.
    pub fn new() -> Self {
        Self
    }

    /// Dispatch an outgoing agent event.
    pub async fn dispatch(&self, _event: AgentEvent) -> Result<(), DaemonError> {
        Ok(())
    }

    /// Handle an incoming JSON-RPC message from a transport.
    ///
    /// This placeholder validates that the method is part of the known catalog
    /// and returns a JSON-RPC "method not found" response for requests. Full
    /// routing (`handler.register`, `agent.send_dm`, etc.) is implemented in
    /// later units.
    pub async fn handle_message(
        &self,
        msg: JsonRpcMessage,
    ) -> Result<Option<JsonRpcMessage>, DaemonError> {
        let id = msg.id().cloned();

        let Some(method) = msg.method() else {
            return Ok(id.map(|id| {
                JsonRpcMessage::error(
                    id,
                    JsonRpcError::new(-32600, "invalid request: missing method"),
                )
            }));
        };

        if parse_method(method).is_err() {
            return Ok(id.map(|id| JsonRpcMessage::error(id, DaemonError::MethodNotFound.into())));
        }

        // Catalog method recognized; routing logic is added by U6/U7.
        Ok(id.map(|id| JsonRpcMessage::error(id, DaemonError::MethodNotFound.into())))
    }
}
