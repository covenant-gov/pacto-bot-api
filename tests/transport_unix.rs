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
    setup_dispatch_with_capabilities(vec!["ReadMessages".into(), "SendMessages".into()]).await
}

async fn setup_dispatch_with_capabilities(
    capabilities: Vec<String>,
) -> Result<(Arc<Dispatch>, tempfile::TempDir), Box<dyn std::error::Error>> {
    let keys = nostr::Keys::generate();
    let bot = BotConfig {
        id: "echo-bot".to_string(),
        display_name: Some("echo-bot Display".to_string()),
        npub: keys.public_key().to_bech32()?,
        signing: SigningConfig::Nsec {
            nsec: SecretString::new(keys.secret_key().to_bech32()?.into()),
        },
        relays: vec![],
        capabilities,
        mls_dedup_window_secs: None,
        ..Default::default()
    };
    let config = DaemonConfig {
        daemon: GlobalDaemonConfig::default(),
        bots: vec![bot],
    };
    let dir = common::tempdir()?;
    let nostr_client = NostrClient::new(vec![]).await?;
    let db = Db::open(dir.path().join("agent.db").as_path()).await?;
    let cm = Arc::new(RwLock::new(
        ClientManager::new(dir.path(), config, nostr_client, &db).await?,
    ));
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
async fn unix_transport_rejects_live_socket() -> Result<(), Box<dyn std::error::Error>> {
    let dir = test_socket_dir()?;
    let path = dir.join("live.sock");

    // Pre-bind a live Unix socket at the daemon's target path.
    let _existing = tokio::net::UnixListener::bind(&path)?;

    let transport = UnixTransport::new(&path).with_limits(1024, Duration::from_secs(1), 10);
    let (_shutdown_tx, shutdown_rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        transport
            .run(echo_handler(), dummy_disconnect_sender(), shutdown_rx)
            .await
    });

    let result = handle.await?;
    assert!(
        result.is_err(),
        "UnixTransport::run should fail when the socket path is already in use"
    );
    let err = result.unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("already in use"),
        "expected an 'already in use' error, got: {err}"
    );
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
    assert_ne!(
        n, 0,
        "expected an error response before the connection closed"
    );

    let response: JsonRpcMessage = serde_json::from_slice(&buf)?;
    let error = response
        .as_error()
        .ok_or("expected a JSON-RPC error response")?;
    assert_eq!(error.code, -32600);
    assert!(
        error.message.contains("frame size") && error.message.contains("exceeds maximum"),
        "unexpected error message: {}",
        error.message
    );

    buf.clear();
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

