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
use nostr::{Event, Kind, PublicKey, RelayUrl, UnsignedEvent};
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

    /// The provided KeyPackage event is not a valid kind:443 with non-empty
    /// content from the expected author.
    #[error("invalid MLS key package")]
    InvalidKeyPackage,

    /// The MLS database path is unsafe (symlink, mountpoint, or shared temp directory).
    #[error("MLS database path is insecure: {0}")]
    InsecurePath(String),

    /// Channel communication error with the MLS worker thread.
    #[error("MLS worker communication error")]
    WorkerDisconnected,
}

impl From<mdk_core::Error> for MlsError {
    fn from(err: mdk_core::Error) -> Self {
        // Log only a sanitized error category at DEBUG; never expose raw MDK
        // strings or key material in logs.
        tracing::debug!(
            category = mdk_error_category(&err),
            "MLS engine error categorized"
        );
        match err {
            mdk_core::Error::GroupNotFound => MlsError::GroupNotFound,
            mdk_core::Error::Crypto(_) => MlsError::CryptoError,
            _ => {
                // Any unclassified MDK error is rewritten to a fixed, generic message
                // so that raw MDK strings never reach callers.
                MlsError::Engine("MLS engine failure".into())
            }
        }
    }
}

/// Map a raw `mdk_core::Error` to a stable, non-leaky category string for
/// logging. The category must never include key material, group IDs, or raw
/// error messages from the engine.
fn mdk_error_category(err: &mdk_core::Error) -> &'static str {
    match err {
        mdk_core::Error::GroupNotFound => "group_not_found",
        mdk_core::Error::Crypto(_) => "crypto",
        mdk_core::Error::KeyPackage(_) => "key_package",
        mdk_core::Error::Group(_) => "group",
        mdk_core::Error::Message(_) => "message",
        mdk_core::Error::Welcome(_) => "welcome",
        mdk_core::Error::ProcessMessageWrongEpoch
        | mdk_core::Error::ProcessMessageWrongGroupId
        | mdk_core::Error::ProcessMessageUseAfterEviction
        | mdk_core::Error::ProcessMessageOther(_) => "process_message",
        _ => "other",
    }
}

impl From<crate::mls_path::MlsPathError> for MlsError {
    fn from(err: crate::mls_path::MlsPathError) -> Self {
        MlsError::InsecurePath(err.to_string())
    }
}

/// Validate a KeyPackage event before passing it to the MDK engine.
///
/// The event must have a valid signature, be kind:443, have non-empty content,
/// and be authored by the expected recipient.
fn validate_key_package(key_package: &Event, recipient: &PublicKey) -> Result<(), MlsError> {
    if key_package.verify().is_err() {
        return Err(MlsError::InvalidKeyPackage);
    }
    if key_package.kind != Kind::MlsKeyPackage {
        return Err(MlsError::InvalidKeyPackage);
    }
    if key_package.content.is_empty() {
        return Err(MlsError::InvalidKeyPackage);
    }
    if key_package.pubkey != *recipient {
        return Err(MlsError::InvalidKeyPackage);
    }
    Ok(())
}

/// Fill a fixed-size byte buffer with random bytes from `getrandom`.
fn random_bytes<const N: usize>() -> Result<[u8; N], MlsError> {
    let mut buf = [0u8; N];
    getrandom::getrandom(&mut buf).map_err(|e| {
        tracing::debug!(error = %e, "failed to generate random bytes");
        MlsError::Engine("failed to generate random bytes".into())
    })?;
    Ok(buf)
}

