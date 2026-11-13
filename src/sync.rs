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

use crate::blob::{working_copy_path, BlobStore};
use crate::config::{path_allowed, Config};
use crate::db::now_ts;
use crate::icloud_api::{ICloudClient, RemoteKind, RemoteNode};
use crate::index::{name_of, parent_of, Index, Node, NodeKind};
use crate::journal::{Journal, Op};

pub struct SyncEngine {
    pub config: Config,
    pub index: Arc<Index>,
    pub journal: Arc<Journal>,
    pub blobs: Arc<BlobStore>,
    pub client: Box<dyn ICloudClient>,
    /// Paths a hydrator is already working on.
    claimed: parking_lot::Mutex<std::collections::HashSet<String>>,
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
            claimed: parking_lot::Mutex::new(std::collections::HashSet::new()),
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

    // ───────────────────────── remote lookups ─────────────────────────

    /// Resolve a path, using the index when it is fresh and the remote
    /// otherwise.
    pub fn resolve_path(&self, path: &str) -> Result<Option<RemoteNode>> {
        if let Some(node) = self.index.get(path)? {
            if node.present && !self.is_stale(&node) {
                return Ok(Some(to_remote(&node)));
            }
        }
        let Some(remote) = self.client.stat_node(path)? else {
            let _ = self.index.set_present(path, false);
            return Ok(None);
        };
        self.store_remote(path, &remote)?;
        Ok(Some(remote))
    }

    /// The children of a directory, from the remote.
    pub fn list_dir(&self, path: &str) -> Result<Vec<RemoteNode>> {
        let children = self.client.list_children(path)?;
        for child in &children {
            let child_path = if path == "/" {
                format!("/{}", child.path)
            } else {
                format!("{}/{}", path.trim_end_matches('/'), child.path)
            };
            self.store_remote(&child_path, child)?;
        }
        Ok(children)
    }

    fn store_remote(&self, path: &str, remote: &RemoteNode) -> Result<()> {
        let kind = match remote.kind {
            RemoteKind::Folder => NodeKind::Folder,
            RemoteKind::File => NodeKind::File,
        };
        let mut node = self
            .index
            .get(path)?
            .unwrap_or_else(|| Node::new(path, kind));
        node.kind = kind;
        node.size = remote.size;
        node.mtime = remote.mtime;
        node.etag = remote.etag.clone();
        node.remote_id = remote.remote_id.clone();
        node.docwsid = remote.docwsid.clone();
        node.zone = remote.zone.clone();
        node.fetched_at = now_ts();
        if kind == NodeKind::Folder {
            node.hydrated = true;
        }
        self.index.upsert(&node)?;
        Ok(())
    }

    fn is_stale(&self, node: &Node) -> bool {
        now_ts() - node.fetched_at > self.config.index_ttl_seconds as i64
    }

    // ─────────────────────────── the journal ───────────────────────────

