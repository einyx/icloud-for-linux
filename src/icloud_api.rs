use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Minimal iCloud Drive node metadata used by the sync engine.
/// This module is intentionally abstracted so the Apple/iCloud HTTP protocol
/// implementation can evolve independently of the local-first filesystem core.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteNode {
    pub path: String,
    pub node_type: String, // folder | file
    pub drivewsid: Option<String>,
    pub docwsid: Option<String>,
    pub etag: Option<String>,
    pub zone: Option<String>,
    pub shareid: Option<String>,
    pub size: i64,
    pub mtime: i64,
}

#[derive(Debug, Clone)]
pub struct AuthSession {
    pub cookie_dir: String,
    pub authenticated: bool,
}

pub trait ICloudClient: Send + Sync {
    fn authenticate(&mut self, interactive: bool) -> Result<()>;
    fn is_authenticated(&self) -> bool;
    fn list_children_recursive(&self) -> Result<Vec<RemoteNode>>;
    fn download_file(&self, node: &RemoteNode, dest: &Path) -> Result<()>;
    fn upload_file(&self, local_path: &Path, remote_parent: &str, name: &str)
        -> Result<RemoteNode>;
    fn mkdir(&self, remote_parent: &str, name: &str) -> Result<RemoteNode>;
    fn delete(&self, node: &RemoteNode) -> Result<()>;
    fn rename_move(
        &self,
        node: &RemoteNode,
        new_parent: &str,
        new_name: &str,
    ) -> Result<RemoteNode>;
}

/// Placeholder client implementation.
///
/// A full production implementation must handle Apple ID auth, 2FA/2SA, session
/// cookies/trust tokens, account partitioning, and the iCloud Drive web service
/// endpoints. This rewrite focuses on the Rust architecture (config, state DB,
/// mirror, sync orchestration, FUSE integration points) and provides a seam for
/// plugging in protocol support.
pub struct UnimplementedClient;

impl ICloudClient for UnimplementedClient {
    fn authenticate(&mut self, _interactive: bool) -> Result<()> {
        Err(anyhow!(
            "iCloud API client is not implemented yet. Implement Apple auth/session and Drive API in src/icloud_api.rs"
        ))
    }

    fn is_authenticated(&self) -> bool {
        false
    }

    fn list_children_recursive(&self) -> Result<Vec<RemoteNode>> {
        Err(anyhow!("iCloud Drive listing not implemented"))
    }

    fn download_file(&self, _node: &RemoteNode, _dest: &Path) -> Result<()> {
        Err(anyhow!("iCloud download not implemented"))
    }

    fn upload_file(
        &self,
        _local_path: &Path,
        _remote_parent: &str,
        _name: &str,
    ) -> Result<RemoteNode> {
        Err(anyhow!("iCloud upload not implemented"))
    }

    fn mkdir(&self, _remote_parent: &str, _name: &str) -> Result<RemoteNode> {
        Err(anyhow!("iCloud mkdir not implemented"))
    }

    fn delete(&self, _node: &RemoteNode) -> Result<()> {
        Err(anyhow!("iCloud delete not implemented"))
    }

    fn rename_move(
        &self,
        _node: &RemoteNode,
        _new_parent: &str,
        _new_name: &str,
    ) -> Result<RemoteNode> {
        Err(anyhow!("iCloud rename/move not implemented"))
    }
}
