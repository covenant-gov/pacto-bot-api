use crate::errors::DaemonError;
use serde::Deserialize;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Top-level daemon configuration loaded from `pacto-bot-api.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct DaemonConfig {
    #[serde(default)]
    pub daemon: GlobalDaemonConfig,
    #[serde(default)]
    pub bots: Vec<BotConfig>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct GlobalDaemonConfig {
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    #[serde(default = "default_socket_path")]
    pub socket_path: String,
    #[serde(default = "default_http_bind")]
    pub http_bind: String,
}

fn default_data_dir() -> String {
    "~/.local/share/pacto-bot-api".into()
}

fn default_socket_path() -> String {
    "~/.local/share/pacto-bot-api/pacto-bot-api.sock".into()
}

fn default_http_bind() -> String {
    "127.0.0.1:9800".into()
}

/// Per-bot identity configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct BotConfig {
    /// Daemon-local label. Must be unique within the config file.
    pub id: String,
    /// The bot's Nostr public key (npub).
    pub npub: String,
    /// Signing backend for this bot.
    pub signing: SigningConfig,
    /// Relay URLs this bot uses.
    #[serde(default)]
    pub relays: Vec<String>,
    /// Capabilities granted to handlers for this bot.
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// Signing backend configuration for a bot identity.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub enum SigningConfig {
    /// Local test key (dev-only).
    Nsec { nsec: String },
    /// Local NIP-46 bunker on the same machine.
    BunkerLocal { uri: String },
    /// Production NIP-46 bunker reachable over `wss://`.
    BunkerRemote { uri: String },
}

impl DaemonConfig {
    /// Load and validate configuration from `path`.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, DaemonError> {
        let path = path.as_ref();

        enforce_config_permissions(path)?;

        let raw = fs::read_to_string(path)?;
        let raw = expand_env_vars(&raw);

        let mut config: DaemonConfig = toml::from_str(&raw)?;

        // Expand paths inside string fields.
        config.daemon.data_dir = expand_path(&config.daemon.data_dir);
        config.daemon.socket_path = expand_path(&config.daemon.socket_path);

        // Validate bot_id uniqueness and signing backend rules.
        validate_bots(&config.bots)?;

        Ok(config)
    }

    /// Data directory with expanded paths.
    pub fn data_dir(&self) -> &str {
        &self.daemon.data_dir
    }

    /// Unix socket path with expanded paths.
    pub fn socket_path(&self) -> &str {
        &self.daemon.socket_path
    }
}

fn enforce_config_permissions(path: &Path) -> Result<(), DaemonError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = fs::metadata(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                DaemonError::Config(format!("config file not found: {}", path.display()))
            } else {
                DaemonError::Io(e)
            }
        })?;
        let mode = metadata.permissions().mode();
        // Reject if group or other have any permissions.
        if mode & 0o077 != 0 {
            return Err(DaemonError::Config(format!(
                "config file {} must be readable only by owner (mode 0o600 or stricter), found 0o{:o}",
                path.display(),
                mode & 0o777
            )));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        // Permission checks are a no-op on non-Unix platforms in this scaffold.
    }
    Ok(())
}

