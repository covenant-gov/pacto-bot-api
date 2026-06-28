use crate::errors::{DaemonError, JsonRpcError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::str::FromStr;

/// Maximum size of a single newline-delimited JSON frame (1 MiB).
pub const MAX_FRAME_BYTES: usize = 1_048_576;

fn jsonrpc_version() -> String {
    "2.0".to_string()
}

/// A JSON-RPC 2.0 message on the wire.
///
/// The enum is untagged so that the presence of `method`, `result`, or `error`
/// selects the correct variant while preserving the wire shape for requests,
/// responses, notifications, and error responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcMessage {
    /// A JSON-RPC request that expects a response.
    Request {
        #[serde(default = "jsonrpc_version")]
        jsonrpc: String,
        id: Value,
        method: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        params: Option<Value>,
    },
    /// A JSON-RPC response carrying a successful result.
    Response {
        #[serde(default = "jsonrpc_version")]
        jsonrpc: String,
        id: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
    },
    /// A JSON-RPC error response.
    #[serde(rename = "error")]
    Error {
        #[serde(default = "jsonrpc_version")]
        jsonrpc: String,
        id: Value,
        error: JsonRpcError,
    },
    /// A JSON-RPC notification (no `id`).
    Notification {
        #[serde(default = "jsonrpc_version")]
        jsonrpc: String,
        method: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        params: Option<Value>,
    },
}

impl JsonRpcMessage {
    /// Build a JSON-RPC request.
    pub fn request(id: Value, method: impl Into<String>, params: Option<Value>) -> Self {
        Self::Request {
            jsonrpc: jsonrpc_version(),
            id,
            method: method.into(),
            params,
        }
    }

    /// Build a JSON-RPC notification.
    pub fn notification(method: impl Into<String>, params: Option<Value>) -> Self {
        Self::Notification {
            jsonrpc: jsonrpc_version(),
            method: method.into(),
            params,
        }
    }

    /// Build a JSON-RPC success response.
    pub fn response(id: Value, result: Option<Value>) -> Self {
        Self::Response {
            jsonrpc: jsonrpc_version(),
            id,
            result,
        }
    }

    /// Build a JSON-RPC error response.
    pub fn error(id: Value, error: JsonRpcError) -> Self {
        Self::Error {
            jsonrpc: jsonrpc_version(),
            id,
            error,
        }
    }

    /// Return the message id, if it has one.
    pub fn id(&self) -> Option<&Value> {
        match self {
            Self::Request { id, .. } | Self::Response { id, .. } | Self::Error { id, .. } => {
                Some(id)
            }
            Self::Notification { .. } => None,
        }
    }

    /// Return the method name, if this is a request or notification.
    pub fn method(&self) -> Option<&str> {
        match self {
            Self::Request { method, .. } | Self::Notification { method, .. } => Some(method),
            Self::Response { .. } | Self::Error { .. } => None,
        }
    }
}

/// Parse a newline-delimited JSON frame into a [`JsonRpcMessage`].
pub fn parse_message(line: &str) -> Result<JsonRpcMessage, DaemonError> {
    let msg: JsonRpcMessage = serde_json::from_str(line)?;
    Ok(msg)
}

/// Validate that `size` does not exceed the maximum frame size.
pub fn validate_frame_size(size: usize) -> Result<(), DaemonError> {
    if size > MAX_FRAME_BYTES {
        Err(DaemonError::FrameTooLarge)
    } else {
        Ok(())
    }
}

/// Parse a byte slice after checking the 1 MB frame-size limit.
pub fn parse_frame(frame: &[u8]) -> Result<JsonRpcMessage, DaemonError> {
    validate_frame_size(frame.len())?;
    let line = std::str::from_utf8(frame)
        .map_err(|_| DaemonError::Config("frame is not valid UTF-8".into()))?;
    parse_message(line)
}

