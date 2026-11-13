//! Content-addressed blob store.
//!
//! File contents are stored once, keyed by the SHA-256 of the content, and
//! referenced by the index. This replaces the upstream "mirror directory with
//! one copy of every file" model:
//!
//! * identical content is stored once, no matter how many paths reference it
//! * integrity is verifiable at any time by re-hashing
//! * the losing side of a conflict is preserved as a blob instead of a copied
//!   file, so nothing is silently discarded
//!
//! Layout under the cache directory:
//!
//! ```text
//! blobs/ab/cdef0123…   # sharded by the first two hex characters
//! staging/<id>.part    # in-flight writes, renamed into place once complete
//! ```

use anyhow::{anyhow, Context, Result};
use sha2::{Digest as Sha2Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Hex-encoded SHA-256 digest of a blob.
pub type BlobDigest = String;

const COPY_BUF: usize = 1024 * 1024;

pub struct BlobStore {
    root: PathBuf,
    staging: PathBuf,
}

impl BlobStore {
    pub fn new(cache_dir: &Path) -> Result<Self> {
        let root = cache_dir.join("blobs");
        let staging = cache_dir.join("staging");
        fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
        fs::create_dir_all(&staging).with_context(|| format!("create {}", staging.display()))?;
        Ok(Self { root, staging })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory in-flight downloads and uploads are staged in.
    pub fn staging_dir(&self) -> &Path {
        &self.staging
    }

    /// Path of a blob inside the store. Does not check existence.
    pub fn path(&self, digest: &str) -> Result<PathBuf> {
        let digest = normalize_digest(digest)?;
        let (shard, rest) = digest.split_at(2);
        Ok(self.root.join(shard).join(rest))
    }

    pub fn contains(&self, digest: &str) -> bool {
        self.path(digest).map(|p| p.is_file()).unwrap_or(false)
    }

    pub fn size(&self, digest: &str) -> Result<u64> {
        let path = self.path(digest)?;
        let meta = fs::metadata(&path).with_context(|| format!("stat blob {}", digest))?;
        Ok(meta.len())
    }

    /// Store a byte slice and return its digest.
    pub fn put_bytes(&self, data: &[u8]) -> Result<BlobDigest> {
        let mut hasher = Sha256::new();
        hasher.update(data);
        let digest = hex::encode(hasher.finalize());
        let target = self.path(&digest)?;
        if target.is_file() {
            return Ok(digest);
        }
        self.ensure_parent(&target)?;
        let tmp = self.staging_file()?;
        {
            let mut file = File::create(&tmp)?;
            file.write_all(data)?;
            file.sync_all()?;
        }
        self.commit(&tmp, &target)?;
        Ok(digest)
    }

    /// Store the contents of a reader, streaming so large files never sit in
    /// memory. The blob is renamed into place only after it is fully written
    /// and fsynced, so a crash can never leave a truncated blob behind.
    pub fn put_reader<R: Read>(&self, mut reader: R) -> Result<BlobDigest> {
        let tmp = self.staging_file()?;
        let digest = {
            let mut file = File::create(&tmp)?;
            let mut hasher = Sha256::new();
            let mut buf = vec![0u8; COPY_BUF];
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                file.write_all(&buf[..n])?;
            }
            file.sync_all()?;
            hex::encode(hasher.finalize())
        };
        let target = self.path(&digest)?;
        if target.is_file() {
            let _ = fs::remove_file(&tmp);
            return Ok(digest);
        }
        self.ensure_parent(&target)?;
        self.commit(&tmp, &target)?;
        Ok(digest)
    }

    /// Open a blob for reading.
    pub fn open(&self, digest: &str) -> Result<File> {
        let path = self.path(digest)?;
        File::open(&path).with_context(|| format!("open blob {}", digest))
    }

    pub fn read_all(&self, digest: &str) -> Result<Vec<u8>> {
        let mut file = self.open(digest)?;
        let mut out = Vec::new();
        file.read_to_end(&mut out)?;
        Ok(out)
    }

    /// Re-hash a blob and compare it with its digest.
    pub fn verify(&self, digest: &str) -> Result<bool> {
        let path = self.path(digest)?;
        if !path.is_file() {
            return Ok(false);
        }
        let mut file = File::open(&path)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; COPY_BUF];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hex::encode(hasher.finalize()) == normalize_digest(digest)?)
    }

    /// Materialise a blob at `target`, preferring a hard link so the content
    /// is not duplicated on disk. Falls back to a copy across devices.
    pub fn link_to(&self, digest: &str, target: &Path) -> Result<()> {
        let source = self.path(digest)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        if target.exists() {
            fs::remove_file(target)?;
        }
        match fs::hard_link(&source, target) {
            Ok(()) => Ok(()),
            Err(_) => fs::copy(&source, target)
                .map(|_| ())
                .map_err(|e| anyhow!("materialise blob {}: {}", digest, e)),
        }
    }

    /// Digests of every blob currently stored.
    pub fn list(&self) -> Result<Vec<BlobDigest>> {
        let mut out = Vec::new();
        if !self.root.is_dir() {
            return Ok(out);
        }
        for shard in fs::read_dir(&self.root)? {
            let shard = shard?.path();
            if !shard.is_dir() {
                continue;
            }
            for entry in fs::read_dir(&shard)? {
                let entry = entry?.path();
                if entry.is_file() {
                    if let Some(name) = entry.file_name().and_then(|n| n.to_str()) {
                        out.push(format!("{}{}", shard_name(&shard), name));
                    }
                }
            }
        }
        Ok(out)
    }

    /// Remove blobs that are not referenced any more.
    pub fn gc(&self, keep: &HashSet<BlobDigest>) -> Result<usize> {
        let mut removed = 0;
        for digest in self.list()? {
            if keep.contains(&digest) {
                continue;
            }
            let path = self.path(&digest)?;
            if fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        // Drop shard directories that became empty.
        if let Ok(shards) = fs::read_dir(&self.root) {
            for shard in shards.flatten() {
                let path = shard.path();
                if path.is_dir() && fs::read_dir(&path).map(|d| d.count() == 0).unwrap_or(false) {
                    let _ = fs::remove_dir(&path);
                }
            }
        }
        Ok(removed)
    }

    /// Total bytes occupied by the store.
    pub fn total_size(&self) -> Result<u64> {
        let mut total = 0;
        for digest in self.list()? {
            total += self.size(&digest)?;
        }
        Ok(total)
    }

    fn staging_file(&self) -> Result<PathBuf> {
        let name = format!("{}.part", uuid_like());
        Ok(self.staging.join(name))
    }

    fn ensure_parent(&self, target: &Path) -> Result<()> {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(())
    }

    fn commit(&self, tmp: &Path, target: &Path) -> Result<()> {
        match fs::rename(tmp, target) {
            Ok(()) => Ok(()),
            Err(e) => {
                // Another writer may have committed the same content first.
                if target.is_file() {
                    let _ = fs::remove_file(tmp);
                    Ok(())
                } else {
                    Err(anyhow!("commit blob: {}", e))
                }
            }
        }
    }
}

