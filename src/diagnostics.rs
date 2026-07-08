use crate::config::{BotConfig, SigningConfig, redact_bunker_uri};
use crate::errors::DaemonError;
use chrono::{DateTime, Utc};
use percent_encoding::percent_decode_str;
use regex::Regex;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, RwLockWriteGuard, watch};
use tokio_tungstenite::connect_async;

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
    pub bot_id: String,
    /// Bot Nostr public key (`npub1...`).
    pub npub: String,
    /// Number of configured relays for this bot.
    pub relay_count: u64,
    /// Configured relay URLs for this bot.
    pub relays: Vec<String>,
    /// Whether the NIP-46 bunker signer is currently connected.
    pub bunker_connected: bool,
    /// Configured signer backend label (`nsec`, `bunker_local`, `bunker_remote`).
    pub signer_backend: String,
    /// Optional stable error state for the bot identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A single redacted error entry retained in diagnostics.
///
/// The `data` field is stored as its redacted JSON serialization so that
/// arbitrary structured context can be preserved without leaking secrets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorRecord {
    /// Optional stable error code reported by the handler.
    pub code: String,
    /// Human-readable, redacted error message.
    pub message: String,
    /// Optional redacted JSON serialization of opaque structured context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
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
    /// Total events successfully decrypted from gift wraps.
    pub events_decrypted_total: u64,
    /// Total events dispatched to handlers.
    pub events_dispatched_total: u64,
    /// Total handler responses received for dispatched events.
    pub handler_responses_total: u64,
    /// Total events dropped due to rate limiting.
    pub rate_limited_total: u64,
    /// Total relay reconnections observed across all bots.
    pub relay_reconnects_total: u64,
    /// Total NIP-46 bunker signing failures observed across all bots.
    pub bunker_sign_failures_total: u64,
    /// Total incoming events rejected due to failed signature verification.
    pub invalid_events_total: u64,
    /// Total reply DMs that failed to publish.
    pub reply_send_failed_total: u64,
    /// Total plain DMs attempted.
    pub send_dm_total: u64,
    /// Total plain DMs that failed to publish.
    pub send_dm_failed_total: u64,
    /// Per-bot health summaries.
    pub bots: Vec<BotHealth>,
    /// Recent redacted error records, oldest first.
    pub errors: Vec<ErrorRecord>,
    /// UTC timestamp when this snapshot was produced.
    pub reported_at: DateTime<Utc>,
    /// Activity counts in the last 10 minutes.
    pub recent_counts: RecentCounts,
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
            events_decrypted_total: 0,
            events_dispatched_total: 0,
            handler_responses_total: 0,
            rate_limited_total: 0,
            relay_reconnects_total: 0,
            bunker_sign_failures_total: 0,
            invalid_events_total: 0,
            reply_send_failed_total: 0,
            send_dm_total: 0,
            send_dm_failed_total: 0,
            bots: Vec::new(),
            errors: Vec::new(),
            reported_at: now,
            recent_counts: RecentCounts::default(),
        }
    }
}

impl HealthSnapshot {
    /// Create a fresh initializing snapshot.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Activity counts observed in a recent time window.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct RecentCounts {
    /// Number of events received from Nostr relays.
    pub events_received: u64,
    /// Number of gift-wrap events successfully decrypted.
    pub events_decrypted: u64,
    /// Number of events dispatched to registered handlers.
    pub events_dispatched: u64,
    /// Number of handler responses received.
    pub handler_responses: u64,
    /// Number of reply DMs attempted.
    pub replies: u64,
    /// Number of reply DMs that failed to publish.
    pub reply_send_failed: u64,
    /// Number of plain DMs attempted.
    pub send_dm: u64,
    /// Number of plain DMs that failed to publish.
    pub send_dm_failed: u64,
    /// Length of the window in seconds that these counts cover.
    #[serde(default)]
    pub window_seconds: u64,
}

/// Fixed-size sliding-window counters indexed by minute.
#[derive(Debug, Clone)]
struct RecentBuckets {
    start: Instant,
    buckets: Vec<u64>,
    current_minute: usize,
    capacity: usize,
}

impl RecentBuckets {
    fn new(capacity_minutes: usize) -> Self {
        Self {
            start: Instant::now(),
            buckets: vec![0; capacity_minutes],
            current_minute: 0,
            capacity: capacity_minutes,
        }
    }

    fn record(&mut self) {
        let minute = self.start.elapsed().as_secs() as usize / 60;
        if minute >= self.current_minute + self.capacity {
            // Window has rolled beyond capacity; reset all buckets.
            self.buckets.fill(0);
            self.current_minute = minute;
        } else if minute > self.current_minute {
            // Zero buckets for the minutes that passed since the last write.
            for m in (self.current_minute + 1)..=minute {
                self.buckets[m % self.capacity] = 0;
            }
            self.current_minute = minute;
        }
        self.buckets[self.current_minute % self.capacity] += 1;
    }

