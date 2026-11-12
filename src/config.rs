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
    pub username: String,
    pub password: String,
    pub cache_dir: String,
    pub warmup_mode: String,
    pub conflict_mode: String,
    pub upload_interval_seconds: u64,
    pub remote_refresh_interval_seconds: u64,
    pub warmup_workers: usize,
    pub cookie_dir: String,
    pub exclude_paths: Vec<String>,
    pub sync_paths: Option<Vec<String>>,
    pub auto_sync: bool,
    pub fuse_options: FuseOptions,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            username: String::new(),
            password: String::new(),
            cache_dir: "~/.cache/icloud-linux".to_string(),
            warmup_mode: "background".to_string(),
            conflict_mode: "copy".to_string(),
            upload_interval_seconds: 30,
            remote_refresh_interval_seconds: 300,
            warmup_workers: 1,
            cookie_dir: "~/.config/icloud-linux/cookies".to_string(),
            exclude_paths: Vec::new(),
            sync_paths: None,
            auto_sync: true,
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
}

pub fn expand_path(s: &str) -> PathBuf {
    PathBuf::from(shellexpand::tilde(s).to_string())
}

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

pub fn normalize_paths(paths: &[String]) -> Vec<String> {
    paths.iter().map(|p| normalize_icloud_path(p)).collect()
}

/// Resolve a path and refuse anything that tries to climb above the drive root.
///
/// Used by the mirror before touching the filesystem so a crafted path can
/// never escape the cache directory.
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
