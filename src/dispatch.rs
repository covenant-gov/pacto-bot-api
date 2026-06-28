use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use nostr::EventId;
use serde_json::Value;
use tokio::sync::{Mutex as TokioMutex, RwLock, mpsc};
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::client_manager::ClientManager;
use crate::db::Database;
use crate::diagnostics::{DaemonStatus, Diagnostics};
use crate::errors::{DaemonError, JsonRpcError};
use crate::events::{AgentEvent, EventType};
use crate::handlers::ConnectionHandle;
use crate::transport::protocol::{JsonRpcMessage, Method, MetricsResponse, parse_method};

/// Maximum time to wait for handler responses before advancing the cursor.
const DISPATCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Default per-handler rate: 10 ops/sec.
const HANDLER_RATE: f64 = 10.0;
/// Default per-handler burst: 20 ops.
const HANDLER_BURST: f64 = 20.0;
/// Default per-bot aggregate rate: 30 ops/sec.
const BOT_RATE: f64 = 30.0;
/// Default per-bot aggregate burst: 60 ops.
const BOT_BURST: f64 = 60.0;

/// Action a handler can take in response to an event.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HandlerAction {
    Ack,
    Reply { content: String },
    Defer,
    Ignore,
}

impl HandlerAction {
    fn from_value(value: &Value) -> Result<Self, DaemonError> {
        let action = value
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| DaemonError::Config("handler.response missing action".into()))?;
        match action {
            "ack" => Ok(HandlerAction::Ack),
            "reply" => {
                let content = value
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        DaemonError::Config("handler.response reply missing content".into())
                    })?;
                Ok(HandlerAction::Reply {
                    content: content.to_string(),
                })
            }
            "defer" => Ok(HandlerAction::Defer),
            "ignore" => Ok(HandlerAction::Ignore),
            other => Err(DaemonError::Config(format!(
                "invalid handler action: {other}"
            ))),
        }
    }
}

#[derive(Debug)]
struct PendingDispatch {
    sender: mpsc::UnboundedSender<(String, HandlerAction)>,
}

/// Token bucket for rate limiting.
#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last_replenished: Instant,
    rate: f64,
    burst: f64,
}

impl Bucket {
    fn new(rate: f64, burst: f64) -> Self {
        Self {
            tokens: burst,
            last_replenished: Instant::now(),
            rate,
            burst,
        }
    }

    fn check(&mut self, now: Instant) -> bool {
        let elapsed = now.duration_since(self.last_replenished).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate).min(self.burst);
        self.last_replenished = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Rate limiter enforcing per-handler and per-bot token buckets.
#[derive(Debug)]
pub struct RateLimiter {
    handlers: TokioMutex<HashMap<String, Bucket>>,
    bots: TokioMutex<HashMap<String, Bucket>>,
    handler_rate: f64,
    handler_burst: f64,
    bot_rate: f64,
    bot_burst: f64,
}

impl RateLimiter {
    /// Create a rate limiter with the given per-handler and per-bot limits.
    pub fn new(handler_rate: f64, handler_burst: f64, bot_rate: f64, bot_burst: f64) -> Self {
        Self {
            handlers: TokioMutex::new(HashMap::new()),
            bots: TokioMutex::new(HashMap::new()),
            handler_rate,
            handler_burst,
            bot_rate,
            bot_burst,
        }
    }

    /// Check whether `handler_id` may perform a mutating operation on `bot_id`
    /// without exceeding rate limits. Returns `true` if allowed.
    pub async fn check(&self, handler_id: &str, bot_id: &str, now: Instant) -> bool {
        // Check bot aggregate limit first.
        let mut bots = self.bots.lock().await;
        let bot_bucket = bots
            .entry(bot_id.to_string())
            .or_insert_with(|| Bucket::new(self.bot_rate, self.bot_burst));
        if !bot_bucket.check(now) {
            return false;
        }
        drop(bots);

        let mut handlers = self.handlers.lock().await;
        let handler_bucket = handlers
            .entry(handler_id.to_string())
            .or_insert_with(|| Bucket::new(self.handler_rate, self.handler_burst));
        handler_bucket.check(now)
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(HANDLER_RATE, HANDLER_BURST, BOT_RATE, BOT_BURST)
    }
}

/// Event dispatch router.
#[derive(Debug)]
pub struct Dispatch {
    client_manager: Arc<RwLock<ClientManager>>,
    db: StdMutex<Database>,
    diagnostics: Diagnostics,
    rate_limiter: RateLimiter,
    pending: Arc<TokioMutex<HashMap<String, PendingDispatch>>>,
    handlers_registered: AtomicU64,
    last_cursor: Arc<TokioMutex<HashMap<String, (String, i64)>>>,
}

impl Dispatch {
    /// Create a new dispatch router with default rate limits.
    pub fn new(
        client_manager: Arc<RwLock<ClientManager>>,
        db: Database,
        diagnostics: Diagnostics,
    ) -> Self {
        Self {
            client_manager,
            db: StdMutex::new(db),
            diagnostics,
            rate_limiter: RateLimiter::default(),
            pending: Arc::new(TokioMutex::new(HashMap::new())),
            handlers_registered: AtomicU64::new(0),
            last_cursor: Arc::new(TokioMutex::new(HashMap::new())),
        }
    }

