use crate::errors::DaemonError;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, RwLock, RwLockWriteGuard};
use std::time::Instant;

/// Number of recent error messages to retain in a snapshot.
const ERROR_BUFFER_CAPACITY: usize = 32;

/// Daemon lifecycle status reported in health snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonStatus {
    /// Daemon is starting up and dependencies are being initialized.
    Initializing,
    /// Daemon is running and accepting JSON-RPC traffic.
    Ready,
    /// Daemon is in the middle of a graceful shutdown.
    ShuttingDown,
    /// Daemon has stopped and final reports have been flushed.
    Stopped,
}

/// Per-bot health summary.
///
/// Contains only public, non-sensitive identifiers. Signing backends or
/// secrets must never be stored here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotHealth {
    /// Daemon-local bot label.
    pub id: String,
    /// Bot Nostr public key (`npub1...`).
    pub npub: String,
    /// Number of configured relays for this bot.
    pub relay_count: u64,
    /// Whether the NIP-46 bunker signer is currently connected.
    pub bunker_connected: bool,
}

/// Aggregated health snapshot used by `agent.metrics` and
/// `pacto-bot-admin diagnose`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthSnapshot {
    /// Current daemon lifecycle status.
    pub status: DaemonStatus,
    /// UTC timestamp recorded when the daemon (or this snapshot source) started.
    pub startup_time: DateTime<Utc>,
    /// Daemon uptime in seconds.
    pub uptime_seconds: u64,
    /// Number of handlers currently registered.
    pub handlers_registered: u64,
    /// Total incoming events accepted by the daemon.
    pub events_received_total: u64,
    /// Total events dispatched to handlers.
    pub events_dispatched_total: u64,
    /// Total events dropped due to rate limiting.
    pub rate_limited_total: u64,
    /// Total relay reconnections observed across all bots.
    pub relay_reconnects_total: u64,
    /// Total NIP-46 bunker signing failures observed across all bots.
    pub bunker_sign_failures_total: u64,
    /// Per-bot health summaries.
    pub bots: Vec<BotHealth>,
    /// Recent redacted error messages, oldest first.
    pub errors: Vec<String>,
    /// UTC timestamp when this snapshot was produced.
    pub reported_at: DateTime<Utc>,
}

impl Default for HealthSnapshot {
    fn default() -> Self {
        let now = Utc::now();
        Self {
            status: DaemonStatus::Initializing,
            startup_time: now,
            uptime_seconds: 0,
            handlers_registered: 0,
            events_received_total: 0,
            events_dispatched_total: 0,
            rate_limited_total: 0,
            relay_reconnects_total: 0,
            bunker_sign_failures_total: 0,
            bots: Vec::new(),
            errors: Vec::new(),
            reported_at: now,
        }
    }
}

impl HealthSnapshot {
    /// Create a fresh initializing snapshot.
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Debug)]
struct Inner {
    snapshot: HealthSnapshot,
    startup_instant: Instant,
    errors: VecDeque<String>,
}

/// Thread-safe diagnostics aggregator.
#[derive(Debug, Clone)]
pub struct Diagnostics {
    inner: Arc<RwLock<Inner>>,
}

impl Default for Diagnostics {
    fn default() -> Self {
        Self::new()
    }
}

