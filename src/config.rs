use crate::errors::DaemonError;
use secrecy::{ExposeSecret, SecretString};
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
    #[serde(default = "default_http_max_connections")]
    pub http_max_connections: usize,
    #[serde(default = "default_http_idle_timeout_secs")]
    pub http_idle_timeout_secs: u64,
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

fn default_http_max_connections() -> usize {
    100
}

fn default_http_idle_timeout_secs() -> u64 {
    60
}

/// Per-bot identity configuration.
#[derive(Debug, Clone, Default, Deserialize)]
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
    /// Human-readable display name for the bot profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Description text for the bot profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    /// URL to the bot's profile picture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
}

/// Signing backend configuration for a bot identity.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub enum SigningConfig {
    /// Local test key (dev-only).
    Nsec { nsec: SecretString },
    /// Local NIP-46 bunker on the same machine.
    BunkerLocal { uri: SecretString },
    /// Production NIP-46 bunker reachable over `wss://`.
    BunkerRemote { uri: SecretString },
}

impl Default for SigningConfig {
    fn default() -> Self {
        SigningConfig::Nsec {
            nsec: SecretString::new(String::new().into()),
        }
    }
}

impl SigningConfig {
    /// Public label for the signing backend used in diagnostics.
    pub fn backend_label(&self) -> &'static str {
        match self {
            SigningConfig::Nsec { .. } => "nsec",
            SigningConfig::BunkerLocal { .. } => "bunker_local",
            SigningConfig::BunkerRemote { .. } => "bunker_remote",
        }
    }
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

        // Also reject if the parent directory is writable by group or other,
        // since a world-writable directory would let anyone replace the file.
        if let Some(parent) = path.parent() {
            // A relative path like `pacto-bot-api.toml` reports an empty parent;
            // treat it as the current directory.
            let parent = if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            };
            let parent_meta = fs::metadata(parent).map_err(DaemonError::Io)?;
            let parent_mode = parent_meta.permissions().mode();
            if parent_mode & 0o022 != 0 {
                return Err(DaemonError::Config(format!(
                    "config file directory {} must not be writable by group or other, found 0o{:o}",
                    parent.display(),
                    parent_mode & 0o777
                )));
            }
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
                if nsec.expose_secret().is_empty() {
                    return Err(DaemonError::Config(format!(
                        "bot {}: nsec backend requires a non-empty nsec value",
                        bot.id
                    )));
                }
            }
            SigningConfig::BunkerLocal { uri } => {
                if uri.expose_secret().is_empty() {
                    return Err(DaemonError::Config(format!(
                        "bot {}: bunker_local backend requires a non-empty uri",
                        bot.id
                    )));
                }
            }
            SigningConfig::BunkerRemote { uri } => {
                let uri = uri.expose_secret();
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

/// Expand `${ENV_VAR}` references in a string. Supports `${ENV_VAR}` and
/// `${ENV_VAR:-default}` syntax; if the variable is unset, the default is used
/// when provided, otherwise the placeholder is replaced with an empty string.
fn expand_env_vars(input: &str) -> String {
    expand_env_vars_with_lookup(input, |var| env::var(var).ok())
}

fn expand_env_vars_with_lookup<F>(input: &str, lookup: F) -> String
where
    F: Fn(&str) -> Option<String>,
{
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut var_name = String::new();
            let mut default_value = String::new();
            let mut found_close = false;
            let mut in_default = false;

            while let Some(inner) = chars.next() {
                if inner == '}' {
                    found_close = true;
                    break;
                }
                if inner == ':' && chars.peek() == Some(&'-') {
                    chars.next(); // consume '-'
                    in_default = true;
                    continue;
                }
                if in_default {
                    default_value.push(inner);
                } else {
                    var_name.push(inner);
                }
            }

            if found_close {
                let value = lookup(&var_name).unwrap_or_default();
                if value.is_empty() {
                    output.push_str(&default_value);
                } else {
                    output.push_str(&value);
                }
            } else {
                output.push('$');
                output.push('{');
                output.push_str(&var_name);
                if in_default {
                    output.push(':');
                    output.push('-');
                }
                output.push_str(&default_value);
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

    fn write_config(content: &str) -> (tempfile::TempDir, tempfile::NamedTempFile, PathBuf) {
        // Create a restricted temp directory so the parent-directory permission
        // check passes on CI runners where /tmp is world-writable.
        let dir = tempfile::tempdir().unwrap();
        let mut file = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        let path = file.path().to_path_buf();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o600);
            fs::set_permissions(&path, perms).unwrap();
        }

        (dir, file, path)
    }

    #[test]
    fn valid_single_bot_config() {
        let (_dir, _file, path) = write_config(
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
        let (_dir, _file, path) = write_config(
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
        let (_dir, _file, path) = write_config(
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
        let (_dir, _file, path) = write_config(
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
        let (_dir, _file, path) = write_config(
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
    #[allow(unsafe_code)]
    fn env_var_expansion() {
        // SAFETY: test-only mutation of a unique environment variable name.
        unsafe { env::set_var("PACT_TEST_NSEC", "nsec1fromenv") };
        let (_dir, _file, path) = write_config(
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
                assert_eq!(nsec.expose_secret(), "nsec1fromenv");
            }
            _ => panic!("expected nsec backend"),
        }
    }

    #[test]
    fn tilde_expansion() {
        let home = env::var("HOME").expect("HOME must be set for this test");
        let (_dir, _file, path) = write_config(
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
        let (_dir, _file, path) = write_config(
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
    fn config_accepts_0o600_permissions() {
        let (_dir, _file, path) = write_config(
            r#"
[[bots]]
id = "echo-bot"
npub = "npub1a"
signing = { backend = "nsec", nsec = "nsec1a" }
"#,
        );

        // write_config already sets 0o600 on Unix.
        DaemonConfig::load(&path).expect("0o600 config should load");
    }

    #[test]
    fn config_rejects_0o644_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
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

    #[test]
    #[cfg(unix)]
    fn config_rejects_world_writable_parent_directory() {
        use std::os::unix::fs::PermissionsExt;

        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("pacto-bot-api.toml");
        fs::write(
            &path,
            r#"
[[bots]]
id = "echo-bot"
npub = "npub1a"
signing = { backend = "nsec", nsec = "nsec1a" }
"#,
        )
        .unwrap();

        // Restrict the file, but leave the parent world-writable.
        let mut file_perms = fs::metadata(&path).unwrap().permissions();
        file_perms.set_mode(0o600);
        fs::set_permissions(&path, file_perms).unwrap();

        let mut dir_perms = fs::metadata(parent.path()).unwrap().permissions();
        dir_perms.set_mode(0o777);
        fs::set_permissions(parent.path(), dir_perms).unwrap();

        let err = DaemonConfig::load(&path).unwrap_err();
        assert!(
            err.to_string()
                .contains("must not be writable by group or other")
        );
    }

    #[test]
    fn expand_env_vars_replaces_set_variable() {
        let values: std::collections::HashMap<&str, String> =
            [("PACTO_TEST_VAR", "replaced".to_string())]
                .into_iter()
                .collect();
        assert_eq!(
            expand_env_vars_with_lookup("${PACTO_TEST_VAR}", |var| values.get(var).cloned()),
            "replaced"
        );
    }

    #[test]
    fn expand_env_vars_leaves_unset_variable_empty() {
        let values: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
        assert_eq!(
            expand_env_vars_with_lookup("${PACTO_TEST_UNSET_VAR}", |var| values.get(var).cloned()),
            ""
        );
    }

    #[test]
    fn expand_env_vars_uses_default_when_unset() {
        let values: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
        assert_eq!(
            expand_env_vars_with_lookup("${PACTO_TEST_UNSET_WITH_DEFAULT:-default_value}", |var| {
                values.get(var).cloned()
            }),
            "default_value"
        );
    }

    #[test]
    fn expand_env_vars_ignores_default_when_set() {
        let values: std::collections::HashMap<&str, String> =
            [("PACTO_TEST_SET_WITH_DEFAULT", "actual_value".to_string())]
                .into_iter()
                .collect();
        assert_eq!(
            expand_env_vars_with_lookup("${PACTO_TEST_SET_WITH_DEFAULT:-default_value}", |var| {
                values.get(var).cloned()
            }),
            "actual_value"
        );
    }

    #[test]
    fn expand_env_vars_preserves_non_placeholder_text() {
        let values: std::collections::HashMap<&str, String> =
            [("PACTO_TEST_RELAY", "wss://relay.example.com".to_string())]
                .into_iter()
                .collect();
        assert_eq!(
            expand_env_vars_with_lookup("relays = [\"${PACTO_TEST_RELAY}\"]", |var| values
                .get(var)
                .cloned()),
            "relays = [\"wss://relay.example.com\"]"
        );
    }
}
