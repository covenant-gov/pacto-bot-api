//! Handler-local configuration for the governance snapshot bot.
//!
//! Configuration is loaded from environment variables first, with an optional
//! JSON config file overlay. The daemon does not own the snapshot cadence or
//! RPC endpoint (per KTD-9); they live here in the handler.

use alloy::primitives::Address;
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
    /// Load configuration from environment variables.
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
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_vars(std::env::vars().map(|(k, v)| (k, Some(v))))
    }

    /// Load configuration from any `Key -> Option<Value>` iterator.
    ///
    /// This is used by tests to avoid unsafe `std::env::set_var` calls.
    pub fn from_vars<I>(vars: I) -> Result<Self, ConfigError>
    where
        I: IntoIterator<Item = (String, Option<String>)>,
    {
        let lookup = VarLookup::new(vars);

        let rpc_url = lookup
            .required("PACTO_GOVERNANCE_RPC_URL")
            .ok_or_else(|| ConfigError::Missing("PACTO_GOVERNANCE_RPC_URL".into()))?;
        let bot_id = lookup
            .required("PACTO_GOVERNANCE_BOT_ID")
            .ok_or_else(|| ConfigError::Missing("PACTO_GOVERNANCE_BOT_ID".into()))?;
        let group_id = lookup
            .required("PACTO_GOVERNANCE_GROUP_ID")
            .ok_or_else(|| ConfigError::Missing("PACTO_GOVERNANCE_GROUP_ID".into()))?;

        let squad_index = lookup
            .parse_u64("PACTO_GOVERNANCE_SQUAD_INDEX")?
            .unwrap_or(0) as usize;
        let cadence_seconds = lookup
            .parse_u64("PACTO_GOVERNANCE_CADENCE_SECONDS")?
            .unwrap_or(86_400);

        let daemon_socket = lookup
            .opt("PACTO_GOVERNANCE_DAEMON_SOCKET")
            .map(PathBuf::from);
        let daemon_http = lookup.opt("PACTO_GOVERNANCE_DAEMON_HTTP");
        let http_secret = lookup.opt("PACTO_GOVERNANCE_HTTP_SECRET");

        let captain = lookup
            .parse_address("PACTO_GOVERNANCE_CAPTAIN")?
            .unwrap_or(Address::ZERO);
        let crew_candidates = lookup.parse_address_list("PACTO_GOVERNANCE_CREW_CANDIDATES")?;
        let proposer_candidates =
            lookup.parse_address_list("PACTO_GOVERNANCE_PROPOSER_CANDIDATES")?;

        let known_tokens = Vec::new();

        if daemon_socket.is_none() && daemon_http.is_none() {
            return Err(ConfigError::Missing(
                "one of PACTO_GOVERNANCE_DAEMON_SOCKET or PACTO_GOVERNANCE_DAEMON_HTTP".into(),
            ));
        }

        if daemon_http.is_some() && http_secret.is_none() {
            return Err(ConfigError::Missing(
                "PACTO_GOVERNANCE_HTTP_SECRET is required when using HTTP transport".into(),
            ));
        }

        Ok(Self {
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
            known_tokens,
        })
    }

    /// Override known tokens from a configured list (e.g. parsed from a config file).
    pub fn with_known_tokens(mut self, tokens: Vec<TokenInfo>) -> Self {
        self.known_tokens = tokens;
        self
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

    fn required(&self, key: &str) -> Option<String> {
        self.vars.get(key).cloned()
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

    fn parse_address(&self, key: &str) -> Result<Option<Address>, ConfigError> {
        match self.vars.get(key) {
            Some(s) => s
                .parse::<Address>()
                .map(Some)
                .map_err(|e| ConfigError::Invalid(format!("{key}: {e}"))),
            None => Ok(None),
        }
    }

    fn parse_address_list(&self, key: &str) -> Result<Vec<Address>, ConfigError> {
        match self.vars.get(key) {
            Some(s) if s.trim().is_empty() => Ok(Vec::new()),
            Some(s) => s
                .split(',')
                .map(|part| part.trim().parse::<Address>())
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| ConfigError::Invalid(format!("{key}: {e}"))),
            None => Ok(Vec::new()),
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
}
