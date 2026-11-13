# Architecture

`icloud-for-linux` is an independent Rust implementation of an iCloud Drive
filesystem for Linux. This document describes the design and, for every
subsystem, why it looks the way it does.

## Design goals

1. **Local-first.** Reads must never wait for the network. Everything the user
   touches is available locally.
2. **No full crawl.** The upstream Python driver scans the entire drive before
   it mounts. On a large drive that costs minutes, a lot of memory and a lot of
   API calls — to build an index the user mostly never reads. Here the index is
   filled on demand, and refreshes are proportional to activity.
3. **Content is addressed, not paths.** Storing one copy per path wastes space
   and makes integrity checks impossible. Content is stored once, keyed by its
   hash.
4. **Every local mutation is recorded before it is applied.** A crash must
   never lose a change, and replaying the log must never apply a change twice.
5. **A conflict must not destroy data.** The losing version of a diverged path
   is kept, not overwritten.

## Layers

```
                    kernel VFS
                        │
                ┌───────▼────────┐
                │  VFS (fuser)   │  src/vfs.rs
                └───────┬────────┘
                        │
        ┌───────────────┼────────────────┐
        │               │                │
┌───────▼──────┐ ┌──────▼───────┐ ┌──────▼──────┐
│ working copy │ │ path index   │ │  journal    │
│ (staging/)   │ │ (SQLite)     │ │ (SQLite)    │
└───────┬──────┘ └──────┬───────┘ └──────┬──────┘
        │               │                │
        └───────────────┼────────────────┘
                        │
              ┌─────────▼─────────┐
              │  blob store (CAS) │  src/blob.rs
              └─────────┬─────────┘
                        │
              ┌─────────▼─────────┐
              │  iCloud client    │  src/icloud_api.rs
              └───────────────────┘
```

### Working copy and content store

The two layers of local state mirror how a version control system works:

| Layer | Where | Purpose |
| --- | --- | --- |
| **Working copy** | `staging/<hash>.work` | The file the kernel reads and writes. One file per path, always complete. |
| **Content store** | `blobs/ab/cdef…` | The content of every committed file, keyed by its SHA-256. Deduplicated, verifiable. |

A write goes to the working copy. When the file is flushed, the working copy is
hashed and committed to the content store, the index is updated with the new
digest and size, and the change is recorded in the journal.

Consequences:

- The kernel never sees a partially written file.
- The same content at two paths is stored once.
- `icloudctl verify` can re-hash every blob and detect corruption.
- The remote only ever receives complete, hashed content.

### Path index

`src/index.rs` — a table of `path → metadata`, filled lazily.

| Column | Meaning |
| --- | --- |
| `path`, `parent` | Location in the drive |
| `kind` | `file` or `folder` |
| `size`, `mtime`, `etag`, `remote_id` | Remote metadata |
| `digest`, `hydrated` | Local content state |
| `dirty` | Local change not yet uploaded |
| `present` | Deleted locally, awaiting upload |
| `fetched_at` | When the entry was last validated against the remote |

`fetched_at` is what makes the refresh a *delta* refresh: the sync engine asks
the remote only about the entries that are older than `index_ttl_seconds`,
instead of walking the whole drive again. A path that was never touched never
costs anything.

### Journal

`src/journal.rs` — a write-ahead log of local mutations.

| Property | How it is achieved |
| --- | --- |
| Durability | The operation is written before the change is queued for upload. |
| Idempotency | Every operation has a key (`op:path:target:digest`). Replaying it cannot duplicate work. |
| Ordering | Content operations are applied before the rename or delete that depends on them. |
| Convergence | A newer content version supersedes the pending older ones for the same path. |
| History | Conflicts are recorded with the digest of the version that lost. |

### Sync engine

`src/sync.rs` — two loops that can also be driven manually.

* **Drain** takes the pending operations and applies them to the remote:
  upload the blob, create the directory, delete the node, move the node. A
  failure marks the operation as failed and leaves it in the journal for the
  next pass.
* **Refresh** re-validates the metadata that went stale. When an entry changed
  remotely *and* changed locally, the local version is preserved as a blob and
  recorded as a conflict — the remote version stays authoritative for the path.

