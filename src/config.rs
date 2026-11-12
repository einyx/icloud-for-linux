//! Configuration.
//!
//! The options describe *when* and *what* the client synchronises. They
//! deliberately differ from the upstream Python project: there is no full
//! crawl, so there is no "warmup" phase to configure, and conflicts are kept
//! as blobs instead of being copied next to the original file.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FuseOptions {
    pub allow_other: bool,
    pub ro: bool,
    pub nonempty: bool,
}

impl Default for FuseOptions {
    fn default() -> Self {
        Self {
            allow_other: false,
            ro: false,
            nonempty: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Apple ID.
    pub username: String,
    /// Apple ID password.
    pub password: String,
    /// Root of the local store (blobs, staging, index, journal).
    pub cache_dir: String,
    /// Where the session cookies are kept.
    pub cookie_dir: String,
    /// How long cached metadata stays valid before it is re-checked.
    pub index_ttl_seconds: u64,
    /// `lazy` downloads a file when it is opened, `background` downloads
    /// eligible files ahead of time.
    pub hydration: String,
    /// `keep-local` preserves the local version of a diverged path as a blob,
    /// `keep-remote` accepts the remote version.
    pub conflict_policy: String,
    /// Seconds between drain passes.
    pub upload_interval_seconds: u64,
    /// Seconds between metadata refresh passes.
    pub remote_refresh_interval_seconds: u64,
    /// Run the background loops. When false, `icloudctl sync` drives them.
    pub auto_sync: bool,
    /// Allow-list of paths that may be hydrated or mutated.
    pub sync_paths: Option<Vec<String>>,
    /// Deny-list, evaluated before the allow-list.
    pub exclude_paths: Vec<String>,
    pub fuse_options: FuseOptions,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            username: String::new(),
            password: String::new(),
            cache_dir: "~/.cache/icloud-for-linux".to_string(),
            cookie_dir: "~/.config/icloud-for-linux/cookies".to_string(),
            index_ttl_seconds: 300,
            hydration: "lazy".to_string(),
            conflict_policy: "keep-local".to_string(),
            upload_interval_seconds: 30,
            remote_refresh_interval_seconds: 300,
            auto_sync: true,
            sync_paths: None,
            exclude_paths: Vec::new(),
            fuse_options: FuseOptions::default(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let cfg: Config = serde_yaml::from_str(&content).context("failed to parse config yaml")?;
        Ok(cfg)
    }

    pub fn expanded_cache_dir(&self) -> PathBuf {
        expand_path(&self.cache_dir)
    }

    pub fn expanded_cookie_dir(&self) -> PathBuf {
        expand_path(&self.cookie_dir)
    }

    pub fn db_path(&self) -> PathBuf {
        self.expanded_cache_dir().join("state.sqlite3")
    }
}

pub fn expand_path(s: &str) -> PathBuf {
    PathBuf::from(shellexpand::tilde(s).to_string())
}

/// Normalise a path to an absolute, canonical iCloud path.
pub fn normalize_icloud_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", parts.join("/"))
    }
}

/// Resolve a path and refuse anything that climbs above the drive root.
pub fn resolve_icloud_path(path: &str) -> Result<String> {
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    anyhow::bail!("path escapes the drive root: {}", path);
                }
            }
            s => parts.push(s),
        }
    }
    if parts.is_empty() {
        Ok("/".to_string())
    } else {
        Ok(format!("/{}", parts.join("/")))
    }
}

pub fn normalize_paths(paths: &[String]) -> Vec<String> {
    paths.iter().map(|p| normalize_icloud_path(p)).collect()
}

/// The synchronisation boundary: deny-list first, then allow-list.
pub fn path_allowed(path: &str, sync_paths: Option<&[String]>, exclude_paths: &[String]) -> bool {
    let path = normalize_icloud_path(path);
    let excludes = normalize_paths(exclude_paths);
    for prefix in &excludes {
        if prefix == "/" || path == *prefix || path.starts_with(&(prefix.clone() + "/")) {
            return false;
        }
    }
    match sync_paths {
        None => true,
        Some([]) => true,
        Some(allow) => {
            let allow = normalize_paths(allow);
            allow.iter().any(|prefix| {
                prefix == "/" || path == *prefix || path.starts_with(&(prefix.clone() + "/"))
            })
        }
    }
}
