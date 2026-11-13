//! Tests for the background work of milestone M2: the one-shot sync over
//! `SIGUSR1`, the background hydration and the conflict export.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use icloud_for_linux::blob::BlobStore;
use icloud_for_linux::config::Config;
use icloud_for_linux::db::Db;
use icloud_for_linux::icloud_api::{ICloudClient, RemoteKind, RemoteNode};
use icloud_for_linux::index::{Index, Node, NodeKind};
use icloud_for_linux::journal::Journal;
use icloud_for_linux::sync::SyncEngine;

/// A client that serves a fixed set of nodes from memory.
struct FakeClient {
    nodes: Mutex<HashMap<String, RemoteNode>>,
    content: Mutex<HashMap<String, Vec<u8>>>,
    uploads: Mutex<Vec<String>>,
    authenticated: bool,
}

impl FakeClient {
    fn new() -> Self {
        Self {
            nodes: Mutex::new(HashMap::new()),
            content: Mutex::new(HashMap::new()),
            uploads: Mutex::new(Vec::new()),
            authenticated: true,
        }
    }

    fn insert(&self, path: &str, content: &[u8], etag: &str) {
        // The service reports the name of a node, not its whole path.
        let name = path.rsplit('/').next().unwrap_or(path).to_string();
        self.nodes.lock().unwrap().insert(
            path.to_string(),
            RemoteNode {
                path: name,
                kind: RemoteKind::File,
                remote_id: Some(format!("id-{path}")),
                docwsid: Some(format!("doc-{path}")),
                zone: Some("com.apple.CloudDocs".to_string()),
                etag: Some(etag.to_string()),
                size: content.len() as u64,
                mtime: 1_700_000_000,
            },
        );
        self.content
            .lock()
            .unwrap()
            .insert(path.to_string(), content.to_vec());
    }

    fn set_etag(&self, path: &str, etag: &str) {
        if let Some(node) = self.nodes.lock().unwrap().get_mut(path) {
            node.etag = Some(etag.to_string());
        }
    }
}

impl ICloudClient for FakeClient {
    fn authenticate(&mut self, _interactive: bool) -> anyhow::Result<()> {
        Ok(())
    }

    fn is_authenticated(&self) -> bool {
        self.authenticated
    }

    fn stat_node(&self, path: &str) -> anyhow::Result<Option<RemoteNode>> {
        Ok(self.nodes.lock().unwrap().get(path).cloned())
    }

    fn list_children(&self, path: &str) -> anyhow::Result<Vec<RemoteNode>> {
        let prefix = if path == "/" {
            "/".to_string()
        } else {
            format!("{}/", path.trim_end_matches('/'))
        };
        let nodes = self.nodes.lock().unwrap();
        let mut out = Vec::new();
        for (key, node) in nodes.iter() {
            if let Some(rest) = key.strip_prefix(&prefix) {
                if !rest.is_empty() && !rest.contains('/') {
                    out.push(node.clone());
                }
            }
        }
        Ok(out)
    }

    fn download_file(&self, node: &RemoteNode, dest: &Path) -> anyhow::Result<u64> {
        let content = self
            .content
            .lock()
            .unwrap()
            .get(&node.path)
            .cloned()
            .unwrap_or_default();
        std::fs::write(dest, &content)?;
        Ok(content.len() as u64)
    }

    fn upload_file(
        &self,
        local_path: &Path,
        remote_parent: &str,
        name: &str,
    ) -> anyhow::Result<RemoteNode> {
        let size = std::fs::metadata(local_path)?.len();
        let path = if remote_parent == "/" {
            format!("/{name}")
        } else {
            format!("{}/{name}", remote_parent.trim_end_matches('/'))
        };
        self.uploads.lock().unwrap().push(path);
        Ok(RemoteNode {
            path: name.to_string(),
            kind: RemoteKind::File,
            remote_id: Some(format!("id-{name}")),
            docwsid: Some(format!("doc-{name}")),
            zone: Some("com.apple.CloudDocs".to_string()),
            etag: Some("etag-new".to_string()),
            size,
            mtime: 1_700_000_000,
        })
    }

    fn mkdir(&self, _remote_parent: &str, name: &str) -> anyhow::Result<RemoteNode> {
        Ok(RemoteNode {
            path: name.to_string(),
            kind: RemoteKind::Folder,
            remote_id: Some(format!("id-{name}")),
            docwsid: Some(format!("doc-{name}")),
            zone: Some("com.apple.CloudDocs".to_string()),
            etag: None,
            size: 0,
            mtime: 1_700_000_000,
        })
    }

    fn delete(&self, remote_id: &str) -> anyhow::Result<()> {
        self.nodes.lock().unwrap().remove(remote_id);
        Ok(())
    }