#[tokio::test]
async fn unix_transport_preserves_connection_level_ordering()
-> Result<(), Box<dyn std::error::Error>> {
    #[cfg(unix)]
    {
        let dir = test_socket_dir()?;
        let path = dir.join("ordering.sock");
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
                "capabilities": ["ReadMessages"],
            })),
        );
        let error = JsonRpcMessage::request(
            2.into(),
            "agent.error",
            Some(serde_json::json!({
                "bot_id": "echo-bot",
                "message": "test",
            })),
        );

        // Write both requests in arrival order before reading either response.
        // The agent.error call requires the handler_id created by the register
        // call; only sequential per-connection processing guarantees it is
        // authorized rather than rejected with HandlerNotRegistered.
        let mut line = serialize_message(&register)?;
        stream.write_all(line.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        line = serialize_message(&error)?;
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
                assert!(
                    r.get("handler_id").and_then(|v| v.as_str()).is_some(),
                    "register response should contain a handler_id"
                );
            }
            JsonRpcMessage::Error { error, .. } => {
                return Err(format!("register failed: {}", error.message).into());
            }
            _ => return Err("unexpected register response".into()),
        }

        let mut response_line = String::new();
        stream.read_line(&mut response_line).await?;
        let response: JsonRpcMessage = serde_json::from_str(&response_line)?;
        match response {
            JsonRpcMessage::Response { .. } => {}
            JsonRpcMessage::Error { error, .. } => {
                panic!(
                    "agent.error on the same connection should be authorized after register: {:?}",
                    error
                );
            }
            _ => panic!("unexpected agent.error response"),
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

async fn send_request_on_stream(
    stream: &mut BufStream<UnixStream>,
    msg: &JsonRpcMessage,
) -> Result<JsonRpcMessage, Box<dyn std::error::Error>> {
    let line = serialize_message(msg)?;
    stream.write_all(line.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await?;

    let mut line = String::new();
    stream.read_line(&mut line).await?;
    let parsed = serde_json::from_str::<JsonRpcMessage>(&line)?;
    Ok(parsed)
}

fn assert_matches_jsonrpc_result_schema(
    result: &Value,
    method_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let catalog: Value = serde_json::from_str(&std::fs::read_to_string("schemas/jsonrpc.json")?)?;
    let method = catalog["methods"]
        .as_array()
        .and_then(|methods| {
            methods
                .iter()
                .find(|m| m["name"].as_str() == Some(method_name))
        })
        .ok_or_else(|| format!("method {method_name} not found in schemas/jsonrpc.json"))?;
    let schema = method["result"]["schema"].clone();
    assert!(
        !schema.is_null(),
        "method {method_name} must declare a result schema"
    );
    let validator = jsonschema::validator_for(&schema)?;
    assert!(
        validator.validate(result).is_ok(),
        "result for {method_name} must validate against schemas/jsonrpc.json"
    );
    Ok(())
}

fn assert_matches_version_schema(result: &Value) -> Result<(), Box<dyn std::error::Error>> {
    let schema: Value = serde_json::from_str(&std::fs::read_to_string("schemas/version.json")?)?;
    let validator = jsonschema::validator_for(&schema)?;
    assert!(
        validator.validate(result).is_ok(),
        "agent.version result must validate against schemas/version.json"
    );
    Ok(())
}

#[tokio::test]
async fn unix_agent_version_returns_version_and_commit() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(unix)]
    {
        let dir = test_socket_dir()?;
        let path = dir.join("version.sock");
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

        let response = send_request(
            &path,
            &JsonRpcMessage::request(1.into(), "agent.version", None),
        )
        .await?;
        match response {
            JsonRpcMessage::Response {
                result: Some(r), ..
            } => {
                let obj = r
                    .as_object()
                    .ok_or("expected object result for agent.version")?;
                assert!(obj.contains_key("version"));
                assert!(obj.contains_key("git_sha"));
                assert_eq!(obj["git_sha"].as_str().map(|s| s.len()), Some(8));
                assert_matches_version_schema(&r)?;
            }
            JsonRpcMessage::Error { error, .. } => {
                return Err(format!("agent.version failed: {}", error.message).into());
            }
            _ => return Err("unexpected response".into()),
        }

        let _ = shutdown_tx.send(());
        let _ = handle.await?;
    }
    Ok(())
}

