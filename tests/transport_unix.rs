mod common;
use nostr::ToBech32;
/// req(R1, R3, R28)
use pacto_bot_api::client_manager::ClientManager;
use pacto_bot_api::config::{BotConfig, DaemonConfig, GlobalDaemonConfig, SigningConfig};
use pacto_bot_api::db::{Database, Db};
use pacto_bot_api::diagnostics::{DaemonStatus, Diagnostics};
use pacto_bot_api::dispatch::Dispatch;
use pacto_bot_api::nostr::NostrClient;
use pacto_bot_api::transport::protocol::{JsonRpcMessage, serialize_message};
use pacto_bot_api::transport::{message_handler, unix::UnixTransport};
use secrecy::SecretString;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufStream};
use tokio::net::UnixStream;
use tokio::sync::{RwLock, oneshot};

fn test_socket_dir() -> Result<PathBuf, std::io::Error> {
    let base = PathBuf::from("target/transport-tests");
    let dir = base.join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn dummy_disconnect_sender() -> tokio::sync::mpsc::Sender<Option<String>> {
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    tx
}

fn echo_handler() -> pacto_bot_api::transport::MessageHandler {
    message_handler(|msg, _connection, _handler_id| async move {
        let id = msg.id().cloned().unwrap_or(Value::Null);
        Ok(Some(JsonRpcMessage::response(
            id,
            Some(Value::String("pong".into())),
        )))
    })
}

async fn setup_dispatch() -> Result<(Arc<Dispatch>, tempfile::TempDir), Box<dyn std::error::Error>>
{
    let keys = nostr::Keys::generate();
    let bot = BotConfig {
        id: "echo-bot".to_string(),
        npub: keys.public_key().to_bech32()?,
        signing: SigningConfig::Nsec {
            nsec: SecretString::new(keys.secret_key().to_bech32()?.into()),
        },
        relays: vec![],
        capabilities: vec!["ReadMessages".to_string(), "SendMessages".to_string()],
        ..Default::default()
    };
    let config = DaemonConfig {
        daemon: GlobalDaemonConfig::default(),
        bots: vec![bot],
    };
    let dir = common::tempdir()?;
    let nostr_client = NostrClient::new(vec![]).await?;
    let cm = Arc::new(RwLock::new(
        ClientManager::new(dir.path(), config, nostr_client).await?,
    ));
    let db = Db::open(dir.path().join("agent.db").as_path()).await?;
    let db = Db::open(dir.path().join("agent.db").as_path()).await?;
    let diagnostics = Diagnostics::new();
    let dispatch = Arc::new(Dispatch::new(cm, db, diagnostics));
    Ok((dispatch, dir))
}

#[tokio::test]
async fn unix_transport_unregisters_handler_on_disconnect() -> Result<(), Box<dyn std::error::Error>>
{
    #[cfg(unix)]
    {
        let dir = test_socket_dir()?;
        let path = dir.join("unregister.sock");
        let (dispatch, _db_dir) = setup_dispatch().await?;
        let db_path = _db_dir.path().join("agent.db");
        let dispatch_for_handler = dispatch.clone();
        let dispatch_for_disconnect = dispatch.clone();

        let (disconnect_tx, mut disconnect_rx) = tokio::sync::mpsc::channel::<Option<String>>(16);
        tokio::spawn(async move {
            while let Some(maybe_id) = disconnect_rx.recv().await {
                if let Some(handler_id) = maybe_id {
                    match dispatch_for_disconnect
                        .unregister_handler(&handler_id)
                        .await
                    {
                        Ok(()) => {}
                        Err(e) => eprintln!("unregister error: {e}"),
                    }
                }
            }
        });

        let handler = message_handler(move |msg, connection, handler_id| {
            let dispatch = dispatch_for_handler.clone();
            async move {
                dispatch
                    .handle_message(msg, handler_id.as_deref(), Some(connection))
                    .await
            }
        });

        let transport = UnixTransport::new(&path).with_limits(4096, Duration::from_secs(2), 10);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle =
            tokio::spawn(async move { transport.run(handler, disconnect_tx, shutdown_rx).await });

        wait_for_connect(&path).await?;

        // Connect and register a handler.
        let mut stream = BufStream::new(UnixStream::connect(&path).await?);
        let register = JsonRpcMessage::request(
            1.into(),
            "handler.register",
            Some(serde_json::json!({
                "bot_ids": ["echo-bot"],
                "event_types": ["dm_received"],
                "capabilities": ["ReadMessages"],
            })),
        );
        let line = serialize_message(&register)?;
        stream.write_all(line.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        stream.flush().await?;

        let mut response_line = String::new();
        stream.read_line(&mut response_line).await?;
        let response: JsonRpcMessage = serde_json::from_str(&response_line)?;
        let (handler_id, _reconnect_token) = match response {
            JsonRpcMessage::Response {
                result: Some(r), ..
            } => {
                let handler_id = r
                    .get("handler_id")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .ok_or("handler_id missing")?;
                let reconnect_token = r
                    .get("reconnect_token")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .ok_or("reconnect_token missing")?;
                assert!(!reconnect_token.is_empty());
                (handler_id, reconnect_token)
            }
            JsonRpcMessage::Error { error, .. } => {
                return Err(format!("register failed: {}", error.message).into());
            }
            _ => return Err("unexpected response".into()),
        };

        assert_eq!(
            dispatch.registered_handler_count(),
            1,
            "handler should be registered"
        );

        // The handler row should be persisted in the database.
        {
            let db = Database::open(&db_path)?;
            assert_eq!(
                db.load_handlers()?.len(),
                1,
                "handler row should be persisted"
            );
        }

        // Drop the connection.
        drop(stream);

        // Wait for the disconnect notification to propagate.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while dispatch.registered_handler_count() > 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert_eq!(
            dispatch.registered_handler_count(),
            0,
            "handler should be unregistered after disconnect"
        );

        // The database row should also be deleted.
        {
            let db = Database::open(&db_path)?;
            assert!(
                db.load_handlers()?.is_empty(),
                "handler row should be deleted on disconnect"
            );
        }

        // Subsequent mutating calls using the old handler_id must be rejected.
        let send_dm = JsonRpcMessage::request(
            2.into(),
            "agent.send_dm",
            Some(serde_json::json!({
                "bot_id": "echo-bot",
                "recipient": "npub1recipient",
                "content": "hello",
            })),
        );
        let rejected = dispatch
            .handle_message(send_dm, Some(&handler_id), None)
            .await?
            .expect("expected response");
        match rejected {
            JsonRpcMessage::Error { error, .. } => {
                assert_eq!(
                    error.code, -32001,
                    "old handler_id should be rejected with HandlerNotRegistered"
                );
            }
            _ => panic!("expected error for disconnected handler_id"),
        }

        let _ = shutdown_tx.send(());
        let _ = handle.await?;
    }
    Ok(())
}

#[tokio::test]
async fn unix_socket_directory_and_socket_are_owner_only() -> Result<(), Box<dyn std::error::Error>>
{
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let dir = test_socket_dir()?;
        let path = dir.join("test.sock");
        let transport = UnixTransport::new(&path).with_limits(1024, Duration::from_secs(1), 10);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            transport
                .run(echo_handler(), dummy_disconnect_sender(), shutdown_rx)
                .await
        });

        wait_for_connect(&path).await?;

        let dir_metadata = std::fs::metadata(&dir)?;
        let dir_mode = dir_metadata.permissions().mode() & 0o777;
        assert_eq!(
            dir_mode, 0o700,
            "Unix socket directory must be owner-only (0o700), got {:03o}",
            dir_mode
        );

        let metadata = std::fs::metadata(&path)?;
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "Unix socket must be owner-only (0o600), got {:03o}",
            mode
        );

        let _ = shutdown_tx.send(());
        let _ = handle.await?;
    }
    Ok(())
}

