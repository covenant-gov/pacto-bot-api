//! Per-bot MLS store encryption key provider.
//!
//! MDK 0.8.0 makes the MLS SQLCipher store's encryption key mandatory in a
//! production build — `MdkSqliteStorage::new_unencrypted` is
//! `#[cfg(any(test, feature = "test-utils"))]` only, and `MdkSqliteStorage::new`
//! sources its key from a platform OS keyring the daemon has no dependency on
//! and that is commonly unavailable on a headless Linux server. This module is
//! the daemon's own key source instead: a 32-byte key held next to the store,
//! in a file named by appending `.key` to the store's file *name* (not via
//! [`PathBuf::set_extension`], so `squad.db` and `squad.sqlite` in one directory
//! do not collide on `squad.key`).
//!
//! Two operations, with different side-effect contracts:
//!
//! - [`load`] reads an existing key and never creates one.
//! - [`load_or_create`] creates a key when none exists. It must only be
//!   called once a caller has decided a fresh store is wanted — a provider
//!   that creates on every read makes "key absent" permanently unobservable.
//!
//! The key is held as `Zeroizing<[u8; 32]>` end to end and never placed in a
//! serde-derived struct: `secrecy::Secret<T>` derives `Serialize`, so a key
//! embedded in a config- or diagnostics-shaped struct could still leak
//! through a stray `#[derive(Serialize)]` even when its `Debug` is redacted.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use zeroize::Zeroizing;

/// Errors from loading, creating, or validating an MLS store encryption key.
#[derive(Debug, thiserror::Error)]
pub enum MlsKeyError {
    /// Filesystem error reading, creating, or syncing the key file or its
    /// parent directory.
    #[error("MLS key filesystem error")]
    Io(#[source] std::io::Error),

    /// An existing key file's permissions are not owner-only.
    #[error(
        "MLS key file {path} has overly permissive mode {mode:03o}; expected 0o600 or stricter"
    )]
    PermissionTooOpen {
        /// The key file path (safe to surface: callers redact before
        /// forwarding to a client-facing error).
        path: PathBuf,
        /// The offending mode bits.
        mode: u32,
    },

    /// An existing key file's contents are not exactly 32 bytes.
    #[error("MLS key file {path} has invalid length: expected 32 bytes, got {actual} bytes")]
    InvalidLength {
        /// The key file path.
        path: PathBuf,
        /// The actual byte length found.
        actual: usize,
    },
}

/// Derive the key file path for `store_path` by appending `.key` to the
/// store's file *name*. Using the file name (not [`PathBuf::set_extension`])
/// means `squad.db` and `squad.sqlite` in the same directory resolve to
/// distinct `squad.db.key` / `squad.sqlite.key` paths rather than colliding
/// on `squad.key`.
pub fn key_path_for_store(store_path: &Path) -> PathBuf {
    let mut file_name = store_path.file_name().unwrap_or_default().to_os_string();
    file_name.push(".key");
    store_path.with_file_name(file_name)
}

/// Load the existing key for `store_path`, if any. Never creates one.
///
/// Returns `Ok(None)` when no key file exists at the derived path. Rejects
/// an existing key file whose permissions are not owner-only, or whose
/// contents are not exactly 32 bytes, without ever returning key material
/// for either rejected case.
pub fn load(store_path: &Path) -> Result<Option<Zeroizing<[u8; 32]>>, MlsKeyError> {
    let key_path = key_path_for_store(store_path);
    load_at(&key_path)
}

fn load_at(key_path: &Path) -> Result<Option<Zeroizing<[u8; 32]>>, MlsKeyError> {
    let metadata = match std::fs::metadata(key_path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(MlsKeyError::Io(e)),
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(MlsKeyError::PermissionTooOpen {
                path: key_path.to_path_buf(),
                mode,
            });
        }
    }
    #[cfg(not(unix))]
    {
        let _ = &metadata;
    }

    let mut file = std::fs::File::open(key_path).map_err(MlsKeyError::Io)?;
    let mut buf = Zeroizing::new(Vec::new());
    file.read_to_end(&mut buf).map_err(MlsKeyError::Io)?;

    if buf.len() != 32 {
        return Err(MlsKeyError::InvalidLength {
            path: key_path.to_path_buf(),
            actual: buf.len(),
        });
    }

    let mut key = Zeroizing::new([0u8; 32]);
    key.copy_from_slice(&buf);
    Ok(Some(key))
}

