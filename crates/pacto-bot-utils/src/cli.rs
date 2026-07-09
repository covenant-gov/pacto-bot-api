use clap::Parser;
use nostr::Keys;
use secrecy::{ExposeSecret, SecretString};
use std::fs;
use std::path::PathBuf;
use thiserror::Error;

/// Create or re-open an MLS group and invite a bot.
#[derive(Debug, Parser)]
#[command(
    name = "create-mls-group",
    about = "Create or re-open an MLS group and invite a bot.",
    long_about = None
)]
pub struct Cli {
    /// The public key of the bot to invite.
    #[arg(long, value_name = "NPUB")]
    pub bot_npub: nostr::PublicKey,

    /// The human-readable group name used as the group idempotency key.
    #[arg(long, value_name = "NAME")]
    pub group_name: String,

    /// The Nostr relay URL.
    #[arg(long, value_name = "URL", default_value = "ws://nostr-relay:8080")]
    pub relay: String,

    /// Path to the JSON metadata state file.
    #[arg(
        long,
        value_name = "PATH",
        default_value = "/data/deployments/31337/.mls-groups.json"
    )]
    pub state_file: PathBuf,

    /// Path to the persistent SQLite database used by the MLS engine.
    #[arg(
        long,
        value_name = "PATH",
        default_value = "/data/deployments/31337/.mls-creator.db"
    )]
    pub mls_db: PathBuf,

    /// Creator nsec (dev-only; exposes secret in process list and shell history).
    #[arg(long, value_name = "NSEC", env = "PACTO_MLS_CREATOR_NSEC")]
    pub nsec: Option<String>,

    /// Path to a file containing the creator nsec.
    #[arg(long, value_name = "PATH")]
    pub nsec_file: Option<PathBuf>,

    /// How long to wait for the bot's KeyPackage before giving up.
    #[arg(long, value_name = "SECONDS", default_value = "30")]
    pub key_package_timeout: u64,
}

#[derive(Debug, Error)]
pub enum CliError {
    #[error("no creator nsec provided; use --nsec, PACTO_MLS_CREATOR_NSEC, or --nsec-file")]
    MissingNsec,
    #[error(
        "multiple creator nsec sources provided; use only one of --nsec, PACTO_MLS_CREATOR_NSEC, or --nsec-file"
    )]
    MultipleNsecSources,
    #[error("failed to read nsec file: {0}")]
    ReadNsecFile(#[from] std::io::Error),
    #[error("invalid creator nsec: {0}")]
    InvalidNsec(String),
}

impl Cli {
    /// Load the creator nsec from the configured source and derive signing keys.
    ///
    /// The plain-text input is zeroized as soon as it is converted into a
    /// [`Keys`]; the returned secret is held in a [`SecretString`] until the
    /// caller drops it.
    pub fn load_creator_keys(&self) -> Result<Keys, CliError> {
        let nsec = self.resolve_nsec()?;
        let secret = SecretString::from(nsec);
        Keys::parse(secret.expose_secret()).map_err(|e| CliError::InvalidNsec(e.to_string()))
    }

    fn resolve_nsec(&self) -> Result<String, CliError> {
        let has_flag = self.nsec.is_some();
        let has_env = std::env::var("PACTO_MLS_CREATOR_NSEC").is_ok();
        let has_file = self.nsec_file.is_some();

        let sources = [has_flag, has_env, has_file].iter().filter(|&&b| b).count();
        if sources == 0 {
            return Err(CliError::MissingNsec);
        }
        if sources > 1 {
            return Err(CliError::MultipleNsecSources);
        }

        if let Some(path) = &self.nsec_file {
            let mut content = fs::read_to_string(path)?;
            content.retain(|c| !c.is_whitespace());
            return Ok(content);
        }

        // The env or flag value is already present in `self.nsec`; clone it.
        self.nsec.clone().ok_or(CliError::MissingNsec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::ToBech32;
    use std::io::Write;

    fn dummy_public_key() -> nostr::PublicKey {
        nostr::PublicKey::from_hex(
            "0000000000000000000000000000000000000000000000000000000000000001",
        )
        .unwrap()
    }

    #[test]
    fn nsec_file_round_trip() {
        let keys = Keys::generate();
        let nsec = keys.secret_key().to_bech32().unwrap();

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "{}", nsec).unwrap();
        let cli = Cli::parse_from([
            "create-mls-group",
            "--bot-npub",
            &dummy_public_key().to_bech32().unwrap(),
            "--group-name",
            "test",
            "--nsec-file",
            tmp.path().to_str().unwrap(),
        ]);
        let loaded = cli.load_creator_keys().unwrap();
        assert_eq!(loaded.public_key(), keys.public_key());
    }
}
