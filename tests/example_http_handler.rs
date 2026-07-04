mod common;
mod support;

/// Example: HTTP-based handler registration and SSE notifications.
///
/// This test is intentionally written as a readable, end-to-end example. It
/// shows how a bot handler written in any language can:
///
/// 1. Register itself with the daemon over HTTP.
/// 2. Open a server-sent events (`/events`) stream to receive notifications.
/// 3. Receive an `agent.event` when a DM is routed to the bot.
/// 4. Acknowledge the event with `handler.response`.
///
/// The HTTP transport binds to a random loopback port and uses the secret
/// token written to `$DATA_DIR/bot_secret_token`. See `tests/transport_http.rs`
/// for more exhaustive HTTP transport tests.
use std::sync::Arc;
use std::time::Duration;

use nostr::ToBech32;
use pacto_bot_api::client_manager::ClientManager;
use pacto_bot_api::config::{BotConfig, DaemonConfig, GlobalDaemonConfig, SigningConfig};
use pacto_bot_api::db::Db;
use pacto_bot_api::diagnostics::Diagnostics;
use pacto_bot_api::dispatch::Dispatch;
use pacto_bot_api::events::{AgentEvent, EventType};
use pacto_bot_api::nostr::NostrClient;
use pacto_bot_api::transport::http::HttpTransport;
use pacto_bot_api::transport::message_handler;
use pacto_bot_api::transport::protocol::{JsonRpcMessage, serialize_message};
use secrecy::SecretString;
use serde_json::Value;
use support::mock_relay::MockRelay;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

#[tokio::test]
async fn http_handler_example_registers_and_receives_notifications()
-> Result<(), Box<dyn std::error::Error>> {
    // A lightweight in-process relay provides a relay URL for the bot config
    // without requiring an external service.
    let relay = MockRelay::start().await?;

    // Start the daemon HTTP transport on a random loopback port.
    let (port, shutdown_tx, data_dir, dispatch) = start_http_daemon(&relay).await?;
    let token = read_token(data_dir.path()).await?;

    // 1. Register a handler for the example bot.
    let handler_id = register_handler(port, &token).await?;

    // 2. Open the SSE notification stream for this handler.
    let mut sse = SseClient::connect(port, &token, &handler_id).await?;

    // 3. Simulate the daemon routing a DM to the bot. In production this
    //    event originates from a kind:1059 gift wrap received from a relay.
    dispatch
        .dispatch_event(AgentEvent {
            bot_id: "example-bot".into(),
            event_id: "evt-example-1".into(),
            event_type: EventType::DmReceived,
            chat_id: None,
            content: "hello from HTTP example".into(),
            rumor_id: "0000000000000000000000000000000000000000000000000000000000000001".into(),
            author: "npub1author".into(),
            timestamp: 1,
        })
        .await?;

    // 4. Wait for the daemon to push the `agent.event` notification.
    let notification = sse.next_notification(Duration::from_secs(5)).await?;
    match notification {
        JsonRpcMessage::Notification { method, params, .. } if method == "agent.event" => {
            let event: AgentEvent = serde_json::from_value(params.unwrap_or(Value::Null))?;
            assert_eq!(event.bot_id, "example-bot");
            assert_eq!(event.content, "hello from HTTP example");
        }
        other => return Err(format!("expected agent.event notification, got {other:?}").into()),
    }

    // 5. Acknowledge the event over HTTP.
    let ack_body = serialize_message(&JsonRpcMessage::request(
        2.into(),
        "handler.response",
        Some(serde_json::json!({
            "event_id": "evt-example-1",
            "action": "ack",
        })),
    ))?;
    let ack_response = raw_http_post(port, Some(&token), Some(&handler_id), &ack_body).await?;
    assert!(
        ack_response.starts_with("HTTP/1.1 200"),
        "got: {ack_response}"
    );

    let _ = shutdown_tx.send(());
    relay.stop().await;
    Ok(())
}

/// Start a minimal daemon with only the HTTP transport exposed.
async fn start_http_daemon(
    relay: &MockRelay,
) -> Result<(u16, oneshot::Sender<()>, tempfile::TempDir, Arc<Dispatch>), Box<dyn std::error::Error>>
{
    let dir = tempfile::tempdir()?;
    let data_dir = dir.path().to_path_buf();

    let bot_keys = nostr::Keys::generate();
    let bot_config = BotConfig {
        id: "example-bot".into(),
        npub: bot_keys.public_key().to_bech32()?,
        signing: SigningConfig::Nsec {
            nsec: SecretString::new(bot_keys.secret_key().to_bech32()?.into()),
        },
        relays: vec![relay.url()],
        capabilities: vec!["ReadMessages".into(), "SendMessages".into()],
        ..Default::default()
    };

    let config = DaemonConfig {
        daemon: GlobalDaemonConfig::default(),
        bots: vec![bot_config],
    };

    let nostr_client = NostrClient::new(vec![relay.url()]).await?;
    let client_manager = Arc::new(tokio::sync::RwLock::new(
        ClientManager::new(config, nostr_client).await?,
    ));
    let db = Db::open(&data_dir.join("example.db")).await?;
    let dispatch = Arc::new(Dispatch::new(client_manager, db, Diagnostics::new()));

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

/// A handler that forwards JSON-RPC messages to the dispatch layer.
fn dispatch_handler(dispatch: Arc<Dispatch>) -> pacto_bot_api::transport::MessageHandler {
    message_handler(move |msg, connection, handler_id| {
        let dispatch = Arc::clone(&dispatch);
        async move {
            dispatch
                .handle_message(msg, handler_id.as_deref(), Some(connection))
                .await
        }
    })
}

/// POST a `handler.register` request and return the issued handler id.
async fn register_handler(port: u16, token: &str) -> Result<String, Box<dyn std::error::Error>> {
    let body = serialize_message(&JsonRpcMessage::request(
        1.into(),
        "handler.register",
        Some(serde_json::json!({
            "bot_ids": ["example-bot"],
            "event_types": ["dm_received"],
            "capabilities": ["ReadMessages"],
        })),
    ))?;
    let response = raw_http_post(port, Some(token), None, &body).await?;

    let body = response
        .split("\r\n\r\n")
        .nth(1)
        .or_else(|| response.split("\n\n").nth(1))
        .unwrap_or("")
        .trim();
    let msg: JsonRpcMessage = serde_json::from_str(body)?;
    match msg {
        JsonRpcMessage::Response {
            result: Some(result),
            ..
        } => result
            .get("handler_id")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| "handler.register response missing handler_id".into()),
        JsonRpcMessage::Error { error, .. } => {
            Err(format!("handler.register error {}: {}", error.code, error.message).into())
        }
        _ => Err("handler.register response was not a response".into()),
    }
}

/// Read the HTTP secret token generated by the transport.
async fn read_token(data_dir: &std::path::Path) -> Result<String, Box<dyn std::error::Error>> {
    let contents = tokio::fs::read_to_string(data_dir.join("bot_secret_token")).await?;
    Ok(contents.trim().to_string())
}

/// Send a raw HTTP POST to the daemon JSON-RPC endpoint.
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

/// Minimal SSE client for the daemon `/events` endpoint.
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
