#![allow(clippy::panic)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
mod support;

use std::error::Error;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use assert_cmd::Command;
use nostr::{EventBuilder, Keys, Kind, RelayUrl, Timestamp, ToBech32};
use pacto_bot_api::client_manager::ClientManager;
use pacto_bot_api::config::{BotConfig, DaemonConfig, GlobalDaemonConfig, SigningConfig};
use pacto_bot_api::db::{Db, MlsGroupRow};
use pacto_bot_api::diagnostics::Diagnostics;
use pacto_bot_api::dispatch::Dispatch;
use pacto_bot_api::mls::{MlsEngineHandle, MlsError};
use pacto_bot_api::nostr::NostrClient;
use pacto_bot_api::transport::protocol::{JsonRpcMessage, MlsGroupResponse};
use secrecy::SecretString;
use serde_json::{Value, json};
use support::mock_bunker::MockBunker;
use support::mock_mls_peer::MockMlsPeer;
use support::mock_relay::MockRelay;
use tokio::sync::{OwnedSemaphorePermit, RwLock, Semaphore};

fn bot_config_with_mls(
    id: &str,
    keys: &Keys,
    capabilities: &[&str],
    mls_db_path: &str,
) -> BotConfig {
    BotConfig {
        id: id.to_string(),
        display_name: Some(format!("{} Display", id)),
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
    let db = Db::open(dir.path().join("agent.db").as_path())
        .await
        .expect("db should open");
    let cm = Arc::new(RwLock::new(
        ClientManager::new(dir.path(), config, nostr_client, &db)
            .await
            .expect("client manager should initialize"),
    ));
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

/// Serializes tests that spawn a real daemon subprocess alongside a mock
/// relay/bunker. This file has grown to dozens of
/// `#[tokio::test(flavor = "multi_thread")]` tests; letting every
/// daemon-spawning test race unboundedly starves the mock bunker's own
/// (in-process, otherwise-instant) relay subscription task under `cargo
/// test`'s default full-parallelism scheduling, producing an intermittent
/// "timeout waiting for relay subscription" failure that does not
/// reproduce when a test runs alone. Capping to 1 removes cross-daemon-test
/// contention and cuts the observed failure rate sharply; it does not fully
/// eliminate contention from this file's other ~35 lighter tests still
/// racing concurrently. `cargo-nextest` (`make test-fast`) sidesteps the
/// remaining flake via process isolation plus its documented retry for
/// exactly this class of load-sensitive test (see `.config/nextest.toml`).
static DAEMON_SLOTS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(1)));

/// Reserve the daemon-test slot; hold the returned permit for the lifetime
/// of the test so daemon-spawning tests never run concurrently.
async fn daemon_slot() -> OwnedSemaphorePermit {
    DAEMON_SLOTS
        .clone()
        .acquire_owned()
        .await
        .expect("DAEMON_SLOTS semaphore is never closed")
}

// ---------------------------------------------------------------------------
// Admin CLI end-to-end tests
// ---------------------------------------------------------------------------

/// req(R1, R4, R8, R17, R19, R20)
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn admin_cli_create_publishes_welcome_gift_wrap() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let _daemon_slot = daemon_slot().await;
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

    bunker.wait_ready(&relay, Duration::from_secs(15)).await?;
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

