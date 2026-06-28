/// req(R7, R21, R24)
use assert_cmd::Command;
use predicates::prelude::*;

mod common;

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

#[tokio::test]
async fn daemon_config_flag_overrides_default() {
    let dir = tempfile::tempdir().unwrap();
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot").unwrap();
    let path = common::make_config(&dir, vec![bot]).unwrap();

    let child = common::spawn_daemon_until_ready(&path)
        .await
        .expect("daemon should become ready");

    common::shutdown_daemon(child).await.unwrap();
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