    fn rename_move(
        &self,
        source_path: &str,
        remote_parent: &str,
        name: &str,
    ) -> anyhow::Result<RemoteNode> {
        let mut nodes = self.nodes.lock().unwrap();
        let node = nodes
            .remove(source_path)
            .ok_or_else(|| anyhow::anyhow!("no such node"))?;
        let mut moved = node;
        moved.path = name.to_string();
        nodes.insert(format!("{remote_parent}/{name}"), moved.clone());
        Ok(moved)
    }
}

struct Harness {
    _tmp: tempfile::TempDir,
    engine: SyncEngine,
    client: Arc<FakeClient>,
    index: Arc<Index>,
    journal: Arc<Journal>,
    blobs: Arc<BlobStore>,
    staging: PathBuf,
}

fn harness() -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("cache");
    let db = Arc::new(Db::open(&cache.join("state.sqlite3")).unwrap());
    let blobs = Arc::new(BlobStore::new(&cache).unwrap());
    let index = Arc::new(Index::new(db.clone()));
    let journal = Arc::new(Journal::new(db));
    let client = Arc::new(FakeClient::new());

    let config = Config {
        cache_dir: cache.display().to_string(),
        auto_sync: false,
        ..Config::default()
    };

    let engine = SyncEngine::new(
        config,
        index.clone(),
        journal.clone(),
        blobs.clone(),
        Box::new(FakeClientAdapter(client.clone())),
    );
    Harness {
        staging: cache.join("staging"),
        _tmp: tmp,
        engine,
        client,
        index,
        journal,
        blobs,
    }
}

/// The engine takes the client by value, so the fake is handed over through a
/// shared handle.
struct FakeClientAdapter(Arc<FakeClient>);

impl ICloudClient for FakeClientAdapter {
    fn authenticate(&mut self, _interactive: bool) -> anyhow::Result<()> {
        Ok(())
    }
    fn is_authenticated(&self) -> bool {
        true
    }
    fn stat_node(&self, path: &str) -> anyhow::Result<Option<RemoteNode>> {
        self.0.stat_node(path)
    }
    fn list_children(&self, path: &str) -> anyhow::Result<Vec<RemoteNode>> {
        self.0.list_children(path)
    }
    fn download_file(&self, node: &RemoteNode, dest: &Path) -> anyhow::Result<u64> {
        self.0.download_file(node, dest)
    }
    fn upload_file(
        &self,
        local_path: &Path,
        remote_parent: &str,
        name: &str,
    ) -> anyhow::Result<RemoteNode> {
        self.0.upload_file(local_path, remote_parent, name)
    }
    fn mkdir(&self, remote_parent: &str, name: &str) -> anyhow::Result<RemoteNode> {
        self.0.mkdir(remote_parent, name)
    }
    fn delete(&self, remote_id: &str) -> anyhow::Result<()> {
        self.0.delete(remote_id)
    }
    fn rename_move(
        &self,
        source_path: &str,
        remote_parent: &str,
        name: &str,
    ) -> anyhow::Result<RemoteNode> {
        self.0.rename_move(source_path, remote_parent, name)
    }
}

#[test]
fn a_conflicted_path_keeps_its_local_version() {
    let harness = harness();
    harness
        .client
        .insert("/notes.txt", b"remote version", "etag-remote");

    // A local change: the working copy differs and the index is dirty.
    let working = icloud_for_linux::blob::working_copy_path(&harness.staging, "/notes.txt");
    std::fs::create_dir_all(harness.staging.clone()).unwrap();
    std::fs::write(&working, b"local version").unwrap();

    let mut node = Node::new("/notes.txt", NodeKind::File);
    node.dirty = true;
    node.etag = Some("etag-local".to_string());
    node.remote_id = Some("id-/notes.txt".to_string());
    node.fetched_at = 0;
    harness.index.upsert(&node).unwrap();

    // The remote changed as well.
    harness.client.set_etag("/notes.txt", "etag-remote");
    let refreshed = harness.engine.refresh_stale().unwrap();
    assert_eq!(refreshed, 1);

    // The conflict is recorded and the local version is preserved.
    let conflicts = harness.engine.conflicts().unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].path, "/notes.txt");
    let digest = conflicts[0].local_digest.clone().expect("a local blob");
    assert!(harness.blobs.contains(&digest));
    assert_eq!(harness.blobs.read_all(&digest).unwrap(), b"local version");

    // The blob survives the garbage collector, because the conflict refers
    // to it.
    let live = harness.engine.live_digests().unwrap();
    assert!(live.contains(&digest));
    let removed = harness.blobs.gc(&live).unwrap();
    assert_eq!(removed, 0);
    assert!(harness.blobs.contains(&digest));
}

