mod common;

/// req(R9, R11)
use assert_cmd::Command;
use predicates::prelude::*;
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
fn new_interactive_outputs_valid_nsec_snippet() -> Result<(), Box<dyn Error>> {
    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.arg("new");
    cmd.write_stdin("interactive-bot\n\n\n\n\n\n\nn\ny\n");
    let output = cmd.assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;

    assert!(stdout.contains("id = \"interactive-bot\""));
    assert!(stdout.contains("backend = \"nsec\""));
    assert!(stdout.contains("nsec = \"nsec1"));
    assert!(stdout.contains("relays = [\"ws://localhost:7000\"]"));
    assert!(stdout.contains("capabilities = [\"ReadMessages\", \"SendMessages\"]"));
    Ok(())
}

#[test]
fn new_interactive_cancellation_prints_no_final_snippet() -> Result<(), Box<dyn Error>> {
    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.arg("new");
    cmd.write_stdin("interactive-bot\n\n\n\n\n\n\nn\nn\n");
    let output = cmd.assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;

    // After cancellation the final snippet should not be emitted.
    assert!(stdout.contains("Cancelled."));
    Ok(())
}

#[test]
fn new_interactive_bunker_remote_prompts_for_uri() -> Result<(), Box<dyn Error>> {
    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.arg("new");
    // backend=3 (bunker_remote), provide URI, then defaults, confirm.
    cmd.write_stdin(
        "bunker-bot\n3\nbunker://abc?relay=wss://relay.example.com\n\n\n\n\n\n\nn\ny\n",
    );
    let output = cmd.assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;

    assert!(stdout.contains("id = \"bunker-bot\""));
    assert!(stdout.contains("backend = \"bunker_remote\""));
    assert!(stdout.contains("uri = \"bunker://abc?relay=wss://relay.example.com\""));
    // nsec should not appear in the final snippet for bunker backends.
    assert!(!stdout.contains("nsec ="));
    Ok(())
}

#[test]
fn new_help_mentions_interactive_wizard() {
    let mut cmd = Command::cargo_bin("pacto-bot-admin").unwrap();
    cmd.arg("new").arg("--help");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("interactive wizard"))
        .stdout(predicate::str::contains("pacto-bot-admin new"));
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
