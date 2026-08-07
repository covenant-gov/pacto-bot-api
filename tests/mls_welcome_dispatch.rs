//! Tests for MLS Welcome gift-wrap processing.
//!
//! req(R-pacto-bot-api-welcome-process)
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
mod support;

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use nostr::{Keys, ToBech32};
use pacto_bot_api::client_manager::ClientManager;
use pacto_bot_api::config::{BotConfig, DaemonConfig, GlobalDaemonConfig, SigningConfig};
use pacto_bot_api::db::Db;
use pacto_bot_api::diagnostics::Diagnostics;
use pacto_bot_api::events::EventType;
use pacto_bot_api::mls::MlsEngineHandle;
use pacto_bot_api::nostr::NostrClient;
use pacto_bot_api::signer::Signer;
use secrecy::SecretString;
use support::mock_mls_peer::{MockMlsPeer, gift_wrap_welcome};
use support::mock_relay::MockRelay;

fn bot_config(id: &str, keys: &Keys) -> BotConfig {
    BotConfig {
        id: id.to_string(),
        display_name: Some(format!("{} Display", id)),
        npub: keys.public_key().to_bech32().unwrap(),
        signing: SigningConfig::Nsec {
            nsec: SecretString::new(keys.secret_key().to_bech32().unwrap().into()),
        },
        relays: vec![],
        capabilities: vec!["ReceiveGroupMessages".into(), "SendGroupMessages".into()],
        mls_dedup_window_secs: None,
        mls_db_path: Some(std::path::PathBuf::from("vector-mls.db")),
        mls_key_package_freshness_secs: None,
        ..Default::default()
    }
}

async fn setup_nostr_client_with_bot() -> Result<
    (
        Keys,
        NostrClient,
        MlsEngineHandle,
        MockRelay,
        tempfile::TempDir,
    ),
    Box<dyn std::error::Error>,
> {
    let keys = Keys::generate();
    let bot = bot_config("welcome-bot", &keys);
    let config = DaemonConfig {
        daemon: GlobalDaemonConfig::default(),
        bots: vec![bot],
    };
    let dir = common::tempdir()?;
    let relay = MockRelay::start().await?;
    let nostr_client = NostrClient::new(vec![relay.url()]).await?;
    let db = Db::open(dir.path().join("test.db").as_path()).await?;
    let mut cm = ClientManager::new(dir.path(), config, nostr_client.clone(), &db).await?;
    cm.subscribe_bots(&db).await?;

    let bot = cm.get_bot_by_id("welcome-bot").expect("bot exists");
    let signer = bot.signer.clone();
    let mls = bot.mls.clone().expect("mls engine configured");
    let pubkey = bot.signer.public_key();
    nostr_client
        .add_signer(pubkey, "welcome-bot".to_string(), Arc::new(signer))
        .await;
    nostr_client
        .add_mls_engine(pubkey, "welcome-bot".to_string(), mls.clone())
        .await;

    relay.wait_for_subscription(Duration::from_secs(5)).await?;

    Ok((keys, nostr_client, mls, relay, dir))
}

async fn setup_nostr_client_with_bot_and_diagnostics() -> Result<
    (
        Keys,
        NostrClient,
        MlsEngineHandle,
        MockRelay,
        tempfile::TempDir,
        Diagnostics,
    ),
    Box<dyn std::error::Error>,
> {
    let keys = Keys::generate();
    let bot = bot_config("welcome-bot", &keys);
    let config = DaemonConfig {
        daemon: GlobalDaemonConfig::default(),
        bots: vec![bot],
    };
    let dir = common::tempdir()?;
    let relay = MockRelay::start().await?;
    let diagnostics = Diagnostics::new();
    let nostr_client = NostrClient::new(vec![relay.url()])
        .await?
        .with_diagnostics(diagnostics.clone());
    let db = Db::open(dir.path().join("test.db").as_path()).await?;
    let mut cm = ClientManager::new(dir.path(), config, nostr_client.clone(), &db).await?;
    cm.subscribe_bots(&db).await?;

    let bot = cm.get_bot_by_id("welcome-bot").expect("bot exists");
    let signer = bot.signer.clone();
    let mls = bot.mls.clone().expect("mls engine configured");
    let pubkey = bot.signer.public_key();
    nostr_client
        .add_signer(pubkey, "welcome-bot".to_string(), Arc::new(signer))
        .await;
    nostr_client
        .add_mls_engine(pubkey, "welcome-bot".to_string(), mls.clone())
        .await;

    relay.wait_for_subscription(Duration::from_secs(5)).await?;

    Ok((keys, nostr_client, mls, relay, dir, diagnostics))
}

/// Build the bot's own KeyPackage event, signed by `keys`, via its MLS engine.
async fn bot_key_package_event(keys: &Keys, mls: &MlsEngineHandle) -> nostr::Event {
    let bot_pubkey = keys.public_key();
    let key_package = mls
        .publish_key_package(
            &bot_pubkey,
            vec![nostr::RelayUrl::parse("wss://test.relay").unwrap()],
        )
        .await
        .expect("publish key package");
    let unsigned_kp = nostr::UnsignedEvent::new(
        bot_pubkey,
        nostr::Timestamp::now(),
        nostr::Kind::MlsKeyPackage,
        key_package.1,
        key_package.0,
    );
    unsigned_kp.sign(keys).await.expect("sign key package")
}

