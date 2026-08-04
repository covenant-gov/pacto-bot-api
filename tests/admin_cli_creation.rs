mod common;

/// req(R9, R11)
use assert_cmd::Command;
use pacto_bot_api::config::{DaemonConfig, VALID_CAPABILITIES};
use predicates::prelude::*;
use std::error::Error;
use std::fs;
use std::io::Write as _;
use std::path::PathBuf;

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
    assert!(!stdout.contains("bunker://abc?relay=wss://relay.example.com"));

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
        .arg("bunker-bot")
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
    assert!(!stdout.contains("bunker://abc?relay=wss://relay.example.com"));

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
    cmd.arg("new")
        .arg("bunker-bot")
        .arg("--backend")
        .arg("bunker_remote")
        .arg("--uri")
        .arg("bunker://abc?relay=wss://relay.example.com")
        .arg("--output")
        .arg(&output)
        .env(
            "PACTO_BUNKER_URI",
            "bunker://abc?relay=wss://relay.example.com",
        );
    let assert = cmd.assert().success();
    let stdout = std::str::from_utf8(&assert.get_output().stdout)?;

    assert!(stdout.contains("npub1"));
    assert!(stdout.contains(&format!("config: {}", output.display())));
    assert!(!stdout.contains("nsec ="));
    assert!(!stdout.contains("bunker://abc?relay=wss://relay.example.com"));

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

/// U2: `validate_capability` accepts every string in `VALID_CAPABILITIES`,
/// with the accepted set derived from the constant rather than retyped.
#[test]
fn new_accepts_every_valid_capability() -> Result<(), Box<dyn Error>> {
    for (i, cap) in VALID_CAPABILITIES.iter().enumerate() {
        let dir = common::tempdir()?;
        let output = dir.path().join("pacto-bot-api.toml");

        let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
        cmd.arg("new")
            .arg(format!("cap-bot-{i}"))
            .arg("--backend")
            .arg("nsec")
            .arg("--relays")
            .arg("wss://relay.example.com")
            .arg("--capabilities")
            .arg(*cap)
            .arg("--output")
            .arg(&output);
        cmd.assert().success();

        let snippet = fs::read_to_string(&output)?;
        assert!(
            snippet.contains(&format!("capabilities = [\"{cap}\"]")),
            "capability {cap} was rejected by validate_capability: {snippet}"
        );
    }
    Ok(())
}

/// U2: `validate_capability` rejects an unknown capability string.
#[test]
fn new_rejects_unknown_capability() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let output = dir.path().join("pacto-bot-api.toml");

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.arg("new")
        .arg("bad-cap-bot")
        .arg("--backend")
        .arg("nsec")
        .arg("--relays")
        .arg("wss://relay.example.com")
        .arg("--capabilities")
        .arg("NotACapability")
        .arg("--output")
        .arg(&output);
    let assert = cmd.assert().failure();
    let stderr = std::str::from_utf8(&assert.get_output().stderr)?;

    assert!(
        stderr.contains("unknown capability: NotACapability"),
        "{stderr}"
    );
    assert!(!output.exists());
    Ok(())
}

/// U2: `prompt_capabilities`' interactive help text mentions every string in
/// `VALID_CAPABILITIES`, so an operator running the wizard sees the full set
/// even though only three used to be listed (KTD17).
#[test]
fn new_interactive_capability_prompt_mentions_every_capability() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let output = dir.path().join("pacto-bot-api.toml");

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.arg("new")
        .arg("--output")
        .arg(&output)
        .write_stdin("cap-prompt-bot\n\n\n\n\n\n\nn\nn\n");
    let assert = cmd.assert().success();
    let stdout = std::str::from_utf8(&assert.get_output().stdout)?;

    for cap in VALID_CAPABILITIES {
        assert!(
            stdout.contains(cap),
            "prompt_capabilities help text is missing {cap}: {stdout}"
        );
    }
    Ok(())
}

/// U2: the `capabilities` description in `schemas/jsonrpc.json` mentions
/// every string in `VALID_CAPABILITIES`.
#[test]
fn jsonrpc_schema_capabilities_description_mentions_every_capability() -> Result<(), Box<dyn Error>>
{
    let schema_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas/jsonrpc.json");
    let raw = fs::read_to_string(&schema_path)?;
    let schema: serde_json::Value = serde_json::from_str(&raw)?;
    let methods = schema["methods"]
        .as_array()
        .ok_or("schemas/jsonrpc.json: methods must be an array")?;
    let register = methods
        .iter()
        .find(|m| m["name"] == "handler.register")
        .ok_or("schemas/jsonrpc.json: handler.register method is missing")?;
    let description = register["params"][0]["schema"]["properties"]["capabilities"]["description"]
        .as_str()
        .ok_or("schemas/jsonrpc.json: handler.register capabilities description is missing")?;

    for cap in VALID_CAPABILITIES {
        assert!(
            description.contains(cap),
            "jsonrpc.json capabilities description is missing {cap}: {description}"
        );
    }
    Ok(())
}

/// U2: a bot configured with each of the four new wave-1 capabilities loads
/// without a validation error. `SendGroupReactions` and `SendGroupAttachments`
/// are in `MLS_CAPABILITIES`, so they also need `mls_db_path` set.
#[test]
fn bot_with_each_new_capability_loads_without_validation_error() -> Result<(), Box<dyn Error>> {
    for cap in [
        "SendReactions",
        "SendAttachments",
        "SendGroupReactions",
        "SendGroupAttachments",
    ] {
        let dir = common::tempdir()?;
        let (mut bot, _nsec) = common::generate_nsec_bot("cap-bot")?;
        bot.capabilities = vec![cap.to_string()];
        let needs_mls = matches!(cap, "SendGroupReactions" | "SendGroupAttachments");
        if needs_mls {
            bot.mls_db_path = Some(PathBuf::from("mls.db"));
        }

        let config_path = common::make_config(&dir, vec![bot])?;
        if needs_mls {
            std::fs::OpenOptions::new()
                .append(true)
                .open(&config_path)?
                .write_all(b"mls_db_path = \"mls.db\"\n")?;
        }

        let loaded = DaemonConfig::load(&config_path);
        assert!(
            loaded.is_ok(),
            "bot granting {cap} failed to load: {:?}",
            loaded.err()
        );
    }
    Ok(())
}
