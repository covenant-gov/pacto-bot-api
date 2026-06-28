//! Generated from schemas/jsonrpc.json — do not edit manually.
//! Run `cargo xtask codegen` to regenerate.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC method catalog entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcMethodGenerated {
    /// Method name (e.g. `handler.register`).
    pub name: String,
    /// Human-readable summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Parameter schemas (by-name JSON-RPC style).
    #[serde(default)]
    pub params: Vec<JsonRpcParamGenerated>,
    /// Result schema, when the method returns a value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<JsonRpcResultGenerated>,
}

/// Named parameter schema for a JSON-RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcParamGenerated {
    /// Outer key used on the wire (`params` for by-name).
    pub name: String,
    /// JSON Schema fragment for the parameter object.
    pub schema: Value,
}

/// Result schema for a JSON-RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResultGenerated {
    /// Descriptive name for the result payload.
    pub name: String,
    /// JSON Schema fragment for the result value.
    pub schema: Value,
}

/// JSON-RPC catalog container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcCatalogGenerated {
    /// OpenRPC version.
    pub openrpc: String,
    /// Catalog metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info: Option<Value>,
    /// Registered JSON-RPC methods.
    pub methods: Vec<JsonRpcMethodGenerated>,
}
