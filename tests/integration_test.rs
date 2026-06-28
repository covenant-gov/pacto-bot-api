mod common;
mod support;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use nostr::{EventBuilder, Keys, Kind, PublicKey};
use pacto_bot_api::transport::protocol::{JsonRpcMessage, serialize_message};
use serde_json::Value;
use support::mock_relay::MockRelay;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufStream};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::timeout;

/// Connect to `path` and retry until the socket accepts or `deadline` passes.
async fn wait_for_socket(
    path: &Path,
    deadline: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let end = tokio::time::Instant::now() + deadline;
    while tokio::time::Instant::now() < end {
        if UnixStream::connect(path).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err("daemon socket did not become available".into())
}

/// A simple handler client connected to the daemon Unix socket.
///
/// Maintains a persistent connection, matches responses by JSON-RPC id, and
/// delivers daemon notifications on a dedicated channel.
struct HandlerClient {
    outgoing_tx: mpsc::UnboundedSender<JsonRpcMessage>,
    notification_rx: mpsc::UnboundedReceiver<JsonRpcMessage>,
    pending: Arc<Mutex<HashMap<Value, oneshot::Sender<JsonRpcMessage>>>>,
    handler_id: String,
}

impl HandlerClient {
    async fn register(
        path: &Path,
        bot_ids: &[&str],
        event_types: &[&str],
        capabilities: &[&str],
    ) -> Result<Self, Box<dyn std::error::Error>> {
        wait_for_socket(path, Duration::from_secs(10)).await?;
        let stream = UnixStream::connect(path).await?;
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();
        let (notification_tx, notification_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();
        let pending: Arc<Mutex<HashMap<Value, oneshot::Sender<JsonRpcMessage>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let pending_for_io = Arc::clone(&pending);
        tokio::spawn(async move {
            let mut stream = BufStream::new(stream);

            async fn read_frame(
                stream: &mut BufStream<UnixStream>,
            ) -> Result<String, std::io::Error> {
                let mut line = String::new();
                stream.read_line(&mut line).await?;
                Ok(line)
            }

            loop {
                tokio::select! {
                    Some(msg) = outgoing_rx.recv() => {
                        let line = match serialize_message(&msg) {
                            Ok(l) => l,
                            Err(_) => continue,
                        };
                        if stream.write_all(line.as_bytes()).await.is_err() {
                            break;
                        }
                        if stream.write_all(b"\n").await.is_err() {
                            break;
                        }
                        if stream.flush().await.is_err() {
                            break;
                        }
                    }
                    result = read_frame(&mut stream) => {
                        let line = match result {
                            Ok(l) => l,
                            Err(_) => break,
                        };
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let msg: JsonRpcMessage = match serde_json::from_str(trimmed) {
                            Ok(m) => m,
                            Err(_) => continue,
                        };

                        if let Some(id) = msg.id().cloned() {
                            if let Some(tx) = pending_for_io.lock().await.remove(&id) {
                                let _ = tx.send(msg);
                                continue;
                            }
                        }
                        let _ = notification_tx.send(msg);
                    }
                }
            }
        });

        let mut client = Self {
            outgoing_tx,
            notification_rx,
            pending,
            handler_id: String::new(),
        };

        let resp = client
            .call(JsonRpcMessage::request(
                1.into(),
                "handler.register",
                Some(serde_json::json!({
                    "bot_ids": bot_ids,
                    "event_types": event_types,
                    "capabilities": capabilities,
                })),
            ))
            .await?;

        let handler_id = match resp {
            JsonRpcMessage::Response { result, .. } => result
                .and_then(|r| r.get("handler_id")?.as_str().map(String::from))
                .ok_or("handler.register response missing handler_id")?,
            JsonRpcMessage::Error { error, .. } => {
                return Err(format!("handler.register failed: {}", error.message).into());
            }
            _ => return Err("unexpected handler.register response".into()),
        };

        client.handler_id = handler_id;
        Ok(client)
    }

    async fn call(
        &self,
        msg: JsonRpcMessage,
    ) -> Result<JsonRpcMessage, Box<dyn std::error::Error>> {
        let id = msg.id().cloned().ok_or("request missing id")?;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        self.outgoing_tx
            .send(msg)
            .map_err(|_| "request channel closed")?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        match timeout(deadline.duration_since(tokio::time::Instant::now()), rx).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(_)) => Err("response channel closed".into()),
            Err(_) => Err("request timed out".into()),
        }
    }

    async fn next_notification(
        &mut self,
        deadline: Duration,
    ) -> Result<JsonRpcMessage, Box<dyn std::error::Error>> {
        match timeout(deadline, self.notification_rx.recv()).await {
            Ok(Some(msg)) => Ok(msg),
            Ok(None) => Err("notification channel closed".into()),
            Err(_) => Err("timed out waiting for notification".into()),
        }
    }

    async fn send_response(
        &self,
        event_id: &str,
        action: &str,
        content: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut params = serde_json::json!({
            "event_id": event_id,
            "action": action,
        });
        if let Some(c) = content {
            params["content"] = Value::String(c.into());
        }
        let msg = JsonRpcMessage::notification("handler.response", Some(params));
        self.outgoing_tx
            .send(msg)
            .map_err(|_| "request channel closed")?;
        Ok(())
    }
}

