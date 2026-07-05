use clap::Parser;
use fs2::FileExt;
use pacto_bot_api::client_manager::ClientManager;
use pacto_bot_api::config::DaemonConfig;
use pacto_bot_api::db::Db;
use pacto_bot_api::dev_env_probe::{log_warnings, run_probe};
use pacto_bot_api::diagnostics::{DaemonStatus, Diagnostics};
use pacto_bot_api::dispatch::Dispatch;
use pacto_bot_api::nostr::NostrClient;
use pacto_bot_api::signer::Signer;
use pacto_bot_api::transport::TransportLayer;
use pacto_bot_api::transport::http;
use std::collections::HashSet;
use std::env;
use std::fs::{File, OpenOptions, Permissions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::time::Duration;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{RwLock, oneshot, watch};
use tokio_stream::StreamExt;
use tracing::{info, warn};

const DAEMON_LOCK_FILE: &str = "daemon.lock";
const AGENT_DB_FILE: &str = "agent.db";
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Parser, Debug)]
#[command(name = "pacto-bot-api")]
#[command(about = "Pacto bot API daemon")]
#[command(version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_COMMIT_SHORT"), ")"))]
struct Cli {
    /// Path to the bot configuration file.
    #[arg(short, long, value_name = "PATH", default_value = "pacto-bot-api.toml")]
    config: PathBuf,

    /// Directory for runtime data (database, socket, reports).
    #[arg(short, long, value_name = "DIR")]
    data_dir: Option<PathBuf>,

    /// Enable the optional localhost HTTP transport.
    #[arg(long)]
    enable_http: bool,

    /// Logging level filter for the daemon.
    #[arg(short, long, value_name = "LEVEL")]
    log_level: Option<String>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let env_filter = if let Some(level) = &cli.log_level {
        tracing_subscriber::EnvFilter::new(level)
    } else if let Some(rust_log) =
        env::var_os("RUST_LOG").and_then(|s| s.into_string().ok().filter(|v| !v.is_empty()))
    {
        tracing_subscriber::EnvFilter::new(rust_log)
    } else {
        tracing_subscriber::EnvFilter::new("info")
    };

    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    if let Err(e) = run_daemon(cli).await {
        eprintln!("{e}");
        process::exit(1);
    }
}

/// Coordinates graceful shutdown on the first signal and forced exit on the
/// second signal.
struct ShutdownCoordinator {
    shutdown_rx: oneshot::Receiver<()>,
    force_rx: oneshot::Receiver<()>,
}

impl ShutdownCoordinator {
    /// Start listening for SIGINT/SIGTERM (Unix) or Ctrl-C (non-Unix).
    #[cfg(unix)]
    fn start() -> Result<Self, String> {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (force_tx, force_rx) = oneshot::channel();

        let mut sigint = signal(SignalKind::interrupt())
            .map_err(|e| format!("failed to install SIGINT handler: {e}"))?;
        let mut sigterm = signal(SignalKind::terminate())
            .map_err(|e| format!("failed to install SIGTERM handler: {e}"))?;

        tokio::spawn(async move {
            tokio::select! {
                _ = sigint.recv() => {},
                _ = sigterm.recv() => {},
            }
            let _ = shutdown_tx.send(());

            tokio::select! {
                _ = sigint.recv() => {},
                _ = sigterm.recv() => {},
            }
            let _ = force_tx.send(());
        });

        Ok(Self {
            shutdown_rx,
            force_rx,
        })
    }

    #[cfg(not(unix))]
    fn start() -> Result<Self, String> {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (force_tx, force_rx) = oneshot::channel();

        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c()
                .map_err(|e| format!("failed to install ctrl-c handler: {e}"))
                .await;
            let _ = shutdown_tx.send(());
            let _ = tokio::signal::ctrl_c()
                .map_err(|e| format!("failed to install ctrl-c handler: {e}"))
                .await;
            let _ = force_tx.send(());
        });

        Ok(Self {
            shutdown_rx,
            force_rx,
        })
    }

    #[cfg(test)]
    fn start_with(
        shutdown_signal: impl Future<Output = ()> + Send + 'static,
        force_signal: impl Future<Output = ()> + Send + 'static,
    ) -> Self {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (force_tx, force_rx) = oneshot::channel();

        tokio::spawn(async move {
            let _ = shutdown_signal.await;
            let _ = shutdown_tx.send(());
        });

        tokio::spawn(async move {
            let _ = force_signal.await;
            let _ = force_tx.send(());
        });

        Self {
            shutdown_rx,
            force_rx,
        }
    }
}

/// Resources produced by the daemon startup sequence.
struct StartupContext {
    config: DaemonConfig,
    data_dir: String,
    #[allow(dead_code)]
    lock_file: File,
    db: Db,
}