#[tokio::test]
async fn unix_transport_removes_stale_socket() -> Result<(), Box<dyn std::error::Error>> {
    let dir = test_socket_dir()?;
    let path = dir.join("stale.sock");
    tokio::fs::write(&path, b"stale").await?;

    let transport = UnixTransport::new(&path).with_limits(1024, Duration::from_secs(1), 10);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        transport
            .run(echo_handler(), dummy_disconnect_sender(), shutdown_rx)
            .await
    });

    wait_for_connect(&path).await?;

    let response = send_request(
        &path,
        &JsonRpcMessage::request(1.into(), "agent.metrics", None),
    )
    .await?;
    assert_eq!(response.id(), Some(&Value::from(1)));

    let _ = shutdown_tx.send(());
    let _ = handle.await?;
    Ok(())
}

#[tokio::test]
async fn unix_transport_rejects_oversized_frames() -> Result<(), Box<dyn std::error::Error>> {
    let dir = test_socket_dir()?;
    let path = dir.join("frame.sock");
    let transport = UnixTransport::new(&path).with_limits(16, Duration::from_secs(1), 10);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        transport
            .run(echo_handler(), dummy_disconnect_sender(), shutdown_rx)
            .await
    });

    wait_for_connect(&path).await?;

    let mut stream = BufStream::new(UnixStream::connect(&path).await?);
    stream.write_all(b"this line is too long\n").await?;
    stream.flush().await?;

    let mut buf = Vec::new();
    let n = stream.read_until(b'\n', &mut buf).await?;
    assert_eq!(n, 0, "connection should be closed after oversized frame");

    let _ = shutdown_tx.send(());
    let _ = handle.await?;
    Ok(())
}

