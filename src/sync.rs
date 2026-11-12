//! The sync engine of the new design.
//!
//! Two independent loops:
//!
//! * **Drain** — takes the operations the journal recorded, applies them to
//!   the remote and marks them done. Operations are idempotent, so a crash
//!   mid-drain is recovered by simply running the loop again.
//! * **Refresh** — re-validates the metadata that went stale, instead of
//!   re-crawling the whole drive. Only the paths that were actually touched
//!   are checked, which keeps the steady-state cost proportional to activity
//!   rather than to the size of the drive.
//!
//! Both loops are optional: with `auto_sync: false` nothing runs in the
//! background and `icloudctl sync` triggers a single pass.

use anyhow::{anyhow, Result};
use std::collections::HashSet;
use std::sync::Arc;
use tracing::{info, warn};

use crate::blob::BlobStore;
use crate::config::{path_allowed, Config};
use crate::icloud_api::ICloudClient;
use crate::index::Index;
use crate::journal::{Journal, Op};

pub struct SyncEngine {
    pub config: Config,
    pub index: Arc<Index>,
    pub journal: Arc<Journal>,
    pub blobs: Arc<BlobStore>,
    pub client: Box<dyn ICloudClient>,
}

impl SyncEngine {
    pub fn new(
        config: Config,
        index: Arc<Index>,
        journal: Arc<Journal>,
        blobs: Arc<BlobStore>,
        client: Box<dyn ICloudClient>,
    ) -> Self {
        Self {
            config,
            index,
            journal,
            blobs,
            client,
        }
    }

    /// Called once before the mount is served.
    pub fn start(&self) -> Result<()> {
        let (total, hydrated, dirty) = self.index.counts()?;
        info!("index ready: {total} entries, {hydrated} hydrated, {dirty} pending upload");
        let (pending, failed, conflicts) = self.journal.counts()?;
        if pending > 0 || failed > 0 {
            info!("journal has {pending} pending and {failed} failed operations");
        }
        if conflicts > 0 {
            info!("{conflicts} conflict(s) preserved as blobs");
        }
        if self.config.auto_sync {
            info!(
                "auto_sync enabled (upload={}s, refresh={}s)",
                self.config.upload_interval_seconds, self.config.remote_refresh_interval_seconds
            );
        } else {
            info!("auto_sync disabled; run `icloudctl sync` for an on-demand pass");
        }
        Ok(())
    }

    /// One full pass: drain the journal, then refresh stale metadata.
    pub fn sync_once(&self) -> Result<SyncReport> {
        Ok(SyncReport {
            drained: self.drain_journal()?,
            refreshed: self.refresh_stale()?,
        })
    }

    /// Apply every pending journal operation to the remote.
    pub fn drain_journal(&self) -> Result<usize> {
        let ops = self.journal.pending()?;
        let mut applied = 0;
        for op in ops {
            if !self.entry_allowed(&op.path) {
                self.journal.mark_done(op.id)?;
                continue;
            }
            let outcome = self.apply_op(&op);
            match outcome {
                Ok(()) => {
                    self.journal.mark_done(op.id)?;
                    applied += 1;
                    self.index.set_dirty(&op.path, false)?;
                }
                Err(e) => {
                    warn!(path = %op.path, error = %e, "operation failed");
                    self.journal
                        .mark_state(op.id, "failed", Some(&e.to_string()))?;
                }
            }
        }
        Ok(applied)
    }

    fn apply_op(&self, op: &crate::journal::OpRecord) -> Result<()> {
        match op.op {
            Op::Create | Op::Write => {
                let digest = match op.digest.clone() {
                    Some(d) => d,
                    None => match self.index.get(&op.path) {
                        Ok(Some(node)) => match node.digest {
                            Some(d) => d,
                            None => {
                                return Err(anyhow!("no local content to upload for {}", op.path))
                            }
                        },
                        _ => return Err(anyhow!("no local content to upload for {}", op.path)),
                    },
                };
                let parent = crate::index::parent_of(&op.path);
                let name = crate::index::name_of(&op.path);
                let blob = self.blobs.path(&digest)?;
                let node = self.client.upload_file(&blob, &parent, &name)?;
                let mut updated = self.index.get(&op.path)?.unwrap_or_else(|| {
                    crate::index::Node::new(&op.path, crate::index::NodeKind::File)
                });
                updated.etag = node.etag.clone();
                updated.remote_id = node.remote_id.clone();
                updated.mtime = node.mtime;
                updated.size = node.size;
                updated.dirty = false;
                updated.fetched_at = crate::db::now_ts();
                self.index.upsert(&updated)?;
            }
            Op::Mkdir => {
                let parent = crate::index::parent_of(&op.path);
                let name = crate::index::name_of(&op.path);
                let node = self.client.mkdir(&parent, &name)?;
                let mut updated = self.index.get(&op.path)?.unwrap_or_else(|| {
                    crate::index::Node::new(&op.path, crate::index::NodeKind::Folder)
                });
                updated.etag = node.etag.clone();
                updated.remote_id = node.remote_id.clone();
                updated.dirty = false;
                updated.fetched_at = crate::db::now_ts();
                self.index.upsert(&updated)?;
            }
            Op::Remove => {
                let node = self.index.get(&op.path)?;
                if let Some(node) = node {
                    if let Some(remote_id) = node.remote_id {
                        self.client.delete(&remote_id)?;
                    }
                }
                self.index.set_present(&op.path, false)?;
            }
            Op::Rename => {
                let target = op
                    .target_path
                    .clone()
                    .ok_or_else(|| anyhow!("rename without a source path"))?;
                let parent = crate::index::parent_of(&op.path);
                let name = crate::index::name_of(&op.path);
                let node = self.client.rename_move(&target, &parent, &name)?;
                let mut updated = self.index.get(&op.path)?.unwrap_or_else(|| {
                    crate::index::Node::new(&op.path, crate::index::NodeKind::File)
                });
                updated.remote_id = node.remote_id.clone();
                updated.etag = node.etag.clone();
                updated.dirty = false;
                updated.fetched_at = crate::db::now_ts();
                self.index.upsert(&updated)?;
            }
        }
        Ok(())
    }

