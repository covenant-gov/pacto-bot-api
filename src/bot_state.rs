use crate::config::BotConfig;
use crate::diagnostics::BotHealth;
use crate::errors::DaemonError;
use crate::mls::MlsEngineHandle;

use crate::signer::SignerBackend;
#[cfg(test)]
use secrecy::SecretString;

/// Runtime state for a single configured bot identity.
#[derive(Debug)]
pub struct BotState {
    pub config: BotConfig,
    pub signer: SignerBackend,
    /// Optional MLS engine for bots that participate in Pacto Squad channels.
    pub mls: Option<MlsEngineHandle>,
    /// Active relay subscription IDs owned by this bot.
    subscriptions: Vec<String>,
}

impl BotState {
    pub fn new(config: BotConfig) -> Result<Self, DaemonError> {
        let signer = SignerBackend::from_config(&config.signing, &config.npub)?;
        Ok(Self {
            config,
            signer,
            mls: None,
            subscriptions: Vec::new(),
        })
    }

    /// Create a bot state with an MLS engine backing the given database path.
    ///
    /// This is used for bots configured to send group messages. The caller is
    /// responsible for creating the parent directory with `0o700` permissions.
    pub fn new_with_mls(
        config: BotConfig,
        mls_db_path: impl AsRef<std::path::Path>,
    ) -> Result<Self, DaemonError> {
        let signer = SignerBackend::from_config(&config.signing, &config.npub)?;
        let mls = Some(MlsEngineHandle::new_persistent(mls_db_path)?);
        Ok(Self {
            config,
            signer,
            mls,
            subscriptions: Vec::new(),
        })
    }

    /// The bot's Nostr public key (npub) as configured.
    pub fn npub(&self) -> &str {
        &self.config.npub
    }

    /// The daemon-local bot identifier.
    pub fn bot_id(&self) -> &str {
        &self.config.id
    }

    /// Track an active relay subscription ID for this bot.
    pub fn add_subscription(&mut self, sub_id: impl Into<String>) {
        self.subscriptions.push(sub_id.into());
    }

    /// Remove and return all tracked subscription IDs, leaving the list empty.
    pub fn clear_subscriptions(&mut self) -> Vec<String> {
        std::mem::take(&mut self.subscriptions)
    }

    /// Produce a non-sensitive health snapshot for this bot identity.
    pub fn to_bot_health(&self) -> BotHealth {
        let bunker_connected = matches!(
            self.signer,
            SignerBackend::BunkerLocal(_) | SignerBackend::BunkerRemote(_)
        );
        BotHealth {
            bot_id: self.config.id.clone(),
            npub: self.config.npub.clone(),
            relay_count: self.config.relays.len() as u64,
            relays: self.config.relays.clone(),
            bunker_connected,
            signer_backend: self.config.signing.backend_label().to_string(),
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BotConfig, SigningConfig};
    use nostr::ToBech32;

    fn test_bot_config() -> BotConfig {
        let keys = nostr::Keys::generate();
        let nsec = keys.secret_key().to_bech32().unwrap();
        let npub = keys.public_key().to_bech32().unwrap();
        BotConfig {
            id: "test-bot".into(),
            npub,
            signing: SigningConfig::Nsec {
                nsec: SecretString::new(nsec.into()),
            },
            relays: vec![],
            capabilities: vec![],
            ..Default::default()
        }
    }

    #[test]
    fn npub_and_bot_id_helpers() {
        let config = test_bot_config();
        let expected_npub = config.npub.clone();
        let expected_id = config.id.clone();
        let bot = BotState::new(config).unwrap();
        assert_eq!(bot.npub(), expected_npub);
        assert_eq!(bot.bot_id(), expected_id);
    }

    #[test]
    fn add_and_clear_subscriptions() {
        let config = test_bot_config();
        let mut bot = BotState::new(config).unwrap();

        bot.add_subscription("sub-1");
        bot.add_subscription("sub-2".to_string());

        let subs = bot.clear_subscriptions();
        assert_eq!(subs, vec!["sub-1", "sub-2"]);
        assert!(bot.clear_subscriptions().is_empty());
    }

    #[test]
    fn new_without_mls_sets_none() {
        let config = test_bot_config();
        let state = BotState::new(config).expect("new");
        assert!(state.mls.is_none());
    }

    #[test]
    fn new_with_mls_creates_engine_and_permissions() {
        let config = test_bot_config();
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("vector-mls.db");

        let state = BotState::new_with_mls(config, &db_path).expect("new_with_mls");
        assert!(state.mls.is_some());
        assert!(db_path.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(&db_path).expect("db metadata");
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);

            let wal = db_path.with_extension("db-wal");
            let shm = db_path.with_extension("db-shm");
            assert!(wal.exists(), "wal sidecar should exist");
            assert!(shm.exists(), "shm sidecar should exist");
            assert_eq!(
                std::fs::metadata(wal)
                    .expect("wal metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(shm)
                    .expect("shm metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn new_with_mls_parent_dir_is_0700() {
        let config = test_bot_config();
        let temp = tempfile::tempdir().expect("tempdir");
        let parent = temp.path().join("bot-id");
        std::fs::create_dir_all(&parent).expect("create parent");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700))
                .expect("chmod");
        }

        let db_path = parent.join("vector-mls.db");
        let state = BotState::new_with_mls(config, &db_path).expect("new_with_mls");
        assert!(state.mls.is_some());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(&parent).expect("parent metadata");
            assert_eq!(meta.permissions().mode() & 0o777, 0o700);
        }
    }
}
