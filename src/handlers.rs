use crate::config::BotConfig;
use crate::errors::DaemonError;
use crate::events::{AgentEvent, EventType};
use crate::transport::protocol::{AgentStatusParams, JsonRpcMessage, MetricsResponse};
use chrono::{DateTime, Utc};
use secrecy::{ExposeSecret, SecretString};
use std::collections::HashMap;
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::sync::mpsc::Sender;

/// Capability a handler may request for a bot.
pub type Capability = String;

/// Validated registration request fields returned by [`HandlerRegistry::validate_request`].
type ValidatedRegistration = (Vec<String>, Vec<EventType>, Vec<Capability>);

/// Lightweight handle to a handler connection for outbound JSON-RPC notifications.
#[derive(Debug, Clone)]
pub struct ConnectionHandle {
    sender: Sender<JsonRpcMessage>,
    transport: String,
}

impl ConnectionHandle {
    pub fn new(sender: Sender<JsonRpcMessage>) -> Self {
        Self::with_transport(sender, "unknown")
    }

    pub fn with_transport(sender: Sender<JsonRpcMessage>, transport: impl Into<String>) -> Self {
        Self {
            sender,
            transport: transport.into(),
        }
    }

    /// The transport label for this connection (e.g. `unix` or `http`).
    pub fn transport(&self) -> &str {
        &self.transport
    }

    /// Send a JSON-RPC notification to the connected handler.
    ///
    /// Notifications are best-effort: if the outbound buffer is full because
    /// the peer is not reading, the notification is dropped so the dispatcher
    /// never blocks on a slow handler.
    pub fn send(&self, msg: JsonRpcMessage) -> Result<(), DaemonError> {
        self.sender
            .try_send(msg)
            .map_err(|_| DaemonError::HandlerNotRegistered)
    }

    /// Returns true if the peer side of this channel is still open.
    pub fn is_alive(&self) -> bool {
        !self.sender.is_closed()
    }
}

/// Reference to a registered handler.
///
/// `connection` is `None` for registrations restored from persistence that do
/// not currently have a live connection.
#[derive(Debug, Clone)]
pub struct HandlerRef {
    pub id: String,
    pub connection: Option<ConnectionHandle>,
    pub bot_ids: Vec<String>,
    pub event_types: Vec<EventType>,
    pub capabilities: Vec<Capability>,
    pub reconnect_token: SecretString,
    pub registered_at: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub transport: String,
}

impl HandlerRef {
    /// Returns true if this handler currently has a live connection.
    pub fn is_connected(&self) -> bool {
        self.connection.as_ref().is_some_and(|c| c.is_alive())
    }

    /// Drop the live connection, turning this registration into a persisted,
    /// disconnected entry until the handler reconnects.
    pub fn disconnect(&mut self) {
        self.connection = None;
        self.last_seen = Utc::now();
    }

    /// Returns true if this handler has been disconnected longer than `timeout`.
    pub fn is_stale(&self, timeout: Duration) -> bool {
        !self.is_connected()
            && Utc::now().signed_duration_since(self.last_seen)
                > chrono::Duration::from_std(timeout).unwrap_or(chrono::Duration::MAX)
    }

    /// Returns true if this handler should receive events for the given bot and event type.
    pub fn matches(&self, bot_id: &str, event_type: EventType) -> bool {
        self.bot_ids.contains(&bot_id.to_string()) && self.event_types.contains(&event_type)
    }

    /// Returns true if this handler is authorized for the given bot and capability.
    pub fn is_authorized(&self, bot_id: &str, capability: &str) -> bool {
        self.bot_ids.contains(&bot_id.to_string())
            && self.capabilities.contains(&capability.to_string())
    }

    /// Send an `agent.event` notification to this handler if it has a live connection.
    pub fn send_event(&self, event: AgentEvent) -> Result<(), DaemonError> {
        let msg = JsonRpcMessage::notification("agent.event", Some(serde_json::to_value(&event)?));
        match &self.connection {
            Some(conn) => conn.send(msg),
            None => Err(DaemonError::HandlerNotRegistered),
        }
    }

