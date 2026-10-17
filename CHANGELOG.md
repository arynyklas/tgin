# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
  `X-Tgin-Source`, `X-Tgin-Update-Type`, `X-Tgin-Attempt` and `Via`
  headers.
- A Bot API gateway on the data plane for the bots behind tgin:
  `/bot<token>/<method>`, `/file/bot<token>/<file_path>` and the same under
  `/c/<consumer>/`. Calls and file downloads stream to the bot's `api_url`
  in both directions with their `Content-Length`, through header
  allowlists, without a total timeout (`gateway.upstream_idle_timeout`
  bounds the time without progress). `getUpdates`, `getMe`, `setWebhook`,
  `deleteWebhook`, `getWebhookInfo`, `logOut` and `close` are answered by
  tgin as telegram-bot-api answers them; on `/c/<consumer>/` `getUpdates`
  polls that pull consumer.
- Per-bot rate limits in the gateway (`gateway.rate_limit`): global,
  per-chat and per-group quotas for `send*`, `copyMessage*` and
  `forwardMessage*`, shaped up to `max_delay` or refused with Telegram's
  `429`; and one transparent retry of a small request after Telegram's
  `429` (`gateway.retry_429.max_wait`).
- Webhook replies: a push endpoint's `2xx` answer carrying a Bot API call
  is made through the gateway before the update is acknowledged
  (`webhook_reply: execute`, the default), with Telegram's refusal rule
  and the bot's rate limits.
- `accept_set_webhook` on push consumers fed by one bot: the bot's
  `setWebhook` on the consumer's gateway path sets a runtime endpoint
  (`"runtime": true` in `/v1/status`, `endpoint="runtime"` in metrics), and
  a push consumer without endpoints queues until one arrives.
  `honor_drop_pending` lets `drop_pending_updates` drop the calling bot's
  queued updates from the calling consumer; other bots' updates stay.
- Gateway metrics: `tgin_gateway_requests_total`,
  `tgin_gateway_upstream_seconds`, `tgin_gateway_ratelimited_total`,
  `tgin_gateway_retries_total`, `tgin_gateway_unauthorized_total` and
  `tgin_webhook_replies_total`.
- An end-to-end suite (`cargo xtask e2e`, run in CI) with aiogram,
  python-telegram-bot, grammY and Telegraf against tgin and a fake Bot
  API, and a manual check against the real Bot API
  (`tests/smoke/real_telegram.py`).
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
  serves webhooks, pull consumers and the Bot API gateway, the admin plane
  (`admin.listen`, default `127.0.0.1:9090`) serves `/livez`, `/readyz`,
  `/metrics` and the admin API.
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
- The gateway forwards a request only when its token equals a configured
  bot's token, compared in constant time with every one; an unknown token
  gets `401`, a malformed one `404`, and neither reaches Telegram. Gateway
  paths and upstream URLs, which carry tokens, are never logged or used as
  metric labels.
- tgin never acts on its own pushes: each push carries tgin's `Via`, and
  both planes answer `508 Loop Detected` to a request carrying it, so a
  push endpoint (a `setWebhook` runtime endpoint included) cannot make tgin
  call itself.
- A runtime endpoint is cleared by a reload that turns
  `accept_set_webhook` off, moves the consumer to another bot, or changes
  that bot's token.

### Dependencies

- Rust dependencies were updated, including axum 0.8, axum-server 0.8 and
  reqwest 0.13. reqwest is built with rustls only, so OpenSSL, native-tls
  and ring are out of the build graph; one aws-lc-rs provider serves the
  HTTP client and the TLS listener.
- CI runs `cargo deny check` on every push and pull request.

### Docs

- README, DOCS and AGENTS describe tgin 1.0, including the Bot API gateway
  and how to point aiogram, python-telegram-bot, grammY, Telegraf and
  go-telegram-bot-api at it; generated config reference; PERF.md compares
  tgin 1.0 with the original engine (`4f0971a`) and lists the changes
  behind the difference.
- Added `SECURITY.md` (supported versions, private reporting, verifying
  release attestations) and this changelog.

[Unreleased]: https://github.com/arynyklas/tgin/compare/4f0971a...HEAD
