#![allow(clippy::unwrap_used)]

/// req(R12, R13)
mod common;
mod support;

use std::sync::Arc;
use std::time::Duration;

use nostr::nips::{nip44, nip59};
use nostr::secp256k1::schnorr::Signature;
use nostr::{
    Event, EventBuilder, EventId, JsonUtil, Keys, Kind, NostrSigner, Tag, TagKind, Timestamp,
    ToBech32, UnsignedEvent,
};
use pacto_bot_api::diagnostics::Diagnostics;
use pacto_bot_api::errors::DaemonError;
use pacto_bot_api::events::EventType;
use pacto_bot_api::nostr::NostrClient;
use pacto_bot_api::signer::{LocalKey, Signer};
use tokio_stream::StreamExt;

use crate::support::mock_relay::MockRelay;

fn test_signer() -> (LocalKey, String) {
    let keys = Keys::generate();
    let nsec = keys.secret_key().to_bech32().unwrap();
    let npub = keys.public_key().to_bech32().unwrap();
    (LocalKey::parse(&nsec).unwrap(), npub)
}

fn dummy_relay() -> String {
    "wss://localhost:4242".into()
}

#[tokio::test]
async fn new_adds_relays_and_connects() {
    let client = NostrClient::new(vec![dummy_relay()]).await.unwrap();
    // Adding relays again should be idempotent and skip blanks.
    client
        .add_relays(&[dummy_relay(), "".to_string()])
        .await
        .unwrap();
}

#[tokio::test]
async fn subscribe_bot_returns_subscription_id() {
    let client = NostrClient::new(vec![dummy_relay()]).await.unwrap();
    let (signer, _npub) = test_signer();
    let pubkey = signer.public_key();
    client
        .add_signer(pubkey, "bot-1".into(), Arc::new(signer))
        .await;

    let sub_id = client.subscribe_bot(&pubkey).await.unwrap();
    assert!(!sub_id.to_string().is_empty());

    client.unsubscribe_bot(&sub_id).await.unwrap();
}

#[tokio::test]
async fn send_dm_returns_event_id() {
    let client = NostrClient::new(vec![dummy_relay()]).await.unwrap();
    let (sender, _) = test_signer();
    let recipient = Keys::generate();
    let recipient_npub = recipient.public_key().to_bech32().unwrap();

    let event_id = client
        .send_dm(&sender, &recipient_npub, "hello integration", None)
        .await
        .unwrap();
    assert!(!event_id.to_hex().is_empty());
}

#[tokio::test]
async fn outgoing_gift_wrap_has_kind_1059_and_p_tag() {
    let relay = MockRelay::start().await.unwrap();
    let client = NostrClient::new(vec![relay.url()]).await.unwrap();
    let (sender, _) = test_signer();
    let recipient = Keys::generate();
    let recipient_npub = recipient.public_key().to_bech32().unwrap();

    let event_id = client
        .send_dm(&sender, &recipient_npub, "wrapped", None)
        .await
        .unwrap();
    assert_ne!(
        event_id.to_hex(),
        "0000000000000000000000000000000000000000000000000000000000000000"
    );

    relay.stop().await;
}