impl std::fmt::Debug for StartupContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StartupContext")
            .field("config", &self.config)
            .field("data_dir", &self.data_dir)
            .field("lock_file", &self.lock_file)
            .finish_non_exhaustive()
    }
}

/// Run the startup sequence up to (but not including) the long-lived network
/// and transport layers.
async fn daemon_startup(cli: &Cli) -> Result<StartupContext, String> {
    info!(
        config = %cli.config.display(),
        enable_http = cli.enable_http,
        "starting pacto-bot-api daemon"
    );

    let config = DaemonConfig::load_async(&cli.config)
        .await
        .map_err(|e| format!("failed to load config: {e}"))?;

    let data_dir = cli
        .data_dir
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| config.data_dir().to_string());

    tokio::fs::create_dir_all(&data_dir)
        .await
        .map_err(|e| format!("failed to create data directory {}: {e}", data_dir))?;

    // Restrict the data directory to the owner. The Unix socket and secret
    // token live (or may live) under this path.
    #[cfg(unix)]
    {
        let data_dir_path = Path::new(&data_dir);
        let metadata = tokio::fs::metadata(data_dir_path)
            .await
            .map_err(|e| format!("failed to stat data directory {}: {e}", data_dir))?;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            tokio::fs::set_permissions(data_dir_path, Permissions::from_mode(0o700))
                .await
                .map_err(|e| {
                    format!("failed to set data directory permissions {}: {e}", data_dir)
                })?;
        }
    }

    let lock_path = Path::new(&data_dir).join(DAEMON_LOCK_FILE);
    let lock_file = acquire_lock_file(&lock_path)
        .await
        .map_err(|e| format!("failed to acquire lock file {}: {e}", lock_path.display()))?;

    let db_path = Path::new(&data_dir).join(AGENT_DB_FILE);
    let db = Db::open(&db_path)
        .await
        .map_err(|e| format!("failed to open database {}: {e}", db_path.display()))?;

    for bot in &config.bots {
        let valid = db
            .validate_npub(&bot.id, &bot.npub)
            .await
            .map_err(|e| format!("failed to validate stored npub for {}: {e}", bot.id))?;
        if !valid {
            warn!(
                bot_id = %bot.id,
                "stored npub does not match config; resetting cursor"
            );
            db.reset_cursor(&bot.id)
                .await
                .map_err(|e| format!("failed to reset cursor for {}: {e}", bot.id))?;
        }
    }

    Ok(StartupContext {
        config,
        data_dir,
        lock_file,
        db,
    })
}

