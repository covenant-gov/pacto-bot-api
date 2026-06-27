use crate::errors::DaemonError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A JSON-RPC 2.0 message on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum JsonRpcMessage {
    Request {
        id: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        params: Option<Value>,
    },
    Response {
        id: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
    },
    #[serde(rename = "error")]
    ErrorResponse {
        id: Value,
        error: crate::errors::JsonRpcError,
    },
    Notification {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        params: Option<Value>,
    },
}

pub fn parse_message(line: &str) -> Result<JsonRpcMessage, DaemonError> {
    let msg: JsonRpcMessage = serde_json::from_str(line)?;
    Ok(msg)
}

pub fn serialize_message(msg: &JsonRpcMessage) -> Result<String, DaemonError> {
    let s = serde_json::to_string(msg)?;
    Ok(s)
}