    /// Send an `agent.status` notification to this handler if it has a live connection.
    pub fn send_status(&self, state: &str, identity: Option<&str>) -> Result<(), DaemonError> {
        let params = AgentStatusParams {
            state: state.to_string(),
            identity: identity.map(String::from),
            capabilities: self.capabilities.clone(),
        };
        let msg =
            JsonRpcMessage::notification("agent.status", Some(serde_json::to_value(&params)?));
        match &self.connection {
            Some(conn) => conn.send(msg),
            None => Ok(()),
        }
    }

    /// Send an `agent.metrics` notification to this handler if it has a live connection.
    pub fn send_metrics(&self, response: &MetricsResponse) -> Result<(), DaemonError> {
        let msg =
            JsonRpcMessage::notification("agent.metrics", Some(serde_json::to_value(response)?));
        match &self.connection {
            Some(conn) => conn.send(msg),
            None => Ok(()),
        }
    }
}

/// Result of a successful handler registration.
#[derive(Debug, Clone)]
pub struct HandlerRegistration {
    pub handler_id: String,
    pub reconnect_token: SecretString,
}

impl HandlerRegistration {
    pub fn handler_id(&self) -> &str {
        &self.handler_id
    }
}

/// Registry of active handler connections.
#[derive(Debug, Default)]
pub struct HandlerRegistry {
    handlers: HashMap<String, HandlerRef>,
}