/// Strip the `encoding` tag from a welcome rumor and clear its `id` so a
/// subsequent `.sign()` recomputes it from the mutated tags/content.
fn strip_encoding_tag(mut rumor: nostr::UnsignedEvent) -> nostr::UnsignedEvent {
    rumor.tags = rumor
        .tags
        .iter()
        .filter(|t| t.as_slice().first().map(String::as_str) != Some("encoding"))
        .cloned()
        .collect();
    rumor.id = None;
    rumor
}

/// req(U13)
#[tokio::test]
async fn welcome_missing_encoding_tag_is_recorded_as_version_mismatch()
-> Result<(), Box<dyn std::error::Error>> {
    let (keys, client, mls, _relay, _dir, diagnostics) =
        setup_nostr_client_with_bot_and_diagnostics().await?;
    let bot_pubkey = keys.public_key();
    let key_package_event = bot_key_package_event(&keys, &mls).await;

    let peer = MockMlsPeer::new();
    let (_group_result, welcome_rumor) = peer.create_group_with(&key_package_event);
    let welcome_rumor = strip_encoding_tag(welcome_rumor);
    let gift_wrap = gift_wrap_welcome(&peer.keys, &bot_pubkey, welcome_rumor).await;

    let result = client.decrypt_event(&gift_wrap).await;
    assert!(
        result.is_err(),
        "a welcome missing the encoding tag should not produce an AgentEvent"
    );

    let snapshot = diagnostics.snapshot().await;
    assert_eq!(snapshot.welcome_version_mismatch_total, 1);
    assert_eq!(snapshot.welcome_rejected_total, 0);
    assert!(
        snapshot.errors.is_empty(),
        "an unsolicited welcome rejection must not push a per-event ErrorRecord"
    );

    Ok(())
}

/// req(U13)
#[tokio::test]
async fn structurally_invalid_tagged_welcome_is_recorded_as_rejected()
-> Result<(), Box<dyn std::error::Error>> {
    let (keys, client, mls, _relay, _dir, diagnostics) =
        setup_nostr_client_with_bot_and_diagnostics().await?;
    let bot_pubkey = keys.public_key();
    let key_package_event = bot_key_package_event(&keys, &mls).await;

    let peer = MockMlsPeer::new();
    let (_group_result, mut welcome_rumor) = peer.create_group_with(&key_package_event);
    // Tags (including `encoding`) stay intact; only the content is garbage,
    // so this is a structurally invalid Welcome that IS correctly tagged.
    welcome_rumor.content = base64::engine::general_purpose::STANDARD
        .encode(b"not a valid mls welcome payload, definitely not tls-encoded");
    welcome_rumor.id = None;
    let gift_wrap = gift_wrap_welcome(&peer.keys, &bot_pubkey, welcome_rumor).await;

    let result = client.decrypt_event(&gift_wrap).await;
    assert!(
        result.is_err(),
        "a structurally invalid welcome should not produce an AgentEvent"
    );

    let snapshot = diagnostics.snapshot().await;
    assert_eq!(snapshot.welcome_rejected_total, 1);
    assert_eq!(
        snapshot.welcome_version_mismatch_total, 0,
        "a correctly-tagged but structurally invalid welcome must not be counted as a version mismatch"
    );
    assert!(
        snapshot.errors.is_empty(),
        "an unsolicited welcome rejection must not push a per-event ErrorRecord"
    );

    Ok(())
}

/// req(U13)
#[tokio::test]
async fn fifty_malformed_welcomes_from_distinct_peers_do_not_evict_diagnostics_or_grow_state()
-> Result<(), Box<dyn std::error::Error>> {
    let (keys, client, mls, _relay, _dir, diagnostics) =
        setup_nostr_client_with_bot_and_diagnostics().await?;
    let bot_pubkey = keys.public_key();
    let key_package_event = bot_key_package_event(&keys, &mls).await;

    // A prior genuine diagnostic entry that must survive the spray below.
    diagnostics
        .record_error(Some("genuine"), "a genuine prior error", None)
        .await;

    for _ in 0..50 {
        // A fresh peer identity each iteration -- 50 distinct peer keys.
        let peer = MockMlsPeer::new();
        let (_group_result, welcome_rumor) = peer.create_group_with(&key_package_event);
        let welcome_rumor = strip_encoding_tag(welcome_rumor);
        let gift_wrap = gift_wrap_welcome(&peer.keys, &bot_pubkey, welcome_rumor).await;
        let _ = client.decrypt_event(&gift_wrap).await;
    }

    let snapshot = diagnostics.snapshot().await;
    assert_eq!(
        snapshot.welcome_version_mismatch_total, 50,
        "all 50 malformed welcomes must be counted in the aggregated total"
    );
    assert_eq!(
        snapshot.errors.len(),
        1,
        "the 50 malformed welcomes from distinct peers must not evict the genuine error \
         or grow the error ring per event"
    );
    assert_eq!(snapshot.errors[0].message, "a genuine prior error");

    Ok(())
}

