mod common;
mod support;

/// Dev-env integration tests against the Docker services started by
/// `pacto-dev-env` (or the equivalent `dev-setup/docker-compose.yml`).
///
/// These tests are ignored by default. Run them with:
///
/// ```bash
/// PACTO_DEV_ENV=1 cargo test -- --ignored
/// ```
///
/// The relay is expected at `ws://localhost:7000`, the EVM node at
/// `http://localhost:8545`, and the optional NIP-46 bunker at
/// `http://127.0.0.1:3001`.
use std::time::Duration;

use assert_cmd::Command;
use nostr::Keys;
use pacto_bot_api::transport::protocol::JsonRpcMessage;
use serde_json::Value;

const DEV_RELAY: &str = common::dev_relay_url();
const DEV_EVM: &str = common::dev_evm_url();

#[tokio::test]
#[ignore = "requires pacto-dev-env Docker services (set PACTO_DEV_ENV=1)"]
async fn dev_env_relay_accepts_websocket() -> Result<(), Box<dyn std::error::Error>> {
    if !common::skip_unless_dev_env() {
        return Ok(());
    }

    let (_ws, _resp) = tokio_tungstenite::connect_async(DEV_RELAY)
        .await
        .map_err(|e| format!("dev-env relay at {DEV_RELAY} is not reachable: {e}"))?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires pacto-dev-env Docker services (set PACTO_DEV_ENV=1)"]
async fn dev_env_evm_rpc_reachable() -> Result<(), Box<dyn std::error::Error>> {
    if !common::skip_unless_dev_env() {
        return Ok(());
    }

    // The EVM RPC port must accept TCP connections. A full JSON-RPC check is
    // intentionally avoided to keep the dev-env test suite free of extra HTTP
    // client dependencies.
    match tokio::net::TcpStream::connect("127.0.0.1:8545").await {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("dev-env EVM RPC at {DEV_EVM} is not reachable: {e}").into()),
    }
}

#[tokio::test]
#[ignore = "requires pacto-dev-env Docker services (set PACTO_DEV_ENV=1)"]
async fn dev_env_admin_diagnose_reports_relay_ok() -> Result<(), Box<dyn std::error::Error>> {
    if !common::skip_unless_dev_env() {
        return Ok(());
    }

    let dir = common::tempdir()?;
    let (mut bot_config, _nsec) = common::generate_nsec_bot("diagnose-bot")?;
    bot_config.relays = vec![DEV_RELAY.to_string()];
    bot_config.capabilities = vec!["ReadMessages".into()];
    let config_path = common::make_config(&dir, vec![bot_config])?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &config_path.to_string_lossy(),
        "diagnose",
        "--format",
        "json",
    ]);

    let output = tokio::task::spawn_blocking(move || cmd.assert().success())
        .await
        .map_err(|e| format!("diagnose command panicked: {e}"))?
        .get_output()
        .clone();

    let stdout = std::str::from_utf8(&output.stdout)?;
    let report: Value = serde_json::from_str(stdout)
        .map_err(|e| format!("diagnose output is not valid JSON: {e}\n{stdout}"))?;

    assert_eq!(
        report.get("config_valid").and_then(Value::as_bool),
        Some(true),
        "diagnose should report a valid config"
    );

    let relay_checks = report
        .get("relay_connectivity")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(
        !relay_checks.is_empty(),
        "diagnose should report relay connectivity checks"
    );
    let relay_ok = relay_checks.iter().any(|check| {
        check.get("relay").and_then(Value::as_str) == Some(DEV_RELAY)
            && check.get("reachable").and_then(Value::as_bool) == Some(true)
    });
    assert!(
        relay_ok,
        "diagnose should report the dev-env relay as reachable: {relay_checks:?}"
    );

    Ok(())
}

#[tokio::test]
#[ignore = "requires pacto-dev-env Docker services (set PACTO_DEV_ENV=1)"]
async fn dev_env_daemon_dm_round_trip_over_unix_socket() -> Result<(), Box<dyn std::error::Error>> {
    if !common::skip_unless_dev_env() {
        return Ok(());
    }

    let dir = common::tempdir()?;
    let (bot_config, _nsec) = common::generate_nsec_bot("echo-bot")?;

    let mut config_bots = bot_config.clone();
    config_bots.relays = vec![DEV_RELAY.to_string()];
    config_bots.capabilities = vec!["ReadMessages".into(), "SendMessages".into()];
    let config_path = common::make_config(&dir, vec![config_bots])?;

    // Keep a relay client subscribed to the sender's replies for the whole
    // round trip so the subscription is active before the daemon publishes.
    let sender = Keys::generate();
    let mut relay_client = common::DevRelayClient::new(DEV_RELAY, &sender.public_key()).await?;

    let log_path = dir.path().join("daemon.log");
    let daemon = common::spawn_daemon_until_ready_with_log(&config_path, Some(&log_path)).await?;

    let socket_path = dir.path().join("pacto-bot-api.sock");
    let mut handler = common::HandlerClient::register(
        &socket_path,
        &["echo-bot"],
        &["dm_received"],
        &["ReadMessages", "SendMessages"],
    )
    .await?;

    let gift = common::build_gift_wrap(&sender, &bot_config.npub, "/echo dev-env").await?;
    relay_client.publish(&gift).await?;

    let result: Result<(), Box<dyn std::error::Error>> = async {
        let notification = handler.next_notification(Duration::from_secs(15)).await?;
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
                assert_eq!(content, "/echo dev-env");
                event_id
            }
            _ => {
                return Err(
                    format!("expected agent.event notification, got {:?}", notification).into(),
                );
            }
        };

        handler
            .send_response(&event_id, "reply", Some("dev-env hello"))
            .await?;

        let _reply = relay_client
            .wait_for_reply(&sender.public_key(), Duration::from_secs(15))
            .await
            .map_err(|e| format!("daemon did not publish reply gift wrap to relay: {e}"))?;

        Ok(())
    }
    .await;

    common::shutdown_daemon(daemon).await?;

    if let Err(e) = &result {
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        eprintln!("daemon log:\n{log}");
        return Err(format!("{e}").into());
    }

    Ok(())
}
