# tgin documentation

## Contents

- [Overview](#overview)
- [Concepts](#concepts)
- [Quick start](#quick-start)
- [Configuration reference](#configuration-reference)
  - [Top-level fields](#top-level-fields)
  - [Update providers (`updates`)](#update-providers-updates)
  - [Routing targets (`route`)](#routing-targets-route)
  - [Load balancers](#load-balancers)
- [HTTP management API](#http-management-api)
- [Observability](#observability)
- [TLS setup](#tls-setup)
- [Configuration example](#configuration-example)
- [Further reading](#further-reading)

## Overview

tgin is a routing layer between the Telegram Bot API and one or more downstream bot instances. It ingests updates over long poll or webhook, dispatches them through a tree of routes and load balancers, and forwards each update to its destination. Configuration is a single RON file (default: `tgin.ron`); behavior can be hot-reconfigured at runtime via an optional HTTP API, and ingress can be served over TLS.

## Concepts

| Term             | Direction | Purpose                                                                                               |
|------------------|-----------|-------------------------------------------------------------------------------------------------------|
| `LongPollUpdate` | ingress   | Polls Telegram's `getUpdates` and feeds every update into the routing tree.                           |
| `WebhookUpdate`  | ingress   | Exposes an HTTP endpoint that Telegram POSTs updates to. Optional self-registration via `setWebhook`. |
| `LongPollRoute`  | egress    | Buffers updates and serves them to downstream bots through a Telegram-shaped `getUpdates` endpoint.   |
| `WebhookRoute`   | egress    | POSTs each update to a downstream HTTP endpoint.                                                      |
| `RoundRobinLB`   | egress    | Rotates through child routes; each update goes to exactly one child.                                  |
| `AllLB`          | egress    | Broadcasts every update to every child concurrently.                                                  |

Updates flow in one direction: every `update` provider pushes into a single shared channel; the consumer is the root `route`. Errors on the egress path are logged and swallowed so a single failing downstream cannot stall the dispatcher — downstreams must be resilient.

## Quick start

```bash
git clone https://github.com/arynyklas/tgin.git
cd tgin
cargo build --release
./target/release/tgin -f tgin.ron
```

The `-f`/`--file` flag selects the configuration file (defaults to `tgin.ron`). `${VAR}` placeholders inside the config are substituted from the environment **before** the RON parser runs; missing variables abort startup.

## Configuration reference

Top-level structure loaded from `tgin.ron` (`src/config/schema.rs`).

### Top-level fields

| Field          | Type                              | Default | Description                                                                                                                                                                                                                 |
|----------------|-----------------------------------|---------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `dark_threads` | `usize`                           | `4`     | Worker threads allocated to the Tokio runtime. Increase for higher concurrency.                                                                                                                                             |
| `server_port`  | `Option<u16>`                     | —       | When set, tgin hosts every HTTP-facing component (webhook ingress, long-poll routes, management API) on `0.0.0.0:<port>`. Set to `None` to disable HTTP ingress entirely (only `LongPollUpdate` / `WebhookRoute` will run). |
| `ssl`          | `Option<SslConfig { cert, key }>` | `None`  | Optional TLS certificate and private key (PEM file paths). When set, the listener serves HTTPS via rustls.                                                                                                                  |
| `updates`      | `Vec<UpdateConfig>`               | —       | Ingress providers. Multiple providers can coexist; tgin merges updates from all of them.                                                                                                                                    |
| `route`        | `RouteConfig`                     | —       | The single root of the routing tree. Either a leaf route or a load-balancer subtree.                                                                                                                                        |
| `api`          | `Option<ApiConfig { base_path }>` | `None`  | Optional management API base path (e.g. `/api`). Routes are nested under this prefix on the same listener as the ingress.                                                                                                   |
| `log_level`    | `String`                          | `"info"`  | Minimum log level: `trace`/`debug`/`info`/`warn`/`error`/`off`. `RUST_LOG` overrides it. |
| `log_format`   | `"Compact"` \| `"Json"`           | `Compact` | `Compact` = human-readable single-line logs; `Json` = one JSON object per line for log shippers. |
| `auth_token`   | `Option<String>`                  | `None`    | Optional shared secret for the control / observability plane. When set, `/status`, `/metrics`, and the management API require `Authorization: Bearer <auth_token>`. A present-but-blank value is rejected at startup. |

### Update providers (`updates`)

#### `LongPollUpdate`

Long-polls Telegram for updates and dispatches each one into the routing tree.

| Field                   | Type             | Default                    | Description                                                                            |
|-------------------------|------------------|----------------------------|----------------------------------------------------------------------------------------|
| `token`                 | `String`         | required                   | Telegram bot token (e.g. `123456:ABC...`).                                             |
| `url`                   | `Option<String>` | `https://api.telegram.org` | Override for the Telegram API base URL.                                                |
| `default_timeout_sleep` | `u64` (seconds)  | `0`                        | Sleep between successful `getUpdates` calls. Bump it to throttle request rate at idle. |
| `error_timeout_sleep`   | `u64` (seconds)  | `0`                        | Sleep between retries after a failed `getUpdates` call.                                |
| `long_poll_timeout`     | `u64` (seconds)  | `30`                       | Server-side hold time passed as `timeout` to `getUpdates`. Telegram caps this at 50.   |
| `long_poll_limit`       | `u64`            | `100`                      | Max updates per `getUpdates` response. Telegram caps this at 100.                      |

Failure handling: `LongPollUpdate` distinguishes transient from permanent upstream failures.

- **Transient** (network errors, 5xx, body-read errors, JSON-parse errors, 429): retry with full-jitter exponential backoff capped at 1 s. After ~60 consecutive failures the updater emits a single `long-poll prolonged outage` warning so a sustained outage is loud, then keeps retrying. Telegram's own multi-minute incidents are riden out, not abandoned.
- **Permanent** (401 Unauthorized, 403 Forbidden, 404 Not Found from `bot<token>/getUpdates`): the token is wrong, revoked, or malformed and no amount of retrying recovers it. The updater logs a `long-poll permanent failure` error line, increments a process-wide health counter, and stops. When the binary eventually exits with no remaining producers, a non-zero permanent-failure count produces exit code `2` (distinct from the generic startup-failure exit `1`) so an orchestrator (systemd, Docker, k8s) can alert or restart.

#### `WebhookUpdate`

Exposes an HTTP endpoint on `server_port` that Telegram POSTs updates to.

| Field          | Type                                | Default  | Description                                                                                                                                                                                                                                                                              |
|----------------|-------------------------------------|----------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `path`         | `String`                            | required | Local URL path Telegram should hit (e.g. `/bot/pull`).                                                                                                                                                                                                                                   |
| `secret_token` | `Option<String>`                    | `None`   | When set, tgin requires every inbound POST to carry a matching `x-telegram-bot-api-secret-token` header. Mismatches are rejected.                                                                                                                                                        |
| `registration` | `Option<RegistrationWebhookConfig>` | `None`   | Enables automatic webhook registration at boot. tgin POSTs `setWebhook` to Telegram with `url = <public_ip><path>`. Override the registration endpoint via `set_webhook_url` (default `https://api.telegram.org/bot<token>/setWebhook`). Omit this block to manage the webhook yourself. |

`RegistrationWebhookConfig` fields: `public_ip: String`, `token: String`, `set_webhook_url: Option<String>`.

### Routing targets (`route`)

#### `LongPollRoute { path, max_buffered_updates? }`

Exposes a Telegram-shaped endpoint at `path` that downstream bots can poll. Updates are buffered in memory until a client calls the route via an `application/x-www-form-urlencoded` request with Telegram-compatible `offset` / `timeout` parameters. `offset` filtering follows Telegram semantics, so multiple bots can safely read from the same buffer.

| Field                  | Type     | Default  | Description                                                                                                                                                                                                                                                                                                    |
|------------------------|----------|----------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `path`                 | `String` | required | Local URL path the downstream bot polls (e.g. `/bot1/getUpdates`).                                                                                                                                                                                                                                             |
| `max_buffered_updates` | `usize`  | `10000`  | Cap on the per-route in-memory buffer. When reached, oldest updates are evicted FIFO and the eviction is reflected in `metrics.queue_dropped_total` on `/api/routes`. Telegram already buffers ~24 h of undelivered updates server-side, so tgin is best-effort recovery; raise the cap only if you need more. |

Each `LongPollRoute` exposes runtime metrics through `GET /api/routes`:

```json
{
  "type": "longpoll",
  "options": { "path": "/bot1/getUpdates", "max_buffered_updates": 10000 },
  "metrics": { "queue_depth": 7, "queue_dropped_total": 0 }
}
```

When `queue_depth` first crosses 80 % of `max_buffered_updates`, tgin emits a one-time `warning: long-poll route ... crossed buffer high-watermark` log line so a downstream that has gone offline does not leak silently.

#### `WebhookRoute { url, request_timeout_ms? }`

Push-based forwarder: every update triggers an HTTP POST of the original JSON payload to `url` (e.g. `http://internal-bot:8080/bot`). HTTP errors are logged and swallowed so one bad downstream cannot stall the dispatcher.

| Field                | Type     | Default  | Description                                                                                                                                       |
|----------------------|----------|----------|---------------------------------------------------------------------------------------------------------------------------------------------------|
| `url`                | `String` | required | Downstream URL.                                                                                                                                   |
| `request_timeout_ms` | `u64`    | `10000`  | Per-request deadline, in milliseconds. Lower it (e.g. `2000`) when downstreams are expected to be fast and you want backpressure to surface fast. |

### Load balancers

Load balancers compose multiple routes and can be nested arbitrarily deep.

#### `RoundRobinLB { routes }` (`src/lb/roundrobin.rs`)

Holds an atomic cursor and forwards each update to the next child in sequence. Children can be heterogeneous — mix `WebhookRoute` and `LongPollRoute` freely.

#### `AllLB { routes }` (`src/lb/all.rs`)

Broadcast strategy: clones every update and dispatches it to every child concurrently. Useful when several specialized services must each see the full update stream (analytics, moderation, etc.). Be mindful of downstream backpressure — each update is processed `N` times.

## HTTP management API

Enable the API by setting `api` in your config:

```ron
api: Some(ApiConfig(
    base_path: "/api",
)),
```

Routes are nested under `base_path` and share the same listener as the ingress endpoints.

> **Authentication:** these endpoints are unauthenticated unless the top-level `auth_token` is set, in which case every request must carry `Authorization: Bearer <auth_token>` (see [Observability → Authentication](#authentication)). They mutate the routing tree, so restrict access at the network layer regardless.

| Endpoint      | Method   | Body                                     | Description                                                                                                                                                                                                                                                                             |
|---------------|----------|------------------------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `/api/routes` | `GET`    | —                                        | Returns the current routing tree as JSON (source: `Routeable::json_struct`).                                                                                                                                                                                                            |
| `/api/route`  | `POST`   | `{ "type": "Webhook"\|"Longpull", ... }` | Adds a route dynamically. `Webhook` form: `{"type": "Webhook", "url": "...", "request_timeout_ms": <ms>?}`. `Longpull` form: `{"type": "Longpull", "path": "...", "max_buffered_updates": <n>?}`. `request_timeout_ms` defaults to `10000`; `max_buffered_updates` defaults to `10000`. |
| `/api/route`  | `DELETE` | `{ "type": "Webhook"\|"Longpull", ... }` | Removes a previously-added route. Match by `url` (`Webhook`) or `path` (`Longpull`).                                                                                                                                                                                                    |

Status codes are honest about the outcome — the API used to return 200 for every request regardless of what the routing tree did:

| Status | Meaning                                                                                                                                                                                                                                  |
|--------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `200`  | Mutation applied (or, for `GET /routes`, snapshot returned).                                                                                                                                                                             |
| `400`  | Caller-supplied input is malformed: empty `url` or `path`, a `path` that doesn't start with `/`, a `url` that fails `reqwest::Url::parse` or whose scheme is not `http`/`https`, or a body that fails to deserialize against the schema. |
| `404`  | `DELETE /route` target is not present anywhere in the tree.                                                                                                                                                                              |
| `409`  | `POST /route` cannot attach a child to the current top-level route — e.g. it is a leaf (`WebhookRoute` / `LongPollRoute`) rather than a load balancer. Wrap the static config in an LB to make dynamic adds work.                        |
| `500`  | Control plane itself failed (channel closed, response dropped). This is a tgin bug, not a caller bug.                                                                                                                                    |

> **Note:** the wire-level discriminant is `Longpull` (not `Longpoll`) — it matches the in-code spelling. User-facing prose says "long poll" / "LongPoll", but JSON and Rust identifiers use `Longpull`.

Add a webhook route at runtime:

```bash
curl -X POST http://localhost:3000/api/route \
  -H 'Content-Type: application/json' \
  -d '{"type": "Webhook", "url": "http://bot-b:9000/bot"}'
```

Add a long-poll route, then poll it as if it were Telegram:

```bash
curl -X POST http://localhost:3000/api/route \
  -H 'Content-Type: application/json' \
  -d '{"type": "Longpull", "path": "/bot-c/getUpdates"}'

curl -X POST http://localhost:3000/bot-c/getUpdates \
  --data-urlencode 'offset=0' \
  --data-urlencode 'timeout=30'
```

The API talks to the routing core via an in-memory channel (see `src/api/router.rs` and `src/api/methods.rs`).

## Observability

tgin exposes structured logs plus two always-on HTTP endpoints. The endpoints are mounted on the main listener whenever `server_port` is set — **independent of `api`**; observability never requires enabling hot-reconfig. `/status` and `/metrics` are reserved paths: a configured route on either is a validation error at startup, not a runtime collision.

### Authentication

The control / observability plane — `/status`, `/metrics`, and the management API (`/<base_path>/*`) — is unauthenticated by default and assumes network-layer isolation (a private network or a reverse proxy). Set the top-level `auth_token` to require `Authorization: Bearer <auth_token>` on all three: a request without the exact credential is rejected with `401 Unauthorized` before any handler runs. The credential is compared in constant time, and a present-but-blank `auth_token` is a startup validation error. The data plane — webhook ingress and long-poll route endpoints — is never gated. Telegram tokens embedded in downstream route URLs are redacted in `/status`, `/metrics`, and `/api/routes`.

For Prometheus, pass the secret through the scrape config's `authorization` block (`type: Bearer`, `credentials: <auth_token>`).

### Logging

All operator output is emitted through `tracing`. `log_level` sets the minimum level (overridable via the `RUST_LOG` environment variable) and `log_format` selects `Compact` (human-readable) or `Json` (one JSON object per line) output. Telegram tokens are redacted from every log line.

### `GET /status`

Returns a JSON snapshot:

- `updaters[]` — one object per ingress provider: `type` (`longpoll`/`webhook`), redacted `source`, and `metrics` (`updates_received_total`, `poll_failures_total`, `permanent_failure`).
- `routes` — the routing tree (same shape as `GET /api/routes`), with per-leaf `metrics`.
- `load_balancer.dropped_empty_total` — updates dropped at a load balancer drained empty at runtime.

### `GET /metrics`

Prometheus text exposition (`Content-Type: text/plain; version=0.0.4`). Metric names:

| Metric | Type | Labels | Meaning |
|--------|------|--------|---------|
| `tgin_updater_updates_received_total` | counter | `source` | Updates accepted from an ingress provider. |
| `tgin_updater_poll_failures_total` | counter | `source` | Long-poll transient poll failures (webhook stays 0). |
| `tgin_updater_permanent_failure` | gauge | `source` | `1` once a long-poll updater stopped on a permanent failure. |
| `tgin_route_queue_depth` | gauge | `route` | Current depth of a long-poll route buffer. |
| `tgin_route_queue_dropped_total` | counter | `route` | Updates evicted from a long-poll buffer at its cap. |
| `tgin_route_served_total` | counter | `route` | Updates served out via a long-poll route's `getUpdates`. |
| `tgin_route_dispatched_total` | counter | `route` | Updates a webhook route attempted to forward. |
| `tgin_route_http_responses_total` | counter | `route`, `result` | Webhook downstream outcomes (`result` = `success`/`failure`/`error`). |
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

3. **Run tgin.** The Axum server binds `0.0.0.0:<server_port>` and serves HTTPS using the supplied certificate. Long-poll routes, webhook ingress, and the management API all use the same TLS listener.

## Lifecycle & shutdown

**Startup.** With `server_port` set, tgin binds `0.0.0.0:<server_port>` on the main task before serving. A bind failure (port already in use, bad address) or an unreadable TLS certificate is reported as a single `tgin: <error>` line on stderr and the process exits `1` — it does not start a half-initialized process with no HTTP plane.

**Graceful shutdown.** On `SIGTERM` (Unix) or `Ctrl-C` (all platforms) tgin logs `shutdown signal received; draining` and drains in order: long-poll ingest stops pulling new updates from Telegram; the HTTP server finishes in-flight requests (bounded to a 10 s deadline, so a parked downstream connection cannot block exit indefinitely); the worker pool then processes the buffered backlog and exits. A clean drain exits `0`. Updates a shutting-down long-poll updater never confirms are redelivered by Telegram on the next start, so no acknowledged update is lost across a restart.

**Exit codes.** `0` — clean shutdown (signal-driven drain, or every producer terminated normally). `1` — startup failure (bad config, bind/TLS error). `2` — at least one long-poll updater hit a permanent failure (401/403/404) and stopped; see [Failure handling](#longpollupdate).

## Configuration example

`${VAR}` placeholders are pulled from the process environment before parsing.

```ron
(
    dark_threads: 6,
    server_port: Some(3000),
    // api: Some(ApiConfig(base_path: "/api")),
    // ssl: Some(SslConfig(cert: "/cert.pem", key: "/privkey.pem")),
    // auth_token: Some("change-me"),
    updates: [
        LongPollUpdate(
            token: "${TOKEN}",
        ),
        // WebhookUpdate(
        //     path: "/bot/pull",
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

- [README.md](README.md) — high-level motivation and quick start.
- [PERF.md](PERF.md) — benchmark methodology and results.
- [examples/simple](examples/simple) — docker-compose demo with multiple downstream bots and a sample `tgin.ron`.
- [integrations/pytgin](integrations/pytgin) — Python `aiogram` shim that reroutes `getUpdates` through tgin.
