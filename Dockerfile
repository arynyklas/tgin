# syntax=docker/dockerfile:1

# rust:1.99.0-alpine3.24 (multi-arch index digest)
FROM rust:1.99.0-alpine3.24@sha256:0cce0a5e0e8ba67b455257a3a02a1d99005f382748789d6464460028810f1627 AS builder

# The musl host target links statically. The C toolchain builds aws-lc-sys
# (the rustls crypto provider used by reqwest and axum-server); cmake and perl
# cover its CMake build path on targets its cc builder does not handle.
RUN apk add --no-cache musl-dev build-base cmake perl

WORKDIR /usr/src/tgin

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY xtask ./xtask

# `target` lives in a cache mount, so the binary is copied out in the same step.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/src/tgin/target \
    cargo build --release --locked -p tgin && \
    install -D -m 0755 target/release/tgin /out/tgin

# gcr.io/distroless/static-debian13:latest (multi-arch index digest)
FROM gcr.io/distroless/static-debian13@sha256:58133991db06659feaabe0f4e97a35cebf15ef4ea08f8a4c6d2ee5f75e4aa6a0

ARG VERSION=dev
ARG REVISION=unknown
ARG CREATED=unknown

LABEL org.opencontainers.image.title="tgin" \
      org.opencontainers.image.description="Routing layer between the Telegram Bot API and downstream bot instances" \
      org.opencontainers.image.source="https://github.com/arynyklas/tgin" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}" \
      org.opencontainers.image.created="${CREATED}"

COPY --from=builder /out/tgin /usr/local/bin/tgin

USER 65532:65532
EXPOSE 3000

ENTRYPOINT ["/usr/local/bin/tgin"]
CMD ["-f", "/etc/tgin/config.ron"]
