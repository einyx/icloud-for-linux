//! Write-ahead journal of local mutations.
//!
//! Every change made through the mount is recorded as an operation before it
//! is sent to iCloud. The journal gives three properties the upstream driver
//! does not have:
//!
//! * **Idempotency** — each operation carries a key derived from its
//!   intent, so replaying the journal after a crash cannot apply an upload
//!   twice.
//! * **Ordering** — content operations are applied before the rename or
//!   delete that depends on them, so a half-finished upload is never
//!   referenced by a move.
//! * **Preserved history** — an operation that loses a conflict keeps the
//!   local version as a blob reference instead of being overwritten.

use anyhow::Result;
use rusqlite::params;

use crate::db::{now_ts, Db};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// A file was created locally.
    Create,
    /// A file's content changed.
    Write,
    /// A directory was created locally.
    Mkdir,
    /// A path was removed locally.
    Remove,
    /// A path was moved or renamed locally.
    Rename,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Create => "create",
            Op::Write => "write",
            Op::Mkdir => "mkdir",
            Op::Remove => "remove",
            Op::Rename => "rename",
        }
    }

    pub fn parse(value: &str) -> Option<Op> {
        match value {
            "create" => Some(Op::Create),
            "write" => Some(Op::Write),
            "mkdir" => Some(Op::Mkdir),
            "remove" => Some(Op::Remove),
            "rename" => Some(Op::Rename),
            _ => None,
        }
    }

    /// Ordering weight: content first, then structural changes.
    fn weight(self) -> i64 {
        match self {
            Op::Create => 0,
            Op::Write => 0,
            Op::Mkdir => 1,
            Op::Rename => 2,
            Op::Remove => 3,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OpRecord {
    pub id: i64,
    pub op: Op,
    pub path: String,
    pub target_path: Option<String>,
    pub digest: Option<String>,
    pub size: Option<i64>,
    pub mtime: Option<i64>,
    pub state: String,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct Conflict {
    pub path: String,
    pub local_digest: Option<String>,
    pub remote_etag: Option<String>,
    pub created_at: i64,
}

pub struct Journal {
    db: std::sync::Arc<Db>,
}

impl Journal {
    pub fn new(db: std::sync::Arc<Db>) -> Self {
        Self { db }
    }

    /// Record an operation. The key makes the call idempotent: enqueueing the
    /// same intent twice returns the existing record instead of duplicating
    /// the work.
    pub fn enqueue(
        &self,
        op: Op,
        path: &str,
        target_path: Option<&str>,
        digest: Option<&str>,
        size: Option<u64>,
    ) -> Result<i64> {
        let key = format!(
            "{}:{}:{}:{}",
            op.as_str(),
            path,
            target_path.unwrap_or(""),
            digest.unwrap_or("")
        );
        let conn = self.db.lock();
        // A new content version supersedes the pending ones for the same path:
        // only the final content has to reach the remote.
        if matches!(op, Op::Write) {
            conn.execute(
                "DELETE FROM ops WHERE path = ?1 AND op IN ('write', 'create') AND state = 'pending' \
                 AND (digest IS NULL OR digest IS NOT ?2)",
                params![path, digest],
            )?;
        }
        conn.execute(
            "INSERT INTO ops (op, path, target_path, digest, size, created_at, key) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(key) DO UPDATE SET state = 'pending', last_error = NULL",
            params![
                op.as_str(),
                path,
                target_path,
                digest,
                size.map(|s| s as i64),
                now_ts(),
                key,
            ],
        )?;
        let id: i64 = conn.query_row("SELECT id FROM ops WHERE key = ?1", params![key], |r| {
            r.get(0)
        })?;
        Ok(id)
    }

    /// Pending operations in apply order.
    pub fn pending(&self) -> Result<Vec<OpRecord>> {
        let conn = self.db.lock();
        let mut stmt = conn.prepare(
            "SELECT id, op, path, target_path, digest, size, mtime, state, attempts, last_error, created_at \
             FROM ops WHERE state IN ('pending', 'failed') ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([], row_to_op)?;
        let mut ops: Vec<OpRecord> = Vec::new();
        for r in rows {
            ops.push(r?);
        }
        drop(stmt);
        // Content operations first, then renames, then deletions.
        ops.sort_by(|a, b| {
            a.op.weight()
                .cmp(&b.op.weight())
                .then_with(|| {
                    a.path
                        .matches('/')
                        .count()
                        .cmp(&b.path.matches('/').count())
                })
                .then_with(|| a.created_at.cmp(&b.created_at))
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(ops)
    }

    pub fn mark_state(&self, id: i64, state: &str, error: Option<&str>) -> Result<()> {
        let conn = self.db.lock();
        conn.execute(
            "UPDATE ops SET state = ?1, last_error = ?2, attempts = attempts + 1 WHERE id = ?3",
            params![state, error, id],
        )?;
        Ok(())
    }

    pub fn mark_done(&self, id: i64) -> Result<()> {
        self.mark_state(id, "done", None)
    }

    /// Drop completed operations older than `older_than` seconds.
    pub fn purge_done(&self, older_than: i64) -> Result<usize> {
        let conn = self.db.lock();
        let removed = conn.execute(
            "DELETE FROM ops WHERE state = 'done' AND created_at < ?1",
            params![now_ts() - older_than],
        )?;
        Ok(removed)
    }

    /// Record a conflict: the local version is kept as a blob so nothing is
    /// lost, and the remote version stays authoritative for the path.
    pub fn record_conflict(
        &self,
        path: &str,
        local_digest: Option<&str>,
        remote_etag: Option<&str>,
    ) -> Result<()> {
        let conn = self.db.lock();
        conn.execute(
            "INSERT INTO conflicts (path, local_digest, remote_etag, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![path, local_digest, remote_etag, now_ts()],
        )?;
        Ok(())
    }

    pub fn conflicts(&self) -> Result<Vec<Conflict>> {
        let conn = self.db.lock();
        let mut stmt = conn.prepare(
            "SELECT path, local_digest, remote_etag, created_at FROM conflicts ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Conflict {
                path: r.get(0)?,
                local_digest: r.get(1)?,
                remote_etag: r.get(2)?,
                created_at: r.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Digests referenced by pending operations — the GC must keep them.
    pub fn referenced_digests(&self) -> Result<std::collections::HashSet<String>> {
        let conn = self.db.lock();
        let mut stmt =
            conn.prepare("SELECT digest FROM ops WHERE digest IS NOT NULL AND state != 'done'")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = std::collections::HashSet::new();
        for r in rows {
            out.insert(r?);
        }
        Ok(out)
    }

    pub fn counts(&self) -> Result<(usize, usize, usize)> {
        let conn = self.db.lock();
        let pending: i64 = conn.query_row(
            "SELECT COUNT(*) FROM ops WHERE state IN ('pending', 'failed')",
            [],
            |r| r.get(0),
        )?;
        let failed: i64 =
            conn.query_row("SELECT COUNT(*) FROM ops WHERE state = 'failed'", [], |r| {
                r.get(0)
            })?;
        let conflicts: i64 = conn.query_row("SELECT COUNT(*) FROM conflicts", [], |r| r.get(0))?;
        Ok((pending as usize, failed as usize, conflicts as usize))
    }
}

fn row_to_op(row: &rusqlite::Row<'_>) -> rusqlite::Result<OpRecord> {
    let op: String = row.get(1)?;
    Ok(OpRecord {
        id: row.get(0)?,
        op: Op::parse(&op).unwrap_or(Op::Write),
        path: row.get(2)?,
        target_path: row.get(3)?,
        digest: row.get(4)?,
        size: row.get(5)?,
        mtime: row.get(6)?,
        state: row.get(7)?,
        attempts: row.get(8)?,
        last_error: row.get(9)?,
        created_at: row.get(10)?,
    })
}
