mod common;
mod support;

use assert_cmd::Command;
use nostr::ToBech32;
use predicates::str::contains;
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

#[cfg(unix)]
#[tokio::test]
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
