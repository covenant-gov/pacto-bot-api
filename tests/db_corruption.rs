use pacto_bot_api::db::Database;
use rusqlite::Connection;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

const MIGRATION_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS cursors (
    bot_id TEXT PRIMARY KEY,
    npub TEXT NOT NULL,
    last_event_id TEXT,
    updated_at INTEGER
);
CREATE TABLE IF NOT EXISTS handlers (
    handler_id TEXT PRIMARY KEY,
    bot_ids TEXT NOT NULL,
    event_types TEXT NOT NULL,
    capabilities TEXT NOT NULL,
    reconnect_token TEXT NOT NULL,
    registered_at INTEGER
);
CREATE TABLE IF NOT EXISTS event_trace (
    bot_id TEXT NOT NULL,
    event_id TEXT NOT NULL,
    author TEXT NOT NULL,
    content_preview TEXT NOT NULL,
    action TEXT NOT NULL,
    reply_event_id TEXT,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_event_trace_bot_created
    ON event_trace (bot_id, created_at DESC);
"#;

fn temp_dir() -> Result<TempDir, Box<dyn std::error::Error>> {
    Ok(tempfile::tempdir()?)
}

fn db_path(dir: &TempDir, name: &str) -> std::path::PathBuf {
    dir.path().join(name)
}

fn setup_schema(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::open(path)?;
    conn.execute_batch(MIGRATION_SQL)?;
    Ok(())
}

#[test]
fn open_rejects_invalid_sqlite_header() -> Result<(), Box<dyn std::error::Error>> {
    let dir = temp_dir()?;
    let path = db_path(&dir, "corrupt_header.db");

    fs::write(&path, b"this is definitely not a sqlite database")?;

    let result = Database::open(&path);
    assert!(
        result.is_err(),
        "opening a file with an invalid SQLite header must return an error"
    );

    Ok(())
}

#[test]
fn open_rejects_truncated_sqlite_file() -> Result<(), Box<dyn std::error::Error>> {
    let dir = temp_dir()?;
    let path = db_path(&dir, "truncated.db");

    // A valid SQLite header followed by no usable page data.
    let mut truncated = b"SQLite format 3\x00".to_vec();
    truncated.extend_from_slice(&[0u8; 100]);
    fs::write(&path, truncated)?;

    let result = Database::open(&path);
    assert!(
        result.is_err(),
        "opening a truncated SQLite file must return an error"
    );

    Ok(())
}

#[test]
fn load_handlers_rejects_invalid_json_in_bot_ids() -> Result<(), Box<dyn std::error::Error>> {
    let dir = temp_dir()?;
    let path = db_path(&dir, "bad_bot_ids.db");

    setup_schema(&path)?;

    let conn = Connection::open(&path)?;
    conn.execute(
        "INSERT INTO handlers (handler_id, bot_ids, event_types, capabilities, reconnect_token, registered_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        (
            "h-1",
            "not valid json",
            "[\"dm_received\"]",
            "[\"send_messages\"]",
            "token",
            0_i64,
        ),
    )?;
    drop(conn);

    let db = Database::open(&path)?;
    let result = db.load_handlers();
    assert!(
        result.is_err(),
        "load_handlers must return an error when bot_ids contains invalid JSON"
    );

    Ok(())
}

#[test]
fn load_handlers_rejects_invalid_json_in_event_types() -> Result<(), Box<dyn std::error::Error>> {
    let dir = temp_dir()?;
    let path = db_path(&dir, "bad_event_types.db");

    setup_schema(&path)?;

    let conn = Connection::open(&path)?;
    conn.execute(
        "INSERT INTO handlers (handler_id, bot_ids, event_types, capabilities, reconnect_token, registered_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        (
            "h-1",
            "[\"bot-1\"]",
            "not valid json",
            "[\"send_messages\"]",
            "token",
            0_i64,
        ),
    )?;
    drop(conn);

    let db = Database::open(&path)?;
    let result = db.load_handlers();
    assert!(
        result.is_err(),
        "load_handlers must return an error when event_types contains invalid JSON"
    );

    Ok(())
}

#[test]
fn load_handlers_rejects_invalid_json_in_capabilities() -> Result<(), Box<dyn std::error::Error>> {
    let dir = temp_dir()?;
    let path = db_path(&dir, "bad_capabilities.db");

    setup_schema(&path)?;

    let conn = Connection::open(&path)?;
    conn.execute(
        "INSERT INTO handlers (handler_id, bot_ids, event_types, capabilities, reconnect_token, registered_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        (
            "h-1",
            "[\"bot-1\"]",
            "[\"dm_received\"]",
            "not valid json",
            "token",
            0_i64,
        ),
    )?;
    drop(conn);

    let db = Database::open(&path)?;
    let result = db.load_handlers();
    assert!(
        result.is_err(),
        "load_handlers must return an error when capabilities contains invalid JSON"
    );

    Ok(())
}

#[test]
fn load_handlers_rejects_malformed_event_type_value() -> Result<(), Box<dyn std::error::Error>> {
    let dir = temp_dir()?;
    let path = db_path(&dir, "bad_event_type_value.db");

    setup_schema(&path)?;

    let conn = Connection::open(&path)?;
    conn.execute(
        "INSERT INTO handlers (handler_id, bot_ids, event_types, capabilities, reconnect_token, registered_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        (
            "h-1",
            "[\"bot-1\"]",
            "[\"unknown_event_type\"]",
            "[\"send_messages\"]",
            "token",
            0_i64,
        ),
    )?;
    drop(conn);

    let db = Database::open(&path)?;
    let result = db.load_handlers();
    assert!(
        result.is_err(),
        "load_handlers must return an error when event_types contains an unrecognized variant"
    );

    Ok(())
}

#[test]
fn load_cursor_rejects_non_integer_last_event_id() -> Result<(), Box<dyn std::error::Error>> {
    let dir = temp_dir()?;
    let path = db_path(&dir, "bad_cursor.db");

    setup_schema(&path)?;

    let conn = Connection::open(&path)?;
    conn.execute(
        "INSERT INTO cursors (bot_id, npub, last_event_id, updated_at)
         VALUES (?1, ?2, ?3, ?4)",
        ("bot-1", "npub-1", "not-a-number", 0_i64),
    )?;
    drop(conn);

    let db = Database::open(&path)?;
    let result = db.load_cursor("bot-1");
    assert!(
        result.is_err(),
        "load_cursor must return an error when last_event_id is not a valid integer string"
    );

    Ok(())
}