#[tokio::test]
async fn unix_agent_list_handlers_returns_routing_table() -> Result<(), Box<dyn std::error::Error>>
{
    #[cfg(unix)]
    {
        let dir = test_socket_dir()?;
        let path = dir.join("list_handlers.sock");
        let (dispatch, _db_dir) =
            setup_dispatch_with_capabilities(vec!["ReadMessages".into(), "Admin".into()]).await?;
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
                "capabilities": ["ReadMessages", "Admin"],
            })),
        );
        let response = send_request_on_stream(&mut stream, &register).await?;
        let handler_id = match response {
            JsonRpcMessage::Response {
                result: Some(r), ..
            } => r
                .get("handler_id")
                .and_then(|v| v.as_str())
                .map(String::from)
                .ok_or("handler_id missing")?,
            JsonRpcMessage::Error { error, .. } => {
                return Err(format!("register failed: {}", error.message).into());
            }
            _ => return Err("unexpected response".into()),
        };

        let list =
            JsonRpcMessage::request(2.into(), "agent.list_handlers", Some(serde_json::json!({})));
        let response = send_request_on_stream(&mut stream, &list).await?;
        match response {
            JsonRpcMessage::Response {
                result: Some(r), ..
            } => {
                let obj = r
                    .as_object()
                    .ok_or("expected object result for agent.list_handlers")?;
                let handlers = obj["handlers"]
                    .as_array()
                    .ok_or("expected handlers array")?;
                assert_eq!(handlers.len(), 1);
                assert_eq!(handlers[0]["handler_id"], handler_id);
                assert_matches_jsonrpc_result_schema(&r, "agent.list_handlers")?;
            }
            JsonRpcMessage::Error { error, .. } => {
                return Err(format!("agent.list_handlers failed: {}", error.message).into());
            }
            _ => return Err("unexpected response".into()),
        }

        let _ = shutdown_tx.send(());
        let _ = handle.await?;
    }
    Ok(())
}

#[tokio::test]
async fn unix_agent_unregister_handler_forcibly_removes_handler()
-> Result<(), Box<dyn std::error::Error>> {
    #[cfg(unix)]
    {
        let dir = test_socket_dir()?;
        let path = dir.join("admin_unregister.sock");
        let (dispatch, _db_dir) =
            setup_dispatch_with_capabilities(vec!["ReadMessages".into(), "Admin".into()]).await?;
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

        let mut admin_stream = BufStream::new(UnixStream::connect(&path).await?);
        let register = JsonRpcMessage::request(
            1.into(),
            "handler.register",
            Some(serde_json::json!({
                "bot_ids": ["echo-bot"],
                "event_types": ["dm_received"],
                "capabilities": ["ReadMessages", "Admin"],
            })),
        );
        let response = send_request_on_stream(&mut admin_stream, &register).await?;
        let _admin_handler_id = match response {
            JsonRpcMessage::Response {
                result: Some(r), ..
            } => r
                .get("handler_id")
                .and_then(|v| v.as_str())
                .map(String::from)
                .ok_or("handler_id missing")?,
            JsonRpcMessage::Error { error, .. } => {
                return Err(format!("register failed: {}", error.message).into());
            }
            _ => return Err("unexpected response".into()),
        };

        let mut target_stream = BufStream::new(UnixStream::connect(&path).await?);
        let register = JsonRpcMessage::request(
            2.into(),
            "handler.register",
            Some(serde_json::json!({
                "bot_ids": ["echo-bot"],
                "event_types": ["dm_received"],
                "capabilities": ["ReadMessages"],
            })),
        );
        let response = send_request_on_stream(&mut target_stream, &register).await?;
        let target_handler_id = match response {
            JsonRpcMessage::Response {
                result: Some(r), ..
            } => r
                .get("handler_id")
                .and_then(|v| v.as_str())
                .map(String::from)
                .ok_or("handler_id missing")?,
            JsonRpcMessage::Error { error, .. } => {
                return Err(format!("register failed: {}", error.message).into());
            }
            _ => return Err("unexpected response".into()),
        };

        let unregister = JsonRpcMessage::request(
            3.into(),
            "agent.unregister_handler",
            Some(serde_json::json!({ "handler_id": target_handler_id })),
        );
        let response = send_request_on_stream(&mut admin_stream, &unregister).await?;
        match response {
            JsonRpcMessage::Response {
                result: Some(r), ..
            } => {
                assert_eq!(r, serde_json::json!({ "unregistered": true }));
                assert_matches_jsonrpc_result_schema(&r, "agent.unregister_handler")?;
            }
            JsonRpcMessage::Error { error, .. } => {
                return Err(format!("agent.unregister_handler failed: {}", error.message).into());
            }
            _ => return Err("unexpected response".into()),
        }

        let _ = shutdown_tx.send(());
        let _ = handle.await?;
    }
    Ok(())
}
