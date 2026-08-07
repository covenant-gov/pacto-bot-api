mod common;

/// req(R31)
use assert_cmd::Command;
use std::error::Error;
use std::time::Duration;

#[tokio::test]
async fn status_reports_live_daemon_metrics() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot])?;

    let child = common::spawn_daemon_until_ready(&config).await?;

    // Wait until the daemon reports ready via the admin CLI.
    wait_until_status_ready(&config, Duration::from_secs(10)).await?;

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
    let dir = common::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot])?;

    let child = common::spawn_daemon_until_ready(&config).await?;
    wait_until_status_ready(&config, Duration::from_secs(10)).await?;

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
    let dir = common::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot])?;

    let child = common::spawn_daemon_until_ready(&config).await?;
    wait_until_status_ready(&config, Duration::from_secs(10)).await?;
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

/// Poll the admin CLI until the daemon reports `daemon_status: ready`.
async fn wait_until_status_ready(
    config: &std::path::Path,
    timeout: Duration,
) -> Result<(), Box<dyn Error>> {
    let deadline = tokio::time::Instant::now() + timeout;
    let config = config.to_path_buf();
    loop {
        let ready = tokio::task::spawn_blocking({
            let config = config.clone();
            move || -> Result<bool, Box<dyn Error + Send + Sync>> {
                let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
                cmd.args([
                    "--config",
                    &config.to_string_lossy(),
                    "status",
                    "--format",
                    "json",
                ]);
                let output = cmd.output()?;
                if !output.status.success() {
                    return Ok(false);
                }
                let stdout = std::str::from_utf8(&output.stdout)?;
                let report: serde_json::Value = serde_json::from_str(stdout.trim())?;
                Ok(report["daemon_status"].as_str() == Some("ready"))
            }
        })
        .await;
        if let Ok(Ok(true)) = ready {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("timeout waiting for daemon status ready".into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// U15: diagnostics, status, and the stuck-bot warning log (R6, R41)
// ---------------------------------------------------------------------------

use pacto_bot_api::client_manager::ClientManager;
use pacto_bot_api::config::{DaemonConfig, GlobalDaemonConfig};
use pacto_bot_api::db::{Database, Db, MlsGroupRow};
use pacto_bot_api::diagnostics::Diagnostics;
use pacto_bot_api::dispatch::Dispatch;
use pacto_bot_api::handlers::ConnectionHandle;
use pacto_bot_api::nostr::NostrClient;
use pacto_bot_api::transport::protocol::JsonRpcMessage;
use std::sync::Arc;
use tokio::sync::RwLock;

const AGENT_DB_FILE: &str = "agent.db";

/// `pacto-bot-admin diagnose --format json` reports the MDK version, the
/// MLS wire generation, and both vendored crypto versions unconditionally
/// -- these are compile-time-pinned facts, not live daemon state, so no
/// daemon needs to be running.
#[test]
fn diagnose_json_reports_version_info() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
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

    let version_info = &report["version_info"];
    assert_eq!(version_info["mdk_version"].as_str(), Some("0.8.0"));
    assert!(
        !version_info["mls_wire_generation"]
            .as_str()
            .unwrap_or("")
            .is_empty()
    );
    assert!(
        !version_info["vendored_openssl_version"]
            .as_str()
            .unwrap_or("")
            .is_empty()
    );
    assert!(
        !version_info["vendored_sqlcipher_version"]
            .as_str()
            .unwrap_or("")
            .is_empty()
    );
    assert!(
        !version_info["daemon_version"]
            .as_str()
            .unwrap_or("")
            .is_empty()
    );
    Ok(())
}

/// A bot whose store was reset reports the reset timestamp; a bot that was
/// never reset reports absence, not a zero timestamp.
#[test]
fn diagnose_json_reports_reset_at_presence_and_absence() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let (reset_bot, _nsec1) = common::generate_nsec_bot("reset-bot")?;
    let (fresh_bot, _nsec2) = common::generate_nsec_bot("fresh-bot")?;
    let config = common::make_config(&dir, vec![reset_bot, fresh_bot])?;

    let reset_at_ts = chrono::Utc::now().timestamp() - 1000;
    {
        let db = Database::open(&dir.path().join(AGENT_DB_FILE))?;
        db.mark_mls_store_reset_start("reset-bot", reset_at_ts - 10)?;
        db.complete_mls_store_reset("reset-bot", reset_at_ts, None)?;
    }

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

    let bots = report["bots"].as_array().expect("bots array");
    let reset_bot_report = bots
        .iter()
        .find(|b| b["id"] == "reset-bot")
        .expect("reset-bot in report");
    assert!(
        reset_bot_report["reset_at"].is_string(),
        "reset-bot should report a reset_at timestamp, got {reset_bot_report}"
    );

    let fresh_bot_report = bots
        .iter()
        .find(|b| b["id"] == "fresh-bot")
        .expect("fresh-bot in report");
    assert!(
        fresh_bot_report.get("reset_at").is_none() || fresh_bot_report["reset_at"].is_null(),
        "fresh-bot should report absence of reset_at, not a zero value; got {fresh_bot_report}"
    );
    Ok(())
}

/// A group marked state-lost appears as such and flips to held once a
/// restoring Welcome clears the mark.
#[test]
fn diagnose_json_group_state_flips_after_restoration() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("flip-bot")?;
    let config = common::make_config(&dir, vec![bot.clone()])?;

    {
        let db = Database::open(&dir.path().join(AGENT_DB_FILE))?;
        db.insert_mls_group(&MlsGroupRow {
            bot_id: "flip-bot".into(),
            group_name: "squad-a".into(),
            wire_id: "wire-flip".into(),
            creator_npub: bot.npub.clone(),
            relay: "wss://relay.example".into(),
            invited_bots: vec![],
            state_lost_at: Some(chrono::Utc::now().timestamp()),
        })?;
    }

    let group_state_held = |report: &serde_json::Value| -> bool {
        report["bots"]
            .as_array()
            .and_then(|bots| bots.iter().find(|b| b["id"] == "flip-bot"))
            .and_then(|b| b["mls_groups"].as_array())
            .and_then(|groups| groups.iter().find(|g| g["group_name"] == "squad-a"))
            .and_then(|g| g["state_held"].as_bool())
            .expect("squad-a group present with state_held")
    };

    let run_diagnose = |config: &std::path::Path| -> Result<serde_json::Value, Box<dyn Error>> {
        let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
        cmd.args([
            "--config",
            &config.to_string_lossy(),
            "diagnose",
            "--format",
            "json",
        ]);
        let output = cmd.assert().success();
        let stdout = std::str::from_utf8(&output.get_output().stdout)?.to_string();
        Ok(serde_json::from_str(&stdout)?)
    };

    let before = run_diagnose(&config)?;
    assert!(!group_state_held(&before), "group should start state-lost");

    {
        let db = Database::open(&dir.path().join(AGENT_DB_FILE))?;
        db.clear_mls_group_state_lost("flip-bot", "squad-a")?;
    }

    let after = run_diagnose(&config)?;
    assert!(
        group_state_held(&after),
        "group should flip to held after restoration"
    );
    Ok(())
}

/// The three sole-admin buckets classify correctly: a held sole-admin
/// squad is repairable, a state-lost sole-admin squad is unrestorable, a
/// state-lost squad on a bot whose reset went through the R26
/// (encrypted-store) path is admin-set-unknown, and a two-admin squad
/// appears in none of the three (KTD5/KTD6).
#[test]
fn diagnose_json_sole_admin_buckets_classify_correctly() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("sole-admin-bot")?;
    let config = common::make_config(&dir, vec![bot.clone()])?;
    let now = chrono::Utc::now().timestamp();

    {
        let db = Database::open(&dir.path().join(AGENT_DB_FILE))?;

        // Bot-level reset marker archived via the R26 (encrypted-store)
        // path -- the archive path itself is a synthetic marker string,
        // never a real filesystem path, and this test never asserts it
        // appears anywhere (that is `secret_redaction.rs`'s job).
        db.mark_mls_store_reset_start("sole-admin-bot", now - 100)?;
        db.complete_mls_store_reset(
            "sole-admin-bot",
            now - 90,
            Some("synthetic-archive-marker-r26"),
        )?;

        let group = |name: &str, wire_id: &str, state_lost_at: Option<i64>| MlsGroupRow {
            bot_id: "sole-admin-bot".into(),
            group_name: name.into(),
            wire_id: wire_id.into(),
            creator_npub: bot.npub.clone(),
            relay: "wss://relay.example".into(),
            invited_bots: vec![],
            state_lost_at,
        };
        db.insert_mls_group(&group("repairable", "wire-repair", None))?;
        db.insert_mls_group(&group("unrestorable", "wire-unrestorable", Some(now)))?;
        db.insert_mls_group(&group("shared", "wire-shared", Some(now)))?;
        db.insert_mls_group(&group("unknown", "wire-unknown", Some(now)))?;

        db.upsert_mls_store_reset_admin("sole-admin-bot", "wire-repair", &bot.npub)?;
        db.upsert_mls_store_reset_admin("sole-admin-bot", "wire-unrestorable", &bot.npub)?;
        db.upsert_mls_store_reset_admin("sole-admin-bot", "wire-shared", &bot.npub)?;
        db.upsert_mls_store_reset_admin("sole-admin-bot", "wire-shared", "npub1othersqu4dadm1n")?;
        // "unknown" gets no harvested admin rows at all -- combined with
        // the R26 marker above, that is what makes it unknown rather than
        // simply unclassified.
    }

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

    let names_in = |bucket: &str| -> Vec<String> {
        report["sole_admin_groups"][bucket]
            .as_array()
            .expect("bucket array")
            .iter()
            .map(|g| g["group_name"].as_str().unwrap_or_default().to_string())
            .collect()
    };
    let repairable = names_in("repairable_now");
    let unrestorable = names_in("unrestorable");
    let unknown = names_in("admin_set_unknown");

    assert_eq!(repairable, vec!["repairable".to_string()]);
    assert_eq!(unrestorable, vec!["unrestorable".to_string()]);
    assert_eq!(unknown, vec!["unknown".to_string()]);

    for bucket in [&repairable, &unrestorable, &unknown] {
        assert!(
            !bucket.contains(&"shared".to_string()),
            "a two-admin squad must appear in none of the three buckets"
        );
    }
    Ok(())
}

