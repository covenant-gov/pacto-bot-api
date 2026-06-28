mod common;

use assert_cmd::Command;
use std::error::Error;

#[test]
fn test_bunker_match() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let bot = common::generate_bunker_bot("echo-bot", true)?;
    let config = common::make_config(&dir, vec![bot])?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &config.to_string_lossy(),
        "test-bunker",
        "echo-bot",
    ]);
    let output = cmd.assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;
    assert!(stdout.contains("bunker public key matches npub"));
    Ok(())
}

#[test]
fn test_bunker_mismatch() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let bot = common::generate_bunker_bot("echo-bot", false)?;
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

#[test]
fn test_bunker_unreachable_or_invalid() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let bot = pacto_bot_api::config::BotConfig {
        id: "echo-bot".to_string(),
        npub: "npub1invalid".to_string(),
        signing: pacto_bot_api::config::SigningConfig::BunkerLocal {
            uri: "not-a-bunker-uri".to_string(),
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
