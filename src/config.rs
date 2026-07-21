use crate::errors::DaemonError;
use percent_encoding::percent_decode_str;
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

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
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
    #[serde(default = "default_handler_stale_timeout_secs")]
    pub handler_stale_timeout_secs: u64,
    #[serde(default = "default_handler_reap_interval_secs")]
    pub handler_reap_interval_secs: u64,
}

impl Default for GlobalDaemonConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            socket_path: default_socket_path(),
            http_bind: default_http_bind(),
            http_max_connections: default_http_max_connections(),
            http_idle_timeout_secs: default_http_idle_timeout_secs(),
            handler_stale_timeout_secs: default_handler_stale_timeout_secs(),
            handler_reap_interval_secs: default_handler_reap_interval_secs(),
        }
    }
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

fn default_handler_stale_timeout_secs() -> u64 {
    30
}

fn default_handler_reap_interval_secs() -> u64 {
    5
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
    /// Time window in seconds for MLS group-message deduplication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mls_dedup_window_secs: Option<u64>,
    /// Path to the per-bot MLS SQLite database.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mls_db_path: Option<PathBuf>,
    /// Freshness window in seconds for MLS KeyPackage events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mls_key_package_freshness_secs: Option<u64>,
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

        // Validate bot_id uniqueness, signing backend rules, and MLS config.
        validate_bots(&mut config.bots, Path::new(&config.daemon.data_dir))?;

        // Validate daemon timing values used by tokio::time::interval.
        validate_daemon_config(&config.daemon)?;

        Ok(config)
    }

    /// Load and validate configuration from `path` without blocking the async
    /// runtime. Used by the daemon; the synchronous [`Self::load`] remains
    /// available for contexts that are already synchronous.
    pub async fn load_async(path: impl AsRef<Path> + Send) -> Result<Self, DaemonError> {
        let path = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || DaemonConfig::load(path))
            .await
            .map_err(|e| DaemonError::Config(format!("config load task failed: {e}")))?
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

