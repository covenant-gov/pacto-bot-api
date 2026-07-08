//! MLS engine wrapper for the daemon.
//!
//! The `mdk_core::MDK<MdkSqliteStorage>` engine is `Send` but not `Sync` because
//! `rusqlite::Connection` contains `RefCell` state. To share the engine across
//! Tokio tasks, the handle runs the engine on a dedicated single-threaded worker
//! thread. All engine calls are serialized through an mpsc channel, eliminating
//! any possibility of lock contention or ordering issues.

use std::path::{Path, PathBuf};

use mdk_core::prelude::*;
use mdk_sqlite_storage::MdkSqliteStorage;
use mdk_storage_traits::GroupId;
use nostr::{Event, PublicKey, RelayUrl};
use tokio::sync::{mpsc, oneshot};

/// Errors that can occur when interacting with the MLS engine.
#[derive(Debug, thiserror::Error)]
pub enum MlsError {
    /// SQLite or storage-layer error from the MDK backend.
    #[error("MLS storage error")]
    Storage(#[from] mdk_sqlite_storage::error::Error),

    /// Filesystem permission error when securing the MLS database.
    #[error("MLS filesystem error")]
    Io(#[from] std::io::Error),

    /// Raw `rusqlite` error when enabling WAL or inspecting the MLS database.
    #[error("MLS sqlite error")]
    Rusqlite(#[from] rusqlite::Error),

    /// Generic engine failure; the message must not contain key material.
    #[error("MLS engine error: {0}")]
    Engine(String),

    /// The requested group does not exist in the engine's storage.
    #[error("MLS group not found")]
    GroupNotFound,

    /// The engine has not been initialized (no accepted Welcome yet).
    #[error("MLS engine not initialized")]
    NotInitialized,

    /// The group is permanently unusable and must be re-created.
    #[error("MLS group poisoned")]
    GroupPoisoned,

    /// A cryptographic operation failed; the message must not leak secrets.
    #[error("MLS crypto error")]
    CryptoError,

    /// Channel communication error with the MLS worker thread.
    #[error("MLS worker communication error")]
    WorkerDisconnected,
}

impl From<mdk_core::Error> for MlsError {
    fn from(err: mdk_core::Error) -> Self {
        let msg = err.to_string();
        // Map specific MDK errors to our typed variants
        if msg.contains("group not found") || msg.contains("unknown group") {
            MlsError::GroupNotFound
        } else if msg.contains("not initialized") || msg.contains("no group") {
            MlsError::NotInitialized
        } else if msg.contains("poison") || msg.contains("corrupt") {
            MlsError::GroupPoisoned
        } else if msg.contains("crypto") || msg.contains("signature") {
            MlsError::CryptoError
        } else {
            MlsError::Engine(msg)
        }
    }
}

/// Internal commands sent to the MLS worker thread.
enum MlsCommand {
    CreateKeyPackage {
        pubkey: PublicKey,
        relays: Vec<RelayUrl>,
        tx: oneshot::Sender<Result<(String, Vec<nostr::Tag>), MlsError>>,
    },
    ProcessWelcome {
        event_id: nostr::EventId,
        welcome_rumor: nostr::Event,
        tx: oneshot::Sender<Result<(), MlsError>>,
    },
    AcceptPendingWelcome {
        tx: oneshot::Sender<Result<GroupInfo, MlsError>>,
    },
    CreateGroupMessage {
        group_id: Vec<u8>,
        rumor: nostr::UnsignedEvent,
        tx: oneshot::Sender<Result<Event, MlsError>>,
    },
    IsGroupMember {
        wire_id: String,
        member: PublicKey,
        tx: oneshot::Sender<Result<bool, MlsError>>,
    },
    DecryptGroupMessage {
        event: nostr::Event,
        tx: oneshot::Sender<Result<Option<DecryptedMessage>, MlsError>>,
    },
    HasGroupWithWireId {
        group_id: String,
        tx: oneshot::Sender<Result<bool, MlsError>>,
    },
}

/// Cloneable handle to the per-bot MLS engine.
///
/// Cloning is cheap: it clones the sender channel, not the engine itself.
/// All engine calls are serialized through a dedicated worker thread.
#[derive(Clone)]
pub struct MlsEngineHandle {
    tx: mpsc::Sender<MlsCommand>,
    db_path: PathBuf,
}

impl std::fmt::Debug for MlsEngineHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MlsEngineHandle")
            .field("db_path", &self.db_path)
            .finish_non_exhaustive()
    }
}
const _: () = {
    // Verify that the handle is Send + Sync, which is required for sharing
    // across Tokio tasks.
    fn assert_send_sync<T: Send + Sync>() {}
    let _ = assert_send_sync::<MlsEngineHandle>;
};

impl MlsEngineHandle {
    /// Create a persistent MLS engine backed by `vector-mls.db` at `db_path`.
    ///
    /// This spawns a dedicated worker thread that owns the `MDK<MdkSqliteStorage>`
    /// engine. All engine calls are serialized through this thread via an mpsc
    /// channel, eliminating any possibility of lock contention.
    pub fn new_persistent<P: AsRef<Path>>(db_path: P) -> Result<Self, MlsError> {
        let db_path = db_path.as_ref().to_path_buf();

        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Enable WAL mode first. We hold a temporary connection open while
        // `MdkSqliteStorage` initializes so the WAL file is not checkpointed
        // and removed before the engine's own connections adopt WAL mode.
        let wal_conn = rusqlite::Connection::open(&db_path)?;
        wal_conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;",
        )?;
        wal_conn.execute(
            "CREATE TABLE IF NOT EXISTS _pacto_mls_wal_trigger (x INTEGER)",
            [],
        )?;

