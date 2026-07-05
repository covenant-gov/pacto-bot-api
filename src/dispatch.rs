use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nostr::EventId;
use serde_json::Value;
use tokio::sync::{Mutex as TokioMutex, RwLock, mpsc, watch};
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::client_manager::ClientManager;
use crate::db::Db;
use crate::diagnostics::{DaemonStatus, Diagnostics, HealthSnapshot};
use crate::errors::{DaemonError, JsonRpcError};
use crate::events::{AgentEvent, EventType};
use crate::handlers::ConnectionHandle;
use crate::transport::protocol::{
    AdminSendTestDmResponse, AgentListHandlersEntry, AgentListHandlersResponse,
    AgentUnregisterHandlerResponse, AgentVersionResponse, HandlerReconnectParams,
    HandlerReconnectResponse, HandlerRegisterResponse, HandlerUnregisterResponse, JsonRpcMessage,
    Method, MetricsResponse, parse_method,
};

use secrecy::{ExposeSecret, SecretString};

/// Maximum time to wait for handler responses before advancing the cursor.
pub const DISPATCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Default per-handler rate: 10 ops/sec.
const HANDLER_RATE: f64 = 10.0;
/// Default per-handler burst: 20 ops.
const HANDLER_BURST: f64 = 20.0;
/// Default per-bot aggregate rate: 20 ops/sec.
const BOT_RATE: f64 = 20.0;
/// Default per-bot aggregate burst: 40 ops.
const BOT_BURST: f64 = 40.0;

/// Production default per-handler rate, exposed for tests.
pub const DEFAULT_HANDLER_RATE: f64 = HANDLER_RATE;
/// Production default per-handler burst, exposed for tests.
pub const DEFAULT_HANDLER_BURST: f64 = HANDLER_BURST;
/// Production default per-bot aggregate rate, exposed for tests.
pub const DEFAULT_BOT_RATE: f64 = BOT_RATE;
/// Production default per-bot aggregate burst, exposed for tests.
pub const DEFAULT_BOT_BURST: f64 = BOT_BURST;

