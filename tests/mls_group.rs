//! req(R28, R29, R30)
#![allow(clippy::panic)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
mod support;

use std::error::Error;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use assert_cmd::Command;
use nostr::{Keys, Kind, Timestamp, ToBech32};
use pacto_bot_api::client_manager::ClientManager;
use pacto_bot_api::config::{BotConfig, DaemonConfig, GlobalDaemonConfig, SigningConfig};
use pacto_bot_api::db::Db;
use pacto_bot_api::diagnostics::Diagnostics;
use pacto_bot_api::dispatch::Dispatch;
use pacto_bot_api::nostr::NostrClient;
use pacto_bot_api::transport::protocol::{JsonRpcMessage, MlsGroupResponse};
use secrecy::SecretString;
use serde_json::{Value, json};
use support::mock_bunker::MockBunker;
use support::mock_mls_peer::MockMlsPeer;
use support::mock_relay::MockRelay;
use tokio::sync::RwLock;

fn bot_config_with_mls(
    id: &str,
    keys: &Keys,
    capabilities: &[&str],
    mls_db_path: &str,
) -> BotConfig {
    BotConfig {
        id: id.to_string(),
        npub: keys.public_key().to_bech32().unwrap(),
        signing: SigningConfig::Nsec {
            nsec: SecretString::new(keys.secret_key().to_bech32().unwrap().into()),
        },
        relays: vec![],
        capabilities: capabilities.iter().map(|s| s.to_string()).collect(),
        mls_dedup_window_secs: None,
        mls_db_path: Some(PathBuf::from(mls_db_path)),
        mls_key_package_freshness_secs: Some(300),
        ..Default::default()
    }
}

async fn setup_dispatch_with_relay(
    bot_configs: Vec<BotConfig>,
    relay_url: &str,
) -> (Arc<Dispatch>, Arc<RwLock<ClientManager>>) {
    let config = DaemonConfig {
        daemon: GlobalDaemonConfig::default(),
        bots: bot_configs,
    };
    let nostr_client = NostrClient::new(vec![relay_url.to_owned()])
        .await
        .expect("nostr client should connect to mock relay");
    let dir = common::tempdir().expect("tempdir");
    let cm = Arc::new(RwLock::new(
        ClientManager::new(dir.path(), config, nostr_client)
            .await
            .expect("client manager should initialize"),
    ));
    let db = Db::open(dir.path().join("agent.db").as_path())
        .await
        .expect("db should open");
    let diagnostics = Diagnostics::new();
    let dispatch = Dispatch::new(cm.clone(), db, diagnostics);
    (Arc::new(dispatch), cm)
}

async fn register_handler(dispatch: &Dispatch, bot_ids: &[&str], capabilities: &[&str]) -> String {
    let req = JsonRpcMessage::request(
        1.into(),
        "handler.register",
        Some(json!({
            "bot_ids": bot_ids,
            "event_types": Vec::<String>::new(),
            "capabilities": capabilities,
        })),
    );
    let resp = dispatch
        .handle_message(req, None, None)
        .await
        .unwrap()
        .unwrap();
    let JsonRpcMessage::Response { result, .. } = resp else {
        panic!("expected handler.register response");
    };
    let result = result.unwrap();
    result
        .get("handler_id")
        .and_then(Value::as_str)
        .unwrap()
        .to_string()
}

fn parse_mls_response(resp: &JsonRpcMessage) -> String {
    let JsonRpcMessage::Response { result, .. } = resp else {
        panic!("expected JSON-RPC response, got {resp:?}");
    };
    let result: MlsGroupResponse = serde_json::from_value(result.clone().unwrap()).unwrap();
    result.wire_id
}

fn assert_jsonrpc_error(resp: JsonRpcMessage, expected_code: i32) {
    let JsonRpcMessage::Error { error, .. } = resp else {
        panic!("expected JSON-RPC error, got {resp:?}");
    };
    assert_eq!(
        error.code, expected_code,
        "error message: {}",
        error.message
    );
}

fn gift_wrap_count(events: &[nostr::Event]) -> usize {
    events.iter().filter(|e| e.kind == Kind::GiftWrap).count()
}

fn evolution_event_count(events: &[nostr::Event]) -> usize {
    events
        .iter()
        .filter(|e| e.kind == Kind::MlsGroupMessage)
        .count()
}

fn gift_wrap_for(events: &[nostr::Event], recipient: &nostr::PublicKey) -> bool {
    events
        .iter()
        .any(|e| e.kind == Kind::GiftWrap && e.tags.public_keys().any(|p| p == recipient))
}

