/// req(R2, R3)
mod common;
mod support;

use nostr::ToBech32;
use pacto_bot_api::client_manager::ClientManager;
use pacto_bot_api::config::{BotConfig, DaemonConfig, GlobalDaemonConfig, SigningConfig};
use pacto_bot_api::db::Database;
use pacto_bot_api::diagnostics::Diagnostics;
use pacto_bot_api::dispatch::Dispatch;
use pacto_bot_api::events::{AgentEvent, EventType};
use pacto_bot_api::nostr::NostrClient;
use pacto_bot_api::transport::http::HttpTransport;
use pacto_bot_api::transport::message_handler;
use pacto_bot_api::transport::protocol::{JsonRpcMessage, serialize_message};
use secrecy::SecretString;
use serde_json::Value;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;
use support::mock_relay::MockRelay;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

fn echo_handler() -> pacto_bot_api::transport::MessageHandler {
    message_handler(|msg, _out_tx, _handler_id| async move {
        let id = msg.id().cloned().unwrap_or(Value::Null);
        Ok(Some(JsonRpcMessage::response(
            id,
            Some(Value::String("pong".into())),
        )))
    })
}

fn dispatch_handler(dispatch: Arc<Dispatch>) -> pacto_bot_api::transport::MessageHandler {
    message_handler(move |msg, out_tx, handler_id| {
        let dispatch = Arc::clone(&dispatch);
        async move {
            dispatch
                .handle_message(msg, handler_id.as_deref(), Some(out_tx))
                .await
        }
    })
}

#[tokio::test]
async fn http_rejects_missing_secret_with_401() -> Result<(), Box<dyn std::error::Error>> {
    let (port, shutdown_tx, _handle) = start_server().await?;

    let response = raw_http_post(port, None, None, "{}").await?;
    assert!(response.starts_with("HTTP/1.1 401"), "got: {response}");
    assert!(
        !response.contains("secret"),
        "401 body must not leak the token"
    );

    let _ = shutdown_tx.send(());
    Ok(())
}

#[tokio::test]
async fn http_rejects_wrong_secret_with_401() -> Result<(), Box<dyn std::error::Error>> {
    let (port, shutdown_tx, _handle) = start_server().await?;

    let response = raw_http_post(port, Some("wrong-token"), None, "{}").await?;
    assert!(response.starts_with("HTTP/1.1 401"), "got: {response}");
    assert!(
        !response.contains("secret"),
        "401 body must not leak the token"
    );

    let _ = shutdown_tx.send(());
    Ok(())
}

#[tokio::test]
async fn http_rejects_wrong_length_secret_with_401() -> Result<(), Box<dyn std::error::Error>> {
    let (port, shutdown_tx, dir) = start_server().await?;
    let token = read_token(dir.path()).await?;

    // Shorter than the real token.
    let response = raw_http_post(port, Some("short"), None, "{}").await?;
    assert!(response.starts_with("HTTP/1.1 401"), "got: {response}");
    assert!(
        !response.contains("secret"),
        "401 body must not leak the token"
    );

    // Longer than the real token.
    let long_token = format!("{token}extra");
    let response = raw_http_post(port, Some(&long_token), None, "{}").await?;
    assert!(response.starts_with("HTTP/1.1 401"), "got: {response}");
    assert!(
        !response.contains("secret"),
        "401 body must not leak the token"
    );

    let _ = shutdown_tx.send(());
    Ok(())
}

#[tokio::test]
async fn http_accepts_correct_secret() -> Result<(), Box<dyn std::error::Error>> {
    let (port, shutdown_tx, dir) = start_server().await?;
    let token = read_token(dir.path()).await?;

    let body = serialize_message(&JsonRpcMessage::request(7.into(), "agent.metrics", None))?;
    let response = raw_http_post(port, Some(&token), None, &body).await?;
    assert!(response.starts_with("HTTP/1.1 200"), "got: {response}");
    assert!(
        response.contains("\"id\":7"),
        "response should echo request id"
    );

    let _ = shutdown_tx.send(());
    Ok(())
}