#[test]
fn a_conflict_can_be_exported() {
    let harness = harness();
    harness
        .client
        .insert("/notes.txt", b"remote", "etag-remote");

    let working = icloud_for_linux::blob::working_copy_path(&harness.staging, "/notes.txt");
    std::fs::create_dir_all(harness.staging.clone()).unwrap();
    std::fs::write(&working, b"local version").unwrap();

    let mut node = Node::new("/notes.txt", NodeKind::File);
    node.dirty = true;
    node.etag = Some("etag-local".to_string());
    node.fetched_at = 0;
    harness.index.upsert(&node).unwrap();
    harness.client.set_etag("/notes.txt", "etag-remote");

    harness.engine.refresh_stale().unwrap();

    let conflicts = harness.engine.conflicts().unwrap();
    let digest = conflicts[0].local_digest.clone().unwrap();

    let dest = harness._tmp.path().join("export");
    std::fs::create_dir_all(&dest).unwrap();
    let target = dest.join("notes.txt.local-conflict");
    harness.blobs.link_to(&digest, &target).unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"local version");
}

#[test]
fn background_hydration_downloads_the_pending_files() {
    let harness = harness();
    harness
        .client
        .insert("/movie.mov", b"the whole movie", "etag-1");

    // The index knows the file, but its content is not local.
    let mut node = Node::new("/movie.mov", NodeKind::File);
    node.remote_id = Some("id-/movie.mov".to_string());
    node.docwsid = Some("doc-/movie.mov".to_string());
    node.zone = Some("com.apple.CloudDocs".to_string());
    node.size = 15;
    node.hydrated = false;
    harness.index.upsert(&node).unwrap();

    // A hydrator claims the file and downloads it.
    let targets = harness.engine.claim_hydration_targets(4).unwrap();
    assert_eq!(targets, vec!["/movie.mov".to_string()]);

    // The same file is not claimed twice.
    assert!(harness
        .engine
        .claim_hydration_targets(4)
        .unwrap()
        .is_empty());

    harness.engine.hydrate_one("/movie.mov").unwrap();

    let entry = harness.index.get("/movie.mov").unwrap().unwrap();
    assert!(entry.hydrated);
    let digest = entry.digest.expect("a digest");
    assert_eq!(harness.blobs.read_all(&digest).unwrap(), b"the whole movie");

    // After the download nothing is left to hydrate.
    assert!(harness
        .engine
        .claim_hydration_targets(4)
        .unwrap()
        .is_empty());
}

#[test]
fn the_journal_is_drained_to_the_remote() {
    let harness = harness();
    let content = b"uploaded content".to_vec();
    let digest = harness.blobs.put_bytes(&content).unwrap();

    harness
        .journal
        .enqueue(
            icloud_for_linux::journal::Op::Write,
            "/upload.bin",
            None,
            Some(&digest),
            Some(content.len() as u64),
        )
        .unwrap();

    let applied = harness.engine.drain_journal().unwrap();
    assert_eq!(applied, 1);
    assert_eq!(
        harness.client.uploads.lock().unwrap().as_slice(),
        ["/upload.bin".to_string()]
    );
    assert!(harness.journal.pending().unwrap().is_empty());
}

#[test]
fn the_sync_flag_is_raised_by_the_signal() {
    icloud_for_linux::signal::install_sync_handler();
    assert!(!icloud_for_linux::signal::take_sync_request());

    // Raising the signal on this process must reach the handler.
    unsafe {
        libc::raise(libc::SIGUSR1);
    }
    // Give the handler a moment.
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(icloud_for_linux::signal::take_sync_request());
    assert!(!icloud_for_linux::signal::take_sync_request());
}

#[test]
fn the_completion_marker_is_written_and_read() {
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join("sync_done");

    icloud_for_linux::control::write_marker(&marker);
    assert!(marker.exists());

    icloud_for_linux::control::wait_for_marker(&marker, std::time::Duration::from_secs(5))
        .expect("the marker must be accepted");

    // A stale marker is not accepted.
    std::fs::write(&marker, "1").unwrap();
    let result =
        icloud_for_linux::control::wait_for_marker(&marker, std::time::Duration::from_millis(500));
    assert!(result.is_err());
}

#[test]
fn a_subtree_is_listed_and_hydrated() {
    let harness = harness();
    harness
        .client
        .insert("/Downloads/movie.mov", b"the movie", "etag-1");
    harness
        .client
        .insert("/Downloads/notes.txt", b"the notes", "etag-2");
    harness
        .client
        .insert("/Pictures/photo.jpg", b"the photo", "etag-3");

    let (total, done) = harness.engine.hydrate_subtree("/Downloads").unwrap();
    assert_eq!(total, 2);
    assert_eq!(done, 2);

    // The files of the subtree are local now.
    for path in ["/Downloads/movie.mov", "/Downloads/notes.txt"] {
        let entry = harness.index.get(path).unwrap().unwrap();
        assert!(entry.hydrated, "{path} must be hydrated");
        let digest = entry.digest.unwrap();
        assert!(!harness.blobs.read_all(&digest).unwrap().is_empty());
    }

    // The rest of the drive was not touched.
    let untouched = harness.index.get("/Pictures/photo.jpg").unwrap();
    assert!(untouched.is_none());
}