#[tokio::test]
async fn unix_unregistered_peer_cannot_send_dm() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(unix)]
    {
        let dir = test_socket_dir()?;
        let path = dir.join("unregistered-send-dm.sock");
        let (dispatch, _db_dir) = setup_dispatch().await?;
        let dispatch_for_handler = dispatch.clone();

        let handler = message_handler(move |msg, connection, handler_id| {
            let dispatch = dispatch_for_handler.clone();
            async move {
                dispatch
                    .handle_message(msg, handler_id.as_deref(), Some(connection))
                    .await
            }
        });

        let transport = UnixTransport::new(&path).with_limits(4096, Duration::from_secs(2), 10);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            transport
                .run(handler, dummy_disconnect_sender(), shutdown_rx)
                .await
        });

        wait_for_connect(&path).await?;

        let req = JsonRpcMessage::request(
            1.into(),
            "agent.send_dm",
            Some(serde_json::json!({
                "bot_id": "echo-bot",
                "recipient": "npub1recipient",
                "content": "hello",
            })),
        );
        let resp = send_request(&path, &req).await?;
        match resp {
            JsonRpcMessage::Error { error, .. } => {
                assert_eq!(error.code, -32001, "expected HandlerNotRegistered");
            }
            _ => panic!("expected error for unregistered peer, got {resp:?}"),
        }

        let _ = shutdown_tx.send(());
        let _ = handle.await?;
    }
    Ok(())
}

#[tokio::test]
async fn unix_unregistered_peer_cannot_set_profile() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(unix)]
    {
        let dir = test_socket_dir()?;
        let path = dir.join("unregistered-set-profile.sock");
        let (dispatch, _db_dir) = setup_dispatch().await?;
        let dispatch_for_handler = dispatch.clone();

        let handler = message_handler(move |msg, connection, handler_id| {
            let dispatch = dispatch_for_handler.clone();
            async move {
                dispatch
                    .handle_message(msg, handler_id.as_deref(), Some(connection))
                    .await
            }
        });

        let transport = UnixTransport::new(&path).with_limits(4096, Duration::from_secs(2), 10);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            transport
                .run(handler, dummy_disconnect_sender(), shutdown_rx)
                .await
        });

        wait_for_connect(&path).await?;

        let req = JsonRpcMessage::request(
            1.into(),
            "agent.set_profile",
            Some(serde_json::json!({
                "bot_id": "echo-bot",
                "name": "Evil",
            })),
        );
        let resp = send_request(&path, &req).await?;
        match resp {
            JsonRpcMessage::Error { error, .. } => {
                assert_eq!(error.code, -32001, "expected HandlerNotRegistered");
            }
            _ => panic!("expected error for unregistered peer, got {resp:?}"),
        }

        let _ = shutdown_tx.send(());
        let _ = handle.await?;
    }
    Ok(())
}

