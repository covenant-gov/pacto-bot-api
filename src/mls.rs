//! MLS engine wrapper for the daemon.
//!
//! The `mdk_core::MDK<MdkSqliteStorage>` engine is `Send` but not `Sync` because
//! `rusqlite::Connection` contains `RefCell` state. To share the engine across
//! Tokio tasks, the handle runs the engine on a dedicated single-threaded worker
//! thread. All engine calls are serialized through an mpsc channel, eliminating
//! any possibility of lock contention or ordering issues.

use std::path::{Path, PathBuf};

use mdk_core::MdkConfig;
use mdk_core::callback::MdkCallback;
use mdk_core::callback::RollbackInfo;
use mdk_core::prelude::*;
use mdk_sqlite_storage::MdkSqliteStorage;
use mdk_sqlite_storage::encryption::EncryptionConfig;
use mdk_storage_traits::GroupId;
use nostr::{Event, Kind, PublicKey, RelayUrl, Tags, UnsignedEvent};
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

    /// The MLS engine has no group state for this bot — no accepted welcome
    /// and no pending welcome were found.
    #[error("no MLS group or pending welcome")]
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

    /// Failed to load or create the store's SQLCipher encryption key.
    #[error("MLS store key error")]
    Key(#[from] crate::mls_key::MlsKeyError),
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
        mdk_core::Error::ProcessMessageWrongEpoch(_, _)
        | mdk_core::Error::ProcessMessageWrongGroupId
        | mdk_core::Error::ProcessMessageUseAfterEviction
        | mdk_core::Error::ProcessMessageOther(_) => "process_message",

        // 0.8.0 typed variants named in the parity plan -- one stable
        // category per variant. Several carry identity or group data in
        // their payload, so no arm below renders the payload into the
        // category string.
        mdk_core::Error::NotAdmin => "not_admin",
        mdk_core::Error::CommitFromNonAdmin => "commit_from_non_admin",
        mdk_core::Error::WelcomePreviouslyFailed(_) => "welcome_previously_failed",
        mdk_core::Error::CannotDecryptOwnMessage => "cannot_decrypt_own_message",
        mdk_core::Error::KeyPackageIdentityMismatch { .. } => "key_package_identity_mismatch",
        mdk_core::Error::IdentityChangeNotAllowed { .. } => "identity_change_not_allowed",
        mdk_core::Error::InviteeMissingRequiredProposal => "invitee_missing_required_proposal",
        mdk_core::Error::EmptyUpgradeSet => "empty_upgrade_set",
        mdk_core::Error::ProposalNotInSupportedSet(_) => "proposal_not_in_supported_set",
        mdk_core::Error::ProposalAlreadyRequired(_) => "proposal_already_required",
        mdk_core::Error::ProposalNotAvailableForUpgrade { .. } => {
            "proposal_not_available_for_upgrade"
        }

        // Everything else, grouped by kind. `syn`/CI has no way to remind us
        // to revisit this grouping, so this match is deliberately
        // non-exhaustive-proof: adding a new upstream variant is a compile
        // error here, not a silent fall-through.
        mdk_core::Error::Hex(_)
        | mdk_core::Error::Keys(_)
        | mdk_core::Error::Event(_)
        | mdk_core::Error::EventBuilder(_)
        | mdk_core::Error::RelayUrl(_)
        | mdk_core::Error::Tls(_)
        | mdk_core::Error::Utf8(_)
        | mdk_core::Error::OpenMlsGeneric(_)
        | mdk_core::Error::InvalidExtension(_)
        | mdk_core::Error::CreateMessage(_)
        | mdk_core::Error::BasicCredential(_) => "protocol",

        mdk_core::Error::Storage(_) => "storage",
        mdk_core::Error::Signer(_) | mdk_core::Error::CantLoadSigner => "signer",

        mdk_core::Error::ExportSecret(_) | mdk_core::Error::GroupExporterSecretNotFound => {
            "export_secret"
        }

        mdk_core::Error::MergePendingCommit(_)
        | mdk_core::Error::CommitToPendingProposalsError
        | mdk_core::Error::SelfUpdate(_)
        | mdk_core::Error::OwnCommitPending => "commit",

        mdk_core::Error::ProcessedWelcomeNotFound | mdk_core::Error::InvalidWelcomeMessage => {
            "welcome"
        }

        mdk_core::Error::Provider(_) | mdk_core::Error::NotImplemented(_) => "provider",

        mdk_core::Error::ProtocolMessage(_)
        | mdk_core::Error::ProtocolGroupIdMismatch
        | mdk_core::Error::OwnLeafNotFound
        | mdk_core::Error::UnexpectedEvent { .. }
        | mdk_core::Error::UnexpectedExtensionType
        | mdk_core::Error::NostrGroupDataExtensionNotFound
        | mdk_core::Error::MessageFromNonMember
        | mdk_core::Error::MessageNotFound
        | mdk_core::Error::UpdateGroupContextExts(_)
        | mdk_core::Error::InvalidImageHashLength
        | mdk_core::Error::InvalidImageKeyLength
        | mdk_core::Error::InvalidImageNonceLength
        | mdk_core::Error::InvalidImageUploadKeyLength
        | mdk_core::Error::InvalidExtensionVersion(_)
        | mdk_core::Error::ExtensionFormatError(_)
        | mdk_core::Error::AuthorMismatch
        | mdk_core::Error::MissingRumorEventId
        | mdk_core::Error::InvalidTimestamp(_)
        | mdk_core::Error::MissingGroupIdTag
        | mdk_core::Error::InvalidGroupIdFormat(_)
        | mdk_core::Error::MultipleGroupIdTags(_)
        | mdk_core::Error::SnapshotCreationFailed(_) => "protocol",
    }
}