    /// Apply every pending journal operation to the remote.
    pub fn drain_journal(&self) -> Result<usize> {
        let ops = self.journal.pending()?;
        let mut applied = 0;
        for op in ops {
            if !self.entry_allowed(&op.path) {
                self.journal.mark_done(op.id)?;
                continue;
            }
            match self.apply_op(&op) {
                Ok(()) => {
                    self.journal.mark_done(op.id)?;
                    applied += 1;
                    let _ = self.index.set_dirty(&op.path, false);
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
                    Some(digest) => digest,
                    None => match self.index.get(&op.path) {
                        Ok(Some(node)) => match node.digest {
                            Some(digest) => digest,
                            None => {
                                return Err(anyhow!("no local content to upload for {}", op.path))
                            }
                        },
                        _ => return Err(anyhow!("no local content to upload for {}", op.path)),
                    },
                };
                let blob = self.blobs.path(&digest)?;
                let parent = parent_of(&op.path);
                let name = name_of(&op.path);
                let remote = self.client.upload_file(&blob, &parent, &name)?;
                let mut updated = self
                    .index
                    .get(&op.path)?
                    .unwrap_or_else(|| Node::new(&op.path, NodeKind::File));
                updated.etag = remote.etag.clone();
                updated.remote_id = remote.remote_id.clone();
                updated.docwsid = remote.docwsid.clone();
                updated.zone = remote.zone.clone();
                updated.mtime = remote.mtime;
                updated.size = remote.size;
                updated.dirty = false;
                updated.fetched_at = now_ts();
                self.index.upsert(&updated)?;
            }
            Op::Mkdir => {
                let parent = parent_of(&op.path);
                let name = name_of(&op.path);
                let remote = self.client.mkdir(&parent, &name)?;
                let mut updated = self
                    .index
                    .get(&op.path)?
                    .unwrap_or_else(|| Node::new(&op.path, NodeKind::Folder));
                updated.etag = remote.etag.clone();
                updated.remote_id = remote.remote_id.clone();
                updated.docwsid = remote.docwsid.clone();
                updated.dirty = false;
                updated.fetched_at = now_ts();
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
                let parent = parent_of(&op.path);
                let name = name_of(&op.path);
                let remote = self.client.rename_move(&target, &parent, &name)?;
                let mut updated = self
                    .index
                    .get(&op.path)?
                    .unwrap_or_else(|| Node::new(&op.path, NodeKind::File));
                updated.remote_id = remote.remote_id.clone();
                updated.etag = remote.etag.clone();
                updated.docwsid = remote.docwsid.clone();
                updated.dirty = false;
                updated.fetched_at = now_ts();
                self.index.upsert(&updated)?;
            }
        }
        Ok(())
    }

