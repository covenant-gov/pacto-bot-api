use std::sync::Arc;
use std::time::Duration;

use nostr::ToBech32;
use pacto_bot_api::client_manager::ClientManager;
use pacto_bot_api::config::{BotConfig, DaemonConfig, GlobalDaemonConfig, SigningConfig};
use pacto_bot_api::db::Database;
use pacto_bot_api::diagnostics::Diagnostics;
use pacto_bot_api::dispatch::{Dispatch, RateLimiter};
use pacto_bot_api::errors::DaemonError;
use pacto_bot_api::events::{AgentEvent, EventType};
use pacto_bot_api::handlers::ConnectionHandle;
use pacto_bot_api::nostr::NostrClient;
use pacto_bot_api::transport::protocol::JsonRpcMessage;
use secrecy::SecretString;
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
    }
}

async fn setup_dispatch(
    bot_configs: Vec<BotConfig>,
    rate_limiter: Option<RateLimiter>,
) -> Result<(Arc<Dispatch>, Arc<RwLock<ClientManager>>), Box<dyn std::error::Error + Send + Sync>> {
    let config = DaemonConfig {
        daemon: GlobalDaemonConfig::default(),
        bots: bot_configs,
    };
    let nostr_client = NostrClient::new(vec![]).await?;
    let cm = Arc::new(RwLock::new(ClientManager::new(config, nostr_client)?));
    let dir = tempdir()?;
    let db = Database::open(dir.path().join("test.db").as_path())?;
    let diagnostics = Diagnostics::new();
    let dispatch = match rate_limiter {
        Some(limiter) => Dispatch::with_rate_limiter(cm.clone(), db, diagnostics, limiter),
        None => Dispatch::new(cm.clone(), db, diagnostics),
    };
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

#[tokio::test]
async fn fan_out_delivers_event_and_advances_cursor()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let keys = test_keys();
    let (dispatch, cm) = setup_dispatch(
        vec![bot_config(
            "echo-bot",
            &keys,
            &["ReadMessages", "SendMessages"],
        )],
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
        let id1 = cm.handler_registry.register(
            ConnectionHandle::new(tx1),
            vec!["echo-bot".to_string()],
            vec!["dm_received".to_string()],
            vec!["ReadMessages".to_string(), "SendMessages".to_string()],
            std::slice::from_ref(&bot_config_for_register),
        )?;
        let id2 = cm.handler_registry.register(
            ConnectionHandle::new(tx2),
            vec!["echo-bot".to_string()],
            vec!["dm_received".to_string()],
            vec!["ReadMessages".to_string()],
            &[bot_config_for_register],
        )?;
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

    let cursor = dispatch.load_cursor("echo-bot")?;
    assert_eq!(cursor, Some((keys.public_key().to_bech32()?, 42)));

    Ok(())
}

#[tokio::test]
async fn defer_prevents_cursor_advance() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let keys = test_keys();
    let (dispatch, cm) =
        setup_dispatch(vec![bot_config("echo-bot", &keys, &["ReadMessages"])], None).await?;

    let bot_config_for_register = {
        let cm = cm.read().await;
        cm.get_bot_by_id("echo-bot").unwrap().config.clone()
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let handler_id = {
        let mut cm = cm.write().await;
        cm.handler_registry.register(
            ConnectionHandle::new(tx),
            vec!["echo-bot".to_string()],
            vec!["dm_received".to_string()],
            vec!["ReadMessages".to_string()],
            &[bot_config_for_register],
        )?
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

    let cursor = dispatch.load_cursor("echo-bot")?;
    assert_eq!(cursor, None);

    Ok(())
}

#[tokio::test]
async fn unauthorized_send_dm_returns_32006() -> Result<(), Box<dyn std::error::Error + Send + Sync>>
{
    let keys = test_keys();
    let (dispatch, cm) =
        setup_dispatch(vec![bot_config("echo-bot", &keys, &["ReadMessages"])], None).await?;

    let bot_config_for_register = {
        let cm = cm.read().await;
        cm.get_bot_by_id("echo-bot").unwrap().config.clone()
    };

    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let handler_id = {
        let mut cm = cm.write().await;
        cm.handler_registry.register(
            ConnectionHandle::new(tx),
            vec!["echo-bot".to_string()],
            vec!["dm_received".to_string()],
            vec!["ReadMessages".to_string()],
            &[bot_config_for_register],
        )?
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
async fn rate_limit_rejects_excess_calls_with_32005()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let keys = test_keys();
    let (dispatch, cm) = setup_dispatch(
        vec![bot_config("echo-bot", &keys, &["ManageProfile"])],
        Some(RateLimiter::new(1.0, 1.0, 10.0, 10.0)),
    )
    .await?;

    let bot_config_for_register = {
        let cm = cm.read().await;
        cm.get_bot_by_id("echo-bot").unwrap().config.clone()
    };

    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let handler_id = {
        let mut cm = cm.write().await;
        cm.handler_registry.register(
            ConnectionHandle::new(tx),
            vec!["echo-bot".to_string()],
            vec!["dm_received".to_string()],
            vec!["ManageProfile".to_string()],
            &[bot_config_for_register],
        )?
    };

    let req = JsonRpcMessage::request(
        1.into(),
        "agent.set_profile",
        Some(serde_json::json!({
            "bot_id": "echo-bot",
            "name": "Bot",
        })),
    );

    // First call consumes the single-token burst; it is authorized but not implemented yet.
    let first = dispatch
        .handle_message(req.clone(), Some(&handler_id), None)
        .await?;
    assert!(matches!(first, Some(JsonRpcMessage::Error { .. })));

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
