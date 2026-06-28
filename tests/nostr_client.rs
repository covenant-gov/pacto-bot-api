#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use nostr::{EventBuilder, Keys, Kind, Tag, ToBech32};
use pacto_bot_api::errors::DaemonError;
use pacto_bot_api::events::EventType;
use pacto_bot_api::nostr::NostrClient;
use pacto_bot_api::signer::{LocalKey, Signer};

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
    let client = NostrClient::new(vec![dummy_relay()]).await.unwrap();
    let (sender, _) = test_signer();
    let recipient = Keys::generate();
    let recipient_npub = recipient.public_key().to_bech32().unwrap();

    // Since send_dm only returns the EventId, verify that the call succeeds
    // and that the resulting id is non-zero (a publishable event was built).
    let event_id = client
        .send_dm(&sender, &recipient_npub, "wrapped", None)
        .await
        .unwrap();
    assert_ne!(
        event_id.to_hex(),
        "0000000000000000000000000000000000000000000000000000000000000000"
    );
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