async fn run_daemon(cli: Cli) -> Result<(), String> {
    let startup = daemon_startup(&cli).await?;
    let config = startup.config;
    let data_dir = startup.data_dir;
    let db = startup.db;
    let lock_file = startup.lock_file;
    let lock_path = Path::new(&data_dir).join(DAEMON_LOCK_FILE);

    // Best-effort dev-env service-version probe; mismatches are logged as
    // warnings and never block daemon startup.
    tokio::spawn(async move {
        let results = run_probe().await;
        log_warnings(&results);
    });

    if cli.enable_http {
        warn!("localhost HTTP transport is enabled; ensure the secret token is protected");
    }

    if config
        .bots
        .iter()
        .any(|b| matches!(b.signing, pacto_bot_api::config::SigningConfig::Nsec { .. }))
    {
        warn!("local test key (nsec) in use — not for production");
    }

    let unique_relays: Vec<String> = config
        .bots
        .iter()
        .flat_map(|b| b.relays.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    let diagnostics = Diagnostics::new();
    diagnostics.set_status(DaemonStatus::Initializing);

    let nostr_client = NostrClient::new(unique_relays)
        .await
        .map_err(|e| format!("failed to initialize nostr client: {e}"))?
        .with_diagnostics(diagnostics.clone());

    let mut client_manager =
        ClientManager::new(Path::new(&data_dir), config.clone(), nostr_client.clone())
            .await
            .map_err(|e| format!("failed to initialize client manager: {e}"))?;

    client_manager.update_diagnostics(&diagnostics);

    // Register every bot signer so gift wraps addressed to its pubkey can be
    // decrypted. The NostrClient clone held by ClientManager shares the same
    // signer map, so registrations apply to both handles.
    for (pubkey, bot) in client_manager.bots() {
        let signer: Arc<dyn Signer> = Arc::new(bot.signer.clone());
        nostr_client
            .add_signer(*pubkey, bot.bot_id().to_string(), signer)
            .await;
    }

    // Subscribe each bot to its gift-wrap filter, using the persisted cursor
    // as the `since` bound, and remember the subscription id in the bot state.
    client_manager
        .subscribe_bots(&db)
        .await
        .map_err(|e| format!("failed to subscribe bots: {e}"))?;

    let mut dispatch = Dispatch::new(
        Arc::new(RwLock::new(client_manager)),
        db,
        diagnostics.clone(),
    );
    dispatch.set_handler_stale_timeout(Duration::from_secs(
        config.daemon.handler_stale_timeout_secs,
    ));
    let dispatch = Arc::new(dispatch);

    if let Err(e) = dispatch.restore_handlers().await {
        return Err(format!("failed to restore handler registrations: {e}"));
    }

    let (metrics_shutdown_tx, metrics_shutdown_rx) = watch::channel(false);
    let metrics_handle = dispatch
        .clone()
        .spawn_periodic_metrics(Duration::from_secs(30), metrics_shutdown_rx);

    let (reaper_shutdown_tx, reaper_shutdown_rx) = watch::channel(false);
    let reaper_handle = dispatch.clone().spawn_handler_reaper(
        Duration::from_secs(config.daemon.handler_stale_timeout_secs),
        Duration::from_secs(config.daemon.handler_reap_interval_secs),
        reaper_shutdown_rx,
    );

    let http_token = if cli.enable_http {
        Some(
            http::init_token(Path::new(&data_dir))
                .await
                .map_err(|e| format!("failed to initialize HTTP token: {e}"))?,
        )
    } else {
        None
    };

    #[cfg(unix)]
    if let Some(token) = http_token.clone() {
        let data_dir_for_sighup = data_dir.clone();
        tokio::spawn(async move {
            let mut sighup = match signal(SignalKind::hangup()) {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "failed to install SIGHUP handler");
                    return;
                }
            };
            loop {
                if sighup.recv().await.is_none() {
                    break;
                }
                match http::reload_token(&token, Path::new(&data_dir_for_sighup)).await {
                    Ok(()) => info!("HTTP secret token reloaded"),
                    Err(e) => warn!(error = %e, "failed to reload HTTP secret token"),
                }
            }
        });
    }

    let mut transport = TransportLayer::new(&config, cli.enable_http);
    if let Some(token) = http_token {
        transport = transport.with_http_token(token);
    }
    let coordinator = ShutdownCoordinator::start()?;

    let (transport_shutdown_tx, transport_shutdown_rx) = oneshot::channel();
    let transport_handle = tokio::spawn(transport.run(dispatch.clone(), transport_shutdown_rx));

    emit_agent_status(&diagnostics, &dispatch, DaemonStatus::Ready).await;

    info!("pacto-bot-api daemon ready");

    let mut event_stream = nostr_client.receive_events();
    let mut flush_interval = tokio::time::interval(Duration::from_secs(30));
    let mut shutdown_rx = coordinator.shutdown_rx;

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown_rx => break,
            _ = flush_interval.tick() => {
                if let Err(e) = diagnostics.flush_report(Path::new(&data_dir)).await {
                    warn!(error = %e, "failed to flush diagnostics report");
                }
            }
            event_result = event_stream.next() => {
                match event_result {
                    Some(Ok(event)) => {
                        // Spawn each event dispatch so one bot's slow signer
                        // or a long handler timeout does not block other bots.
                        let dispatch = dispatch.clone();
                        tokio::spawn(async move {
                            if let Err(e) = dispatch.dispatch_event(event).await {
                                warn!(error = %e, "failed to dispatch event");
                            }
                        });
                    }
                    Some(Err(e)) => warn!(error = %e, "nostr event error"),
                    None => {
                        warn!("nostr event stream ended, shutting down");
                        break;
                    }
                }
            }
        }
    }

    info!("pacto-bot-api daemon shutting down");
    emit_agent_status(&diagnostics, &dispatch, DaemonStatus::ShuttingDown).await;

    let _ = metrics_shutdown_tx.send(true);
    let _ = reaper_shutdown_tx.send(true);

    let force_rx = coordinator.force_rx;
    let graceful_shutdown = async {
        // Brief grace window so a second signal received immediately after the
        // first can be detected and trigger a forced exit before cleanup completes.
        tokio::time::sleep(Duration::from_millis(100)).await;

        if let Err(e) = dispatch.flush_cursors().await {
            warn!(error = %e, "failed to flush cursors during shutdown");
        }

        if let Err(e) = diagnostics.flush_report(Path::new(&data_dir)).await {
            warn!(error = %e, "failed to flush diagnostics report during shutdown");
        }

        let _ = transport_shutdown_tx.send(());
        let _ = transport_handle.await;
        let _ = metrics_handle.await;
        let _ = reaper_handle.await;

        nostr_client.shutdown().await;

        // Release the daemon lock before declaring stopped and clean up the
        // lock file so the admin CLI does not see a stale PID.
        drop(lock_file);
        let _ = tokio::fs::remove_file(&lock_path).await;

        emit_agent_status(&diagnostics, &dispatch, DaemonStatus::Stopped).await;

        if let Err(e) = diagnostics.flush_report(Path::new(&data_dir)).await {
            warn!(error = %e, "failed to flush final diagnostics report");
        }
    };

    tokio::select! {
        _ = graceful_shutdown => {},
        _ = force_rx => {
            warn!("second shutdown signal received, forcing exit");
            process::exit(1);
        }
        _ = tokio::time::sleep(SHUTDOWN_TIMEOUT) => {
            warn!("graceful shutdown timed out, forcing exit");
            process::exit(1);
        }
    }

    Ok(())
}