        let storage = MdkSqliteStorage::new(&db_path)?;
        set_db_permissions(&db_path)?;

        let engine = MDK::new(storage);

        // Spawn the worker thread that owns the engine
        let (tx, mut rx) = mpsc::channel::<MlsCommand>(32);

        std::thread::spawn(move || {
            // Worker thread: own the engine and process commands serially
            let engine = engine;

            while let Some(cmd) = rx.blocking_recv() {
                match cmd {
                    MlsCommand::CreateKeyPackage { pubkey, relays, tx } => {
                        let result: Result<(String, Vec<nostr::Tag>), MlsError> = engine
                            .create_key_package_for_event(&pubkey, relays)
                            .map_err(MlsError::from)
                            .map(|(encoded, tags)| (encoded, tags.to_vec()));
                        let _ = tx.send(result);
                    }
                    MlsCommand::ProcessWelcome {
                        event_id,
                        welcome_rumor,
                        tx,
                    } => {
                        let result: Result<(), MlsError> = (|| {
                            // Convert nostr::Event to UnsignedEvent for process_welcome
                            let unsigned = nostr::UnsignedEvent {
                                id: Some(welcome_rumor.id),
                                pubkey: welcome_rumor.pubkey,
                                created_at: welcome_rumor.created_at,
                                kind: welcome_rumor.kind,
                                tags: welcome_rumor.tags.clone(),
                                content: welcome_rumor.content.clone(),
                            };
                            engine.process_welcome(&event_id, &unsigned)?;
                            Ok(())
                        })();
                        let _ = tx.send(result);
                    }
                    MlsCommand::AcceptPendingWelcome { tx } => {
                        let result: Result<GroupInfo, MlsError> = (|| {
                            let welcomes = engine.get_pending_welcomes()?;
                            let welcome = welcomes.first().ok_or(MlsError::NotInitialized)?;
                            engine.accept_welcome(welcome)?;

                            // Get the group info for the accepted group
                            let groups = engine.get_groups()?;
                            let group = groups.first().ok_or(MlsError::NotInitialized)?;
                            Ok(GroupInfo {
                                mls_group_id: group.mls_group_id.as_slice().to_vec(),
                                nostr_group_id: group.nostr_group_id.to_vec(),
                                name: group.name.clone(),
                            })
                        })();
                        let _ = tx.send(result);
                    }
                    MlsCommand::CreateGroupMessage {
                        group_id,
                        rumor,
                        tx,
                    } => {
                        let result: Result<Event, MlsError> = (|| {
                            let group_id = GroupId::from_slice(&group_id);
                            let event = engine.create_message(&group_id, rumor)?;
                            Ok(event)
                        })();
                        let _ = tx.send(result);
                    }
                    MlsCommand::DecryptGroupMessage { event, tx } => {
                        let group_id = event
                            .tags
                            .iter()
                            .find(|t| t.kind() == nostr::TagKind::h())
                            .and_then(|t| t.content())
                            .map(|s| s.to_string());

                        let result = match group_id {
                            Some(group_id) => match engine.process_message(&event) {
                                Ok(MessageProcessingResult::ApplicationMessage(msg)) => {
                                    Ok(Some(DecryptedMessage {
                                        content: msg.content,
                                        group_id,
                                        author: msg.pubkey.to_hex(),
                                        event_id: event.id.to_hex(),
                                        timestamp: event.created_at.as_u64(),
                                    }))
                                }
                                Ok(
                                    MessageProcessingResult::Proposal(_)
                                    | MessageProcessingResult::Commit { .. }
                                    | MessageProcessingResult::ExternalJoinProposal { .. }
                                    | MessageProcessingResult::Unprocessable { .. },
                                ) => Ok(None),
                                Err(_) => {
                                    Err(MlsError::Engine("failed to process group message".into()))
                                }
                            },
                            None => Err(MlsError::Engine("missing group id h tag".into())),
                        };
                        let _ = tx.send(result);
                    }
                    MlsCommand::HasGroupWithWireId { group_id, tx } => {
                        let result: Result<bool, MlsError> = (|| {
                            let groups = engine.get_groups()?;
                            Ok(groups
                                .iter()
                                .any(|g| hex::encode(g.nostr_group_id.as_slice()) == group_id))
                        })();
                        let _ = tx.send(result);
                    }
                    MlsCommand::IsGroupMember {
                        wire_id,
                        member,
                        tx,
                    } => {
                        let result: Result<bool, MlsError> = (|| {
                            let groups = engine.get_groups()?;
                            let group = groups
                                .iter()
                                .find(|g| hex::encode(g.nostr_group_id.as_slice()) == wire_id)
                                .ok_or(MlsError::GroupNotFound)?;
                            let members = engine.get_members(&group.mls_group_id)?;
                            Ok(members.contains(&member))
                        })();
                        let _ = tx.send(result);
                    }
                }
            }

            // Channel closed, worker shutting down
            drop(engine);
        });

