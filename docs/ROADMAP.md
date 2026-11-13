# Roadmap

Status and plan for `icloud-for-linux`.

## Where we are

| Area | Status | Notes |
| --- | --- | --- |
| Project scaffold, CI, dev harness | ✅ Done | Makefile, Dockerfile, compose, devcontainer, GitHub Actions |
| License (GPLv3) + NOTICE + provenance | ✅ Done | See `LICENSE`, `NOTICE` |
| Content-addressed blob store | ✅ Done | `src/blob.rs` — dedup, atomic commit, integrity check, GC |
| Lazy path index | ✅ Done | `src/index.rs` — TTL-based staleness, subtree moves, no full crawl |
| Write-ahead journal | ✅ Done | `src/journal.rs` — idempotent, ordered, supersedes older versions |
| FUSE surface | ✅ Done | `src/vfs.rs` — working copy + content store, staged writes, tombstones |
| Sync engine (drain + delta refresh) | ✅ Done | `src/sync.rs` — journal drain, stale-only refresh, conflict capture |
| CLI (`icloudctl`) | ✅ Done | 15 subcommands including `verify`, `gc`, `export`, `index` |
| systemd user unit generation | ✅ Done | `src/service.rs` |
| Apple protocol client | ✅ Done | `src/icloud/` — SRP sign-in, 2FA, drive service, 14 protocol tests |
| Interactive 2FA bootstrap | ✅ Done | `src/auth.rs` — push code and SMS fallback |
| Background drain/refresh loops | ✅ Done | `src/main.rs` — the sync loop runs while the mount is served |
| Conflict resolution end to end | ✅ Done | the local version is kept as a blob and recorded |
| Lazy resolution through the mount | ✅ Done | `lookup`/`readdir` resolve through the client on demand |

## Milestones

### M1 — Protocol client ✅ done

`src/icloud/` implements the service:

- SRP-6a sign-in (Apple's variant: PBKDF2 derived password, no username in `x`)
- two-factor authentication: push code, SMS request and SMS validation
- trust token, session token and cookie persistence in `cookie_dir`
- account partition detection (`x-apple-user-partition`) to avoid `421`
- drive service: path resolution, folder listing, download, upload (three step),
  create folder, rename, move, trash
- the session is verified before the mount is served; without one the driver
  parks in "unauthenticated" mode and keeps serving the local store

The protocol is covered by 14 tests that run against scripted responses, so
no account is needed to verify it. The SRP proof is checked against a server
side computation rather than a recorded value.

### M2 — Background loops and conflicts ✅ mostly done

- drain and refresh timers driven by `upload_interval_seconds` ✅
- `auto_sync: false` on-demand path (`icloudctl sync`) ✅
- lazy resolution: `lookup` and `readdir` ask the service on demand ✅
- conflict blobs are captured and recorded ✅
- still open: conflict blob export (`icloudctl export-conflicts`), background
  hydration (prefetching eligible files), the `SIGUSR1` completion marker

### M3 — Parity & hardening

- port the upstream behaviour scenarios to Rust integration tests
- memory ceiling check (the Python driver had a 4-minute / 11 GB boot blowup)
- crash safety: atomic working-copy writes, WAL SQLite, blob integrity sweep
- FUSE edge cases: `fallocate`, xattrs, `renameat2`, files > 4 GB, `fsync`
  ordering

### M4 — Distribution

- release tarballs + container images (already wired in CI)
- AUR / Nix flake packaging
- `icloudctl doctor` covering kernel FUSE version, `/dev/fuse` permissions,
  clock skew

## Design decisions to revisit

| Decision | Alternative | When to reconsider |
| --- | --- | --- |
| Working copy per path in `staging/` | Read-only mount backed by the blob store only | If the working copy duplicates too much data — a hardlink into the blob store could replace it, at the cost of losing the "always complete file" guarantee |
| One SQLite file for index and journal | Separate files | If write contention becomes visible |
| SHA-256 blobs | BLAKE3 | If hashing throughput matters on large drives (SHA-256 is used because it is what the protocol side expects) |
| Conflict blobs kept forever | Expire conflicts after N days | Once the conflict export workflow exists |

## Provenance and licensing

This project is an independent implementation written from scratch in Rust. No
source code from another project is vendored or transliterated. The
inspiration — the idea of a local-first iCloud Drive filesystem for Linux — is
credited to the community project
[`IsmaeelAkram/icloud-linux`](https://github.com/IsmaeelAkram/icloud-linux).

That upstream project ships **no license file**, so it is "all rights reserved"
by default and nothing from it may be reused without a grant. This repository
therefore:

- contains no upstream source,
- credits the idea rather than copying the implementation,
- documents the design decisions independently in `docs/ARCHITECTURE.md`.

If you want to align with upstream, ask its author for an explicit license
grant. GPL-3.0 would keep any derivative under the same terms.

## Non-goals

- No system-wide (root) service — user-level systemd only.
- No aggressive parallel downloads — iCloud throttles and can lock the account.
- No write support for shared folders / app libraries until M2.

## How to help

- Implement a piece of the protocol client — that is the critical path.
- Port a behaviour scenario to `tests/` — see `CONTRIBUTING.md`.
- File an issue with a reproducible scenario; include `icloudctl doctor` output.
