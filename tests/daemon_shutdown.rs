mod common;

use pacto_bot_api::db::Database;
use std::error::Error;
use std::process::Child;
use std::time::Duration;

async fn wait_for_child(
    mut child: Child,
    timeout_secs: u64,
) -> Result<std::process::ExitStatus, Box<dyn Error>> {
    let fut = async move {
        tokio::task::spawn_blocking(move || child.wait())
            .await
            .map_err(|e| format!("child wait panicked: {e}"))?
            .map_err(|e| format!("child wait failed: {e}"))
    };
    tokio::time::timeout(Duration::from_secs(timeout_secs), fut)
        .await
        .map_err(|_| Box::<dyn Error>::from("timed out waiting for daemon to exit"))?
        .map_err(|e| -> Box<dyn Error> { e.into() })
}

#[tokio::test]
async fn sigterm_triggers_clean_shutdown_and_writes_report() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot])?;

    let child = common::spawn_daemon_until_ready(&config).await?;

    let pid = child.id();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    )?;

    let status = wait_for_child(child, 10).await?;
    assert!(status.success(), "daemon should exit cleanly on SIGTERM");

    let latest_path = dir.path().join("reports").join("latest.json");
    assert!(
        latest_path.exists(),
        "latest.json should be written on shutdown"
    );

    let report = std::fs::read_to_string(&latest_path)?;
    assert!(
        report.contains("\"status\""),
        "report should contain a status field"
    );

    Ok(())
}

#[tokio::test]
async fn second_sigterm_forces_immediate_exit() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot])?;

    let child = common::spawn_daemon_until_ready(&config).await?;

    let pid = child.id();
    let nix_pid = nix::unistd::Pid::from_raw(pid as i32);
    nix::sys::signal::kill(nix_pid, nix::sys::signal::Signal::SIGTERM)?;

    // Keep sending SIGTERM until the child exits. The first signal starts
    // graceful shutdown; a subsequent signal processed during shutdown forces
    // an immediate exit.
    let signal_task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(1)).await;
            let _ = nix::sys::signal::kill(nix_pid, nix::sys::signal::Signal::SIGTERM);
        }
    });

    let status = wait_for_child(child, 10).await?;
    signal_task.abort();
    assert!(
        !status.success(),
        "second SIGTERM should force a non-zero exit"
    );

    let code = status.code().unwrap_or(0);
    assert!(
        code == 1 || code == 130,
        "force exit code should be 1 or 130, got {code}"
    );

    Ok(())
}

#[tokio::test]
async fn shutdown_preserves_cursor_after_prior_dispatch() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot.clone()])?;

    // Pre-populate the database with a cursor, simulating a previously
    // dispatched event, so the shutdown path can be verified not to lose it.
    common::populate_db(&dir, "echo-bot", &bot.npub, 12_345, vec![])?;

    let child = common::spawn_daemon_until_ready(&config).await?;

    let pid = child.id();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    )?;

    let _status = wait_for_child(child, 10).await?;

    let db_path = dir.path().join("agent.db");
    let db = Database::open(&db_path)?;
    let cursor = db.load_cursor("echo-bot")?;
    assert_eq!(
        cursor,
        Some((bot.npub, 12_345)),
        "cursor should be preserved after shutdown"
    );

    Ok(())
}
