mod common;
mod support;

/// req(R4, R5, R12, R13, R15, R17, R33)
///
/// Integration test verifying that a single daemon instance can multiplex
/// multiple bot identities using different signing backends. One bot uses a
/// local nsec signer and another uses an in-process NIP-46 bunker
/// (`bunker_local`). A DM is routed to each bot, each handler receives the
/// correct `agent.event` notification, and each bot replies under its own
/// configured npub.
use std::time::Duration;

use nostr::{JsonUtil, Keys, Kind, NostrSigner, PublicKey};
use pacto_bot_api::transport::protocol::JsonRpcMessage;
use serde_json::Value;
use support::mock_bunker::MockBunker;
use support::mock_relay::MockRelay;

/// Wait for the daemon to publish kind:1059 gift wraps addressed to `sender_pubkey`.
async fn wait_for_replies(
    relay: &MockRelay,
    sender_pubkey: &PublicKey,
    expected_count: usize,
    timeout_duration: Duration,
) -> Result<Vec<nostr::Event>, Box<dyn std::error::Error>> {
    let deadline = tokio::time::Instant::now() + timeout_duration;

    loop {
        let events = relay.events().await;
        let replies: Vec<_> = events
            .iter()
            .filter(|e| {
                e.kind == Kind::GiftWrap && e.tags.public_keys().any(|p| p == sender_pubkey)
            })
            .cloned()
            .collect();

        if replies.len() >= expected_count {
            return Ok(replies);
        }

        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for {expected_count} reply gift wraps, found {}",
                replies.len()
            )
            .into());
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Decrypt a kind:1059 gift wrap received by `recipient` and return the
/// public key of the sender (from the inner seal) along with the plaintext
/// rumor content.
async fn decrypt_reply_content(
    recipient: &Keys,
    gift_wrap: &nostr::Event,
) -> Result<(PublicKey, String), Box<dyn std::error::Error>> {
    // The gift-wrap content is encrypted from the ephemeral pubkey to the recipient.
    let seal_json = recipient
        .nip44_decrypt(&gift_wrap.pubkey, &gift_wrap.content)
        .await?;
    let seal: nostr::Event = nostr::Event::from_json(&seal_json)?;

    // The seal's pubkey identifies the bot that authored the reply.
    let sender_pubkey = seal.pubkey;

    // The seal content is the encrypted rumor carrying the DM plaintext.
    let rumor_json = recipient
        .nip44_decrypt(&sender_pubkey, &seal.content)
        .await?;
    let rumor: nostr::UnsignedEvent = nostr::UnsignedEvent::from_json(&rumor_json)?;

    Ok((sender_pubkey, rumor.content))
}

