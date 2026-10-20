# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

tgin 1.0: a new engine, config and HTTP surface. It began as a fork of
[chesnokpeter/tgin](https://github.com/chesnokpeter/tgin).

### Added

- Several bots per process, each with its own token, Bot API server and
  ingress (long poll or webhook), and per-bot routing rules on update type,
  chat, chat type, user, command, text and callback data. Targets send an
  update to consumers, to `all` of a group, or to one of a `split`
  (`round_robin`, sticky `hash_chat`, or `weighted`).
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
  `/c/<consumer>/`. Calls and file downloads stream to the bot's Bot API
  server in both directions with their `Content-Length`, through header
  allowlists, without a total timeout (`gateway.upstream_idle_timeout`
  bounds the time without progress while a body moves). `getUpdates`,
  `getMe`, `setWebhook`, `deleteWebhook` and `getWebhookInfo` are answered
  by tgin as telegram-bot-api answers them; on `/c/<consumer>/`
  `getUpdates` polls that pull consumer. `logOut` and `close` are refused
  with `400 Bad Request: logOut and close are disabled by tgin; operators
  move bots with tgin admin bot logout|close`.
- Telegram user accounts can be logged in and out: `tgin admin login
  <account> [--code]` draws the login QR in the terminal (renewed by
  itself) and asks for the 2FA password without echo; `tgin admin logout
  <account> --yes` calls `auth.logOut` and clears the session. Through
  `POST /v1/accounts/{name}/login` (`qr` or `code`), `…/login/code`,
  `…/login/password`, `GET …/login` and `POST …/logout`. A login is
  refused with `409` while the account runs; a logout parks a running
  account first (its ingest stops with everything admitted committed, its
  RPC calls get up to 10 s, and it releases its session), then it shows
  `logged_out`, or runs again on its key when Telegram did not confirm the
  logout. With a `phone` in the config, a login by another account is
  logged out again (`wrong_account`). A code login follows Telegram to the
  account's DC (`PHONE_MIGRATE`, `NETWORK_MIGRATE`, `USER_MIGRATE`).
  `tgin admin login` refuses plain `http` to a host other than loopback
  (exit code 64) unless `--insecure-http` is given, since the code and the
  2FA password would cross the network in clear text.
- Logged-in accounts run: each holds its session on its home DC, catches
  up with `getDifference` (channels only when they show a gap), hands its
  updates to consumers and serves `POST /mtproto/v1/{account}/invoke`. A
  panic restarts the account after 1 s, doubling to 60 s; an account that
  loses its authorization (a 401, a transport −404, or
  `AUTH_KEY_DUPLICATED` when a copied session directory was used from two
  places) goes `needs_login`, and of two accounts logged in
  as the same user one runs and the other goes `failed`
  (`duplicate_account`) until the first gives the user up. Telegram's
  flood waits on catch-up calls are waited out (`flood_wait`). An account starts
  once a login of it ends, without a restart. A configured account keeps
  tgin running when every bot has failed for good. `GET /v1/accounts` gives
  each account's state (with `session_held_by: login|logout` while one
  holds a logged-out account's session), layer, home DC and the boxes
  catching up;
  `tgin_mtproto_account_state`, `tgin_mtproto_updates_total`,
  `tgin_mtproto_updates_dropped_total`, `tgin_mtproto_rpc_total` and
  `tgin_mtproto_rpc_seconds` count them (no series without accounts). A
  reload restarts an account whose `api_id`, `api_hash` or `session_dir`
  changed and applies its `rpc` settings in place; the shutdown waits for
  RPC calls in flight.
- `accounts` in the config: Telegram user accounts by name, each with
  `api_id` (`${env:…}` and `${file:…}` allowed), `api_hash`, a
  `session_dir` of its own, an optional `phone`, `rpc` (`max_rps`,
  `flood_sleep_threshold`, `clients` with a token, `allow`/`deny` method
  globs and `allow_paid_stars`, off by default and warned about when on)
  and `routes`. Account rules match `constructors`,
  `peer_ids` (Bot API form), `user_ids`, `commands` (unsuffixed only) and
  `text`, and target consumers like bot rules.
- Account updates reach consumers as envelope v1: JSON with `update_id`
  (strictly increasing per account, never repeated, not contiguous, unique
  per `session_dir`), `account`, `layer`, `constructor`, `received_at` and
  the update's TL, users and chats in base64 (the entities Telegram sent
  with the update's container or difference slice; rely only on those the
  update references); `render: tl_json` adds the update as TL-JSON.
  A pull consumer an account feeds is served on
  `/c/<consumer>/mtproto/v1/getUpdates` and needs `auth_token`; a consumer
  takes bot updates or account updates, never both, and a reload that
  would switch one is refused.
