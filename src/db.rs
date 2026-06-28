use crate::errors::DaemonError;
use rusqlite::Connection;

/// Placeholder SQLite persistence handle.
#[derive(Debug)]
pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open(path: &std::path::Path) -> Result<Self, DaemonError> {
        let conn = Connection::open(path)?;
        Ok(Self { conn })
    }

    pub fn initialize(&self) -> Result<(), DaemonError> {
        self.conn
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        Ok(())
    }
}