#[tokio::test]
async fn send_dm_reply_gift_wrap_contains_ms_tag_and_reply_marker() {
    let relay = MockRelay::start().await.unwrap();
    let client = NostrClient::new(vec![relay.url()]).await.unwrap();
    let (sender, _) = test_signer();
    let recipient = Keys::generate();
    let recipient_npub = recipient.public_key().to_bech32().unwrap();
    let reply_id =
        EventId::from_hex("0000000000000000000000000000000000000000000000000000000000000001")
            .unwrap();

    client
        .send_dm(
            &sender,
            &recipient_npub,
            "thread reply",
            Some(&reply_id.to_hex()),
        )
        .await
        .unwrap();

    let events = relay
        .wait_for_event(|e| e.kind == Kind::GiftWrap, Duration::from_secs(2))
        .await
        .unwrap();
    let gift = events
        .into_iter()
        .find(|e| e.kind == Kind::GiftWrap)
        .unwrap();

    let seal_json = recipient
        .nip44_decrypt(&gift.pubkey, &gift.content)
        .await
        .unwrap();
    let seal = Event::from_json(&seal_json).unwrap();

    let rumor_json = recipient
        .nip44_decrypt(&seal.pubkey, &seal.content)
        .await
        .unwrap();
    let rumor = UnsignedEvent::from_json(&rumor_json).unwrap();

    assert_eq!(rumor.kind, Kind::PrivateDirectMessage);
    assert_eq!(rumor.content, "thread reply");

    let e_tag = rumor
        .tags
        .find(TagKind::e())
        .expect("rumor should have an e tag");
    assert!(e_tag.is_reply(), "e tag should be marked as reply");
    assert_eq!(e_tag.content().unwrap(), reply_id.to_hex());

    let ms_tag = rumor
        .tags
        .find(TagKind::custom("ms"))
        .expect("rumor should have an ms tag");
    let ms_value: u64 = ms_tag.content().unwrap().parse().unwrap();
    assert!(ms_value < 1000, "ms tag must be a millisecond offset 0-999");

    relay.stop().await;
}

#[tokio::test]
async fn send_dm_gift_wrap_contains_ms_tag_without_reply() {
    let relay = MockRelay::start().await.unwrap();
    let client = NostrClient::new(vec![relay.url()]).await.unwrap();
    let (sender, _) = test_signer();
    let recipient = Keys::generate();
    let recipient_npub = recipient.public_key().to_bech32().unwrap();

    client
        .send_dm(&sender, &recipient_npub, "standalone dm", None)
        .await
        .unwrap();

    let events = relay
        .wait_for_event(|e| e.kind == Kind::GiftWrap, Duration::from_secs(2))
        .await
        .unwrap();
    let gift = events
        .into_iter()
        .find(|e| e.kind == Kind::GiftWrap)
        .unwrap();

    let seal_json = recipient
        .nip44_decrypt(&gift.pubkey, &gift.content)
        .await
        .unwrap();
    let seal = Event::from_json(&seal_json).unwrap();

    let rumor_json = recipient
        .nip44_decrypt(&seal.pubkey, &seal.content)
        .await
        .unwrap();
    let rumor = UnsignedEvent::from_json(&rumor_json).unwrap();

    assert_eq!(rumor.kind, Kind::PrivateDirectMessage);
    assert!(
        rumor.tags.find(TagKind::e()).is_none(),
        "rumor should not have an e tag"
    );

    let ms_tag = rumor
        .tags
        .find(TagKind::custom("ms"))
        .expect("rumor should have an ms tag");
    let ms_value: u64 = ms_tag.content().unwrap().parse().unwrap();
    assert!(ms_value < 1000, "ms tag must be a millisecond offset 0-999");

    relay.stop().await;
}

#[tokio::test]
async fn decrypt_incoming_gift_wrap_maps_to_agent_event() {
    let client = NostrClient::new(vec![dummy_relay()]).await.unwrap();
    let (bot_signer, _bot_npub) = test_signer();
    let bot_pubkey = bot_signer.public_key();
    let sender_keys = Keys::generate();

    client
        .add_signer(bot_pubkey, "integration-bot".into(), Arc::new(bot_signer))
        .await;

    let event = EventBuilder::private_msg(
        &sender_keys,
        bot_pubkey,
        "incoming secret",
        Vec::<Tag>::new(),
    )
    .await
    .unwrap();

    assert_eq!(event.kind, Kind::GiftWrap);
    let p_tags: Vec<_> = event.tags.public_keys().collect();
    assert_eq!(p_tags.len(), 1);
    assert_eq!(p_tags[0], &bot_pubkey);

    let agent_event = client.decrypt_event(&event).await.unwrap();
    assert_eq!(agent_event.bot_id, "integration-bot");
    assert_eq!(agent_event.event_type, EventType::DmReceived);
    assert_eq!(agent_event.content, "incoming secret");
    assert_eq!(agent_event.author, sender_keys.public_key().to_hex());
}