/// Open the daemon lock file with owner-only permissions.
fn open_lock_file(path: &Path) -> Result<File, std::io::Error> {
    #[cfg(unix)]
    {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
    }
}

/// Open the daemon lock file on a blocking thread and attempt to take the
/// exclusive advisory lock. Returns the locked file handle on success.
async fn acquire_lock_file(lock_path: &Path) -> Result<File, String> {
    let lock_path = lock_path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<File, String> {
        let mut file = open_lock_file(&lock_path)
            .map_err(|e| format!("failed to open lock file {}: {e}", lock_path.display()))?;

        if file.try_lock_exclusive().is_err() {
            return Err(format!(
                "daemon is already running (lock held at {})",
                lock_path.display()
            ));
        }

        // Write the daemon's PID into the lock file so the admin CLI can verify
        // the process is still alive without relying solely on the advisory lock.
        let pid = process::id();
        file.write_all(format!("{pid}\n").as_bytes())
            .map_err(|e| format!("failed to write PID to lock file: {e}"))?;
        file.flush()
            .map_err(|e| format!("failed to flush PID to lock file: {e}"))?;

        Ok(file)
    })
    .await
    .map_err(|e| format!("lock file task failed: {e}"))?
}

/// Emit an `agent.status` lifecycle notification and update diagnostics.
async fn emit_agent_status(diagnostics: &Diagnostics, dispatch: &Dispatch, status: DaemonStatus) {
    diagnostics.set_status(status);
    dispatch.broadcast_status(status).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs2::FileExt;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::time::timeout;

    fn write_test_config(dir: &std::path::Path) -> std::io::Result<PathBuf> {
        let path = dir.join("pacto-bot-api.toml");
        std::fs::write(&path, "[daemon]\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&path, perms)?;
        }
        Ok(path)
    }

    #[tokio::test]
    async fn startup_succeeds_with_valid_config() {
        let dir = TempDir::new().unwrap();
        let config_path = write_test_config(dir.path()).unwrap();
        let data_dir = dir.path().join("data");
        let cli = Cli {
            config: config_path,
            data_dir: Some(data_dir.clone()),
            enable_http: false,
            log_level: None,
        };

        let result = daemon_startup(&cli).await;
        assert!(result.is_ok(), "expected startup to succeed: {result:?}");

        let lock_path = data_dir.join(DAEMON_LOCK_FILE);
        assert!(lock_path.exists(), "lock file should exist after startup");

        let contents = std::fs::read_to_string(&lock_path).unwrap();
        let pid: u32 = contents.trim().parse().unwrap();
        assert_eq!(pid, std::process::id());
    }

    #[tokio::test]
    async fn startup_exits_when_lock_already_held() {
        let dir = TempDir::new().unwrap();
        let config_path = write_test_config(dir.path()).unwrap();
        let data_dir = dir.path().join("data");

        // Pre-create the lock file and hold it.
        let lock_path = data_dir.join(DAEMON_LOCK_FILE);
        std::fs::create_dir_all(&data_dir).unwrap();
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&lock_path)
            .unwrap();
        held.try_lock_exclusive().unwrap();

        let cli = Cli {
            config: config_path,
            data_dir: Some(data_dir),
            enable_http: false,
            log_level: None,
        };

        let result = daemon_startup(&cli).await;
        assert!(
            result.is_err(),
            "expected startup to fail when lock is held"
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("already running"),
            "expected lock-held error, got: {err}"
        );
    }

    #[tokio::test]
    async fn clean_shutdown_on_signal() {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let (_force_tx, force_rx) = tokio::sync::oneshot::channel::<()>();
        let coordinator = ShutdownCoordinator::start_with(
            async move {
                let _ = shutdown_rx.await;
            },
            async move {
                let _ = force_rx.await;
            },
        );

        let _ = shutdown_tx.send(());

        let ShutdownCoordinator {
            shutdown_rx,
            force_rx: _,
        } = coordinator;
        timeout(Duration::from_secs(1), shutdown_rx)
            .await
            .expect("shutdown signal should resolve")
            .expect("coordinator should receive shutdown");
    }
}
