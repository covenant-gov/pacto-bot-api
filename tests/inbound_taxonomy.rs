//! Tests for U4: inbound rumor-kind taxonomy on the DM surface.
//!
//! Covers R1, R2, R3, R4, R5, R7 from
//! `docs/plans/2026-08-03-001-feat-reactions-attachments-parity-plan.md`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
mod support;

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use nostr::nips::{nip44, nip59};
use nostr::{Event, EventBuilder, Keys, Kind, PublicKey, Tag, Timestamp, ToBech32, UnsignedEvent};
use pacto_bot_api::client_manager::ClientManager;
use pacto_bot_api::config::{BotConfig, DaemonConfig, GlobalDaemonConfig, SigningConfig};
use pacto_bot_api::db::Db;
use pacto_bot_api::diagnostics::Diagnostics;
use pacto_bot_api::dispatch::Dispatch;
use pacto_bot_api::errors::DaemonError;
use pacto_bot_api::events::{AgentEvent, EventType};
use pacto_bot_api::handlers::ConnectionHandle;
use pacto_bot_api::mls::MlsEngineHandle;
use pacto_bot_api::nostr::NostrClient;
use pacto_bot_api::nostr_tags;
use pacto_bot_api::signer::{Signer, SignerBackend};
use pacto_bot_api::transport::protocol::JsonRpcMessage;
use secrecy::SecretString;
use support::mock_mls_peer::{MockMlsPeer, gift_wrap_welcome};
use support::mock_relay::MockRelay;
use tokio::sync::{RwLock, mpsc};
use tokio::time::timeout;

const BOT_ID: &str = "taxonomy-bot";

// ---------------------------------------------------------------------------
// Shared harness (mirrors tests/mls_inbound.rs and tests/mls_welcome_dispatch.rs)
// ---------------------------------------------------------------------------

