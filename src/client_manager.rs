use crate::bot_state::BotState;
use crate::config::{DaemonConfig, validate_bot_id};
use crate::db::Db;
use crate::diagnostics::{BotHealth, Diagnostics};
use crate::errors::DaemonError;

use crate::handlers::HandlerRegistry;
use crate::nostr::NostrClient;
use crate::nostr::NostrSubscribe;
use crate::signer::Signer;
use nostr::nips::nip59;
use nostr::{PublicKey, Timestamp};
#[cfg(test)]
use secrecy::SecretString;
use std::collections::HashMap;
use std::path::Path;
use tracing::warn;

/// Manages multiple bot identities and provides npub/bot_id lookups.
///
/// # Lock ordering
///
/// When both the `ClientManager` lock and the [`Database`](crate::db::Database)
/// lock are required, the `ClientManager` lock must be acquired first. This
/// ordering is global: no code path may hold the database lock while waiting to
/// acquire the `ClientManager` lock.
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
    pub async fn new(
        data_dir: impl AsRef<Path>,
        config: DaemonConfig,
        nostr_client: NostrClient,
    ) -> Result<Self, DaemonError> {
        let mut bots = HashMap::with_capacity(config.bots.len());
        let mut bot_id_to_pubkey = HashMap::with_capacity(config.bots.len());

        for bot_config in config.bots {
            let bot_id = bot_config.id.clone();
            validate_bot_id(&bot_id)?;
            if bot_id_to_pubkey.contains_key(&bot_id) {
                return Err(DaemonError::Config(format!("duplicate bot_id: {bot_id}")));
            }

            // Bots configured to send group messages need an MLS engine. The
            // engine database lives under a per-bot directory inside the daemon
            // data directory.
            let bot_state = if bot_config
                .capabilities
                .iter()
                .any(|c| c == "SendGroupMessages")
            {
                let bot_data_dir = data_dir.as_ref().join(&bot_id);
                tokio::fs::create_dir_all(&bot_data_dir).await?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    tokio::fs::set_permissions(
                        &bot_data_dir,
                        std::fs::Permissions::from_mode(0o700),
                    )
                    .await?;
                }
                let mls_db_path = bot_data_dir.join("vector-mls.db");
                BotState::new_with_mls(bot_config, mls_db_path)?
            } else {
                BotState::new(bot_config)?
            };

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
    /// NIP-59 allows gift-wrap `created_at` to be tweaked up to 2 days into the
    /// past (`RANGE_RANDOM_TIMESTAMP_TWEAK`), so the `since` bound is shifted
    /// back by that maximum offset. This prevents the daemon from missing DMs
    /// sent shortly after a restart. The dispatch cursor still advances based on
    /// the actual event timestamp, so historical events are not reprocessed.
    ///
    /// Must be called after signers are registered with the underlying
    /// [`NostrClient`] so that incoming events can be decrypted.
    pub async fn subscribe_bots(&mut self, db: &Db) -> Result<(), DaemonError> {
        let client = self.nostr_client.clone();
        self.subscribe_bots_with_client(db, &client).await
    }

    /// Subscribe each bot using the provided [`NostrSubscribe`] implementation.
    ///
    /// This is the testable core of [`Self::subscribe_bots`]; production code
    /// passes `self.nostr_client`, while unit tests pass a mock client.
    pub async fn subscribe_bots_with_client(
        &mut self,
        db: &Db,
        client: &dyn NostrSubscribe,
    ) -> Result<(), DaemonError> {
        for (pubkey, bot) in self.bots.iter_mut() {
            let since = match db.load_cursor(bot.bot_id()).await? {
                Some((stored_npub, cursor)) if stored_npub == bot.npub() => {
                    let cursor_ts = Timestamp::from(cursor as u64);
                    let max_tweak = nip59::RANGE_RANDOM_TIMESTAMP_TWEAK.end;
                    Some(cursor_ts - max_tweak)
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

            let sub_id = client.subscribe_bot_with_since(pubkey, since).await?;
            bot.add_subscription(sub_id.to_string());

            if bot.mls.is_some() {
                let mls_sub_id = client
                    .subscribe_group_messages_with_since(pubkey, since)
                    .await?;
                bot.add_subscription(mls_sub_id.to_string());
            }
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
    pub async fn update_diagnostics(&self, diagnostics: &Diagnostics) {
        diagnostics.set_bots(self.bot_health_snapshots()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BotConfig, DaemonConfig, GlobalDaemonConfig, SigningConfig};
    use crate::db::Db;
    use crate::handlers::ConnectionHandle;
    use nostr::nips::nip59;
    use nostr::{SubscriptionId, Timestamp, ToBech32};
    use parking_lot::Mutex;
    use std::sync::Arc;
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
            mls_dedup_window_secs: None,
            ..Default::default()
        }
    }

    fn manager_with_bots(bot_configs: Vec<BotConfig>) -> ClientManager {
        let config = DaemonConfig {
            daemon: GlobalDaemonConfig::default(),
            bots: bot_configs,
        };
        let data_dir = tempfile::tempdir().unwrap();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            ClientManager::new(
                data_dir.path(),
                config,
                NostrClient::new(vec![]).await.unwrap(),
            )
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
    fn mls_bot_gets_persistent_engine_and_non_mls_bot_does_not() {
        let keys = nostr::Keys::generate();
        let mut mls_bot = bot_config("mls-bot", &keys);
        mls_bot.capabilities.push("SendGroupMessages".into());
        let dm_only_bot = bot_config("dm-bot", &nostr::Keys::generate());

        let _temp = tempfile::tempdir().unwrap();
        let config = DaemonConfig {
            daemon: GlobalDaemonConfig::default(),
            bots: vec![mls_bot, dm_only_bot],
        };
        let manager = tokio::runtime::Runtime::new().unwrap().block_on(async {
            ClientManager::new(
                _temp.path(),
                config,
                NostrClient::new(vec![]).await.unwrap(),
            )
            .await
            .unwrap()
        });

        let mls_state = manager.get_bot_by_id("mls-bot").unwrap();
        assert!(mls_state.mls.is_some());

        let dm_state = manager.get_bot_by_id("dm-bot").unwrap();
        assert!(dm_state.mls.is_none());
    }

    #[test]
    fn mls_bots_get_distinct_db_paths() {
        let keys_a = nostr::Keys::generate();
        let keys_b = nostr::Keys::generate();
        let mut bot_a = bot_config("mls-a", &keys_a);
        bot_a.capabilities.push("SendGroupMessages".into());
        let mut bot_b = bot_config("mls-b", &keys_b);
        bot_b.capabilities.push("SendGroupMessages".into());

        let _temp = tempfile::tempdir().unwrap();
        let config = DaemonConfig {
            daemon: GlobalDaemonConfig::default(),
            bots: vec![bot_a, bot_b],
        };
        let manager = tokio::runtime::Runtime::new().unwrap().block_on(async {
            ClientManager::new(
                _temp.path(),
                config,
                NostrClient::new(vec![]).await.unwrap(),
            )
            .await
            .unwrap()
        });

        let path_a = manager
            .get_bot_by_id("mls-a")
            .unwrap()
            .mls
            .as_ref()
            .unwrap()
            .db_path();
        let path_b = manager
            .get_bot_by_id("mls-b")
            .unwrap()
            .mls
            .as_ref()
            .unwrap()
            .db_path();
        assert_ne!(path_a, path_b);
        assert!(path_a.to_string_lossy().contains("mls-a"));
        assert!(path_b.to_string_lossy().contains("mls-b"));
    }

    #[test]
    fn mls_bot_db_parent_is_0700() {
        let keys = nostr::Keys::generate();
        let mut bot = bot_config("mls-perm", &keys);
        bot.capabilities.push("SendGroupMessages".into());

        let _temp = tempfile::tempdir().unwrap();
        let config = DaemonConfig {
            daemon: GlobalDaemonConfig::default(),
            bots: vec![bot],
        };
        let manager = tokio::runtime::Runtime::new().unwrap().block_on(async {
            ClientManager::new(
                _temp.path(),
                config,
                NostrClient::new(vec![]).await.unwrap(),
            )
            .await
            .unwrap()
        });

        let state = manager.get_bot_by_id("mls-perm").unwrap();
        let db_path = state.mls.as_ref().unwrap().db_path();
        let parent = db_path.parent().expect("parent directory");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(parent).expect("parent metadata");
            assert_eq!(meta.permissions().mode() & 0o777, 0o700);
        }
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
            let data_dir = tempfile::tempdir().unwrap();
            ClientManager::new(
                data_dir.path(),
                config,
                NostrClient::new(vec![]).await.unwrap(),
            )
            .await
            .unwrap_err()
        });
        assert!(matches!(err, DaemonError::Config(_)));
        assert!(err.to_string().contains("duplicate bot_id"));
    }

    #[test]
    fn unsafe_bot_id_is_rejected() {
        let keys = nostr::Keys::generate();
        let mut bad_bot = bot_config("..", &keys);
        bad_bot.capabilities.push("SendGroupMessages".into());
        let config = DaemonConfig {
            daemon: GlobalDaemonConfig::default(),
            bots: vec![bad_bot],
        };

        let err = tokio::runtime::Runtime::new().unwrap().block_on(async {
            let data_dir = tempfile::tempdir().unwrap();
            ClientManager::new(
                data_dir.path(),
                config,
                NostrClient::new(vec![]).await.unwrap(),
            )
            .await
            .unwrap_err()
        });
        assert!(matches!(err, DaemonError::Config(_)));
        assert!(err.to_string().contains("bot_id must not be '..'"));
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
                mls_dedup_window_secs: None,
                ..Default::default()
            }],
        };

        let err = tokio::runtime::Runtime::new().unwrap().block_on(async {
            let data_dir = tempfile::tempdir().unwrap();
            ClientManager::new(
                data_dir.path(),
                config,
                NostrClient::new(vec![]).await.unwrap(),
            )
            .await
            .unwrap_err()
        });
        assert!(matches!(err, DaemonError::Config(_)));
        assert!(err.to_string().contains("invalid npub"));
    }

    async fn manager_with_bots_async(bot_configs: Vec<BotConfig>) -> ClientManager {
        let config = DaemonConfig {
            daemon: GlobalDaemonConfig::default(),
            bots: bot_configs,
        };
        let data_dir = tempfile::tempdir().unwrap();
        ClientManager::new(
            data_dir.path(),
            config,
            NostrClient::new(vec![]).await.unwrap(),
        )
        .await
        .unwrap()
    }

    async fn temp_db() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("agent.db")).await.unwrap();
        (db, dir)
    }

    type SubscriptionCall = (PublicKey, Option<Timestamp>);

    /// A minimal [`NostrSubscribe`] implementation that records subscription
    /// requests and returns deterministic subscription IDs for testing.
    #[derive(Default)]
    struct MockNostrClient {
        calls: Arc<Mutex<Vec<SubscriptionCall>>>,
    }

    impl MockNostrClient {
        fn calls(&self) -> Vec<SubscriptionCall> {
            self.calls.lock().clone()
        }
    }

    #[async_trait::async_trait]
    impl NostrSubscribe for MockNostrClient {
        async fn subscribe_bot_with_since(
            &self,
            npub: &PublicKey,
            since: Option<Timestamp>,
        ) -> Result<SubscriptionId, DaemonError> {
            let sub_id = format!("sub-{}", self.calls.lock().len());
            self.calls.lock().push((*npub, since));
            Ok(SubscriptionId::new(sub_id))
        }

        async fn subscribe_group_messages_with_since(
            &self,
            npub: &PublicKey,
            since: Option<Timestamp>,
        ) -> Result<SubscriptionId, DaemonError> {
            let sub_id = format!("group-sub-{}", self.calls.lock().len());
            self.calls.lock().push((*npub, since));
            Ok(SubscriptionId::new(sub_id))
        }
    }

    #[tokio::test]
    async fn subscribe_bots_uses_cursor_as_since_filter() {
        let keys = nostr::Keys::generate();
        let pubkey = keys.public_key();
        let npub = pubkey.to_bech32().unwrap();
        let mut manager = manager_with_bots_async(vec![bot_config("cursor-bot", &keys)]).await;
        let (db, _dir) = temp_db().await;

        let cursor = 1_700_000_000_i64;
        db.save_cursor("cursor-bot", &npub, cursor).await.unwrap();

        let mock = MockNostrClient::default();
        manager
            .subscribe_bots_with_client(&db, &mock)
            .await
            .unwrap();

        let calls = mock.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, pubkey);

        let cursor_ts = Timestamp::from(cursor as u64);
        let expected_since = Some(cursor_ts - nip59::RANGE_RANDOM_TIMESTAMP_TWEAK.end);
        assert_eq!(
            calls[0].1, expected_since,
            "since should be cursor minus the max NIP-59 tweak"
        );

        let bot = manager.get_bot_mut(&pubkey).unwrap();
        assert_eq!(bot.clear_subscriptions(), vec!["sub-0"]);
    }

    #[tokio::test]
    async fn subscribe_bots_without_cursor_uses_no_since() {
        let keys = nostr::Keys::generate();
        let pubkey = keys.public_key();
        let mut manager = manager_with_bots_async(vec![bot_config("no-cursor-bot", &keys)]).await;
        let (db, _dir) = temp_db().await;

        let mock = MockNostrClient::default();
        manager
            .subscribe_bots_with_client(&db, &mock)
            .await
            .unwrap();

        let calls = mock.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, pubkey);
        assert_eq!(
            calls[0].1, None,
            "since should be None when no cursor is persisted"
        );

        assert!(
            !manager
                .get_bot_mut(&pubkey)
                .unwrap()
                .clear_subscriptions()
                .is_empty(),
            "subscription should be tracked even when no cursor exists"
        );
    }

    #[tokio::test]
    async fn subscribe_bots_ignores_cursor_on_npub_mismatch() {
        let keys = nostr::Keys::generate();
        let _pubkey = keys.public_key();
        let other_keys = nostr::Keys::generate();
        let other_npub = other_keys.public_key().to_bech32().unwrap();

        let mut manager = manager_with_bots_async(vec![bot_config("mismatch-bot", &keys)]).await;
        let (db, _dir) = temp_db().await;
        db.save_cursor("mismatch-bot", &other_npub, 1_700_000_000)
            .await
            .unwrap();

        let mock = MockNostrClient::default();
        manager
            .subscribe_bots_with_client(&db, &mock)
            .await
            .unwrap();

        let calls = mock.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].1, None,
            "since should be None when stored npub does not match config"
        );
    }

    #[tokio::test]
    async fn subscribe_bots_tracks_multiple_subscription_ids() {
        let keys_a = nostr::Keys::generate();
        let keys_b = nostr::Keys::generate();
        let pubkey_a = keys_a.public_key();
        let pubkey_b = keys_b.public_key();

        let mut manager = manager_with_bots_async(vec![
            bot_config("bot-a", &keys_a),
            bot_config("bot-b", &keys_b),
        ])
        .await;
        let (db, _dir) = temp_db().await;

        let mock = MockNostrClient::default();
        manager
            .subscribe_bots_with_client(&db, &mock)
            .await
            .unwrap();

        assert_eq!(mock.calls().len(), 2);

        let subs_a = manager
            .get_bot_mut(&pubkey_a)
            .unwrap()
            .clear_subscriptions();
        let subs_b = manager
            .get_bot_mut(&pubkey_b)
            .unwrap()
            .clear_subscriptions();
        assert_eq!(subs_a.len(), 1);
        assert_eq!(subs_b.len(), 1);
        assert_ne!(
            subs_a[0], subs_b[0],
            "each bot should receive a distinct subscription id"
        );

        let all_ids: std::collections::HashSet<_> =
            [subs_a[0].clone(), subs_b[0].clone()].into_iter().collect();
        assert_eq!(
            all_ids,
            std::collections::HashSet::from(["sub-0".into(), "sub-1".into()])
        );
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
            .unwrap()
            .handler_id;

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
