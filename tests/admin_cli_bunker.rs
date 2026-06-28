mod common;
mod support;

use std::error::Error;
use std::time::Duration;

use assert_cmd::Command;

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
