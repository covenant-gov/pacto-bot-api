mod common;
mod support;

/// req(R10, R29, R31, R35)
use assert_cmd::Command;
use pacto_bot_api::db::{Database, MlsGroupRow};
use pacto_bot_api::events::EventType;
use rusqlite::Connection;
use serde_json::json;
use std::error::Error;
use std::fs;

fn assert_mls_table_schema(
    conn: &Connection,
    table: &str,
    expected_columns: &[&str],
    expected_pk: &[&str],
    unique_first_col: Option<&str>,
    unique_second_col: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns: Vec<(String, i32, bool)> = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>("name")?,
                row.get::<_, i32>("pk")?,
                row.get::<_, bool>("notnull")?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let names: std::collections::HashSet<&str> = columns.iter().map(|c| c.0.as_str()).collect();
    let expected_names: std::collections::HashSet<&str> =
        expected_columns.iter().copied().collect();
    assert_eq!(names, expected_names, "{table} column set mismatch");

    let pk: std::collections::HashSet<&str> = columns
        .iter()
        .filter(|c| c.1 > 0)
        .map(|c| c.0.as_str())
        .collect();
    let expected_pk_set: std::collections::HashSet<&str> = expected_pk.iter().copied().collect();
    assert_eq!(pk, expected_pk_set, "{table} primary key mismatch");

    for col in &columns {
        // state_lost_at is a nullable INTEGER timestamp added by U10; every
        // other column here is a NOT NULL pubkey/name/id field.
        if col.0 == "state_lost_at" {
            assert!(!col.2, "{table}.state_lost_at should be nullable");
            continue;
        }
        assert!(col.2, "{table}.{} should be NOT NULL", col.0);
    }

    let mut unique_indexes = Vec::new();
    let mut idx_stmt = conn.prepare(&format!("PRAGMA index_list({table})"))?;
    let indexes: Vec<(String, i32)> = idx_stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>("name")?, row.get::<_, i32>("unique")?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (name, unique) in indexes {
        if unique != 1 {
            continue;
        }
        let mut info_stmt = conn.prepare(&format!("PRAGMA index_info({name})"))?;
        let cols: Vec<String> = info_stmt
            .query_map([], |row| row.get::<_, String>("name"))?
            .collect::<Result<Vec<_>, _>>()?;
        unique_indexes.push(cols);
    }

    let pk_index: Vec<String> = expected_pk.iter().map(|s| (*s).to_string()).collect();
    assert!(
        unique_indexes.contains(&pk_index),
        "{table} primary key index missing"
    );

    if let (Some(first), Some(second)) = (unique_first_col, unique_second_col) {
        let composite_index: Vec<String> = vec![first.to_string(), second.to_string()];
        assert!(
            unique_indexes.contains(&composite_index),
            "{table}.({first}, {second}) unique index missing"
        );
    } else if let Some(unique_col) = unique_first_col {
        let unique_col_index: Vec<String> = vec![unique_col.to_string()];
        assert!(
            unique_indexes.contains(&unique_col_index),
            "{table}.{unique_col} unique index missing"
        );
    }

    Ok(())
}

fn assert_mls_tables_in_schema(path: &std::path::Path) -> Result<(), Box<dyn Error>> {
    let conn = Connection::open(path)?;
    assert_mls_table_schema(
        &conn,
        "mls_groups",
        &[
            "bot_id",
            "group_name",
            "wire_id",
            "creator_npub",
            "relay",
            "invited_bots",
            "state_lost_at",
        ],
        &["bot_id", "group_name"],
        Some("bot_id"),
        Some("wire_id"),
    )?;
    assert_mls_table_schema(
        &conn,
        "mls_group_members",
        &["bot_id", "group_name", "member_npub"],
        &["bot_id", "group_name", "member_npub"],
        None,
        None,
    )?;

    for table in ["mls_groups", "mls_group_members"] {
        let mut stmt = conn.prepare(&format!("PRAGMA foreign_key_list({table})"))?;
        let count: usize = stmt.query_map([], |_| Ok(()))?.count();
        assert_eq!(count, 0, "{table} should declare no foreign keys");
    }

    Ok(())
}

