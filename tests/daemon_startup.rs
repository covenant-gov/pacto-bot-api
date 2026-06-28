use fs2::FileExt;
use nostr::ToBech32;
use pacto_bot_api::db::Database;
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

mod common;
mod support;

async fn spawn_until_ready(
    config: &Path,
) -> Result<std::process::Child, Box<dyn std::error::Error>> {
    common::spawn_daemon_until_ready(config).await
}

#[tokio::test]
async fn startup_succeeds_with_valid_config_and_acquires_lock()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot])?;

    let child = spawn_until_ready(&config).await?;

    let lock_path = dir.path().join("daemon.lock");
    assert!(lock_path.exists(), "lock file should be created");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&lock_path)?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "lock file should be owner-only");
    }

    common::shutdown_daemon(child).await?;
    Ok(())
}

#[tokio::test]
async fn startup_succeeds_with_bunker_local_backend() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let relay = support::mock_relay::MockRelay::start().await?;

    let (mut bot, bunker_keys) = common::generate_bunker_bot_with_keys("bunker-bot", true)?;
    let bunker = support::mock_bunker::MockBunker::new(bunker_keys, vec![relay.url()]).await?;
    let uri = bunker
        .uri_from_relays(&[relay.url()])
        .ok_or("mock bunker produced no URI")?;
    common::set_bunker_uri(&mut bot, &uri);
    bot.relays = vec![relay.url()];

    let config = common::make_config(&dir, vec![bot])?;

    let child = spawn_until_ready(&config).await?;
    common::shutdown_daemon(child).await?;

    bunker.stop().await;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn startup_exits_when_lock_already_held() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot])?;

    let lock_path = dir.path().join("daemon.lock");
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&lock_path)?;
    lock_file
        .try_lock_exclusive()
        .expect("test should acquire lock");

    let output = std::process::Command::new(common::daemon_bin_path()?)
        .arg("--config")
        .arg(&config)
        .output()?;

    assert!(
        !output.status.success(),
        "daemon should exit with error when lock is held"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("already running") || stderr.contains("lock held"),
        "stderr should mention the held lock: {stderr}"
    );
    Ok(())
}

#[tokio::test]
async fn startup_exits_with_error_on_invalid_config() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let config = dir.path().join("pacto-bot-api.toml");
    std::fs::write(&config, "not valid toml [[")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&config)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&config, perms)?;
    }

    let output = std::process::Command::new(common::daemon_bin_path()?)
        .arg("--config")
        .arg(&config)
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("failed to load config"),
        "stderr should report config failure: {stderr}"
    );
    Ok(())
}

#[tokio::test]
async fn startup_exits_with_error_on_loose_config_permissions()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let config = dir.path().join("pacto-bot-api.toml");
    common::write_loose_config(
        &config,
        r#"
[[bots]]
id = "echo-bot"
npub = "npub1a"
signing = { backend = "nsec", nsec = "nsec1a" }
"#,
    )?;

    let output = std::process::Command::new(common::daemon_bin_path()?)
        .arg("--config")
        .arg(&config)
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("readable only by owner"),
        "stderr should report permission error: {stderr}"
    );
    Ok(())
}

#[tokio::test]
async fn startup_resets_cursor_when_stored_npub_mismatches_config()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot.clone()])?;

    // Pre-populate the database with a cursor tied to a different npub.
    let other_keys = nostr::Keys::generate();
    let other_npub = other_keys.public_key().to_bech32()?;
    let db_path = dir.path().join("agent.db");
    let db = Database::open(&db_path)?;
    db.save_cursor(&bot.id, &other_npub, 123)?;
    drop(db);

    let child = spawn_until_ready(&config).await?;

    common::shutdown_daemon(child).await?;

    let db = Database::open(&db_path)?;
    let cursor = db.load_cursor(&bot.id)?;
    assert!(
        cursor.is_none(),
        "cursor should be reset after npub mismatch"
    );
    Ok(())
}
