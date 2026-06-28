mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;
use std::time::Duration;

use pacto_bot_api::config::DaemonConfig;
use pacto_bot_api::errors::DaemonError;
use pacto_bot_api::signer::{BunkerConnection, LocalKey};
use pacto_bot_api::transport::http::HttpTransport;
use pacto_bot_api::transport::message_handler;
use pacto_bot_api::transport::protocol::{JsonRpcMessage, serialize_message};
use support::secret_scan::{
    SensitiveFixture, assert_no_leak, assert_no_leak_bytes, capture_logs_during, strings_output,
    write_config_file,
};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// Path to a release build of `pacto-bot-api`, built on first use into a
/// separate target directory to avoid locking the default `target` tree while
/// tests run under `cargo test`.
fn release_binary_path() -> Result<&'static Path, &'static String> {
    static PATH: LazyLock<Result<PathBuf, String>> = LazyLock::new(|| {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let target_dir = manifest.join("target/secret-scan-release");
        let binary = target_dir.join("release/pacto-bot-api");

        if !binary.exists() {
            let status = Command::new("cargo")
                .args([
                    "build",
                    "--release",
                    "--bin",
                    "pacto-bot-api",
                    "--target-dir",
                ])
                .arg(&target_dir)
                .status()
                .map_err(|e| format!("failed to spawn cargo build: {e}"))?;
            if !status.success() {
                return Err("release build of pacto-bot-api failed".into());
            }
        }

        Ok(binary)
    });
    PATH.as_ref().map(|p| p.as_path())
}

#[test]
fn capture_logs_during_helper_records_events_without_leaks() {
    let fixture = SensitiveFixture::new();
    let (_, logs) = capture_logs_during(|| {
        tracing::warn!(target: "secret_scan_test", "synthetic warning event");
    });
    assert!(logs.contains("synthetic warning event"));
    assert_no_leak(&logs, &fixture);
}

#[test]
fn startup_logs_warn_about_nsec_without_leaking_marker() {
    let fixture = SensitiveFixture::new();
    let dir = TempDir::new().unwrap();
    let config = format!(
        r#"
[daemon]
data_dir = "{}"

[[bots]]
id = "leak-test-bot"
npub = "npub1leaktest"
signing = {{ backend = "nsec", nsec = "{}" }}
"#,
        dir.path().join("data").to_string_lossy(),
        fixture.nsec_marker
    );
    let config_path = write_config_file(dir.path(), &config).unwrap();

    let output = Command::new(release_binary_path().unwrap())
        .env("RUST_LOG", "debug")
        .arg("--config")
        .arg(&config_path)
        .arg("--data-dir")
        .arg(dir.path().join("data"))
        .output()
        .expect("failed to run daemon binary");

    let logs = String::from_utf8_lossy(&output.stdout);
    assert!(
        logs.contains("local test key (nsec) in use"),
        "expected nsec warning in logs: {logs}"
    );
    assert_no_leak(&logs, &fixture);
}

#[test]
fn bunker_connection_error_does_not_leak_uri_marker() {
    let fixture = SensitiveFixture::new();
    let expected_keys = nostr::Keys::generate();
    let other_keys = nostr::Keys::generate();

    // A syntactically valid bunker URI whose remote signer pubkey does not
    // match the configured bot pubkey. The URI embeds the synthetic marker so
    // that any echo of the URI in the error would be detected.
    let uri = format!(
        "bunker://{}?relay=wss://relay-{}.example.com",
        other_keys.public_key().to_hex(),
        fixture.bunker_uri_marker
    );

    let err = BunkerConnection::connect(&uri, &expected_keys.public_key(), true).unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("pubkey"),
        "expected pubkey mismatch error, got: {msg}"
    );
    assert_no_leak(&msg, &fixture);
}

#[tokio::test]
async fn http_401_response_body_does_not_echo_token_marker()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = SensitiveFixture::new();
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().to_path_buf();
    write_token_file(&data_dir, &fixture.http_token_marker).await?;

    let (port, shutdown_tx, _handle) = start_http_server(&data_dir, echo_handler()).await?;

    let response = raw_http_post(port, Some("wrong-token"), "{}").await?;
    assert!(
        response.starts_with("HTTP/1.1 401"),
        "expected 401, got: {response}"
    );
    assert_no_leak(&response, &fixture);

    let _ = shutdown_tx.send(());
    Ok(())
}

