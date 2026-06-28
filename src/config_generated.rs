//! Generated from schemas/config.json — do not edit manually.
//! Run `cargo xtask codegen` to regenerate.

use serde::{Deserialize, Serialize};

/// Daemon-wide settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DaemonConfigGenerated {
    /// data_dir
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<String>,
    /// http_bind
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_bind: Option<String>,
    /// socket_path
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket_path: Option<String>,
}

/// Per-bot identity configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BotConfigGenerated {
    /// capabilities
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    /// id
    pub id: String,
    /// npub
    pub npub: String,
    /// relays
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relays: Option<Vec<String>>,
    /// signing
    pub signing: serde_json::Value,
}
