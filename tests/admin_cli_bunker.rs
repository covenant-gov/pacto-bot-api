mod common;
mod support;

/// req(R6)
use std::error::Error;
use std::time::Duration;

use assert_cmd::Command;
use nostr::NostrSigner as NostrSignerTrait;
use nostr::{Keys, Kind, Timestamp, ToBech32, UnsignedEvent};
use pacto_bot_api::signer::{Signer, SignerBackend};
use serde_json::json;
use support::mock_bunker::MockBunker;
use support::mock_relay::MockRelay;

#[tokio::test]
async fn test_bunker_match() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let relay = support::mock_relay::MockRelay::start().await?;

    let (mut bot, bunker_keys) = common::generate_bunker_bot_with_keys("echo-bot", true)?;
    let bunker = support::mock_bunker::MockBunker::new(bunker_keys, vec![relay.url()]).await?;

    let uri = bunker
        .uri_from_relays(&[relay.url()])
        .ok_or("mock bunker produced no URI")?;
    common::set_bunker_uri(&mut bot, &uri);

    // Wait for the signer to bootstrap and subscribe before the CLI connects.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let config = common::make_config(&dir, vec![bot])?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &config.to_string_lossy(),
        "test-bunker",
        "echo-bot",
    ]);
    // Run the CLI in a blocking task so this test's Tokio runtime keeps
    // polling the mock relay and bunker while the child process runs.
    let output = tokio::task::spawn_blocking(move || cmd.assert().success()).await?;
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;
    assert!(stdout.contains("bunker public key matches npub"));

    bunker.stop().await;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn test_bunker_mismatch() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let relay = support::mock_relay::MockRelay::start().await?;

    // Generate a bot config whose configured npub does not match the bunker.
    let (mut bot, bunker_keys) = common::generate_bunker_bot_with_keys("echo-bot", false)?;
    let bunker = support::mock_bunker::MockBunker::new(bunker_keys, vec![relay.url()]).await?;

    let uri = bunker
        .uri_from_relays(&[relay.url()])
        .ok_or("mock bunker produced no URI")?;
    common::set_bunker_uri(&mut bot, &uri);

    // Wait for the signer to bootstrap and subscribe before the CLI connects.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let config = common::make_config(&dir, vec![bot])?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &config.to_string_lossy(),
        "test-bunker",
        "echo-bot",
    ]);
    tokio::task::spawn_blocking(move || cmd.assert().failure()).await?;

    bunker.stop().await;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn verify_bunker_public_key_directly() -> Result<(), Box<dyn Error>> {
    let relay = support::mock_relay::MockRelay::start().await?;
    let keys = nostr::Keys::generate();
    let bunker = support::mock_bunker::MockBunker::new(keys.clone(), vec![relay.url()]).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let uri = bunker.uri(&relay.url());
    pacto_bot_api::nip46::verify_bunker_public_key(
        &uri,
        &keys.public_key(),
        Duration::from_secs(10),
    )
    .await?;

    bunker.stop().await;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn test_bunker_unreachable_or_invalid() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let bot = pacto_bot_api::config::BotConfig {
        id: "echo-bot".to_string(),
        npub: "npub1invalid".to_string(),
        signing: pacto_bot_api::config::SigningConfig::BunkerLocal {
            uri: pacto_bot_api::secrecy::SecretString::new("not-a-bunker-uri".into()),
        },
        relays: vec![],
        capabilities: vec![],
        ..Default::default()
    };
    let config = common::make_config(&dir, vec![bot])?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &config.to_string_lossy(),
        "test-bunker",
        "echo-bot",
    ]);
    cmd.assert().failure();
    Ok(())
}

