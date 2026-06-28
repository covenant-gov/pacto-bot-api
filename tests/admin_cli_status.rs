mod common;

/// req(R31)
use assert_cmd::Command;
use std::error::Error;
use std::time::Duration;

#[tokio::test]
async fn status_reports_live_daemon_metrics() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot])?;

    let child = common::spawn_daemon_until_ready(&config).await?;

    // Give the daemon a moment to finish startup and populate BotHealth.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args(["--config", &config.to_string_lossy(), "status"]);
    let output = tokio::task::spawn_blocking(move || cmd.assert().success()).await?;
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;

    assert!(
        stdout.contains("daemon: running"),
        "expected running daemon, got:\n{stdout}"
    );
    assert!(
        stdout.contains("status: ready"),
        "expected ready status, got:\n{stdout}"
    );
    assert!(
        stdout.contains("uptime:"),
        "expected uptime field, got:\n{stdout}"
    );
    assert!(
        stdout.contains("handlers:"),
        "expected handlers field, got:\n{stdout}"
    );
    assert!(
        stdout.contains("bots:"),
        "expected bots section, got:\n{stdout}"
    );
    assert!(
        stdout.contains("id: echo-bot"),
        "expected bot id, got:\n{stdout}"
    );

    common::shutdown_daemon(child).await?;
    Ok(())
}

#[tokio::test]
async fn status_json_format_reports_expected_fields() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot])?;

    let child = common::spawn_daemon_until_ready(&config).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &config.to_string_lossy(),
        "status",
        "--format",
        "json",
    ]);
    let output = tokio::task::spawn_blocking(move || cmd.assert().success()).await?;
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;

    let report: serde_json::Value = serde_json::from_str(stdout.trim())?;
    assert_eq!(
        report["daemon_running"].as_bool(),
        Some(true),
        "expected daemon_running true, got {report}"
    );
    assert_eq!(
        report["daemon_status"].as_str(),
        Some("ready"),
        "expected daemon_status ready, got {report}"
    );
    assert!(
        report["uptime_seconds"].as_u64().is_some(),
        "expected uptime_seconds, got {report}"
    );
    assert!(
        report["handlers_registered"].as_u64().is_some(),
        "expected handlers_registered, got {report}"
    );
    let bots = report["bots"].as_array().expect("expected bots array");
    assert!(!bots.is_empty(), "expected at least one bot, got {report}");
    let first = &bots[0];
    assert_eq!(first["id"].as_str(), Some("echo-bot"));
    assert!(first["npub"].as_str().is_some());
    assert!(first["relays"].is_array());

    common::shutdown_daemon(child).await?;
    Ok(())
}

#[tokio::test]
async fn status_reads_latest_report_when_daemon_stopped() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot])?;

    let child = common::spawn_daemon_until_ready(&config).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    common::shutdown_daemon(child).await?;

    let latest_path = dir.path().join("reports").join("latest.json");
    assert!(
        latest_path.exists(),
        "latest.json should exist after daemon shutdown"
    );

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args(["--config", &config.to_string_lossy(), "status"]);
    let output = tokio::task::spawn_blocking(move || cmd.assert().success()).await?;
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;

    assert!(
        stdout.contains("daemon: stopped"),
        "expected stopped daemon, got:\n{stdout}"
    );
    assert!(
        stdout.contains("id: echo-bot"),
        "expected bot id from latest.json, got:\n{stdout}"
    );
    Ok(())
}
