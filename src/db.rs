use crate::errors::DaemonError;
use crate::handlers::HandlerRef;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension};
use secrecy::{ExposeSecret, SecretString};
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};

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
                ON event_trace (bot_id, created_at DESC);",
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
        let reconnect_token = handler.reconnect_token.expose_secret();
        self.conn.execute(
            "INSERT INTO handlers (handler_id, bot_ids, event_types, capabilities, reconnect_token, registered_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(handler_id) DO UPDATE SET
                bot_ids = excluded.bot_ids,
                event_types = excluded.event_types,
                capabilities = excluded.capabilities,
                reconnect_token = excluded.reconnect_token,
                registered_at = excluded.registered_at",
            (
                &handler.id,
                bot_ids,
                event_types,
                capabilities,
                reconnect_token,
                registered_at,
            ),
        )?;
        Ok(())
    }

    /// Load all persisted handler registrations.
    ///
    /// Loaded handlers have no live connection; any attempt to send events to
    /// them will fail until they reconnect and re-register.
    pub fn load_handlers(&self) -> Result<Vec<HandlerRef>, DaemonError> {
        let mut stmt = self.conn.prepare(
            "SELECT handler_id, bot_ids, event_types, capabilities, reconnect_token, registered_at FROM handlers",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?;
        let mut handlers = Vec::new();
        for row in rows {
            let (id, bot_ids, event_types, capabilities, reconnect_token, registered_at) = row?;
            let registered_at = DateTime::from_timestamp(registered_at, 0).unwrap_or_else(Utc::now);
            handlers.push(HandlerRef {
                id,
                connection: None,
                bot_ids: serde_json::from_str(&bot_ids)?,
                event_types: serde_json::from_str(&event_types)?,
                capabilities: serde_json::from_str(&capabilities)?,
                reconnect_token: SecretString::new(reconnect_token.into()),
                registered_at,
                last_seen: registered_at,
                transport: "unknown".to_string(),
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

    /// Persist an event trace row.
    pub fn save_event_trace(
        &self,
        bot_id: &str,
        event_id: &str,
        author: &str,
        content_preview: &str,
        action: &str,
        reply_event_id: Option<&str>,
    ) -> Result<(), DaemonError> {
        let created_at = Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO event_trace (bot_id, event_id, author, content_preview, action, reply_event_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            (bot_id, event_id, author, content_preview, action, reply_event_id, created_at),
        )?;
        Ok(())
    }

    /// Load event trace rows for a bot since a given UTC time.
    pub fn load_event_trace(
        &self,
        bot_id: &str,
        since: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<EventTraceRow>, DaemonError> {
        let since_ts = since.timestamp();
        let mut stmt = self.conn.prepare(
            "SELECT event_id, author, content_preview, action, reply_event_id, created_at
             FROM event_trace
             WHERE bot_id = ?1 AND created_at >= ?2
             ORDER BY created_at DESC, rowid DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map((bot_id, since_ts, limit as i64), |row| {
            Ok(EventTraceRow {
                event_id: row.get(0)?,
                author: row.get(1)?,
                content_preview: row.get(2)?,
                action: row.get(3)?,
                reply_event_id: row.get(4)?,
                created_at: row.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

/// A single row from the event trace table.
#[derive(Debug, Clone)]
pub struct EventTraceRow {
    pub event_id: String,
    pub author: String,
    pub content_preview: String,
    pub action: String,
    pub reply_event_id: Option<String>,
    pub created_at: i64,
}

/// Async wrapper around [`Database`] that runs blocking SQLite work on
/// Tokio's blocking thread pool so the async runtime stays responsive.
#[derive(Debug, Clone)]
pub struct Db {
    inner: Arc<StdMutex<Database>>,
}

impl Db {
    /// Open (or create) the SQLite database at `path` on a blocking worker.
    pub async fn open(path: &Path) -> Result<Self, DaemonError> {
        let path = path.to_path_buf();
        let db = tokio::task::spawn_blocking(move || Database::open(&path))
            .await
            .map_err(|e| DaemonError::Config(format!("database open task failed: {e}")))??;
        Ok(Self {
            inner: Arc::new(StdMutex::new(db)),
        })
    }

    /// Run a blocking database closure on a worker thread.
    async fn run<F, T>(&self, f: F) -> Result<T, DaemonError>
    where
        F: FnOnce(&Database) -> Result<T, DaemonError> + Send + 'static,
        T: Send + 'static,
    {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let db = inner
                .lock()
                .map_err(|_| DaemonError::Config("database lock poisoned".into()))?;
            f(&db)
        })
        .await
        .map_err(|e| DaemonError::Config(format!("database task failed: {e}")))?
    }

    /// Persist or update the cursor for `bot_id`.
    pub async fn save_cursor(
        &self,
        bot_id: &str,
        npub: &str,
        cursor: i64,
    ) -> Result<(), DaemonError> {
        let bot_id = bot_id.to_string();
        let npub = npub.to_string();
        self.run(move |db| db.save_cursor(&bot_id, &npub, cursor))
            .await
    }

    /// Load the stored npub and cursor for `bot_id`.
    pub async fn load_cursor(&self, bot_id: &str) -> Result<Option<(String, i64)>, DaemonError> {
        let bot_id = bot_id.to_string();
        self.run(move |db| db.load_cursor(&bot_id)).await
    }

    /// Return true if the stored npub for `bot_id` matches `npub`.
    pub async fn validate_npub(&self, bot_id: &str, npub: &str) -> Result<bool, DaemonError> {
        let bot_id = bot_id.to_string();
        let npub = npub.to_string();
        self.run(move |db| db.validate_npub(&bot_id, &npub)).await
    }

    /// Reset the cursor for `bot_id`, removing any persisted event position.
    pub async fn reset_cursor(&self, bot_id: &str) -> Result<(), DaemonError> {
        let bot_id = bot_id.to_string();
        self.run(move |db| db.reset_cursor(&bot_id)).await
    }

    /// Persist a handler registration, replacing any existing row.
    pub async fn save_handler(&self, handler: &HandlerRef) -> Result<(), DaemonError> {
        let handler = handler.clone();
        self.run(move |db| db.save_handler(&handler)).await
    }

    /// Load all persisted handler registrations.
    pub async fn load_handlers(&self) -> Result<Vec<HandlerRef>, DaemonError> {
        self.run(|db| db.load_handlers()).await
    }

    /// Delete a persisted handler registration.
    pub async fn delete_handler(&self, handler_id: &str) -> Result<(), DaemonError> {
        let handler_id = handler_id.to_string();
        self.run(move |db| db.delete_handler(&handler_id)).await
    }

    /// Persist an event trace row.
    pub async fn save_event_trace(
        &self,
        bot_id: &str,
        event_id: &str,
        author: &str,
        content_preview: &str,
        action: &str,
        reply_event_id: Option<&str>,
    ) -> Result<(), DaemonError> {
        let bot_id = bot_id.to_string();
        let event_id = event_id.to_string();
        let author = author.to_string();
        let content_preview = content_preview.to_string();
        let action = action.to_string();
        let reply_event_id = reply_event_id.map(|s| s.to_string());
        self.run(move |db| {
            db.save_event_trace(
                &bot_id,
                &event_id,
                &author,
                &content_preview,
                &action,
                reply_event_id.as_deref(),
            )
        })
        .await
    }

    /// Load event trace rows for a bot since a given UTC time.
    pub async fn load_event_trace(
        &self,
        bot_id: &str,
        since: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<EventTraceRow>, DaemonError> {
        let bot_id = bot_id.to_string();
        self.run(move |db| db.load_event_trace(&bot_id, since, limit))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventType;
    use crate::handlers::ConnectionHandle;
    use rusqlite::Connection;
    use tokio::sync::mpsc::channel;

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
        let now = Utc::now();
        HandlerRef {
            id: id.to_string(),
            connection: Some(disconnected_handle()),
            bot_ids: bot_ids.iter().map(|s| s.to_string()).collect(),
            event_types: event_types.to_vec(),
            capabilities: capabilities.iter().map(|s| s.to_string()).collect(),
            reconnect_token: SecretString::new("deadbeef".to_string().into()),
            registered_at: now,
            last_seen: now,
            transport: "unknown".to_string(),
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

    #[tokio::test]
    async fn db_wrapper_async_methods_round_trip() -> Result<(), DaemonError> {
        let dir = tempfile::tempdir()?;
        let db = Db::open(&dir.path().join("agent.db")).await?;

        db.save_cursor("bot-1", "npub-1", 42).await?;
        let (npub, cursor) = db
            .load_cursor("bot-1")
            .await?
            .ok_or_else(|| DaemonError::Config("cursor missing".into()))?;
        assert_eq!(npub, "npub-1");
        assert_eq!(cursor, 42);
        assert!(db.validate_npub("bot-1", "npub-1").await?);

        db.reset_cursor("bot-1").await?;
        assert!(db.load_cursor("bot-1").await?.is_none());

        let handler = handler_ref(
            "h-1",
            &["bot-1"],
            &[EventType::DmReceived],
            &["send_messages"],
        );
        db.save_handler(&handler).await?;
        let loaded = db.load_handlers().await?;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "h-1");

        db.delete_handler("h-1").await?;
        assert!(db.load_handlers().await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn db_wrapper_concurrent_operations_leave_runtime_responsive() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("agent.db")).await.unwrap();

        let mut interval = tokio::time::interval(Duration::from_millis(5));
        let ticks = Arc::new(AtomicUsize::new(0));
        let ticks_clone = Arc::clone(&ticks);
        let timer = tokio::spawn(async move {
            for _ in 0..50 {
                interval.tick().await;
                ticks_clone.fetch_add(1, Ordering::SeqCst);
            }
        });

        let mut ops = Vec::new();
        for i in 0..20u64 {
            let db = db.clone();
            let bot = format!("bot-{i}");
            let handler = handler_ref(
                &format!("h-{i}"),
                &[&bot],
                &[EventType::DmReceived],
                &["send_messages"],
            );
            ops.push(tokio::spawn(async move {
                db.save_cursor(&bot, "npub", i as i64).await.unwrap();
                db.load_cursor(&bot).await.unwrap();
                db.save_handler(&handler).await.unwrap();
                db.load_handlers().await.unwrap();
                db.delete_handler(&format!("h-{i}")).await.unwrap();
            }));
        }
        let _ = futures::future::join_all(ops).await;
        timer.await.unwrap();

        let tick_count = ticks.load(Ordering::SeqCst);
        assert!(
            tick_count >= 45,
            "runtime was blocked; only {tick_count} timer ticks fired"
        );
    }

    #[tokio::test]
    async fn event_trace_round_trip() -> Result<(), DaemonError> {
        let dir = tempfile::tempdir()?;
        let db = Db::open(&dir.path().join("agent.db")).await?;

        db.save_event_trace(
            "bot-1",
            "event-id-1",
            "author-1",
            "hello world",
            "reply",
            Some("reply-id-1"),
        )
        .await?;

        db.save_event_trace(
            "bot-1",
            "event-id-2",
            "author-2",
            "ack content",
            "ack",
            None,
        )
        .await?;

        let since = Utc::now() - chrono::Duration::minutes(1);
        let rows = db.load_event_trace("bot-1", since, 10).await?;
        assert_eq!(rows.len(), 2);

        let reply_row = rows.iter().find(|r| r.action == "reply").unwrap();
        assert_eq!(reply_row.event_id, "event-id-1");
        assert_eq!(reply_row.author, "author-1");
        assert_eq!(reply_row.content_preview, "hello world");
        assert_eq!(reply_row.reply_event_id.as_deref(), Some("reply-id-1"));

        let ack_row = rows.iter().find(|r| r.action == "ack").unwrap();
        assert_eq!(ack_row.reply_event_id, None);
        Ok(())
    }
}
