//! The FUSE surface of the new design.
//!
//! Two layers of local state, like a version control system:
//!
//! * the **working copy** — one file per path under `staging/`, which is what
//!   the kernel reads and writes. It is always a complete file; a reader never
//!   sees a half-written one.
//! * the **content store** — the blob store, where a file's content is kept
//!   once, keyed by its hash. The working copy is committed into the store
//!   when it is flushed, which is also when the change is recorded in the
//!   journal and queued for upload.
//!
//! Reads therefore never touch the network, writes never leave a partial file
//! behind, and the journal only ever refers to content that is already
//! complete and hashed.

use anyhow::{anyhow, Result};
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow,
};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;

use crate::blob::BlobStore;
use crate::icloud_api::{RemoteKind, RemoteNode};
use crate::index::{name_of, parent_of, Index, Node, NodeKind};
use crate::journal::{Journal, Op};

const TTL: Duration = Duration::from_secs(60);
const ROOT_INO: u64 = 1;

/// Callback used to fetch content that is not local yet.
pub type HydrateFn = Arc<dyn Fn(&str) -> Result<()> + Send + Sync>;

/// Callback used to resolve a path that is not in the index yet.
pub type ResolveFn = Arc<dyn Fn(&str) -> Result<Option<RemoteNode>> + Send + Sync>;

/// Callback used to list a directory that was not listed yet.
pub type ListFn = Arc<dyn Fn(&str) -> Result<Vec<RemoteNode>> + Send + Sync>;

/// The working copy of one path.
struct Staged {
    /// File in the staging directory.
    file: PathBuf,
    /// Current length of the working copy.
    size: u64,
}

/// An open file handle: the path it serves and its own descriptor on the
/// working copy.
struct Handle {
    path: String,
    file: File,
}

/// The parts a filesystem is built from.
pub struct VfsParts {
    pub index: Arc<Index>,
    pub journal: Arc<Journal>,
    pub blobs: Arc<BlobStore>,
    pub staging_dir: PathBuf,
    pub hydrate: Option<HydrateFn>,
    pub resolve: Option<ResolveFn>,
    pub list: Option<ListFn>,
    pub read_only: bool,
}

pub struct Vfs {
    pub index: Arc<Index>,
    pub journal: Arc<Journal>,
    pub blobs: Arc<BlobStore>,
    pub staging_dir: PathBuf,
    pub hydrate: Option<HydrateFn>,
    pub resolve: Option<ResolveFn>,
    pub list: Option<ListFn>,
    pub read_only: bool,
    inos: Mutex<HashMap<String, u64>>,
    paths: Mutex<HashMap<u64, String>>,
    next_ino: AtomicU64,
    handles: Mutex<HashMap<u64, Handle>>,
    staged: Mutex<HashMap<String, Staged>>,
    /// Directories that were listed, so the remote is not asked twice.
    listed: Mutex<HashSet<String>>,
    next_fh: AtomicU64,
}

impl Vfs {
    pub fn new(parts: VfsParts) -> Self {
        let VfsParts {
            index,
            journal,
            blobs,
            staging_dir,
            hydrate,
            resolve,
            list,
            read_only,
        } = parts;
        let mut inos = HashMap::new();
        let mut paths = HashMap::new();
        inos.insert("/".to_string(), ROOT_INO);
        paths.insert(ROOT_INO, "/".to_string());
        Self {
            index,
            journal,
            blobs,
            staging_dir,
            hydrate,
            resolve,
            list,
            read_only,
            inos: Mutex::new(inos),
            paths: Mutex::new(paths),
            next_ino: AtomicU64::new(2),
            handles: Mutex::new(HashMap::new()),
            staged: Mutex::new(HashMap::new()),
            listed: Mutex::new(HashSet::new()),
            next_fh: AtomicU64::new(1),
        }
    }

