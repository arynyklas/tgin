# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Until 1.0, minor releases (0.x.0) may contain breaking changes.

## [Unreleased]

tgin 1.0: a new engine, config and HTTP surface. It began as a fork of
[chesnokpeter/tgin](https://github.com/chesnokpeter/tgin).

### Added

- Several bots per process, each with its own token, `api_url` and ingress
  (long poll or webhook), and per-bot routing rules on update type, chat,
  chat type, user, command, text and callback data. Targets send an update to
  consumers, to `all` of a group, or to one of a `split` (`round_robin`,
  sticky `hash_chat`, or `weighted`).
- Pull consumers: a Telegram-shaped `getUpdates` on `/c/<name>/getUpdates`
  and on alias paths, with leases acknowledged by `offset`, redelivery after
  `lease_timeout`, instances named by `X-Tgin-Instance` or client address,
  and an optional per-consumer token.
- Push consumers: delivery to one or more endpoints with `round_robin`,
  `hash_chat` or `least_in_flight` balancing, retries with backoff and
  `Retry-After`, dead letters, a circuit breaker per endpoint, and
  `X-Tgin-Source`, `X-Tgin-Update-Type` and `X-Tgin-Attempt` headers.
- Bounded queues per consumer (`capacity`, `capacity_bytes`) with
  `on_full: backpressure` or `drop_oldest`, and `ordering: per_chat`.
- Webhook ingress with a secret derived from the bot token by default,
  `setWebhook` registration, an IP allowlist (including Telegram's
  networks) and trusted proxies for `X-Forwarded-For`.
- Hot reload on file change, `SIGHUP` and `POST /v1/reload`: rules,
  consumers and bots change live; only bots whose token, `api_url` or
  ingress changed restart; removed consumers drain; settings that need a
  restart are reported.
- Admin API v1: `/v1/status`, `/v1/config` (secrets redacted),
  `/v1/reload`, and pause, resume, drain and purge per consumer; and
  `tgin admin`, its command-line client (`status`, `reload`, `consumers`,
  `pause`, `resume`, `drain`, `purge`, `health`).
- `tgin config validate`, `print` and `schema`; config errors
  carry YAML path, line and column; a JSON Schema
  (`schema/tgin.v1.schema.json`) and a generated reference
  (`docs/config-reference.md`).
- Metrics for sources, routing, consumer queues, deliveries, reloads and
  shutdown, plus `tgin_build_info`.
- TLS on the data plane with certificate reload; listener limits
  (`server.max_connections`, `server.header_read_timeout`).
- Graceful shutdown with a lame duck and a drain, on SIGTERM and SIGINT and
  on Windows console events (Ctrl+C, Ctrl+Break, console close, system
  shutdown); a second signal exits at once with code 130.
- `tgin version` and `tgin --version`.
- Release binaries for Linux (glibc and static musl), macOS and Windows on
  x86_64 and aarch64, with shell and PowerShell installers, SHA-256
  checksums, a CycloneDX SBOM, embedded dependency metadata
  (`cargo auditable`) and build provenance attestations.
- Multi-arch (amd64, arm64) container image at `ghcr.io/arynyklas/tgin`,
  tagged `<version>`, `<major>.<minor>` and, for the highest stable release
  only, `latest`, with a build provenance attestation.
- A YAML config (`version: 1`), read from `tgin.yaml`, `-c PATH` or
  `TGIN_CONFIG`; unknown keys are errors. References in it
  (`${env:NAME}`, `${file:PATH}`) are expanded only in secrets, URLs and
  file paths; relative file paths resolve against the config file's
  directory.
- Two listeners: the data plane (`server.listen`, default `0.0.0.0:8080`)
  serves webhooks and pull consumers, the admin plane (`admin.listen`,
  default `127.0.0.1:9090`) serves `/livez`, `/readyz`, `/metrics` and the
  admin API.
- Exit codes: 2 when every source failed (Telegram rejected the token of
  every long-poll bot and no webhook bot exists), 64 for a malformed
  command line, 130 for a second shutdown signal.
- Long-poll bots delete an active webhook before polling
  (`delete_webhook_on_start`, on by default), keeping pending updates.
- The container image is a static musl binary on distroless, without a
  shell, running as the numeric user and group `65532:65532` (mounted
  config files, secrets and TLS keys must be readable by that uid). It
  reads `/etc/tgin/tgin.yaml` (`TGIN_CONFIG`), declares ports 8080 and
  9090, has a `HEALTHCHECK` running `tgin admin health`, and its
  `ENTRYPOINT` is the tgin binary.
- Linux musl builds (the static release archives and the container image)
  use mimalloc as the global allocator.
- Shutdown takes up to about 22 s with the defaults; give the container a
  longer stop timeout than Docker's 10 s (`docker run --stop-timeout 30`,
  Compose `stop_grace_period: 30s`, Kubernetes
  `terminationGracePeriodSeconds: 30`).
- The minimum supported Rust version is 1.89.

### Security

- Secrets are masked in every log line, including panics, as are strings
  shaped like bot tokens; secrets of earlier configs stay masked after a
  reload.
- `/v1/config`, `/v1/status` and `tgin config print` never show secrets,
  and show URLs only as `scheme://host[:port]`. Metric labels carry no URLs.
- The admin API requires `admin.token` unless every admin listener is on
  loopback; removing the token by a reload while a non-loopback listener
  is bound refuses every protected request until restart.
- `tgin admin` reads the admin token only from a file and never sends it
  through an HTTP proxy.
- Webhook secrets are checked in constant time before the request body is
  read.
- Outgoing requests never follow redirects; a push endpoint answering with
  a redirect dead-letters the update.
- Config errors never quote secret values or the source line.

### Dependencies

- Rust dependencies were updated, including axum 0.8, axum-server 0.8 and
  reqwest 0.13. reqwest is built with rustls only, so OpenSSL, native-tls
  and ring are out of the build graph; one aws-lc-rs provider serves the
  HTTP client and the TLS listener.
- CI runs `cargo deny check` on every push and pull request.

### Docs

- README, DOCS and AGENTS describe tgin 1.0; generated config reference;
  PERF.md compares tgin 1.0 with the original engine (`4f0971a`) and lists
  the changes behind the difference.
- Added `SECURITY.md` (supported versions, private reporting, verifying
  release attestations) and this changelog.

[Unreleased]: https://github.com/arynyklas/tgin/compare/4f0971a...HEAD