- tgin keeps each account's update state itself, with gap recovery by
  `getDifference` and `getChannelDifference`: sequenced updates of the
  common box reach `backpressure` consumers at least once while the
  process lives (channel boxes: to be confirmed; see DOCS § Delivery),
  a refused batch is fetched again after a backoff, and there is no
  dedup. Each account serves one layer, 230; a change of that layer ships
  only in a minor release, and the CHANGELOG says so.
- `POST /mtproto/v1/{account}/invoke` runs one raw-TL (or TL-JSON) call as
  the account for an RPC client: `X-Tgin-Layer` must be the account's
  layer (230); `invokeWithLayer` and `initConnection` are stripped, other
  wrappers refused; the update methods, the `auth.*` methods that mint or
  move an authorization, `bots.exportBotToken` and
  `phone.getGroupCallStreamRtmpUrl` are always refused. Globs grant only
  methods of reviewed namespaces that change nothing account-wide; every
  other method, any namespace a later layer adds included, needs an exact
  `allow` entry, spending methods such as `payments.*`, `wallet.*` and
  `messages.sendPaidReaction` among them, as are a channel's deletion,
  usernames and conversion, `stickers.deleteStickerSet`, the secret-chat
  methods and the session listings `account.getAuthorizations` and
  `account.getWebAuthorizations`; the loader warns about `allow: ["*"]`.
  A call that agrees to pay
  through a paid-call marker (`allow_paid_stars` or `allow_paid_floodskip`
  on the function, a priced `suggestedPost` or an `inputMediaStakeDice`
  anywhere in it) also needs the client's `allow_paid_stars: true` (403
  `paid_stars_not_allowed`), and one that
  writes quick replies the exact `messages.editQuickReplyShortcut` grant
  (403 `quick_replies_not_allowed`). Flood waits up to
  `flood_sleep_threshold` are slept out once, longer ones answer `429`
  with `Retry-After`, and tgin remembers them per method (slow mode per
  method and chat), answering early retries itself until they end. A call
  written without an answer gets `502 no_answer` and is never resent; an
  account that cannot call answers `503 account_unavailable`.
- `examples/mtproto`: a Telethon adapter for the RPC endpoint and an
  account's pull consumer.
- Self-hosted Bot API servers: `upstreams` names Bot API servers (`kind:
  cloud` or `local`, `url`, `local_mode`, `files`, `response_timeout`), and
  a bot picks one with `upstream`; `api_url` stays the shorthand for an
  unnamed `cloud` upstream. An upstream (or `files.url`) reached over plain
  `http` on a host that is not loopback loads with a warning; on an
  `api_url`, the warning adds that a self-hosted server belongs under
  `upstreams` with `kind: local`.
- File serving for `local` upstreams: from the server's `--files-dir`
  mounted in tgin (`files.dir`, with `server_dir` for the server's own
  path) or from an HTTP origin (`files.url`, bounded by
  `upstream_idle_timeout`, its failures answered `502 Bad Gateway: the
  file server is unreachable` or `… did not answer`), each bot only from
  its own directory and only below a directory in it, never its top
  level; in local mode, `getFile`'s absolute paths come back relative, so
  frameworks download through tgin. A `local` upstream without `files`
  answers downloads with `404 Not Found: files are not served for this
  upstream`. Downloads from disk hold the shutdown drain open.
