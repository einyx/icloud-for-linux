//! Path index: a lazily populated metadata cache.
//!
//! The upstream driver crawls the whole drive into a table before it mounts.
//! That costs minutes and a lot of memory on large drives, and it is wasted
//! work when the user only ever touches two folders.
//!
//! This index is filled on demand instead: a path is resolved when it is
//! first looked up, and a directory is listed when it is first read. Entries
//! carry the timestamp of their last remote validation so the sync engine can
//! re-check exactly the entries that went stale — a delta refresh rather than
//! a full re-crawl.

use anyhow::Result;
use rusqlite::params;

use crate::db::{now_ts, Db};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    File,
    Folder,
}

impl NodeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeKind::File => "file",
            NodeKind::Folder => "folder",
        }
    }

    pub fn parse(value: &str) -> NodeKind {
        match value {
            "folder" => NodeKind::Folder,
            _ => NodeKind::File,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Node {
    pub path: String,
    pub kind: NodeKind,
    pub size: u64,
    pub mtime: i64,
    pub etag: Option<String>,
    pub remote_id: Option<String>,
    /// Identifier of the document that carries the content.
    pub docwsid: Option<String>,
    /// The zone the document lives in.
    pub zone: Option<String>,
    /// Digest of the local blob holding the content, when hydrated.
    pub digest: Option<String>,
    pub hydrated: bool,
    pub dirty: bool,
    pub present: bool,
    /// When this entry was last validated against the remote.
    pub fetched_at: i64,
}

impl Node {
    pub fn new(path: &str, kind: NodeKind) -> Self {
        Self {
            path: path.to_string(),
            kind,
            size: 0,
            mtime: 0,
            etag: None,
            remote_id: None,
            docwsid: None,
            zone: None,
            digest: None,
            hydrated: false,
            dirty: false,
            present: true,
            fetched_at: now_ts(),
        }
    }
}

/// Parent directory of a normalised iCloud path.
pub fn parent_of(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => path[..i].to_string(),
    }
}

/// File name of a normalised iCloud path.
pub fn name_of(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

pub struct Index {
    db: std::sync::Arc<Db>,
}

impl Index {
    pub fn new(db: std::sync::Arc<Db>) -> Self {
        Self { db }
    }

    pub fn get(&self, path: &str) -> Result<Option<Node>> {
        let conn = self.db.lock();
        let mut stmt = conn.prepare(
            "SELECT path, kind, size, mtime, etag, remote_id, docwsid, zone, digest, hydrated, dirty, present, fetched_at \
             FROM nodes WHERE path = ?1",
        )?;
        let node = stmt.query_row(params![path], row_to_node).ok();
        Ok(node)
    }

    pub fn children(&self, parent: &str) -> Result<Vec<Node>> {
        let conn = self.db.lock();
        let mut stmt = conn.prepare(
            "SELECT path, kind, size, mtime, etag, remote_id, docwsid, zone, digest, hydrated, dirty, present, fetched_at \
             FROM nodes WHERE parent = ?1 AND present = 1 ORDER BY kind DESC, path ASC",
        )?;
        let rows = stmt.query_map(params![parent], row_to_node)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn upsert(&self, node: &Node) -> Result<()> {
        let conn = self.db.lock();
        conn.execute(
            "INSERT INTO nodes (path, parent, kind, size, mtime, etag, remote_id, docwsid, zone, digest, hydrated, dirty, present, fetched_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14) \
             ON CONFLICT(path) DO UPDATE SET \
               parent = excluded.parent, kind = excluded.kind, size = excluded.size, mtime = excluded.mtime, \
               etag = excluded.etag, remote_id = excluded.remote_id, digest = excluded.digest, \
               docwsid = excluded.docwsid, zone = excluded.zone, \
               hydrated = excluded.hydrated, dirty = excluded.dirty, present = excluded.present, \
               fetched_at = excluded.fetched_at",
            params![
                node.path,
                parent_of(&node.path),
                node.kind.as_str(),
                node.size as i64,
                node.mtime,
                node.etag,
                node.remote_id,
                node.docwsid,
                node.zone,
                node.digest,
                node.hydrated as i64,
                node.dirty as i64,
                node.present as i64,
                node.fetched_at,
            ],
        )?;
        Ok(())
    }

    /// Mark a path (and everything below it) as needing a remote re-check.
    pub fn invalidate(&self, path: &str) -> Result<()> {
        let conn = self.db.lock();
        conn.execute(
            "UPDATE nodes SET fetched_at = 0 WHERE path = ?1 OR path LIKE ?2",
            params![path, format!("{}/%", path.trim_end_matches('/'))],
        )?;
        Ok(())
    }

    pub fn set_digest(
        &self,
        path: &str,
        digest: Option<&str>,
        hydrated: bool,
        size: Option<u64>,
    ) -> Result<()> {
        let conn = self.db.lock();
        conn.execute(
            "UPDATE nodes SET digest = COALESCE(?1, digest), hydrated = ?2, size = COALESCE(?3, size) WHERE path = ?4",
            params![digest, hydrated as i64, size.map(|s| s as i64), path],
        )?;
        Ok(())
    }

    pub fn set_dirty(&self, path: &str, dirty: bool) -> Result<()> {
        let conn = self.db.lock();
        conn.execute(
            "UPDATE nodes SET dirty = ?1 WHERE path = ?2",
            params![dirty as i64, path],
        )?;
        Ok(())
    }

    /// Hide a path from the mount without losing its history.
    pub fn set_present(&self, path: &str, present: bool) -> Result<()> {
        let conn = self.db.lock();
        conn.execute(
            "UPDATE nodes SET present = ?1 WHERE path = ?2",
            params![present as i64, path],
        )?;
        Ok(())
    }

    pub fn remove_subtree(&self, path: &str) -> Result<()> {
        let conn = self.db.lock();
        conn.execute(
            "DELETE FROM nodes WHERE path = ?1 OR path LIKE ?2",
            params![path, format!("{}/%", path.trim_end_matches('/'))],
        )?;
        Ok(())
    }

    /// Move a subtree inside the index.
    pub fn rename_subtree(&self, old_path: &str, new_path: &str) -> Result<()> {
        let conn = self.db.lock();
        let prefix = format!("{}/%", old_path.trim_end_matches('/'));
        let mut stmt = conn.prepare("SELECT path FROM nodes WHERE path = ?1 OR path LIKE ?2")?;
        let paths: Vec<String> = stmt
            .query_map(params![old_path, prefix], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        for path in paths {
            let suffix = if path == old_path {
                String::new()
            } else {
                path[old_path.len()..].to_string()
            };
            let updated = if suffix.is_empty() {
                new_path.to_string()
            } else {
                format!("{}{}", new_path.trim_end_matches('/'), suffix)
            };
            conn.execute(
                "UPDATE nodes SET path = ?1, parent = ?2 WHERE path = ?3",
                params![updated, parent_of(&updated), path],
            )?;
        }
        Ok(())
    }

    /// Entries whose cached metadata is older than the TTL.
    pub fn stale_entries(&self, ttl_seconds: u64, limit: usize) -> Result<Vec<Node>> {
        let conn = self.db.lock();
        let cutoff = now_ts() - ttl_seconds as i64;
        let mut stmt = conn.prepare(
            "SELECT path, kind, size, mtime, etag, remote_id, docwsid, zone, digest, hydrated, dirty, present, fetched_at \
             FROM nodes WHERE fetched_at < ?1 AND present = 1 ORDER BY fetched_at ASC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![cutoff, limit as i64], row_to_node)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn all(&self) -> Result<Vec<Node>> {
        let conn = self.db.lock();
        let mut stmt = conn.prepare(
            "SELECT path, kind, size, mtime, etag, remote_id, docwsid, zone, digest, hydrated, dirty, present, fetched_at \
             FROM nodes ORDER BY path",
        )?;
        let rows = stmt.query_map([], row_to_node)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Digests referenced by the index — the roots the blob GC must keep.
    pub fn referenced_digests(&self) -> Result<std::collections::HashSet<String>> {
        let conn = self.db.lock();
        let mut stmt = conn.prepare("SELECT digest FROM nodes WHERE digest IS NOT NULL")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = std::collections::HashSet::new();
        for r in rows {
            out.insert(r?);
        }
        Ok(out)
    }

    pub fn counts(&self) -> Result<(usize, usize, usize)> {
        let conn = self.db.lock();
        let total: i64 = conn.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))?;
        let hydrated: i64 = conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE hydrated = 1 AND present = 1",
            [],
            |r| r.get(0),
        )?;
        let dirty: i64 = conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE dirty = 1 AND present = 1",
            [],
            |r| r.get(0),
        )?;
        Ok((total as usize, hydrated as usize, dirty as usize))
    }
}

fn row_to_node(row: &rusqlite::Row<'_>) -> rusqlite::Result<Node> {
    Ok(Node {
        path: row.get(0)?,
        kind: NodeKind::parse(&row.get::<_, String>(1)?),
        size: row.get::<_, i64>(2)?.max(0) as u64,
        mtime: row.get(3)?,
        etag: row.get(4)?,
        remote_id: row.get(5)?,
        docwsid: row.get(6)?,
        zone: row.get(7)?,
        digest: row.get(8)?,
        hydrated: row.get::<_, i64>(9)? != 0,
        dirty: row.get::<_, i64>(10)? != 0,
        present: row.get::<_, i64>(11)? != 0,
        fetched_at: row.get(12)?,
    })
}
