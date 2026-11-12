use anyhow::Result;
use std::sync::Arc;
use tracing::info;

use crate::config::{path_allowed, Config};
use crate::icloud_api::ICloudClient;
use crate::mirror::LocalMirror;
use crate::state::SyncState;

pub struct SyncEngine {
    pub config: Config,
    pub state: Arc<SyncState>,
    pub mirror: Arc<LocalMirror>,
    pub client: Box<dyn ICloudClient>,
}

impl SyncEngine {
    pub fn new(
        config: Config,
        state: Arc<SyncState>,
        mirror: Arc<LocalMirror>,
        client: Box<dyn ICloudClient>,
    ) -> Self {
        Self {
            config,
            state,
            mirror,
            client,
        }
    }

    pub fn start_background(&self) -> Result<()> {
        if self.state.count_entries()? > 0 {
            info!(
                "Using persistent local cache from {}",
                self.mirror.root.display()
            );
            self.reconcile_persistent_cache()?;
        } else {
            info!("Persistent cache not initialized; first remote crawl required");
            self.initial_scan()?;
        }
        if self.config.auto_sync {
            info!(
                "auto_sync enabled (upload={}s, refresh={}s)",
                self.config.upload_interval_seconds, self.config.remote_refresh_interval_seconds
            );
            // Background loops would be spawned here (tokio tasks / threads).
        } else {
            info!("auto_sync disabled; run `icloudctl sync` to refresh on demand");
        }
        Ok(())
    }

    pub fn initial_scan(&self) -> Result<()> {
        info!("Starting remote metadata crawl");
        let nodes = self.client.list_children_recursive()?;
        info!("Discovered {} remote entries", nodes.len());
        // Materialize metadata/stubs into mirror + state (respecting sync/exclude boundaries).
        for n in nodes {
            let allowed = path_allowed(
                &n.path,
                self.config.sync_paths.as_deref(),
                &self.config.exclude_paths,
            );
            let _ = allowed; // used by hydration/mutation policy
                             // TODO: upsert entry and create placeholder/dir in mirror
            let _ = n;
        }
        Ok(())
    }

    pub fn reconcile_persistent_cache(&self) -> Result<()> {
        info!("Reconciling persistent cache");
        // Verify local mirror contents against state DB, recompute hashes when needed,
        // recreate missing dirs/placeholders, and schedule hydration.
        Ok(())
    }

    pub fn ensure_local_file(&self, path: &str) -> Result<()> {
        if !path_allowed(
            path,
            self.config.sync_paths.as_deref(),
            &self.config.exclude_paths,
        ) {
            return Ok(());
        }
        if let Some(entry) = self.state.get_entry(path)? {
            if entry.entry_type == "file" && !entry.hydrated {
                // Download via client into mirror atomically, then mark hydrated.
                let _ = entry;
            }
        }
        Ok(())
    }

    pub fn sync_dirty_entries(&self) -> Result<()> {
        // Upload dirty local changes, process tombstones/renames, then mark clean.
        Ok(())
    }

    pub fn request_remote_refresh(&self) -> Result<()> {
        info!("Remote refresh requested");
        self.initial_scan()
    }

    pub fn hydrate_all(&self, dry_run: bool) -> Result<(usize, usize)> {
        let paths = self.state.list_unhydrated_paths()?;
        let mut eligible = Vec::new();
        for p in paths {
            if path_allowed(
                &p,
                self.config.sync_paths.as_deref(),
                &self.config.exclude_paths,
            ) {
                eligible.push(p);
            }
        }
        if dry_run {
            return Ok((eligible.len(), 0));
        }
        // Trigger hydration for each eligible path.
        Ok((eligible.len(), eligible.len()))
    }
}