### The remote boundary

`src/icloud_api.rs` defines a path-oriented trait (`stat_node`, `list_children`,
`download_file`, `upload_file`, `mkdir`, `delete`, `rename_move`). Nothing above
that trait knows about HTTP, cookies or 2FA, and nothing above it changes when
the protocol implementation does.

The implementation behind it lives in `src/icloud/`:

| Module | Responsibility |
| --- | --- |
| `transport.rs` | The HTTP boundary (`Transport` trait), the cookie jar, and the `reqwest` backed implementation |
| `srp.rs` | Apple's SRP-6a variant: PBKDF2 derived password, no username in `x` |
| `auth.rs` | The sign-in flow, two-factor authentication, trust, and the persisted session |
| `drive.rs` | The drive service: path resolution, listing, download, upload, rename, move, trash |
| `client.rs` | The adapter that implements the client interface |

Because the HTTP layer is a trait, the whole protocol is tested against
scripted responses: no account and no network are needed to verify the flow.
The SRP implementation is checked against a server side computation, so the
algebra is verified rather than a recorded value.

## The control loop

While the filesystem is mounted, one thread (`src/control.rs`) keeps the local
state in step with the remote:

* `SIGUSR1` raises a flag that the loop picks up; it then runs a single pass
  and writes a completion marker, which is what `icloudctl sync` waits for.
* with `auto_sync` the journal is drained and stale metadata is refreshed on
  the configured schedule.
* with `hydration: background` a small pool of workers downloads the files the
  index knows about. Each file is claimed by exactly one worker, so nothing is
  downloaded twice.

The signal handler itself only stores a flag — no allocation, no I/O in signal
context.

## Crash safety

The working copies are mutable, so a crash can leave one half written. They are
registered in the database, and the recovery pass runs before the mount is
served:

| State of a working copy | What the recovery does |
| --- | --- |
| The path is clean and the size matches | nothing — the content is already in the store |
| The path is clean and the size differs | restore it from the committed blob |
| The path has an unuploaded change | commit it to the store, so the change survives |
| The path is unknown to the index | drop it |

The pass compares sizes and only hashes what really changed. That is what keeps
the start fast and the memory flat on a drive with tens of gigabytes of
content.

## The synchronisation boundary

The boundary is enforced by the mount, not only by the sync engine: a path
outside `sync_paths` (or inside `exclude_paths`) cannot be created, written,
truncated, renamed or deleted. The kernel gets `EACCES` and nothing is queued
for upload.

A rename is checked as a whole: moving a folder that contains an excluded path
is refused, because the move would take the excluded content out of the
boundary.

## Inodes

The inode number belongs to the file, not to the path. When a file is renamed
its inode moves with it, and when two files are exchanged their inodes are
swapped. The kernel keeps the inode of a renamed file together with its cached
pages — with the number pinned to the path, the kernel could end up serving
the cached pages of a different file.

## Concurrency

- The FUSE session runs on its own thread.
- The index and the journal share one SQLite connection behind a mutex, in WAL
  mode, so the sync workers never block the kernel path for long.
- The blob store is lock-free: blobs are immutable and named by their digest.
  Writers stage into `staging/` and rename into place, so a reader can never
  observe a partial blob.

## The synchronisation boundary

Two rules, evaluated in this order, shared by the VFS, the hydrator and the
uploader (`config::path_allowed`):

1. `exclude_paths` (deny-list) — a matching prefix blocks hydration and mutation.
2. `sync_paths` (allow-list) — when set, only matching prefixes are hydrated or mutable.

`config::resolve_icloud_path` refuses paths that try to climb above the drive
root.

## Provenance

This is an independent implementation. The design decisions above were taken
from first principles (and from how git, rsync, restic and Syncthing solve the
same problems), not transcribed from another codebase. The protocol client that
will sit behind `ICloudClient` is written against Apple's public web service
behaviour.

The upstream Python project that inspired the goal — a local-first iCloud Drive
filesystem for Linux — is credited in `README.md` and `NOTICE`. No upstream
source code is included in this repository.
