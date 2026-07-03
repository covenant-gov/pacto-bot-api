mod common;

use assert_cmd::Command;
use predicates::prelude::*;
use std::error::Error;
use std::fs;
use std::path::Path;

/// Create the fixture cache and template repo env used by the resolver.
fn setup_update_env(cmd: &mut Command, temp: &Path) -> Result<(), Box<dyn Error>> {
    let fixture_repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("templates");
    let cache_dir = temp.join("cache");
    fs::create_dir_all(cache_dir.join("contracts"))?;
    fs::create_dir_all(cache_dir.join("sdks"))?;
    fs::write(
        cache_dir
            .join("contracts")
            .join("pacto-contract-0.1.0.json"),
        "{}",
    )?;
    fs::write(
        cache_dir.join("sdks").join("pacto-bot-sdk-0.2.0.json"),
        "{}",
    )?;
    cmd.env("PACTO_TEMPLATE_REPO", &fixture_repo);
    cmd.env("PACTO_CACHE_DIR", &cache_dir);
    Ok(())
}

#[test]
fn update_succeeds_for_scaffolded_project() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let project_dir = temp.path().join("echo-bot");

    let mut new_cmd = Command::cargo_bin("pacto-bot-admin")?;
    new_cmd.args([
        "new",
        "--scaffold",
        "echo-bot",
        "--backend",
        "nsec",
        "--relays",
        "ws://localhost:7000",
        "--commands",
        "echo",
        "--project-dir",
        &project_dir.to_string_lossy(),
    ]);
    setup_update_env(&mut new_cmd, temp.path())?;
    new_cmd.assert().success();

    let lock_path = project_dir
        .join(".pacto")
        .join("bots")
        .join("echo-bot")
        .join("scaffold.lock");
    assert!(lock_path.is_file(), "scaffold lock should exist after new");
    let _original_lock = fs::read_to_string(&lock_path)?;

    let mut update_cmd = Command::cargo_bin("pacto-bot-admin")?;
    update_cmd.args([
        "update",
        "echo-bot",
        "--project-dir",
        &project_dir.to_string_lossy(),
    ]);
    setup_update_env(&mut update_cmd, temp.path())?;
    update_cmd
        .assert()
        .success()
        .stdout(predicate::str::contains("Updated scaffold for echo-bot"));

    let updated_lock = fs::read_to_string(&lock_path)?;
    assert!(
        updated_lock.contains("lock_version"),
        "updated lock should remain valid TOML"
    );
    assert!(
        project_dir
            .join("bots")
            .join("echo-bot")
            .join("echo_bot.py")
            .is_file(),
        "bot handler should still exist after update"
    );

    Ok(())
}

#[test]
fn update_fails_without_lock_file() {
    let temp = tempfile::tempdir().unwrap();
    let project_dir = temp.path().join("missing-lock");
    fs::create_dir_all(project_dir.join(".pacto").join("bots").join("echo-bot")).unwrap();

    let mut cmd = Command::cargo_bin("pacto-bot-admin").unwrap();
    cmd.args([
        "update",
        "echo-bot",
        "--project-dir",
        &project_dir.to_string_lossy(),
    ]);
    setup_update_env(&mut cmd, temp.path()).unwrap();
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("scaffold lock file not found"));
}
