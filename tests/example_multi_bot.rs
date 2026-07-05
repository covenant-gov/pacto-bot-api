mod common;
mod support;

/// Example: multi-bot multiplexing over a single daemon.
///
/// This test is intentionally written as a readable, end-to-end example. It
/// shows how one `pacto-bot-api` daemon can own several bot identities at the
/// same time and route incoming DMs to the handler registered for each bot.
///
/// 1. Start one daemon with two nsec-backed bots (`alice` and `bob`).
/// 2. Register a separate Unix-socket handler for each bot.
/// 3. Publish a kind:1059 gift wrap addressed to Alice and another to Bob.
/// 4. Confirm each handler receives only its own bot's `agent.event`.
///
/// See `tests/multi_bot.rs` for a more exhaustive multiplexing test that also
/// exercises mixed signing backends (nsec + bunker_local).
use std::time::Duration;

use nostr::Keys;
use pacto_bot_api::transport::protocol::JsonRpcMessage;
use serde_json::Value;
use support::mock_relay::MockRelay;

#[tokio::test]
async fn multi_bot_example_daemon_routes_dms_to_the_right_handler()
-> Result<(), Box<dyn std::error::Error>> {
    let relay = MockRelay::start().await?;
    let dir = common::tempdir()?;

    // 1. Configure two bots that share the same relay.
    let (mut alice_config, _alice_nsec) = common::generate_nsec_bot("alice")?;
    alice_config.relays = vec![relay.url()];
    alice_config.capabilities = vec!["ReadMessages".into(), "SendMessages".into()];

    let (mut bob_config, _bob_nsec) = common::generate_nsec_bot("bob")?;
    bob_config.relays = vec![relay.url()];
    bob_config.capabilities = vec!["ReadMessages".into(), "SendMessages".into()];

    let config_path = common::make_config(&dir, vec![alice_config.clone(), bob_config.clone()])?;
    let daemon = common::spawn_daemon_until_ready(&config_path).await?;

    let socket_path = dir.path().join("pacto-bot-api.sock");

    // 2. Register one handler per bot.
    let mut alice_handler = common::HandlerClient::register(
        &socket_path,
        &["alice"],
        &["dm_received"],
        &["ReadMessages", "SendMessages"],
    )
    .await?;

    let mut bob_handler = common::HandlerClient::register(
        &socket_path,
        &["bob"],
        &["dm_received"],
        &["ReadMessages", "SendMessages"],
    )
    .await?;

    // Give the daemon relay subscriptions time to settle.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 3. A single user sends a DM to each bot.
    let sender = Keys::generate();
    let alice_gift = common::build_gift_wrap(&sender, &alice_config.npub, "hello Alice").await?;
    let bob_gift = common::build_gift_wrap(&sender, &bob_config.npub, "hello Bob").await?;
    relay.inject_event(alice_gift).await;
    relay.inject_event(bob_gift).await;

    // 4. Wait for both notifications concurrently. Each handler must receive
    //    exactly the DM addressed to its bot.
    let (alice_notification, bob_notification) = tokio::join!(
        alice_handler.next_notification(Duration::from_secs(10)),
        bob_handler.next_notification(Duration::from_secs(10)),
    );

    let alice_notification = alice_notification?;
    let bob_notification = bob_notification?;
    assert_event_for_bot(&alice_notification, "alice", "hello Alice")?;
    assert_event_for_bot(&bob_notification, "bob", "hello Bob")?;

    // 5. Handlers can reply independently. Here we just ack both events to
    //    show the call is authorized per-handler.
    let alice_event_id = extract_event_id(&alice_notification)?;
    let bob_event_id = extract_event_id(&bob_notification)?;
    alice_handler
        .send_response(&alice_event_id, "ack", None)
        .await?;
    bob_handler
        .send_response(&bob_event_id, "ack", None)
        .await?;

    common::shutdown_daemon(daemon).await?;
    relay.stop().await;
    Ok(())
}

/// Assert that a notification is an `agent.event` for the expected bot.
fn assert_event_for_bot(
    notification: &JsonRpcMessage,
    expected_bot_id: &str,
    expected_content: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    match notification {
        JsonRpcMessage::Notification { method, params, .. } if method == "agent.event" => {
            let params = params.as_ref().ok_or("agent.event missing params")?;
            let bot_id = params
                .get("bot_id")
                .and_then(Value::as_str)
                .ok_or("agent.event missing bot_id")?;
            let content = params
                .get("content")
                .and_then(Value::as_str)
                .ok_or("agent.event missing content")?;
            assert_eq!(bot_id, expected_bot_id);
            assert_eq!(content, expected_content);
            Ok(())
        }
        _ => {
            Err(format!("expected agent.event for {expected_bot_id}, got {notification:?}").into())
        }
    }
}

/// Extract the event_id from an `agent.event` notification.
fn extract_event_id(notification: &JsonRpcMessage) -> Result<String, Box<dyn std::error::Error>> {
    match notification {
        JsonRpcMessage::Notification { method, params, .. } if method == "agent.event" => {
            let params = params.as_ref().ok_or("agent.event missing params")?;
            params
                .get("event_id")
                .and_then(Value::as_str)
                .map(String::from)
                .ok_or_else(|| "agent.event missing event_id".into())
        }
        _ => Err("expected agent.event notification".into()),
    }
}
