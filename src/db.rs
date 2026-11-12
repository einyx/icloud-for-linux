//! Shared SQLite handle.
//!
//! One database file holds the three tables of the new design:
//!
//! * `nodes`     — the path index (metadata cache)
//! * `ops`       — the write-ahead journal of local mutations
//! * `conflicts` — versions that lost a race, kept as blob references
//!
//! The connection runs in WAL mode so the FUSE thread and the background sync
//! workers never block each other on a write.

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::Connection;
use std::path::Path;

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create db dir {}", parent.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("open db {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.init_schema()?;
        Ok(db)
    }

    fn init_schema(&self) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute_batch(
            r#"
            -- Path index: metadata cache, populated lazily.
            CREATE TABLE IF NOT EXISTS nodes (
                path       TEXT PRIMARY KEY,
                parent     TEXT NOT NULL,
                kind       TEXT NOT NULL,
                size       INTEGER NOT NULL DEFAULT 0,
                mtime      INTEGER NOT NULL DEFAULT 0,
                etag       TEXT,
                remote_id  TEXT,
                digest     TEXT,
                hydrated   INTEGER NOT NULL DEFAULT 0,
                dirty      INTEGER NOT NULL DEFAULT 0,
                present    INTEGER NOT NULL DEFAULT 1,
                fetched_at INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_nodes_parent ON nodes(parent);
            CREATE INDEX IF NOT EXISTS idx_nodes_digest ON nodes(digest);
            CREATE INDEX IF NOT EXISTS idx_nodes_dirty ON nodes(dirty);

            -- Write-ahead journal of local mutations waiting to be applied.
            CREATE TABLE IF NOT EXISTS ops (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                op          TEXT NOT NULL,
                path        TEXT NOT NULL,
                target_path TEXT,
                digest      TEXT,
                size        INTEGER,
                mtime       INTEGER,
                state       TEXT NOT NULL DEFAULT 'pending',
                attempts    INTEGER NOT NULL DEFAULT 0,
                last_error  TEXT,
                created_at  INTEGER NOT NULL,
                key         TEXT NOT NULL UNIQUE
            );
            CREATE INDEX IF NOT EXISTS idx_ops_state ON ops(state, created_at);

            -- Versions that lost a conflict, preserved as blob references.
            CREATE TABLE IF NOT EXISTS conflicts (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                path         TEXT NOT NULL,
                local_digest TEXT,
                remote_etag  TEXT,
                created_at   INTEGER NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    pub fn lock(&self) -> parking_lot::MutexGuard<'_, Connection> {
        self.conn.lock()
    }
}

/// Seconds since the unix epoch.
pub fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
