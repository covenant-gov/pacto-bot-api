#![allow(dead_code)]

use chrono::Utc;
use nostr::ToBech32;
use pacto_bot_api::config::{BotConfig, SigningConfig};
use pacto_bot_api::db::Database;
use pacto_bot_api::events::EventType;
use pacto_bot_api::handlers::{ConnectionHandle, HandlerRef};
use secrecy::{ExposeSecret, SecretString};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

/// Generate a bot config backed by a freshly generated local nsec key.
pub fn generate_nsec_bot(id: &str) -> Result<(BotConfig, String), Box<dyn Error>> {
    let keys = nostr::Keys::generate();
    let nsec = keys.secret_key().to_bech32()?;
    let npub = keys.public_key().to_bech32()?;
    let bot = BotConfig {
        id: id.to_string(),
        npub,
        signing: SigningConfig::Nsec {
            nsec: SecretString::new(nsec.clone().into()),
        },
        relays: vec!["wss://127.0.0.1:65535".to_string()],
        capabilities: vec!["ReadMessages".to_string()],
    };
    Ok((bot, nsec))
}

/// Generate a bot config backed by a bunker_local URI.
///
/// `match_npub` controls whether the bunker URI declares the configured bot
/// pubkey (`true`) or a different one (`false`).
pub fn generate_bunker_bot(id: &str, match_npub: bool) -> Result<BotConfig, Box<dyn Error>> {
    let keys = nostr::Keys::generate();
    let npub = keys.public_key().to_bech32()?;
    let remote_keys = if match_npub {
        keys
    } else {
        nostr::Keys::generate()
    };
    let uri = format!(
        "bunker://{}?relay=ws://127.0.0.1:4848",
        remote_keys.public_key().to_hex()
    );
    Ok(BotConfig {
        id: id.to_string(),
        npub,
        signing: SigningConfig::BunkerLocal {
            uri: SecretString::new(uri.into()),
        },
        relays: vec![],
        capabilities: vec![],
    })
}

/// Generate a bot config backed by a bunker_local URI, returning the bunker
/// keys so a test can start a live mock bunker.
///
/// `match_npub` controls whether the configured bot npub matches the bunker
/// keys (`true`) or a different generated key (`false`).
pub fn generate_bunker_bot_with_keys(
    id: &str,
    match_npub: bool,
) -> Result<(BotConfig, nostr::Keys), Box<dyn Error>> {
    let keys = nostr::Keys::generate();
    let npub = keys.public_key().to_bech32()?;
    let remote_keys = if match_npub {
        keys.clone()
    } else {
        nostr::Keys::generate()
    };
    let uri = format!(
        "bunker://{}?relay=ws://127.0.0.1:4848",
        remote_keys.public_key().to_hex()
    );
    let bot = BotConfig {
        id: id.to_string(),
        npub,
        signing: SigningConfig::BunkerLocal {
            uri: SecretString::new(uri.into()),
        },
        relays: vec![],
        capabilities: vec![],
    };
    Ok((bot, remote_keys))
}

/// Replace the bunker URI on a bot config.
pub fn set_bunker_uri(bot: &mut BotConfig, new_uri: &str) {
    match &mut bot.signing {
        SigningConfig::BunkerLocal { uri } | SigningConfig::BunkerRemote { uri } => {
            *uri = SecretString::new(new_uri.into());
        }
        SigningConfig::Nsec { .. } => {}
    }
}

