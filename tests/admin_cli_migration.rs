mod common;
mod support;

/// req(R10, R29, R31, R35)
use assert_cmd::Command;
use pacto_bot_api::db::Database;
use pacto_bot_api::events::EventType;
use std::error::Error;
use std::fs;

#[test]
fn export_import_roundtrip() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot.clone()])?;

    let handler = common::handler_ref(
        "handler-1",
        &["echo-bot"],
        &[EventType::DmReceived],
        &["ReadMessages"],
    );
    common::populate_db(&dir, "echo-bot", &bot.npub, 42, vec![handler])?;

    // Export
    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args(["--config", &config.to_string_lossy(), "export", "echo-bot"]);
    let output = cmd.assert().success();
    let state_json = std::str::from_utf8(&output.get_output().stdout)?;

    let state: serde_json::Value = serde_json::from_str(state_json)?;
    assert_eq!(state["cursors"].as_array().map(|a| a.len()), Some(1));
    assert_eq!(state["handlers"].as_array().map(|a| a.len()), Some(1));
    assert_eq!(state["split_brain_warning"], true);

    // Save state to file, delete DB, then import
    let state_path = dir.path().join("state.json");
    fs::write(&state_path, state_json)?;
    fs::remove_file(dir.path().join("agent.db"))?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &config.to_string_lossy(),
        "import",
        "echo-bot",
        &state_path.to_string_lossy(),
    ]);
    cmd.assert().success();

    let db = Database::open(&dir.path().join("agent.db"))?;
    let cursor = db
        .load_cursor("echo-bot")?
        .ok_or("cursor missing after import")?;
    assert_eq!(cursor.1, 42);
    let handlers = db.load_handlers()?;
    assert_eq!(handlers.len(), 1);
    Ok(())
}

#[test]
fn export_refuses_when_daemon_lock_held() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot])?;
    let _lock = common::hold_daemon_lock(&dir)?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args(["--config", &config.to_string_lossy(), "export", "echo-bot"]);
    let output = cmd.assert().failure();
    let stderr = std::str::from_utf8(&output.get_output().stderr)?;
    assert!(stderr.contains("daemon lock is held"));
    Ok(())
}

#[test]
fn rotate_http_token_refuses_when_daemon_lock_held() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot])?;
    let _lock = common::hold_daemon_lock(&dir)?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args(["--config", &config.to_string_lossy(), "rotate-http-token"]);
    let output = cmd.assert().failure();
    let stderr = std::str::from_utf8(&output.get_output().stderr)?;
    assert!(stderr.contains("daemon lock is held"));
    Ok(())
}

#[test]
fn validate_config_reports_duplicate_bot_id() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let config_path = dir.path().join("pacto-bot-api.toml");
    let content = r#"
[[bots]]
id = "echo-bot"
npub = "npub1a"
signing = { backend = "nsec", nsec = "nsec1a" }

[[bots]]
id = "echo-bot"
npub = "npub1b"
signing = { backend = "nsec", nsec = "nsec1b" }
"#;
    fs::write(&config_path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&config_path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&config_path, perms)?;
    }

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &config_path.to_string_lossy(),
        "validate-config",
    ]);
    let output = cmd.assert().failure();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;
    assert!(stdout.contains("duplicate bot_id"));
    Ok(())
}

#[test]
fn validate_config_reports_loose_permissions() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let config_path = dir.path().join("pacto-bot-api.toml");
    let content = r#"
[[bots]]
id = "echo-bot"
npub = "npub1a"
signing = { backend = "nsec", nsec = "nsec1a" }
"#;
    common::write_loose_config(&config_path, content)?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &config_path.to_string_lossy(),
        "validate-config",
    ]);
    let output = cmd.assert().failure();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;
    assert!(stdout.contains("must be readable only by owner"));
    Ok(())
}

#[test]
fn rotate_http_token_creates_restricted_token() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot])?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args(["--config", &config.to_string_lossy(), "rotate-http-token"]);
    cmd.assert().success();

    let token_path = dir.path().join("bot_secret_token");
    let token = fs::read_to_string(&token_path)?;
    assert_eq!(token.len(), 64);
    assert!(token.chars().all(|c| c.is_ascii_hexdigit()));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&token_path)?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    Ok(())
}