/// Append `mls_db_path` to the single bot section `common::make_config`
/// wrote, since that helper doesn't emit it (most tests in this file
/// exercise the non-MLS export/import path and don't need it).
fn append_mls_db_path(
    config_path: &std::path::Path,
    mls_db_path: &str,
) -> Result<(), Box<dyn Error>> {
    use std::io::Write as _;
    let mut file = fs::OpenOptions::new().append(true).open(config_path)?;
    writeln!(file, "mls_db_path = {mls_db_path:?}")?;
    Ok(())
}

/// Return type for [`build_encrypted_store_with_group`]: the store's key
/// and the created group's name.
type StoreWithGroup = (zeroize::Zeroizing<[u8; 32]>, String);

/// Build a real encrypted MLS store at `store_path`, matching the fixture
/// pattern `src/mls_reset.rs`'s own tests use (`mls_key::load_or_create` +
/// `MdkSqliteStorage::new_with_key`), containing one self-group (creator
/// only, no invitees -- MDK's documented empty-invitee "message to self"
/// path). Returns the store's key and the created group's name so a test
/// can assert the group is still present after a copy round-trip.
fn build_encrypted_store_with_group(
    store_path: &std::path::Path,
) -> Result<StoreWithGroup, Box<dyn Error>> {
    if let Some(parent) = store_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let key = pacto_bot_api::mls_key::load_or_create(store_path)?;
    let storage = mdk_sqlite_storage::MdkSqliteStorage::new_with_key(
        store_path,
        mdk_sqlite_storage::encryption::EncryptionConfig::new(*key),
    )?;
    let engine = mdk_core::MDK::new(storage);
    let keys = nostr::Keys::generate();
    let config = mdk_core::prelude::NostrGroupConfigData::new(
        "Bundle Test Squad".to_string(),
        "created for a U12 export/import test".to_string(),
        None,
        None,
        None,
        vec![nostr::RelayUrl::parse("wss://test.relay")?],
        vec![keys.public_key()],
    );
    let result = engine.create_group(&keys.public_key(), vec![], config)?;
    Ok((key, result.group.name))
}

/// Build a minimal `ExportState`-shaped JSON blob for hand-crafted import
/// tests, matching `import_validates_bot_exists_in_config`'s inline shape
/// but parameterized on `mls_groups`/`mls_store`.
fn export_state_json(
    mls_groups: serde_json::Value,
    mls_store: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut value = json!({
        "metadata": {
            "daemon_version": "0.1.0",
            "exported_at": "2026-01-01T00:00:00Z",
            "source_data_dir": "/tmp"
        },
        "cursors": [],
        "handlers": [],
        "mls_groups": mls_groups,
        "split_brain_warning": true
    });
    if let Some(store) = mls_store {
        value["mls_store"] = store;
    }
    value
}

#[test]
fn export_import_roundtrip() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot.clone()])?;

    let handler = common::handler_ref(
        "handler-1",
        &["echo-bot"],
        &[EventType::DmReceived],
        &["ReadMessages"],
    );
    common::populate_db(&dir, "echo-bot", &bot.npub, 42, vec![handler])?;

    // Export
    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args(["--config", &config.to_string_lossy(), "export", "echo-bot"]);
    let output = cmd.assert().success();
    let state_json = std::str::from_utf8(&output.get_output().stdout)?;

    let state: serde_json::Value = serde_json::from_str(state_json)?;
    assert_eq!(state["cursors"].as_array().map(|a| a.len()), Some(1));
    assert_eq!(state["handlers"].as_array().map(|a| a.len()), Some(1));
    assert_eq!(state["split_brain_warning"], true);

    // Save state to file, delete DB, then import
    let state_path = dir.path().join("state.json");
    fs::write(&state_path, state_json)?;
    fs::remove_file(dir.path().join("agent.db"))?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &config.to_string_lossy(),
        "import",
        "echo-bot",
        &state_path.to_string_lossy(),
    ]);
    cmd.assert().success();

    let db = Database::open(&dir.path().join("agent.db"))?;
    let cursor = db
        .load_cursor("echo-bot")?
        .ok_or("cursor missing after import")?;
    assert_eq!(cursor.1, 42);
    let handlers = db.load_handlers()?;
    assert_eq!(handlers.len(), 1);
    Ok(())
}