    fn count_last_n_minutes(&self, n: usize) -> u64 {
        if n == 0 {
            return 0;
        }
        let current = self.start.elapsed().as_secs() as usize / 60;
        if current >= self.current_minute + self.capacity {
            // No writes in the last `capacity` minutes of real time.
            return 0;
        }
        let cidx = self.current_minute % self.capacity;
        let window_start = current.saturating_sub(n - 1) as i64;
        let current_i64 = current as i64;
        self.buckets
            .iter()
            .enumerate()
            .filter_map(|(idx, &count)| {
                let offset = (cidx + self.capacity - idx) % self.capacity;
                let bucket_minute = self.current_minute as i64 - offset as i64;
                if bucket_minute >= window_start && bucket_minute <= current_i64 {
                    Some(count)
                } else {
                    None
                }
            })
            .sum()
    }
}

#[derive(Debug)]
struct Inner {
    snapshot: HealthSnapshot,
    startup_instant: Instant,
    errors: VecDeque<ErrorRecord>,
    metrics_tx: watch::Sender<HealthSnapshot>,
    received: RecentBuckets,
    dispatched: RecentBuckets,
    replies: RecentBuckets,
    reply_failed: RecentBuckets,
    send_dm: RecentBuckets,
    send_dm_failed: RecentBuckets,
    decrypted: RecentBuckets,
    handler_responses: RecentBuckets,
}

/// Thread-safe diagnostics aggregator.
#[derive(Debug, Clone)]
pub struct Diagnostics {
    inner: Arc<RwLock<Inner>>,
    metrics_tx: watch::Sender<HealthSnapshot>,
}

impl Default for Diagnostics {
    fn default() -> Self {
        Self::new()
    }
}

impl Diagnostics {
    /// Create a new diagnostics aggregator.
    pub fn new() -> Self {
        let (metrics_tx, _) = watch::channel(HealthSnapshot::default());
        Self {
            inner: Arc::new(RwLock::new(Inner {
                snapshot: HealthSnapshot::default(),
                startup_instant: Instant::now(),
                errors: VecDeque::with_capacity(ERROR_BUFFER_CAPACITY),
                metrics_tx: metrics_tx.clone(),
                received: RecentBuckets::new(60),
                dispatched: RecentBuckets::new(60),
                replies: RecentBuckets::new(60),
                reply_failed: RecentBuckets::new(60),
                send_dm: RecentBuckets::new(60),
                send_dm_failed: RecentBuckets::new(60),
                decrypted: RecentBuckets::new(60),
                handler_responses: RecentBuckets::new(60),
            })),
            metrics_tx,
        }
    }

    /// Return a current snapshot with `reported_at` and `uptime_seconds`
    /// refreshed.
    pub async fn snapshot(&self) -> HealthSnapshot {
        let mut inner = write_guard(&self.inner).await;
        let now = Utc::now();
        inner.snapshot.reported_at = now;
        inner.snapshot.uptime_seconds = inner.startup_instant.elapsed().as_secs();
        inner.snapshot.recent_counts = RecentCounts {
            events_received: inner.received.count_last_n_minutes(10),
            events_decrypted: inner.decrypted.count_last_n_minutes(10),
            events_dispatched: inner.dispatched.count_last_n_minutes(10),
            handler_responses: inner.handler_responses.count_last_n_minutes(10),
            replies: inner.replies.count_last_n_minutes(10),
            reply_send_failed: inner.reply_failed.count_last_n_minutes(10),
            send_dm: inner.send_dm.count_last_n_minutes(10),
            send_dm_failed: inner.send_dm_failed.count_last_n_minutes(10),
            window_seconds: 600,
        };
        let snap = inner.snapshot.clone();
        let _ = inner.metrics_tx.send(snap.clone());
        snap
    }

    /// Replace the per-bot health summaries.
    pub async fn set_bots(&self, bots: Vec<BotHealth>) {
        self.with_snapshot(|snapshot| snapshot.bots = bots).await;
    }

    /// Set the daemon lifecycle status.
    pub async fn set_status(&self, status: DaemonStatus) {
        self.with_snapshot(|snapshot| snapshot.status = status)
            .await;
    }

    /// Increment the counter for events received from Nostr relays.
    pub async fn record_event_received(&self) {
        let mut inner = write_guard(&self.inner).await;
        inner.snapshot.events_received_total += 1;
        inner.received.record();
        let snap = inner.snapshot.clone();
        let _ = inner.metrics_tx.send(snap);
    }

    /// Increment the counter for events dispatched to registered handlers.
    pub async fn record_event_dispatched(&self) {
        let mut inner = write_guard(&self.inner).await;
        inner.snapshot.events_dispatched_total += 1;
        inner.dispatched.record();
        let snap = inner.snapshot.clone();
        let _ = inner.metrics_tx.send(snap);
    }

    /// Increment the counter for rate-limited events.
    pub async fn record_rate_limited(&self) {
        self.with_snapshot(|snapshot| snapshot.rate_limited_total += 1)
            .await;
    }

    /// Increment the counter for relay reconnections.
    pub async fn record_relay_reconnect(&self) {
        self.with_snapshot(|snapshot| snapshot.relay_reconnects_total += 1)
            .await;
    }

    /// Increment the counter for bunker signing failures.
    pub async fn record_bunker_sign_failure(&self) {
        self.with_snapshot(|snapshot| snapshot.bunker_sign_failures_total += 1)
            .await;
    }