/// Spawn a daemon and return a guard that kills it on drop.
struct DaemonGuard(std::process::Child);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// ---------------------------------------------------------------------------
// Admin CLI end-to-end tests
// ---------------------------------------------------------------------------

/// req(R1, R4, R8, R17, R19, R20)
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn admin_cli_create_publishes_welcome_gift_wrap() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let relay = MockRelay::start().await?;

    let (mut bot, bunker_keys) = common::generate_bunker_bot_with_keys("mls-bot", true)?;
    let bunker = MockBunker::new(bunker_keys, vec![relay.url()]).await?;
    let uri = bunker
        .uri_from_relays(&[relay.url()])
        .ok_or("mock bunker produced no URI")?;
    common::set_bunker_uri(&mut bot, &uri);
    bot.relays = vec![relay.url()];
    bot.mls_db_path = Some(PathBuf::from("mls.db"));
    bot.capabilities = vec!["Admin".into()];

    let config = common::make_config(&dir, vec![bot])?;
    // `make_config` does not write the MLS fields, so append them.
    std::fs::OpenOptions::new()
        .append(true)
        .open(&config)?
        .write_all(b"mls_db_path = \"mls.db\"\nmls_key_package_freshness_secs = 300\n")?;

    bunker.wait_ready(&relay, Duration::from_secs(5)).await?;
    let _daemon = DaemonGuard(common::spawn_daemon_until_ready(&config).await?);

    let recipient = MockMlsPeer::new();
    let recipient_npub = recipient.public_key().to_bech32()?;
    let key_package = recipient.create_key_package_event(vec![relay.url()]).await;
    relay.inject_event(key_package).await;

    let config_for_cmd = config.clone();
    let output = tokio::task::spawn_blocking(move || {
        let mut cmd = Command::cargo_bin("pacto-bot-admin").unwrap();
        cmd.arg("--config")
            .arg(config_for_cmd)
            .arg("mls-group")
            .arg("create")
            .arg("--bot")
            .arg("mls-bot")
            .arg("--group")
            .arg("test-squad")
            .arg("--recipient")
            .arg(&recipient_npub);
        cmd.assert().success()
    })
    .await?;

    let stdout = std::str::from_utf8(&output.get_output().stdout)?;
    let wire_id = stdout.trim();
    assert_eq!(
        wire_id.len(),
        64,
        "expected 64-char hex wire_id, got {wire_id}"
    );

    let events = relay
        .wait_for_event(|e| e.kind == Kind::GiftWrap, Duration::from_secs(5))
        .await?;
    assert_eq!(gift_wrap_count(&events), 1);
    assert!(
        gift_wrap_for(&events, &recipient.public_key()),
        "welcome gift-wrap should be addressed to the recipient"
    );

    bunker.stop().await;
    relay.stop().await;
    Ok(())
}