    // ──────────────────────────── refresh ──────────────────────────────

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
                        let local = self.commit_local(&node.path)?;
                        self.journal.record_conflict(
                            &node.path,
                            local.as_deref(),
                            remote.etag.as_deref(),
                        )?;
                        info!(path = %node.path, "conflict detected; local version preserved as a blob");
                    }
                    let mut updated = node.clone();
                    updated.etag = remote.etag.clone();
                    updated.remote_id = remote.remote_id.clone();
                    updated.docwsid = remote.docwsid.clone();
                    updated.zone = remote.zone.clone();
                    updated.size = remote.size;
                    updated.mtime = remote.mtime;
                    updated.fetched_at = now_ts();
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

    // ─────────────────────────── conflicts ────────────────────────────

    /// Commit the local version of a path into the content store.
    ///
    /// The working copy is the local truth; committing it gives the conflict
    /// a blob that survives the next upload and the garbage collector.
    pub fn commit_local(&self, path: &str) -> Result<Option<String>> {
        let working = working_copy_path(self.blobs.staging_dir(), path);
        if !working.exists() {
            return Ok(self.index.get(path)?.and_then(|node| node.digest));
        }
        let size = std::fs::metadata(&working)?.len();
        let mut reader = std::fs::File::open(&working)?;
        let digest = self.blobs.put_reader(&mut reader)?;
        let mut node = self
            .index
            .get(path)?
            .unwrap_or_else(|| Node::new(path, NodeKind::File));
        node.digest = Some(digest.clone());
        node.hydrated = true;
        node.size = size;
        self.index.upsert(&node)?;
        Ok(Some(digest))
    }

    /// The conflicts that were recorded, newest first.
    pub fn conflicts(&self) -> Result<Vec<crate::journal::Conflict>> {
        self.journal.conflicts()
    }

    // ─────────────────────────── hydration ─────────────────────────────

    /// Claim paths that should be downloaded in the background.
    ///
    /// A path is claimed by exactly one hydrator, so several workers never
    /// download the same file twice.
    pub fn claim_hydration_targets(&self, limit: usize) -> Result<Vec<String>> {
        let nodes = self.index.all()?;
        let mut claimed = self.claimed.lock();
        let mut out = Vec::new();
        for node in nodes {
            if out.len() >= limit {
                break;
            }
            if node.kind != NodeKind::File || !node.present {
                continue;
            }
            if node.hydrated && node.digest.is_some() {
                continue;
            }
            if !self.entry_allowed(&node.path) {
                continue;
            }
            if claimed.contains(&node.path) {
                continue;
            }
            claimed.insert(node.path.clone());
            out.push(node.path);
        }
        Ok(out)
    }

    /// Release a claimed path.
    pub fn release_claim(&self, path: &str) {
        self.claimed.lock().remove(path);
    }

    /// Download one file and commit it to the content store.
    pub fn hydrate_one(&self, path: &str) -> Result<()> {
        let result = self.ensure_local_file(path);
        if result.is_err() {
            self.release_claim(path);
        }
        result
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
        let remote = to_remote(&node);
        if remote.docwsid.is_none() && remote.remote_id.is_none() {
            return Err(anyhow!("the remote metadata for {} is incomplete", path));
        }

        // Download into the store; the blob is only linked into the index once
        // it is complete and hashed.
        let tmp = self.blobs.staging_dir().join("download.part");
        let size = self.client.download_file(&remote, &tmp)?;
        let digest = {
            let mut file = std::fs::File::open(&tmp)?;
            self.blobs.put_reader(&mut file)?
        };
        let _ = std::fs::remove_file(&tmp);
        self.index
            .set_digest(path, Some(&digest), true, Some(size))?;
        Ok(())
    }

    /// List a subtree of the drive and download everything in it.
    ///
    /// The index is filled on demand, so a subtree has to be listed before
    /// its files can be downloaded. This is the explicit counterpart of the
    /// background hydration: the user asks for a folder, the client walks it
    /// and the files land in the content store.
    pub fn hydrate_subtree(&self, path: &str) -> Result<(usize, usize)> {
        let root = crate::config::normalize_icloud_path(path);
        let mut directories = vec![root.clone()];
        let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut files: Vec<String> = Vec::new();

        while let Some(directory) = directories.pop() {
            if !visited.insert(directory.clone()) {
                continue;
            }
            if !self.entry_allowed(&directory) {
                continue;
            }
            let children = match self.client.list_children(&directory) {
                Ok(children) => children,
                Err(e) => {
                    warn!(path = %directory, error = %e, "listing failed");
                    continue;
                }
            };
            for child in children {
                let child_path = if directory == "/" {
                    format!("/{}", child.path)
                } else {
                    format!("{}/{}", directory.trim_end_matches('/'), child.path)
                };
                self.store_remote(&child_path, &child)?;
                match child.kind {
                    RemoteKind::Folder => directories.push(child_path),
                    RemoteKind::File => files.push(child_path),
                }
            }
        }

        let total = files.len();
        let mut done = 0;
        for file in files {
            match self.ensure_local_file(&file) {
                Ok(()) => done += 1,
                Err(e) => warn!(path = %file, error = %e, "hydration failed"),
            }
        }
        Ok((total, done))
    }

    /// Download every eligible file that is not local yet.
    pub fn hydrate_all(&self, dry_run: bool) -> Result<(usize, usize)> {
        let nodes = self.index.all()?;
        let mut pending = Vec::new();
        for node in nodes {
            if node.kind != NodeKind::File {
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
        let total = pending.len();
        let mut done = 0;
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
        // The versions that lost a conflict have to survive as well.
        set.extend(self.journal.conflict_digests()?);
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

/// Convert an index entry into the node type of the client interface.
fn to_remote(node: &Node) -> RemoteNode {
    RemoteNode {
        path: node.path.clone(),
        kind: match node.kind {
            NodeKind::Folder => RemoteKind::Folder,
            NodeKind::File => RemoteKind::File,
        },
        remote_id: node.remote_id.clone(),
        docwsid: node.docwsid.clone(),
        zone: node.zone.clone(),
        etag: node.etag.clone(),
        size: node.size,
        mtime: node.mtime,
    }
}

#[derive(Debug, Default)]
pub struct SyncReport {
    pub drained: usize,
    pub refreshed: usize,
}
