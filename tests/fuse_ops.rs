//! The kernel operations an everyday file tool uses.
//!
//! The filesystem is mounted for real, so these tests go through the same
//! path as `cp`, `tar`, `rsync` and the editors do.

use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use icloud_for_linux::blob::BlobStore;
use icloud_for_linux::db::Db;
use icloud_for_linux::index::{Index, Node, NodeKind};
use icloud_for_linux::journal::Journal;
use icloud_for_linux::vfs::{Vfs, VfsParts};
use icloud_for_linux::working::WorkingCopies;
use icloud_for_linux::xattr::Xattrs;

fn which(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|p| p.join(cmd))
        .find(|c| c.is_file())
}

fn fuse_available() -> bool {
    Path::new("/dev/fuse").exists()
        && (which("fusermount3").is_some() || which("fusermount").is_some())
}

struct Mounted {
    _tmp: tempfile::TempDir,
    index: Arc<Index>,
    journal: Arc<Journal>,
    blobs: Arc<BlobStore>,
    mountpoint: PathBuf,
    /// Held to keep the mount alive for the duration of the test.
    _session: fuser::BackgroundSession,
}

fn mount(boundary: Option<(Vec<String>, Vec<String>)>) -> Option<Mounted> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_test_writer()
        .try_init();
    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let tmp = tempfile::tempdir().unwrap();
    // The store lives under the target directory: a temporary file system
    // cannot hold sparse files, and the fallocate test needs one. Every test
    // gets its own store, so they can run in parallel.
    let cache = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "store-{}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&cache).unwrap();
    let mountpoint = tmp.path().join("mnt");
    fs::create_dir_all(&mountpoint).unwrap();

    let db = Arc::new(Db::open(&cache.join("state.sqlite3")).unwrap());
    let blobs = Arc::new(BlobStore::new(&cache).unwrap());
    let index = Arc::new(Index::new(db.clone()));
    let journal = Arc::new(Journal::new(db.clone()));
    let working = Arc::new(WorkingCopies::new(db.clone()));
    let xattrs = Arc::new(Xattrs::new(db.clone()));

    let boundary = boundary.map(|(sync, exclude)| {
        let callback: icloud_for_linux::vfs::BoundaryFn = Arc::new(move |path: &str| {
            icloud_for_linux::config::path_allowed(path, Some(&sync), &exclude)
        });
        callback
    });

    let fs = Vfs::new(VfsParts {
        index: index.clone(),
        journal: journal.clone(),
        working: working.clone(),
        xattrs: xattrs.clone(),
        blobs: blobs.clone(),
        staging_dir: cache.join("staging"),
        hydrate: None,
        resolve: None,
        list: None,
        boundary,
        read_only: false,
    });

    let options = vec![
        fuser::MountOption::FSName("icloud-for-linux-test".to_string()),
        fuser::MountOption::Subtype("icloud-for-linux-test".to_string()),
    ];
    match fuser::spawn_mount2(fs, &mountpoint, &options) {
        Ok(session) => Some(Mounted {
            mountpoint,
            index,
            journal,
            blobs,
            _session: session,
            _tmp: tmp,
        }),
        Err(e) => {
            eprintln!("skipping: could not mount the FUSE filesystem ({e})");
            None
        }
    }
}

#[test]
fn fsync_commits_the_content() {
    if !fuse_available() {
        eprintln!("skipping: FUSE not available");
        return;
    }
    let Some(mounted) = mount(None) else {
        return;
    };

    let path = mounted.mountpoint.join("file.txt");
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .unwrap();
    use std::io::Write;
    let mut file = file;
    file.write_all(b"durable content").unwrap();
    file.sync_all().unwrap();
    drop(file);

    // The content was committed to the store and journalled.
    let entry = mounted.index.get("/file.txt").unwrap().unwrap();
    let digest = entry.digest.expect("the content was committed");
    assert_eq!(mounted.blobs.read_all(&digest).unwrap(), b"durable content");
    assert!(!mounted.journal.pending().unwrap().is_empty());
}

#[test]
fn fallocate_reserves_space() {
    if !fuse_available() {
        eprintln!("skipping: FUSE not available");
        return;
    }
    let Some(mounted) = mount(None) else {
        return;
    };

    let path = mounted.mountpoint.join("sparse.bin");
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .unwrap();
    let result = unsafe { libc::fallocate(file.as_raw_fd(), 0, 0, 8 * 1024 * 1024) };
    assert_eq!(
        result,
        0,
        "fallocate must succeed: {}",
        std::io::Error::last_os_error()
    );
    drop(file);

    // The space is reserved: the file grew to the requested size.
    let metadata = fs::metadata(&path).unwrap();
    assert_eq!(metadata.len(), 8 * 1024 * 1024);
}

#[test]
fn a_file_larger_than_four_gigabytes_is_reported() {
    if !fuse_available() {
        eprintln!("skipping: FUSE not available");
        return;
    }
    let Some(mounted) = mount(None) else {
        return;
    };

    // A file of five gigabytes: the size has to survive the index and reach
    // the kernel unchanged.
    let mut node =
        icloud_for_linux::index::Node::new("/huge.bin", icloud_for_linux::index::NodeKind::File);
    node.size = 5 * 1024 * 1024 * 1024;
    node.hydrated = true;
    mounted.index.upsert(&node).unwrap();

    let path = mounted.mountpoint.join("huge.bin");
    let metadata = fs::metadata(&path).unwrap();
    assert_eq!(metadata.len(), 5 * 1024 * 1024 * 1024);
}

