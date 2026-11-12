use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub path: String,
    pub entry_type: String, // "file" | "folder"
    pub parent_path: String,
    pub remote_drivewsid: Option<String>,
    pub remote_docwsid: Option<String>,
    pub remote_etag: Option<String>,
    pub remote_zone: Option<String>,
    pub remote_shareid: Option<String>,
    pub size: i64,
    pub mtime: i64,
    pub hydrated: bool,
    pub dirty: bool,
    pub tombstone: bool,
    pub local_sha256: Option<String>,
    pub last_synced_at: Option<i64>,
    pub synced_path: Option<String>,
}

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub struct SyncState {
    conn: Mutex<Connection>,
}

impl SyncState {
    pub fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create db dir {}", parent.display()))?;
        }
        let conn =
            Connection::open(db_path).with_context(|| format!("open db {}", db_path.display()))?;
        let state = Self {
            conn: Mutex::new(conn),
        };
        state.init_db()?;
        Ok(state)
    }

    fn init_db(&self) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS entries (
                path TEXT PRIMARY KEY,
                type TEXT NOT NULL,
                parent_path TEXT NOT NULL,
                remote_drivewsid TEXT,
                remote_docwsid TEXT,
                remote_etag TEXT,
                remote_zone TEXT,
                remote_shareid TEXT,
                size INTEGER NOT NULL DEFAULT 0,
                mtime INTEGER NOT NULL DEFAULT 0,
                hydrated INTEGER NOT NULL DEFAULT 0,
                dirty INTEGER NOT NULL DEFAULT 0,
                tombstone INTEGER NOT NULL DEFAULT 0,
                local_sha256 TEXT,
                last_synced_at INTEGER,
                synced_path TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_entries_remote_drivewsid ON entries(remote_drivewsid);
            CREATE INDEX IF NOT EXISTS idx_entries_dirty ON entries(dirty, tombstone);
            CREATE TABLE IF NOT EXISTS pending_ops (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                op TEXT NOT NULL,
                path TEXT NOT NULL,
                target_path TEXT,
                queued_at INTEGER NOT NULL,
                retry_count INTEGER NOT NULL DEFAULT 0,
                last_error TEXT
            );
            "#,
        )?;
        Ok(())
    }

    fn row_to_entry(row: &Row<'_>) -> rusqlite::Result<Entry> {
        Ok(Entry {
            path: row.get(0)?,
            entry_type: row.get(1)?,
            parent_path: row.get(2)?,
            remote_drivewsid: row.get(3)?,
            remote_docwsid: row.get(4)?,
            remote_etag: row.get(5)?,
            remote_zone: row.get(6)?,
            remote_shareid: row.get(7)?,
            size: row.get(8)?,
            mtime: row.get(9)?,
            hydrated: row.get::<_, i64>(10)? != 0,
            dirty: row.get::<_, i64>(11)? != 0,
            tombstone: row.get::<_, i64>(12)? != 0,
            local_sha256: row.get(13)?,
            last_synced_at: row.get(14)?,
            synced_path: row.get(15)?,
        })
    }

    pub fn upsert_entry(&self, e: &Entry) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            r#"INSERT INTO entries (
                path, type, parent_path, remote_drivewsid, remote_docwsid, remote_etag,
                remote_zone, remote_shareid, size, mtime, hydrated, dirty, tombstone,
                local_sha256, last_synced_at, synced_path
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
            ON CONFLICT(path) DO UPDATE SET
                type=excluded.type, parent_path=excluded.parent_path,
                remote_drivewsid=excluded.remote_drivewsid, remote_docwsid=excluded.remote_docwsid,
                remote_etag=excluded.remote_etag, remote_zone=excluded.remote_zone,
                remote_shareid=excluded.remote_shareid, size=excluded.size, mtime=excluded.mtime,
                hydrated=excluded.hydrated, dirty=excluded.dirty, tombstone=excluded.tombstone,
                local_sha256=excluded.local_sha256, last_synced_at=excluded.last_synced_at,
                synced_path=excluded.synced_path"#,
            params![
                e.path,
                e.entry_type,
                e.parent_path,
                e.remote_drivewsid,
                e.remote_docwsid,
                e.remote_etag,
                e.remote_zone,
                e.remote_shareid,
                e.size,
                e.mtime,
                e.hydrated as i64,
                e.dirty as i64,
                e.tombstone as i64,
                e.local_sha256,
                e.last_synced_at,
                e.synced_path
            ],
        )?;
        Ok(())
    }

    pub fn get_entry(&self, path: &str) -> Result<Option<Entry>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT path,type,parent_path,remote_drivewsid,remote_docwsid,remote_etag,remote_zone,remote_shareid,size,mtime,hydrated,dirty,tombstone,local_sha256,last_synced_at,synced_path FROM entries WHERE path=?1")?;
        let entry = stmt
            .query_row(params![path], Self::row_to_entry)
            .optional()?;
        Ok(entry)
    }

    pub fn count_entries(&self) -> Result<i64> {
        let conn = self.conn.lock();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))?;
        Ok(count)
    }

    pub fn list_entries(&self) -> Result<Vec<Entry>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT path,type,parent_path,remote_drivewsid,remote_docwsid,remote_etag,remote_zone,remote_shareid,size,mtime,hydrated,dirty,tombstone,local_sha256,last_synced_at,synced_path FROM entries ORDER BY path")?;
        let rows = stmt.query_map([], Self::row_to_entry)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn list_children(&self, parent_path: &str) -> Result<Vec<Entry>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT path,type,parent_path,remote_drivewsid,remote_docwsid,remote_etag,remote_zone,remote_shareid,size,mtime,hydrated,dirty,tombstone,local_sha256,last_synced_at,synced_path FROM entries WHERE parent_path=?1 AND tombstone=0 ORDER BY path")?;
        let rows = stmt.query_map(params![parent_path], Self::row_to_entry)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn get_entry_by_remote_id(&self, drivewsid: &str) -> Result<Option<Entry>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT path,type,parent_path,remote_drivewsid,remote_docwsid,remote_etag,remote_zone,remote_shareid,size,mtime,hydrated,dirty,tombstone,local_sha256,last_synced_at,synced_path FROM entries WHERE remote_drivewsid=?1")?;
        let entry = stmt
            .query_row(params![drivewsid], Self::row_to_entry)
            .optional()?;
        Ok(entry)
    }

    pub fn rename_tree(&self, oldpath: &str, newpath: &str) -> Result<()> {
        let conn = self.conn.lock();
        let prefix = format!("{}/%", oldpath.trim_end_matches('/'));
        let children = {
            let mut stmt =
                conn.prepare("SELECT path FROM entries WHERE path=?1 OR path LIKE ?2")?;
            let rows = stmt.query_map(params![oldpath, prefix], |r| r.get::<_, String>(0))?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r?);
            }
            v
        };
        for child in children {
            let suffix = if child == oldpath {
                String::new()
            } else {
                child[oldpath.len()..].to_string()
            };
            let updated = if suffix.is_empty() {
                newpath.to_string()
            } else {
                format!("{}{}", newpath.trim_end_matches('/'), suffix)
            };
            let parent = match updated.rfind('/') {
                Some(0) => "/".to_string(),
                Some(i) => updated[..i].to_string(),
                None => "/".to_string(),
            };
            conn.execute(
                "UPDATE entries SET path=?1, parent_path=?2 WHERE path=?3",
                params![updated, parent, child],
            )?;
        }
        Ok(())
    }

    pub fn remove_entry(&self, path: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM entries WHERE path=?1", params![path])?;
        Ok(())
    }

    pub fn list_unhydrated_paths(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT path FROM entries WHERE type='file' AND tombstone=0 AND hydrated=0 ORDER BY path",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn mark_hydrated(
        &self,
        path: &str,
        sha: Option<&str>,
        size: Option<i64>,
        mtime: Option<i64>,
    ) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE entries SET hydrated=1, local_sha256=COALESCE(?1,local_sha256), size=COALESCE(?2,size), mtime=COALESCE(?3,mtime) WHERE path=?4",
            params![sha, size, mtime, path],
        )?;
        Ok(())
    }

    pub fn mark_dirty(&self, path: &str, size: Option<i64>, mtime: Option<i64>) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE entries SET dirty=1, tombstone=0, size=COALESCE(?1,size), mtime=COALESCE(?2,mtime) WHERE path=?3",
            params![size, mtime, path],
        )?;
        Ok(())
    }

    /// Mark a path as locally deleted: it stays in the index until the deletion
    /// has been uploaded, but it disappears from the mount immediately.
    pub fn mark_tombstone(&self, path: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE entries SET tombstone=1, dirty=1 WHERE path=?1",
            params![path],
        )?;
        Ok(())
    }

    pub fn mark_clean(&self, path: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE entries SET dirty=0, tombstone=0, last_synced_at=?1, synced_path=path WHERE path=?2",
            params![now_ts(), path],
        )?;
        Ok(())
    }

    pub fn remove_subtree(&self, path: &str) -> Result<()> {
        let conn = self.conn.lock();
        let prefix = format!("{}/%", path.trim_end_matches('/'));
        conn.execute(
            "DELETE FROM entries WHERE path=?1 OR path LIKE ?2",
            params![path, prefix],
        )?;
        Ok(())
    }

    pub fn queue_op(&self, op: &str, path: &str, target: Option<&str>) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO pending_ops (op, path, target_path, queued_at) VALUES (?1,?2,?3,?4)",
            params![op, path, target, now_ts()],
        )?;
        Ok(())
    }
}
