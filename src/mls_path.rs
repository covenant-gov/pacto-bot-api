//! Shared MLS database path hardening helpers.
//!
//! Both config validation and runtime engine startup must ensure the MLS
//! database directory is created safely, outside of shared temporary
//! directories, with no symlinks or mountpoints in the parent chain, and
//! with owner-only Unix permissions. These helpers centralize that logic so
//! the two call sites cannot drift.

use std::path::{Path, PathBuf};

/// Errors that can occur when validating or securing an MLS database path.
#[derive(Debug, thiserror::Error)]
pub enum MlsPathError {
    /// The database path has no parent directory.
    #[error("MLS database path has no parent directory")]
    NoParent,

    /// The database path is not absolute.
    #[error("MLS database path is not absolute: {0}")]
    NotAbsolute(PathBuf),

    /// A symlink was found in the database path or its parent chain.
    #[error("MLS database path contains a symlink: {0}")]
    Symlink(PathBuf),

    /// The database path resolves under `/tmp` or `/dev/shm`.
    #[error("MLS database path resolves under /tmp or /dev/shm")]
    SharedTemp,

    /// A parent directory is a mountpoint.
    #[error("MLS database path parent is a mountpoint: {0}")]
    Mountpoint(PathBuf),

    /// A parent directory is not a directory.
    #[error("MLS database path parent is not a directory: {0}")]
    NotADirectory(PathBuf),

    /// A parent directory is readable or writable by group/other.
    #[error("MLS database path parent is not owner-only: {0}")]
    NotOwnerOnly(PathBuf),

    /// An underlying I/O operation failed.
    #[error("MLS database path filesystem error: {0}")]
    Io(#[from] std::io::Error),
}

/// Validate and harden the parent directory of an MLS database path.
///
/// The path must be absolute. On Unix this creates the parent directory (and
/// any missing ancestors) with mode `0o700`, rejects symlinks anywhere in the
/// parent chain, rejects paths that resolve under `/tmp` or `/dev/shm`, and
/// rejects mountpoints in the parent chain. On other platforms it is
/// equivalent to `create_dir_all` plus basic directory validation.
///
/// Returns the canonicalized parent directory on success.
pub fn secure_ensure_mls_parent_dir(db_path: &Path) -> Result<PathBuf, MlsPathError> {
    if !db_path.is_absolute() {
        return Err(MlsPathError::NotAbsolute(db_path.to_path_buf()));
    }

    let parent = db_path.parent().ok_or(MlsPathError::NoParent)?;

    // Reject any symlinks in the parent chain before creating directories, so
    // an attacker cannot redirect a newly created directory into a sensitive
    // location.
    if path_contains_symlink(parent) {
        return Err(MlsPathError::Symlink(parent.to_path_buf()));
    }

    // Create the parent directory (and any missing ancestors) with explicit
    // owner-only permissions. `DirBuilder::create` with `recursive(true)` is
    // equivalent to `create_dir_all` and returns `Ok` if the path already
    // exists, so we unconditionally harden the final directory afterwards.
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(parent)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(parent)?;
    }

    // Harden the directory to owner-only permissions and verify it is safe.
    let canonical = validate_parent_structure(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&canonical, std::fs::Permissions::from_mode(0o700))?;
    }
    validate_parent_permissions(&canonical)?;

    Ok(canonical)
}

/// Validate that `parent` is a real directory in a safe location.
fn validate_parent_structure(parent: &Path) -> Result<PathBuf, MlsPathError> {
    let meta = std::fs::symlink_metadata(parent)?;
    if meta.file_type().is_symlink() {
        return Err(MlsPathError::Symlink(parent.to_path_buf()));
    }
    if !meta.is_dir() {
        return Err(MlsPathError::NotADirectory(parent.to_path_buf()));
    }

    // Reject shared temp directories and mountpoints in the parent chain.
    let canonical = parent.canonicalize()?;
    let tmp = Path::new("/tmp");
    let shm = Path::new("/dev/shm");
    if canonical.starts_with(tmp) || canonical.starts_with(shm) {
        return Err(MlsPathError::SharedTemp);
    }
    if is_mountpoint(&canonical)? {
        return Err(MlsPathError::Mountpoint(canonical.clone()));
    }

    Ok(canonical)
}

/// Validate that `parent` is readable and writable only by the owner.
fn validate_parent_permissions(parent: &Path) -> Result<(), MlsPathError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::symlink_metadata(parent)?;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(MlsPathError::NotOwnerOnly(parent.to_path_buf()));
        }
    }
    Ok(())
}

/// Walk from `path` up to the root and return `true` if any component is a
/// symlink.
fn path_contains_symlink(path: &Path) -> bool {
    let mut current = path;
    loop {
        if let Ok(meta) = std::fs::symlink_metadata(current)
            && meta.file_type().is_symlink()
        {
            return true;
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent,
            _ => break,
        }
    }
    false
}

/// Return `true` if `path` is a mountpoint (its device differs from its
/// parent's device).
#[cfg(unix)]
fn is_mountpoint(path: &Path) -> Result<bool, MlsPathError> {
    use std::os::unix::fs::MetadataExt;

    let Some(parent) = path.parent() else {
        return Ok(false);
    };
    let path_dev = std::fs::metadata(path)?.dev();
    let parent_dev = std::fs::metadata(parent)?.dev();
    Ok(path_dev != parent_dev)
}

#[cfg(not(unix))]
fn is_mountpoint(_path: &Path) -> Result<bool, MlsPathError> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_temp_root() -> PathBuf {
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"));
        target.join("test-temp").join("mls-path-unit")
    }

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

    #[test]
    fn creates_parent_with_owner_only_permissions() {
        let temp = test_tempdir();
        let db_path = temp.path().join("nested").join("vector-mls.db");

        let parent = secure_ensure_mls_parent_dir(&db_path).expect("ensure parent");

        assert!(parent.exists());
        assert_eq!(parent, temp.path().join("nested").canonicalize().unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(&parent).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o700);
        }
    }

    #[test]
    fn rejects_non_absolute_path() {
        let err = secure_ensure_mls_parent_dir(Path::new("relative/vector-mls.db"))
            .expect_err("expected non-absolute rejection");
        assert!(matches!(err, MlsPathError::NotAbsolute(_)), "{err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_parent() {
        let temp = test_tempdir();
        let real = temp.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let db_path = link.join("vector-mls.db");
        let err = secure_ensure_mls_parent_dir(&db_path).expect_err("expected symlink rejection");
        assert!(matches!(err, MlsPathError::Symlink(_)), "{err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn tightens_loose_parent_permissions() {
        let temp = test_tempdir();
        let parent = temp.path().join("loose");
        std::fs::create_dir_all(&parent).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let db_path = parent.join("vector-mls.db");
        let canonical = secure_ensure_mls_parent_dir(&db_path).expect("should tighten and succeed");

        assert_eq!(canonical, parent.canonicalize().unwrap());
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(&canonical).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);
    }
}