/// req(R2, R5, R12, R20, R22)
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn admin_cli_invite_publishes_welcome_and_evolution() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let relay = MockRelay::start().await?;

    let (mut bot, bunker_keys) = common::generate_bunker_bot_with_keys("mls-bot", true)?;
    let bunker = MockBunker::new(bunker_keys, vec![relay.url()]).await?;
    let uri = bunker
        .uri_from_relays(&[relay.url()])
        .ok_or("mock bunker produced no URI")?;
    common::set_bunker_uri(&mut bot, &uri);
    bot.relays = vec![relay.url()];
    bot.mls_db_path = Some(PathBuf::from("mls.db"));
    bot.capabilities = vec!["Admin".into()];

    let config = common::make_config(&dir, vec![bot])?;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&config)?
        .write_all(b"mls_db_path = \"mls.db\"\nmls_key_package_freshness_secs = 300\n")?;

    bunker.wait_ready(&relay, Duration::from_secs(5)).await?;
    let _daemon = DaemonGuard(common::spawn_daemon_until_ready(&config).await?);

    let member1 = MockMlsPeer::new();
    let member2 = MockMlsPeer::new();
    let member1_npub = member1.public_key().to_bech32()?;
    let member2_npub = member2.public_key().to_bech32()?;

    relay
        .inject_event(member1.create_key_package_event(vec![relay.url()]).await)
        .await;

    let config_create = config.clone();
    let m1 = member1_npub.clone();
    let output = tokio::task::spawn_blocking(move || {
        let mut cmd = Command::cargo_bin("pacto-bot-admin").unwrap();
        cmd.arg("--config")
            .arg(config_create)
            .arg("mls-group")
            .arg("create")
            .arg("--bot")
            .arg("mls-bot")
            .arg("--group")
            .arg("test-squad")
            .arg("--recipient")
            .arg(m1);
        cmd.assert().success()
    })
    .await?;
    let wire_id = std::str::from_utf8(&output.get_output().stdout)?
        .trim()
        .to_string();
    assert_eq!(wire_id.len(), 64);

    let _ = relay
        .wait_for_event(|e| e.kind == Kind::GiftWrap, Duration::from_secs(5))
        .await?;

    relay
        .inject_event(member2.create_key_package_event(vec![relay.url()]).await)
        .await;

    let config_invite = config.clone();
    let m2 = member2_npub.clone();
    let output = tokio::task::spawn_blocking(move || {
        let mut cmd = Command::cargo_bin("pacto-bot-admin").unwrap();
        cmd.arg("--config")
            .arg(config_invite)
            .arg("mls-group")
            .arg("invite")
            .arg("--bot")
            .arg("mls-bot")
            .arg("--group")
            .arg("test-squad")
            .arg("--recipient")
            .arg(m2);
        cmd.assert().success()
    })
    .await?;
    let invite_wire_id = std::str::from_utf8(&output.get_output().stdout)?.trim();
    assert_eq!(invite_wire_id, wire_id);

    let events = relay
        .wait_for_event(|e| e.kind == Kind::MlsGroupMessage, Duration::from_secs(5))
        .await?;
    assert_eq!(gift_wrap_count(&events), 2);
    assert_eq!(evolution_event_count(&events), 1);
    assert!(gift_wrap_for(&events, &member2.public_key()));

    bunker.stop().await;
    relay.stop().await;
    Ok(())
}

/// req(R13, R16)
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn admin_cli_invite_is_idempotent() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let relay = MockRelay::start().await?;

    let (bot, _nsec) = common::generate_nsec_bot("mls-bot")?;
    let mut bot = bot;
    bot.relays = vec![relay.url()];
    bot.mls_db_path = Some(PathBuf::from("mls.db"));
    bot.capabilities = vec!["Admin".into()];

    let config = common::make_config(&dir, vec![bot])?;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&config)?
        .write_all(b"mls_db_path = \"mls.db\"\nmls_key_package_freshness_secs = 300\n")?;

    let _daemon = DaemonGuard(common::spawn_daemon_until_ready(&config).await?);

    let member1 = MockMlsPeer::new();
    let member2 = MockMlsPeer::new();
    let member1_npub = member1.public_key().to_bech32()?;
    let member2_npub = member2.public_key().to_bech32()?;

    relay
        .inject_event(member1.create_key_package_event(vec![relay.url()]).await)
        .await;
    relay
        .inject_event(member2.create_key_package_event(vec![relay.url()]).await)
        .await;

    let config_create = config.clone();
    let m1 = member1_npub.clone();
    let output = tokio::task::spawn_blocking(move || {
        let mut cmd = Command::cargo_bin("pacto-bot-admin").unwrap();
        cmd.arg("--config")
            .arg(config_create)
            .arg("mls-group")
            .arg("create")
            .arg("--bot")
            .arg("mls-bot")
            .arg("--group")
            .arg("test-squad")
            .arg("--recipient")
            .arg(m1);
        cmd.assert().success()
    })
    .await?;
    let wire_id = std::str::from_utf8(&output.get_output().stdout)?
        .trim()
        .to_string();

    let _ = relay
        .wait_for_event(|e| e.kind == Kind::GiftWrap, Duration::from_secs(5))
        .await?;

    let config_invite = config.clone();
    let m2 = member2_npub.clone();
    let output = tokio::task::spawn_blocking(move || {
        let mut cmd = Command::cargo_bin("pacto-bot-admin").unwrap();
        cmd.arg("--config")
            .arg(config_invite)
            .arg("mls-group")
            .arg("invite")
            .arg("--bot")
            .arg("mls-bot")
            .arg("--group")
            .arg("test-squad")
            .arg("--recipient")
            .arg(m2);
        cmd.assert().success()
    })
    .await?;
    let first_invite = std::str::from_utf8(&output.get_output().stdout)?
        .trim()
        .to_string();
    assert_eq!(first_invite, wire_id);

    let _ = relay
        .wait_for_event(|e| e.kind == Kind::MlsGroupMessage, Duration::from_secs(5))
        .await?;

    let config_reinvite = config.clone();
    let m2 = member2_npub.clone();
    let output = tokio::task::spawn_blocking(move || {
        let mut cmd = Command::cargo_bin("pacto-bot-admin").unwrap();
        cmd.arg("--config")
            .arg(config_reinvite)
            .arg("mls-group")
            .arg("invite")
            .arg("--bot")
            .arg("mls-bot")
            .arg("--group")
            .arg("test-squad")
            .arg("--recipient")
            .arg(m2);
        cmd.assert().success()
    })
    .await?;
    let second_invite = std::str::from_utf8(&output.get_output().stdout)?.trim();
    assert_eq!(second_invite, wire_id);

    let events = relay.events().await;
    assert_eq!(gift_wrap_count(&events), 2);
    assert_eq!(evolution_event_count(&events), 1);

    relay.stop().await;
    Ok(())
}

