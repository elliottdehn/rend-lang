# syntax=docker/dockerfile:1.7
#
# Multi-stage build for rend-server.
#
#   stage 1 — `cargo-chef`-style dep cache layer using a stub workspace
#             so we don't recompile RocksDB / axum / tokio every time
#             only the rend source changes.
#
#   stage 2 — actual compile against real source.
#
#   stage 3 — distroless runtime with just the binary + a /data
#             volume mount point.
#
# Build:  docker build -t rend-server -f deploy/Dockerfile .
# Run:    docker run --rm -p 8080:8080 -v rend-data:/data rend-server

# ---- stage 1: build environment + dep cache ---------------------

FROM rust:1.83-slim-bookworm AS chef
WORKDIR /build

# RocksDB needs clang for bindgen + the usual C++ toolchain. Snappy
# we link statically through the rocksdb crate's bundled feature.
RUN apt-get update && apt-get install -y --no-install-recommends \
        clang libclang-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*

# ---- stage 2: real build ----------------------------------------

FROM chef AS builder
WORKDIR /build
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release -p rend-server \
    && cp /build/target/release/rend-server /usr/local/bin/rend-server

# ---- stage 3: runtime -------------------------------------------
#
# Debian-slim instead of distroless: RocksDB pulls in libstdc++ which
# is awkward on distroless/static. The slim base is ~30 MB; the
# binary is ~25 MB. Total image ~55 MB.

FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 1000 -m -d /home/rend rend

COPY --from=builder /usr/local/bin/rend-server /usr/local/bin/rend-server

# RocksDB dir lives under /data — mount a volume here in compose.
RUN mkdir -p /data && chown rend:rend /data
VOLUME ["/data"]

USER rend
WORKDIR /home/rend

ENV REND_LISTEN=0.0.0.0:8080
ENV REND_DATA_DIR=/data
ENV RUST_LOG=rend_server=info,tower_http=info

EXPOSE 8080

HEALTHCHECK --interval=10s --timeout=2s --start-period=5s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/healthz || exit 1

ENTRYPOINT ["/usr/local/bin/rend-server"]