#[tokio::test]
async fn http_token_file_is_owner_only() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(unix)]
    {
        let (_port, shutdown_tx, dir) = start_server().await?;
        let token_path = dir.path().join("bot_secret_token");

        // Wait for the server to finish creating the token file.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let metadata = tokio::fs::metadata(&token_path).await?;
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "HTTP secret token file must be owner-only (0o600), got {:03o}",
            mode
        );

        let _ = shutdown_tx.send(());
    }
    Ok(())
}

#[tokio::test]
async fn http_handler_register_returns_handler_id() -> Result<(), Box<dyn std::error::Error>> {
    let (port, shutdown_tx, dir, _dispatch) = start_dispatch_server().await?;
    let token = read_token(dir.path()).await?;

    let body = serialize_message(&JsonRpcMessage::request(
        8.into(),
        "handler.register",
        Some(serde_json::json!({
            "bot_ids": ["echo-bot"],
            "event_types": ["dm_received"],
            "capabilities": ["ReadMessages", "SendMessages"],
        })),
    ))?;
    let response = raw_http_post(port, Some(&token), None, &body).await?;
    assert!(response.starts_with("HTTP/1.1 200"), "got: {response}");

    let handler_id = extract_handler_id(&response)?;
    assert!(!handler_id.is_empty());

    let _ = shutdown_tx.send(());
    Ok(())
}

#[tokio::test]
async fn http_handler_response_is_accepted() -> Result<(), Box<dyn std::error::Error>> {
    let (port, shutdown_tx, dir, _dispatch) = start_dispatch_server().await?;
    let token = read_token(dir.path()).await?;

    // Register first so the handler id is valid.
    let register_body = serialize_message(&JsonRpcMessage::request(
        1.into(),
        "handler.register",
        Some(serde_json::json!({
            "bot_ids": ["echo-bot"],
            "event_types": ["dm_received"],
            "capabilities": ["ReadMessages", "SendMessages"],
        })),
    ))?;
    let register_response = raw_http_post(port, Some(&token), None, &register_body).await?;
    let handler_id = extract_handler_id(&register_response)?;

    let response_body = serialize_message(&JsonRpcMessage::request(
        9.into(),
        "handler.response",
        Some(serde_json::json!({
            "event_id": "evt-123",
            "action": "ack",
        })),
    ))?;
    let response = raw_http_post(port, Some(&token), Some(&handler_id), &response_body).await?;
    assert!(response.starts_with("HTTP/1.1 200"), "got: {response}");

    let _ = shutdown_tx.send(());
    Ok(())
}

#[tokio::test]
async fn http_handler_unregister_returns_unregistered_flag()
-> Result<(), Box<dyn std::error::Error>> {
    let (port, shutdown_tx, dir, _dispatch) = start_dispatch_server().await?;
    let token = read_token(dir.path()).await?;

    let register_body = serialize_message(&JsonRpcMessage::request(
        1.into(),
        "handler.register",
        Some(serde_json::json!({
            "bot_ids": ["echo-bot"],
            "event_types": ["dm_received"],
            "capabilities": ["ReadMessages", "SendMessages"],
        })),
    ))?;
    let register_response = raw_http_post(port, Some(&token), None, &register_body).await?;
    let handler_id = extract_handler_id(&register_response)?;

    let unregister_body = serialize_message(&JsonRpcMessage::request(
        2.into(),
        "handler.unregister",
        None,
    ))?;
    let unregister_response =
        raw_http_post(port, Some(&token), Some(&handler_id), &unregister_body).await?;
    assert!(
        unregister_response.starts_with("HTTP/1.1 200"),
        "got: {unregister_response}"
    );

    let body = unregister_response
        .split("\r\n\r\n")
        .nth(1)
        .or_else(|| unregister_response.split("\n\n").nth(1))
        .unwrap_or("")
        .trim();
    let msg: JsonRpcMessage = serde_json::from_str(body)?;
    match msg {
        JsonRpcMessage::Response {
            result: Some(r), ..
        } => {
            assert_eq!(r, serde_json::json!({ "unregistered": true }));
        }
        _ => panic!("expected unregister response, got {msg:?}"),
    }

    let _ = shutdown_tx.send(());
    Ok(())
}

