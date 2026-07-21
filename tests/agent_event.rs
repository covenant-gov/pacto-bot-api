//! req(R5, R6, R7, R12)
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use pacto_bot_api::events::{AgentEvent, EventType};

#[test]
fn deserialize_agent_event_with_all_mention_fields() {
    let json = r#"{
        "bot_id": "echo-bot",
        "event_id": "evt-1",
        "type": "mls_group_message_received",
        "chat_id": "squad-wire-id",
        "content": "@Echo Bot hello",
        "mentions": ["npub1echo"],
        "is_mentioned": true,
        "mentioned_bot_ids": ["echo-bot"],
        "rumor_id": "rumor-1",
        "author": "npub1author",
        "timestamp": 1234567890
    }"#;

    let event: AgentEvent = serde_json::from_str(json).unwrap();
    assert_eq!(event.bot_id, "echo-bot");
    assert_eq!(event.event_type, EventType::MlsGroupMessageReceived);
    assert_eq!(event.content, "@Echo Bot hello");
    assert_eq!(event.mentions, vec!["npub1echo"]);
    assert!(event.is_mentioned);
    assert_eq!(event.mentioned_bot_ids, vec!["echo-bot"]);
}

#[test]
fn deserialize_agent_event_without_new_fields_uses_defaults() {
    let json = r#"{
        "bot_id": "echo-bot",
        "event_id": "evt-1",
        "type": "dm_received",
        "chat_id": "npub1sender",
        "content": "hello",
        "rumor_id": "rumor-1",
        "author": "npub1author",
        "timestamp": 1234567890
    }"#;

    let event: AgentEvent = serde_json::from_str(json).unwrap();
    assert_eq!(event.event_type, EventType::DmReceived);
    assert!(event.mentions.is_empty());
    assert!(!event.is_mentioned);
    assert!(event.mentioned_bot_ids.is_empty());
}

#[test]
fn deserialize_agent_event_partial_mention_fields_uses_defaults() {
    let json = r#"{
        "bot_id": "echo-bot",
        "event_id": "evt-1",
        "type": "mls_group_message_received",
        "chat_id": "squad-wire-id",
        "content": "hello",
        "mentions": ["npub1other"],
        "rumor_id": "rumor-1",
        "author": "npub1author",
        "timestamp": 1234567890
    }"#;

    let event: AgentEvent = serde_json::from_str(json).unwrap();
    assert_eq!(event.mentions, vec!["npub1other"]);
    assert!(!event.is_mentioned);
    assert!(event.mentioned_bot_ids.is_empty());
}

#[test]
fn serialize_agent_event_includes_new_fields() {
    let event = AgentEvent {
        bot_id: "echo-bot".into(),
        event_id: "evt-1".into(),
        event_type: EventType::MlsGroupMessageReceived,
        chat_id: Some("squad-wire-id".into()),
        content: "hello".into(),
        mentions: vec!["npub1echo".into()],
        is_mentioned: true,
        mentioned_bot_ids: vec!["echo-bot".into()],
        rumor_id: "rumor-1".into(),
        author: "npub1author".into(),
        timestamp: 1234567890,
    };

    let value = serde_json::to_value(&event).unwrap();
    assert_eq!(value["mentions"], serde_json::json!(["npub1echo"]));
    assert_eq!(value["is_mentioned"], serde_json::json!(true));
    assert_eq!(value["mentioned_bot_ids"], serde_json::json!(["echo-bot"]));
}
