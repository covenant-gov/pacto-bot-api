use serde::{Deserialize, Serialize};

/// Incoming event types a handler may receive.
#[derive(Debug, Clone, Default, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    #[default]
    DmReceived,
    MlsWelcomeReceived,
    MlsGroupMessageReceived,
}

impl EventType {
    /// Return the snake_case wire name for this event type.
    pub fn as_wire_name(self) -> &'static str {
        match self {
            EventType::DmReceived => "dm_received",
            EventType::MlsWelcomeReceived => "mls_welcome_received",
            EventType::MlsGroupMessageReceived => "mls_group_message_received",
        }
    }
}

/// Notification sent from daemon to handler when an event arrives for a bot.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentEvent {
    pub bot_id: String,
    pub event_id: String,
    #[serde(rename = "type")]
    pub event_type: EventType,
    pub chat_id: Option<String>,
    pub content: String,
    #[serde(default)]
    pub mentions: Vec<String>,
    #[serde(default)]
    pub is_mentioned: bool,
    #[serde(default)]
    pub mentioned_bot_ids: Vec<String>,
    #[serde(
        rename = "pacto_virtual_bucket",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub pacto_virtual_bucket: Option<String>,
    pub rumor_id: String,
    pub author: String,
    pub timestamp: u64,
}
