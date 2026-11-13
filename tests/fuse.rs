//! End-to-end test of the FUSE surface.
//!
//! The filesystem is mounted on a temporary directory and the kernel-facing
//! operations are exercised: listing, creating a file, writing to it, reading
//! it back, and the deletion path. The test is skipped automatically when the
//! host has no `/dev/fuse` or no `fusermount3` helper.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use icloud_for_linux::blob::BlobStore;
use icloud_for_linux::db::Db;
use icloud_for_linux::index::Index;
use icloud_for_linux::journal::Journal;
use icloud_for_linux::vfs::{Vfs, VfsParts};

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

fn mount_fs(
    cache: &Path,
    mountpoint: &Path,
) -> Option<(Arc<Journal>, Arc<Index>, fuser::BackgroundSession)> {
    let db = Arc::new(Db::open(&cache.join("state.sqlite3")).unwrap());
    let blobs = Arc::new(BlobStore::new(cache).unwrap());
    let index = Arc::new(Index::new(db.clone()));
    let journal = Arc::new(Journal::new(db));
    let fs = Vfs::new(VfsParts {
        index: index.clone(),
        journal: journal.clone(),
        blobs,
        staging_dir: cache.join("staging"),
        hydrate: None,
        resolve: None,
        list: None,
        read_only: false,
    });
    let options = vec![
        fuser::MountOption::FSName("icloud-for-linux-test".to_string()),
        fuser::MountOption::Subtype("icloud-for-linux-test".to_string()),
    ];
    match fuser::spawn_mount2(fs, mountpoint, &options) {
        Ok(session) => Some((journal, index, session)),
        Err(e) => {
            eprintln!("skipping: could not mount FUSE filesystem ({e})");
            None
        }
    }
}

#[test]
fn mount_write_read_roundtrip() {
    if !fuse_available() {
        eprintln!("skipping: FUSE not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("cache");
    let mountpoint = tmp.path().join("mnt");
    fs::create_dir_all(&mountpoint).unwrap();

    let Some((journal, _index, session)) = mount_fs(&cache, &mountpoint) else {
        return;
    };

    // The root must be listable.
    fs::read_dir(&mountpoint).expect("root directory must be readable");

    // Create a file and write to it.
    let file_path = mountpoint.join("hello.txt");
    fs::write(&file_path, b"hello icloud").expect("write through the mount must succeed");

    // Read it back through the mount.
    assert_eq!(fs::read(&file_path).unwrap(), b"hello icloud");
    assert_eq!(
        fs::metadata(&file_path).unwrap().len(),
        12,
        "stat must report the real size"
    );

    // Overwriting an existing file truncates it first. The kernel flushes
    // before the first write on this path, so the write handle must survive
    // a flush.
    fs::write(&file_path, b"second version").unwrap();
    assert_eq!(fs::read(&file_path).unwrap(), b"second version");
    assert_eq!(
        fs::metadata(&file_path).unwrap().len(),
        14,
        "stat must report the size after the overwrite"
    );

    // Appending keeps the previous content.
    {
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(&file_path)
            .unwrap();
        f.write_all(b"!").unwrap();
    }
    assert_eq!(fs::read(&file_path).unwrap(), b"second version!");
    assert_eq!(
        fs::metadata(&file_path).unwrap().len(),
        15,
        "stat must report the size after the update"
    );

    // The content must be visible in a directory listing.
    let listed: Vec<String> = fs::read_dir(&mountpoint)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(listed.contains(&"hello.txt".to_string()));

    // Directories.
    fs::create_dir(mountpoint.join("subdir")).unwrap();
    let listed: Vec<String> = fs::read_dir(&mountpoint)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(listed.contains(&"subdir".to_string()));

    // Deleting removes the entry from the listing.
    fs::remove_file(&file_path).unwrap();
    let listed: Vec<String> = fs::read_dir(&mountpoint)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        !listed.contains(&"hello.txt".to_string()),
        "deleted file must disappear from the listing"
    );

    // The journal must have recorded the local mutations.
    let (pending, _, _) = journal.counts().unwrap();
    assert!(pending > 0, "local mutations must be journalled");

    drop(session);
}