impl HandlerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a handler after validating its requested bots, event types, and capabilities.
    ///
    /// The server generates a UUIDv4 handler_id and a 256-bit reconnect token.
    /// Clients must not supply a handler_id.
    pub fn register(
        &mut self,
        connection: ConnectionHandle,
        bot_ids: Vec<String>,
        event_types: Vec<String>,
        capabilities: Vec<Capability>,
        bot_configs: &[BotConfig],
    ) -> Result<HandlerRegistration, DaemonError> {
        let (bot_ids, event_types, capabilities) =
            Self::validate_request(bot_ids, event_types, capabilities, bot_configs)?;

        let id = uuid::Uuid::new_v4().to_string();
        let reconnect_token = generate_reconnect_token();
        let now = Utc::now();
        let transport = connection.transport().to_string();
        let handler = HandlerRef {
            id: id.clone(),
            connection: Some(connection),
            bot_ids,
            event_types,
            capabilities,
            reconnect_token: reconnect_token.clone(),
            registered_at: now,
            last_seen: now,
            transport,
        };

        self.handlers.insert(id.clone(), handler);
        Ok(HandlerRegistration {
            handler_id: id,
            reconnect_token,
        })
    }

    /// Reconnect a previously registered handler using its secret reconnect token.
    ///
    /// Rejects the request if the handler is already connected (live takeover
    /// is not allowed) or if the token does not match.
    pub fn reconnect(
        &mut self,
        handler_id: String,
        reconnect_token: SecretString,
        connection: ConnectionHandle,
        _bot_configs: &[BotConfig],
    ) -> Result<String, DaemonError> {
        let existing = self
            .handlers
            .get_mut(&handler_id)
            .ok_or(DaemonError::HandlerNotRegistered)?;

        if existing.is_connected() {
            return Err(DaemonError::HandlerAlreadyConnected);
        }

        let token_bytes = hex::decode(reconnect_token.expose_secret())
            .map_err(|_| DaemonError::InvalidReconnectToken)?;
        let stored_bytes = hex::decode(existing.reconnect_token.expose_secret())
            .map_err(|_| DaemonError::InvalidReconnectToken)?;
        if !bool::from(token_bytes.ct_eq(&stored_bytes)) {
            return Err(DaemonError::InvalidReconnectToken);
        }

        let transport = connection.transport().to_string();
        existing.connection = Some(connection);
        existing.last_seen = Utc::now();
        existing.transport = transport;
        Ok(handler_id)
    }

    /// Insert a persisted handler registration if it is not already present.
    pub fn restore(&mut self, mut handler: HandlerRef) {
        handler.last_seen = handler.registered_at;
        handler.transport = "unknown".to_string();
        self.handlers.entry(handler.id.clone()).or_insert(handler);
    }

    /// Remove a handler from the registry and delete its persisted row.
    ///
    /// `handler_id` must be the connection-derived identifier assigned by the
    /// daemon during registration; callers must not trust client-supplied ids.
    pub fn unregister(&mut self, handler_id: &str) -> Result<(), DaemonError> {
        self.handlers
            .remove(handler_id)
            .ok_or(DaemonError::HandlerNotRegistered)?;
        Ok(())
    }

    pub fn get_handler(&self, handler_id: &str) -> Option<&HandlerRef> {
        self.handlers.get(handler_id)
    }

    pub fn get_handler_mut(&mut self, handler_id: &str) -> Option<&mut HandlerRef> {
        self.handlers.get_mut(handler_id)
    }

    /// Find all *connected* handlers registered for the given bot and event type (fan-out).
    pub fn find(&self, bot_id: &str, event_type: EventType) -> Vec<HandlerRef> {
        self.handlers
            .values()
            .filter(|h| h.is_connected() && h.matches(bot_id, event_type))
            .cloned()
            .collect()
    }

    /// Return a clone of every registered handler reference.
    pub fn all_handlers(&self) -> Vec<HandlerRef> {
        self.handlers.values().cloned().collect()
    }

    /// Check whether the handler is registered for the bot and has the required capability.
    pub fn is_authorized(
        &self,
        handler_id: &str,
        bot_id: &str,
        capability: &str,
    ) -> Result<bool, DaemonError> {
        let handler = self
            .handlers
            .get(handler_id)
            .ok_or(DaemonError::HandlerNotRegistered)?;
        Ok(handler.is_authorized(bot_id, capability))
    }

    fn validate_request(
        bot_ids: Vec<String>,
        event_types: Vec<String>,
        capabilities: Vec<Capability>,
        bot_configs: &[BotConfig],
    ) -> Result<ValidatedRegistration, DaemonError> {
        for bot_id in &bot_ids {
            let bot = bot_configs
                .iter()
                .find(|b| b.id == *bot_id)
                .ok_or_else(|| DaemonError::UnknownBot(bot_id.clone()))?;
            for cap in &capabilities {
                if cap == "Admin" {
                    continue;
                }
                if !bot.capabilities.contains(cap) {
                    return Err(DaemonError::Config(format!(
                        "capability {cap} not granted to bot {bot_id}"
                    )));
                }
            }
        }

        let mut parsed_event_types = Vec::with_capacity(event_types.len());
        for event_type in &event_types {
            parsed_event_types.push(parse_event_type(event_type)?);
        }

        Ok((bot_ids, parsed_event_types, capabilities))
    }
}

fn generate_reconnect_token() -> SecretString {
    let mut bytes = [0u8; 32];
    let _ = getrandom::getrandom(&mut bytes);
    SecretString::new(hex::encode(bytes).into())
}

