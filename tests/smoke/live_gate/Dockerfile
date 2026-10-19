# syntax=docker/dockerfile:1
#
# The live-gate example (crates/tgin-mtproto/examples/live_gate.rs) for the
# soak containers: a static musl build, as tgin's own image. Build from the
# repository root; see tests/smoke/live_gate.md.

# rust:1.99.0-alpine3.24 (multi-arch index digest), as in the root Dockerfile.
FROM rust:1.99.0-alpine3.24@sha256:0cce0a5e0e8ba67b455257a3a02a1d99005f382748789d6464460028810f1627 AS builder

RUN apk add --no-cache musl-dev build-base cmake perl

WORKDIR /usr/src/tgin

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY xtask ./xtask
COPY tests/e2e/fake-telegram ./tests/e2e/fake-telegram

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/src/tgin/target \
    cargo build --release --locked -p tgin-mtproto --example live_gate && \
    install -D -m 0755 target/release/examples/live_gate /out/live_gate

# gcr.io/distroless/static-debian13 (multi-arch index digest), as in the
# root Dockerfile: CA certificates for the Bot API, no shell.
FROM gcr.io/distroless/static-debian13@sha256:58133991db06659feaabe0f4e97a35cebf15ef4ea08f8a4c6d2ee5f75e4aa6a0

COPY --from=builder /out/live_gate /usr/local/bin/live_gate
ENTRYPOINT ["/usr/local/bin/live_gate"]
