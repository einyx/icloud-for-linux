# icloud-for-linux

Mount iCloud Drive on Linux as a fast, local-first FUSE filesystem — with a
content-addressed local store, a lazily filled path index, and a journal that
makes every local change durable and replayable.

Independent Rust implementation, released under the **GNU General Public
License v3.0**.

---

## What this is

Your iCloud Drive appears as an ordinary folder on Linux, and it behaves like an
ordinary folder:

- `find`, editors, shells and file browsers work against a local store
- reads are served from disk, never from the network
- writes are recorded in a journal before they are uploaded
- remote changes are pulled in when they matter, not on a full rescan

## How it works

Three cooperating parts:

| Part | What it does |
| --- | --- |
| **Content store** | Every file's content is stored once, keyed by its SHA-256. Duplicates cost nothing, corruption is detectable. |
| **Path index** | A lazily filled SQLite index of paths and their metadata. Nothing is scanned up front; a path is resolved when you touch it. |
| **Journal** | A write-ahead log of local changes. Idempotent, ordered, and replayable after a crash. |

On top of those sits the FUSE filesystem, which serves reads from the local
store and turns writes into journal entries.

Important behaviour:

- The mount is local-first; opening a file never blocks on a download unless
  the content has never been fetched.
- Restarts reuse the existing store.
- A file is never half-written: the kernel sees complete content only.
- If a path changed both locally and remotely, the local version is **kept** as a
  blob in the store and recorded as a conflict — nothing is overwritten silently.
- With `auto_sync: false` nothing polls in the background; `icloudctl sync`
  drives a single pass when you want one.

## Requirements

- Linux with FUSE (`/dev/fuse`)
- Rust 1.85+ to build from source, or use the container image
- `systemctl --user` for the background service

### Debian / Ubuntu

```bash
sudo apt-get update
sudo apt-get install -y fuse3 build-essential
```

### Fedora

```bash
sudo dnf install fuse gcc make
```

The FUSE layer is pure Rust — it talks to `/dev/fuse` directly, so no `libfuse`
development headers are needed. `build-essential` is only required for the
bundled SQLite build.

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

### Release tarball

```bash
curl -LO https://github.com/einyx/icloud-for-linux/releases/latest/download/icloud-for-linux-0.2.0-linux-x86_64.tar.gz
tar -xzf icloud-for-linux-0.2.0-linux-x86_64.tar.gz
sudo install -m755 icloud-for-linux-0.2.0/icloudctl /usr/local/bin/icloudctl
```

## Quick setup

```bash
icloudctl init ~/iCloud      # store, config and systemd user unit
icloudctl configure          # Apple ID email + password
icloudctl auth               # interactive 2FA bootstrap
icloudctl mount ~/iCloud     # mount it
```

`icloudctl init` writes a systemd user unit, so `systemctl --user start
icloud-for-linux.service` runs the same mount in the background afterwards.

## Everyday commands

| Command | What it does |
| --- | --- |
| `icloudctl init [dir]` | Create the local store, the config file and the systemd unit |
| `icloudctl configure [email]` | Write `config.yaml` with credentials |
| `icloudctl auth` | Interactive Apple ID + 2FA bootstrap |
| `icloudctl mount [dir]` | Mount the filesystem (foreground) |
| `icloudctl unmount [dir]` | Unmount the filesystem |
| `icloudctl sync [--dry-run]` | Drain the journal and refresh stale metadata |
| `icloudctl hydrate [--dry-run]` | Download the content of every eligible file |
| `icloudctl export <path> <dir>` | Materialise a subtree as plain files |
| `icloudctl verify` | Re-hash every blob and report corruption |
| `icloudctl gc` | Remove blobs nothing references any more |
| `icloudctl index` | Show what the local index knows |
| `icloudctl status` | Show service status |
| `icloudctl logs` | Tail service logs |
| `icloudctl doctor` | Check the local setup |
| `icloudctl clear-cache` | Remove the local store (rebuilt on next start) |

### Why authentication is split into two steps

Systemd user services are non-interactive — they cannot pause for a 2FA code.

- `icloudctl auth` performs the interactive one-time Apple login and 2FA flow
- the generated user service reuses the saved session in the background

If Apple expires your session:

```bash
icloudctl auth
systemctl --user restart icloud-for-linux.service
```

If Apple reports that updated terms of service must be accepted:

```bash
icloudctl auth --accept-terms
```

