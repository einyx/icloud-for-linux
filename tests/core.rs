//! Tests for the local-first core: blob store, path index and journal.

use std::collections::HashSet;

use icloud_for_linux::blob::{digest_of, BlobStore};
use icloud_for_linux::config::{normalize_icloud_path, path_allowed, resolve_icloud_path};
use icloud_for_linux::db::Db;
use icloud_for_linux::index::{Index, Node, NodeKind};
use icloud_for_linux::journal::{Journal, Op};

// ───────────────────────────── blob store ─────────────────────────────

#[test]
fn blob_roundtrip_and_verification() {
    let tmp = tempfile::tempdir().unwrap();
    let store = BlobStore::new(tmp.path()).unwrap();

    let digest = store.put_bytes(b"hello icloud").unwrap();
    assert_eq!(digest.len(), 64);
    assert_eq!(digest, digest_of(b"hello icloud"));
    assert!(store.contains(&digest));
    assert_eq!(store.size(&digest).unwrap(), 12);
    assert_eq!(store.read_all(&digest).unwrap(), b"hello icloud");
    assert!(store.verify(&digest).unwrap());
}

#[test]
fn identical_content_is_stored_once() {
    let tmp = tempfile::tempdir().unwrap();
    let store = BlobStore::new(tmp.path()).unwrap();

    let a = store.put_bytes(b"same bytes").unwrap();
    let b = store.put_bytes(b"same bytes").unwrap();
    assert_eq!(a, b);
    assert_eq!(store.list().unwrap().len(), 1);
}

#[test]
fn blob_streaming_matches_buffered_digest() {
    let tmp = tempfile::tempdir().unwrap();
    let store = BlobStore::new(tmp.path()).unwrap();

    let payload = vec![7u8; 3 * 1024 * 1024];
    let streamed = store.put_reader(&payload[..]).unwrap();
    assert_eq!(streamed, digest_of(&payload));
    assert_eq!(store.size(&streamed).unwrap(), payload.len() as u64);
}

#[test]
fn blob_gc_keeps_referenced_content() {
    let tmp = tempfile::tempdir().unwrap();
    let store = BlobStore::new(tmp.path()).unwrap();

    let keep = store.put_bytes(b"keep").unwrap();
    let drop = store.put_bytes(b"drop").unwrap();

    let mut live: HashSet<String> = HashSet::new();
    live.insert(keep.clone());
    let removed = store.gc(&live).unwrap();

    assert_eq!(removed, 1);
    assert!(store.contains(&keep));
    assert!(!store.contains(&drop));
}

#[test]
fn blob_can_be_materialised_as_a_file() {
    let tmp = tempfile::tempdir().unwrap();
    let store = BlobStore::new(tmp.path()).unwrap();
    let digest = store.put_bytes(b"exported").unwrap();

    let target = tmp.path().join("out").join("file.txt");
    store.link_to(&digest, &target).unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"exported");
}

// ───────────────────────────── path index ─────────────────────────────

fn node(path: &str, kind: NodeKind) -> Node {
    let mut n = Node::new(path, kind);
    n.size = 10;
    n.mtime = 1_700_000_000;
    n.etag = Some(format!("etag-{}", path));
    n.remote_id = Some(format!("id-{}", path));
    n
}

#[test]
fn index_stores_and_lists_children() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("state.sqlite3")).unwrap();
    let index = Index::new(std::sync::Arc::new(db));

    index.upsert(&node("/Documents", NodeKind::Folder)).unwrap();
    index
        .upsert(&node("/Documents/a.txt", NodeKind::File))
        .unwrap();
    index
        .upsert(&node("/Documents/b.txt", NodeKind::File))
        .unwrap();
    index.upsert(&node("/Pictures", NodeKind::Folder)).unwrap();

    let children = index.children("/Documents").unwrap();
    assert_eq!(children.len(), 2);
    assert!(children.iter().all(|c| c.path.starts_with("/Documents/")));

    let fetched = index.get("/Documents/a.txt").unwrap().unwrap();
    assert_eq!(fetched.kind, NodeKind::File);
    assert_eq!(fetched.size, 10);
}

#[test]
fn index_hides_removed_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("state.sqlite3")).unwrap();
    let index = Index::new(std::sync::Arc::new(db));

    index.upsert(&node("/a.txt", NodeKind::File)).unwrap();
    index.set_present("/a.txt", false).unwrap();

    assert!(!index.get("/a.txt").unwrap().unwrap().present);
    assert!(index.children("/").unwrap().is_empty());
}

