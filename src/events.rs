use serde::{Deserialize, Serialize};

/// Incoming event types a handler may receive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    DmReceived,
    MlsWelcomeReceived,
}

impl EventType {
    /// Return the snake_case wire name for this event type.
    pub fn as_wire_name(self) -> &'static str {
        match self {
            EventType::DmReceived => "dm_received",
            EventType::MlsWelcomeReceived => "mls_welcome_received",
        }
    }
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
