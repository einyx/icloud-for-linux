# Contributing

Thanks for helping improve `icloud-for-linux`.

## Getting set up

You do not need a local Rust install — the dev container has everything:

```bash
make docker-dev      # shell in the dev container
make docker-check    # fmt + clippy + tests
```

Or on the host:

```bash
make deps
make check
```

## Workflow

1. Open an issue first for anything larger than a bug fix, so we agree on the shape.
2. Branch from `main`: `feat/<short-name>`, `fix/<short-name>`.
3. Keep the tree green: `make check` (fmt + clippy + tests) must pass before you push.
4. Add or update tests for behaviour you change. The suite lives in `tests/`.
5. Open a PR with a short "what and why". Screenshots/logs help for FUSE behaviour.

## Code conventions

- Rust 2021 edition, `rustfmt` defaults (see `rustfmt.toml`).
- Clippy clean with `-D warnings` — CI enforces this.
- Errors carry context: use `anyhow::Context` at the call site, `thiserror` for
  typed domain errors.
- Public items get a doc comment explaining *why*, not *what*.
- Keep the state DB schema compatible with the Python implementation unless you
  deliberately bump it (see `docs/ARCHITECTURE.md`).

## Testing

```bash
make test                      # unit + integration
cargo test --test core -- --nocapture
```

For FUSE behaviour you need `/dev/fuse` and `SYS_ADMIN`; use the `fuse` compose
profile:

```bash
make docker-fuse
```

## Commit messages

Conventional-ish, imperative, scoped:

```
feat(mirror): atomic replace for downloaded chunks
fix(state): keep hydrated flag when a rename touches a subtree
docs(readme): document the sync boundary rules
```

## Licensing

By contributing you agree that your contribution is licensed under the
GNU General Public License v3.0 (or later), the license of this project.

Do **not** copy code from projects whose license you have not checked. See the
legal-risk section of `docs/ROADMAP.md` for the specific situation this repository
is in.

## Security

Do not open a public issue for a vulnerability. Contact the maintainers privately
first, then we will publish an advisory and a fix together.
