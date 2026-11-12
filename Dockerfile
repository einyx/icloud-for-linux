# syntax=docker/dockerfile:1
#
# Production image for icloud-for-linux.
#
# Build:
#   docker build -t icloud-for-linux:latest .
#
# Run (needs FUSE access):
#   docker run --rm --device /dev/fuse --cap-add SYS_ADMIN \
#     -v icloud-cache:/var/cache/icloud-linux \
#     -v icloud-config:/var/config/icloud-linux \
#     icloud-for-linux:latest mount /mnt/icloud

ARG RUST_VERSION=1.90

# ─────────────────────────────── builder ───────────────────────────────
FROM rust:${RUST_VERSION}-slim-bookworm AS builder

# gcc is needed by the bundled SQLite build (rusqlite/bundled); the FUSE layer
# is pure Rust, so no libfuse headers are needed.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        gcc \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Dependency layer: cache the crates registry separately from the source tree.
COPY Cargo.toml Cargo.lock ./
# Warm the dependency cache with stub sources so dependency compilation is
# cached independently from the real source tree.
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && cargo build --release --locked || true

COPY src ./src
COPY config.example.yaml ./
# `touch` is required: COPY preserves the source mtimes from the build context,
# which are older than the stub build artefacts, so cargo would otherwise
# consider the tree unchanged and keep the stub binary.
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

# ─────────────────────────────── runtime ───────────────────────────────
FROM debian:bookworm-slim AS runtime

LABEL org.opencontainers.image.title="icloud-for-linux" \
      org.opencontainers.image.description="Mount iCloud Drive on Linux as a local-first FUSE filesystem" \
      org.opencontainers.image.licenses="GPL-3.0-only" \
      org.opencontainers.image.source="https://github.com/einyx/icloud-for-linux"

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        fuse3 \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/icloudctl /usr/local/bin/icloudctl

# State locations (override with -v)
ENV ICLOUD_CONFIG_DIR=/var/config/icloud-linux \
    ICLOUD_CACHE_DIR=/var/cache/icloud-linux \
    XDG_CONFIG_HOME=/var/config \
    XDG_CACHE_HOME=/var/cache \
    XDG_STATE_HOME=/var/state

RUN mkdir -p /var/config/icloud-linux /var/cache/icloud-linux /var/state/icloud-linux /mnt/icloud

VOLUME ["/var/config/icloud-linux", "/var/cache/icloud-linux", "/var/state/icloud-linux"]

ENTRYPOINT ["/usr/local/bin/icloudctl"]
CMD ["--help"]
