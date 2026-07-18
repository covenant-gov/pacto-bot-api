use crate::errors::DaemonError;
use crate::handlers::HandlerRef;
use chrono::{DateTime, Utc};
use refinery::embed_migrations;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};
use secrecy::{ExposeSecret, SecretString};
use std::fs::OpenOptions;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};

/// Set file permissions to owner-read/write only (`0o600`).
///
/// No-op on non-Unix platforms.
#[cfg(unix)]
pub(crate) fn set_owner_only_permissions(path: &Path) -> Result<(), DaemonError> {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn set_owner_only_permissions(_path: &Path) -> Result<(), DaemonError> {
    Ok(())
}

/// SQLite persistence handle for cursors and handler registrations.
#[derive(Debug)]
pub struct Database {
    conn: Connection,
}

impl Database {
    /// Open (or create) the SQLite database at `path`, enable WAL mode, set
    /// synchronous=NORMAL, and run idempotent migrations.
    pub fn open(path: &Path) -> Result<Self, DaemonError> {
        // Create the database file atomically with owner-only permissions if it
        // does not already exist. SQLite will then open the existing file and
        // initialize its schema; we then enforce the permission bits regardless
        // of the process umask or any existing file.
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        #[cfg(unix)]
        {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
            {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
        }
        let conn = Connection::open(path)?;
        set_owner_only_permissions(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;",
        )?;
        let mut db = Self { conn };
        db.run_migrations()?;
        Ok(db)
    }

    /// Run database migrations to set up or upgrade the schema.
    pub fn run_migrations(&mut self) -> Result<(), DaemonError> {
        embed_migrations!("migrations");
        let report = migrations::runner()
            .set_migration_table_name("_refinery_schema_history_pacto_bot_api")
            .run(&mut self.conn)?;
        for migration in report.applied_migrations() {
            tracing::info!(
                "Applied migration: {} (version: {})",
                migration.name(),
                migration.version()
            );
        }
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

    /// Load a single MLS group row by composite key.
    ///
    /// Returns `None` when no matching group exists.
    pub fn load_mls_group(
        &self,
        bot_id: &str,
        group_name: &str,
    ) -> Result<Option<MlsGroupRow>, DaemonError> {
        let row: Option<(String, String, String)> = self
            .conn
            .query_row(
                "SELECT wire_id, creator_npub, relay
                 FROM mls_groups
                 WHERE bot_id = ?1 AND group_name = ?2",
                (bot_id, group_name),
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        match row {
            Some((wire_id, creator_npub, relay)) => {
                let mut stmt = self.conn.prepare(
                    "SELECT member_npub FROM mls_group_members
                     WHERE bot_id = ?1 AND group_name = ?2
                     ORDER BY member_npub",
                )?;
                let invited_bots = stmt
                    .query_map((bot_id, group_name), |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Some(MlsGroupRow {
                    bot_id: bot_id.to_string(),
                    group_name: group_name.to_string(),
                    wire_id,
                    creator_npub,
                    relay,
                    invited_bots,
                }))
            }
            None => Ok(None),
        }
    }

    /// Load all MLS groups for `bot_id` together with their members.
    pub fn load_all_mls_groups(&self, bot_id: &str) -> Result<Vec<MlsGroupRow>, DaemonError> {
        let mut stmt = self.conn.prepare(
            "SELECT g.group_name, g.wire_id, g.creator_npub, g.relay,
                    GROUP_CONCAT(m.member_npub ORDER BY m.member_npub) AS members
             FROM mls_groups g
             LEFT JOIN mls_group_members m
                 ON g.bot_id = m.bot_id AND g.group_name = m.group_name
             WHERE g.bot_id = ?1
             GROUP BY g.bot_id, g.group_name, g.wire_id, g.creator_npub, g.relay
             ORDER BY g.group_name",
        )?;
        let rows = stmt.query_map([bot_id], |row| {
            let group_name: String = row.get(0)?;
            let wire_id: String = row.get(1)?;
            let creator_npub: String = row.get(2)?;
            let relay: String = row.get(3)?;
            let members: Option<String> = row.get(4)?;
            let invited_bots = members
                .map(|s| s.split(',').map(|x| x.to_string()).collect())
                .unwrap_or_default();
            Ok(MlsGroupRow {
                bot_id: bot_id.to_string(),
                group_name,
                wire_id,
                creator_npub,
                relay,
                invited_bots,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(DaemonError::Sqlite)
    }

    /// Insert a new MLS group row.
    ///
    /// Fails on duplicate `(bot_id, group_name)` or duplicate `(bot_id, wire_id)`.
    pub fn insert_mls_group(&self, row: &MlsGroupRow) -> Result<(), DaemonError> {
        let invited_bots = serde_json::to_string(&row.invited_bots)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO mls_groups (bot_id, group_name, wire_id, creator_npub, relay, invited_bots)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            (
                &row.bot_id,
                &row.group_name,
                &row.wire_id,
                &row.creator_npub,
                &row.relay,
                invited_bots,
            ),
        )?;
        for member in &row.invited_bots {
            tx.execute(
                "INSERT OR IGNORE INTO mls_group_members (bot_id, group_name, member_npub)
                 VALUES (?1, ?2, ?3)",
                (&row.bot_id, &row.group_name, member),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Import an MLS group row and its members idempotently for export/import.
    ///
    /// The group row is inserted strictly (fails on duplicate key). Member rows
    /// are replaced so repeated imports converge to the same membership.
    pub fn insert_mls_group_export(&self, row: &MlsGroupRow) -> Result<(), DaemonError> {
        let invited_bots = serde_json::to_string(&row.invited_bots)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO mls_groups (bot_id, group_name, wire_id, creator_npub, relay, invited_bots)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            (
                &row.bot_id,
                &row.group_name,
                &row.wire_id,
                &row.creator_npub,
                &row.relay,
                invited_bots,
            ),
        )?;
        for member in &row.invited_bots {
            tx.execute(
                "INSERT OR REPLACE INTO mls_group_members (bot_id, group_name, member_npub)
                 VALUES (?1, ?2, ?3)",
                (&row.bot_id, &row.group_name, member),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Insert an MLS group row from engine reconciliation if it is missing.
    ///
    /// This is idempotent: if `(bot_id, group_name)` already exists with the
    /// same wire id, no changes are made. If the existing row has a different
    /// wire id, or if the wire id is already assigned to a different group, a
    /// collision error is returned.
    pub fn upsert_mls_group_from_reconciliation(
        &self,
        bot_id: &str,
        bot_npub: &str,
        wire_id: &str,
        group_name: &str,
        members: &[String],
    ) -> Result<(), DaemonError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;

        let existing_wire_id: Option<String> = tx
            .query_row(
                "SELECT wire_id FROM mls_groups
                 WHERE bot_id = ?1 AND group_name = ?2",
                (bot_id, group_name),
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        if let Some(existing_wire_id) = existing_wire_id {
            if existing_wire_id != wire_id {
                return Err(DaemonError::Config(format!(
                    "MLS group '{group_name}' has wire id {existing_wire_id}, but engine reports {wire_id}"
                )));
            }
            tx.commit()?;
            return Ok(());
        }

        let wire_collision: Option<String> = tx
            .query_row(
                "SELECT group_name FROM mls_groups
                 WHERE bot_id = ?1 AND wire_id = ?2",
                (bot_id, wire_id),
                |row| row.get(0),
            )
            .optional()?;

        if let Some(colliding_name) = wire_collision {
            return Err(DaemonError::Config(format!(
                "MLS wire id {wire_id} already belongs to group '{colliding_name}'"
            )));
        }

        let invited_bots = serde_json::to_string(members)?;
        tx.execute(
            "INSERT INTO mls_groups (bot_id, group_name, wire_id, creator_npub, relay, invited_bots)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            (bot_id, group_name, wire_id, bot_npub, "", invited_bots),
        )?;

        for member in members {
            tx.execute(
                "INSERT OR IGNORE INTO mls_group_members (bot_id, group_name, member_npub)
                 VALUES (?1, ?2, ?3)",
                (bot_id, group_name, member),
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    /// Append a recipient to the `invited_bots` JSON array only if not already present.
    ///
    /// Returns `Ok(true)` when the row existed and the recipient was added or
    /// already present, and `Ok(false)` when the row did not exist.
    pub fn update_mls_group_invite(
        &self,
        bot_id: &str,
        group_name: &str,
        recipient_npub: &str,
    ) -> Result<bool, DaemonError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;

        let group_exists: bool = tx
            .query_row(
                "SELECT 1 FROM mls_groups
                 WHERE bot_id = ?1 AND group_name = ?2",
                (bot_id, group_name),
                |_row| Ok(()),
            )
            .optional()?
            .is_some();

        if !group_exists {
            tx.rollback()?;
            return Ok(false);
        }

        tx.execute(
            "INSERT OR IGNORE INTO mls_group_members (bot_id, group_name, member_npub)
             VALUES (?1, ?2, ?3)",
            (bot_id, group_name, recipient_npub),
        )?;

        tx.commit()?;
        Ok(true)
    }

    /// Return true when `recipient_npub` is present in the stored `invited_bots` array.
    pub fn is_bot_invited(
        &self,
        bot_id: &str,
        group_name: &str,
        recipient_npub: &str,
    ) -> Result<bool, DaemonError> {
        let exists: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM mls_group_members
                 WHERE bot_id = ?1 AND group_name = ?2 AND member_npub = ?3",
                (bot_id, group_name, recipient_npub),
                |_row| Ok(()),
            )
            .optional()?
            .is_some();
        Ok(exists)
    }

    /// Delete a single member row.
    ///
    /// Returns `Ok(true)` when a row was deleted, `Ok(false)` otherwise.
    pub fn delete_mls_group_member(
        &self,
        bot_id: &str,
        group_name: &str,
        recipient_npub: &str,
    ) -> Result<bool, DaemonError> {
        let changed = self.conn.execute(
            "DELETE FROM mls_group_members
             WHERE bot_id = ?1 AND group_name = ?2 AND member_npub = ?3",
            (bot_id, group_name, recipient_npub),
        )?;
        Ok(changed > 0)
    }
}

/// A single row from the MLS groups table.
#[derive(Debug, Clone, PartialEq)]
pub struct MlsGroupRow {
    pub bot_id: String,
    pub group_name: String,
    pub wire_id: String,
    pub creator_npub: String,
    pub relay: String,
    pub invited_bots: Vec<String>,
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
///
/// # Lock ordering
///
/// The `ClientManager` lock is always acquired before the database lock. When
/// a caller needs both, it must take the `ClientManager` lock first and then use
/// [`Db`]; database methods must never be invoked while holding the database
/// lock and then waiting on the `ClientManager` lock.
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

    /// Load a single MLS group row by composite key.
    pub async fn load_mls_group(
        &self,
        bot_id: &str,
        group_name: &str,
    ) -> Result<Option<MlsGroupRow>, DaemonError> {
        let bot_id = bot_id.to_string();
        let group_name = group_name.to_string();
        self.run(move |db| db.load_mls_group(&bot_id, &group_name))
            .await
    }

    /// Insert a new MLS group row.
    pub async fn insert_mls_group(&self, row: MlsGroupRow) -> Result<(), DaemonError> {
        self.run(move |db| db.insert_mls_group(&row)).await
    }

    /// Append a recipient to the `invited_bots` JSON array only if not already present.
    pub async fn update_mls_group_invite(
        &self,
        bot_id: &str,
        group_name: &str,
        recipient_npub: &str,
    ) -> Result<bool, DaemonError> {
        let bot_id = bot_id.to_string();
        let group_name = group_name.to_string();
        let recipient_npub = recipient_npub.to_string();
        self.run(move |db| db.update_mls_group_invite(&bot_id, &group_name, &recipient_npub))
            .await
    }

    /// Return true when `recipient_npub` is present in the stored `invited_bots` array.
    pub async fn is_bot_invited(
        &self,
        bot_id: &str,
        group_name: &str,
        recipient_npub: &str,
    ) -> Result<bool, DaemonError> {
        let bot_id = bot_id.to_string();
        let group_name = group_name.to_string();
        let recipient_npub = recipient_npub.to_string();
        self.run(move |db| db.is_bot_invited(&bot_id, &group_name, &recipient_npub))
            .await
    }

    /// Delete a single member row.
    pub async fn delete_mls_group_member(
        &self,
        bot_id: &str,
        group_name: &str,
        recipient_npub: &str,
    ) -> Result<bool, DaemonError> {
        let bot_id = bot_id.to_string();
        let group_name = group_name.to_string();
        let recipient_npub = recipient_npub.to_string();
        self.run(move |db| db.delete_mls_group_member(&bot_id, &group_name, &recipient_npub))
            .await
    }

    /// Load all MLS group rows for a bot together with their members.
    pub async fn load_all_mls_groups(&self, bot_id: &str) -> Result<Vec<MlsGroupRow>, DaemonError> {
        let bot_id = bot_id.to_string();
        self.run(move |db| db.load_all_mls_groups(&bot_id)).await
    }

    /// Import an MLS group row and its members idempotently for export/import.
    pub async fn insert_mls_group_export(&self, row: MlsGroupRow) -> Result<(), DaemonError> {
        self.run(move |db| db.insert_mls_group_export(&row)).await
    }

    /// Insert an MLS group row from engine reconciliation if it is missing.
    pub async fn upsert_mls_group_from_reconciliation(
        &self,
        bot_id: &str,
        bot_npub: &str,
        wire_id: &str,
        group_name: &str,
        members: Vec<String>,
    ) -> Result<(), DaemonError> {
        let bot_id = bot_id.to_string();
        let bot_npub = bot_npub.to_string();
        let wire_id = wire_id.to_string();
        let group_name = group_name.to_string();
        self.run(move |db| {
            db.upsert_mls_group_from_reconciliation(
                &bot_id,
                &bot_npub,
                &wire_id,
                &group_name,
                &members,
            )
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventType;
    use crate::handlers::ConnectionHandle;
    use rusqlite::Connection;
    use std::collections::HashSet;
    use tokio::sync::mpsc::channel;

    fn in_memory_db() -> Result<Database, DaemonError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;",
        )?;
        let mut db = Database { conn };
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

    #[derive(Debug)]
    struct ColumnInfo {
        name: String,
        type_: String,
        not_null: bool,
        pk: i32,
    }

    fn table_info(conn: &Connection, table: &str) -> Result<Vec<ColumnInfo>, DaemonError> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let rows = stmt.query_map([], |row| {
            Ok(ColumnInfo {
                name: row.get("name")?,
                type_: row.get("type")?,
                not_null: row.get("notnull")?,
                pk: row.get("pk")?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(DaemonError::Sqlite)
    }

    fn unique_index_columns(
        conn: &Connection,
        table: &str,
    ) -> Result<Vec<Vec<String>>, DaemonError> {
        let mut stmt = conn.prepare(&format!("PRAGMA index_list({table})"))?;
        let indexes = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>("name")?, row.get::<_, i32>("unique")?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut result = Vec::new();
        for (name, unique) in indexes {
            if unique != 1 {
                continue;
            }
            let mut col_stmt = conn.prepare(&format!("PRAGMA index_info({name})"))?;
            let cols = col_stmt
                .query_map([], |row| row.get::<_, String>("name"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(DaemonError::Sqlite)?;
            result.push(cols);
        }
        Ok(result)
    }

    fn assert_mls_groups_schema(conn: &Connection) -> Result<(), DaemonError> {
        let columns = table_info(conn, "mls_groups")?;
        let names: HashSet<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        let expected: HashSet<&str> = [
            "bot_id",
            "group_name",
            "wire_id",
            "creator_npub",
            "relay",
            "invited_bots",
        ]
        .iter()
        .copied()
        .collect();
        assert_eq!(names, expected, "mls_groups column set mismatch");

        let pk: HashSet<&str> = columns
            .iter()
            .filter(|c| c.pk > 0)
            .map(|c| c.name.as_str())
            .collect();
        let expected_pk: HashSet<&str> = ["bot_id", "group_name"].iter().copied().collect();
        assert_eq!(pk, expected_pk, "mls_groups primary key mismatch");

        for col in &columns {
            assert!(col.not_null, "mls_groups.{} should be NOT NULL", col.name);
            assert_eq!(col.type_, "TEXT", "mls_groups.{} should be TEXT", col.name);
        }

        let unique_indexes = unique_index_columns(conn, "mls_groups")?;
        let pk_index: Vec<String> = ["bot_id", "group_name"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let bot_id_wire_id_index: Vec<String> = ["bot_id", "wire_id"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert!(
            unique_indexes.contains(&pk_index),
            "mls_groups primary key index missing"
        );
        assert!(
            unique_indexes.contains(&bot_id_wire_id_index),
            "mls_groups (bot_id, wire_id) unique index missing"
        );

        Ok(())
    }

    fn assert_mls_group_members_schema(conn: &Connection) -> Result<(), DaemonError> {
        let columns = table_info(conn, "mls_group_members")?;
        let names: HashSet<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        let expected: HashSet<&str> = ["bot_id", "group_name", "member_npub"]
            .iter()
            .copied()
            .collect();
        assert_eq!(names, expected, "mls_group_members column set mismatch");

        let pk: HashSet<&str> = columns
            .iter()
            .filter(|c| c.pk > 0)
            .map(|c| c.name.as_str())
            .collect();
        let expected_pk: HashSet<&str> = ["bot_id", "group_name", "member_npub"]
            .iter()
            .copied()
            .collect();
        assert_eq!(pk, expected_pk, "mls_group_members primary key mismatch");

        for col in &columns {
            assert!(
                col.not_null,
                "mls_group_members.{} should be NOT NULL",
                col.name
            );
            assert_eq!(
                col.type_, "TEXT",
                "mls_group_members.{} should be TEXT",
                col.name
            );
        }

        let unique_indexes = unique_index_columns(conn, "mls_group_members")?;
        let pk_index: Vec<String> = ["bot_id", "group_name", "member_npub"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert!(
            unique_indexes.contains(&pk_index),
            "mls_group_members primary key index missing"
        );

        Ok(())
    }

    #[test]
    fn open_creates_file_and_sets_wal_mode() -> Result<(), DaemonError> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("agent.db");
        let db = Database::open(&path)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)?.permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "agent.db should be owner-only");
        }

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
             WHERE type = 'table' AND name IN ('cursors', 'handlers', 'mls_groups', 'mls_group_members')",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(table_count, 4);
        Ok(())
    }

    #[test]
    fn migrations_are_idempotent() -> Result<(), DaemonError> {
        let mut db = in_memory_db()?;
        db.run_migrations()?;
        db.run_migrations()?;
        // If migrations were not idempotent, the second call would error.
        let table_count: i32 = db.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name IN ('cursors', 'handlers', 'mls_groups', 'mls_group_members')",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(table_count, 4);
        Ok(())
    }

    #[test]
    fn migrations_create_mls_tables_with_expected_schema() -> Result<(), DaemonError> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("agent.db");
        let db = Database::open(&path)?;

        assert_mls_groups_schema(&db.conn)?;
        assert_mls_group_members_schema(&db.conn)?;

        let fk_groups: Vec<String> = db
            .conn
            .prepare("PRAGMA foreign_key_list(mls_groups)")?
            .query_map([], |row| row.get::<_, String>("from"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(DaemonError::Sqlite)?;
        assert!(
            fk_groups.is_empty(),
            "mls_groups should declare no foreign keys"
        );

        let fk_members: Vec<String> = db
            .conn
            .prepare("PRAGMA foreign_key_list(mls_group_members)")?
            .query_map([], |row| row.get::<_, String>("from"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(DaemonError::Sqlite)?;
        assert!(
            fk_members.is_empty(),
            "mls_group_members should declare no foreign keys"
        );

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

    #[test]
    fn mls_group_insert_and_load_round_trip() -> Result<(), DaemonError> {
        let db = in_memory_db()?;
        let row = MlsGroupRow {
            bot_id: "bot-1".to_string(),
            group_name: "my-group".to_string(),
            wire_id: "wire-id-1".to_string(),
            creator_npub: "npub-creator".to_string(),
            relay: "wss://relay.example".to_string(),
            invited_bots: vec!["npub-a".to_string(), "npub-b".to_string()],
        };

        db.insert_mls_group(&row)?;
        let loaded = db
            .load_mls_group("bot-1", "my-group")?
            .expect("group should exist");
        assert_eq!(loaded, row);
        Ok(())
    }

    #[test]
    fn mls_group_insert_fails_on_duplicate_composite_key() {
        let db = in_memory_db().unwrap();
        let row = MlsGroupRow {
            bot_id: "bot-1".to_string(),
            group_name: "my-group".to_string(),
            wire_id: "wire-id-1".to_string(),
            creator_npub: "npub-creator".to_string(),
            relay: "wss://relay.example".to_string(),
            invited_bots: vec![],
        };
        db.insert_mls_group(&row).unwrap();

        let mut dup = row.clone();
        dup.wire_id = "wire-id-2".to_string();
        assert!(
            db.insert_mls_group(&dup).is_err(),
            "duplicate PK should fail"
        );
    }

    #[test]
    fn mls_group_insert_allows_same_wire_id_across_bots() {
        let db = in_memory_db().unwrap();
        let row = MlsGroupRow {
            bot_id: "bot-1".to_string(),
            group_name: "group-a".to_string(),
            wire_id: "wire-id-1".to_string(),
            creator_npub: "npub-creator".to_string(),
            relay: "wss://relay.example".to_string(),
            invited_bots: vec![],
        };
        db.insert_mls_group(&row).unwrap();

        // The same Squad wire id in a different bot's engine must succeed.
        let other_bot = MlsGroupRow {
            bot_id: "bot-2".to_string(),
            group_name: "group-a".to_string(),
            wire_id: "wire-id-1".to_string(),
            creator_npub: "npub-creator".to_string(),
            relay: "wss://relay.example".to_string(),
            invited_bots: vec![],
        };
        db.insert_mls_group(&other_bot).expect(
            "same wire_id for a different bot should be allowed");

        // But the same bot cannot reuse the wire_id under a different group name.
        let mut dup = row.clone();
        dup.group_name = "group-b".to_string();
        assert!(
            db.insert_mls_group(&dup).is_err(),
            "duplicate (bot_id, wire_id) should fail"
        );
    }

    #[test]
    fn mls_group_update_invite_appends_and_is_idempotent() -> Result<(), DaemonError> {
        let db = in_memory_db()?;
        let row = MlsGroupRow {
            bot_id: "bot-1".to_string(),
            group_name: "my-group".to_string(),
            wire_id: "wire-id-1".to_string(),
            creator_npub: "npub-creator".to_string(),
            relay: "wss://relay.example".to_string(),
            invited_bots: vec!["npub-a".to_string()],
        };
        db.insert_mls_group(&row)?;

        assert!(db.update_mls_group_invite("bot-1", "my-group", "npub-b")?);
        assert!(db.update_mls_group_invite("bot-1", "my-group", "npub-b")?);

        let loaded = db.load_mls_group("bot-1", "my-group")?.unwrap();
        assert_eq!(loaded.invited_bots, vec!["npub-a", "npub-b"]);

        assert!(db.is_bot_invited("bot-1", "my-group", "npub-b")?);
        assert!(!db.is_bot_invited("bot-1", "my-group", "npub-c")?);
        Ok(())
    }

    #[test]
    fn mls_group_update_invite_missing_group_returns_false_and_is_noop() -> Result<(), DaemonError>
    {
        let db = in_memory_db()?;
        let result = db.update_mls_group_invite("bot-1", "no-such-group", "npub-x")?;
        assert!(!result);
        assert!(db.load_mls_group("bot-1", "no-such-group")?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn db_mls_group_async_methods_round_trip() -> Result<(), DaemonError> {
        let dir = tempfile::tempdir()?;
        let db = Db::open(&dir.path().join("agent.db")).await?;

        let row = MlsGroupRow {
            bot_id: "bot-1".to_string(),
            group_name: "my-group".to_string(),
            wire_id: "wire-id-1".to_string(),
            creator_npub: "npub-creator".to_string(),
            relay: "wss://relay.example".to_string(),
            invited_bots: vec![],
        };
        db.insert_mls_group(row.clone()).await?;

        let loaded = db.load_mls_group("bot-1", "my-group").await?.unwrap();
        assert_eq!(loaded.wire_id, "wire-id-1");

        assert!(
            db.update_mls_group_invite("bot-1", "my-group", "npub-a")
                .await?
        );
        assert!(db.is_bot_invited("bot-1", "my-group", "npub-a").await?);
        Ok(())
    }

    #[test]
    fn load_all_mls_groups_returns_members() -> Result<(), DaemonError> {
        let db = in_memory_db()?;
        let row1 = MlsGroupRow {
            bot_id: "bot-1".to_string(),
            group_name: "alpha".to_string(),
            wire_id: "wire-id-1".to_string(),
            creator_npub: "npub-creator".to_string(),
            relay: "wss://relay.example".to_string(),
            invited_bots: vec!["npub-a".to_string(), "npub-b".to_string()],
        };
        let row2 = MlsGroupRow {
            bot_id: "bot-1".to_string(),
            group_name: "beta".to_string(),
            wire_id: "wire-id-2".to_string(),
            creator_npub: "npub-creator".to_string(),
            relay: "wss://relay.example".to_string(),
            invited_bots: vec!["npub-c".to_string()],
        };
        db.insert_mls_group(&row1)?;
        db.insert_mls_group(&row2)?;

        let mut groups = db.load_all_mls_groups("bot-1")?;
        groups.sort_by(|a, b| a.group_name.cmp(&b.group_name));
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0], row1);
        assert_eq!(groups[1], row2);
        Ok(())
    }

    #[test]
    fn insert_mls_group_export_is_idempotent_for_members() -> Result<(), DaemonError> {
        let db = in_memory_db()?;
        let row = MlsGroupRow {
            bot_id: "bot-1".to_string(),
            group_name: "my-group".to_string(),
            wire_id: "wire-id-1".to_string(),
            creator_npub: "npub-creator".to_string(),
            relay: "wss://relay.example".to_string(),
            invited_bots: vec!["npub-a".to_string()],
        };
        db.insert_mls_group_export(&row)?;
        let mut reimport = row.clone();
        reimport.invited_bots = vec!["npub-a".to_string(), "npub-b".to_string()];
        // Re-importing the same group should fail because the group row is strict.
        assert!(db.insert_mls_group_export(&reimport).is_err());

        // Verify the original members are intact.
        let loaded = db.load_mls_group("bot-1", "my-group")?.unwrap();
        assert_eq!(loaded.invited_bots, vec!["npub-a"]);
        Ok(())
    }

    #[test]
    fn is_bot_invited_uses_member_table() -> Result<(), DaemonError> {
        let db = in_memory_db()?;
        let row = MlsGroupRow {
            bot_id: "bot-1".to_string(),
            group_name: "my-group".to_string(),
            wire_id: "wire-id-1".to_string(),
            creator_npub: "npub-creator".to_string(),
            relay: "wss://relay.example".to_string(),
            invited_bots: vec!["npub-a".to_string()],
        };
        db.insert_mls_group(&row)?;

        assert!(db.is_bot_invited("bot-1", "my-group", "npub-a")?);
        assert!(!db.is_bot_invited("bot-1", "my-group", "npub-b")?);
        Ok(())
    }
}