fn shard_name(shard: &Path) -> String {
    shard
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Validate and lowercase a digest string.
pub fn normalize_digest(digest: &str) -> Result<String> {
    let d = digest.trim().to_ascii_lowercase();
    if d.len() != 64 || !d.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(anyhow!("invalid digest: {}", digest));
    }
    Ok(d)
}

/// Digest of an in-memory buffer.
pub fn digest_of(data: &[u8]) -> BlobDigest {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// Cheap unique id for staging files (no external dependency, no clock
/// collisions within a process because of the counter).
fn uuid_like() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}-{:x}-{:x}", ts, std::process::id(), n)
}

/// Copy a blob into a destination directory, preserving the file name.
pub fn export_blob(
    store: &BlobStore,
    digest: &str,
    dest_dir: &Path,
    name: &str,
) -> Result<PathBuf> {
    fs::create_dir_all(dest_dir)?;
    let target = dest_dir.join(name);
    store.link_to(digest, &target)?;
    Ok(target)
}

/// Open a file for writing into the staging area.
pub fn create_staging_file(staging_dir: &Path) -> Result<(PathBuf, File)> {
    fs::create_dir_all(staging_dir)?;
    let path = staging_dir.join(format!("{}.part", uuid_like()));
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)?;
    Ok((path, file))
}