/// Build a kind:1059 gift wrap from `sender` to `recipient_npub`.
async fn build_gift_wrap(
    sender: &Keys,
    recipient_npub: &str,
    content: &str,
) -> Result<nostr::Event, Box<dyn std::error::Error>> {
    let recipient = PublicKey::parse(recipient_npub)?;
    let event = EventBuilder::private_msg(sender, recipient, content, Vec::new()).await?;
    Ok(event)
}

/// Wait for the daemon to publish a kind:1059 gift wrap addressed to `sender_pubkey`.
async fn wait_for_reply(
    relay: &MockRelay,
    sender_pubkey: &PublicKey,
    timeout_duration: Duration,
) -> Result<Vec<nostr::Event>, Box<dyn std::error::Error>> {
    relay
        .wait_for_event(
            |e| e.kind == Kind::GiftWrap && e.tags.public_keys().any(|p| p == sender_pubkey),
            timeout_duration,
        )
        .await
}

#[tokio::test]
async fn full_dm_round_trip_over_unix_socket() -> Result<(), Box<dyn std::error::Error>> {
    let relay = MockRelay::start().await?;
    let dir = tempfile::tempdir()?;

    let (bot_config, _nsec) = common::generate_nsec_bot("echo-bot")?;

    let mut config_bots = bot_config.clone();
    config_bots.relays = vec![relay.url()];
    config_bots.capabilities = vec!["ReadMessages".into(), "SendMessages".into()];
    let config_path = common::make_config(&dir, vec![config_bots])?;

    let daemon = common::spawn_daemon_until_ready(&config_path).await?;

    let socket_path = dir.path().join("pacto-bot-api.sock");
    let mut handler = HandlerClient::register(
        &socket_path,
        &["echo-bot"],
        &["dm_received"],
        &["ReadMessages", "SendMessages"],
    )
    .await?;

    // Sender keys represent the human/user sending a DM to the bot.
    let sender = Keys::generate();
    let gift = build_gift_wrap(&sender, &bot_config.npub, "/echo hello").await?;
    relay.inject_event(gift).await;

    // Wait for the daemon to dispatch the event to the handler.
    let notification = handler.next_notification(Duration::from_secs(5)).await?;
    let event_id = match &notification {
        JsonRpcMessage::Notification { method, params, .. } if method == "agent.event" => {
            let params = params.as_ref().ok_or("agent.event missing params")?;
            let event_id = params
                .get("event_id")
                .and_then(Value::as_str)
                .ok_or("agent.event missing event_id")?
                .to_string();
            let content = params
                .get("content")
                .and_then(Value::as_str)
                .ok_or("agent.event missing content")?;
            assert_eq!(content, "/echo hello");
            event_id
        }
        _ => {
            return Err(
                format!("expected agent.event notification, got {:?}", notification).into(),
            );
        }
    };

    // Reply with the echoed content.
    handler
        .send_response(&event_id, "reply", Some("hello"))
        .await?;

    // Wait for the daemon to publish an echo reply gift wrap addressed to the sender.
    let replies = wait_for_reply(&relay, &sender.public_key(), Duration::from_secs(5)).await?;
    assert!(
        !replies.is_empty(),
        "daemon should publish a reply gift wrap"
    );

    common::shutdown_daemon(daemon).await?;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn multi_bot_multiplexing() -> Result<(), Box<dyn std::error::Error>> {
    let relay = MockRelay::start().await?;
    let dir = tempfile::tempdir()?;

    let (mut echo_config, _nsec) = common::generate_nsec_bot("echo-bot")?;
    echo_config.relays = vec![relay.url()];
    echo_config.capabilities = vec!["ReadMessages".into(), "SendMessages".into()];

    let (mut other_config, _nsec2) = common::generate_nsec_bot("other-bot")?;
    other_config.relays = vec![relay.url()];
    other_config.capabilities = vec!["ReadMessages".into(), "SendMessages".into()];

    let config_path = common::make_config(&dir, vec![echo_config.clone(), other_config.clone()])?;
    let daemon = common::spawn_daemon_until_ready(&config_path).await?;

    let socket_path = dir.path().join("pacto-bot-api.sock");
    let mut echo_handler = HandlerClient::register(
        &socket_path,
        &["echo-bot"],
        &["dm_received"],
        &["ReadMessages", "SendMessages"],
    )
    .await?;

    let sender = Keys::generate();

    // DM for echo-bot should be delivered.
    let echo_gift = build_gift_wrap(&sender, &echo_config.npub, "for echo").await?;
    relay.inject_event(echo_gift).await;

    let notification = echo_handler
        .next_notification(Duration::from_secs(5))
        .await?;
    assert_eq!(notification.method(), Some("agent.event"));

    // DM for other-bot should not be delivered to the echo-only handler.
    let other_gift = build_gift_wrap(&sender, &other_config.npub, "for other").await?;
    relay.inject_event(other_gift).await;

    let timeout_result = echo_handler
        .next_notification(Duration::from_millis(500))
        .await;
    assert!(
        timeout_result.is_err(),
        "echo handler should not receive other-bot events"
    );

    common::shutdown_daemon(daemon).await?;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn handler_fan_out() -> Result<(), Box<dyn std::error::Error>> {
    let relay = MockRelay::start().await?;
    let dir = tempfile::tempdir()?;

    let (mut bot_config, _nsec) = common::generate_nsec_bot("echo-bot")?;
    bot_config.relays = vec![relay.url()];
    bot_config.capabilities = vec!["ReadMessages".into(), "SendMessages".into()];
    let config_path = common::make_config(&dir, vec![bot_config.clone()])?;

    let daemon = common::spawn_daemon_until_ready(&config_path).await?;

    let socket_path = dir.path().join("pacto-bot-api.sock");
    let mut handler_a = HandlerClient::register(
        &socket_path,
        &["echo-bot"],
        &["dm_received"],
        &["ReadMessages", "SendMessages"],
    )
    .await?;
    let mut handler_b = HandlerClient::register(
        &socket_path,
        &["echo-bot"],
        &["dm_received"],
        &["ReadMessages", "SendMessages"],
    )
    .await?;

    let sender = Keys::generate();
    let gift = build_gift_wrap(&sender, &bot_config.npub, "fan out").await?;
    relay.inject_event(gift).await;

    let notif_a = handler_a.next_notification(Duration::from_secs(5)).await?;
    let notif_b = handler_b.next_notification(Duration::from_secs(5)).await?;

    assert_eq!(notif_a.method(), Some("agent.event"));
    assert_eq!(notif_b.method(), Some("agent.event"));

    common::shutdown_daemon(daemon).await?;
    relay.stop().await;
    Ok(())
}
