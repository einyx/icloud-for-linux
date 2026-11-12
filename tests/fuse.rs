//! End-to-end test of the FUSE surface.
//!
//! The test mounts the filesystem on a temporary directory and exercises the
//! operations that the kernel relies on: readdir cookies, create/write/read,
//! mkdir/rmdir and unlink. It is skipped automatically when the host has no
//! `/dev/fuse` or no `fusermount3` helper (some CI sandboxes).

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use icloud_for_linux::fuse_fs::ICloudFs;
use icloud_for_linux::mirror::LocalMirror;
use icloud_for_linux::state::SyncState;

fn fuse_available() -> bool {
    Path::new("/dev/fuse").exists() && which("fusermount3").is_some()
        || which("fusermount").is_some()
}

fn which(cmd: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|p| p.join(cmd))
        .find(|c| c.is_file())
}

#[test]
fn mount_read_write_roundtrip() {
    if !fuse_available() {
        eprintln!("skipping: FUSE not available in this environment");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("cache");
    let mountpoint = tmp.path().join("mnt");
    fs::create_dir_all(&mountpoint).unwrap();

    let mirror = Arc::new(LocalMirror::new(&cache).unwrap());
    let state = Arc::new(SyncState::open(&cache.join("state.sqlite3")).unwrap());
    let fs_impl = ICloudFs::new(state, mirror, None, false);

    let options = vec![
        fuser::MountOption::FSName("icloud-for-linux-test".to_string()),
        fuser::MountOption::Subtype("icloud-for-linux-test".to_string()),
    ];

    let session = match fuser::spawn_mount2(fs_impl, &mountpoint, &options) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skipping: could not mount FUSE filesystem ({e})");
            return;
        }
    };

    // Wait for the mount to become visible.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && fs::read_dir(&mountpoint).is_err() {
        std::thread::sleep(Duration::from_millis(50));
    }

    // The root must exist and be listable. `readdir(3)` filters out "." and
    // "..", so only the presence of a readable, non-hanging listing matters.
    let root = fs::read_dir(&mountpoint).expect("root directory must be readable");
    let names: Vec<String> = root
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(!names.contains(&"..".to_string()));

    // Create and write a file through the mount.
    let file_path = mountpoint.join("hello.txt");
    fs::write(&file_path, b"hello icloud").expect("write through the mount must succeed");
    assert_eq!(fs::read(&file_path).unwrap(), b"hello icloud");
    assert_eq!(
        fs::metadata(&file_path).unwrap().len(),
        12,
        "stat must report the real size"
    );

    // The file must be visible in a directory listing.
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
    fs::remove_dir(mountpoint.join("subdir")).unwrap();

    // Deletion removes the entry from the listing.
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

    // Dropping the session unmounts the filesystem.
    drop(session);
}
