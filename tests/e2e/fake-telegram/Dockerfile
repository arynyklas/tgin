# syntax=docker/dockerfile:1
# Build from the repository root (the fake is a workspace member):
#   docker build -f tests/e2e/fake-telegram/Dockerfile -t tgin-fake-telegram:dev .

# rust:1.99.0-alpine3.24 (multi-arch index digest), as the root Dockerfile
FROM rust:1.99.0-alpine3.24@sha256:0cce0a5e0e8ba67b455257a3a02a1d99005f382748789d6464460028810f1627 AS builder

# The musl host target links statically; aws-lc-sys (reqwest's rustls
# provider, used for webhook delivery) needs the C toolchain, cmake and perl.
RUN apk add --no-cache musl-dev build-base cmake perl

WORKDIR /usr/src/tgin

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY xtask ./xtask
COPY tests/e2e/fake-telegram ./tests/e2e/fake-telegram

# `target` lives in a cache mount, so the binary is copied out in the same step.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/src/tgin/target \
    cargo build --release --locked -p tgin-fake-telegram && \
    install -D -m 0755 target/release/fake-telegram /out/fake-telegram

# gcr.io/distroless/static-debian13:latest (multi-arch index digest)
FROM gcr.io/distroless/static-debian13@sha256:58133991db06659feaabe0f4e97a35cebf15ef4ea08f8a4c6d2ee5f75e4aa6a0

LABEL org.opencontainers.image.title="tgin-fake-telegram" \
      org.opencontainers.image.description="A fake Telegram Bot API server for tgin's end-to-end tests" \
      org.opencontainers.image.source="https://github.com/arynyklas/tgin" \
      org.opencontainers.image.licenses="MIT"

COPY --from=builder /out/fake-telegram /usr/local/bin/fake-telegram

USER 65532:65532
ENV FAKE_TELEGRAM_LISTEN=0.0.0.0:8081
EXPOSE 8081
# Exec form: distroless has no shell.
HEALTHCHECK --interval=2s --timeout=3s --start-period=5s --retries=3 \
    CMD ["/usr/local/bin/fake-telegram", "health"]
ENTRYPOINT ["/usr/local/bin/fake-telegram"]
CMD []