#[tokio::test]
async fn unix_unregistered_peer_cannot_error() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(unix)]
    {
        let dir = test_socket_dir()?;
        let path = dir.join("unregistered-error.sock");
        let (dispatch, _db_dir) = setup_dispatch().await?;
        let dispatch_for_handler = dispatch.clone();

        let handler = message_handler(move |msg, connection, handler_id| {
            let dispatch = dispatch_for_handler.clone();
            async move {
                dispatch
                    .handle_message(msg, handler_id.as_deref(), Some(connection))
                    .await
            }
        });

        let transport = UnixTransport::new(&path).with_limits(4096, Duration::from_secs(2), 10);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            transport
                .run(handler, dummy_disconnect_sender(), shutdown_rx)
                .await
        });

        wait_for_connect(&path).await?;

        let req = JsonRpcMessage::request(
            1.into(),
            "agent.error",
            Some(serde_json::json!({
                "bot_id": "echo-bot",
                "message": "should not be accepted",
            })),
        );
        let resp = send_request(&path, &req).await?;
        match resp {
            JsonRpcMessage::Error { error, .. } => {
                assert_eq!(error.code, -32001, "expected HandlerNotRegistered");
            }
            _ => panic!("expected error for unregistered peer, got {resp:?}"),
        }

        let _ = shutdown_tx.send(());
        let _ = handle.await?;
    }
    Ok(())
}

#[tokio::test]
async fn unix_status_notification_matches_catalog() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(unix)]
    {
        let dir = test_socket_dir()?;
        let path = dir.join("status.sock");
        let (dispatch, _db_dir) = setup_dispatch().await?;
        let dispatch_for_handler = dispatch.clone();

        let handler = message_handler(move |msg, connection, handler_id| {
            let dispatch = dispatch_for_handler.clone();
            async move {
                dispatch
                    .handle_message(msg, handler_id.as_deref(), Some(connection))
                    .await
            }
        });

        let transport = UnixTransport::new(&path).with_limits(4096, Duration::from_secs(2), 10);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            transport
                .run(handler, dummy_disconnect_sender(), shutdown_rx)
                .await
        });

        wait_for_connect(&path).await?;

        let mut stream = BufStream::new(UnixStream::connect(&path).await?);
        let register = JsonRpcMessage::request(
            1.into(),
            "handler.register",
            Some(serde_json::json!({
                "bot_ids": ["echo-bot"],
                "event_types": ["dm_received"],
                "capabilities": ["ReadMessages", "SendMessages"],
            })),
        );
        let line = serialize_message(&register)?;
        stream.write_all(line.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        stream.flush().await?;

        let mut response_line = String::new();
        stream.read_line(&mut response_line).await?;

        // Broadcast a daemon lifecycle status notification.
        dispatch.broadcast_status(DaemonStatus::Ready).await;

        let mut notification_line = String::new();
        stream.read_line(&mut notification_line).await?;
        let notification: JsonRpcMessage = serde_json::from_str(&notification_line)?;
        let JsonRpcMessage::Notification { method, params, .. } = notification else {
            panic!("expected notification, got {notification:?}");
        };
        assert_eq!(method, "agent.status");

        let payload = params.expect("agent.status params should be present");
        let status: pacto_bot_api::transport::protocol::AgentStatusParams =
            serde_json::from_value(payload)?;
        assert_eq!(status.state, "ready");
        assert!(status.identity.is_none(), "daemon status has no identity");
        assert_eq!(
            status.capabilities,
            vec!["ReadMessages".to_string(), "SendMessages".to_string()]
        );

        let _ = shutdown_tx.send(());
        let _ = handle.await?;
    }
    Ok(())
}