    /// Increment the counter for events successfully decrypted from gift wraps.
    pub async fn record_event_decrypted(&self) {
        let mut inner = write_guard(&self.inner).await;
        inner.snapshot.events_decrypted_total += 1;
        inner.decrypted.record();
        let snap = inner.snapshot.clone();
        let _ = inner.metrics_tx.send(snap);
    }

    /// Increment the counter for handler responses received after dispatch.
    pub async fn record_handler_response(&self) {
        let mut inner = write_guard(&self.inner).await;
        inner.snapshot.handler_responses_total += 1;
        inner.handler_responses.record();
        let snap = inner.snapshot.clone();
        let _ = inner.metrics_tx.send(snap);
    }

    /// Increment the counter for events rejected due to failed verification.
    pub async fn record_invalid_event(&self) {
        self.with_snapshot(|snapshot| snapshot.invalid_events_total += 1)
            .await;
    }

    /// Record that a reply DM was attempted.
    pub async fn record_reply(&self) {
        let mut inner = write_guard(&self.inner).await;
        inner.replies.record();
        let snap = inner.snapshot.clone();
        let _ = inner.metrics_tx.send(snap);
    }

    /// Increment the counter for reply DMs that failed to publish.
    pub async fn record_reply_send_failed(&self) {
        let mut inner = write_guard(&self.inner).await;
        inner.snapshot.reply_send_failed_total += 1;
        inner.reply_failed.record();
        let snap = inner.snapshot.clone();
        let _ = inner.metrics_tx.send(snap);
    }

    /// Record that a plain DM was attempted.
    pub async fn record_send_dm(&self) {
        let mut inner = write_guard(&self.inner).await;
        inner.snapshot.send_dm_total += 1;
        inner.send_dm.record();
        let snap = inner.snapshot.clone();
        let _ = inner.metrics_tx.send(snap);
    }

    /// Increment the counter for plain DMs that failed to publish.
    pub async fn record_send_dm_failed(&self) {
        let mut inner = write_guard(&self.inner).await;
        inner.snapshot.send_dm_failed_total += 1;
        inner.send_dm_failed.record();
        let snap = inner.snapshot.clone();
        let _ = inner.metrics_tx.send(snap);
    }

    /// Set the number of registered handlers.
    pub async fn set_handlers_registered(&self, count: u64) {
        self.with_snapshot(|snapshot| snapshot.handlers_registered = count)
            .await;
    }

    /// Record a recent error message.
    ///
    /// The message and optional structured `data` are redacted before storage
    /// so that secrets (`nsec1...`, query parameters such as `secret=...`,
    /// `token=...`) never appear in snapshots or on-disk reports.
    pub async fn record_error(&self, code: Option<&str>, message: &str, data: Option<&Value>) {
        let code = code.unwrap_or("unknown").to_string();
        let redacted_message = redact_secrets(message);
        let redacted_data =
            data.map(|d| redact_secrets(&serde_json::to_string(d).unwrap_or_default()));
        let record = ErrorRecord {
            code,
            message: redacted_message,
            data: redacted_data,
        };
        let mut inner = write_guard(&self.inner).await;
        if inner.errors.len() >= ERROR_BUFFER_CAPACITY {
            inner.errors.pop_front();
        }
        inner.errors.push_back(record);
        inner.snapshot.errors = inner.errors.iter().cloned().collect();
    }

    /// Atomically write the current snapshot to
    /// `<data_dir>/reports/latest.json`.
    pub async fn flush_report(&self, data_dir: &Path) -> Result<(), DaemonError> {
        let snapshot = self.snapshot().await;
        let reports_dir = data_dir.join("reports");
        tokio::fs::create_dir_all(&reports_dir).await?;

        let tmp_name = format!("latest.json.tmp.{}", uuid::Uuid::new_v4());
        let tmp_path = reports_dir.join(tmp_name);
        let final_path = reports_dir.join("latest.json");

        let json = serde_json::to_string_pretty(&snapshot)?;

        // Open the temp file with owner-only permissions *before* writing any
        // diagnostic data. On Unix this sets the mode directly, so a permissive
        // umask cannot expose the report during the write.
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)
            .await?;

        #[cfg(unix)]
        {
            use std::fs::Permissions;
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&tmp_path, Permissions::from_mode(0o600)).await?;
        }

        use tokio::io::AsyncWriteExt;
        file.write_all(json.as_bytes()).await?;
        file.shutdown().await?;
        drop(file.into_std().await);

        tokio::fs::rename(&tmp_path, &final_path).await?;

        // Defensive: ensure the final path is also owner-only, in case the
        // rename crossed a mount point or the temp file's permissions were
        // altered.
        #[cfg(unix)]
        {
            use std::fs::Permissions;
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&final_path, Permissions::from_mode(0o600)).await?;
        }

        Ok(())
    }

    /// Subscribe to live metrics updates.
    ///
    /// The returned receiver is notified every time the daemon updates the
    /// health snapshot, including periodic metrics broadcasts.
    pub fn subscribe_metrics(&self) -> watch::Receiver<HealthSnapshot> {
        self.metrics_tx.subscribe()
    }

    async fn with_snapshot<F>(&self, f: F)
    where
        F: FnOnce(&mut HealthSnapshot),
    {
        let mut inner = write_guard(&self.inner).await;
        f(&mut inner.snapshot);
        let snap = inner.snapshot.clone();
        let _ = inner.metrics_tx.send(snap);
    }
}