fn bot_config(id: &str, keys: &Keys, capabilities: &[&str]) -> BotConfig {
    BotConfig {
        id: id.to_string(),
        npub: keys.public_key().to_bech32().unwrap(),
        signing: SigningConfig::Nsec {
            nsec: SecretString::new(keys.secret_key().to_bech32().unwrap().into()),
        },
        relays: vec![],
        capabilities: capabilities.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

fn test_signer() -> (SignerBackend, String) {
    let keys = Keys::generate();
    let npub = keys.public_key().to_bech32().unwrap();
    let config = SigningConfig::Nsec {
        nsec: SecretString::new(keys.secret_key().to_bech32().unwrap().into()),
    };
    let signer = SignerBackend::from_config(&config, &npub).expect("build signer backend");
    (signer, npub)
}

/// Set up a `Dispatch` + `NostrClient` pair backed by a mock relay, with the
/// bot's signer registered so gift wraps addressed to it decrypt. Used for
/// scenarios that need to observe handler fan-out or cursor persistence.
async fn setup_dispatch(
    capabilities: &[&str],
    dispatch_timeout: Duration,
) -> Result<
    (
        Keys,
        Arc<Dispatch>,
        NostrClient,
        MockRelay,
        tempfile::TempDir,
    ),
    Box<dyn std::error::Error>,
> {
    let keys = Keys::generate();
    let bot = bot_config(BOT_ID, &keys, capabilities);
    let config = DaemonConfig {
        daemon: GlobalDaemonConfig::default(),
        bots: vec![bot],
    };
    let dir = common::tempdir()?;
    let relay = MockRelay::start().await?;
    let nostr_client = NostrClient::new(vec![relay.url()]).await?;
    let db = Db::open(dir.path().join("test.db").as_path()).await?;
    let cm = Arc::new(RwLock::new(
        ClientManager::new(dir.path(), config, nostr_client, &db).await?,
    ));
    let mut dispatch = Dispatch::new(cm.clone(), db.clone(), Diagnostics::new());
    dispatch.set_dispatch_timeout(dispatch_timeout);
    let dispatch = Arc::new(dispatch);

    {
        let cm_guard = cm.read().await;
        let bot = cm_guard.get_bot_by_id(BOT_ID).expect("bot exists");
        let signer = bot.signer.clone();
        let pubkey = bot.signer.public_key();
        cm_guard
            .nostr_client
            .add_signer(pubkey, BOT_ID.to_string(), Arc::new(signer))
            .await;
    }
    {
        let mut cm_guard = cm.write().await;
        cm_guard.subscribe_bots(&db).await?;
    }

    relay.wait_for_subscription(Duration::from_secs(5)).await?;

    let client = cm.read().await.nostr_client.clone();
    Ok((keys, dispatch, client, relay, dir))
}

async fn register_handler(
    dispatch: &Dispatch,
    event_types: &[&str],
    capabilities: &[&str],
) -> Result<(String, mpsc::Receiver<JsonRpcMessage>), Box<dyn std::error::Error>> {
    let (tx, rx) = mpsc::channel(16);
    let connection = ConnectionHandle::new(tx);
    let response = dispatch
        .handle_message(
            JsonRpcMessage::request(
                1.into(),
                "handler.register",
                Some(serde_json::json!({
                    "bot_ids": [BOT_ID],
                    "event_types": event_types,
                    "capabilities": capabilities,
                })),
            ),
            None,
            Some(connection),
        )
        .await?;
    let handler_id = response
        .and_then(|r| match r {
            JsonRpcMessage::Response { result, .. } => result,
            _ => None,
        })
        .and_then(|v| {
            v.get("handler_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .ok_or("handler.register did not return handler_id")?;
    Ok((handler_id, rx))
}

fn parse_agent_event(msg: &JsonRpcMessage) -> Option<AgentEvent> {
    match msg {
        JsonRpcMessage::Notification { method, params, .. } if method == "agent.event" => params
            .as_ref()
            .and_then(|p| serde_json::from_value(p.clone()).ok()),
        _ => None,
    }
}

async fn consume_stream(
    dispatch: Arc<Dispatch>,
    mut stream: impl StreamExt<Item = Result<AgentEvent, DaemonError>> + Send + Unpin + 'static,
) {
    while let Some(event_result) = stream.next().await {
        match event_result {
            Ok(event) => {
                if let Err(e) = dispatch.dispatch_event(event).await {
                    eprintln!("dispatch error: {e}");
                }
            }
            Err(e) => eprintln!("event error: {e}"),
        }
    }
}

async fn next_message(rx: &mut mpsc::Receiver<JsonRpcMessage>) -> Option<JsonRpcMessage> {
    timeout(Duration::from_secs(5), rx.recv()).await.ok()?
}

/// Gift-wrap an arbitrary rumor (any kind) from `sender` to `recipient`,
/// mirroring `tests/common::build_gift_wrap_with_timestamp` but for a
/// caller-supplied rumor instead of a hardcoded kind:14 private message.
async fn gift_wrap_rumor(
    sender: &Keys,
    recipient: PublicKey,
    rumor: UnsignedEvent,
) -> Result<Event, Box<dyn std::error::Error>> {
    let seal = EventBuilder::seal(sender, &recipient, rumor)
        .await?
        .sign(sender)
        .await?;

    let ephemeral = Keys::generate();
    let gift_content = nip44::encrypt(
        ephemeral.secret_key(),
        &recipient,
        pacto_bot_api::nostr_json::event_to_json(&seal),
        nip44::Version::default(),
    )?;
    let gift = UnsignedEvent::new(
        ephemeral.public_key(),
        Timestamp::tweaked(nip59::RANGE_RANDOM_TIMESTAMP_TWEAK),
        Kind::GiftWrap,
        [Tag::public_key(recipient)],
        gift_content,
    );
    Ok(pacto_bot_api::nostr_json::sign_unsigned(gift, &ephemeral)?)
}

// ---------------------------------------------------------------------------
// Scenario 1: a gift-wrapped kind:7 reaches a `reaction_received` subscriber
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reaction_reaches_subscribed_handler() -> Result<(), Box<dyn std::error::Error>> {
    let (keys, dispatch, client, relay, _dir) =
        setup_dispatch(&["ReadMessages"], Duration::from_millis(300)).await?;
    let (_, mut rx) =
        register_handler(&dispatch, &["reaction_received"], &["ReadMessages"]).await?;

    let sender_keys = Keys::generate();
    let target_keys = Keys::generate();
    let target = EventBuilder::new(Kind::PrivateDirectMessage, "original message")
        .build(target_keys.public_key());
    let target_id = target.id.ok_or("target rumor missing id")?;

    let rumor = nostr_tags::reaction_event(
        target_id,
        target_keys.public_key(),
        Some(Kind::PrivateDirectMessage),
        "🔥",
    )
    .build(sender_keys.public_key());
    let gift_wrap = gift_wrap_rumor(&sender_keys, keys.public_key(), rumor).await?;
    relay.inject_event(gift_wrap).await;

    let stream = client.receive_events();
    let consumer = tokio::spawn(consume_stream(dispatch, stream));

    let msg = next_message(&mut rx)
        .await
        .ok_or("no agent.event notification")?;
    let event = parse_agent_event(&msg).ok_or("not an agent.event")?;
    assert_eq!(event.event_type, EventType::ReactionReceived);
    let reaction = event.reaction.ok_or("reaction payload missing")?;
    assert_eq!(reaction.target_rumor_id, target_id.to_hex());
    assert_eq!(reaction.emoji, "🔥");

    consumer.abort();
    let _ = consumer.await;
    relay.stop().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Scenario 2: a dm_received-only subscriber gets nothing for that kind:7
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dm_only_handler_receives_nothing_for_reaction() -> Result<(), Box<dyn std::error::Error>> {
    let (keys, dispatch, client, relay, _dir) =
        setup_dispatch(&["ReadMessages"], Duration::from_millis(300)).await?;
    let (_, mut rx) = register_handler(&dispatch, &["dm_received"], &["ReadMessages"]).await?;

    let sender_keys = Keys::generate();
    let target_keys = Keys::generate();
    let target = EventBuilder::new(Kind::PrivateDirectMessage, "original message")
        .build(target_keys.public_key());
    let target_id = target.id.ok_or("target rumor missing id")?;
    let rumor = nostr_tags::reaction_event(
        target_id,
        target_keys.public_key(),
        Some(Kind::PrivateDirectMessage),
        "👍",
    )
    .build(sender_keys.public_key());
    let gift_wrap = gift_wrap_rumor(&sender_keys, keys.public_key(), rumor).await?;
    relay.inject_event(gift_wrap).await;

    let stream = client.receive_events();
    let consumer = tokio::spawn(consume_stream(dispatch, stream));

    let result = timeout(Duration::from_secs(2), rx.recv()).await;
    assert!(
        result.ok().flatten().is_none(),
        "a handler subscribed only to dm_received must not receive a reaction event, \
         and in particular not a text event whose body is the bare emoji"
    );

    consumer.abort();
    let _ = consumer.await;
    relay.stop().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Scenario 3: kind:14 still delivers as dm_received, fields unchanged
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dm_kind_still_delivers_with_unchanged_fields() -> Result<(), Box<dyn std::error::Error>> {
    let client = NostrClient::new(vec![]).await?;
    let (bot_signer, _bot_npub) = test_signer();
    let bot_pubkey = bot_signer.public_key();
    let sender_keys = Keys::generate();
    client
        .add_signer(bot_pubkey, BOT_ID.into(), Arc::new(bot_signer))
        .await;

    let gift_wrap =
        EventBuilder::private_msg(&sender_keys, bot_pubkey, "still a dm", Vec::<Tag>::new())
            .await
            .unwrap();
    let agent_event = client.decrypt_event(&gift_wrap).await?;

    assert_eq!(agent_event.bot_id, BOT_ID);
    assert_eq!(agent_event.event_type, EventType::DmReceived);
    assert_eq!(agent_event.content, "still a dm");
    assert_eq!(agent_event.author, sender_keys.public_key().to_hex());
    assert!(agent_event.chat_id.is_none());
    assert!(agent_event.reaction.is_none());
    Ok(())
}

// ---------------------------------------------------------------------------
// Scenario 4: kind:443/444 welcome still delivers as mls_welcome_received
// ---------------------------------------------------------------------------

async fn setup_nostr_client_with_mls_bot() -> Result<
    (
        Keys,
        NostrClient,
        MlsEngineHandle,
        MockRelay,
        tempfile::TempDir,
    ),
    Box<dyn std::error::Error>,
> {
    let keys = Keys::generate();
    let bot = BotConfig {
        id: "taxonomy-welcome-bot".to_string(),
        npub: keys.public_key().to_bech32()?,
        signing: SigningConfig::Nsec {
            nsec: SecretString::new(keys.secret_key().to_bech32()?.into()),
        },
        relays: vec![],
        capabilities: vec!["ReceiveGroupMessages".into(), "SendGroupMessages".into()],
        mls_dedup_window_secs: None,
        mls_db_path: Some(std::path::PathBuf::from("vector-mls.db")),
        mls_key_package_freshness_secs: None,
        ..Default::default()
    };
    let config = DaemonConfig {
        daemon: GlobalDaemonConfig::default(),
        bots: vec![bot],
    };
    let dir = common::tempdir()?;
    let relay = MockRelay::start().await?;
    let nostr_client = NostrClient::new(vec![relay.url()]).await?;
    let db = Db::open(dir.path().join("test.db").as_path()).await?;
    let mut cm = ClientManager::new(dir.path(), config, nostr_client.clone(), &db).await?;
    cm.subscribe_bots(&db).await?;

    let bot = cm
        .get_bot_by_id("taxonomy-welcome-bot")
        .expect("bot exists");
    let signer = bot.signer.clone();
    let mls = bot.mls.clone().expect("mls engine configured");
    let pubkey = bot.signer.public_key();
    nostr_client
        .add_signer(pubkey, "taxonomy-welcome-bot".to_string(), Arc::new(signer))
        .await;
    nostr_client
        .add_mls_engine(pubkey, "taxonomy-welcome-bot".to_string(), mls.clone())
        .await;

    relay.wait_for_subscription(Duration::from_secs(5)).await?;

    Ok((keys, nostr_client, mls, relay, dir))
}

#[tokio::test]
async fn welcome_kind_still_delivers_as_mls_welcome_received()
-> Result<(), Box<dyn std::error::Error>> {
    let (keys, client, mls, _relay, _dir) = setup_nostr_client_with_mls_bot().await?;
    let bot_pubkey = keys.public_key();

    let key_package = mls
        .publish_key_package(&bot_pubkey, vec![])
        .await
        .expect("publish key package");
    let unsigned_kp = UnsignedEvent::new(
        bot_pubkey,
        Timestamp::now(),
        Kind::MlsKeyPackage,
        key_package.1,
        key_package.0,
    );
    let key_package_event = unsigned_kp.sign(&keys).await?;

    let peer = MockMlsPeer::new();
    let (_group_result, welcome_rumor) = peer.create_group_with(&key_package_event);
    let gift_wrap = gift_wrap_welcome(&peer.keys, &bot_pubkey, welcome_rumor).await;

    let agent_event = client.decrypt_event(&gift_wrap).await?;
    assert_eq!(agent_event.event_type, EventType::MlsWelcomeReceived);
    assert!(agent_event.chat_id.is_some());
    assert!(agent_event.reaction.is_none());
    Ok(())
}

// ---------------------------------------------------------------------------
// Scenario 5: an unrepresented kind delivers no event; cursor still advances
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unrepresented_kind_is_skipped_and_cursor_still_advances()
-> Result<(), Box<dyn std::error::Error>> {
    let dispatch_timeout = Duration::from_millis(300);
    let (keys, dispatch, client, relay, _dir) =
        setup_dispatch(&["ReadMessages"], dispatch_timeout).await?;
    let (_, mut rx) = register_handler(&dispatch, &["dm_received"], &["ReadMessages"]).await?;

    let sender_keys = Keys::generate();
    let bot_pubkey = keys.public_key();

    // Kind 1 (plain text note) is not represented in the taxonomy.
    let unrepresented_rumor =
        EventBuilder::new(Kind::TextNote, "just a note").build(sender_keys.public_key());
    let unrepresented_gift = gift_wrap_rumor(&sender_keys, bot_pubkey, unrepresented_rumor).await?;
    relay.inject_event(unrepresented_gift).await;

    let dm_rumor = EventBuilder::private_msg_rumor(bot_pubkey, "after the skip")
        .build(sender_keys.public_key());
    let dm_timestamp = dm_rumor.created_at.as_u64();
    let dm_gift = gift_wrap_rumor(&sender_keys, bot_pubkey, dm_rumor).await?;
    relay.inject_event(dm_gift).await;

    let stream = client.receive_events();
    let consumer = tokio::spawn(consume_stream(dispatch.clone(), stream));

    // Only the DM should ever surface; the unrepresented kind delivers
    // nothing, and processing is not stalled by it.
    let msg = next_message(&mut rx)
        .await
        .ok_or("no agent.event notification")?;
    let event = parse_agent_event(&msg).ok_or("not an agent.event")?;
    assert_eq!(event.event_type, EventType::DmReceived);
    assert_eq!(event.content, "after the skip");

    let extra = timeout(Duration::from_millis(300), rx.recv()).await;
    assert!(
        extra.ok().flatten().is_none(),
        "the unrepresented kind must not deliver a second event"
    );

    // R7: cursor advancement is not stalled by the skip. Give dispatch_event
    // time to finish waiting out its (shortened) handler-response window and
    // persist the cursor, then confirm it reflects the dispatched DM's
    // timestamp — i.e. processing moved cleanly past the skipped rumor.
    tokio::time::sleep(dispatch_timeout + Duration::from_millis(200)).await;
    let cursor = dispatch
        .load_cursor(BOT_ID)
        .await?
        .map(|(_, cursor)| cursor)
        .ok_or("cursor should be persisted after dispatching the dm")?;
    assert_eq!(cursor, dm_timestamp as i64);

    consumer.abort();
    let _ = consumer.await;
    relay.stop().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Scenarios 6 & 7: an invalid kind:7 rumor delivers nothing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reaction_without_e_tag_delivers_nothing_and_counts_invalid()
-> Result<(), Box<dyn std::error::Error>> {
    let diagnostics = Diagnostics::new();
    let client = NostrClient::new(vec![])
        .await?
        .with_diagnostics(diagnostics.clone());
    let (bot_signer, _bot_npub) = test_signer();
    let bot_pubkey = bot_signer.public_key();
    let sender_keys = Keys::generate();
    client
        .add_signer(bot_pubkey, BOT_ID.into(), Arc::new(bot_signer))
        .await;

    // A reaction rumor with content but no target `e` tag at all.
    let rumor = EventBuilder::new(Kind::Reaction, "👍").build(sender_keys.public_key());
    let gift_wrap = gift_wrap_rumor(&sender_keys, bot_pubkey, rumor).await?;

    let err = client.decrypt_event(&gift_wrap).await.unwrap_err();
    assert!(matches!(err, DaemonError::Nostr(_)));

    let snap = diagnostics.snapshot().await;
    assert_eq!(snap.invalid_events_total, 1);
    Ok(())
}

#[tokio::test]
async fn reaction_with_empty_content_delivers_nothing() -> Result<(), Box<dyn std::error::Error>> {
    let diagnostics = Diagnostics::new();
    let client = NostrClient::new(vec![])
        .await?
        .with_diagnostics(diagnostics.clone());
    let (bot_signer, _bot_npub) = test_signer();
    let bot_pubkey = bot_signer.public_key();
    let sender_keys = Keys::generate();
    client
        .add_signer(bot_pubkey, BOT_ID.into(), Arc::new(bot_signer))
        .await;

    let target_keys = Keys::generate();
    let target =
        EventBuilder::new(Kind::PrivateDirectMessage, "original").build(target_keys.public_key());
    let target_id = target.id.ok_or("target rumor missing id")?;
    // A well-formed target `e` tag, but empty reaction content.
    let rumor = nostr_tags::reaction_event(
        target_id,
        target_keys.public_key(),
        Some(Kind::PrivateDirectMessage),
        "",
    )
    .build(sender_keys.public_key());
    let gift_wrap = gift_wrap_rumor(&sender_keys, bot_pubkey, rumor).await?;

    let err = client.decrypt_event(&gift_wrap).await.unwrap_err();
    assert!(matches!(err, DaemonError::Nostr(_)));

    let snap = diagnostics.snapshot().await;
    assert_eq!(snap.invalid_events_total, 1);
    Ok(())
}

// ---------------------------------------------------------------------------
// Scenarios 8 & 9: handler.register accepts/rejects event-type strings
// ---------------------------------------------------------------------------

#[tokio::test]
async fn handler_register_accepts_reaction_received() -> Result<(), Box<dyn std::error::Error>> {
    let (_keys, dispatch, _client, relay, _dir) =
        setup_dispatch(&["ReadMessages"], Duration::from_millis(300)).await?;

    let (tx, _rx) = mpsc::channel(16);
    let connection = ConnectionHandle::new(tx);
    let response = dispatch
        .handle_message(
            JsonRpcMessage::request(
                1.into(),
                "handler.register",
                Some(serde_json::json!({
                    "bot_ids": [BOT_ID],
                    "event_types": ["reaction_received"],
                    "capabilities": ["ReadMessages"],
                })),
            ),
            None,
            Some(connection),
        )
        .await?;
    let result = match response {
        Some(JsonRpcMessage::Response {
            result: Some(result),
            ..
        }) => result,
        other => return Err(format!("unexpected handler.register response: {other:?}").into()),
    };
    let registered_events = result
        .get("registered_events")
        .and_then(|v| v.as_array())
        .ok_or("registered_events missing")?;
    assert!(
        registered_events
            .iter()
            .any(|v| v.as_str() == Some("reaction_received")),
        "registered_events should echo reaction_received, got {registered_events:?}"
    );

    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn handler_register_rejects_unknown_event_type() -> Result<(), Box<dyn std::error::Error>> {
    let (_keys, dispatch, _client, relay, _dir) =
        setup_dispatch(&["ReadMessages"], Duration::from_millis(300)).await?;

    let (tx, _rx) = mpsc::channel(16);
    let connection = ConnectionHandle::new(tx);
    let response = dispatch
        .handle_message(
            JsonRpcMessage::request(
                1.into(),
                "handler.register",
                Some(serde_json::json!({
                    "bot_ids": [BOT_ID],
                    "event_types": ["not_a_real_event_type"],
                    "capabilities": ["ReadMessages"],
                })),
            ),
            None,
            Some(connection),
        )
        .await?;
    match response {
        Some(JsonRpcMessage::Error { error, .. }) => {
            assert_eq!(error.code, -32002);
        }
        other => return Err(format!("expected an error response, got {other:?}").into()),
    }

    relay.stop().await;
    Ok(())
}