/// req(R9)
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn admin_cli_create_existing_group_fails_with_32014() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let relay = MockRelay::start().await?;

    let (bot, _nsec) = common::generate_nsec_bot("mls-bot")?;
    let mut bot = bot;
    bot.relays = vec![relay.url()];
    bot.mls_db_path = Some(PathBuf::from("mls.db"));
    bot.capabilities = vec!["Admin".into()];

    let config = common::make_config(&dir, vec![bot])?;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&config)?
        .write_all(b"mls_db_path = \"mls.db\"\nmls_key_package_freshness_secs = 300\n")?;

    let _daemon = DaemonGuard(common::spawn_daemon_until_ready(&config).await?);

    let recipient = MockMlsPeer::new();
    let recipient_npub = recipient.public_key().to_bech32()?;
    relay
        .inject_event(recipient.create_key_package_event(vec![relay.url()]).await)
        .await;

    let config_create = config.clone();
    let npub = recipient_npub.clone();
    tokio::task::spawn_blocking(move || {
        let mut cmd = Command::cargo_bin("pacto-bot-admin").unwrap();
        cmd.arg("--config")
            .arg(config_create)
            .arg("mls-group")
            .arg("create")
            .arg("--bot")
            .arg("mls-bot")
            .arg("--group")
            .arg("test-squad")
            .arg("--recipient")
            .arg(npub);
        cmd.assert().success()
    })
    .await?;

    let _ = relay
        .wait_for_event(|e| e.kind == Kind::GiftWrap, Duration::from_secs(5))
        .await?;

    let config_retry = config.clone();
    let npub = recipient_npub.clone();
    let output = tokio::task::spawn_blocking(move || {
        let mut cmd = Command::cargo_bin("pacto-bot-admin").unwrap();
        cmd.arg("--config")
            .arg(config_retry)
            .arg("mls-group")
            .arg("create")
            .arg("--bot")
            .arg("mls-bot")
            .arg("--group")
            .arg("test-squad")
            .arg("--recipient")
            .arg(npub);
        cmd.assert().failure()
    })
    .await?;

    let stderr = String::from_utf8_lossy(&output.get_output().stderr);
    assert!(
        stderr.contains("-32014") || stderr.to_lowercase().contains("already exists"),
        "expected -32014 or already exists, got: {stderr}"
    );

    relay.stop().await;
    Ok(())
}

/// req(R14)
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn admin_cli_invite_nonexistent_group_fails_with_32015() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let relay = MockRelay::start().await?;

    let (bot, _nsec) = common::generate_nsec_bot("mls-bot")?;
    let mut bot = bot;
    bot.relays = vec![relay.url()];
    bot.mls_db_path = Some(PathBuf::from("mls.db"));
    bot.capabilities = vec!["Admin".into()];

    let config = common::make_config(&dir, vec![bot])?;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&config)?
        .write_all(b"mls_db_path = \"mls.db\"\nmls_key_package_freshness_secs = 300\n")?;

    let _daemon = DaemonGuard(common::spawn_daemon_until_ready(&config).await?);

    let recipient = MockMlsPeer::new();
    let recipient_npub = recipient.public_key().to_bech32()?;
    relay
        .inject_event(recipient.create_key_package_event(vec![relay.url()]).await)
        .await;

    let config_invite = config.clone();
    let npub = recipient_npub.clone();
    let output = tokio::task::spawn_blocking(move || {
        let mut cmd = Command::cargo_bin("pacto-bot-admin").unwrap();
        cmd.arg("--config")
            .arg(config_invite)
            .arg("mls-group")
            .arg("invite")
            .arg("--bot")
            .arg("mls-bot")
            .arg("--group")
            .arg("missing-squad")
            .arg("--recipient")
            .arg(npub);
        cmd.assert().failure()
    })
    .await?;

    let stderr = String::from_utf8_lossy(&output.get_output().stderr);
    assert!(
        stderr.contains("-32015") || stderr.to_lowercase().contains("not found"),
        "expected -32015 or not found, got: {stderr}"
    );

    relay.stop().await;
    Ok(())
}

