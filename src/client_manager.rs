use crate::bot_state::BotState;
use crate::config::DaemonConfig;
use crate::db::Db;
use crate::diagnostics::{BotHealth, Diagnostics};
use crate::errors::DaemonError;

use crate::handlers::HandlerRegistry;
use crate::nostr::NostrClient;
use crate::signer::Signer;
use nostr::{PublicKey, Timestamp};
#[cfg(test)]
use secrecy::SecretString;
use std::collections::HashMap;
use tracing::warn;

/// Manages multiple bot identities and provides npub/bot_id lookups.
#[derive(Debug)]
pub struct ClientManager {
    /// Bots keyed by their parsed Nostr public key.
    bots: HashMap<PublicKey, BotState>,
    /// Bidirectional lookup from daemon-local `bot_id` to public key.
    /// The reverse direction is satisfied by `BotState::bot_id`.
    bot_id_to_pubkey: HashMap<String, PublicKey>,
    pub nostr_client: NostrClient,
    pub handler_registry: HandlerRegistry,
}

impl ClientManager {
    pub async fn new(config: DaemonConfig, nostr_client: NostrClient) -> Result<Self, DaemonError> {
        let mut bots = HashMap::with_capacity(config.bots.len());
        let mut bot_id_to_pubkey = HashMap::with_capacity(config.bots.len());

        for bot_config in config.bots {
            let bot_id = bot_config.id.clone();
            if bot_id_to_pubkey.contains_key(&bot_id) {
                return Err(DaemonError::Config(format!("duplicate bot_id: {bot_id}")));
            }

            let bot_state = BotState::new(bot_config)?;
            // Live verification for bunker backends; local keys are checked
            // synchronously during BotState construction.
            bot_state.signer.verify_bunker_public_key().await?;
            let pubkey = bot_state.signer.public_key();

            bots.insert(pubkey, bot_state);
            bot_id_to_pubkey.insert(bot_id, pubkey);
        }

        Ok(Self {
            bots,
            bot_id_to_pubkey,
            nostr_client,
            handler_registry: HandlerRegistry::new(),
        })
    }

    /// Iterate over all bots keyed by public key.
    pub fn bots(&self) -> impl Iterator<Item = (&PublicKey, &BotState)> {
        self.bots.iter()
    }

    /// Iterate over all daemon-local bot identifiers.
    pub fn bot_ids(&self) -> impl Iterator<Item = &str> {
        self.bot_id_to_pubkey.keys().map(String::as_str)
    }

    /// Look up a bot by its parsed public key.
    pub fn get_bot(&self, npub: &PublicKey) -> Option<&BotState> {
        self.bots.get(npub)
    }

    /// Look up a bot by its daemon-local identifier.
    pub fn get_bot_by_id(&self, bot_id: &str) -> Option<&BotState> {
        self.bot_id_to_pubkey
            .get(bot_id)
            .and_then(|pubkey| self.bots.get(pubkey))
    }

    /// Mutable lookup by public key.
    pub fn get_bot_mut(&mut self, npub: &PublicKey) -> Option<&mut BotState> {
        self.bots.get_mut(npub)
    }

    /// Mutable lookup by daemon-local identifier.
    pub fn get_bot_by_id_mut(&mut self, bot_id: &str) -> Option<&mut BotState> {
        self.bot_id_to_pubkey
            .get(bot_id)
            .copied()
            .and_then(|pubkey| self.bots.get_mut(&pubkey))
    }

    /// Subscribe each bot to its gift-wrap filter, using the persisted cursor
    /// as the `since` value so events older than the cursor are skipped.
    ///
    /// Must be called after signers are registered with the underlying
    /// [`NostrClient`] so that incoming events can be decrypted.
    pub async fn subscribe_bots(&mut self, db: &Db) -> Result<(), DaemonError> {
        for (pubkey, bot) in self.bots.iter_mut() {
            let since = match db.load_cursor(bot.bot_id()).await? {
                Some((stored_npub, cursor)) if stored_npub == bot.npub() => {
                    Some(Timestamp::from(cursor as u64))
                }
                Some((stored_npub, _cursor)) => {
                    warn!(
                        bot_id = %bot.bot_id(),
                        stored_npub = %stored_npub,
                        config_npub = %bot.npub(),
                        "stored npub mismatch; ignoring persisted cursor"
                    );
                    None
                }
                None => None,
            };

            let sub_id = self
                .nostr_client
                .subscribe_bot_with_since(pubkey, since)
                .await?;
            bot.add_subscription(sub_id.to_string());
        }
        Ok(())
    }

    /// Check whether the handler is registered for the bot and has the required capability.
    pub fn is_authorized(
        &self,
        handler_id: &str,
        bot_id: &str,
        capability: &str,
    ) -> Result<bool, DaemonError> {
        self.handler_registry
            .is_authorized(handler_id, bot_id, capability)
    }

    /// Build a per-bot health snapshot for every configured identity.
    pub fn bot_health_snapshots(&self) -> Vec<BotHealth> {
        let mut bots: Vec<BotHealth> = self.bots.values().map(BotState::to_bot_health).collect();
        bots.sort_by(|a, b| a.bot_id.cmp(&b.bot_id));
        bots
    }

