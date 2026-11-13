//! Hardening: the behaviour around crashes, large files and the kernel
//! operations that the everyday file tools use.

use std::sync::Arc;

use icloud_for_linux::blob::{working_copy_path, BlobStore};
use icloud_for_linux::db::Db;
use icloud_for_linux::index::{Index, Node, NodeKind};
use icloud_for_linux::journal::Journal;
use icloud_for_linux::working::WorkingCopies;

/// A store with an index, a journal, a blob store and the working copies.
struct Store {
    _tmp: tempfile::TempDir,
    index: Arc<Index>,
    journal: Arc<Journal>,
    blobs: Arc<BlobStore>,
    working: Arc<WorkingCopies>,
    staging: std::path::PathBuf,
}

fn store() -> Store {
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("cache");
    let db = Arc::new(Db::open(&cache.join("state.sqlite3")).unwrap());
    let blobs = Arc::new(BlobStore::new(&cache).unwrap());
    let index = Arc::new(Index::new(db.clone()));
    let journal = Arc::new(Journal::new(db.clone()));
    let working = Arc::new(WorkingCopies::new(db.clone()));
    Store {
        staging: cache.join("staging"),
        _tmp: tmp,
        index,
        journal,
        blobs,
        working,
    }
}

#[test]
fn a_partial_working_copy_is_restored_from_the_store() {
    let store = store();
    std::fs::create_dir_all(&store.staging).unwrap();

    // The committed content.
    let digest = store.blobs.put_bytes(b"the complete content").unwrap();
    let mut node = Node::new("/file.txt", NodeKind::File);
    node.digest = Some(digest.clone());
    node.hydrated = true;
    node.size = 19;
    store.index.upsert(&node).unwrap();

    // A working copy that a crash left half written.
    let working = working_copy_path(&store.staging, "/file.txt");
    std::fs::write(&working, b"the half").unwrap();
    store.working.register("/file.txt", &working).unwrap();

    let report = store_recover(&store);
    assert_eq!(report.restored, 1);
    assert_eq!(report.hashed_bytes, 0, "a clean file must not be read");
    assert_eq!(std::fs::read(&working).unwrap(), b"the complete content");
}

#[test]
fn a_local_change_that_was_not_uploaded_is_committed() {
    let store = store();
    std::fs::create_dir_all(&store.staging).unwrap();

    // A path with a local change: the index knows it and it is dirty.
    let mut node = Node::new("/file.txt", NodeKind::File);
    node.dirty = true;
    node.size = 8;
    store.index.upsert(&node).unwrap();

    let working = working_copy_path(&store.staging, "/file.txt");
    std::fs::write(&working, b"changed!").unwrap();
    store.working.register("/file.txt", &working).unwrap();

    let report = store_recover(&store);
    assert_eq!(report.committed, 1);
    assert_eq!(report.hashed_bytes, 8);

    // The change is now a blob and the index refers to it.
    let stored = store.index.get("/file.txt").unwrap().unwrap();
    let digest = stored.digest.expect("the change was committed");
    assert_eq!(store.blobs.read_all(&digest).unwrap(), b"changed!");
}

#[test]
fn the_startup_pass_does_not_read_the_content() {
    let store = store();
    std::fs::create_dir_all(&store.staging).unwrap();

    // A large file that is clean: the pass must not hash it.
    let payload = vec![0u8; 4 * 1024 * 1024];
    let digest = store.blobs.put_bytes(&payload).unwrap();
    let mut node = Node::new("/big.bin", NodeKind::File);
    node.digest = Some(digest.clone());
    node.hydrated = true;
    node.size = payload.len() as u64;
    store.index.upsert(&node).unwrap();

    let working = working_copy_path(&store.staging, "/big.bin");
    std::fs::write(&working, &payload).unwrap();
    store.working.register("/big.bin", &working).unwrap();

    let report = store_recover(&store);
    assert_eq!(report.hashed_bytes, 0);
    assert_eq!(report.restored, 0);
    assert_eq!(report.committed, 0);
}

#[test]
fn a_working_copy_of_an_unknown_path_is_dropped() {
    let store = store();
    std::fs::create_dir_all(&store.staging).unwrap();

    let working = working_copy_path(&store.staging, "/gone.txt");
    std::fs::write(&working, b"orphan").unwrap();
    store.working.register("/gone.txt", &working).unwrap();

    let report = store_recover(&store);
    assert_eq!(report.removed, 1);
    assert!(!working.exists());
    assert!(store.working.list().unwrap().is_empty());
}

