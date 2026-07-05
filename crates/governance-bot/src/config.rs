//! Handler-local configuration for the governance snapshot bot.
//!
//! Configuration starts from environment variables. When `PACTO_GOVERNANCE_CONFIG_FILE`
//! is set, the file it points to is read as JSON and its values are merged on top of
//! the environment values. The daemon does not own the snapshot cadence or RPC endpoint
//! (per KTD-9); they live here in the handler.

use alloy::primitives::Address;
use serde::Deserialize;
use std::path::PathBuf;

use crate::evm::reader::{GovernanceError, TokenInfo};

/// Runtime configuration for the snapshot bot.
#[derive(Debug, Clone)]
pub struct BotConfig {
    /// JSON-RPC endpoint for the EVM chain (e.g. Sepolia or anvil).
    pub rpc_url: String,
    /// Registry index of the squad to snapshot.
    pub squad_index: usize,
    /// Seconds between autonomous snapshot posts.
    pub cadence_seconds: u64,
    /// Bot identity configured in `pacto-bot-api.toml`.
    pub bot_id: String,
    /// Hex-encoded MLS group ID for the Squad channel.
    pub group_id: String,
    /// Path to the daemon Unix socket. Takes precedence over HTTP when set.
    pub daemon_socket: Option<PathBuf>,
    /// Daemon HTTP endpoint, e.g. `http://127.0.0.1:9800`.
    pub daemon_http: Option<String>,
    /// HTTP secret token required for the HTTP transport.
    pub http_secret: Option<String>,
    /// Address of the current captain (used for crew state reads).
    pub captain: Address,
    /// Candidate addresses to check for pending crew additions/removals.
    pub crew_candidates: Vec<Address>,
    /// Candidate addresses to check for open proposals.
    pub proposer_candidates: Vec<Address>,
    /// ERC-20 tokens whose balances should appear in the treasury summary.
    pub known_tokens: Vec<TokenInfo>,
}

impl BotConfig {
    /// Load configuration from environment variables, with an optional JSON file overlay.
    ///
    /// If `PACTO_GOVERNANCE_CONFIG_FILE` is set, it is read as JSON and merged over the
    /// environment values. File values take precedence.
    ///
    /// Required:
    /// - `PACTO_GOVERNANCE_RPC_URL`
    /// - `PACTO_GOVERNANCE_BOT_ID`
    /// - `PACTO_GOVERNANCE_GROUP_ID`
    ///
    /// Optional:
    /// - `PACTO_GOVERNANCE_SQUAD_INDEX` (default 0)
    /// - `PACTO_GOVERNANCE_CADENCE_SECONDS` (default 86400)
    /// - `PACTO_GOVERNANCE_DAEMON_SOCKET`
    /// - `PACTO_GOVERNANCE_DAEMON_HTTP`
    /// - `PACTO_GOVERNANCE_HTTP_SECRET`
    /// - `PACTO_GOVERNANCE_CAPTAIN`
    /// - `PACTO_GOVERNANCE_CREW_CANDIDATES` (comma-separated)
    /// - `PACTO_GOVERNANCE_PROPOSER_CANDIDATES` (comma-separated)
    /// - `PACTO_GOVERNANCE_CONFIG_FILE`
    pub fn from_env() -> Result<Self, ConfigError> {
        let file_path = std::env::var_os("PACTO_GOVERNANCE_CONFIG_FILE").map(PathBuf::from);
        let lookup = VarLookup::new(std::env::vars().map(|(k, v)| (k, Some(v))));
        let mut raw = RawConfig::from_lookup(lookup)?;
        if let Some(path) = file_path {
            let content = std::fs::read_to_string(&path)
                .map_err(|e| ConfigError::Invalid(format!("PACTO_GOVERNANCE_CONFIG_FILE: {e}")))?;
            let file: FileConfig = serde_json::from_str(&content)
                .map_err(|e| ConfigError::Invalid(format!("PACTO_GOVERNANCE_CONFIG_FILE: {e}")))?;
            raw.merge_file(file)?;
        }
        raw.finish()
    }

    /// Load configuration from any `Key -> Option<Value>` iterator.
    ///
    /// This is used by tests to avoid unsafe `std::env::set_var` calls.
    pub fn from_vars<I>(vars: I) -> Result<Self, ConfigError>
    where
        I: IntoIterator<Item = (String, Option<String>)>,
    {
        RawConfig::from_lookup(VarLookup::new(vars))?.finish()
    }

    /// Override known tokens from a configured list (e.g. parsed from a config file).
    pub fn with_known_tokens(mut self, tokens: Vec<TokenInfo>) -> Self {
        self.known_tokens = tokens;
        self
    }
}

