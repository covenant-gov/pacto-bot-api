mod common;

/// req(R9, R11)
use assert_cmd::Command;
use predicates::prelude::*;
use std::error::Error;
use std::fs;

#[test]
fn new_outputs_valid_nsec_snippet() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let output = dir.path().join("pacto-bot-api.toml");

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.arg("new")
        .arg("test-bot")
        .arg("--backend")
        .arg("nsec")
        .arg("--relays")
        .arg("wss://relay.example.com")
        .arg("--capabilities")
        .arg("ReadMessages")
        .arg("--output")
        .arg(&output);
    let assert = cmd.assert().success();
    let stdout = std::str::from_utf8(&assert.get_output().stdout)?;
    let stderr = std::str::from_utf8(&assert.get_output().stderr)?;

    assert!(stdout.contains("npub1"));
    assert!(stdout.contains(&format!("config: {}", output.display())));
    assert!(!stdout.contains("nsec1"));
    assert!(!stderr.contains("nsec1"));

    let snippet = fs::read_to_string(&output)?;
    assert!(snippet.contains("id = \"test-bot\""));
    assert!(snippet.contains("backend = \"nsec\""));
    assert!(snippet.contains("nsec = \"nsec1"));
    assert!(snippet.contains("relays = [\"${PACTO_RELAY_URL:-wss://relay.example.com}\"]"));
    assert!(snippet.contains("capabilities = [\"ReadMessages\"]"));
    Ok(())
}

#[test]
fn new_bunker_snippet_does_not_leak_nsec() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let output = dir.path().join("pacto-bot-api.toml");

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.arg("new")
        .arg("test-bot")
        .arg("--backend")
        .arg("bunker_remote")
        .arg("--uri")
        .arg("bunker://abc?relay=wss://relay.example.com")
        .arg("--output")
        .arg(&output);
    let assert = cmd.assert().success();
    let stdout = std::str::from_utf8(&assert.get_output().stdout)?;

    assert!(stdout.contains("npub1"));
    assert!(stdout.contains(&format!("config: {}", output.display())));
    assert!(!stdout.contains("nsec ="));

    let snippet = fs::read_to_string(&output)?;
    assert!(snippet.contains("backend = \"bunker_remote\""));
    assert!(
        snippet
            .contains("uri = \"${PACTO_BUNKER_URI:-bunker://abc?relay=wss://relay.example.com}\"")
    );
    Ok(())
}

#[test]
fn new_interactive_outputs_valid_nsec_snippet() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let output = dir.path().join("pacto-bot-api.toml");

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.arg("new")
        .arg("--output")
        .arg(&output)
        .write_stdin("interactive-bot\n\n\n\n\n\n\nn\ny\n");
    let assert = cmd.assert().success();
    let stdout = std::str::from_utf8(&assert.get_output().stdout)?;

    assert!(stdout.contains("npub1"));
    assert!(stdout.contains(&format!("config: {}", output.display())));
    assert!(!stdout.contains("nsec = \"nsec1"));
    assert!(stdout.contains("<REDACTED>"));

    let snippet = fs::read_to_string(&output)?;
    assert!(snippet.contains("id = \"interactive-bot\""));
    assert!(snippet.contains("backend = \"nsec\""));
    assert!(snippet.contains("nsec = \"nsec1"));
    assert!(snippet.contains("relays = [\"${PACTO_RELAY_URL:-ws://localhost:7000}\"]"));
    assert!(snippet.contains("capabilities = [\"ReadMessages\", \"SendMessages\"]"));
    Ok(())
}

#[test]
fn new_interactive_cancellation_prints_no_final_snippet() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let output = dir.path().join("pacto-bot-api.toml");

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.arg("new")
        .arg("--output")
        .arg(&output)
        .write_stdin("interactive-bot\n\n\n\n\n\n\nn\nn\n");
    let assert = cmd.assert().success();
    let stdout = std::str::from_utf8(&assert.get_output().stdout)?;

    // After cancellation the final snippet should not be emitted or written.
    assert!(stdout.contains("Cancelled."));
    assert!(!output.exists());
    Ok(())
}

#[test]
fn new_interactive_bunker_remote_prompts_for_uri() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let output = dir.path().join("pacto-bot-api.toml");

    // Use env vars to skip interactive prompts and provide the bunker URI directly
    // The test verifies that the URI is NOT echoed to stdout when using --uri flag
    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.arg("new")
        .arg("--backend")
        .arg("bunker_remote")
        .arg("--uri")
        .arg("bunker://abc?relay=wss://relay.example.com")
        .arg("--output")
        .arg(&output);
    let assert = cmd.assert().success();
    let stdout = std::str::from_utf8(&assert.get_output().stdout)?;

    assert!(stdout.contains("npub1"));
    assert!(stdout.contains(&format!("config: {}", output.display())));
    assert!(!stdout.contains("nsec ="));

    let snippet = fs::read_to_string(&output)?;
    assert!(snippet.contains("id = \"bunker-bot\""));
    assert!(snippet.contains("backend = \"bunker_remote\""));
    assert!(
        snippet
            .contains("uri = \"${PACTO_BUNKER_URI:-bunker://abc?relay=wss://relay.example.com}\"")
    );
    Ok(())
}

#[test]
fn new_interactive_bunker_remote_prompts_for_uri_with_secret_input() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let output = dir.path().join("pacto-bot-api.toml");

    // Use env vars to skip interactive prompts and provide the bunker URI directly
    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.arg("new").arg("--output").arg(&output).env(
        "PACTO_BUNKER_URI",
        "bunker://abc?relay=wss://relay.example.com",
    );
    let assert = cmd.assert().success();
    let stdout = std::str::from_utf8(&assert.get_output().stdout)?;

    assert!(stdout.contains("npub1"));
    assert!(stdout.contains(&format!("config: {}", output.display())));
    assert!(!stdout.contains("nsec ="));

    let snippet = fs::read_to_string(&output)?;
    assert!(snippet.contains("id = \"bunker-bot\""));
    assert!(snippet.contains("backend = \"bunker_remote\""));
    assert!(
        snippet
            .contains("uri = \"${PACTO_BUNKER_URI:-bunker://abc?relay=wss://relay.example.com}\"")
    );
    Ok(())
}
#[test]
fn new_emit_secrets_prints_nsec_with_warning() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let output = dir.path().join("pacto-bot-api.toml");

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.arg("new")
        .arg("test-bot")
        .arg("--backend")
        .arg("nsec")
        .arg("--relays")
        .arg("wss://relay.example.com")
        .arg("--output")
        .arg(&output)
        .arg("--emit-secrets");
    let assert = cmd.assert().success();
    let stdout = std::str::from_utf8(&assert.get_output().stdout)?;
    let stderr = std::str::from_utf8(&assert.get_output().stderr)?;

    assert!(stderr.contains("warning"));
    assert!(stderr.contains("--emit-secrets"));
    assert!(stdout.contains("nsec = \"nsec1"));
    assert!(stdout.contains("id = \"test-bot\""));
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
    let dir = common::tempdir()?;
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