#[test]
fn a_missing_blob_is_reported() {
    let store = store();
    let mut node = Node::new("/file.txt", NodeKind::File);
    node.digest = Some("0".repeat(64));
    node.hydrated = true;
    store.index.upsert(&node).unwrap();

    let dangling = dangling_references(&store);
    assert_eq!(dangling, vec!["/file.txt".to_string()]);
}

#[test]
fn an_inconsistent_working_copy_is_reported() {
    let store = store();
    std::fs::create_dir_all(&store.staging).unwrap();

    let digest = store.blobs.put_bytes(b"complete").unwrap();
    let mut node = Node::new("/file.txt", NodeKind::File);
    node.digest = Some(digest);
    node.hydrated = true;
    node.size = 8;
    store.index.upsert(&node).unwrap();

    let working = working_copy_path(&store.staging, "/file.txt");
    std::fs::write(&working, b"half").unwrap();
    store.working.register("/file.txt", &working).unwrap();

    let inconsistent = inconsistent_working_copies(&store);
    assert_eq!(inconsistent, vec!["/file.txt".to_string()]);
}

#[test]
fn the_journal_keeps_the_pending_operations_of_a_path() {
    let store = store();
    store
        .journal
        .enqueue(
            icloud_for_linux::journal::Op::Write,
            "/a.txt",
            None,
            Some("aa"),
            Some(1),
        )
        .unwrap();
    store
        .journal
        .enqueue(
            icloud_for_linux::journal::Op::Write,
            "/a.txt",
            None,
            Some("bb"),
            Some(2),
        )
        .unwrap();

    // Only the newest version is pending: the older one would be uploaded
    // for nothing.
    let pending = store.journal.pending().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].digest.as_deref(), Some("bb"));
}

// The helpers below drive the engine through the public interface; they are
// thin wrappers so the test reads like the behaviour it checks.

fn store_recover(store: &Store) -> icloud_for_linux::sync::RecoveryReport {
    use icloud_for_linux::icloud_api::ICloudClient;

    struct Offline;
    impl ICloudClient for Offline {
        fn authenticate(&mut self, _interactive: bool) -> anyhow::Result<()> {
            Ok(())
        }
        fn is_authenticated(&self) -> bool {
            false
        }
        fn stat_node(
            &self,
            _path: &str,
        ) -> anyhow::Result<Option<icloud_for_linux::icloud_api::RemoteNode>> {
            Ok(None)
        }
        fn list_children(
            &self,
            _path: &str,
        ) -> anyhow::Result<Vec<icloud_for_linux::icloud_api::RemoteNode>> {
            Ok(Vec::new())
        }
        fn download_file(
            &self,
            _node: &icloud_for_linux::icloud_api::RemoteNode,
            _dest: &std::path::Path,
        ) -> anyhow::Result<u64> {
            Ok(0)
        }
        fn upload_file(
            &self,
            _local: &std::path::Path,
            _parent: &str,
            _name: &str,
        ) -> anyhow::Result<icloud_for_linux::icloud_api::RemoteNode> {
            anyhow::bail!("offline")
        }
        fn mkdir(
            &self,
            _parent: &str,
            _name: &str,
        ) -> anyhow::Result<icloud_for_linux::icloud_api::RemoteNode> {
            anyhow::bail!("offline")
        }
        fn delete(&self, _remote_id: &str) -> anyhow::Result<()> {
            anyhow::bail!("offline")
        }
        fn rename_move(
            &self,
            _source: &str,
            _parent: &str,
            _name: &str,
        ) -> anyhow::Result<icloud_for_linux::icloud_api::RemoteNode> {
            anyhow::bail!("offline")
        }
    }

    let engine = icloud_for_linux::sync::SyncEngine::new(
        icloud_for_linux::config::Config::default(),
        store.index.clone(),
        store.journal.clone(),
        store.blobs.clone(),
        store.working.clone(),
        Box::new(Offline),
    );
    engine.recover().unwrap()
}

fn dangling_references(store: &Store) -> Vec<String> {
    let mut out = Vec::new();
    for node in store.index.all().unwrap() {
        if let Some(digest) = node.digest {
            if !store.blobs.contains(&digest) {
                out.push(node.path);
            }
        }
    }
    out
}

fn inconsistent_working_copies(store: &Store) -> Vec<String> {
    let mut out = Vec::new();
    for entry in store.working.list().unwrap() {
        let Ok(metadata) = std::fs::metadata(&entry.file) else {
            out.push(entry.path);
            continue;
        };
        let Ok(Some(node)) = store.index.get(&entry.path) else {
            out.push(entry.path);
            continue;
        };
        if !node.dirty && metadata.len() != node.size {
            out.push(entry.path);
        }
    }
    out
}