    /// Update the shared diagnostics aggregator with the current bot health snapshots.
    pub fn update_diagnostics(&self, diagnostics: &Diagnostics) {
        diagnostics.set_bots(self.bot_health_snapshots());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BotConfig, DaemonConfig, GlobalDaemonConfig, SigningConfig};
    use crate::handlers::ConnectionHandle;
    use nostr::ToBech32;
    use tokio::sync::mpsc;

    fn bot_config(id: &str, keys: &nostr::Keys) -> BotConfig {
        BotConfig {
            id: id.into(),
            npub: keys.public_key().to_bech32().unwrap(),
            signing: SigningConfig::Nsec {
                nsec: SecretString::new(keys.secret_key().to_bech32().unwrap().into()),
            },
            relays: vec![],
            capabilities: vec!["ReadMessages".into()],
            ..Default::default()
        }
    }

    fn manager_with_bots(bot_configs: Vec<BotConfig>) -> ClientManager {
        let config = DaemonConfig {
            daemon: GlobalDaemonConfig::default(),
            bots: bot_configs,
        };
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            ClientManager::new(config, NostrClient::new(vec![]).await.unwrap())
                .await
                .unwrap()
        })
    }

    #[test]
    fn empty_manager_has_no_bots() {
        let manager = manager_with_bots(vec![]);
        assert_eq!(manager.bots().count(), 0);
        assert_eq!(manager.bot_ids().count(), 0);
    }

    #[test]
    fn lookups_by_pubkey_and_bot_id() {
        let keys = nostr::Keys::generate();
        let pubkey = keys.public_key();
        let mut manager = manager_with_bots(vec![bot_config("echo-bot", &keys)]);

        assert_eq!(manager.get_bot(&pubkey).unwrap().bot_id(), "echo-bot");
        assert_eq!(
            manager.get_bot_by_id("echo-bot").unwrap().npub(),
            keys.public_key().to_bech32().unwrap()
        );

        manager
            .get_bot_mut(&pubkey)
            .unwrap()
            .add_subscription("sub-1");
        assert_eq!(
            manager
                .get_bot_by_id_mut("echo-bot")
                .unwrap()
                .clear_subscriptions(),
            vec!["sub-1"]
        );
    }

    #[test]
    fn missing_lookups_return_none() {
        let keys = nostr::Keys::generate();
        let mut manager = manager_with_bots(vec![bot_config("echo-bot", &keys)]);
        let other_keys = nostr::Keys::generate();

        assert!(manager.get_bot(&other_keys.public_key()).is_none());
        assert!(manager.get_bot_by_id("missing").is_none());
        assert!(manager.get_bot_mut(&other_keys.public_key()).is_none());
        assert!(manager.get_bot_by_id_mut("missing").is_none());
    }

    #[test]
    fn duplicate_bot_id_is_rejected() {
        let keys = nostr::Keys::generate();
        let config = DaemonConfig {
            daemon: GlobalDaemonConfig::default(),
            bots: vec![
                bot_config("dup-bot", &keys),
                bot_config("dup-bot", &nostr::Keys::generate()),
            ],
        };

        let err = tokio::runtime::Runtime::new().unwrap().block_on(async {
            ClientManager::new(config, NostrClient::new(vec![]).await.unwrap())
                .await
                .unwrap_err()
        });
        assert!(matches!(err, DaemonError::Config(_)));
        assert!(err.to_string().contains("duplicate bot_id"));
    }

    #[test]
    fn invalid_npub_is_rejected() {
        let config = DaemonConfig {
            daemon: GlobalDaemonConfig::default(),
            bots: vec![BotConfig {
                id: "bad-bot".into(),
                npub: "not-a-valid-npub".into(),
                signing: SigningConfig::Nsec {
                    nsec: SecretString::new(
                        nostr::Keys::generate()
                            .secret_key()
                            .to_bech32()
                            .unwrap()
                            .into(),
                    ),
                },
                relays: vec![],
                capabilities: vec![],
                ..Default::default()
            }],
        };

        let err = tokio::runtime::Runtime::new().unwrap().block_on(async {
            ClientManager::new(config, NostrClient::new(vec![]).await.unwrap())
                .await
                .unwrap_err()
        });
        assert!(matches!(err, DaemonError::Config(_)));
        assert!(err.to_string().contains("invalid npub"));
    }

    #[test]
    fn is_authorized_delegates_to_registry() {
        let keys = nostr::Keys::generate();
        let bot_cfg = bot_config("auth-bot", &keys);
        let mut manager = manager_with_bots(vec![bot_cfg.clone()]);

        let (tx, _rx) = mpsc::channel::<crate::transport::protocol::JsonRpcMessage>(1);
        let handler_id = manager
            .handler_registry
            .register(
                ConnectionHandle::new(tx),
                vec!["auth-bot".into()],
                vec!["dm_received".into()],
                vec!["ReadMessages".into()],
                &[bot_cfg],
            )
            .unwrap();

        assert!(
            manager
                .is_authorized(&handler_id, "auth-bot", "ReadMessages")
                .unwrap()
        );
        assert!(
            !manager
                .is_authorized(&handler_id, "auth-bot", "SendMessages")
                .unwrap()
        );
        assert!(
            manager
                .is_authorized("unknown-handler", "auth-bot", "ReadMessages")
                .is_err()
        );
    }
}
