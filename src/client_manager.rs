use crate::bot_state::BotState;
use crate::config::{DaemonConfig, validate_bot_id};
use crate::db::Db;
use crate::diagnostics::{BotHealth, Diagnostics};
use crate::errors::DaemonError;
use crate::handlers::HandlerRegistry;
use crate::mls::MlsEngineHandle;
use crate::nostr::NostrClient;
use crate::nostr::NostrSubscribe;
use crate::signer::Signer;
use nostr::nips::nip59;
use nostr::{PublicKey, Timestamp, ToBech32};
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

/// Reconcile a bot's MLS engine groups with the daemon database (U11/R28).
///
/// Loads this bot's `agent.db` rows first and diffs them against the
/// engine's live wire-id set in both directions:
///
/// - An engine group with no matching `agent.db` row is inserted (unchanged
///   from before U11: e.g. a crash after the engine mutation but before the
///   DB insert).
/// - An `agent.db` row with no matching engine group is a *candidate*
///   state-lost group, but is only actually marked when this bot has a
///   completed store reset on record (U10). A bare diff with no reset
///   marker also describes a bot legitimately evicted from a group by a
///   remote admin; marking that case would wrongly refuse sends on every
///   future startup. A fresh install (no rows, no marker) marks nothing.
async fn reconcile_mls_groups(
    bot_id: &str,
    bot_npub: &str,
    mls: &MlsEngineHandle,
    db: &Db,
) -> Result<(), DaemonError> {
    let db_rows = db.load_all_mls_groups(bot_id).await?;
    let engine_groups = mls.list_groups().await?;
    let engine_wire_ids: std::collections::HashSet<&str> =
        engine_groups.iter().map(|g| g.wire_id.as_str()).collect();

    for group in &engine_groups {
        let members: Vec<String> = group
            .members
            .iter()
            .map(|pk| {
                pk.to_bech32()
                    .map_err(|e| DaemonError::Config(format!("invalid public key: {e}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        db.upsert_mls_group_from_reconciliation(
            bot_id,
            bot_npub,
            &group.wire_id,
            &group.name,
            members,
        )
        .await?;
    }

    let has_completed_reset = db
        .load_mls_store_reset_marker(bot_id)
        .await?
        .is_some_and(|marker| marker.reset_at.is_some());

    if has_completed_reset {
        let now = chrono::Utc::now().timestamp();
        for row in &db_rows {
            if row.state_lost_at.is_none() && !engine_wire_ids.contains(row.wire_id.as_str()) {
                db.mark_mls_group_state_lost(bot_id, &row.group_name, now)
                    .await?;
            }
        }
    }

    Ok(())
}

impl ClientManager {
    pub async fn new(
        data_dir: impl AsRef<Path>,
        config: DaemonConfig,
        nostr_client: NostrClient,
        db: &Db,
    ) -> Result<Self, DaemonError> {
        let mut bots = HashMap::with_capacity(config.bots.len());
        let mut bot_id_to_pubkey = HashMap::with_capacity(config.bots.len());
        let data_dir = data_dir.as_ref();

        for bot_config in config.bots {
            let bot_id = bot_config.id.clone();
            validate_bot_id(&bot_id)?;
            if bot_id_to_pubkey.contains_key(&bot_id) {
                return Err(DaemonError::Config(format!("duplicate bot_id: {bot_id}")));
            }

            // Bots with an explicit mls_db_path get an MLS engine; otherwise the
            // bot has no MLS engine regardless of capabilities.
            let bot_state = if bot_config.mls_db_path.is_some() {
                match Self::construct_mls_bot_state(
                    bot_config.clone(),
                    data_dir,
                    db,
                    config.daemon.mls_archive_retention_days,
                    &nostr_client,
                )
                .await
                {
                    Ok(state) => state,
                    Err(reason) => {
                        // R49: isolate an MLS-store-specific failure to this
                        // bot instead of aborting startup for every
                        // configured bot. Genuinely fatal, non-store
                        // conditions (duplicate/invalid bot_id, signer
                        // construction failure, bunker verification) are
                        // not caught here and still propagate below.
                        warn!(
                            bot_id = %bot_id,
                            reason,
                            "MLS engine unavailable for bot; continuing startup for other bots"
                        );
                        BotState::new_mls_engine_unavailable(bot_config, reason)?
                    }
                }
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

    /// Build the MLS-engine portion of a single bot's startup (U11/R49):
    /// store path validation, classification/reset, engine construction, an
    /// after-reset KeyPackage republish (R27), and group reconciliation.
    ///
    /// Every failure here belongs to this bot alone -- the caller isolates
    /// it instead of aborting every other configured bot -- so failures map
    /// to a fixed, non-leaking reason string rather than propagate as a
    /// `DaemonError`, which may carry a raw path or SQL fragment (matching
    /// the redaction discipline `mls_reset.rs`/`mls_key.rs` already follow).
    async fn construct_mls_bot_state(
        bot_config: crate::config::BotConfig,
        data_dir: &Path,
        db: &Db,
        archive_retention_days: u32,
        nostr_client: &NostrClient,
    ) -> Result<BotState, &'static str> {
        let canonical_path = crate::config::validate_mls_db_path(&bot_config, data_dir)
            .map_err(|_| "MLS engine unavailable: store path validation failed")?;

        let reset_occurred = crate::mls_reset::classify_and_prepare(
            db,
            &bot_config.id,
            &canonical_path,
            archive_retention_days,
        )
        .await
        .map_err(|_| "MLS engine unavailable: store classification failed closed")?;

        let state = BotState::new_with_mls(bot_config, &canonical_path)
            .map_err(|_| "MLS engine unavailable: engine construction failed")?;

        if let Some(mls) = state.mls.as_ref() {
            // R27: republish the KeyPackage before reconciliation can mark
            // any group state-lost, and definitely before the bot
            // subscribes to relays (`subscribe_bots` runs only after
            // `ClientManager::new` returns) -- so a restoration attempt can
            // never reach this bot before its fresh KeyPackage is live.
            // Best-effort: a relay hiccup here should not strand an
            // otherwise-healthy engine, so a publish failure only logs.
            if reset_occurred {
                let relays = state.config.relays.clone();
                if let Err(e) = nostr_client
                    .publish_key_package(mls, &state.signer, relays)
                    .await
                {
                    warn!(
                        bot_id = %state.bot_id(),
                        error = %e,
                        "failed to republish KeyPackage after MLS store reset"
                    );
                }
            }

            reconcile_mls_groups(state.bot_id(), state.npub(), mls, db)
                .await
                .map_err(|_| "MLS engine unavailable: group reconciliation failed")?;
        }

        Ok(state)
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

    /// Build a map from configured bot npub to bot_id.
    pub fn npub_to_bot_id_map(&self) -> HashMap<String, String> {
        self.bots
            .values()
            .map(|bot| (bot.npub().to_string(), bot.bot_id().to_string()))
            .collect()
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
    use crate::test_support::mock_relay::MockRelay;
    use crate::test_support::test_tempdir;
    use mdk_sqlite_storage::MdkSqliteStorage;
    use mdk_sqlite_storage::encryption::EncryptionConfig;
    use nostr::nips::nip59;
    use nostr::{SubscriptionId, Timestamp, ToBech32};
    use parking_lot::Mutex;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn bot_config(id: &str, keys: &nostr::Keys) -> BotConfig {
        BotConfig {
            id: id.into(),
            display_name: Some(format!("{} Display", id)),
            npub: keys.public_key().to_bech32().unwrap(),
            signing: SigningConfig::Nsec {
                nsec: SecretString::new(keys.secret_key().to_bech32().unwrap().into()),
            },
            relays: vec![],
            capabilities: vec!["ReadMessages".into()],
            mls_dedup_window_secs: None,
            mls_db_path: None,
            mls_key_package_freshness_secs: None,
            ..Default::default()
        }
    }

    fn manager_with_bots(bot_configs: Vec<BotConfig>) -> ClientManager {
        let config = DaemonConfig {
            daemon: GlobalDaemonConfig::default(),
            bots: bot_configs,
        };
        let data_dir = test_tempdir();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let db = Db::open(&data_dir.path().join("agent.db")).await.unwrap();
            ClientManager::new(
                data_dir.path(),
                config,
                NostrClient::new(vec![]).await.unwrap(),
                &db,
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
        mls_bot.mls_db_path = Some(std::path::PathBuf::from("vector-mls.db"));
        mls_bot.capabilities.push("SendGroupMessages".into());
        let dm_only_bot = bot_config("dm-bot", &nostr::Keys::generate());

        let _temp = test_tempdir();
        let config = DaemonConfig {
            daemon: GlobalDaemonConfig::default(),
            bots: vec![mls_bot, dm_only_bot],
        };
        let manager = tokio::runtime::Runtime::new().unwrap().block_on(async {
            let db = Db::open(&_temp.path().join("agent.db")).await.unwrap();
            ClientManager::new(
                _temp.path(),
                config,
                NostrClient::new(vec![]).await.unwrap(),
                &db,
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
        bot_a.mls_db_path = Some(std::path::PathBuf::from("vector-mls.db"));
        bot_a.capabilities.push("SendGroupMessages".into());
        let mut bot_b = bot_config("mls-b", &keys_b);
        bot_b.mls_db_path = Some(std::path::PathBuf::from("vector-mls.db"));
        bot_b.capabilities.push("SendGroupMessages".into());

        let _temp = test_tempdir();
        let config = DaemonConfig {
            daemon: GlobalDaemonConfig::default(),
            bots: vec![bot_a, bot_b],
        };
        let manager = tokio::runtime::Runtime::new().unwrap().block_on(async {
            let db = Db::open(&_temp.path().join("agent.db")).await.unwrap();
            ClientManager::new(
                _temp.path(),
                config,
                NostrClient::new(vec![]).await.unwrap(),
                &db,
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
        bot.mls_db_path = Some(std::path::PathBuf::from("vector-mls.db"));
        bot.capabilities.push("SendGroupMessages".into());

        let _temp = test_tempdir();
        let config = DaemonConfig {
            daemon: GlobalDaemonConfig::default(),
            bots: vec![bot],
        };
        let manager = tokio::runtime::Runtime::new().unwrap().block_on(async {
            let db = Db::open(&_temp.path().join("agent.db")).await.unwrap();
            ClientManager::new(
                _temp.path(),
                config,
                NostrClient::new(vec![]).await.unwrap(),
                &db,
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
            let data_dir = test_tempdir();
            let db = Db::open(&data_dir.path().join("agent.db")).await.unwrap();
            ClientManager::new(
                data_dir.path(),
                config,
                NostrClient::new(vec![]).await.unwrap(),
                &db,
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
        let bad_bot = bot_config("..", &keys);
        let config = DaemonConfig {
            daemon: GlobalDaemonConfig::default(),
            bots: vec![bad_bot],
        };

        let err = tokio::runtime::Runtime::new().unwrap().block_on(async {
            let data_dir = test_tempdir();
            let db = Db::open(&data_dir.path().join("agent.db")).await.unwrap();
            ClientManager::new(
                data_dir.path(),
                config,
                NostrClient::new(vec![]).await.unwrap(),
                &db,
            )
            .await
            .unwrap_err()
        });
        assert!(matches!(err, DaemonError::Config(_)));
        assert!(
            err.to_string()
                .contains("must start with a lowercase letter or digit")
        );
    }

    #[test]
    fn invalid_npub_is_rejected() {
        let config = DaemonConfig {
            daemon: GlobalDaemonConfig::default(),
            bots: vec![BotConfig {
                id: "bad-bot".into(),
                display_name: Some("bad-bot Display".to_string()),
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
            let data_dir = test_tempdir();
            let db = Db::open(&data_dir.path().join("agent.db")).await.unwrap();
            ClientManager::new(
                data_dir.path(),
                config,
                NostrClient::new(vec![]).await.unwrap(),
                &db,
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
        let data_dir = test_tempdir();
        let db = Db::open(&data_dir.path().join("agent.db")).await.unwrap();
        ClientManager::new(
            data_dir.path(),
            config,
            NostrClient::new(vec![]).await.unwrap(),
            &db,
        )
        .await
        .unwrap()
    }

    async fn temp_db() -> (Db, tempfile::TempDir) {
        let dir = test_tempdir();
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

    #[tokio::test]
    async fn mls_engine_construction_failure_is_isolated_and_recorded_in_health() {
        let keys_a = nostr::Keys::generate();
        let keys_b = nostr::Keys::generate();
        let keys_bad = nostr::Keys::generate();

        let bot_a = bot_config("healthy-a", &keys_a);
        let bot_b = bot_config("healthy-b", &keys_b);
        let mut bot_bad = bot_config("unrecognised-store", &keys_bad);
        bot_bad.mls_db_path = Some(std::path::PathBuf::from("vector-mls.db"));
        bot_bad.capabilities.push("SendGroupMessages".into());

        let data_dir = test_tempdir();
        let bot_dir = data_dir.path().join(&bot_bad.id);
        std::fs::create_dir_all(&bot_dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bot_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let store_path = bot_dir.join("vector-mls.db");
        {
            let conn = rusqlite::Connection::open(&store_path).unwrap();
            conn.execute_batch("CREATE TABLE unrelated (x INTEGER);")
                .unwrap();
        }

        let config = DaemonConfig {
            daemon: GlobalDaemonConfig::default(),
            bots: vec![bot_a, bot_bad.clone(), bot_b],
        };

        let db = Db::open(&data_dir.path().join("agent.db")).await.unwrap();
        let manager = ClientManager::new(
            data_dir.path(),
            config,
            NostrClient::new(vec![]).await.unwrap(),
            &db,
        )
        .await
        .expect("daemon must stay up despite one bad MLS store");

        assert!(manager.get_bot_by_id("healthy-a").is_some());
        assert!(manager.get_bot_by_id("healthy-b").is_some());

        let failed = manager
            .get_bot_by_id("unrecognised-store")
            .expect("failed bot still tracked so its health is visible");
        assert!(failed.mls.is_none());
        assert!(failed.mls_engine_unavailable());

        let health = manager.bot_health_snapshots();
        let failed_health = health
            .iter()
            .find(|h| h.bot_id == "unrecognised-store")
            .unwrap();
        let error = failed_health.error.as_deref().expect("error recorded");
        assert!(
            !error.contains(&store_path.display().to_string()),
            "must not leak the store path: {error}"
        );
        assert!(
            !error.to_lowercase().contains("unrelated"),
            "must not leak table/SQL fragments: {error}"
        );

        for id in ["healthy-a", "healthy-b"] {
            let h = health.iter().find(|h| h.bot_id == id).unwrap();
            assert!(h.error.is_none(), "healthy bot must have no error");
        }
    }

    #[tokio::test]
    async fn reset_republishes_key_package_before_reconciliation() {
        let relay = MockRelay::start().await.expect("mock relay");
        let keys = nostr::Keys::generate();
        let mut bot = bot_config("reset-bot", &keys);
        bot.mls_db_path = Some(std::path::PathBuf::from("vector-mls.db"));
        bot.capabilities.push("SendGroupMessages".into());
        bot.relays = vec![relay.url()];

        let data_dir = test_tempdir();
        let bot_dir = data_dir.path().join(&bot.id);
        std::fs::create_dir_all(&bot_dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bot_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let store_path = bot_dir.join("vector-mls.db");

        // Build an encrypted store, then corrupt its key so classification
        // sees WrongEncryptionKey and resets it (R26 -- always archived).
        let original_key = crate::mls_key::load_or_create(&store_path).unwrap();
        drop(
            MdkSqliteStorage::new_with_key(&store_path, EncryptionConfig::new(*original_key))
                .unwrap(),
        );
        let key_path = crate::mls_key::key_path_for_store(&store_path);
        std::fs::write(&key_path, [0xAB_u8; 32]).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let config = DaemonConfig {
            daemon: GlobalDaemonConfig::default(),
            bots: vec![bot],
        };
        let db = Db::open(&data_dir.path().join("agent.db")).await.unwrap();

        let manager = ClientManager::new(
            data_dir.path(),
            config,
            NostrClient::new(vec![relay.url()]).await.unwrap(),
            &db,
        )
        .await
        .expect("client manager initializes after reset");

        assert!(manager.get_bot_by_id("reset-bot").unwrap().mls.is_some());

        // The reset must have republished a fresh KeyPackage before this
        // function returns -- the only way a restoration (an admin re-add
        // targeting this bot's new store) can reach it. `subscribe_bots`,
        // and therefore any Welcome-driven restoration attempt, has not
        // even been called yet at this point: that ordering is structural,
        // not just observed here.
        let events = relay
            .wait_for_event(
                |e| e.kind == nostr::Kind::MlsKeyPackage,
                std::time::Duration::from_secs(2),
            )
            .await
            .expect("KeyPackage event published to the relay");
        assert!(!events.is_empty());

        relay.stop().await;
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

    /// Build and sign a real kind:443 KeyPackage event against `engine` for
    /// `keys` -- a single engine can act as both creator and (via a
    /// self-issued KeyPackage) recipient for group-creation tests, so no
    /// second party is needed just to exercise reconciliation. Uses the
    /// `nostr_json` seam (not `sign_with_keys` directly) per the
    /// containment lint.
    async fn build_key_package_event(engine: &MlsEngineHandle, keys: &nostr::Keys) -> nostr::Event {
        let relays = vec![nostr::RelayUrl::parse("wss://test.relay").unwrap()];
        let (content, tags) = engine
            .publish_key_package(&keys.public_key(), relays)
            .await
            .expect("publish_key_package");
        let rumor = nostr::UnsignedEvent::new(
            keys.public_key(),
            nostr::Timestamp::now(),
            nostr::Kind::MlsKeyPackage,
            tags,
            content,
        );
        crate::nostr_json::sign_unsigned(rumor, keys).expect("sign key package")
    }

    #[tokio::test]
    async fn fresh_install_reconciliation_marks_nothing() {
        let temp = test_tempdir();
        let db = Db::open(&temp.path().join("agent.db")).await.unwrap();
        let engine = MlsEngineHandle::new_persistent(temp.path().join("vector-mls.db"))
            .expect("new_persistent");

        reconcile_mls_groups("fresh-bot", "npub1fresh", &engine, &db)
            .await
            .expect("reconcile");

        assert!(
            db.load_all_mls_groups("fresh-bot")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            db.load_mls_store_reset_marker("fresh-bot")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn eviction_without_reset_marker_is_not_marked_state_lost() {
        let temp = test_tempdir();
        let db = Db::open(&temp.path().join("agent.db")).await.unwrap();
        let engine = MlsEngineHandle::new_persistent(temp.path().join("vector-mls.db"))
            .expect("new_persistent");

        // A row the bot no longer has engine state for, but with NO
        // completed reset on record -- a legitimate remote eviction, not a
        // reset. Must not be marked, or the bot's sends into any future
        // group would wrongly refuse forever.
        db.insert_mls_group(crate::db::MlsGroupRow {
            bot_id: "evicted-bot".into(),
            group_name: "old-squad".into(),
            wire_id: "a".repeat(64),
            creator_npub: "npub1someone".into(),
            relay: "wss://relay.example".into(),
            invited_bots: vec![],
            state_lost_at: None,
        })
        .await
        .unwrap();

        reconcile_mls_groups("evicted-bot", "npub1evicted", &engine, &db)
            .await
            .expect("reconcile");

        assert!(
            db.load_mls_group_state_lost_at("evicted-bot", &"a".repeat(64))
                .await
                .unwrap()
                .is_none(),
            "a bare diff with no reset marker must not mark eviction as state-lost"
        );
    }

    #[tokio::test]
    async fn completed_reset_marks_orphaned_groups_state_lost_and_leaves_members_untouched() {
        let temp = test_tempdir();
        let db = Db::open(&temp.path().join("agent.db")).await.unwrap();
        let engine = MlsEngineHandle::new_persistent(temp.path().join("vector-mls.db"))
            .expect("new_persistent");

        db.insert_mls_group(crate::db::MlsGroupRow {
            bot_id: "reset-bot".into(),
            group_name: "pre-reset-squad".into(),
            wire_id: "b".repeat(64),
            creator_npub: "npub1someone".into(),
            relay: "wss://relay.example".into(),
            invited_bots: vec!["npub1member".into()],
            state_lost_at: None,
        })
        .await
        .unwrap();
        assert!(
            db.is_bot_invited("reset-bot", "pre-reset-squad", "npub1member")
                .await
                .unwrap()
        );

        // Simulate a completed U10 reset for this bot.
        db.mark_mls_store_reset_start("reset-bot", 1_000)
            .await
            .unwrap();
        db.complete_mls_store_reset("reset-bot", 1_100, None)
            .await
            .unwrap();

        reconcile_mls_groups("reset-bot", "npub1resetbot", &engine, &db)
            .await
            .expect("reconcile");

        assert!(
            db.load_mls_group_state_lost_at("reset-bot", &"b".repeat(64))
                .await
                .unwrap()
                .is_some(),
            "every pre-reset group with no matching engine state must carry state_lost_at"
        );
        assert!(
            db.is_bot_invited("reset-bot", "pre-reset-squad", "npub1member")
                .await
                .unwrap(),
            "mls_group_members rows must survive the reset untouched"
        );
    }

    #[tokio::test]
    async fn group_renamed_while_bot_was_out_reconciles_on_wire_id_without_erroring() {
        let temp = test_tempdir();
        let db = Db::open(&temp.path().join("agent.db")).await.unwrap();
        let engine = MlsEngineHandle::new_persistent(temp.path().join("vector-mls.db"))
            .expect("new_persistent");

        let creator_keys = nostr::Keys::generate();
        let recipient_keys = nostr::Keys::generate();
        let key_package = build_key_package_event(&engine, &recipient_keys).await;
        let (wire_id, _welcome) = engine
            .create_group(
                creator_keys.public_key(),
                recipient_keys.public_key(),
                key_package,
                "renamed-squad".to_string(),
                vec![nostr::RelayUrl::parse("wss://test.relay").unwrap()],
            )
            .await
            .expect("create_group");

        // agent.db still has the OLD name for this real wire_id, as if the
        // Squad was renamed on pacto-app while this bot was out of it.
        db.insert_mls_group(crate::db::MlsGroupRow {
            bot_id: "rename-bot".into(),
            group_name: "old-squad-name".into(),
            wire_id: wire_id.clone(),
            creator_npub: creator_keys.public_key().to_bech32().unwrap(),
            relay: "wss://relay.example".into(),
            invited_bots: vec![],
            state_lost_at: None,
        })
        .await
        .unwrap();

        reconcile_mls_groups(
            "rename-bot",
            &creator_keys.public_key().to_bech32().unwrap(),
            &engine,
            &db,
        )
        .await
        .expect("reconcile must not abort startup on a rename");

        let rows = db.load_all_mls_groups("rename-bot").await.unwrap();
        assert_eq!(
            rows.len(),
            1,
            "the rename must update in place, not duplicate"
        );
        assert_eq!(rows[0].wire_id, wire_id);
        assert_eq!(
            rows[0].group_name, "renamed-squad",
            "group_name must be updated to the engine's current name on a wire_id match"
        );
    }
}
