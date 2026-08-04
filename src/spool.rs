//! Spool directory management (KTD4).
//!
//! `$DATA_DIR/spool/inbound/` holds decrypted attachment payloads fetched
//! from a counterparty; `$DATA_DIR/spool/outbound/` is where a handler
//! process stages a payload before asking the daemon to send it. Both
//! directories are created and kept at mode `0o700`, following the same
//! symlink and shared-temp-directory guards `secure_ensure_mls_parent_dir`
//! (`src/mls_path.rs`) already applies to the MLS database.
//!
//! [`Spool::resolve_outbound`] implements the path-confinement decision in
//! KTD5: canonicalize, require a strict component-wise descendant of the
//! canonicalized outbound root (never a string `starts_with`, which would
//! accept a sibling directory sharing the root's name as a prefix), then
//! re-verify through the *opened handle's* metadata that the target is a
//! regular file.
//!
//! [`Spool::sweep`] is an amortized retention sweep gated by an elapsed-time
//! interval plus an inbound-write-count threshold, mirroring
//! `BucketMap::needs_sweep` / `sweep` (`src/dispatch.rs`).

use crate::errors::DaemonError;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};
use tracing::warn;

/// Inbound spool entries expire after this long (R12).
pub const INBOUND_RETENTION: Duration = Duration::from_secs(3600);

/// Minimum wall-clock gap between opportunistic [`Spool::sweep`] calls,
/// absent write-count pressure.
const SWEEP_INTERVAL: Duration = Duration::from_secs(300);

/// Number of inbound writes since the last sweep that forces an early sweep
/// regardless of elapsed time, mirroring `BucketMap`'s `max_buckets` gate.
const SWEEP_ENTRY_THRESHOLD: usize = 32;

/// Owner-only inbound/outbound spool directories under `$DATA_DIR/spool`.
///
/// Cheap to share: every method takes `&self`, so callers that need to reach
/// the same spool from multiple tasks wrap it in `Arc<Spool>`.
#[derive(Debug)]
pub struct Spool {
    inbound_root: PathBuf,
    outbound_root: PathBuf,
    sweep_state: Mutex<SweepState>,
}

#[derive(Debug)]
struct SweepState {
    last_sweep: Instant,
    inbound_writes_since_sweep: usize,
}

impl Spool {
    /// Create/validate `$DATA_DIR/spool/{inbound,outbound}` at `0o700`.
    ///
    /// Idempotent: calling this again on an already-provisioned data
    /// directory never loosens the directories' permissions.
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self, DaemonError> {
        let data_dir = data_dir.as_ref();
        let inbound_root = secure_ensure_spool_dir(data_dir, &["spool", "inbound"])?;
        let outbound_root = secure_ensure_spool_dir(data_dir, &["spool", "outbound"])?;
        Ok(Self {
            inbound_root,
            outbound_root,
            sweep_state: Mutex::new(SweepState {
                last_sweep: Instant::now(),
                inbound_writes_since_sweep: 0,
            }),
        })
    }

    pub fn inbound_root(&self) -> &Path {
        &self.inbound_root
    }

    pub fn outbound_root(&self) -> &Path {
        &self.outbound_root
    }

    /// Create a fresh `0o600` inbound file with a random hex stem and the
    /// given extension (no leading dot). Returns the path and an open
    /// handle.
    ///
    /// If a later write against the returned handle fails, the caller MUST
    /// call [`Spool::discard_inbound`] with the returned path so no partial
    /// payload survives on disk.
    pub fn create_inbound(&self, extension: &str) -> Result<(PathBuf, fs::File), DaemonError> {
        let mut stem_bytes = [0u8; 16];
        getrandom::getrandom(&mut stem_bytes)?;
        let stem = hex::encode(stem_bytes);
        let filename = if extension.is_empty() {
            stem
        } else {
            format!("{stem}.{extension}")
        };
        let path = self.inbound_root.join(filename);

        #[cfg(unix)]
        let file = {
            use std::os::unix::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .map_err(DaemonError::Io)?
        };
        #[cfg(not(unix))]
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(DaemonError::Io)?;

        self.lock_state().inbound_writes_since_sweep += 1;

        Ok((path, file))
    }