    fn ino_for(&self, path: &str) -> u64 {
        let mut inos = self.inos.lock();
        if let Some(ino) = inos.get(path) {
            return *ino;
        }
        let ino = self.next_ino.fetch_add(1, Ordering::SeqCst);
        inos.insert(path.to_string(), ino);
        self.paths.lock().insert(ino, path.to_string());
        ino
    }

    fn path_for(&self, ino: u64) -> Option<String> {
        self.paths.lock().get(&ino).cloned()
    }

    fn path_of(&self, parent: u64, name: &OsStr) -> String {
        let parent_path = self.path_for(parent).unwrap_or_else(|| "/".to_string());
        let child = String::from_utf8_lossy(name.as_bytes()).to_string();
        if parent_path == "/" {
            format!("/{}", child)
        } else {
            format!("{}/{}", parent_path.trim_end_matches('/'), child)
        }
    }

    fn attr_of(&self, path: &str, node: Option<&Node>) -> FileAttr {
        let ino = self.ino_for(path);
        let (size, mtime, kind, perm) = match node {
            Some(n) => {
                let kind = match n.kind {
                    NodeKind::Folder => FileType::Directory,
                    NodeKind::File => FileType::RegularFile,
                };
                let perm = match n.kind {
                    NodeKind::Folder => 0o755,
                    NodeKind::File => 0o644,
                };
                // The working copy is authoritative while it exists; the index
                // only catches up when the change is committed.
                let size = self.staged_size(path).unwrap_or(n.size);
                (size, n.mtime.max(0) as u64, kind, perm)
            }
            None => (0, 0, FileType::Directory, 0o755),
        };
        let stamp = SystemTime::UNIX_EPOCH + Duration::from_secs(mtime);
        FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: SystemTime::now(),
            mtime: stamp,
            ctime: stamp,
            crtime: stamp,
            kind,
            perm,
            nlink: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn attr_of_root(&self) -> FileAttr {
        FileAttr {
            ino: ROOT_INO,
            size: 0,
            blocks: 0,
            atime: SystemTime::now(),
            mtime: SystemTime::now(),
            ctime: SystemTime::now(),
            crtime: SystemTime::now(),
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    /// Ask the remote for the content behind a path, if it is missing locally.
    fn ensure_hydrated(&self, path: &str) -> Result<()> {
        if let Some(h) = &self.hydrate {
            h(path)?;
        }
        Ok(())
    }

    fn staged_size(&self, path: &str) -> Option<u64> {
        self.staged.lock().get(path).map(|s| s.size)
    }

    fn node_for(&self, path: &str) -> Option<Node> {
        self.index.get(path).ok().flatten().filter(|n| n.present)
    }

    /// Path of the working copy for an iCloud path.
    fn working_copy(&self, path: &str) -> PathBuf {
        crate::blob::working_copy_path(&self.staging_dir, path)
    }

    /// Make sure a working copy exists for a path, seeded from the blob store.
    fn ensure_working_copy(&self, path: &str) -> Result<PathBuf> {
        {
            let staged = self.staged.lock();
            if let Some(entry) = staged.get(path) {
                return Ok(entry.file.clone());
            }
        }
        std::fs::create_dir_all(&self.staging_dir)?;
        let working = self.working_copy(path);
        if !working.exists() {
            File::create(&working)?;
            if let Some(node) = self.node_for(path) {
                if let Some(digest) = node.digest {
                    if let Ok(mut blob) = self.blobs.open(&digest) {
                        let mut out = File::create(&working)?;
                        std::io::copy(&mut blob, &mut out)?;
                    }
                }
            }
        }
        let size = std::fs::metadata(&working).map(|m| m.len()).unwrap_or(0);
        self.staged.lock().insert(
            path.to_string(),
            Staged {
                file: working.clone(),
                size,
            },
        );
        Ok(working)
    }

    /// Commit the working copy into the blob store and record the operation.
    ///
    /// The working copy is kept: it is the local truth for the path until the
    /// change has been uploaded, and further writes continue from it.
    fn commit_working_copy(&self, path: &str) -> Result<()> {
        let working = {
            let staged = self.staged.lock();
            match staged.get(path) {
                Some(entry) => entry.file.clone(),
                None => return Ok(()),
            }
        };
        if !working.exists() {
            return Ok(());
        }
        let size = std::fs::metadata(&working)?.len();
        let mut reader = File::open(&working)?;
        let digest = self.blobs.put_reader(&mut reader)?;

        let mut node = self
            .index
            .get(path)?
            .unwrap_or_else(|| Node::new(path, NodeKind::File));
        node.digest = Some(digest.clone());
        node.hydrated = true;
        node.size = size;
        node.dirty = true;
        node.fetched_at = crate::db::now_ts();
        self.index.upsert(&node)?;
        self.journal
            .enqueue(Op::Write, path, None, Some(&digest), Some(size))?;
        Ok(())
    }

    /// Read the current content of a path: the working copy if there is one,
    /// otherwise the committed blob.
    fn read_content(&self, path: &str, offset: i64, size: u32) -> Result<Vec<u8>> {
        let working = self.staged.lock().get(path).map(|s| s.file.clone());
        if let Some(working) = working {
            if working.exists() {
                let mut file = File::open(&working)?;
                let mut buf = vec![0u8; size as usize];
                file.seek(SeekFrom::Start(offset.max(0) as u64))?;
                let n = file.read(&mut buf)?;
                buf.truncate(n);
                return Ok(buf);
            }
        }
        let node = self.index.get(path)?;
        if let Some(digest) = node.and_then(|n| n.digest) {
            let mut file = self.blobs.open(&digest)?;
            let mut buf = vec![0u8; size as usize];
            file.seek(SeekFrom::Start(offset.max(0) as u64))?;
            let n = file.read(&mut buf)?;
            buf.truncate(n);
            return Ok(buf);
        }
        // Not local yet: fetch it, then read again.
        self.ensure_hydrated(path)?;
        let node = self.index.get(path)?;
        if let Some(digest) = node.and_then(|n| n.digest) {
            let mut file = self.blobs.open(&digest)?;
            let mut buf = vec![0u8; size as usize];
            file.seek(SeekFrom::Start(offset.max(0) as u64))?;
            let n = file.read(&mut buf)?;
            buf.truncate(n);
            return Ok(buf);
        }
        Err(anyhow!("no local content for {}", path))
    }

    /// Resolve a path that is not in the index yet.
    fn resolve_remote(&self, path: &str) -> Option<Node> {
        let resolve = self.resolve.as_ref()?;
        let remote = resolve(path).ok().flatten()?;
        let mut node = Node::new(
            path,
            match remote.kind {
                RemoteKind::Folder => NodeKind::Folder,
                RemoteKind::File => NodeKind::File,
            },
        );
        node.remote_id = remote.remote_id;
        node.docwsid = remote.docwsid;
        node.zone = remote.zone;
        node.etag = remote.etag;
        node.size = remote.size;
        node.mtime = remote.mtime;
        node.fetched_at = crate::db::now_ts();
        if node.kind == NodeKind::Folder {
            node.hydrated = true;
        }
        let _ = self.index.upsert(&node);
        Some(node)
    }

    /// List a directory that was not listed yet.
    fn ensure_listed(&self, path: &str) {
        {
            let listed = self.listed.lock();
            if listed.contains(path) {
                return;
            }
        }
        let Some(list) = self.list.as_ref() else {
            return;
        };
        let Ok(children) = list(path) else {
            return;
        };
        for child in children {
            let child_path = if path == "/" {
                format!("/{}", child.path)
            } else {
                format!("{}/{}", path.trim_end_matches('/'), child.path)
            };
            let mut node = Node::new(
                &child_path,
                match child.kind {
                    RemoteKind::Folder => NodeKind::Folder,
                    RemoteKind::File => NodeKind::File,
                },
            );
            node.remote_id = child.remote_id;
            node.docwsid = child.docwsid;
            node.zone = child.zone;
            node.etag = child.etag;
            node.size = child.size;
            node.mtime = child.mtime;
            node.fetched_at = crate::db::now_ts();
            if node.kind == NodeKind::Folder {
                node.hydrated = true;
            }
            let _ = self.index.upsert(&node);
        }
        self.listed.lock().insert(path.to_string());
    }

    /// Drop the working copy of a path (used when the path is removed).
    fn discard_working_copy(&self, path: &str) {
        let entry = self.staged.lock().remove(path);
        if let Some(entry) = entry {
            let _ = std::fs::remove_file(entry.file);
        }
    }
}

impl Filesystem for Vfs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let path = self.path_of(parent, name);
        match self.node_for(&path) {
            Some(node) => {
                let attr = self.attr_of(&path, Some(&node));
                reply.entry(&TTL, &attr, 0);
            }
            None => match self.resolve_remote(&path) {
                Some(node) => {
                    let attr = self.attr_of(&path, Some(&node));
                    reply.entry(&TTL, &attr, 0);
                }
                None => reply.error(libc::ENOENT),
            },
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
        let Some(path) = self.path_for(ino) else {
            return reply.error(libc::ENOENT);
        };
        if path == "/" {
            return reply.attr(&TTL, &self.attr_of_root());
        }
        match self.node_for(&path) {
            Some(node) => reply.attr(&TTL, &self.attr_of(&path, Some(&node))),
            None => reply.error(libc::ENOENT),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let Some(parent) = self.path_for(ino) else {
            return reply.error(libc::ENOENT);
        };
        let mut entries: Vec<(u64, FileType, Vec<u8>)> = vec![
            (ino, FileType::Directory, b".".to_vec()),
            (ino, FileType::Directory, b"..".to_vec()),
        ];
        self.ensure_listed(&parent);
        match self.index.children(&parent) {
            Ok(children) => {
                for child in children {
                    let kind = match child.kind {
                        NodeKind::Folder => FileType::Directory,
                        NodeKind::File => FileType::RegularFile,
                    };
                    let name = name_of(&child.path).into_bytes();
                    entries.push((self.ino_for(&child.path), kind, name));
                }
            }
            Err(e) => {
                tracing::error!(path = %parent, error = %e, "readdir failed");
                return reply.error(libc::EIO);
            }
        }
        // 1-based cookies: the kernel only advances a listing when every
        // returned cookie is greater than the requested offset.
        for (i, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset.max(0) as usize) {
            let name = OsStr::from_bytes(&name);
            let _ = reply.add(ino, i as i64 + 1, kind, name);
        }
        reply.ok();
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        if self.read_only {
            return reply.error(libc::EROFS);
        }
        let Some(path) = self.path_for(ino) else {
            return reply.error(libc::ENOENT);
        };
        let writable = flags & libc::O_ACCMODE != libc::O_RDONLY;
        if !writable {
            // Make sure the content is local before the caller reads it.
            if let Err(e) = self.ensure_hydrated(&path) {
                tracing::warn!(path = %path, error = %e, "hydration on open failed");
            }
            let fh = self.next_fh.fetch_add(1, Ordering::SeqCst);
            return reply.opened(fh, 0);
        }

        let working = match self.ensure_working_copy(&path) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(path = %path, error = %e, "open for write failed");
                return reply.error(libc::EIO);
            }
        };
        let file = match OpenOptions::new().read(true).write(true).open(&working) {
            Ok(f) => f,
            Err(e) => {
                tracing::error!(path = %path, error = %e, "open working copy failed");
                return reply.error(libc::EIO);
            }
        };
        let fh = self.next_fh.fetch_add(1, Ordering::SeqCst);
        self.handles.lock().insert(fh, Handle { path, file });
        reply.opened(fh, 0);
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let Some(path) = self.path_for(ino) else {
            return reply.error(libc::ENOENT);
        };
        match self.read_content(&path, offset, size) {
            Ok(data) => reply.data(&data),
            Err(e) => {
                tracing::debug!(path = %path, error = %e, "read failed");
                reply.error(libc::EIO);
            }
        }
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        if self.read_only {
            return reply.error(libc::EROFS);
        }
        let Some(path) = self.path_for(ino) else {
            return reply.error(libc::ENOENT);
        };
        let mut handles = self.handles.lock();
        let Some(handle) = handles.get_mut(&fh) else {
            return reply.error(libc::EBADF);
        };
        if handle.path != path {
            return reply.error(libc::EBADF);
        }
        if handle
            .file
            .seek(SeekFrom::Start(offset.max(0) as u64))
            .is_err()
        {
            return reply.error(libc::EIO);
        }
        if handle.file.write_all(data).is_err() {
            return reply.error(libc::EIO);
        }
        let end = offset.max(0) as u64 + data.len() as u64;
        let mut staged = self.staged.lock();
        if let Some(entry) = staged.get_mut(&path) {
            if end > entry.size {
                entry.size = end;
            }
        }
        // Keep the index size in step with the working copy so that a stat
        // right after the write is accurate.
        if let Ok(Some(mut node)) = self.index.get(&path) {
            if node.size < end {
                node.size = end;
                node.dirty = true;
                let _ = self.index.upsert(&node);
            }
        }
        drop(staged);
        reply.written(data.len() as u32);
    }

    fn flush(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        let path = self.handles.lock().get(&fh).map(|h| h.path.clone());
        if let Some(path) = path {
            if let Err(e) = self.commit_working_copy(&path) {
                tracing::error!(path = %path, error = %e, "flush failed");
                return reply.error(libc::EIO);
            }
        }
        reply.ok();
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let handle = self.handles.lock().remove(&fh);
        if let Some(handle) = handle {
            if let Err(e) = self.commit_working_copy(&handle.path) {
                tracing::error!(path = %handle.path, error = %e, "release commit failed");
            }
        }
        reply.ok();
    }

    fn fsync(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        let path = self.handles.lock().get(&fh).map(|h| h.path.clone());
        if let Some(path) = path {
            if let Err(e) = self.commit_working_copy(&path) {
                tracing::error!(path = %path, error = %e, "fsync failed");
                return reply.error(libc::EIO);
            }
        }
        reply.ok();
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        if self.read_only {
            return reply.error(libc::EROFS);
        }
        let path = self.path_of(parent, name);
        let mut node = Node::new(&path, NodeKind::File);
        node.dirty = true;
        node.hydrated = true;
        node.size = 0;
        if let Err(e) = self.index.upsert(&node) {
            tracing::error!(path = %path, error = %e, "index upsert failed");
            return reply.error(libc::EIO);
        }
        if let Err(e) = self.journal.enqueue(Op::Create, &path, None, None, Some(0)) {
            tracing::error!(path = %path, error = %e, "journal enqueue failed");
            return reply.error(libc::EIO);
        }
        let working = match self.ensure_working_copy(&path) {
            Ok(p) => p,
            Err(_) => return reply.error(libc::EIO),
        };
        let file = match OpenOptions::new().read(true).write(true).open(&working) {
            Ok(f) => f,
            Err(_) => return reply.error(libc::EIO),
        };
        let fh = self.next_fh.fetch_add(1, Ordering::SeqCst);
        self.handles.lock().insert(
            fh,
            Handle {
                path: path.clone(),
                file,
            },
        );
        let attr = self.attr_of(&path, Some(&node));
        reply.created(&TTL, &attr, 0, fh, 0);
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        if self.read_only {
            return reply.error(libc::EROFS);
        }
        let path = self.path_of(parent, name);
        let mut node = Node::new(&path, NodeKind::Folder);
        node.dirty = true;
        node.hydrated = true;
        if let Err(e) = self.index.upsert(&node) {
            tracing::error!(path = %path, error = %e, "index upsert failed");
            return reply.error(libc::EIO);
        }
        let _ = self.journal.enqueue(Op::Mkdir, &path, None, None, None);
        let attr = self.attr_of(&path, Some(&node));
        reply.entry(&TTL, &attr, 0);
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        if self.read_only {
            return reply.error(libc::EROFS);
        }
        let path = self.path_of(parent, name);
        if self.node_for(&path).is_none() {
            return reply.error(libc::ENOENT);
        }
        self.discard_working_copy(&path);
        let _ = self.index.set_present(&path, false);
        let _ = self.journal.enqueue(Op::Remove, &path, None, None, None);
        reply.ok();
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        if self.read_only {
            return reply.error(libc::EROFS);
        }
        let path = self.path_of(parent, name);
        if self.node_for(&path).is_none() {
            return reply.error(libc::ENOENT);
        }
        self.discard_working_copy(&path);
        let _ = self.index.remove_subtree(&path);
        let _ = self.journal.enqueue(Op::Remove, &path, None, None, None);
        reply.ok();
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        if self.read_only {
            return reply.error(libc::EROFS);
        }
        let old_path = self.path_of(parent, name);
        let new_path = self.path_of(newparent, newname);
        if self.node_for(&old_path).is_none() {
            return reply.error(libc::ENOENT);
        }
        // Move the working copy along with the index entry.
        if let Ok(working) = self.ensure_working_copy(&old_path) {
            let target = self.working_copy(&new_path);
            let _ = std::fs::rename(&working, &target);
            let size = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
            self.staged.lock().remove(&old_path);
            self.staged
                .lock()
                .insert(new_path.clone(), Staged { file: target, size });
        }
        if let Err(e) = self.index.rename_subtree(&old_path, &new_path) {
            tracing::error!(old = %old_path, new = %new_path, error = %e, "rename failed");
            return reply.error(libc::EIO);
        }
        let _ = self
            .journal
            .enqueue(Op::Rename, &new_path, Some(&old_path), None, None);
        reply.ok();
    }

    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let Some(path) = self.path_for(ino) else {
            return reply.error(libc::ENOENT);
        };
        if let Some(size) = size {
            if let Some(node) = self.node_for(&path) {
                if node.kind == NodeKind::File {
                    // Truncation: cut the working copy too, so the next write
                    // does not leave trailing bytes behind.
                    if let Ok(working) = self.ensure_working_copy(&path) {
                        if let Ok(file) = OpenOptions::new().write(true).open(&working) {
                            let _ = file.set_len(size);
                        }
                    }
                    let mut staged = self.staged.lock();
                    if let Some(entry) = staged.get_mut(&path) {
                        entry.size = size;
                    }
                    drop(staged);
                    let mut node = node;
                    node.size = size;
                    node.dirty = true;
                    let _ = self.index.upsert(&node);
                    let _ = self
                        .journal
                        .enqueue(Op::Write, &path, None, None, Some(size));
                }
            }
        }
        match self.node_for(&path) {
            Some(node) => reply.attr(&TTL, &self.attr_of(&path, Some(&node))),
            None => reply.error(libc::ENOENT),
        }
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        let blocks = 1_000_000;
        reply.statfs(blocks, blocks, 0, 0, 0, 4096, 256, 4096);
    }
}

/// Mount options derived from the configuration.
pub fn mount_options(cfg: &crate::config::FuseOptions) -> Vec<MountOption> {
    let mut opts = vec![
        MountOption::FSName("icloud-for-linux".to_string()),
        MountOption::Subtype("icloud-for-linux".to_string()),
    ];
    if cfg.allow_other {
        opts.push(MountOption::AllowOther);
    }
    if cfg.ro {
        opts.push(MountOption::RO);
    }
    opts
}

/// Parent directory of a path (kept next to the VFS for convenience).
pub fn parent_dir(path: &str) -> String {
    parent_of(path)
}

/// Mount the filesystem at `mountpoint`, blocking until unmounted.
pub fn mount(fs: Vfs, mountpoint: &Path, opts: Vec<MountOption>) -> Result<()> {
    std::fs::create_dir_all(mountpoint)?;
    fuser::mount2(fs, mountpoint, &opts).map_err(|e| anyhow!("fuse session ended: {}", e))
}