#[tokio::test]
async fn multi_bot_multiplexing_with_mixed_signing_backends()
-> Result<(), Box<dyn std::error::Error>> {
    let relay = MockRelay::start().await?;
    let dir = common::tempdir()?;

    // Nsec-backed bot.
    let (mut nsec_config, _nsec) = common::generate_nsec_bot("nsec-bot")?;
    nsec_config.relays = vec![relay.url()];
    nsec_config.capabilities = vec!["ReadMessages".into(), "SendMessages".into()];

    // Bunker-local-backed bot.
    let (mut bunker_config, bunker_keys) =
        common::generate_bunker_bot_with_keys("bunker-bot", true)?;
    let bunker = MockBunker::new(bunker_keys, vec![relay.url()]).await?;
    let uri = bunker
        .uri_from_relays(&[relay.url()])
        .ok_or("mock bunker produced no URI")?;
    common::set_bunker_uri(&mut bunker_config, &uri);
    bunker_config.relays = vec![relay.url()];
    bunker_config.capabilities = vec!["ReadMessages".into(), "SendMessages".into()];

    // Let the mock bunker subscribe before the daemon starts and connects.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let config_path = common::make_config(&dir, vec![nsec_config.clone(), bunker_config.clone()])?;
    let log_path = dir.path().join("daemon.log");
    let daemon = common::spawn_daemon_until_ready_with_log(&config_path, Some(&log_path)).await?;

    let result: Result<(), Box<dyn std::error::Error>> = async {
        let socket_path = dir.path().join("pacto-bot-api.sock");

        // Register a handler for each bot.
        let mut nsec_handler = common::HandlerClient::register(
            &socket_path,
            &["nsec-bot"],
            &["dm_received"],
            &["ReadMessages", "SendMessages"],
        )
        .await?;

        let mut bunker_handler = common::HandlerClient::register(
            &socket_path,
            &["bunker-bot"],
            &["dm_received"],
            &["ReadMessages", "SendMessages"],
        )
        .await?;

        // Give the daemon relay subscriptions and bunker connection time to settle.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let sender = Keys::generate();

        // Send a DM to the nsec bot.
        let nsec_gift = common::build_gift_wrap(&sender, &nsec_config.npub, "for nsec").await?;
        relay.inject_event(nsec_gift).await;

        // Send a DM to the bunker bot.
        let bunker_gift =
            common::build_gift_wrap(&sender, &bunker_config.npub, "for bunker").await?;
        relay.inject_event(bunker_gift).await;

        // Each handler should receive exactly its own notification. Wait for
        // both concurrently so the nsec bot isn't held up by the bunker bot's
        // NIP-46 handshake.
        let (nsec_notification, bunker_notification) = tokio::join!(
            nsec_handler.next_notification(Duration::from_secs(10)),
            bunker_handler.next_notification(Duration::from_secs(10)),
        );
        let nsec_event_id =
            extract_event_id_and_content(&nsec_notification?, "nsec-bot", "for nsec")?;
        let bunker_event_id =
            extract_event_id_and_content(&bunker_notification?, "bunker-bot", "for bunker")?;

        // Reply from each bot.
        nsec_handler
            .send_response(&nsec_event_id, "reply", Some("nsec reply"))
            .await?;
        bunker_handler
            .send_response(&bunker_event_id, "reply", Some("bunker reply"))
            .await?;

        // Collect replies published to the relay.
        let replies = wait_for_replies(
            &relay, &sender.public_key(), 2, Duration::from_secs(10)).await?;
        assert_eq!(
            replies.len(),
            2,
            "daemon should publish exactly two reply gift wraps"
        );

        // Parse the configured npubs into public keys for comparison.
        let nsec_pubkey = PublicKey::parse(&nsec_config.npub)?;
        let bunker_pubkey = PublicKey::parse(&bunker_config.npub)?;

        let mut seen_nsec = false;
        let mut seen_bunker = false;
        for reply in &replies {
            let (reply_pubkey, content) = decrypt_reply_content(&sender, reply).await?;
            if reply_pubkey == nsec_pubkey {
                assert_eq!(content, "nsec reply");
                seen_nsec = true;
            } else if reply_pubkey == bunker_pubkey {
                assert_eq!(content, "bunker reply");
                seen_bunker = true;
            } else {
                return Err(format!(
                    "unexpected reply signer {reply_pubkey}, expected {nsec_pubkey} or {bunker_pubkey}"
                )
                .into());
            }
        }
        assert!(seen_nsec, "expected a reply signed by the nsec bot");
        assert!(seen_bunker, "expected a reply signed by the bunker bot");

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

fn extract_event_id_and_content(
    notification: &JsonRpcMessage,
    expected_bot_id: &str,
    expected_content: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    match notification {
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
            let bot_id = params
                .get("bot_id")
                .and_then(Value::as_str)
                .ok_or("agent.event missing bot_id")?;
            assert_eq!(bot_id, expected_bot_id);
            assert_eq!(content, expected_content);
            Ok(event_id)
        }
        _ => Err(format!(
            "expected agent.event notification for {expected_bot_id}, got {:?}",
            notification
        )
        .into()),
    }
}
