//! The registry of working copies.
//!
//! A working copy is the file the kernel reads and writes. It is mutable, so
//! a crash can leave it half written. The registry maps the paths to their
//! working copies, which lets the recovery pass find them again after a
//! restart and reconcile them with the content store.

use anyhow::Result;
use rusqlite::params;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::db::{now_ts, Db};

#[derive(Debug, Clone)]
pub struct WorkingCopy {
    pub path: String,
    pub file: PathBuf,
    pub size: u64,
    pub updated_at: i64,
}

pub struct WorkingCopies {
    db: Arc<Db>,
}

impl WorkingCopies {
    pub fn new(db: Arc<Db>) -> Self {
        Self { db }
    }

    /// Remember a working copy.
    pub fn register(&self, path: &str, file: &Path) -> Result<()> {
        let conn = self.db.lock();
        conn.execute(
            "INSERT INTO working (path, file, size, updated_at) VALUES (?1, ?2, 0, ?3) \
             ON CONFLICT(path) DO UPDATE SET file = excluded.file, updated_at = excluded.updated_at",
            params![path, file.display().to_string(), now_ts()],
        )?;
        Ok(())
    }

    /// Record the size of a working copy.
    pub fn update_size(&self, path: &str, size: u64) -> Result<()> {
        let conn = self.db.lock();
        conn.execute(
            "UPDATE working SET size = ?1, updated_at = ?2 WHERE path = ?3",
            params![size as i64, now_ts(), path],
        )?;
        Ok(())
    }

    /// Forget a working copy.
    pub fn forget(&self, path: &str) -> Result<()> {
        let conn = self.db.lock();
        conn.execute("DELETE FROM working WHERE path = ?1", params![path])?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<WorkingCopy>> {
        let conn = self.db.lock();
        let mut stmt = conn.prepare("SELECT path, file, size, updated_at FROM working")?;
        let rows = stmt.query_map([], |row| {
            Ok(WorkingCopy {
                path: row.get(0)?,
                file: PathBuf::from(row.get::<_, String>(1)?),
                size: row.get::<_, i64>(2)?.max(0) as u64,
                updated_at: row.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Forget every working copy.
    pub fn clear(&self) -> Result<()> {
        let conn = self.db.lock();
        conn.execute("DELETE FROM working", [])?;
        Ok(())
    }
}