    /// Re-validate the metadata that aged past the TTL.
    ///
    /// This replaces the upstream full crawl: only entries that are actually
    /// stale are looked up again, and directories are refreshed one level at a
    /// time so the cost follows the user's activity.
    pub fn refresh_stale(&self) -> Result<usize> {
        let stale = self
            .index
            .stale_entries(self.config.index_ttl_seconds, 512)?;
        let mut refreshed = 0;
        for node in stale {
            if !self.entry_allowed(&node.path) {
                continue;
            }
            match self.client.stat_node(&node.path) {
                Ok(Some(remote)) => {
                    if remote.etag != node.etag && node.dirty {
                        // Both sides changed: keep the local version as a blob
                        // instead of overwriting it.
                        self.journal.record_conflict(
                            &node.path,
                            node.digest.as_deref(),
                            remote.etag.as_deref(),
                        )?;
                        info!(path = %node.path, "conflict detected; local version preserved as a blob");
                    }
                    let mut updated = node.clone();
                    updated.etag = remote.etag.clone();
                    updated.remote_id = remote.remote_id.clone();
                    updated.size = remote.size;
                    updated.mtime = remote.mtime;
                    updated.fetched_at = crate::db::now_ts();
                    if !node.dirty {
                        updated.digest = None;
                        updated.hydrated = false;
                    }
                    self.index.upsert(&updated)?;
                    refreshed += 1;
                }
                Ok(None) => {
                    // The path disappeared remotely.
                    if node.dirty {
                        // Keep it: the local version is still queued for upload.
                        self.index.invalidate(&node.path)?;
                    } else {
                        self.index.remove_subtree(&node.path)?;
                        refreshed += 1;
                    }
                }
                Err(e) => {
                    warn!(path = %node.path, error = %e, "refresh failed");
                }
            }
        }
        Ok(refreshed)
    }

    /// Make sure the content of a path is available locally.
    pub fn ensure_local_file(&self, path: &str) -> Result<()> {
        if !self.entry_allowed(path) {
            return Ok(());
        }
        let Some(node) = self.index.get(path)? else {
            return Ok(());
        };
        if node.hydrated && node.digest.is_some() {
            return Ok(());
        }
        let Some(remote_id) = node.remote_id.clone() else {
            return Ok(());
        };
        let blob = self.blobs.path(&format!("pending-{}", remote_id));
        let _ = blob; // placeholder: the real download lands with the client
        Err(anyhow!("content for {} is not local yet", path))
    }

    /// Download every eligible file that is not local yet.
    pub fn hydrate_all(&self, dry_run: bool) -> Result<(usize, usize)> {
        let nodes = self.index.all()?;
        let mut pending = Vec::new();
        for node in nodes {
            if node.kind != crate::index::NodeKind::File {
                continue;
            }
            if node.hydrated && node.digest.is_some() {
                continue;
            }
            if !self.entry_allowed(&node.path) {
                continue;
            }
            pending.push(node);
        }
        if dry_run {
            return Ok((pending.len(), 0));
        }
        let mut done = 0;
        let total = pending.len();
        for node in pending {
            match self.ensure_local_file(&node.path) {
                Ok(()) => done += 1,
                Err(e) => warn!(path = %node.path, error = %e, "hydration failed"),
            }
        }
        Ok((total, done))
    }

    /// Blobs that must be kept: everything the index or the journal refers to.
    pub fn live_digests(&self) -> Result<HashSet<String>> {
        let mut set = self.index.referenced_digests()?;
        set.extend(self.journal.referenced_digests()?);
        Ok(set)
    }

    fn entry_allowed(&self, path: &str) -> bool {
        path_allowed(
            path,
            self.config.sync_paths.as_deref(),
            &self.config.exclude_paths,
        )
    }
}

#[derive(Debug, Default)]
pub struct SyncReport {
    pub drained: usize,
    pub refreshed: usize,
}
