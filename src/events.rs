use serde::{Deserialize, Serialize};

/// Incoming event types a handler may receive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    DmReceived,
}

/// Notification sent from daemon to handler when an event arrives for a bot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEvent {
    pub bot_id: String,
    pub event_id: String,
    #[serde(rename = "type")]
    pub event_type: EventType,
    pub chat_id: Option<String>,
    pub content: String,
    pub rumor_id: String,
    pub author: String,
    pub timestamp: u64,
}