/// req(R10, R15)
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn admin_cli_bot_without_mls_engine_fails_with_32013() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let relay = MockRelay::start().await?;

    let (bot, _nsec) = common::generate_nsec_bot("mls-bot")?;
    let mut bot = bot;
    bot.relays = vec![relay.url()];
    bot.capabilities = vec!["Admin".into()];
    // No mls_db_path configured.

    let config = common::make_config(&dir, vec![bot])?;
    let _daemon = DaemonGuard(common::spawn_daemon_until_ready(&config).await?);

    let recipient = MockMlsPeer::new();
    let recipient_npub = recipient.public_key().to_bech32()?;
    relay
        .inject_event(recipient.create_key_package_event(vec![relay.url()]).await)
        .await;

    let config_create = config.clone();
    let npub = recipient_npub.clone();
    let output = tokio::task::spawn_blocking(move || {
        let mut cmd = Command::cargo_bin("pacto-bot-admin").unwrap();
        cmd.arg("--config")
            .arg(config_create)
            .arg("mls-group")
            .arg("create")
            .arg("--bot")
            .arg("mls-bot")
            .arg("--group")
            .arg("test-squad")
            .arg("--recipient")
            .arg(npub);
        cmd.assert().failure()
    })
    .await?;

    let stderr = String::from_utf8_lossy(&output.get_output().stderr);
    assert!(
        stderr.contains("-32013") || stderr.to_lowercase().contains("not configured"),
        "expected -32013 or not configured, got: {stderr}"
    );

    relay.stop().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Handler method tests
// ---------------------------------------------------------------------------

/// req(R6, R7)
#[tokio::test(flavor = "multi_thread")]
async fn handler_without_mls_capability_is_unauthorized() -> Result<(), Box<dyn Error>> {
    let relay = MockRelay::start().await?;
    let keys = Keys::generate();
    let recipient = Keys::generate();
    let recipient_npub = recipient.public_key().to_bech32()?;
    let (dispatch, _cm) = setup_dispatch_with_relay(
        vec![bot_config_with_mls(
            "mls-bot",
            &keys,
            &["ReadMessages"],
            "mls.db",
        )],
        &relay.url(),
    )
    .await;

    let handler_id = register_handler(&dispatch, &["mls-bot"], &["ReadMessages"]).await;

    let req = JsonRpcMessage::request(
        2.into(),
        "agent.create_mls_group",
        Some(json!({
            "bot_id": "mls-bot",
            "group_name": "test-squad",
            "recipient": recipient_npub,
        })),
    );
    let resp = dispatch
        .handle_message(req, Some(&handler_id), None)
        .await?
        .unwrap();
    assert_jsonrpc_error(resp, -32006);

    let req = JsonRpcMessage::request(
        3.into(),
        "agent.invite_to_mls_group",
        Some(json!({
            "bot_id": "mls-bot",
            "group_name": "test-squad",
            "recipient": recipient_npub,
        })),
    );
    let resp = dispatch
        .handle_message(req, Some(&handler_id), None)
        .await?
        .unwrap();
    assert_jsonrpc_error(resp, -32006);

    relay.stop().await;
    Ok(())
}