pub fn enforce_config_permissions(path: &Path) -> Result<(), DaemonError> {
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

fn validate_daemon_config(daemon: &GlobalDaemonConfig) -> Result<(), DaemonError> {
    if daemon.handler_reap_interval_secs == 0 {
        return Err(DaemonError::Config(
            "daemon.handler_reap_interval_secs must be greater than 0".into(),
        ));
    }
    if daemon.handler_stale_timeout_secs == 0 {
        return Err(DaemonError::Config(
            "daemon.handler_stale_timeout_secs must be greater than 0".into(),
        ));
    }
    Ok(())
}

/// Valid bot capability strings.
const VALID_CAPABILITIES: &[&str] = &[
    "ReadMessages",
    "SendMessages",
    "ManageProfile",
    "SendGroupMessages",
    "ReceiveGroupMessages",
    "CreateMlsGroup",
    "InviteToMlsGroup",
    "ExitMlsGroup",
    "Admin",
];

/// Capabilities that require an MLS engine to be configured.
const MLS_CAPABILITIES: &[&str] = &[
    "SendGroupMessages",
    "ReceiveGroupMessages",
    "CreateMlsGroup",
    "InviteToMlsGroup",
    "ExitMlsGroup",
];

/// Redact query-parameter values from a `bunker://` URI.
///
/// Keeps the scheme and host/path portion visible for debugging while
/// replacing every `key=value` query parameter with `key=[REDACTED]` so that
/// secret tokens are not leaked in error messages.
pub fn redact_bunker_uri(uri: &str) -> String {
    let Some((base, query)) = uri.split_once('?') else {
        return uri.to_string();
    };
    let redacted = query
        .split('&')
        .map(|param| match param.split_once('=') {
            Some((key, _)) => format!("{key}=[REDACTED]"),
            None => param.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&");
    format!("{base}?{redacted}")
}

/// Extract and percent-decode every `relay` query parameter from a `bunker://` URI.
fn parse_bunker_relays(uri: &str) -> Result<Vec<String>, DaemonError> {
    let redacted = redact_bunker_uri(uri);
    let after_scheme = uri.strip_prefix("bunker://").ok_or_else(|| {
        DaemonError::Config(format!("bunker uri missing bunker:// scheme: {redacted}"))
    })?;
    let query = after_scheme.split_once('?').map(|(_, q)| q).unwrap_or("");
    let mut relays = Vec::new();
    for param in query.split('&') {
        if param.is_empty() {
            continue;
        }
        if let Some((key, value)) = param.split_once('=')
            && key == "relay"
        {
            let decoded = percent_decode_str(value)
                .decode_utf8()
                .map_err(|e| {
                    DaemonError::Config(format!("bunker uri relay param is not valid UTF-8: {e}"))
                })?
                .to_string();
            if decoded.is_empty() {
                return Err(DaemonError::Config(
                    "bunker uri relay param is empty".into(),
                ));
            }
            relays.push(decoded);
        }
    }
    Ok(relays)
}

/// Validate that `bot_id` is a safe slug.
///
/// Enforces lowercase alphanumeric, hyphen, and underscore only; the id must
/// start and end with alphanumeric and be 64 characters or fewer. This keeps
/// bot ids usable as directory names, log labels, and config keys while still
/// being human-readable.
pub fn validate_bot_id(bot_id: &str) -> Result<(), DaemonError> {
    if bot_id.is_empty() {
        return Err(DaemonError::Config("bot_id must not be empty".into()));
    }
    if bot_id.len() > 64 {
        return Err(DaemonError::Config(
            "bot_id must be 64 characters or fewer".into(),
        ));
    }
    let first = bot_id.as_bytes()[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err(DaemonError::Config(format!(
            "bot_id '{bot_id}' must start with a lowercase letter or digit"
        )));
    }
    let last = bot_id.as_bytes()[bot_id.len() - 1];
    if !(last.is_ascii_lowercase() || last.is_ascii_digit()) {
        return Err(DaemonError::Config(format!(
            "bot_id '{bot_id}' must end with a lowercase letter or digit"
        )));
    }
    if bot_id
        .chars()
        .any(|c| !c.is_ascii_lowercase() && !c.is_ascii_digit() && c != '-' && c != '_')
    {
        return Err(DaemonError::Config(format!(
            "bot_id '{bot_id}' must be a slug: lowercase letters, digits, hyphens, and underscores only",
        )));
    }
    Ok(())
}

/// Light validation for a profile picture URL.
///
/// Rejects obviously invalid values; this is not a full URL parser, but it
/// catches the common mistakes of leaving the field empty, forgetting a
/// scheme, or using a non-HTTP scheme.
fn validate_picture_url(url: &str) -> Result<(), DaemonError> {
    if url.is_empty() {
        return Err(DaemonError::Config(
            "picture URL must not be empty when configured".into(),
        ));
    }
    let lower = url.to_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err(DaemonError::Config(format!(
            "picture URL must start with http:// or https://, got: {url}"
        )));
    }
    Ok(())
}

fn validate_bots(bots: &mut [BotConfig], data_dir: &Path) -> Result<(), DaemonError> {
    let mut seen_ids = HashSet::new();
    for bot in &mut *bots {
        validate_bot_id(&bot.id)?;
        if !seen_ids.insert(bot.id.clone()) {
            return Err(DaemonError::Config(format!("duplicate bot_id: {}", bot.id)));
        }
    }

    // Validate display_name presence, non-empty value, and uniqueness.
    // Display names are compared case-insensitively so that "Joke Bot" and
    // "joke bot" are treated as the same alias; this prevents ambiguous
    // alias-to-npub resolution in the UI.
    let mut seen_display_names = HashSet::new();
    for bot in &mut *bots {
        let display_name = bot.display_name.as_ref().ok_or_else(|| {
            DaemonError::Config(format!("bot {}: display_name is required", bot.id))
        })?;
        if display_name.trim().is_empty() {
            return Err(DaemonError::Config(format!(
                "bot {}: display_name must be non-empty",
                bot.id
            )));
        }
        let normalized = display_name.to_lowercase();
        if !seen_display_names.insert(normalized) {
            return Err(DaemonError::Config(format!(
                "duplicate display_name: {}",
                display_name
            )));
        }
    }

    for bot in &mut *bots {
        for cap in &bot.capabilities {
            if !VALID_CAPABILITIES.contains(&cap.as_str()) {
                return Err(DaemonError::Config(format!(
                    "bot {}: unknown capability {}; expected one of: {}",
                    bot.id,
                    cap,
                    VALID_CAPABILITIES.join(", ")
                )));
            }
        }

        let has_mls_capability = bot
            .capabilities
            .iter()
            .any(|c| MLS_CAPABILITIES.contains(&c.as_str()));
        if has_mls_capability && bot.mls_db_path.is_none() {
            return Err(DaemonError::Config(format!(
                "bot {}: MLS capability requires mls_db_path",
                bot.id
            )));
        }

        if bot.mls_db_path.is_some() {
            let canonical = validate_mls_db_path(bot, data_dir)?;
            bot.mls_db_path = Some(canonical);
        }

        if let Some(picture) = &bot.picture {
            validate_picture_url(picture)
                .map_err(|e| DaemonError::Config(format!("bot {}: {}", bot.id, e)))?;
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
                let relays = parse_bunker_relays(uri)?;
                if relays.iter().any(|r| !r.starts_with("wss://")) {
                    return Err(DaemonError::Config(format!(
                        "bot {}: bunker_remote backend must use wss://, got {}",
                        bot.id,
                        redact_bunker_uri(uri)
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Validate and canonicalize a bot's configured `mls_db_path`.
///
/// The path is resolved against the per-bot data directory if relative. It
/// must remain inside that directory after canonicalization, must not
/// contain symlinks or mountpoints in the parent chain, and must not resolve
/// under `/tmp` or `/dev/shm`. The parent directory is created with `0o700`
/// permissions through the shared MLS path helper.
pub fn validate_mls_db_path(
    bot: &BotConfig,
    data_dir: impl AsRef<Path>,
) -> Result<PathBuf, DaemonError> {
    let data_dir = data_dir.as_ref();
    let path = bot
        .mls_db_path
        .as_ref()
        .ok_or_else(|| DaemonError::Config(format!("bot {}: mls_db_path is missing", bot.id)))?;

    // Resolve the configured path relative to the bot data directory if needed.
    let bot_data_dir = data_dir.join(&bot.id);
    let resolved_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        bot_data_dir.join(path)
    };

    // The shared helper requires an absolute path. Make it absolute relative
    // to the current directory (which is what the relative data_dir would have
    // used anyway).
    let resolved_path = if resolved_path.is_absolute() {
        resolved_path
    } else {
        std::env::current_dir()
            .map_err(DaemonError::Io)?
            .join(resolved_path)
    };

    // Validate and harden the parent directory. The shared helper rejects
    // symlinks, mountpoints, shared temporary directories, and non-owner-only
    // permissions, and creates the parent with 0o700.
    let canonical_parent = crate::mls_path::secure_ensure_mls_parent_dir(&resolved_path)
        .map_err(|e| map_mls_path_error_to_config(bot, e))?;

    // Verify the parent remains inside the bot data directory sandbox.
    let canonical_bot_data_dir = bot_data_dir.canonicalize().map_err(DaemonError::Io)?;
    if !canonical_parent.starts_with(&canonical_bot_data_dir) {
        return Err(DaemonError::Config(format!(
            "bot {}: mls_db_path {} escapes data directory {}",
            bot.id,
            resolved_path.display(),
            canonical_bot_data_dir.display()
        )));
    }

    // If the database file already exists, canonicalize it and verify too.
    let canonical_path = if resolved_path.exists() {
        let canon = resolved_path.canonicalize().map_err(DaemonError::Io)?;
        if !canon.starts_with(&canonical_bot_data_dir) {
            return Err(DaemonError::Config(format!(
                "bot {}: mls_db_path {} escapes data directory {}",
                bot.id,
                resolved_path.display(),
                canonical_bot_data_dir.display()
            )));
        }
        canon
    } else {
        canonical_parent.join(resolved_path.file_name().ok_or_else(|| {
            DaemonError::Config(format!("bot {}: mls_db_path is invalid", bot.id))
        })?)
    };

    Ok(canonical_path)
}

fn map_mls_path_error_to_config(
    bot: &BotConfig,
    err: crate::mls_path::MlsPathError,
) -> DaemonError {
    match err {
        crate::mls_path::MlsPathError::SharedTemp => DaemonError::Config(format!(
            "bot {}: mls_db_path must not be under /tmp or /dev/shm",
            bot.id
        )),
        crate::mls_path::MlsPathError::Symlink(_) => DaemonError::Config(format!(
            "bot {}: mls_db_path must not contain symlinks",
            bot.id
        )),
        crate::mls_path::MlsPathError::Io(io) => DaemonError::Io(io),
        other => DaemonError::Config(format!("bot {}: {}", bot.id, other)),
    }
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

    /// Resolved MLS group-message deduplication window.
    pub fn dedup_window(&self) -> u64 {
        self.mls_dedup_window_secs.unwrap_or(900)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::Duration;

    fn write_config(content: &str) -> (tempfile::TempDir, tempfile::NamedTempFile, PathBuf) {
        // Create a restricted temp directory so the parent-directory permission
        // check passes on CI runners where /tmp is world-writable.
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(dir.path()).unwrap().permissions();
            perms.set_mode(0o700);
            fs::set_permissions(dir.path(), perms).unwrap();
        }
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
display_name = "Echo Bot"
npub = "npub1echobot"
signing = { backend = "nsec", nsec = "nsec1deadbeef" }
relays = ["wss://relay.example.com"]
capabilities = ["ReadMessages", "SendMessages"]
"#,
        );

        let config = DaemonConfig::load(&path).unwrap();
        assert_eq!(config.bots.len(), 1);
        assert_eq!(config.bots[0].id, "echo-bot");
        assert_eq!(config.bots[0].display_name.as_deref(), Some("Echo Bot"));
        assert_eq!(config.bots[0].npub, "npub1echobot");
        assert!(matches!(config.bots[0].signing, SigningConfig::Nsec { .. }));
    }

    #[test]
    fn valid_multi_bot_config() {
        let (_dir, _file, path) = write_config(
            r#"
[[bots]]
id = "echo-bot"
display_name = "Echo Bot"
npub = "npub1echo"
signing = { backend = "nsec", nsec = "nsec1echo" }
relays = ["wss://relay.example.com"]
capabilities = ["ReadMessages", "SendMessages"]

[[bots]]
id = "welcome-bot"
display_name = "Welcome Bot"
npub = "npub1welcome"
signing = { backend = "bunker_local", uri = "bunker://abcd1234@127.0.0.1:4848" }
relays = ["wss://relay.example.com"]
capabilities = ["ReadMessages"]

[[bots]]
id = "treasury-bot"
display_name = "Treasury Bot"
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
display_name = "Echo Bot"
npub = "npub1a"
signing = { backend = "nsec", nsec = "nsec1a" }

[[bots]]
id = "echo-bot"
display_name = "Echo Bot Two"
npub = "npub1b"
signing = { backend = "nsec", nsec = "nsec1b" }
"#,
        );

        let err = DaemonConfig::load(&path).unwrap_err();
        assert!(err.to_string().contains("duplicate bot_id"));
    }

    #[test]
    fn missing_display_name_error() {
        let (_dir, _file, path) = write_config(
            r#"
[[bots]]
id = "echo-bot"
npub = "npub1a"
signing = { backend = "nsec", nsec = "nsec1a" }
"#,
        );

        let err = DaemonConfig::load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("display_name is required"), "{msg}");
    }

    #[test]
    fn empty_display_name_error() {
        let (_dir, _file, path) = write_config(
            r#"
[[bots]]
id = "echo-bot"
display_name = ""
npub = "npub1a"
signing = { backend = "nsec", nsec = "nsec1a" }
"#,
        );

        let err = DaemonConfig::load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("display_name must be non-empty"), "{msg}");
    }

    #[test]
    fn whitespace_only_display_name_error() {
        let (_dir, _file, path) = write_config(
            r#"
[[bots]]
id = "echo-bot"
display_name = "   "
npub = "npub1a"
signing = { backend = "nsec", nsec = "nsec1a" }
"#,
        );

        let err = DaemonConfig::load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("display_name must be non-empty"), "{msg}");
    }

    #[test]
    fn duplicate_display_name_error() {
        let (_dir, _file, path) = write_config(
            r#"
[[bots]]
id = "echo-bot"
display_name = "Echo Bot"
npub = "npub1a"
signing = { backend = "nsec", nsec = "nsec1a" }

[[bots]]
id = "echo-bot-2"
display_name = "Echo Bot"
npub = "npub1b"
signing = { backend = "nsec", nsec = "nsec1b" }
"#,
        );

        let err = DaemonConfig::load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("duplicate display_name"), "{msg}");
    }

    #[test]
    fn duplicate_display_name_case_insensitive_error() {
        let (_dir, _file, path) = write_config(
            r#"
[[bots]]
id = "echo-bot"
display_name = "Echo Bot"
npub = "npub1a"
signing = { backend = "nsec", nsec = "nsec1a" }

[[bots]]
id = "echo-bot-2"
display_name = "echo bot"
npub = "npub1b"
signing = { backend = "nsec", nsec = "nsec1b" }
"#,
        );

        let err = DaemonConfig::load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("duplicate display_name"), "{msg}");
    }

    #[test]
    fn missing_required_field_npub() {
        let (_dir, _file, path) = write_config(
            r#"
[[bots]]
id = "echo-bot"
display_name = "Echo Bot"
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
display_name = "Echo Bot"
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
        unsafe { env::set_var("PACTO_TEST_NSEC", "nsec1fromenv") };
        let (_dir, _file, path) = write_config(
            r#"
[[bots]]
id = "echo-bot"
display_name = "Echo Bot"
npub = "npub1a"
signing = { backend = "nsec", nsec = "${PACTO_TEST_NSEC}" }
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
display_name = "Echo Bot"
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
display_name = "Bad Bot"
npub = "npub1a"
signing = { backend = "bunker_remote", uri = "bunker://efgh5678?relay=ws://relay.nsec.app" }
"#,
        );

        let err = DaemonConfig::load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("must use wss://"),
            "expected wss requirement: {msg}"
        );
        assert!(
            !msg.contains("ws://relay.nsec.app"),
            "raw bunker relay URL leaked in error: {msg}"
        );
        assert!(
            msg.contains("relay=[REDACTED]"),
            "query parameter value should be redacted: {msg}"
        );
    }

    #[test]
    fn bunker_remote_error_redacts_secret_query_params() {
        let (_dir, _file, path) = write_config(
            r#"
[[bots]]
id = "bad-bot"
display_name = "Bad Bot"
npub = "npub1a"
signing = { backend = "bunker_remote", uri = "bunker://efgh5678?relay=ws://relay.nsec.app&secret=topsecret" }
"#,
        );

        let err = DaemonConfig::load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("must use wss://"),
            "expected wss requirement: {msg}"
        );
        assert!(!msg.contains("topsecret"), "secret leaked in error: {msg}");
        assert!(
            !msg.contains("ws://relay.nsec.app"),
            "raw relay URL leaked: {msg}"
        );
        assert!(
            msg.contains("secret=[REDACTED]"),
            "secret not redacted: {msg}"
        );
    }

    #[test]
    fn bunker_remote_rejects_mixed_ws_and_wss_relays() {
        let (_dir, _file, path) = write_config(
            r#"
[[bots]]
id = "bad-bot"
display_name = "Bad Bot"
npub = "npub1a"
signing = { backend = "bunker_remote", uri = "bunker://efgh5678?relay=ws://relay1.nsec.app&relay=wss://relay2.nsec.app" }
"#,
        );

        let err = DaemonConfig::load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("must use wss://"),
            "expected wss requirement: {msg}"
        );
        assert!(
            !msg.contains("ws://relay1.nsec.app"),
            "raw bunker relay URL leaked in error: {msg}"
        );
        assert!(
            !msg.contains("wss://relay2.nsec.app"),
            "raw bunker relay URL leaked in error: {msg}"
        );
    }

    #[test]
    fn bunker_remote_accepts_only_wss_relays() {
        let (_dir, _file, path) = write_config(
            r#"
[[bots]]
id = "good-bot"
display_name = "Good Bot"
npub = "npub1a"
signing = { backend = "bunker_remote", uri = "bunker://efgh5678?relay=wss://relay1.nsec.app&relay=wss://relay2.nsec.app" }
relays = ["wss://relay.example.com"]
capabilities = ["ReadMessages"]
"#,
        );

        let config = DaemonConfig::load(&path).unwrap();
        assert_eq!(config.bots.len(), 1);
        assert!(matches!(
            config.bots[0].signing,
            SigningConfig::BunkerRemote { .. }
        ));
    }

    #[test]
    fn redact_bunker_uri_preserves_path_and_masks_query_values() {
        let uri = "bunker://pubkey@127.0.0.1:4848?relay=wss://relay.example&secret=shh&token=abc";
        let out = redact_bunker_uri(uri);
        assert!(
            out.contains("bunker://pubkey@127.0.0.1:4848"),
            "host/path missing: {out}"
        );
        assert!(
            out.contains("relay=[REDACTED]"),
            "relay not redacted: {out}"
        );
        assert!(
            out.contains("secret=[REDACTED]"),
            "secret not redacted: {out}"
        );
        assert!(
            out.contains("token=[REDACTED]"),
            "token not redacted: {out}"
        );
        assert!(
            !out.contains("wss://relay.example"),
            "raw relay URL leaked: {out}"
        );
        assert!(!out.contains("shh"), "secret leaked: {out}");
        assert!(!out.contains("abc"), "token leaked: {out}");
    }

    #[test]
    fn config_accepts_0o600_permissions() {
        let (_dir, _file, path) = write_config(
            r#"
[[bots]]
id = "echo-bot"
display_name = "Echo Bot"
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
display_name = "Echo Bot"
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
display_name = "Echo Bot"
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
    fn rejects_zero_handler_reap_interval_secs() {
        let (_dir, _file, path) = write_config(
            r#"
[daemon]
handler_reap_interval_secs = 0

[[bots]]
id = "echo-bot"
display_name = "Echo Bot"
npub = "npub1a"
signing = { backend = "nsec", nsec = "nsec1a" }
"#,
        );

        let err = DaemonConfig::load(&path).unwrap_err();
        assert!(err.to_string().contains("handler_reap_interval_secs"));
        assert!(err.to_string().contains("greater than 0"));
    }

    #[test]
    fn rejects_zero_handler_stale_timeout_secs() {
        let (_dir, _file, path) = write_config(
            r#"
[daemon]
handler_stale_timeout_secs = 0

[[bots]]
id = "echo-bot"
display_name = "Echo Bot"
npub = "npub1a"
signing = { backend = "nsec", nsec = "nsec1a" }
"#,
        );

        let err = DaemonConfig::load(&path).unwrap_err();
        assert!(err.to_string().contains("handler_stale_timeout_secs"));
        assert!(err.to_string().contains("greater than 0"));
    }

    #[test]
    fn mls_capability_requires_mls_db_path() {
        let (_dir, _file, path) = write_config(
            r#"
[[bots]]
id = "echo-bot"
display_name = "Echo Bot"
npub = "npub1a"
signing = { backend = "nsec", nsec = "nsec1a" }
capabilities = ["SendGroupMessages"]
"#,
        );

        let err = DaemonConfig::load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("MLS capability requires mls_db_path"), "{msg}");
    }

    #[test]
    fn create_mls_group_capability_requires_mls_db_path() {
        let (_dir, _file, path) = write_config(
            r#"
[[bots]]
id = "echo-bot"
display_name = "Echo Bot"
npub = "npub1a"
signing = { backend = "nsec", nsec = "nsec1a" }
capabilities = ["CreateMlsGroup"]
"#,
        );

        let err = DaemonConfig::load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("MLS capability requires mls_db_path"), "{msg}");
    }

    fn restricted_tempdir() -> (tempfile::TempDir, PathBuf) {
        // Keep the temp directory out of /tmp so that relative mls_db_path
        // values do not accidentally resolve under /tmp and get rejected.
        let current_dir = std::env::current_dir().unwrap();
        let dir = tempfile::tempdir_in(&current_dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
            perms.set_mode(0o700);
            std::fs::set_permissions(dir.path(), perms).unwrap();
        }
        let file_path = dir.path().join("pacto-bot-api.toml");
        (dir, file_path)
    }

    #[cfg(target_os = "linux")]
    fn tempdir_under(base: &Path) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir_in(base).unwrap();
        let path = dir.path().to_path_buf();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o700);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        (dir, path)
    }

    #[cfg(target_os = "linux")]
    fn write_config_file(path: &Path, content: &str) {
        std::fs::write(path, content).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(path, perms).unwrap();
        }
    }

    #[test]
    fn mls_db_path_is_validated_and_canonicalized() {
        let (dir, file_path) = restricted_tempdir();
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(
            &file_path,
            format!(
                r#"
[daemon]
data_dir = "{}"

[[bots]]
id = "mls-bot"
display_name = "Mls Bot"
npub = "npub1a"
signing = {{ backend = "nsec", nsec = "nsec1a" }}
capabilities = ["SendGroupMessages"]
mls_db_path = "vector-mls.db"
"#,
                data_dir.display()
            ),
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&file_path).unwrap().permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&file_path, perms).unwrap();
        }

        let config = DaemonConfig::load(&file_path).unwrap();
        let bot = &config.bots[0];
        let resolved = bot.mls_db_path.as_ref().unwrap();
        assert!(
            resolved.is_absolute(),
            "mls_db_path should be canonicalized: {}",
            resolved.display()
        );
        assert!(
            resolved.to_string_lossy().contains("mls-bot"),
            "mls_db_path should be inside bot data dir: {}",
            resolved.display()
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let parent = resolved.parent().unwrap();
            let meta = std::fs::metadata(parent).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o700);
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn mls_db_path_rejects_absolute_tmp() {
        let (_dir, file_path) = restricted_tempdir();
        let data_dir = _dir.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        write_config_file(
            &file_path,
            &format!(
                r#"
[daemon]
data_dir = "{}"

[[bots]]
id = "mls-bot"
display_name = "Mls Bot"
npub = "npub1a"
signing = {{ backend = "nsec", nsec = "nsec1a" }}
mls_db_path = "/tmp/vector-mls.db"
"#,
                data_dir.display()
            ),
        );

        let err = DaemonConfig::load(&file_path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("must not be under /tmp or /dev/shm"),
            "expected /tmp rejection: {msg}"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn mls_db_path_rejects_relative_tmp() {
        let (_dir, file_path) = restricted_tempdir();
        let (_data_dir_tmp, data_dir) = tempdir_under(Path::new("/tmp"));
        write_config_file(
            &file_path,
            &format!(
                r#"
[daemon]
data_dir = "{}"

[[bots]]
id = "mls-bot"
display_name = "Mls Bot"
npub = "npub1a"
signing = {{ backend = "nsec", nsec = "nsec1a" }}
mls_db_path = "vector-mls.db"
"#,
                data_dir.display()
            ),
        );

        let err = DaemonConfig::load(&file_path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("must not be under /tmp or /dev/shm"),
            "expected /tmp rejection for relative path: {msg}"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn mls_db_path_rejects_relative_shm() {
        let (_dir, file_path) = restricted_tempdir();
        let (_data_dir_tmp, data_dir) = tempdir_under(Path::new("/dev/shm"));
        write_config_file(
            &file_path,
            &format!(
                r#"
[daemon]
data_dir = "{}"

[[bots]]
id = "mls-bot"
display_name = "Mls Bot"
npub = "npub1a"
signing = {{ backend = "nsec", nsec = "nsec1a" }}
mls_db_path = "vector-mls.db"
"#,
                data_dir.display()
            ),
        );

        let err = DaemonConfig::load(&file_path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("must not be under /tmp or /dev/shm"),
            "expected /dev/shm rejection for relative path: {msg}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn mls_db_path_rejects_symlink() {
        use std::os::unix::fs::symlink;

        let (dir, file_path) = restricted_tempdir();
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let real_dir = dir.path().join("real");
        std::fs::create_dir_all(&real_dir).unwrap();
        let link = data_dir.join("mls-bot");
        symlink(&real_dir, &link).unwrap();

        std::fs::write(
            &file_path,
            format!(
                r#"
[daemon]
data_dir = "{}"

[[bots]]
id = "mls-bot"
display_name = "Mls Bot"
npub = "npub1a"
signing = {{ backend = "nsec", nsec = "nsec1a" }}
mls_db_path = "vector-mls.db"
"#,
                data_dir.display()
            ),
        )
        .unwrap();

        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&file_path).unwrap().permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&file_path, perms).unwrap();
        }

        let err = DaemonConfig::load(&file_path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("must not contain symlinks"),
            "expected symlink rejection: {msg}"
        );
    }

    #[test]
    fn mls_db_path_rejects_escape_from_data_dir() {
        let (dir, file_path) = restricted_tempdir();
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(
            &file_path,
            format!(
                r#"
[daemon]
data_dir = "{}"

[[bots]]
id = "mls-bot"
display_name = "Mls Bot"
npub = "npub1a"
signing = {{ backend = "nsec", nsec = "nsec1a" }}
mls_db_path = "../vector-mls.db"
"#,
                data_dir.display()
            ),
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&file_path).unwrap().permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&file_path, perms).unwrap();
        }

        let err = DaemonConfig::load(&file_path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("escapes data directory"),
            "expected escape rejection: {msg}"
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

    #[tokio::test]
    async fn load_async_round_trips() -> Result<(), DaemonError> {
        let (_dir, _file, path) = write_config(
            r#"
[daemon]
data_dir = "/tmp/pacto"

[[bots]]
id = "echo-bot"
display_name = "Echo Bot"
npub = "npub1a"
signing = { backend = "nsec", nsec = "nsec1a" }
capabilities = ["ReadMessages", "SendMessages"]
"#,
        );

        let config = DaemonConfig::load_async(&path).await?;
        assert_eq!(config.bots[0].id, "echo-bot");
        assert_eq!(config.daemon.data_dir, "/tmp/pacto");
        Ok(())
    }

    #[tokio::test]
    async fn load_async_does_not_block_runtime() {
        let (_dir, _file, path) = write_config(
            r#"
[[bots]]
id = "echo-bot"
display_name = "Echo Bot"
npub = "npub1a"
signing = { backend = "nsec", nsec = "nsec1a" }
"#,
        );

        let mut interval = tokio::time::interval(Duration::from_millis(5));
        let ticks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ticks_clone = std::sync::Arc::clone(&ticks);
        let timer = tokio::spawn(async move {
            for _ in 0..50 {
                interval.tick().await;
                ticks_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });

        let path = path.clone();
        let load = tokio::spawn(async move { DaemonConfig::load_async(&path).await.unwrap() });

        // The runtime should remain responsive while the config is parsed on a
        // blocking thread.
        tokio::time::timeout(
            Duration::from_millis(5),
            tokio::time::sleep(Duration::from_millis(1)),
        )
        .await
        .unwrap();

        let config = load.await.unwrap();
        timer.await.unwrap();

        assert_eq!(config.bots[0].id, "echo-bot");
        let tick_count = ticks.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            tick_count >= 45,
            "runtime blocked during async config load; only {tick_count} timer ticks fired"
        );
    }

    #[test]
    fn validate_bot_id_accepts_safe_names() {
        assert!(validate_bot_id("echo-bot").is_ok());
        assert!(validate_bot_id("a").is_ok());
        assert!(validate_bot_id("bot_42").is_ok());
        assert!(validate_bot_id("a".repeat(64).as_str()).is_ok());
    }

    #[test]
    fn validate_bot_id_rejects_non_slug_characters() {
        assert!(validate_bot_id("EchoBot").is_err());
        assert!(validate_bot_id("-bot").is_err());
        assert!(validate_bot_id("bot-").is_err());
        assert!(validate_bot_id("bot.id").is_err());
        assert!(validate_bot_id("bot@id").is_err());
    }

    #[test]
    fn validate_bot_id_rejects_unsafe_names() {
        assert!(validate_bot_id("").is_err());
        assert!(validate_bot_id("..").is_err());
        assert!(validate_bot_id("foo/bar").is_err());
        assert!(validate_bot_id("foo\\bar").is_err());
        assert!(validate_bot_id("bot id").is_err());
        assert!(validate_bot_id("a".repeat(65).as_str()).is_err());
    }
}
