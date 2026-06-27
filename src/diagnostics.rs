use serde::{Deserialize, Serialize};

/// Aggregated health snapshot used by `agent.metrics` and `pacto-bot-admin diagnose`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HealthSnapshot {
    pub uptime_seconds: u64,
    pub handlers_registered: u64,
    pub events_received_total: u64,
    pub events_dispatched_total: u64,
    pub rate_limited_total: u64,
    pub relay_reconnects_total: u64,
    pub bunker_sign_failures_total: u64,
}

impl HealthSnapshot {
    pub fn new() -> Self {
        Self::default()
    }
}