/// Action a handler can take in response to an event.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HandlerAction {
    Ack,
    Reply { content: String },
    SendDm { content: String },
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
            "send_dm" => {
                let content = value
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        DaemonError::Config("handler.response send_dm missing content".into())
                    })?;
                Ok(HandlerAction::SendDm {
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
    handler_ids: HashSet<String>,
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

/// Default time a handler may be disconnected before the reaper removes it.
const HANDLER_STALE_TIMEOUT: Duration = Duration::from_secs(30);

/// Event dispatch router.
#[derive(Debug)]
pub struct Dispatch {
    client_manager: Arc<RwLock<ClientManager>>,
    db: Db,
    pub diagnostics: Diagnostics,
    rate_limiter: RateLimiter,
    pending: Arc<TokioMutex<HashMap<String, PendingDispatch>>>,
    handlers_registered: AtomicU64,
    last_cursor: Arc<TokioMutex<HashMap<String, (String, i64)>>>,
    dispatch_timeout: Duration,
    handler_stale_timeout: Duration,
}

impl Dispatch {
    /// Create a new dispatch router with default rate limits.
    pub fn new(
        client_manager: Arc<RwLock<ClientManager>>,
        db: Db,
        diagnostics: Diagnostics,
    ) -> Self {
        Self {
            client_manager,
            db,
            diagnostics,
            rate_limiter: RateLimiter::default(),
            pending: Arc::new(TokioMutex::new(HashMap::new())),
            handlers_registered: AtomicU64::new(0),
            last_cursor: Arc::new(TokioMutex::new(HashMap::new())),
            dispatch_timeout: DISPATCH_TIMEOUT,
            handler_stale_timeout: HANDLER_STALE_TIMEOUT,
        }
    }

    /// Create a dispatch router with a custom rate limiter (useful in tests).
    pub fn with_rate_limiter(
        client_manager: Arc<RwLock<ClientManager>>,
        db: Db,
        diagnostics: Diagnostics,
        rate_limiter: RateLimiter,
    ) -> Self {
        Self {
            client_manager,
            db,
            diagnostics,
            rate_limiter,
            pending: Arc::new(TokioMutex::new(HashMap::new())),
            handlers_registered: AtomicU64::new(0),
            last_cursor: Arc::new(TokioMutex::new(HashMap::new())),
            dispatch_timeout: DISPATCH_TIMEOUT,
            handler_stale_timeout: HANDLER_STALE_TIMEOUT,
        }
    }

    /// Override the dispatch timeout. Intended for tests only.
    pub fn set_dispatch_timeout(&mut self, timeout: Duration) {
        self.dispatch_timeout = timeout;
    }

    /// Override the stale-handler timeout. Intended for tests only.
    pub fn set_handler_stale_timeout(&mut self, timeout: Duration) {
        self.handler_stale_timeout = timeout;
    }

    /// Return the current diagnostics health snapshot.
    pub fn diagnostics_snapshot(&self) -> HealthSnapshot {
        self.diagnostics.snapshot()
    }

    /// Number of handlers currently registered.
    pub fn registered_handler_count(&self) -> u64 {
        self.handlers_registered.load(Ordering::SeqCst)
    }

    /// Remove a handler from the registry and delete its persisted row.
    ///
    /// Used by the transport layer when a connection drops, as well as by
    /// explicit `handler.unregister` requests.
    pub async fn unregister_handler(&self, handler_id: &str) -> Result<(), DaemonError> {
        {
            let mut cm = self.client_manager.write().await;
            cm.handler_registry.unregister(handler_id)?;
        }

        self.db.delete_handler(handler_id).await?;

        self.handlers_registered.fetch_sub(1, Ordering::SeqCst);
        self.diagnostics
            .set_handlers_registered(self.handlers_registered.load(Ordering::SeqCst));

        Ok(())
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
        let handler_ids: HashSet<String> = handlers.iter().map(|h| h.id.clone()).collect();

        {
            let mut pending = self.pending.lock().await;
            pending.insert(
                event_id.clone(),
                PendingDispatch {
                    sender: response_tx,
                    handler_ids,
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
                    Err(e) => diag.record_error(
                        Some("handler_send_failed"),
                        &format!("handler {handler_id} send failed: {e}"),
                        None,
                    ),
                }
            });
        }

        let deadline = Instant::now() + self.dispatch_timeout;
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
            let mut action_label = match action {
                HandlerAction::Ack => "ack",
                HandlerAction::Reply { .. } => "reply",
                HandlerAction::SendDm { .. } => "send_dm",
                HandlerAction::Defer => "defer",
                HandlerAction::Ignore => "ignore",
            };
            let mut reply_event_id: Option<String> = None;
            match action {
                HandlerAction::Reply { content } => {
                    info!(
                        bot_id = %event.bot_id,
                        recipient = %event.author,
                        reply_to = %event.rumor_id,
                        "sending reply DM"
                    );
                    match self
                        .handle_send_dm(
                            &event.bot_id,
                            &event.author,
                            content,
                            Some(&event.rumor_id),
                            Some(handler_id),
                        )
                        .await
                    {
                        Ok(event_id) => {
                            reply_event_id = Some(event_id.to_hex());
                            self.diagnostics.record_reply();
                        }
                        Err(e) => {
                            action_label = "reply_failed";
                            self.diagnostics.record_reply_send_failed();
                            tracing::error!(
                                bot_id = %event.bot_id,
                                recipient = %event.author,
                                reply_to = %event.rumor_id,
                                error = %e,
                                "failed to send reply DM"
                            );
                        }
                    }
                }
                HandlerAction::SendDm { content } => {
                    info!(
                        bot_id = %event.bot_id,
                        recipient = %event.author,
                        "sending plain DM"
                    );
                    match self
                        .handle_send_dm(
                            &event.bot_id,
                            &event.author,
                            content,
                            None,
                            Some(handler_id),
                        )
                        .await
                    {
                        Ok(event_id) => {
                            reply_event_id = Some(event_id.to_hex());
                            self.diagnostics.record_send_dm();
                        }
                        Err(e) => {
                            action_label = "send_dm_failed";
                            self.diagnostics.record_send_dm_failed();
                            tracing::error!(
                                bot_id = %event.bot_id,
                                recipient = %event.author,
                                error = %e,
                                "failed to send plain DM"
                            );
                        }
                    }
                }
                _ => {}
            }
            if let Err(e) = self
                .db
                .save_event_trace(
                    &event.bot_id,
                    &event.event_id,
                    &event.author,
                    &content_preview(&event.content),
                    action_label,
                    reply_event_id.as_deref(),
                )
                .await
            {
                warn!(
                    bot_id = %event.bot_id,
                    event_id = %event.event_id,
                    error = %e,
                    "failed to save event trace"
                );
            }
        }

        if !any_defer {
            let cursor = i64::try_from(event.timestamp)
                .map_err(|_| DaemonError::Config("event timestamp out of range".into()))?;
            {
                let mut last_cursor = self.last_cursor.lock().await;
                last_cursor.insert(event.bot_id.clone(), (npub.clone(), cursor));
            }
            self.db.save_cursor(&event.bot_id, &npub, cursor).await?;
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
            if let Err(e) = handler.send_status(state, None) {
                warn!(handler_id = %handler.id, error = %e, "failed to send status notification");
            }
        }
    }

    /// Broadcast an `agent.metrics` notification to all registered handlers.
    ///
    /// Uses the same payload shape as the `agent.metrics` response.
    pub async fn broadcast_metrics(&self) {
        let snapshot = self.diagnostics.snapshot();
        let response = MetricsResponse::from(snapshot);

        let handlers = {
            let cm = self.client_manager.read().await;
            cm.handler_registry.all_handlers()
        };

        for handler in handlers {
            if let Err(e) = handler.send_metrics(&response) {
                warn!(handler_id = %handler.id, error = %e, "failed to send metrics notification");
            }
        }
    }

    /// Spawn a background task that broadcasts `agent.metrics` notifications
    /// to all registered handlers every `interval` until `shutdown` fires.
    pub fn spawn_periodic_metrics(
        self: Arc<Self>,
        interval: Duration,
        mut shutdown: watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(interval);
            loop {
                tokio::select! {
                    _ = interval.tick() => self.broadcast_metrics().await,
                    _ = shutdown.changed() => break,
                }
            }
        })
    }

    /// Persist the latest cursor for every bot seen by this dispatch instance.
    pub async fn flush_cursors(&self) -> Result<(), DaemonError> {
        let cursors: Vec<(String, String, i64)> = {
            let last_cursor = self.last_cursor.lock().await;
            last_cursor
                .iter()
                .map(|(bot_id, (npub, cursor))| (bot_id.clone(), npub.clone(), *cursor))
                .collect()
        };
        for (bot_id, npub, cursor) in cursors {
            self.db.save_cursor(&bot_id, &npub, cursor).await?;
        }
        Ok(())
    }

    /// Handle an incoming JSON-RPC message from a transport.
    ///
    /// `connection` is the handle for the connection that sent `msg`. It is
    /// used to wire the live connection during `handler.register` so that the
    /// daemon can push `agent.event` and `agent.status` notifications back to
    /// the handler.
    pub async fn handle_message(
        &self,
        msg: JsonRpcMessage,
        handler_id: Option<&str>,
        connection: Option<ConnectionHandle>,
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
            Method::HandlerRegister => self.handle_register(params, connection).await,
            Method::HandlerReconnect => self.handle_reconnect(params, connection).await,
            Method::HandlerUnregister => self.handle_unregister(handler_id, params).await,
            Method::AgentSendDm => self.handle_send_dm_msg(handler_id, params).await,
            Method::AgentSetProfile => self.handle_set_profile(handler_id, params).await,
            Method::AgentError => self.handle_error(handler_id, params).await,
            Method::HandlerResponse => self.handle_response(handler_id, params).await,
            Method::AgentMetrics => self.handle_metrics().await,
            Method::AgentListHandlers => self.handle_list_handlers(handler_id).await,
            Method::AgentUnregisterHandler => {
                self.handle_admin_unregister_handler(handler_id, params)
                    .await
            }
            Method::AgentVersion => self.handle_version().await,
            Method::AdminSendTestDm => self.handle_admin_send_test_dm(handler_id, params).await,
            Method::AgentEvent | Method::AgentStatus => Err(DaemonError::MethodNotFound),
        };

        match result {
            Ok(value) => Ok(id.map(|id| JsonRpcMessage::response(id, value))),
            Err(e) => Ok(id.map(|id| JsonRpcMessage::error(id, e.into()))),
        }
    }

    async fn handle_register(
        &self,
        params: Option<&Value>,
        connection: Option<ConnectionHandle>,
    ) -> Result<Option<Value>, DaemonError> {
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

        let connection = connection.unwrap_or_else(|| {
            let (tx, _rx) = mpsc::channel(1);
            ConnectionHandle::new(tx)
        });

        let bot_configs = {
            let cm = self.client_manager.read().await;
            cm.bots().map(|(_, b)| b.config.clone()).collect::<Vec<_>>()
        };

        let mut cm = self.client_manager.write().await;
        let registration = cm.handler_registry.register(
            connection,
            bot_ids,
            event_types,
            capabilities,
            &bot_configs,
        )?;
        let handler_id = registration.handler_id;
        let reconnect_token = registration.reconnect_token;

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

        let handler = cm
            .handler_registry
            .get_handler(&handler_id)
            .ok_or_else(|| DaemonError::HandlerNotRegistered)?
            .clone();
        drop(cm);
        self.db.save_handler(&handler).await?;

        self.handlers_registered.fetch_add(1, Ordering::SeqCst);
        self.diagnostics
            .set_handlers_registered(self.handlers_registered.load(Ordering::SeqCst));

        Ok(Some(serde_json::to_value(HandlerRegisterResponse {
            handler_id,
            reconnect_token: reconnect_token.expose_secret().to_string(),
            registered_events,
        })?))
    }

    async fn handle_reconnect(
        &self,
        params: Option<&Value>,
        connection: Option<ConnectionHandle>,
    ) -> Result<Option<Value>, DaemonError> {
        let params =
            params.ok_or_else(|| DaemonError::Config("handler.reconnect missing params".into()))?;
        let reconnect_params: HandlerReconnectParams = serde_json::from_value(params.clone())?;

        let connection = connection.unwrap_or_else(|| {
            let (tx, _rx) = mpsc::channel(1);
            ConnectionHandle::new(tx)
        });

        let bot_configs = {
            let cm = self.client_manager.read().await;
            cm.bots().map(|(_, b)| b.config.clone()).collect::<Vec<_>>()
        };

        let mut cm = self.client_manager.write().await;
        let handler_id = cm.handler_registry.reconnect(
            reconnect_params.handler_id,
            SecretString::new(reconnect_params.reconnect_token.into()),
            connection,
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

        let handler = cm
            .handler_registry
            .get_handler(&handler_id)
            .ok_or_else(|| DaemonError::HandlerNotRegistered)?
            .clone();
        drop(cm);
        self.db.save_handler(&handler).await?;

        Ok(Some(serde_json::to_value(HandlerReconnectResponse {
            handler_id,
            registered_events,
        })?))
    }

    async fn handle_unregister(
        &self,
        handler_id: Option<&str>,
        _params: Option<&Value>,
    ) -> Result<Option<Value>, DaemonError> {
        let id = handler_id.ok_or_else(|| {
            DaemonError::Config("handler.unregister requires a registered connection".into())
        })?;

        self.unregister_handler(id).await?;

        Ok(Some(serde_json::to_value(HandlerUnregisterResponse {
            unregistered: true,
        })?))
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
        let hid = handler_id.ok_or(DaemonError::HandlerNotRegistered)?;
        let authorized = {
            let cm = self.client_manager.read().await;
            cm.is_authorized(hid, bot_id, "SendMessages")?
        };
        if !authorized {
            return Err(DaemonError::UnauthorizedBot);
        }

        let now = Instant::now();
        if !self.rate_limiter.check(hid, bot_id, now).await {
            self.diagnostics.record_rate_limited();
            return Err(DaemonError::RateLimited);
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
        let name = params.get("name").and_then(Value::as_str);
        let about = params.get("about").and_then(Value::as_str);
        let picture = params.get("picture").and_then(Value::as_str);

        let hid = handler_id.ok_or(DaemonError::HandlerNotRegistered)?;
        let authorized = {
            let cm = self.client_manager.read().await;
            cm.is_authorized(hid, bot_id, "ManageProfile")?
        };
        if !authorized {
            return Err(DaemonError::UnauthorizedBot);
        }

        let now = Instant::now();
        if !self.rate_limiter.check(hid, bot_id, now).await {
            self.diagnostics.record_rate_limited();
            return Err(DaemonError::RateLimited);
        }

        let cm = self.client_manager.read().await;
        let bot = cm
            .get_bot_by_id(bot_id)
            .ok_or_else(|| DaemonError::UnknownBot(bot_id.into()))?;
        let event_id = cm
            .nostr_client
            .set_profile(&bot.signer, name, about, picture)
            .await?;
        Ok(Some(Value::String(event_id.to_hex())))
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
        let code = params.get("code").and_then(Value::as_str);
        let data = params.get("data");

        let hid = handler_id.ok_or(DaemonError::HandlerNotRegistered)?;
        let authorized = {
            let cm = self.client_manager.read().await;
            cm.is_authorized(hid, bot_id, "ReadMessages")?
        };
        if !authorized {
            return Err(DaemonError::UnauthorizedBot);
        }

        let now = Instant::now();
        if !self.rate_limiter.check(hid, bot_id, now).await {
            self.diagnostics.record_rate_limited();
            return Err(DaemonError::RateLimited);
        }

        self.diagnostics.record_error(code, message, data);
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
            let response_handler_id = handler_id.unwrap_or("unknown");
            if !dispatch.handler_ids.contains(response_handler_id) {
                warn!(
                    handler_id = %response_handler_id,
                    event_id = %event_id,
                    "handler.response from handler not dispatched this event"
                );
                return Err(DaemonError::HandlerNotDispatched);
            }
            let _ = dispatch
                .sender
                .send((response_handler_id.to_string(), action));
        } else {
            warn!(
                handler_id = handler_id.unwrap_or("unknown"),
                event_id = %event_id,
                "handler.response for unknown or expired event; consider increasing the dispatch timeout"
            );
        }
        Ok(None)
    }

    async fn handle_metrics(&self) -> Result<Option<Value>, DaemonError> {
        let snapshot = self.diagnostics.snapshot();
        let response = MetricsResponse::from(snapshot);
        Ok(Some(serde_json::to_value(response)?))
    }

    async fn handle_list_handlers(
        &self,
        caller_id: Option<&str>,
    ) -> Result<Option<Value>, DaemonError> {
        self.require_admin_or_self(caller_id, None).await?;

        let stale_timeout = self.handler_stale_timeout;
        let handlers = {
            let cm = self.client_manager.read().await;
            cm.handler_registry.all_handlers()
        };
        let entries: Vec<AgentListHandlersEntry> = handlers
            .into_iter()
            .map(|h| {
                let connected = h.is_connected();
                let state = if connected {
                    "connected".to_string()
                } else if h.is_stale(stale_timeout) {
                    "stale".to_string()
                } else {
                    "disconnected".to_string()
                };
                AgentListHandlersEntry {
                    handler_id: h.id,
                    bot_ids: h.bot_ids,
                    event_types: h.event_types.iter().map(event_type_name).collect(),
                    capabilities: h.capabilities,
                    connected,
                    state,
                    transport: h.transport,
                    last_seen: h.last_seen.to_rfc3339(),
                    registered_at: h.registered_at.to_rfc3339(),
                }
            })
            .collect();

        let response = AgentListHandlersResponse { handlers: entries };
        Ok(Some(serde_json::to_value(response)?))
    }

    async fn handle_admin_unregister_handler(
        &self,
        caller_id: Option<&str>,
        params: Option<&Value>,
    ) -> Result<Option<Value>, DaemonError> {
        let params = params
            .ok_or_else(|| DaemonError::Config("agent.unregister_handler missing params".into()))?;
        let handler_id = params
            .get("handler_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                DaemonError::Config("agent.unregister_handler missing handler_id".into())
            })?;

        self.require_admin_or_self(caller_id, Some(handler_id))
            .await?;

        self.unregister_handler(handler_id).await?;

        Ok(Some(serde_json::to_value(
            AgentUnregisterHandlerResponse { unregistered: true },
        )?))
    }

    /// Ensure the caller is either the target handler itself or has the Admin capability.
    async fn require_admin_or_self(
        &self,
        caller_id: Option<&str>,
        target_handler_id: Option<&str>,
    ) -> Result<(), DaemonError> {
        let Some(caller_id) = caller_id else {
            return Err(DaemonError::HandlerNotRegistered);
        };

        if target_handler_id == Some(caller_id) {
            return Ok(());
        }

        let cm = self.client_manager.read().await;
        let handler = cm
            .handler_registry
            .get_handler(caller_id)
            .ok_or(DaemonError::HandlerNotRegistered)?;
        if handler.capabilities.contains(&"Admin".to_string()) {
            return Ok(());
        }
        Err(DaemonError::UnauthorizedBot)
    }

    async fn handle_admin_send_test_dm(
        &self,
        caller_id: Option<&str>,
        params: Option<&Value>,
    ) -> Result<Option<Value>, DaemonError> {
        self.require_admin_or_self(caller_id, None).await?;

        let params = params
            .ok_or_else(|| DaemonError::Config("admin.send_test_dm missing params".into()))?;
        let bot_id = params
            .get("bot_id")
            .and_then(Value::as_str)
            .ok_or_else(|| DaemonError::Config("admin.send_test_dm missing bot_id".into()))?;
        let recipient = params
            .get("recipient")
            .and_then(Value::as_str)
            .ok_or_else(|| DaemonError::Config("admin.send_test_dm missing recipient".into()))?;
        let content = params
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| DaemonError::Config("admin.send_test_dm missing content".into()))?;

        let event_id = self.send_test_dm(bot_id, recipient, content).await?;
        Ok(Some(serde_json::to_value(AdminSendTestDmResponse {
            event_id: event_id.to_hex(),
        })?))
    }

    async fn send_test_dm(
        &self,
        bot_id: &str,
        recipient: &str,
        content: &str,
    ) -> Result<EventId, DaemonError> {
        let cm = self.client_manager.read().await;
        let bot = cm
            .get_bot_by_id(bot_id)
            .ok_or_else(|| DaemonError::UnknownBot(bot_id.into()))?;
        cm.nostr_client
            .send_dm(&bot.signer, recipient, content, None)
            .await
    }

    async fn handle_version(&self) -> Result<Option<Value>, DaemonError> {
        let response = AgentVersionResponse {
            version: crate::version::VERSION.to_string(),
            commit: crate::version::GIT_COMMIT_SHORT.to_string(),
        };
        Ok(Some(serde_json::to_value(response)?))
    }

    /// Spawn a background task that removes disconnected handlers that have
    /// been stale longer than `stale_timeout`.
    pub fn spawn_handler_reaper(
        self: Arc<Self>,
        stale_timeout: Duration,
        interval: Duration,
        mut shutdown: watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(interval);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let stale_ids: Vec<String> = {
                            let cm = self.client_manager.read().await;
                            cm.handler_registry
                                .all_handlers()
                                .into_iter()
                                .filter(|h| h.is_stale(stale_timeout))
                                .map(|h| h.id)
                                .collect()
                        };
                        for handler_id in stale_ids {
                            match self.unregister_handler(&handler_id).await {
                                Ok(()) => warn!(handler_id = %handler_id, "reaped stale handler"),
                                Err(e) => warn!(
                                    handler_id = %handler_id,
                                    error = %e,
                                    "failed to reap stale handler"
                                ),
                            }
                        }
                    }
                    _ = shutdown.changed() => break,
                }
            }
        })
    }

    /// Load the persisted cursor for a bot.
    pub async fn load_cursor(&self, bot_id: &str) -> Result<Option<(String, i64)>, DaemonError> {
        self.db.load_cursor(bot_id).await
    }

    /// Access the diagnostics collector.
    pub fn diagnostics(&self) -> &Diagnostics {
        &self.diagnostics
    }

    /// Restore persisted handler registrations as disconnected entries.
    pub async fn restore_handlers(&self) -> Result<(), DaemonError> {
        let handlers = self.db.load_handlers().await?;

        let count = handlers.len() as u64;
        let mut cm = self.client_manager.write().await;
        for handler in handlers {
            cm.handler_registry.restore(handler);
        }
        self.handlers_registered.store(count, Ordering::SeqCst);
        self.diagnostics.set_handlers_registered(count);
        Ok(())
    }

    /// Mark a handler's live connection as dead without removing its persisted
    /// registration. A later reconnect can reuse the stored row.
    pub async fn disconnect_handler(&self, handler_id: &str) {
        let mut cm = self.client_manager.write().await;
        if let Some(handler) = cm.handler_registry.get_handler_mut(handler_id) {
            handler.disconnect();
        }
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

/// Truncate event content to a safe preview length for diagnostics.
fn content_preview(content: &str) -> String {
    let limit = 120;
    if content.chars().count() <= limit {
        content.to_string()
    } else {
        format!("{}…", content.chars().take(limit).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BotConfig, DaemonConfig, GlobalDaemonConfig, SigningConfig};
    use crate::db::Db;
    use crate::handlers::{ConnectionHandle, HandlerRegistry};
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
                nsec: SecretString::new(keys.secret_key().to_bech32().unwrap().into()),
            },
            relays: vec![],
            capabilities: capabilities.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
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
            ClientManager::new(config, nostr_client).await.unwrap(),
        ));
        let dir = tempdir().unwrap();
        let db = Db::open(dir.path().join("test.db").as_path())
            .await
            .unwrap();
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
    fn default_rate_limiter_allows_handler_burst_of_20() {
        let limiter = RateLimiter::default();
        let now = Instant::now();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            for _ in 0..DEFAULT_HANDLER_BURST as usize {
                assert!(
                    limiter.check("handler-1", "bot-a", now).await,
                    "call within per-handler burst should succeed"
                );
            }
            assert!(
                !limiter.check("handler-1", "bot-a", now).await,
                "21st call from one handler should be rate limited"
            );
        });
    }

    #[test]
    fn default_rate_limiter_enforces_bot_aggregate_burst_of_40() {
        let limiter = RateLimiter::default();
        let now = Instant::now();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            for _ in 0..20 {
                assert!(
                    limiter.check("handler-1", "bot-a", now).await,
                    "handler-1 call within shared bot burst should succeed"
                );
                assert!(
                    limiter.check("handler-2", "bot-a", now).await,
                    "handler-2 call within shared bot burst should succeed"
                );
            }
            assert!(
                !limiter.check("handler-1", "bot-a", now).await,
                "41st call across two handlers on same bot should be rate limited"
            );
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

        let send_dm = serde_json::json!({"action": "send_dm", "content": "hi"});
        assert_eq!(
            HandlerAction::from_value(&send_dm).unwrap(),
            HandlerAction::SendDm {
                content: "hi".to_string()
            }
        );

        let missing_content = serde_json::json!({"action": "reply"});
        assert!(HandlerAction::from_value(&missing_content).is_err());

        let missing_send_dm_content = serde_json::json!({"action": "send_dm"});
        assert!(HandlerAction::from_value(&missing_send_dm_content).is_err());

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

        let resp = dispatch
            .handle_message(req, None, None)
            .await
            .unwrap()
            .unwrap();
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
            let (tx, _rx) = mpsc::channel(1);
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
                .handler_id
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
            .handle_message(req, Some(&handler_id), None)
            .await
            .unwrap()
            .unwrap();
        let JsonRpcMessage::Error { error, .. } = resp else {
            panic!("expected error response");
        };
        assert_eq!(error.code, -32006);
    }

    #[tokio::test]
    async fn agent_version_returns_version_and_commit() {
        let keys = test_keys();
        let (dispatch, _cm) =
            dispatch_with_bots(vec![bot_config("echo-bot", &keys, &["ReadMessages"])]).await;

        let req = JsonRpcMessage::request(1.into(), "agent.version", None);
        let resp = dispatch
            .handle_message(req, None, None)
            .await
            .unwrap()
            .unwrap();
        let JsonRpcMessage::Response { result, .. } = resp else {
            panic!("expected response");
        };
        let result = result.unwrap();
        assert_eq!(
            result.get("version").and_then(|v| v.as_str()),
            Some(crate::version::VERSION)
        );
        assert_eq!(
            result.get("commit").and_then(|v| v.as_str()),
            Some(crate::version::GIT_COMMIT_SHORT)
        );
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
            let (tx, _rx) = mpsc::channel(1);
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
                .handler_id
        };

        let req = JsonRpcMessage::notification(
            "agent.error",
            Some(serde_json::json!({
                "bot_id": "echo-bot",
                "message": "something went wrong",
            })),
        );

        dispatch
            .handle_message(req, Some(&handler_id), None)
            .await
            .unwrap();
        let snapshot = dispatch.diagnostics.snapshot();
        assert!(
            snapshot
                .errors
                .iter()
                .any(|e| e.message.contains("something went wrong"))
        );
    }

    #[tokio::test]
    async fn agent_list_handlers_returns_routing_table() {
        let keys = test_keys();
        let (dispatch, cm) = dispatch_with_bots(vec![bot_config(
            "echo-bot",
            &keys,
            &["ReadMessages", "Admin"],
        )])
        .await;

        let (tx, rx) = mpsc::channel(1);
        let handler_id = {
            let bot_config_for_register = {
                let cm = cm.read().await;
                cm.get_bot_by_id("echo-bot").unwrap().config.clone()
            };
            let mut registry = HandlerRegistry::new();
            let id = registry
                .register(
                    ConnectionHandle::with_transport(tx, "unix"),
                    vec!["echo-bot".to_string()],
                    vec!["dm_received".to_string()],
                    vec!["ReadMessages".to_string(), "Admin".to_string()],
                    &[bot_config_for_register],
                )
                .unwrap()
                .handler_id;
            {
                let mut cm = cm.write().await;
                cm.handler_registry = registry;
            }
            id
        };
        let _rx = rx;

        let req =
            JsonRpcMessage::request(1.into(), "agent.list_handlers", Some(serde_json::json!({})));
        let resp = dispatch
            .handle_message(req, Some(&handler_id), None)
            .await
            .unwrap()
            .unwrap();
        let JsonRpcMessage::Response { result, .. } = resp else {
            panic!("expected response");
        };
        let result = result.unwrap();
        let handlers = result
            .get("handlers")
            .and_then(|v| v.as_array())
            .expect("handlers array should be present");
        assert_eq!(handlers.len(), 1);
        assert_eq!(
            handlers[0].get("handler_id").and_then(|v| v.as_str()),
            Some(handler_id.as_str())
        );
        assert_eq!(
            handlers[0].get("transport").and_then(|v| v.as_str()),
            Some("unix")
        );
        assert_eq!(
            handlers[0].get("state").and_then(|v| v.as_str()),
            Some("connected")
        );
    }

    #[tokio::test]
    async fn agent_unregister_handler_removes_stale_handler() {
        let keys = test_keys();
        let (dispatch, cm) =
            dispatch_with_bots(vec![bot_config("echo-bot", &keys, &["ReadMessages"])]).await;

        let (tx, rx) = mpsc::channel(1);
        let handler_id = {
            let bot_config_for_register = {
                let cm = cm.read().await;
                cm.get_bot_by_id("echo-bot").unwrap().config.clone()
            };
            let mut registry = HandlerRegistry::new();
            let id = registry
                .register(
                    ConnectionHandle::new(tx),
                    vec!["echo-bot".to_string()],
                    vec!["dm_received".to_string()],
                    vec!["ReadMessages".to_string()],
                    &[bot_config_for_register],
                )
                .unwrap()
                .handler_id;
            {
                let mut cm = cm.write().await;
                cm.handler_registry = registry;
            }
            id
        };
        let _rx = rx;

        let req = JsonRpcMessage::request(
            1.into(),
            "agent.unregister_handler",
            Some(serde_json::json!({"handler_id": handler_id})),
        );
        let resp = dispatch
            .handle_message(req, Some(&handler_id), None)
            .await
            .unwrap()
            .unwrap();
        let JsonRpcMessage::Response { result, .. } = resp else {
            panic!("expected response");
        };
        let result = result.unwrap();
        assert_eq!(
            result.get("unregistered").and_then(|v| v.as_bool()),
            Some(true)
        );

        let cm = cm.read().await;
        assert!(cm.handler_registry.get_handler(&handler_id).is_none());
    }

    #[tokio::test]
    async fn handler_response_from_dispatched_handler_is_accepted() {
        let keys = test_keys();
        let (dispatch, cm) =
            dispatch_with_bots(vec![bot_config("echo-bot", &keys, &["ReadMessages"])]).await;

        let (tx_a, mut rx_a) = mpsc::channel(1);
        let handler_a = {
            let bot_config_for_register = {
                let cm = cm.read().await;
                cm.get_bot_by_id("echo-bot").unwrap().config.clone()
            };
            let mut registry = HandlerRegistry::new();
            let id = registry
                .register(
                    ConnectionHandle::new(tx_a),
                    vec!["echo-bot".to_string()],
                    vec!["dm_received".to_string()],
                    vec!["ReadMessages".to_string()],
                    &[bot_config_for_register],
                )
                .unwrap()
                .handler_id;
            {
                let mut cm = cm.write().await;
                cm.handler_registry = registry;
            }
            id
        };

        let event = AgentEvent {
            bot_id: "echo-bot".into(),
            event_id: "evt-1".into(),
            event_type: EventType::DmReceived,
            chat_id: None,
            content: "hello".into(),
            rumor_id: "0000000000000000000000000000000000000000000000000000000000000001".into(),
            author: "npub1author".into(),
            timestamp: 1,
        };

        let dispatch = Arc::new(dispatch);
        let dispatch_clone = Arc::clone(&dispatch);
        let dispatch_task = tokio::spawn(async move {
            dispatch_clone.dispatch_event(event).await.unwrap();
        });

        let received = rx_a.recv().await.unwrap();
        let agent_event: AgentEvent = match received {
            JsonRpcMessage::Notification { params, .. } => {
                serde_json::from_value(params.unwrap_or(Value::Null)).unwrap()
            }
            other => panic!("expected agent.event notification, got {other:?}"),
        };
        assert_eq!(agent_event.event_id, "evt-1");

        let req = JsonRpcMessage::request(
            1.into(),
            "handler.response",
            Some(serde_json::json!({
                "event_id": "evt-1",
                "action": "ack",
            })),
        );
        dispatch
            .handle_message(req, Some(&handler_a), None)
            .await
            .unwrap();

        dispatch_task.await.unwrap();
    }

    #[tokio::test]
    async fn handler_response_from_undispatched_handler_is_rejected() {
        let keys = test_keys();
        let other_keys = test_keys();
        let (dispatch, cm) = dispatch_with_bots(vec![
            bot_config("echo-bot", &keys, &["ReadMessages"]),
            bot_config("other-bot", &other_keys, &["ReadMessages"]),
        ])
        .await;

        let (tx_a, mut rx_a) = mpsc::channel(1);
        let (tx_b, _rx_b) = mpsc::channel(1);
        let (handler_a, handler_b) = {
            let bot_config_for_a = {
                let cm = cm.read().await;
                cm.get_bot_by_id("echo-bot").unwrap().config.clone()
            };
            let bot_config_for_b = {
                let cm = cm.read().await;
                cm.get_bot_by_id("other-bot").unwrap().config.clone()
            };
            let mut registry = HandlerRegistry::new();
            let id_a = registry
                .register(
                    ConnectionHandle::new(tx_a),
                    vec!["echo-bot".to_string()],
                    vec!["dm_received".to_string()],
                    vec!["ReadMessages".to_string()],
                    &[bot_config_for_a],
                )
                .unwrap()
                .handler_id;
            let id_b = registry
                .register(
                    ConnectionHandle::new(tx_b),
                    vec!["other-bot".to_string()],
                    vec!["dm_received".to_string()],
                    vec!["ReadMessages".to_string()],
                    &[bot_config_for_b],
                )
                .unwrap()
                .handler_id;
            {
                let mut cm = cm.write().await;
                cm.handler_registry = registry;
            }
            (id_a, id_b)
        };

        let mut dispatch = dispatch;
        dispatch.set_dispatch_timeout(Duration::from_millis(100));

        let event = AgentEvent {
            bot_id: "echo-bot".into(),
            event_id: "evt-1".into(),
            event_type: EventType::DmReceived,
            chat_id: None,
            content: "hello".into(),
            rumor_id: "0000000000000000000000000000000000000000000000000000000000000001".into(),
            author: "npub1author".into(),
            timestamp: 1,
        };

        let dispatch = Arc::new(dispatch);
        let dispatch_clone = Arc::clone(&dispatch);
        let dispatch_task = tokio::spawn(async move {
            dispatch_clone.dispatch_event(event).await.unwrap();
        });

        let received = rx_a.recv().await.unwrap();
        let agent_event: AgentEvent = match received {
            JsonRpcMessage::Notification { params, .. } => {
                serde_json::from_value(params.unwrap_or(Value::Null)).unwrap()
            }
            other => panic!("expected agent.event notification, got {other:?}"),
        };
        assert_eq!(agent_event.event_id, "evt-1");

        let req = JsonRpcMessage::request(
            1.into(),
            "handler.response",
            Some(serde_json::json!({
                "event_id": "evt-1",
                "action": "ack",
            })),
        );
        let resp = dispatch
            .handle_message(req, Some(&handler_b), None)
            .await
            .unwrap()
            .unwrap();
        let JsonRpcMessage::Error { error, .. } = resp else {
            panic!("expected error response");
        };
        assert_eq!(error.code, -32010);

        dispatch_task.await.unwrap();
    }

    #[tokio::test]
    async fn lock_ordering_releases_last_cursor_before_db() {
        let keys = test_keys();
        let (dispatch, _cm) =
            dispatch_with_bots(vec![bot_config("echo-bot", &keys, &["ReadMessages"])]).await;
        let dispatch = Arc::new(dispatch);

        let npub = keys.public_key().to_bech32().unwrap();
        {
            let mut last_cursor = dispatch.last_cursor.lock().await;
            last_cursor.insert("echo-bot".into(), (npub.clone(), 42));
        }

        let hold = dispatch.last_cursor.clone();
        let blocker = tokio::spawn(async move {
            let _guard = hold.lock().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        });

        let flush = tokio::spawn({
            let dispatch = Arc::clone(&dispatch);
            async move { dispatch.flush_cursors().await.unwrap() }
        });

        // Give the blocker time to take the last_cursor lock.
        tokio::time::sleep(Duration::from_millis(10)).await;
        // The runtime should still be responsive while flush_cursors waits
        // for the lock, because no database work has started yet.
        tokio::time::timeout(
            Duration::from_millis(5),
            tokio::time::sleep(Duration::from_millis(1)),
        )
        .await
        .unwrap();

        blocker.await.unwrap();
        flush.await.unwrap();

        let cursor = dispatch.load_cursor("echo-bot").await.unwrap();
        assert_eq!(cursor, Some((npub, 42)));
    }
}
