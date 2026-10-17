# tgin documentation

This is the manual of tgin 1.0. The key-by-key reference of the config file is [docs/config-reference.md](docs/config-reference.md), generated from the config model.

## Contents

- [Concepts](#concepts)
- [Configuration](#configuration)
- [Delivery semantics](#delivery-semantics)
- [Pull consumers: the `getUpdates` endpoint](#pull-consumers-the-getupdates-endpoint)
- [Push consumers: the requests tgin sends](#push-consumers-the-requests-tgin-sends)
- [Webhook ingress](#webhook-ingress)
- [Bot API gateway](#bot-api-gateway)
- [Hot reload](#hot-reload)
- [Admin API v1](#admin-api-v1)
- [`tgin admin`](#tgin-admin)
- [Metrics](#metrics)
- [Logging](#logging)
- [Security](#security)
- [Shutdown](#shutdown)
- [Deployment](#deployment)
- [Command line and exit codes](#command-line-and-exit-codes)

## Concepts

tgin sits between the Telegram Bot API and the processes that handle a bot's updates. One tgin serves any number of bots.

- **Bots** (`bots`). Each bot has a unique name, a token and at most one **ingress**: `long_poll` (tgin calls `getUpdates` on the Bot API) or `webhook` (Telegram posts updates to tgin). A bot without an ingress receives nothing.
- **Consumers** (`consumers`). A named destination for updates, in one of two modes:
  - **pull**: downstream bots fetch updates from tgin with a Telegram-shaped `getUpdates` call on `/c/<consumer>/getUpdates` (and on any alias path), or through the [gateway](#bot-api-gateway) on `/c/<consumer>/bot<token>/getUpdates`. Several processes may poll one pull consumer; they compete for its updates. A pull consumer serves exactly one bot: two bots routing to the same pull consumer is a config error.
  - **push**: tgin posts each update to one or more HTTP endpoints, with retries and a circuit breaker per endpoint. Several bots may route to one push consumer; the `X-Tgin-Source` header names the bot.
  Each consumer has its own bounded queue (`capacity` updates, `capacity_bytes` bytes).
- **Rules** (`bots[].routes`). Each bot's rules are tried in order; the first rule whose `match` holds decides where the update goes, and a rule without `match` matches everything. An update no rule matches is dropped and counted in `tgin_routes_unmatched_total`; `tgin config validate` warns when a bot's last rule has a `match`.
- **Targets** (`to`). Every entry of a rule's `to` receives the update. An entry is a consumer name, `{split: {strategy, among, weights?}}` (exactly one child receives it: `round_robin`, `hash_chat` — sticky by chat, else by user, else round robin — or smooth `weighted` round robin) or `{all: […]}` (every child receives it). Targets nest. A consumer reached twice through one rule's targets receives the update once.
- **Gateway.** The data plane also serves the Bot API to the bots behind tgin, on `/bot<token>/…` and `/c/<consumer>/bot<token>/…`: a downstream points its framework's Bot API base URL at tgin, which forwards its calls to Telegram within the bot's flood limits and answers itself the calls that would disturb tgin or the bot's other downstreams ([Bot API gateway](#bot-api-gateway)).

`match` fields: `update_types`, `chat_ids`, `chat_types`, `user_ids`, `commands`, `text` and `callback_data` (each `prefix`, `regex` or `equals`). Values in one list are alternatives; all given fields must hold. `text` is the message text or caption. A command matches when its name equals one of the rule's (ASCII case-insensitive) and an `@bot` suffix, if any, names this bot; until tgin has learned the bot's username from `getMe`, a command with an `@bot` suffix matches no rule.

## Configuration

The config file is YAML, `version: 1`. tgin reads `tgin.yaml` in the working directory unless `-c/--config` or `TGIN_CONFIG` names another file. Unknown keys are errors, and errors name their YAML path, line and column. With this file, whose rule names a consumer that does not exist:

```yaml
version: 1
server: {}
bots:
  - name: main
    token: ${env:TOKEN}
    ingress:
      long_poll: {}
    routes:
      - to: [bot]
consumers:
  bots:
    pull: {}
```

```console
$ tgin config validate tgin.yaml
error: bots[0].routes[0].to[0] (line 9, column 14): unknown consumer `bot`
  hint: defined consumers: bots
tgin: tgin.yaml has 1 error
```

`tgin config validate` lists the errors (all validation errors at once; a YAML syntax or shape error alone) and warnings, and exits 1 on errors; `tgin config print` prints the config as JSON with defaults filled in (except a webhook's omitted `path`, which defaults to `/webhook/<bot name>`) and secrets redacted; `tgin config schema` prints the JSON Schema ([schema/tgin.v1.schema.json](schema/tgin.v1.schema.json)), which YAML editors can use (`# yaml-language-server: $schema=…`). Every `${env:…}` the file references must be set for these commands too.

**References.** Only secrets (`token`, `secret_token`, `auth_token`, `admin.token`) and fields holding URLs or file paths (including webhook `path` and pull `aliases`) expand references; every other value is literal.

- `${env:NAME}` is the environment variable `NAME`; an unset variable is an error.
- `${file:PATH}` is the content of the file, minus one trailing newline. A relative `PATH` is relative to the config file's directory.
- `$${` is a literal `${`.

File-path fields (`server.tls.cert_file`, `server.tls.key_file`, a webhook's `certificate_file`) are relative to the config file's directory too, never to the working directory.

### Sections

| Section | What it sets | Defaults |
|---|---|---|
| `runtime` | `worker_threads` (`auto` = one per CPU), `lame_duck`, `shutdown_grace` | `auto`, `5s`, `15s` |
| `log` | `level` (`trace` … `error`), `format` (`text` or `json`) | `info`, `text` |
| `server` | The data plane: `listen`, `tls`, `trusted_proxies`, `max_connections`, `header_read_timeout`. Omitted, tgin serves no webhooks and no pull consumers (both then fail validation). | `["0.0.0.0:8080"]`, none, none, `10000`, `10s` |
| `admin` | The admin plane: `listen`, `token`, `public_metrics` | `["127.0.0.1:9090"]`, none, `false` |
| `reload` | `watch` the config file, its `debounce` (at least `10ms`), and the `drain_timeout` of removed consumers | `true`, `500ms`, `30s` |
| `bots` | Bots with `name`, `token`, `api_url`, `gateway`, `ingress`, `routes` | `api_url: https://api.telegram.org` |
| `consumers` | Pull and push consumers by name | see below |

Names of bots and consumers are 1 to 63 lowercase letters, digits and `-`, starting with a letter or digit.

**Long poll** (`ingress.long_poll`): `timeout` (`30s`, at least `1s`, at most 50 s is sent), `limit` (`100`, 1 to 100), `allowed_updates`, `delete_webhook_on_start` (`true`). tgin sends `allowed_updates` with every `getUpdates`: the configured list, or `[]` when it is omitted, which is Telegram's default (every update type except `chat_member`, `message_reaction` and `message_reaction_count`). A list Telegram stored for the token from an earlier client never decides what arrives; list those three types to receive them.

**Webhook** (`ingress.webhook`): `path` (default `/webhook/<bot name>`), `public_url` (the public `https` base URL; `setWebhook` registers `public_url` followed by `path`), `register` (`false`; `true` calls `setWebhook` when the bot's source starts and needs `public_url`), `secret_token` (`auto`; a configured secret is 1 to 256 ASCII letters, digits, `_` or `-`, as Telegram requires), `ip_allowlist` (`none`), and Telegram's `allowed_updates` (sent with every `setWebhook`, `[]` when omitted, as for long polls), `max_connections`, `drop_pending_updates`, `ip_address` and `certificate_file` for `setWebhook`.

**Gateway** (`gateway`, per bot): `upstream_idle_timeout` (`1m`, at least `1s`), `rate_limit` (`mode` `shape`, `max_delay` `5s` (at most `1m`), `methods` `["send*", "copyMessage*", "forwardMessage*"]`, `global` 30 per `1s` with `burst` 30, `per_chat` 1 per `1s` with `burst` 3, `per_group` 20 per `1m` with `burst` 20) and `retry_429` (`max_wait` `10s`, at most `1m`). See [Bot API gateway](#bot-api-gateway).

**Tokens and paths.** A bot token may use only ASCII letters, digits, `:`, `_` and `-`; a token with anything else (a space, quotes, a second trailing newline in a token file) is a config error, at start and on reload alike. Webhook paths and pull aliases are matched byte for byte, so they must reach tgin unchanged inside a URL: no `?`, `#`, spaces, control or non-ASCII characters (write them percent-encoded), no `.` or `..` segments and no `\`.

**Pull consumer** (`pull`): `capacity` (`10000`), `capacity_bytes` (`64MiB`, at least `1MiB`), `on_full` (`backpressure`), `lease_timeout` (`2m`, at least `1s`: leases are expired by a check that runs once a second), `ordering` (`none`), `auth_token`, `aliases`, `honor_drop_pending` (`false`).

**Push consumer** (`push`): `endpoints` (each `url`, `secret_token`, `timeout` `10s`; at least one, unless `accept_set_webhook: true`), `balance` (`round_robin`, `hash_chat` or `least_in_flight`), `max_in_flight` (`32`), `ordering` (`none`), `capacity` (`10000`), `capacity_bytes` (`64MiB`), `on_full` (`backpressure`), `retry` (`initial` `1s`, `max` `1m`, `max_attempts` `10`, `max_age` `1h`), `breaker` (`failure_threshold` `5`, `open_for` `30s`), `webhook_reply` (`execute` or `ignore`; see [Webhook replies](#webhook-replies)), `accept_set_webhook` (`false`; see [`setWebhook` capture](#setwebhook-capture)), `honor_drop_pending` (`false`). An endpoint `timeout`, `retry.initial`, `retry.max`, `retry.max_age`, `breaker.open_for` and `server.header_read_timeout` of 0 would disable or break what they configure, and are config errors; `runtime.lame_duck`, `runtime.shutdown_grace` and `reload.drain_timeout` may be 0 (no lame duck, no drain).

`max_in_flight` counts deliveries in progress over all of a consumer's endpoints, retries waiting included. With downstreams that handle requests concurrently, the default, 32, delivered every update at every webhook point of the benchmarks. A downstream that handles one request at a time (a single bot process) can deliver more with a lower value: in benchmarks from an earlier session, before the load generator's 100-connection cap, one aiogram process offered 5 000 updates/s took 1 809 updates/s with 32 and 2 268–2 280 with 8 ([PERF.md → `max_in_flight`](PERF.md#max_in_flight)).

**Gateway flags.** `honor_drop_pending: true` lets `drop_pending_updates` on the consumer's gateway path empty its queue; `accept_set_webhook: true` (push only, and only on a consumer fed by one bot) lets that bot's `setWebhook` give the consumer an endpoint. Both are described under [Bot API gateway](#bot-api-gateway).

### Example

```yaml
# yaml-language-server: $schema=schema/tgin.v1.schema.json
version: 1

log:
  format: json

server:
  listen: ["0.0.0.0:8080"]
  # The reverse proxy in front of tgin, whose X-Forwarded-For is believed.
  trusted_proxies: ["10.0.0.0/8"]

admin:
  listen: ["0.0.0.0:9090"]
  token: ${file:/run/secrets/tgin_admin_token}

bots:
  - name: shop
    token: ${env:SHOP_TOKEN}
    ingress:
      long_poll: {}
    routes:
      - match:
          commands: [start, help]
        to: [frontdesk]
      - match:
          update_types: [callback_query]
        to:
          - split:
              strategy: hash_chat
              among: [workers-a, workers-b]
      - to: [frontdesk, audit]

  - name: support
    token: ${env:SUPPORT_TOKEN}
    ingress:
      webhook:
        public_url: https://bots.example.com
        register: true
        ip_allowlist: telegram
    routes:
      - to: [support]

consumers:
  frontdesk:
    pull:
      auth_token: ${env:FRONTDESK_TOKEN}
  workers-a:
    pull: {}
  workers-b:
    pull: {}
  support:
    pull:
      ordering: per_chat
  audit:
    push:
      endpoints:
        - url: https://audit.internal/telegram
          secret_token: ${env:AUDIT_SECRET}
      on_full: drop_oldest
```

Here `shop` polls Telegram: `/start` and `/help` go to the `frontdesk` processes, callback queries are spread over two worker pools by chat, and everything else goes both to `frontdesk` and to the `audit` service. Telegram posts `support`'s updates to `https://bots.example.com/webhook/support`, which a proxy forwards to tgin.

## Delivery semantics

**Admission.** An update is routed by its bot's rules and queued in every target consumer, or in none:

1. The queue of every `on_full: backpressure` target must have room; if one is full, nothing is queued anywhere and the source is told to retry: a long poll holds the rest of its batch and offers the update again every 250 ms, without polling Telegram meanwhile; a webhook answers `503` with `Retry-After: 5`, and Telegram sends the update again later.
2. A full `on_full: drop_oldest` target evicts its oldest updates that are not leased; if everything in it is leased, it skips the update. Either way the dropped updates count as `tgin_consumer_dropped_total{reason="overflow"}`.
3. An update larger than a target's `capacity_bytes` is dropped for that target, with `reason="oversized"`, in either mode; the other targets still get it.

A queue's `capacity` and `capacity_bytes` count leased updates too. While a backpressure queue refuses updates, `tgin_consumer_backpressure_seconds_total` grows.

**Memory.** `capacity_bytes` counts each update's own bytes. A long-poll update shares memory with the rest of its `getUpdates` response (each response is copied once, to its exact size, and its updates are slices of it), so a queued update keeps its whole response alive. A consumer that receives a small share of a busy bot's updates (an `/admin` command route, an audit filter) and whose clients or endpoint are down can therefore hold up to `limit` × its `capacity_bytes`; a lower `long_poll.limit` bounds this. A webhook update is copied to its own allocation.

**At least once, while the process lives.** Queues live in memory: a crash or kill loses what they hold, and so does a shutdown that ends before they drain. A long poll moves its `getUpdates` offset past an update only once the update is admitted (or matched no rule), and Telegram learns the new offset on the next `getUpdates`. Updates not admitted when tgin stops are not confirmed, so Telegram sends them again to the next poller; after a restart, tgin starts from Telegram's unconfirmed updates, so the last batch admitted before the stop may be delivered a second time. A webhook update is answered `200` only once it is queued; a `503` while shutting down may still let a queued copy through, so Telegram's retry can produce a duplicate. Consumers should tolerate duplicates, for example by `update_id`.

**Pull leases.** A poll leases updates to the polling instance. The instance acknowledges them with the next poll's `offset`, as with Telegram: every update it holds with an `update_id` below a positive `offset` is acknowledged, a negative `offset` acknowledges all it holds, and `0` acknowledges nothing. A poll first returns what the instance still holds from `offset` on (a response lost on the way is delivered again) and renews those leases; otherwise it leases new updates, waiting up to its `timeout`. A lease that is neither acknowledged nor renewed within `lease_timeout` (default 2 minutes) returns to its place in the queue for any instance. A client that always sends `offset=0` never acknowledges anything and receives the same updates again on every poll.

**Ordering.** With `ordering: per_chat`, at most one update per chat (by `chat.id`, else by the user's id) is leased at a time; the next one of that chat waits until the current one is acknowledged (pull) or delivered or dead-lettered (push). Updates with neither id are never held back. With `ordering: none` (the default) several updates of a chat can be in flight at once.

**Push retries.** A push consumer leases up to `max_in_flight` updates and posts each one to an endpoint chosen by `balance` among those whose breaker admits a request:

- a `2xx` answer delivers it;
- `408`, `429`, any `5xx`, a timeout or a network error is transient: the update is retried after a backoff, a random delay up to `retry.initial` doubled per attempt and capped at `retry.max`, or the endpoint's `Retry-After` if that is longer (capped at `retry.max`);
- any other `4xx`, any `3xx` (tgin never follows redirects), and a `508 Loop Detected` that carries the pushing process's own `Via` (the endpoint is this tgin itself, which refused its own push) dead-letter it at once. Any other `508` is a transient `5xx`.

An update is dead-lettered after `retry.max_attempts` attempts, or when its next attempt would come after `retry.max_age` since tgin queued it (time a long-poll update was held back by backpressure, or a webhook update spent in Telegram's retries, is not counted). A dead-lettered update is dropped: logged as `update dead-lettered` (consumer, bot, `update_id`, update type, endpoint index and last error; never the payload or URL) and counted as `tgin_consumer_dropped_total{reason="dead_letter"}`. There is no dead-letter queue.

**Circuit breakers.** Each endpoint has a breaker: `breaker.failure_threshold` consecutive transient failures open it for `breaker.open_for`, then it admits a single probe request, whose success closes it. A permanent failure (the endpoint answered) counts as a success for the breaker. While every breaker is open, deliveries wait; an update that waits past `retry.max_age` is dead-lettered.

**Backpressure or drop.** `on_full: backpressure` (the default) never drops an update for a full queue, but a consumer that stops taking updates fills its queue and then stalls its bot's source, and so every other consumer of that bot. `on_full: drop_oldest` keeps the source moving and drops that consumer's oldest updates instead. Choose `drop_oldest` for consumers whose outage must not hold back the others.

## Pull consumers: the `getUpdates` endpoint

A pull consumer is served on the data plane at `GET` or `POST /c/<consumer>/getUpdates` and at each path in its `aliases`. It speaks the Bot API's `getUpdates`:

- Parameters come from the query string and, when there is a body, from a JSON (`Content-Type: application/json`) or form body, which fills in those the query string lacks: a parameter in both places takes the query string's value, as telegram-bot-api reads it. `offset` (default 0), `limit` (default 100, clamped to 1 … 100) and `timeout` in seconds (default 0, clamped to 0 … 50) are read; `allowed_updates` and anything else is ignored (routing is the config's). `multipart/form-data` is refused.
- The answer is `{"ok":true,"result":[…]}` with the updates exactly as Telegram sent them. Errors look like the Bot API's: `404 {"ok":false,"error_code":404,"description":"Not Found"}` for an unknown consumer, `401 … "Unauthorized"` without the right token, `400 … "Bad Request: …"` for bad parameters.
- With `auth_token` set, a poll must send `Authorization: Bearer <auth_token>`.
- The polling **instance** is named by the `X-Tgin-Instance` header (1 to 64 visible ASCII characters) or, without it, by the client's IP address (from `X-Forwarded-For` only when the peer is in `server.trusted_proxies`). Leases and acknowledgements are per instance, so processes that share an address — behind NAT, or several on one host — must each send their own `X-Tgin-Instance`.

Most bot frameworks point at tgin by changing their Bot API base URL to the gateway, `http://<tgin>:8080/c/<consumer>`: their `getUpdates` then polls this consumer and every other call goes to Telegram through tgin ([Framework setup](#framework-setup)).

## Push consumers: the requests tgin sends

Each attempt is a `POST` of the update's JSON to the endpoint `url` with these headers:

| Header | Value |
|---|---|
| `Content-Type` | `application/json` |
| `X-Tgin-Attempt` | The attempt number, from 1. |
| `X-Tgin-Source` | The name of the bot the update came from. |
| `X-Tgin-Update-Type` | The update type, such as `message`. |
| `X-Telegram-Bot-Api-Secret-Token` | The endpoint's `secret_token`, if set (1 to 256 of `A-Za-z0-9_-`, checked at load). |
| `Via` | `1.1 tgin-<16 hex digits>`: a pseudonym chosen at random when the process starts. |

Each request is bounded by the endpoint's `timeout` (default 10 s). The endpoint should answer `2xx` once it has taken the update; see [Delivery semantics](#delivery-semantics) for the other answers. A `2xx` answer may carry a Bot API call, which tgin makes for the endpoint ([Webhook replies](#webhook-replies)).

Every push carries tgin's `Via`, so a push that comes back to the same tgin is refused instead of acted on ([Loop detection](#loop-detection)).

## Webhook ingress

A webhook bot's route is `POST <path>` on the data plane (default path `/webhook/<bot name>`). A request is answered, in this order of checks:

1. `405` for a method other than `POST`;
2. `403` if the client address is not in `ip_allowlist` (`telegram` means 149.154.160.0/20 and 91.108.4.0/22; the client address is the TCP peer's, or the right-most `X-Forwarded-For` address not in `server.trusted_proxies` when the peer is a trusted proxy);
3. `401` without the right `X-Telegram-Bot-Api-Secret-Token` (compared in constant time);
4. `503` with `Retry-After: 5` once ingest has stopped (shutdown);
5. `413` for a body over 1 MiB, `400` for a body the client fails to send;
6. `400` for a body that is not a JSON object with an integer `update_id`;
7. `503` with `Retry-After: 5` when a consumer pushes back, or when ingest stopped while the update was being queued;
8. otherwise `200` with an empty body, also when no rule matches.

Checks 1 to 4 read only the request head; the body is read only for an authorized request. A request still running after 30 s is answered `408`.

`secret_token: auto` (the default) derives the secret from the bot token (HMAC-SHA256 of the token over a fixed context, 43 characters), so it is the same across restarts and replicas. tgin sends it to Telegram only with `register: true`; with `register: false`, set `secret_token` to the secret you register yourself, or `none`. `secret_token: none` checks nothing: anyone who can reach the path can post updates.

`register: true` calls `setWebhook` whenever the bot's source starts (at start and when a reload starts or restarts it). A failure is logged, not fatal: Telegram may still have the webhook from an earlier run.

A long-poll bot and Telegram's webhook exclude each other: with `delete_webhook_on_start: true` (the default) a long poll calls `deleteWebhook`, keeping pending updates, before its first poll and again when a poll finds a webhook set (but not again within 60 s of deleting it, which means another process keeps setting it).

## Bot API gateway

The data plane also serves the Bot API to the bots behind tgin. A downstream sets its framework's Bot API base URL to tgin instead of `https://api.telegram.org` ([Framework setup](#framework-setup)) and keeps using the bot's token. tgin forwards each call to the bot's `api_url`, answers itself the calls that would disturb tgin or the bot's other downstreams, holds the bot's sends to Telegram's flood limits, and absorbs short `429`s. The gateway is part of the data plane, so it needs a `server` block.

### Routes and auth

| Path | Serves |
|---|---|
| `/bot<token>/<method>` | A Bot API method, for any downstream of the bot. |
| `/c/<consumer>/bot<token>/<method>` | The same, on behalf of `<consumer>`, which the bot must route to: `getUpdates` there polls that pull consumer, and `setWebhook` may give that push consumer its endpoint. |
| `/file/bot<token>/<file_path>`, `/c/<consumer>/file/bot<token>/<file_path>` | A file download (the `file_path` of `getFile`). |

Configured routes (webhook paths, `/c/<consumer>/getUpdates` and aliases) are matched first; every other data-plane path goes to the gateway. It serves `GET` and `POST`, and passes the query string on as it came. A path is a gateway path when:

- its token segment, percent-decoded once (python-telegram-bot sends the `:` as `%3A` in file URLs), has Telegram's shape `<digits>:<secret>`, with 1 to 128 of `A-Za-z0-9_-` after the colon;
- the method, the last segment, is a letter followed by up to 63 letters, digits or `_`; it is matched ignoring ASCII case, as Telegram matches it (`GETME` is `getMe`);
- a file path is not empty and has no `.` or `..` segment (also when written `%2e`) and no `\`;
- `<consumer>` is a valid name.

The token in the path is the credential: tgin compares it in constant time with the token of every configured bot, and answers

1. `404 Not Found` to a path that is no gateway path, a malformed or double-encoded (`%253A`) token included, as Telegram does;
2. `401 Unauthorized` to a token no configured bot has (counted in `tgin_gateway_unauthorized_total`);
3. `404 Not Found: consumer not found` to `/c/<consumer>/…` when the bot does not route to that consumer (a removed pull consumer the bot fed stays reachable while it drains);
4. `405 Method Not Allowed` to a method other than `GET` or `POST`.

None of these sends anything to Telegram. Everything tgin answers itself on a gateway path has Telegram's shape: `{"ok":false,"error_code":N,"description":"…"}` with the HTTP status equal to `error_code`, or `{"ok":true,"result":…}`. No gateway path, token or upstream URL reaches a log line, a metric or an error body; logs name the bot and the method. A bot whose token is not shaped `<digits>:<secret>` loads with a warning, and the gateway does not serve it.

Whoever holds a bot's token can use the gateway as that bot, as with Telegram, and can poll every pull consumer the bot routes to on `/c/<consumer>/bot<token>/getUpdates`, where the bot token is the credential and the consumer's `auth_token` is not asked for.

### Forwarding

A call that no intercept answers goes to `{api_url}/bot<token>/<method>`, and a download to `{api_url}/file/bot<token>/<file_path>`, keeping the path of `api_url`. Both bodies stream in both directions and keep their `Content-Length` when they have one, so uploads and downloads of any size pass through in constant memory. tgin reads a body whole only where it must, each with a bound: the JSON or urlencoded body of a method in [`rate_limit.methods`](#rate-limits) (its chat, and a possible resend), up to 64 KiB; the parameters of an [intercepted](#intercepts) call, up to 64 KiB; Telegram's `429` answers and the `getMe` answer that fills tgin's cache, up to 64 KiB; Telegram's error answer to a webhook reply, up to 64 KiB; and [webhook replies](#webhook-replies), up to 1 MiB.

Only these headers pass, except one that the message's `Connection` header names:

- to Telegram: `Content-Type`, `Content-Length`, `Accept`, `Accept-Encoding` and `User-Agent`, and on file paths also `Range`, `If-None-Match` and `If-Modified-Since`;
- back to the client: `Content-Type`, `Content-Length`, `Content-Encoding`, `Content-Disposition`, `Retry-After`, `Cache-Control`, `ETag`, `Last-Modified`, `Accept-Ranges` and `Content-Range`.

Connecting to Telegram may take 5 s; after that no total timeout applies, and the bot's `gateway.upstream_idle_timeout` (`1m`) bounds the time in which nothing moves. Telegram's response headers must arrive within it of the request body's last bytes, whether the client stopped sending or Telegram stopped reading, and each part of the response body within it of the previous one. A transfer that keeps moving is never cut. A request that gets no response is answered `502 Bad Gateway: Telegram is unreachable`; a response body that fails or stalls after its headers aborts the client's connection, except for the two answers tgin reads whole before it replies (Telegram's `429` to a call whose body tgin holds, and the `getMe` answer that fills its cache), where the client gets `502 Bad Gateway: Telegram is unreachable` instead (counted `upstream_error`). The gateway has no circuit breaker: each request fails on its own. Its requests use the `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` environment if set and never follow redirects.

tgin knows the methods of Bot API 10.3. A method it does not know is forwarded like any other and counted with the metric label `other`; only known methods are intercepted.

### Intercepts

| Method | On `/bot<token>/…` | On `/c/<consumer>/bot<token>/…` |
|---|---|---|
| `getUpdates` | `409 Conflict: tgin serves getUpdates per consumer at /c/<consumer>/bot<token>/getUpdates`: tgin polls Telegram itself, and a forwarded call would take its updates. | The pull consumer's queue, exactly as `/c/<consumer>/getUpdates` serves it ([Pull consumers](#pull-consumers-the-getupdates-endpoint)), without its `auth_token`. A consumer that is not a pull consumer of this bot: `409 Conflict: <consumer> is not a pull consumer of this bot`. |
| `getMe` | The bot's identity, as Telegram sent it to tgin's own `getMe` (at start for bots with an ingress); forwarded until tgin has it, and the first successful answer is kept. | Same. |
| `setWebhook` | `Webhook was set`, or `Webhook was deleted` for an empty `url`; nothing changes. | With `accept_set_webhook: true` on a push consumer, sets its runtime endpoint ([`setWebhook` capture](#setwebhook-capture)); otherwise as on the bot path. |
| `deleteWebhook` | `Webhook was deleted`; nothing changes. | With `accept_set_webhook: true` on a push consumer, removes its runtime endpoint; otherwise as on the bot path. |
| `getWebhookInfo` | `{"url":"","has_custom_certificate":false,"pending_update_count":0}` | `url` is the consumer's runtime endpoint, or `""`; `pending_update_count` counts the consumer's updates queued or leased. |
| `logOut`, `close` | `{"ok":true,"result":true}`, and a warning in the log. | Same. |

`drop_pending_updates` on a `deleteWebhook`, or on a `setWebhook` that a consumer accepts, drops the calling bot's queued updates from the calling consumer (as `purge` drops them) when that consumer has `honor_drop_pending: true`, and is ignored otherwise; on the bot path it never drops anything. As Telegram drops only the caller's pending updates, other bots' updates stay: those of another bot feeding the same push consumer, and those a bot left before a reload moved its routes elsewhere.

The answers follow telegram-bot-api's `Client.cpp`: its descriptions (`Webhook is already set`, `Webhook is already deleted`, `Bad Request: invalid webhook URL specified`, `Bad Request: secret token is too long`, `Bad Request: secret token contains illegal characters`), its order of parameters (the query string first, then a JSON, urlencoded or multipart body's text fields) and its booleans (`true`, `yes` or `1`, in any case). Intercepted calls (except a `getMe` before tgin knows the bot's identity) never reach Telegram and count as outcome `local`, so a framework that calls `deleteWebhook` when it starts polling (python-telegram-bot, grammY and Telegraf do), `setWebhook` or `logOut` cannot remove tgin's webhook, drop the updates Telegram holds for it or log the bot out. The body of a `setWebhook`, a `deleteWebhook` or a consumer-scoped `getUpdates` must arrive within the bot's `upstream_idle_timeout` (else `408 Request Timeout`) and whole (else `400 Bad Request: the request body ended early`); a body announced over 64 KiB is refused at once (`413 Request Entity Too Large`, or `400` for `getUpdates`, as on `/c/<consumer>/getUpdates`).

### `setWebhook` capture

A push consumer with `accept_set_webhook: true` takes its endpoint from a `setWebhook` on its gateway path, `/c/<consumer>/bot<token>/setWebhook`, so a webhook bot can register itself with tgin as it would with Telegram. Validation allows the flag only on a consumer fed by one bot, and only that bot's token can set the endpoint; such a consumer may have no configured `endpoints` at all.

- The `url` is `http` or `https` (`https` when it has no scheme, as on Telegram); a `secret_token`, 1 to 256 of `A-Za-z0-9_-`, is sent as `X-Telegram-Bot-Api-Secret-Token`. The same call again answers `Webhook is already set`.
- The runtime endpoint comes after the configured ones, with a breaker of its own (the consumer's `breaker` settings) and a 10 s timeout. `/v1/status` lists it with `"runtime": true`, and metrics label it `endpoint="runtime"`.
- `setWebhook` with an empty `url`, or `deleteWebhook`, removes it.
- It survives a reload that keeps the consumer, `accept_set_webhook: true`, the bot feeding the consumer and that bot's token. A reload that changes any of them clears it (logged), so rotating a bot's token evicts the endpoint its holder set; a restart forgets it. A `setWebhook` or `deleteWebhook` still in flight when its token is rotated changes nothing and answers `401 Unauthorized`. A removed consumer that still drains keeps its endpoint to deliver what it holds, unless the reload changed its bot's token.
- A [webhook reply](#webhook-replies) from the runtime endpoint runs only with the token whose `setWebhook` set the endpoint. Once a reload has changed the bot's token, a reply still on its way is refused (counted `refused`), never sent with the new token.
- A push consumer with no endpoint at all leases nothing: its updates wait in the queue, bounded by `capacity` and `on_full`, and `retry.max_age` still counts from admission.

**Trust model.** With `accept_set_webhook`, whoever holds the bot's token chooses where tgin posts the consumer's updates: any `http` or `https` URL tgin can reach, from inside your network. Enable it only where every holder of the token is trusted. The one destination tgin refuses is itself ([Loop detection](#loop-detection)): a runtime endpoint pointed at tgin's admin API, webhook ingress, pull endpoints or gateway dead-letters each update instead of acting on it.

### Rate limits

Each bot's `gateway.rate_limit` holds its calls to Telegram's flood limits before they leave, so that the bot's downstreams together stay within them instead of collecting `429`s. It covers the methods in `methods` (`send*`, `copyMessage*` and `forwardMessage*` by default: a method name or a prefix followed by `*`, ignoring case) and [webhook replies](#webhook-replies) of those methods. Three quotas apply, each a rate with a burst:

| Quota | Default | Counts |
|---|---|---|
| `global` | 30 per second, burst 30 | every covered call of the bot |
| `per_chat` | 1 per second, burst 3 | the calls to one `chat_id` |
| `per_group` | 20 per minute, burst 20 | on top of `per_chat`, the calls to a negative `chat_id` or an `@username` (groups, supergroups and channels) |

A call books every quota it needs, or none, each for the instant the call leaves: when one quota delays it, the others are charged for that later slot, so a backlog on `global` does not let one chat's calls bunch up beyond `per_chat`. `global` is a calendar of the bot's slots, at most `burst` calls in any window of `burst × per / count` (1 s with the defaults): a call delayed by its chat or group books the earliest free slot at or after its start, and the free slots before it stay free, so a chat over its quota never holds up the bot's other chats. Its chat is the query string's `chat_id`, else, when tgin holds its body (a JSON or urlencoded body up to 64 KiB), the body's, as telegram-bot-api reads a parameter given in both places; a body that streams unread (a multipart upload, or a longer body) leaves only the query string's. A call whose chat tgin does not know counts against `global` only. `@username` keys ignore case.

What a call beyond the limits gets depends on `mode`:

- `shape` (the default): it waits until its quotas admit it, the longest of their delays, if that is at most `max_delay` (`5s`); a longer wait is answered `429` at once. `max_delay: 0s` behaves like `reject`.
- `reject`: `429` at once.
- `off`: no limits.

tgin's `429` is Telegram's: `{"ok":false,"error_code":429,"description":"Too Many Requests: retry after N","parameters":{"retry_after":N}}` with `Retry-After: N`, N being the wait in whole seconds, rounded up and at most 3600. Delays and refusals count in `tgin_gateway_ratelimited_total`. The quotas belong to the process: several tgin processes serving one bot count separately. They survive a reload that keeps the bot's token and `rate_limit`, and every 60 s the `gateway-sweep` task forgets the chats whose quotas are full again.

### 429 smoothing

When Telegram answers `429` to a call whose body tgin holds (a method in `rate_limit.methods`, with a JSON, urlencoded or empty body up to 64 KiB) and its `parameters.retry_after` is at least 1 s and at most `gateway.retry_429.max_wait` (`10s`), tgin waits that long from the `429`, books the rate limits again (unless `mode: off`; a refusal passes Telegram's `429` on) and sends the call once more. The client gets the second answer and never sees the first. This retry does not depend on `mode`: with `mode: off` tgin still holds the bodies of those methods while `max_wait` is above 0. In every other case (a streamed body, a `429` without `retry_after` or with a longer one, `max_wait: 0s`) the `429` reaches the client as Telegram sent it. So a call goes to Telegram at most twice, and a streamed body only once. Each retry counts in `tgin_gateway_retries_total`.

### Webhook replies

A push endpoint can answer an update with a Bot API call, as a webhook can answer Telegram. With `webhook_reply: execute` (the default), a `2xx` answer whose `Content-Type` contains `application/json`, `application/x-www-form-urlencoded` or `multipart/form-data` (in any case) and whose body is not empty is a call: its method is the body's `method` parameter (a JSON member, a form field or a multipart text field). tgin

- refuses it, as Telegram does, when the method is missing or empty, starts with `get`, or is `setWebhook`, `deleteWebhook`, `close` or `logOut`, in any case; it also refuses, on its own, a method name longer than 64 bytes or with a character outside `A-Za-z0-9_`;
- does not execute a body over 1 MiB (logged);
- otherwise applies the bot's rate limits (in `shape` mode the call may wait; a call the limits refuse is skipped; a multipart reply counts against `global` only), then POSTs the body unchanged, with its `Content-Type`, to `{api_url}/bot<token>/<method>`. Nothing is re-encoded, multipart files included. The call is made once, never retried, and bounded by 30 s. Nobody sees Telegram's answer, as with Telegram; a failure is logged with Telegram's `description`, tokens masked.

The reply runs before the update is acknowledged, so under `ordering: per_chat` the chat's next update waits for it. It never fails the delivery: the endpoint has processed the update, so a refused or failed reply still counts the update delivered. tgin never executes the body of a non-`2xx` answer, nor one that fails to arrive whole (logged), and with `webhook_reply: ignore` it reads no answer body. A graceful shutdown waits for a reply in progress. Replies share the delivery's at-least-once guarantee: an update that tgin is stopped by force while its reply runs is not acknowledged, and if the update is delivered again ([Delivery semantics](#delivery-semantics)), so is its reply. Outcomes count in `tgin_webhook_replies_total`.

Frameworks that reply this way: aiogram's `SimpleRequestHandler(handle_in_background=False)` when a handler returns a method object (sent as multipart), grammY with `canUseWebhookReply`, and Telegraf with `webhookReply: true` (its default) for `answerCallbackQuery`, `answerInlineQuery`, `deleteMessage`, `leaveChat` and `sendChatAction`. python-telegram-bot never replies. aiogram's default `handle_in_background=True` answers `{}` to every update, which counts as `refused` (no method); use `webhook_reply: ignore` with it.

### Loop detection

tgin never acts on its own pushes. Every push carries `Via: 1.1 tgin-<pseudonym>`, 16 hex digits chosen at random when the process starts, and both planes answer `508 Loop Detected` to a request whose `Via` lists the process's own pseudonym (RFC 9110 §7.6.3): the admin plane `{"error":"loop detected"}`, the data plane Telegram's shape, each carrying the process's `Via`. So an endpoint that points back at tgin (its webhook ingress, a pull endpoint, the gateway, or the admin API) is refused, and the pushing dispatcher, seeing its own `Via` on the `508`, dead-letters the update at once. Only that `508` is final: any other `508` is a transient `5xx`, retried. The refusal is logged as a warning at most once a minute per plane, with the bot from `X-Tgin-Source`. Another tgin process has another pseudonym, so chained instances still pass updates to each other.

The gateway's calls to Telegram and its webhook replies carry the same `Via`, after any `Via` the downstream's request carried. A bot whose `api_url` points at tgin itself therefore gets `508` (counted `upstream_error`) instead of tgin forwarding the call to itself without end.

### Gateway metrics

`tgin_gateway_requests_total{bot,method,outcome}` counts every gateway request of a configured bot once, and every webhook reply that went to Telegram. `method` is the method's canonical name (`sendMessage`, whatever case the client used), `other` for a method tgin does not know, and `file` for downloads; a method's series appear on its first request. `outcome` is one of:

| `outcome` | The request |
|---|---|
| `ok` | Telegram answered with a status that is not an error. |
| `rejected` | Telegram answered with a `4xx` other than `429`, or tgin refused it for this bot: `404` for a consumer the bot does not route to, `405`, or `401` for a token rotated while the request was in flight. |
| `rate_limited` | It got a `429`: from the bot's rate limits, or Telegram's own, passed on. |
| `upstream_error` | Telegram answered with a `5xx`, or its response body failed, or stalled for `upstream_idle_timeout`, after the headers. |
| `unreachable` | Telegram gave no response: it could not be reached, or sent no response headers within `upstream_idle_timeout` of the request's last progress (the client stalled its upload, or Telegram stopped reading it). Answered `502`. |
| `client_closed` | The client went away mid-request: its body broke off or was malformed (`400 Bad Request: the request body ended early`, which nobody reads), it left while tgin waited for Telegram's response headers, a rate-limit delay or Telegram's `retry_after` (no answer; Telegram may have received the call), or the body of an intercepted call did not arrive within `upstream_idle_timeout` (`408`). |
| `local` | An intercept answered it. |

Requests with an unknown token count only in `tgin_gateway_unauthorized_total`, without labels, so no token ever creates a series; paths that are no gateway path are not counted. `tgin_gateway_upstream_seconds{bot,method}` is the time from sending a request (or a reply) to Telegram's response headers. `tgin_gateway_ratelimited_total{bot,scope,action}` counts delays and refusals by the rate limits, `scope` `global`, `chat` or `group` (the quota that decided), `action` `delayed` or `rejected`, for requests and webhook replies alike. `tgin_gateway_retries_total{bot}` counts the second sends after Telegram's `429`.

`tgin_webhook_replies_total{consumer,outcome}`:

| `outcome` | The reply |
|---|---|
| `executed` | Telegram answered `2xx`. |
| `failed` | Telegram answered otherwise or not within 30 s, or the gateway does not serve the consumer's bot. |
| `refused` | No method, or one that is refused (see above), or it came from a runtime endpoint whose bot's token changed since its `setWebhook`. |
| `too_large` | Its body was over 1 MiB. |
| `rate_limited` | The bot's rate limits refused it. |

### Framework setup

Point each downstream at its consumer's gateway path, `http://<tgin>:8080/c/<consumer>`, with the bot's own token. A polling bot then polls the pull consumer; a webhook bot behind a push consumer with `accept_set_webhook: true` registers its URL with tgin; every other call goes to Telegram through tgin. The [end-to-end suite](tests/e2e) runs the first four frameworks below, in these versions, with these settings:

| Framework | Setting | Quirks |
|---|---|---|
| aiogram 3.31 | `Bot(token, session=AiohttpSession(api=TelegramAPIServer.from_base("http://tgin:8080/c/<consumer>")))` | `from_base` strips a trailing `/`; files go to `<base>/file/bot<token>/…`. aiogram does not confirm the last batch when it stops polling, so a restarted process gets it again. |
| python-telegram-bot 22.8 | `Application.builder().token(token).base_url("http://tgin:8080/c/<consumer>/bot").base_file_url("http://tgin:8080/c/<consumer>/file/bot")` | Both URLs end in `/bot`: PTB appends the token to them. It percent-encodes file paths, so the token's `:` arrives as `%3A`, which tgin decodes. |
| grammY 1.46 | `new Bot(token, { client: { apiRoot: "http://tgin:8080/c/<consumer>" } })` | No trailing `/`: grammY refuses one. Its core has no download helper; fetch `${apiRoot}/file/bot${token}/${file_path}`. |
| Telegraf 4.16 | `new Telegraf(token, { telegram: { apiRoot: "http://tgin:8080/c/<consumer>/" } })` | The trailing `/` is required: Telegraf resolves `./bot<token>/<method>` against `apiRoot`, so without it the consumer segment is dropped. File links use the same root. |
| go-telegram-bot-api 5.5 | `tgbotapi.NewBotAPIWithAPIEndpoint(token, "http://tgin:8080/c/<consumer>/bot%s/%s")` | The file URL is a hard-coded constant, `https://api.telegram.org/file/bot%s/%s`, so downloads bypass tgin. |

Processes polling one pull consumer from one address (several on one host, or behind NAT) must each send their own `X-Tgin-Instance` header; otherwise tgin treats them as one instance.

An example, a pull consumer for the polling processes and a push consumer a webhook bot registers with:

```yaml
bots:
  - name: shop
    token: ${env:SHOP_TOKEN}
    ingress:
      long_poll: {}
    routes:
      - match:
          update_types: [callback_query]
        to: [hooks]
      - to: [workers]

consumers:
  workers:
    pull: {}          # aiogram: TelegramAPIServer.from_base("http://tgin:8080/c/workers")
  hooks:
    push:
      accept_set_webhook: true   # the bot calls setWebhook on http://tgin:8080/c/hooks
      honor_drop_pending: true
```

[examples/simple](examples/simple) runs a complete stack.

### Self-hosted Bot API server

A bot's `api_url` may point at a self-hosted [telegram-bot-api](https://github.com/tdlib/telegram-bot-api) server instead of `https://api.telegram.org`. Its method calls and tgin's long poll work through it as through Telegram, and so does the gateway's forwarding of method calls. Two things do not work through tgin with such a server:

- **File downloads.** The gateway forwards downloads to `{api_url}/file/bot<token>/<file_path>`, a route the server does not serve; with `--local`, `getFile` answers an absolute path on the server's disk instead of a `file_path` to download.
- **Webhook ingress from it.** `register: true` registers only an `https` `public_url`, and `ip_allowlist: telegram` names Telegram's networks, not the server's.

Never expose the server itself: it has no authentication of its own, and its statistics port (`--http-stat-port`) shows the bots' tokens. Keep it on a network only tgin reaches.

Two more caveats hold for this version:

- **`--local` and untrusted downstreams.** With `--local`, the server reads a `file://` URI or an absolute path given for a file as a file on its own disk. tgin passes such parameters through unchanged, in gateway calls and in executed [webhook replies](#webhook-replies), so whoever holds a configured bot's token can make the server read and upload any file it can read; a private network for the server does not prevent that. Do not run a `--local` server behind tgin with downstreams you do not trust.
- **Large uploads.** `gateway.upstream_idle_timeout` (`1m`) also runs while the server, having received the whole upload, sends it on to Telegram before it answers. A large `--local` upload can then get `502 Bad Gateway: Telegram is unreachable` while the server still sends it, and a client that retries sends it twice. Raise the bot's `gateway.upstream_idle_timeout` above the longest upload the server must forward.

### Checking against real Telegram

Before a release, a maintainer runs `tests/smoke/real_telegram.py` (Python 3, standard library only) against the real Bot API with a test bot. It starts tgin with [its own config](tests/smoke/real_telegram.yaml) and the [webhook downstream](tests/smoke/hook.py), and reads the token from `TGIN_SMOKE_TOKEN` or from a file given as its argument; it never prints the token. Phase A needs no chat: identity, intercepts, proxied calls, unknown tokens, a capture and metrics. Phase B asks a person to send the bot a message containing `tgin-smoke-reply`, then checks the long poll, the pull consumer, the pushes, a webhook reply, per-chat shaping and a multipart upload with its download. CI never runs it; `--help` lists its options.

## Hot reload

**Triggers.** A reload re-reads the config file. It runs on:

- `SIGHUP` (Unix only);
- `POST /v1/reload` on the admin API, or `tgin admin reload`, which answer with the result;
- a change of the config file while `reload.watch` is on (the default). tgin watches the file's directory, waits `reload.debounce` (500 ms) of quiet, and also re-reads the file every 2 s for file systems that send no change events; a change is a change of the file's bytes. When file events cannot be set up at all (an unreadable directory, the system's watch limit reached), tgin logs `file events unavailable (…); polling every 2 s` and keeps re-reading the file. A ConfigMap volume updates mounted files in place, but a file mounted with `subPath` is never updated by Kubernetes, so tgin cannot see the new version. Likewise, a Docker bind mount of the single file keeps the original inode: an edit saved by renaming over the file (most editors, `sed -i`, Ansible) never reaches the container, though reloads still succeed. Mount the directory instead.

Reloads run one at a time, in order. While tgin shuts down, `POST /v1/reload` answers `503 {"error":"draining"}` and `SIGHUP` is ignored.

**Rejection.** A config that cannot be read, fails validation or cannot be applied changes nothing: the running config stays, `tgin_config_reloads_total{outcome="rejected"}` grows, the errors are logged on one line, and `POST /v1/reload` answers `422` with `{"errors":[{"path","line","column","message","hint"}, …]}` (the location fields are `null` for an error without one).

**What applies live.** An applied reload raises the generation (`tgin_config_generation`, 1 at start) by one and answers with a report:

```json
{"generation":2,"added":["audit"],"removed":[],"changed":["frontdesk"],
 "sources_added":[],"sources_removed":[],"sources_restarted":["shop"],
 "restart_required":[]}
```

- Rules and targets apply to the next update.
- A consumer whose settings changed keeps its queue and applies them live; a push consumer whose endpoints, `balance`, `max_in_flight`, `retry` or `breaker` changed gets a new endpoint set with fresh breakers. A reload that only adds bots keeps every push consumer's endpoints, with their breakers and in-flight counts; only the bot names it reports in `X-Tgin-Source` grow.
- A new consumer starts empty. A consumer whose mode changed (pull ↔ push) is a new, empty instance; the old one drains as if removed.
- A removed consumer stops receiving updates at once and drains for `reload.drain_timeout` (30 s); what it still holds then is dropped as `tgin_consumer_dropped_total{reason="reload"}`, and its metric series go. A removed pull consumer stays reachable while it drains, on `/c/<name>/getUpdates` and on its aliases (unless the new config gives the path to something else), with its old `auth_token`, so its clients can fetch and acknowledge what it holds.
- A consumer added back with the same mode while its old instance still drains gets that instance back, with its queue and the new settings; a revived push consumer gets fresh breakers and stays paused if it was paused.
- A bot whose `token`, `api_url` or `ingress` changed is restarted alone; added bots start, removed bots stop. Webhook paths and pull aliases change without restarting the listeners.
- `log.level`, `admin.token` (a rotated token applies to the next request), `admin.public_metrics`, `server.trusted_proxies`, `reload.*` and `runtime.lame_duck` / `runtime.shutdown_grace` (used when a shutdown begins) apply live.
- A bot's `gateway` settings apply live, without restarting its source. Its rate-limit quotas keep their state while its token and `rate_limit` stay, and the identity tgin learned from `getMe` while its token stays. A push consumer's `webhook_reply` applies to the next delivery, keeping its endpoints and breakers; `accept_set_webhook` and `honor_drop_pending` apply to the next request. A reload that turns `accept_set_webhook` off, gives the consumer another feeding bot (or none), or changes that bot's token clears the consumer's runtime endpoint ([`setWebhook` capture](#setwebhook-capture)).
- A changed TLS certificate or key file is loaded again on every reload; a certificate that does not load leaves the old one in use.
- Secrets of removed bots and consumers stay masked in the logs.

**What needs a restart.** These settings are set up once at start. A reload that changes them applies everything else, lists them in `restart_required` and logs a warning: `server.listen` (including adding or removing the `server` block), `server.max_connections`, `server.header_read_timeout`, `server.tls` (turning TLS on or off), `admin.listen`, `runtime.worker_threads` and `log.format`.

Removing `admin.token` while an admin listener is bound to a non-loopback address does not open the API: until a restart applies the new `admin.listen`, every protected endpoint answers `401`, and tgin logs a warning.

## Admin API v1

The admin plane is its own plain-HTTP listener, `admin.listen` (default `127.0.0.1:9090`). It binds before the bots start and closes last on shutdown, so the probes and the status answer throughout.

| Method and path | Auth | Answer |
|---|---|---|
| `GET /livez` | none | `200 ok` |
| `GET /readyz` | none | `200` when ready, else `503`; body `{"ready":…,"phase":…,"components":{…}}` |
| `GET /metrics` | token, unless `admin.public_metrics: true` | OpenMetrics text |
| `GET /v1/status` | token | the [status](#status) |
| `GET /v1/config` | token | the running config, with defaults, secrets redacted |
| `POST /v1/reload` | token | `200` reload report; `422 {"errors":[…]}`; `503 {"error":"draining"}` during shutdown |
| `POST /v1/consumers/<name>/pause` | token | `204` |
| `POST /v1/consumers/<name>/resume` | token | `204`; `409 {"error":"draining"}` during shutdown |
| `POST /v1/consumers/<name>/drain` | token | `204` |
| `POST /v1/consumers/<name>/purge` | token | `200 {"purged":N}` |

**Auth.** The token is sent as `Authorization: Bearer <admin.token>` and compared in constant time; a request without it gets `401 {"error":"unauthorized"}` with `WWW-Authenticate: Bearer`. Without `admin.token` the API needs no token, which validation allows only when every `admin.listen` address is a loopback address. An unknown consumer answers `404 {"error":"unknown consumer"}`.

**Readiness.** `/readyz` is `200` while tgin runs and every component is ready: `engine`, `admin_plane`, `data_plane` (with a `server` block) and `source:<bot>` for each bot with an ingress. A source is not ready until it has started, also while a reload restarts it; once shutdown begins, `/readyz` answers `503` with `"phase":"draining"`. A source that later fails does not make tgin unready; watch `tgin_source_state` for that.

**Consumer operations.**

- `pause` stops handing out new updates; the queue still admits them (so a paused `backpressure` consumer fills up and then holds back its bot), and a pull instance still gets back what it holds.
- `resume` hands out updates again, and also ends a drain started by `drain`. During shutdown it answers `409`, as the shutdown's drain must not end.
- `drain` lets a pull consumer's pollers wind down: a poll on its empty queue answers within 500 ms instead of waiting its full timeout, until `resume`.
- `purge` drops the updates waiting in the queue (not those leased), counted as `tgin_consumer_dropped_total{reason="purge"}`.

### Status

`GET /v1/status`:

```json
{
  "version": "1.0.0-dev",
  "generation": 1,
  "phase": "running",
  "sources": [
    {"name": "shop", "kind": "bot", "ingress": "long_poll", "state": "running",
     "updates_total": 120, "errors_total": 0}
  ],
  "consumers": [
    {"name": "audit", "mode": "push", "depth": 0, "leased": 1, "in_flight": 1,
     "paused": false,
     "endpoints": [{"url_redacted": "https://audit.internal", "breaker": "closed", "runtime": false}]}
  ],
  "tasks": [
    {"name": "source:shop", "state": "running"},
    {"name": "push:audit", "state": "running"}
  ]
}
```

- `phase`: `starting`, `running`, `draining` or `stopped`.
- `sources[]`, one per bot: `ingress` is `long_poll`, `webhook` or `null`; `state` is `starting`, `running`, `backoff`, `failed` or `stopped`. A long poll is `backoff` while it waits out a failed `getMe` or `getUpdates` (its task keeps running); a bot whose source could not be set up is `failed`. `error` (present only when there is one) says why the source backs off or failed, with secrets masked.
- `consumers[]`: `depth` (waiting to be leased), `leased`, `in_flight` (push deliveries in progress, retries waiting included), `paused`, and for push consumers each endpoint's origin (`scheme://host[:port]`; path, query and credentials are never shown), breaker (`closed`, `open` or `half_open`) and `runtime`: `true` for the endpoint a bot's `setWebhook` set through the gateway (listed after the configured ones), `false` for a configured one.
- `tasks[]`: every supervised task (long-poll sources, push dispatchers, background `getMe` calls, and `gateway-sweep`, which forgets idle rate-limit keys) with the supervisor's view of it: its state and, when it is restarted after a panic or failed, its last error, secrets masked. A long poll that backs off by itself stays `running` here; see `sources[]`.

`GET /v1/config` shows the config as written, with defaults filled in (except a webhook's omitted `path`, which defaults to `/webhook/<bot name>`): every secret is `"[redacted]"`, URL fields (`api_url`, `public_url`, endpoint `url`) show only `scheme://host[:port]` (followed by `/…` when something was left out), and other strings keep their `${…}` references unexpanded with bot tokens and the config's secrets masked as `#####`.

## `tgin admin`

`tgin admin` calls the admin API of a running tgin:

```console
$ tgin admin status
$ tgin admin reload
$ tgin admin consumers
$ tgin admin pause <consumer>     # also: resume, drain, purge
$ tgin admin health
```

| Option | Environment | Meaning |
|---|---|---|
| `--url URL` | `TGIN_ADMIN_URL` | The admin API, `http` or `https`; default `http://127.0.0.1:9090`. |
| `--token-file PATH` | `TGIN_ADMIN_TOKEN_FILE` | A file holding `admin.token`; trailing line breaks are ignored. |
| `--json` | | Print the API's JSON instead of tables. |

`--url` and `--token-file` go before the subcommand; `--json` may go anywhere after `admin`. The CLI reads the token only from a file — never from an argument or an environment variable — so it does not show up in process listings or shell history. It never uses an HTTP proxy, whatever the proxy environment variables say, so the token goes straight to tgin.

`status` prints the version, generation and phase, then a table of sources and one of consumers; `consumers` prints the consumer table; `reload` prints the new generation and what changed, including settings that need a restart; `purge` prints how many updates it dropped. A request waits at most 60 s (a reload may wait on Telegram).

`health` asks `GET /readyz`, needs no token (it does not read the token file), waits at most 2 s, prints `ready` or `not ready (…)`, and exits 0 when ready, else 1; when the admin API cannot be reached or does not answer in time, it prints the error on stderr and exits 1. The container image's `HEALTHCHECK` runs it.

Exit codes: 0 on success; 1 when the token file cannot be read, the API cannot be reached or does not answer in time, it answers with an error (printed with the server's `error` or its list of config errors), or `health` finds tgin not ready; 64 for a malformed command line.

## Metrics

`GET /metrics` on the admin plane serves the OpenMetrics text format (`application/openmetrics-text; version=1.0.0; charset=utf-8`). Every name carries the `tgin_` prefix; counters end in `_total`.

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `tgin_build_info` | info | `version`, `commit` | The running build; `commit` is `unknown` when the build did not record it. |
| `tgin_source_updates_total` | counter | `source` | Updates a bot received and handed to routing, including those no rule matched. |
| `tgin_source_errors_total` | counter | `source`, `reason` | Long poll: failed Bot API calls, by `reason` `network`, `http`, `decode`, `unauthorized` (401/403/404), `rate_limited` (429) or `conflict` (409). Webhook: requests refused by the allowlist or the secret (`unauthorized`), bodies that could not be read (`network`), and bodies that were not an update or were over the 1 MiB limit (`decode`). |
| `tgin_source_dropped_total` | counter | `source`, `reason="buffer_full"` | Exported for every bot; no source increments it in this version. |
| `tgin_source_state` | gauge | `source`, `state` | 1 for the bot source's current state (`starting`, `running`, `backoff`, `failed`, `stopped`), 0 for the others. |
| `tgin_routes_unmatched_total` | counter | `source` | Updates no rule matched; they are dropped. |
| `tgin_consumer_queue_depth` | gauge | `consumer` | Updates waiting to be leased. |
| `tgin_consumer_leased` | gauge | `consumer` | Updates leased: handed to a pull instance and not yet acknowledged, or being delivered by a push consumer. |
| `tgin_consumer_in_flight` | gauge | `consumer` | Push deliveries in progress, retries waiting included; 0 for pull consumers. |
| `tgin_consumer_dropped_total` | counter | `consumer`, `reason` | Updates a consumer dropped: `overflow` (`drop_oldest` eviction, or no room), `dead_letter`, `reload` (still held when a removed consumer's drain ended), `purge`, `oversized` (larger than `capacity_bytes`). |
| `tgin_consumer_backpressure_seconds_total` | counter | `consumer` | Seconds the consumer's queue refused updates. |
| `tgin_delivery_attempts_total` | counter | `consumer`, `endpoint`, `outcome` | Push attempts per endpoint (its index in `endpoints`, from 0, or `runtime` for the endpoint a `setWebhook` set; never the URL), by `outcome` `delivered`, `retry` or `dead_letter`. |
| `tgin_delivery_latency_seconds` | histogram | `consumer` | From admission (when tgin queued the update) to acknowledgement (pull) or to successful delivery (push); buckets from 1 ms, doubling, 16 of them. |
| `tgin_config_reloads_total` | counter | `outcome` | Reloads `applied` or `rejected`. |
| `tgin_config_generation` | gauge | | The config generation in effect. |
| `tgin_shutdown_abandoned_total` | counter | | Updates still pending when the shutdown ended. |
| `tgin_gateway_requests_total` | counter | `bot`, `method`, `outcome` | Gateway requests of configured bots, and webhook replies sent to Telegram; `method` is the canonical method name, `other` or `file`; `outcome` `ok`, `rejected`, `rate_limited`, `upstream_error`, `unreachable`, `client_closed` or `local` ([Gateway metrics](#gateway-metrics)). |
| `tgin_gateway_upstream_seconds` | histogram | `bot`, `method` | From sending a gateway request or webhook reply to Telegram's response headers; buckets as `tgin_delivery_latency_seconds`. |
| `tgin_gateway_ratelimited_total` | counter | `bot`, `scope`, `action` | Requests and webhook replies the rate limits delayed or refused: `scope` `global`, `chat` or `group`, `action` `delayed` or `rejected`. |
| `tgin_gateway_retries_total` | counter | `bot` | Requests sent again after Telegram's `429`. |
| `tgin_gateway_unauthorized_total` | counter | | Gateway requests whose token matches no configured bot. |
| `tgin_webhook_replies_total` | counter | `consumer`, `outcome` | Webhook replies by `outcome` `executed`, `failed`, `refused`, `too_large` or `rate_limited` ([Gateway metrics](#gateway-metrics)). |

Apart from `tgin_build_info`'s, every label value is a bot or consumer name, an endpoint index, a method name from tgin's table or a fixed word, so no series carries a URL or a token. A bot's series go when a reload removes it; a consumer's when its drain ends.

## Logging

Logs go to stdout, as `text` or `json` lines (`log.format`, fixed at start). `log.level` sets the most verbose level and follows reloads; a non-empty `RUST_LOG` overrides it (its directives keep winning after a reload). Text logs are coloured only when stdout is a terminal and `NO_COLOR` is unset or empty. Every line passes through the redactor (see [Security](#security)), panics included.

## Security

**Secrets.** Write secrets as `${env:…}` or `${file:…}` references rather than literally, so the config file can be shared. `${file:…}` suits Docker and Kubernetes secrets mounted as files. An empty secret is an error. Config errors never quote a secret: a malformed reference in a secret or URL field is reported without its text, and YAML errors carry the path, line and column without the source line.

**Redaction.** Every log line is masked before it is written: each secret of every config loaded since start (bot tokens, webhook secrets, `admin.token`, pull `auth_token`s, push `secret_token`s) and anything shaped like a bot token is replaced by `#####`. Secrets shorter than 8 characters are not masked, as they would match ordinary words; use long random secrets. `/v1/status` and `/v1/config` redact as described under [Admin API v1](#admin-api-v1), and metric labels hold no URLs. The gateway never logs a request path or an upstream URL, since both carry the bot token.

**Auth model.**

| Surface | Protection |
|---|---|
| Admin plane (`/v1/*`, `/metrics`) | `admin.token` as a bearer token; required unless every admin listener is on loopback. `/metrics` can be opened with `admin.public_metrics`. `/livez` and `/readyz` are always open. |
| Pull consumers | Optional `auth_token` per consumer, as a bearer token. Without it, anyone who can reach the data plane can take the consumer's updates. |
| Webhook ingress | `secret_token` (`auto` by default) checked before the body is read; optional `ip_allowlist`; `trusted_proxies` decides whose `X-Forwarded-For` counts. |
| Bot API gateway | The bot token in the path, compared in constant time with every configured bot's token; an unknown token gets `401` and nothing goes to Telegram. Its holder acts as the bot, polls the pull consumers the bot routes to, and, with `accept_set_webhook`, chooses where a push consumer posts ([trust model](#setwebhook-capture)). |
| Push endpoints | The endpoint's `secret_token` is sent as `X-Telegram-Bot-Api-Secret-Token`, for the downstream to check. |

Every token compare runs in constant time.

**TLS.** The data plane terminates TLS itself when `server.tls` is set (rustls, HTTP/1.1); the certificate files are read at start (a missing or bad file stops tgin with exit code 1) and again on every reload. The admin plane has no TLS: keep it on loopback or a private network, or put a TLS proxy in front, since the bearer token travels in clear text. Outgoing calls to the Bot API and to push endpoints verify server certificates against the platform's trust store, never follow redirects, and use the `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` environment if set (`tgin admin` never does).

**Telegram webhooks need HTTPS** on 443, 80, 88 or 8443 (Telegram's rule): put tgin behind a TLS-terminating proxy, or give it `server.tls` and a `listen` on one of those ports.

## Shutdown

`SIGTERM` or `SIGINT` (Ctrl+C, Ctrl+Break, closing the console or a system shutdown on Windows) starts the shutdown, as does every source failing for good. tgin then:

1. **Stops being ready.** `/readyz` answers `503`, reloads are refused.
2. **Lame duck** (`runtime.lame_duck`, 5 s; skipped without a `server` block). Everything keeps working, so load balancers can take tgin out of rotation.
3. **Stops ingest.** Long polls stop (updates not yet admitted are left to Telegram); webhooks answer `503` with `Retry-After: 5`.
4. **Drains** for up to `runtime.shutdown_grace` (15 s), until no update is pending and, for 1 s since, no call through the gateway has gone to Telegram or is still under way (waiting for Telegram's answer, or streaming it, a download included): push consumers keep delivering, pull consumers keep serving (a poll on an empty queue answering within 500 ms), and the gateway keeps forwarding the downstreams' calls, so a bot that answers an update after acknowledging it still gets its answer through. Calls the gateway answers itself, polls included, do not hold the drain open; without a data plane the drain ends once nothing is pending.
5. **Closes** the consumer tasks and the data plane, giving requests 1 s; tasks still running then are aborted. Updates still pending are lost: they are logged (`exiting with updates not delivered`) and counted in `tgin_shutdown_abandoned_total`.
6. **Closes the admin plane** last.

With the defaults the whole shutdown takes at most about 22 s; set the orchestrator's stop timeout to 30 s (Kubernetes `terminationGracePeriodSeconds: 30`, `docker run --stop-timeout 30`, Compose `stop_grace_period: 30s`; Docker's default of 10 s cuts the drain short).

A second `SIGTERM`/`SIGINT` (or Ctrl+C, Ctrl+Break) during the shutdown stops at once with exit code 130, losing what is pending. When the shutdown began because every source failed, the first signal is only logged and a second one forces the exit. A signal while tgin is still starting stops what has started and exits 0. Closing the console on Windows, which kills the process about 5 s later, skips the lame duck and caps the drain at 4 s.

**Every source failing.** A long-poll source fails for good when Telegram rejects its token (`401`, `403` or `404`). When every bot with an ingress uses long poll and all of their sources have failed, tgin shuts down as above and exits with code 2. A webhook bot never fails this way, so a config with one never exits 2.

## Deployment

**Rules that follow from the design.**

- **One tgin per long-poll token.** Telegram allows one `getUpdates` poller per token; a second gets `409 Conflict`, and tgin backs off 5 s and logs `another process is polling this bot token`. Replace a tgin that polls with stop-then-start (Kubernetes `strategy: Recreate`, `replicas: 1`), not a rolling update that briefly runs two.
- **Queues are per process**, and so are the gateway's rate limits. A pull client must poll the tgin that holds its consumer's updates.
- **Downstream bots go through the gateway.** Bots behind tgin share its token. Through the gateway, their `getUpdates`, `setWebhook`, `deleteWebhook`, `logOut` and `close` are answered by tgin. A bot that calls Telegram directly with that token can still take tgin's updates (`getUpdates`), remove tgin's webhook or drop the updates Telegram holds for it (`deleteWebhook`, as python-telegram-bot, grammY and Telegraf do when they start polling, or aiogram's `delete_webhook(drop_pending_updates=True)`).

### Binaries

Releases (none of 1.0 yet; this branch is `1.0.0-dev`) carry archives for Linux (glibc, and static musl), macOS and Windows, each on x86_64 and aarch64, with shell and PowerShell installers. To build one yourself (Rust 1.89 or newer and a C compiler, for `aws-lc-sys`):

```bash
cargo build --release -p tgin      # → target/release/tgin
```

Run `tgin` (or `tgin run`) next to `tgin.yaml`, or `tgin -c /etc/tgin/tgin.yaml`. On Linux and macOS, `SIGHUP` reloads the config; Windows has no `SIGHUP`, so reload there with the file watcher or `tgin admin reload`. Under systemd, set `TimeoutStopSec=30`.

### Docker

The image (from the repository's `Dockerfile`) is one static binary on `gcr.io/distroless/static-debian13`, running as uid/gid `65532:65532`, with no shell:

- the entrypoint is `tgin`, which reads `TGIN_CONFIG=/etc/tgin/tgin.yaml`: mount the config there (readable by uid 65532), or pass `-c <path>`;
- it declares ports `8080` (data plane) and `9090` (admin plane);
- its `HEALTHCHECK` runs `tgin admin health` against `http://127.0.0.1:9090` every 10 s; set `TGIN_ADMIN_URL` (for example `-e TGIN_ADMIN_URL=http://127.0.0.1:9191`) when `admin.listen` moves the admin plane.

```bash
docker build -t tgin .
docker run -d --name tgin \
  -p 8080:8080 \
  -v "$PWD/config:/etc/tgin:ro" \
  -e TOKEN=123456:ABC \
  --stop-timeout 30 \
  tgin
```

**Admin token as a file.** `tgin admin` reads the token only from a file, and the image has no shell to write one, so mount it as a secret file and point both tgin and the CLI at it:

```yaml
# tgin.yaml
admin:
  token: ${file:/run/secrets/tgin_admin_token}
```

```yaml
# compose.yaml
services:
  tgin:
    image: tgin
    stop_grace_period: 30s
    volumes:
      - ./config:/etc/tgin:ro   # the directory: a file mount misses edits saved by rename
    environment:
      TGIN_ADMIN_TOKEN_FILE: /run/secrets/tgin_admin_token
    secrets:
      - tgin_admin_token
secrets:
  tgin_admin_token:
    file: ./tgin_admin_token.txt
```

Then `docker compose exec tgin tgin admin status` (or `docker exec <container> tgin admin status`) works. The secret file must be readable by uid 65532. [examples/simple](examples/simple) is a complete Compose stack.

### Kubernetes

Kubernetes ignores the image's `HEALTHCHECK`, and its HTTP probes come from outside the pod, while the admin plane listens on `127.0.0.1` by default. Either:

- listen on all addresses with a token (required for a non-loopback listener) and probe over HTTP — `/livez` and `/readyz` need no token:

  ```yaml
  admin:
    listen: ["0.0.0.0:9090"]
    token: ${file:/run/secrets/tgin/admin-token}
  ```

  ```yaml
  livenessProbe:
    httpGet: { path: /livez, port: 9090 }
  readinessProbe:
    httpGet: { path: /readyz, port: 9090 }
  ```

- or keep the admin plane on loopback and use exec probes:

  ```yaml
  readinessProbe:
    exec: { command: ["tgin", "admin", "health"] }
  ```

A Deployment for long-poll bots:

```yaml
spec:
  replicas: 1
  strategy:
    type: Recreate
  template:
    spec:
      terminationGracePeriodSeconds: 30
      containers:
        - name: tgin
          image: tgin
          ports:
            - { name: data, containerPort: 8080 }
            - { name: admin, containerPort: 9090 }
          env:
            - { name: TGIN_ADMIN_TOKEN_FILE, value: /run/secrets/tgin/admin-token }
          volumeMounts:
            - { name: config, mountPath: /etc/tgin }   # the whole directory, not subPath
            - { name: secrets, mountPath: /run/secrets/tgin }
      volumes:
        - name: config
          configMap: { name: tgin }                     # key: tgin.yaml
        - name: secrets
          secret: { secretName: tgin }                  # key: admin-token
```

Mount the ConfigMap as a directory: with `subPath` the file never changes, so hot reload never sees an update.

## Command line and exit codes

| Command | Does |
|---|---|
| `tgin` / `tgin run [-c PATH]` | Run with the config file (`-c/--config`, `TGIN_CONFIG`, default `tgin.yaml`). |
| `tgin config validate <path>` | Check a config file; list every error and warning. |
| `tgin config print <path>` | Print the config as JSON, defaults filled in (except a webhook's omitted `path`), secrets redacted. |
| `tgin config schema` | Print the JSON Schema. |
| `tgin admin …` | Call a running tgin's admin API ([`tgin admin`](#tgin-admin)). |
| `tgin version` / `tgin --version` | Print the version (`tgin version` adds the commit when the build recorded one). |

| Code | Meaning |
|---|---|
| 0 | Clean shutdown after a signal (also a signal during start-up), or a command that succeeded. |
| 1 | The config cannot be read or is invalid, tgin could not start (a listener cannot bind, a certificate cannot be loaded), or a failed `config`/`admin` command (including `tgin admin health` when not ready). |
| 2 | Every source failed for good: Telegram rejected each long-poll bot's token and no webhook bot exists. |
| 64 | Malformed command line (unknown command or option, missing argument). |
| 130 | A second shutdown signal cut the drain short. |

Updates lost at the end of a shutdown do not change the exit code; see `tgin_shutdown_abandoned_total` and the log.

## Further reading

- [docs/config-reference.md](docs/config-reference.md) — every config key, type and default.
- [PERF.md](PERF.md) — benchmarks against the original engine, and their method.
- [CHANGELOG.md](CHANGELOG.md) — changes per release.
- [SECURITY.md](SECURITY.md) — reporting vulnerabilities, verifying releases.
- [examples/simple](examples/simple) — a Compose stack with pull and push consumers whose bots use the gateway.
- [tests/e2e](tests/e2e) — the end-to-end suite: aiogram, python-telegram-bot, grammY and Telegraf against tgin and a fake Bot API (`cargo xtask e2e`).