#[test]
fn export_import_roundtrips_mls_groups() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot.clone()])?;

    // Seed an MLS group row for the bot.
    {
        let db = Database::open(&dir.path().join("agent.db"))?;
        let row = MlsGroupRow {
            bot_id: "echo-bot".to_string(),
            group_name: "my-squad".to_string(),
            wire_id: "aabbccdd".to_string(),
            creator_npub: bot.npub.clone(),
            relay: "wss://relay.example.com".to_string(),
            invited_bots: vec!["npub1member".to_string()],
            state_lost_at: None,
        };
        db.insert_mls_group_export(&row)?;
    }

    // Export
    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args(["--config", &config.to_string_lossy(), "export", "echo-bot"]);
    let output = cmd.assert().success();
    let state_json = std::str::from_utf8(&output.get_output().stdout)?;

    let state: serde_json::Value = serde_json::from_str(state_json)?;
    let mls_groups = state["mls_groups"].as_array().expect("mls_groups array");
    assert_eq!(mls_groups.len(), 1);
    assert_eq!(mls_groups[0]["group_name"], "my-squad");
    assert_eq!(mls_groups[0]["wire_id"], "aabbccdd");
    assert_eq!(mls_groups[0]["invited_bots"], json!(["npub1member"]));

    // Save state to file, delete DB, then import
    let state_path = dir.path().join("state.json");
    fs::write(&state_path, state_json)?;
    fs::remove_file(dir.path().join("agent.db"))?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &config.to_string_lossy(),
        "import",
        "echo-bot",
        &state_path.to_string_lossy(),
    ]);
    cmd.assert().success();

    let db = Database::open(&dir.path().join("agent.db"))?;
    let groups = db.load_all_mls_groups("echo-bot")?;
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].group_name, "my-squad");
    assert_eq!(groups[0].wire_id, "aabbccdd");
    assert_eq!(groups[0].invited_bots, vec!["npub1member"]);
    assert_mls_tables_in_schema(&dir.path().join("agent.db"))?;
    Ok(())
}

#[test]
fn export_import_bundles_mls_store_and_decrypts_group() -> Result<(), Box<dyn Error>> {
    let source_dir = common::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let source_config = common::make_config(&source_dir, vec![bot.clone()])?;
    append_mls_db_path(&source_config, "mls.db")?;

    let store_path = source_dir.path().join("echo-bot").join("mls.db");
    let (key, group_name) = build_encrypted_store_with_group(&store_path)?;
    let key_hex = hex::encode(*key);

    let bundle_dir = source_dir.path().join("bundle");
    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &source_config.to_string_lossy(),
        "export",
        "echo-bot",
        "--bundle-dir",
        &bundle_dir.to_string_lossy(),
    ]);
    let output = cmd.assert().success();
    let state_json = std::str::from_utf8(&output.get_output().stdout)?.to_string();

    // The manifest is a bare filename; no key bytes and no absolute
    // store/key path reach the JSON blob.
    let state: serde_json::Value = serde_json::from_str(&state_json)?;
    assert_eq!(state["mls_store"]["store_file"], "mls.db");
    assert!(
        !state_json.contains(&key_hex),
        "export JSON must not contain key material"
    );
    assert!(
        !state_json.contains(&store_path.display().to_string()),
        "export JSON must not contain the absolute store path"
    );

    let state_path = source_dir.path().join("state.json");
    fs::write(&state_path, &state_json)?;

    // Import into a clean data dir: a fresh install that has never seen
    // this bot's store before.
    let dest_dir = common::tempdir()?;
    let dest_config = common::make_config(&dest_dir, vec![bot.clone()])?;
    append_mls_db_path(&dest_config, "mls.db")?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &dest_config.to_string_lossy(),
        "import",
        "echo-bot",
        &state_path.to_string_lossy(),
        "--bundle-dir",
        &bundle_dir.to_string_lossy(),
    ]);
    let output = cmd.assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;
    assert!(stdout.contains("imported MLS store"), "{stdout}");

    let dest_store_path = dest_dir.path().join("echo-bot").join("mls.db");
    assert!(dest_store_path.exists(), "imported store must exist");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let key_path = pacto_bot_api::mls_key::key_path_for_store(&dest_store_path);
        let mode = fs::metadata(&key_path)?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "imported key file must be 0o600");
    }

    // The imported store opens with the imported key and still decrypts
    // the group created before export.
    let dest_key = pacto_bot_api::mls_key::load(&dest_store_path)?.expect("imported key present");
    let storage = mdk_sqlite_storage::MdkSqliteStorage::new_with_key(
        &dest_store_path,
        mdk_sqlite_storage::encryption::EncryptionConfig::new(*dest_key),
    )?;
    let engine = mdk_core::MDK::new(storage);
    let groups = engine.get_groups()?;
    assert!(
        groups.iter().any(|g| g.name == group_name),
        "imported store must still contain the group created before export"
    );

    Ok(())
}