        // The engine's own connections keep WAL active; the temporary connection
        // can now be safely dropped.
        drop(wal_conn);

        Ok(Self { tx, db_path })
    }

    /// Create a key package for the bot and return the encoded content and tags.
    ///
    /// The returned event content should be published as a kind:443 event.
    pub async fn publish_key_package(
        &self,
        pubkey: &PublicKey,
        relays: Vec<RelayUrl>,
    ) -> Result<(String, Vec<nostr::Tag>), MlsError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(MlsCommand::CreateKeyPackage {
                pubkey: *pubkey,
                relays,
                tx,
            })
            .await
            .map_err(|_| MlsError::WorkerDisconnected)?;
        rx.await.map_err(|_| MlsError::WorkerDisconnected)?
    }

    /// Process a received Welcome message.
    ///
    /// This decrypts and validates the Welcome, persisting the group state.
    pub async fn process_welcome(
        &self,
        event_id: nostr::EventId,
        welcome_rumor: nostr::Event,
    ) -> Result<(), MlsError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(MlsCommand::ProcessWelcome {
                event_id,
                welcome_rumor,
                tx,
            })
            .await
            .map_err(|_| MlsError::WorkerDisconnected)?;
        rx.await.map_err(|_| MlsError::WorkerDisconnected)?
    }

    /// Accept any pending Welcome messages.
    ///
    /// Returns the group info for the accepted group, or `NotInitialized` if
    /// there are no pending welcomes.
    pub async fn accept_pending_welcome(&self) -> Result<GroupInfo, MlsError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(MlsCommand::AcceptPendingWelcome { tx })
            .await
            .map_err(|_| MlsError::WorkerDisconnected)?;
        rx.await.map_err(|_| MlsError::WorkerDisconnected)?
    }

    /// Create an MLS group message.
    ///
    /// Returns the wrapper event (kind:445) ready to be published.
    pub async fn create_group_message(
        &self,
        group_id: Vec<u8>,
        rumor: nostr::UnsignedEvent,
    ) -> Result<Event, MlsError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(MlsCommand::CreateGroupMessage {
                group_id,
                rumor,
                tx,
            })
            .await
            .map_err(|_| MlsError::WorkerDisconnected)?;
        rx.await.map_err(|_| MlsError::WorkerDisconnected)?
    }

    /// Decrypt an inbound MLS group message wrapper (kind:445).
    ///
    /// Returns `Ok(None)` for protocol-only messages (proposals, commits, etc.)
    /// and `Ok(Some(DecryptedMessage))` for application messages.
    pub async fn decrypt_group_message(
        &self,
        event: &nostr::Event,
    ) -> Result<Option<DecryptedMessage>, MlsError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(MlsCommand::DecryptGroupMessage {
                event: event.clone(),
                tx,
            })
            .await
            .map_err(|_| MlsError::WorkerDisconnected)?;
        rx.await.map_err(|_| MlsError::WorkerDisconnected)?
    }

    /// Check whether `member` is a member of the group identified by its
    /// Squad wire id (`hex(nostr_group_id)`).
    pub async fn is_group_member(
        &self,
        wire_id: &str,
        member: &PublicKey,
    ) -> Result<bool, MlsError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(MlsCommand::IsGroupMember {
                wire_id: wire_id.to_string(),
                member: *member,
                tx,
            })
            .await
            .map_err(|_| MlsError::WorkerDisconnected)?;
        rx.await.map_err(|_| MlsError::WorkerDisconnected)?
    }

    /// Return the path to the underlying `vector-mls.db` file.
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Check whether the engine knows a group whose `nostr_group_id` hex-matches
    /// the given wire id.
    pub async fn has_group_with_wire_id(&self, group_id: &str) -> Result<bool, MlsError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(MlsCommand::HasGroupWithWireId {
                group_id: group_id.to_string(),
                tx,
            })
            .await
            .map_err(|_| MlsError::WorkerDisconnected)?;
        rx.await.map_err(|_| MlsError::WorkerDisconnected)?
    }
}

