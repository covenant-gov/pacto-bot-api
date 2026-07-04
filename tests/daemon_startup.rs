/// req(R6, R7, R8, R20, R21, R24, R25)
use fs2::FileExt;
use nostr::nips::nip59;
use nostr::{Timestamp, ToBech32};
use pacto_bot_api::db::Database;
use pacto_bot_api::transport::protocol::JsonRpcMessage;
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

mod common;
mod support;

async fn spawn_until_ready(
    config: &Path,
) -> Result<std::process::Child, Box<dyn std::error::Error>> {
    common::spawn_daemon_until_ready(config).await
}

async fn wait_for_exit(
    mut child: std::process::Child,
    timeout_secs: u64,
) -> Result<std::process::ExitStatus, Box<dyn std::error::Error>> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = child.kill();
            return Err("timed out waiting for daemon to exit".into());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
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

    // Give the mock bunker time to subscribe before the daemon connects.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let child = spawn_until_ready(&config).await?;
    common::shutdown_daemon(child).await?;

    bunker.stop().await;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn startup_exits_when_bunker_pubkey_mismatches() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let relay = support::mock_relay::MockRelay::start().await?;

    // Configured npub differs from the live bunker pubkey.
    let (mut bot, bunker_keys) = common::generate_bunker_bot_with_keys("mismatch-bot", false)?;
    let bunker = support::mock_bunker::MockBunker::new(bunker_keys, vec![relay.url()]).await?;
    let uri = bunker
        .uri_from_relays(&[relay.url()])
        .ok_or("mock bunker produced no URI")?;
    common::set_bunker_uri(&mut bot, &uri);
    bot.relays = vec![relay.url()];

    let config = common::make_config(&dir, vec![bot])?;

    // Give the mock bunker time to subscribe before the daemon connects.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let log_path = dir.path().join("mismatch.log");
    let log_file = std::fs::File::create(&log_path)?;
    let mut child = std::process::Command::new(common::daemon_bin_path()?)
        .arg("--config")
        .arg(&config)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(log_file))
        .env("PACTO_BUNKER_TIMEOUT_SECS", "5")
        .spawn()?;

    let status = tokio::task::spawn_blocking(move || child.wait())
        .await?
        .map_err(|e| format!("failed to wait for daemon: {e}"))?;

    let log_tail = {
        let log = std::fs::read_to_string(&log_path)?;
        let start = log.len().saturating_sub(4000);
        log[start..].to_string()
    };

    assert!(
        !status.success(),
        "daemon should exit with error on bunker pubkey mismatch"
    );
    assert!(
        log_tail.contains("configured npub does not match live bunker public key"),
        "log should report bunker pubkey mismatch: {log_tail}"
    );

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