/// Intermediate representation built from environment variables and an optional file overlay.
#[derive(Debug, Default)]
struct RawConfig {
    rpc_url: Option<String>,
    squad_index: Option<u64>,
    cadence_seconds: Option<u64>,
    bot_id: Option<String>,
    group_id: Option<String>,
    daemon_socket: Option<String>,
    daemon_http: Option<String>,
    http_secret: Option<String>,
    captain: Option<String>,
    crew_candidates: Option<String>,
    proposer_candidates: Option<String>,
    known_tokens: Vec<TokenInfo>,
}

impl RawConfig {
    fn from_lookup(lookup: VarLookup) -> Result<Self, ConfigError> {
        Ok(Self {
            rpc_url: lookup.opt("PACTO_GOVERNANCE_RPC_URL"),
            bot_id: lookup.opt("PACTO_GOVERNANCE_BOT_ID"),
            group_id: lookup.opt("PACTO_GOVERNANCE_GROUP_ID"),
            squad_index: lookup.parse_u64("PACTO_GOVERNANCE_SQUAD_INDEX")?,
            cadence_seconds: lookup.parse_u64("PACTO_GOVERNANCE_CADENCE_SECONDS")?,
            daemon_socket: lookup.opt("PACTO_GOVERNANCE_DAEMON_SOCKET"),
            daemon_http: lookup.opt("PACTO_GOVERNANCE_DAEMON_HTTP"),
            http_secret: lookup.opt("PACTO_GOVERNANCE_HTTP_SECRET"),
            captain: lookup.opt("PACTO_GOVERNANCE_CAPTAIN"),
            crew_candidates: lookup.opt("PACTO_GOVERNANCE_CREW_CANDIDATES"),
            proposer_candidates: lookup.opt("PACTO_GOVERNANCE_PROPOSER_CANDIDATES"),
            ..Default::default()
        })
    }

