# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.0] — background work

### Added

- **On-demand sync over `SIGUSR1`.** The running driver answers the signal
  with a single pass and writes a completion marker. `icloudctl sync` asks
  the driver, waits for the marker and reports the result; without a driver
  the pass runs in the calling process.
- **Background hydration.** With `hydration: background` a small pool of
  workers downloads the files the index knows about. The pool is deliberately
  small: iCloud throttles aggressive parallel downloads.
- **Subtree hydration.** `icloudctl hydrate --path /Downloads` lists the
  subtree through the service and downloads it, so a folder can be prefetched
  without crawling the drive.
- **Conflict export.** `icloudctl conflicts` lists the preserved versions and
  `icloudctl export-conflicts <dir>` writes them out.
- **A conflict keeps the local version.** When a path diverged on both sides,
  the working copy is committed to the content store before the remote version
  wins, so nothing is lost.
- The garbage collector keeps the blobs a conflict refers to.
- 7 new tests for the background work: conflict capture, conflict export,
  hydration claims, subtree hydration, the journal drain, the signal flag and
  the completion marker.

### Changed

- The control loop is one component (`src/control.rs`): it handles the
  one-shot requests, the automatic schedule and the hydration workers.
- The signal handler only sets a flag; the loop picks it up. Nothing but an
  atomic store happens in signal context.

## [0.3.0] — the iCloud protocol client

### Added

- **SRP sign-in.** Apple's variant of SRP-6a: the password never leaves the
  machine. The password is derived with PBKDF2-HMAC-SHA256 over its hash, the
  salt and the iteration count the service chooses; the username is omitted
  from `x`, as Apple does.
- **Two-factor authentication.** The code of a push notification, or a code
  requested by SMS (`--force-sms`), including the phone numbers of the
  account.
- **Session persistence.** Cookies, the session token, the trust token and the
  service endpoints are stored in `cookie_dir` with owner-only permissions, so
  a restart resumes without a new code.
- **Partition detection.** The account's shard is detected once and used for
  the service endpoints, which avoids the `421` errors.
- **Drive service.** Resolve a path, list a folder, download, upload (the
  three step exchange the service expects), create a folder, rename, move and
  move to the trash.
- **Lazy resolution through the mount.** `lookup` and `readdir` ask the
  service for a path the first time it is used, so the index fills as the
  drive is explored instead of being crawled.
- **Background sync loop.** While the mount is served, the journal is drained
  and stale metadata is refreshed on the configured schedule.
- **Degraded mode.** Without a valid session the driver parks in
  "unauthenticated" mode: the mount still works and serves the local store.
- **14 protocol tests**, including an SRP round trip that checks the client
  proof against a server side computation instead of a recorded value.

### Changed

- The HTTP layer sits behind a `Transport` trait with a cookie jar of our own,
  so the protocol can be tested against scripted responses and the session can
  be persisted between runs.

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