fn validate_bots(bots: &[BotConfig]) -> Result<(), DaemonError> {
    let mut seen = HashSet::new();
    for bot in bots {
        if !seen.insert(bot.id.clone()) {
            return Err(DaemonError::Config(format!("duplicate bot_id: {}", bot.id)));
        }

        match &bot.signing {
            SigningConfig::Nsec { nsec } => {
                if nsec.is_empty() {
                    return Err(DaemonError::Config(format!(
                        "bot {}: nsec backend requires a non-empty nsec value",
                        bot.id
                    )));
                }
            }
            SigningConfig::BunkerLocal { uri } => {
                if uri.is_empty() {
                    return Err(DaemonError::Config(format!(
                        "bot {}: bunker_local backend requires a non-empty uri",
                        bot.id
                    )));
                }
            }
            SigningConfig::BunkerRemote { uri } => {
                if uri.is_empty() {
                    return Err(DaemonError::Config(format!(
                        "bot {}: bunker_remote backend requires a non-empty uri",
                        bot.id
                    )));
                }
                // Production bunker URIs must use wss:// relays.
                if uri.contains("ws://") && !uri.contains("wss://") {
                    return Err(DaemonError::Config(format!(
                        "bot {}: bunker_remote backend must use wss://, got {}",
                        bot.id, uri
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Expand `${ENV_VAR}` references in a string.
fn expand_env_vars(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut var_name = String::new();
            let mut found_close = false;
            for inner in chars.by_ref() {
                if inner == '}' {
                    found_close = true;
                    break;
                }
                var_name.push(inner);
            }
            if found_close {
                if let Ok(value) = env::var(&var_name) {
                    output.push_str(&value);
                }
                // If the variable is unset, leave the placeholder empty.
            } else {
                output.push('$');
                output.push('{');
                output.push_str(&var_name);
            }
        } else {
            output.push(ch);
        }
    }

    output
}

/// Expand `~` and environment variables in a filesystem path.
fn expand_path(input: &str) -> String {
    let expanded = if input.starts_with("~/") || input == "~" {
        if let Ok(home) = env::var("HOME") {
            if input == "~" {
                home
            } else {
                format!("{}{}", home, &input[1..])
            }
        } else {
            input.to_string()
        }
    } else {
        input.to_string()
    };
    expand_env_vars(&expanded)
}

impl BotConfig {
    /// Resolved data directory path.
    pub fn data_dir_path(&self, global: &GlobalDaemonConfig) -> PathBuf {
        PathBuf::from(expand_path(&global.data_dir))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_config(content: &str) -> (tempfile::NamedTempFile, PathBuf) {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(content.as_bytes()).unwrap();
        let path = file.path().to_path_buf();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o600);
            fs::set_permissions(&path, perms).unwrap();
        }

        (file, path)
    }

    #[test]
    fn valid_single_bot_config() {
        let (_file, path) = write_config(
            r#"
[daemon]
data_dir = "/tmp/pacto"

[[bots]]
id = "echo-bot"
npub = "npub1echobot"
signing = { backend = "nsec", nsec = "nsec1deadbeef" }
relays = ["wss://relay.example.com"]
capabilities = ["ReadMessages", "SendMessages"]
"#,
        );

        let config = DaemonConfig::load(&path).unwrap();
        assert_eq!(config.bots.len(), 1);
        assert_eq!(config.bots[0].id, "echo-bot");
        assert_eq!(config.bots[0].npub, "npub1echobot");
        assert!(matches!(config.bots[0].signing, SigningConfig::Nsec { .. }));
    }

    #[test]
    fn valid_multi_bot_config() {
        let (_file, path) = write_config(
            r#"
[[bots]]
id = "echo-bot"
npub = "npub1echo"
signing = { backend = "nsec", nsec = "nsec1echo" }
relays = ["wss://relay.example.com"]
capabilities = ["ReadMessages", "SendMessages"]

[[bots]]
id = "welcome-bot"
npub = "npub1welcome"
signing = { backend = "bunker_local", uri = "bunker://abcd1234@127.0.0.1:4848" }
relays = ["wss://relay.example.com"]
capabilities = ["ReadMessages"]

[[bots]]
id = "treasury-bot"
npub = "npub1treasury"
signing = { backend = "bunker_remote", uri = "bunker://efgh5678?relay=wss://relay.nsec.app" }
relays = ["wss://relay.example.com"]
capabilities = ["ReadMessages", "SendMessages"]
"#,
        );

        let config = DaemonConfig::load(&path).unwrap();
        assert_eq!(config.bots.len(), 3);
        assert_eq!(config.bots[2].id, "treasury-bot");
    }

    #[test]
    fn duplicate_bot_id_error() {
        let (_file, path) = write_config(
            r#"
[[bots]]
id = "echo-bot"
npub = "npub1a"
signing = { backend = "nsec", nsec = "nsec1a" }

[[bots]]
id = "echo-bot"
npub = "npub1b"
signing = { backend = "nsec", nsec = "nsec1b" }
"#,
        );

        let err = DaemonConfig::load(&path).unwrap_err();
        assert!(err.to_string().contains("duplicate bot_id"));
    }

    #[test]
    fn missing_required_field_npub() {
        let (_file, path) = write_config(
            r#"
[[bots]]
id = "echo-bot"
signing = { backend = "nsec", nsec = "nsec1a" }
"#,
        );

        let err = DaemonConfig::load(&path).unwrap_err();
        assert!(err.to_string().contains("npub"));
    }

    #[test]
    fn missing_required_field_nsec() {
        let (_file, path) = write_config(
            r#"
[[bots]]
id = "echo-bot"
npub = "npub1a"
signing = { backend = "nsec" }
"#,
        );

        let err = DaemonConfig::load(&path).unwrap_err();
        assert!(err.to_string().contains("nsec"));
    }

    #[test]
    fn env_var_expansion() {
        // SAFETY: test-only mutation of a unique environment variable name.
        unsafe { env::set_var("PACT_TEST_NSEC", "nsec1fromenv") };
        let (_file, path) = write_config(
            r#"
[[bots]]
id = "echo-bot"
npub = "npub1a"
signing = { backend = "nsec", nsec = "${PACT_TEST_NSEC}" }
"#,
        );

        let config = DaemonConfig::load(&path).unwrap();
        match &config.bots[0].signing {
            SigningConfig::Nsec { nsec } => {
                assert_eq!(nsec, "nsec1fromenv");
            }
            _ => panic!("expected nsec backend"),
        }
    }

    #[test]
    fn tilde_expansion() {
        let home = env::var("HOME").expect("HOME must be set for this test");
        let (_file, path) = write_config(
            r#"
[daemon]
data_dir = "~/pacto-test"

[[bots]]
id = "echo-bot"
npub = "npub1a"
signing = { backend = "nsec", nsec = "nsec1a" }
"#,
        );

        let config = DaemonConfig::load(&path).unwrap();
        assert_eq!(config.daemon.data_dir, format!("{}/pacto-test", home));
    }

    #[test]
    fn bunker_remote_rejects_ws() {
        let (_file, path) = write_config(
            r#"
[[bots]]
id = "bad-bot"
npub = "npub1a"
signing = { backend = "bunker_remote", uri = "bunker://efgh5678?relay=ws://relay.nsec.app" }
"#,
        );

        let err = DaemonConfig::load(&path).unwrap_err();
        assert!(err.to_string().contains("must use wss://"));
    }

    #[test]
    fn config_permissions_enforced() {
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
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o644);
            fs::set_permissions(&path, perms).unwrap();
        }

        let err = DaemonConfig::load(&path).unwrap_err();
        assert!(err.to_string().contains("must be readable only by owner"));
    }
}