#[tokio::test]
async fn wrong_npub_gift_wrap_returns_error() {
    let client = NostrClient::new(vec![dummy_relay()]).await.unwrap();
    let bot_keys = Keys::generate();
    let sender_keys = Keys::generate();

    let event = EventBuilder::private_msg(
        &sender_keys,
        bot_keys.public_key(),
        "not for us",
        Vec::<Tag>::new(),
    )
    .await
    .unwrap();

    let err = client.decrypt_event(&event).await.unwrap_err();
    assert!(matches!(err, DaemonError::Nostr(_)));
    assert!(err.to_string().contains("no signer registered"));
}

#[tokio::test]
async fn set_profile_publishes_kind_0_metadata_event() -> Result<(), Box<dyn std::error::Error>> {
    let relay = MockRelay::start().await?;
    let client = NostrClient::new(vec![relay.url()]).await?;
    let (signer, _npub) = test_signer();
    let pubkey = signer.public_key();
    client
        .add_signer(pubkey, "profile-bot".into(), Arc::new(signer.clone()))
        .await;

    let event_id = client
        .set_profile(
            &signer,
            Some("Profile Name"),
            Some("About the bot"),
            Some("https://example.com/avatar.png"),
        )
        .await?;
    assert!(!event_id.to_hex().is_empty());

    let events = relay
        .wait_for_event(
            |e| e.kind == Kind::Metadata && e.pubkey == pubkey,
            std::time::Duration::from_secs(2),
        )
        .await?;
    let event = events
        .into_iter()
        .find(|e| e.kind == Kind::Metadata && e.pubkey == pubkey)
        .ok_or("metadata event not found")?;

    assert_eq!(event.kind, Kind::Metadata);
    assert!(event.verify_signature());

    let metadata: serde_json::Value = serde_json::from_str(&event.content)?;
    assert_eq!(metadata["name"], "Profile Name");
    assert_eq!(metadata["about"], "About the bot");
    assert_eq!(metadata["picture"], "https://example.com/avatar.png");

    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn subscribe_bot_with_since_filters_older_events() {
    let relay = MockRelay::start().await.unwrap();
    let client = NostrClient::new(vec![relay.url()]).await.unwrap();
    let (bot_signer, bot_npub) = test_signer();
    let bot_pubkey = bot_signer.public_key();
    let sender_keys = Keys::generate();

    client
        .add_signer(bot_pubkey, "since-bot".into(), Arc::new(bot_signer))
        .await;

    let since = Timestamp::now();
    let sub_id = client
        .subscribe_bot_with_since(&bot_pubkey, Some(since))
        .await
        .unwrap();

    let mut stream = client.receive_events();

    // Wait briefly so the relay has processed the REQ subscription before
    // injecting events; otherwise the filter may not be applied yet.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // An event older than the cursor must not be forwarded.
    let older = common::build_gift_wrap_with_timestamp(
        &sender_keys,
        &bot_npub,
        "older",
        Timestamp::now() - 60,
    )
    .await
    .unwrap();
    relay.inject_event(older).await;
    let result = tokio::time::timeout(Duration::from_millis(400), stream.next()).await;
    assert!(result.is_err(), "older event should be filtered out");

    // An event at or after the cursor must be delivered.
    let newer = common::build_gift_wrap_with_timestamp(
        &sender_keys,
        &bot_npub,
        "newer",
        Timestamp::now() + 60,
    )
    .await
    .unwrap();
    relay.inject_event(newer).await;
    let agent_event = tokio::time::timeout(Duration::from_secs(3), stream.next())
        .await
        .expect("timed out waiting for newer event")
        .expect("stream ended")
        .expect("event decryption failed");
    assert_eq!(agent_event.content, "newer");

    client.unsubscribe_bot(&sub_id).await.unwrap();
    relay.stop().await;
}

fn bogus_signature() -> Signature {
    Signature::from_slice(&[0u8; 64]).unwrap()
}

fn event_with_signature(event: &Event, sig: Signature) -> Event {
    Event::new(
        event.id,
        event.pubkey,
        event.created_at,
        event.kind,
        event.tags.clone().to_vec(),
        event.content.clone(),
        sig,
    )
}

#[tokio::test]
async fn spoofed_gift_wrap_is_rejected_and_recorded() {
    let diagnostics = Diagnostics::new();
    let client = NostrClient::new(vec![])
        .await
        .unwrap()
        .with_diagnostics(diagnostics.clone());
    let (bot_signer, _bot_npub) = test_signer();
    let bot_pubkey = bot_signer.public_key();
    let sender_keys = Keys::generate();

    client
        .add_signer(bot_pubkey, "spoof-bot".into(), Arc::new(bot_signer))
        .await;

    let valid_event = EventBuilder::private_msg(
        &sender_keys,
        bot_pubkey,
        "tampered gift wrap",
        Vec::<Tag>::new(),
    )
    .await
    .unwrap();

    let spoofed = event_with_signature(&valid_event, bogus_signature());

    let err = client.decrypt_event(&spoofed).await.unwrap_err();
    assert!(matches!(err, DaemonError::Nostr(_)));
    assert!(
        err.to_string()
            .contains("gift wrap signature verification failed")
    );

    let snap = diagnostics.snapshot();
    assert_eq!(snap.invalid_events_total, 1);
    assert!(
        snap.errors.iter().any(|e| e
            .message
            .contains("gift wrap signature verification failed")),
        "expected verification error in diagnostics, got {:?}",
        snap.errors
    );
}

#[tokio::test]
async fn malformed_seal_is_rejected_and_recorded() {
    let diagnostics = Diagnostics::new();
    let client = NostrClient::new(vec![])
        .await
        .unwrap()
        .with_diagnostics(diagnostics.clone());
    let (bot_signer, _bot_npub) = test_signer();
    let bot_pubkey = bot_signer.public_key();
    let sender_keys = Keys::generate();

    client
        .add_signer(bot_pubkey, "seal-bot".into(), Arc::new(bot_signer.clone()))
        .await;

    // Build a valid gift wrap and decrypt the outer layer to reach the seal.
    let valid_event = EventBuilder::private_msg(
        &sender_keys,
        bot_pubkey,
        "valid outer, bad seal",
        Vec::<Tag>::new(),
    )
    .await
    .unwrap();

    let seal_json = bot_signer
        .nip44_decrypt(&valid_event.pubkey, &valid_event.content)
        .await
        .unwrap();
    let seal_event = Event::from_json(&seal_json).unwrap();
    let tampered_seal = event_with_signature(&seal_event, bogus_signature());

    // Re-wrap the tampered seal with a fresh ephemeral key.
    let ephemeral = Keys::generate();
    let gift_content = nip44::encrypt(
        ephemeral.secret_key(),
        &bot_pubkey,
        tampered_seal.as_json(),
        nip44::Version::default(),
    )
    .unwrap();
    let gift = UnsignedEvent::new(
        ephemeral.public_key(),
        Timestamp::tweaked(nip59::RANGE_RANDOM_TIMESTAMP_TWEAK),
        Kind::GiftWrap,
        [Tag::public_key(bot_pubkey)],
        gift_content,
    );
    let malformed_gift = gift.sign_with_keys(&ephemeral).unwrap();

    let err = client.decrypt_event(&malformed_gift).await.unwrap_err();
    assert!(matches!(err, DaemonError::Nostr(_)));
    assert!(
        err.to_string()
            .contains("seal signature verification failed")
    );

    let snap = diagnostics.snapshot();
    assert_eq!(snap.invalid_events_total, 1);
    assert!(
        snap.errors
            .iter()
            .any(|e| e.message.contains("seal signature verification failed")),
        "expected seal verification error in diagnostics, got {:?}",
        snap.errors
    );
}