/// Load the existing key for `store_path`, or create one if none exists.
///
/// Only call this once a caller has already decided a fresh store is
/// wanted — see the module-level docs. The key file is created with
/// owner-only permissions from the moment it is created (never tightened
/// after the fact), `fsync`ed, and its containing directory is `fsync`ed too
/// so a torn write cannot leave a corrupt key in place. Any partially
/// written temp file is removed on failure.
pub fn load_or_create(store_path: &Path) -> Result<Zeroizing<[u8; 32]>, MlsKeyError> {
    let key_path = key_path_for_store(store_path);

    if let Some(key) = load_at(&key_path)? {
        return Ok(key);
    }

    let mut key = Zeroizing::new([0u8; 32]);
    getrandom::getrandom(&mut *key).map_err(|e| MlsKeyError::Io(std::io::Error::other(e)))?;

    let parent = key_path.parent().unwrap_or_else(|| Path::new("."));
    let tmp_path = key_path.with_extension(format!(
        "{}.tmp",
        key_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
    ));

    let write_result = (|| -> Result<(), std::io::Error> {
        let mut file = crate::secure_file::create_restricted_file(&tmp_path)?;
        file.write_all(&*key)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp_path, &key_path)?;

        // fsync the parent directory so the rename itself is durable, not
        // just the file contents — a lost key write turns into a store
        // reset, so a torn key write is self-inflicting destruction.
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();

    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(MlsKeyError::Io(e));
    }

    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_path_appends_dot_key_to_file_name() {
        let store = Path::new("/data/bots/alice/squad.db");
        assert_eq!(
            key_path_for_store(store),
            Path::new("/data/bots/alice/squad.db.key")
        );
    }

    #[test]
    fn distinct_extensions_do_not_collide_on_key_path() {
        let db = key_path_for_store(Path::new("/data/squad.db"));
        let sqlite = key_path_for_store(Path::new("/data/squad.sqlite"));
        assert_ne!(db, sqlite);
        assert_eq!(db, Path::new("/data/squad.db.key"));
        assert_eq!(sqlite, Path::new("/data/squad.sqlite.key"));
    }

    #[test]
    fn load_on_fresh_path_returns_none_and_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("squad.db");
        assert!(load(&store).unwrap().is_none());
        assert!(!key_path_for_store(&store).exists());
    }

    #[test]
    fn load_or_create_on_fresh_dir_produces_a_0600_key_and_load_returns_the_same_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("squad.db");

        let created = load_or_create(&store).unwrap();
        let key_path = key_path_for_store(&store);
        assert!(key_path.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let loaded = load(&store).unwrap().expect("key should now exist");
        assert_eq!(*created, *loaded);

        // load_or_create again must return the SAME key, not mint a new one.
        let second = load_or_create(&store).unwrap();
        assert_eq!(*created, *second);
    }

    #[cfg(unix)]
    #[test]
    fn a_0644_key_file_is_rejected_and_nothing_is_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("squad.db");
        let key_path = key_path_for_store(&store);

        std::fs::write(&key_path, [7u8; 32]).unwrap();
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let err = load(&store).unwrap_err();
        assert!(
            matches!(err, MlsKeyError::PermissionTooOpen { .. }),
            "{err:?}"
        );

        // The file must be left exactly as it was -- no repair, no overwrite.
        let mode = std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
        assert_eq!(std::fs::read(&key_path).unwrap(), vec![7u8; 32]);
    }

    #[test]
    fn a_truncated_key_file_is_rejected_with_a_distinct_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("squad.db");
        let key_path = key_path_for_store(&store);

        let mut file = crate::secure_file::create_restricted_file(&key_path).unwrap();
        file.write_all(&[1u8; 16]).unwrap();
        drop(file);

        let err = load(&store).unwrap_err();
        assert!(
            matches!(err, MlsKeyError::InvalidLength { actual: 16, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn two_stores_in_the_same_directory_get_independent_keys() {
        let dir = tempfile::tempdir().unwrap();
        let a = load_or_create(&dir.path().join("a.db")).unwrap();
        let b = load_or_create(&dir.path().join("b.db")).unwrap();
        assert_ne!(*a, *b);
    }
}