#[tokio::test]
async fn http_rejects_non_loopback_bind() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let transport = HttpTransport::new("0.0.0.0:0", dir.path());
    let (_shutdown_tx, shutdown_rx) = oneshot::channel();
    let (_disconnect_tx, _disconnect_rx) = mpsc::channel::<Option<String>>(1);
    let result = transport
        .run(echo_handler(), _disconnect_tx, shutdown_rx)
        .await;
    assert!(result.is_err(), "binding to 0.0.0.0 should be rejected");
    Ok(())
}

#[tokio::test]
async fn http_unregistered_send_dm_rejected() -> Result<(), Box<dyn std::error::Error>> {
    let (port, shutdown_tx, dir, _dispatch) = start_dispatch_server().await?;
    let token = read_token(dir.path()).await?;

    let body = serialize_message(&JsonRpcMessage::request(
        1.into(),
        "agent.send_dm",
        Some(serde_json::json!({
            "bot_id": "echo-bot",
            "recipient": "npub1recipient",
            "content": "hello",
        })),
    ))?;
    let response = raw_http_post(port, Some(&token), None, &body).await?;
    assert!(response.starts_with("HTTP/1.1 401"), "got: {response}");

    let _ = shutdown_tx.send(());
    Ok(())
}

#[tokio::test]
async fn http_unregistered_set_profile_rejected() -> Result<(), Box<dyn std::error::Error>> {
    let (port, shutdown_tx, dir, _dispatch) = start_dispatch_server().await?;
    let token = read_token(dir.path()).await?;

    let body = serialize_message(&JsonRpcMessage::request(
        1.into(),
        "agent.set_profile",
        Some(serde_json::json!({
            "bot_id": "echo-bot",
            "name": "Evil",
        })),
    ))?;
    let response = raw_http_post(port, Some(&token), None, &body).await?;
    assert!(response.starts_with("HTTP/1.1 401"), "got: {response}");

    let _ = shutdown_tx.send(());
    Ok(())
}

#[tokio::test]
async fn http_invalid_handler_id_send_dm_rejected() -> Result<(), Box<dyn std::error::Error>> {
    let (port, shutdown_tx, dir, _dispatch) = start_dispatch_server().await?;
    let token = read_token(dir.path()).await?;

    let body = serialize_message(&JsonRpcMessage::request(
        1.into(),
        "agent.send_dm",
        Some(serde_json::json!({
            "bot_id": "echo-bot",
            "recipient": "npub1recipient",
            "content": "hello",
        })),
    ))?;
    let response = raw_http_post(port, Some(&token), Some("not-a-real-handler-id"), &body).await?;
    assert!(response.starts_with("HTTP/1.1 200"), "got: {response}");
    assert!(
        response.contains("\"code\":-32001"),
        "expected HandlerNotRegistered in body: {response}"
    );

    let _ = shutdown_tx.send(());
    Ok(())
}

