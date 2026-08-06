#![allow(clippy::unwrap_used)]

/// req(R12, R13)
mod common;
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use nostr::nips::{nip44, nip59};
use nostr::secp256k1::schnorr::Signature;
use nostr::{
    Event, EventBuilder, EventId, Keys, Kind, PublicKey, Tag, Timestamp, ToBech32, UnsignedEvent,
};
use pacto_bot_api::diagnostics::Diagnostics;
use pacto_bot_api::errors::DaemonError;
use pacto_bot_api::events::EventType;
use pacto_bot_api::nostr::{
    GIFT_WRAP_PROCESS_TIMEOUT, MAX_CONCURRENT_GIFT_WRAP_TASKS, NostrClient,
};
use pacto_bot_api::nostr_tags;
use pacto_bot_api::signer::LocalKeyCrypto;
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

fn assert_valid_event_id(event_id: &EventId) {
    let hex = event_id.to_hex();
    assert_eq!(hex.len(), 64, "event id should be 64 hex chars");
    assert_ne!(
        hex, "0000000000000000000000000000000000000000000000000000000000000000",
        "event id should not be the zero id"
    );
}

#[tokio::test]
async fn new_adds_relays_and_connects() {
    let client = NostrClient::new(vec![dummy_relay()]).await.unwrap();
    // Adding relays again should be idempotent and skip blanks.
    client
        .add_relays(&[dummy_relay(), "".to_string()])
        .await
        .unwrap();

    let relays = client.relays().await;
    assert_eq!(
        relays.len(),
        1,
        "relay pool should contain exactly one relay"
    );
    assert_eq!(relays[0], dummy_relay());
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
    let relay = MockRelay::start().await.unwrap();
    let client = NostrClient::new(vec![relay.url()]).await.unwrap();
    let (sender, _) = test_signer();
    let recipient = Keys::generate();
    let recipient_npub = recipient.public_key().to_bech32().unwrap();

    let event_id = client
        .send_dm(&sender, &recipient_npub, "hello integration", None)
        .await
        .unwrap();
    assert_valid_event_id(&event_id);

    let events = relay
        .wait_for_event(|e| e.kind == Kind::GiftWrap, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(events.iter().any(|e| e.kind == Kind::GiftWrap));

    relay.stop().await;
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
    assert_valid_event_id(&event_id);

    let events = relay
        .wait_for_event(|e| e.kind == Kind::GiftWrap, Duration::from_secs(2))
        .await
        .unwrap();
    let gift = events
        .into_iter()
        .find(|e| e.kind == Kind::GiftWrap)
        .expect("gift wrap should be published");

    assert_eq!(gift.kind, Kind::GiftWrap);
    let p_tags: Vec<_> = gift.tags.public_keys().collect();
    assert_eq!(p_tags.len(), 1, "gift wrap should have exactly one p tag");
    assert_eq!(
        p_tags[0],
        &recipient.public_key(),
        "gift wrap should be addressed to the recipient"
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
        .expect("gift wrap should be published");

    assert_eq!(gift.kind, Kind::GiftWrap);
    let p_tags: Vec<_> = gift.tags.public_keys().collect();
    assert_eq!(p_tags.len(), 1, "gift wrap should have exactly one p tag");
    assert_eq!(p_tags[0], &recipient.public_key());

    let seal_json = LocalKeyCrypto::nip44_decrypt(&recipient, &gift.pubkey, &gift.content)
        .await
        .unwrap();
    let seal = pacto_bot_api::nostr_json::event_from_json(&seal_json).unwrap();

    let rumor_json = LocalKeyCrypto::nip44_decrypt(&recipient, &seal.pubkey, &seal.content)
        .await
        .unwrap();
    let rumor = pacto_bot_api::nostr_json::unsigned_event_from_json(&rumor_json).unwrap();

    assert_eq!(rumor.kind, Kind::PrivateDirectMessage);
    assert_eq!(rumor.content, "thread reply");

    let e_tag = nostr_tags::find_e_tag(&rumor.tags).expect("rumor should have an e tag");
    assert!(e_tag.is_reply(), "e tag should be marked as reply");
    assert_eq!(e_tag.content().unwrap(), reply_id.to_hex());

    let ms_tag =
        nostr_tags::find_custom_tag(&rumor.tags, "ms").expect("rumor should have an ms tag");
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
        .expect("gift wrap should be published");

    assert_eq!(gift.kind, Kind::GiftWrap);
    let p_tags: Vec<_> = gift.tags.public_keys().collect();
    assert_eq!(p_tags.len(), 1, "gift wrap should have exactly one p tag");
    assert_eq!(p_tags[0], &recipient.public_key());

    let seal_json = LocalKeyCrypto::nip44_decrypt(&recipient, &gift.pubkey, &gift.content)
        .await
        .unwrap();
    let seal = pacto_bot_api::nostr_json::event_from_json(&seal_json).unwrap();

    let rumor_json = LocalKeyCrypto::nip44_decrypt(&recipient, &seal.pubkey, &seal.content)
        .await
        .unwrap();
    let rumor = pacto_bot_api::nostr_json::unsigned_event_from_json(&rumor_json).unwrap();

    assert_eq!(rumor.kind, Kind::PrivateDirectMessage);
    assert!(
        nostr_tags::find_e_tag(&rumor.tags).is_none(),
        "rumor should not have an e tag"
    );

    let ms_tag =
        nostr_tags::find_custom_tag(&rumor.tags, "ms").expect("rumor should have an ms tag");
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
    let expected = format!("no signer registered for {}", bot_keys.public_key());
    let msg = match err {
        DaemonError::Nostr(msg) => msg,
        other => panic!("expected DaemonError::Nostr, got {other:?}"),
    };
    assert_eq!(msg, expected);
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
    assert_valid_event_id(&event_id);

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

    assert_eq!(
        event.id, event_id,
        "published event id should match returned id"
    );
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
    tokio::time::sleep(Duration::from_millis(50)).await;

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

async fn build_key_package(
    keys: &Keys,
    content: &str,
    created_at: Timestamp,
) -> Result<Event, Box<dyn std::error::Error>> {
    let unsigned = UnsignedEvent::new(
        keys.public_key(),
        created_at,
        Kind::MlsKeyPackage,
        Vec::new(),
        content.to_string(),
    );
    Ok(unsigned.sign(keys).await?)
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
    let expected = format!(
        "gift wrap signature verification failed: {}",
        spoofed.verify().expect_err("bogus signature should fail")
    );
    let msg = match err {
        DaemonError::Nostr(msg) => msg,
        other => panic!("expected DaemonError::Nostr, got {other:?}"),
    };
    assert_eq!(msg, expected);

    let snap = diagnostics.snapshot().await;
    assert_eq!(snap.invalid_events_total, 1);
    assert_eq!(snap.gift_wrap_rejected_total, 1);
    // Gift-wrap senders are unauthenticated and attacker-mintable, so this
    // rejection is counted in the aggregated `gift_wrap_rejected_total`
    // rather than recorded per event in `errors`, which a spray of
    // malformed wraps could otherwise use to evict genuine entries from
    // the fixed-size diagnostics ring (R33, R42).
    assert!(snap.errors.is_empty());
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
    let seal_event = pacto_bot_api::nostr_json::event_from_json(&seal_json).unwrap();
    let tampered_seal = event_with_signature(&seal_event, bogus_signature());

    // Re-wrap the tampered seal with a fresh ephemeral key.
    let ephemeral = Keys::generate();
    let gift_content = nip44::encrypt(
        ephemeral.secret_key(),
        &bot_pubkey,
        pacto_bot_api::nostr_json::event_to_json(&tampered_seal),
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
    let malformed_gift = pacto_bot_api::nostr_json::sign_unsigned(gift, &ephemeral).unwrap();

    let err = client.decrypt_event(&malformed_gift).await.unwrap_err();
    let expected = format!(
        "seal signature verification failed: {}",
        tampered_seal
            .verify()
            .expect_err("bogus seal signature should fail")
    );
    let msg = match err {
        DaemonError::Nostr(msg) => msg,
        other => panic!("expected DaemonError::Nostr, got {other:?}"),
    };
    assert_eq!(msg, expected);

    let snap = diagnostics.snapshot().await;
    assert_eq!(snap.invalid_events_total, 1);
    assert_eq!(snap.gift_wrap_rejected_total, 1);
    assert!(
        snap.errors.is_empty(),
        "unauthenticated gift-wrap rejections must not evict the diagnostics ring"
    );
}

#[tokio::test]
async fn fetch_key_package_selects_fresh_over_stale() -> Result<(), Box<dyn std::error::Error>> {
    let relay = MockRelay::start().await?;
    let client = NostrClient::new(vec![relay.url()]).await?;

    let recipient_keys = Keys::generate();
    let recipient = recipient_keys.public_key();
    let stale_marker = "STALE_KP_CIPHERTEXT_abc123";
    let fresh_marker = "FRESH_KP_CIPHERTEXT_xyz789";

    // Inject a stale package first, then a fresh one. A relay that respects
    // Filter::limit would return only the stale package if the filter had a
    // limit of 1; the client must collect all events and select the fresh one.
    let stale_ts = Timestamp::from_secs(Timestamp::now().as_secs() - 3600);
    let stale_event = build_key_package(&recipient_keys, stale_marker, stale_ts).await?;
    relay.inject_event(stale_event).await;

    let fresh_event = build_key_package(&recipient_keys, fresh_marker, Timestamp::now()).await?;
    let fresh_id = fresh_event.id;
    relay.inject_event(fresh_event).await;

    let (fetched, age) = client
        .fetch_key_package(&recipient, Duration::from_secs(5), Duration::from_secs(300))
        .await?;

    assert_eq!(fetched.kind, Kind::MlsKeyPackage);
    assert_eq!(fetched.pubkey, recipient);
    assert_eq!(
        fetched.id, fresh_id,
        "fetched event should be the fresh one, not the stale one"
    );
    assert!(
        fetched.content.contains(fresh_marker),
        "fetched package should contain the fresh content"
    );
    assert!(
        !fetched.content.contains(stale_marker),
        "fetched package should not contain the stale content"
    );
    assert!(age <= Duration::from_secs(5));
    Ok(())
}

#[tokio::test]
async fn rumor_author_mismatch_gift_wrap_is_rejected() {
    let diagnostics = Diagnostics::new();
    let client = NostrClient::new(vec![])
        .await
        .unwrap()
        .with_diagnostics(diagnostics.clone());
    let (bot_signer, _bot_npub) = test_signer();
    let bot_pubkey = bot_signer.public_key();
    let sender_keys = Keys::generate();
    let impostor_keys = Keys::generate();

    client
        .add_signer(bot_pubkey, "mismatch-bot".into(), Arc::new(bot_signer))
        .await;

    // The seal is signed by `sender_keys`, but the rumor it encrypts
    // declares `impostor_keys` as its author — a mismatch the daemon must
    // reject rather than attribute to the seal's signer (R30).
    let mut rumor = UnsignedEvent::new(
        impostor_keys.public_key(),
        Timestamp::now(),
        Kind::PrivateDirectMessage,
        Vec::new(),
        "spoofed author".to_string(),
    );
    rumor.ensure_id();
    let seal_content = sender_keys
        .nip44_encrypt(
            &bot_pubkey,
            &pacto_bot_api::nostr_json::unsigned_event_to_json(&rumor),
        )
        .await
        .unwrap();
    let seal = pacto_bot_api::nostr_json::sign_builder(
        EventBuilder::new(Kind::Seal, seal_content)
            .custom_created_at(Timestamp::tweaked(nip59::RANGE_RANDOM_TIMESTAMP_TWEAK)),
        &sender_keys,
    )
    .unwrap();

    let ephemeral = Keys::generate();
    let gift_content = nip44::encrypt(
        ephemeral.secret_key(),
        &bot_pubkey,
        pacto_bot_api::nostr_json::event_to_json(&seal),
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
    let mismatched_gift = pacto_bot_api::nostr_json::sign_unsigned(gift, &ephemeral).unwrap();

    let err = client.decrypt_event(&mismatched_gift).await.unwrap_err();
    let msg = match err {
        DaemonError::Nostr(msg) => msg,
        other => panic!("expected DaemonError::Nostr, got {other:?}"),
    };
    assert_eq!(msg, "rumor author does not match seal author");

    let snap = diagnostics.snapshot().await;
    assert_eq!(snap.gift_wrap_rejected_total, 1);
    assert!(
        snap.errors.is_empty(),
        "unauthenticated gift-wrap rejections must not evict the diagnostics ring"
    );
}

#[tokio::test]
async fn truncated_gift_wrap_payload_does_not_stall_intake() {
    let relay = MockRelay::start().await.unwrap();
    let client = NostrClient::new(vec![relay.url()]).await.unwrap();
    let (bot_signer, bot_npub) = test_signer();
    let bot_pubkey = bot_signer.public_key();
    let sender_keys = Keys::generate();

    client
        .add_signer(bot_pubkey, "truncated-bot".into(), Arc::new(bot_signer))
        .await;

    client.subscribe_bot(&bot_pubkey).await.unwrap();
    let mut stream = client.receive_events();
    // Wait briefly so the relay has processed the REQ subscription before
    // injecting events; otherwise the filter may not be applied yet.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // A validly signed gift wrap whose content is a truncated NIP-44 v2
    // ciphertext: the signature covers this exact (truncated) content, so
    // it passes `event.verify()`, but the payload is too short to decrypt.
    let ephemeral = Keys::generate();
    let full_ciphertext = nip44::encrypt(
        ephemeral.secret_key(),
        &bot_pubkey,
        "irrelevant seal payload",
        nip44::Version::default(),
    )
    .unwrap();
    let truncated = full_ciphertext[..full_ciphertext.len() / 2].to_string();
    let malformed_gift = pacto_bot_api::nostr_json::sign_builder(
        EventBuilder::new(Kind::GiftWrap, truncated)
            .tags([Tag::public_key(bot_pubkey)])
            .custom_created_at(Timestamp::tweaked(nip59::RANGE_RANDOM_TIMESTAMP_TWEAK)),
        &ephemeral,
    )
    .unwrap();
    relay.inject_event(malformed_gift).await;

    let first = tokio::time::timeout(Duration::from_secs(3), stream.next())
        .await
        .expect("timed out waiting for the truncated event's outcome")
        .expect("stream ended");
    assert!(
        first.is_err(),
        "a truncated payload should surface as a decrypt error, not an AgentEvent"
    );

    // The next well-formed event in the same subscription must still be
    // processed normally.
    let good = common::build_gift_wrap(&sender_keys, &bot_npub, "still works")
        .await
        .unwrap();
    relay.inject_event(good).await;
    let agent_event = tokio::time::timeout(Duration::from_secs(3), stream.next())
        .await
        .expect("timed out waiting for the well-formed event")
        .expect("stream ended")
        .expect("well-formed event should decrypt");
    assert_eq!(agent_event.content, "still works");

    relay.stop().await;
}

/// Wraps a [`LocalKey`], recording how many `nip44_decrypt` calls are
/// concurrently in flight and holding each one open briefly so overlapping
/// gift-wrap tasks are observable from the test.
struct ConcurrencyTrackingSigner {
    inner: LocalKey,
    live: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    hold: Duration,
}

#[async_trait]
impl Signer for ConcurrencyTrackingSigner {
    fn public_key(&self) -> PublicKey {
        self.inner.public_key()
    }

    async fn sign_event(&self, payload: &[u8]) -> Result<String, DaemonError> {
        self.inner.sign_event(payload).await
    }

    async fn nip44_encrypt(
        &self,
        public_key: &PublicKey,
        content: &str,
    ) -> Result<String, DaemonError> {
        self.inner.nip44_encrypt(public_key, content).await
    }

    async fn nip44_decrypt(
        &self,
        public_key: &PublicKey,
        payload: &str,
    ) -> Result<String, DaemonError> {
        let now = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(self.hold).await;
        let result = self.inner.nip44_decrypt(public_key, payload).await;
        self.live.fetch_sub(1, Ordering::SeqCst);
        result
    }
}

#[tokio::test]
async fn gift_wrap_burst_stays_within_concurrency_bound() {
    let relay = MockRelay::start().await.unwrap();
    let client = NostrClient::new(vec![relay.url()]).await.unwrap();
    let (bot_signer, bot_npub) = test_signer();
    let bot_pubkey = bot_signer.public_key();

    let live = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let tracked = ConcurrencyTrackingSigner {
        inner: bot_signer,
        live: live.clone(),
        peak: peak.clone(),
        hold: Duration::from_millis(150),
    };
    client
        .add_signer(bot_pubkey, "burst-bot".into(), Arc::new(tracked))
        .await;

    client.subscribe_bot(&bot_pubkey).await.unwrap();
    let mut stream = client.receive_events();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let burst = 2 * MAX_CONCURRENT_GIFT_WRAP_TASKS;
    for i in 0..burst {
        let sender = Keys::generate();
        let gift = common::build_gift_wrap(&sender, &bot_npub, &format!("burst {i}"))
            .await
            .unwrap();
        relay.inject_event(gift).await;
    }

    let mut received = 0;
    while received < burst {
        let item = tokio::time::timeout(Duration::from_secs(20), stream.next())
            .await
            .expect("timed out waiting for burst events")
            .expect("stream ended");
        assert!(item.is_ok(), "burst events should all decrypt successfully");
        received += 1;
    }

    let observed_peak = peak.load(Ordering::SeqCst);
    assert!(
        observed_peak <= MAX_CONCURRENT_GIFT_WRAP_TASKS,
        "observed peak concurrency {observed_peak} exceeded the semaphore bound of {MAX_CONCURRENT_GIFT_WRAP_TASKS}"
    );
    assert!(observed_peak > 1, "no meaningful concurrency was observed");

    relay.stop().await;
}

/// Wraps a [`LocalKey`], adding an artificial delay to `nip44_decrypt` calls
/// whose payload matches one specific ciphertext, so a test can make exactly
/// one event's processing exceed [`GIFT_WRAP_PROCESS_TIMEOUT`] without
/// affecting any other event handled by the same signer.
struct DelayedSigner {
    inner: LocalKey,
    stuck_payload: String,
    delay: Duration,
}

#[async_trait]
impl Signer for DelayedSigner {
    fn public_key(&self) -> PublicKey {
        self.inner.public_key()
    }

    async fn sign_event(&self, payload: &[u8]) -> Result<String, DaemonError> {
        self.inner.sign_event(payload).await
    }

    async fn nip44_encrypt(
        &self,
        public_key: &PublicKey,
        content: &str,
    ) -> Result<String, DaemonError> {
        self.inner.nip44_encrypt(public_key, content).await
    }

    async fn nip44_decrypt(
        &self,
        public_key: &PublicKey,
        payload: &str,
    ) -> Result<String, DaemonError> {
        if payload == self.stuck_payload {
            tokio::time::sleep(self.delay).await;
        }
        self.inner.nip44_decrypt(public_key, payload).await
    }
}

#[tokio::test]
async fn gift_wrap_processing_timeout_does_not_stall_intake() {
    let relay = MockRelay::start().await.unwrap();
    let client = NostrClient::new(vec![relay.url()]).await.unwrap();
    let (bot_signer, bot_npub) = test_signer();
    let bot_pubkey = bot_signer.public_key();
    let sender_keys = Keys::generate();

    let stuck_event = common::build_gift_wrap(&sender_keys, &bot_npub, "stuck")
        .await
        .unwrap();

    let delayed = DelayedSigner {
        inner: bot_signer,
        stuck_payload: stuck_event.content.clone(),
        delay: GIFT_WRAP_PROCESS_TIMEOUT + Duration::from_secs(3),
    };
    client
        .add_signer(bot_pubkey, "slow-bot".into(), Arc::new(delayed))
        .await;

    client.subscribe_bot(&bot_pubkey).await.unwrap();
    let mut stream = client.receive_events();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let start = std::time::Instant::now();
    relay.inject_event(stuck_event).await;

    let follow_up = common::build_gift_wrap(&sender_keys, &bot_npub, "unstuck")
        .await
        .unwrap();
    relay.inject_event(follow_up).await;

    // The follow-up must be processed promptly: the intake loop does not
    // wait for the stuck event's `process_gift_wrap` call to finish or
    // time out before moving on to the next notification.
    let agent_event = tokio::time::timeout(Duration::from_secs(3), stream.next())
        .await
        .expect("timed out waiting for the follow-up event; the intake loop stalled")
        .expect("stream ended")
        .expect("follow-up event should decrypt");
    assert_eq!(agent_event.content, "unstuck");

    // The stuck event is abandoned once GIFT_WRAP_PROCESS_TIMEOUT elapses
    // and must never surface on the stream.
    let remaining =
        (GIFT_WRAP_PROCESS_TIMEOUT + Duration::from_secs(1)).saturating_sub(start.elapsed());
    let no_event = tokio::time::timeout(remaining, stream.next()).await;
    assert!(
        no_event.is_err(),
        "a timed-out event must not surface on the stream"
    );

    relay.stop().await;
}

#[tokio::test]
async fn malformed_gift_wrap_spray_does_not_evict_genuine_diagnostics() {
    let diagnostics = Diagnostics::new();
    let client = NostrClient::new(vec![])
        .await
        .unwrap()
        .with_diagnostics(diagnostics.clone());
    let (bot_signer, _bot_npub) = test_signer();
    let bot_pubkey = bot_signer.public_key();

    client
        .add_signer(bot_pubkey, "spray-bot".into(), Arc::new(bot_signer))
        .await;

    // A genuine diagnostic entry recorded before the spray begins; it must
    // still be present afterward.
    diagnostics
        .record_error(Some("genuine_bug"), "a real operator-visible failure", None)
        .await;

    for _ in 0..50 {
        let sender = Keys::generate();
        let valid_event =
            EventBuilder::private_msg(&sender, bot_pubkey, "malformed spray", Vec::<Tag>::new())
                .await
                .unwrap();
        let spoofed = event_with_signature(&valid_event, bogus_signature());
        let _ = client.decrypt_event(&spoofed).await;
    }

    let snap = diagnostics.snapshot().await;
    assert_eq!(snap.gift_wrap_rejected_total, 50);
    assert_eq!(
        snap.errors.len(),
        1,
        "a spray of malformed gift wraps must not evict the genuine entry"
    );
    assert_eq!(snap.errors[0].code, "genuine_bug");
    assert_eq!(snap.errors[0].message, "a real operator-visible failure");
}
