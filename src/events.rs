use serde::{Deserialize, Serialize};

/// Incoming event types a handler may receive.
#[derive(Debug, Clone, Default, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    #[default]
    DmReceived,
    MlsWelcomeReceived,
    MlsGroupMessageReceived,
    ReactionReceived,
    AttachmentReceived,
    MlsGroupReactionReceived,
    MlsGroupAttachmentReceived,
}

impl EventType {
    /// Return the snake_case wire name for this event type.
    pub fn as_wire_name(self) -> &'static str {
        match self {
            EventType::DmReceived => "dm_received",
            EventType::MlsWelcomeReceived => "mls_welcome_received",
            EventType::MlsGroupMessageReceived => "mls_group_message_received",
            EventType::ReactionReceived => "reaction_received",
            EventType::AttachmentReceived => "attachment_received",
            EventType::MlsGroupReactionReceived => "mls_group_reaction_received",
            EventType::MlsGroupAttachmentReceived => "mls_group_attachment_received",
        }
    }
}

/// Detail carried by a [`EventType::ReactionReceived`] event: the reacted-to
/// rumor and the emoji the reaction used.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionPayload {
    pub target_rumor_id: String,
    pub emoji: String,
}

/// Detail carried by a decrypted and verified attachment event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentPayload {
    pub mime_type: String,
    /// Plaintext byte count, equal to the size of the file at [`Self::path`].
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ox: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blurhash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dim: Option<String>,
    pub path: String,
    /// Unix timestamp after which the inbound spool file may be removed.
    pub expires_at: u64,
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
    /// Present only on [`EventType::ReactionReceived`] events; mutually
    /// exclusive with any future sub-object per KTD1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reaction: Option<ReactionPayload>,
    /// Present only on [`EventType::AttachmentReceived`] events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<AttachmentPayload>,
}