/// Serialize a [`JsonRpcMessage`] to a compact JSON string.
pub fn serialize_message(msg: &JsonRpcMessage) -> Result<String, DaemonError> {
    let s = serde_json::to_string(msg)?;
    Ok(s)
}

/// Known JSON-RPC methods in the daemon catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    #[serde(rename = "handler.register")]
    HandlerRegister,
    #[serde(rename = "handler.unregister")]
    HandlerUnregister,
    #[serde(rename = "agent.send_dm")]
    AgentSendDm,
    #[serde(rename = "agent.set_profile")]
    AgentSetProfile,
    #[serde(rename = "agent.error")]
    AgentError,
    #[serde(rename = "handler.response")]
    HandlerResponse,
    #[serde(rename = "agent.event")]
    AgentEvent,
    #[serde(rename = "agent.status")]
    AgentStatus,
    #[serde(rename = "agent.metrics")]
    AgentMetrics,
}

impl FromStr for Method {
    type Err = DaemonError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "handler.register" => Ok(Self::HandlerRegister),
            "handler.unregister" => Ok(Self::HandlerUnregister),
            "agent.send_dm" => Ok(Self::AgentSendDm),
            "agent.set_profile" => Ok(Self::AgentSetProfile),
            "agent.error" => Ok(Self::AgentError),
            "handler.response" => Ok(Self::HandlerResponse),
            "agent.event" => Ok(Self::AgentEvent),
            "agent.status" => Ok(Self::AgentStatus),
            "agent.metrics" => Ok(Self::AgentMetrics),
            _ => Err(DaemonError::MethodNotFound),
        }
    }
}

/// Validate that a method name belongs to the known catalog.
pub fn parse_method(method: &str) -> Result<Method, DaemonError> {
    method.parse()
}

/// Typed payload returned by the `agent.metrics` JSON-RPC method.
///
/// The response is serialized as a flat object so that adding fields to
/// [`crate::diagnostics::HealthSnapshot`] does not require changes to the
/// wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsResponse {
    #[serde(flatten)]
    pub snapshot: crate::diagnostics::HealthSnapshot,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_serialize_catalog_methods() -> Result<(), DaemonError> {
        let methods = [
            "handler.register",
            "handler.unregister",
            "agent.send_dm",
            "agent.set_profile",
            "agent.error",
            "handler.response",
            "agent.event",
            "agent.status",
            "agent.metrics",
        ];

        for method in methods {
            let req =
                JsonRpcMessage::request(1.into(), method, Some(Value::Object(Default::default())));
            let line = serialize_message(&req)?;
            let parsed = parse_message(&line)?;
            assert_eq!(parsed.method(), Some(method));

            let notif = JsonRpcMessage::notification(method, None);
            let line = serialize_message(&notif)?;
            let parsed = parse_message(&line)?;
            assert_eq!(parsed.method(), Some(method));
            assert!(parsed.id().is_none());
        }

        Ok(())
    }

    #[test]
    fn response_and_error_round_trip() -> Result<(), DaemonError> {
        let resp = JsonRpcMessage::response(42.into(), Some(Value::String("ok".into())));
        let line = serialize_message(&resp)?;
        let parsed = parse_message(&line)?;
        assert_eq!(parsed.id(), Some(&Value::from(42)));

        let err = JsonRpcMessage::error(7.into(), JsonRpcError::new(-32601, "method not found"));
        let line = serialize_message(&err)?;
        let parsed = parse_message(&line)?;
        assert_eq!(parsed.id(), Some(&Value::from(7)));
        Ok(())
    }

    #[test]
    fn frame_size_limit_is_enforced() {
        assert!(validate_frame_size(MAX_FRAME_BYTES).is_ok());
        assert!(validate_frame_size(MAX_FRAME_BYTES + 1).is_err());
    }

    #[test]
    fn unknown_method_is_rejected() {
        assert!(parse_method("not.in.catalog").is_err());
    }
}
