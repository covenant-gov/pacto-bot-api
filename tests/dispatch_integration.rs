/// req(R14, R15, R16, R17, R18, R26, R27, R30)
mod support;

use crate::support::mock_relay::MockRelay;

use std::sync::Arc;
use std::time::{Duration, Instant};

use nostr::{Kind, ToBech32};
use pacto_bot_api::client_manager::ClientManager;
use pacto_bot_api::config::{BotConfig, DaemonConfig, GlobalDaemonConfig, SigningConfig};
use pacto_bot_api::db::Db;
use pacto_bot_api::diagnostics::Diagnostics;
use pacto_bot_api::dispatch::{
    DEFAULT_BOT_BURST, DEFAULT_BOT_RATE, DEFAULT_HANDLER_BURST, DEFAULT_HANDLER_RATE, Dispatch,
    RateLimiter,
};
use pacto_bot_api::errors::DaemonError;
use pacto_bot_api::events::{AgentEvent, EventType};
use pacto_bot_api::handlers::ConnectionHandle;
use pacto_bot_api::nostr::NostrClient;
use pacto_bot_api::transport::protocol::{JsonRpcMessage, MetricsResponse};
use secrecy::SecretString;
use serde_json::Value;
use tempfile::tempdir;
use tokio::sync::RwLock;
use tokio::time::timeout;

fn test_keys() -> nostr::Keys {
    nostr::Keys::generate()
}

