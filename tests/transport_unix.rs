use pacto_bot_api::transport::protocol::{JsonRpcMessage, serialize_message};
use pacto_bot_api::transport::{message_handler, unix::UnixTransport};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufStream};
use tokio::net::UnixStream;
use tokio::sync::oneshot;

fn test_socket_dir() -> Result<PathBuf, std::io::Error> {
    let base = PathBuf::from("target/transport-tests");
    let dir = base.join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn echo_handler() -> pacto_bot_api::transport::MessageHandler {
    message_handler(|msg| async move {
        let id = msg.id().cloned().unwrap_or(Value::Null);
        Ok(Some(JsonRpcMessage::response(
            id,
            Some(Value::String("pong".into())),
        )))
    })
}

#[tokio::test]
async fn unix_socket_permissions_are_0o600() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let dir = test_socket_dir()?;
        let path = dir.join("test.sock");
        let transport = UnixTransport::new(&path).with_limits(1024, Duration::from_secs(1), 10);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = tokio::spawn(async move { transport.run(echo_handler(), shutdown_rx).await });

        wait_for_connect(&path).await?;

        let metadata = std::fs::metadata(&path)?;
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

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
    let handle = tokio::spawn(async move { transport.run(echo_handler(), shutdown_rx).await });

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
    let handle = tokio::spawn(async move { transport.run(echo_handler(), shutdown_rx).await });

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