    fn merge_file(&mut self, file: FileConfig) -> Result<(), ConfigError> {
        if let Some(v) = file.rpc_url {
            self.rpc_url = Some(v);
        }
        if let Some(v) = file.squad_index {
            self.squad_index = Some(v);
        }
        if let Some(v) = file.cadence_seconds {
            self.cadence_seconds = Some(v);
        }
        if let Some(v) = file.bot_id {
            self.bot_id = Some(v);
        }
        if let Some(v) = file.group_id {
            self.group_id = Some(v);
        }
        if let Some(v) = file.daemon_socket {
            self.daemon_socket = Some(v);
        }
        if let Some(v) = file.daemon_http {
            self.daemon_http = Some(v);
        }
        if let Some(v) = file.http_secret {
            self.http_secret = Some(v);
        }
        if let Some(v) = file.captain {
            self.captain = Some(v);
        }
        if let Some(v) = file.crew_candidates {
            self.crew_candidates = Some(v.join(","));
        }
        if let Some(v) = file.proposer_candidates {
            self.proposer_candidates = Some(v.join(","));
        }
        if !file.known_tokens.is_empty() {
            self.known_tokens =
                file.known_tokens
                    .into_iter()
                    .map(|t| -> Result<TokenInfo, ConfigError> {
                        let address = t.address.parse::<Address>().map_err(|e| {
                            ConfigError::Invalid(format!("known_tokens.address: {e}"))
                        })?;
                        Ok(TokenInfo {
                            address,
                            symbol: t.symbol,
                            decimals: t.decimals,
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
        }
        Ok(())
    }

    fn finish(self) -> Result<BotConfig, ConfigError> {
        let rpc_url = self
            .rpc_url
            .ok_or_else(|| ConfigError::Missing("PACTO_GOVERNANCE_RPC_URL".into()))?;
        let bot_id = self
            .bot_id
            .ok_or_else(|| ConfigError::Missing("PACTO_GOVERNANCE_BOT_ID".into()))?;
        let group_id = self
            .group_id
            .ok_or_else(|| ConfigError::Missing("PACTO_GOVERNANCE_GROUP_ID".into()))?;
        let squad_index = self.squad_index.unwrap_or(0) as usize;
        let cadence_seconds = self.cadence_seconds.unwrap_or(86_400);
        let daemon_socket = self.daemon_socket.map(PathBuf::from);
        let daemon_http = self.daemon_http;
        let http_secret = self.http_secret;
        let captain = match self.captain {
            Some(s) => s
                .parse::<Address>()
                .map_err(|e| ConfigError::Invalid(format!("PACTO_GOVERNANCE_CAPTAIN: {e}")))?,
            None => Address::ZERO,
        };
        let crew_candidates =
            parse_address_list(&self.crew_candidates, "PACTO_GOVERNANCE_CREW_CANDIDATES")?;
        let proposer_candidates = parse_address_list(
            &self.proposer_candidates,
            "PACTO_GOVERNANCE_PROPOSER_CANDIDATES",
        )?;

        if daemon_socket.is_none() && daemon_http.is_none() {
            return Err(ConfigError::Missing(
                "one of PACTO_GOVERNANCE_DAEMON_SOCKET or PACTO_GOVERNANCE_DAEMON_HTTP".into(),
            ));
        }

        if daemon_socket.is_none() && daemon_http.is_some() && http_secret.is_none() {
            return Err(ConfigError::Missing(
                "PACTO_GOVERNANCE_HTTP_SECRET is required when using HTTP transport".into(),
            ));
        }

        Ok(BotConfig {
            rpc_url,
            squad_index,
            cadence_seconds,
            bot_id,
            group_id,
            daemon_socket,
            daemon_http,
            http_secret,
            captain,
            crew_candidates,
            proposer_candidates,
            known_tokens: self.known_tokens,
        })
    }
}

/// JSON file representation for optional configuration overlay.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "snake_case")]
struct FileConfig {
    rpc_url: Option<String>,
    squad_index: Option<u64>,
    cadence_seconds: Option<u64>,
    bot_id: Option<String>,
    group_id: Option<String>,
    daemon_socket: Option<String>,
    daemon_http: Option<String>,
    http_secret: Option<String>,
    captain: Option<String>,
    crew_candidates: Option<Vec<String>>,
    proposer_candidates: Option<Vec<String>>,
    known_tokens: Vec<KnownTokenFile>,
}

/// Token entry as it appears in the JSON overlay.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct KnownTokenFile {
    address: String,
    symbol: String,
    decimals: u8,
}

/// Parse a comma-separated address list or an empty value into a vector of addresses.
fn parse_address_list(value: &Option<String>, key: &str) -> Result<Vec<Address>, ConfigError> {
    match value {
        None => Ok(Vec::new()),
        Some(s) if s.trim().is_empty() => Ok(Vec::new()),
        Some(s) => s
            .split(',')
            .map(|part| part.trim().parse::<Address>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ConfigError::Invalid(format!("{key}: {e}"))),
    }
}

/// Lightweight helper for reading typed values from a synthetic variable map.
struct VarLookup {
    vars: std::collections::HashMap<String, String>,
}

impl VarLookup {
    fn new<I>(vars: I) -> Self
    where
        I: IntoIterator<Item = (String, Option<String>)>,
    {
        let mut map = std::collections::HashMap::new();
        for (k, v) in vars {
            if let Some(v) = v {
                map.insert(k, v);
            }
        }
        Self { vars: map }
    }

    fn opt(&self, key: &str) -> Option<String> {
        self.vars.get(key).cloned()
    }

    fn parse_u64(&self, key: &str) -> Result<Option<u64>, ConfigError> {
        match self.vars.get(key) {
            Some(s) => s
                .parse::<u64>()
                .map(Some)
                .map_err(|e| ConfigError::Invalid(format!("{key}: {e}"))),
            None => Ok(None),
        }
    }
}

/// Errors that can occur while loading configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A required environment variable is missing.
    #[error("missing required config: {0}")]
    Missing(String),
    /// A value could not be parsed.
    #[error("invalid config value: {0}")]
    Invalid(String),
}

impl From<GovernanceError> for ConfigError {
    fn from(err: GovernanceError) -> Self {
        ConfigError::Invalid(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, Option<String>)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), Some(v.to_string())))
            .collect()
    }

    #[test]
    fn valid_env_config_loads() {
        let addr1 = "0x0000000000000000000000000000000000000001"
            .parse::<Address>()
            .unwrap();
        let addr2 = "0x0000000000000000000000000000000000000002"
            .parse::<Address>()
            .unwrap();

        let cfg = BotConfig::from_vars(vars(&[
            ("PACTO_GOVERNANCE_RPC_URL", "http://localhost:8545"),
            ("PACTO_GOVERNANCE_BOT_ID", "gov-bot"),
            ("PACTO_GOVERNANCE_GROUP_ID", "deadbeef"),
            ("PACTO_GOVERNANCE_DAEMON_SOCKET", "/tmp/pacto.sock"),
            ("PACTO_GOVERNANCE_SQUAD_INDEX", "2"),
            ("PACTO_GOVERNANCE_CADENCE_SECONDS", "3600"),
            ("PACTO_GOVERNANCE_CAPTAIN", "0x0000000000000000000000000000000000000001"),
            (
                "PACTO_GOVERNANCE_CREW_CANDIDATES",
                "0x0000000000000000000000000000000000000001,0x0000000000000000000000000000000000000002",
            ),
            ("PACTO_GOVERNANCE_PROPOSER_CANDIDATES", "0x0000000000000000000000000000000000000002"),
        ]))
        .unwrap();

        assert_eq!(cfg.rpc_url, "http://localhost:8545");
        assert_eq!(cfg.bot_id, "gov-bot");
        assert_eq!(cfg.group_id, "deadbeef");
        assert_eq!(cfg.squad_index, 2);
        assert_eq!(cfg.cadence_seconds, 3600);
        assert_eq!(cfg.captain, addr1);
        assert_eq!(cfg.crew_candidates, vec![addr1, addr2]);
        assert_eq!(cfg.proposer_candidates, vec![addr2]);
        assert_eq!(cfg.daemon_socket, Some(PathBuf::from("/tmp/pacto.sock")));
    }

    #[test]
    fn missing_rpc_url_errors() {
        let err = BotConfig::from_vars(vars(&[
            ("PACTO_GOVERNANCE_BOT_ID", "x"),
            ("PACTO_GOVERNANCE_GROUP_ID", "x"),
            ("PACTO_GOVERNANCE_DAEMON_SOCKET", "/tmp/x.sock"),
        ]))
        .expect_err("should fail");
        assert!(matches!(
            err,
            ConfigError::Missing(s) if s == "PACTO_GOVERNANCE_RPC_URL"
        ));
    }

    #[test]
    fn http_without_secret_errors() {
        let err = BotConfig::from_vars(vars(&[
            ("PACTO_GOVERNANCE_RPC_URL", "http://x"),
            ("PACTO_GOVERNANCE_BOT_ID", "x"),
            ("PACTO_GOVERNANCE_GROUP_ID", "x"),
            ("PACTO_GOVERNANCE_DAEMON_HTTP", "http://x"),
        ]))
        .expect_err("should fail");
        assert!(matches!(
            err,
            ConfigError::Missing(s) if s.contains("HTTP_SECRET")
        ));
    }

    #[test]
    fn both_transports_without_secret_succeeds() {
        let cfg = BotConfig::from_vars(vars(&[
            ("PACTO_GOVERNANCE_RPC_URL", "http://x"),
            ("PACTO_GOVERNANCE_BOT_ID", "x"),
            ("PACTO_GOVERNANCE_GROUP_ID", "x"),
            ("PACTO_GOVERNANCE_DAEMON_SOCKET", "/tmp/pacto.sock"),
            ("PACTO_GOVERNANCE_DAEMON_HTTP", "http://x"),
        ]))
        .unwrap();

        assert_eq!(cfg.daemon_socket, Some(PathBuf::from("/tmp/pacto.sock")));
        assert_eq!(cfg.daemon_http, Some("http://x".to_string()));
        assert!(cfg.http_secret.is_none());
    }

    #[test]
    fn defaults_are_applied_when_optional_vars_missing() {
        let cfg = BotConfig::from_vars(vars(&[
            ("PACTO_GOVERNANCE_RPC_URL", "http://x"),
            ("PACTO_GOVERNANCE_BOT_ID", "x"),
            ("PACTO_GOVERNANCE_GROUP_ID", "x"),
            ("PACTO_GOVERNANCE_DAEMON_SOCKET", "/tmp/x.sock"),
        ]))
        .unwrap();
        assert_eq!(cfg.squad_index, 0);
        assert_eq!(cfg.cadence_seconds, 86_400);
        assert_eq!(cfg.captain, Address::ZERO);
        assert!(cfg.crew_candidates.is_empty());
    }

    #[test]
    fn no_transport_errors() {
        let err = BotConfig::from_vars(vars(&[
            ("PACTO_GOVERNANCE_RPC_URL", "http://x"),
            ("PACTO_GOVERNANCE_BOT_ID", "x"),
            ("PACTO_GOVERNANCE_GROUP_ID", "x"),
        ]))
        .expect_err("should fail");
        assert!(matches!(
            err,
            ConfigError::Missing(s) if s.contains("DAEMON_SOCKET")
        ));
    }

    #[test]
    fn json_file_overlay_overrides_env_values() {
        let captain = "0x0000000000000000000000000000000000000001"
            .parse::<Address>()
            .unwrap();
        let token_addr = "0x0000000000000000000000000000000000000003"
            .parse::<Address>()
            .unwrap();
        let path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
        std::fs::write(
            &path,
            r#"{
                "cadence_seconds": 1800,
                "http_secret": "file-secret",
                "captain": "0x0000000000000000000000000000000000000001",
                "known_tokens": [
                    {
                        "address": "0x0000000000000000000000000000000000000003",
                        "symbol": "TST",
                        "decimals": 18
                    }
                ]
            }"#,
        )
        .unwrap();

        let mut raw = RawConfig::from_lookup(VarLookup::new(vars(&[
            ("PACTO_GOVERNANCE_RPC_URL", "http://localhost:8545"),
            ("PACTO_GOVERNANCE_BOT_ID", "gov-bot"),
            ("PACTO_GOVERNANCE_GROUP_ID", "deadbeef"),
            ("PACTO_GOVERNANCE_DAEMON_SOCKET", "/tmp/pacto.sock"),
            ("PACTO_GOVERNANCE_CADENCE_SECONDS", "3600"),
            ("PACTO_GOVERNANCE_HTTP_SECRET", "env-secret"),
        ])))
        .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let file: FileConfig = serde_json::from_str(&content).unwrap();
        raw.merge_file(file).unwrap();
        let cfg = raw.finish().unwrap();

        assert_eq!(cfg.cadence_seconds, 1800);
        assert_eq!(cfg.http_secret, Some("file-secret".to_string()));
        assert_eq!(cfg.captain, captain);
        assert_eq!(cfg.known_tokens.len(), 1);
        assert_eq!(cfg.known_tokens[0].address, token_addr);
        assert_eq!(cfg.known_tokens[0].symbol, "TST");
        assert_eq!(cfg.known_tokens[0].decimals, 18);
    }

    #[test]
    fn json_file_overlay_provides_missing_required_fields() {
        let path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
        std::fs::write(
            &path,
            r#"{
                "rpc_url": "http://localhost:8545",
                "bot_id": "gov-bot",
                "group_id": "deadbeef",
                "daemon_socket": "/tmp/pacto.sock"
            }"#,
        )
        .unwrap();

        let mut raw = RawConfig::default();
        let content = std::fs::read_to_string(&path).unwrap();
        let file: FileConfig = serde_json::from_str(&content).unwrap();
        raw.merge_file(file).unwrap();
        let cfg = raw.finish().unwrap();

        assert_eq!(cfg.rpc_url, "http://localhost:8545");
        assert_eq!(cfg.bot_id, "gov-bot");
        assert_eq!(cfg.group_id, "deadbeef");
        assert_eq!(cfg.daemon_socket, Some(PathBuf::from("/tmp/pacto.sock")));
    }

    #[test]
    fn json_file_overlay_invalid_address_errors() {
        let path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
        std::fs::write(&path, r#"{"captain":"not-an-address"}"#).unwrap();

        let mut raw = RawConfig::from_lookup(VarLookup::new(vars(&[
            ("PACTO_GOVERNANCE_RPC_URL", "http://x"),
            ("PACTO_GOVERNANCE_BOT_ID", "x"),
            ("PACTO_GOVERNANCE_GROUP_ID", "x"),
            ("PACTO_GOVERNANCE_DAEMON_SOCKET", "/tmp/x.sock"),
        ])))
        .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let file: FileConfig = serde_json::from_str(&content).unwrap();
        raw.merge_file(file).unwrap();
        let err = raw.finish().expect_err("should fail");
        assert!(matches!(err, ConfigError::Invalid(s) if s.contains("CAPTAIN")));
    }

    #[test]
    fn json_file_overlay_list_fields_merge() {
        let addr1 = "0x0000000000000000000000000000000000000001"
            .parse::<Address>()
            .unwrap();
        let addr2 = "0x0000000000000000000000000000000000000002"
            .parse::<Address>()
            .unwrap();
        let path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
        std::fs::write(
            &path,
            r#"{
                "crew_candidates": [
                    "0x0000000000000000000000000000000000000001",
                    "0x0000000000000000000000000000000000000002"
                ]
            }"#,
        )
        .unwrap();

        let mut raw = RawConfig::from_lookup(VarLookup::new(vars(&[
            ("PACTO_GOVERNANCE_RPC_URL", "http://x"),
            ("PACTO_GOVERNANCE_BOT_ID", "x"),
            ("PACTO_GOVERNANCE_GROUP_ID", "x"),
            ("PACTO_GOVERNANCE_DAEMON_SOCKET", "/tmp/x.sock"),
        ])))
        .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let file: FileConfig = serde_json::from_str(&content).unwrap();
        raw.merge_file(file).unwrap();
        let cfg = raw.finish().unwrap();

        assert_eq!(cfg.crew_candidates, vec![addr1, addr2]);
    }
}