#[test]
fn export_bundle_dir_and_key_permissions_are_restricted() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot.clone()])?;
    append_mls_db_path(&config, "mls.db")?;

    let store_path = dir.path().join("echo-bot").join("mls.db");
    build_encrypted_store_with_group(&store_path)?;

    let bundle_dir = dir.path().join("bundle");
    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &config.to_string_lossy(),
        "export",
        "echo-bot",
        "--bundle-dir",
        &bundle_dir.to_string_lossy(),
    ]);
    cmd.assert().success();

    // `create_restricted_dir`/`create_restricted_file` set explicit mode
    // bits at creation and never rely on the process umask to narrow a
    // permissive default, so these hold under any umask.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir_mode = fs::metadata(&bundle_dir)?.permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "bundle dir must be 0o700");

        let key_path = bundle_dir.join("mls.db.key");
        let key_mode = fs::metadata(&key_path)?.permissions().mode() & 0o777;
        assert_eq!(key_mode, 0o600, "bundle key file must be 0o600");
    }

    Ok(())
}

#[test]
fn import_with_missing_key_imports_metadata_and_marks_state_lost() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot.clone()])?;
    append_mls_db_path(&config, "mls.db")?;

    // A bundle whose store file exists but has no `.key` sidecar.
    let bundle_dir = dir.path().join("bundle");
    fs::create_dir_all(&bundle_dir)?;
    fs::write(bundle_dir.join("mls.db"), b"not a real sqlite file")?;

    let state_path = dir.path().join("state.json");
    let state = export_state_json(
        json!([{
            "bot_id": "echo-bot",
            "group_name": "my-squad",
            "wire_id": "aabbccdd",
            "creator_npub": bot.npub,
            "relay": "wss://relay.example.com",
            "invited_bots": []
        }]),
        Some(json!({ "store_file": "mls.db", "sidecar_files": [] })),
    );
    fs::write(&state_path, state.to_string())?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &config.to_string_lossy(),
        "import",
        "echo-bot",
        &state_path.to_string_lossy(),
        "--bundle-dir",
        &bundle_dir.to_string_lossy(),
    ]);
    let output = cmd.assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;
    assert!(
        stdout.contains("skipped"),
        "must report the store skip: {stdout}"
    );

    let dest_store_path = dir.path().join("echo-bot").join("mls.db");
    assert!(
        !dest_store_path.exists(),
        "store must not be imported without a key"
    );

    let db = Database::open(&dir.path().join("agent.db"))?;
    let groups = db.load_all_mls_groups("echo-bot")?;
    assert_eq!(groups.len(), 1, "group metadata must still import");
    assert!(
        groups[0].state_lost_at.is_some(),
        "group without a working store must be marked state-lost"
    );

    Ok(())
}

