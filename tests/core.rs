//! Integration tests for the local-first core: config boundaries, sync state
//! database and the local mirror.

use std::fs;

use icloud_for_linux::config::{normalize_icloud_path, path_allowed};
use icloud_for_linux::mirror::LocalMirror;
use icloud_for_linux::state::{Entry, SyncState};

fn entry(path: &str, kind: &str, parent: &str) -> Entry {
    Entry {
        path: path.to_string(),
        entry_type: kind.to_string(),
        parent_path: parent.to_string(),
        remote_drivewsid: None,
        remote_docwsid: None,
        remote_etag: None,
        remote_zone: None,
        remote_shareid: None,
        size: 0,
        mtime: 0,
        hydrated: true,
        dirty: false,
        tombstone: false,
        local_sha256: None,
        last_synced_at: None,
        synced_path: Some(path.to_string()),
    }
}

#[test]
fn normalizes_paths() {
    assert_eq!(normalize_icloud_path("Downloads"), "/Downloads");
    assert_eq!(normalize_icloud_path("/Downloads/"), "/Downloads");
    assert_eq!(normalize_icloud_path("//Downloads//x//"), "/Downloads/x");
    assert_eq!(normalize_icloud_path("/"), "/");
}

#[test]
fn deny_list_wins_over_allow_list() {
    let sync = vec!["/Downloads".to_string()];
    let exclude = vec!["/Downloads/Large Archive".to_string()];

    assert!(path_allowed("/Downloads/file.txt", Some(&sync), &exclude));
    assert!(!path_allowed(
        "/Downloads/Large Archive/file.txt",
        Some(&sync),
        &exclude
    ));
    assert!(!path_allowed("/Documents/file.txt", Some(&sync), &exclude));
}

#[test]
fn empty_allow_list_means_unrestricted() {
    let sync: Vec<String> = Vec::new();
    assert!(path_allowed("/anything", Some(&sync), &[]));
    assert!(path_allowed("/anything", None, &[]));
}

#[test]
fn state_roundtrip_and_rename() {
    let tmp = tempfile::tempdir().unwrap();
    let state = SyncState::open(&tmp.path().join("state.sqlite3")).unwrap();

    state
        .upsert_entry(&entry("/Documents", "folder", "/"))
        .unwrap();
    state
        .upsert_entry(&entry("/Documents/a.txt", "file", "/Documents"))
        .unwrap();

    assert_eq!(state.count_entries().unwrap(), 2);
    let children = state.list_children("/Documents").unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].path, "/Documents/a.txt");

    state.rename_tree("/Documents", "/Docs").unwrap();
    assert!(state.get_entry("/Documents/a.txt").unwrap().is_none());
    assert!(state.get_entry("/Docs/a.txt").unwrap().is_some());

    state.remove_subtree("/Docs").unwrap();
    assert_eq!(state.count_entries().unwrap(), 0);
}

#[test]
fn mirror_read_write_and_traversal_guard() {
    let tmp = tempfile::tempdir().unwrap();
    let mirror = LocalMirror::new(tmp.path()).unwrap();

    mirror.ensure_dir("/Docs").unwrap();
    mirror.create_file("/Docs/a.txt").unwrap();
    let written = mirror.write("/Docs/a.txt", 0, b"hello").unwrap();
    assert_eq!(written, 5);

    let data = mirror.read("/Docs/a.txt", 0, 5).unwrap();
    assert_eq!(data, b"hello");

    let sha = mirror.file_sha256("/Docs/a.txt").unwrap();
    assert_eq!(sha.len(), 64);

    assert!(mirror.local_path("/Docs/../../etc/passwd").is_err());
}

#[test]
fn mirror_placeholder_creates_sized_file() {
    let tmp = tempfile::tempdir().unwrap();
    let mirror = LocalMirror::new(tmp.path()).unwrap();
    mirror.materialize_placeholder("/big.bin", 4096).unwrap();
    let local = mirror.local_path("/big.bin").unwrap();
    assert_eq!(fs::metadata(local).unwrap().len(), 4096);
}
