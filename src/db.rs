use crate::errors::DaemonError;
use crate::handlers::{ConnectionHandle, HandlerRef};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension};
use std::path::Path;
use tokio::sync::mpsc::channel;

/// SQLite persistence handle for cursors and handler registrations.
#[derive(Debug)]
pub struct Database {
    conn: Connection,
}

impl Database {
    /// Open (or create) the SQLite database at `path`, enable WAL mode, set
    /// synchronous=NORMAL, and run idempotent migrations.
    pub fn open(path: &Path) -> Result<Self, DaemonError> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;",
        )?;
        let db = Self { conn };
        db.run_migrations()?;
        Ok(db)
    }

    /// Run idempotent migrations to create required tables.
    pub fn run_migrations(&self) -> Result<(), DaemonError> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS cursors (
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
                registered_at INTEGER
            );",
        )?;
        Ok(())
    }

    /// Persist or update the cursor for `bot_id`.
    pub fn save_cursor(&self, bot_id: &str, npub: &str, cursor: i64) -> Result<(), DaemonError> {
        let now = Utc::now().timestamp();
        let last_event_id = cursor.to_string();
        self.conn.execute(
            "INSERT INTO cursors (bot_id, npub, last_event_id, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(bot_id) DO UPDATE SET
                npub = excluded.npub,
                last_event_id = excluded.last_event_id,
                updated_at = excluded.updated_at",
            (bot_id, npub, last_event_id, now),
        )?;
        Ok(())
    }

    /// Load the stored npub and cursor for `bot_id`.
    ///
    /// Returns `None` when no cursor has been persisted for the bot.
    pub fn load_cursor(&self, bot_id: &str) -> Result<Option<(String, i64)>, DaemonError> {
        let row: Option<(String, Option<String>)> = self
            .conn
            .query_row(
                "SELECT npub, last_event_id FROM cursors WHERE bot_id = ?1",
                [bot_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match row {
            Some((npub, Some(last_event_id))) => {
                let cursor = last_event_id
                    .parse::<i64>()
                    .map_err(|e| DaemonError::Config(format!("invalid cursor in database: {e}")))?;
                Ok(Some((npub, cursor)))
            }
            Some((npub, None)) => Ok(Some((npub, 0))),
            None => Ok(None),
        }
    }

    /// Return true if the stored npub for `bot_id` matches `npub`.
    ///
    /// Returns true when there is no stored cursor, because there is nothing
    /// to validate.
    pub fn validate_npub(&self, bot_id: &str, npub: &str) -> Result<bool, DaemonError> {
        let stored: Option<String> = self
            .conn
            .query_row(
                "SELECT npub FROM cursors WHERE bot_id = ?1",
                [bot_id],
                |row| row.get(0),
            )
            .optional()?;
        match stored {
            Some(stored_npub) => Ok(stored_npub == npub),
            None => Ok(true),
        }
    }

    /// Reset the cursor for `bot_id`, removing any persisted event position.
    pub fn reset_cursor(&self, bot_id: &str) -> Result<(), DaemonError> {
        self.conn
            .execute("DELETE FROM cursors WHERE bot_id = ?1", [bot_id])?;
        Ok(())
    }

    /// Persist a handler registration, replacing any existing row.
    pub fn save_handler(&self, handler: &HandlerRef) -> Result<(), DaemonError> {
        let registered_at = handler.registered_at.timestamp();
        let bot_ids = serde_json::to_string(&handler.bot_ids)?;
        let event_types = serde_json::to_string(&handler.event_types)?;
        let capabilities = serde_json::to_string(&handler.capabilities)?;
        self.conn.execute(
            "INSERT INTO handlers (handler_id, bot_ids, event_types, capabilities, registered_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(handler_id) DO UPDATE SET
                bot_ids = excluded.bot_ids,
                event_types = excluded.event_types,
                capabilities = excluded.capabilities,
                registered_at = excluded.registered_at",
            (
                &handler.id,
                bot_ids,
                event_types,
                capabilities,
                registered_at,
            ),
        )?;
        Ok(())
    }

    /// Load all persisted handler registrations.
    ///
    /// Loaded handlers receive a disconnected connection handle; any attempt to
    /// send events to them will fail until they reconnect and re-register.
    pub fn load_handlers(&self) -> Result<Vec<HandlerRef>, DaemonError> {
        let mut stmt = self.conn.prepare(
            "SELECT handler_id, bot_ids, event_types, capabilities, registered_at FROM handlers",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        let mut handlers = Vec::new();
        for row in rows {
            let (id, bot_ids, event_types, capabilities, registered_at) = row?;
            let (sender, _receiver) = channel(1);
            handlers.push(HandlerRef {
                id,
                connection: Some(ConnectionHandle::new(sender)),
                bot_ids: serde_json::from_str(&bot_ids)?,
                event_types: serde_json::from_str(&event_types)?,
                capabilities: serde_json::from_str(&capabilities)?,
                registered_at: DateTime::from_timestamp(registered_at, 0).unwrap_or_else(Utc::now),
            });
        }
        Ok(handlers)
    }

    /// Delete a persisted handler registration.
    pub fn delete_handler(&self, handler_id: &str) -> Result<(), DaemonError> {
        self.conn
            .execute("DELETE FROM handlers WHERE handler_id = ?1", [handler_id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventType;
    use rusqlite::Connection;

    fn in_memory_db() -> Result<Database, DaemonError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;",
        )?;
        let db = Database { conn };
        db.run_migrations()?;
        Ok(db)
    }

    fn disconnected_handle() -> ConnectionHandle {
        let (sender, _receiver) = channel(1);
        ConnectionHandle::new(sender)
    }

    fn handler_ref(
        id: &str,
        bot_ids: &[&str],
        event_types: &[EventType],
        capabilities: &[&str],
    ) -> HandlerRef {
        HandlerRef {
            id: id.to_string(),
            connection: Some(disconnected_handle()),
            bot_ids: bot_ids.iter().map(|s| s.to_string()).collect(),
            event_types: event_types.to_vec(),
            capabilities: capabilities.iter().map(|s| s.to_string()).collect(),
            registered_at: Utc::now(),
        }
    }

    #[test]
    fn open_creates_file_and_sets_wal_mode() -> Result<(), DaemonError> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("agent.db");
        let db = Database::open(&path)?;

        let journal_mode: String = db
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        assert_eq!(journal_mode, "wal");

        let synchronous: i32 = db
            .conn
            .query_row("PRAGMA synchronous", [], |row| row.get(0))?;
        assert_eq!(synchronous, 1); // NORMAL

        Ok(())
    }

    #[test]
    fn migrations_create_tables() -> Result<(), DaemonError> {
        let db = in_memory_db()?;
        let table_count: i32 = db.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name IN ('cursors', 'handlers')",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(table_count, 2);
        Ok(())
    }

    #[test]
    fn migrations_are_idempotent() -> Result<(), DaemonError> {
        let db = in_memory_db()?;
        db.run_migrations()?;
        db.run_migrations()?;
        // If migrations were not idempotent, the second call would error.
        let table_count: i32 = db.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name IN ('cursors', 'handlers')",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(table_count, 2);
        Ok(())
    }

    #[test]
    fn save_and_load_cursor_round_trips() -> Result<(), DaemonError> {
        let db = in_memory_db()?;
        db.save_cursor("bot-1", "npub-1", 42)?;
        let result = db.load_cursor("bot-1")?;
        assert!(result.is_some());
        let (npub, cursor) = result.ok_or_else(|| DaemonError::Config("cursor missing".into()))?;
        assert_eq!(npub, "npub-1");
        assert_eq!(cursor, 42);
        Ok(())
    }

    #[test]
    fn load_cursor_for_unknown_bot_returns_none() -> Result<(), DaemonError> {
        let db = in_memory_db()?;
        let result = db.load_cursor("unknown")?;
        assert!(result.is_none());
        Ok(())
    }

    #[test]
    fn save_cursor_overwrites_existing_cursor() -> Result<(), DaemonError> {
        let db = in_memory_db()?;
        db.save_cursor("bot-1", "npub-1", 10)?;
        db.save_cursor("bot-1", "npub-1", 20)?;
        let result = db.load_cursor("bot-1")?;
        assert!(result.is_some());
        let (_, cursor) = result.ok_or_else(|| DaemonError::Config("cursor missing".into()))?;
        assert_eq!(cursor, 20);
        Ok(())
    }

    #[test]
    fn validate_npub_matches_stored_value() -> Result<(), DaemonError> {
        let db = in_memory_db()?;
        db.save_cursor("bot-1", "npub-1", 0)?;
        assert!(db.validate_npub("bot-1", "npub-1")?);
        assert!(!db.validate_npub("bot-1", "npub-2")?);
        Ok(())
    }

    #[test]
    fn validate_npub_for_unknown_bot_returns_true() -> Result<(), DaemonError> {
        let db = in_memory_db()?;
        assert!(db.validate_npub("bot-1", "npub-1")?);
        Ok(())
    }

    #[test]
    fn reset_cursor_removes_cursor() -> Result<(), DaemonError> {
        let db = in_memory_db()?;
        db.save_cursor("bot-1", "npub-1", 100)?;
        db.reset_cursor("bot-1")?;
        let result = db.load_cursor("bot-1")?;
        assert!(result.is_none());
        Ok(())
    }

    #[test]
    fn save_and_load_handlers_round_trips() -> Result<(), DaemonError> {
        let db = in_memory_db()?;
        let handler = handler_ref(
            "h-1",
            &["bot-1"],
            &[EventType::DmReceived],
            &["send_messages"],
        );
        db.save_handler(&handler)?;
        let loaded = db.load_handlers()?;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "h-1");
        assert_eq!(loaded[0].bot_ids, vec!["bot-1"]);
        assert_eq!(loaded[0].event_types, vec![EventType::DmReceived]);
        assert_eq!(loaded[0].capabilities, vec!["send_messages"]);
        Ok(())
    }

    #[test]
    fn delete_handler_removes_row() -> Result<(), DaemonError> {
        let db = in_memory_db()?;
        let handler = handler_ref(
            "h-1",
            &["bot-1"],
            &[EventType::DmReceived],
            &["send_messages"],
        );
        db.save_handler(&handler)?;
        db.delete_handler("h-1")?;
        let loaded = db.load_handlers()?;
        assert!(loaded.is_empty());
        Ok(())
    }

    #[test]
    fn multiple_handlers_for_same_bot_are_persisted() -> Result<(), DaemonError> {
        let db = in_memory_db()?;
        let h1 = handler_ref(
            "h-1",
            &["bot-1"],
            &[EventType::DmReceived],
            &["send_messages"],
        );
        let h2 = handler_ref(
            "h-2",
            &["bot-1", "bot-2"],
            &[EventType::DmReceived],
            &["send_messages"],
        );
        db.save_handler(&h1)?;
        db.save_handler(&h2)?;
        let mut loaded = db.load_handlers()?;
        loaded.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, "h-1");
        assert_eq!(loaded[1].id, "h-2");
        assert_eq!(loaded[1].bot_ids, vec!["bot-1", "bot-2"]);
        Ok(())
    }

    #[test]
    fn save_handler_overwrites_existing_handler() -> Result<(), DaemonError> {
        let db = in_memory_db()?;
        let h1 = handler_ref(
            "h-1",
            &["bot-1"],
            &[EventType::DmReceived],
            &["send_messages"],
        );
        let h2 = handler_ref(
            "h-1",
            &["bot-2"],
            &[EventType::DmReceived],
            &["set_profile"],
        );
        db.save_handler(&h1)?;
        db.save_handler(&h2)?;
        let loaded = db.load_handlers()?;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].bot_ids, vec!["bot-2"]);
        assert_eq!(loaded[0].capabilities, vec!["set_profile"]);
        Ok(())
    }
}