- Local file references: for a bot on any Bot API server but the Telegram
  cloud (an upstream URL `https://api.telegram.org`; plain `http` to that
  host counts as any other server), the `api_url` shorthand and
  `kind: cloud` upstreams included, and for every `kind: local` upstream,
  `local_mode` or not, the gateway and webhook
  replies refuse any parameter that begins with `file:` (query string,
  JSON at any depth, urlencoded values, multipart text fields read as
  telegram-bot-api reads them) with `400 Bad Request: local file uploads
  are disabled by tgin`, unless the bot sets `allow_local_files: true`.
  Such bots' JSON and urlencoded bodies are read whole up to 64 KiB and
  scanned as they stream above it, up to 1 MiB (`413` above). A
  self-hosted server reached through `api_url` is protected too; it needs
  `upstreams` with `kind: local` for its file routes and the `getFile`
  rewrite.
- `response_timeout` per upstream (`1m` for `cloud`, `10m` for `local`, at
  most `1h`) bounds the wait for the server's answer once a request was
  sent; on the `api_url` shorthand that wait is the bot's
  `upstream_idle_timeout`. Webhook replies wait the smaller of
  `response_timeout` and 30 s.
- Webhooks from a self-hosted server in local mode: `public_url` may be
  `http` on any port, and `max_connections` up to 100 000; a value above
  100 (the local-mode range) must not exceed `server.max_connections`.
  `ip_allowlist: telegram` on a `local` upstream is a config error.
- `setWebhook` registration retries in the background (`register:<bot>`,
  1 s to 5 min, honouring `retry_after`) until it succeeds or the bot
  stops; a cloud server's `401`, `403` or `404` ends it. On a `local`
  upstream, a `401` or `404` (the server's own api_id or api_hash
  rejected) is retried by registration and by the long poll alike.
- Moving bots between Bot API servers: `tgin admin bot logout|close <bot>
  --yes [--timeout D]` and `POST /v1/bots/{name}/logout|close` drain the
  server and tgin's queues of the bot, stop sending its calls to any
  server and wait for those being sent, then call `logOut` (or
  `deleteWebhook` and `close`) on its server and park it until a reload
  changes its upstream. The timeout, at most 1 h, runs from the request;
  moves run with reloads, before any reload that has not begun; a move
  whose requester leaves before the server has run dry is dropped.
  Failures answer `504 upstream not drained`, `504 calls in flight` or
  `502 upstream refused` with the server's masked description; after a
  failed `close`, a `register: false` bot's webhook is set again.
- Parked bots: every call tgin would send for a parked bot to a Bot API
  server or file origin gets `503 Service Unavailable: the bot is being
  moved to another Bot API server by tgin` with `Retry-After: 5`, counted
  `rejected`, and its webhook replies are refused; intercepts, pull
  consumers and files served from disk keep working.
- `tgin_bot_info{bot,upstream}` and `tgin_upstream_up{bot}` metrics;
  `upstream` and `parked` in `/v1/status` sources, an `UPSTREAM` column in
  `tgin admin status`, and `upstream_changed` in the reload report, with a
  warning about split updates when a bot's upstream changes, unless a
  move parked the bot first.
- The admin API, the probe bodies and the metrics follow a compatibility
  rule: minor releases only add fields, enum values, metric families and
  error codes, and clients ignore what they do not know.
- Per-bot rate limits in the gateway (`gateway.rate_limit`): global,
  per-chat and per-group quotas for `send*`, `copyMessage*` and
  `forwardMessage*`, shaped up to `max_delay` or refused with Telegram's
  `429`; and one transparent retry of a small request after the Bot API
  server's `429` (`gateway.retry_429.max_wait`).
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
  `tgin_gateway_upstream_seconds` (buckets from 1 ms to about 524 s),
  `tgin_gateway_ratelimited_total`, `tgin_gateway_retries_total`,
  `tgin_gateway_unauthorized_total` and `tgin_webhook_replies_total`.
- An end-to-end suite (`cargo xtask e2e`, run in CI) with aiogram,
  python-telegram-bot, grammY and Telegraf against tgin and a fake Bot
  API, a self-hosted scenario against the fake in local mode (files from
  disk, webhooks over plain http, `file:` refusals), and a manual check
  against the real Bot API (`tests/smoke/real_telegram.py`, with
  `--self-hosted`, `--migrate` and `--fake` for a self-hosted server).