impl From<crate::mls_path::MlsPathError> for MlsError {
    fn from(err: crate::mls_path::MlsPathError) -> Self {
        MlsError::InsecurePath(err.to_string())
    }
}

/// `MdkCallback` implementation registered on every engine for KTD8
/// observability. `on_rollback` is invoked synchronously from inside
/// `process_message` on the MLS worker thread, so this implementation must
/// neither await nor re-enter `MlsEngineHandle` (that would deadlock the
/// single-threaded worker). It only increments an aggregated counter --
/// never the raw `GroupId` `RollbackInfo` carries, which R16 forbids
/// logging.
#[derive(Debug, Default)]
struct MdkRollbackObserver {
    count: std::sync::atomic::AtomicU64,
}

impl MdkCallback for MdkRollbackObserver {
    fn on_rollback(&self, _info: &RollbackInfo) {
        self.count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::warn!("MLS commit race resolution rolled back a group to an earlier epoch");
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
    LeaveGroup {
        wire_id: String,
        tx: oneshot::Sender<Result<Event, MlsError>>,
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
        tx: oneshot::Sender<Result<GroupMessageOutcome, MlsError>>,
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
    rollback_observer: std::sync::Arc<MdkRollbackObserver>,
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

        // MDK 0.8.0 makes the store's SQLCipher encryption key mandatory in
        // a production build. `new_with_key` must be the first thing that
        // touches the path: unlike the pre-0.8.0 engine it pre-creates the
        // file securely itself and hardens it and its sidecars, so the old
        // priming connection and trigger-table dance is not just
        // unnecessary but actively harmful here -- a plain `rusqlite`
        // connection would write a plaintext `SQLite format 3\0` header,
        // which `new_with_key` then rejects as an unencrypted database on
        // the very next open.
        let key = crate::mls_key::load_or_create(&db_path)?;
        let storage = MdkSqliteStorage::new_with_key(&db_path, EncryptionConfig::new(*key))?;
        set_db_permissions(&db_path)?;

        // Explicitly enable WAL. MDK's own connections never set
        // journal_mode (defaulting to SQLite's rollback journal); this
        // connection reproduces the store's full keyed pragma prologue --
        // `PRAGMA key` must be the first statement on any connection to an
        // encrypted database, `cipher_compatibility` must match what
        // `MdkSqliteStorage` itself pins, and `temp_store = MEMORY` avoids
        // spilling decrypted temp data to a plaintext file on disk.
        {
            let wal_conn = rusqlite::Connection::open(&db_path)?;
            let hex_key = hex::encode(*key);
            wal_conn.execute_batch(&format!(
                "PRAGMA key = \"x'{hex_key}'\";
                 PRAGMA cipher_compatibility = 4;
                 PRAGMA temp_store = MEMORY;
                 PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;"
            ))?;
            set_db_permissions(&db_path)?;
        }

        // KTD8: name every value explicitly rather than inherit
        // `MdkConfig::default()` silently, so a future retention-window
        // question is a diff against this comment instead of an
        // investigation. These numbers currently equal MDK's own defaults.
        let config = MdkConfig {
            max_event_age_secs: 3_888_000,   // 45 days
            max_future_skew_secs: 300,       // 5 minutes
            out_of_order_tolerance: 100,     // 100 past messages
            maximum_forward_distance: 1_000, // 1000 forward messages
            max_past_epochs: 5,
            epoch_snapshot_retention: 5,
            snapshot_ttl_seconds: 604_800, // 1 week
        };
        let rollback_observer = std::sync::Arc::new(MdkRollbackObserver::default());
        let engine = MDK::builder(storage)
            .with_config(config)
            .with_callback(
                std::sync::Arc::clone(&rollback_observer) as std::sync::Arc<dyn MdkCallback>
            )
            .build();

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
                                    .map(|data| (data.content, data.tags_443))
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

                                    // Idempotency: if this welcome was already processed,
                                    // there may be no pending welcome but the group already
                                    // exists. Accept only when a pending welcome is present,
                                    // otherwise fall through to the existing group lookup.
                                    let welcomes = engine.get_pending_welcomes(None)?;
                                    if let Some(welcome) = welcomes.first() {
                                        engine.accept_welcome(welcome)?;
                                    } else {
                                        tracing::debug!(
                                            "no pending welcome after process_welcome; using existing group if present"
                                        );
                                    }

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
                                    let welcomes = engine.get_pending_welcomes(None)?;
                                    if let Some(welcome) = welcomes.first() {
                                        engine.accept_welcome(welcome)?;
                                    } else {
                                        tracing::debug!(
                                            "no pending welcome to accept; using existing group if present"
                                        );
                                    }

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
                    MlsCommand::LeaveGroup { wire_id, tx } => {
                        let result: Result<Event, MlsError> =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                (|| {
                                    let groups = engine.get_groups()?;
                                    let group = groups
                                        .iter()
                                        .find(|g| {
                                            hex::encode(g.nostr_group_id.as_slice()) == wire_id
                                        })
                                        .ok_or(MlsError::GroupNotFound)?;

                                    let update = engine.leave_group(&group.mls_group_id)?;
                                    Ok(update.evolution_event)
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
                                    let event = engine.create_message(&group_id, rumor, None)?;
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
                        let result: Result<GroupMessageOutcome, MlsError> =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                let group_id = crate::nostr_tags::h_tag_content(&event.tags);

                                match group_id {
                                    Some(group_id) => match engine.process_message(&event) {
                                        Ok(MessageProcessingResult::ApplicationMessage(msg)) => {
                                            Ok(GroupMessageOutcome::Message(DecryptedMessage {
                                                content: msg.content,
                                                kind: msg.kind,
                                                tags: msg.tags,
                                                rumor_id: msg.id.to_hex(),
                                                group_id,
                                                author: msg.pubkey.to_hex(),
                                                event_id: event.id.to_hex(),
                                                timestamp: event.created_at.as_secs(),
                                            }))
                                        }
                                        // MDK auto-committed a self-remove proposal because
                                        // this bot is an admin. The evolution event must
                                        // reach the group's relays or every peer's epoch
                                        // diverges from this bot's and stops decrypting --
                                        // surface it as a publish obligation rather than
                                        // silently dropping it.
                                        Ok(MessageProcessingResult::Proposal(update)) => {
                                            engine.merge_pending_commit(&update.mls_group_id)?;
                                            Ok(GroupMessageOutcome::PublishEvolution(
                                                update.evolution_event,
                                            ))
                                        }
                                        Ok(MessageProcessingResult::PendingProposal { .. }) => {
                                            tracing::debug!(
                                                "pending proposal stored, awaiting admin commit"
                                            );
                                            Ok(GroupMessageOutcome::None)
                                        }
                                        Ok(MessageProcessingResult::IgnoredProposal { .. }) => {
                                            // Never log `reason` or `mls_group_id` here --
                                            // both are engine-controlled payload text, not a
                                            // fixed category.
                                            tracing::debug!("proposal ignored");
                                            Ok(GroupMessageOutcome::None)
                                        }
                                        Ok(
                                            MessageProcessingResult::ExternalJoinProposal { .. }
                                            | MessageProcessingResult::Commit { .. }
                                            | MessageProcessingResult::Unprocessable { .. },
                                        ) => Ok(GroupMessageOutcome::None),
                                        Ok(MessageProcessingResult::PreviouslyFailed) => {
                                            tracing::debug!(
                                                "group message previously failed to process; not retried"
                                            );
                                            Ok(GroupMessageOutcome::None)
                                        }
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

        Ok(Self {
            tx,
            db_path,
            rollback_observer,
        })
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
    /// See [`GroupMessageOutcome`] for the three possible results.
    pub async fn decrypt_group_message(
        &self,
        event: &nostr::Event,
    ) -> Result<GroupMessageOutcome, MlsError> {
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

    /// Leave an existing MLS group identified by its wire id.
    ///
    /// Returns the signed kind:445 evolution event containing the leave
    /// proposal. The caller is responsible for publishing it to relays.
    pub async fn leave_group(&self, wire_id: &str) -> Result<Event, MlsError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(MlsCommand::LeaveGroup {
                wire_id: wire_id.to_string(),
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

    /// Return the number of MLS commit-race rollbacks observed since this
    /// engine was constructed (KTD8 observability counter).
    pub fn rollback_count(&self) -> u64 {
        self.rollback_observer
            .count
            .load(std::sync::atomic::Ordering::Relaxed)
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

/// Outcome of decrypting one kind:445 group message.
#[derive(Debug, Clone)]
pub enum GroupMessageOutcome {
    /// An application message ready to fan out to handlers.
    Message(DecryptedMessage),
    /// MDK auto-committed a self-remove proposal on this bot's behalf
    /// (this bot is an admin). The carried event is the signed evolution
    /// event (kind:445) and **must** be published to the group's relays:
    /// dropping it advances this bot's epoch locally while no peer sees
    /// the commit, and every later message in the group then fails to
    /// decrypt with no attributable error.
    PublishEvolution(Event),
    /// A protocol-only message (pending/ignored proposal, external-join
    /// proposal, commit, unprocessable, or previously-failed) that
    /// produces no handler event and needs no publish.
    None,
}

/// A decrypted MLS application message.
#[derive(Debug, Clone)]
pub struct DecryptedMessage {
    /// Plaintext content of the application message.
    pub content: String,
    /// Inner rumor kind, distinct from the kind:445 wrapper.
    pub kind: Kind,
    /// Inner rumor tags used by reaction and attachment taxonomy.
    pub tags: Tags,
    /// Inner rumor event id in hex.
    pub rumor_id: String,
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
        crate::nostr_json::sign_builder(
            EventBuilder::new(Kind::MlsKeyPackage, content).tags(tags),
            keys,
        )
        .expect("sign key package")
    }

    #[test]
    fn rollback_observer_increments_on_callback_and_starts_at_zero() {
        let observer = MdkRollbackObserver::default();
        assert_eq!(observer.count.load(std::sync::atomic::Ordering::Relaxed), 0);

        let info = RollbackInfo {
            group_id: GroupId::from_slice(&[0u8; 32]),
            target_epoch: 3,
            new_head_event: nostr::EventId::all_zeros(),
            invalidated_messages: vec![nostr::EventId::all_zeros()],
            messages_needing_refetch: vec![nostr::EventId::all_zeros()],
        };
        observer.on_rollback(&info);
        observer.on_rollback(&info);

        assert_eq!(observer.count.load(std::sync::atomic::Ordering::Relaxed), 2);
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
    async fn leave_group_returns_evolution_event() {
        // MDK 0.8.0 enforces MIP-03: an admin must self-demote before it can
        // send a SelfRemove proposal, and a sole admin has no one to demote
        // to. `MlsEngineHandle::create_group` always makes the creator the
        // sole admin (tracked as KD7/R11, deferred to a later unit), so this
        // test exercises the other side of the membership: the invited
        // recipient, who joins as a plain (non-admin) member and can leave
        // directly.
        let temp = test_tempdir();
        let creator_keys = Keys::generate();
        let recipient_keys = Keys::generate();
        let creator_engine = MlsEngineHandle::new_persistent(temp.path().join("creator-mls.db"))
            .expect("new_persistent");
        let member_engine = MlsEngineHandle::new_persistent(temp.path().join("member-mls.db"))
            .expect("new_persistent");

        let key_package = build_key_package(&member_engine, &recipient_keys).await;
        let (wire_id, welcome_rumor) = creator_engine
            .create_group(
                creator_keys.public_key(),
                recipient_keys.public_key(),
                key_package,
                "test-group".to_string(),
                vec![RelayUrl::parse("wss://test.relay").unwrap()],
            )
            .await
            .expect("create_group failed");
        member_engine
            .process_welcome_and_return_wire_id(nostr::EventId::all_zeros(), welcome_rumor)
            .await
            .expect("process_welcome_and_return_wire_id failed");

        let evolution_event = member_engine
            .leave_group(&wire_id)
            .await
            .expect("leave_group failed");

        assert!(!evolution_event.content.is_empty());
        assert_eq!(evolution_event.kind, Kind::MlsGroupMessage);
    }

    #[tokio::test]
    async fn leave_group_unknown_wire_id_returns_group_not_found() {
        let temp = test_tempdir();
        let engine = MlsEngineHandle::new_persistent(temp.path().join("vector-mls.db"))
            .expect("new_persistent");

        let wire_id = "a".repeat(64);
        let result = engine.leave_group(&wire_id).await;

        assert!(matches!(result, Err(MlsError::GroupNotFound)));
    }

    #[tokio::test]
    async fn admin_auto_commits_self_remove_proposal_and_publishes_evolution() {
        // The scenario the parity plan calls out as the one silent failure
        // mode in this migration: MDK auto-commits an inbound self-remove
        // proposal when the receiver is a group admin, but only *stages*
        // the commit -- same as `add_members`, the caller must explicitly
        // merge it. If the daemon returns the evolution event to the caller
        // without merging, the admin's own local state is stuck mid-commit;
        // if it drops the evolution event outright instead of publishing
        // it, every other member's epoch permanently diverges from the
        // admin's with no attributable error. This test proves the
        // daemon surfaces the evolution event (`PublishEvolution`), merges
        // its own commit before returning, and that a third, uninvolved
        // member can process the published evolution event without error.
        let temp = test_tempdir();
        let admin_keys = Keys::generate();
        let leaving_keys = Keys::generate();
        let staying_keys = Keys::generate();
        let admin_engine = MlsEngineHandle::new_persistent(temp.path().join("admin-mls.db"))
            .expect("new_persistent");
        let leaving_engine = MlsEngineHandle::new_persistent(temp.path().join("leaving-mls.db"))
            .expect("new_persistent");
        let staying_engine = MlsEngineHandle::new_persistent(temp.path().join("staying-mls.db"))
            .expect("new_persistent");
        let relays = vec![RelayUrl::parse("wss://test.relay").unwrap()];

        // Admin creates the group with the soon-to-leave member as the
        // initial member.
        let leaving_key_package = build_key_package(&leaving_engine, &leaving_keys).await;
        let (wire_id, welcome_rumor) = admin_engine
            .create_group(
                admin_keys.public_key(),
                leaving_keys.public_key(),
                leaving_key_package,
                "test-group".to_string(),
                relays.clone(),
            )
            .await
            .expect("create_group failed");
        leaving_engine
            .process_welcome_and_return_wire_id(nostr::EventId::all_zeros(), welcome_rumor)
            .await
            .expect("leaving member welcome failed");

        // Admin adds a third, uninvolved member. The leaving member must
        // process that addition to stay in sync before it leaves.
        let staying_key_package = build_key_package(&staying_engine, &staying_keys).await;
        let (staying_welcome, add_evolution_event) = admin_engine
            .add_member(&wire_id, staying_keys.public_key(), staying_key_package)
            .await
            .expect("add_member failed");
        staying_engine
            .process_welcome_and_return_wire_id(nostr::EventId::all_zeros(), staying_welcome)
            .await
            .expect("staying member welcome failed");
        leaving_engine
            .decrypt_group_message(&add_evolution_event)
            .await
            .expect("leaving member failed to sync the add-member commit");

        // The member leaves: MDK builds a SelfRemove proposal but does not
        // merge it locally -- the departing member cannot commit its own
        // removal, per MDK's `leave_group` contract.
        let leave_event = leaving_engine
            .leave_group(&wire_id)
            .await
            .expect("leave_group failed");

        // Admin receives the proposal and, because it is an admin,
        // auto-commits it. The daemon must surface the resulting evolution
        // event as a publish obligation rather than silently drop it.
        let outcome = admin_engine
            .decrypt_group_message(&leave_event)
            .await
            .expect("admin failed to process the self-remove proposal");
        let removal_evolution_event = match outcome {
            GroupMessageOutcome::PublishEvolution(event) => event,
            other => panic!("expected PublishEvolution, got {other:?}"),
        };

        // The evolution event is well-formed and processable by a third,
        // uninvolved member without error -- if the daemon had silently
        // dropped it instead of publishing it, that peer would never see it
        // and would stay stuck at the pre-removal membership forever.
        let outcome = staying_engine
            .decrypt_group_message(&removal_evolution_event)
            .await
            .expect("staying member failed to process the evolution event");
        assert!(
            matches!(outcome, GroupMessageOutcome::None),
            "a commit-only message should carry no handler payload, got {outcome:?}"
        );

        // Prove admin's own local state was genuinely merged, not just
        // staged: building a further group message requires the prior
        // commit to be applied, so this call fails if `merge_pending_commit`
        // was skipped after the auto-commit.
        let group_id_bytes = admin_engine
            .resolve_wire_id(&wire_id)
            .await
            .expect("resolve_wire_id failed");
        let rumor = nostr::UnsignedEvent::new(
            admin_keys.public_key(),
            nostr::Timestamp::now(),
            Kind::TextNote,
            Vec::new(),
            "still here after the removal commit",
        );
        admin_engine
            .create_group_message(group_id_bytes, rumor)
            .await
            .expect(
                "admin failed to build a further group message; \
                 the auto-committed proposal was not actually merged",
            );
    }

    #[tokio::test]
    async fn create_group_invalid_key_package_returns_invalid_key_package() {
        let temp = test_tempdir();
        let creator_keys = Keys::generate();
        let recipient_keys = Keys::generate();
        let engine = MlsEngineHandle::new_persistent(temp.path().join("vector-mls.db"))
            .expect("new_persistent");

        let wrong_kind = crate::nostr_json::sign_builder(
            EventBuilder::new(Kind::TextNote, "not a key package"),
            &recipient_keys,
        )
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

        let empty_content = crate::nostr_json::sign_builder(
            EventBuilder::new(Kind::MlsKeyPackage, ""),
            &recipient_keys,
        )
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
        let wrong_author = crate::nostr_json::sign_builder(
            EventBuilder::new(Kind::MlsKeyPackage, content).tags(tags),
            &other_keys,
        )
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
        let mut forged_signature = crate::nostr_json::sign_builder(
            EventBuilder::new(Kind::MlsKeyPackage, content).tags(tags),
            &recipient_keys,
        )
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

        let bad_key_package = crate::nostr_json::sign_builder(
            EventBuilder::new(Kind::MlsKeyPackage, "invalid-key-package-content"),
            &recipient_keys,
        )
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
        let key_package = crate::nostr_json::sign_builder(
            EventBuilder::new(Kind::MlsKeyPackage, "valid key package"),
            &recipient_keys,
        )
        .expect("sign");

        let result = validate_key_package(&key_package, &recipient_keys.public_key());
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn validate_key_package_rejects_invalid_signature() {
        let recipient_keys = Keys::generate();
        let mut key_package = crate::nostr_json::sign_builder(
            EventBuilder::new(Kind::MlsKeyPackage, "valid key package"),
            &recipient_keys,
        )
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
        let key_package = crate::nostr_json::sign_builder(
            EventBuilder::new(Kind::TextNote, "not a key package"),
            &recipient_keys,
        )
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
        let key_package = crate::nostr_json::sign_builder(
            EventBuilder::new(Kind::MlsKeyPackage, "valid key package"),
            &author_keys,
        )
        .expect("sign");

        let result = validate_key_package(&key_package, &recipient_keys.public_key());
        assert!(matches!(result, Err(MlsError::InvalidKeyPackage)));
        assert_eq!(
            DaemonError::from(result.unwrap_err()).to_json_rpc_code(),
            -32018
        );
    }
}
