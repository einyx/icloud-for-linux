//! Extended attributes.
//!
//! The files in the local store are plain files, so the attributes live in
//! the index instead of in the file system. They are kept per path and are
//! part of the local state, so they survive a restart.

use anyhow::Result;
use rusqlite::params;
use std::sync::Arc;

use crate::db::Db;

pub struct Xattrs {
    db: Arc<Db>,
}

impl Xattrs {
    pub fn new(db: Arc<Db>) -> Self {
        Self { db }
    }

    /// Set an attribute.
    pub fn set(&self, path: &str, name: &str, value: &[u8]) -> Result<()> {
        let conn = self.db.lock();
        conn.execute(
            "INSERT INTO xattrs (path, name, value) VALUES (?1, ?2, ?3) \
             ON CONFLICT(path, name) DO UPDATE SET value = excluded.value",
            params![path, name, value],
        )?;
        Ok(())
    }

    /// Read an attribute.
    pub fn get(&self, path: &str, name: &str) -> Result<Option<Vec<u8>>> {
        let conn = self.db.lock();
        let mut stmt = conn.prepare("SELECT value FROM xattrs WHERE path = ?1 AND name = ?2")?;
        let value = stmt
            .query_row(params![path, name], |row| row.get::<_, Vec<u8>>(0))
            .ok();
        Ok(value)
    }

    /// The names of the attributes of a path.
    pub fn list(&self, path: &str) -> Result<Vec<String>> {
        let conn = self.db.lock();
        let mut stmt = conn.prepare("SELECT name FROM xattrs WHERE path = ?1 ORDER BY name")?;
        let rows = stmt.query_map(params![path], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Remove an attribute.
    pub fn remove(&self, path: &str, name: &str) -> Result<bool> {
        let conn = self.db.lock();
        let removed = conn.execute(
            "DELETE FROM xattrs WHERE path = ?1 AND name = ?2",
            params![path, name],
        )?;
        Ok(removed > 0)
    }

    /// Forget every attribute of a path.
    pub fn forget(&self, path: &str) -> Result<()> {
        let conn = self.db.lock();
        conn.execute("DELETE FROM xattrs WHERE path = ?1", params![path])?;
        Ok(())
    }
}