#[test]
fn extended_attributes_are_stored() {
    if !fuse_available() {
        eprintln!("skipping: FUSE not available");
        return;
    }
    let Some(mounted) = mount(None) else {
        return;
    };

    let path = mounted.mountpoint.join("file.txt");
    fs::write(&path, b"content").unwrap();

    let path_c = std::ffi::CString::new(path.to_string_lossy().as_bytes()).unwrap();
    let name = std::ffi::CString::new("user.test").unwrap();
    let value = b"a value";
    let result = unsafe {
        libc::setxattr(
            path_c.as_ptr(),
            name.as_ptr(),
            value.as_ptr() as *const libc::c_void,
            value.len(),
            0,
        )
    };
    assert_eq!(
        result,
        0,
        "setxattr must succeed: {}",
        std::io::Error::last_os_error()
    );

    let mut buffer = [0u8; 64];
    let size = unsafe {
        libc::getxattr(
            path_c.as_ptr(),
            name.as_ptr(),
            buffer.as_mut_ptr() as *mut libc::c_void,
            buffer.len(),
        )
    };
    assert_eq!(size as usize, value.len());
    assert_eq!(&buffer[..value.len()], value);

    // The attribute is listed and can be removed again.
    let listed = unsafe { libc::listxattr(path_c.as_ptr(), std::ptr::null_mut(), 0) };
    assert!(listed > 0);

    let result = unsafe { libc::removexattr(path_c.as_ptr(), name.as_ptr()) };
    assert_eq!(result, 0);
}

#[test]
fn rename_without_replace_refuses_an_existing_target() {
    if !fuse_available() {
        eprintln!("skipping: FUSE not available");
        return;
    }
    let Some(mounted) = mount(None) else {
        return;
    };

    fs::write(mounted.mountpoint.join("a.txt"), b"a").unwrap();
    fs::write(mounted.mountpoint.join("b.txt"), b"b").unwrap();

    let old = std::ffi::CString::new(
        mounted
            .mountpoint
            .join("a.txt")
            .to_string_lossy()
            .as_bytes(),
    )
    .unwrap();
    let new = std::ffi::CString::new(
        mounted
            .mountpoint
            .join("b.txt")
            .to_string_lossy()
            .as_bytes(),
    )
    .unwrap();
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            old.as_ptr(),
            libc::AT_FDCWD,
            new.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    assert_eq!(result, -1, "the rename must be refused");
    let error = std::io::Error::last_os_error();
    assert_eq!(error.raw_os_error(), Some(libc::EEXIST));
}

#[test]
fn rename_with_exchange_swaps_the_two_paths() {
    if !fuse_available() {
        eprintln!("skipping: FUSE not available");
        return;
    }
    let Some(mounted) = mount(None) else {
        return;
    };

    fs::write(mounted.mountpoint.join("a.txt"), b"a").unwrap();
    fs::write(mounted.mountpoint.join("b.txt"), b"b").unwrap();

    let old = std::ffi::CString::new(
        mounted
            .mountpoint
            .join("a.txt")
            .to_string_lossy()
            .as_bytes(),
    )
    .unwrap();
    let new = std::ffi::CString::new(
        mounted
            .mountpoint
            .join("b.txt")
            .to_string_lossy()
            .as_bytes(),
    )
    .unwrap();
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            old.as_ptr(),
            libc::AT_FDCWD,
            new.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    assert_eq!(
        result,
        0,
        "the exchange must succeed: {}",
        std::io::Error::last_os_error()
    );

    assert_eq!(fs::read(mounted.mountpoint.join("a.txt")).unwrap(), b"b");
    assert_eq!(fs::read(mounted.mountpoint.join("b.txt")).unwrap(), b"a");
}

#[test]
fn the_synchronisation_boundary_is_enforced_by_the_mount() {
    if !fuse_available() {
        eprintln!("skipping: FUSE not available");
        return;
    }
    // Only /Downloads may be changed.
    let Some(mounted) = mount(Some((vec!["/Downloads".to_string()], Vec::new()))) else {
        return;
    };

    // The allowed path works.
    let allowed = mounted.mountpoint.join("Downloads");
    fs::create_dir_all(&allowed).unwrap();
    fs::write(allowed.join("ok.txt"), b"fine").unwrap();
    assert!(allowed.join("ok.txt").exists());

    // Everything else is refused, and nothing is queued.
    let outside = mounted.mountpoint.join("outside.txt");
    let error = fs::write(&outside, b"nope").unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::EACCES));
    assert!(!outside.exists());

    // The refused write was not queued either.
    let pending = mounted.journal.pending().unwrap();
    assert!(
        pending.iter().all(|op| op.path.starts_with("/Downloads")),
        "no operation outside the boundary may be queued: {:?}",
        pending
    );
}

#[test]
fn a_directory_with_an_excluded_descendant_cannot_be_moved() {
    if !fuse_available() {
        eprintln!("skipping: FUSE not available");
        return;
    }
    // /work may be changed, but not the excluded /work/secret below it.
    let Some(mounted) = mount(Some((
        vec!["/work".to_string()],
        vec!["/work/secret".to_string()],
    ))) else {
        return;
    };

    // The index knows about the excluded file.
    let mut secret = Node::new("/work/secret", NodeKind::File);
    secret.hydrated = true;
    mounted.index.upsert(&secret).unwrap();
    let mut work = Node::new("/work", NodeKind::Folder);
    work.hydrated = true;
    mounted.index.upsert(&work).unwrap();

    let from = mounted.mountpoint.join("work");
    let to = mounted.mountpoint.join("moved");
    let result = std::fs::rename(&from, &to);
    assert!(result.is_err(), "the rename must be refused");
    assert_eq!(
        result.unwrap_err().raw_os_error(),
        Some(libc::EACCES),
        "the excluded descendant must block the rename"
    );
}