#[tokio::test]
async fn decrypt_event_accepts_welcome_and_returns_wire_id()
-> Result<(), Box<dyn std::error::Error>> {
    let (keys, client, mls, _relay, _dir) = setup_nostr_client_with_bot().await?;
    let bot_pubkey = keys.public_key();

    // Publish the bot's KeyPackage via the daemon MLS engine.
    let key_package = mls
        .publish_key_package(
            &bot_pubkey,
            vec![nostr::RelayUrl::parse("wss://test.relay").unwrap()],
        )
        .await
        .expect("publish key package");
    let unsigned_kp = nostr::UnsignedEvent::new(
        bot_pubkey,
        nostr::Timestamp::now(),
        nostr::Kind::MlsKeyPackage,
        key_package.1,
        key_package.0,
    );
    let key_package_event = unsigned_kp.sign(&keys).await?;

    // A peer creates a group that includes the bot.
    let peer = MockMlsPeer::new();
    let (_group_result, welcome_rumor) = peer.create_group_with(&key_package_event);
    let gift_wrap = gift_wrap_welcome(&peer.keys, &bot_pubkey, welcome_rumor).await;

    let agent_event = client.decrypt_event(&gift_wrap).await?;

    assert_eq!(agent_event.bot_id, "welcome-bot");
    assert_eq!(agent_event.event_type, EventType::MlsWelcomeReceived);
    assert!(
        agent_event.chat_id.is_some(),
        "chat_id should contain the Squad wire id after accepting the welcome"
    );
    let wire_id = agent_event.chat_id.unwrap();
    assert_eq!(wire_id.len(), 64, "wire id should be 64 hex characters");

    Ok(())
}

#[tokio::test]
async fn decrypt_event_welcome_is_idempotent_on_replay() -> Result<(), Box<dyn std::error::Error>> {
    let (keys, client, mls, _relay, _dir) = setup_nostr_client_with_bot().await?;
    let bot_pubkey = keys.public_key();

    let key_package = mls
        .publish_key_package(
            &bot_pubkey,
            vec![nostr::RelayUrl::parse("wss://test.relay").unwrap()],
        )
        .await
        .expect("publish key package");
    let unsigned_kp = nostr::UnsignedEvent::new(
        bot_pubkey,
        nostr::Timestamp::now(),
        nostr::Kind::MlsKeyPackage,
        key_package.1,
        key_package.0,
    );
    let key_package_event = unsigned_kp.sign(&keys).await?;

    let peer = MockMlsPeer::new();
    let (_group_result, welcome_rumor) = peer.create_group_with(&key_package_event);
    let gift_wrap = gift_wrap_welcome(&peer.keys, &bot_pubkey, welcome_rumor).await;

    let first = client.decrypt_event(&gift_wrap).await?;
    let second = client.decrypt_event(&gift_wrap).await?;

    assert_eq!(first.bot_id, "welcome-bot");
    assert_eq!(first.event_type, EventType::MlsWelcomeReceived);
    assert_eq!(second.event_type, EventType::MlsWelcomeReceived);
    assert_eq!(
        first.chat_id, second.chat_id,
        "replayed welcome should return the same wire id"
    );
    let wire_id = first.chat_id.expect("wire id present");
    assert_eq!(wire_id.len(), 64, "wire id should be 64 hex characters");

    Ok(())
}

#[tokio::test]
async fn decrypt_event_dm_without_mls_engine_still_works() -> Result<(), Box<dyn std::error::Error>>
{
    let client = NostrClient::new(vec![]).await?;
    let (bot_signer, _bot_npub) = test_signer();
    let bot_pubkey = bot_signer.public_key();
    let sender_keys = nostr::Keys::generate();

    client
        .add_signer(bot_pubkey, "bot-1".into(), Arc::new(bot_signer))
        .await;

    let event = nostr::EventBuilder::private_msg(
        &sender_keys,
        bot_pubkey,
        "secret message",
        Vec::<nostr::Tag>::new(),
    )
    .await
    .unwrap();

    let agent_event = client.decrypt_event(&event).await?;

    assert_eq!(agent_event.bot_id, "bot-1");
    assert_eq!(agent_event.event_type, EventType::DmReceived);
    assert_eq!(agent_event.content, "secret message");
    assert!(agent_event.chat_id.is_none());

    Ok(())
}

fn test_signer() -> (pacto_bot_api::signer::SignerBackend, String) {
    let keys = Keys::generate();
    let npub = keys.public_key().to_bech32().unwrap();
    let config = SigningConfig::Nsec {
        nsec: SecretString::new(keys.secret_key().to_bech32().unwrap().into()),
    };
    let signer = pacto_bot_api::signer::SignerBackend::from_config(&config, &npub)
        .expect("build signer backend");
    (signer, npub)
}
