use anyhow::{anyhow, Result};
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow,
};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;

use crate::mirror::LocalMirror;
use crate::state::{Entry, SyncState};

const TTL: Duration = Duration::from_secs(60);
const ROOT_INO: u64 = 1;

/// Callback used to hydrate a file on demand before serving reads.
pub type HydrateFn = Arc<dyn Fn(&str) -> Result<()> + Send + Sync>;

pub struct ICloudFs {
    pub state: Arc<SyncState>,
    pub mirror: Arc<LocalMirror>,
    pub hydrate: Option<HydrateFn>,
    pub read_only: bool,
    inos: Mutex<HashMap<String, u64>>,
    paths: Mutex<HashMap<u64, String>>,
    next_ino: AtomicU64,
}

impl ICloudFs {
    pub fn new(
        state: Arc<SyncState>,
        mirror: Arc<LocalMirror>,
        hydrate: Option<HydrateFn>,
        read_only: bool,
    ) -> Self {
        let mut inos = HashMap::new();
        let mut paths = HashMap::new();
        inos.insert("/".to_string(), ROOT_INO);
        paths.insert(ROOT_INO, "/".to_string());
        Self {
            state,
            mirror,
            hydrate,
            read_only,
            inos: Mutex::new(inos),
            paths: Mutex::new(paths),
            next_ino: AtomicU64::new(2),
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

    fn attr_of(&self, path: &str, entry: Option<&Entry>) -> FileAttr {
        let ino = self.ino_for(path);
        let (size, mtime, kind, perm) = match entry {
            Some(e) => {
                let kind = if e.entry_type == "folder" {
                    FileType::Directory
                } else {
                    FileType::RegularFile
                };
                let perm = if e.entry_type == "folder" {
                    0o755
                } else {
                    0o644
                };
                let size = if e.entry_type == "folder" {
                    0
                } else {
                    // The mirror is the source of truth for a hydrated file: the
                    // index can lag behind a local write until the next sync
                    // pass, but the kernel must never observe a stale size.
                    self.local_size(path)
                        .unwrap_or_else(|| e.size.max(0) as u64)
                };
                (size, e.mtime.max(0) as u64, kind, perm)
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

    /// Attributes of the mount root. The root always exists, even when the
    /// index has not been populated yet — replying ENOENT for it would make
    /// the whole mountpoint disappear.
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

    fn ensure_hydrated(&self, path: &str) -> Result<()> {
        if let Some(h) = &self.hydrate {
            h(path)?;
        }
        Ok(())
    }

    /// Size of the local mirror file, if it is present on disk.
    fn local_size(&self, path: &str) -> Option<u64> {
        let local = self.mirror.local_path(path).ok()?;
        std::fs::metadata(local).ok().map(|m| m.len())
    }

    fn new_entry(&self, path: String, parent_path: String, kind: &str) -> Entry {
        Entry {
            path: path.clone(),
            entry_type: kind.to_string(),
            parent_path,
            remote_drivewsid: None,
            remote_docwsid: None,
            remote_etag: None,
            remote_zone: None,
            remote_shareid: None,
            size: 0,
            mtime: chrono::Utc::now().timestamp(),
            hydrated: true,
            dirty: true,
            tombstone: false,
            local_sha256: None,
            last_synced_at: None,
            synced_path: Some(path),
        }
    }
}

impl Filesystem for ICloudFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let path = self.path_of(parent, name);
        match self.state.get_entry(&path) {
            Ok(Some(entry)) if !entry.tombstone => {
                let attr = self.attr_of(&path, Some(&entry));
                reply.entry(&TTL, &attr, 0);
            }
            Ok(_) => reply.error(libc::ENOENT),
            Err(e) => {
                tracing::error!(path = %path, error = %e, "lookup failed");
                reply.error(libc::EIO);
            }
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
        let Some(path) = self.path_for(ino) else {
            return reply.error(libc::ENOENT);
        };
        match self.state.get_entry(&path) {
            Ok(Some(entry)) if !entry.tombstone => {
                reply.attr(&TTL, &self.attr_of(&path, Some(&entry)))
            }
            Ok(None) if path == "/" => reply.attr(&TTL, &self.attr_of_root()),
            Ok(_) => reply.error(libc::ENOENT),
            Err(e) => {
                tracing::error!(path = %path, error = %e, "getattr failed");
                reply.error(libc::EIO);
            }
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
        let Some(parent_path) = self.path_for(ino) else {
            return reply.error(libc::ENOENT);
        };
        let mut entries: Vec<(u64, FileType, Vec<u8>)> = Vec::new();
        entries.push((ino, FileType::Directory, b".".to_vec()));
        entries.push((ino, FileType::Directory, b"..".to_vec()));
        match self.state.list_children(&parent_path) {
            Ok(children) => {
                for child in children {
                    let kind = if child.entry_type == "folder" {
                        FileType::Directory
                    } else {
                        FileType::RegularFile
                    };
                    let name = child
                        .path
                        .rsplit('/')
                        .next()
                        .unwrap_or("")
                        .as_bytes()
                        .to_vec();
                    entries.push((self.ino_for(&child.path), kind, name));
                }
            }
            Err(e) => {
                tracing::error!(path = %parent_path, error = %e, "readdir failed");
                return reply.error(libc::EIO);
            }
        }
        // The kernel resumes a directory listing from the cookie of the last
        // entry it received, and only makes progress when every returned
        // cookie is strictly greater than the requested offset. Cookies are
        // therefore 1-based: entry `i` carries cookie `i + 1`.
        for (i, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset.max(0) as usize) {
            let name = OsStr::from_bytes(&name);
            let _ = reply.add(ino, i as i64 + 1, kind, name);
        }
        reply.ok();
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        if self.read_only {
            return reply.error(libc::EROFS);
        }
        let Some(path) = self.path_for(ino) else {
            return reply.error(libc::ENOENT);
        };
        if let Err(e) = self.ensure_hydrated(&path) {
            tracing::warn!(path = %path, error = %e, "hydration on open failed");
            return reply.error(libc::EIO);
        }
        reply.opened(0, 0);
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
        if let Err(e) = self.ensure_hydrated(&path) {
            tracing::warn!(path = %path, error = %e, "hydration before read failed");
            return reply.error(libc::EIO);
        }
        match self.mirror.read(&path, offset.max(0) as u64, size) {
            Ok(data) => reply.data(&data),
            Err(e) => {
                tracing::error!(path = %path, error = %e, "read failed");
                reply.error(libc::EIO);
            }
        }
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
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
        match self.mirror.write(&path, offset.max(0) as u64, data) {
            Ok(n) => {
                let size = self.local_size(&path).map(|s| s as i64);
                let _ = self.state.mark_dirty(&path, size, None);
                reply.written(n);
            }
            Err(e) => {
                tracing::error!(path = %path, error = %e, "write failed");
                reply.error(libc::EIO);
            }
        }
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
        let parent_path = self.path_for(parent).unwrap_or_else(|| "/".to_string());
        if let Err(e) = self.mirror.create_file(&path) {
            tracing::error!(path = %path, error = %e, "create failed");
            return reply.error(libc::EIO);
        }
        let entry = self.new_entry(path.clone(), parent_path, "file");
        if let Err(e) = self.state.upsert_entry(&entry) {
            tracing::error!(path = %path, error = %e, "state upsert failed");
            return reply.error(libc::EIO);
        }
        let attr = self.attr_of(&path, Some(&entry));
        reply.created(&TTL, &attr, 0, 0, 0);
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
        let parent_path = self.path_for(parent).unwrap_or_else(|| "/".to_string());
        if let Err(e) = self.mirror.ensure_dir(&path) {
            tracing::error!(path = %path, error = %e, "mkdir failed");
            return reply.error(libc::EIO);
        }
        let entry = self.new_entry(path.clone(), parent_path, "folder");
        let _ = self.state.upsert_entry(&entry);
        let attr = self.attr_of(&path, Some(&entry));
        reply.entry(&TTL, &attr, 0);
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        if self.read_only {
            return reply.error(libc::EROFS);
        }
        let path = self.path_of(parent, name);
        if let Err(_e) = self.mirror.remove_file(&path) {
            return reply.error(libc::EIO);
        }
        let _ = self.state.mark_tombstone(&path);
        reply.ok();
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        if self.read_only {
            return reply.error(libc::EROFS);
        }
        let path = self.path_of(parent, name);
        let _ = self.mirror.remove_tree(&path);
        let _ = self.state.remove_subtree(&path);
        let _ = self.state.mark_tombstone(&path);
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
        if let Err(e) = self.mirror.rename_path(&old_path, &new_path) {
            tracing::error!(old = %old_path, new = %new_path, error = %e, "rename failed");
            return reply.error(libc::EIO);
        }
        let _ = self.state.rename_tree(&old_path, &new_path);
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
        if let Some(sz) = size {
            if let Err(_e) = self.mirror.truncate(&path, sz) {
                return reply.error(libc::EIO);
            }
            let _ = self.state.mark_dirty(&path, Some(sz as i64), None);
        }
        match self.state.get_entry(&path) {
            Ok(entry) => reply.attr(&TTL, &self.attr_of(&path, entry.as_ref())),
            Err(_) => reply.error(libc::EIO),
        }
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        let blocks = 1_000_000;
        reply.statfs(blocks, blocks, 0, 0, 0, 4096, 256, 4096);
    }
}

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

/// Mount the filesystem at `mountpoint`, blocking until unmounted.
pub fn mount(fs: ICloudFs, mountpoint: &Path, opts: Vec<MountOption>) -> Result<()> {
    std::fs::create_dir_all(mountpoint)?;
    fuser::mount2(fs, mountpoint, &opts).map_err(|e| anyhow!("fuse session ended: {}", e))
}