#[tokio::test]
async fn http_dm_round_trip_registers_replies_and_publishes_gift_wrap()
-> Result<(), Box<dyn std::error::Error>> {
    let relay = MockRelay::start().await?;
    let (port, shutdown_tx, dir, dispatch) = start_dispatch_server_with_relay(&relay).await?;
    let token = read_token(dir.path()).await?;

    // Register an HTTP handler for the echo bot.
    let register_body = serialize_message(&JsonRpcMessage::request(
        1.into(),
        "handler.register",
        Some(serde_json::json!({
            "bot_ids": ["echo-bot"],
            "event_types": ["dm_received"],
            "capabilities": ["ReadMessages", "SendMessages"],
        })),
    ))?;
    let register_response = raw_http_post(port, Some(&token), None, &register_body).await?;
    let handler_id = extract_handler_id(&register_response)?;

    // Open the SSE notification stream.
    let mut sse = SseClient::connect(port, &token, &handler_id).await?;

    // Generate a sender and push a synthetic DM event to the dispatch layer.
    let sender_keys = nostr::Keys::generate();
    let sender_npub = sender_keys.public_key().to_bech32()?;
    let rumor_id = "0000000000000000000000000000000000000000000000000000000000000001";

    dispatch
        .dispatch_event(AgentEvent {
            bot_id: "echo-bot".into(),
            event_id: "evt-123".into(),
            event_type: EventType::DmReceived,
            chat_id: None,
            content: "hello".into(),
            rumor_id: rumor_id.into(),
            author: sender_npub.clone(),
            timestamp: 1,
        })
        .await?;

    // Wait for the daemon to push the agent.event notification over SSE.
    let notification = sse.next_notification(Duration::from_secs(5)).await?;
    let event = match notification {
        JsonRpcMessage::Notification { method, params, .. } if method == "agent.event" => {
            serde_json::from_value::<AgentEvent>(params.unwrap_or(Value::Null))?
        }
        _ => return Err("expected agent.event notification".into()),
    };
    assert_eq!(event.bot_id, "echo-bot");
    assert_eq!(event.content, "hello");

    // Reply via agent.send_dm over HTTP.
    let send_body = serialize_message(&JsonRpcMessage::request(
        2.into(),
        "agent.send_dm",
        Some(serde_json::json!({
            "bot_id": "echo-bot",
            "recipient": sender_npub,
            "content": "echo: hello",
            "reply_to": rumor_id,
        })),
    ))?;
    let send_response = raw_http_post(port, Some(&token), Some(&handler_id), &send_body).await?;
    assert!(
        send_response.starts_with("HTTP/1.1 200"),
        "got: {send_response}"
    );

    // The daemon should have published a kind:1059 gift wrap addressed to the sender.
    let sender_pubkey = sender_keys.public_key();
    let events = relay
        .wait_for_event(
            |e| {
                e.kind == nostr::Kind::GiftWrap && e.tags.public_keys().any(|p| p == &sender_pubkey)
            },
            Duration::from_secs(5),
        )
        .await?;
    assert!(
        events.iter().any(|e| e.kind == nostr::Kind::GiftWrap),
        "reply gift wrap not found on relay"
    );

    let _ = shutdown_tx.send(());
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn http_rejects_over_limit_with_503() -> Result<(), Box<dyn std::error::Error>> {
    let (port, shutdown_tx, dir) = start_server_with_limits(1, Duration::from_secs(60)).await?;
    let token = read_token(dir.path()).await?;

    // Hold the single permitted connection open after a successful request.
    let mut hold = TcpStream::connect(format!("127.0.0.1:{port}")).await?;
    let body = serialize_message(&JsonRpcMessage::request(1.into(), "agent.metrics", None))?;
    let request = format!(
        "POST / HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         X-Pacto-Bot-Secret: {token}\r\n\r\n\
         {body}",
        body.len()
    );
    hold.write_all(request.as_bytes()).await?;
    hold.flush().await?;

    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(5), hold.read(&mut buf))
        .await
        .map_err(|_| "timed out reading hold response")??;
    assert!(n > 0);
    assert!(
        String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"),
        "expected 200 on first connection"
    );

    // A second connection should be rejected with 503.
    let response = raw_http_post(port, Some(&token), None, "{}").await?;
    assert!(
        response.starts_with("HTTP/1.1 503"),
        "expected 503 when connection limit exceeded, got: {response}"
    );

    let _ = shutdown_tx.send(());
    Ok(())
}

