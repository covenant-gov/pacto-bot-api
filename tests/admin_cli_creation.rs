mod common;

/// req(R9, R11)
use assert_cmd::Command;
use std::error::Error;

#[test]
fn new_outputs_valid_nsec_snippet() -> Result<(), Box<dyn Error>> {
    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "new",
        "test-bot",
        "--backend",
        "nsec",
        "--relays",
        "wss://relay.example.com",
        "--capabilities",
        "ReadMessages",
    ]);
    let output = cmd.assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;
    let stderr = std::str::from_utf8(&output.get_output().stderr)?;

    assert!(stdout.contains("id = \"test-bot\""));
    assert!(stdout.contains("backend = \"nsec\""));
    assert!(stdout.contains("nsec = \"nsec1"));
    assert!(stdout.contains("relays = [\"wss://relay.example.com\"]"));
    assert!(stdout.contains("capabilities = [\"ReadMessages\"]"));
    assert!(!stderr.contains("nsec1"));
    Ok(())
}

#[test]
fn new_bunker_snippet_does_not_leak_nsec() -> Result<(), Box<dyn Error>> {
    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "new",
        "test-bot",
        "--backend",
        "bunker_remote",
        "--uri",
        "bunker://abc?relay=wss://relay.example.com",
    ]);
    let output = cmd.assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;

    assert!(stdout.contains("backend = \"bunker_remote\""));
    assert!(stdout.contains("uri = \"bunker://abc?relay=wss://relay.example.com\""));
    assert!(!stdout.contains("nsec ="));
    Ok(())
}

#[test]
fn publish_profile_builds_kind0_event() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot])?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &config.to_string_lossy(),
        "publish-profile",
        "echo-bot",
    ]);
    let output = cmd.assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;
    let event_id = stdout.trim();

    assert_eq!(event_id.len(), 64);
    assert!(event_id.chars().all(|c| c.is_ascii_hexdigit()));
    Ok(())
}
