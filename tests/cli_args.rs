use assert_cmd::Command;
use predicates::prelude::*;
use std::io::Write;

#[test]
fn daemon_help_prints_usage() {
    let mut cmd = Command::cargo_bin("pacto-bot-api").unwrap();
    cmd.arg("--help");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("--config"))
        .stdout(predicate::str::contains("--data-dir"))
        .stdout(predicate::str::contains("--enable-http"));
}

#[test]
fn admin_help_prints_usage() {
    let mut cmd = Command::cargo_bin("pacto-bot-admin").unwrap();
    cmd.arg("--help");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("new"))
        .stdout(predicate::str::contains("publish-profile"))
        .stdout(predicate::str::contains("validate-config"));
}

#[test]
fn daemon_config_flag_overrides_default() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(
        br#"
[[bots]]
id = "echo-bot"
npub = "npub1a"
signing = { backend = "nsec", nsec = "nsec1a" }
"#,
    )
    .unwrap();
    let path = file.path().to_path_buf();

    #[cfg(unix)]
    {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&path, perms).unwrap();
    }

    let mut cmd = Command::cargo_bin("pacto-bot-api").unwrap();
    cmd.arg("--config").arg(&path);
    cmd.assert().success();
}

#[test]
fn daemon_invalid_config_path_exits_with_error() {
    let mut cmd = Command::cargo_bin("pacto-bot-api").unwrap();
    cmd.arg("--config")
        .arg("/nonexistent/path/pacto-bot-api.toml");
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("failed to load config"));
}