/// Result of probing a single relay for WebSocket connectivity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayCheck {
    /// Daemon-local bot label.
    pub bot_id: String,
    /// Relay URL that was probed.
    pub relay: String,
    /// Whether the relay accepted the WebSocket upgrade.
    pub reachable: bool,
    /// Optional error description when the relay is unreachable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Result of probing a single NIP-46 bunker relay for connectivity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BunkerCheck {
    /// Daemon-local bot label.
    pub bot_id: String,
    /// Whether the bunker relay accepted the WebSocket upgrade.
    pub reachable: bool,
    /// Optional error description when the bunker relay is unreachable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Probe WebSocket connectivity for every configured relay of `bot`.
pub async fn check_relay_connectivity(bot: &BotConfig) -> Vec<RelayCheck> {
    let mut checks = Vec::new();
    for relay in &bot.relays {
        let trimmed = relay.trim();
        if trimmed.is_empty() {
            continue;
        }
        let result = tokio::time::timeout(Duration::from_secs(3), try_ws_connect(trimmed)).await;
        let (reachable, error) = match result {
            Ok(Ok(())) => (true, None),
            Ok(Err(e)) => (false, Some(e.to_string())),
            Err(_) => (false, Some("connection timed out".to_string())),
        };
        checks.push(RelayCheck {
            bot_id: bot.id.clone(),
            relay: trimmed.to_string(),
            reachable,
            error,
        });
    }
    checks
}

/// Attempt a WebSocket upgrade to `url`.
pub async fn try_ws_connect(url: &str) -> Result<(), DaemonError> {
    if !url.starts_with("ws://") && !url.starts_with("wss://") {
        return Err(DaemonError::Config(format!("not a websocket url: {url}")));
    }
    let _ = connect_async(url)
        .await
        .map_err(|e| DaemonError::Nostr(format!("ws connect failed: {e}")))?;
    Ok(())
}

/// Probe WebSocket connectivity for the NIP-46 bunker relay of `bot`, if any.
pub async fn check_bunker_connectivity(bot: &BotConfig) -> Option<BunkerCheck> {
    let uri = match &bot.signing {
        SigningConfig::BunkerLocal { uri } | SigningConfig::BunkerRemote { uri } => {
            uri.expose_secret().to_string()
        }
        _ => return None,
    };
    let (reachable, error) = match parse_bunker_relay(&uri) {
        Ok(relay) => {
            match tokio::time::timeout(Duration::from_secs(3), try_ws_connect(&relay)).await {
                Ok(Ok(())) => (true, None),
                Ok(Err(e)) => (false, Some(e.to_string())),
                Err(_) => (false, Some("connection timed out".to_string())),
            }
        }
        Err(e) => (false, Some(e.to_string())),
    };
    Some(BunkerCheck {
        bot_id: bot.id.clone(),
        reachable,
        error,
    })
}

/// Extract the relay URL from a `bunker://` URI.
pub fn parse_bunker_relay(uri: &str) -> Result<String, DaemonError> {
    let redacted = redact_bunker_uri(uri);
    let after_scheme = uri.strip_prefix("bunker://").ok_or_else(|| {
        DaemonError::Config(format!("bunker uri missing bunker:// scheme: {redacted}"))
    })?;
    let idx = after_scheme.find("?relay=").ok_or_else(|| {
        DaemonError::Config(format!("bunker uri missing relay param: {redacted}"))
    })?;
    let relay_start = idx + "?relay=".len();
    let encoded_relay = after_scheme[relay_start..].split('&').next().unwrap_or("");
    if encoded_relay.is_empty() {
        return Err(DaemonError::Config(
            "bunker uri relay param is empty".into(),
        ));
    }
    let relay = percent_decode_str(encoded_relay)
        .decode_utf8()
        .map_err(|e| {
            DaemonError::Config(format!("bunker uri relay param is not valid UTF-8: {e}"))
        })?
        .to_string();
    if relay.is_empty() {
        return Err(DaemonError::Config(
            "bunker uri relay param is empty".into(),
        ));
    }
    Ok(relay)
}

async fn write_guard<'a, T>(lock: &'a RwLock<T>) -> RwLockWriteGuard<'a, T> {
    lock.write().await
}

/// Precompiled redaction regexes, built once per process.
///
/// The patterns are static constants; failures here are a programming bug, not
/// a runtime condition, so we intentionally fail fast during initialization.
static NSEC1_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used, clippy::panic)]
    {
        Regex::new(r"(?i)\bnsec1[A-Za-z0-9]*").expect("nsec1 regex is valid")
    }
});

static HEX_SECRET_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used, clippy::panic)]
    {
        Regex::new(r"[0-9a-fA-F]{64}").expect("hex secret regex is valid")
    }
});

