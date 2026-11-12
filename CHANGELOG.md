# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] — redesign

The project was rebuilt around a different architecture. The goal is the same
(local-first iCloud Drive on Linux), the machinery underneath is not.

### Changed

- **Content-addressed local store.** File contents are stored once, keyed by
  their SHA-256, instead of one mirror copy per path. Duplicates cost nothing
  and corruption is detectable (`icloudctl verify`).
- **Working copy + content store.** The kernel reads and writes a complete
  working copy; the content store keeps the hashed, deduplicated version that
  is uploaded. A file is never half-written.
- **Lazy path index.** No full crawl on startup. A path is resolved when it is
  first used, and refreshes only re-check the entries that went stale
  (`index_ttl_seconds`).
- **Write-ahead journal.** Every local mutation is recorded before it is
  uploaded. Operations are idempotent, ordered so content lands before the
  rename that depends on it, and a newer content version supersedes the older
  pending ones.
- **Conflicts keep data.** When a path diverged on both sides, the local version
  is preserved as a blob and recorded, instead of being overwritten or copied
  next to the original.
- **New CLI.** `icloudctl` gained `sync --dry-run`, `export`, `verify`, `gc`
  and `index`; `hydrate` now reports against the lazy index.
- **New configuration keys.** `index_ttl_seconds`, `hydration`,
  `conflict_policy` replaced `warmup_mode`, `warmup_workers` and
  `conflict_mode`.
- **New paths.** The store lives in `~/.cache/icloud-for-linux`, the service is
  `icloud-for-linux.service`.

### Added

- Blob store with streaming writes, atomic commits, hardlink materialisation
  and a mark-and-sweep garbage collector.
- Path index with subtree moves, tombstones and staleness tracking.
- Journal with idempotency keys, ordering, superseding and conflict records.
- Sync engine with a drain pass and a delta refresh pass.
- 17 unit and integration tests, including an end-to-end FUSE test that mounts
  the filesystem and exercises create/write/read/append/truncate/rename/delete.
- Architecture, roadmap, provenance and contribution documentation.

### Fixed

- FUSE `readdir` cookies are 1-based. The kernel only advances a listing when
  every returned cookie is greater than the requested offset; the previous
  0-based cookies produced an infinite loop that hung every `ls`.
- The mount root is always reported as a directory, even before the index has
  been populated. Answering ENOENT for the root made the mountpoint vanish.
- A flush no longer destroys the write handle. The kernel flushes before the
  first write when a file is truncated, which previously turned every
  overwrite into `EBADF`.
- `stat` reports the size of the working copy, so a file that was just written
  is no longer reported with a stale size.

## [0.1.0]

### Added

- First release: local-first core (config, SQLite state, mirror, FUSE surface,
  CLI, service integration) and the development harness (Makefile, Dockerfile,
  compose, devcontainer, CI, release workflow).
