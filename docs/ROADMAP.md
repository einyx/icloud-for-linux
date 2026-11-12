# Roadmap

Strategy for shipping `icloud-for-linux` as a credible, license-clean Rust
successor to the Python `icloud-linux` project.

## Where we are

| Area | Status | Notes |
| --- | --- | --- |
| Project scaffold, CI, dev harness | ✅ Done | Makefile, Dockerfile, compose, devcontainer, GitHub Actions |
| License (GPLv3) + NOTICE + attribution | ✅ Done | See `LICENSE`, `NOTICE` |
| Config model + sync boundary rules | ✅ Done | `src/config.rs`, unit tested |
| SQLite state DB | ✅ Done | `src/state.rs`, schema-compatible with upstream |
| Local mirror (atomic writes, placeholders, hashing) | ✅ Done | `src/mirror.rs`, unit tested |
| FUSE surface (fuser) | ✅ Done | `src/fuse_fs.rs` — lookup/readdir/read/write/create/mkdir/rename/rmdir/unlink/setattr |
| CLI (`icloudctl`) | ✅ Done | `src/main.rs` — init/configure/auth/mount/unmount/hydrate/sync/status/logs/doctor/clear-cache |
| systemd user unit generation | ✅ Done | `src/service.rs` |
| Apple/iCloud protocol client | 🔲 **Next** | `src/icloud_api.rs` — trait defined, implementation pending |
| Interactive 2FA bootstrap | 🔲 Next | `src/auth.rs` — flow defined, protocol pending |
| Background upload/refresh loops | 🔲 Next | `src/sync.rs` — engine skeleton present, loops pending |
| Conflict resolution (`copy` mode) | 🔲 Next | depends on the protocol client |
| Upstream parity test suite | 🔲 Next | port `test_driver.py` / `test_hydrate.py` scenarios to Rust |

## Milestones

### M1 — Protocol client (blocking)
Deliver a working `ICloudClient` implementation:

- Apple ID login + 2FA/2SA (push, trusted-device bridge, SMS fallback)
- account partition detection (`x-apple-user-partition`) to avoid `421` errors
- session/trust cookie persistence in `cookie_dir`
- Drive metadata crawl (`drivewsid`, `docwsid`, `etag`, `zone`, `shareid`)
- file download / upload / mkdir / delete / move+rename
- updated terms-of-service acceptance

**Exit criteria:** a real iCloud account can be crawled, hydrated, and written back.

### M2 — Sync engine
- background upload timer + remote refresh timer
- `auto_sync: false` on-demand path (`SIGUSR1` → crawl → `sync_done` marker)
- dirty/tombstone tracking with conflict copies (`<name>.local-conflict-<ts>`)
- retry with exponential backoff for failed downloads
- selective hydration honouring `sync_paths` / `exclude_paths`

**Exit criteria:** bidirectional sync works across restarts without data loss.

### M3 — Parity & hardening
- port the upstream Python test scenarios to Rust integration tests
- memory ceiling check (the Python driver had a 4-minute / 11 GB boot blowup)
- crash-safety: atomic mirror writes, WAL-mode SQLite
- FUSE edge cases: `fsync`, `fallocate`, xattrs, `renameat2`, large files > 4 GB

### M4 — Distribution
- release tarballs + container images (already wired in CI)
- AUR / Nix flake packaging
- `icloudctl doctor` covering kernel FUSE version, `/dev/fuse` perms, clock skew

## Legal risk

The upstream Python repository (`IsmaeelAkram/icloud-linux`) ships **no LICENSE
file**. Unlicensed code is "All rights reserved" by default, so a rewrite that
reproduces upstream logic, structure or code cannot be redistributed until the
author grants a license.

Checklist before publishing a build that claims to be a rewrite:

- [ ] Contact the upstream author and request an explicit license grant
      (GPL-3.0 is ideal — it keeps the derivative under the same terms).
- [ ] If no grant is obtained, verify that the Rust implementation is an independent,
      clean-room work: no upstream source, no verbatim strings, no direct
      transliteration of upstream functions.
- [ ] Keep a written record of the implementation provenance per module
      (`docs/ARCHITECTURE.md` already maps every Rust module to its upstream origin).
- [ ] Keep `NOTICE` and the attribution section in `README.md` in place.
- [ ] Have the final release reviewed by someone competent in licensing before
      announcing it publicly.

Until that is resolved, the repository is published as an **independent Rust
implementation** with attribution, and the README states the risk explicitly.

## Non-goals

- No system-wide (root) service — user-level systemd only, like upstream.
- No aggressive parallel downloads — iCloud throttles and can lock the account.
- No write support for shared folders / "app libraries" until M2 is done.

## How to help

- Port a scenario from `test_driver.py` to `tests/` — see `CONTRIBUTING.md`.
- Improve the protocol client — that is the critical path.
- File an issue with a reproducible scenario; include `icloudctl doctor` output.
