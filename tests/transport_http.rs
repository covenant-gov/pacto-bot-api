use pacto_bot_api::transport::http::HttpTransport;
use pacto_bot_api::transport::message_handler;
use pacto_bot_api::transport::protocol::{JsonRpcMessage, serialize_message};
use serde_json::Value;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

fn echo_handler() -> pacto_bot_api::transport::MessageHandler {
    message_handler(|msg, _out_tx, _handler_id| async move {
        let id = msg.id().cloned().unwrap_or(Value::Null);
        Ok(Some(JsonRpcMessage::response(
            id,
            Some(Value::String("pong".into())),
        )))
    })
}

#[tokio::test]
async fn http_rejects_missing_secret_with_401() -> Result<(), Box<dyn std::error::Error>> {
    let (port, shutdown_tx, _handle) = start_server().await?;

    let response = raw_http_post(port, None, "{}").await?;
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

    let response = raw_http_post(port, Some("wrong-token"), "{}").await?;
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
    let response = raw_http_post(port, Some(&token), &body).await?;
    assert!(response.starts_with("HTTP/1.1 200"), "got: {response}");
    assert!(
        response.contains("\"id\":7"),
        "response should echo request id"
    );

    let _ = shutdown_tx.send(());
    Ok(())
}

#[tokio::test]
async fn http_rejects_non_loopback_bind() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let transport = HttpTransport::new("0.0.0.0:0", dir.path());
    let (_shutdown_tx, shutdown_rx) = oneshot::channel();
    let result = transport.run(echo_handler(), shutdown_rx).await;
    assert!(result.is_err(), "binding to 0.0.0.0 should be rejected");
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
    tokio::spawn(async move {
        let _ = transport
            .run_with_listener(listener, echo_handler(), shutdown_rx)
            .await;
    });

    // Give the server a moment to start accepting.
    tokio::time::sleep(Duration::from_millis(50)).await;
    Ok((port, shutdown_tx, dir))
}

async fn read_token(data_dir: &std::path::Path) -> Result<String, Box<dyn std::error::Error>> {
    let contents = tokio::fs::read_to_string(data_dir.join("bot_secret_token")).await?;
    Ok(contents.trim().to_string())
}

async fn raw_http_post(
    port: u16,
    secret: Option<&str>,
    body: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}")).await?;

    let secret_header = secret
        .map(|s| format!("X-Pacto-Bot-Secret: {s}\r\n"))
        .unwrap_or_default();

    let request = format!(
        "POST / HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         {secret_header}\
         \r\n\
         {body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;

    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
        .await
        .map_err(|_| "timed out reading HTTP response")??;
    buf.truncate(n);
    Ok(String::from_utf8_lossy(&buf).to_string())
}