/// NIP-59 allows gift-wrap `created_at` to be tweaked up to 2 days into the
/// past. After a restart, a DM sent "now" may have a gift-wrap timestamp that
/// is slightly older than the persisted cursor. The daemon must still receive
/// it, while not reprocessing older historical events.
#[tokio::test]
async fn startup_receives_dm_with_gift_wrap_timestamp_before_persisted_cursor()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let relay = support::mock_relay::MockRelay::start().await?;
    let (mut bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    bot.relays = vec![relay.url()];
    let config = common::make_config(&dir, vec![bot.clone()])?;
    let socket_path = dir.path().join("pacto-bot-api.sock");
    let db_path = dir.path().join("agent.db");

    // Seed a persisted cursor in the past.
    let cursor_time = Timestamp::now() - 300;
    let db = Database::open(&db_path)?;
    db.save_cursor(&bot.id, &bot.npub, cursor_time.as_u64() as i64)?;
    drop(db);

    let child = spawn_until_ready(&config).await?;
    let mut handler = common::HandlerClient::register(
        &socket_path,
        &["echo-bot"],
        &["dm_received"],
        &["ReadMessages"],
    )
    .await?;

    let sender_keys = nostr::Keys::generate();

    // A historical event older than the maximum NIP-59 tweak window must not
    // be redispatched, since the `since` filter is shifted back by at most
    // that amount to avoid missing freshly sent DMs after a restart. Use a
    // one-minute cushion so the timestamp is strictly outside the shifted window.
    let older = common::build_gift_wrap_with_timestamp(
        &sender_keys,
        &bot.npub,
        "older than cursor",
        cursor_time - nip59::RANGE_RANDOM_TIMESTAMP_TWEAK.end - 60,
    )
    .await?;
    relay.inject_event(older).await;
    assert!(
        handler
            .next_notification(std::time::Duration::from_millis(500))
            .await
            .is_err(),
        "older event should not be dispatched"
    );

    // Simulate a NIP-59 tweak: the gift-wrap timestamp is slightly before the
    // persisted cursor, but the message was sent after restart.
    let tweaked = common::build_gift_wrap_with_timestamp(
        &sender_keys,
        &bot.npub,
        "tweaked before cursor",
        cursor_time - 30,
    )
    .await?;
    relay.inject_event(tweaked).await;
    let notification = handler
        .next_notification(std::time::Duration::from_secs(5))
        .await?;
    match notification {
        JsonRpcMessage::Notification { params, .. } => {
            let content = params
                .as_ref()
                .and_then(|p| p.get("content"))
                .and_then(|v| v.as_str())
                .ok_or("agent.event notification missing content")?;
            assert_eq!(content, "tweaked before cursor");
        }
        _ => panic!("expected agent.event notification, got {notification:?}"),
    }

    // Cursor still advances based on the actual event timestamp, not the
    // tweaked gift-wrap timestamp in this synthetic case.
    common::shutdown_daemon(child).await?;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn startup_uses_persisted_cursor_for_since_filter() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = tempfile::tempdir()?;
    let relay = support::mock_relay::MockRelay::start().await?;
    let (mut bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    bot.relays = vec![relay.url()];
    let config = common::make_config(&dir, vec![bot.clone()])?;
    let socket_path = dir.path().join("pacto-bot-api.sock");
    let db_path = dir.path().join("agent.db");

    // Seed a persisted cursor in the past.
    let cursor_time = Timestamp::now() - 300;
    let db = Database::open(&db_path)?;
    db.save_cursor(&bot.id, &bot.npub, cursor_time.as_u64() as i64)?;
    drop(db);

    let child = spawn_until_ready(&config).await?;
    let mut handler = common::HandlerClient::register(
        &socket_path,
        &["echo-bot"],
        &["dm_received"],
        &["ReadMessages"],
    )
    .await?;

    let sender_keys = nostr::Keys::generate();

    // An event older than the maximum NIP-59 tweak window must not be
    // redispatched, because the `since` filter is shifted back by that
    // amount to avoid missing freshly sent DMs after a restart. Use a
    // one-minute cushion so the timestamp is strictly outside the shifted window.
    let older = common::build_gift_wrap_with_timestamp(
        &sender_keys,
        &bot.npub,
        "older than cursor",
        cursor_time - nip59::RANGE_RANDOM_TIMESTAMP_TWEAK.end - 60,
    )
    .await?;
    relay.inject_event(older).await;
    assert!(
        handler
            .next_notification(std::time::Duration::from_millis(500))
            .await
            .is_err(),
        "older event should not be dispatched"
    );

    // An event within the NIP-59 tweak window of the cursor must still be
    // dispatched, since the `since` filter accounts for the up-to-2-day
    // timestamp tweak.
    let newer = common::build_gift_wrap_with_timestamp(
        &sender_keys,
        &bot.npub,
        "newer than cursor",
        cursor_time + 60,
    )
    .await?;
    relay.inject_event(newer).await;
    let notification = handler
        .next_notification(std::time::Duration::from_secs(5))
        .await?;
    match notification {
        JsonRpcMessage::Notification { params, .. } => {
            let content = params
                .as_ref()
                .and_then(|p| p.get("content"))
                .and_then(|v| v.as_str())
                .ok_or("agent.event notification missing content")?;
            assert_eq!(content, "newer than cursor");
        }
        _ => panic!("expected agent.event notification, got {notification:?}"),
    }

    common::shutdown_daemon(child).await?;
    relay.stop().await;
    Ok(())
}

#[tokio::test]
async fn restart_preserves_handler_registrations() -> Result<(), Box<dyn std::error::Error>> {
    use pacto_bot_api::events::EventType;

    let dir = tempfile::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot.clone()])?;
    let socket_path = dir.path().join("pacto-bot-api.sock");
    let db_path = dir.path().join("agent.db");

    // First daemon run: register a handler.
    let child = spawn_until_ready(&config).await?;
    let handler = common::HandlerClient::register(
        &socket_path,
        &["echo-bot"],
        &["dm_received"],
        &["ReadMessages"],
    )
    .await?;
    let handler_id = handler.handler_id().to_string();
    let reconnect_token = handler.reconnect_token().to_string();
    drop(handler);

    let pid = child.id();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGINT,
    )?;
    let _status = wait_for_exit(child, 30).await?;

    // The registration row must survive the restart.
    let db = Database::open(&db_path)?;
    let loaded = db.load_handlers()?;
    assert_eq!(loaded.len(), 1, "handler row should survive shutdown");
    assert_eq!(loaded[0].id, handler_id);
    assert_eq!(loaded[0].bot_ids, vec!["echo-bot"]);
    assert_eq!(loaded[0].event_types, vec![EventType::DmReceived]);
    assert_eq!(loaded[0].capabilities, vec!["ReadMessages"]);
    assert!(
        !loaded[0].is_connected(),
        "loaded handler should be disconnected"
    );
    drop(db);

    // Second daemon run: reconnect using the secret reconnect token.
    let child = spawn_until_ready(&config).await?;
    let reconnected = common::HandlerClient::reconnect(
        &socket_path,
        &handler_id,
        &reconnect_token,
        &["echo-bot"],
        &["dm_received"],
        &["ReadMessages"],
    )
    .await?;
    assert_eq!(
        reconnected.handler_id(),
        handler_id,
        "reconnect should reuse persisted handler_id"
    );
    drop(reconnected);

    let db = Database::open(&db_path)?;
    let loaded = db.load_handlers()?;
    assert_eq!(
        loaded.len(),
        1,
        "reconnect should not duplicate persisted row"
    );
    drop(db);

    let pid = child.id();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGINT,
    )?;
    let _status = wait_for_exit(child, 30).await?;
    Ok(())
}