    /// Create a dispatch router with a custom rate limiter (useful in tests).
    pub fn with_rate_limiter(
        client_manager: Arc<RwLock<ClientManager>>,
        db: Database,
        diagnostics: Diagnostics,
        rate_limiter: RateLimiter,
    ) -> Self {
        Self {
            client_manager,
            db: StdMutex::new(db),
            diagnostics,
            rate_limiter,
            pending: Arc::new(TokioMutex::new(HashMap::new())),
            handlers_registered: AtomicU64::new(0),
            last_cursor: Arc::new(TokioMutex::new(HashMap::new())),
        }
    }

    /// Dispatch an outgoing agent event to all matching handlers.
    pub async fn dispatch_event(&self, event: AgentEvent) -> Result<(), DaemonError> {
        self.diagnostics.record_event_received();

        let (handlers, npub) = {
            let cm = self.client_manager.read().await;
            let handlers = cm.handler_registry.find(&event.bot_id, event.event_type);
            let npub = cm
                .get_bot_by_id(&event.bot_id)
                .ok_or_else(|| DaemonError::UnknownBot(event.bot_id.clone()))?
                .npub()
                .to_string();
            (handlers, npub)
        };

        self.diagnostics
            .set_handlers_registered(self.handlers_registered.load(Ordering::SeqCst));

        let expected = handlers.len();
        let event_id = event.event_id.clone();
        let (response_tx, mut response_rx) = mpsc::unbounded_channel();

        {
            let mut pending = self.pending.lock().await;
            pending.insert(
                event_id.clone(),
                PendingDispatch {
                    sender: response_tx,
                },
            );
        }

        // Fan-out event notifications concurrently.
        for handler in handlers {
            let event = event.clone();
            let diag = self.diagnostics.clone();
            tokio::spawn(async move {
                let handler_id = handler.id.clone();
                match handler.send_event(event) {
                    Ok(()) => diag.record_event_dispatched(),
                    Err(e) => diag.record_error(&format!("handler {handler_id} send failed: {e}")),
                }
            });
        }

        let deadline = Instant::now() + DISPATCH_TIMEOUT;
        let mut responses = Vec::new();
        let mut any_defer = false;

        while responses.len() < expected {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match timeout(remaining, response_rx.recv()).await {
                Ok(Some((handler_id, action))) => {
                    if matches!(action, HandlerAction::Defer) {
                        any_defer = true;
                        break;
                    }
                    responses.push((handler_id, action));
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }

        // Clean up pending tracker.
        self.pending.lock().await.remove(&event_id);

        // Process replies.
        for (handler_id, action) in &responses {
            if let HandlerAction::Reply { content } = action {
                if let Err(e) = self
                    .handle_send_dm(
                        &event.bot_id,
                        &event.author,
                        content,
                        Some(&event.rumor_id),
                        Some(handler_id),
                    )
                    .await
                {
                    self.diagnostics
                        .record_error(&format!("reply send failed: {e}"));
                }
            }
        }

        if !any_defer {
            let cursor = i64::try_from(event.timestamp)
                .map_err(|_| DaemonError::Config("event timestamp out of range".into()))?;
            {
                let mut last_cursor = self.last_cursor.lock().await;
                last_cursor.insert(event.bot_id.clone(), (npub.clone(), cursor));
            }
            let db = self
                .db
                .lock()
                .map_err(|_| DaemonError::Config("database lock poisoned".into()))?;
            db.save_cursor(&event.bot_id, &npub, cursor)?;
        }

        Ok(())
    }

    /// Broadcast an `agent.status` notification to all registered handlers.
    pub async fn broadcast_status(&self, status: DaemonStatus) {
        let state = match status {
            DaemonStatus::Initializing => "initializing",
            DaemonStatus::Ready => "ready",
            DaemonStatus::ShuttingDown => "shutting_down",
            DaemonStatus::Stopped => "stopped",
        };

        let handlers = {
            let cm = self.client_manager.read().await;
            cm.handler_registry.all_handlers()
        };

        for handler in handlers {
            if let Err(e) = handler.send_status(state) {
                warn!(handler_id = %handler.id, error = %e, "failed to send status notification");
            }
        }
    }

    /// Persist the latest cursor for every bot seen by this dispatch instance.
    pub async fn flush_cursors(&self) -> Result<(), DaemonError> {
        let last_cursor = self.last_cursor.lock().await;
        let db = self
            .db
            .lock()
            .map_err(|_| DaemonError::Config("database lock poisoned".into()))?;
        for (bot_id, (npub, cursor)) in last_cursor.iter() {
            db.save_cursor(bot_id, npub, *cursor)?;
        }
        Ok(())
    }

    /// Handle an incoming JSON-RPC message from a transport.
    pub async fn handle_message(
        &self,
        msg: JsonRpcMessage,
        handler_id: Option<&str>,
    ) -> Result<Option<JsonRpcMessage>, DaemonError> {
        let id = msg.id().cloned();

        let Some(method) = msg.method() else {
            return Ok(id.map(|id| {
                JsonRpcMessage::error(
                    id,
                    JsonRpcError::new(-32600, "invalid request: missing method"),
                )
            }));
        };

        let method = match parse_method(method) {
            Ok(m) => m,
            Err(_) => {
                return Ok(
                    id.map(|id| JsonRpcMessage::error(id, DaemonError::MethodNotFound.into()))
                );
            }
        };

        let params = message_params(&msg);
        let result = match method {
            Method::HandlerRegister => self.handle_register(params).await,
            Method::HandlerUnregister => self.handle_unregister(handler_id, params).await,
            Method::AgentSendDm => self.handle_send_dm_msg(handler_id, params).await,
            Method::AgentSetProfile => self.handle_set_profile(handler_id, params).await,
            Method::AgentError => self.handle_error(handler_id, params).await,
            Method::HandlerResponse => self.handle_response(handler_id, params).await,
            Method::AgentMetrics => self.handle_metrics().await,
            Method::AgentEvent | Method::AgentStatus => Err(DaemonError::MethodNotFound),
        };

        match result {
            Ok(value) => Ok(id.map(|id| JsonRpcMessage::response(id, value))),
            Err(e) => Ok(id.map(|id| JsonRpcMessage::error(id, e.into()))),
        }
    }

    async fn handle_register(&self, params: Option<&Value>) -> Result<Option<Value>, DaemonError> {
        let params =
            params.ok_or_else(|| DaemonError::Config("handler.register missing params".into()))?;
        let bot_ids: Vec<String> = serde_json::from_value(
            params
                .get("bot_ids")
                .cloned()
                .unwrap_or(Value::Array(vec![])),
        )?;
        let event_types: Vec<String> = serde_json::from_value(
            params
                .get("event_types")
                .cloned()
                .unwrap_or(Value::Array(vec![])),
        )?;
        let capabilities: Vec<String> = serde_json::from_value(
            params
                .get("capabilities")
                .cloned()
                .unwrap_or(Value::Array(vec![])),
        )?;

        let (tx, _rx) = mpsc::unbounded_channel();
        let connection = ConnectionHandle::new(tx);

        let bot_configs = {
            let cm = self.client_manager.read().await;
            cm.bots().map(|(_, b)| b.config.clone()).collect::<Vec<_>>()
        };

        let mut cm = self.client_manager.write().await;
        let handler_id = cm.handler_registry.register(
            connection,
            bot_ids,
            event_types,
            capabilities,
            &bot_configs,
        )?;

        let registered_events: Vec<String> = cm
            .handler_registry
            .get_handler(&handler_id)
            .map(|h| {
                h.event_types
                    .iter()
                    .map(event_type_name)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        self.handlers_registered.fetch_add(1, Ordering::SeqCst);
        self.diagnostics
            .set_handlers_registered(self.handlers_registered.load(Ordering::SeqCst));

        Ok(Some(serde_json::json!({
            "handler_id": handler_id,
            "registered_events": registered_events,
        })))
    }

    async fn handle_unregister(
        &self,
        handler_id: Option<&str>,
        params: Option<&Value>,
    ) -> Result<Option<Value>, DaemonError> {
        let id = handler_id
            .map(|s| s.to_string())
            .or_else(|| {
                params
                    .and_then(|p| p.get("handler_id").and_then(Value::as_str))
                    .map(String::from)
            })
            .ok_or_else(|| DaemonError::Config("handler.unregister missing handler_id".into()))?;

        let mut cm = self.client_manager.write().await;
        cm.handler_registry.unregister(&id)?;

        self.handlers_registered.fetch_sub(1, Ordering::SeqCst);
        self.diagnostics
            .set_handlers_registered(self.handlers_registered.load(Ordering::SeqCst));

        Ok(Some(Value::Object(Default::default())))
    }

    async fn handle_send_dm_msg(
        &self,
        handler_id: Option<&str>,
        params: Option<&Value>,
    ) -> Result<Option<Value>, DaemonError> {
        let params =
            params.ok_or_else(|| DaemonError::Config("agent.send_dm missing params".into()))?;
        let bot_id = params
            .get("bot_id")
            .and_then(Value::as_str)
            .ok_or_else(|| DaemonError::Config("agent.send_dm missing bot_id".into()))?;
        let recipient = params
            .get("recipient")
            .and_then(Value::as_str)
            .ok_or_else(|| DaemonError::Config("agent.send_dm missing recipient".into()))?;
        let content = params
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| DaemonError::Config("agent.send_dm missing content".into()))?;
        let reply_to = params.get("reply_to").and_then(Value::as_str);

        let event_id = self
            .handle_send_dm(bot_id, recipient, content, reply_to, handler_id)
            .await?;
        Ok(Some(Value::String(event_id.to_hex())))
    }

    async fn handle_send_dm(
        &self,
        bot_id: &str,
        recipient: &str,
        content: &str,
        reply_to: Option<&str>,
        handler_id: Option<&str>,
    ) -> Result<EventId, DaemonError> {
        if let Some(hid) = handler_id {
            let authorized = {
                let cm = self.client_manager.read().await;
                cm.is_authorized(hid, bot_id, "SendMessages")?
            };
            if !authorized {
                return Err(DaemonError::UnauthorizedBot);
            }

            let now = Instant::now();
            if !self.rate_limiter.check(hid, bot_id, now).await {
                return Err(DaemonError::RateLimited);
            }
        }

        let cm = self.client_manager.read().await;
        let bot = cm
            .get_bot_by_id(bot_id)
            .ok_or_else(|| DaemonError::UnknownBot(bot_id.into()))?;
        let event_id = cm
            .nostr_client
            .send_dm(&bot.signer, recipient, content, reply_to)
            .await?;
        Ok(event_id)
    }

    async fn handle_set_profile(
        &self,
        handler_id: Option<&str>,
        params: Option<&Value>,
    ) -> Result<Option<Value>, DaemonError> {
        let params =
            params.ok_or_else(|| DaemonError::Config("agent.set_profile missing params".into()))?;
        let bot_id = params
            .get("bot_id")
            .and_then(Value::as_str)
            .ok_or_else(|| DaemonError::Config("agent.set_profile missing bot_id".into()))?;

        if let Some(hid) = handler_id {
            let authorized = {
                let cm = self.client_manager.read().await;
                cm.is_authorized(hid, bot_id, "ManageProfile")?
            };
            if !authorized {
                return Err(DaemonError::UnauthorizedBot);
            }

            let now = Instant::now();
            if !self.rate_limiter.check(hid, bot_id, now).await {
                return Err(DaemonError::RateLimited);
            }
        }

        // Publishing kind:0 events requires a new NostrClient API; defer to a
        // follow-up unit once the client surface is ready.
        Err(DaemonError::MethodNotFound)
    }

    async fn handle_error(
        &self,
        handler_id: Option<&str>,
        params: Option<&Value>,
    ) -> Result<Option<Value>, DaemonError> {
        let params =
            params.ok_or_else(|| DaemonError::Config("agent.error missing params".into()))?;
        let bot_id = params
            .get("bot_id")
            .and_then(Value::as_str)
            .ok_or_else(|| DaemonError::Config("agent.error missing bot_id".into()))?;
        let message = params
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");

        if let Some(hid) = handler_id {
            let authorized = {
                let cm = self.client_manager.read().await;
                cm.is_authorized(hid, bot_id, "ReadMessages")?
            };
            if !authorized {
                return Err(DaemonError::UnauthorizedBot);
            }
        }

        self.diagnostics.record_error(message);
        Ok(None)
    }

    async fn handle_response(
        &self,
        handler_id: Option<&str>,
        params: Option<&Value>,
    ) -> Result<Option<Value>, DaemonError> {
        let params =
            params.ok_or_else(|| DaemonError::Config("handler.response missing params".into()))?;
        let event_id = params
            .get("event_id")
            .and_then(Value::as_str)
            .ok_or_else(|| DaemonError::Config("handler.response missing event_id".into()))?;
        let action = HandlerAction::from_value(params)?;

        if let Some(hid) = handler_id {
            debug!(handler_id = %hid, event_id = %event_id, ?action, "handler response received");
        }

        let pending = self.pending.lock().await;
        if let Some(dispatch) = pending.get(event_id) {
            let _ = dispatch
                .sender
                .send((handler_id.unwrap_or("unknown").to_string(), action));
        } else {
            warn!(event_id = %event_id, "handler.response for unknown or expired event");
        }
        Ok(None)
    }

    async fn handle_metrics(&self) -> Result<Option<Value>, DaemonError> {
        let snapshot = self.diagnostics.snapshot();
        let response = MetricsResponse { snapshot };
        Ok(Some(serde_json::to_value(response)?))
    }

    /// Load the persisted cursor for a bot.
    pub fn load_cursor(&self, bot_id: &str) -> Result<Option<(String, i64)>, DaemonError> {
        let db = self
            .db
            .lock()
            .map_err(|_| DaemonError::Config("database lock poisoned".into()))?;
        db.load_cursor(bot_id)
    }
}

fn message_params(msg: &JsonRpcMessage) -> Option<&Value> {
    match msg {
        JsonRpcMessage::Request { params, .. } | JsonRpcMessage::Notification { params, .. } => {
            params.as_ref()
        }
        _ => None,
    }
}

fn event_type_name(event_type: &EventType) -> String {
    match event_type {
        EventType::DmReceived => "dm_received".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BotConfig, DaemonConfig, GlobalDaemonConfig, SigningConfig};
    use crate::handlers::ConnectionHandle;
    use crate::nostr::NostrClient;
    use crate::transport::protocol::JsonRpcMessage;
    use nostr::ToBech32;
    use tempfile::tempdir;

    fn test_keys() -> nostr::Keys {
        nostr::Keys::generate()
    }

    fn bot_config(id: &str, keys: &nostr::Keys, capabilities: &[&str]) -> BotConfig {
        BotConfig {
            id: id.to_string(),
            npub: keys.public_key().to_bech32().unwrap(),
            signing: SigningConfig::Nsec {
                nsec: keys.secret_key().to_bech32().unwrap(),
            },
            relays: vec![],
            capabilities: capabilities.iter().map(|s| s.to_string()).collect(),
        }
    }

    async fn dispatch_with_bots(
        bot_configs: Vec<BotConfig>,
    ) -> (Dispatch, Arc<RwLock<ClientManager>>) {
        let config = DaemonConfig {
            daemon: GlobalDaemonConfig::default(),
            bots: bot_configs,
        };
        let nostr_client = NostrClient::new(vec![]).await.unwrap();
        let cm = Arc::new(RwLock::new(
            ClientManager::new(config, nostr_client).unwrap(),
        ));
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path().join("test.db").as_path()).unwrap();
        let diagnostics = Diagnostics::new();
        let dispatch = Dispatch::new(cm.clone(), db, diagnostics);
        (dispatch, cm)
    }

    #[test]
    fn rate_limiter_allows_burst_then_limits() {
        let limiter = RateLimiter::new(1.0, 2.0, 10.0, 20.0);
        let now = Instant::now();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            assert!(limiter.check("h1", "b1", now).await);
            assert!(limiter.check("h1", "b1", now).await);
            assert!(!limiter.check("h1", "b1", now).await);
        });
    }