fn bot_config(id: &str, keys: &nostr::Keys, capabilities: &[&str]) -> BotConfig {
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

async fn setup_dispatch(
    bot_configs: Vec<BotConfig>,
    rate_limiter: Option<RateLimiter>,
    dispatch_timeout: Option<Duration>,
) -> Result<(Arc<Dispatch>, Arc<RwLock<ClientManager>>), Box<dyn std::error::Error>> {
    setup_dispatch_with_client(
        bot_configs,
        NostrClient::new(vec![]).await?,
        rate_limiter,
        dispatch_timeout,
    )
    .await
}

async fn setup_dispatch_with_client(
    bot_configs: Vec<BotConfig>,
    nostr_client: NostrClient,
    rate_limiter: Option<RateLimiter>,
    dispatch_timeout: Option<Duration>,
) -> Result<(Arc<Dispatch>, Arc<RwLock<ClientManager>>), Box<dyn std::error::Error>> {
    let config = DaemonConfig {
        daemon: GlobalDaemonConfig::default(),
        bots: bot_configs,
    };
    let cm = Arc::new(RwLock::new(ClientManager::new(config, nostr_client).await?));
    let dir = tempdir()?;
    let db = Db::open(dir.path().join("test.db").as_path()).await?;
    let diagnostics = Diagnostics::new();
    let mut dispatch = match rate_limiter {
        Some(limiter) => Dispatch::with_rate_limiter(cm.clone(), db, diagnostics, limiter),
        None => Dispatch::new(cm.clone(), db, diagnostics),
    };
    if let Some(timeout) = dispatch_timeout {
        dispatch.set_dispatch_timeout(timeout);
    }
    Ok((Arc::new(dispatch), cm))
}

fn sample_event(bot_id: &str) -> AgentEvent {
    AgentEvent {
        bot_id: bot_id.to_string(),
        event_id: "evt1".to_string(),
        event_type: EventType::DmReceived,
        chat_id: None,
        content: "hello".to_string(),
        rumor_id: "rumor1".to_string(),
        author: "npub1sender".to_string(),
        timestamp: 42,
    }
}

fn parse_event_notification(msg: &JsonRpcMessage) -> Option<AgentEvent> {
    let JsonRpcMessage::Notification { method, params, .. } = msg else {
        return None;
    };
    if method != "agent.event" {
        return None;
    }
    let params = params.as_ref()?;
    serde_json::from_value(params.clone()).ok()
}

fn parse_metrics_notification(msg: &JsonRpcMessage) -> Option<MetricsResponse> {
    let JsonRpcMessage::Notification { method, params, .. } = msg else {
        return None;
    };
    if method != "agent.metrics" {
        return None;
    }
    let params = params.as_ref()?;
    serde_json::from_value(params.clone()).ok()
}

#[tokio::test]
async fn fan_out_delivers_event_and_advances_cursor() -> Result<(), Box<dyn std::error::Error>> {
    let keys = test_keys();
    let (dispatch, cm) = setup_dispatch(
        vec![bot_config(
            "echo-bot",
            &keys,
            &["ReadMessages", "SendMessages"],
        )],
        None,
        None,
    )
    .await?;

    let bot_config_for_register = {
        let cm = cm.read().await;
        cm.get_bot_by_id("echo-bot").unwrap().config.clone()
    };

    let (tx1, mut rx1) = tokio::sync::mpsc::channel(64);
    let (tx2, mut rx2) = tokio::sync::mpsc::channel(64);

    let (handler_id1, handler_id2) = {
        let mut cm = cm.write().await;
        let id1 = cm
            .handler_registry
            .register(
                ConnectionHandle::new(tx1),
                vec!["echo-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["ReadMessages".to_string(), "SendMessages".to_string()],
                std::slice::from_ref(&bot_config_for_register),
            )?
            .handler_id;
        let id2 = cm
            .handler_registry
            .register(
                ConnectionHandle::new(tx2),
                vec!["echo-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["ReadMessages".to_string()],
                &[bot_config_for_register],
            )?
            .handler_id;
        (id1, id2)
    };

    let dispatch_for_h1 = Arc::clone(&dispatch);
    let handler_id1_for_task = handler_id1.clone();
    let h1 = tokio::spawn(async move {
        let msg = timeout(Duration::from_secs(1), rx1.recv())
            .await
            .map_err(|_| DaemonError::Config("timeout waiting for handler 1 event".into()))?
            .ok_or(DaemonError::Config(
                "handler 1 did not receive event".into(),
            ))?;
        let event = parse_event_notification(&msg).ok_or(DaemonError::Config(
            "handler 1 received invalid event notification".into(),
        ))?;
        assert_eq!(event.event_id, "evt1");
        dispatch_for_h1
            .handle_message(
                JsonRpcMessage::notification(
                    "handler.response",
                    Some(serde_json::json!({
                        "event_id": "evt1",
                        "action": "ack",
                    })),
                ),
                Some(&handler_id1_for_task),
                None,
            )
            .await?;
        Ok::<(), DaemonError>(())
    });

    let dispatch_for_h2 = Arc::clone(&dispatch);
    let handler_id2_for_task = handler_id2.clone();
    let h2 = tokio::spawn(async move {
        let msg = timeout(Duration::from_secs(1), rx2.recv())
            .await
            .map_err(|_| DaemonError::Config("timeout waiting for handler 2 event".into()))?
            .ok_or(DaemonError::Config(
                "handler 2 did not receive event".into(),
            ))?;
        let event = parse_event_notification(&msg).ok_or(DaemonError::Config(
            "handler 2 received invalid event notification".into(),
        ))?;
        assert_eq!(event.event_id, "evt1");
        dispatch_for_h2
            .handle_message(
                JsonRpcMessage::notification(
                    "handler.response",
                    Some(serde_json::json!({
                        "event_id": "evt1",
                        "action": "ack",
                    })),
                ),
                Some(&handler_id2_for_task),
                None,
            )
            .await?;
        Ok::<(), DaemonError>(())
    });

    dispatch.dispatch_event(sample_event("echo-bot")).await?;

    h1.await
        .map_err(|e| DaemonError::Config(format!("handler 1 task panicked: {e}")))??;
    h2.await
        .map_err(|e| DaemonError::Config(format!("handler 2 task panicked: {e}")))??;

    let cursor = dispatch.load_cursor("echo-bot").await?;
    assert_eq!(cursor, Some((keys.public_key().to_bech32()?, 42)));

    Ok(())
}

#[tokio::test]
async fn defer_prevents_cursor_advance() -> Result<(), Box<dyn std::error::Error>> {
    let keys = test_keys();
    let (dispatch, cm) = setup_dispatch(
        vec![bot_config("echo-bot", &keys, &["ReadMessages"])],
        None,
        None,
    )
    .await?;

    let bot_config_for_register = {
        let cm = cm.read().await;
        cm.get_bot_by_id("echo-bot").unwrap().config.clone()
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let handler_id = {
        let mut cm = cm.write().await;
        cm.handler_registry
            .register(
                ConnectionHandle::new(tx),
                vec!["echo-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["ReadMessages".to_string()],
                &[bot_config_for_register],
            )?
            .handler_id
    };

    let dispatch_for_h = Arc::clone(&dispatch);
    let handler_id_for_task = handler_id.clone();
    let h = tokio::spawn(async move {
        let msg = timeout(Duration::from_secs(1), rx.recv())
            .await
            .map_err(|_| DaemonError::Config("timeout waiting for handler event".into()))?
            .ok_or(DaemonError::Config("handler did not receive event".into()))?;
        let event = parse_event_notification(&msg).ok_or(DaemonError::Config(
            "handler received invalid event notification".into(),
        ))?;
        assert_eq!(event.event_id, "evt1");
        dispatch_for_h
            .handle_message(
                JsonRpcMessage::notification(
                    "handler.response",
                    Some(serde_json::json!({
                        "event_id": "evt1",
                        "action": "defer",
                    })),
                ),
                Some(&handler_id_for_task),
                None,
            )
            .await?;
        Ok::<(), DaemonError>(())
    });

    dispatch.dispatch_event(sample_event("echo-bot")).await?;

    h.await
        .map_err(|e| DaemonError::Config(format!("handler task panicked: {e}")))??;

    let cursor = dispatch.load_cursor("echo-bot").await?;
    assert_eq!(cursor, None);

    Ok(())
}

#[tokio::test]
async fn unauthorized_send_dm_returns_32006() -> Result<(), Box<dyn std::error::Error>> {
    let keys = test_keys();
    let (dispatch, cm) = setup_dispatch(
        vec![bot_config("echo-bot", &keys, &["ReadMessages"])],
        None,
        None,
    )
    .await?;

    let bot_config_for_register = {
        let cm = cm.read().await;
        cm.get_bot_by_id("echo-bot").unwrap().config.clone()
    };

    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let handler_id = {
        let mut cm = cm.write().await;
        cm.handler_registry
            .register(
                ConnectionHandle::new(tx),
                vec!["echo-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["ReadMessages".to_string()],
                &[bot_config_for_register],
            )?
            .handler_id
    };

    let req = JsonRpcMessage::request(
        1.into(),
        "agent.send_dm",
        Some(serde_json::json!({
            "bot_id": "echo-bot",
            "recipient": "npub1recipient",
            "content": "hello",
        })),
    );

    let resp = dispatch
        .handle_message(req, Some(&handler_id), None)
        .await?;
    match resp {
        Some(JsonRpcMessage::Error { error, .. }) => {
            assert_eq!(error.code, -32006);
        }
        _ => return Err(DaemonError::Config("expected unauthorized error".into()).into()),
    }

    Ok(())
}

#[tokio::test]
async fn rate_limit_rejects_excess_calls_with_32005() -> Result<(), Box<dyn std::error::Error>> {
    let keys = test_keys();
    let relay = MockRelay::start().await?;
    let nostr_client = NostrClient::new(vec![relay.url()]).await?;
    let (dispatch, cm) = setup_dispatch_with_client(
        vec![bot_config("echo-bot", &keys, &["ManageProfile"])],
        nostr_client,
        Some(RateLimiter::new(1.0, 1.0, 10.0, 10.0)),
        None,
    )
    .await?;

    let bot_config_for_register = {
        let cm = cm.read().await;
        cm.get_bot_by_id("echo-bot").unwrap().config.clone()
    };

    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let handler_id = {
        let mut cm = cm.write().await;
        cm.handler_registry
            .register(
                ConnectionHandle::new(tx),
                vec!["echo-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["ManageProfile".to_string()],
                &[bot_config_for_register],
            )?
            .handler_id
    };

    let req = JsonRpcMessage::request(
        1.into(),
        "agent.set_profile",
        Some(serde_json::json!({
            "bot_id": "echo-bot",
            "name": "Bot",
        })),
    );

    // First call consumes the single-token burst and succeeds.
    let first = dispatch
        .handle_message(req.clone(), Some(&handler_id), None)
        .await?;
    assert!(
        matches!(first, Some(JsonRpcMessage::Response { .. })),
        "expected set_profile success, got {first:?}"
    );

    // Second call is rate limited.
    let second = dispatch
        .handle_message(req, Some(&handler_id), None)
        .await?;
    match second {
        Some(JsonRpcMessage::Error { error, .. }) => {
            assert_eq!(error.code, -32005);
        }
        _ => return Err(DaemonError::Config("expected rate limit error".into()).into()),
    }

    Ok(())
}

#[tokio::test]
async fn rate_limit_increments_rate_limited_total_counter() -> Result<(), Box<dyn std::error::Error>>
{
    let keys = test_keys();
    let relay = MockRelay::start().await?;
    let nostr_client = NostrClient::new(vec![relay.url()]).await?;
    let (dispatch, cm) = setup_dispatch_with_client(
        vec![bot_config("echo-bot", &keys, &["ManageProfile"])],
        nostr_client,
        Some(RateLimiter::new(1.0, 1.0, 10.0, 10.0)),
        None,
    )
    .await?;

    let bot_config_for_register = {
        let cm = cm.read().await;
        cm.get_bot_by_id("echo-bot").unwrap().config.clone()
    };

    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let handler_id = {
        let mut cm = cm.write().await;
        cm.handler_registry
            .register(
                ConnectionHandle::new(tx),
                vec!["echo-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["ManageProfile".to_string()],
                &[bot_config_for_register],
            )?
            .handler_id
    };

    let req = set_profile_request(1, "echo-bot");

    let first = dispatch
        .handle_message(req.clone(), Some(&handler_id), None)
        .await?;
    assert_not_rate_limited(first, "first set_profile call");

    let before = dispatch.diagnostics.snapshot().rate_limited_total;
    assert_eq!(before, 0);

    let second = dispatch
        .handle_message(req, Some(&handler_id), None)
        .await?;
    assert_rate_limited(second, "second set_profile call");

    let after = dispatch.diagnostics.snapshot().rate_limited_total;
    assert_eq!(after, before + 1);

    Ok(())
}

fn set_profile_request(id: usize, bot_id: &str) -> JsonRpcMessage {
    JsonRpcMessage::request(
        serde_json::json!(id),
        "agent.set_profile",
        Some(serde_json::json!({
            "bot_id": bot_id,
            "name": "Bot",
        })),
    )
}

fn assert_rate_limited(resp: Option<JsonRpcMessage>, label: &str) {
    let code = match resp {
        Some(JsonRpcMessage::Error { ref error, .. }) => error.code,
        _ => i32::MIN,
    };
    assert_eq!(
        code, -32005,
        "{label} expected rate limit error, got {resp:?}"
    );
}

fn assert_not_rate_limited(resp: Option<JsonRpcMessage>, label: &str) {
    if let Some(JsonRpcMessage::Error { error, .. }) = resp {
        assert_ne!(
            error.code, -32005,
            "{label} should not be rate limited (got {error:?})"
        );
    }
}

#[test]
fn default_rate_limiter_constants_match_plan() {
    // Hard-coded sentinel: changing a production default without updating this
    // test and the expectations below is a breaking change to tested behavior.
    assert_eq!(DEFAULT_HANDLER_RATE, 10.0);
    assert_eq!(DEFAULT_HANDLER_BURST, 20.0);
    assert_eq!(DEFAULT_BOT_RATE, 20.0);
    assert_eq!(DEFAULT_BOT_BURST, 40.0);
}

#[tokio::test]
async fn default_rate_limit_rejects_11th_handler_call_within_one_second()
-> Result<(), Box<dyn std::error::Error>> {
    let keys = test_keys();
    let (dispatch, cm) = setup_dispatch(
        vec![bot_config("echo-bot", &keys, &["ManageProfile"])],
        None,
        None,
    )
    .await?;

    let bot_config_for_register = {
        let cm = cm.read().await;
        cm.get_bot_by_id("echo-bot").unwrap().config.clone()
    };

    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let handler_id = {
        let mut cm = cm.write().await;
        cm.handler_registry
            .register(
                ConnectionHandle::new(tx),
                vec!["echo-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["ManageProfile".to_string()],
                &[bot_config_for_register],
            )?
            .handler_id
    };

    // Burst of 20 calls is allowed by the per-handler bucket.
    for i in 0..20 {
        let resp = dispatch
            .handle_message(set_profile_request(i, "echo-bot"), Some(&handler_id), None)
            .await?;
        assert_not_rate_limited(resp, &format!("burst call {i}"));
    }

    // The 21st call immediately after the burst is rejected.
    let resp = dispatch
        .handle_message(set_profile_request(20, "echo-bot"), Some(&handler_id), None)
        .await?;
    assert_rate_limited(resp, "21st immediate call");

    // Wait one second for the per-handler rate to replenish 10 tokens.
    tokio::time::sleep(Duration::from_secs(1)).await;

    // The next 10 calls within the same one-second window are allowed.
    for i in 21..31 {
        let resp = dispatch
            .handle_message(set_profile_request(i, "echo-bot"), Some(&handler_id), None)
            .await?;
        assert_not_rate_limited(resp, &format!("replenished call {i}"));
    }

    // The 11th call within that one-second window exceeds the 10 ops/sec limit.
    let resp = dispatch
        .handle_message(set_profile_request(31, "echo-bot"), Some(&handler_id), None)
        .await?;
    assert_rate_limited(resp, "11th call within one second");

    Ok(())
}

#[tokio::test]
async fn default_rate_limit_enforces_bot_aggregate_with_two_handlers()
-> Result<(), Box<dyn std::error::Error>> {
    let keys = test_keys();
    let (dispatch, cm) = setup_dispatch(
        vec![bot_config("echo-bot", &keys, &["ManageProfile"])],
        None,
        None,
    )
    .await?;

    let bot_config_for_register = {
        let cm = cm.read().await;
        cm.get_bot_by_id("echo-bot").unwrap().config.clone()
    };

    let (tx1, _rx1) = tokio::sync::mpsc::channel(64);
    let (tx2, _rx2) = tokio::sync::mpsc::channel(64);
    let handler_id1 = {
        let mut cm = cm.write().await;
        cm.handler_registry
            .register(
                ConnectionHandle::new(tx1),
                vec!["echo-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["ManageProfile".to_string()],
                std::slice::from_ref(&bot_config_for_register),
            )?
            .handler_id
    };
    let handler_id2 = {
        let mut cm = cm.write().await;
        cm.handler_registry
            .register(
                ConnectionHandle::new(tx2),
                vec!["echo-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["ManageProfile".to_string()],
                std::slice::from_ref(&bot_config_for_register),
            )?
            .handler_id
    };

    // Two handlers together can burst up to the per-bot aggregate burst of 40.
    for i in 0..20 {
        let resp = dispatch
            .handle_message(
                set_profile_request(i * 2, "echo-bot"),
                Some(&handler_id1),
                None,
            )
            .await?;
        assert_not_rate_limited(resp, &format!("handler1 burst call {i}"));

        let resp = dispatch
            .handle_message(
                set_profile_request(i * 2 + 1, "echo-bot"),
                Some(&handler_id2),
                None,
            )
            .await?;
        assert_not_rate_limited(resp, &format!("handler2 burst call {i}"));
    }

    // The 41st call immediately after the shared burst is rejected.
    let resp = dispatch
        .handle_message(
            set_profile_request(40, "echo-bot"),
            Some(&handler_id1),
            None,
        )
        .await?;
    assert_rate_limited(resp, "41st immediate call (bot aggregate)");

    // Wait one second for the per-bot rate to replenish 20 tokens.
    tokio::time::sleep(Duration::from_secs(1)).await;

    // The next 20 calls within the same one-second window are allowed.
    for i in 0..10 {
        let resp = dispatch
            .handle_message(
                set_profile_request(41 + i * 2, "echo-bot"),
                Some(&handler_id1),
                None,
            )
            .await?;
        assert_not_rate_limited(resp, &format!("handler1 replenished call {i}"));

        let resp = dispatch
            .handle_message(
                set_profile_request(42 + i * 2, "echo-bot"),
                Some(&handler_id2),
                None,
            )
            .await?;
        assert_not_rate_limited(resp, &format!("handler2 replenished call {i}"));
    }

    // The 21st call within that one-second window exceeds the per-bot aggregate rate.
    let resp = dispatch
        .handle_message(
            set_profile_request(61, "echo-bot"),
            Some(&handler_id1),
            None,
        )
        .await?;
    assert_rate_limited(resp, "21st call within one second (bot aggregate)");

    Ok(())
}

#[tokio::test]
async fn slow_handler_does_not_block_fast_handler_and_cursor_advances()
-> Result<(), Box<dyn std::error::Error>> {
    let keys = test_keys();
    let timeout_duration = Duration::from_millis(200);
    let (dispatch, cm) = setup_dispatch(
        vec![bot_config(
            "echo-bot",
            &keys,
            &["ReadMessages", "SendMessages"],
        )],
        None,
        Some(timeout_duration),
    )
    .await?;

    let bot_config_for_register = {
        let cm = cm.read().await;
        cm.get_bot_by_id("echo-bot").unwrap().config.clone()
    };

    let (tx_slow, mut rx_slow) = tokio::sync::mpsc::channel(64);
    let (tx_fast, mut rx_fast) = tokio::sync::mpsc::channel(64);

    let (_handler_id_slow, handler_id_fast) = {
        let mut cm = cm.write().await;
        let id_slow = cm
            .handler_registry
            .register(
                ConnectionHandle::new(tx_slow),
                vec!["echo-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["ReadMessages".to_string(), "SendMessages".to_string()],
                std::slice::from_ref(&bot_config_for_register),
            )?
            .handler_id;
        let id_fast = cm
            .handler_registry
            .register(
                ConnectionHandle::new(tx_fast),
                vec!["echo-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["ReadMessages".to_string()],
                &[bot_config_for_register],
            )?
            .handler_id;
        (id_slow, id_fast)
    };

    let slow_h = tokio::spawn(async move {
        let msg = timeout(Duration::from_secs(1), rx_slow.recv())
            .await
            .map_err(|_| DaemonError::Config("timeout waiting for slow handler event".into()))?
            .ok_or(DaemonError::Config(
                "slow handler did not receive event".into(),
            ))?;
        let event = parse_event_notification(&msg).ok_or(DaemonError::Config(
            "slow handler received invalid event notification".into(),
        ))?;
        assert_eq!(event.event_id, "evt1");
        // Intentionally never respond so the dispatcher hits DISPATCH_TIMEOUT.
        Ok::<(), DaemonError>(())
    });

    let dispatch_for_fast = Arc::clone(&dispatch);
    let handler_id_fast_for_task = handler_id_fast.clone();
    let fast_h = tokio::spawn(async move {
        let msg = timeout(Duration::from_secs(1), rx_fast.recv())
            .await
            .map_err(|_| DaemonError::Config("timeout waiting for fast handler event".into()))?
            .ok_or(DaemonError::Config(
                "fast handler did not receive event".into(),
            ))?;
        let event = parse_event_notification(&msg).ok_or(DaemonError::Config(
            "fast handler received invalid event notification".into(),
        ))?;
        assert_eq!(event.event_id, "evt1");
        dispatch_for_fast
            .handle_message(
                JsonRpcMessage::notification(
                    "handler.response",
                    Some(serde_json::json!({
                        "event_id": "evt1",
                        "action": "ack",
                    })),
                ),
                Some(&handler_id_fast_for_task),
                None,
            )
            .await?;
        Ok::<(), DaemonError>(())
    });

    let start = Instant::now();
    dispatch.dispatch_event(sample_event("echo-bot")).await?;
    let elapsed = start.elapsed();

    fast_h
        .await
        .map_err(|e| DaemonError::Config(format!("fast handler task panicked: {e}")))??;
    slow_h
        .await
        .map_err(|e| DaemonError::Config(format!("slow handler task panicked: {e}")))??;

    // The dispatcher must wait the full dispatch timeout for the slow handler.
    assert!(
        elapsed >= timeout_duration.saturating_sub(Duration::from_millis(20)),
        "dispatch returned too early: {:?}",
        elapsed
    );

    // Cursor advances even though the slow handler never responded.
    let cursor = dispatch.load_cursor("echo-bot").await?;
    assert_eq!(cursor, Some((keys.public_key().to_bech32()?, 42)));

    Ok(())
}

#[tokio::test]
async fn disconnected_handler_unregistered_and_registry_cleaned()
-> Result<(), Box<dyn std::error::Error>> {
    let keys = test_keys();
    let (dispatch, cm) = setup_dispatch(
        vec![bot_config(
            "echo-bot",
            &keys,
            &["ReadMessages", "SendMessages"],
        )],
        None,
        None,
    )
    .await?;

    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let register_req = JsonRpcMessage::request(
        1.into(),
        "handler.register",
        Some(serde_json::json!({
            "bot_ids": ["echo-bot"],
            "event_types": ["dm_received"],
            "capabilities": ["ReadMessages"],
        })),
    );

    let resp = dispatch
        .handle_message(register_req, None, Some(ConnectionHandle::new(tx)))
        .await?
        .ok_or(DaemonError::Config(
            "handler.register returned no response".into(),
        ))?;
    let handler_id = match resp {
        JsonRpcMessage::Response {
            result: Some(r), ..
        } => r
            .get("handler_id")
            .and_then(|v| v.as_str())
            .ok_or(DaemonError::Config(
                "handler.register response missing handler_id".into(),
            ))?
            .to_string(),
        _ => return Err(DaemonError::Config("expected handler.register response".into()).into()),
    };

    {
        let cm = cm.read().await;
        assert!(cm.handler_registry.get_handler(&handler_id).is_some());
    }
    assert_eq!(dispatch.registered_handler_count(), 1);

    let unregister_req = JsonRpcMessage::request(
        2.into(),
        "agent.unregister_handler",
        Some(serde_json::json!({
            "handler_id": handler_id,
        })),
    );
    let resp = dispatch
        .handle_message(unregister_req, Some(&handler_id), None)
        .await?;
    match resp {
        Some(JsonRpcMessage::Response {
            result: Some(r), ..
        }) => {
            assert_eq!(r, serde_json::json!({ "unregistered": true }));
        }
        _ => return Err(DaemonError::Config("expected unregister response".into()).into()),
    }

    {
        let cm = cm.read().await;
        assert!(
            cm.handler_registry.get_handler(&handler_id).is_none(),
            "handler should be removed from registry after unregister"
        );
    }
    assert_eq!(dispatch.registered_handler_count(), 0);

    // A call from the now-unregistered handler is rejected as no longer registered.
    let send_req = JsonRpcMessage::request(
        3.into(),
        "agent.send_dm",
        Some(serde_json::json!({
            "bot_id": "echo-bot",
            "recipient": "npub1recipient",
            "content": "hello",
        })),
    );
    let resp = dispatch
        .handle_message(send_req, Some(&handler_id), None)
        .await?;
    match resp {
        Some(JsonRpcMessage::Error { error, .. }) => {
            assert_eq!(error.code, -32001);
        }
        _ => return Err(DaemonError::Config("expected HandlerNotRegistered error".into()).into()),
    }

    Ok(())
}

#[tokio::test]
async fn unregister_without_registered_connection_returns_32001()
-> Result<(), Box<dyn std::error::Error>> {
    let keys = test_keys();
    let (dispatch, _cm) = setup_dispatch(
        vec![bot_config("echo-bot", &keys, &["ReadMessages"])],
        None,
        None,
    )
    .await?;

    let req = JsonRpcMessage::request(
        1.into(),
        "agent.unregister_handler",
        Some(serde_json::json!({
            "handler_id": "nonexistent-handler",
        })),
    );
    let resp = dispatch.handle_message(req, None, None).await?;
    match resp {
        Some(JsonRpcMessage::Error { error, .. }) => {
            assert_eq!(
                error.code, -32001,
                "expected HandlerNotRegistered when caller is not registered"
            );
        }
        _ => return Err(DaemonError::Config("expected HandlerNotRegistered error".into()).into()),
    }

    Ok(())
}

#[tokio::test]
async fn unauthorized_set_profile_returns_32006() -> Result<(), Box<dyn std::error::Error>> {
    let keys = test_keys();
    let (dispatch, cm) = setup_dispatch(
        vec![bot_config("echo-bot", &keys, &["ReadMessages"])],
        None,
        None,
    )
    .await?;

    let bot_config_for_register = {
        let cm = cm.read().await;
        cm.get_bot_by_id("echo-bot").unwrap().config.clone()
    };

    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let handler_id = {
        let mut cm = cm.write().await;
        cm.handler_registry
            .register(
                ConnectionHandle::new(tx),
                vec!["echo-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["ReadMessages".to_string()],
                &[bot_config_for_register],
            )?
            .handler_id
    };

    let req = JsonRpcMessage::request(
        1.into(),
        "agent.set_profile",
        Some(serde_json::json!({
            "bot_id": "echo-bot",
            "name": "Bot",
        })),
    );

    let resp = dispatch
        .handle_message(req, Some(&handler_id), None)
        .await?;
    match resp {
        Some(JsonRpcMessage::Error { error, .. }) => {
            assert_eq!(error.code, -32006);
        }
        _ => return Err(DaemonError::Config("expected unauthorized error".into()).into()),
    }

    Ok(())
}

#[tokio::test]
async fn authorized_set_profile_publishes_kind_0_event() -> Result<(), Box<dyn std::error::Error>> {
    let relay = MockRelay::start().await?;
    let keys = test_keys();
    let nostr_client = NostrClient::new(vec![relay.url()]).await?;
    let (dispatch, cm) = setup_dispatch_with_client(
        vec![bot_config("profile-bot", &keys, &["ManageProfile"])],
        nostr_client,
        None,
        None,
    )
    .await?;

    let bot_config_for_register = {
        let cm = cm.read().await;
        cm.get_bot_by_id("profile-bot").unwrap().config.clone()
    };

    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let handler_id = {
        let mut cm = cm.write().await;
        cm.handler_registry
            .register(
                ConnectionHandle::new(tx),
                vec!["profile-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["ManageProfile".to_string()],
                &[bot_config_for_register],
            )?
            .handler_id
    };

    let req = JsonRpcMessage::request(
        1.into(),
        "agent.set_profile",
        Some(serde_json::json!({
            "bot_id": "profile-bot",
            "name": "Updated Name",
            "about": "Updated about",
            "picture": "https://example.com/pic.png",
        })),
    );

    let resp = dispatch
        .handle_message(req, Some(&handler_id), None)
        .await?;
    let event_id = match resp {
        Some(JsonRpcMessage::Response {
            result: Some(Value::String(id)),
            ..
        }) => id,
        _ => {
            return Err(
                DaemonError::Config("expected set_profile success with event id".into()).into(),
            );
        }
    };
    assert_eq!(event_id.len(), 64);

    let pubkey = keys.public_key();
    let events = relay
        .wait_for_event(
            |e| e.kind == Kind::Metadata && e.pubkey == pubkey,
            Duration::from_secs(2),
        )
        .await?;
    let event = events
        .into_iter()
        .find(|e| e.kind == Kind::Metadata && e.pubkey == pubkey)
        .ok_or("metadata event not found")?;

    assert_eq!(event.kind, Kind::Metadata);
    assert!(event.verify_signature());
    assert_eq!(event.id.to_hex(), event_id);

    let metadata: serde_json::Value = serde_json::from_str(&event.content)?;
    assert_eq!(metadata["name"], "Updated Name");
    assert_eq!(metadata["about"], "Updated about");
    assert_eq!(metadata["picture"], "https://example.com/pic.png");

    relay.stop().await;
    Ok(())
}

fn agent_error_request(
    id: usize,
    bot_id: &str,
    code: Option<&str>,
    data: Option<Value>,
) -> JsonRpcMessage {
    let mut params = serde_json::json!({
        "bot_id": bot_id,
        "message": "handler error",
    });
    if let Some(code) = code {
        params["code"] = serde_json::json!(code);
    }
    if let Some(data) = data {
        params["data"] = data;
    }
    JsonRpcMessage::request(serde_json::json!(id), "agent.error", Some(params))
}

fn agent_error_notification(
    bot_id: &str,
    code: Option<&str>,
    data: Option<Value>,
) -> JsonRpcMessage {
    let mut params = serde_json::json!({
        "bot_id": bot_id,
        "message": "handler error",
    });
    if let Some(code) = code {
        params["code"] = serde_json::json!(code);
    }
    if let Some(data) = data {
        params["data"] = data;
    }
    JsonRpcMessage::notification("agent.error", Some(params))
}

#[tokio::test]
async fn rate_limit_rejects_excess_agent_error_notifications_with_32005()
-> Result<(), Box<dyn std::error::Error>> {
    let keys = test_keys();
    let (dispatch, cm) = setup_dispatch(
        vec![bot_config("echo-bot", &keys, &["ReadMessages"])],
        Some(RateLimiter::new(1.0, 1.0, 10.0, 10.0)),
        None,
    )
    .await?;

    let bot_config_for_register = {
        let cm = cm.read().await;
        cm.get_bot_by_id("echo-bot").unwrap().config.clone()
    };

    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let handler_id = {
        let mut cm = cm.write().await;
        cm.handler_registry
            .register(
                ConnectionHandle::new(tx),
                vec!["echo-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["ReadMessages".to_string()],
                &[bot_config_for_register],
            )?
            .handler_id
    };

    // First notification consumes the single-token burst.
    let first = dispatch
        .handle_message(
            agent_error_request(1, "echo-bot", None, None),
            Some(&handler_id),
            None,
        )
        .await?;
    assert!(
        matches!(first, Some(JsonRpcMessage::Response { .. })),
        "expected agent.error success, got {first:?}"
    );

    // Second call is rate limited.
    let second = dispatch
        .handle_message(
            agent_error_request(2, "echo-bot", None, None),
            Some(&handler_id),
            None,
        )
        .await?;
    assert_rate_limited(second, "agent.error second call");

    Ok(())
}

#[tokio::test]
async fn agent_error_preserves_code_and_data_in_diagnostics()
-> Result<(), Box<dyn std::error::Error>> {
    let keys = test_keys();
    let (dispatch, cm) = setup_dispatch(
        vec![bot_config("echo-bot", &keys, &["ReadMessages"])],
        None,
        None,
    )
    .await?;

    let bot_config_for_register = {
        let cm = cm.read().await;
        cm.get_bot_by_id("echo-bot").unwrap().config.clone()
    };

    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let handler_id = {
        let mut cm = cm.write().await;
        cm.handler_registry
            .register(
                ConnectionHandle::new(tx),
                vec!["echo-bot".to_string()],
                vec!["dm_received".to_string()],
                vec!["ReadMessages".to_string()],
                &[bot_config_for_register],
            )?
            .handler_id
    };

    let data = serde_json::json!({
        "context": "dm parsing",
        "secret": "nsec1do-not-leak-this",
    });

    dispatch
        .handle_message(
            agent_error_notification("echo-bot", Some("E_DM_PARSE"), Some(data)),
            Some(&handler_id),
            None,
        )
        .await?;

    let metrics_resp = dispatch
        .handle_message(
            JsonRpcMessage::request(serde_json::json!(42), "agent.metrics", None),
            None,
            None,
        )
        .await?;
    let metrics = match metrics_resp {
        Some(JsonRpcMessage::Response {
            result: Some(r), ..
        }) => r,
        other => return Err(format!("expected agent.metrics response, got {other:?}").into()),
    };
    // `agent.metrics` must match `schemas/metrics.json`: counters plus an
    // optional per-bot health array, but no status or errors (those live in
    // the diagnostics snapshot / flushed report).
    assert!(
        metrics["errors"].is_null(),
        "agent.metrics response must not contain errors, got {metrics}"
    );
    assert!(
        metrics["status"].is_null(),
        "agent.metrics response must not contain status, got {metrics}"
    );
    assert!(
        metrics["bots"].is_null() || metrics["bots"].as_array().is_some(),
        "agent.metrics bots field must be null or an array, got {metrics}"
    );
    assert_eq!(
        metrics["events_received_total"],
        serde_json::json!(0),
        "agent.metrics must expose counter fields, got {metrics}"
    );

    // The redacted error is still retained in the diagnostics snapshot and
    // flushed report; that path is covered in `tests/diagnostics.rs`.
    let snap = dispatch.diagnostics().snapshot();
    let record = snap
        .errors
        .iter()
        .find(|e| e.code == "E_DM_PARSE")
        .expect("expected error record with code E_DM_PARSE");
    assert_eq!(record.message, "handler error");
    let data_str = record.data.as_ref().expect("expected data to be preserved");
    assert!(
        data_str.contains("dm parsing"),
        "data should retain context, got {data_str}"
    );
    assert!(
        !data_str.contains("nsec1do-not-leak-this"),
        "data should be redacted, got {data_str}"
    );
    assert!(
        data_str.contains("[REDACTED]"),
        "data should show redaction marker, got {data_str}"
    );

    Ok(())
}

#[tokio::test]
async fn periodic_metrics_notification_reaches_registered_handler()
-> Result<(), Box<dyn std::error::Error>> {
    let keys = test_keys();
    let (dispatch, _cm) = setup_dispatch(
        vec![bot_config("echo-bot", &keys, &["ReadMessages"])],
        None,
        None,
    )
    .await?;

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let register_req = JsonRpcMessage::request(
        serde_json::json!(1),
        "handler.register",
        Some(serde_json::json!({
            "bot_ids": ["echo-bot"],
            "event_types": ["dm_received"],
            "capabilities": ["ReadMessages"],
        })),
    );
    let _ = dispatch
        .handle_message(register_req, None, Some(ConnectionHandle::new(tx)))
        .await?;

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let metrics_handle = dispatch
        .clone()
        .spawn_periodic_metrics(Duration::from_millis(100), shutdown_rx);

    let mut count = 0;
    while count < 3 {
        let msg = timeout(Duration::from_secs(2), rx.recv())
            .await?
            .ok_or("handler channel closed")?;
        if let Some(metrics) = parse_metrics_notification(&msg) {
            count += 1;
            assert_eq!(
                metrics.handlers_registered,
                Some(1),
                "metrics should report one registered handler"
            );
        }
    }

    let _ = shutdown_tx.send(true);
    let _ = metrics_handle.await;

    Ok(())
}

#[tokio::test]
async fn handler_register_returns_server_assigned_id_and_reconnect_token()
-> Result<(), Box<dyn std::error::Error>> {
    let keys = test_keys();
    let (dispatch, _cm) = setup_dispatch(
        vec![bot_config("echo-bot", &keys, &["ReadMessages"])],
        None,
        None,
    )
    .await?;

    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let register_req = JsonRpcMessage::request(
        1.into(),
        "handler.register",
        Some(serde_json::json!({
            "bot_ids": ["echo-bot"],
            "event_types": ["dm_received"],
            "capabilities": ["ReadMessages"],
        })),
    );

    let resp = dispatch
        .handle_message(register_req, None, Some(ConnectionHandle::new(tx)))
        .await?
        .ok_or(DaemonError::Config(
            "handler.register returned no response".into(),
        ))?;
    let (handler_id, reconnect_token) = match resp {
        JsonRpcMessage::Response {
            result: Some(r), ..
        } => {
            let handler_id = r
                .get("handler_id")
                .and_then(|v| v.as_str())
                .ok_or(DaemonError::Config("missing handler_id".into()))?;
            let reconnect_token = r
                .get("reconnect_token")
                .and_then(|v| v.as_str())
                .ok_or(DaemonError::Config("missing reconnect_token".into()))?;
            assert!(!handler_id.is_empty());
            assert!(!reconnect_token.is_empty());
            assert!(
                reconnect_token.len() >= 64,
                "reconnect_token should be at least 256 bits hex encoded"
            );
            (handler_id.to_string(), reconnect_token.to_string())
        }
        _ => return Err(DaemonError::Config("expected handler.register response".into()).into()),
    };

    // Reconnect with the same token should reuse the handler id.
    {
        let mut cm = _cm.write().await;
        cm.handler_registry
            .get_handler_mut(&handler_id)
            .unwrap()
            .disconnect();
    }
    let (tx2, _rx2) = tokio::sync::mpsc::channel(64);
    let reconnect_req = JsonRpcMessage::request(
        3.into(),
        "handler.reconnect",
        Some(serde_json::json!({
            "handler_id": handler_id,
            "reconnect_token": reconnect_token,
            "bot_ids": ["echo-bot"],
            "event_types": ["dm_received"],
            "capabilities": ["ReadMessages"],
        })),
    );
    let resp = dispatch
        .handle_message(reconnect_req, None, Some(ConnectionHandle::new(tx2)))
        .await?
        .ok_or(DaemonError::Config(
            "handler.reconnect returned no response".into(),
        ))?;
    match resp {
        JsonRpcMessage::Response {
            result: Some(r), ..
        } => {
            assert_eq!(
                r.get("handler_id").and_then(|v| v.as_str()),
                Some(handler_id.as_str())
            );
        }
        _ => return Err(DaemonError::Config("expected handler.reconnect response".into()).into()),
    }

    Ok(())
}

#[tokio::test]
async fn handler_reconnect_rejects_invalid_token() -> Result<(), Box<dyn std::error::Error>> {
    let keys = test_keys();
    let (dispatch, _cm) = setup_dispatch(
        vec![bot_config("echo-bot", &keys, &["ReadMessages"])],
        None,
        None,
    )
    .await?;

    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let register_req = JsonRpcMessage::request(
        1.into(),
        "handler.register",
        Some(serde_json::json!({
            "bot_ids": ["echo-bot"],
            "event_types": ["dm_received"],
            "capabilities": ["ReadMessages"],
        })),
    );
    let resp = dispatch
        .handle_message(register_req, None, Some(ConnectionHandle::new(tx)))
        .await?
        .ok_or(DaemonError::Config(
            "handler.register returned no response".into(),
        ))?;
    let handler_id = match resp {
        JsonRpcMessage::Response {
            result: Some(r), ..
        } => r
            .get("handler_id")
            .and_then(|v| v.as_str())
            .ok_or(DaemonError::Config("missing handler_id".into()))?
            .to_string(),
        _ => return Err(DaemonError::Config("expected handler.register response".into()).into()),
    };

    // Drop the connection first so reconnect is allowed.
    {
        let mut cm = _cm.write().await;
        cm.handler_registry
            .get_handler_mut(&handler_id)
            .unwrap()
            .disconnect();
    }

    let (tx2, _rx2) = tokio::sync::mpsc::channel(64);
    let reconnect_req = JsonRpcMessage::request(
        2.into(),
        "handler.reconnect",
        Some(serde_json::json!({
            "handler_id": handler_id,
            "reconnect_token": "deadbeef-dead-beef-dead-beefdeadbeef",
            "bot_ids": ["echo-bot"],
            "event_types": ["dm_received"],
            "capabilities": ["ReadMessages"],
        })),
    );
    let resp = dispatch
        .handle_message(reconnect_req, None, Some(ConnectionHandle::new(tx2)))
        .await?;
    match resp {
        Some(JsonRpcMessage::Error { error, .. }) => {
            assert_eq!(error.code, -32008, "expected InvalidReconnectToken");
        }
        _ => return Err(DaemonError::Config("expected InvalidReconnectToken error".into()).into()),
    }

    Ok(())
}

#[tokio::test]
async fn handler_reconnect_rejects_live_takeover() -> Result<(), Box<dyn std::error::Error>> {
    let keys = test_keys();
    let (dispatch, _cm) = setup_dispatch(
        vec![bot_config("echo-bot", &keys, &["ReadMessages"])],
        None,
        None,
    )
    .await?;

    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let register_req = JsonRpcMessage::request(
        1.into(),
        "handler.register",
        Some(serde_json::json!({
            "bot_ids": ["echo-bot"],
            "event_types": ["dm_received"],
            "capabilities": ["ReadMessages"],
        })),
    );
    let resp = dispatch
        .handle_message(register_req, None, Some(ConnectionHandle::new(tx)))
        .await?
        .ok_or(DaemonError::Config(
            "handler.register returned no response".into(),
        ))?;
    let handler_id = match &resp {
        JsonRpcMessage::Response {
            result: Some(r), ..
        } => r
            .get("handler_id")
            .and_then(|v| v.as_str())
            .ok_or(DaemonError::Config("missing handler_id".into()))?
            .to_string(),
        _ => return Err(DaemonError::Config("expected handler.register response".into()).into()),
    };
    let reconnect_token = match &resp {
        JsonRpcMessage::Response {
            result: Some(r), ..
        } => r
            .get("reconnect_token")
            .and_then(|v| v.as_str())
            .ok_or(DaemonError::Config("missing reconnect_token".into()))?
            .to_string(),
        _ => return Err(DaemonError::Config("expected handler.register response".into()).into()),
    };

    // Attempting to reconnect while the handler is still connected is a takeover.
    let (tx2, _rx2) = tokio::sync::mpsc::channel(64);
    let reconnect_req = JsonRpcMessage::request(
        2.into(),
        "handler.reconnect",
        Some(serde_json::json!({
            "handler_id": handler_id,
            "reconnect_token": reconnect_token,
            "bot_ids": ["echo-bot"],
            "event_types": ["dm_received"],
            "capabilities": ["ReadMessages"],
        })),
    );
    let resp = dispatch
        .handle_message(reconnect_req, None, Some(ConnectionHandle::new(tx2)))
        .await?;
    match resp {
        Some(JsonRpcMessage::Error { error, .. }) => {
            assert_eq!(error.code, -32007, "expected HandlerAlreadyConnected");
        }
        _ => {
            return Err(
                DaemonError::Config("expected HandlerAlreadyConnected error".into()).into(),
            );
        }
    }

    Ok(())
}

#[tokio::test]
async fn agent_unregister_handler_authorizes_self_and_admin()
-> Result<(), Box<dyn std::error::Error>> {
    let self_keys = test_keys();
    let admin_keys = nostr::Keys::generate();
    let other_keys = nostr::Keys::generate();
    let (dispatch, cm) = setup_dispatch(
        vec![
            bot_config("self-bot", &self_keys, &["ReadMessages"]),
            bot_config("admin-bot", &admin_keys, &["ReadMessages", "Admin"]),
            bot_config("other-bot", &other_keys, &["ReadMessages"]),
        ],
        None,
        None,
    )
    .await?;

    // Register three handlers.
    let mut handlers = Vec::new();
    for (name, caps) in [
        ("self-bot", vec!["ReadMessages"]),
        ("admin-bot", vec!["ReadMessages", "Admin"]),
        ("other-bot", vec!["ReadMessages"]),
    ] {
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let caps_str: Vec<&str> = caps.to_vec();
        let req = JsonRpcMessage::request(
            (handlers.len() + 1).into(),
            "handler.register",
            Some(serde_json::json!({
                "bot_ids": [name],
                "event_types": ["dm_received"],
                "capabilities": caps_str,
            })),
        );
        let resp = dispatch
            .handle_message(req, None, Some(ConnectionHandle::new(tx)))
            .await?
            .ok_or(DaemonError::Config(
                "handler.register returned no response".into(),
            ))?;
        let handler_id = match resp {
            JsonRpcMessage::Response {
                result: Some(r), ..
            } => r
                .get("handler_id")
                .and_then(|v| v.as_str())
                .ok_or(DaemonError::Config("missing handler_id".into()))?
                .to_string(),
            _ => {
                return Err(
                    DaemonError::Config("expected handler.register response".into()).into(),
                );
            }
        };
        handlers.push(handler_id);
    }

    // A non-admin handler cannot unregister another handler.
    let unauthorized_req = JsonRpcMessage::request(
        10.into(),
        "agent.unregister_handler",
        Some(serde_json::json!({
            "handler_id": &handlers[2],
        })),
    );
    let resp = dispatch
        .handle_message(unauthorized_req, Some(&handlers[0]), None)
        .await?;
    match resp {
        Some(JsonRpcMessage::Error { error, .. }) => {
            assert_eq!(error.code, -32006, "expected UnauthorizedBot");
        }
        _ => return Err(DaemonError::Config("expected UnauthorizedBot error".into()).into()),
    }

    // An admin handler can unregister another handler.
    let admin_req = JsonRpcMessage::request(
        11.into(),
        "agent.unregister_handler",
        Some(serde_json::json!({
            "handler_id": &handlers[2],
        })),
    );
    let resp = dispatch
        .handle_message(admin_req, Some(&handlers[1]), None)
        .await?;
    match resp {
        Some(JsonRpcMessage::Response {
            result: Some(r), ..
        }) => {
            assert_eq!(r, serde_json::json!({ "unregistered": true }));
        }
        _ => return Err(DaemonError::Config("expected admin unregister response".into()).into()),
    }

    // A handler can unregister itself.
    let self_req = JsonRpcMessage::request(
        12.into(),
        "agent.unregister_handler",
        Some(serde_json::json!({
            "handler_id": &handlers[0],
        })),
    );
    let resp = dispatch
        .handle_message(self_req, Some(&handlers[0]), None)
        .await?;
    match resp {
        Some(JsonRpcMessage::Response {
            result: Some(r), ..
        }) => {
            assert_eq!(r, serde_json::json!({ "unregistered": true }));
        }
        _ => return Err(DaemonError::Config("expected self unregister response".into()).into()),
    }

    // Both unregistered handlers are gone.
    let cm = cm.read().await;
    assert!(cm.handler_registry.get_handler(&handlers[0]).is_none());
    assert!(cm.handler_registry.get_handler(&handlers[2]).is_none());
    assert!(cm.handler_registry.get_handler(&handlers[1]).is_some());
    drop(cm);

    Ok(())
}
