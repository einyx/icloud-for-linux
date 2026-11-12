# icloud-for-linux

Mount iCloud Drive on Linux as a fast, local-first FUSE filesystem — with persistent
caching, selective hydration, and bidirectional sync.

This is a **Rust rewrite** of the Python project
[`IsmaeelAkram/icloud-linux`](https://github.com/IsmaeelAkram/icloud-linux), released
under the **GNU General Public License v3.0**.

---

## What this is

`icloud-for-linux` makes your iCloud Drive show up like a normal folder on Linux.

It is designed to feel much more local than a naive network mount:

- folders and filenames are cached on disk
- file contents are downloaded into a local mirror on demand or in the background
- reads usually come from local storage, not from iCloud on every access
- local changes are written immediately and synced back on the next sync pass
- remote changes are pulled in by the sync engine on demand or on a timer

In practice, `find`, editors, shells and normal file browsing work against a persistent
local cache instead of blocking on iCloud for every operation.

## How it works

Three main pieces:

| Piece | What it does |
| --- | --- |
| **Metadata crawl** | First run scans iCloud Drive and builds a local index (SQLite). |
| **Hydration** | File contents are downloaded into the local mirror — lazily on open, or proactively in the background. |
| **Sync engine** | Local edits upload and remote changes refresh, automatically on a timer or on demand via `icloudctl sync`. |

Important behaviour:

- The mount is local-first.
- Restarts reuse the existing cache instead of starting from zero.
- Opening a file before it finished hydrating downloads that file first, then serves it locally.
- Failed warmup downloads are retried automatically with backoff.
- If a path changed both locally and remotely, the local version is preserved as a conflict copy instead of being silently overwritten.
- With `auto_sync: false` the driver starts and mounts immediately without spawning background polling threads. Run `icloudctl sync` when you want a fresh pull.

## Requirements

- Linux with FUSE (`/dev/fuse`)
- Rust 1.85+ to build from source (or use the prebuilt container image)
- `systemctl --user` for the background service

### Debian / Ubuntu

```bash
sudo apt-get update
sudo apt-get install -y fuse3 pkg-config build-essential
```

### Fedora

```bash
sudo dnf install fuse fuse-devel gcc make pkgconf-pkg-config
```

## Install

### From source

```bash
git clone https://github.com/einyx/icloud-for-linux.git
cd icloud-for-linux
make install          # installs ~/.local/bin/icloudctl
```

### Container

```bash
docker compose --profile fuse up --build mount
```

### Prebuilt release tarball

```bash
curl -LO https://github.com/einyx/icloud-for-linux/releases/latest/download/icloud-for-linux-linux-x86_64.tar.gz
tar -xzf icloud-for-linux-linux-x86_64.tar.gz
sudo install -m755 icloudctl /usr/local/bin/icloudctl
```

## Quick setup

```bash
icloudctl init ~/iCloud      # config + cache dirs + systemd user unit
icloudctl configure          # Apple ID email + password
icloudctl auth               # interactive 2FA bootstrap
icloudctl mount ~/iCloud     # mount it
```

`icloudctl init` writes a systemd user unit, so `systemctl --user start icloud.service`
runs the same mount in the background afterwards.

## Everyday commands

| Command | What it does |
| --- | --- |
| `icloudctl init [dir]` | Create config/cache dirs, config file and the systemd user unit |
| `icloudctl configure [email]` | Write `config.yaml` with credentials |
| `icloudctl auth` | Interactive Apple ID + 2FA bootstrap |
| `icloudctl mount [dir]` | Mount the filesystem (foreground) |
| `icloudctl unmount [dir]` | Unmount the filesystem |
| `icloudctl hydrate [--dry-run] [--verbose]` | Download all eligible files into the local mirror |
| `icloudctl sync [--timeout SEC] [--quiet]` | Trigger an on-demand remote sync in the running driver |
| `icloudctl status` | Show service status |
| `icloudctl logs` | Tail service logs |
| `icloudctl doctor` | Check local setup health |
| `icloudctl clear-cache` | Remove the local mirror and state DB (rebuilt on next start) |

### Why authentication is split into two steps

Systemd user services are non-interactive — they cannot pause for a 2FA code.

So the project uses:

- `icloudctl auth` for the interactive one-time Apple login and 2FA flow
- a generated user service that reuses the saved session in the background

If Apple expires your session:

```bash
icloudctl auth
systemctl --user restart icloud.service
```

If Apple reports that updated terms of service must be accepted:

```bash
icloudctl auth --accept-terms
```

### iOS 26 beta 2FA workaround

iOS 26 beta may deliver a push notification popup instead of a numeric code. Force an
SMS code instead:

```bash
icloudctl auth --force-sms
```

`icloudctl auth --debug` prints Apple's reported auth mode to diagnose delivery issues.

## Controlling the synchronisation boundary

By default the whole drive is indexed (stubs everywhere) and the mount permits
mutations throughout. `sync_paths` makes selective sync a hard boundary.

**`sync_paths`** — allow-list. Only paths in this list have file contents downloaded, or
may be created, written, truncated, renamed, moved or deleted through the mount.
Operations outside the allow-list fail with `EACCES` and are never queued for upload.
Empty or omitted means unrestricted.

**`exclude_paths`** — deny-list. Matching paths are never hydrated or mutated, even if
they fall under a `sync_path`. The deny-list is evaluated first.

```yaml
# Only hydrate /Downloads
sync_paths:
  - /Downloads

# But skip this large subfolder for now
exclude_paths:
  - /Downloads/Large Archive
```

For a rename or move, **both** the source and the destination must be permitted.

## Auto-sync vs on-demand sync

**`auto_sync: true`** (default) — background loops poll iCloud on the
`upload_interval_seconds` and `remote_refresh_interval_seconds` schedules.

**`auto_sync: false`** — no polling threads start. The driver mounts and waits. Use
`icloudctl sync` to pull the latest remote changes when you need them.

```bash
icloudctl sync          # pull latest metadata from iCloud
icloudctl hydrate       # download file contents for all eligible paths
# now copy from the mirror: ~/.cache/icloud-linux/mirror/Downloads/
```

## Local cache and sync state

| What | Where |
| --- | --- |
| Config | `~/.config/icloud-linux/config.yaml` |
| Session cookies | `~/.config/icloud-linux/cookies` |
| Service env | `~/.config/icloud-linux/icloud.env` |
| User service | `~/.config/systemd/user/icloud.service` |
| Local cache root | `~/.cache/icloud-linux` |
| Local mirror | `~/.cache/icloud-linux/mirror` |
| Sync state database | `~/.cache/icloud-linux/state.sqlite3` |
| Logs | `~/.local/state/icloud-linux/icloud.log` |

## Development harness

The repo ships a complete dev harness so you never need a local Rust install:

```bash
make check         # fmt + clippy + tests on the host
make docker-dev   # shell inside the dev container
make docker-check # fmt + clippy + tests inside the dev container
make docker-fuse  # run the FUSE mount inside a container (needs /dev/fuse)
make help         # all targets
```

Layout:

- `Dockerfile` — multi-stage production image (builder → slim runtime with `fuse3`)
- `Dockerfile.dev` — toolchain image: rustfmt, clippy, cargo-watch, cargo-audit, just
- `docker-compose.yml` — `dev`, `build` and opt-in `fuse` services
- `.devcontainer/devcontainer.json` — VS Code devcontainer
- `Makefile` — the single entry point for every workflow
- `.github/workflows/` — CI (fmt/clippy/test, multi-arch build, container) and release publishing

## Troubleshooting

**The service will not start**

```bash
icloudctl status
icloudctl doctor
journalctl --user -u icloud.service -f
```

**Authentication expired**

```bash
icloudctl auth
systemctl --user restart icloud.service
```

**Service starts but auth fails silently** — under systemd there is no TTY, so a failed
auth parks the service in unauthenticated mode instead of crash-looping (this avoids
Apple account lockout). You will see `UNAUTHENTICATED mode` in the logs.

**Rebuild everything locally**

```bash
icloudctl clear-cache
```

## Project status

This is an active rewrite. The local-first core (config, SQLite state, mirror, FUSE
surface, CLI, service integration) is implemented and tested; the Apple/iCloud
protocol client is being ported. See [docs/ROADMAP.md](docs/ROADMAP.md) for the
current status and [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the module map.

## License & attribution

Copyright (C) 2026 einyx and contributors.

This program is free software: you can redistribute it and/or modify it under the
terms of the **GNU General Public License as published by the Free Software Foundation,
either version 3 of the License, or (at your option) any later version.**

This program is distributed in the hope that it will be useful, but WITHOUT ANY
WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A
PARTICULAR PURPOSE. See the GNU General Public License for more details.

You should have received a copy of the GNU General Public License along with this
program. If not, see <https://www.gnu.org/licenses/>.

### Upstream project

This work is a rewrite of, and is inspired by, the Python project
[`IsmaeelAkram/icloud-linux`](https://github.com/IsmaeelAkram/icloud-linux) by
Ismaeel Akram. The upstream repository ships **no license file**, which means it is
not licensed as open source by default.

> **Before distributing this rewrite, resolve upstream licensing.** Either obtain an
> explicit license grant from the upstream author, or confirm that the Rust
> implementation is a clean-room, independent work that does not reproduce upstream
> code. See [docs/ROADMAP.md](docs/ROADMAP.md#legal-risk) for the checklist.

Apple, iCloud, macOS and iOS are trademarks of Apple Inc. This project is not
affiliated with, endorsed by, or sponsored by Apple Inc.
