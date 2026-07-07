mod common;

use assert_cmd::Command;
use predicates::prelude::*;
use std::error::Error;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

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

/// Scaffold a fresh `echo-bot` project in an isolated temp directory.
fn scaffold_echo_bot(temp: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let project_dir = temp.join("echo-bot");
    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "new",
        "--scaffold",
        "echo-bot",
        "--backend",
        "nsec",
        "--relays",
        "ws://localhost:7000",
        "--commands",
        "echo",
        "--http",
        "--no-tests",
        "--project-dir",
        &project_dir.to_string_lossy(),
    ]);
    setup_update_env(&mut cmd, temp)?;
    cmd.assert().success();
    Ok(project_dir)
}

/// Set the modification time of `path` to `days` days in the past.
fn set_mtime_in_past(path: &Path, days: u64) -> Result<(), Box<dyn Error>> {
    let file = fs::OpenOptions::new().write(true).open(path)?;
    let mtime = SystemTime::now() - Duration::from_secs(days * 24 * 60 * 60);
    let times = fs::FileTimes::new().set_modified(mtime);
    file.set_times(times)?;
    Ok(())
}

#[test]
fn update_succeeds_for_scaffolded_project() -> Result<(), Box<dyn Error>> {
    let temp = common::tempdir()?;
    let project_dir = scaffold_echo_bot(temp.path())?;

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
        "--force",
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
        updated_lock.contains("commands = [\"echo\"]"),
        "updated lock should preserve original commands"
    );
    assert!(
        updated_lock.contains("http = true"),
        "updated lock should preserve original http flag"
    );
    assert!(
        updated_lock.contains("with_tests = false"),
        "updated lock should preserve original no-tests flag"
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
fn update_without_force_preserves_protected_files() -> Result<(), Box<dyn Error>> {
    let temp = common::tempdir()?;
    let project_dir = scaffold_echo_bot(temp.path())?;

    let bot_dir = project_dir.join("bots").join("echo-bot");
    let handler_path = bot_dir.join("echo_bot.py");
    let tests_path = bot_dir.join("tests").join("test_handlers.py");

    fs::write(&handler_path, "# user-protected handler edit\n")?;
    fs::create_dir_all(tests_path.parent().expect("tests path has parent"))?;
    fs::write(&tests_path, "# user-protected test edit\n")?;

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

    assert_eq!(
        fs::read_to_string(&handler_path)?,
        "# user-protected handler edit\n",
        "protected bot handler must be preserved"
    );
    assert_eq!(
        fs::read_to_string(&tests_path)?,
        "# user-protected test edit\n",
        "protected test file must be preserved"
    );

    let lock_path = project_dir
        .join(".pacto")
        .join("bots")
        .join("echo-bot")
        .join("scaffold.lock");
    let lock_text = fs::read_to_string(&lock_path)?;
    assert!(
        lock_text.contains("lock_version"),
        "lock should remain valid after update"
    );
    assert!(
        lock_text.contains("commands = [\"echo\"]"),
        "lock should preserve original commands"
    );

    Ok(())
}

#[test]
fn update_with_refresh_refetches_artifacts() -> Result<(), Box<dyn Error>> {
    let temp = common::tempdir()?;
    let project_dir = scaffold_echo_bot(temp.path())?;

    let mut update_cmd = Command::cargo_bin("pacto-bot-admin")?;
    update_cmd.args([
        "update",
        "echo-bot",
        "--project-dir",
        &project_dir.to_string_lossy(),
        "--refresh",
    ]);
    setup_update_env(&mut update_cmd, temp.path())?;
    update_cmd
        .assert()
        .success()
        .stdout(predicate::str::contains("Updated scaffold for echo-bot"));

    let lock_path = project_dir
        .join(".pacto")
        .join("bots")
        .join("echo-bot")
        .join("scaffold.lock");
    let lock_text = fs::read_to_string(&lock_path)?;
    assert!(
        lock_text.contains("lock_version"),
        "lock should remain valid after refresh"
    );
    assert!(
        lock_text.contains("commands = [\"echo\"]"),
        "lock should preserve original commands"
    );

    Ok(())
}

#[test]
fn update_with_prune_cache_removes_stale_entries() -> Result<(), Box<dyn Error>> {
    let temp = common::tempdir()?;
    let project_dir = scaffold_echo_bot(temp.path())?;

    let stale_dir = temp.path().join("cache").join("contracts");
    fs::create_dir_all(&stale_dir)?;
    let stale_file = stale_dir.join("stale-contract-0.0.1.json");
    fs::write(&stale_file, "{}")?;
    set_mtime_in_past(&stale_file, 100)?;

    let mut update_cmd = Command::cargo_bin("pacto-bot-admin")?;
    update_cmd.args([
        "update",
        "echo-bot",
        "--project-dir",
        &project_dir.to_string_lossy(),
        "--prune-cache",
    ]);
    setup_update_env(&mut update_cmd, temp.path())?;
    update_cmd
        .assert()
        .success()
        .stdout(predicate::str::contains("Pruned 1 stale cache entries"))
        .stdout(predicate::str::contains("Updated scaffold for echo-bot"));

    assert!(
        !stale_file.exists(),
        "stale cache entry should be removed by prune-cache"
    );

    Ok(())
}

#[test]
fn update_fails_without_lock_file() {
    let temp = common::tempdir().unwrap();
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