#[tokio::test]
async fn unix_handler_unregister_returns_unregistered_flag()
-> Result<(), Box<dyn std::error::Error>> {
    #[cfg(unix)]
    {
        let dir = test_socket_dir()?;
        let path = dir.join("unregister-method.sock");
        let (dispatch, _db_dir) = setup_dispatch().await?;
        let dispatch_for_handler = dispatch.clone();

        let handler = message_handler(move |msg, connection, handler_id| {
            let dispatch = dispatch_for_handler.clone();
            async move {
                dispatch
                    .handle_message(msg, handler_id.as_deref(), Some(connection))
                    .await
            }
        });

        let transport = UnixTransport::new(&path).with_limits(4096, Duration::from_secs(2), 10);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            transport
                .run(handler, dummy_disconnect_sender(), shutdown_rx)
                .await
        });

        wait_for_connect(&path).await?;

        // Use a single persistent connection so the transport can derive the
        // handler id from the registration and attach it to the unregister call.
        let mut stream = BufStream::new(UnixStream::connect(&path).await?);

        let register = JsonRpcMessage::request(
            1.into(),
            "handler.register",
            Some(serde_json::json!({
                "bot_ids": ["echo-bot"],
                "event_types": ["dm_received"],
                "capabilities": ["ReadMessages"],
            })),
        );
        let line = serialize_message(&register)?;
        stream.write_all(line.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        stream.flush().await?;

        let mut response_line = String::new();
        stream.read_line(&mut response_line).await?;
        let response: JsonRpcMessage = serde_json::from_str(&response_line)?;
        let handler_id = match response {
            JsonRpcMessage::Response {
                result: Some(r), ..
            } => {
                let handler_id = r
                    .get("handler_id")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .ok_or("handler_id missing")?;
                let reconnect_token = r
                    .get("reconnect_token")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .ok_or("reconnect_token missing")?;
                assert!(!reconnect_token.is_empty());
                handler_id
            }
            JsonRpcMessage::Error { error, .. } => {
                return Err(format!("register failed: {}", error.message).into());
            }
            _ => return Err("unexpected response".into()),
        };
        assert_eq!(dispatch.registered_handler_count(), 1);

        // Unregister using the authorized agent.unregister_handler RPC.
        let unregister = JsonRpcMessage::request(
            2.into(),
            "agent.unregister_handler",
            Some(serde_json::json!({
                "handler_id": handler_id,
            })),
        );
        let line = serialize_message(&unregister)?;
        stream.write_all(line.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        stream.flush().await?;

        let mut response_line = String::new();
        stream.read_line(&mut response_line).await?;
        let response: JsonRpcMessage = serde_json::from_str(&response_line)?;
        match response {
            JsonRpcMessage::Response {
                result: Some(r), ..
            } => {
                assert_eq!(r, serde_json::json!({ "unregistered": true }));
            }
            JsonRpcMessage::Error { error, .. } => {
                return Err(format!("unregister failed: {}", error.message).into());
            }
            _ => return Err("unexpected unregister response".into()),
        }
        assert_eq!(dispatch.registered_handler_count(), 0);

        // A subsequent call on the same connection is still tied to the old
        // handler id, but the registry no longer knows it.
        let send_dm = JsonRpcMessage::request(
            3.into(),
            "agent.send_dm",
            Some(serde_json::json!({
                "bot_id": "echo-bot",
                "recipient": "npub1recipient",
                "content": "hello",
            })),
        );
        let line = serialize_message(&send_dm)?;
        stream.write_all(line.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        stream.flush().await?;

        let mut response_line = String::new();
        stream.read_line(&mut response_line).await?;
        let response: JsonRpcMessage = serde_json::from_str(&response_line)?;
        match response {
            JsonRpcMessage::Error { error, .. } => {
                assert_eq!(
                    error.code, -32001,
                    "expected HandlerNotRegistered after unregister"
                );
            }
            _ => panic!("expected error for unregistered handler, got {response:?}"),
        }

        let _ = shutdown_tx.send(());
        let _ = handle.await?;
    }
    Ok(())
}

async fn wait_for_connect(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if UnixStream::connect(path).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err("socket did not accept connections in time".into())
}

async fn send_request(
    path: &std::path::Path,
    msg: &JsonRpcMessage,
) -> Result<JsonRpcMessage, Box<dyn std::error::Error>> {
    let mut stream = BufStream::new(UnixStream::connect(path).await?);
    let line = serialize_message(msg)?;
    stream.write_all(line.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await?;

    let mut line = String::new();
    stream.read_line(&mut line).await?;
    let parsed = serde_json::from_str::<JsonRpcMessage>(&line)?;
    Ok(parsed)
}