/// Internal commands sent to the MLS worker thread.
enum MlsCommand {
    CreateKeyPackage {
        pubkey: PublicKey,
        relays: Vec<RelayUrl>,
        tx: oneshot::Sender<Result<(String, Vec<nostr::Tag>), MlsError>>,
    },
    ProcessWelcomeAndAccept {
        event_id: nostr::EventId,
        welcome_rumor: UnsignedEvent,
        tx: oneshot::Sender<Result<String, MlsError>>,
    },
    ProcessWelcomeUnsigned {
        event_id: nostr::EventId,
        welcome_rumor: UnsignedEvent,
        tx: oneshot::Sender<Result<(), MlsError>>,
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
        rumor: UnsignedEvent,
        tx: oneshot::Sender<Result<Event, MlsError>>,
    },
    CreateGroup {
        creator: PublicKey,
        recipient: PublicKey,
        key_package: Event,
        group_name: String,
        relays: Vec<RelayUrl>,
        tx: oneshot::Sender<Result<(String, UnsignedEvent), MlsError>>,
    },
    AddMember {
        wire_id: String,
        recipient: PublicKey,
        key_package: Event,
        tx: oneshot::Sender<Result<(UnsignedEvent, Event), MlsError>>,
    },
    ResolveWireId {
        wire_id: String,
        tx: oneshot::Sender<Result<Vec<u8>, MlsError>>,
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
    ListGroups {
        tx: oneshot::Sender<Result<Vec<MlsGroupListEntry>, MlsError>>,
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

        prepare_mls_db_dir(&db_path)?;

        // Reject a database file that is already a symlink. The parent
        // directory has already been hardened; this catches the case where the
        // file path itself is a symlink to a sensitive location.
        if let Ok(meta) = std::fs::symlink_metadata(&db_path)
            && meta.file_type().is_symlink()
        {
            return Err(MlsError::InsecurePath(format!(
                "MLS database path is a symlink: {}",
                db_path.display()
            )));
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
        set_db_permissions(&db_path)?;

        let storage = MdkSqliteStorage::new(&db_path)?;
        set_db_permissions(&db_path)?;
        wal_conn.execute("DROP TABLE IF EXISTS _pacto_mls_wal_trigger;", [])?;

        let engine = MDK::new(storage);

        // Spawn the worker thread that owns the engine
        let (tx, mut rx) = mpsc::channel::<MlsCommand>(32);

        std::thread::spawn(move || {
            // Worker thread: own the engine and process commands serially
            let engine = engine;

            while let Some(cmd) = rx.blocking_recv() {
                match cmd {
                    MlsCommand::CreateKeyPackage { pubkey, relays, tx } => {
                        let result: Result<(String, Vec<nostr::Tag>), MlsError> =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                engine
                                    .create_key_package_for_event(&pubkey, relays)
                                    .map_err(MlsError::from)
                                    .map(|(encoded, tags)| (encoded, tags.to_vec()))
                            }))
                            .unwrap_or_else(|e| {
                                tracing::error!(panic = ?e, "MLS worker panic");
                                Err(MlsError::Engine("MLS worker panic".into()))
                            });
                        let _ = tx.send(result);
                    }
                    MlsCommand::ProcessWelcomeAndAccept {
                        event_id,
                        welcome_rumor,
                        tx,
                    } => {
                        let result: Result<String, MlsError> =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                (|| {
                                    engine.process_welcome(&event_id, &welcome_rumor)?;

                                    let welcomes = engine.get_pending_welcomes()?;
                                    let welcome =
                                        welcomes.first().ok_or(MlsError::NotInitialized)?;
                                    engine.accept_welcome(welcome)?;

                                    let groups = engine.get_groups()?;
                                    let group = groups.first().ok_or(MlsError::NotInitialized)?;
                                    Ok(hex::encode(group.nostr_group_id.as_slice()))
                                })()
                            }))
                            .unwrap_or_else(|e| {
                                tracing::error!(panic = ?e, "MLS worker panic");
                                Err(MlsError::Engine("MLS worker panic".into()))
                            });
                        let _ = tx.send(result);
                    }
                    MlsCommand::ProcessWelcomeUnsigned {
                        event_id,
                        welcome_rumor,
                        tx,
                    } => {
                        let result: Result<(), MlsError> =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                (|| {
                                    engine.process_welcome(&event_id, &welcome_rumor)?;
                                    Ok(())
                                })()
                            }))
                            .unwrap_or_else(|e| {
                                tracing::error!(panic = ?e, "MLS worker panic");
                                Err(MlsError::Engine("MLS worker panic".into()))
                            });
                        let _ = tx.send(result);
                    }
                    MlsCommand::ProcessWelcome {
                        event_id,
                        welcome_rumor,
                        tx,
                    } => {
                        let result: Result<(), MlsError> =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                (|| {
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
                                })()
                            }))
                            .unwrap_or_else(|e| {
                                tracing::error!(panic = ?e, "MLS worker panic");
                                Err(MlsError::Engine("MLS worker panic".into()))
                            });
                        let _ = tx.send(result);
                    }
                    MlsCommand::AcceptPendingWelcome { tx } => {
                        let result: Result<GroupInfo, MlsError> =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                (|| {
                                    let welcomes = engine.get_pending_welcomes()?;
                                    let welcome =
                                        welcomes.first().ok_or(MlsError::NotInitialized)?;
                                    engine.accept_welcome(welcome)?;

                                    // Get the group info for the accepted group
                                    let groups = engine.get_groups()?;
                                    let group = groups.first().ok_or(MlsError::NotInitialized)?;
                                    Ok(GroupInfo {
                                        mls_group_id: group.mls_group_id.as_slice().to_vec(),
                                        nostr_group_id: group.nostr_group_id.to_vec(),
                                        name: group.name.clone(),
                                    })
                                })()
                            }))
                            .unwrap_or_else(|e| {
                                tracing::error!(panic = ?e, "MLS worker panic");
                                Err(MlsError::Engine("MLS worker panic".into()))
                            });
                        let _ = tx.send(result);
                    }
                    MlsCommand::CreateGroup {
                        creator,
                        recipient,
                        key_package,
                        group_name,
                        relays,
                        tx,
                    } => {
                        let result: Result<(String, UnsignedEvent), MlsError> =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                (|| {
                                    validate_key_package(&key_package, &recipient)?;

                                    let image_hash = random_bytes::<32>()?;
                                    let image_key = random_bytes::<32>()?;
                                    let image_nonce = random_bytes::<12>()?;

                                    let config = NostrGroupConfigData::new(
                                        group_name,
                                        String::new(),
                                        Some(image_hash),
                                        Some(image_key),
                                        Some(image_nonce),
                                        relays,
                                        vec![creator],
                                    );

                                    let result =
                                        engine.create_group(&creator, vec![key_package], config)?;
                                    let wire_id =
                                        hex::encode(result.group.nostr_group_id.as_slice());
                                    let welcome_rumor =
                                        result.welcome_rumors.into_iter().next().ok_or_else(
                                            || MlsError::Engine("missing welcome rumor".into()),
                                        )?;
                                    Ok((wire_id, welcome_rumor))
                                })()
                            }))
                            .unwrap_or_else(|e| {
                                tracing::error!(panic = ?e, "MLS worker panic");
                                Err(MlsError::Engine("MLS worker panic".into()))
                            });
                        let _ = tx.send(result);
                    }
                    MlsCommand::AddMember {
                        wire_id,
                        recipient,
                        key_package,
                        tx,
                    } => {
                        let result: Result<(UnsignedEvent, Event), MlsError> =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                (|| {
                                    let groups = engine.get_groups()?;
                                    let group = groups
                                        .iter()
                                        .find(|g| {
                                            hex::encode(g.nostr_group_id.as_slice()) == wire_id
                                        })
                                        .ok_or(MlsError::GroupNotFound)?;

                                    validate_key_package(&key_package, &recipient)?;

                                    let update =
                                        engine.add_members(&group.mls_group_id, &[key_package])?;
                                    engine.merge_pending_commit(&group.mls_group_id)?;

                                    let welcome_rumor = update
                                        .welcome_rumors
                                        .and_then(|v| v.into_iter().next())
                                        .ok_or_else(|| {
                                            MlsError::Engine("missing welcome rumor".into())
                                        })?;
                                    Ok((welcome_rumor, update.evolution_event))
                                })()
                            }))
                            .unwrap_or_else(|e| {
                                tracing::error!(panic = ?e, "MLS worker panic");
                                Err(MlsError::Engine("MLS worker panic".into()))
                            });
                        let _ = tx.send(result);
                    }
                    MlsCommand::CreateGroupMessage {
                        group_id,
                        rumor,
                        tx,
                    } => {
                        let result: Result<Event, MlsError> =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                (|| {
                                    let group_id = GroupId::from_slice(&group_id);
                                    let event = engine.create_message(&group_id, rumor)?;
                                    Ok(event)
                                })()
                            }))
                            .unwrap_or_else(|e| {
                                tracing::error!(panic = ?e, "MLS worker panic");
                                Err(MlsError::Engine("MLS worker panic".into()))
                            });
                        let _ = tx.send(result);
                    }
                    MlsCommand::DecryptGroupMessage { event, tx } => {
                        let result: Result<Option<DecryptedMessage>, MlsError> =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                let group_id = event
                                    .tags
                                    .iter()
                                    .find(|t| t.kind() == nostr::TagKind::h())
                                    .and_then(|t| t.content())
                                    .map(|s| s.to_string());

                                match group_id {
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
                                            | MessageProcessingResult::ExternalJoinProposal {
                                                ..
                                            }
                                            | MessageProcessingResult::Unprocessable { .. },
                                        ) => Ok(None),
                                        Err(_) => Err(MlsError::Engine(
                                            "failed to process group message".into(),
                                        )),
                                    },
                                    None => Err(MlsError::Engine("missing group id h tag".into())),
                                }
                            }))
                            .unwrap_or_else(|e| {
                                tracing::error!(panic = ?e, "MLS worker panic");
                                Err(MlsError::Engine("MLS worker panic".into()))
                            });
                        let _ = tx.send(result);
                    }
                    MlsCommand::HasGroupWithWireId { group_id, tx } => {
                        let result: Result<bool, MlsError> =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                (|| {
                                    let groups = engine.get_groups()?;
                                    Ok(groups.iter().any(|g| {
                                        hex::encode(g.nostr_group_id.as_slice()) == group_id
                                    }))
                                })()
                            }))
                            .unwrap_or_else(|e| {
                                tracing::error!(panic = ?e, "MLS worker panic");
                                Err(MlsError::Engine("MLS worker panic".into()))
                            });
                        let _ = tx.send(result);
                    }
                    MlsCommand::ListGroups { tx } => {
                        let result: Result<Vec<MlsGroupListEntry>, MlsError> =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                (|| {
                                    let groups = engine.get_groups()?;
                                    let mut entries = Vec::with_capacity(groups.len());
                                    for group in groups {
                                        let members = engine
                                            .get_members(&group.mls_group_id)?
                                            .into_iter()
                                            .map(|p| p.to_owned())
                                            .collect();
                                        entries.push(MlsGroupListEntry {
                                            wire_id: hex::encode(group.nostr_group_id.as_slice()),
                                            name: group.name.clone(),
                                            members,
                                            mls_group_id: group.mls_group_id.as_slice().to_vec(),
                                        });
                                    }
                                    Ok(entries)
                                })()
                            }))
                            .unwrap_or_else(|e| {
                                tracing::error!(panic = ?e, "MLS worker panic");
                                Err(MlsError::Engine("MLS worker panic".into()))
                            });
                        let _ = tx.send(result);
                    }
                    MlsCommand::ResolveWireId { wire_id, tx } => {
                        let result: Result<Vec<u8>, MlsError> =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                (|| {
                                    let groups = engine.get_groups()?;
                                    let group = groups
                                        .iter()
                                        .find(|g| {
                                            hex::encode(g.nostr_group_id.as_slice()) == wire_id
                                        })
                                        .ok_or(MlsError::GroupNotFound)?;
                                    Ok(group.mls_group_id.as_slice().to_vec())
                                })()
                            }))
                            .unwrap_or_else(|e| {
                                tracing::error!(panic = ?e, "MLS worker panic");
                                Err(MlsError::Engine("MLS worker panic".into()))
                            });
                        let _ = tx.send(result);
                    }
                    MlsCommand::IsGroupMember {
                        wire_id,
                        member,
                        tx,
                    } => {
                        let result: Result<bool, MlsError> =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                (|| {
                                    let groups = engine.get_groups()?;
                                    let group = groups
                                        .iter()
                                        .find(|g| {
                                            hex::encode(g.nostr_group_id.as_slice()) == wire_id
                                        })
                                        .ok_or(MlsError::GroupNotFound)?;
                                    let members = engine.get_members(&group.mls_group_id)?;
                                    Ok(members.contains(&member))
                                })()
                            }))
                            .unwrap_or_else(|e| {
                                tracing::error!(panic = ?e, "MLS worker panic");
                                Err(MlsError::Engine("MLS worker panic".into()))
                            });
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

    /// Process a received Welcome message and return the Squad wire id.
    ///
    /// This is a convenience wrapper that decrypts and validates the Welcome,
    /// accepts it, and returns the hex-encoded Nostr group id. Use this when
    /// the welcome rumor has already been unwrapped from a NIP-59 gift-wrap.
    pub async fn process_welcome_and_return_wire_id(
        &self,
        event_id: nostr::EventId,
        welcome_rumor: UnsignedEvent,
    ) -> Result<String, MlsError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(MlsCommand::ProcessWelcomeAndAccept {
                event_id,
                welcome_rumor,
                tx,
            })
            .await
            .map_err(|_| MlsError::WorkerDisconnected)?;
        rx.await.map_err(|_| MlsError::WorkerDisconnected)?
    }

    /// Process a received Welcome message from an unsigned rumor.
    ///
    /// This decrypts and validates the Welcome, persisting the group state.
    /// Use this when the welcome rumor has already been unwrapped from a
    /// NIP-59 gift-wrap and only the unsigned rumor is available.
    pub async fn process_welcome_unsigned(
        &self,
        event_id: nostr::EventId,
        welcome_rumor: UnsignedEvent,
    ) -> Result<(), MlsError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(MlsCommand::ProcessWelcomeUnsigned {
                event_id,
                welcome_rumor,
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

    /// Resolve a Squad wire id (`hex(nostr_group_id)`) to the raw MLS group id
    /// bytes used by the engine for creating group messages.
    ///
    /// Returns `MlsError::GroupNotFound` if no group matches the wire id.
    pub async fn resolve_wire_id(&self, wire_id: &str) -> Result<Vec<u8>, MlsError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(MlsCommand::ResolveWireId {
                wire_id: wire_id.to_string(),
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

    /// List all groups currently known to the engine.
    ///
    /// Each entry carries the Squad wire id (`hex(nostr_group_id)`), the group
    /// name, and the engine-reported member public keys. This is used on daemon
    /// startup to reconcile `agent.db` with the engine's persisted state.
    pub async fn list_groups(&self) -> Result<Vec<MlsGroupListEntry>, MlsError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(MlsCommand::ListGroups { tx })
            .await
            .map_err(|_| MlsError::WorkerDisconnected)?;
        rx.await.map_err(|_| MlsError::WorkerDisconnected)?
    }

    /// Create a new MLS group with the recipient as the initial member.
    ///
    /// Returns the Squad wire id (`hex(nostr_group_id)`) and the unsigned welcome
    /// rumor for the new member.
    pub async fn create_group(
        &self,
        creator: PublicKey,
        recipient: PublicKey,
        key_package: Event,
        group_name: String,
        relays: Vec<RelayUrl>,
    ) -> Result<(String, UnsignedEvent), MlsError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(MlsCommand::CreateGroup {
                creator,
                recipient,
                key_package,
                group_name,
                relays,
                tx,
            })
            .await
            .map_err(|_| MlsError::WorkerDisconnected)?;
        rx.await.map_err(|_| MlsError::WorkerDisconnected)?
    }

    /// Add a member to an existing MLS group identified by its wire id.
    ///
    /// Returns the unsigned welcome rumor for the new member and the signed group
    /// evolution event (kind:445) to publish to existing members.
    pub async fn add_member(
        &self,
        wire_id: &str,
        recipient: PublicKey,
        key_package: Event,
    ) -> Result<(UnsignedEvent, Event), MlsError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(MlsCommand::AddMember {
                wire_id: wire_id.to_string(),
                recipient,
                key_package,
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

/// A group entry returned by [`MlsEngineHandle::list_groups`].
///
/// This includes the Squad wire id, the human-readable group name, and the
/// current member public keys as known by the MLS engine. It is used during
/// daemon startup to reconcile any groups that exist in engine storage but
/// are missing from `agent.db`.
#[derive(Debug, Clone)]
pub struct MlsGroupListEntry {
    pub wire_id: String,
    pub name: String,
    /// MLS group members as reported by the engine.
    pub members: Vec<PublicKey>,
    /// Raw MLS group id; primarily useful for further engine queries.
    pub mls_group_id: Vec<u8>,
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

/// Create and secure the parent directory of the MLS database.
///
/// This delegates to the shared MLS path helper so config validation and
/// runtime engine startup use the same hardening logic. On Unix this creates
/// the directory with mode `0o700`, rejects symlinks and mountpoints in the
/// parent chain, and rejects paths that resolve under `/tmp` or `/dev/shm`.
fn prepare_mls_db_dir(db_path: &Path) -> Result<(), MlsError> {
    crate::mls_path::secure_ensure_mls_parent_dir(db_path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::DaemonError;
    use nostr::secp256k1::schnorr::Signature;
    use nostr::{EventBuilder, Keys, Kind};
    use std::path::PathBuf;

    /// Return a test temp directory outside of `/tmp` and `/dev/shm` so the
    /// MLS path-hardening checks do not reject the test fixtures.
    fn test_tempdir() -> tempfile::TempDir {
        let root = test_temp_root();
        std::fs::create_dir_all(&root).expect("create test temp root");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
                .expect("chmod test temp root");
        }
        tempfile::tempdir_in(root).expect("tempdir")
    }

    fn test_temp_root() -> PathBuf {
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"));
        target.join("test-temp").join("mls-unit")
    }

    async fn build_key_package(engine: &MlsEngineHandle, keys: &Keys) -> Event {
        let relays = vec![RelayUrl::parse("wss://test.relay").unwrap()];
        let (content, tags) = engine
            .publish_key_package(&keys.public_key(), relays)
            .await
            .expect("publish_key_package");
        EventBuilder::new(Kind::MlsKeyPackage, content)
            .tags(tags)
            .sign_with_keys(keys)
            .expect("sign key package")
    }

    #[test]
    fn new_persistent_creates_0600_db_and_sidecars() {
        let temp = test_tempdir();
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
        let temp = test_tempdir();
        let handle = MlsEngineHandle::new_persistent(temp.path().join("vector-mls.db"))
            .expect("new_persistent");
        let _clone = handle.clone();
    }

    #[cfg(unix)]
    #[test]
    fn new_persistent_rejects_symlink_parent() {
        let temp = test_tempdir();
        let real = temp.path().join("real");
        std::fs::create_dir_all(&real).expect("create real dir");
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        let db_path = link.join("vector-mls.db");
        let result = MlsEngineHandle::new_persistent(&db_path);
        assert!(
            matches!(result, Err(MlsError::InsecurePath(_))),
            "expected InsecurePath for symlink parent, got {result:?}"
        );
        assert!(!db_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn new_persistent_enforces_parent_dir_permissions() {
        let temp = test_tempdir();
        let parent = temp.path().join("loose-parent");
        std::fs::create_dir_all(&parent).expect("create parent");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755))
                .expect("loosen parent permissions");
        }

        let db_path = parent.join("vector-mls.db");
        let _handle = MlsEngineHandle::new_persistent(&db_path).expect("new_persistent");

        let meta = std::fs::metadata(&parent).expect("metadata");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);
    }

    #[tokio::test]
    async fn create_group_returns_wire_id_and_welcome_rumor() {
        let temp = test_tempdir();
        let creator_keys = Keys::generate();
        let recipient_keys = Keys::generate();
        let engine = MlsEngineHandle::new_persistent(temp.path().join("vector-mls.db"))
            .expect("new_persistent");

        let key_package = build_key_package(&engine, &recipient_keys).await;
        let relays = vec![RelayUrl::parse("wss://test.relay").unwrap()];
        let (wire_id, welcome_rumor) = engine
            .create_group(
                creator_keys.public_key(),
                recipient_keys.public_key(),
                key_package,
                "test-group".to_string(),
                relays,
            )
            .await
            .expect("create_group failed");

        assert_eq!(wire_id.len(), 64);
        assert!(!welcome_rumor.content.is_empty());
    }

    #[tokio::test]
    async fn add_member_returns_welcome_rumor_and_evolution_event() {
        let temp = test_tempdir();
        let creator_keys = Keys::generate();
        let member1_keys = Keys::generate();
        let member2_keys = Keys::generate();
        let engine = MlsEngineHandle::new_persistent(temp.path().join("vector-mls.db"))
            .expect("new_persistent");

        let key_package1 = build_key_package(&engine, &member1_keys).await;
        let relays = vec![RelayUrl::parse("wss://test.relay").unwrap()];
        let (wire_id, _) = engine
            .create_group(
                creator_keys.public_key(),
                member1_keys.public_key(),
                key_package1,
                "test-group".to_string(),
                relays.clone(),
            )
            .await
            .expect("create_group failed");

        let key_package2 = build_key_package(&engine, &member2_keys).await;
        let (welcome_rumor, evolution_event) = engine
            .add_member(&wire_id, member2_keys.public_key(), key_package2)
            .await
            .expect("add_member failed");

        assert!(!welcome_rumor.content.is_empty());
        assert!(!evolution_event.content.is_empty());
    }

    #[tokio::test]
    async fn add_member_unknown_wire_id_returns_group_not_found() {
        let temp = test_tempdir();
        let keys = Keys::generate();
        let engine = MlsEngineHandle::new_persistent(temp.path().join("vector-mls.db"))
            .expect("new_persistent");

        let key_package = build_key_package(&engine, &keys).await;
        let wire_id = "a".repeat(64);
        let result = engine
            .add_member(&wire_id, keys.public_key(), key_package)
            .await;

        assert!(matches!(result, Err(MlsError::GroupNotFound)));
    }

    #[tokio::test]
    async fn create_group_invalid_key_package_returns_invalid_key_package() {
        let temp = test_tempdir();
        let creator_keys = Keys::generate();
        let recipient_keys = Keys::generate();
        let engine = MlsEngineHandle::new_persistent(temp.path().join("vector-mls.db"))
            .expect("new_persistent");

        let wrong_kind = EventBuilder::new(Kind::TextNote, "not a key package")
            .sign_with_keys(&recipient_keys)
            .expect("sign");
        let result = engine
            .create_group(
                creator_keys.public_key(),
                recipient_keys.public_key(),
                wrong_kind,
                "test-group".to_string(),
                vec![],
            )
            .await;
        assert!(matches!(result, Err(MlsError::InvalidKeyPackage)));

        let empty_content = EventBuilder::new(Kind::MlsKeyPackage, "")
            .sign_with_keys(&recipient_keys)
            .expect("sign");
        let result = engine
            .create_group(
                creator_keys.public_key(),
                recipient_keys.public_key(),
                empty_content,
                "test-group".to_string(),
                vec![],
            )
            .await;
        assert!(matches!(result, Err(MlsError::InvalidKeyPackage)));

        let other_keys = Keys::generate();
        let (content, tags) = engine
            .publish_key_package(&other_keys.public_key(), vec![])
            .await
            .expect("publish_key_package");
        let wrong_author = EventBuilder::new(Kind::MlsKeyPackage, content)
            .tags(tags)
            .sign_with_keys(&other_keys)
            .expect("sign");
        let result = engine
            .create_group(
                creator_keys.public_key(),
                recipient_keys.public_key(),
                wrong_author,
                "test-group".to_string(),
                vec![],
            )
            .await;
        assert!(matches!(result, Err(MlsError::InvalidKeyPackage)));

        // Signature forgery: valid kind/content/author, but the signature is
        // invalid (does not verify against the claimed pubkey).
        let (content, tags) = engine
            .publish_key_package(&recipient_keys.public_key(), vec![])
            .await
            .expect("publish_key_package");
        let mut forged_signature = EventBuilder::new(Kind::MlsKeyPackage, content)
            .tags(tags)
            .sign_with_keys(&recipient_keys)
            .expect("sign");
        forged_signature.sig = Signature::from_slice(&[0u8; 64]).expect("signature bytes");
        let result = engine
            .create_group(
                creator_keys.public_key(),
                recipient_keys.public_key(),
                forged_signature,
                "test-group".to_string(),
                vec![],
            )
            .await;
        assert!(matches!(result, Err(MlsError::InvalidKeyPackage)));
    }

    #[tokio::test]
    async fn create_group_bad_key_package_content_maps_to_safe_engine_error() {
        let temp = test_tempdir();
        let creator_keys = Keys::generate();
        let recipient_keys = Keys::generate();
        let engine = MlsEngineHandle::new_persistent(temp.path().join("vector-mls.db"))
            .expect("new_persistent");

        let bad_key_package = EventBuilder::new(Kind::MlsKeyPackage, "invalid-key-package-content")
            .sign_with_keys(&recipient_keys)
            .expect("sign");
        let result = engine
            .create_group(
                creator_keys.public_key(),
                recipient_keys.public_key(),
                bad_key_package,
                "test-group".to_string(),
                vec![],
            )
            .await;

        match result {
            Err(MlsError::Engine(msg)) => {
                assert!(!msg.contains("invalid-key-package-content"));
                assert_eq!(msg, "MLS engine failure");
            }
            Err(MlsError::CryptoError) => {
                // MDK may classify malformed key packages as crypto errors; this
                // is acceptable as long as it is not the raw MDK string.
            }
            Err(other) => panic!("unexpected error: {other:?}"),
            Ok(_) => panic!("expected error for invalid key package content"),
        }
    }

    #[test]
    fn mdk_error_group_not_found_maps_to_group_not_found() {
        let err: MlsError = mdk_core::Error::GroupNotFound.into();
        assert!(matches!(err, MlsError::GroupNotFound));
    }

    #[test]
    fn mdk_error_crypto_maps_to_crypto_error() {
        use openmls_traits::types::CryptoError;
        let err: MlsError = mdk_core::Error::Crypto(CryptoError::CryptoLibraryError).into();
        assert!(matches!(err, MlsError::CryptoError));
    }

    #[test]
    fn mdk_error_unmapped_variant_maps_to_engine_fallback() {
        let err: MlsError = mdk_core::Error::KeyPackage("malformed key package".into()).into();
        match err {
            MlsError::Engine(msg) => assert_eq!(msg, "MLS engine failure"),
            other => panic!("unexpected MlsError: {other:?}"),
        }
    }

    #[tokio::test]
    async fn validate_key_package_accepts_valid_key_package() {
        let recipient_keys = Keys::generate();
        let key_package = EventBuilder::new(Kind::MlsKeyPackage, "valid key package")
            .sign_with_keys(&recipient_keys)
            .expect("sign");

        let result = validate_key_package(&key_package, &recipient_keys.public_key());
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn validate_key_package_rejects_invalid_signature() {
        let recipient_keys = Keys::generate();
        let mut key_package = EventBuilder::new(Kind::MlsKeyPackage, "valid key package")
            .sign_with_keys(&recipient_keys)
            .expect("sign");
        key_package.sig = Signature::from_slice(&[0u8; 64]).expect("signature bytes");

        let result = validate_key_package(&key_package, &recipient_keys.public_key());
        assert!(matches!(result, Err(MlsError::InvalidKeyPackage)));
        assert_eq!(
            DaemonError::from(result.unwrap_err()).to_json_rpc_code(),
            -32018
        );
    }

    #[tokio::test]
    async fn validate_key_package_rejects_wrong_kind() {
        let recipient_keys = Keys::generate();
        let key_package = EventBuilder::new(Kind::TextNote, "not a key package")
            .sign_with_keys(&recipient_keys)
            .expect("sign");

        let result = validate_key_package(&key_package, &recipient_keys.public_key());
        assert!(matches!(result, Err(MlsError::InvalidKeyPackage)));
        assert_eq!(
            DaemonError::from(result.unwrap_err()).to_json_rpc_code(),
            -32018
        );
    }

    #[tokio::test]
    async fn validate_key_package_rejects_wrong_author() {
        let author_keys = Keys::generate();
        let recipient_keys = Keys::generate();
        let key_package = EventBuilder::new(Kind::MlsKeyPackage, "valid key package")
            .sign_with_keys(&author_keys)
            .expect("sign");

        let result = validate_key_package(&key_package, &recipient_keys.public_key());
        assert!(matches!(result, Err(MlsError::InvalidKeyPackage)));
        assert_eq!(
            DaemonError::from(result.unwrap_err()).to_json_rpc_code(),
            -32018
        );
    }
}
