//! MLS group creation and member-addition logic backed by the MDK engine.

use mdk_core::prelude::*;
use mdk_sqlite_storage::MdkSqliteStorage;
use mdk_storage_traits::GroupId;
use nostr::{Event, Keys, UnsignedEvent};
use std::path::Path;
use thiserror::Error;

/// Errors that can occur when interacting with the MLS engine.
#[derive(Debug, Error)]
pub enum MlsError {
    /// Parent directory for the SQLite database could not be created.
    #[error("failed to create parent directory for MLS database: {0}")]
    CreateParentDir(#[source] std::io::Error),

    /// The persistent SQLite storage failed to initialize.
    #[error("MLS storage error: {0}")]
    Storage(#[from] mdk_sqlite_storage::error::Error),

    /// The MDK engine returned an error.
    #[error("MLS engine error: {0}")]
    Engine(#[from] mdk_core::Error),

    /// The MDK operation did not generate a welcome rumor for the invited bot.
    #[error("no welcome rumor generated for the invited bot")]
    NoWelcome,

    /// No group with the requested wire ID exists in the local database.
    #[error("MLS group with wire ID {0} not found in local database")]
    GroupNotFound(String),

    /// Failed to generate random bytes for group metadata.
    #[error("failed to generate random bytes: {0}")]
    Random(#[from] getrandom::Error),

    /// The bot key package could not be parsed.
    #[error("failed to parse bot key package: {0}")]
    ParseKeyPackage(#[source] mdk_core::Error),
}

/// Headless wrapper around the MDK MLS engine.
///
/// The engine is backed by a persistent SQLite database so that group state
/// (epochs, tree, secrets) survives across process invocations. This is
/// required to add members to an existing group later.
pub struct MlsManager {
    engine: MDK<MdkSqliteStorage>,
}

impl MlsManager {
    /// Open the persistent MLS engine at `db_path`.
    ///
    /// Creates parent directories if they do not exist.
    pub fn new(db_path: &Path) -> Result<Self, MlsError> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(MlsError::CreateParentDir)?;
        }
        let storage = MdkSqliteStorage::new(db_path)?;
        let engine = MDK::new(storage);
        Ok(Self { engine })
    }
}

/// Result of creating a new MLS group.
#[derive(Debug, Clone)]
pub struct GroupCreation {
    /// The MLS group ID used by the engine.
    #[allow(dead_code)]
    pub mls_group_id: GroupId,
    /// The hex-encoded `nostr_group_id` published in Nostr events (the `h` tag).
    pub wire_id: String,
    /// Unsigned welcome rumor for the invited bot.
    pub welcome_rumor: UnsignedEvent,
}

/// Result of adding a member to an existing MLS group.
#[derive(Debug, Clone)]
pub struct MemberAddition {
    /// Unsigned welcome rumor for the newly invited bot.
    pub welcome_rumor: UnsignedEvent,
    /// Signed evolution event (kind:445 commit) produced by the MDK API, if any.
    ///
    /// This should be published to the group relay to keep other members' group
    /// state in sync.
    pub evolution_event: Option<Event>,
}

impl MlsManager {
    /// Create a new MLS group containing the bot as the initial member.
    ///
    /// Mirrors the reference implementation in `tests/support/mock_mls_peer.rs`.
    pub fn create_group(
        &self,
        creator: &Keys,
        bot_key_package: &Event,
        group_name: &str,
    ) -> Result<GroupCreation, MlsError> {
        // Validate the key package before using it.
        self.engine
            .parse_key_package(bot_key_package)
            .map_err(MlsError::ParseKeyPackage)?;

        let creator_pubkey = creator.public_key();
        let image_hash = random_bytes::<32>()?;
        let image_key = random_bytes::<32>()?;
        let image_nonce = random_bytes::<12>()?;

        let config = NostrGroupConfigData::new(
            group_name.to_owned(),
            "Pacto bot squad".to_owned(),
            Some(image_hash),
            Some(image_key),
            Some(image_nonce),
            vec![],
            vec![creator_pubkey],
        );

        let result =
            self.engine
                .create_group(&creator_pubkey, vec![bot_key_package.clone()], config)?;

        let welcome_rumor = result
            .welcome_rumors
            .into_iter()
            .next()
            .ok_or(MlsError::NoWelcome)?;

        let wire_id = hex::encode(result.group.nostr_group_id);

        Ok(GroupCreation {
            mls_group_id: result.group.mls_group_id,
            wire_id,
            welcome_rumor,
        })
    }

    /// Re-open the existing group identified by `wire_id` and add the bot.
    ///
    /// The MDK `add_members` API produces a pending commit that must be merged
    /// locally after the events are generated; this method does so before
    /// returning.
    pub fn add_member(
        &self,
        wire_id: &str,
        bot_key_package: &Event,
    ) -> Result<MemberAddition, MlsError> {
        // Validate the key package before using it.
        self.engine
            .parse_key_package(bot_key_package)
            .map_err(MlsError::ParseKeyPackage)?;

        let groups = self.engine.get_groups()?;
        let group = groups
            .into_iter()
            .find(|g| hex::encode(g.nostr_group_id) == wire_id)
            .ok_or_else(|| MlsError::GroupNotFound(wire_id.to_owned()))?;

        let update = self
            .engine
            .add_members(&group.mls_group_id, std::slice::from_ref(bot_key_package))?;

        // Merge the pending commit so the local SQLite state stays consistent.
        self.engine.merge_pending_commit(&group.mls_group_id)?;

        let welcome_rumor = update
            .welcome_rumors
            .and_then(|rumors| rumors.into_iter().next())
            .ok_or(MlsError::NoWelcome)?;

        Ok(MemberAddition {
            welcome_rumor,
            evolution_event: Some(update.evolution_event),
        })
    }
}

fn random_bytes<const N: usize>() -> Result<[u8; N], getrandom::Error> {
    let mut buf = [0u8; N];
    getrandom::getrandom(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use nostr::{EventBuilder, Kind};

    async fn bot_key_package(bot_keys: &Keys) -> Event {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("kp.db");
        let engine = MDK::new(MdkSqliteStorage::new(&db).unwrap());
        let (encoded, tags) = engine
            .create_key_package_for_event(&bot_keys.public_key(), vec![])
            .unwrap();
        EventBuilder::new(Kind::MlsKeyPackage, encoded)
            .tags(tags)
            .build(bot_keys.public_key())
            .sign(bot_keys)
            .await
            .unwrap()
    }

    fn temp_manager() -> (tempfile::TempDir, MlsManager) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("mls.db");
        let manager = MlsManager::new(&db).unwrap();
        (dir, manager)
    }

    #[tokio::test]
    async fn create_group_in_memory() -> Result<(), MlsError> {
        let creator = Keys::generate();
        let (_dir, manager) = temp_manager();
        let bot = Keys::generate();
        let kp = bot_key_package(&bot).await;

        let result = manager.create_group(&creator, &kp, "test-squad")?;

        assert_eq!(result.wire_id.len(), 64);
        assert!(!result.welcome_rumor.content.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn reopen_group_by_wire_id_adds_member() -> Result<(), MlsError> {
        let creator = Keys::generate();
        let (_dir, manager) = temp_manager();

        let bot1 = Keys::generate();
        let kp1 = bot_key_package(&bot1).await;
        let created = manager.create_group(&creator, &kp1, "test-squad")?;

        let bot2 = Keys::generate();
        let kp2 = bot_key_package(&bot2).await;
        let added = manager.add_member(&created.wire_id, &kp2)?;

        assert!(!added.welcome_rumor.content.is_empty());
        assert!(added.evolution_event.is_some());

        // The group can be looked up again with the same wire ID.
        let groups = manager.engine.get_groups()?;
        let group = groups
            .into_iter()
            .find(|g| hex::encode(g.nostr_group_id) == created.wire_id)
            .unwrap();
        assert_eq!(group.name, "test-squad");
        Ok(())
    }

    #[tokio::test]
    async fn add_member_fails_for_unknown_wire_id() -> Result<(), MlsError> {
        let (_dir, manager) = temp_manager();
        let bot = Keys::generate();
        let kp = bot_key_package(&bot).await;

        let err = manager
            .add_member(
                "0000000000000000000000000000000000000000000000000000000000000000",
                &kp,
            )
            .unwrap_err();
        assert!(
            matches!(err, MlsError::GroupNotFound(_)),
            "expected GroupNotFound, got {err:?}"
        );
        Ok(())
    }

    #[test]
    fn create_parent_directory() -> Result<(), MlsError> {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b/c/mls.db");
        let _manager = MlsManager::new(&nested)?;
        assert!(nested.exists());
        Ok(())
    }
}
