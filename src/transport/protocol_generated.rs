//! Generated from schemas/jsonrpc.json — do not edit manually.
//! Run `cargo xtask codegen` to regenerate.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC method catalog entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcMethod {
    /// Method name (e.g. `handler.register`).
    pub name: String,
    /// Parameter schema fragments.
    pub params: Option<Value>,
    /// Result schema fragment.
    pub result: Option<Value>,
}

/// JSON-RPC catalog container.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JsonRpcCatalogGenerated {
    /// info
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info: Option<serde_json::Value>,
    /// methods
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub methods: Option<Vec<serde_json::Value>>,
    /// openrpc
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openrpc: Option<String>,
}

