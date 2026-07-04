//! Generated from schemas/metrics.json — do not edit manually.
//! Run `cargo xtask codegen` to regenerate.

use serde::{Deserialize, Serialize};

/// Generated from `#/definitions/botHealth`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BotHealthGenerated {
    /// bot_id
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_id: Option<String>,
    /// bunker_connected
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bunker_connected: Option<bool>,
    /// error
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// npub
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npub: Option<String>,
    /// relay_count
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_count: Option<u64>,
    /// relays
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relays: Option<Vec<String>>,
    /// signer_backend
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signer_backend: Option<String>,
}

/// Metrics payload generated from schemas/metrics.json.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetricsPayloadGenerated {
    /// bots
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bots: Option<Vec<BotHealthGenerated>>,
    /// bunker_sign_failures_total
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bunker_sign_failures_total: Option<u64>,
    /// events_dispatched_last_10_min
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events_dispatched_last_10_min: Option<u64>,
    /// events_dispatched_total
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events_dispatched_total: Option<u64>,
    /// events_received_last_10_min
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events_received_last_10_min: Option<u64>,
    /// events_received_total
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events_received_total: Option<u64>,
    /// handlers_registered
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handlers_registered: Option<u64>,
    /// invalid_events_total
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid_events_total: Option<u64>,
    /// rate_limited_total
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limited_total: Option<u64>,
    /// relay_reconnects_total
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_reconnects_total: Option<u64>,
    /// replies_last_10_min
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replies_last_10_min: Option<u64>,
    /// reply_send_failed_last_10_min
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_send_failed_last_10_min: Option<u64>,
    /// reply_send_failed_total
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_send_failed_total: Option<u64>,
    /// uptime_seconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_seconds: Option<u64>,
}
