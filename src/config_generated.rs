//! Generated from schemas/config.json — do not edit manually.
//! Run `cargo xtask codegen` to regenerate.

use serde::{Deserialize, Serialize};

/// Daemon-wide settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DaemonConfigGenerated {
    /// data_dir
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<String>,
    /// Seconds between stale-handler reaper sweeps
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handler_reap_interval_secs: Option<u64>,
    /// Seconds after a handler disconnect before it is reaped from the routing table
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handler_stale_timeout_secs: Option<u64>,
    /// http_bind
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_bind: Option<String>,
    /// Idle timeout for HTTP keep-alive connections in seconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_idle_timeout_secs: Option<u64>,
    /// Maximum concurrent HTTP connections
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_max_connections: Option<u64>,
    /// socket_path
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket_path: Option<String>,
}

/// Per-bot identity configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BotConfigGenerated {
    /// Description text published in the bot's kind:0 profile
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    /// capabilities
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    /// Human-readable display name for the bot profile; used as the @mention alias in squad channels. Must be unique among bots.
    pub display_name: String,
    /// Daemon-local bot identifier. Must be a slug: lowercase letters, digits, hyphens, and underscores only. Maximum 64 characters.
    pub id: String,
    /// Path to the per-bot MLS SQLite database
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mls_db_path: Option<String>,
    /// Time window in seconds for MLS group-message deduplication
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mls_dedup_window_secs: Option<u64>,
    /// Freshness window in seconds for MLS KeyPackage events
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mls_key_package_freshness_secs: Option<u64>,
    /// npub
    pub npub: String,
    /// URL to the bot's profile picture (http:// or https://)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
    /// relays
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relays: Option<Vec<String>>,
    /// signing
    pub signing: serde_json::Value,
}