#[tokio::test]
async fn bunker_local_sign_encrypt_decrypt() -> Result<(), Box<dyn Error>> {
    let relay = MockRelay::start().await?;
    let (mut bot, bunker_keys) = common::generate_bunker_bot_with_keys("sign-bot", true)?;
    let bunker = MockBunker::new(bunker_keys, vec![relay.url()]).await?;
    let uri = bunker
        .uri_from_relays(&[relay.url()])
        .ok_or("mock bunker produced no URI")?;
    common::set_bunker_uri(&mut bot, &uri);

    // Give the bunker time to subscribe before the signer connects.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let signer = SignerBackend::from_config(&bot.signing, &bot.npub)?;
    assert!(matches!(signer, SignerBackend::BunkerLocal(_)));

    // sign_event: build a kind-1 event preimage and obtain a signature.
    let mut unsigned = UnsignedEvent::new(
        signer.public_key(),
        Timestamp::now(),
        Kind::TextNote,
        Vec::new(),
        "hello bunker",
    );
    unsigned.ensure_id();
    let payload = serde_json::to_vec(&json!([
        0,
        unsigned.pubkey,
        unsigned.created_at,
        unsigned.kind,
        unsigned.tags,
        unsigned.content
    ]))?;
    let sig_hex = signer.sign_event(&payload).await?;
    assert!(!sig_hex.is_empty());

    // nip44_encrypt / nip44_decrypt round-trip through the bunker.
    let peer = Keys::generate();
    let plaintext = "secret bunker message";
    let ciphertext = signer.nip44_encrypt(&peer.public_key(), plaintext).await?;
    assert_ne!(ciphertext, plaintext);

    let decrypted = peer
        .nip44_decrypt(&signer.public_key(), &ciphertext)
        .await?;
    assert_eq!(decrypted, plaintext);

    bunker.stop().await;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn bunker_local_publish_profile() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let relay = MockRelay::start().await?;

    let (mut bot, bunker_keys) = common::generate_bunker_bot_with_keys("profile-bot", true)?;
    let bunker = MockBunker::new(bunker_keys, vec![relay.url()]).await?;
    let uri = bunker
        .uri_from_relays(&[relay.url()])
        .ok_or("mock bunker produced no URI")?;
    common::set_bunker_uri(&mut bot, &uri);
    bot.relays = vec![relay.url()];

    // Give the bunker time to subscribe before the CLI connects.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let config = common::make_config(&dir, vec![bot])?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &config.to_string_lossy(),
        "publish-profile",
        "profile-bot",
    ]);
    let output = tokio::task::spawn_blocking(move || cmd.assert().success()).await?;
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;
    let event_id = stdout.trim();
    assert_eq!(event_id.len(), 64, "expected a 64-character hex event id");

    // The profile event should have been published to the mock relay.
    let events = relay.events().await;
    assert!(
        events
            .iter()
            .any(|e| e.kind == Kind::Metadata && e.id.to_hex() == event_id),
        "published kind-0 profile event should appear on the mock relay"
    );

    bunker.stop().await;
    relay.stop().await;
    Ok(())
}

/// End-to-end bunker_remote test. Ignored by default because it requires a
/// trusted wss relay/bunker pair that the in-process mock cannot provide.
/// Run with `PACTO_DEV_ENV=1 cargo test -- --ignored`.
#[tokio::test]
#[ignore = "requires PACTO_DEV_ENV=1 with a live wss bunker and relay"]
async fn bunker_remote_publish_profile_and_dm() -> Result<(), Box<dyn Error>> {
    if std::env::var("PACTO_DEV_ENV").unwrap_or_default() != "1" {
        return Ok(());
    }

    // Expected dev-env endpoints. These must match the local pacto-dev-env
    // bunker and relay services.
    let relay_url = "wss://relay.localtest.me:7443".to_string();
    let bunker_uri = std::env::var("PACTO_DEV_ENV_BUNKER_URI")
        .unwrap_or_else(|_| "bunker://PUBKEY?relay=wss://bunker.localtest.me:7443".into());

    let dir = tempfile::tempdir()?;
    let bot = pacto_bot_api::config::BotConfig {
        id: "remote-bot".to_string(),
        npub: std::env::var("PACTO_DEV_ENV_BOT_NPUB").unwrap_or_else(|_| "npub1remote".into()),
        signing: pacto_bot_api::config::SigningConfig::BunkerRemote {
            uri: pacto_bot_api::secrecy::SecretString::new(bunker_uri.into()),
        },
        relays: vec![relay_url],
        capabilities: vec!["ReadMessages".into(), "SendMessages".into()],
        ..Default::default()
    };
    let config = common::make_config(&dir, vec![bot])?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &config.to_string_lossy(),
        "publish-profile",
        "remote-bot",
    ]);
    tokio::task::spawn_blocking(move || cmd.assert().success()).await?;

    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn local_key_send_test_dm_publishes_gift_wrap() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let relay = MockRelay::start().await?;
    let (mut bot, _nsec) = common::generate_nsec_bot("send-bot")?;
    bot.relays = vec![relay.url()];
    bot.capabilities = vec!["ReadMessages".into(), "Admin".into()];
    let config = common::make_config(&dir, vec![bot])?;

    let _daemon = DaemonGuard(common::spawn_daemon_until_ready(&config).await?);

    let recipient_keys = Keys::generate();
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
        .wait_for_event(|e| e.kind == Kind::GiftWrap, Duration::from_secs(5))
        .await?;
    assert!(
        events.iter().any(|e| e.id.to_hex() == event_id),
        "published gift wrap should appear on the mock relay"
    );

    Ok(())
}

/// Spawn a daemon and return a guard that kills it on drop.
struct DaemonGuard(std::process::Child);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