#[tokio::test]
async fn json_rpc_error_response_does_not_contain_secret_markers()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = SensitiveFixture::new();
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().to_path_buf();
    let token = "known-test-token";
    write_token_file(&data_dir, token).await?;

    let error_handler = message_handler(|_| async move {
        Err::<Option<JsonRpcMessage>, DaemonError>(DaemonError::MethodNotFound)
    });

    let (port, shutdown_tx, _handle) = start_http_server(&data_dir, error_handler).await?;

    // Embed every synthetic marker in the request parameters so that a naive
    // echo of the input would be caught, while the legitimate error path should
    // never return them.
    let params = serde_json::json!({
        "nsec": fixture.nsec_marker,
        "bunker_uri": fixture.bunker_uri_marker,
        "http_token": fixture.http_token_marker,
    });
    let body = serialize_message(&JsonRpcMessage::request(
        7.into(),
        "agent.metrics",
        Some(params),
    ))?;
    let response = raw_http_post(port, Some(token), &body).await?;

    assert!(
        response.starts_with("HTTP/1.1 200"),
        "expected 200, got: {response}"
    );
    assert!(
        response.contains("error"),
        "expected JSON-RPC error: {response}"
    );
    assert_no_leak(&response, &fixture);

    let _ = shutdown_tx.send(());
    Ok(())
}

#[test]
fn release_binary_strings_contain_no_synthetic_markers() {
    let fixture = SensitiveFixture::new();
    let binary = release_binary_path().unwrap();
    let Some(strings) = strings_output(binary) else {
        // `strings(1)` is unavailable on this platform; skip.
        return;
    };
    assert_no_leak(&strings, &fixture);
}

#[test]
fn config_parse_error_nsec_backend_does_not_leak_marker() {
    let fixture = SensitiveFixture::new();
    let dir = TempDir::new().unwrap();
    let config = format!(
        r#"
[[bots]]
id = "leak-test-bot"
npub = "{}"
signing = {{ backend = "nsec", nsec = "" }}
"#,
        fixture.nsec_marker
    );
    let path = write_config_file(dir.path(), &config).unwrap();

    let err = DaemonConfig::load(&path).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("non-empty nsec"),
        "expected nsec validation error, got: {msg}"
    );
    assert_no_leak(&msg, &fixture);
}

#[test]
fn simulated_core_dump_after_nsec_load_does_not_leak_marker() {
    let fixture = SensitiveFixture::new();

    // Load the synthetic nsec into the local signer, exercising the existing
    // secrecy / zeroize path.
    let signer = LocalKey::parse(&fixture.nsec_marker).unwrap();
    drop(signer);

    let Some(memory) = fixture.scan_memory() else {
        // Core-dump simulation is only implemented on Linux.
        return;
    };
    assert_no_leak_bytes(&memory, &fixture);
}

async fn write_token_file(data_dir: &Path, token: &str) -> std::io::Result<()> {
    let path = data_dir.join("bot_secret_token");
    tokio::fs::write(&path, token).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        tokio::fs::set_permissions(&path, perms).await?;
    }
    Ok(())
}

fn echo_handler() -> pacto_bot_api::transport::MessageHandler {
    message_handler(|msg| async move {
        let id = msg.id().cloned().unwrap_or(serde_json::Value::Null);
        Ok(Some(JsonRpcMessage::response(
            id,
            Some(serde_json::Value::String("pong".into())),
        )))
    })
}

async fn start_http_server(
    data_dir: &Path,
    handler: pacto_bot_api::transport::MessageHandler,
) -> Result<(u16, oneshot::Sender<()>, tempfile::TempDir), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

    let transport = HttpTransport::new("127.0.0.1:0", data_dir).with_max_frame_size(1024);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    tokio::spawn(async move {
        let _ = transport
            .run_with_listener(listener, handler, shutdown_rx)
            .await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    Ok((port, shutdown_tx, dir))
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
