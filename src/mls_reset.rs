//! Store classification, reset, and fresh-store construction (U10).
//!
//! Runs once per bot identity in `ClientManager::new`, before
//! `MlsEngineHandle::new_persistent` ever touches the store path. Classifies
//! the store per KD4's order — presence, then encryption state, then the
//! legacy refinery schema version — harvests a legacy store's admin set,
//! commits a durable reset-in-progress marker before any destructive step,
//! and moves the legacy file set out of the way. By the time this returns
//! `Ok(())`, the live path is safe for `new_persistent`: either nothing was
//! there, an interrupted reset was completed, an already-encrypted store
//! opened with its own key and needs no reset, or a legacy/reset-eligible
//! store was just archived-or-deleted and the path is clear for a fresh
//! encrypted store.
//!
//! `new_persistent`'s own `load_or_create` + `new_with_key` sequence is
//! unchanged: it already reuses an existing key/store and mints a fresh key
//! only when the path is clear, so once this module has cleared or
//! confirmed the path there is nothing left for it to coordinate.
//!
//! See `docs/plans/2026-08-05-001-chore-nostr-mdk-parity-plan.md`,
//! "U10. Classification, reset, and store construction".

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex as StdMutex};
use std::time::{SystemTime, UNIX_EPOCH};

use mdk_sqlite_storage::MdkSqliteStorage;
use mdk_sqlite_storage::encryption::EncryptionConfig;
use rusqlite::OpenFlags;
use tokio::sync::Mutex as AsyncMutex;

use crate::db::Db;
use crate::errors::DaemonError;
use crate::mls::MlsError;
use crate::mls_key;

/// Serialises this module per bot identity (R23) so two concurrent callers
/// for the same bot cannot both observe "needs reset" and both run it.
/// Keyed by `bot_id`, which is the unit every other lookup in this daemon
/// reasons about, not the configured path.
static RESET_LOCKS: LazyLock<StdMutex<HashMap<String, Arc<AsyncMutex<()>>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

