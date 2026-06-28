mod common;
mod support;

/// req(R4, R5, R12, R13, R15, R17, R33)
use std::time::Duration;

use nostr::{Keys, Kind, PublicKey};
use pacto_bot_api::transport::protocol::JsonRpcMessage;
use serde_json::Value;
use support::mock_bunker::MockBunker;
use support::mock_relay::MockRelay;

/// Wait for the daemon to publish a kind:1059 gift wrap addressed to `sender_pubkey`.
async fn wait_for_reply(
    relay: &MockRelay,
    sender_pubkey: &PublicKey,
    timeout_duration: Duration,
) -> Result<Vec<nostr::Event>, Box<dyn std::error::Error>> {
    relay
        .wait_for_event(
            |e| e.kind == Kind::GiftWrap && e.tags.public_keys().any(|p| p == sender_pubkey),
            timeout_duration,
        )
        .await
}

#[tokio::test]
async fn full_dm_round_trip_over_unix_socket() -> Result<(), Box<dyn std::error::Error>> {
    let relay = MockRelay::start().await?;
    let dir = tempfile::tempdir()?;

    let (bot_config, _nsec) = common::generate_nsec_bot("echo-bot")?;

    let mut config_bots = bot_config.clone();
    config_bots.relays = vec![relay.url()];
    config_bots.capabilities = vec!["ReadMessages".into(), "SendMessages".into()];
    let config_path = common::make_config(&dir, vec![config_bots])?;

    let daemon = common::spawn_daemon_until_ready(&config_path).await?;

    let socket_path = dir.path().join("pacto-bot-api.sock");
    let mut handler = common::HandlerClient::register(
        &socket_path,
        &["echo-bot"],
        &["dm_received"],
        &["ReadMessages", "SendMessages"],
    )
    .await?;

    // Sender keys represent the human/user sending a DM to the bot.
    let sender = Keys::generate();
    let gift = common::build_gift_wrap(&sender, &bot_config.npub, "/echo hello").await?;
    relay.inject_event(gift).await;

    // Wait for the daemon to dispatch the event to the handler.
    let notification = handler.next_notification(Duration::from_secs(5)).await?;
    let event_id = match &notification {
        JsonRpcMessage::Notification { method, params, .. } if method == "agent.event" => {
            let params = params.as_ref().ok_or("agent.event missing params")?;
            let event_id = params
                .get("event_id")
                .and_then(Value::as_str)
                .ok_or("agent.event missing event_id")?
                .to_string();
            let content = params
                .get("content")
                .and_then(Value::as_str)
                .ok_or("agent.event missing content")?;
            assert_eq!(content, "/echo hello");
            event_id
        }
        _ => {
            return Err(
                format!("expected agent.event notification, got {:?}", notification).into(),
            );
        }
    };

    // Reply with the echoed content.
    handler
        .send_response(&event_id, "reply", Some("hello"))
        .await?;

    // Wait for the daemon to publish an echo reply gift wrap addressed to the sender.
    let replies = wait_for_reply(&relay, &sender.public_key(), Duration::from_secs(5)).await?;
    assert!(
        !replies.is_empty(),
        "daemon should publish a reply gift wrap"
    );

    common::shutdown_daemon(daemon).await?;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn bunker_local_dm_round_trip_over_unix_socket() -> Result<(), Box<dyn std::error::Error>> {
    let relay = MockRelay::start().await?;
    let dir = tempfile::tempdir()?;

    let (mut bot_config, bunker_keys) = common::generate_bunker_bot_with_keys("echo-bot", true)?;
    let bunker = MockBunker::new(bunker_keys, vec![relay.url()]).await?;
    let uri = bunker
        .uri_from_relays(&[relay.url()])
        .ok_or("mock bunker produced no URI")?;
    common::set_bunker_uri(&mut bot_config, &uri);
    bot_config.relays = vec![relay.url()];
    bot_config.capabilities = vec!["ReadMessages".into(), "SendMessages".into()];

    let config_path = common::make_config(&dir, vec![bot_config.clone()])?;
    let log_path = dir.path().join("daemon.log");
    let daemon = common::spawn_daemon_until_ready_with_log(&config_path, Some(&log_path)).await?;

    let result: Result<(), Box<dyn std::error::Error>> = async {
        let socket_path = dir.path().join("pacto-bot-api.sock");
        let mut handler = common::HandlerClient::register(
            &socket_path,
            &["echo-bot"],
            &["dm_received"],
            &["ReadMessages", "SendMessages"],
        )
        .await?;

        // Give the bunker and daemon subscriptions time to settle.
        tokio::time::sleep(Duration::from_millis(500)).await;

        let sender = Keys::generate();
        let gift = common::build_gift_wrap(&sender, &bot_config.npub, "/echo bunker").await?;
        relay.inject_event(gift).await;

        let notification = handler.next_notification(Duration::from_secs(10)).await?;
        let event_id = match &notification {
            JsonRpcMessage::Notification { method, params, .. } if method == "agent.event" => {
                let params = params.as_ref().ok_or("agent.event missing params")?;
                let event_id = params
                    .get("event_id")
                    .and_then(Value::as_str)
                    .ok_or("agent.event missing event_id")?
                    .to_string();
                let content = params
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or("agent.event missing content")?;
                assert_eq!(content, "/echo bunker");
                event_id
            }
            _ => {
                return Err(
                    format!("expected agent.event notification, got {:?}", notification).into(),
                );
            }
        };

        handler
            .send_response(&event_id, "reply", Some("bunker hello"))
            .await?;

        let replies = wait_for_reply(&relay, &sender.public_key(), Duration::from_secs(10)).await?;
        assert!(
            !replies.is_empty(),
            "daemon should publish a reply gift wrap for a bunker_local bot"
        );

        Ok(())
    }
    .await;

    common::shutdown_daemon(daemon).await?;
    bunker.stop().await;
    relay.stop().await;

    if let Err(e) = &result {
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        eprintln!("daemon log:\n{log}");
        return Err(format!("{e}").into());
    }

    Ok(())
}

#[tokio::test]
async fn multi_bot_multiplexing() -> Result<(), Box<dyn std::error::Error>> {
    let relay = MockRelay::start().await?;
    let dir = tempfile::tempdir()?;

    let (mut echo_config, _nsec) = common::generate_nsec_bot("echo-bot")?;
    echo_config.relays = vec![relay.url()];
    echo_config.capabilities = vec!["ReadMessages".into(), "SendMessages".into()];

    let (mut other_config, _nsec2) = common::generate_nsec_bot("other-bot")?;
    other_config.relays = vec![relay.url()];
    other_config.capabilities = vec!["ReadMessages".into(), "SendMessages".into()];

    let config_path = common::make_config(&dir, vec![echo_config.clone(), other_config.clone()])?;
    let daemon = common::spawn_daemon_until_ready(&config_path).await?;

    let socket_path = dir.path().join("pacto-bot-api.sock");
    let mut echo_handler = common::HandlerClient::register(
        &socket_path,
        &["echo-bot"],
        &["dm_received"],
        &["ReadMessages", "SendMessages"],
    )
    .await?;

    let sender = Keys::generate();

    // DM for echo-bot should be delivered.
    let echo_gift = common::build_gift_wrap(&sender, &echo_config.npub, "for echo").await?;
    relay.inject_event(echo_gift).await;

    let notification = echo_handler
        .next_notification(Duration::from_secs(5))
        .await?;
    assert_eq!(notification.method(), Some("agent.event"));

    // DM for other-bot should not be delivered to the echo-only handler.
    let other_gift = common::build_gift_wrap(&sender, &other_config.npub, "for other").await?;
    relay.inject_event(other_gift).await;

    let timeout_result = echo_handler
        .next_notification(Duration::from_millis(500))
        .await;
    assert!(
        timeout_result.is_err(),
        "echo handler should not receive other-bot events"
    );

    common::shutdown_daemon(daemon).await?;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn handler_fan_out() -> Result<(), Box<dyn std::error::Error>> {
    let relay = MockRelay::start().await?;
    let dir = tempfile::tempdir()?;

    let (mut bot_config, _nsec) = common::generate_nsec_bot("echo-bot")?;
    bot_config.relays = vec![relay.url()];
    bot_config.capabilities = vec!["ReadMessages".into(), "SendMessages".into()];
    let config_path = common::make_config(&dir, vec![bot_config.clone()])?;

    let daemon = common::spawn_daemon_until_ready(&config_path).await?;

    let socket_path = dir.path().join("pacto-bot-api.sock");
    let mut handler_a = common::HandlerClient::register(
        &socket_path,
        &["echo-bot"],
        &["dm_received"],
        &["ReadMessages", "SendMessages"],
    )
    .await?;
    let mut handler_b = common::HandlerClient::register(
        &socket_path,
        &["echo-bot"],
        &["dm_received"],
        &["ReadMessages", "SendMessages"],
    )
    .await?;

    let sender = Keys::generate();
    let gift = common::build_gift_wrap(&sender, &bot_config.npub, "fan out").await?;
    relay.inject_event(gift).await;

    let notif_a = handler_a.next_notification(Duration::from_secs(5)).await?;
    let notif_b = handler_b.next_notification(Duration::from_secs(5)).await?;

    assert_eq!(notif_a.method(), Some("agent.event"));
    assert_eq!(notif_b.method(), Some("agent.event"));

    common::shutdown_daemon(daemon).await?;
    relay.stop().await;
    Ok(())
}
