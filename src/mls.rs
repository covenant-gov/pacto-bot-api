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

use std::path::{Path, PathBuf};
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

    /// Raw `rusqlite` error when enabling WAL or inspecting the MLS database.
    #[error("MLS sqlite error")]
    Rusqlite(#[from] rusqlite::Error),

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
    db_path: PathBuf,
}

impl std::fmt::Debug for MlsEngineHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MlsEngineHandle")
            .field("db_path", &self.db_path)
            .finish_non_exhaustive()
    }
}

impl Clone for MlsEngineHandle {
    fn clone(&self) -> Self {
        Self {
            engine: Arc::clone(&self.engine),
            db_path: self.db_path.clone(),
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
    /// The parent directory is created before the database is opened. WAL mode
    /// is enabled before `MdkSqliteStorage` opens its own connections so that
    /// the `-wal` and `-shm` sidecars are created and can be permissioned at
    /// initialization time.
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
        let handle = Self {
            engine: Arc::new(Mutex::new(engine)),
            db_path,
        };

        // The engine's own connections keep WAL active; the temporary connection
        // can now be safely dropped.
        drop(wal_conn);
        Ok(handle)
    }

    /// Clone the inner engine Arc for use inside a `spawn_blocking` closure.
    ///
    /// Callers must lock the mutex, use the engine synchronously, and drop the
    /// lock before returning to the async runtime.
    pub(crate) fn engine(&self) -> Arc<Mutex<MDK<MdkSqliteStorage>>> {
        Arc::clone(&self.engine)
    }

    /// Return the path to the underlying `vector-mls.db` file.
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }
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

        let handle = MlsEngineHandle::new_persistent(&db_path).expect("new_persistent");
        assert!(db_path.exists());
        assert!(db_path.with_extension("db-wal").exists());
        assert!(db_path.with_extension("db-shm").exists());
        assert!(Arc::strong_count(&handle.engine) >= 1);

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