/// Write a `pacto-bot-api.toml` into `dir` and return its path.
pub fn make_config(
    dir: &tempfile::TempDir,
    bots: Vec<BotConfig>,
) -> Result<PathBuf, Box<dyn Error>> {
    let data_dir = dir.path().to_string_lossy();
    let socket_path = dir.path().join("pacto-bot-api.sock");
    let mut content = format!(
        "[daemon]\ndata_dir = {:?}\nsocket_path = {:?}\n\n",
        data_dir, socket_path
    );

    for bot in bots {
        content.push_str("[[bots]]\n");
        content.push_str(&format!("id = {:?}\n", bot.id));
        content.push_str(&format!("npub = {:?}\n", bot.npub));
        match &bot.signing {
            SigningConfig::Nsec { nsec } => {
                content.push_str(&format!(
                    "signing = {{ backend = \"nsec\", nsec = {:?} }}\n",
                    nsec.expose_secret()
                ));
            }
            SigningConfig::BunkerLocal { uri } => {
                content.push_str(&format!(
                    "signing = {{ backend = \"bunker_local\", uri = {:?} }}\n",
                    uri.expose_secret()
                ));
            }
            SigningConfig::BunkerRemote { uri } => {
                content.push_str(&format!(
                    "signing = {{ backend = \"bunker_remote\", uri = {:?} }}\n",
                    uri.expose_secret()
                ));
            }
        }
        if !bot.relays.is_empty() {
            content.push_str(&format!("relays = {:?}\n", bot.relays));
        }
        if !bot.capabilities.is_empty() {
            content.push_str(&format!("capabilities = {:?}\n", bot.capabilities));
        }
        content.push('\n');
    }

    let path = dir.path().join("pacto-bot-api.toml");
    fs::write(&path, content)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&path, perms)?;
    }

    Ok(path)
}

/// Create a disconnected handler reference for tests.
pub fn handler_ref(
    id: &str,
    bot_ids: &[&str],
    event_types: &[EventType],
    capabilities: &[&str],
) -> HandlerRef {
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    HandlerRef {
        id: id.to_string(),
        connection: Some(ConnectionHandle::new(tx)),
        bot_ids: bot_ids.iter().map(|s| s.to_string()).collect(),
        event_types: event_types.to_vec(),
        capabilities: capabilities.iter().map(|s| s.to_string()).collect(),
        registered_at: Utc::now(),
    }
}

/// Populate `agent.db` in `dir` with a cursor and handlers for `bot_id`.
pub fn populate_db(
    dir: &tempfile::TempDir,
    bot_id: &str,
    npub: &str,
    cursor: i64,
    handlers: Vec<HandlerRef>,
) -> Result<(), Box<dyn Error>> {
    let db_path = dir.path().join("agent.db");
    let db = Database::open(&db_path)?;
    db.save_cursor(bot_id, npub, cursor)?;
    for handler in handlers {
        db.save_handler(&handler)?;
    }
    Ok(())
}

/// Write an invalid config file with loose permissions for negative tests.
pub fn write_loose_config(path: &Path, content: &str) -> Result<(), Box<dyn Error>> {
    fs::write(path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o644);
        fs::set_permissions(path, perms)?;
    }
    Ok(())
}

use std::process::{Child, Stdio};
use std::time::Duration;

/// Path to the daemon binary for integration tests.
pub fn daemon_bin_path() -> Result<PathBuf, Box<dyn Error>> {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_pacto-bot-api") {
        return Ok(PathBuf::from(path));
    }
    let cmd = assert_cmd::Command::cargo_bin("pacto-bot-api")?;
    Ok(cmd.get_program().into())
}

/// Spawn the daemon with `config` and wait until its Unix socket appears,
/// which indicates the transport layer is bound and the daemon is ready.
///
/// The caller is responsible for killing the returned child.
pub async fn spawn_daemon_until_ready(config: &Path) -> Result<Child, Box<dyn std::error::Error>> {
    let socket_path = config
        .parent()
        .ok_or("config has no parent directory")?
        .join("pacto-bot-api.sock");

    let mut child = std::process::Command::new(daemon_bin_path()?)
        .arg("--config")
        .arg(config)
        .stderr(Stdio::null())
        .spawn()?;

    let start = tokio::time::Instant::now();
    loop {
        if start.elapsed() > Duration::from_secs(15) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("daemon did not become ready".into());
        }

        if socket_path.exists() {
            return Ok(child);
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Send SIGINT to a daemon child and wait for it to exit cleanly.
///
/// This allows coverage data and shutdown hooks to flush, unlike SIGKILL.
pub async fn shutdown_daemon(mut child: Child) -> Result<(), Box<dyn std::error::Error>> {
    let pid = child.id();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGINT,
    )?;

    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = child.wait();
    })
    .await;
    Ok(())
}
