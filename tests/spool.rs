#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! U1: spool root, confinement, and retention sweep.
//!
//! Covers R9, R12, R20, R26, R30 via KTD4, KTD5, KTD15.

mod common;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::time::{Duration, SystemTime};

use pacto_bot_api::errors::DaemonError;
use pacto_bot_api::spool::{INBOUND_RETENTION, Spool};

/// Set a file's mtime directly, without pulling in a dependency: `File::set_times`
/// has been stable std since Rust 1.75.
fn set_mtime(path: &Path, when: SystemTime) {
    let file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open file to backdate mtime");
    file.set_times(fs::FileTimes::new().set_modified(when))
        .expect("set mtime");
}

/// Create and immediately discard enough inbound entries to trip the
/// entry-count half of the sweep gate, without depending on the exact
/// (private) threshold value.
fn trip_sweep_gate(spool: &Spool) {
    for _ in 0..64 {
        let (path, file) = spool.create_inbound("tmp").expect("create_inbound");
        drop(file);
        spool.discard_inbound(&path);
    }
}

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).expect("stat path").permissions().mode() & 0o777
}

#[test]
fn spool_open_creates_both_dirs_at_0700() {
    let dir = common::tempdir().expect("tempdir");
    let spool = Spool::open(dir.path()).expect("open spool");

    assert_eq!(mode_of(spool.inbound_root()), 0o700);
    assert_eq!(mode_of(spool.outbound_root()), 0o700);
}

#[test]
fn spool_open_twice_is_idempotent_and_does_not_loosen_permissions() {
    let dir = common::tempdir().expect("tempdir");
    let first = Spool::open(dir.path()).expect("open spool");

    // Deliberately loosen the directory to prove a second `open` restores
    // strict permissions rather than trusting whatever it finds.
    fs::set_permissions(first.inbound_root(), fs::Permissions::from_mode(0o755))
        .expect("loosen permissions");

    let second = Spool::open(dir.path()).expect("re-open spool");

    assert_eq!(mode_of(second.inbound_root()), 0o700);
    assert_eq!(mode_of(second.outbound_root()), 0o700);
}

#[test]
fn create_inbound_file_is_0600_before_any_bytes_written() {
    let dir = common::tempdir().expect("tempdir");
    let spool = Spool::open(dir.path()).expect("open spool");

    let (path, _file) = spool.create_inbound("jpg").expect("create_inbound");

    let metadata = fs::metadata(&path).expect("stat inbound file");
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    assert_eq!(metadata.len(), 0);
}

#[test]
fn create_inbound_write_failure_leaves_no_file_after_discard() {
    let dir = common::tempdir().expect("tempdir");
    let spool = Spool::open(dir.path()).expect("open spool");

    let (path, file) = spool.create_inbound("bin").expect("create_inbound");
    assert!(path.exists());

    // Simulate the point immediately after a write against the handle
    // failed (e.g. disk full): the caller drops the handle and discards.
    drop(file);
    spool.discard_inbound(&path);

    assert!(!path.exists());
}

#[test]
fn resolve_outbound_accepts_plain_relative_name() {
    let dir = common::tempdir().expect("tempdir");
    let spool = Spool::open(dir.path()).expect("open spool");

    let target = spool.outbound_root().join("report.pdf");
    fs::write(&target, b"payload").expect("write outbound payload");

    let (resolved, _file) = spool
        .resolve_outbound("report.pdf")
        .expect("resolve relative name");
    assert_eq!(resolved, target);
}

#[test]
fn resolve_outbound_accepts_absolute_path_inside_root() {
    let dir = common::tempdir().expect("tempdir");
    let spool = Spool::open(dir.path()).expect("open spool");

    let target = spool.outbound_root().join("photo.png");
    fs::write(&target, b"payload").expect("write outbound payload");

    let (resolved, _file) = spool
        .resolve_outbound(&target.to_string_lossy())
        .expect("resolve absolute path inside root");
    assert_eq!(resolved, target);
}