impl Diagnostics {
    /// Create a new diagnostics aggregator.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner {
                snapshot: HealthSnapshot::default(),
                startup_instant: Instant::now(),
                errors: VecDeque::with_capacity(ERROR_BUFFER_CAPACITY),
            })),
        }
    }

    /// Return a current snapshot with `reported_at` and `uptime_seconds`
    /// refreshed.
    pub fn snapshot(&self) -> HealthSnapshot {
        let mut inner = write_guard(&self.inner);
        let now = Utc::now();
        inner.snapshot.reported_at = now;
        inner.snapshot.uptime_seconds = inner.startup_instant.elapsed().as_secs();
        inner.snapshot.clone()
    }

    /// Replace the per-bot health summaries.
    pub fn set_bots(&self, bots: Vec<BotHealth>) {
        self.with_snapshot(|snapshot| snapshot.bots = bots);
    }

    /// Set the daemon lifecycle status.
    pub fn set_status(&self, status: DaemonStatus) {
        self.with_snapshot(|snapshot| snapshot.status = status);
    }

    /// Increment the counter for events received from Nostr relays.
    pub fn record_event_received(&self) {
        self.with_snapshot(|snapshot| snapshot.events_received_total += 1);
    }

    /// Increment the counter for events dispatched to registered handlers.
    pub fn record_event_dispatched(&self) {
        self.with_snapshot(|snapshot| snapshot.events_dispatched_total += 1);
    }

    /// Increment the counter for rate-limited events.
    pub fn record_rate_limited(&self) {
        self.with_snapshot(|snapshot| snapshot.rate_limited_total += 1);
    }

    /// Increment the counter for relay reconnections.
    pub fn record_relay_reconnect(&self) {
        self.with_snapshot(|snapshot| snapshot.relay_reconnects_total += 1);
    }

    /// Increment the counter for bunker signing failures.
    pub fn record_bunker_sign_failure(&self) {
        self.with_snapshot(|snapshot| snapshot.bunker_sign_failures_total += 1);
    }

    /// Set the number of registered handlers.
    pub fn set_handlers_registered(&self, count: u64) {
        self.with_snapshot(|snapshot| snapshot.handlers_registered = count);
    }

    /// Record a recent error message.
    ///
    /// The message is redacted before storage so that secrets (`nsec1...`,
    /// query parameters such as `secret=...`, `token=...`) never appear in
    /// snapshots or on-disk reports.
    pub fn record_error(&self, message: &str) {
        let redacted = redact_secrets(message);
        let mut inner = write_guard(&self.inner);
        if inner.errors.len() >= ERROR_BUFFER_CAPACITY {
            inner.errors.pop_front();
        }
        inner.errors.push_back(redacted);
        inner.snapshot.errors = inner.errors.iter().cloned().collect();
    }

    /// Atomically write the current snapshot to
    /// `<data_dir>/reports/latest.json`.
    pub fn flush_report(&self, data_dir: &Path) -> Result<(), DaemonError> {
        let snapshot = self.snapshot();
        let reports_dir = data_dir.join("reports");
        std::fs::create_dir_all(&reports_dir)?;

        let tmp_path = reports_dir.join("latest.json.tmp");
        let final_path = reports_dir.join("latest.json");

        let json = serde_json::to_string_pretty(&snapshot)?;
        std::fs::write(&tmp_path, json)?;
        std::fs::rename(&tmp_path, &final_path)?;

        Ok(())
    }

    fn with_snapshot<F>(&self, f: F)
    where
        F: FnOnce(&mut HealthSnapshot),
    {
        let mut inner = write_guard(&self.inner);
        f(&mut inner.snapshot);
    }
}

fn write_guard<'a, T>(lock: &'a RwLock<T>) -> RwLockWriteGuard<'a, T> {
    match lock.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Redact likely secret material from a diagnostic message.
///
/// This is a best-effort filter; application code should avoid logging
/// secrets in the first place.
fn redact_secrets(input: &str) -> String {
    let mut out = redact_word_prefix(input, "nsec1");
    out = redact_query_param(&out, "secret");
    redact_query_param(&out, "token")
}

/// Redact a contiguous alphanumeric token that starts with `prefix`.
fn redact_word_prefix(input: &str, prefix: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(pos) = rest.find(prefix) {
        result.push_str(&rest[..pos]);
        result.push_str("[REDACTED]");

        let after = &rest[pos + prefix.len()..];
        let consumed = after
            .chars()
            .take_while(|&c| c.is_ascii_alphanumeric())
            .count();
        rest = &after[consumed..];
    }
    result.push_str(rest);
    result
}