#[test]
fn index_renames_subtrees() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("state.sqlite3")).unwrap();
    let index = Index::new(std::sync::Arc::new(db));

    index.upsert(&node("/dir", NodeKind::Folder)).unwrap();
    index
        .upsert(&node("/dir/file.txt", NodeKind::File))
        .unwrap();

    index.rename_subtree("/dir", "/renamed").unwrap();

    assert!(index.get("/dir/file.txt").unwrap().is_none());
    let moved = index.get("/renamed/file.txt").unwrap().unwrap();
    assert_eq!(moved.path, "/renamed/file.txt");
    assert_eq!(index.children("/renamed").unwrap().len(), 1);
}

#[test]
fn index_reports_stale_entries_only() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("state.sqlite3")).unwrap();
    let index = Index::new(std::sync::Arc::new(db));

    let mut fresh = node("/fresh.txt", NodeKind::File);
    fresh.fetched_at = icloud_for_linux::db::now_ts();
    index.upsert(&fresh).unwrap();

    let mut old = node("/old.txt", NodeKind::File);
    old.fetched_at = 0;
    index.upsert(&old).unwrap();

    let stale = index.stale_entries(300, 100).unwrap();
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].path, "/old.txt");
}

// ─────────────────────────────── journal ───────────────────────────────

#[test]
fn journal_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("state.sqlite3")).unwrap();
    let journal = Journal::new(std::sync::Arc::new(db));

    let first = journal
        .enqueue(Op::Write, "/a.txt", None, Some("deadbeef"), Some(3))
        .unwrap();
    let second = journal
        .enqueue(Op::Write, "/a.txt", None, Some("deadbeef"), Some(3))
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(journal.pending().unwrap().len(), 1);
}

#[test]
fn journal_orders_content_before_rename_and_delete() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("state.sqlite3")).unwrap();
    let journal = Journal::new(std::sync::Arc::new(db));

    journal
        .enqueue(Op::Remove, "/a.txt", None, None, None)
        .unwrap();
    journal
        .enqueue(Op::Rename, "/a.txt", Some("/old.txt"), None, None)
        .unwrap();
    journal
        .enqueue(Op::Write, "/a.txt", None, Some("cafe"), Some(1))
        .unwrap();

    let ops = journal.pending().unwrap();
    assert_eq!(ops[0].op, Op::Write);
    assert_eq!(ops[1].op, Op::Rename);
    assert_eq!(ops[2].op, Op::Remove);
}

#[test]
fn journal_supersedes_older_content_versions() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("state.sqlite3")).unwrap();
    let journal = Journal::new(std::sync::Arc::new(db));

    journal
        .enqueue(Op::Write, "/a.txt", None, Some("aaa"), Some(1))
        .unwrap();
    journal
        .enqueue(Op::Write, "/a.txt", None, Some("bbb"), Some(2))
        .unwrap();
    journal
        .enqueue(Op::Write, "/a.txt", None, Some("ccc"), Some(3))
        .unwrap();

    let pending = journal.pending().unwrap();
    assert_eq!(pending.len(), 1, "only the newest version stays pending");
    assert_eq!(pending[0].digest.as_deref(), Some("ccc"));
}

#[test]
fn journal_tracks_failures_and_conflicts() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("state.sqlite3")).unwrap();
    let journal = Journal::new(std::sync::Arc::new(db));

    let id = journal
        .enqueue(Op::Write, "/a.txt", None, Some("cafe"), Some(1))
        .unwrap();
    journal
        .mark_state(id, "failed", Some("network down"))
        .unwrap();

    let (pending, failed, conflicts) = journal.counts().unwrap();
    assert_eq!(pending, 1);
    assert_eq!(failed, 1);
    assert_eq!(conflicts, 0);

    journal
        .record_conflict("/a.txt", Some("cafe"), Some("etag-remote"))
        .unwrap();
    let conflicts = journal.conflicts().unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].local_digest.as_deref(), Some("cafe"));
}

// ─────────────────────────────── config ───────────────────────────────

#[test]
fn normalizes_paths() {
    assert_eq!(normalize_icloud_path("Downloads"), "/Downloads");
    assert_eq!(normalize_icloud_path("/Downloads/"), "/Downloads");
    assert_eq!(normalize_icloud_path("//Downloads//x//"), "/Downloads/x");
    assert_eq!(normalize_icloud_path("/"), "/");
}

#[test]
fn refuses_paths_above_the_root() {
    assert!(resolve_icloud_path("/a/../../etc").is_err());
    assert_eq!(resolve_icloud_path("/a/../b").unwrap(), "/b");
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