/// req(R12)
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn admin_cli_invite_publishes_welcome_and_evolution() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let _daemon_slot = daemon_slot().await;
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

    bunker.wait_ready(&relay, Duration::from_secs(15)).await?;
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
    let _daemon_slot = daemon_slot().await;
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
    let _daemon_slot = daemon_slot().await;
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
    let _daemon_slot = daemon_slot().await;
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
    let _daemon_slot = daemon_slot().await;
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
    // reports a KeyPackageNotFound error.
    let JsonRpcMessage::Error { error, .. } = resp else {
        panic!("expected error, got {resp:?}");
    };
    assert_eq!(
        error.code, -32017,
        "expected KeyPackageNotFound, got {error:?}"
    );
    assert!(
        error
            .message
            .to_lowercase()
            .contains("no key package found"),
        "error should tell the operator the recipient has no key package: {error:?}"
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

/// req(U13)
#[tokio::test(flavor = "multi_thread")]
async fn kind_30443_key_package_is_fetched_and_accepted() -> Result<(), Box<dyn Error>> {
    let relay = MockRelay::start().await?;
    let keys = Keys::generate();
    let recipient = MockMlsPeer::new();
    let recipient_npub = recipient.public_key().to_bech32()?;
    let (dispatch, cm) = setup_dispatch_with_relay(
        vec![bot_config_with_mls("mls-bot", &keys, &["Admin"], "mls.db")],
        &relay.url(),
    )
    .await;

    let handler_id = register_handler(&dispatch, &["mls-bot"], &["Admin"]).await;

    let addressable = recipient
        .create_key_package_event_kind_30443(vec![relay.url()])
        .await;
    assert_eq!(addressable.kind, Kind::Custom(30443));
    relay.inject_event(addressable.clone()).await;

    // The filter widening, not just the validator: fetch_key_package must
    // actually return the kind:30443 event from the relay.
    let nostr_client = cm.read().await.nostr_client.clone();
    let (fetched, _age) = nostr_client
        .fetch_key_package(
            &recipient.public_key(),
            Duration::from_secs(5),
            Duration::from_secs(300),
        )
        .await?;
    assert_eq!(fetched.id, addressable.id);
    assert_eq!(fetched.kind, Kind::Custom(30443));

    // And it is accepted end-to-end: MDK parses tags_30443 (including the
    // mandatory `d` tag) and the group is created.
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
    let wire_id = parse_mls_response(&resp);
    assert_eq!(wire_id.len(), 64);

    relay.stop().await;
    Ok(())
}

/// req(U13)
#[tokio::test(flavor = "multi_thread")]
async fn hex_key_package_missing_encoding_tag_returns_32025() -> Result<(), Box<dyn Error>> {
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
                .create_key_package_event_missing_encoding_tag(vec![relay.url()])
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
    let JsonRpcMessage::Error { error, .. } = resp else {
        panic!("expected error, got {resp:?}");
    };
    assert_eq!(
        error.code, -32025,
        "hex KeyPackage with no encoding tag must be named a peer-version mismatch, got {error:?}"
    );
    assert!(
        error.message.to_lowercase().contains("version"),
        "error should name a peer version mismatch, not a generic parse failure: {error:?}"
    );

    relay.stop().await;
    Ok(())
}

/// req(U13)
#[tokio::test(flavor = "multi_thread")]
async fn forged_kind_30443_key_package_is_treated_as_absent() -> Result<(), Box<dyn Error>> {
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

    let forged = MockMlsPeer::create_forged_key_package_event_kind_30443(
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
    // The kind-guard widening (accepting kind:30443) must not weaken the
    // authorship check: the forged event is still filtered out, leaving no
    // valid package for the recipient.
    let JsonRpcMessage::Error { error, .. } = resp else {
        panic!("expected error, got {resp:?}");
    };
    assert_eq!(
        error.code, -32017,
        "expected KeyPackageNotFound, got {error:?}"
    );

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

// ---------------------------------------------------------------------------
// U11: post-reset recovery, failure isolation, and handler signal
// ---------------------------------------------------------------------------

/// req(R28)
#[tokio::test(flavor = "multi_thread")]
async fn send_into_state_lost_group_returns_error_32026() -> Result<(), Box<dyn Error>> {
    let relay = MockRelay::start().await?;
    let keys = Keys::generate();
    let member = MockMlsPeer::new();
    let member_npub = member.public_key().to_bech32()?;

    let config = DaemonConfig {
        daemon: GlobalDaemonConfig::default(),
        bots: vec![bot_config_with_mls(
            "mls-bot",
            &keys,
            &["CreateMlsGroup", "SendGroupMessages"],
            "mls.db",
        )],
    };
    let nostr_client = NostrClient::new(vec![relay.url()]).await?;
    let dir = common::tempdir()?;
    let db = Db::open(dir.path().join("agent.db").as_path()).await?;
    let cm = Arc::new(RwLock::new(
        ClientManager::new(dir.path(), config, nostr_client, &db).await?,
    ));
    let dispatch = Arc::new(Dispatch::new(cm, db.clone(), Diagnostics::new()));

    let handler_id = register_handler(
        &dispatch,
        &["mls-bot"],
        &["CreateMlsGroup", "SendGroupMessages"],
    )
    .await;

    relay
        .inject_event(member.create_key_package_event(vec![relay.url()]).await)
        .await;
    let req = JsonRpcMessage::request(
        2.into(),
        "agent.create_mls_group",
        Some(json!({
            "bot_id": "mls-bot",
            "group_name": "test-squad",
            "recipient": member_npub,
        })),
    );
    let resp = dispatch
        .handle_message(req, Some(&handler_id), None)
        .await?
        .unwrap();
    let wire_id = parse_mls_response(&resp);

    // Simulate a completed reset marking this group state-lost (the actual
    // ClientManager::new startup path is covered by
    // completed_reset_marks_orphaned_groups_state_lost_and_leaves_members_untouched
    // in src/client_manager.rs's own tests; this test isolates the send-gate).
    let now = chrono::Utc::now().timestamp();
    db.mark_mls_group_state_lost("mls-bot", "test-squad", now)
        .await?;

    let req = JsonRpcMessage::request(
        3.into(),
        "agent.send_group_message",
        Some(json!({
            "bot_id": "mls-bot",
            "group_id": wire_id,
            "content": "hello",
        })),
    );
    let resp = dispatch
        .handle_message(req, Some(&handler_id), None)
        .await?
        .unwrap();
    assert_jsonrpc_error(resp, -32026);

    relay.stop().await;
    Ok(())
}

/// req(R28)
#[tokio::test(flavor = "multi_thread")]
async fn welcome_event_clears_state_lost_mark_and_next_send_succeeds() -> Result<(), Box<dyn Error>>
{
    use pacto_bot_api::events::{AgentEvent, EventType};

    let relay = MockRelay::start().await?;
    let keys = Keys::generate();
    let member = MockMlsPeer::new();
    let member_npub = member.public_key().to_bech32()?;

    let config = DaemonConfig {
        daemon: GlobalDaemonConfig::default(),
        bots: vec![bot_config_with_mls(
            "mls-bot",
            &keys,
            &["CreateMlsGroup", "SendGroupMessages"],
            "mls.db",
        )],
    };
    let nostr_client = NostrClient::new(vec![relay.url()]).await?;
    let dir = common::tempdir()?;
    let db = Db::open(dir.path().join("agent.db").as_path()).await?;
    let cm = Arc::new(RwLock::new(
        ClientManager::new(dir.path(), config, nostr_client, &db).await?,
    ));
    let dispatch = Arc::new(Dispatch::new(cm, db.clone(), Diagnostics::new()));

    let handler_id = register_handler(
        &dispatch,
        &["mls-bot"],
        &["CreateMlsGroup", "SendGroupMessages"],
    )
    .await;

    relay
        .inject_event(member.create_key_package_event(vec![relay.url()]).await)
        .await;
    let req = JsonRpcMessage::request(
        2.into(),
        "agent.create_mls_group",
        Some(json!({
            "bot_id": "mls-bot",
            "group_name": "test-squad",
            "recipient": member_npub,
        })),
    );
    let resp = dispatch
        .handle_message(req, Some(&handler_id), None)
        .await?
        .unwrap();
    let wire_id = parse_mls_response(&resp);

    let now = chrono::Utc::now().timestamp();
    db.mark_mls_group_state_lost("mls-bot", "test-squad", now)
        .await?;
    assert!(
        db.load_mls_group_state_lost_at("mls-bot", &wire_id)
            .await?
            .is_some()
    );

    // Drive the exact code path a real inbound Welcome reaches: dispatch_event
    // with an MlsWelcomeReceived event carrying the group's wire id in
    // chat_id (see NostrClient::finish_gift_wrap for where that's set on a
    // real gift wrap).
    dispatch
        .dispatch_event(AgentEvent {
            bot_id: "mls-bot".to_string(),
            event_id: "welcome-event-id".to_string(),
            event_type: EventType::MlsWelcomeReceived,
            chat_id: Some(wire_id.clone()),
            rumor_id: "rumor-id".to_string(),
            author: member_npub.clone(),
            timestamp: now as u64,
            ..Default::default()
        })
        .await?;

    assert!(
        db.load_mls_group_state_lost_at("mls-bot", &wire_id)
            .await?
            .is_none(),
        "processing the Welcome must clear the state-lost mark"
    );

    let req = JsonRpcMessage::request(
        3.into(),
        "agent.send_group_message",
        Some(json!({
            "bot_id": "mls-bot",
            "group_id": wire_id,
            "content": "hello again",
        })),
    );
    let resp = dispatch
        .handle_message(req, Some(&handler_id), None)
        .await?
        .unwrap();
    // No longer -32026; the send reaches the real engine and succeeds.
    let JsonRpcMessage::Response { .. } = resp else {
        panic!("expected a successful send after the mark clears, got {resp:?}");
    };

    relay.stop().await;
    Ok(())
}

/// req(R49)
#[tokio::test(flavor = "multi_thread")]
async fn call_against_fail_closed_bot_returns_error_32028() -> Result<(), Box<dyn Error>> {
    let relay = MockRelay::start().await?;
    let keys = Keys::generate();

    let dir = common::tempdir()?;
    let bot_dir = dir.path().join("mls-bot");
    std::fs::create_dir_all(&bot_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bot_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    // An unencrypted store with no recognisable schema -- classification
    // fails closed (same fixture shape as
    // client_manager::tests::mls_engine_construction_failure_is_isolated_and_recorded_in_health).
    let store_path = bot_dir.join("mls.db");
    {
        let conn = rusqlite::Connection::open(&store_path)?;
        conn.execute_batch("CREATE TABLE unrelated (x INTEGER);")?;
    }

    let config = DaemonConfig {
        daemon: GlobalDaemonConfig::default(),
        bots: vec![bot_config_with_mls(
            "mls-bot",
            &keys,
            &["CreateMlsGroup", "SendGroupMessages"],
            "mls.db",
        )],
    };
    let nostr_client = NostrClient::new(vec![relay.url()]).await?;
    let db = Db::open(dir.path().join("agent.db").as_path()).await?;
    let cm = Arc::new(RwLock::new(
        ClientManager::new(dir.path(), config, nostr_client, &db)
            .await
            .expect("daemon starts despite the fail-closed store"),
    ));
    let dispatch = Arc::new(Dispatch::new(cm, db, Diagnostics::new()));

    let handler_id = register_handler(
        &dispatch,
        &["mls-bot"],
        &["CreateMlsGroup", "SendGroupMessages"],
    )
    .await;

    let member = MockMlsPeer::new();
    let member_npub = member.public_key().to_bech32()?;
    let req = JsonRpcMessage::request(
        2.into(),
        "agent.create_mls_group",
        Some(json!({
            "bot_id": "mls-bot",
            "group_name": "test-squad",
            "recipient": member_npub,
        })),
    );
    let resp = dispatch
        .handle_message(req, Some(&handler_id), None)
        .await?
        .unwrap();
    assert_jsonrpc_error(resp, -32028);

    relay.stop().await;
    Ok(())
}

/// req(R37)
#[tokio::test(flavor = "multi_thread")]
async fn handler_registration_accepts_bot_unavailable_event_type() -> Result<(), Box<dyn Error>> {
    let relay = MockRelay::start().await?;
    let keys = Keys::generate();
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

    let req = JsonRpcMessage::request(
        1.into(),
        "handler.register",
        Some(json!({
            "bot_ids": ["mls-bot"],
            "event_types": ["bot_unavailable"],
            "capabilities": ["ReadMessages"],
        })),
    );
    let resp = dispatch.handle_message(req, None, None).await?.unwrap();
    let JsonRpcMessage::Response { .. } = resp else {
        panic!("expected handler.register to accept bot_unavailable, got {resp:?}");
    };

    relay.stop().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// U14: admin-set model and restoration
// ---------------------------------------------------------------------------

/// Build a signed kind:443 KeyPackage event for `keys`, published against a
/// fresh single-shot `MlsEngineHandle` -- mirrors the private helper in
/// `src/mls.rs`'s own test module, duplicated here because that one is not
/// exported.
async fn build_key_package_event(engine: &MlsEngineHandle, keys: &Keys) -> nostr::Event {
    let relays = vec![RelayUrl::parse("wss://test.relay").unwrap()];
    let (content, tags) = engine
        .publish_key_package(&keys.public_key(), relays)
        .await
        .expect("publish_key_package");
    pacto_bot_api::nostr_json::sign_builder(
        EventBuilder::new(Kind::MlsKeyPackage, content).tags(tags),
        keys,
    )
    .expect("sign key package")
}

/// req(R11)
#[tokio::test(flavor = "multi_thread")]
async fn create_mls_group_default_admins_are_creator_and_recipient() -> Result<(), Box<dyn Error>> {
    let relay = MockRelay::start().await?;
    let keys = Keys::generate();
    let member = MockMlsPeer::new();
    let member_npub = member.public_key().to_bech32()?;
    let (dispatch, cm) = setup_dispatch_with_relay(
        vec![bot_config_with_mls(
            "mls-bot",
            &keys,
            &["CreateMlsGroup"],
            "mls.db",
        )],
        &relay.url(),
    )
    .await;
    let handler_id = register_handler(&dispatch, &["mls-bot"], &["CreateMlsGroup"]).await;

    relay
        .inject_event(member.create_key_package_event(vec![relay.url()]).await)
        .await;
    let req = JsonRpcMessage::request(
        2.into(),
        "agent.create_mls_group",
        Some(json!({
            "bot_id": "mls-bot",
            "group_name": "test-squad",
            "recipient": member_npub,
        })),
    );
    let resp = dispatch
        .handle_message(req, Some(&handler_id), None)
        .await?
        .unwrap();
    let wire_id = parse_mls_response(&resp);

    let mls = {
        let cm = cm.read().await;
        cm.get_bot_by_id("mls-bot")
            .expect("bot exists")
            .mls
            .clone()
            .expect("mls enabled")
    };
    let groups = mls.list_groups().await?;
    let group = groups
        .iter()
        .find(|g| g.wire_id == wire_id)
        .expect("created group present");
    let admins: std::collections::BTreeSet<_> = group.admin_pubkeys.iter().copied().collect();
    assert_eq!(
        admins,
        std::collections::BTreeSet::from([keys.public_key(), member.public_key()]),
        "default admin set must be exactly creator + invited recipient"
    );

    relay.stop().await;
    Ok(())
}

/// req(R11, R35)
#[tokio::test(flavor = "multi_thread")]
async fn create_mls_group_explicit_admins_list_is_honoured() -> Result<(), Box<dyn Error>> {
    let relay = MockRelay::start().await?;
    let keys = Keys::generate();
    let member = MockMlsPeer::new();
    let member_npub = member.public_key().to_bech32()?;
    let (dispatch, cm) = setup_dispatch_with_relay(
        vec![bot_config_with_mls(
            "mls-bot",
            &keys,
            &["CreateMlsGroup"],
            "mls.db",
        )],
        &relay.url(),
    )
    .await;
    let handler_id = register_handler(&dispatch, &["mls-bot"], &["CreateMlsGroup"]).await;

    relay
        .inject_event(member.create_key_package_event(vec![relay.url()]).await)
        .await;
    // Explicit admins list names only the creator -- additive-only, but
    // proves the default (creator+recipient) is NOT silently applied on
    // top of an explicit list.
    let req = JsonRpcMessage::request(
        2.into(),
        "agent.create_mls_group",
        Some(json!({
            "bot_id": "mls-bot",
            "group_name": "test-squad",
            "recipient": member_npub,
            "admins": [keys.public_key().to_bech32()?],
        })),
    );
    let resp = dispatch
        .handle_message(req, Some(&handler_id), None)
        .await?
        .unwrap();
    let wire_id = parse_mls_response(&resp);

    let mls = {
        let cm = cm.read().await;
        cm.get_bot_by_id("mls-bot")
            .expect("bot exists")
            .mls
            .clone()
            .expect("mls enabled")
    };
    let groups = mls.list_groups().await?;
    let group = groups
        .iter()
        .find(|g| g.wire_id == wire_id)
        .expect("created group present");
    assert_eq!(
        group.admin_pubkeys,
        vec![keys.public_key()],
        "explicit admins list must be honoured verbatim, not widened with the recipient"
    );

    relay.stop().await;
    Ok(())
}

/// req(R11, R12)
#[tokio::test(flavor = "multi_thread")]
async fn repair_mls_group_admins_on_held_sole_admin_group_produces_two_admins()
-> Result<(), Box<dyn Error>> {
    let relay = MockRelay::start().await?;
    let keys = Keys::generate();
    let member = MockMlsPeer::new();
    let member_npub = member.public_key().to_bech32()?;
    let (dispatch, _cm) = setup_dispatch_with_relay(
        vec![bot_config_with_mls(
            "mls-bot",
            &keys,
            &["CreateMlsGroup"],
            "mls.db",
        )],
        &relay.url(),
    )
    .await;
    let create_handler = register_handler(&dispatch, &["mls-bot"], &["CreateMlsGroup"]).await;
    let admin_handler = register_handler(&dispatch, &[], &["Admin"]).await;

    relay
        .inject_event(member.create_key_package_event(vec![relay.url()]).await)
        .await;
    // Explicit sole-admin creation -- the pre-parity default this unit
    // exists to stop creating going forward, but still repairable once
    // held.
    let req = JsonRpcMessage::request(
        2.into(),
        "agent.create_mls_group",
        Some(json!({
            "bot_id": "mls-bot",
            "group_name": "test-squad",
            "recipient": member_npub,
            "admins": [keys.public_key().to_bech32()?],
        })),
    );
    let resp = dispatch
        .handle_message(req, Some(&create_handler), None)
        .await?
        .unwrap();
    let wire_id = parse_mls_response(&resp);

    let repair_req = JsonRpcMessage::request(
        3.into(),
        "admin.repair_mls_group_admins",
        Some(json!({
            "bot_id": "mls-bot",
            "group_name": "test-squad",
        })),
    );
    let resp = dispatch
        .handle_message(repair_req, Some(&admin_handler), None)
        .await?
        .unwrap();
    let JsonRpcMessage::Response { result, .. } = resp else {
        panic!("expected repair response, got {resp:?}");
    };
    let result: pacto_bot_api::transport::protocol::RepairMlsGroupAdminsResponse =
        serde_json::from_value(result.unwrap())?;
    assert_eq!(
        result.admins.len(),
        2,
        "repair must expand a sole-admin group to every current member: {:?}",
        result.admins
    );
    let admin_set: std::collections::BTreeSet<_> = result.admins.iter().collect();
    assert!(admin_set.contains(&keys.public_key().to_bech32()?));
    assert!(admin_set.contains(&member.public_key().to_bech32()?));

    let events = relay
        .wait_for_event(|e| e.kind == Kind::MlsGroupMessage, Duration::from_secs(5))
        .await?;
    assert_eq!(
        evolution_event_count(&events),
        1,
        "repair must publish the resulting evolution event"
    );

    let _ = wire_id;
    relay.stop().await;
    Ok(())
}

/// req(R12)
#[tokio::test(flavor = "multi_thread")]
async fn repair_mls_group_admins_on_state_lost_group_with_invited_member_names_restoration()
-> Result<(), Box<dyn Error>> {
    let relay = MockRelay::start().await?;
    let keys = Keys::generate();
    let config = DaemonConfig {
        daemon: GlobalDaemonConfig::default(),
        bots: vec![bot_config_with_mls(
            "mls-bot",
            &keys,
            &["CreateMlsGroup"],
            "mls.db",
        )],
    };
    let nostr_client = NostrClient::new(vec![relay.url()]).await?;
    let dir = common::tempdir()?;
    let db = Db::open(dir.path().join("agent.db").as_path()).await?;
    let cm = Arc::new(RwLock::new(
        ClientManager::new(dir.path(), config, nostr_client, &db).await?,
    ));
    let dispatch = Arc::new(Dispatch::new(cm, db.clone(), Diagnostics::new()));
    let admin_handler = register_handler(&dispatch, &[], &["Admin"]).await;

    db.insert_mls_group(MlsGroupRow {
        bot_id: "mls-bot".into(),
        group_name: "test-squad".into(),
        wire_id: "a".repeat(64),
        creator_npub: keys.public_key().to_bech32()?,
        relay: relay.url(),
        invited_bots: vec!["npub1invitedmember".into()],
        state_lost_at: None,
    })
    .await?;
    db.mark_mls_group_state_lost("mls-bot", "test-squad", chrono::Utc::now().timestamp())
        .await?;

    let req = JsonRpcMessage::request(
        2.into(),
        "admin.repair_mls_group_admins",
        Some(json!({
            "bot_id": "mls-bot",
            "group_name": "test-squad",
        })),
    );
    let resp = dispatch
        .handle_message(req, Some(&admin_handler), None)
        .await?
        .unwrap();
    let JsonRpcMessage::Error { error, .. } = resp else {
        panic!("expected repair to refuse a state-lost group, got {resp:?}");
    };
    assert_ne!(
        error.code, -32015,
        "state-lost refusal must not be a bare GroupNotFound"
    );
    assert!(
        error.message.contains("restoration"),
        "message must name restoration as the prerequisite when a member was cached: {}",
        error.message
    );

    relay.stop().await;
    Ok(())
}

/// req(R12)
#[tokio::test(flavor = "multi_thread")]
async fn repair_mls_group_admins_on_state_lost_group_with_no_cached_member_names_recreation()
-> Result<(), Box<dyn Error>> {
    let relay = MockRelay::start().await?;
    let keys = Keys::generate();
    let config = DaemonConfig {
        daemon: GlobalDaemonConfig::default(),
        bots: vec![bot_config_with_mls(
            "mls-bot",
            &keys,
            &["CreateMlsGroup"],
            "mls.db",
        )],
    };
    let nostr_client = NostrClient::new(vec![relay.url()]).await?;
    let dir = common::tempdir()?;
    let db = Db::open(dir.path().join("agent.db").as_path()).await?;
    let cm = Arc::new(RwLock::new(
        ClientManager::new(dir.path(), config, nostr_client, &db).await?,
    ));
    let dispatch = Arc::new(Dispatch::new(cm, db.clone(), Diagnostics::new()));
    let admin_handler = register_handler(&dispatch, &[], &["Admin"]).await;

    db.insert_mls_group(MlsGroupRow {
        bot_id: "mls-bot".into(),
        group_name: "test-squad".into(),
        wire_id: "b".repeat(64),
        creator_npub: keys.public_key().to_bech32()?,
        relay: relay.url(),
        invited_bots: vec![],
        state_lost_at: None,
    })
    .await?;
    db.mark_mls_group_state_lost("mls-bot", "test-squad", chrono::Utc::now().timestamp())
        .await?;

    let req = JsonRpcMessage::request(
        2.into(),
        "admin.repair_mls_group_admins",
        Some(json!({
            "bot_id": "mls-bot",
            "group_name": "test-squad",
        })),
    );
    let resp = dispatch
        .handle_message(req, Some(&admin_handler), None)
        .await?
        .unwrap();
    let JsonRpcMessage::Error { error, .. } = resp else {
        panic!("expected repair to refuse a state-lost group, got {resp:?}");
    };
    assert_ne!(
        error.code, -32015,
        "state-lost refusal must not be a bare GroupNotFound"
    );
    assert!(
        error.message.contains("re-creation"),
        "message must name re-creation when no member was ever cached: {}",
        error.message
    );

    relay.stop().await;
    Ok(())
}

/// req(R12)
#[tokio::test(flavor = "multi_thread")]
async fn repair_mls_group_admins_by_non_admin_bot_refuses() -> Result<(), Box<dyn Error>> {
    let relay = MockRelay::start().await?;
    let bot_keys = Keys::generate();
    let config = DaemonConfig {
        daemon: GlobalDaemonConfig::default(),
        bots: vec![bot_config_with_mls(
            "mls-bot",
            &bot_keys,
            &["ReceiveGroupMessages"],
            "mls.db",
        )],
    };
    let nostr_client = NostrClient::new(vec![relay.url()]).await?;
    let dir = common::tempdir()?;
    let db = Db::open(dir.path().join("agent.db").as_path()).await?;
    let cm = Arc::new(RwLock::new(
        ClientManager::new(dir.path(), config, nostr_client, &db).await?,
    ));
    let dispatch = Arc::new(Dispatch::new(cm.clone(), db.clone(), Diagnostics::new()));
    let admin_handler = register_handler(&dispatch, &[], &["Admin"]).await;

    // An external admin creates the group with the bot as a plain
    // (non-admin) member, mirroring
    // `handler_with_capabilities_can_create_and_invite`'s reversed-roles
    // setup elsewhere in this crate.
    let external_temp = common::tempdir()?;
    let external_admin = MlsEngineHandle::new_persistent(external_temp.path().join("ext.db"))?;
    let admin_keys = Keys::generate();

    let bot_mls = {
        let cm = cm.read().await;
        cm.get_bot_by_id("mls-bot")
            .expect("bot exists")
            .mls
            .clone()
            .expect("mls enabled")
    };
    let bot_key_package = build_key_package_event(&bot_mls, &bot_keys).await;

    let (wire_id, welcome_rumor) = external_admin
        .create_group(
            admin_keys.public_key(),
            bot_keys.public_key(),
            bot_key_package,
            "test-squad".to_string(),
            vec![RelayUrl::parse("wss://test.relay").unwrap()],
            vec![admin_keys.public_key()],
        )
        .await?;
    bot_mls
        .process_welcome_and_return_wire_id(nostr::EventId::all_zeros(), welcome_rumor)
        .await?;

    db.insert_mls_group(MlsGroupRow {
        bot_id: "mls-bot".into(),
        group_name: "test-squad".into(),
        wire_id,
        creator_npub: admin_keys.public_key().to_bech32()?,
        relay: relay.url(),
        invited_bots: vec![bot_keys.public_key().to_bech32()?],
        state_lost_at: None,
    })
    .await?;

    let req = JsonRpcMessage::request(
        2.into(),
        "admin.repair_mls_group_admins",
        Some(json!({
            "bot_id": "mls-bot",
            "group_name": "test-squad",
        })),
    );
    let resp = dispatch
        .handle_message(req, Some(&admin_handler), None)
        .await?
        .unwrap();
    let JsonRpcMessage::Error { error, .. } = resp else {
        panic!("expected repair by a non-admin bot to refuse, got {resp:?}");
    };
    assert_ne!(
        error.code, -32029,
        "a live non-admin refusal is not the state-lost repair-prerequisite code"
    );

    relay.stop().await;
    Ok(())
}

/// req(R12, R13)
#[tokio::test(flavor = "multi_thread")]
async fn add_member_restoration_removes_old_leaf_so_it_stops_decrypting()
-> Result<(), Box<dyn Error>> {
    let temp = common::tempdir()?;
    let bot_keys = Keys::generate();
    let member_keys = Keys::generate();
    let bot_engine = MlsEngineHandle::new_persistent(temp.path().join("bot.db"))?;
    let member_engine_old = MlsEngineHandle::new_persistent(temp.path().join("member-old.db"))?;

    let key_package = build_key_package_event(&member_engine_old, &member_keys).await;
    let (wire_id, welcome_rumor) = bot_engine
        .create_group(
            bot_keys.public_key(),
            member_keys.public_key(),
            key_package,
            "test-group".to_string(),
            vec![RelayUrl::parse("wss://test.relay").unwrap()],
            vec![bot_keys.public_key()],
        )
        .await?;
    member_engine_old
        .process_welcome_and_return_wire_id(nostr::EventId::all_zeros(), welcome_rumor)
        .await?;

    // Restore: the recipient already holds a leaf (from the create above),
    // so this must remove-then-re-add rather than plain-add.
    let fresh_key_package = build_key_package_event(&member_engine_old, &member_keys).await;
    let outcome = bot_engine
        .add_member(&wire_id, member_keys.public_key(), fresh_key_package)
        .await?;
    assert!(
        outcome.remove_evolution_event.is_some(),
        "re-adding an existing leaf must be detected as a restoration"
    );

    // The old engine instance never processes the new welcome or the
    // remove/add commits -- it still holds only the OLD (now-evicted)
    // leaf. It must not be able to decrypt a message sent to the new
    // epoch.

    relay_free_epoch_check(&bot_engine, &member_engine_old, &wire_id).await?;
    Ok(())
}

/// Send one application message from `bot_engine` and assert
/// `stale_engine` (an old, now-evicted leaf) fails to decrypt it.
async fn relay_free_epoch_check(
    bot_engine: &MlsEngineHandle,
    stale_engine: &MlsEngineHandle,
    wire_id: &str,
) -> Result<(), Box<dyn Error>> {
    let mls_group_id = bot_engine.resolve_wire_id(wire_id).await?;
    let rumor = pacto_bot_api::nostr::NostrClient::build_group_text_rumor(
        &Keys::generate().public_key(),
        "post-restoration".to_string(),
        None,
    )?;
    let event = bot_engine.create_group_message(mls_group_id, rumor).await?;

    let outcome = stale_engine.decrypt_group_message(&event).await;
    match outcome {
        Ok(pacto_bot_api::mls::GroupMessageOutcome::Message(_)) => {
            panic!("a removed leaf must not be able to decrypt a post-restoration message")
        }
        _ => Ok(()),
    }
}

/// req(R12, R13)
#[tokio::test(flavor = "multi_thread")]
async fn add_member_restoration_publishes_remove_then_add_and_third_party_still_decrypts()
-> Result<(), Box<dyn Error>> {
    let temp = common::tempdir()?;
    let admin_keys = Keys::generate();
    let restoring_keys = Keys::generate();
    let staying_keys = Keys::generate();
    let admin_engine = MlsEngineHandle::new_persistent(temp.path().join("admin.db"))?;
    let restoring_engine = MlsEngineHandle::new_persistent(temp.path().join("restoring.db"))?;
    let staying_engine = MlsEngineHandle::new_persistent(temp.path().join("staying.db"))?;

    let restoring_kp = build_key_package_event(&restoring_engine, &restoring_keys).await;
    let (wire_id, restoring_welcome) = admin_engine
        .create_group(
            admin_keys.public_key(),
            restoring_keys.public_key(),
            restoring_kp,
            "test-group".to_string(),
            vec![RelayUrl::parse("wss://test.relay").unwrap()],
            vec![admin_keys.public_key()],
        )
        .await?;
    restoring_engine
        .process_welcome_and_return_wire_id(nostr::EventId::all_zeros(), restoring_welcome)
        .await?;

    let staying_kp = build_key_package_event(&staying_engine, &staying_keys).await;
    let staying_outcome = admin_engine
        .add_member(&wire_id, staying_keys.public_key(), staying_kp)
        .await?;
    assert!(staying_outcome.remove_evolution_event.is_none());
    staying_engine
        .process_welcome_and_return_wire_id(
            nostr::EventId::all_zeros(),
            staying_outcome.welcome_rumor,
        )
        .await?;
    restoring_engine
        .decrypt_group_message(&staying_outcome.evolution_event)
        .await?;

    // Restore the first member. Both commits must reach every peer, in
    // remove-then-add order, before the group is usable again.
    let fresh_kp = build_key_package_event(&restoring_engine, &restoring_keys).await;
    let restoration = admin_engine
        .add_member(&wire_id, restoring_keys.public_key(), fresh_kp)
        .await?;
    let remove_evt = restoration
        .remove_evolution_event
        .expect("re-adding an already-present member must be a restoration");

    staying_engine.decrypt_group_message(&remove_evt).await?;
    staying_engine
        .decrypt_group_message(&restoration.evolution_event)
        .await?;

    // The third-party (uninvolved) member must still be able to decrypt
    // the next application message -- proves neither commit was dropped.
    let mls_group_id = admin_engine.resolve_wire_id(&wire_id).await?;
    let rumor = pacto_bot_api::nostr::NostrClient::build_group_text_rumor(
        &admin_keys.public_key(),
        "after restore".to_string(),
        None,
    )?;
    let next_message = admin_engine
        .create_group_message(mls_group_id, rumor)
        .await?;
    let outcome = staying_engine.decrypt_group_message(&next_message).await?;
    assert!(
        matches!(outcome, pacto_bot_api::mls::GroupMessageOutcome::Message(_)),
        "third-party peer must still decrypt after both restoration commits publish"
    );

    Ok(())
}

/// req(R11, R12)
#[tokio::test(flavor = "multi_thread")]
async fn add_member_first_time_invite_returns_no_remove_evolution_event()
-> Result<(), Box<dyn Error>> {
    let temp = common::tempdir()?;
    let bot_keys = Keys::generate();
    let member1_keys = Keys::generate();
    let member2_keys = Keys::generate();
    let bot_engine = MlsEngineHandle::new_persistent(temp.path().join("bot.db"))?;
    let member1_engine = MlsEngineHandle::new_persistent(temp.path().join("member1.db"))?;

    let kp1 = build_key_package_event(&member1_engine, &member1_keys).await;
    let (wire_id, _welcome) = bot_engine
        .create_group(
            bot_keys.public_key(),
            member1_keys.public_key(),
            kp1,
            "test-group".to_string(),
            vec![RelayUrl::parse("wss://test.relay").unwrap()],
            vec![bot_keys.public_key()],
        )
        .await?;

    // First-time invite for a brand-new pubkey never seen in this group --
    // must take the plain single-add path (no remove commit), distinct
    // from the restoration path exercised above.
    let member2_engine = MlsEngineHandle::new_persistent(temp.path().join("member2.db"))?;
    let kp2 = build_key_package_event(&member2_engine, &member2_keys).await;
    let outcome = bot_engine
        .add_member(&wire_id, member2_keys.public_key(), kp2)
        .await?;
    assert!(
        outcome.remove_evolution_event.is_none(),
        "a first-time invite must not produce a remove commit"
    );

    Ok(())
}

/// req(R12)
#[tokio::test(flavor = "multi_thread")]
async fn add_member_restoration_by_non_admin_engine_is_refused_before_any_commit()
-> Result<(), Box<dyn Error>> {
    let temp = common::tempdir()?;
    let admin_keys = Keys::generate();
    let member1_keys = Keys::generate();
    let member2_keys = Keys::generate();
    let admin_engine = MlsEngineHandle::new_persistent(temp.path().join("admin.db"))?;
    let member1_engine = MlsEngineHandle::new_persistent(temp.path().join("member1.db"))?;
    let member2_engine = MlsEngineHandle::new_persistent(temp.path().join("member2.db"))?;

    let kp1 = build_key_package_event(&member1_engine, &member1_keys).await;
    let (wire_id, welcome1) = admin_engine
        .create_group(
            admin_keys.public_key(),
            member1_keys.public_key(),
            kp1,
            "test-group".to_string(),
            vec![RelayUrl::parse("wss://test.relay").unwrap()],
            vec![admin_keys.public_key()],
        )
        .await?;
    member1_engine
        .process_welcome_and_return_wire_id(nostr::EventId::all_zeros(), welcome1)
        .await?;

    let kp2 = build_key_package_event(&member2_engine, &member2_keys).await;
    let add2 = admin_engine
        .add_member(&wire_id, member2_keys.public_key(), kp2)
        .await?;
    member1_engine
        .decrypt_group_message(&add2.evolution_event)
        .await?;
    member2_engine
        .process_welcome_and_return_wire_id(nostr::EventId::all_zeros(), add2.welcome_rumor)
        .await?;

    // member1 is a plain (non-admin) member; it must be refused before any
    // commit when it attempts to restore member2.
    let fresh_kp2 = build_key_package_event(&member2_engine, &member2_keys).await;
    let result = member1_engine
        .add_member(&wire_id, member2_keys.public_key(), fresh_kp2)
        .await;
    assert!(
        !matches!(result, Err(MlsError::RestorationIncomplete { .. })),
        "a non-admin refusal must occur before any remove commit, not after: {result:?}"
    );
    assert!(result.is_err());

    Ok(())
}

/// req(R12)
#[tokio::test(flavor = "multi_thread")]
async fn add_member_mid_restoration_failure_returns_restoration_incomplete()
-> Result<(), Box<dyn Error>> {
    let temp = common::tempdir()?;
    let bot_keys = Keys::generate();
    let member_keys = Keys::generate();
    let bot_engine = MlsEngineHandle::new_persistent(temp.path().join("bot.db"))?;
    let member_engine = MlsEngineHandle::new_persistent(temp.path().join("member.db"))?;

    let kp = build_key_package_event(&member_engine, &member_keys).await;
    let (wire_id, _welcome) = bot_engine
        .create_group(
            bot_keys.public_key(),
            member_keys.public_key(),
            kp,
            "test-group".to_string(),
            vec![RelayUrl::parse("wss://test.relay").unwrap()],
            vec![bot_keys.public_key()],
        )
        .await?;

    // A structurally valid Nostr event (correct kind, non-empty content,
    // correct author, valid signature) but garbage MLS KeyPackage payload
    // -- passes `validate_key_package`'s Nostr-level checks but fails deep
    // inside the engine's `add_members`, AFTER the remove commit for this
    // already-present member has merged. Mirrors the existing
    // `create_group_bad_key_package_content_maps_to_safe_engine_error`
    // technique in `src/mls.rs`.
    let garbage_key_package = pacto_bot_api::nostr_json::sign_builder(
        EventBuilder::new(Kind::MlsKeyPackage, "invalid-key-package-content"),
        &member_keys,
    )?;

    let result = bot_engine
        .add_member(&wire_id, member_keys.public_key(), garbage_key_package)
        .await;
    match result {
        Err(MlsError::RestorationIncomplete {
            remove_evolution_event,
        }) => {
            assert_eq!(remove_evolution_event.kind, Kind::MlsGroupMessage);
        }
        other => panic!(
            "expected RestorationIncomplete naming the member outside the group, got {other:?}"
        ),
    }

    Ok(())
}