#[test]
fn import_with_wrong_key_marks_state_lost_and_does_not_archive() -> Result<(), Box<dyn Error>> {
    let source_dir = common::tempdir()?;
    let real_store_path = source_dir.path().join("source-mls.db");
    build_encrypted_store_with_group(&real_store_path)?;

    let dir = common::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot.clone()])?;
    append_mls_db_path(&config, "mls.db")?;

    let bundle_dir = dir.path().join("bundle");
    fs::create_dir_all(&bundle_dir)?;
    fs::copy(&real_store_path, bundle_dir.join("mls.db"))?;
    let key_path = bundle_dir.join("mls.db.key");
    fs::write(&key_path, [0xABu8; 32])?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))?;
    }

    let state_path = dir.path().join("state.json");
    let state = export_state_json(
        json!([{
            "bot_id": "echo-bot",
            "group_name": "my-squad",
            "wire_id": "aabbccdd",
            "creator_npub": bot.npub,
            "relay": "wss://relay.example.com",
            "invited_bots": []
        }]),
        Some(json!({ "store_file": "mls.db", "sidecar_files": [] })),
    );
    fs::write(&state_path, state.to_string())?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &config.to_string_lossy(),
        "import",
        "echo-bot",
        &state_path.to_string_lossy(),
        "--bundle-dir",
        &bundle_dir.to_string_lossy(),
    ]);
    let output = cmd.assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;
    assert!(
        stdout.contains("skipped"),
        "must report the store skip: {stdout}"
    );

    let dest_store_path = dir.path().join("echo-bot").join("mls.db");
    assert!(
        !dest_store_path.exists(),
        "store must not be imported with the wrong key"
    );

    let db = Database::open(&dir.path().join("agent.db"))?;
    let groups = db.load_all_mls_groups("echo-bot")?;
    assert_eq!(groups.len(), 1);
    assert!(groups[0].state_lost_at.is_some());

    // Import must never invoke `mls_reset`'s archive machinery.
    assert!(!dir.path().join("echo-bot").join("mls-archive").exists());
    assert!(!dir.path().join("mls-archive").exists());

    Ok(())
}

#[test]
fn import_rejects_manifest_entries_with_path_traversal() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot.clone()])?;
    append_mls_db_path(&config, "mls.db")?;

    let bundle_dir = dir.path().join("bundle");
    fs::create_dir_all(&bundle_dir)?;

    for malicious in ["../evil.db", "/etc/passwd", "sub/dir.db", "..", "."] {
        let state_path = dir.path().join("state.json");
        let state = export_state_json(
            json!([]),
            Some(json!({ "store_file": malicious, "sidecar_files": [] })),
        );
        fs::write(&state_path, state.to_string())?;

        let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
        cmd.args([
            "--config",
            &config.to_string_lossy(),
            "import",
            "echo-bot",
            &state_path.to_string_lossy(),
            "--bundle-dir",
            &bundle_dir.to_string_lossy(),
        ]);
        cmd.assert().success();

        let bot_data_dir = dir.path().join("echo-bot");
        let entries: Vec<_> = fs::read_dir(&bot_data_dir)?.collect::<Result<_, _>>()?;
        assert!(
            entries.is_empty(),
            "rejected manifest entry {malicious:?} must write nothing to the bot data dir: {entries:?}"
        );
    }

    Ok(())
}

#[cfg(unix)]
#[test]
fn import_rejects_destination_that_escapes_via_symlink() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot.clone()])?;
    append_mls_db_path(&config, "mls.db")?;

    // Pre-create the bot data dir with a symlink at the configured store
    // path that points to a real file outside it.
    let bot_data_dir = dir.path().join("echo-bot");
    fs::create_dir_all(&bot_data_dir)?;
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&bot_data_dir, fs::Permissions::from_mode(0o700))?;
    }
    let victim_dir = common::tempdir()?;
    let victim_path = victim_dir.path().join("victim.db");
    fs::write(&victim_path, b"do not touch")?;
    std::os::unix::fs::symlink(&victim_path, bot_data_dir.join("mls.db"))?;

    let state_path = dir.path().join("state.json");
    fs::write(&state_path, export_state_json(json!([]), None).to_string())?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &config.to_string_lossy(),
        "import",
        "echo-bot",
        &state_path.to_string_lossy(),
    ]);
    cmd.assert().failure();

    assert_eq!(
        fs::read_to_string(&victim_path)?,
        "do not touch",
        "a symlink escaping the bot data dir must never be written through"
    );

    Ok(())
}