fn parse_event_type(event_type: &str) -> Result<EventType, DaemonError> {
    match event_type {
        "dm_received" => Ok(EventType::DmReceived),
        _ => Err(DaemonError::InvalidEventType(event_type.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_manager::ClientManager;
    use crate::config::{BotConfig, DaemonConfig, GlobalDaemonConfig, SigningConfig};
    use crate::db::Db;
    use crate::diagnostics::Diagnostics;
    use crate::dispatch::Dispatch;
    use crate::nostr::NostrClient;
    use nostr::ToBech32;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio::sync::RwLock;

    fn dummy_bot(id: &str, capabilities: &[&str]) -> BotConfig {
        BotConfig {
            id: id.to_string(),
            npub: format!("npub1{id}"),
            signing: SigningConfig::Nsec {
                nsec: SecretString::new("nsec1dummy".to_string().into()),
            },
            relays: vec![],
            capabilities: capabilities.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn dummy_handle() -> ConnectionHandle {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        // Keep the channel open so the handle appears alive to the registry.
        std::mem::forget(rx);
        ConnectionHandle::new(tx)
    }

    fn sample_event(bot_id: &str) -> AgentEvent {
        AgentEvent {
            bot_id: bot_id.to_string(),
            event_id: "evt1".to_string(),
            event_type: EventType::DmReceived,
            chat_id: None,
            content: "hello".to_string(),
            rumor_id: "rumor1".to_string(),
            author: "npub1sender".to_string(),
            timestamp: 1,
        }
    }

    #[test]
    fn register_returns_server_generated_uuid() {
        let bots = vec![dummy_bot("echo-bot", &["ReadMessages", "SendMessages"])];
        let mut registry = HandlerRegistry::new();

        let handler_id = registry
            .register(
                dummy_handle(),
                vec!["echo-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["ReadMessages".to_string()],
                &bots,
            )
            .expect("registration should succeed")
            .handler_id;

        assert!(
            uuid::Uuid::parse_str(&handler_id).is_ok(),
            "handler_id should be a valid UUID"
        );
        assert_eq!(registry.handlers.len(), 1);
    }

    #[test]
    fn register_rejects_unknown_bot() {
        let bots = vec![dummy_bot("echo-bot", &["ReadMessages"])];
        let mut registry = HandlerRegistry::new();

        let err = registry
            .register(
                dummy_handle(),
                vec!["ghost-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["ReadMessages".to_string()],
                &bots,
            )
            .unwrap_err();

        assert!(matches!(err, DaemonError::UnknownBot(_)));
    }

    #[test]
    fn register_rejects_unsupported_event_type() {
        let bots = vec![dummy_bot("echo-bot", &["ReadMessages"])];
        let mut registry = HandlerRegistry::new();

        let err = registry
            .register(
                dummy_handle(),
                vec!["echo-bot".to_string()],
                vec!["unknown_event".to_string()],
                vec!["ReadMessages".to_string()],
                &bots,
            )
            .unwrap_err();

        assert!(matches!(err, DaemonError::InvalidEventType(_)));
    }

    #[test]
    fn register_rejects_capability_not_granted_to_bot() {
        let bots = vec![dummy_bot("echo-bot", &["ReadMessages"])];
        let mut registry = HandlerRegistry::new();

        let err = registry
            .register(
                dummy_handle(),
                vec!["echo-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["SendMessages".to_string()],
                &bots,
            )
            .unwrap_err();

        assert!(matches!(err, DaemonError::Config(_)));
        assert!(err.to_string().contains("SendMessages"));
    }

    #[test]
    fn register_validates_capabilities_for_every_requested_bot() {
        let bots = vec![
            dummy_bot("echo-bot", &["ReadMessages", "SendMessages"]),
            dummy_bot("read-bot", &["ReadMessages"]),
        ];
        let mut registry = HandlerRegistry::new();

        let err = registry
            .register(
                dummy_handle(),
                vec!["echo-bot".to_string(), "read-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["SendMessages".to_string()],
                &bots,
            )
            .unwrap_err();

        assert!(matches!(err, DaemonError::Config(_)));
        assert!(err.to_string().contains("read-bot"));
    }

    #[test]
    fn find_fans_out_to_all_matching_handlers() {
        let bots = vec![dummy_bot("echo-bot", &["ReadMessages", "SendMessages"])];
        let mut registry = HandlerRegistry::new();

        let id_a = registry
            .register(
                dummy_handle(),
                vec!["echo-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["ReadMessages".to_string()],
                &bots,
            )
            .expect("register handler a")
            .handler_id;
        let id_b = registry
            .register(
                dummy_handle(),
                vec!["echo-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["ReadMessages".to_string(), "SendMessages".to_string()],
                &bots,
            )
            .expect("register handler b")
            .handler_id;

        let matches = registry.find("echo-bot", EventType::DmReceived);
        assert_eq!(matches.len(), 2);
        let matched_ids: Vec<_> = matches.iter().map(|h| h.id.clone()).collect();
        assert!(matched_ids.contains(&id_a));
        assert!(matched_ids.contains(&id_b));
    }

    #[test]
    fn find_excludes_handlers_for_other_bots() {
        let bots = vec![
            dummy_bot("echo-bot", &["ReadMessages"]),
            dummy_bot("other-bot", &["ReadMessages"]),
        ];
        let mut registry = HandlerRegistry::new();

        registry
            .register(
                dummy_handle(),
                vec!["echo-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["ReadMessages".to_string()],
                &bots,
            )
            .expect("register handler for echo-bot");
        registry
            .register(
                dummy_handle(),
                vec!["other-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["ReadMessages".to_string()],
                &bots,
            )
            .expect("register handler for other-bot");

        let matches = registry.find("echo-bot", EventType::DmReceived);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].bot_ids, vec!["echo-bot".to_string()]);
    }

    #[test]
    fn is_authorized_requires_bot_and_capability() {
        let bots = vec![dummy_bot("echo-bot", &["ReadMessages", "SendMessages"])];
        let mut registry = HandlerRegistry::new();

        let handler_id = registry
            .register(
                dummy_handle(),
                vec!["echo-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["ReadMessages".to_string()],
                &bots,
            )
            .expect("register handler")
            .handler_id;

        assert!(
            registry
                .is_authorized(&handler_id, "echo-bot", "ReadMessages")
                .expect("lookup should succeed"),
            "handler should be authorized for ReadMessages on echo-bot"
        );
        assert!(
            !registry
                .is_authorized(&handler_id, "echo-bot", "SendMessages")
                .expect("lookup should succeed"),
            "handler should not be authorized for SendMessages on echo-bot"
        );
        assert!(
            !registry
                .is_authorized(&handler_id, "other-bot", "ReadMessages")
                .expect("lookup should succeed"),
            "handler should not be authorized for a different bot"
        );
    }

    #[test]
    fn is_authorized_fails_for_unknown_handler() {
        let registry = HandlerRegistry::new();

        let err = registry
            .is_authorized("not-a-real-id", "echo-bot", "ReadMessages")
            .unwrap_err();
        assert!(matches!(err, DaemonError::HandlerNotRegistered));
    }

    #[test]
    fn unregister_removes_handler() {
        let bots = vec![dummy_bot("echo-bot", &["ReadMessages"])];
        let mut registry = HandlerRegistry::new();

        let handler_id = registry
            .register(
                dummy_handle(),
                vec!["echo-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["ReadMessages".to_string()],
                &bots,
            )
            .expect("register handler")
            .handler_id;

        registry
            .unregister(&handler_id)
            .expect("unregister should succeed");
        assert!(registry.get_handler(&handler_id).is_none());

        let err = registry.unregister(&handler_id).unwrap_err();
        assert!(matches!(err, DaemonError::HandlerNotRegistered));
    }

    #[tokio::test]
    async fn connection_handle_can_deliver_events() {
        let bots = vec![dummy_bot("echo-bot", &["ReadMessages"])];
        let mut registry = HandlerRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);

        let handler_id = registry
            .register(
                ConnectionHandle::new(tx),
                vec!["echo-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["ReadMessages".to_string()],
                &bots,
            )
            .expect("register handler")
            .handler_id;

        let handler = registry
            .get_handler(&handler_id)
            .expect("handler should exist");
        let event = sample_event("echo-bot");
        handler
            .send_event(event.clone())
            .expect("send should succeed");

        let received = rx.recv().await.expect("should receive event");
        let JsonRpcMessage::Notification { method, params, .. } = received else {
            panic!("expected notification");
        };
        assert_eq!(method, "agent.event");
        let payload = params.expect("params should be present");
        let received_event: AgentEvent = serde_json::from_value(payload).expect("valid event");
        assert_eq!(received_event.bot_id, event.bot_id);
        assert_eq!(received_event.content, event.content);
    }

    #[tokio::test]
    async fn status_notification_matches_schema_shape() {
        let bots = vec![dummy_bot("echo-bot", &["ReadMessages", "SendMessages"])];
        let mut registry = HandlerRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);

        let handler_id = registry
            .register(
                ConnectionHandle::new(tx),
                vec!["echo-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["ReadMessages".to_string(), "SendMessages".to_string()],
                &bots,
            )
            .expect("register handler")
            .handler_id;

        let handler = registry
            .get_handler(&handler_id)
            .expect("handler should exist");
        handler
            .send_status("ready", Some("npub1test"))
            .expect("send should succeed");

        let received = rx.recv().await.expect("should receive status");
        let JsonRpcMessage::Notification { method, params, .. } = received else {
            panic!("expected notification");
        };
        assert_eq!(method, "agent.status");
        let payload = params.expect("params should be present");
        let status: AgentStatusParams = serde_json::from_value(payload).expect("valid status");
        assert_eq!(status.state, "ready");
        assert_eq!(status.identity.as_deref(), Some("npub1test"));
        assert_eq!(
            status.capabilities,
            vec!["ReadMessages".to_string(), "SendMessages".to_string()]
        );
    }

    #[test]
    fn connection_handle_carries_transport_label() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let handle = ConnectionHandle::with_transport(tx, "http");
        assert_eq!(handle.transport(), "http");

        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let handle = ConnectionHandle::new(tx);
        assert_eq!(handle.transport(), "unknown");
    }

    #[test]
    fn disconnected_handler_is_not_stale_immediately() {
        let bots = vec![dummy_bot("echo-bot", &["ReadMessages"])];
        let mut registry = HandlerRegistry::new();
        let handler_id = registry
            .register(
                dummy_handle(),
                vec!["echo-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["ReadMessages".to_string()],
                &bots,
            )
            .expect("register handler")
            .handler_id;

        let handler = registry.get_handler_mut(&handler_id).unwrap();
        handler.disconnect();

        let handler = registry.get_handler(&handler_id).unwrap();
        assert!(!handler.is_connected());
        assert!(!handler.is_stale(Duration::from_secs(60)));
    }

    #[tokio::test]
    async fn reaping_removes_stale_handlers() {
        let keys = nostr::Keys::generate();
        let bot_config = BotConfig {
            id: "echo-bot".to_string(),
            npub: keys.public_key().to_bech32().unwrap(),
            signing: SigningConfig::Nsec {
                nsec: SecretString::new(keys.secret_key().to_bech32().unwrap().into()),
            },
            relays: vec![],
            capabilities: vec!["ReadMessages".to_string()],
            ..Default::default()
        };
        let bots = vec![bot_config.clone()];
        let config = DaemonConfig {
            daemon: GlobalDaemonConfig::default(),
            bots,
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
        let mut dispatch = Dispatch::new(cm.clone(), db, diagnostics);
        dispatch.set_handler_stale_timeout(Duration::from_millis(10));
        let dispatch = Arc::new(dispatch);

        let handler_id = {
            let mut cm = cm.write().await;
            let (tx, _rx) = tokio::sync::mpsc::channel(1);
            cm.handler_registry
                .register(
                    ConnectionHandle::with_transport(tx, "unix"),
                    vec!["echo-bot".to_string()],
                    vec!["dm_received".to_string()],
                    vec!["ReadMessages".to_string()],
                    &[bot_config],
                )
                .unwrap()
                .handler_id
        };

        // Disconnect the handler and wait for the reaper to remove it.
        dispatch.disconnect_handler(&handler_id).await;
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let reaper = dispatch.clone().spawn_handler_reaper(
            Duration::from_millis(10),
            Duration::from_millis(20),
            shutdown_rx,
        );
        tokio::time::sleep(Duration::from_millis(100)).await;

        let cm = cm.read().await;
        assert!(cm.handler_registry.get_handler(&handler_id).is_none());

        reaper.abort();
    }
}
