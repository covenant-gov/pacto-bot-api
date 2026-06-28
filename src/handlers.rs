use crate::config::BotConfig;
use crate::errors::DaemonError;
use crate::events::{AgentEvent, EventType};
use crate::transport::protocol::JsonRpcMessage;
use chrono::{DateTime, Utc};
#[cfg(test)]
use secrecy::SecretString;
use serde_json::json;
use std::collections::HashMap;
use tokio::sync::mpsc::Sender;

/// Capability a handler may request for a bot.
pub type Capability = String;

/// Lightweight handle to a handler connection for outbound JSON-RPC notifications.
#[derive(Debug, Clone)]
pub struct ConnectionHandle {
    sender: Sender<JsonRpcMessage>,
}

impl ConnectionHandle {
    pub fn new(sender: Sender<JsonRpcMessage>) -> Self {
        Self { sender }
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
    pub registered_at: DateTime<Utc>,
}

impl HandlerRef {
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
    pub fn send_status(&self, state: &str) -> Result<(), DaemonError> {
        let msg = JsonRpcMessage::notification("agent.status", Some(json!({ "state": state })));
        match &self.connection {
            Some(conn) => conn.send(msg),
            None => Err(DaemonError::HandlerNotRegistered),
        }
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
    /// The server generates a UUIDv4 handler_id; clients must not supply one.
    pub fn register(
        &mut self,
        connection: ConnectionHandle,
        bot_ids: Vec<String>,
        event_types: Vec<String>,
        capabilities: Vec<Capability>,
        bot_configs: &[BotConfig],
    ) -> Result<String, DaemonError> {
        // Validate all bot_ids exist and that requested capabilities are a subset
        // of each bot's configured capabilities.
        for bot_id in &bot_ids {
            let bot = bot_configs
                .iter()
                .find(|b| b.id == *bot_id)
                .ok_or_else(|| DaemonError::UnknownBot(bot_id.clone()))?;
            for cap in &capabilities {
                if !bot.capabilities.contains(cap) {
                    return Err(DaemonError::Config(format!(
                        "capability {cap} not granted to bot {bot_id}"
                    )));
                }
            }
        }

        // Validate event types are recognized.
        let mut parsed_event_types = Vec::with_capacity(event_types.len());
        for event_type in &event_types {
            let parsed = parse_event_type(event_type)?;
            parsed_event_types.push(parsed);
        }

        let id = uuid::Uuid::new_v4().to_string();
        let handler = HandlerRef {
            id: id.clone(),
            connection: Some(connection),
            bot_ids,
            event_types: parsed_event_types,
            capabilities,
            registered_at: Utc::now(),
        };

        self.handlers.insert(id.clone(), handler);
        Ok(id)
    }

    pub fn unregister(&mut self, handler_id: &str) -> Result<(), DaemonError> {
        self.handlers
            .remove(handler_id)
            .ok_or(DaemonError::HandlerNotRegistered)?;
        Ok(())
    }

    pub fn get_handler(&self, handler_id: &str) -> Option<&HandlerRef> {
        self.handlers.get(handler_id)
    }

    /// Find all handlers registered for the given bot and event type (fan-out).
    pub fn find(&self, bot_id: &str, event_type: EventType) -> Vec<HandlerRef> {
        self.handlers
            .values()
            .filter(|h| h.matches(bot_id, event_type))
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
    use crate::config::{BotConfig, SigningConfig};

    fn dummy_bot(id: &str, capabilities: &[&str]) -> BotConfig {
        BotConfig {
            id: id.to_string(),
            npub: format!("npub1{id}"),
            signing: SigningConfig::Nsec {
                nsec: SecretString::new("nsec1dummy".to_string().into()),
            },
            relays: vec![],
            capabilities: capabilities.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn dummy_handle() -> ConnectionHandle {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
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
            .expect("registration should succeed");

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
            .expect("register handler a");
        let id_b = registry
            .register(
                dummy_handle(),
                vec!["echo-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["ReadMessages".to_string(), "SendMessages".to_string()],
                &bots,
            )
            .expect("register handler b");

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
            .expect("register handler");

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
            .expect("register handler");

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
            .expect("register handler");

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
}
