# Architecture

A Rust port of the Python `icloud-linux` FUSE driver. The design keeps the same
mental model — *local-first, persistent cache, sync engine on top* — while moving
the concurrency model from threads + `ThreadPoolExecutor` to explicit state machine +
worker pool.

## Module map

| Rust module | Python original | Responsibility |
| --- | --- | --- |
| `src/config.rs` | `config.example.yaml` handling | Config parsing (YAML), path normalisation, sync boundary rules |
| `src/state.rs` | `driver.py` → `SyncState` | SQLite state DB: entries, pending ops, dirty/tombstone tracking |
| `src/mirror.rs` | `driver.py` → `LocalMirror` | Local content mirror: atomic writes, placeholders, hashing |
| `src/icloud_api.rs` | `pyicloud` dependency | Trait boundary for the Apple/iCloud protocol client |
| `src/sync.rs` | `driver.py` → `ICloudSyncEngine` | Crawl, hydrate, upload, conflict resolution, background loops |
| `src/fuse_fs.rs` | `driver.py` → `Operations` (fusepy) | FUSE operations surface (lookup, readdir, read/write, rename, …) |
| `src/auth.rs` | `auth.py` | Interactive Apple ID / 2FA bootstrap |
| `src/service.rs` | `icloudctl` (bash) | systemd unit generation, env file, service control helpers |
| `src/main.rs` | `icloudctl` (bash) | CLI surface |

## Data flow

```
              ┌─────────────┐        crawl / upload / download
              │  iCloud API │◄──────────────────────────────┐
              └─────────────┘                               │
                     │                                     │
              ┌──────▼──────┐   metadata   ┌─────────────┐ │
              │ SyncEngine  │─────────────►│ SyncState   │ │
              │             │◄─────────────│ (SQLite)    │ │
              └──────┬──────┘  dirty/hydrated└─────────────┘ │
                     │                                     │
              ┌──────▼──────┐                              │
              │ LocalMirror │  content                     │
              └──────┬──────┘                              │
                     │                                     │
              ┌──────▼─────────────────────────────────────▼┐
              │  ICloudFs (fuser)  ──►  kernel VFS          │
              └──────────────────────────────────────────────┘
```

Reads are served from `LocalMirror`. When a file is not hydrated yet, the FUSE
`open`/`read` path calls back into `SyncEngine::ensure_local_file`, which downloads
the content first and then serves it locally — the same "download then serve"
behaviour the Python driver had.

## State DB

Schema is intentionally compatible with the Python implementation's
`~/.cache/icloud-linux/state.sqlite3`:

```sql
entries(
  path PRIMARY KEY, type, parent_path,
  remote_drivewsid, remote_docwsid, remote_etag, remote_zone, remote_shareid,
  size, mtime, hydrated, dirty, tombstone,
  local_sha256, last_synced_at, synced_path
)
pending_ops(id, op, path, target_path, queued_at, retry_count, last_error)
```

Keeping the schema compatible means an existing cache can be reused by the Rust
build and vice versa, which makes migration and A/B testing painless.

## Sync boundary rules

Two rules, evaluated in this order, shared by the FUSE layer, the hydrator and the
uploader (single source of truth, `config::path_allowed`):

1. `exclude_paths` (deny-list) — a matching prefix blocks hydration and mutation.
2. `sync_paths` (allow-list) — when set, only matching prefixes are hydrated or mutable.

`config::resolve_icloud_path` refuses paths that try to climb above the drive root,
and `mirror::LocalMirror::local_path` re-checks containment on the resolved path.

## Threading model

- The FUSE session owns one thread (the `fuser` session loop).
- The sync engine owns a bounded worker pool for downloads/uploads.
- Background loops (upload timer, remote refresh timer) run as cancellable tasks and
  are signalled through an atomic "refresh requested" flag plus `SIGUSR1` for the
  on-demand path.

`SIGUSR1` keeps the upstream contract: `icloudctl sync` signals the running driver
and waits for the completion marker in `~/.local/state/icloud-linux/sync_done`.

## Extension points

- `icloud_api::ICloudClient` — implement this trait to plug in a real protocol
  client (or a mock for tests).
- `fuse_fs::HydrateFn` — the callback used to hydrate on demand; lets tests mount a
  filesystem with a fake hydrator.