#[test]
fn resolve_outbound_rejects_dot_dot_traversal() {
    let dir = common::tempdir().expect("tempdir");
    let spool = Spool::open(dir.path()).expect("open spool");

    // A file that exists one level above the outbound root; `../` must not
    // reach it even though the path resolves to something real.
    let secret = dir.path().join("spool").join("secret.txt");
    fs::write(&secret, b"secret").expect("write sibling file");

    let err = spool
        .resolve_outbound("../secret.txt")
        .expect_err("traversal must be rejected");
    assert!(matches!(err, DaemonError::AttachmentPathRejected));
}

#[test]
fn resolve_outbound_rejects_absolute_path_outside_root() {
    let dir = common::tempdir().expect("tempdir");
    let spool = Spool::open(dir.path()).expect("open spool");

    let outside = dir.path().join("outside.bin");
    fs::write(&outside, b"payload").expect("write outside file");

    let err = spool
        .resolve_outbound(&outside.to_string_lossy())
        .expect_err("absolute path outside root must be rejected");
    assert!(matches!(err, DaemonError::AttachmentPathRejected));
}

#[test]
fn resolve_outbound_rejects_symlink_escaping_root() {
    let dir = common::tempdir().expect("tempdir");
    let spool = Spool::open(dir.path()).expect("open spool");

    let secret = dir.path().join("secret.bin");
    fs::write(&secret, b"payload").expect("write secret file");

    let link = spool.outbound_root().join("escape");
    symlink(&secret, &link).expect("create escaping symlink");

    let err = spool
        .resolve_outbound("escape")
        .expect_err("symlink escape must be rejected");
    assert!(matches!(err, DaemonError::AttachmentPathRejected));
}

#[test]
fn resolve_outbound_rejects_nonexistent_path() {
    let dir = common::tempdir().expect("tempdir");
    let spool = Spool::open(dir.path()).expect("open spool");

    let err = spool
        .resolve_outbound("does-not-exist.bin")
        .expect_err("nonexistent path must be rejected");
    assert!(matches!(err, DaemonError::AttachmentPathRejected));
}

#[test]
fn resolve_outbound_rejects_sibling_directory_sharing_string_prefix() {
    let dir = common::tempdir().expect("tempdir");
    let spool = Spool::open(dir.path()).expect("open spool");

    // "outbound-evil" shares the string prefix "outbound" with the real
    // outbound root. A `starts_with` on the rendered path would wrongly
    // accept a file inside it; a component-wise check must not.
    let sibling = dir.path().join("spool").join("outbound-evil");
    fs::create_dir_all(&sibling).expect("create sibling directory");
    let target = sibling.join("payload.bin");
    fs::write(&target, b"payload").expect("write sibling payload");

    let err = spool
        .resolve_outbound(&target.to_string_lossy())
        .expect_err("string-prefix sibling must be rejected");
    assert!(matches!(err, DaemonError::AttachmentPathRejected));
}

#[test]
fn resolve_outbound_rejects_directory_target() {
    let dir = common::tempdir().expect("tempdir");
    let spool = Spool::open(dir.path()).expect("open spool");

    fs::create_dir_all(spool.outbound_root().join("subdir")).expect("create subdir");

    let err = spool
        .resolve_outbound("subdir")
        .expect_err("directory target must be rejected");
    assert!(matches!(err, DaemonError::AttachmentPathRejected));
}

/// The guard must reject a spool component an attacker pre-planted as a
/// symlink, because that would redirect decrypted payloads out of the tree.
#[test]
fn spool_open_rejects_symlinked_spool_root() {
    let dir = common::tempdir().expect("tempdir");
    let data_dir = dir.path().join("data");
    fs::create_dir_all(&data_dir).expect("create data directory");

    let elsewhere = dir.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("create target directory");
    symlink(&elsewhere, data_dir.join("spool")).expect("plant symlinked spool root");

    let err = Spool::open(&data_dir).expect_err("symlinked spool root must be rejected");
    assert!(matches!(err, DaemonError::Config(_)));
}

#[test]
fn spool_open_rejects_symlinked_direction_subdirectory() {
    let dir = common::tempdir().expect("tempdir");
    let data_dir = dir.path().join("data");
    fs::create_dir_all(data_dir.join("spool")).expect("create spool root");

    let elsewhere = dir.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("create target directory");
    symlink(&elsewhere, data_dir.join("spool").join("inbound"))
        .expect("plant symlinked inbound directory");

    let err = Spool::open(&data_dir).expect_err("symlinked inbound dir must be rejected");
    assert!(matches!(err, DaemonError::Config(_)));
}

