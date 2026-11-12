use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub struct LocalMirror {
    pub root: PathBuf,
    pub tmp_dir: PathBuf,
}

impl LocalMirror {
    pub fn new(cache_dir: &Path) -> Result<Self> {
        let root = cache_dir.join("mirror");
        let tmp_dir = cache_dir.join("tmp");
        fs::create_dir_all(&root).with_context(|| format!("create mirror {}", root.display()))?;
        fs::create_dir_all(&tmp_dir)
            .with_context(|| format!("create tmp {}", tmp_dir.display()))?;
        Ok(Self { root, tmp_dir })
    }

    pub fn local_path(&self, icloud_path: &str) -> Result<PathBuf> {
        let normalized = crate::config::resolve_icloud_path(icloud_path)?;
        let rel = normalized.trim_start_matches('/');
        let p = self.root.join(rel);
        // Containment check: the resolved path must stay inside the mirror root.
        let canonical_root = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        let canonical = p.canonicalize().unwrap_or_else(|_| p.clone());
        if canonical != canonical_root && !canonical.starts_with(&canonical_root) {
            anyhow::bail!("path escapes mirror root: {}", icloud_path);
        }
        Ok(p)
    }

    pub fn ensure_dir(&self, path: &str) -> Result<()> {
        let p = self.local_path(path)?;
        if p.exists() && !p.is_dir() {
            fs::remove_file(&p)?;
        }
        fs::create_dir_all(&p)?;
        Ok(())
    }

    pub fn ensure_parent(&self, path: &str) -> Result<()> {
        let parent = Path::new(path)
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("/");
        let p = self.local_path(parent)?;
        fs::create_dir_all(p)?;
        Ok(())
    }

    pub fn materialize_placeholder(&self, path: &str, size: u64) -> Result<()> {
        self.ensure_parent(path)?;
        let p = self.local_path(path)?;
        if p.is_dir() {
            fs::remove_dir_all(&p)?;
        }
        let f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&p)?;
        f.set_len(size)?;
        Ok(())
    }

    pub fn create_file(&self, path: &str) -> Result<()> {
        self.ensure_parent(path)?;
        let p = self.local_path(path)?;
        OpenOptions::new().create(true).append(true).open(p)?;
        Ok(())
    }

    pub fn read(&self, path: &str, offset: u64, size: u32) -> Result<Vec<u8>> {
        let p = self.local_path(path)?;
        let mut f = File::open(p)?;
        f.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; size as usize];
        let n = f.read(&mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    pub fn write(&self, path: &str, offset: u64, data: &[u8]) -> Result<u32> {
        self.ensure_parent(path)?;
        let p = self.local_path(path)?;
        let mut f = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&p)?;
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(data)?;
        Ok(data.len() as u32)
    }

    pub fn truncate(&self, path: &str, size: u64) -> Result<()> {
        self.ensure_parent(path)?;
        let p = self.local_path(path)?;
        let f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&p)?;
        f.set_len(size)?;
        Ok(())
    }

    pub fn exists(&self, path: &str) -> bool {
        self.local_path(path).map(|p| p.exists()).unwrap_or(false)
    }

    pub fn is_dir(&self, path: &str) -> bool {
        self.local_path(path).map(|p| p.is_dir()).unwrap_or(false)
    }

    pub fn rename_path(&self, old: &str, new: &str) -> Result<()> {
        self.ensure_parent(new)?;
        let oldp = self.local_path(old)?;
        let newp = self.local_path(new)?;
        fs::rename(oldp, newp)?;
        Ok(())
    }

    pub fn remove_file(&self, path: &str) -> Result<()> {
        let p = self.local_path(path)?;
        if p.is_dir() {
            fs::remove_dir(&p)?;
        } else {
            fs::remove_file(&p)?;
        }
        Ok(())
    }

    pub fn remove_tree(&self, path: &str) -> Result<()> {
        let p = self.local_path(path)?;
        if p.is_dir() {
            fs::remove_dir_all(&p)?;
        } else if p.exists() {
            fs::remove_file(&p)?;
        }
        Ok(())
    }

    pub fn file_sha256(&self, path: &str) -> Result<String> {
        let p = self.local_path(path)?;
        let mut f = File::open(p)?;
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 1024 * 1024];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hex::encode(hasher.finalize()))
    }
}