/// Redact the value of a URL-style query parameter `key=`.
fn redact_query_param(input: &str, key: &str) -> String {
    let pattern = format!("{key}=");
    let mut result = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(pos) = rest.find(&pattern) {
        result.push_str(&rest[..pos]);
        result.push_str(&pattern);
        result.push_str("[REDACTED]");

        let after = &rest[pos + pattern.len()..];
        let end = match after.find(|c: char| c == '&' || c.is_whitespace()) {
            Some(idx) => idx,
            None => after.len(),
        };
        rest = &after[end..];
    }
    result.push_str(rest);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_snapshot_initializes_counters_to_zero() {
        let diag = Diagnostics::new();
        let snap = diag.snapshot();
        assert_eq!(snap.status, DaemonStatus::Initializing);
        assert_eq!(snap.events_received_total, 0);
        assert_eq!(snap.events_dispatched_total, 0);
        assert_eq!(snap.rate_limited_total, 0);
        assert_eq!(snap.relay_reconnects_total, 0);
        assert_eq!(snap.bunker_sign_failures_total, 0);
        assert!(snap.bots.is_empty());
        assert!(snap.errors.is_empty());
    }

    #[test]
    fn counters_increment_independently() {
        let diag = Diagnostics::new();
        diag.record_event_received();
        diag.record_event_received();
        diag.record_event_dispatched();
        diag.record_rate_limited();
        diag.record_relay_reconnect();
        diag.record_bunker_sign_failure();
        diag.record_bunker_sign_failure();
        diag.set_handlers_registered(5);

        let snap = diag.snapshot();
        assert_eq!(snap.events_received_total, 2);
        assert_eq!(snap.events_dispatched_total, 1);
        assert_eq!(snap.rate_limited_total, 1);
        assert_eq!(snap.relay_reconnects_total, 1);
        assert_eq!(snap.bunker_sign_failures_total, 2);
        assert_eq!(snap.handlers_registered, 5);
    }

    #[test]
    fn status_transitions_are_reflected() {
        let diag = Diagnostics::new();
        assert_eq!(diag.snapshot().status, DaemonStatus::Initializing);

        diag.set_status(DaemonStatus::Ready);
        assert_eq!(diag.snapshot().status, DaemonStatus::Ready);

        diag.set_status(DaemonStatus::ShuttingDown);
        assert_eq!(diag.snapshot().status, DaemonStatus::ShuttingDown);

        diag.set_status(DaemonStatus::Stopped);
        assert_eq!(diag.snapshot().status, DaemonStatus::Stopped);
    }

    #[test]
    fn bots_are_stored_in_snapshot() {
        let diag = Diagnostics::new();
        diag.set_bots(vec![
            BotHealth {
                id: "bot-a".into(),
                npub: "npub1example".into(),
                relay_count: 3,
                bunker_connected: true,
            },
            BotHealth {
                id: "bot-b".into(),
                npub: "npub1other".into(),
                relay_count: 0,
                bunker_connected: false,
            },
        ]);

        let snap = diag.snapshot();
        assert_eq!(snap.bots.len(), 2);
        assert_eq!(snap.bots[0].id, "bot-a");
        assert_eq!(snap.bots[1].relay_count, 0);
    }

    #[test]
    fn errors_are_redacted_in_snapshot() {
        let diag = Diagnostics::new();
        diag.record_error("signing failed for nsec1deadbeef1234 on bot-a");
        diag.record_error("bunker uri: bunker://relay.example?secret=supersecret&token=abc123");

        let snap = diag.snapshot();
        assert_eq!(snap.errors.len(), 2);
        let joined = snap.errors.join(" ");
        assert!(!joined.contains("nsec1deadbeef1234"));
        assert!(!joined.contains("supersecret"));
        assert!(!joined.contains("abc123"));
        assert!(joined.contains("[REDACTED]"));
    }

    #[test]
    fn error_buffer_drops_oldest_messages() {
        let diag = Diagnostics::new();
        for i in 0..ERROR_BUFFER_CAPACITY + 5 {
            diag.record_error(&format!("error {i}"));
        }
        let snap = diag.snapshot();
        assert_eq!(snap.errors.len(), ERROR_BUFFER_CAPACITY);
        assert!(snap.errors[0].contains("error 5"));
        let last = snap.errors.iter().last();
        assert!(last.is_some_and(|s| s.contains(&format!("error {}", ERROR_BUFFER_CAPACITY + 4))));
    }

    #[test]
    fn flush_report_round_trips() -> Result<(), DaemonError> {
        let tmp = tempfile::tempdir()?;
        let diag = Diagnostics::new();
        diag.set_status(DaemonStatus::Ready);
        diag.record_event_received();
        diag.set_bots(vec![BotHealth {
            id: "bot-x".into(),
            npub: "npub1x".into(),
            relay_count: 2,
            bunker_connected: true,
        }]);

        diag.flush_report(tmp.path())?;

        let report_path = tmp.path().join("reports").join("latest.json");
        let contents = std::fs::read_to_string(&report_path)?;
        let parsed: HealthSnapshot = serde_json::from_str(&contents)?;
        assert_eq!(parsed.status, DaemonStatus::Ready);
        assert_eq!(parsed.events_received_total, 1);
        assert_eq!(parsed.bots.len(), 1);
        Ok(())
    }

    #[test]
    fn flushed_report_contains_no_secrets() -> Result<(), DaemonError> {
        let tmp = tempfile::tempdir()?;
        let diag = Diagnostics::new();
        diag.record_error("leaked nsec1verysecretandlonghexstring");
        diag.record_error("bunker secret=shh! token=do-not-leak");
        diag.flush_report(tmp.path())?;

        let report_path = tmp.path().join("reports").join("latest.json");
        let contents = std::fs::read_to_string(&report_path)?;
        assert!(!contents.contains("nsec1verysecretandlonghexstring"));
        assert!(!contents.contains("shh!"));
        assert!(!contents.contains("do-not-leak"));
        assert!(contents.contains("[REDACTED]"));
        Ok(())
    }

    #[test]
    fn redact_secrets_does_not_mutate_secret_free_input() {
        let input = "relay wss://relay.example connected for npub1public";
        assert_eq!(redact_secrets(input), input);
    }
}