static QUERY_PARAM_REGEXES: LazyLock<HashMap<&'static str, Regex>> = LazyLock::new(|| {
    const VALUE: &str = r"(?:[^&\s%]|%([013-9A-Fa-f][0-9A-Fa-f]|2[0-57A-Fa-f]))*";
    const KEYS: &[&str] = &[
        "secret",
        "token",
        "api_key",
        "password",
        "priv_key",
        "api_secret",
        "private_key",
        "auth_token",
        "access_token",
        "refresh_token",
        "client_secret",
        "secret_key",
    ];
    #[allow(clippy::expect_used, clippy::panic)]
    {
        KEYS.iter()
            .map(|key| {
                let pattern = format!(
                    r"(?i)(?P<pre>^|&|%26|[^A-Za-z0-9_])(?P<key>{})(?P<sep>=|%3D)(?P<value>{})",
                    regex::escape(key),
                    VALUE
                );
                (
                    *key,
                    Regex::new(&pattern).expect("query param regex is valid"),
                )
            })
            .collect()
    }
});

static BEARER_TOKEN_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used, clippy::panic)]
    {
        Regex::new(r"(?i)(^|[^A-Za-z0-9_])(bearer)([ \t]+)([^\s]+)")
            .expect("bearer token regex is valid")
    }
});

static AUTHORIZATION_HEADER_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used, clippy::panic)]
    {
        Regex::new(r"(?i)(^|[^A-Za-z0-9_])(authorization)([ \t]*:)(?:[ \t]*)([^\r\n]*)")
            .expect("authorization header regex is valid")
    }
});

/// Redact likely secret material from a diagnostic message.
///
/// This is a best-effort filter; application code should avoid logging
/// secrets in the first place.
fn redact_secrets(input: &str) -> String {
    let mut out = redact_word_prefix(input, "nsec1");
    out = redact_hex_secret(&out);
    for key in &[
        "secret",
        "token",
        "api_key",
        "password",
        "priv_key",
        "api_secret",
        "private_key",
        "auth_token",
        "access_token",
        "refresh_token",
        "client_secret",
        "secret_key",
    ] {
        out = redact_query_param(&out, key);
    }
    out = redact_bearer_token(&out);
    out = redact_authorization_header(&out);
    redact_mls_paths(&out)
}

/// Redact the path to per-bot MLS databases so their content and location
/// are not preserved in diagnostics.
fn redact_mls_paths(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut rest = input;
    const FILENAME: &str = "vector-mls.db";

    while let Some(pos) = rest.find(FILENAME) {
        // Find the beginning of the filesystem path by scanning backward over
        // path characters. This redacts the entire per-bot directory
        // (data_dir/<bot_id>/) rather than just the filename.
        let path_start = rest[..pos]
            .rfind(|c: char| !is_path_char(c))
            .map(|idx| idx + 1)
            .unwrap_or(0);

        result.push_str(&rest[..path_start]);
        result.push_str("[REDACTED]/");
        result.push_str(FILENAME);
        rest = &rest[pos + FILENAME.len()..];
    }
    result.push_str(rest);
    result
}

/// Return true for characters that can appear in a filesystem path component.
fn is_path_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '/' | '\\' | '_' | '-' | '~' | '%')
}

/// Redact a contiguous alphanumeric token that starts with `prefix`.
fn redact_word_prefix(input: &str, prefix: &str) -> String {
    if prefix == "nsec1" {
        return NSEC1_REGEX.replace_all(input, "[REDACTED]").into_owned();
    }
    let Ok(re) = Regex::new(&format!(r"(?i)\b{}[A-Za-z0-9]*", regex::escape(prefix))) else {
        return input.to_string();
    };
    re.replace_all(input, "[REDACTED]").into_owned()
}

/// Redact the value of a URL-style query parameter `key=`.
///
/// Handles case-insensitive keys and percent-encoded separators (`%3D`)
/// and delimiters (`%26`).
fn redact_query_param(input: &str, key: &str) -> String {
    let Some(re) = QUERY_PARAM_REGEXES.get(key) else {
        return input.to_string();
    };
    re.replace_all(input, "${pre}${key}${sep}[REDACTED]")
        .into_owned()
}

/// Redact 64-character hexadecimal sequences (raw secp256k1 private keys).
fn redact_hex_secret(input: &str) -> String {
    HEX_SECRET_REGEX
        .replace_all(input, "[REDACTED]")
        .into_owned()
}

/// Redact a `bearer <token>` token, case-insensitively.
fn redact_bearer_token(input: &str) -> String {
    BEARER_TOKEN_REGEX
        .replace_all(input, "${1}${2}${3}[REDACTED]")
        .into_owned()
}