- Bounded queues per consumer (`capacity`, `capacity_bytes`) with
  `on_full: backpressure` or `drop_oldest`, and `ordering: per_chat`.
- Webhook ingress with a secret derived from the bot token by default,
  `setWebhook` registration, an IP allowlist (including Telegram's
  networks) and trusted proxies for `X-Forwarded-For`.
- Hot reload on file change, `SIGHUP` and `POST /v1/reload`: rules,
  consumers and bots change live; only bots whose token, Bot API server or
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
- Exit codes: 2 when every source failed (the Bot API server rejected the
  token of every long-poll bot, no webhook bot exists and no bot is
  parked), 64 for a malformed command line, 130 for a second shutdown
  signal.
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
- `docker/telegram-bot-api`: an image of the official Telegram Bot API
  server (Bot API 10.3), built from a pinned upstream commit on Alpine,
  running as `10001:10001` with no credentials inside; CI builds it on amd64
  and arm64 with an SBOM and provenance, and does not publish it.
  `examples/self-hosted` runs tgin in front of it with Compose, serving
  the bot's files from the server's volume, and with `compose.fake.yaml`
  against the fake Bot API in local mode instead.
- `tgin` and `tgin-botapi` are licensed `MIT AND BSL-1.0`: they include a
  port of TDLib's `HttpReader` under the Boost Software License 1.0
  (`LICENSES/BSL-1.0.txt`). The other crates stay MIT.
- The minimum supported Rust version is 1.89.
- `tgin-mtproto`, the user-account engine, is the only crate that may
  depend on grammers (`cargo xtask grammers-confinement` checks it in CI).

### Changed

- A token shared by two bots is a config error, reported at the second
  bot without quoting the token.
- A webhook `public_url` on a `cloud` upstream (or the `api_url`
  shorthand) on a port other than 443, 80, 88 or 8443 is a config error at
  load; Telegram never posted to such ports.
- Downstream `logOut` and `close` answer `400` instead of `true`: moving a
  bot is the operator's `tgin admin bot logout|close`.
- A gateway call that gets no answer is answered `502 Bad Gateway: the Bot
  API server is unreachable` when the server never had the whole request
  (its connect never finished, or flood control refused it), and `502 Bad
  Gateway: the Bot API server did not answer; the call may have been
  executed` once it had.

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
  gets `401`, a malformed one `404`, and neither reaches the Bot API
  server. Gateway paths and upstream URLs, which carry tokens, are never
  logged or used as metric labels.
- tgin serves a bot's files only from that bot's own directory on the Bot
  API server: traversal, encoded traversal and symbolic links that leave
  it get `404`, and a `getFile` path from another bot's directory is never
  rewritten. Bot directory names (the token, with `:` or `~`) and the
  `%3A` forms of each token are masked in logs like the token. A
  `files.url` origin's access logs record every bot's token.
- Accounts' `api_hash`es, phones and RPC client tokens are masked in logs
  like other secrets; login URLs, codes, passwords and RPC bodies are
  never logged. The account login routes need `admin.token` even on a
  loopback listener, and their answers carry `Cache-Control: no-store`.
  An account's session directory is created `0700`, its files `0600`
  (Unix), under a lock that one process holds.
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
- grammers 0.11 (`grammers-crypto`, `grammers-mtproto`, `grammers-tl-types`,
  exact pins) and redb 4.1 for the account session store, in
  `tgin-mtproto` only.

### Docs

- README, DOCS and AGENTS describe tgin 1.0, including the Bot API gateway,
  self-hosted Bot API servers (upstreams, file modes, `file:` references,
  moving bots, running a server) and how to point aiogram,
  python-telegram-bot, grammY, Telegraf and go-telegram-bot-api at it;
  generated config reference; PERF.md compares
  tgin 1.0 with the original engine (`4f0971a`) and lists the changes
  behind the difference.
- DOCS describes user accounts over MTProto: login, the session
  directory, envelope v1, the RPC contract, one layer per account, the
  delivery claim, account rules, metrics and Telegram's terms.
- Added `SECURITY.md` (supported versions, private reporting, verifying
  release attestations) and this changelog.

[Unreleased]: https://github.com/arynyklas/tgin/compare/4f0971a...HEAD
