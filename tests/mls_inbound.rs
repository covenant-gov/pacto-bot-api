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
use pacto_bot_api::nostr::NostrClient;
use pacto_bot_api::signer::Signer;
use pacto_bot_api::transport::protocol::JsonRpcMessage;
use secrecy::SecretString;
use tokio::sync::{RwLock, mpsc};
use tokio::time::{Instant, timeout};

fn bot_config(id: &str, keys: &Keys, capabilities: &[&str]) -> BotConfig {
    BotConfig {
        id: id.to_string(),
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
    let cm = Arc::new(RwLock::new(
        ClientManager::new(dir.path(), config, nostr_client).await?,
    ));
    let db = Db::open(dir.path().join("test.db").as_path()).await?;
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
    let (tx, rx) = mpsc::channel(16);
    let connection = ConnectionHandle::new(tx);
    let response = dispatch
        .handle_message(
            JsonRpcMessage::request(
                1.into(),
                "handler.register",
                Some(serde_json::json!({
                    "bot_ids": ["mls-bot"],
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
