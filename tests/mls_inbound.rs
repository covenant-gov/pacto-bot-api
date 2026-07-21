//! req(R13)
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
mod support;

use support::mock_mls_peer::MockMlsPeer;
use support::mock_relay::MockRelay;

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use nostr::{Keys, ToBech32};
use pacto_bot_api::client_manager::ClientManager;
use pacto_bot_api::config::{BotConfig, DaemonConfig, GlobalDaemonConfig, SigningConfig};
use pacto_bot_api::db::Db;
use pacto_bot_api::diagnostics::Diagnostics;
use pacto_bot_api::dispatch::Dispatch;
use pacto_bot_api::events::{AgentEvent, EventType};
use pacto_bot_api::handlers::ConnectionHandle;
use pacto_bot_api::mls::MlsEngineHandle;
use pacto_bot_api::nostr::NostrClient;
use pacto_bot_api::signer::Signer;
use pacto_bot_api::transport::protocol::JsonRpcMessage;
use secrecy::SecretString;
use tokio::sync::{RwLock, mpsc};
use tokio::time::{Instant, timeout};

fn bot_config(id: &str, keys: &Keys, capabilities: &[&str]) -> BotConfig {
    BotConfig {
        id: id.to_string(),
        display_name: Some(format!("{} Display", id)),
        npub: keys.public_key().to_bech32().unwrap(),
        signing: SigningConfig::Nsec {
            nsec: SecretString::new(keys.secret_key().to_bech32().unwrap().into()),
        },
        relays: vec![],
        capabilities: capabilities.iter().map(|s| s.to_string()).collect(),
        mls_dedup_window_secs: None,
        mls_db_path: Some(std::path::PathBuf::from("vector-mls.db")),
        mls_key_package_freshness_secs: None,
        ..Default::default()
    }
}

async fn setup_mls_dispatch(
    capabilities: &[&str],
) -> Result<
    (
        Keys,
        Arc<Dispatch>,
        Arc<RwLock<ClientManager>>,
        NostrClient,
        MockRelay,
        tempfile::TempDir,
    ),
    Box<dyn std::error::Error>,
> {
    let keys = Keys::generate();
    let bot = bot_config("mls-bot", &keys, capabilities);
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
    let dispatch = Arc::new(Dispatch::new(cm.clone(), db.clone(), Diagnostics::new()));

    {
        let cm_guard = cm.read().await;
        let bot = cm_guard.get_bot_by_id("mls-bot").expect("bot exists");
        let signer = bot.signer.clone();
        let pubkey = bot.signer.public_key();
        cm_guard
            .nostr_client
            .add_signer(pubkey, "mls-bot".to_string(), Arc::new(signer))
            .await;
        if let Some(mls) = bot.mls.clone() {
            cm_guard
                .nostr_client
                .add_mls_engine(pubkey, "mls-bot".to_string(), mls)
                .await;
        }
    }

    {
        let mut cm_guard = cm.write().await;
        cm_guard.subscribe_bots(&db).await?;
    }

    relay.wait_for_subscription(Duration::from_secs(5)).await?;

    let client = cm.read().await.nostr_client.clone();
    Ok((keys, dispatch, cm, client, relay, dir))
}

