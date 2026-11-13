//! The boundary to the iCloud service.
//!
//! Everything above this trait (index, journal, blob store, FUSE surface) is
//! independent of the wire protocol. The implementation behind it lives in
//! [`crate::icloud`] and speaks to Apple's iCloud Drive web service.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteKind {
    File,
    Folder,
}

/// Metadata of a single remote node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteNode {
    pub path: String,
    pub kind: RemoteKind,
    /// Stable identifier of the node on the server (drivewsid).
    pub remote_id: Option<String>,
    /// Identifier of the document that carries the content.
    pub docwsid: Option<String>,
    /// The zone the document lives in.
    pub zone: Option<String>,
    /// Version marker of the content, used for conflict detection.
    pub etag: Option<String>,
    pub size: u64,
    pub mtime: i64,
}

pub trait ICloudClient: Send + Sync {
    /// Log in. `interactive` is true when a terminal is available, which
    /// allows the client to ask for a 2FA code.
    fn authenticate(&mut self, interactive: bool) -> Result<()>;

    fn is_authenticated(&self) -> bool;

    /// Metadata for a single path. `Ok(None)` means "does not exist".
    fn stat_node(&self, path: &str) -> Result<Option<RemoteNode>>;

    /// Direct children of a directory.
    fn list_children(&self, path: &str) -> Result<Vec<RemoteNode>>;

    /// Download the content of a node into `dest`, returning the byte count.
    fn download_file(&self, node: &RemoteNode, dest: &Path) -> Result<u64>;

    /// Upload a local file below `remote_parent` under `name`.
    fn upload_file(&self, local_path: &Path, remote_parent: &str, name: &str)
        -> Result<RemoteNode>;

    /// Create a directory below `remote_parent`.
    fn mkdir(&self, remote_parent: &str, name: &str) -> Result<RemoteNode>;

    /// Delete a node by its remote identifier.
    fn delete(&self, remote_id: &str) -> Result<()>;

    /// Move or rename `source_path` below `remote_parent` under `name`.
    fn rename_move(&self, source_path: &str, remote_parent: &str, name: &str)
        -> Result<RemoteNode>;
}

/// Placeholder client used until a protocol implementation is configured.
///
/// The rest of the system works against the local store, so the filesystem
/// still mounts and serves cached content; only the remote side is missing.
pub struct UnimplementedClient;

impl ICloudClient for UnimplementedClient {
    fn authenticate(&mut self, _interactive: bool) -> Result<()> {
        Err(anyhow!(
            "no iCloud protocol client is configured (see docs/ROADMAP.md, milestone M1)"
        ))
    }

    fn is_authenticated(&self) -> bool {
        false
    }

    fn stat_node(&self, _path: &str) -> Result<Option<RemoteNode>> {
        Err(anyhow!("iCloud stat is not implemented"))
    }

    fn list_children(&self, _path: &str) -> Result<Vec<RemoteNode>> {
        Err(anyhow!("iCloud directory listing is not implemented"))
    }

    fn download_file(&self, _node: &RemoteNode, _dest: &Path) -> Result<u64> {
        Err(anyhow!("iCloud download is not implemented"))
    }

    fn upload_file(
        &self,
        _local_path: &Path,
        _remote_parent: &str,
        _name: &str,
    ) -> Result<RemoteNode> {
        Err(anyhow!("iCloud upload is not implemented"))
    }

    fn mkdir(&self, _remote_parent: &str, _name: &str) -> Result<RemoteNode> {
        Err(anyhow!("iCloud mkdir is not implemented"))
    }

    fn delete(&self, _remote_id: &str) -> Result<()> {
        Err(anyhow!("iCloud delete is not implemented"))
    }

    fn rename_move(
        &self,
        _source_path: &str,
        _remote_parent: &str,
        _name: &str,
    ) -> Result<RemoteNode> {
        Err(anyhow!("iCloud rename is not implemented"))
    }
}
