mod common;
mod support;

use assert_cmd::Command;
use nostr::ToBech32;
use predicates::str::contains;
use serde_json::Value;
use std::time::Duration;

/// Spawn a daemon and return a guard that kills it on drop.
struct DaemonGuard(std::process::Child);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn doctor_reports_config_and_daemon_status() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let config = common::make_config(&dir, vec![])?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.arg("--config").arg(&config).arg("doctor");
    cmd.assert()
        .failure()
        .stdout(contains("[PASS] config:"))
        .stdout(contains("[FAIL] daemon_lock:"))
        .stdout(contains("[FAIL] bots:"))
        .stdout(contains("checks passed, 2 failed"));

    Ok(())
}

#[tokio::test]
async fn trace_events_prints_recent_rows() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot])?;
    let db_path = dir.path().join("agent.db");

    let db = pacto_bot_api::db::Db::open(&db_path).await?;
    db.save_event_trace(
        "echo-bot",
        "event-id-123",
        "author-npub",
        "hello preview",
        "reply",
        Some("reply-id-456"),
    )
    .await?;
    db.save_event_trace(
        "echo-bot",
        "event-id-789",
        "author-npub",
        "unknown command",
        "ignore",
        None,
    )
    .await?;
    drop(db);

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.arg("--config")
        .arg(&config)
        .arg("trace-events")
        .arg("echo-bot")
        .arg("--since")
        .arg("1")
        .arg("--limit")
        .arg("10");
    cmd.assert()
        .success()
        .stdout(contains("event-id-123"))
        .stdout(contains("event-id-789"))
        .stdout(contains("reply-id-456"))
        .stdout(contains("ignore"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn diagnose_json_contains_socket_fields_when_daemon_stopped() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let config = common::make_config(&dir, vec![])?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &config.to_string_lossy(),
        "diagnose",
        "--format",
        "json",
    ]);
    let output = cmd.assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;
    let report: Value = serde_json::from_str(stdout)?;

    let socket = report
        .get("socket")
        .expect("diagnose JSON should include socket");
    assert_eq!(socket["exists"], false);
    assert_eq!(socket["owner_readable"], false);
    assert_eq!(socket["owner_writable"], false);
    assert!(
        socket["path"].as_str().expect("socket path should be a string").ends_with("pacto-bot-api.sock"),
        "socket path should end with the expected socket filename, got: {socket:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn diagnose_json_reports_connectivity_and_service_versions_with_mock_relay() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let relay = support::mock_relay::MockRelay::start().await?;
    let relay_url = relay.url();

    let (mut bot, _nsec) = common::generate_nsec_bot("diagnose-bot")?;
    bot.relays = vec![relay_url.clone()];
    bot.capabilities = vec!["ReadMessages".into()];
    let config = common::make_config(&dir, vec![bot])?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &config.to_string_lossy(),
        "diagnose",
        "--format",
        "json",
    ]);
    cmd.env("PACTO_DEV_ENV", "1");
    let output = cmd.assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;
    let report: Value = serde_json::from_str(stdout)?;

    let relay_checks = report["relay_connectivity"]
        .as_array()
        .expect("relay_connectivity should be an array");
    assert!(
        relay_checks.iter().any(|c| {
            c["bot_id"] == "diagnose-bot"
                && c["relay"] == relay_url
                && c["reachable"] == true
        }),
        "expected reachable relay check for diagnose-bot, got: {relay_checks:?}"
    );

    let bunker_checks = report["bunker_connectivity"]
        .as_array()
        .expect("bunker_connectivity should be an array");
    assert_eq!(
        bunker_checks.len(),
        0,
        "nsec bot should have no bunker connectivity entry"
    );

    let service_versions = report
        .get("service_versions")
        .expect("service_versions should be present under PACTO_DEV_ENV=1");
    assert!(
        service_versions.get("relay").is_some(),
        "service_versions.relay should be probed under PACTO_DEV_ENV=1"
    );
    assert!(
        service_versions.get("evm_node").is_some(),
        "service_versions.evm_node should be probed under PACTO_DEV_ENV=1"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn diagnose_text_includes_socket_connectivity_and_service_sections() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let relay = support::mock_relay::MockRelay::start().await?;
    let relay_url = relay.url();

    let (mut bot, _nsec) = common::generate_nsec_bot("diagnose-bot")?;
    bot.relays = vec![relay_url.clone()];
    bot.capabilities = vec!["ReadMessages".into()];
    let config = common::make_config(&dir, vec![bot])?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args(["--config", &config.to_string_lossy(), "diagnose"]);
    cmd.env("PACTO_DEV_ENV", "1");
    let output = cmd.assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;

    assert!(stdout.contains("socket:"), "text output should include socket section\n{stdout}");
    assert!(stdout.contains("path:"), "text output should include socket path\n{stdout}");
    assert!(stdout.contains("exists:"), "text output should include socket exists\n{stdout}");
    assert!(
        stdout.contains("owner_readable:"),
        "text output should include socket owner_readable\n{stdout}"
    );
    assert!(
        stdout.contains("owner_writable:"),
        "text output should include socket owner_writable\n{stdout}"
    );
    assert!(
        stdout.contains("relay_connectivity:"),
        "text output should include relay_connectivity section\n{stdout}"
    );
    assert!(
        stdout.contains("bunker_connectivity:"),
        "text output should include bunker_connectivity section\n{stdout}"
    );
    assert!(
        stdout.contains("service_versions:"),
        "text output should include service_versions section under PACTO_DEV_ENV=1\n{stdout}"
    );
    assert!(
        stdout.contains(&relay_url),
        "text output should mention the mock relay URL\n{stdout}"
    );

    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn send_test_dm_publishes_gift_wrap() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let relay = support::mock_relay::MockRelay::start().await?;
    let (mut bot, _nsec) = common::generate_nsec_bot("send-bot")?;
    bot.relays = vec![relay.url()];
    bot.capabilities = vec!["ReadMessages".into(), "Admin".into()];
    let config = common::make_config(&dir, vec![bot])?;

    let _daemon = DaemonGuard(common::spawn_daemon_until_ready(&config).await?);

    let recipient_keys = nostr::Keys::generate();
    let recipient = recipient_keys.public_key().to_bech32()?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.arg("--config")
        .arg(&config)
        .arg("send-test-dm")
        .arg("send-bot")
        .arg(&recipient)
        .arg("integration test message");
    let output = cmd.assert().success();
    let stdout = String::from_utf8(output.get_output().stdout.clone())?;
    let event_id = stdout.trim();
    assert!(
        !event_id.is_empty() && event_id.len() == 64,
        "expected a 64-char hex event id, got: {event_id:?}"
    );

    // Give the relay a moment to receive the published gift wrap.
    let events = relay
        .wait_for_event(|_| true, Duration::from_secs(5))
        .await?;
    assert!(
        !events.is_empty(),
        "expected at least one event published to the mock relay"
    );

    Ok(())
}