### iOS 26 beta 2FA workaround

iOS 26 beta may deliver a push notification popup instead of a numeric code.
Force an SMS code instead:

```bash
icloudctl auth --force-sms
```

`icloudctl auth --debug` prints Apple's reported auth mode to diagnose delivery
issues.

## Controlling the synchronisation boundary

By default the whole drive is reachable and the mount permits mutations
throughout. `sync_paths` makes selective sync a hard boundary.

**`sync_paths`** — allow-list. Only paths in this list may be downloaded or
modified through the mount. Anything else fails with `EACCES` and is never
queued for upload. Empty or omitted means unrestricted.

**`exclude_paths`** — deny-list. Matching paths are never downloaded or
modified, even if they fall under a `sync_path`. The deny-list is evaluated
first.

```yaml
# Only download /Downloads
sync_paths:
  - /Downloads

# But skip this large subfolder for now
exclude_paths:
  - /Downloads/Large Archive
```

For a rename or move, **both** the source and the destination must be permitted.

## Auto-sync vs on-demand sync

**`auto_sync: true`** (default) — the drain and refresh loops run automatically.

**`auto_sync: false`** — nothing runs in the background. The driver mounts and
waits; `icloudctl sync` runs a single pass when you want one.

```bash
icloudctl sync          # apply pending changes, refresh stale metadata
icloudctl hydrate       # download the content of every eligible file
icloudctl export /Downloads ~/Desktop/intake   # copy out as plain files
```

## Local store layout

| What | Where |
| --- | --- |
| Config | `~/.config/icloud-for-linux/config.yaml` |
| Session cookies | `~/.config/icloud-for-linux/cookies` |
| Service env | `~/.config/icloud-for-linux/icloud.env` |
| User service | `~/.config/systemd/user/icloud-for-linux.service` |
| Store root | `~/.cache/icloud-for-linux` |
| Content store | `~/.cache/icloud-for-linux/blobs/ab/cdef…` |
| Working copies | `~/.cache/icloud-for-linux/staging/*.work` |
| Index + journal | `~/.cache/icloud-for-linux/state.sqlite3` |
| Logs | `~/.local/state/icloud-for-linux/icloud.log` |

## Development harness

The repo ships a complete dev harness so you never need a local Rust install:

```bash
make check         # fmt + clippy + tests on the host
make docker-dev    # shell inside the dev container
make docker-check  # fmt + clippy + tests inside the dev container
make docker-fuse   # run the FUSE mount inside a container (needs /dev/fuse)
make help          # all targets
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
journalctl --user -u icloud-for-linux.service -f
```

**Authentication expired**

```bash
icloudctl auth
systemctl --user restart icloud-for-linux.service
```

**Service starts but auth fails silently** — under systemd there is no TTY, so a
failed auth parks the service in unauthenticated mode instead of crash-looping
(this avoids Apple account lockout). You will see the warning in the logs.

**Rebuild everything locally**

```bash
icloudctl clear-cache
```

**Check what is stored locally**

```bash
icloudctl index
icloudctl verify
```

## Project status

This is an active implementation. The local store — content store, path index,
journal, FUSE surface, CLI, service integration — is implemented and tested. The
Apple protocol client is the piece still being built. See
[docs/ROADMAP.md](docs/ROADMAP.md) for the status and
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design.

## License & attribution

Copyright (C) 2026 einyx and contributors.

This program is free software: you can redistribute it and/or modify it under
the terms of the **GNU General Public License as published by the Free Software
Foundation, either version 3 of the License, or (at your option) any later
version.**

This program is distributed in the hope that it will be useful, but WITHOUT ANY
WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A
PARTICULAR PURPOSE. See the GNU General Public License for more details.

You should have received a copy of the GNU General Public License along with
this program. If not, see <https://www.gnu.org/licenses/>.

### Provenance

This is an **independent implementation**. It was written from scratch in Rust;
no source code from another project is included. The goal it pursues — a
local-first iCloud Drive filesystem for Linux — comes from the community project
[`IsmaeelAkram/icloud-linux`](https://github.com/IsmaeelAkram/icloud-linux) by
Ismaeel Akram, which is credited here for the idea and for proving the idea
works. That project ships no license file, so nothing from it is reused here.

Apple, iCloud, macOS and iOS are trademarks of Apple Inc. This project is not
affiliated with, endorsed by, or sponsored by Apple Inc.