/// A symlinked ancestor of the operator-supplied data directory is normal and
/// trusted — `/var` is a symlink to `/private/var` on macOS — so it must NOT
/// be rejected. Before this was narrowed, the daemon refused to start against
/// a stock `$TMPDIR` data directory.
#[test]
fn spool_open_accepts_symlinked_data_dir_ancestor() {
    let dir = common::tempdir().expect("tempdir");

    let real = dir.path().join("real");
    fs::create_dir_all(&real).expect("create real directory");
    let linked = dir.path().join("linked");
    symlink(&real, &linked).expect("create symlinked ancestor");

    let data_dir = linked.join("data");
    let spool = Spool::open(&data_dir).expect("symlinked ancestor must be accepted");
    assert!(spool.inbound_root().is_dir());
    assert!(spool.outbound_root().is_dir());
}

#[test]
fn sweep_deletes_stale_inbound_and_keeps_fresh() {
    let dir = common::tempdir().expect("tempdir");
    let spool = Spool::open(dir.path()).expect("open spool");
    trip_sweep_gate(&spool);

    let stale = spool.inbound_root().join("stale.bin");
    fs::write(&stale, b"old").expect("write stale inbound entry");
    set_mtime(
        &stale,
        SystemTime::now() - INBOUND_RETENTION - Duration::from_secs(60),
    );

    let fresh = spool.inbound_root().join("fresh.bin");
    fs::write(&fresh, b"new").expect("write fresh inbound entry");

    spool.sweep(Duration::from_secs(86_400)).expect("sweep");

    assert!(!stale.exists(), "stale inbound entry must be swept");
    assert!(fresh.exists(), "fresh inbound entry must survive");
}

#[test]
fn sweep_deletes_stale_outbound_and_keeps_fresh() {
    let dir = common::tempdir().expect("tempdir");
    let spool = Spool::open(dir.path()).expect("open spool");
    trip_sweep_gate(&spool);

    let retention = Duration::from_secs(60);
    let stale = spool.outbound_root().join("stale.bin");
    fs::write(&stale, b"old").expect("write stale outbound entry");
    set_mtime(
        &stale,
        SystemTime::now() - retention - Duration::from_secs(30),
    );

    let fresh = spool.outbound_root().join("fresh.bin");
    fs::write(&fresh, b"new").expect("write fresh outbound entry");

    spool.sweep(retention).expect("sweep");

    assert!(!stale.exists(), "stale outbound entry must be swept");
    assert!(fresh.exists(), "fresh outbound entry must survive");
}

#[test]
fn needs_sweep_false_after_sweep_true_after_cadence_elapsed() {
    let dir = common::tempdir().expect("tempdir");
    let spool = Spool::open(dir.path()).expect("open spool");
    assert!(!spool.needs_sweep(), "a fresh spool has nothing to sweep");

    trip_sweep_gate(&spool);
    assert!(
        spool.needs_sweep(),
        "the write-count cadence must have tripped"
    );

    spool.sweep(Duration::from_secs(86_400)).expect("sweep");
    assert!(
        !spool.needs_sweep(),
        "the gate must reset immediately after a sweep"
    );

    trip_sweep_gate(&spool);
    assert!(
        spool.needs_sweep(),
        "the cadence must be able to trip again after resetting"
    );
}

#[test]
fn forced_inbound_sweep_ignores_cadence_gate() {
    let dir = common::tempdir().expect("tempdir");
    let spool = Spool::open(dir.path()).expect("open spool");
    assert!(
        !spool.needs_sweep(),
        "the gate starts closed on a fresh spool"
    );

    let leftover = spool.inbound_root().join("leftover.bin");
    fs::write(&leftover, b"old").expect("write leftover inbound entry");
    set_mtime(
        &leftover,
        SystemTime::now() - INBOUND_RETENTION - Duration::from_secs(60),
    );

    spool.sweep_inbound_now().expect("forced inbound sweep");

    assert!(
        !leftover.exists(),
        "a forced sweep must ignore the cadence gate"
    );
}
