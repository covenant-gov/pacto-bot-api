//! Runtime HTTP token reload on SIGHUP.
//!
//! req(R3)
#![cfg(unix)]

mod common;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use nostr::ToBech32;
use pacto_bot_api::transport::protocol::{JsonRpcMessage, serialize_message};
use serde_json::Value;
use std::error::Error;
use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{Instant, sleep, timeout};

const NEW_TOKEN: &str = "reloaded-token-from-sighup-test";

#[tokio::test]
async fn sighup_reloads_http_token() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let data_dir = dir.path();
    let socket_path = data_dir.join("pacto-bot-api.sock");
    let log_path = data_dir.join("daemon.log");
    let config_path = write_config(data_dir).await?;

    // Pre-create the token so the test knows the initial secret.
    write_token(data_dir, "initial-token").await?;

    let log_file = File::create(&log_path)?;

    let mut child = Command::new(common::daemon_bin_path()?)
        .arg("--config")
        .arg(&config_path)
        .arg("--enable-http")
        .stdout(Stdio::from(log_file.try_clone()?))
        .stderr(Stdio::from(log_file))
        .env("RUST_LOG", "info")
        .spawn()?;

    common::wait_for_socket(&socket_path, Duration::from_secs(15)).await?;

    let port = parse_http_port(&log_path).await?;

    let body = serialize_message(&JsonRpcMessage::request(
        Value::Number(1.into()),
        "agent.metrics",
        None,
    ))?;

    let old_token = read_token(data_dir).await?;
    assert_request_succeeds(port, &old_token, &body).await?;

    write_token(data_dir, NEW_TOKEN).await?;
    send_sighup(&mut child)?;
    wait_for_log(
        &log_path,
        "HTTP secret token reloaded",
        Duration::from_secs(5),
    )
    .await?;

    assert_request_rejected(port, &old_token, &body).await?;
    assert_request_succeeds(port, NEW_TOKEN, &body).await?;

    common::shutdown_daemon(child).await?;
    Ok(())
}

async fn write_config(data_dir: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let keys = nostr::Keys::generate();
    let npub = keys.public_key().to_bech32()?;
    let nsec = keys.secret_key().to_bech32()?;
    let socket_path = data_dir.join("pacto-bot-api.sock");

    let content = format!(
        "[daemon]\n\
         data_dir = {:?}\n\
         socket_path = {:?}\n\
         http_bind = \"127.0.0.1:0\"\n\n\
         [[bots]]\n\
         id = \"reload-bot\"\n\
         display_name = \"Reload Bot\"\n\
         npub = {:?}\n\
         signing = {{ backend = \"nsec\", nsec = {:?} }}\n\
         relays = [\"wss://127.0.0.1:65535\"]\n\
         capabilities = []\n",
        data_dir.to_string_lossy(),
        socket_path.to_string_lossy(),
        npub,
        nsec,
    );

    let path = data_dir.join("pacto-bot-api.toml");
    fs::write(&path, content)?;
    let mut perms = fs::metadata(&path)?.permissions();
    perms.set_mode(0o600);
    fs::set_permissions(&path, perms)?;
    Ok(path)
}

async fn write_token(data_dir: &Path, token: &str) -> Result<(), Box<dyn Error>> {
    let path = data_dir.join("bot_secret_token");
    fs::write(&path, token)?;
    let mut perms = fs::metadata(&path)?.permissions();
    perms.set_mode(0o600);
    fs::set_permissions(&path, perms)?;
    Ok(())
}

async fn read_token(data_dir: &Path) -> Result<String, Box<dyn Error>> {
    let contents = tokio::fs::read_to_string(data_dir.join("bot_secret_token")).await?;
    Ok(contents.trim().to_string())
}

fn send_sighup(child: &mut Child) -> Result<(), Box<dyn Error>> {
    kill(Pid::from_raw(child.id() as i32), Signal::SIGHUP)?;
    Ok(())
}

async fn assert_request_succeeds(port: u16, token: &str, body: &str) -> Result<(), Box<dyn Error>> {
    let response = raw_http_post(port, Some(token), body).await?;
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "expected 200 with valid token, got: {response}"
    );
    Ok(())
}

async fn assert_request_rejected(port: u16, token: &str, body: &str) -> Result<(), Box<dyn Error>> {
    let response = raw_http_post(port, Some(token), body).await?;
    assert!(
        response.starts_with("HTTP/1.1 401"),
        "expected 401 with stale token, got: {response}"
    );
    Ok(())
}

async fn raw_http_post(
    port: u16,
    secret: Option<&str>,
    body: &str,
) -> Result<String, Box<dyn Error>> {
    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}")).await?;

    let secret_header = secret
        .map(|s| format!("X-Pacto-Bot-Secret: {s}\r\n"))
        .unwrap_or_default();

    let request = format!(
        "POST / HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         {secret_header}\r\n\
         {body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;

    let mut buf = vec![0u8; 4096];
    let n = timeout(Duration::from_secs(2), stream.read(&mut buf))
        .await
        .map_err(|_| "timed out reading HTTP response")??;
    buf.truncate(n);
    Ok(String::from_utf8_lossy(&buf).to_string())
}

async fn parse_http_port(log_path: &Path) -> Result<u16, Box<dyn Error>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let log = tokio::fs::read_to_string(log_path)
            .await
            .unwrap_or_default();
        for line in log.lines() {
            if !line.contains("localhost HTTP transport bound") {
                continue;
            }
            if let Some(start) = line.find("127.0.0.1:") {
                let rest = &line[start + "127.0.0.1:".len()..];
                if let Some(end) = rest.find(|c: char| !c.is_ascii_digit()) {
                    if let Ok(port) = rest[..end].parse::<u16>() {
                        return Ok(port);
                    }
                } else if let Ok(port) = rest.parse::<u16>() {
                    return Ok(port);
                }
            }
        }
        if Instant::now() >= deadline {
            return Err("HTTP port not found in daemon logs".into());
        }
        sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_log(
    log_path: &Path,
    needle: &str,
    within: Duration,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + within;
    loop {
        let log = tokio::fs::read_to_string(log_path)
            .await
            .unwrap_or_default();
        if log.contains(needle) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("did not find '{needle}' in daemon logs").into());
        }
        sleep(Duration::from_millis(50)).await;
    }
}
