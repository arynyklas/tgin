# tgin documentation

## Contents

- [Overview](#overview)
- [Concepts](#concepts)
- [Delivery semantics](#delivery-semantics)
- [Quick start](#quick-start)
- [Configuration reference](#configuration-reference)
  - [Top-level fields](#top-level-fields)
  - [Update providers (`updates`)](#update-providers-updates)
  - [Routing targets (`route`)](#routing-targets-route)
  - [Load balancers](#load-balancers)
  - [Sizing](#sizing)
- [HTTP management API](#http-management-api)
- [Observability](#observability)
- [TLS setup](#tls-setup)
- [Lifecycle & shutdown](#lifecycle--shutdown)
- [Running in Docker](#running-in-docker)
- [Configuration example](#configuration-example)
- [Further reading](#further-reading)

## Overview

tgin is a routing layer between the Telegram Bot API and one or more downstream bot instances. It ingests updates over long poll or webhook, dispatches them through a tree of routes and load balancers, and forwards each update to its destination. Configuration is a single RON file (default: `tgin.ron`); routes can be added and removed at runtime through an optional HTTP API, and the HTTP listener can serve TLS.

tgin is pre-1.0: until 1.0, a minor release may contain breaking changes ([CHANGELOG.md](CHANGELOG.md)).

## Concepts

| Term             | Direction | Purpose                                                                                                         |
|------------------|-----------|-----------------------------------------------------------------------------------------------------------------|
| `LongPollUpdate` | ingress   | Polls Telegram's `getUpdates` and feeds every update into the routing tree.                                     |
| `WebhookUpdate`  | ingress   | Exposes an HTTP endpoint that Telegram POSTs updates to. Optional self-registration via `setWebhook`.           |
| `LongPollRoute`  | egress    | Buffers updates in memory and hands each one out once through a Telegram-shaped `getUpdates` endpoint.          |
| `WebhookRoute`   | egress    | POSTs each update to a downstream HTTP endpoint, once.                                                          |
| `RoundRobinLB`   | egress    | Rotates through child routes; each update goes to exactly one child.                                            |
| `AllLB`          | egress    | Sends every update to every child concurrently.                                                                 |

Updates flow in one direction. Every ingress provider pushes the update's original JSON bytes into one bounded dispatch queue (1024 updates). `dark_threads` workers take updates from that queue one at a time and run each through the root `route` to completion before taking the next. When the queue is full, ingress waits: a long-poll updater stops calling `getUpdates` until there is room, and a webhook request is answered only once its update is queued.

Errors on the egress path are logged and counted, never retried. A failing downstream does not stop the dispatcher, but a slow one holds a worker: a `WebhookRoute` keeps its worker busy until the downstream answers or `request_timeout_ms` expires, so `dark_threads` slow downstream requests in flight stall all dispatch.

## Delivery semantics

- **Between tgin and downstream bots, delivery is at most once.** A `WebhookRoute` sends each update once, with no retries. A `LongPollRoute` removes updates from its buffer when it returns them in a `getUpdates` response; if that response never reaches the bot (the bot crashed mid-request, the connection dropped), those updates are lost.
- **Webhook ingress acknowledges on enqueue.** tgin answers Telegram `200` once the update is in the dispatch queue, before any downstream has it. Telegram does not redeliver an acknowledged update.
- **Long-poll ingress can redeliver after a stop in a narrow window.** tgin confirms a batch to Telegram with its next `getUpdates` call, whose `offset` is past the batch. If tgin stops after receiving a batch but before that next call (for example while it waits for room in a full dispatch queue, during `default_timeout_sleep`, or because it was killed in that window), the batch is not confirmed and Telegram returns it on the next start, so downstreams can see those updates twice.
- **Everything buffered lives in process memory.** If the process is killed rather than drained, updates in the dispatch queue and in long-poll route buffers are lost. See [Lifecycle & shutdown](#lifecycle--shutdown) for what a graceful stop delivers.

## Quick start

```bash
git clone https://github.com/arynyklas/tgin.git
cd tgin
cargo build --release
TOKEN=123456:ABC TGIN_AUTH_TOKEN=$(openssl rand -hex 32) ./target/release/tgin -f tgin.ron
```

The shipped `tgin.ron` reads the bot token from `TOKEN` and, because it enables the management API, the API secret from `TGIN_AUTH_TOKEN`. Building requires Rust 1.88 or newer and a C compiler for `aws-lc-sys` (the TLS crypto backend).

The `-f`/`--file` flag selects the configuration file (defaults to `tgin.ron`); `--version` prints the tgin version. `${VAR}` placeholders inside the config are substituted from the environment **before** the RON parser runs, everywhere in the file, including comments. Every unset variable is listed on stderr and tgin exits with code `1`.

Prebuilt release archives and the container image are listed in the [README](README.md#install-a-release); [SECURITY.md](SECURITY.md#verifying-releases) shows how to verify their build provenance.

## Configuration reference

Top-level structure loaded from `tgin.ron` (`src/config/schema.rs`).

The schema is strict: an unknown field anywhere in the file (top level, inside a variant such as `LongPollUpdate(...)`, or in a nested struct) is a parse error that names the field, so a typo like `server_prot` fails startup instead of silently falling back to a default. After parsing, tgin validates the whole config and reports every problem it finds in one message (exit code `1`).

### Top-level fields

| Field          | Type                              | Default   | Description |
|----------------|-----------------------------------|-----------|-------------|
| `dark_threads` | `usize`                           | `4`       | Tokio worker threads, and the number of dispatch workers (one update in flight per worker). Must be at least `1`. |
| `server_port`  | `Option<u16>`                     | `None`    | When set, tgin serves every HTTP-facing component (webhook ingress, long-poll routes, probes, `/status`, `/metrics`, management API) on `0.0.0.0:<port>`. Without it there is no HTTP listener, so a config with a `WebhookUpdate`, a `LongPollRoute` or `api` fails validation (`RequiresServerPort`); long-poll ingress feeding `WebhookRoute`s is the only shape that runs without it. |
| `ssl`          | `Option<SslConfig { cert, key }>` | `None`    | TLS certificate and private key (PEM file paths). When set, the listener serves HTTPS only, via rustls. |
| `updates`      | `Vec<UpdateConfig>`               | required  | Ingress providers. Multiple providers can coexist; their updates share the dispatch queue. |
| `route`        | `RouteConfig`                     | required  | The single root of the routing tree: a leaf route or a load-balancer subtree. |
| `api`          | `Option<ApiConfig { base_path }>` | `None`    | Management API base path (e.g. `/api`), nested on the same listener. Requires a non-blank `auth_token` (`ApiWithoutAuthToken` validation error otherwise). |
| `log_level`    | `String`                          | `"info"`  | Minimum log level: `trace`/`debug`/`info`/`warn`/`error`/`off` (case-insensitive). `RUST_LOG` overrides it. |
| `log_format`   | `Compact` \| `Json`               | `Compact` | `Compact` = human-readable single-line logs; `Json` = one JSON object per line for log shippers. |
| `auth_token`   | `Option<String>`                  | `None`    | Shared secret for the control / observability plane. When set, `/status`, `/metrics` and the management API require `Authorization: Bearer <auth_token>`. **Required when `api` is set.** Without `api`, a present-but-blank value is ignored with a startup warning (auth stays disabled). |

### Update providers (`updates`)

#### `LongPollUpdate`

Long-polls Telegram with `GET getUpdates` and dispatches each update into the routing tree.

| Field                   | Type                  | Default  | Description |
|-------------------------|-----------------------|----------|-------------|
| `token`                 | `String`              | required | Telegram bot token (e.g. `123456:ABC...`). Builds the default poll URL `https://api.telegram.org/bot<token>/getUpdates`. |
| `url`                   | `Option<String>`      | `None`   | Full `getUpdates` URL that replaces the default one, including the `bot<token>` segment. `token` is then not used to build the URL. |
| `default_timeout_sleep` | `u64` (milliseconds)  | `0`      | Pause after every successful `getUpdates` call. `0` polls again immediately. |
| `error_timeout_sleep`   | `u64` (milliseconds)  | `0`      | Base of the retry backoff after a failed call; values below 50 ms are raised to 50 ms. See failure handling below. |
| `long_poll_timeout`     | `u64` (seconds)       | `30`     | `timeout` passed to `getUpdates` (server-side hold). Clamped to at most 50. The HTTP request deadline is this value plus 10 s. |
| `long_poll_limit`       | `u64`                 | `100`    | `limit` passed to `getUpdates`. Clamped to 1–100. |

To poll a [local Bot API server](https://github.com/tdlib/telegram-bot-api) instead of `api.telegram.org`, give the full URL:

```ron
LongPollUpdate(
    token: "${TOKEN}",
    url: Some("http://telegram-bot-api:8081/bot${TOKEN}/getUpdates"),
),
```

Failure handling distinguishes transient from permanent upstream failures:

- **Transient** (network errors, 5xx and other non-success statuses, body-read errors, JSON-parse errors): retry after a full-jitter exponential backoff. The base is `error_timeout_sleep` (at least 50 ms); it doubles after each consecutive failure up to a 1 s cap, and the actual sleep is a random value between 0 and the current backoff. A successful call resets it. After 60 consecutive transient failures the updater logs one `long-poll prolonged outage` warning, then keeps retrying; it never gives up on transient errors.
- **429 Too Many Requests**: tgin sleeps for the `Retry-After` response header (seconds) when present, otherwise for the jittered backoff above. 429 does not count towards the outage warning.
- **409 Conflict**: if Telegram says a webhook is active on the token, tgin calls `deleteWebhook` (keeping pending updates) and polls again; it does this at most once per 60 s, and only when the poll URL ends in `/getUpdates` (the `deleteWebhook` URL is derived from it). Otherwise — a webhook re-set within 60 s, a custom URL, or another process polling the same token — tgin logs the cause and retries after 5 s. Telegram allows one `getUpdates` poller per token; see [one replica per token](#deployment-rules).
- **Permanent** (401 Unauthorized, 403 Forbidden, 404 Not Found): the token is wrong, revoked or malformed, and retrying cannot fix it. The updater logs a `long-poll permanent failure` error, increments a process-wide counter, and stops. Once every long-poll updater has stopped this way and no `WebhookUpdate` is configured, nothing can deliver updates any more: tgin starts a graceful shutdown within about a second and exits with code `2` after the drain (see [Lifecycle & shutdown](#lifecycle--shutdown)).

#### `WebhookUpdate`

Exposes `POST <path>` on `server_port` for Telegram to deliver updates to.

| Field          | Type                                | Default  | Description |
|----------------|-------------------------------------|----------|-------------|
| `path`         | `String`                            | required | Local URL path Telegram should hit (e.g. `/bot/pull`). |
| `secret_token` | `Option<String>`                    | `None`   | 1–256 characters of `A-Z`, `a-z`, `0-9`, `_`, `-` (Telegram's rule; checked at startup). When set, every request must carry a matching `X-Telegram-Bot-Api-Secret-Token` header, compared in constant time; otherwise tgin answers `401` without reading the body. When omitted, tgin logs a startup warning: anyone who can reach `path` can inject updates. |
| `registration` | `Option<RegistrationWebhookConfig>` | `None`   | Registers the webhook with Telegram at startup (see below). Omit it to manage the webhook yourself. |

`RegistrationWebhookConfig` fields:

- `public_ip: String` — the public `http(s)://` origin Telegram reaches tgin at (e.g. `"https://example.com"`); tgin appends `path` to it.
- `token: String` — bot token used for the default endpoint `https://api.telegram.org/bot<token>/setWebhook`.
- `set_webhook_url: Option<String>` — full `setWebhook` URL that replaces the default (e.g. for a local Bot API server).

At startup tgin POSTs `setWebhook` with `url = <public_ip><path>` and, when configured, the same `secret_token`, so Telegram sends the header tgin checks. A failed registration is logged and does not stop tgin. tgin never deletes the webhook on shutdown.

Request handling, in order: a wrong or missing secret gets `401`; a body over 64 KiB gets `413`; once ingest has stopped during shutdown, `503` with `Retry-After: 5`; a body that is not valid JSON gets `400`. Otherwise tgin waits for room in the dispatch queue, queues the original bytes, and answers `200`.

### Routing targets (`route`)

#### `LongPollRoute { path, max_buffered_updates? }`

Exposes `POST <path>` as a Telegram-shaped `getUpdates` endpoint that downstream bots poll. Updates dispatched to the route wait in an in-memory FIFO buffer until a poll takes them.

| Field                  | Type     | Default  | Description |
|------------------------|----------|----------|-------------|
| `path`                 | `String` | required | Local URL path the downstream bot polls (e.g. `/bot1/getUpdates`). |
| `max_buffered_updates` | `usize`  | `10000`  | Cap on the buffer (values below 1 become 1). When full, the oldest update is evicted to make room; evicted updates are lost and counted in `queue_dropped_total`. tgin has already confirmed them to Telegram, so they cannot be fetched from Telegram again. |

How a poll is answered:

- **Drain-on-read.** A poll removes up to `limit` updates from the buffer and returns them as `{"ok":true,"result":[...]}`. An update is handed out once; this is at-most-once delivery (see [Delivery semantics](#delivery-semantics)).
- **`offset` is ignored**, and so is `allowed_updates`. Bots may keep sending them, but they do not filter or acknowledge anything. Several bots polling the same path split its updates between them; each update goes to whichever poll takes it first.
- **Parameters** come from the request body, either `application/json` or `application/x-www-form-urlencoded`. An empty body without a JSON content type means all defaults; an empty body sent with `Content-Type: application/json` is malformed JSON and gets the error envelope below. `limit` defaults to 100 and is clamped to 1–100. `timeout` defaults to 0 (answer at once) and is capped at 50 s; with a timeout, a poll on an empty buffer waits until an update arrives or the timeout expires, then answers with an empty `result`.
- **Errors** follow the Bot API convention: a malformed body gets HTTP `200` with `{"ok":false,"error_code":400,...}`. Request bodies are capped at 8 KiB.
- **During shutdown** a poll on an empty buffer is held for at most 500 ms; see [Lifecycle & shutdown](#lifecycle--shutdown).

When the buffer depth first reaches 80 % of `max_buffered_updates`, tgin logs a one-time `long-poll route crossed buffer high-watermark` warning so a downstream that has gone offline does not go unnoticed.

Each `LongPollRoute` reports its state in `GET /api/routes` and `GET /status`:

```json
{
  "type": "longpoll",
  "options": { "path": "/bot1/getUpdates", "max_buffered_updates": 10000 },
  "metrics": { "queue_depth": 7, "queue_dropped_total": 0, "served_total": 1234 }
}
```

#### `WebhookRoute { url, request_timeout_ms? }`

Push-based forwarder: every update is POSTed once to `url` (e.g. `http://internal-bot:8080/bot`) with `Content-Type: application/json` and the original update bytes as the body. There are no retries. A non-2xx answer, a network error or a timeout is logged as a warning and counted (`http_failure_total` / `http_error_total`); the update is then dropped. The response body is ignored, so a bot cannot answer Telegram by replying to the webhook request; it has to call the Bot API itself.

| Field                | Type     | Default  | Description |
|----------------------|----------|----------|-------------|
| `url`                | `String` | required | Absolute `http://` or `https://` downstream URL (checked at startup). |
| `request_timeout_ms` | `u64`    | `10000`  | Per-request deadline in milliseconds. The dispatching worker waits up to this long, so lower it (e.g. `2000`) when downstreams should be fast. |

### Load balancers

Load balancers compose multiple routes and can be nested arbitrarily deep. An empty load balancer in the config is a validation error; one emptied at runtime through the API drops updates and counts them in `dropped_empty_total`.

#### `RoundRobinLB { routes }` (`src/lb/roundrobin.rs`)

Forwards each update to the next child in turn. Children can be heterogeneous: mix `WebhookRoute` and `LongPollRoute` freely. There is no health checking: a child whose downstream is down keeps receiving its share.

#### `AllLB { routes }` (`src/lb/all.rs`)

Sends every update to every child concurrently and returns once all of them have finished, so the slowest child sets the pace of the worker. Useful when several specialized services must each see the full update stream (analytics, moderation, etc.).

### Sizing

- **Memory.** The dominant cost is long-poll route buffers: at most Σ over `LongPollRoute`s of `max_buffered_updates` × update size, plus up to 1024 updates in the dispatch queue. Webhook ingress rejects updates over 64 KiB; long-poll ingress takes whatever Telegram sends. An update sent to several routes by `AllLB` is stored once and shared, so the sum is an upper bound. With the defaults, one route that fills up with 64 KiB updates holds 10 000 × 64 KiB ≈ 625 MiB; lower `max_buffered_updates` if that does not fit.
- **Webhook throughput.** Each worker forwards one update at a time, so a tree of `WebhookRoute`s delivers at most about `dark_threads` ÷ (downstream response time) updates per second.

## HTTP management API

Enable the API by setting `api` in your config, together with the `auth_token` it requires:

```ron
api: Some(ApiConfig(
    base_path: "/api",
)),
auth_token: Some("${TGIN_AUTH_TOKEN}"),
```

Routes are nested under `base_path` and share the listener with the ingress endpoints.

Every HTTP path (`WebhookUpdate.path`, `LongPollRoute.path`, `api.base_path`, and `Longpull` paths added via `POST /api/route`) is matched literally and must start with `/`; paths containing `{` or `}`, or with a segment starting with `:` or `*`, are rejected (an `InvalidPath` validation error at startup, `400` at runtime), and `api.base_path` may not be `/`.

> **Authentication:** every request must carry `Authorization: Bearer <auth_token>` (see [Observability → Authentication](#authentication)); tgin refuses to start with `api` set and `auth_token` missing or blank. These endpoints rewrite the routing tree, so restrict access at the network layer as well.

| Endpoint      | Method   | Body                                     | Description |
|---------------|----------|------------------------------------------|-------------|
| `/api/routes` | `GET`    | —                                        | Returns the current routing tree as JSON, with per-leaf metrics. |
| `/api/route`  | `POST`   | `{ "type": "Webhook"\|"Longpull", ... }` | Adds a route as a child of the top-level load balancer. `Webhook` form: `{"type": "Webhook", "url": "...", "request_timeout_ms": <ms>?}`. `Longpull` form: `{"type": "Longpull", "path": "...", "max_buffered_updates": <n>?}`. Defaults: `request_timeout_ms` 10000, `max_buffered_updates` 10000. |
| `/api/route`  | `DELETE` | `{ "type": "Webhook"\|"Longpull", ... }` | Removes a route anywhere in the tree, matched by `url` (`Webhook`) or `path` (`Longpull`). |

Routes added at runtime live in memory only; a restart reloads the routing tree from the config file. With `api` enabled, a request to a path no route handles is answered by the dynamic long-poll handler with a Bot API-style error envelope (HTTP `200`, `{"ok":false,...}`).

Status codes:

| Status | Meaning |
|--------|---------|
| `200`  | Mutation applied (or, for `GET /routes`, snapshot returned). |
| `400`  | tgin rejected the input: empty `url` or `path`, a `path` that doesn't start with `/` or contains `{`/`}` or a segment starting with `:`/`*`, or a `url` that does not parse or whose scheme is not `http`/`https`. |
| `400` / `415` / `422` | axum rejected the body before tgin saw it: `415` without `Content-Type: application/json`, `400` for malformed JSON, `422` for JSON that does not match the schema (e.g. unknown `type`). |
| `404`  | `DELETE /route` target is not present anywhere in the tree. |
| `409`  | `POST /route` cannot attach a child because the top-level route is a leaf (`WebhookRoute` / `LongPollRoute`), not a load balancer. Wrap the static config in an LB to make dynamic adds work. |
| `500`  | The control plane itself failed (channel closed, response dropped). This is a tgin bug, not a caller bug. |

> **Note:** the wire-level discriminant is `Longpull` (not `Longpoll`); it matches the in-code spelling. User-facing prose says "long poll", but JSON and Rust identifiers use `Longpull`.

Add a webhook route at runtime:

```bash
curl -X POST http://localhost:3000/api/route \
  -H "Authorization: Bearer $TGIN_AUTH_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"type": "Webhook", "url": "http://bot-b:9000/bot"}'
```

Add a long-poll route, then poll it as if it were Telegram:

```bash
curl -X POST http://localhost:3000/api/route \
  -H "Authorization: Bearer $TGIN_AUTH_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"type": "Longpull", "path": "/bot-c/getUpdates"}'

curl -X POST http://localhost:3000/bot-c/getUpdates \
  --data-urlencode 'timeout=30'
```

The API talks to the routing core via an in-memory channel (see `src/api/router.rs` and `src/api/methods.rs`).

## Observability

tgin exposes structured logs plus four always-on HTTP endpoints: `/livez`, `/readyz`, `/status` and `/metrics`. They are mounted on the main listener whenever `server_port` is set, **independent of `api`**. All four paths are reserved: a `WebhookUpdate` or `LongPollRoute` on any of them is a validation error at startup.

### Authentication

The control / observability plane — `/status`, `/metrics`, and the management API (`/<base_path>/*`) — is gated by the top-level `auth_token`: when set, a request without the exact `Authorization: Bearer <auth_token>` credential is rejected with `401 Unauthorized` before any handler runs. The credential is compared in constant time. The management API cannot run without it: setting `api` with a missing or blank `auth_token` fails validation at startup. Without `api`, `auth_token` is optional — `/status` and `/metrics` are then unauthenticated and assume network-layer isolation (a private network or a reverse proxy), and a present-but-blank value is ignored with a startup warning so the misconfiguration is visible. The probes `/livez` and `/readyz` and the data plane — webhook ingress and long-poll route endpoints — are never gated by `auth_token`. Telegram tokens embedded in webhook route URLs, webhook ingress paths and long-poll route paths are redacted in `/status`, `/metrics`, and `/api/routes`.

For Prometheus, pass the secret through the scrape config's `authorization` block (`type: Bearer`, `credentials: <auth_token>`).

### Probes

- `GET /livez` answers `200 ok` whenever the process serves HTTP.
- `GET /readyz` answers `200 ready`, and `503 draining` from the moment shutdown starts. It does not check Telegram or downstream reachability.

Both are plain HTTP(S) on the main listener; with `ssl` set, probe them over HTTPS.

### Logging

All operator output is emitted through `tracing` to stdout; startup errors (bad config, bind or TLS failure) are printed to stderr as a single `tgin: <error>` message. `log_level` sets the minimum level (overridable via the `RUST_LOG` environment variable) and `log_format` selects `Compact` (human-readable) or `Json` (one JSON object per line) output. `Compact` output is coloured only when stdout is a terminal and the `NO_COLOR` environment variable is unset or empty, so `docker logs` and log files get plain text; `Json` output never contains colour codes.

Bot tokens are replaced with `#####` wherever tgin shows a configured URL or path: the long-poll URL, the `setWebhook` URL, webhook route URLs, and the `WebhookUpdate.path` and `LongPollRoute.path` values (a common webhook pattern puts the token in the path). This covers log lines, startup banners and warnings, startup configuration errors on stderr, `/status`, `/metrics` and `/api/routes`. HTTP client errors are logged without their request URL. A token is recognised by its shape (`<8–15 digits>:<30–50 of A-Z a-z 0-9 _ ->`); other secrets you put into a path are shown as written.

### `GET /status`

Returns a JSON snapshot:

- `updaters[]` — one object per ingress provider: `type` (`longpoll`/`webhook`), `source` (the token-redacted poll URL for long poll, `0.0.0.0<path>` with the path token-redacted for webhook), and `metrics` (`updates_received_total`, `poll_failures_total`, `permanent_failure`).
- `routes` — the routing tree (same shape as `GET /api/routes`), with per-leaf `metrics`.
- `load_balancer.dropped_empty_total` — updates dropped at a load balancer emptied at runtime.

### `GET /metrics`

Prometheus text exposition (`Content-Type: text/plain; version=0.0.4`). Metric names:

| Metric | Type | Labels | Meaning |
|--------|------|--------|---------|
| `tgin_updater_updates_received_total` | counter | `source` | Updates accepted from an ingress provider. |
| `tgin_updater_poll_failures_total` | counter | `source` | Failed long-poll calls: transient errors, and `409` conflicts that tgin backs off on (not the one that triggers a `deleteWebhook`, nor `429`). Always `0` for webhook ingress. |
| `tgin_updater_permanent_failure` | gauge | `source` | `1` once a long-poll updater stopped on a permanent failure. |
| `tgin_route_queue_depth` | gauge | `route` | Current depth of a long-poll route buffer. |
| `tgin_route_queue_dropped_total` | counter | `route` | Updates evicted from a long-poll buffer at its cap. |
| `tgin_route_served_total` | counter | `route` | Updates served out via a long-poll route's `getUpdates`. |
| `tgin_route_dispatched_total` | counter | `route` | Updates a webhook route attempted to forward. |
| `tgin_route_http_responses_total` | counter | `route`, `result` | Webhook downstream outcomes: `success` (2xx), `failure` (other status), `error` (no response: network error or timeout). |
| `tgin_lb_dropped_empty_total` | counter | — | Updates dropped at an empty load balancer. |

All `source` / `route` label values are token-redacted and Prometheus-escaped.

## TLS setup

tgin terminates TLS itself using rustls (`axum_server::tls_rustls`).

1. **Obtain certificates.** Use your CA-issued PEM files or generate self-signed ones for testing:

   ```bash
   openssl req -x509 -nodes -days 365 -newkey rsa:2048 \
     -keyout tls/key.pem -out tls/cert.pem \
     -subj "/CN=your.domain"
   ```

2. **Wire them into the config:**

   ```ron
   ssl: Some(SslConfig(
       cert: "tls/cert.pem",
       key:  "tls/key.pem",
   )),
   server_port: Some(3000),
   ```

3. **Run tgin.** The listener binds `0.0.0.0:<server_port>` and serves HTTPS only, using the supplied certificate. Long-poll routes, webhook ingress, the probes, `/status`, `/metrics` and the management API all use the same TLS listener. Startup fails (exit `1`) if either file is missing or cannot be loaded. In the container image, the key file must be readable by uid `65532` (see [Running in Docker](#running-in-docker)).

## Lifecycle & shutdown

**Startup.** tgin loads and validates the config, then, with `server_port` set, binds `0.0.0.0:<server_port>` before starting any updater. A config error, a bind failure (port already in use) or a certificate that cannot be loaded is reported as a single `tgin: <error>` message on stderr and the process exits `1`; it never runs half-initialized without its HTTP plane.

**Graceful shutdown.** On `SIGTERM` or `SIGINT` (Unix), on Ctrl-C, Ctrl-Break, console close, or system shutdown (Windows), or once every ingress has failed permanently, tgin logs `shutdown started; draining` (with the cause and timing) and drains in order:

1. **Lame duck (5 s).** `/readyz` answers `503` so load balancers stop routing here; everything else keeps working normally, including webhook ingress.
2. **Stop ingest.** Long-poll updaters stop pulling from Telegram (an in-flight `getUpdates` is abandoned); webhook ingress answers `503` with `Retry-After: 5`, so Telegram redelivers those updates later (to a replacement instance, if one is up) rather than having them acknowledged by a process about to exit.
3. **Drain (up to 15 s).** Updates already accepted keep flowing through the routing tree, and long-poll routes keep serving their buffers to downstream bots. A poll on an empty buffer is held for at most 500 ms (a poll already parked is released within 500 ms of the drain starting) and then answered with an empty batch, so consumers are neither held for their full `timeout` nor spinning on empty answers. Every 50 ms tgin checks whether the dispatch queue and every long-poll buffer are empty, and moves on as soon as they are.
4. **Close.** The HTTP plane closes (in-flight requests get 1 s). The workers keep dispatching what is left in the dispatch queue until the drain deadline: the end of the 15 s, or 1 s after the HTTP close began if the 15 s are already spent. At the deadline they stop, aborting dispatches still in flight (for example a webhook request waiting on a slow or dead downstream). Updates still queued, aborted mid-dispatch or left in a long-poll buffer are lost and logged once as a warning with their count (`abandoned`, split into `queued`, `aborted` and `buffered`).

With the defaults, the whole shutdown takes at least the 5 s lame duck and at most about 5 s + 15 s + 1 s ≈ 21 s, whatever the downstreams do.

**Second signal.** A second `SIGINT` or `SIGTERM` (Unix), or Ctrl-C or Ctrl-Break (Windows; a console close or system shutdown event counts too), once shutdown has started skips the rest of the drain: tgin logs `second shutdown signal; exiting without completing the drain` and exits at once with code `130`. Whatever is still queued or buffered is lost.

**Known limitation.** The drain is best-effort. A worker that has taken an update off the dispatch queue but not yet pushed it into a long-poll route is invisible to the emptiness check. If the check samples in that instant, the HTTP plane can close before the update reaches the buffer; it is then counted in the `abandoned` warning instead of being delivered.

**Other cases.** When the console window is closed on Windows, which kills the process about 5 s later, the lame duck is skipped and the drain is capped at 4 s. Without `server_port` there is no HTTP plane to drain: tgin logs `shutdown started; no HTTP plane, stopping ingest`, skips the lame duck, stops ingest, and gives the workers up to the 15 s grace to finish the backlog before stopping them as in step 4. When every long-poll updater has failed permanently and there is no webhook ingress, the shutdown starts within about a second and runs the full drain above.

**Exit codes.**

| Code  | Meaning |
|-------|---------|
| `0`   | Clean shutdown (signal-driven drain, or every ingress stopped on its own without a permanent failure). Updates abandoned at the drain deadline do not change the code; see the `abandoned` warning. |
| `1`   | Startup failure: config read, env substitution, parse or validation error; bind or TLS error. |
| `2`   | At least one long-poll updater stopped on a permanent failure (401/403/404) during the run, whatever ended the process (unless a second signal forced the exit); this includes the self-initiated shutdown once all ingress has failed. See [Failure handling](#longpollupdate). |
| `130` | A second shutdown signal forced the exit before the drain finished. |

### Deployment rules

- **One replica per bot token with long-poll ingress.** Telegram accepts one `getUpdates` poller per token; a second one gets `409 Conflict`, and tgin backs off and logs `another process is polling this token`. Run a single tgin per token and replace it with a stop-then-start rollout (Kubernetes `strategy: Recreate`, `replicas: 1`), not a rolling update that briefly runs two.
- **Long-poll route buffers are per process.** A downstream bot must poll the tgin instance that holds its updates.
- **Give the drain time.** Set the orchestrator's stop timeout to at least 30 s: Kubernetes `terminationGracePeriodSeconds: 30`, Docker `--stop-timeout 30` or Compose `stop_grace_period: 30s` (Docker's default is 10 s, which cuts the drain short). The drain takes at most about 21 s with the defaults.
- **Downstream bots must not manage the webhook.** Bots behind tgin usually share its token to send messages. Frameworks that call `deleteWebhook` or `setWebhook` against Telegram on that token — aiogram's `bot.delete_webhook(drop_pending_updates=True)`, and the polling start-up of python-telegram-bot and grammY — can delete tgin's webhook (a `WebhookUpdate` then receives nothing) or drop the updates Telegram holds for tgin. Point such bots at Telegram only for sending, never for webhook management, until tgin's planned Bot API gateway lands.
- **Probes.** Liveness on `/livez`, readiness on `/readyz`.

```yaml
# Kubernetes Deployment excerpt for long-poll ingress
spec:
  replicas: 1
  strategy:
    type: Recreate
  template:
    spec:
      terminationGracePeriodSeconds: 30
      containers:
        - name: tgin
          image: ghcr.io/arynyklas/tgin:0.2
          livenessProbe:
            httpGet: { path: /livez, port: 3000 }
          readinessProbe:
            httpGet: { path: /readyz, port: 3000 }
```

## Running in Docker

The image (built from the repository `Dockerfile`, published as `ghcr.io/arynyklas/tgin` for `linux/amd64` and `linux/arm64`, tagged `<version>`, `<major>.<minor>` and, for the highest stable release only, `latest`) contains one static binary on `gcr.io/distroless/static-debian13`:

- **Entrypoint** `/usr/local/bin/tgin`, default arguments `-f /etc/tgin/config.ron`: mount your config there, or pass your own `-f <path>` (`docker run IMAGE -f /path/config.ron`; arguments go straight to tgin, so `docker run IMAGE tgin -f …` fails with a usage error).
- **User** numeric uid/gid `65532:65532`. A mounted config file must be readable by that uid (`chmod 0644 tgin.ron`); for a TLS private key, `chown 65532:65532 key.pem` or run the container with `--user` set to the key's owner.
- **No shell, no curl.** A `HEALTHCHECK` that execs a command inside the container cannot work; probe `/livez` and `/readyz` over HTTP from outside (orchestrator HTTP probes).
- **Port** `3000` is declared with `EXPOSE`, but tgin listens on the config's `server_port`; publish that one.

```bash
docker build -t tgin .            # or use ghcr.io/arynyklas/tgin:<version>
chmod 0644 tgin.ron
docker run -d --name tgin \
  -p 3000:3000 \
  -v "$PWD/tgin.ron:/etc/tgin/config.ron:ro" \
  -e TOKEN=123456:ABC \
  -e TGIN_AUTH_TOKEN="$(openssl rand -hex 32)" \
  --stop-timeout 30 \
  tgin
```

`examples/simple` runs the same image with Docker Compose. [SECURITY.md](SECURITY.md#verifying-releases) shows how to verify a published image's provenance attestation.

## Configuration example

`${VAR}` placeholders are pulled from the process environment before parsing. They are substituted everywhere in the file, comments included, so a commented-out line that contains a placeholder still needs that variable set; the commented lines below use literal values for that reason.

```ron
(
    dark_threads: 6,
    server_port: Some(3000),
    // api: Some(ApiConfig(base_path: "/api")),
    // auth_token: Some("change-me"),  // required when `api` is set; use a long random secret
    // ssl: Some(SslConfig(cert: "/cert.pem", key: "/privkey.pem")),
    updates: [
        LongPollUpdate(
            token: "${TOKEN}",
        ),
        // WebhookUpdate(
        //     path: "/bot/pull",
        //     secret_token: Some("change-me"),
        // ),
    ],
    route: RoundRobinLB(
        routes: [
            LongPollRoute(path: "/bot2/getUpdates"),
            WebhookRoute(url: "http://127.0.0.1:8080/bot2"),
        ],
    ),
)
```

## Further reading

- [README.md](README.md) — high-level motivation, quick start and release artefacts.
- [PERF.md](PERF.md) — benchmark methodology, results and their known limits.
- [CHANGELOG.md](CHANGELOG.md) — changes per release.
- [SECURITY.md](SECURITY.md) — vulnerability reporting and release verification.
- [examples/simple](examples/simple) — docker-compose demo with multiple downstream bots and a sample `tgin.ron`.
- [integrations/pytgin](integrations/pytgin) — Python `aiogram` shim that reroutes `getUpdates` through tgin.
