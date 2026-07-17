use assert_cmd::Command;
use predicates::str::contains;
use std::error::Error;

#[test]
fn top_level_help_includes_examples_and_llm_help_pointer() -> Result<(), Box<dyn Error>> {
    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.arg("--help");
    let output = cmd.assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;

    assert!(stdout.contains("Examples:"));
    assert!(stdout.contains("pacto-bot-admin new echo-bot --backend nsec"));
    assert!(stdout.contains("pacto-bot-admin --llm-help"));
    Ok(())
}

#[test]
fn new_help_lists_backends_and_capabilities() -> Result<(), Box<dyn Error>> {
    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args(["new", "--help"]);
    let output = cmd.assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;

    assert!(stdout.contains("nsec (dev-only)"));
    assert!(stdout.contains("bunker_local"));
    assert!(stdout.contains("bunker_remote"));
    assert!(stdout.contains("ReadMessages"));
    assert!(stdout.contains("SendMessages"));
    assert!(stdout.contains("ManageProfile"));
    assert!(stdout.contains("ExitMlsGroup"));
    assert!(stdout.contains("Examples:"));
    Ok(())
}

#[test]
fn diagnose_help_lists_format_values() -> Result<(), Box<dyn Error>> {
    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args(["diagnose", "--help"]);
    let output = cmd.assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;

    assert!(stdout.contains("text"));
    assert!(stdout.contains("json"));
    assert!(stdout.contains("Examples:"));
    Ok(())
}

#[test]
fn status_help_lists_format_values() -> Result<(), Box<dyn Error>> {
    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args(["status", "--help"]);
    let output = cmd.assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;

    assert!(stdout.contains("text"));
    assert!(stdout.contains("json"));
    assert!(stdout.contains("Examples:"));
    Ok(())
}

#[test]
fn help_output_contains_no_real_secrets() -> Result<(), Box<dyn Error>> {
    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.arg("--help");
    let output = cmd.assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;

    // Real nsec bech32 strings start with nsec1 followed by alphanumeric chars.
    assert!(!stdout.contains("nsec1"));
    // No literal bunker URI scheme tokens that could be real secrets.
    assert!(!stdout.contains("bunker://"));
    Ok(())
}

#[test]
fn every_subcommand_help_includes_examples() -> Result<(), Box<dyn Error>> {
    let subcommands = [
        "new",
        "publish-profile",
        "test-bunker",
        "export",
        "import",
        "validate-config",
        "rotate-http-token",
        "diagnose",
        "status",
    ];

    for sub in subcommands {
        let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
        cmd.args([sub, "--help"]);
        cmd.assert().success().stdout(contains("Examples:"));
    }
    Ok(())
}

#[test]
fn llm_help_prints_operator_guide() -> Result<(), Box<dyn Error>> {
    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.arg("--llm-help");
    let output = cmd.assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;

    assert!(stdout.contains("# Pacto Bot Operator's Guide"));
    assert!(stdout.contains("## When to use which"));
    Ok(())
}

#[test]
fn docs_format_llm_prints_operator_guide() -> Result<(), Box<dyn Error>> {
    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args(["docs", "--format", "llm"]);
    let output = cmd.assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;

    assert!(stdout.contains("# Pacto Bot Operator's Guide"));
    Ok(())
}

#[test]
fn docs_format_unknown_exits_with_error() -> Result<(), Box<dyn Error>> {
    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args(["docs", "--format", "not-a-format"]);
    cmd.assert()
        .failure()
        .stderr(contains("unsupported docs format"));
    Ok(())
}
