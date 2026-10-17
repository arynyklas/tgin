# syntax=docker/dockerfile:1
# The official Telegram Bot API server (github.com/tdlib/telegram-bot-api),
# built from source at a pinned commit. Build it from this directory:
#   docker build -t tgin-telegram-bot-api:local docker/telegram-bot-api
# Flags, volumes and security notes: README.md next to this file.

# alpine:3.24.2 (multi-arch index digest), for both stages
FROM alpine:3.24.2@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6 AS builder

RUN apk add --no-cache build-base cmake gperf openssl-dev zlib-dev linux-headers git

# Upstream has no tags or releases: the commit is the version. Raise the two
# together; the build fails when the binary reports another Bot API version.
# The td submodule is pinned by this commit.
ARG TELEGRAM_BOT_API_COMMIT=e3e9dd8e5b3d7ab8537cd5a10dc31d5ffa8f82d1
ARG BOT_API_VERSION=10.3
# Parallel compile jobs; empty means one per CPU. TDLib needs a few GB of
# memory per job, so lower it on a small builder.
ARG BUILD_JOBS=

WORKDIR /usr/src/telegram-bot-api

# A fetch by full SHA (GitHub serves any reachable commit), so a moved
# branch or a rewritten history cannot change what is built.
RUN git init -q . && \
    git remote add origin https://github.com/tdlib/telegram-bot-api.git && \
    git fetch -q --depth 1 origin "${TELEGRAM_BOT_API_COMMIT}" && \
    git checkout -q FETCH_HEAD && \
    test "$(git rev-parse HEAD)" = "${TELEGRAM_BOT_API_COMMIT}" && \
    git submodule update -q --init --recursive --depth 1

RUN cmake -S . -B build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/usr/local && \
    cmake --build build --target telegram-bot-api -j "${BUILD_JOBS:-$(nproc)}" && \
    cmake --install build --strip && \
    test "$(/usr/local/bin/telegram-bot-api --version 2>&1)" = "Bot API ${BOT_API_VERSION}"

# alpine:3.24.2 (multi-arch index digest), as the builder
FROM alpine:3.24.2@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6

ARG TELEGRAM_BOT_API_COMMIT=e3e9dd8e5b3d7ab8537cd5a10dc31d5ffa8f82d1
ARG BOT_API_VERSION=10.3

LABEL org.opencontainers.image.title="telegram-bot-api" \
      org.opencontainers.image.description="The Telegram Bot API server, built by tgin from the upstream source" \
      org.opencontainers.image.source="https://github.com/arynyklas/tgin" \
      org.opencontainers.image.licenses="BSL-1.0" \
      org.opencontainers.image.version="${BOT_API_VERSION}" \
      org.opencontainers.image.revision="${TELEGRAM_BOT_API_COMMIT}" \
      org.opencontainers.image.base.name="docker.io/library/alpine:3.24.2" \
      io.github.tdlib.telegram-bot-api.commit="${TELEGRAM_BOT_API_COMMIT}" \
      io.github.tdlib.telegram-bot-api.bot-api-version="${BOT_API_VERSION}"

RUN apk add --no-cache libstdc++ openssl zlib && \
    addgroup -S -g 10001 telegram-bot-api && \
    adduser -S -D -H -h /var/lib/telegram-bot-api -s /sbin/nologin -u 10001 -G telegram-bot-api telegram-bot-api && \
    install -d -m 0700 -o 10001 -g 10001 /var/lib/telegram-bot-api /var/lib/telegram-bot-api/files

COPY --from=builder /usr/local/bin/telegram-bot-api /usr/local/bin/telegram-bot-api

USER 10001:10001
# The server's working directory (`--dir`, default `./`): the bots' TDLib
# databases, the update queue and the webhook settings. `files` is where the
# README points `--files-dir`, the directory tgin reads files from.
WORKDIR /var/lib/telegram-bot-api
VOLUME ["/var/lib/telegram-bot-api"]
# The Bot API listener (`--http-port`). The statistics port is off unless
# `--http-stat-port` is given; never publish either.
EXPOSE 8081

# No credentials here: pass --api-id/--api-hash, or TELEGRAM_API_ID and
# TELEGRAM_API_HASH, at run time. No HEALTHCHECK: the server has no health
# endpoint and its port is a flag; compose checks the listener instead.
ENTRYPOINT ["telegram-bot-api"]
CMD []