#[test]
fn migration_creates_mls_tables() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let _config = common::make_config(&dir, vec![bot])?;

    let db_path = dir.path().join("agent.db");
    let _db = Database::open(&db_path)?;
    drop(_db);

    assert_mls_tables_in_schema(&db_path)?;
    Ok(())
}

#[test]
fn export_refuses_when_daemon_lock_held() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot])?;
    let _lock = common::hold_daemon_lock(&dir)?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args(["--config", &config.to_string_lossy(), "export", "echo-bot"]);
    let output = cmd.assert().failure();
    let stderr = std::str::from_utf8(&output.get_output().stderr)?;
    assert!(stderr.contains("daemon lock is held"));
    Ok(())
}

#[test]
fn rotate_http_token_refuses_when_daemon_lock_held() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot])?;
    let _lock = common::hold_daemon_lock(&dir)?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args(["--config", &config.to_string_lossy(), "rotate-http-token"]);
    let output = cmd.assert().failure();
    let stderr = std::str::from_utf8(&output.get_output().stderr)?;
    assert!(stderr.contains("daemon lock is held"));
    Ok(())
}

#[test]
fn validate_config_reports_duplicate_bot_id() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let config_path = dir.path().join("pacto-bot-api.toml");
    let content = r#"
[[bots]]
id = "echo-bot"
npub = "npub1a"
signing = { backend = "nsec", nsec = "nsec1a" }

[[bots]]
id = "echo-bot"
npub = "npub1b"
signing = { backend = "nsec", nsec = "nsec1b" }
"#;
    fs::write(&config_path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&config_path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&config_path, perms)?;
    }

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &config_path.to_string_lossy(),
        "validate-config",
    ]);
    let output = cmd.assert().failure();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;
    assert!(stdout.contains("duplicate bot_id"));
    Ok(())
}

#[test]
fn validate_config_reports_loose_permissions() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let config_path = dir.path().join("pacto-bot-api.toml");
    let content = r#"
[[bots]]
id = "echo-bot"
npub = "npub1a"
signing = { backend = "nsec", nsec = "nsec1a" }
"#;
    common::write_loose_config(&config_path, content)?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &config_path.to_string_lossy(),
        "validate-config",
    ]);
    let output = cmd.assert().failure();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;
    assert!(stdout.contains("must be readable only by owner"));
    Ok(())
}

#[test]
fn rotate_http_token_creates_restricted_token() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot])?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args(["--config", &config.to_string_lossy(), "rotate-http-token"]);
    cmd.assert().success();

    let token_path = dir.path().join("bot_secret_token");
    let token = fs::read_to_string(&token_path)?;
    assert_eq!(token.len(), 64);
    assert!(token.chars().all(|c| c.is_ascii_hexdigit()));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&token_path)?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    Ok(())
}

#[test]
fn diagnose_reports_config_and_lock_status() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot])?;
    let _lock = common::hold_daemon_lock(&dir)?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &config.to_string_lossy(),
        "diagnose",
        "--format",
        "json",
    ]);
    let output = cmd.assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;
    let report: serde_json::Value = serde_json::from_str(stdout)?;

    assert_eq!(report["config_valid"], true);
    assert_eq!(report["lock_held"], true);
    assert!(!report["data_dir"].as_str().unwrap_or("").is_empty());
    assert_eq!(report["bots"].as_array().map(|a| a.len()), Some(1));
    assert_eq!(report["db_cursor_count"], 0);

    assert!(
        report.get("socket").is_some(),
        "report should include socket health"
    );
    assert_eq!(report["socket"]["exists"], false);
    assert!(!report["socket"]["path"].as_str().unwrap_or("").is_empty());
    assert!(
        report.get("relay_connectivity").is_some(),
        "report should include relay_connectivity"
    );
    assert!(
        report.get("bunker_connectivity").is_some(),
        "report should include bunker_connectivity"
    );
    assert!(
        report.get("service_versions").is_some(),
        "report should include service_versions"
    );
    Ok(())
}