#[tokio::test]
async fn unregister_deletes_persisted_handler_row() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot.clone()])?;
    let socket_path = dir.path().join("pacto-bot-api.sock");
    let db_path = dir.path().join("agent.db");

    let child = spawn_until_ready(&config).await?;
    let handler = common::HandlerClient::register(
        &socket_path,
        &["echo-bot"],
        &["dm_received"],
        &["ReadMessages"],
    )
    .await?;
    handler.unregister().await?;
    drop(handler);

    let pid = child.id();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGINT,
    )?;
    let _status = wait_for_exit(child, 30).await?;

    let db = Database::open(&db_path)?;
    let loaded = db.load_handlers()?;
    assert!(loaded.is_empty(), "unregister should delete persisted row");
    Ok(())
}

#[tokio::test]
async fn startup_path_completes_without_blocking() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot])?;

    // The daemon startup path performs config parsing, filesystem setup,
    // lock acquisition and SQLite open/migrations in the same process as the
    // daemon. If any of that ran on the async runtime, the timer ticks below
    // would be starved.
    let mut interval = tokio::time::interval(Duration::from_millis(5));
    let ticks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let ticks_clone = Arc::clone(&ticks);
    let timer = tokio::spawn(async move {
        for _ in 0..50 {
            interval.tick().await;
            ticks_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    });

    let child = tokio::time::timeout(Duration::from_secs(10), spawn_until_ready(&config))
        .await
        .map_err(|_| "daemon startup timed out")??;

    common::shutdown_daemon(child).await?;
    timer.await?;

    let tick_count = ticks.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        tick_count >= 5,
        "runtime was blocked during daemon startup; only {tick_count} timer ticks fired"
    );
    Ok(())
}