/// Redact an `authorization: <value>` header, case-insensitively.
fn redact_authorization_header(input: &str) -> String {
    AUTHORIZATION_HEADER_REGEX
        .replace_all(input, "${1}${2}${3} [REDACTED]")
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn default_snapshot_initializes_counters_to_zero() {
        let diag = Diagnostics::new();
        let snap = diag.snapshot().await;
        assert_eq!(snap.status, DaemonStatus::Initializing);
        assert_eq!(snap.events_received_total, 0);
        assert_eq!(snap.events_dispatched_total, 0);
        assert_eq!(snap.rate_limited_total, 0);
        assert_eq!(snap.relay_reconnects_total, 0);
        assert_eq!(snap.bunker_sign_failures_total, 0);
        assert!(snap.bots.is_empty());
        assert!(snap.errors.is_empty());
    }

    #[tokio::test]
    async fn counters_increment_independently() {
        let diag = Diagnostics::new();
        diag.record_event_received().await;
        diag.record_event_received().await;
        diag.record_event_dispatched().await;
        diag.record_rate_limited().await;
        diag.record_relay_reconnect().await;
        diag.record_bunker_sign_failure().await;
        diag.record_bunker_sign_failure().await;
        diag.set_handlers_registered(5).await;

        let snap = diag.snapshot().await;
        assert_eq!(snap.events_received_total, 2);
        assert_eq!(snap.events_dispatched_total, 1);
        assert_eq!(snap.rate_limited_total, 1);
        assert_eq!(snap.relay_reconnects_total, 1);
        assert_eq!(snap.bunker_sign_failures_total, 2);
        assert_eq!(snap.handlers_registered, 5);
    }

    #[tokio::test]
    async fn status_transitions_are_reflected() {
        let diag = Diagnostics::new();
        assert_eq!(diag.snapshot().await.status, DaemonStatus::Initializing);

        diag.set_status(DaemonStatus::Ready).await;
        assert_eq!(diag.snapshot().await.status, DaemonStatus::Ready);

        diag.set_status(DaemonStatus::ShuttingDown).await;
        assert_eq!(diag.snapshot().await.status, DaemonStatus::ShuttingDown);

        diag.set_status(DaemonStatus::Stopped).await;
        assert_eq!(diag.snapshot().await.status, DaemonStatus::Stopped);
    }

    #[tokio::test]
    async fn bots_are_stored_in_snapshot() {
        let diag = Diagnostics::new();
        diag.set_bots(vec![
            BotHealth {
                bot_id: "bot-a".into(),
                npub: "npub1example".into(),
                relay_count: 3,
                relays: vec![
                    "wss://a1.example".into(),
                    "wss://a2.example".into(),
                    "wss://a3.example".into(),
                ],
                bunker_connected: true,
                signer_backend: "bunker_local".into(),
                error: None,
            },
            BotHealth {
                bot_id: "bot-b".into(),
                npub: "npub1other".into(),
                relay_count: 0,
                relays: vec![],
                bunker_connected: false,
                signer_backend: "nsec".into(),
                error: None,
            },
        ])
        .await;

        let snap = diag.snapshot().await;
        assert_eq!(snap.bots.len(), 2);
        assert_eq!(snap.bots[0].bot_id, "bot-a");
        assert_eq!(snap.bots[1].relay_count, 0);
    }

    #[tokio::test]
    async fn errors_are_redacted_in_snapshot() {
        let diag = Diagnostics::new();
        diag.record_error(
            Some("sign_failed"),
            "signing failed for nsec1deadbeef1234 on bot-a",
            None,
        )
        .await;
        diag.record_error(
            None,
            "bunker uri: bunker://relay.example?secret=supersecret&token=abc123",
            None,
        )
        .await;

        let snap = diag.snapshot().await;
        assert_eq!(snap.errors.len(), 2);
        let joined = snap
            .errors
            .iter()
            .map(|e| format!("{} {}", e.code, e.message))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!joined.contains("nsec1deadbeef1234"));
        assert!(!joined.contains("supersecret"));
        assert!(!joined.contains("abc123"));
        assert!(joined.contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn error_buffer_drops_oldest_messages() {
        let diag = Diagnostics::new();
        for i in 0..ERROR_BUFFER_CAPACITY + 5 {
            diag.record_error(None, &format!("error {i}"), None).await;
        }
        let snap = diag.snapshot().await;
        assert_eq!(snap.errors.len(), ERROR_BUFFER_CAPACITY);
        assert!(snap.errors[0].message.contains("error 5"));
        let last = snap.errors.iter().last();
        assert!(last.is_some_and(|e| {
            e.message
                .contains(&format!("error {}", ERROR_BUFFER_CAPACITY + 4))
        }));
    }

    #[tokio::test]
    async fn flush_report_round_trips() -> Result<(), DaemonError> {
        let tmp = tempfile::tempdir()?;
        let diag = Diagnostics::new();
        diag.set_status(DaemonStatus::Ready).await;
        diag.record_event_received().await;
        diag.set_bots(vec![BotHealth {
            bot_id: "bot-x".into(),
            npub: "npub1x".into(),
            relay_count: 2,
            relays: vec!["wss://x1.example".into(), "wss://x2.example".into()],
            bunker_connected: true,
            signer_backend: "bunker_remote".into(),
            error: None,
        }])
        .await;

        diag.flush_report(tmp.path()).await?;

        let report_path = tmp.path().join("reports").join("latest.json");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = tokio::fs::metadata(&report_path)
                .await?
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "latest.json should be owner-only");
        }

        let contents = tokio::fs::read_to_string(&report_path).await?;
        let parsed: HealthSnapshot = serde_json::from_str(&contents)?;
        assert_eq!(parsed.status, DaemonStatus::Ready);
        assert_eq!(parsed.events_received_total, 1);
        assert_eq!(parsed.bots.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn flushed_report_contains_no_secrets() -> Result<(), DaemonError> {
        let tmp = tempfile::tempdir()?;
        let diag = Diagnostics::new();
        diag.record_error(None, "leaked nsec1verysecretandlonghexstring", None)
            .await;
        diag.record_error(None, "bunker secret=shh! token=do-not-leak", None)
            .await;
        diag.flush_report(tmp.path()).await?;

        let report_path = tmp.path().join("reports").join("latest.json");
        let contents = tokio::fs::read_to_string(&report_path).await?;
        assert!(!contents.contains("nsec1verysecretandlonghexstring"));
        assert!(!contents.contains("shh!"));
        assert!(!contents.contains("do-not-leak"));
        assert!(contents.contains("[REDACTED]"));
        Ok(())
    }

    #[tokio::test]
    async fn flush_report_runs_concurrently_with_updates() {
        let tmp = tempfile::tempdir().unwrap();
        let diag = Diagnostics::new();

        let mut interval = tokio::time::interval(std::time::Duration::from_millis(5));
        let ticks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ticks_clone = Arc::clone(&ticks);
        let timer = tokio::spawn(async move {
            for _ in 0..50 {
                interval.tick().await;
                ticks_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });

        let mut flushes = Vec::new();
        for i in 0..20u64 {
            let diag = diag.clone();
            let dir = tmp.path().to_path_buf();
            flushes.push(tokio::spawn(async move {
                diag.set_handlers_registered(i).await;
                diag.flush_report(&dir).await.unwrap();
            }));
        }
        let _ = futures::future::join_all(flushes).await;
        timer.await.unwrap();

        let tick_count = ticks.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            tick_count >= 45,
            "runtime was blocked during flush_report; only {tick_count} timer ticks fired"
        );

        let report_path = tmp.path().join("reports").join("latest.json");
        let contents = tokio::fs::read_to_string(&report_path).await.unwrap();
        let parsed: HealthSnapshot = serde_json::from_str(&contents).unwrap();
        // The last registered count should be one of the values set during the race.
        assert!(parsed.handlers_registered <= 19);
    }

    #[test]
    fn redact_secrets_does_not_mutate_secret_free_input() {
        let input = "relay wss://relay.example connected for npub1public";
        assert_eq!(redact_secrets(input), input);
    }

    #[test]
    fn redact_secrets_masks_required_patterns() {
        assert_eq!(redact_secrets("nsec1deadbeef"), "[REDACTED]");
        assert_eq!(redact_secrets("secret=supersecret"), "secret=[REDACTED]");
        assert_eq!(redact_secrets("token=tokentoken"), "token=[REDACTED]");
        assert_eq!(redact_secrets("bearer bearertoken"), "bearer [REDACTED]");
        assert_eq!(
            redact_secrets("authorization: authvalue"),
            "authorization: [REDACTED]"
        );
        assert_eq!(redact_secrets("api_key=apikeyvalue"), "api_key=[REDACTED]");
        assert_eq!(
            redact_secrets("password=passwordvalue"),
            "password=[REDACTED]"
        );
        assert_eq!(
            redact_secrets("priv_key=privkeyvalue"),
            "priv_key=[REDACTED]"
        );
    }

    #[test]
    fn redact_secrets_masks_hex_private_key() {
        let hex = "a".repeat(64);
        let input = format!("raw key {hex} trailing");
        let out = redact_secrets(&input);
        assert!(!out.contains(&hex));
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn redact_secrets_masks_common_secret_identifiers() {
        let input = "api_secret=foo private_key=bar auth_token=baz access_token=qux refresh_token=quux client_secret=corge secret_key=grault";
        let out = redact_secrets(input);
        for secret in &["foo", "bar", "baz", "qux", "quux", "corge", "grault"] {
            assert!(
                !out.contains(secret),
                "secret '{secret}' leaked in redacted output: {out}"
            );
        }
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn redact_secrets_is_case_insensitive_for_bearer_and_authorization() {
        let out = redact_secrets("Authorization: Bearer abc123");
        assert!(!out.contains("abc123"));
        assert!(out.contains("Authorization: [REDACTED]"));
    }

    #[test]
    fn redact_secrets_handles_case_insensitive_query_params() {
        let out = redact_secrets("Secret=supersecret&Api_Key=keyvalue TOKEN=tokvalue");
        assert!(!out.contains("supersecret"));
        assert!(!out.contains("keyvalue"));
        assert!(!out.contains("tokvalue"));
        assert!(out.contains("Secret=[REDACTED]"));
        assert!(out.contains("Api_Key=[REDACTED]"));
        assert!(out.contains("TOKEN=[REDACTED]"));
    }

    #[test]
    fn redact_secrets_handles_url_encoded_query_params() {
        let out = redact_secrets("secret%3Dsupersecret%26token%3Dabc123");
        assert!(!out.contains("supersecret"));
        assert!(!out.contains("abc123"));
        assert!(out.contains("secret%3D[REDACTED]"));
        assert!(out.contains("token%3D[REDACTED]"));
    }

    #[test]
    fn redact_secrets_handles_json_escaped_nsec() {
        let input = r#"{"key":"nsec1deadbeef1234"}"#;
        let out = redact_secrets(input);
        assert!(!out.contains("nsec1deadbeef1234"));
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn redact_secrets_does_not_over_redact_innocuous_substrings() {
        assert_eq!(redact_secrets("mysecret=foo"), "mysecret=foo");
        assert_eq!(redact_secrets("npub1public"), "npub1public");
    }

    #[test]
    fn redact_secrets_does_not_panic_with_non_ascii_prefix() {
        // U+0130 (Latin capital I with dot above) lowercases to a single
        // ASCII byte, so the old Unicode-lowercase search would return a byte
        // index that is not a valid char boundary in the original string.
        let input = "İBearer abc123";
        let out = redact_secrets(input);
        assert!(!out.contains("abc123"));
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn redact_mls_db_path() {
        let input = "storage error for /data/bots/squad/vector-mls.db";
        let out = redact_secrets(input);
        assert!(
            !out.contains("/data/bots/squad/"),
            "redacted output still contains per-bot directory: {out}"
        );
        assert!(
            !out.contains("/data/bots/squad/vector-mls.db"),
            "redacted output still contains mls db path: {out}"
        );
        assert!(
            out.contains("[REDACTED]/vector-mls.db"),
            "missing redaction marker: {out}"
        );
        assert_eq!(out, "storage error for [REDACTED]/vector-mls.db");
    }

    #[test]
    fn parse_bunker_relay_extracts_relay_url() {
        let uri = "bunker://deadbeef?relay=ws://127.0.0.1:4848&secret=shh";
        assert_eq!(parse_bunker_relay(uri).unwrap(), "ws://127.0.0.1:4848");
    }

    #[test]
    fn parse_bunker_relay_url_decodes_relay_param() {
        let uri = "bunker://deadbeef?relay=wss%3A%2F%2Frelay.nsec.app&secret=shh";
        assert_eq!(parse_bunker_relay(uri).unwrap(), "wss://relay.nsec.app");
    }

    #[test]
    fn parse_bunker_relay_error_redacts_query_params() {
        let uri = "bunker://deadbeef?secret=topsecret";
        let err = parse_bunker_relay(uri).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("missing relay param"),
            "expected missing relay error: {msg}"
        );
        assert!(!msg.contains("topsecret"), "secret leaked in error: {msg}");
        assert!(
            msg.contains("secret=[REDACTED]"),
            "secret not redacted: {msg}"
        );
    }

    #[test]
    fn parse_bunker_relay_error_redacts_non_bunker_uri() {
        let err = parse_bunker_relay("http://deadbeef?token=secrettoken").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("missing bunker:// scheme"),
            "expected scheme error: {msg}"
        );
        assert!(!msg.contains("secrettoken"), "token leaked in error: {msg}");
        assert!(
            msg.contains("token=[REDACTED]"),
            "token not redacted: {msg}"
        );
    }

    #[test]
    fn redact_bunker_uri_masks_query_values() {
        let out = crate::config::redact_bunker_uri(
            "bunker://pubkey?relay=wss://relay.example&secret=shh&token=abc",
        );
        assert!(out.contains("bunker://pubkey"));
        assert!(out.contains("relay=[REDACTED]"));
        assert!(out.contains("secret=[REDACTED]"));
        assert!(out.contains("token=[REDACTED]"));
        assert!(!out.contains("wss://relay.example"));
        assert!(!out.contains("shh"));
        assert!(!out.contains("abc"));
    }

    #[test]
    fn count_last_n_minutes_returns_zero_after_long_idle() {
        let capacity = 10;
        // Simulate a write 5 minutes ago, then an idle gap of 10 minutes.
        let start = Instant::now() - Duration::from_secs(15 * 60);
        let mut buckets = RecentBuckets {
            start,
            buckets: vec![0; capacity],
            current_minute: 5,
            capacity,
        };
        buckets.buckets[5 % capacity] = 3;
        assert_eq!(buckets.count_last_n_minutes(10), 0);
        // Any shorter window also returns 0.
        assert_eq!(buckets.count_last_n_minutes(1), 0);
    }

    #[test]
    fn count_last_n_minutes_counts_real_window_after_short_idle() {
        let capacity = 10;
        // Last write at minute 12, current real minute is 15.
        let start = Instant::now() - Duration::from_secs(15 * 60 + 30);
        let mut buckets = RecentBuckets {
            start,
            buckets: vec![0; capacity],
            current_minute: 12,
            capacity,
        };
        buckets.buckets[12 % capacity] = 7; // minute 12
        buckets.buckets[11 % capacity] = 3; // minute 11
        buckets.buckets[10 % capacity] = 100; // minute 10, outside the window

        // Window [11, 15] should include minutes 11 and 12 only.
        assert_eq!(buckets.count_last_n_minutes(5), 10);
        // Window [15, 15] includes no writes.
        assert_eq!(buckets.count_last_n_minutes(1), 0);
        // Window [12, 15] includes minute 12 only.
        assert_eq!(buckets.count_last_n_minutes(4), 7);
    }

    #[test]
    fn count_last_n_minutes_works_for_active_recording() {
        let capacity = 10;
        // Set the clock so the current minute is 5.
        let start = Instant::now() - Duration::from_secs(5 * 60 + 10);
        let mut buckets = RecentBuckets::new(capacity);
        buckets.start = start;
        buckets.record();
        buckets.record();
        assert_eq!(buckets.current_minute, 5);
        assert_eq!(buckets.count_last_n_minutes(1), 2);
        assert_eq!(buckets.count_last_n_minutes(10), 2);
        assert_eq!(buckets.count_last_n_minutes(60), 2);
    }
}