    /// Remove an inbound file created by [`Spool::create_inbound`], for use
    /// after a write failure left a partial payload. Swallows a
    /// missing-file error; every other removal failure is logged (path and
    /// I/O error only, never file contents).
    pub fn discard_inbound(&self, path: &Path) {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "failed to discard inbound spool entry"
                );
            }
        }
    }

    /// Confine a handler-supplied path to the outbound root per KTD5.
    ///
    /// Joins `supplied` onto the outbound root (unless it is already
    /// absolute), canonicalizes, and requires the result to be a strict
    /// component-wise descendant of the canonicalized outbound root —
    /// `Path::strip_prefix` compares path components, not raw string
    /// prefixes, so a sibling directory whose name merely shares the root's
    /// string prefix is rejected. The confined path is then opened and its
    /// metadata (through the open handle, not a second path resolution) is
    /// checked to be a regular file. Every failure returns
    /// `DaemonError::AttachmentPathRejected` before any content is read.
    pub fn resolve_outbound(&self, supplied: &str) -> Result<(PathBuf, fs::File), DaemonError> {
        let supplied_path = Path::new(supplied);
        let joined = if supplied_path.is_absolute() {
            supplied_path.to_path_buf()
        } else {
            self.outbound_root.join(supplied_path)
        };

        let canonical = joined
            .canonicalize()
            .map_err(|_| DaemonError::AttachmentPathRejected)?;

        if canonical.strip_prefix(&self.outbound_root).is_err() {
            return Err(DaemonError::AttachmentPathRejected);
        }

        let file = fs::File::open(&canonical).map_err(|_| DaemonError::AttachmentPathRejected)?;
        let metadata = file
            .metadata()
            .map_err(|_| DaemonError::AttachmentPathRejected)?;
        if !metadata.is_file() {
            return Err(DaemonError::AttachmentPathRejected);
        }

        Ok((canonical, file))
    }

    /// `true` once the amortized sweep gate has tripped: the write-count
    /// threshold has been reached, or the cadence interval has elapsed
    /// since the last sweep.
    pub fn needs_sweep(&self) -> bool {
        let state = self.lock_state();
        state.inbound_writes_since_sweep >= SWEEP_ENTRY_THRESHOLD
            || state.last_sweep.elapsed() >= SWEEP_INTERVAL
    }

    /// Amortized sweep: deletes inbound entries older than
    /// [`INBOUND_RETENTION`] and outbound entries older than
    /// `outbound_retention`. No-op unless [`Spool::needs_sweep`] is true.
    pub fn sweep(&self, outbound_retention: Duration) -> Result<(), DaemonError> {
        if !self.needs_sweep() {
            return Ok(());
        }

        delete_stale(&self.inbound_root, INBOUND_RETENTION)?;
        delete_stale(&self.outbound_root, outbound_retention)?;

        let mut state = self.lock_state();
        state.last_sweep = Instant::now();
        state.inbound_writes_since_sweep = 0;

        Ok(())
    }

    /// Unconditional inbound sweep, ignoring the cadence gate. Used at
    /// startup (to clear anything an unclean shutdown left) and at graceful
    /// shutdown (KTD15).
    pub fn sweep_inbound_now(&self) -> Result<(), DaemonError> {
        delete_stale(&self.inbound_root, INBOUND_RETENTION)
    }

    pub fn inbound_entry_count(&self) -> usize {
        count_regular_files(&self.inbound_root)
    }

    pub fn outbound_entry_count(&self) -> usize {
        count_regular_files(&self.outbound_root)
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, SweepState> {
        match self.sweep_state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// Delete every regular file in `dir` whose mtime is at least `retention`
/// old. A missing directory is not an error (nothing to sweep). Entries that
/// vanish or whose metadata cannot be read concurrently are skipped rather
/// than treated as fatal.
fn delete_stale(dir: &Path, retention: Duration) -> Result<(), DaemonError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(DaemonError::Io(e)),
    };

    let now = SystemTime::now();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let age = now.duration_since(modified).unwrap_or(Duration::ZERO);
        if age >= retention {
            let _ = fs::remove_file(entry.path());
        }
    }

    Ok(())
}

/// Count regular files directly inside `dir`. A missing or unreadable
/// directory counts as zero rather than erroring, since this feeds gauges
/// rather than a security decision.
fn count_regular_files(dir: &Path) -> usize {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.metadata().map(|m| m.is_file()).unwrap_or(false))
        .count()
}

/// Create (or validate an already-existing) spool subdirectory at
/// `data_dir` joined with `relative`, with owner-only permissions,
/// rejecting a symlinked daemon-owned component and a root resolving under
/// `/tmp` or `/dev/shm`. Adapts the guard set `secure_ensure_mls_parent_dir`
/// (`src/mls_path.rs:56-92`) applies to the MLS database's parent directory,
/// but hardens the target directory itself rather than its parent.
///
/// The symlink check covers only the components this function creates — the
/// `spool` root and its direction subdirectory. It deliberately does NOT walk
/// the operator-supplied `data_dir`'s own ancestry: a symlinked ancestor there
/// is normal and trusted (`/var` is a symlink to `/private/var` on macOS, and
/// an operator may legitimately place `$DATA_DIR` behind one), while the threat
/// the check exists to stop is an attacker pre-planting a spool component as a
/// symlink to redirect freshly written payloads out of the tree.
fn secure_ensure_spool_dir(data_dir: &Path, relative: &[&str]) -> Result<PathBuf, DaemonError> {
    let mut dir = data_dir.to_path_buf();
    for component in relative {
        dir.push(component);
        if let Ok(meta) = fs::symlink_metadata(&dir)
            && meta.file_type().is_symlink()
        {
            return Err(DaemonError::Config(format!(
                "spool path contains a symlink: {}",
                dir.display()
            )));
        }
    }
    let dir = dir.as_path();

    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(DaemonError::Io)?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir).map_err(DaemonError::Io)?;
    }

    // `DirBuilder::create` with `recursive(true)` is a no-op if `dir` already
    // exists, so re-verify it is a real directory (not a symlink that raced
    // in) and unconditionally re-harden permissions afterwards.
    let meta = fs::symlink_metadata(dir).map_err(DaemonError::Io)?;
    if meta.file_type().is_symlink() {
        return Err(DaemonError::Config(format!(
            "spool path is a symlink: {}",
            dir.display()
        )));
    }
    if !meta.is_dir() {
        return Err(DaemonError::Config(format!(
            "spool path is not a directory: {}",
            dir.display()
        )));
    }

    let canonical = dir.canonicalize().map_err(DaemonError::Io)?;
    let tmp = Path::new("/tmp");
    let shm = Path::new("/dev/shm");
    if canonical.starts_with(tmp) || canonical.starts_with(shm) {
        return Err(DaemonError::Config(format!(
            "spool path resolves under /tmp or /dev/shm: {}",
            canonical.display()
        )));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&canonical, fs::Permissions::from_mode(0o700))
            .map_err(DaemonError::Io)?;
    }

    Ok(canonical)
}