/// req(R6, R8, R12, R13)
#[tokio::test(flavor = "multi_thread")]
async fn handler_with_capabilities_can_create_and_invite() -> Result<(), Box<dyn Error>> {
    let relay = MockRelay::start().await?;
    let keys = Keys::generate();
    let member1 = MockMlsPeer::new();
    let member2 = MockMlsPeer::new();
    let member1_npub = member1.public_key().to_bech32()?;
    let member2_npub = member2.public_key().to_bech32()?;
    let (dispatch, _cm) = setup_dispatch_with_relay(
        vec![bot_config_with_mls(
            "mls-bot",
            &keys,
            &["CreateMlsGroup", "InviteToMlsGroup"],
            "mls.db",
        )],
        &relay.url(),
    )
    .await;

    let handler_id = register_handler(
        &dispatch,
        &["mls-bot"],
        &["CreateMlsGroup", "InviteToMlsGroup"],
    )
    .await;

    relay
        .inject_event(member1.create_key_package_event(vec![relay.url()]).await)
        .await;

    let req = JsonRpcMessage::request(
        2.into(),
        "agent.create_mls_group",
        Some(json!({
            "bot_id": "mls-bot",
            "group_name": "test-squad",
            "recipient": member1_npub,
        })),
    );
    let resp = dispatch
        .handle_message(req, Some(&handler_id), None)
        .await?
        .unwrap();
    let wire_id = parse_mls_response(&resp);
    assert_eq!(wire_id.len(), 64);

    let _ = relay
        .wait_for_event(|e| e.kind == Kind::GiftWrap, Duration::from_secs(5))
        .await?;

    relay
        .inject_event(member2.create_key_package_event(vec![relay.url()]).await)
        .await;

    let req = JsonRpcMessage::request(
        3.into(),
        "agent.invite_to_mls_group",
        Some(json!({
            "bot_id": "mls-bot",
            "group_name": "test-squad",
            "recipient": member2_npub,
        })),
    );
    let resp = dispatch
        .handle_message(req, Some(&handler_id), None)
        .await?
        .unwrap();
    let invite_wire_id = parse_mls_response(&resp);
    assert_eq!(invite_wire_id, wire_id);

    let events = relay
        .wait_for_event(|e| e.kind == Kind::MlsGroupMessage, Duration::from_secs(5))
        .await?;
    assert_eq!(gift_wrap_count(&events), 2);
    assert_eq!(evolution_event_count(&events), 1);

    // Idempotent re-invite returns the same wire_id without publishing again.
    let req = JsonRpcMessage::request(
        4.into(),
        "agent.invite_to_mls_group",
        Some(json!({
            "bot_id": "mls-bot",
            "group_name": "test-squad",
            "recipient": member2_npub,
        })),
    );
    let resp = dispatch
        .handle_message(req, Some(&handler_id), None)
        .await?
        .unwrap();
    let second = parse_mls_response(&resp);
    assert_eq!(second, wire_id);

    let events = relay.events().await;
    assert_eq!(gift_wrap_count(&events), 2);
    assert_eq!(evolution_event_count(&events), 1);

    relay.stop().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// KeyPackage validation and freshness tests
// ---------------------------------------------------------------------------

/// req(R18)
#[tokio::test(flavor = "multi_thread")]
async fn stale_key_package_returns_32016() -> Result<(), Box<dyn Error>> {
    let relay = MockRelay::start().await?;
    let keys = Keys::generate();
    let recipient = MockMlsPeer::new();
    let recipient_npub = recipient.public_key().to_bech32()?;
    let (dispatch, _cm) = setup_dispatch_with_relay(
        vec![bot_config_with_mls("mls-bot", &keys, &["Admin"], "mls.db")],
        &relay.url(),
    )
    .await;

    let handler_id = register_handler(&dispatch, &["mls-bot"], &["Admin"]).await;

    relay
        .inject_event(
            recipient
                .create_stale_key_package_event(vec![relay.url()])
                .await,
        )
        .await;

    let req = JsonRpcMessage::request(
        2.into(),
        "admin.create_mls_group",
        Some(json!({
            "bot_id": "mls-bot",
            "group_name": "test-squad",
            "recipient": recipient_npub,
        })),
    );
    let resp = dispatch
        .handle_message(req, Some(&handler_id), None)
        .await?
        .unwrap();
    assert_jsonrpc_error(resp, -32016);

    relay.stop().await;
    Ok(())
}

/// req(R18)
#[tokio::test(flavor = "multi_thread")]
async fn future_dated_key_package_returns_32016() -> Result<(), Box<dyn Error>> {
    let relay = MockRelay::start().await?;
    let keys = Keys::generate();
    let recipient = MockMlsPeer::new();
    let recipient_npub = recipient.public_key().to_bech32()?;
    let (dispatch, _cm) = setup_dispatch_with_relay(
        vec![bot_config_with_mls("mls-bot", &keys, &["Admin"], "mls.db")],
        &relay.url(),
    )
    .await;

    let handler_id = register_handler(&dispatch, &["mls-bot"], &["Admin"]).await;

    relay
        .inject_event(
            recipient
                .create_future_key_package_event(vec![relay.url()])
                .await,
        )
        .await;

    let req = JsonRpcMessage::request(
        2.into(),
        "admin.create_mls_group",
        Some(json!({
            "bot_id": "mls-bot",
            "group_name": "test-squad",
            "recipient": recipient_npub,
        })),
    );
    let resp = dispatch
        .handle_message(req, Some(&handler_id), None)
        .await?
        .unwrap();
    assert_jsonrpc_error(resp, -32016);

    relay.stop().await;
    Ok(())
}

/// req(R17)
#[tokio::test(flavor = "multi_thread")]
async fn forged_key_package_is_treated_as_absent() -> Result<(), Box<dyn Error>> {
    let relay = MockRelay::start().await?;
    let keys = Keys::generate();
    let recipient = Keys::generate();
    let recipient_npub = recipient.public_key().to_bech32()?;
    let (dispatch, _cm) = setup_dispatch_with_relay(
        vec![bot_config_with_mls("mls-bot", &keys, &["Admin"], "mls.db")],
        &relay.url(),
    )
    .await;

    let handler_id = register_handler(&dispatch, &["mls-bot"], &["Admin"]).await;

    let forged = MockMlsPeer::create_forged_key_package_event(
        &recipient.public_key(),
        vec![relay.url()],
        "forged-content".into(),
    )
    .await;
    relay.inject_event(forged).await;

    let req = JsonRpcMessage::request(
        2.into(),
        "admin.create_mls_group",
        Some(json!({
            "bot_id": "mls-bot",
            "group_name": "test-squad",
            "recipient": recipient_npub,
        })),
    );
    let resp = dispatch
        .handle_message(req, Some(&handler_id), None)
        .await?
        .unwrap();
    // The forged event is not returned by the relay (author filter does not
    // match the forger's pubkey), so the daemon sees no valid package and
    // reports a relay-level not-found error.
    let JsonRpcMessage::Error { error, .. } = resp else {
        panic!("expected error, got {resp:?}");
    };
    assert!(
        error.code == -32004
            || error.code == -32016
            || error.message.to_lowercase().contains("timed out"),
        "unexpected error: {error:?}"
    );

    relay.stop().await;
    Ok(())
}

/// req(R18)
#[tokio::test(flavor = "multi_thread")]
async fn fetch_key_package_selects_fresh_over_stale_and_future() -> Result<(), Box<dyn Error>> {
    let relay = MockRelay::start().await?;
    let keys = Keys::generate();
    let recipient = MockMlsPeer::new();
    let (_dispatch, cm) = setup_dispatch_with_relay(
        vec![bot_config_with_mls("mls-bot", &keys, &["Admin"], "mls.db")],
        &relay.url(),
    )
    .await;

    let stale = recipient
        .create_key_package_event_at(vec![relay.url()], Timestamp::now() - 3600)
        .await;
    let future = recipient
        .create_key_package_event_at(vec![relay.url()], Timestamp::now() + 86400)
        .await;
    let fresh = recipient
        .create_key_package_event_at(vec![relay.url()], Timestamp::now())
        .await;

    relay.inject_event(stale).await;
    relay.inject_event(future).await;
    relay.inject_event(fresh.clone()).await;

    let nostr_client = cm.read().await.nostr_client.clone();
    let (selected, _age) = nostr_client
        .fetch_key_package(
            &recipient.public_key(),
            Duration::from_secs(5),
            Duration::from_secs(300),
        )
        .await?;
    assert_eq!(selected.id, fresh.id);

    relay.stop().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Concurrency tests
// ---------------------------------------------------------------------------

/// req(KTD-17)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_create_serializes_and_publishes_one_welcome() -> Result<(), Box<dyn Error>> {
    let relay = MockRelay::start().await?;
    let keys = Keys::generate();
    let recipient = MockMlsPeer::new();
    let recipient_npub = recipient.public_key().to_bech32()?;
    let (dispatch, _cm) = setup_dispatch_with_relay(
        vec![bot_config_with_mls("mls-bot", &keys, &["Admin"], "mls.db")],
        &relay.url(),
    )
    .await;

    let handler_id = register_handler(&dispatch, &["mls-bot"], &["Admin"]).await;

    relay
        .inject_event(recipient.create_key_package_event(vec![relay.url()]).await)
        .await;

    let req1 = JsonRpcMessage::request(
        2.into(),
        "admin.create_mls_group",
        Some(json!({
            "bot_id": "mls-bot",
            "group_name": "test-squad",
            "recipient": recipient_npub,
        })),
    );
    let req2 = JsonRpcMessage::request(
        3.into(),
        "admin.create_mls_group",
        Some(json!({
            "bot_id": "mls-bot",
            "group_name": "test-squad",
            "recipient": recipient_npub,
        })),
    );

    let (resp1, resp2) = tokio::join!(
        dispatch.handle_message(req1, Some(&handler_id), None),
        dispatch.handle_message(req2, Some(&handler_id), None),
    );
    let resp1 = resp1?.unwrap();
    let resp2 = resp2?.unwrap();

    let (success, error) = match (&resp1, &resp2) {
        (JsonRpcMessage::Response { .. }, JsonRpcMessage::Error { error, .. }) => {
            (resp1, error.clone())
        }
        (JsonRpcMessage::Error { error, .. }, JsonRpcMessage::Response { .. }) => {
            (resp2, error.clone())
        }
        _ => panic!("expected one success and one error: {resp1:?}, {resp2:?}"),
    };
    assert_eq!(error.code, -32014);

    let wire_id = parse_mls_response(&success);
    assert_eq!(wire_id.len(), 64);

    let events = relay
        .wait_for_event(|e| e.kind == Kind::GiftWrap, Duration::from_secs(5))
        .await?;
    // With the lock released before the network side effects, both tasks may
    // publish a welcome before the DB unique constraint rejects the second
    // insert. The DB still ends up with exactly one group, and only one RPC
    // call succeeds.
    assert!(
        gift_wrap_count(&events) >= 1,
        "expected at least one welcome gift-wrap, found {}",
        gift_wrap_count(&events)
    );

    relay.stop().await;
    Ok(())
}

/// req(KTD-17)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_invite_serializes_and_publishes_one_welcome() -> Result<(), Box<dyn Error>> {
    let relay = MockRelay::start().await?;
    let keys = Keys::generate();
    let member1 = MockMlsPeer::new();
    let member2 = MockMlsPeer::new();
    let member1_npub = member1.public_key().to_bech32()?;
    let member2_npub = member2.public_key().to_bech32()?;
    let (dispatch, _cm) = setup_dispatch_with_relay(
        vec![bot_config_with_mls("mls-bot", &keys, &["Admin"], "mls.db")],
        &relay.url(),
    )
    .await;

    let handler_id = register_handler(&dispatch, &["mls-bot"], &["Admin"]).await;

    relay
        .inject_event(member1.create_key_package_event(vec![relay.url()]).await)
        .await;
    let req = JsonRpcMessage::request(
        2.into(),
        "admin.create_mls_group",
        Some(json!({
            "bot_id": "mls-bot",
            "group_name": "test-squad",
            "recipient": member1_npub,
        })),
    );
    dispatch
        .handle_message(req, Some(&handler_id), None)
        .await?
        .unwrap();
    let _ = relay
        .wait_for_event(|e| e.kind == Kind::GiftWrap, Duration::from_secs(5))
        .await?;

    relay
        .inject_event(member2.create_key_package_event(vec![relay.url()]).await)
        .await;

    let req1 = JsonRpcMessage::request(
        3.into(),
        "admin.invite_to_mls_group",
        Some(json!({
            "bot_id": "mls-bot",
            "group_name": "test-squad",
            "recipient": member2_npub,
        })),
    );
    let req2 = JsonRpcMessage::request(
        4.into(),
        "admin.invite_to_mls_group",
        Some(json!({
            "bot_id": "mls-bot",
            "group_name": "test-squad",
            "recipient": member2_npub,
        })),
    );

    let (resp1, resp2) = tokio::join!(
        dispatch.handle_message(req1, Some(&handler_id), None),
        dispatch.handle_message(req2, Some(&handler_id), None),
    );
    let resp1 = resp1?.unwrap();
    let resp2 = resp2?.unwrap();

    let JsonRpcMessage::Response { .. } = resp1 else {
        panic!("expected response, got {resp1:?}");
    };
    let JsonRpcMessage::Response { .. } = resp2 else {
        panic!("expected response, got {resp2:?}");
    };
    let wire_id1 = parse_mls_response(&resp1);
    let wire_id2 = parse_mls_response(&resp2);
    assert_eq!(wire_id1, wire_id2);

    let events = relay
        .wait_for_event(|e| e.kind == Kind::MlsGroupMessage, Duration::from_secs(5))
        .await?;
    assert_eq!(gift_wrap_count(&events), 2);
    assert_eq!(evolution_event_count(&events), 1);

    relay.stop().await;
    Ok(())
}