#[test]
fn diagnose_reports_config_and_lock_status() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot])?;
    let _lock = common::hold_daemon_lock(&dir)?;

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
    let report: serde_json::Value = serde_json::from_str(stdout)?;

    assert_eq!(report["config_valid"], true);
    assert_eq!(report["lock_held"], true);
    assert!(!report["data_dir"].as_str().unwrap_or("").is_empty());
    assert_eq!(report["bots"].as_array().map(|a| a.len()), Some(1));
    assert_eq!(report["db_cursor_count"], 0);

    assert!(
        report.get("socket").is_some(),
        "report should include socket health"
    );
    assert_eq!(report["socket"]["exists"], false);
    assert!(!report["socket"]["path"].as_str().unwrap_or("").is_empty());
    assert!(
        report.get("relay_connectivity").is_some(),
        "report should include relay_connectivity"
    );
    assert!(
        report.get("bunker_connectivity").is_some(),
        "report should include bunker_connectivity"
    );
    assert!(
        report.get("service_versions").is_some(),
        "report should include service_versions"
    );
    Ok(())
}

#[test]
fn diagnose_text_format_reports_bots() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot])?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args(["--config", &config.to_string_lossy(), "diagnose"]);
    let output = cmd.assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;

    assert!(stdout.contains("config_valid: true"));
    assert!(stdout.contains("id: echo-bot"));
    assert!(stdout.contains("signing_backend: nsec"));
    Ok(())
}

#[test]
fn import_validates_bot_exists_in_config() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot])?;

    let state_path = dir.path().join("state.json");
    fs::write(
        &state_path,
        serde_json::json!({
            "metadata": {
                "daemon_version": "0.1.0",
                "exported_at": "2026-01-01T00:00:00Z",
                "source_data_dir": "/tmp"
            },
            "cursors": [],
            "handlers": [],
            "split_brain_warning": true
        })
        .to_string(),
    )?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &config.to_string_lossy(),
        "import",
        "missing-bot",
        &state_path.to_string_lossy(),
    ]);
    let output = cmd.assert().failure();
    let stderr = std::str::from_utf8(&output.get_output().stderr)?;
    assert!(stderr.contains("unknown bot"));
    Ok(())
}

#[test]
fn validate_config_reports_npub_mismatch_with_db() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot.clone()])?;

    // Persist a cursor with a different npub than the config.
    {
        let db = Database::open(&dir.path().join("agent.db"))?;
        db.save_cursor("echo-bot", "npub1other", 7)?;
    }

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args(["--config", &config.to_string_lossy(), "validate-config"]);
    let output = cmd.assert().failure();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;
    assert!(stdout.contains("DB npub") && stdout.contains("does not match config npub"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn diagnose_reports_relay_connectivity_with_mock_relay() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let relay = support::mock_relay::MockRelay::start().await?;
    let relay_url = relay.url();

    let (mut bot, _nsec) = common::generate_nsec_bot("relay-bot")?;
    bot.relays.push(relay_url.clone());
    let config = common::make_config(&dir, vec![bot])?;

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
    let report: serde_json::Value = serde_json::from_str(stdout)?;

    let checks = report["relay_connectivity"]
        .as_array()
        .ok_or("relay_connectivity should be an array")?;
    assert_eq!(checks.len(), 2);
    let live_check = checks
        .iter()
        .find(|c| c["relay"] == relay_url)
        .ok_or("expected check for mock relay")?;
    assert_eq!(live_check["bot_id"], "relay-bot");
    if live_check["reachable"] != true {
        panic!(
            "mock relay should be reachable; got error: {:?}",
            live_check["error"]
        );
    }

    relay.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn diagnose_reports_bunker_connectivity_with_mock_relay() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let relay = support::mock_relay::MockRelay::start().await?;
    let relay_url = relay.url();

    let mut bot = common::generate_bunker_bot("bunker-bot", true)?;
    let bunker_uri = format!(
        "bunker://{}?relay={}",
        nostr::Keys::generate().public_key().to_hex(),
        relay_url
    );
    common::set_bunker_uri(&mut bot, &bunker_uri);
    let config = common::make_config(&dir, vec![bot])?;

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
    let report: serde_json::Value = serde_json::from_str(stdout)?;

    let checks = report["bunker_connectivity"]
        .as_array()
        .ok_or("bunker_connectivity should be an array")?;
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0]["bot_id"], "bunker-bot");
    assert_eq!(checks[0]["reachable"], true);

    relay.stop().await;
    Ok(())
}
