mod common;
mod support;

/// req(R10, R29, R31, R35)
use assert_cmd::Command;
use pacto_bot_api::db::{Database, MlsGroupRow};
use pacto_bot_api::events::EventType;
use rusqlite::Connection;
use serde_json::json;
use std::error::Error;
use std::fs;

fn assert_mls_table_schema(
    conn: &Connection,
    table: &str,
    expected_columns: &[&str],
    expected_pk: &[&str],
    unique_column: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns: Vec<(String, i32, bool)> = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>("name")?,
                row.get::<_, i32>("pk")?,
                row.get::<_, bool>("notnull")?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let names: std::collections::HashSet<&str> = columns.iter().map(|c| c.0.as_str()).collect();
    let expected_names: std::collections::HashSet<&str> =
        expected_columns.iter().copied().collect();
    assert_eq!(names, expected_names, "{table} column set mismatch");

    let pk: std::collections::HashSet<&str> = columns
        .iter()
        .filter(|c| c.1 > 0)
        .map(|c| c.0.as_str())
        .collect();
    let expected_pk_set: std::collections::HashSet<&str> = expected_pk.iter().copied().collect();
    assert_eq!(pk, expected_pk_set, "{table} primary key mismatch");

    for col in &columns {
        assert!(col.2, "{table}.{} should be NOT NULL", col.0);
    }

    let mut unique_indexes = Vec::new();
    let mut idx_stmt = conn.prepare(&format!("PRAGMA index_list({table})"))?;
    let indexes: Vec<(String, i32)> = idx_stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>("name")?, row.get::<_, i32>("unique")?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (name, unique) in indexes {
        if unique != 1 {
            continue;
        }
        let mut info_stmt = conn.prepare(&format!("PRAGMA index_info({name})"))?;
        let cols: Vec<String> = info_stmt
            .query_map([], |row| row.get::<_, String>("name"))?
            .collect::<Result<Vec<_>, _>>()?;
        unique_indexes.push(cols);
    }

    let pk_index: Vec<String> = expected_pk.iter().map(|s| (*s).to_string()).collect();
    assert!(
        unique_indexes.contains(&pk_index),
        "{table} primary key index missing"
    );

    if let Some(unique_col) = unique_column {
        let unique_col_index: Vec<String> = vec![unique_col.to_string()];
        assert!(
            unique_indexes.contains(&unique_col_index),
            "{table}.{unique_col} unique index missing"
        );
    }

    Ok(())
}

fn assert_mls_tables_in_schema(path: &std::path::Path) -> Result<(), Box<dyn Error>> {
    let conn = Connection::open(path)?;
    assert_mls_table_schema(
        &conn,
        "mls_groups",
        &[
            "bot_id",
            "group_name",
            "wire_id",
            "creator_npub",
            "relay",
            "invited_bots",
        ],
        &["bot_id", "group_name"],
        Some("wire_id"),
    )?;
    assert_mls_table_schema(
        &conn,
        "mls_group_members",
        &["bot_id", "group_name", "member_npub"],
        &["bot_id", "group_name", "member_npub"],
        None,
    )?;

    for table in ["mls_groups", "mls_group_members"] {
        let mut stmt = conn.prepare(&format!("PRAGMA foreign_key_list({table})"))?;
        let count: usize = stmt.query_map([], |_| Ok(()))?.count();
        assert_eq!(count, 0, "{table} should declare no foreign keys");
    }

    Ok(())
}

#[test]
fn export_import_roundtrip() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
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
fn export_import_roundtrips_mls_groups() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot.clone()])?;

    // Seed an MLS group row for the bot.
    {
        let db = Database::open(&dir.path().join("agent.db"))?;
        let row = MlsGroupRow {
            bot_id: "echo-bot".to_string(),
            group_name: "my-squad".to_string(),
            wire_id: "aabbccdd".to_string(),
            creator_npub: bot.npub.clone(),
            relay: "wss://relay.example.com".to_string(),
            invited_bots: vec!["npub1member".to_string()],
        };
        db.insert_mls_group_export(&row)?;
    }

    // Export
    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args(["--config", &config.to_string_lossy(), "export", "echo-bot"]);
    let output = cmd.assert().success();
    let state_json = std::str::from_utf8(&output.get_output().stdout)?;

    let state: serde_json::Value = serde_json::from_str(state_json)?;
    let mls_groups = state["mls_groups"].as_array().expect("mls_groups array");
    assert_eq!(mls_groups.len(), 1);
    assert_eq!(mls_groups[0]["group_name"], "my-squad");
    assert_eq!(mls_groups[0]["wire_id"], "aabbccdd");
    assert_eq!(mls_groups[0]["invited_bots"], json!(["npub1member"]));

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
    let groups = db.load_all_mls_groups("echo-bot")?;
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].group_name, "my-squad");
    assert_eq!(groups[0].wire_id, "aabbccdd");
    assert_eq!(groups[0].invited_bots, vec!["npub1member"]);
    assert_mls_tables_in_schema(&dir.path().join("agent.db"))?;
    Ok(())
}

#[test]
fn migration_creates_mls_tables() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let _config = common::make_config(&dir, vec![bot])?;

    let db_path = dir.path().join("agent.db");
    let _db = Database::open(&db_path)?;
    drop(_db);

    assert_mls_tables_in_schema(&db_path)?;
    Ok(())
}

#[test]
fn export_refuses_when_daemon_lock_held() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
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
    let dir = common::tempdir()?;
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
    let dir = common::tempdir()?;
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
    let dir = common::tempdir()?;
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
    let dir = common::tempdir()?;
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
    let dir = common::tempdir()?;
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
    let dir = common::tempdir()?;
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
    let dir = common::tempdir()?;
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
    let dir = common::tempdir()?;
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
    let dir = common::tempdir()?;
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
    let dir = common::tempdir()?;
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
