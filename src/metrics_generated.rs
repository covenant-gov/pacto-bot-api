//! Generated from schemas/metrics.json — do not edit manually.
//! Run `cargo xtask codegen` to regenerate.

use serde::{Deserialize, Serialize};

/// Metrics payload generated from schemas/metrics.json.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetricsPayloadGenerated {
    /// bunker_sign_failures_total
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bunker_sign_failures_total: Option<u64>,
    /// events_dispatched_total
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events_dispatched_total: Option<u64>,
    /// events_received_total
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events_received_total: Option<u64>,
    /// handlers_registered
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handlers_registered: Option<u64>,
    /// rate_limited_total
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limited_total: Option<u64>,
    /// relay_reconnects_total
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_reconnects_total: Option<u64>,
    /// uptime_seconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_seconds: Option<u64>,
}