/// Group information returned after accepting a Welcome.
#[derive(Debug, Clone)]
pub struct GroupInfo {
    pub mls_group_id: Vec<u8>,
    pub nostr_group_id: Vec<u8>,
    pub name: String,
}

/// A decrypted MLS application message.
#[derive(Debug, Clone)]
pub struct DecryptedMessage {
    /// Plaintext content of the application message.
    pub content: String,
    /// Squad wire id from the wrapper event's `h` tag.
    pub group_id: String,
    /// Sender's Nostr pubkey in hex.
    pub author: String,
    /// Wrapper event id in hex.
    pub event_id: String,
    /// Wrapper event `created_at` timestamp.
    pub timestamp: u64,
}

/// Enforce `0o600` on the SQLite database and its WAL/SHM sidecars if present.
///
/// SQLite creates files using the process umask, so an explicit `set_permissions`
/// call is required even when the parent directory is `0o700`.
#[cfg(unix)]
pub(crate) fn set_db_permissions<P: AsRef<Path>>(db_path: P) -> Result<(), MlsError> {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let db_path = db_path.as_ref();
    fs::set_permissions(db_path, fs::Permissions::from_mode(0o600))?;

    for ext in ["-wal", "-shm"] {
        let sidecar = db_path.with_extension(format!("db{}", ext));
        if sidecar.exists() {
            fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn set_db_permissions<P: AsRef<Path>>(_db_path: P) -> Result<(), MlsError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_persistent_creates_0600_db_and_sidecars() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("vector-mls.db");

        let _handle = MlsEngineHandle::new_persistent(&db_path).expect("new_persistent");
        assert!(db_path.exists());
        assert!(db_path.with_extension("db-wal").exists());
        assert!(db_path.with_extension("db-shm").exists());

        let meta = std::fs::metadata(&db_path).expect("metadata");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);

            let wal = db_path.with_extension("db-wal");
            let shm = db_path.with_extension("db-shm");
            assert_eq!(
                std::fs::metadata(&wal)
                    .expect("wal metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&shm)
                    .expect("shm metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn handle_is_clone() {
        let temp = tempfile::tempdir().expect("tempdir");
        let handle = MlsEngineHandle::new_persistent(temp.path().join("vector-mls.db"))
            .expect("new_persistent");
        let _clone = handle.clone();
    }
}