#[test]
fn diagnose_text_format_reports_bots() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot])?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args(["--config", &config.to_string_lossy(), "diagnose"]);
    let output = cmd.assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;

    assert!(stdout.contains("config_valid: true"));
    assert!(stdout.contains("id: echo-bot"));
    assert!(stdout.contains("signing_backend: nsec"));
    Ok(())
}

#[test]
fn import_validates_bot_exists_in_config() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot])?;

    let state_path = dir.path().join("state.json");
    fs::write(
        &state_path,
        serde_json::json!({
            "metadata": {
                "daemon_version": "0.1.0",
                "exported_at": "2026-01-01T00:00:00Z",
                "source_data_dir": "/tmp"
            },
            "cursors": [],
            "handlers": [],
            "split_brain_warning": true
        })
        .to_string(),
    )?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &config.to_string_lossy(),
        "import",
        "missing-bot",
        &state_path.to_string_lossy(),
    ]);
    let output = cmd.assert().failure();
    let stderr = std::str::from_utf8(&output.get_output().stderr)?;
    assert!(stderr.contains("unknown bot"));
    Ok(())
}

#[test]
fn validate_config_reports_npub_mismatch_with_db() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let (bot, _nsec) = common::generate_nsec_bot("echo-bot")?;
    let config = common::make_config(&dir, vec![bot.clone()])?;

    // Persist a cursor with a different npub than the config.
    {
        let db = Database::open(&dir.path().join("agent.db"))?;
        db.save_cursor("echo-bot", "npub1other", 7)?;
    }

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args(["--config", &config.to_string_lossy(), "validate-config"]);
    let output = cmd.assert().failure();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;
    assert!(stdout.contains("DB npub") && stdout.contains("does not match config npub"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn diagnose_reports_relay_connectivity_with_mock_relay() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let relay = support::mock_relay::MockRelay::start().await?;
    let relay_url = relay.url();

    let (mut bot, _nsec) = common::generate_nsec_bot("relay-bot")?;
    bot.relays.push(relay_url.clone());
    let config = common::make_config(&dir, vec![bot])?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &config.to_string_lossy(),
        "diagnose",
        "--format",
        "json",
    ]);
    let output = cmd.assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;
    let report: serde_json::Value = serde_json::from_str(stdout)?;

    let checks = report["relay_connectivity"]
        .as_array()
        .ok_or("relay_connectivity should be an array")?;
    assert_eq!(checks.len(), 2);
    let live_check = checks
        .iter()
        .find(|c| c["relay"] == relay_url)
        .ok_or("expected check for mock relay")?;
    assert_eq!(live_check["bot_id"], "relay-bot");
    if live_check["reachable"] != true {
        panic!(
            "mock relay should be reachable; got error: {:?}",
            live_check["error"]
        );
    }

    relay.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn diagnose_reports_bunker_connectivity_with_mock_relay() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let relay = support::mock_relay::MockRelay::start().await?;
    let relay_url = relay.url();

    let mut bot = common::generate_bunker_bot("bunker-bot", true)?;
    let bunker_uri = format!(
        "bunker://{}?relay={}",
        nostr::Keys::generate().public_key().to_hex(),
        relay_url
    );
    common::set_bunker_uri(&mut bot, &bunker_uri);
    let config = common::make_config(&dir, vec![bot])?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "--config",
        &config.to_string_lossy(),
        "diagnose",
        "--format",
        "json",
    ]);
    let output = cmd.assert().success();
    let stdout = std::str::from_utf8(&output.get_output().stdout)?;
    let report: serde_json::Value = serde_json::from_str(stdout)?;

    let checks = report["bunker_connectivity"]
        .as_array()
        .ok_or("bunker_connectivity should be an array")?;
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0]["bot_id"], "bunker-bot");
    assert_eq!(checks[0]["reachable"], true);

    relay.stop().await;
    Ok(())
}
