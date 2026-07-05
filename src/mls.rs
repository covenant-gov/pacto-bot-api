//! MLS engine wrapper for the daemon.
//!
//! The `mdk_core::MDK<MdkSqliteStorage>` engine is `Send` but not `Sync` because
//! `rusqlite::Connection` contains `RefCell` state. To share the engine across
//! Tokio tasks while keeping every engine call on a blocking thread, the handle
//! wraps it in `std::sync::Mutex` and invokes it via `tokio::task::spawn_blocking`.
//!
//! The engine is never held across an `.await` point: the `Arc<Mutex<MDK>>` is
//! cloned into the blocking closure, the mutex is locked, the synchronous engine
//! call runs, the lock is dropped, and the result is returned to the async caller.

use std::path::Path;
use std::sync::{Arc, Mutex};

use mdk_core::prelude::*;
use mdk_sqlite_storage::MdkSqliteStorage;

/// Errors that can occur when interacting with the MLS engine.
#[derive(Debug, thiserror::Error)]
pub enum MlsError {
    /// SQLite or storage-layer error from the MDK backend.
    #[error("MLS storage error")]
    Storage(#[from] mdk_sqlite_storage::error::Error),

    /// Filesystem permission error when securing the MLS database.
    #[error("MLS filesystem error")]
    Io(#[from] std::io::Error),

    /// Generic engine failure; the message must not contain key material.
    #[error("MLS engine error")]
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
}

/// Cloneable handle to the per-bot MLS engine.
///
/// Cloning is cheap: it clones the inner `Arc`, not the engine itself. The
/// engine is protected by a `std::sync::Mutex` so it can be shared across Tokio
/// tasks while remaining `Send` and `Sync`.
pub struct MlsEngineHandle {
    engine: Arc<Mutex<MDK<MdkSqliteStorage>>>,
}

impl std::fmt::Debug for MlsEngineHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MlsEngineHandle").finish_non_exhaustive()
    }
}

impl Clone for MlsEngineHandle {
    fn clone(&self) -> Self {
        Self {
            engine: Arc::clone(&self.engine),
        }
    }
}

const _: () = {
    // Verify that the wrapped handle type is `Send + Sync`, which is what
    // actually moves across Tokio task boundaries. `MDK<MdkSqliteStorage>` is
    // `Send` but not `Sync`, so the mutex is required.
    fn assert_send_sync<T: Send + Sync>() {}
    let _ = assert_send_sync::<Arc<Mutex<MDK<MdkSqliteStorage>>>>;
};

impl MlsEngineHandle {
    /// Create a persistent MLS engine backed by `vector-mls.db` at `db_path`.
    ///
    /// The parent directory is created by `MdkSqliteStorage::new`. After the
    /// database file is created, this constructor enforces `0o600` permissions
    /// on it.
    pub fn new_persistent<P: AsRef<Path>>(db_path: P) -> Result<Self, MlsError> {
        let storage = MdkSqliteStorage::new(db_path.as_ref())?;
        set_db_permissions(db_path.as_ref())?;
        let engine = MDK::new(storage);
        Ok(Self {
            engine: Arc::new(Mutex::new(engine)),
        })
    }

    /// Clone the inner engine Arc for use inside a `spawn_blocking` closure.
    ///
    /// Callers must lock the mutex, use the engine synchronously, and drop the
    /// lock before returning to the async runtime.
    pub(crate) fn engine(&self) -> Arc<Mutex<MDK<MdkSqliteStorage>>> {
        Arc::clone(&self.engine)
    }
}

/// Enforce `0o600` on the main SQLite database file.
///
/// SQLite creates files using the process umask, so an explicit `set_permissions`
/// call is required even when the parent directory is `0o700`. WAL/SHM sidecars
/// are handled in `U2` after the first write triggers WAL creation.
#[cfg(unix)]
pub(crate) fn set_db_permissions<P: AsRef<Path>>(db_path: P) -> Result<(), MlsError> {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(db_path.as_ref(), fs::Permissions::from_mode(0o600))?;
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
    fn new_persistent_creates_0600_db() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("vector-mls.db");

        let handle = MlsEngineHandle::new_persistent(&db_path).expect("new_persistent");
        assert!(db_path.exists());
        assert!(Arc::strong_count(&handle.engine) >= 1);

        let meta = std::fs::metadata(&db_path).expect("metadata");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
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
