use serde::{Deserialize, Serialize};
use std::fmt;

/// JSON-RPC 2.0 error object returned to handlers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl JsonRpcError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
        self
    }
}

impl fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JSON-RPC error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for JsonRpcError {}

/// Operational errors inside the daemon.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("config error: {0}")]
    Config(String),

    #[error("toml parse error: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("nostr relay error: {0}")]
    Nostr(String),

    #[error("bunker error: {0}")]
    Bunker(String),

    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("json-rpc error: {0}")]
    JsonRpc(#[from] JsonRpcError),

    #[error("unknown bot: {0}")]
    UnknownBot(String),

    #[error("handler not registered")]
    HandlerNotRegistered,

    #[error("invalid event type: {0}")]
    InvalidEventType(String),

    #[error("rate limited")]
    RateLimited,

    #[error("unauthorized bot")]
    UnauthorizedBot,

    #[error("method not found")]
    MethodNotFound,

    #[error("method {0} not supported over this transport")]
    MethodNotSupported(String),

    #[error("frame too large")]
    FrameTooLarge,
}

impl DaemonError {
    /// Map this daemon error to a JSON-RPC 2.0 error code.
    pub fn to_json_rpc_code(&self) -> i32 {
        match self {
            DaemonError::UnknownBot(_) => -32000,
            DaemonError::HandlerNotRegistered => -32001,
            DaemonError::InvalidEventType(_) => -32002,
            DaemonError::Bunker(_) => -32003,
            DaemonError::Nostr(_) => -32004,
            DaemonError::RateLimited => -32005,
            DaemonError::UnauthorizedBot => -32006,
            DaemonError::JsonRpc(e) => e.code,
            DaemonError::MethodNotFound => -32601,
            DaemonError::MethodNotSupported(_) => -32007,
            // Malformed input or bad params are treated as invalid request/params.
            DaemonError::FrameTooLarge | DaemonError::Json(_) | DaemonError::Io(_) => -32600,
            DaemonError::Config(_) | DaemonError::Toml(_) => -32602,
            DaemonError::Sqlite(_) => -32603,
        }
    }
}

impl From<DaemonError> for JsonRpcError {
    fn from(err: DaemonError) -> Self {
        match err {
            DaemonError::JsonRpc(e) => e,
            other => {
                let code = other.to_json_rpc_code();
                let message = other.to_string();
                JsonRpcError::new(code, message)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_match_plan() {
        assert_eq!(
            DaemonError::UnknownBot("x".into()).to_json_rpc_code(),
            -32000
        );
        assert_eq!(DaemonError::HandlerNotRegistered.to_json_rpc_code(), -32001);
        assert_eq!(
            DaemonError::InvalidEventType("x".into()).to_json_rpc_code(),
            -32002
        );
        assert_eq!(DaemonError::Bunker("x".into()).to_json_rpc_code(), -32003);
        assert_eq!(DaemonError::Nostr("x".into()).to_json_rpc_code(), -32004);
        assert_eq!(DaemonError::RateLimited.to_json_rpc_code(), -32005);
        assert_eq!(DaemonError::UnauthorizedBot.to_json_rpc_code(), -32006);
        assert_eq!(DaemonError::MethodNotFound.to_json_rpc_code(), -32601);
    }

    #[test]
    fn into_json_rpc_preserves_code() {
        let err = DaemonError::UnknownBot("echo-bot".into());
        let rpc: JsonRpcError = err.into();
        assert_eq!(rpc.code, -32000);
        assert!(rpc.message.contains("echo-bot"));
    }
}
