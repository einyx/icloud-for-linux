# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Rust rewrite of the Python `icloud-linux` driver, licensed under GPL-3.0-only.
- Local-first core: config model with sync boundary rules, SQLite state database
  (schema-compatible with the Python implementation), and a local content mirror with
  atomic writes and SHA-256 hashing.
- FUSE filesystem surface built on `fuser`: lookup, getattr, readdir, read, write,
  create, mkdir, unlink, rmdir, rename, setattr, statfs.
- `icloudctl` CLI: `init`, `configure`, `auth`, `mount`, `unmount`, `hydrate`,
  `sync`, `status`, `logs`, `doctor`, `clear-cache`.
- systemd user unit + env file generation, with `SIGUSR1` on-demand sync contract.
- Development harness: Makefile, production Dockerfile, dev Dockerfile, compose
  file with an opt-in FUSE profile, VS Code devcontainer.
- CI: fmt, clippy (`-D warnings`), tests, multi-arch release build, container build.
- Release workflow publishing tarballs to GitHub Releases.
- Documentation: README, ARCHITECTURE, ROADMAP, CONTRIBUTING, NOTICE.

### Known limitations

- The Apple/iCloud protocol client is not implemented yet; the driver currently
  operates on the local mirror only.
- Interactive 2FA bootstrap is wired but awaits the protocol client.
- Background upload/refresh loops and conflict copies land with the protocol client.
- Upstream licensing must be resolved before distributing a build that reproduces
  upstream code (see `docs/ROADMAP.md`).