/// `agent.status` carries the daemon-wide version and MLS wire generation
/// only -- no per-bot group map. Registers a live handler connection
/// in-process (no CLI, no socket) and captures the real notification
/// `Dispatch::broadcast_status` produces.
#[tokio::test]
async fn agent_status_notification_carries_no_per_bot_group_map() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("status-bot")?;
    let config = DaemonConfig {
        daemon: GlobalDaemonConfig::default(),
        bots: vec![bot],
    };
    let client = NostrClient::new(vec![]).await?;
    let db = Db::open(dir.path().join(AGENT_DB_FILE).as_path()).await?;
    let cm = Arc::new(RwLock::new(
        ClientManager::new(dir.path(), config, client, &db).await?,
    ));
    let dispatch = Arc::new(Dispatch::new(cm, db, Diagnostics::new()));

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let register = JsonRpcMessage::request(
        1.into(),
        "handler.register",
        Some(serde_json::json!({
            "bot_ids": ["status-bot"],
            "event_types": Vec::<String>::new(),
            "capabilities": ["ReadMessages"],
        })),
    );
    dispatch
        .handle_message(register, None, Some(ConnectionHandle::new(tx)))
        .await?
        .expect("handler.register response");

    dispatch
        .broadcast_status(pacto_bot_api::diagnostics::DaemonStatus::Ready)
        .await;

    let received = rx.recv().await.expect("should receive agent.status");
    let JsonRpcMessage::Notification { method, params, .. } = received else {
        panic!("expected notification");
    };
    assert_eq!(method, "agent.status");
    let payload = params.expect("params present");

    let obj = payload.as_object().expect("params must be a JSON object");
    let allowed: std::collections::HashSet<&str> = [
        "state",
        "identity",
        "capabilities",
        "daemon_version",
        "mls_wire_generation",
    ]
    .into_iter()
    .collect();
    for key in obj.keys() {
        assert!(
            allowed.contains(key.as_str()),
            "agent.status must carry no per-bot/group field, found: {key}"
        );
    }
    assert!(
        !obj.contains_key("groups") && !obj.contains_key("bots"),
        "agent.status leaked a per-bot group map: {payload}"
    );
    assert_eq!(payload["state"], "ready");
    assert!(
        payload["daemon_version"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );
    assert!(
        payload["mls_wire_generation"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );
    Ok(())
}

/// The daemon's periodic tick warns about a state-lost MLS group once it
/// has been stuck past the configured minimum age, and does not warn
/// about one still inside it.
#[tokio::test(flavor = "multi_thread")]
async fn periodic_tick_warns_past_threshold_not_before() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let (bot, nsec) = common::generate_nsec_bot("stuck-bot")?;

    let data_dir = dir.path().to_string_lossy();
    let socket_path = dir.path().join("pacto-bot-api.sock");
    let display_name = bot.display_name.clone().unwrap_or_default();
    let relays = format!("{:?}", bot.relays);
    let config_content = format!(
        "[daemon]\ndata_dir = {:?}\nsocket_path = {:?}\nstuck_bot_warning_min_age_secs = 60\n\n\
         [[bots]]\nid = {:?}\ndisplay_name = {:?}\nnpub = {:?}\nsigning = {{ backend = \"nsec\", nsec = {:?} }}\nrelays = {relays}\n",
        data_dir, socket_path, bot.id, display_name, bot.npub, nsec,
    );
    let config_path = dir.path().join("pacto-bot-api.toml");
    std::fs::write(&config_path, config_content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&config_path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&config_path, perms)?;
    }

    let now = chrono::Utc::now().timestamp();
    {
        let db = Database::open(&dir.path().join(AGENT_DB_FILE))?;
        db.insert_mls_group(&MlsGroupRow {
            bot_id: "stuck-bot".into(),
            group_name: "old-stuck-group".into(),
            wire_id: "wire-old-stuck".into(),
            creator_npub: bot.npub.clone(),
            relay: "wss://relay.example".into(),
            invited_bots: vec![],
            state_lost_at: Some(now - 120),
        })?;
        db.insert_mls_group(&MlsGroupRow {
            bot_id: "stuck-bot".into(),
            group_name: "fresh-stuck-group".into(),
            wire_id: "wire-fresh-stuck".into(),
            creator_npub: bot.npub.clone(),
            relay: "wss://relay.example".into(),
            invited_bots: vec![],
            state_lost_at: Some(now),
        })?;
    }

    let log_path = dir.path().join("daemon.log");
    let child = common::spawn_daemon_until_ready_with_log(&config_path, Some(&log_path)).await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(55);
    let logs = loop {
        let logs = std::fs::read_to_string(&log_path).unwrap_or_default();
        if logs.contains("old-stuck-group") || tokio::time::Instant::now() >= deadline {
            break logs;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    common::shutdown_daemon(child).await?;

    assert!(
        logs.contains("old-stuck-group"),
        "expected a warning naming the group past the threshold; log:\n{logs}"
    );
    assert!(
        !logs.contains("fresh-stuck-group"),
        "did not expect a warning for the group still inside the threshold; log:\n{logs}"
    );
    Ok(())
}