fn lock_for(bot_id: &str) -> Arc<AsyncMutex<()>> {
    let mut locks = match RESET_LOCKS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    Arc::clone(
        locks
            .entry(bot_id.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
    )
}

/// Classify `store_path` for `bot_id`, reset it if needed, and prune expired
/// legacy archives. Returns `Ok(true)` when the path is ready for
/// [`crate::mls::MlsEngineHandle::new_persistent`] and this call performed an
/// actual reset (legacy harvest-and-move, or an encrypted store reset under
/// R26) -- the caller must republish this bot's KeyPackage before any
/// restoration can be attempted (R27). Returns `Ok(false)` for a fresh
/// install, an already-valid store, or finishing an interrupted reset whose
/// destructive step already completed in a prior run. Every fail-closed
/// condition returns `Err`; the caller isolates that per bot (U11) rather
/// than propagating it to every configured bot.
pub async fn classify_and_prepare(
    db: &Db,
    bot_id: &str,
    store_path: &Path,
    archive_retention_days: u32,
) -> Result<bool, MlsError> {
    let guard = lock_for(bot_id);
    let _permit = guard.lock().await;

    let sidecars = sidecar_paths(store_path);
    let mut reset_occurred = false;

    if !store_path.exists() {
        if sidecars.iter().any(|p| p.exists()) {
            // Interrupted reset: the main file is already gone (or this is
            // the first time we have seen this bot after a crash mid-move);
            // only leftover sidecars remain. Finish removing them the same
            // way a completed reset would, then fall through to a fresh
            // store below. The reset itself (and any KeyPackage republish
            // it required) already happened in a prior run that recorded
            // the completed marker; this call performs no new reset.
            finish_sidecar_cleanup(store_path, &sidecars, archive_retention_days)?;
        }
        prune_expired_archives(store_path, archive_retention_days)?;
        return Ok(false);
    }

    let encrypted = mdk_sqlite_storage::encryption::is_database_encrypted(store_path)
        .map_err(|_| MlsError::Engine("MLS store: unreadable (fail-closed)".into()))?;

    if !encrypted {
        match read_legacy_schema_version(store_path)? {
            Some(version) if version >= 100 => {
                reset_legacy_store(db, bot_id, store_path, &sidecars, archive_retention_days)
                    .await?;
                reset_occurred = true;
            }
            _ => {
                return Err(MlsError::Engine(
                    "MLS store: unrecognised unencrypted schema (fail-closed)".into(),
                ));
            }
        }
        prune_expired_archives(store_path, archive_retention_days)?;
        return Ok(reset_occurred);
    }

    match mls_key::load(store_path) {
        Ok(None) => {
            // Key absent: reset-eligible, always archived (R26) regardless
            // of the retention setting.
            reset_encrypted_store(db, bot_id, store_path, &sidecars).await?;
            reset_occurred = true;
        }
        Ok(Some(key)) => match try_open_with_key(store_path, &key) {
            Ok(()) => {}
            Err(OpenOutcome::WrongKey) => {
                reset_encrypted_store(db, bot_id, store_path, &sidecars).await?;
                reset_occurred = true;
            }
            Err(OpenOutcome::Other) => {
                return Err(MlsError::Engine(
                    "MLS store: open failed (fail-closed)".into(),
                ));
            }
        },
        Err(_key_err) => {
            return Err(MlsError::Engine(
                "MLS store: key unusable (fail-closed)".into(),
            ));
        }
    }

    prune_expired_archives(store_path, archive_retention_days)?;
    Ok(reset_occurred)
}

enum OpenOutcome {
    WrongKey,
    Other,
}

/// Try opening `store_path` with `key` without leaving the daemon holding
/// the connection: the caller only needs to know whether the key is right,
/// not to keep the store open. `new_persistent` opens it again for real
/// immediately afterward.
fn try_open_with_key(store_path: &Path, key: &[u8; 32]) -> Result<(), OpenOutcome> {
    match MdkSqliteStorage::new_with_key(store_path, EncryptionConfig::new(*key)) {
        Ok(_storage) => Ok(()),
        Err(mdk_sqlite_storage::error::Error::WrongEncryptionKey) => Err(OpenOutcome::WrongKey),
        Err(_other) => Err(OpenOutcome::Other),
    }
}

/// Read `max(version)` from the legacy store's own refinery migration table
/// by direct SQL — R20 never hands an unencrypted store to MDK. `None` means
/// the table exists but is empty (never legitimately happens for a real
/// legacy store); a missing table is a `rusqlite::Error` that fails closed
/// like any other unreadable-store condition.
fn read_legacy_schema_version(store_path: &Path) -> Result<Option<i64>, MlsError> {
    let conn = rusqlite::Connection::open_with_flags(store_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| MlsError::Engine("MLS store: unreadable (fail-closed)".into()))?;
    conn.query_row(
        "SELECT MAX(version) FROM _refinery_schema_history_nostr_mls",
        (),
        |row| row.get::<_, Option<i64>>(0),
    )
    .map_err(|_| MlsError::Engine("MLS store: unreadable (fail-closed)".into()))
}

/// Enumerate a store's sidecars by literal filename suffix on the full path
/// — never [`Path::with_extension`]/[`PathBuf::set_extension`], which silently
/// misses sidecars for a store not named `*.db` (R21).
fn sidecar_paths(store_path: &Path) -> Vec<PathBuf> {
    let base = store_path.as_os_str();
    ["-wal", "-shm", "-journal"]
        .iter()
        .map(|suffix| {
            let mut owned = base.to_os_string();
            owned.push(suffix);
            PathBuf::from(owned)
        })
        .collect()
}

async fn reset_legacy_store(
    db: &Db,
    bot_id: &str,
    store_path: &Path,
    sidecars: &[PathBuf],
    archive_retention_days: u32,
) -> Result<(), MlsError> {
    // Harvest before any destructive step (KTD6); the R26 branch below never
    // reaches this function because an encrypted store cannot be harvested.
    let harvested = harvest_legacy_admins(store_path)?;

    checkpoint_wal(store_path)?;

    let marked_at = unix_now();
    db.mark_mls_store_reset_start(bot_id, marked_at)
        .await
        .map_err(db_err)?;

    for (wire_id, admin_npubs) in &harvested {
        for admin_npub in admin_npubs {
            db.upsert_mls_store_reset_admin(bot_id, wire_id, admin_npub)
                .await
                .map_err(db_err)?;
        }
    }

    let archive_path = remove_or_archive(store_path, sidecars, archive_retention_days, "legacy")?;

    db.complete_mls_store_reset(bot_id, unix_now(), archive_path.as_deref())
        .await
        .map_err(db_err)?;

    Ok(())
}

async fn reset_encrypted_store(
    db: &Db,
    bot_id: &str,
    store_path: &Path,
    sidecars: &[PathBuf],
) -> Result<(), MlsError> {
    let marked_at = unix_now();
    db.mark_mls_store_reset_start(bot_id, marked_at)
        .await
        .map_err(db_err)?;

    // R26: always archived — retention_days is irrelevant here, and the
    // archive is exempt from pruning (tagged "r26" below).
    let archive_path = remove_or_archive(store_path, sidecars, 0, "r26")?;

    db.complete_mls_store_reset(bot_id, unix_now(), archive_path.as_deref())
        .await
        .map_err(db_err)?;

    Ok(())
}

/// Finish an interrupted reset: only sidecars remain at the live path (the
/// main file is already gone). Their origin (a retention-gated legacy reset
/// or an always-archived R26 reset) cannot be recovered from the sidecars
/// alone, so they follow the ordinary retention policy.
fn finish_sidecar_cleanup(
    store_path: &Path,
    sidecars: &[PathBuf],
    archive_retention_days: u32,
) -> Result<(), MlsError> {
    let existing: Vec<PathBuf> = sidecars.iter().filter(|p| p.exists()).cloned().collect();
    if existing.is_empty() {
        return Ok(());
    }
    if archive_retention_days == 0 {
        for path in &existing {
            std::fs::remove_file(path).map_err(reset_io_err)?;
        }
        return Ok(());
    }
    let dest_dir = new_archive_dir(&archive_root_for(store_path), "legacy")?;
    for path in &existing {
        chmod_0600(path)?;
        let file_name = path
            .file_name()
            .ok_or_else(|| MlsError::Engine("MLS store: invalid sidecar path".into()))?;
        std::fs::rename(path, dest_dir.join(file_name)).map_err(reset_io_err)?;
    }
    Ok(())
}

/// Checkpoint out of WAL so the log folds into the main file before the
/// move (R21). A no-op if the store is not currently in WAL mode.
fn checkpoint_wal(store_path: &Path) -> Result<(), MlsError> {
    let conn = rusqlite::Connection::open(store_path)
        .map_err(|_| MlsError::Engine("MLS store: unreadable (fail-closed)".into()))?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .map_err(|_| MlsError::Engine("MLS store: unreadable (fail-closed)".into()))?;
    Ok(())
}

/// Harvest each legacy group's admin set before the move (KTD6), crossing
/// both encoding boundaries: `nostr_group_id` is declared `TEXT` but bound
/// as a 32-byte array, so it reads back as a blob that must be hex-encoded
/// to become a `wire_id`; `admin_pubkeys` is a JSON array of 64-char hex
/// that must become bech32 npub to match every other pubkey column in
/// `agent.db`. Returns `(wire_id, admin_npubs)` pairs.
fn harvest_legacy_admins(store_path: &Path) -> Result<Vec<(String, Vec<String>)>, MlsError> {
    let conn = rusqlite::Connection::open_with_flags(store_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| MlsError::Engine("MLS store: unreadable (fail-closed)".into()))?;
    let mut stmt = conn
        .prepare("SELECT nostr_group_id, admin_pubkeys FROM groups")
        .map_err(|_| MlsError::Engine("MLS store: unreadable (fail-closed)".into()))?;
    let rows = stmt
        .query_map((), |row| {
            let group_id: Vec<u8> = row.get(0)?;
            let admins_json: String = row.get(1)?;
            Ok((group_id, admins_json))
        })
        .map_err(|_| MlsError::Engine("MLS store: unreadable (fail-closed)".into()))?;

    let mut harvested = Vec::new();
    for row in rows {
        let (group_id, admins_json) =
            row.map_err(|_| MlsError::Engine("MLS store: unreadable (fail-closed)".into()))?;
        let wire_id = hex::encode(&group_id);
        let admin_hexes: Vec<String> = serde_json::from_str(&admins_json)
            .map_err(|_| MlsError::Engine("MLS store: malformed legacy admin set".into()))?;
        let mut admin_npubs = Vec::with_capacity(admin_hexes.len());
        for hex_pubkey in admin_hexes {
            let pubkey = nostr::PublicKey::from_hex(&hex_pubkey)
                .map_err(|_| MlsError::Engine("MLS store: malformed legacy admin pubkey".into()))?;
            let npub = nostr::ToBech32::to_bech32(&pubkey)
                .map_err(|_| MlsError::Engine("MLS store: admin pubkey encoding failed".into()))?;
            admin_npubs.push(npub);
        }
        harvested.push((wire_id, admin_npubs));
    }
    Ok(harvested)
}

/// Remove the legacy file set as a single move: deleted when
/// `archive_retention_days` is `0` and `kind` is not `"r26"`, or moved into
/// a timestamped child of the stable archive root otherwise. Sidecars are
/// chmod'd `0o600` before the move; nothing may remain at the live path
/// afterward. Returns the archive directory path when one was created.
fn remove_or_archive(
    store_path: &Path,
    sidecars: &[PathBuf],
    archive_retention_days: u32,
    kind: &str,
) -> Result<Option<String>, MlsError> {
    let mut all_paths = vec![store_path.to_path_buf()];
    all_paths.extend(sidecars.iter().cloned());
    let existing: Vec<PathBuf> = all_paths.iter().filter(|p| p.exists()).cloned().collect();

    let archived = kind == "r26" || archive_retention_days > 0;

    let archive_path = if archived {
        let dest_dir = new_archive_dir(&archive_root_for(store_path), kind)?;
        for path in &existing {
            chmod_0600(path)?;
            let file_name = path
                .file_name()
                .ok_or_else(|| MlsError::Engine("MLS store: invalid path".into()))?;
            std::fs::rename(path, dest_dir.join(file_name)).map_err(reset_io_err)?;
        }
        Some(dest_dir.display().to_string())
    } else {
        for path in &existing {
            std::fs::remove_file(path).map_err(reset_io_err)?;
        }
        None
    };

    for path in &all_paths {
        if path.exists() {
            return Err(MlsError::Engine(
                "MLS store: file remained at the live path after reset".into(),
            ));
        }
    }

    Ok(archive_path)
}

/// One stable archive root beside the live store.
fn archive_root_for(store_path: &Path) -> PathBuf {
    let parent = store_path.parent().unwrap_or_else(|| Path::new("."));
    parent.join("mls-archive")
}

/// Create a new timestamped, `0o700` archive directory under `archive_root`.
/// `kind` is embedded in the directory name so [`prune_expired_archives`]
/// can skip `"r26"` archives, which are exempt from pruning regardless of
/// the retention window (R22).
fn new_archive_dir(archive_root: &Path, kind: &str) -> Result<PathBuf, MlsError> {
    std::fs::create_dir_all(archive_root).map_err(reset_io_err)?;
    chmod_0700(archive_root)?;

    let mut suffix = [0u8; 4];
    getrandom::getrandom(&mut suffix)
        .map_err(|_| MlsError::Engine("MLS store: failed to generate archive suffix".into()))?;
    let dir = archive_root.join(format!("{}-{}-{kind}", unix_now(), hex::encode(suffix)));
    std::fs::create_dir_all(&dir).map_err(reset_io_err)?;
    chmod_0700(&dir)?;
    Ok(dir)
}

/// Delete legacy (`"-legacy"`-tagged) archive directories whose timestamp is
/// older than `retention_days`. R26 (`"-r26"`-tagged) archives are always
/// exempt. A no-op when `retention_days` is `0`: nothing is created at that
/// setting, and lowering it later does not retroactively purge archives
/// created under a prior non-zero setting.
fn prune_expired_archives(store_path: &Path, retention_days: u32) -> Result<(), MlsError> {
    if retention_days == 0 {
        return Ok(());
    }
    let archive_root = archive_root_for(store_path);
    let Ok(entries) = std::fs::read_dir(&archive_root) else {
        return Ok(());
    };
    let cutoff = unix_now().saturating_sub(i64::from(retention_days) * 86_400);
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.ends_with("-r26") {
            continue;
        }
        let Some(stamp_str) = name.split('-').next() else {
            continue;
        };
        let Ok(stamp) = stamp_str.parse::<i64>() else {
            continue;
        };
        if stamp < cutoff {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
    Ok(())
}

#[cfg(unix)]
fn chmod_0600(path: &Path) -> Result<(), MlsError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(reset_io_err)
}
#[cfg(not(unix))]
fn chmod_0600(_path: &Path) -> Result<(), MlsError> {
    Ok(())
}

#[cfg(unix)]
fn chmod_0700(path: &Path) -> Result<(), MlsError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(reset_io_err)
}
#[cfg(not(unix))]
fn chmod_0700(_path: &Path) -> Result<(), MlsError> {
    Ok(())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn reset_io_err(_e: std::io::Error) -> MlsError {
    MlsError::Engine("MLS store filesystem error".into())
}

fn db_err(_e: DaemonError) -> MlsError {
    MlsError::Engine("MLS store: agent database error".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use nostr::ToBech32;
    use refinery::embed_migrations;

    embed_migrations!("tests/fixtures/legacy_mls_migrations");

    async fn test_db() -> Db {
        let dir = tempfile::tempdir().expect("tempdir");
        Db::open(&dir.path().join("agent.db"))
            .await
            .expect("open agent.db")
    }

    /// Build a real legacy (V100-V104) MDK 0.5.2-shaped store at `path`,
    /// using the actual upstream migrations rather than hand-rolled SQL, and
    /// insert one group row with a known admin set.
    fn build_legacy_store(path: &Path, group_id: &[u8; 32], admin_hexes: &[&str]) {
        let mut conn = rusqlite::Connection::open(path).expect("open legacy store");
        migrations::runner()
            .set_migration_table_name("_refinery_schema_history_nostr_mls")
            .run(&mut conn)
            .expect("run legacy migrations");

        let admins_json = serde_json::to_string(admin_hexes).expect("serialize admins");
        conn.execute(
            "INSERT INTO groups (mls_group_id, nostr_group_id, name, description, admin_pubkeys, epoch, state)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, 'active')",
            (
                vec![1u8, 2, 3, 4].as_slice(),
                group_id.as_slice(),
                "legacy squad",
                "a pre-upgrade squad",
                &admins_json,
            ),
        )
        .expect("insert legacy group");
    }

    #[tokio::test]
    async fn fresh_path_needs_no_reset_and_creates_nothing() {
        let dir = common_tempdir();
        let store = dir.path().join("vector-mls.db");
        let db = test_db().await;

        let reset_occurred = classify_and_prepare(&db, "bot-1", &store, 0)
            .await
            .expect("classify");
        assert!(!reset_occurred, "a fresh install performs no reset");

        assert!(!store.exists(), "classify must not create the store itself");
        assert!(
            db.load_mls_store_reset_marker("bot-1")
                .await
                .unwrap()
                .is_none(),
            "a fresh install must not be marked as reset"
        );
    }

    #[tokio::test]
    async fn legacy_store_is_harvested_reset_and_deleted_at_zero_retention() {
        let dir = common_tempdir();
        let store = dir.path().join("vector-mls.db");
        let admin_keys = nostr::Keys::generate();
        let admin_hex = admin_keys.public_key().to_hex();
        let group_id = [7u8; 32];
        build_legacy_store(&store, &group_id, &[admin_hex.as_str()]);
        let db = test_db().await;

        let reset_occurred = classify_and_prepare(&db, "bot-legacy", &store, 0)
            .await
            .expect("classify legacy store");
        assert!(reset_occurred, "harvesting a legacy store is a reset");

        assert!(!store.exists(), "legacy store must leave the live path");
        let marker = db
            .load_mls_store_reset_marker("bot-legacy")
            .await
            .unwrap()
            .expect("marker recorded");
        assert!(marker.reset_at.is_some(), "reset must be marked complete");
        assert!(
            marker.archive_path.is_none(),
            "zero retention must delete, not archive"
        );

        let expected_wire_id = hex::encode(group_id);
        let harvested = db
            .load_mls_store_reset_admins("bot-legacy", &expected_wire_id)
            .await
            .unwrap();
        let expected_npub = admin_keys.public_key().to_bech32().unwrap();
        assert_eq!(harvested, vec![expected_npub]);
    }

    #[tokio::test]
    async fn legacy_store_is_archived_when_retention_is_nonzero() {
        let dir = common_tempdir();
        let store = dir.path().join("vector-mls.db");
        build_legacy_store(&store, &[9u8; 32], &[]);
        let db = test_db().await;

        let reset_occurred = classify_and_prepare(&db, "bot-archived", &store, 7)
            .await
            .expect("classify legacy store");
        assert!(reset_occurred, "archiving a legacy store is a reset");

        assert!(!store.exists());
        let marker = db
            .load_mls_store_reset_marker("bot-archived")
            .await
            .unwrap()
            .expect("marker recorded");
        let archive_path = marker.archive_path.expect("archived, not deleted");
        assert!(
            Path::new(&archive_path).join("vector-mls.db").exists(),
            "archived file must exist under the archive directory"
        );
    }

    #[tokio::test]
    async fn unencrypted_store_with_unrecognised_version_fails_closed() {
        let dir = common_tempdir();
        let store = dir.path().join("vector-mls.db");
        // A plain SQLite file with no refinery history at all -- not a
        // legacy MDK store, not an already-migrated one either.
        {
            let conn = rusqlite::Connection::open(&store).unwrap();
            conn.execute_batch("CREATE TABLE unrelated (x INTEGER);")
                .unwrap();
        }
        let db = test_db().await;

        let err = classify_and_prepare(&db, "bot-unknown", &store, 0)
            .await
            .expect_err("must fail closed");
        let message = err.to_string();
        assert!(
            !message.contains(&store.display().to_string()),
            "fail-closed error must not leak the store path: {message}"
        );

        assert!(store.exists(), "fail-closed must touch nothing on disk");
        assert!(
            db.load_mls_store_reset_marker("bot-unknown")
                .await
                .unwrap()
                .is_none(),
            "fail-closed must not mark a reset"
        );
    }

    #[tokio::test]
    async fn encrypted_store_opens_directly_with_no_reset() {
        let dir = common_tempdir();
        let store = dir.path().join("vector-mls.db");
        let key = mls_key::load_or_create(&store).unwrap();
        let storage = MdkSqliteStorage::new_with_key(&store, EncryptionConfig::new(*key)).unwrap();
        drop(storage);
        let db = test_db().await;

        let reset_occurred = classify_and_prepare(&db, "bot-valid", &store, 0)
            .await
            .expect("already-valid store opens with no reset");
        assert!(
            !reset_occurred,
            "opening an already-valid store performs no reset"
        );

        assert!(store.exists(), "an already-valid store must not be removed");
        assert!(
            db.load_mls_store_reset_marker("bot-valid")
                .await
                .unwrap()
                .is_none(),
            "opening an already-valid store must not mark a reset"
        );
    }

    #[tokio::test]
    async fn encrypted_store_with_wrong_key_is_reset_and_always_archived() {
        let dir = common_tempdir();
        let store = dir.path().join("vector-mls.db");
        let original_key = mls_key::load_or_create(&store).unwrap();
        drop(MdkSqliteStorage::new_with_key(&store, EncryptionConfig::new(*original_key)).unwrap());

        // Corrupt the on-disk key so the next open sees WrongEncryptionKey.
        let key_path = mls_key::key_path_for_store(&store);
        std::fs::write(&key_path, [0xAB_u8; 32]).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let db = test_db().await;

        // retention_days = 0 -- still archived, per R26.
        let reset_occurred = classify_and_prepare(&db, "bot-wrongkey", &store, 0)
            .await
            .expect("wrong-key store resets");
        assert!(reset_occurred, "a wrong-key store must be reset");

        assert!(!store.exists());
        let marker = db
            .load_mls_store_reset_marker("bot-wrongkey")
            .await
            .unwrap()
            .expect("marker recorded");
        assert!(
            marker.archive_path.is_some(),
            "R26 archive must survive retention_days = 0"
        );
    }

    #[tokio::test]
    async fn interrupted_reset_with_only_sidecars_completes_the_move() {
        let dir = common_tempdir();
        let store = dir.path().join("vector-mls.db");
        // No main file, but a leftover -wal as if a prior run crashed
        // between removing the main file and its sidecars.
        std::fs::write(format!("{}-wal", store.display()), b"stale wal").unwrap();
        let db = test_db().await;

        let reset_occurred = classify_and_prepare(&db, "bot-interrupted", &store, 0)
            .await
            .expect("finish interrupted reset");
        assert!(
            !reset_occurred,
            "finishing a prior run's interrupted reset performs no new reset"
        );

        assert!(!Path::new(&format!("{}-wal", store.display())).exists());
        assert!(
            !store.exists(),
            "no fresh store is created by classification itself"
        );
    }

    #[tokio::test]
    async fn sidecars_are_found_for_a_store_not_named_dot_db() {
        let dir = common_tempdir();
        let store = dir.path().join("squad.sqlite");
        std::fs::write(format!("{}-wal", store.display()), b"stale wal").unwrap();
        std::fs::write(format!("{}-shm", store.display()), b"stale shm").unwrap();
        let db = test_db().await;

        let reset_occurred = classify_and_prepare(&db, "bot-nondb", &store, 0)
            .await
            .expect("classify");
        assert!(!reset_occurred, "only leftover sidecars, no new reset");

        assert!(!Path::new(&format!("{}-wal", store.display())).exists());
        assert!(!Path::new(&format!("{}-shm", store.display())).exists());
    }

    #[tokio::test]
    async fn concurrent_classification_for_the_same_bot_resets_exactly_once() {
        let dir = common_tempdir();
        let store = dir.path().join("vector-mls.db");
        build_legacy_store(&store, &[3u8; 32], &[]);
        let db = test_db().await;

        let store_a = store.clone();
        let db_a = db.clone();
        let store_b = store.clone();
        let db_b = db.clone();

        let (a, b) = tokio::join!(
            classify_and_prepare(&db_a, "bot-race", &store_a, 0),
            classify_and_prepare(&db_b, "bot-race", &store_b, 0),
        );

        // Whichever ran first performs the reset; the other, running after
        // the lock releases, sees a clear path and also succeeds -- exactly
        // one reset is recorded either way.
        assert!(a.is_ok() && b.is_ok(), "{a:?} / {b:?}");
        assert_eq!(
            [a.unwrap(), b.unwrap()].into_iter().filter(|r| *r).count(),
            1,
            "exactly one of the two concurrent calls performs the reset"
        );
        let marker = db
            .load_mls_store_reset_marker("bot-race")
            .await
            .unwrap()
            .expect("exactly one reset recorded");
        assert!(marker.reset_at.is_some());
    }

    #[tokio::test]
    async fn archive_pruning_respects_retention_and_r26_exemption() {
        let dir = common_tempdir();
        let store = dir.path().join("vector-mls.db");
        let archive_root = archive_root_for(&store);
        std::fs::create_dir_all(&archive_root).unwrap();

        let old_stamp = unix_now() - 30 * 86_400;
        let recent_stamp = unix_now();
        let old_legacy = archive_root.join(format!("{old_stamp}-aaaa-legacy"));
        let recent_legacy = archive_root.join(format!("{recent_stamp}-bbbb-legacy"));
        let old_r26 = archive_root.join(format!("{old_stamp}-cccc-r26"));
        std::fs::create_dir_all(&old_legacy).unwrap();
        std::fs::create_dir_all(&recent_legacy).unwrap();
        std::fs::create_dir_all(&old_r26).unwrap();

        prune_expired_archives(&store, 7).unwrap();

        assert!(!old_legacy.exists(), "an archive past its window is pruned");
        assert!(
            recent_legacy.exists(),
            "an archive inside its window is kept"
        );
        assert!(old_r26.exists(), "an R26 archive is exempt from pruning");
    }

    #[tokio::test]
    async fn zero_retention_prunes_nothing() {
        let dir = common_tempdir();
        let store = dir.path().join("vector-mls.db");
        let archive_root = archive_root_for(&store);
        let old = archive_root.join(format!("{}-aaaa-legacy", unix_now() - 30 * 86_400));
        std::fs::create_dir_all(&old).unwrap();

        prune_expired_archives(&store, 0).unwrap();

        assert!(
            old.exists(),
            "retention_days = 0 does not retroactively prune"
        );
    }

    /// `tempfile::tempdir()` resolves through `/var` -> `/private/var` on
    /// macOS, which the MLS path-safety check in `mls.rs` rejects as a
    /// symlinked parent when a store is actually opened by `new_with_key`.
    /// Mirrors `src/mls.rs`'s own `test_tempdir` helper.
    fn common_tempdir() -> tempfile::TempDir {
        let root = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"))
            .join("test-temp")
            .join("mls-reset-unit");
        std::fs::create_dir_all(&root).expect("create test temp root");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
                .expect("chmod test temp root");
        }
        tempfile::tempdir_in(root).expect("tempdir")
    }
}