#[tokio::test]
async fn http_idle_timeout_closes_connection() -> Result<(), Box<dyn std::error::Error>> {
    let (port, shutdown_tx, dir) = start_server_with_limits(10, Duration::from_millis(200)).await?;
    let token = read_token(dir.path()).await?;

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).await?;
    let body = serialize_message(&JsonRpcMessage::request(1.into(), "agent.metrics", None))?;
    let request = format!(
        "POST / HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         X-Pacto-Bot-Secret: {token}\r\n\r\n\
         {body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;

    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
        .await
        .map_err(|_| "timed out reading first response")??;
    assert!(n > 0);
    assert!(
        String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"),
        "expected successful response to keep connection alive"
    );

    // Wait longer than the idle timeout between keep-alive requests.
    tokio::time::sleep(Duration::from_millis(400)).await;

    let mut tail = [0u8; 1];
    let n = stream.read(&mut tail).await?;
    assert_eq!(
        n, 0,
        "expected idle keep-alive timeout to close the connection"
    );

    let _ = shutdown_tx.send(());
    Ok(())
}

async fn start_server()
-> Result<(u16, oneshot::Sender<()>, tempfile::TempDir), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let data_dir = dir.path().to_path_buf();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

    let transport = HttpTransport::new("127.0.0.1:0", &data_dir).with_max_frame_size(1024);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (_disconnect_tx, _disconnect_rx) = mpsc::channel::<Option<String>>(1);
    tokio::spawn(async move {
        let _ = transport
            .run_with_listener(listener, echo_handler(), _disconnect_tx, shutdown_rx)
            .await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    Ok((port, shutdown_tx, dir))
}

async fn start_server_with_limits(
    max_connections: usize,
    idle_timeout: Duration,
) -> Result<(u16, oneshot::Sender<()>, tempfile::TempDir), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let data_dir = dir.path().to_path_buf();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

    let transport = HttpTransport::new("127.0.0.1:0", &data_dir)
        .with_max_frame_size(1024)
        .with_limits(max_connections, idle_timeout);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (_disconnect_tx, _disconnect_rx) = mpsc::channel::<Option<String>>(1);
    tokio::spawn(async move {
        let _ = transport
            .run_with_listener(listener, echo_handler(), _disconnect_tx, shutdown_rx)
            .await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    Ok((port, shutdown_tx, dir))
}

async fn start_dispatch_server()
-> Result<(u16, oneshot::Sender<()>, tempfile::TempDir, Arc<Dispatch>), Box<dyn std::error::Error>>
{
    let relay = MockRelay::start().await?;
    start_dispatch_server_with_relay(&relay).await
}

async fn start_dispatch_server_with_relay(
    relay: &MockRelay,
) -> Result<(u16, oneshot::Sender<()>, tempfile::TempDir, Arc<Dispatch>), Box<dyn std::error::Error>>
{
    let dir = tempfile::tempdir()?;
    let data_dir = dir.path().to_path_buf();

    let bot_keys = nostr::Keys::generate();
    let bot_config = BotConfig {
        id: "echo-bot".into(),
        npub: bot_keys.public_key().to_bech32()?,
        signing: SigningConfig::Nsec {
            nsec: SecretString::new(bot_keys.secret_key().to_bech32()?.into()),
        },
        relays: vec![relay.url()],
        capabilities: vec!["ReadMessages".into(), "SendMessages".into()],
    };

    let config = DaemonConfig {
        daemon: GlobalDaemonConfig::default(),
        bots: vec![bot_config],
    };

    let nostr_client = NostrClient::new(vec![relay.url()]).await?;
    let cm = Arc::new(tokio::sync::RwLock::new(
        ClientManager::new(config, nostr_client).await?,
    ));
    let db = Database::open(&data_dir.join("test.db"))?;
    let dispatch = Arc::new(Dispatch::new(cm, db, Diagnostics::new()));

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let transport = HttpTransport::new("127.0.0.1:0", &data_dir);
    let handler = dispatch_handler(Arc::clone(&dispatch));
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (_disconnect_tx, _disconnect_rx) = mpsc::channel::<Option<String>>(1);
    tokio::spawn(async move {
        let _ = transport
            .run_with_listener(listener, handler, _disconnect_tx, shutdown_rx)
            .await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    Ok((port, shutdown_tx, dir, dispatch))
}

async fn read_token(data_dir: &std::path::Path) -> Result<String, Box<dyn std::error::Error>> {
    let contents = tokio::fs::read_to_string(data_dir.join("bot_secret_token")).await?;
    Ok(contents.trim().to_string())
}

async fn raw_http_post(
    port: u16,
    secret: Option<&str>,
    handler_id: Option<&str>,
    body: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).await?;

    let secret_header = secret
        .map(|s| format!("X-Pacto-Bot-Secret: {s}\r\n"))
        .unwrap_or_default();
    let handler_header = handler_id
        .map(|s| format!("X-Pacto-Handler-Id: {s}\r\n"))
        .unwrap_or_default();

    let request = format!(
        "POST / HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         {secret_header}\
         {handler_header}\
         \r\n\
         {body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;

    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(30), stream.read(&mut buf))
        .await
        .map_err(|_| "timed out reading HTTP response")??;
    buf.truncate(n);
    Ok(String::from_utf8_lossy(&buf).to_string())
}

fn extract_handler_id(response: &str) -> Result<String, Box<dyn std::error::Error>> {
    let mut lines = response.lines();
    let status = lines.next().ok_or("empty HTTP response")?;
    if !status.starts_with("HTTP/1.1 200") {
        return Err(format!("unexpected HTTP status: {status}").into());
    }

    let body = response
        .split("\r\n\r\n")
        .nth(1)
        .or_else(|| response.split("\n\n").nth(1))
        .unwrap_or("");
    let trimmed = body.trim();
    let msg: JsonRpcMessage =
        serde_json::from_str(trimmed).map_err(|e| format!("failed to parse JSON-RPC body: {e}"))?;

    match msg {
        JsonRpcMessage::Response {
            result: Some(result),
            ..
        } => result
            .get("handler_id")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| "handler.register response missing handler_id".into()),
        JsonRpcMessage::Error { error, .. } => Err(format!(
            "handler.register returned error {}: {}",
            error.code, error.message
        )
        .into()),
        _ => Err("handler.register response was not a response".into()),
    }
}

struct SseClient {
    notification_rx: mpsc::UnboundedReceiver<JsonRpcMessage>,
    _handle: tokio::task::JoinHandle<()>,
}

impl SseClient {
    async fn connect(
        port: u16,
        secret: &str,
        handler_id: &str,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).await?;
        let request = format!(
            "GET /events?handler_id={handler_id} HTTP/1.1\r\n\
             Host: 127.0.0.1:{port}\r\n\
             X-Pacto-Bot-Secret: {secret}\r\n\
             Accept: text/event-stream\r\n\
             \r\n"
        );
        stream.write_all(request.as_bytes()).await?;
        stream.flush().await?;

        let (notification_tx, notification_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();
        let handle = tokio::spawn(async move {
            let mut reader = BufReader::new(stream);
            let mut header_buf = String::new();
            loop {
                header_buf.clear();
                match reader.read_line(&mut header_buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if header_buf == "\r\n" || header_buf == "\n" {
                            break;
                        }
                    }
                }
            }

            let mut data_lines: Vec<String> = Vec::new();
            let mut line = String::new();
            loop {
                line.clear();
                match tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
                    .await
                {
                    Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                    Ok(Ok(_)) => {
                        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                        if trimmed.is_empty() {
                            if !data_lines.is_empty() {
                                let payload = data_lines.join("\n");
                                data_lines.clear();
                                if let Ok(msg) = serde_json::from_str::<JsonRpcMessage>(&payload) {
                                    let _ = notification_tx.send(msg);
                                }
                            }
                        } else if let Some(stripped) = trimmed.strip_prefix("data:") {
                            data_lines.push(stripped.trim_start().to_string());
                        }
                    }
                }
            }
        });

        Ok(Self {
            notification_rx,
            _handle: handle,
        })
    }

    async fn next_notification(
        &mut self,
        deadline: Duration,
    ) -> Result<JsonRpcMessage, Box<dyn std::error::Error>> {
        match tokio::time::timeout(deadline, self.notification_rx.recv()).await {
            Ok(Some(msg)) => Ok(msg),
            Ok(None) => Err("notification channel closed".into()),
            Err(_) => Err("timed out waiting for SSE notification".into()),
        }
    }
}
