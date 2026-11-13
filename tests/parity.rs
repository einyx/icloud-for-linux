//! Parity with the behaviour of the project this one is modelled on.
//!
//! These are the scenarios that matter, not the code: the synchronisation
//! boundary, the path policy, the treatment of app libraries, the migration
//! of an older index and the recovery after a crash.

use std::sync::Arc;

use icloud_for_linux::config::{normalize_icloud_path, path_allowed, resolve_icloud_path};
use icloud_for_linux::db::Db;
use icloud_for_linux::index::{Index, Node, NodeKind};

// ─────────────────────────── path policy ───────────────────────────

#[test]
fn trailing_slashes_are_normalized_for_allow_and_exclude_paths() {
    let sync = vec![normalize_icloud_path("/allowed/")];
    let exclude = vec![normalize_icloud_path("/allowed/excluded/")];

    assert!(path_allowed("/allowed/file.txt", Some(&sync), &exclude));
    assert!(!path_allowed(
        "/allowed/excluded/file.txt",
        Some(&sync),
        &exclude
    ));
}

#[test]
fn dot_segments_cannot_escape_an_allowed_path() {
    let sync = vec![normalize_icloud_path("/allowed")];
    assert!(!path_allowed("/allowed/../outside.txt", Some(&sync), &[]));
}

#[test]
fn a_path_above_the_root_is_refused() {
    assert!(resolve_icloud_path("/allowed/../../etc/passwd").is_err());
}

#[test]
fn an_empty_allow_list_keeps_everything_unrestricted() {
    let empty: Vec<String> = Vec::new();
    assert!(path_allowed("/anything", Some(&empty), &[]));
    assert!(path_allowed("/anything", None, &[]));
}

#[test]
fn the_deny_list_wins_over_the_allow_list() {
    let sync = vec!["/Downloads".to_string()];
    let exclude = vec!["/Downloads/Large".to_string()];
    assert!(path_allowed("/Downloads/a.txt", Some(&sync), &exclude));
    assert!(!path_allowed(
        "/Downloads/Large/a.txt",
        Some(&sync),
        &exclude
    ));
}

// ─────────────────────────── app libraries ───────────────────────────

#[test]
fn an_app_library_is_a_directory() {
    use icloud_for_linux::icloud::drive::DriveNode;
    let node = DriveNode {
        drivewsid: Some("FOLDER::com.apple.CloudDocs::app".to_string()),
        docwsid: Some("doc-app".to_string()),
        name: "App Library".to_string(),
        node_type: "app_library".to_string(),
        etag: None,
        zone: Some("com.apple.CloudDocs".to_string()),
        size: 0,
        mtime: 0,
        parent_id: None,
    };
    assert!(node.is_folder());

    // The same holds for a shared folder.
    let shared = DriveNode {
        node_type: "shared_folder".to_string(),
        ..node
    };
    assert!(shared.is_folder());
}

// ─────────────────────────── the migration ──────────────────────────

#[test]
fn an_index_from_an_earlier_version_gets_the_new_columns() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.sqlite3");

    // An index of the previous version: no docwsid and no zone.
    {
        let db = Db::open(&path).unwrap();
        let conn = db.lock();
        conn.execute(
            "CREATE TABLE IF NOT EXISTS nodes (
                path TEXT PRIMARY KEY, parent TEXT NOT NULL, kind TEXT NOT NULL,
                size INTEGER NOT NULL DEFAULT 0, mtime INTEGER NOT NULL DEFAULT 0,
                etag TEXT, remote_id TEXT, digest TEXT, hydrated INTEGER NOT NULL DEFAULT 0,
                dirty INTEGER NOT NULL DEFAULT 0, present INTEGER NOT NULL DEFAULT 1,
                fetched_at INTEGER NOT NULL DEFAULT 0)",
            [],
        )
        .unwrap();
    }

    // Opening it again adds the columns.
    let db = Db::open(&path).unwrap();
    let index = Index::new(Arc::new(db));
    let mut node = Node::new("/a.txt", NodeKind::File);
    node.docwsid = Some("doc-1".to_string());
    node.zone = Some("com.apple.CloudDocs".to_string());
    index.upsert(&node).unwrap();

    let stored = index.get("/a.txt").unwrap().unwrap();
    assert_eq!(stored.docwsid.as_deref(), Some("doc-1"));
    assert_eq!(stored.zone.as_deref(), Some("com.apple.CloudDocs"));
}

// ─────────────────────── offline behaviour ───────────────────────

#[test]
fn a_hydrated_file_is_readable_without_a_session() {
    use icloud_for_linux::blob::BlobStore;
    use icloud_for_linux::journal::Journal;
    use icloud_for_linux::working::WorkingCopies;

    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("cache");
    let db = Arc::new(Db::open(&cache.join("state.sqlite3")).unwrap());
    let blobs = Arc::new(BlobStore::new(&cache).unwrap());
    let index = Arc::new(Index::new(db.clone()));
    let _journal = Arc::new(Journal::new(db.clone()));
    let working = Arc::new(WorkingCopies::new(db.clone()));

    // A file whose content is in the store.
    let digest = blobs.put_bytes(b"cached content").unwrap();
    let mut node = Node::new("/cached.txt", NodeKind::File);
    node.digest = Some(digest.clone());
    node.hydrated = true;
    node.size = 13;
    index.upsert(&node).unwrap();

    // Reading it needs neither a session nor the network.
    assert_eq!(blobs.read_all(&digest).unwrap(), b"cached content");
    let stored = index.get("/cached.txt").unwrap().unwrap();
    assert!(stored.hydrated);
    assert_eq!(stored.size, 13);

    // The working copy of a path is known, even when there is no session.
    let file = cache.join("staging").join("cached.work");
    working.register("/cached.txt", &file).unwrap();
    assert_eq!(working.list().unwrap().len(), 1);
}
