use assert_cmd::Command;
use std::error::Error;
use std::path::Path;

#[test]
fn llms_txt_matches_cli_output() -> Result<(), Box<dyn Error>> {
    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.arg("--llm-help");
    let output = cmd.assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;

    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let guide_path = Path::new(manifest_dir)
        .join("docs")
        .join("pacto-bot-admin-llms.txt");
    let committed = std::fs::read_to_string(&guide_path)
        .map_err(|e| format!("failed to read {}: {e}", guide_path.display()))?;

    assert_eq!(
        stdout, committed,
        "docs/pacto-bot-admin-llms.txt is out of sync with pacto-bot-admin --llm-help; run cargo xtask docs"
    );
    Ok(())
}