    #[test]
    fn rate_limiter_enforces_bot_aggregate() {
        let limiter = RateLimiter::new(100.0, 100.0, 1.0, 1.0);
        let now = Instant::now();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            assert!(limiter.check("h1", "b1", now).await);
            assert!(!limiter.check("h2", "b1", now).await);
        });
    }

    #[test]
    fn handler_action_parsing() {
        let ack = serde_json::json!({"action": "ack"});
        assert_eq!(HandlerAction::from_value(&ack).unwrap(), HandlerAction::Ack);

        let ignore = serde_json::json!({"action": "ignore"});
        assert_eq!(
            HandlerAction::from_value(&ignore).unwrap(),
            HandlerAction::Ignore
        );

        let defer = serde_json::json!({"action": "defer"});
        assert_eq!(
            HandlerAction::from_value(&defer).unwrap(),
            HandlerAction::Defer
        );

        let reply = serde_json::json!({"action": "reply", "content": "hi"});
        assert_eq!(
            HandlerAction::from_value(&reply).unwrap(),
            HandlerAction::Reply {
                content: "hi".to_string()
            }
        );

        let missing_content = serde_json::json!({"action": "reply"});
        assert!(HandlerAction::from_value(&missing_content).is_err());

        let unknown = serde_json::json!({"action": "unknown"});
        assert!(HandlerAction::from_value(&unknown).is_err());
    }

    #[tokio::test]
    async fn handle_register_returns_handler_id_and_events() {
        let keys = test_keys();
        let (dispatch, _cm) =
            dispatch_with_bots(vec![bot_config("echo-bot", &keys, &["ReadMessages"])]).await;

        let req = JsonRpcMessage::request(
            1.into(),
            "handler.register",
            Some(serde_json::json!({
                "bot_ids": ["echo-bot"],
                "event_types": ["dm_received"],
                "capabilities": ["ReadMessages"],
            })),
        );

        let resp = dispatch.handle_message(req, None).await.unwrap().unwrap();
        let JsonRpcMessage::Response { result, .. } = resp else {
            panic!("expected response");
        };
        let result = result.unwrap();
        assert!(result.get("handler_id").is_some());
        let events = result.get("registered_events").unwrap().as_array().unwrap();
        assert_eq!(events, &["dm_received"]);
    }

    #[tokio::test]
    async fn unauthorized_send_dm_returns_32006() {
        let keys = test_keys();
        let (dispatch, cm) =
            dispatch_with_bots(vec![bot_config("echo-bot", &keys, &["ReadMessages"])]).await;

        let bot_config_for_register = {
            let cm = cm.read().await;
            cm.get_bot_by_id("echo-bot").unwrap().config.clone()
        };

        let handler_id = {
            let mut cm = cm.write().await;
            let (tx, _rx) = mpsc::unbounded_channel();
            let handle = ConnectionHandle::new(tx);
            cm.handler_registry
                .register(
                    handle,
                    vec!["echo-bot".to_string()],
                    vec!["dm_received".to_string()],
                    vec!["ReadMessages".to_string()],
                    &[bot_config_for_register],
                )
                .unwrap()
        };

        let req = JsonRpcMessage::request(
            1.into(),
            "agent.send_dm",
            Some(serde_json::json!({
                "bot_id": "echo-bot",
                "recipient": "npub1recipient",
                "content": "hello",
            })),
        );

        let resp = dispatch
            .handle_message(req, Some(&handler_id))
            .await
            .unwrap()
            .unwrap();
        let JsonRpcMessage::Error { error, .. } = resp else {
            panic!("expected error response");
        };
        assert_eq!(error.code, -32006);
    }

    #[tokio::test]
    async fn agent_error_records_diagnostics() {
        let keys = test_keys();
        let (dispatch, cm) =
            dispatch_with_bots(vec![bot_config("echo-bot", &keys, &["ReadMessages"])]).await;

        let bot_config_for_register = {
            let cm = cm.read().await;
            cm.get_bot_by_id("echo-bot").unwrap().config.clone()
        };

        let handler_id = {
            let mut cm = cm.write().await;
            let (tx, _rx) = mpsc::unbounded_channel();
            let handle = ConnectionHandle::new(tx);
            cm.handler_registry
                .register(
                    handle,
                    vec!["echo-bot".to_string()],
                    vec!["dm_received".to_string()],
                    vec!["ReadMessages".to_string()],
                    &[bot_config_for_register],
                )
                .unwrap()
        };

        let req = JsonRpcMessage::notification(
            "agent.error",
            Some(serde_json::json!({
                "bot_id": "echo-bot",
                "message": "something went wrong",
            })),
        );

        dispatch
            .handle_message(req, Some(&handler_id))
            .await
            .unwrap();
        let snapshot = dispatch.diagnostics.snapshot();
        assert!(
            snapshot
                .errors
                .iter()
                .any(|e| e.contains("something went wrong"))
        );
    }
}