async fn register_handler_for_bot(
    dispatch: &Dispatch,
    bot_id: &str,
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
                    "bot_ids": [bot_id],
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

async fn setup_multi_bot_mls_dispatch(
    bot_ids: &[&str],
) -> Result<
    (
        Vec<Keys>,
        Arc<Dispatch>,
        Arc<RwLock<ClientManager>>,
        NostrClient,
        MockRelay,
        tempfile::TempDir,
    ),
    Box<dyn std::error::Error>,
> {
    let keys: Vec<Keys> = bot_ids.iter().map(|_| Keys::generate()).collect();
    let bots: Vec<BotConfig> = keys
        .iter()
        .zip(bot_ids.iter())
        .map(|(k, id)| bot_config(id, k, &["ReceiveGroupMessages", "SendGroupMessages"]))
        .collect();
    let config = DaemonConfig {
        daemon: GlobalDaemonConfig::default(),
        bots,
    };
    let dir = common::tempdir()?;
    let relay = MockRelay::start().await?;
    let nostr_client = NostrClient::new(vec![relay.url()]).await?;
    let db = Db::open(dir.path().join("test.db").as_path()).await?;
    let cm = Arc::new(RwLock::new(
        ClientManager::new(dir.path(), config, nostr_client, &db).await?,
    ));
    let dispatch = Arc::new(Dispatch::new(cm.clone(), db.clone(), Diagnostics::new()));

    {
        let cm_guard = cm.read().await;
        for id in bot_ids {
            let bot = cm_guard.get_bot_by_id(id).expect("bot exists");
            let signer = bot.signer.clone();
            let pubkey = bot.signer.public_key();
            cm_guard
                .nostr_client
                .add_signer(pubkey, id.to_string(), Arc::new(signer))
                .await;
            if let Some(mls) = bot.mls.clone() {
                cm_guard
                    .nostr_client
                    .add_mls_engine(pubkey, id.to_string(), mls)
                    .await;
            }
        }
    }

    {
        let mut cm_guard = cm.write().await;
        cm_guard.subscribe_bots(&db).await?;
    }

    relay.wait_for_subscription(Duration::from_secs(5)).await?;

    let client = cm.read().await.nostr_client.clone();
    Ok((keys, dispatch, cm, client, relay, dir))
}

async fn multi_bot_peer_group_setup(
    keys: &[Keys],
    cm: &RwLock<ClientManager>,
) -> Result<(MockMlsPeer, String), Box<dyn std::error::Error>> {
    let mut bot_mls: Vec<MlsEngineHandle> = {
        let cm_guard = cm.read().await;
        cm_guard
            .bot_ids()
            .map(|id| {
                cm_guard
                    .get_bot_by_id(id)
                    .expect("bot exists")
                    .mls
                    .clone()
                    .expect("mls enabled")
            })
            .collect()
    };

    let peer = MockMlsPeer::new();

    // Create key package events for all configured bots.
    let mut bot_key_package_events = Vec::new();
    for (i, mls) in bot_mls.iter().enumerate() {
        let bot_keys = &keys[i];
        let daemon_pubkey = bot_keys.public_key();
        let bot_key_package = mls
            .publish_key_package(&daemon_pubkey, vec![])
            .await
            .expect("publish key package");
        let unsigned = nostr::UnsignedEvent::new(
            daemon_pubkey,
            nostr::Timestamp::now(),
            nostr::Kind::MlsKeyPackage,
            bot_key_package.1,
            bot_key_package.0,
        );
        let signed = unsigned.sign(bot_keys).await?;
        bot_key_package_events.push(signed);
    }

    // Create group with the first bot as the initial member.
    let (group_result, welcome1_rumor) = peer.create_group_with(&bot_key_package_events[0]);

    let wrapper_event_id = nostr::EventId::all_zeros();
    bot_mls[0]
        .process_welcome(wrapper_event_id, peer.sign(welcome1_rumor).await)
        .await?;
    let group_info = bot_mls[0].accept_pending_welcome().await?;
    let wire_id = hex::encode(group_info.nostr_group_id);

    // Add each remaining bot to the group and advance the group state on all
    // existing daemon members so everyone can decrypt subsequent messages.
    for i in 1..bot_mls.len() {
        let (evolution_event, welcome_i_rumor) =
            peer.add_member_to_group(&group_result, &bot_key_package_events[i]);

        for existing_mls in bot_mls.iter_mut().take(i) {
            existing_mls.decrypt_group_message(&evolution_event).await?;
        }

        bot_mls[i]
            .process_welcome(wrapper_event_id, peer.sign(welcome_i_rumor).await)
            .await?;
        bot_mls[i].accept_pending_welcome().await?;
    }

    Ok((peer, wire_id))
}

async fn peer_group_setup(
    keys: &Keys,
    cm: &RwLock<ClientManager>,
) -> Result<(MockMlsPeer, nostr::EventId, String), Box<dyn std::error::Error>> {
    let cm_guard = cm.read().await;
    let bot = cm_guard.get_bot_by_id("mls-bot").expect("bot exists");
    let mls = bot.mls.as_ref().expect("mls enabled");
    let daemon_pubkey = bot.signer.public_key();

    let bot_key_package = mls
        .publish_key_package(&daemon_pubkey, vec![])
        .await
        .expect("publish key package");
    let bot_key_package_event = {
        let unsigned = nostr::UnsignedEvent::new(
            daemon_pubkey,
            nostr::Timestamp::now(),
            nostr::Kind::MlsKeyPackage,
            bot_key_package.1,
            bot_key_package.0,
        );
        unsigned.sign(keys).await?
    };

    let peer = MockMlsPeer::new();
    let (_group_result, welcome_rumor) = peer.create_group_with(&bot_key_package_event);
    let signed_welcome = peer.sign(welcome_rumor).await;
    let wrapper_event_id = nostr::EventId::all_zeros();

    let group_info = {
        mls.process_welcome(wrapper_event_id, signed_welcome)
            .await?;
        mls.accept_pending_welcome().await?
    };
    let wire_id = hex::encode(group_info.nostr_group_id);

    Ok((peer, wrapper_event_id, wire_id))
}

async fn register_handler(
    dispatch: &Dispatch,
    event_types: &[&str],
    capabilities: &[&str],
) -> Result<(String, mpsc::Receiver<JsonRpcMessage>), Box<dyn std::error::Error>> {
    register_handler_for_bot(dispatch, "mls-bot", event_types, capabilities).await
}

fn parse_agent_event(msg: &JsonRpcMessage) -> Option<AgentEvent> {
    match msg {
        JsonRpcMessage::Notification { method, params, .. } if method == "agent.event" => params
            .as_ref()
            .and_then(|p| serde_json::from_value(p.clone()).ok()),
        _ => None,
    }
}

fn parse_rate_limited(msg: &JsonRpcMessage) -> Option<(String, String, u64)> {
    match msg {
        JsonRpcMessage::Notification { method, params, .. } if method == "agent.rate_limited" => {
            let params = params.as_ref()?;
            let bot_id = params.get("bot_id")?.as_str()?.to_string();
            let group_id = params.get("group_id")?.as_str()?.to_string();
            let window_seconds = params.get("window_seconds")?.as_u64()?;
            Some((bot_id, group_id, window_seconds))
        }
        _ => None,
    }
}

async fn consume_stream(
    dispatch: Arc<Dispatch>,
    mut stream: impl StreamExt<
        Item = Result<pacto_bot_api::events::AgentEvent, pacto_bot_api::errors::DaemonError>,
    > + Send
    + Unpin
    + 'static,
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

#[tokio::test]
async fn authorized_handler_receives_group_message() -> Result<(), Box<dyn std::error::Error>> {
    let (keys, dispatch, cm, client, relay, _dir) =
        setup_mls_dispatch(&["ReceiveGroupMessages", "SendGroupMessages"]).await?;
    let (peer, _welcome_id, _wire_id) = peer_group_setup(&keys, &cm).await?;
    let (_, mut rx) = register_handler(
        &dispatch,
        &["mls_group_message_received"],
        &["ReceiveGroupMessages"],
    )
    .await?;

    let message_event = peer.create_group_message("!snapshot").await;
    let expected_group_id = support::mock_mls_peer::group_wire_id(&message_event);
    relay.inject_event(message_event).await;

    let stream = client.receive_events();
    let consumer = tokio::spawn(consume_stream(dispatch, stream));

    let msg = next_message(&mut rx)
        .await
        .ok_or("no agent.event notification")?;
    let event = parse_agent_event(&msg).ok_or("not an agent.event")?;
    assert_eq!(event.bot_id, "mls-bot");
    assert_eq!(event.event_type, EventType::MlsGroupMessageReceived);
    assert_eq!(event.content, "!snapshot");
    assert_eq!(event.chat_id, expected_group_id);

    consumer.abort();
    let _ = consumer.await;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn unauthorized_handler_is_excluded() -> Result<(), Box<dyn std::error::Error>> {
    let (keys, dispatch, cm, client, relay, _dir) =
        setup_mls_dispatch(&["ReceiveGroupMessages", "SendGroupMessages"]).await?;
    let (peer, _welcome_id, _wire_id) = peer_group_setup(&keys, &cm).await?;
    let (_, mut rx) = register_handler(
        &dispatch,
        &["mls_group_message_received"],
        &["SendGroupMessages"],
    )
    .await?;

    let message_event = peer.create_group_message("!snapshot").await;
    relay.inject_event(message_event).await;

    let stream = client.receive_events();
    let consumer = tokio::spawn(consume_stream(dispatch, stream));

    let result = timeout(Duration::from_secs(2), rx.recv()).await;
    let received = result.ok().flatten().is_some();
    assert!(
        !received,
        "handler without ReceiveGroupMessages should not receive the event"
    );

    consumer.abort();
    let _ = consumer.await;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn is_squad_member_returns_true_for_member() -> Result<(), Box<dyn std::error::Error>> {
    let (keys, dispatch, cm, _client, relay, _dir) =
        setup_mls_dispatch(&["ReceiveGroupMessages", "SendGroupMessages"]).await?;
    let (peer, _welcome_id, wire_id) = peer_group_setup(&keys, &cm).await?;
    let (handler_id, _rx) = register_handler(
        &dispatch,
        &["mls_group_message_received"],
        &["ReceiveGroupMessages"],
    )
    .await?;

    let response = dispatch
        .handle_message(
            JsonRpcMessage::request(
                1.into(),
                "agent.is_squad_member",
                Some(serde_json::json!({
                    "bot_id": "mls-bot",
                    "group_id": wire_id,
                    "member_pubkey": peer.public_key().to_hex(),
                })),
            ),
            Some(handler_id.as_str()),
            None,
        )
        .await?;

    let result = response
        .and_then(|r| match r {
            JsonRpcMessage::Response { result, .. } => result,
            _ => None,
        })
        .ok_or("missing response")?;
    let is_member = result
        .get("is_member")
        .and_then(serde_json::Value::as_bool)
        .ok_or("missing is_member in response")?;
    assert!(is_member, "peer should be a member of the squad");

    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn is_squad_member_returns_false_for_non_member() -> Result<(), Box<dyn std::error::Error>> {
    let (keys, dispatch, cm, _client, relay, _dir) =
        setup_mls_dispatch(&["ReceiveGroupMessages", "SendGroupMessages"]).await?;
    let (_peer, _welcome_id, wire_id) = peer_group_setup(&keys, &cm).await?;
    let (handler_id, _rx) = register_handler(
        &dispatch,
        &["mls_group_message_received"],
        &["ReceiveGroupMessages"],
    )
    .await?;

    let non_member = Keys::generate();
    let response = dispatch
        .handle_message(
            JsonRpcMessage::request(
                1.into(),
                "agent.is_squad_member",
                Some(serde_json::json!({
                    "bot_id": "mls-bot",
                    "group_id": wire_id,
                    "member_pubkey": non_member.public_key().to_hex(),
                })),
            ),
            Some(handler_id.as_str()),
            None,
        )
        .await?;

    let result = response
        .and_then(|r| match r {
            JsonRpcMessage::Response { result, .. } => result,
            _ => None,
        })
        .ok_or("missing response")?;
    let is_member = result
        .get("is_member")
        .and_then(serde_json::Value::as_bool)
        .ok_or("missing is_member in response")?;
    assert!(!is_member, "random key should not be a member of the squad");

    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn rate_limit_emits_agent_rate_limited() -> Result<(), Box<dyn std::error::Error>> {
    let (keys, dispatch, cm, client, relay, _dir) =
        setup_mls_dispatch(&["ReceiveGroupMessages", "SendGroupMessages"]).await?;
    let (peer, _welcome_id, _wire_id) = peer_group_setup(&keys, &cm).await?;
    let (handler_id, mut rx) = register_handler(
        &dispatch,
        &["mls_group_message_received"],
        &["ReceiveGroupMessages"],
    )
    .await?;

    let message_event1 = peer.create_group_message("!snapshot").await;
    // Ensure the two wrapper events have distinct event IDs so the second
    // message is not dropped by the deduplication cache before rate limiting.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let message_event2 = peer.create_group_message("!snapshot-2").await;
    let expected_group_id = support::mock_mls_peer::group_wire_id(&message_event1);
    relay.inject_event(message_event1).await;
    relay.inject_event(message_event2).await;

    let stream = client.receive_events();
    let consumer = tokio::spawn(consume_stream(dispatch.clone(), stream));

    let mut got_event = false;
    let mut got_rate_limited = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !(got_event && got_rate_limited) {
        if let Some(msg) = next_message(&mut rx).await {
            if let Some(event) = parse_agent_event(&msg) {
                got_event = true;
                // Acknowledge the first event so the dispatch loop can move on
                // and rate-limit the next message instead of waiting for the
                // default dispatch timeout.
                let _ = dispatch
                    .handle_message(
                        JsonRpcMessage::notification(
                            "handler.response",
                            Some(serde_json::json!({
                                "event_id": event.event_id,
                                "action": "ack",
                            })),
                        ),
                        Some(handler_id.as_str()),
                        None,
                    )
                    .await;
            } else if let Some((bot_id, group_id, window)) = parse_rate_limited(&msg) {
                assert_eq!(bot_id, "mls-bot");
                assert_eq!(Some(group_id), expected_group_id);
                assert_eq!(window, 60);
                got_rate_limited = true;
            }
        }
    }

    assert!(
        got_event,
        "first message should be dispatched as agent.event"
    );
    assert!(
        got_rate_limited,
        "second message within the window should trigger agent.rate_limited"
    );

    consumer.abort();
    let _ = consumer.await;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn duplicate_event_is_deduplicated() -> Result<(), Box<dyn std::error::Error>> {
    let (keys, dispatch, cm, client, relay, _dir) =
        setup_mls_dispatch(&["ReceiveGroupMessages", "SendGroupMessages"]).await?;
    let (peer, _welcome_id, _wire_id) = peer_group_setup(&keys, &cm).await?;
    let (_, mut rx) = register_handler(
        &dispatch,
        &["mls_group_message_received"],
        &["ReceiveGroupMessages"],
    )
    .await?;

    let message_event = peer.create_group_message("!snapshot").await;
    relay.inject_event(message_event.clone()).await;
    relay.inject_event(message_event).await;

    let stream = client.receive_events();
    let consumer = tokio::spawn(consume_stream(dispatch, stream));

    let msg1 = next_message(&mut rx).await.ok_or("no first agent.event")?;
    assert!(parse_agent_event(&msg1).is_some());

    let result = timeout(Duration::from_secs(2), rx.recv()).await;
    assert!(
        result.is_err(),
        "duplicate event should not be dispatched again"
    );

    consumer.abort();
    let _ = consumer.await;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn daemon_skips_own_group_message() -> Result<(), Box<dyn std::error::Error>> {
    let (keys, dispatch, cm, client, relay, _dir) =
        setup_mls_dispatch(&["ReceiveGroupMessages", "SendGroupMessages"]).await?;
    let (_peer, _welcome_id, _wire_id) = peer_group_setup(&keys, &cm).await?;
    let (_, mut rx) = register_handler(
        &dispatch,
        &["mls_group_message_received"],
        &["ReceiveGroupMessages"],
    )
    .await?;

    let own_pubkey = {
        let cm_guard = cm.read().await;
        let bot = cm_guard.get_bot_by_id("mls-bot").expect("bot exists");
        bot.signer.public_key()
    };
    let own_message = nostr::EventBuilder::new(nostr::Kind::MlsGroupMessage, "ignored")
        .tags([nostr::Tag::parse([
            "h",
            "0000000000000000000000000000000000000000000000000000000000000000",
        ])
        .expect("h tag")])
        .sign_with_keys(&keys)
        .expect("sign own message");
    assert_eq!(own_message.pubkey, own_pubkey);
    relay.inject_event(own_message).await;

    let stream = client.receive_events();
    let consumer = tokio::spawn(consume_stream(dispatch, stream));

    let result = timeout(Duration::from_secs(2), rx.recv()).await;
    assert!(result.is_err(), "daemon should skip its own group message");

    consumer.abort();
    let _ = consumer.await;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn non_member_group_message_is_dropped() -> Result<(), Box<dyn std::error::Error>> {
    let (_keys, dispatch, _cm, client, relay, _dir) =
        setup_mls_dispatch(&["ReceiveGroupMessages", "SendGroupMessages"]).await?;
    let (_, mut rx) = register_handler(
        &dispatch,
        &["mls_group_message_received"],
        &["ReceiveGroupMessages"],
    )
    .await?;

    // A message from a random sender with a group id the daemon does not know.
    let sender = Keys::generate();
    let fake_message = nostr::EventBuilder::new(nostr::Kind::MlsGroupMessage, "not a member")
        .tags([nostr::Tag::parse([
            "h",
            "0000000000000000000000000000000000000000000000000000000000000001",
        ])
        .expect("h tag")])
        .sign_with_keys(&sender)
        .expect("sign fake message");
    relay.inject_event(fake_message).await;

    let stream = client.receive_events();
    let consumer = tokio::spawn(consume_stream(dispatch, stream));

    let result = timeout(Duration::from_secs(2), rx.recv()).await;
    assert!(
        result.is_err(),
        "message for an unknown squad should be dropped"
    );

    consumer.abort();
    let _ = consumer.await;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn malformed_group_message_is_dropped() -> Result<(), Box<dyn std::error::Error>> {
    let (_keys, dispatch, _cm, client, relay, _dir) =
        setup_mls_dispatch(&["ReceiveGroupMessages", "SendGroupMessages"]).await?;
    let (_, mut rx) = register_handler(
        &dispatch,
        &["mls_group_message_received"],
        &["ReceiveGroupMessages"],
    )
    .await?;

    let sender = Keys::generate();
    let fake_message = nostr::EventBuilder::new(nostr::Kind::MlsGroupMessage, "not valid mls")
        .tags([nostr::Tag::parse([
            "h",
            "0000000000000000000000000000000000000000000000000000000000000000",
        ])
        .expect("h tag")])
        .sign_with_keys(&sender)
        .expect("sign fake message");
    relay.inject_event(fake_message).await;

    let stream = client.receive_events();
    let consumer = tokio::spawn(consume_stream(dispatch, stream));

    let result = timeout(Duration::from_secs(2), rx.recv()).await;
    assert!(
        result.is_err(),
        "malformed group message should not be dispatched to handlers"
    );

    consumer.abort();
    let _ = consumer.await;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn valid_envelope_is_parsed_into_content_and_mentions()
-> Result<(), Box<dyn std::error::Error>> {
    let (keys, dispatch, cm, client, relay, _dir) =
        setup_mls_dispatch(&["ReceiveGroupMessages", "SendGroupMessages"]).await?;
    let (peer, _welcome_id, _wire_id) = peer_group_setup(&keys, &cm).await?;
    let (_, mut rx) = register_handler(
        &dispatch,
        &["mls_group_message_received"],
        &["ReceiveGroupMessages"],
    )
    .await?;

    let target = Keys::generate();
    let target_npub = target.public_key().to_bech32()?;
    let envelope = serde_json::json!({
        "body": "@Joke Bot /help",
        "mentions": [{"npub": target_npub, "alias": "Joke Bot"}],
    });
    let message_event = peer.create_group_message(&envelope.to_string()).await;
    let expected_group_id = support::mock_mls_peer::group_wire_id(&message_event);
    relay.inject_event(message_event).await;

    let stream = client.receive_events();
    let consumer = tokio::spawn(consume_stream(dispatch, stream));

    let msg = next_message(&mut rx)
        .await
        .ok_or("no agent.event notification")?;
    let event = parse_agent_event(&msg).ok_or("not an agent.event")?;
    assert_eq!(event.bot_id, "mls-bot");
    assert_eq!(event.event_type, EventType::MlsGroupMessageReceived);
    assert_eq!(event.content, "@Joke Bot /help");
    assert_eq!(event.chat_id, expected_group_id);
    assert_eq!(event.mentions, vec![target_npub]);

    consumer.abort();
    let _ = consumer.await;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn legacy_plaintext_message_is_preserved() -> Result<(), Box<dyn std::error::Error>> {
    let (keys, dispatch, cm, client, relay, _dir) =
        setup_mls_dispatch(&["ReceiveGroupMessages", "SendGroupMessages"]).await?;
    let (peer, _welcome_id, _wire_id) = peer_group_setup(&keys, &cm).await?;
    let (_, mut rx) = register_handler(
        &dispatch,
        &["mls_group_message_received"],
        &["ReceiveGroupMessages"],
    )
    .await?;

    let message_event = peer.create_group_message("!snapshot").await;
    relay.inject_event(message_event).await;

    let stream = client.receive_events();
    let consumer = tokio::spawn(consume_stream(dispatch, stream));

    let msg = next_message(&mut rx)
        .await
        .ok_or("no agent.event notification")?;
    let event = parse_agent_event(&msg).ok_or("not an agent.event")?;
    assert_eq!(event.content, "!snapshot");
    assert!(event.mentions.is_empty());

    consumer.abort();
    let _ = consumer.await;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn json_without_envelope_shape_is_treated_as_legacy() -> Result<(), Box<dyn std::error::Error>>
{
    let (keys, dispatch, cm, client, relay, _dir) =
        setup_mls_dispatch(&["ReceiveGroupMessages", "SendGroupMessages"]).await?;
    let (peer, _welcome_id, _wire_id) = peer_group_setup(&keys, &cm).await?;
    let (_, mut rx) = register_handler(
        &dispatch,
        &["mls_group_message_received"],
        &["ReceiveGroupMessages"],
    )
    .await?;

    let raw = r#"{"foo":"bar"}"#;
    let message_event = peer.create_group_message(raw).await;
    relay.inject_event(message_event).await;

    let stream = client.receive_events();
    let consumer = tokio::spawn(consume_stream(dispatch, stream));

    let msg = next_message(&mut rx)
        .await
        .ok_or("no agent.event notification")?;
    let event = parse_agent_event(&msg).ok_or("not an agent.event")?;
    assert_eq!(event.content, raw);
    assert!(event.mentions.is_empty());

    consumer.abort();
    let _ = consumer.await;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn envelope_missing_npub_falls_back_to_legacy() -> Result<(), Box<dyn std::error::Error>> {
    let (keys, dispatch, cm, client, relay, _dir) =
        setup_mls_dispatch(&["ReceiveGroupMessages", "SendGroupMessages"]).await?;
    let (peer, _welcome_id, _wire_id) = peer_group_setup(&keys, &cm).await?;
    let (_, mut rx) = register_handler(
        &dispatch,
        &["mls_group_message_received"],
        &["ReceiveGroupMessages"],
    )
    .await?;

    let raw = r#"{"body":"hi","mentions":[{"alias":"Joke Bot"}]}"#;
    let message_event = peer.create_group_message(raw).await;
    relay.inject_event(message_event).await;

    let stream = client.receive_events();
    let consumer = tokio::spawn(consume_stream(dispatch, stream));

    let msg = next_message(&mut rx)
        .await
        .ok_or("no agent.event notification")?;
    let event = parse_agent_event(&msg).ok_or("not an agent.event")?;
    assert_eq!(event.content, raw);
    assert!(event.mentions.is_empty());

    consumer.abort();
    let _ = consumer.await;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn invalid_json_is_treated_as_legacy() -> Result<(), Box<dyn std::error::Error>> {
    let (keys, dispatch, cm, client, relay, _dir) =
        setup_mls_dispatch(&["ReceiveGroupMessages", "SendGroupMessages"]).await?;
    let (peer, _welcome_id, _wire_id) = peer_group_setup(&keys, &cm).await?;
    let (_, mut rx) = register_handler(
        &dispatch,
        &["mls_group_message_received"],
        &["ReceiveGroupMessages"],
    )
    .await?;

    let raw = "not valid json {";
    let message_event = peer.create_group_message(raw).await;
    relay.inject_event(message_event).await;

    let stream = client.receive_events();
    let consumer = tokio::spawn(consume_stream(dispatch, stream));

    let msg = next_message(&mut rx)
        .await
        .ok_or("no agent.event notification")?;
    let event = parse_agent_event(&msg).ok_or("not an agent.event")?;
    assert_eq!(event.content, raw);
    assert!(event.mentions.is_empty());

    consumer.abort();
    let _ = consumer.await;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn envelope_is_parsed_before_deduplication_gate() -> Result<(), Box<dyn std::error::Error>> {
    let (keys, dispatch, cm, client, relay, _dir) =
        setup_mls_dispatch(&["ReceiveGroupMessages", "SendGroupMessages"]).await?;
    let (peer, _welcome_id, _wire_id) = peer_group_setup(&keys, &cm).await?;
    let (_, mut rx) = register_handler(
        &dispatch,
        &["mls_group_message_received"],
        &["ReceiveGroupMessages"],
    )
    .await?;

    let target = Keys::generate();
    let target_npub = target.public_key().to_bech32()?;
    let envelope = serde_json::json!({
        "body": "@Joke Bot /help",
        "mentions": [{"npub": target_npub, "alias": "Joke Bot"}],
    });
    let message_event = peer.create_group_message(&envelope.to_string()).await;
    relay.inject_event(message_event.clone()).await;
    relay.inject_event(message_event).await;

    let stream = client.receive_events();
    let consumer = tokio::spawn(consume_stream(dispatch, stream));

    let msg1 = next_message(&mut rx).await.ok_or("no first agent.event")?;
    let event = parse_agent_event(&msg1).ok_or("not an agent.event")?;
    assert_eq!(event.content, "@Joke Bot /help");
    assert_eq!(event.mentions, vec![target_npub]);

    let result = timeout(Duration::from_secs(2), rx.recv()).await;
    assert!(
        result.is_err(),
        "duplicate envelope event should be deduplicated"
    );

    consumer.abort();
    let _ = consumer.await;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn multi_bot_mention_metadata() -> Result<(), Box<dyn std::error::Error>> {
    let bot_ids = ["bot-a", "bot-b"];
    let (keys, dispatch, cm, client, relay, _dir) = setup_multi_bot_mls_dispatch(&bot_ids).await?;
    let (peer, _wire_id) = multi_bot_peer_group_setup(&keys, &cm).await?;

    let (handler_id_a, mut rx_a) = register_handler_for_bot(
        &dispatch,
        "bot-a",
        &["mls_group_message_received"],
        &["ReceiveGroupMessages"],
    )
    .await?;
    let (handler_id_b, mut rx_b) = register_handler_for_bot(
        &dispatch,
        "bot-b",
        &["mls_group_message_received"],
        &["ReceiveGroupMessages"],
    )
    .await?;

    let npub_b = keys[1].public_key().to_bech32()?;
    let envelope = serde_json::json!({
        "body": "@bot-b hi",
        "mentions": [{"npub": npub_b, "alias": "bot-b"}],
    });
    let message_event = peer.create_group_message(&envelope.to_string()).await;

    let stream = client.receive_events();
    let consumer = tokio::spawn(consume_stream(dispatch.clone(), stream));
    relay.inject_event(message_event).await;

    let mut event_a: Option<AgentEvent> = None;
    let mut event_b: Option<AgentEvent> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while event_a.is_none() || event_b.is_none() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("timeout waiting for both bots to receive event".into());
        }
        tokio::select! {
            Some(msg) = rx_a.recv() => {
                let event = parse_agent_event(&msg).ok_or("not an agent.event")?;
                dispatch
                    .handle_message(
                        JsonRpcMessage::request(
                            1.into(),
                            "handler.response",
                            Some(serde_json::json!({
                                "event_id": event.event_id,
                                "action": "ack",
                            })),
                        ),
                        Some(&handler_id_a),
                        None,
                    )
                    .await?;
                event_a = Some(event);
            }
            Some(msg) = rx_b.recv() => {
                let event = parse_agent_event(&msg).ok_or("not an agent.event")?;
                dispatch
                    .handle_message(
                        JsonRpcMessage::request(
                            1.into(),
                            "handler.response",
                            Some(serde_json::json!({
                                "event_id": event.event_id,
                                "action": "ack",
                            })),
                        ),
                        Some(&handler_id_b),
                        None,
                    )
                    .await?;
                event_b = Some(event);
            }
            _ = tokio::time::sleep(remaining) => {
                return Err("timeout waiting for both bots to receive event".into());
            }
        }
    }

    let event_a = event_a.unwrap();
    let event_b = event_b.unwrap();

    assert_eq!(event_a.bot_id, "bot-a");
    assert!(!event_a.is_mentioned);
    assert_eq!(event_a.mentioned_bot_ids, vec!["bot-b"]);
    assert_eq!(event_a.content, "@bot-b hi");

    assert_eq!(event_b.bot_id, "bot-b");
    assert!(event_b.is_mentioned);
    assert_eq!(event_b.mentioned_bot_ids, vec!["bot-b"]);
    assert_eq!(event_b.content, "@bot-b hi");

    consumer.abort();
    let _ = consumer.await;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn mentioned_bot_receives_is_mentioned_true() -> Result<(), Box<dyn std::error::Error>> {
    let (keys, dispatch, cm, client, relay, _dir) =
        setup_mls_dispatch(&["ReceiveGroupMessages", "SendGroupMessages"]).await?;
    let (peer, _welcome_id, _wire_id) = peer_group_setup(&keys, &cm).await?;
    let (_, mut rx) = register_handler(
        &dispatch,
        &["mls_group_message_received"],
        &["ReceiveGroupMessages"],
    )
    .await?;

    let bot_npub = keys.public_key().to_bech32()?;
    let envelope = serde_json::json!({
        "body": "@mls-bot /help",
        "mentions": [{"npub": bot_npub, "alias": "mls-bot"}],
    });
    let message_event = peer.create_group_message(&envelope.to_string()).await;
    relay.inject_event(message_event).await;

    let stream = client.receive_events();
    let consumer = tokio::spawn(consume_stream(dispatch, stream));

    let msg = next_message(&mut rx)
        .await
        .ok_or("no agent.event notification")?;
    let event = parse_agent_event(&msg).ok_or("not an agent.event")?;
    assert_eq!(event.bot_id, "mls-bot");
    assert_eq!(event.content, "@mls-bot /help");
    assert!(event.is_mentioned);
    assert_eq!(event.mentioned_bot_ids, vec!["mls-bot"]);

    consumer.abort();
    let _ = consumer.await;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn joke_bot_mention_reaches_only_target() -> Result<(), Box<dyn std::error::Error>> {
    let bot_ids = ["joke-bot", "snapshot-bot"];
    let (keys, dispatch, cm, client, relay, _dir) = setup_multi_bot_mls_dispatch(&bot_ids).await?;
    let (peer, _wire_id) = multi_bot_peer_group_setup(&keys, &cm).await?;

    let (handler_id_joke, mut rx_joke) = register_handler_for_bot(
        &dispatch,
        "joke-bot",
        &["mls_group_message_received"],
        &["ReceiveGroupMessages"],
    )
    .await?;
    let (handler_id_snapshot, mut rx_snapshot) = register_handler_for_bot(
        &dispatch,
        "snapshot-bot",
        &["mls_group_message_received"],
        &["ReceiveGroupMessages"],
    )
    .await?;

    let joke_npub = keys[0].public_key().to_bech32()?;
    let envelope = serde_json::json!({
        "body": "@Joke Bot /help",
        "mentions": [{"npub": joke_npub, "alias": "Joke Bot"}],
    });
    let message_event = peer.create_group_message(&envelope.to_string()).await;

    let stream = client.receive_events();
    let consumer = tokio::spawn(consume_stream(dispatch.clone(), stream));
    relay.inject_event(message_event).await;

    let mut joke_event: Option<AgentEvent> = None;
    let mut snapshot_event: Option<AgentEvent> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while joke_event.is_none() || snapshot_event.is_none() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("timeout waiting for both bots to receive event".into());
        }
        tokio::select! {
            Some(msg) = rx_joke.recv() => {
                let event = parse_agent_event(&msg).ok_or("not an agent.event")?;
                dispatch
                    .handle_message(
                        JsonRpcMessage::request(
                            1.into(),
                            "handler.response",
                            Some(serde_json::json!({
                                "event_id": event.event_id,
                                "action": "ack",
                            })),
                        ),
                        Some(&handler_id_joke),
                        None,
                    )
                    .await?;
                joke_event = Some(event);
            }
            Some(msg) = rx_snapshot.recv() => {
                let event = parse_agent_event(&msg).ok_or("not an agent.event")?;
                dispatch
                    .handle_message(
                        JsonRpcMessage::request(
                            1.into(),
                            "handler.response",
                            Some(serde_json::json!({
                                "event_id": event.event_id,
                                "action": "ack",
                            })),
                        ),
                        Some(&handler_id_snapshot),
                        None,
                    )
                    .await?;
                snapshot_event = Some(event);
            }
            _ = tokio::time::sleep(remaining) => {
                return Err("timeout waiting for both bots to receive event".into());
            }
        }
    }

    let joke_event = joke_event.unwrap();
    let snapshot_event = snapshot_event.unwrap();

    assert_eq!(joke_event.bot_id, "joke-bot");
    assert!(joke_event.is_mentioned);
    assert_eq!(joke_event.mentioned_bot_ids, vec!["joke-bot"]);
    assert_eq!(joke_event.content, "@Joke Bot /help");

    assert_eq!(snapshot_event.bot_id, "snapshot-bot");
    assert!(!snapshot_event.is_mentioned);
    assert_eq!(snapshot_event.mentioned_bot_ids, vec!["joke-bot"]);
    assert_eq!(snapshot_event.content, "@Joke Bot /help");

    consumer.abort();
    let _ = consumer.await;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn envelope_with_empty_mentions_marks_all_bots_not_mentioned()
-> Result<(), Box<dyn std::error::Error>> {
    let bot_ids = ["bot-a", "bot-b"];
    let (keys, dispatch, cm, client, relay, _dir) = setup_multi_bot_mls_dispatch(&bot_ids).await?;
    let (peer, _wire_id) = multi_bot_peer_group_setup(&keys, &cm).await?;

    let (handler_id_a, mut rx_a) = register_handler_for_bot(
        &dispatch,
        "bot-a",
        &["mls_group_message_received"],
        &["ReceiveGroupMessages"],
    )
    .await?;
    let (handler_id_b, mut rx_b) = register_handler_for_bot(
        &dispatch,
        "bot-b",
        &["mls_group_message_received"],
        &["ReceiveGroupMessages"],
    )
    .await?;

    let envelope = serde_json::json!({
        "body": "just a regular message",
        "mentions": [],
    });
    let message_event = peer.create_group_message(&envelope.to_string()).await;

    let stream = client.receive_events();
    let consumer = tokio::spawn(consume_stream(dispatch.clone(), stream));
    relay.inject_event(message_event).await;

    let mut event_a: Option<AgentEvent> = None;
    let mut event_b: Option<AgentEvent> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while event_a.is_none() || event_b.is_none() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("timeout waiting for both bots to receive event".into());
        }
        tokio::select! {
            Some(msg) = rx_a.recv() => {
                let event = parse_agent_event(&msg).ok_or("not an agent.event")?;
                dispatch
                    .handle_message(
                        JsonRpcMessage::request(
                            1.into(),
                            "handler.response",
                            Some(serde_json::json!({
                                "event_id": event.event_id,
                                "action": "ack",
                            })),
                        ),
                        Some(&handler_id_a),
                        None,
                    )
                    .await?;
                event_a = Some(event);
            }
            Some(msg) = rx_b.recv() => {
                let event = parse_agent_event(&msg).ok_or("not an agent.event")?;
                dispatch
                    .handle_message(
                        JsonRpcMessage::request(
                            1.into(),
                            "handler.response",
                            Some(serde_json::json!({
                                "event_id": event.event_id,
                                "action": "ack",
                            })),
                        ),
                        Some(&handler_id_b),
                        None,
                    )
                    .await?;
                event_b = Some(event);
            }
            _ = tokio::time::sleep(remaining) => {
                return Err("timeout waiting for both bots to receive event".into());
            }
        }
    }

    let event_a = event_a.unwrap();
    let event_b = event_b.unwrap();

    assert_eq!(event_a.bot_id, "bot-a");
    assert!(!event_a.is_mentioned);
    assert!(event_a.mentioned_bot_ids.is_empty());
    assert!(event_a.mentions.is_empty());
    assert_eq!(event_a.content, "just a regular message");

    assert_eq!(event_b.bot_id, "bot-b");
    assert!(!event_b.is_mentioned);
    assert!(event_b.mentioned_bot_ids.is_empty());
    assert!(event_b.mentions.is_empty());
    assert_eq!(event_b.content, "just a regular message");

    consumer.abort();
    let _ = consumer.await;
    relay.stop().await;
    Ok(())
}
