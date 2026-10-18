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
- [Self-hosted Bot API servers](#self-hosted-bot-api-servers)
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

- **Bots** (`bots`). Each bot has a unique name, a token, a Bot API server (its **upstream**: Telegram's, unless `api_url` or `upstream` names another) and at most one **ingress**: `long_poll` (tgin calls `getUpdates` on the Bot API server) or `webhook` (the server posts updates to tgin). A bot without an ingress receives nothing.
- **Upstreams** (`upstreams`). Named Bot API servers: Telegram's (`kind: cloud`) or a self-hosted `telegram-bot-api` (`kind: local`), whose files tgin can serve ([Self-hosted Bot API servers](#self-hosted-bot-api-servers)).
- **Consumers** (`consumers`). A named destination for updates, in one of two modes:
  - **pull**: downstream bots fetch updates from tgin with a Telegram-shaped `getUpdates` call on `/c/<consumer>/getUpdates` (and on any alias path), or through the [gateway](#bot-api-gateway) on `/c/<consumer>/bot<token>/getUpdates`. Several processes may poll one pull consumer; they compete for its updates. A pull consumer serves exactly one bot: two bots routing to the same pull consumer is a config error.
  - **push**: tgin posts each update to one or more HTTP endpoints, with retries and a circuit breaker per endpoint. Several bots may route to one push consumer; the `X-Tgin-Source` header names the bot.
  Each consumer has its own bounded queue (`capacity` updates, `capacity_bytes` bytes).
- **Rules** (`bots[].routes`). Each bot's rules are tried in order; the first rule whose `match` holds decides where the update goes, and a rule without `match` matches everything. An update no rule matches is dropped and counted in `tgin_routes_unmatched_total`; `tgin config validate` warns when a bot's last rule has a `match`.
- **Targets** (`to`). Every entry of a rule's `to` receives the update. An entry is a consumer name, `{split: {strategy, among, weights?}}` (exactly one child receives it: `round_robin`, `hash_chat` — sticky by chat, else by user, else round robin — or smooth `weighted` round robin) or `{all: […]}` (every child receives it). Targets nest. A consumer reached twice through one rule's targets receives the update once.
- **Gateway.** The data plane also serves the Bot API to the bots behind tgin, on `/bot<token>/…` and `/c/<consumer>/bot<token>/…`: a downstream points its framework's Bot API base URL at tgin, which forwards its calls to the bot's Bot API server within Telegram's flood limits and answers itself the calls that would disturb tgin or the bot's other downstreams ([Bot API gateway](#bot-api-gateway)).

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

File-path fields (`server.tls.cert_file`, `server.tls.key_file`, a webhook's `certificate_file`, an upstream's `files.dir`) are relative to the config file's directory too, never to the working directory.

### Sections

| Section | What it sets | Defaults |
|---|---|---|
| `runtime` | `worker_threads` (`auto` = one per CPU), `lame_duck`, `shutdown_grace` | `auto`, `5s`, `15s` |
| `log` | `level` (`trace` … `error`), `format` (`text` or `json`) | `info`, `text` |
| `server` | The data plane: `listen`, `tls`, `trusted_proxies`, `max_connections`, `header_read_timeout`. Omitted, tgin serves no webhooks and no pull consumers (both then fail validation). | `["0.0.0.0:8080"]`, none, none, `10000`, `10s` |
| `admin` | The admin plane: `listen`, `token`, `public_metrics` | `["127.0.0.1:9090"]`, none, `false` |
| `reload` | `watch` the config file, its `debounce` (at least `10ms`), and the `drain_timeout` of removed consumers | `true`, `500ms`, `30s` |
| `upstreams` | Bot API servers by name: `kind`, `url`, `local_mode`, `files`, `response_timeout` ([Self-hosted Bot API servers](#self-hosted-bot-api-servers)) | none; `local_mode: false`, `response_timeout` `1m` (`cloud`) or `10m` (`local`) |
| `bots` | Bots with `name`, `token`, `api_url` or `upstream`, `allow_local_files`, `gateway`, `ingress`, `routes` | `api_url: https://api.telegram.org`, `allow_local_files: false` |
| `consumers` | Pull and push consumers by name | see below |

Names of bots, consumers and upstreams are 1 to 63 lowercase letters, digits and `-`, starting with a letter or digit.

**Long poll** (`ingress.long_poll`): `timeout` (`30s`, at least `1s`, at most 50 s is sent), `limit` (`100`, 1 to 100), `allowed_updates`, `delete_webhook_on_start` (`true`). tgin sends `allowed_updates` with every `getUpdates`: the configured list, or `[]` when it is omitted, which is the Bot API's default (every update type except `chat_member`, `message_reaction` and `message_reaction_count`). A list the Bot API server stored for the token from an earlier client never decides what arrives; list those three types to receive them.

**Webhook** (`ingress.webhook`): `path` (default `/webhook/<bot name>`), `public_url` (the base URL the Bot API server reaches tgin at; `setWebhook` registers `public_url` followed by `path`; `https` on port 443, 80, 88 or 8443, except on a `local` upstream in local mode, which takes `http` and any port), `register` (`false`; `true` calls `setWebhook` when the bot's source starts and needs `public_url`), `secret_token` (`auto`; a configured secret is 1 to 256 ASCII letters, digits, `_` or `-`, as Telegram requires), `ip_allowlist` (`none`; `telegram` is an error on a `local` upstream), and Telegram's `allowed_updates` (sent with every `setWebhook`, `[]` when omitted, as for long polls), `max_connections` (1 to 100, or to 100 000 on a `local` upstream in local mode, where a value above 100 must not exceed `server.max_connections`), `drop_pending_updates`, `ip_address` and `certificate_file` for `setWebhook`.

**Gateway** (`gateway`, per bot): `upstream_idle_timeout` (`1m`, at least `1s`), `rate_limit` (`mode` `shape`, `max_delay` `5s` (at most `1m`), `methods` `["send*", "copyMessage*", "forwardMessage*"]`, `global` 30 per `1s` with `burst` 30, `per_chat` 1 per `1s` with `burst` 3, `per_group` 20 per `1m` with `burst` 20) and `retry_429` (`max_wait` `10s`, at most `1m`). See [Bot API gateway](#bot-api-gateway).

**Tokens and paths.** A bot token may use only ASCII letters, digits, `:`, `_` and `-`; a token with anything else (a space, quotes, a second trailing newline in a token file) is a config error, at start and on reload alike. A token two bots share is a config error too, reported at the second bot without quoting the token: Telegram splits a token's updates between everyone who receives them. Webhook paths and pull aliases are matched byte for byte, so they must reach tgin unchanged inside a URL: no `?`, `#`, spaces, control or non-ASCII characters (write them percent-encoded), no `.` or `..` segments and no `\`.

**Pull consumer** (`pull`): `capacity` (`10000`), `capacity_bytes` (`64MiB`, at least `1MiB`), `on_full` (`backpressure`), `lease_timeout` (`2m`, at least `1s`: leases are expired by a check that runs once a second), `ordering` (`none`), `auth_token`, `aliases`, `honor_drop_pending` (`false`).

**Push consumer** (`push`): `endpoints` (each `url`, `secret_token`, `timeout` `10s`; at least one, unless `accept_set_webhook: true`), `balance` (`round_robin`, `hash_chat` or `least_in_flight`), `max_in_flight` (`32`), `ordering` (`none`), `capacity` (`10000`), `capacity_bytes` (`64MiB`), `on_full` (`backpressure`), `retry` (`initial` `1s`, `max` `1m`, `max_attempts` `10`, `max_age` `1h`), `breaker` (`failure_threshold` `5`, `open_for` `30s`), `webhook_reply` (`execute` or `ignore`; see [Webhook replies](#webhook-replies)), `accept_set_webhook` (`false`; see [`setWebhook` capture](#setwebhook-capture)), `honor_drop_pending` (`false`). An endpoint `timeout`, `retry.initial`, `retry.max`, `retry.max_age`, `breaker.open_for` and `server.header_read_timeout` of 0 would disable or break what they configure, and are config errors; `runtime.lame_duck`, `runtime.shutdown_grace` and `reload.drain_timeout` may be 0 (no lame duck, no drain).

`max_in_flight` counts deliveries in progress over all of a consumer's endpoints, retries waiting included. With downstreams that handle requests concurrently, the default, 32, delivered every update at every webhook point of the benchmarks. A downstream that handles one request at a time (a single bot process) can deliver more with a lower value: in benchmarks from an earlier session, before the load generator's 100-connection cap, one aiogram process offered 5 000 updates/s took 1 809 updates/s with 32 and 2 268–2 280 with 8 ([PERF.md → `max_in_flight`](PERF.md#max_in_flight)).

**Gateway flags.** `honor_drop_pending: true` lets `drop_pending_updates` on the consumer's gateway path drop the calling bot's queued updates from it; `accept_set_webhook: true` (push only, and only on a consumer fed by one bot) lets that bot's `setWebhook` give the consumer an endpoint. Both are described under [Bot API gateway](#bot-api-gateway).

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

1. The queue of every `on_full: backpressure` target must have room; if one is full, nothing is queued anywhere and the source is told to retry: a long poll holds the rest of its batch and offers the update again every 250 ms, without polling the Bot API server meanwhile; a webhook answers `503` with `Retry-After: 5`, and the Bot API server sends the update again later.
2. A full `on_full: drop_oldest` target evicts its oldest updates that are not leased; if everything in it is leased, it skips the update. Either way the dropped updates count as `tgin_consumer_dropped_total{reason="overflow"}`.
3. An update larger than a target's `capacity_bytes` is dropped for that target, with `reason="oversized"`, in either mode; the other targets still get it.

A queue's `capacity` and `capacity_bytes` count leased updates too. While a backpressure queue refuses updates, `tgin_consumer_backpressure_seconds_total` grows.

**Memory.** `capacity_bytes` counts each update's own bytes. A long-poll update shares memory with the rest of its `getUpdates` response (each response is copied once, to its exact size, and its updates are slices of it), so a queued update keeps its whole response alive. A consumer that receives a small share of a busy bot's updates (an `/admin` command route, an audit filter) and whose clients or endpoint are down can therefore hold up to `limit` × its `capacity_bytes`; a lower `long_poll.limit` bounds this. A webhook update is copied to its own allocation.

**At least once, while the process lives.** Queues live in memory: a crash or kill loses what they hold, and so does a shutdown that ends before they drain. A long poll moves its `getUpdates` offset past an update only once the update is admitted (or matched no rule), and the Bot API server learns the new offset on the next `getUpdates`. Updates not admitted when tgin stops are not confirmed, so the server sends them again to the next poller; after a restart, tgin starts from the server's unconfirmed updates, so the last batch admitted before the stop may be delivered a second time. A webhook update is answered `200` only once it is queued; a `503` while shutting down may still let a queued copy through, so the server's retry can produce a duplicate. Consumers should tolerate duplicates, for example by `update_id`.

**Pull leases.** A poll leases updates to the polling instance. The instance acknowledges them with the next poll's `offset`, as with Telegram: every update it holds with an `update_id` below a positive `offset` is acknowledged, a negative `offset` acknowledges all it holds, and `0` acknowledges nothing. A poll first returns what the instance still holds from `offset` on (a response lost on the way is delivered again) and renews those leases; otherwise it leases new updates, waiting up to its `timeout`. A lease that is neither acknowledged nor renewed within `lease_timeout` (default 2 minutes) returns to its place in the queue for any instance. A client that always sends `offset=0` never acknowledges anything and receives the same updates again on every poll.

**Ordering.** With `ordering: per_chat`, at most one update per chat (by `chat.id`, else by the user's id) is leased at a time; the next one of that chat waits until the current one is acknowledged (pull) or delivered or dead-lettered (push). Updates with neither id are never held back. With `ordering: none` (the default) several updates of a chat can be in flight at once.

**Push retries.** A push consumer leases up to `max_in_flight` updates and posts each one to an endpoint chosen by `balance` among those whose breaker admits a request:

- a `2xx` answer delivers it;
- `408`, `429`, any `5xx`, a timeout or a network error is transient: the update is retried after a backoff, a random delay up to `retry.initial` doubled per attempt and capped at `retry.max`, or the endpoint's `Retry-After` if that is longer (capped at `retry.max`);
- any other `4xx`, any `3xx` (tgin never follows redirects), and a `508 Loop Detected` that carries the pushing process's own `Via` (the endpoint is this tgin itself, which refused its own push) dead-letter it at once. Any other `508` is a transient `5xx`.

An update is dead-lettered after `retry.max_attempts` attempts, or when its next attempt would come after `retry.max_age` since tgin queued it (time a long-poll update was held back by backpressure, or a webhook update spent in the Bot API server's retries, is not counted). A dead-lettered update is dropped: logged as `update dead-lettered` (consumer, bot, `update_id`, update type, endpoint index and last error; never the payload or URL) and counted as `tgin_consumer_dropped_total{reason="dead_letter"}`. There is no dead-letter queue.

**Circuit breakers.** Each endpoint has a breaker: `breaker.failure_threshold` consecutive transient failures open it for `breaker.open_for`, then it admits a single probe request, whose success closes it. A permanent failure (the endpoint answered) counts as a success for the breaker. While every breaker is open, deliveries wait; an update that waits past `retry.max_age` is dead-lettered.

**Backpressure or drop.** `on_full: backpressure` (the default) never drops an update for a full queue, but a consumer that stops taking updates fills its queue and then stalls its bot's source, and so every other consumer of that bot. `on_full: drop_oldest` keeps the source moving and drops that consumer's oldest updates instead. Choose `drop_oldest` for consumers whose outage must not hold back the others.

## Pull consumers: the `getUpdates` endpoint

A pull consumer is served on the data plane at `GET` or `POST /c/<consumer>/getUpdates` and at each path in its `aliases`. It speaks the Bot API's `getUpdates`:

- Parameters come from the query string and, when there is a body, from a JSON (`Content-Type: application/json`) or form body, which fills in those the query string lacks: a parameter in both places takes the query string's value, as telegram-bot-api reads it. `offset` (default 0), `limit` (default 100, clamped to 1 … 100) and `timeout` in seconds (default 0, clamped to 0 … 50) are read; `allowed_updates` and anything else is ignored (routing is the config's). `multipart/form-data` is refused.
- The answer is `{"ok":true,"result":[…]}` with the updates exactly as the Bot API server sent them. Errors look like the Bot API's: `404 {"ok":false,"error_code":404,"description":"Not Found"}` for an unknown consumer, `401 … "Unauthorized"` without the right token, `400 … "Bad Request: …"` for bad parameters.
- With `auth_token` set, a poll must send `Authorization: Bearer <auth_token>`.
- The polling **instance** is named by the `X-Tgin-Instance` header (1 to 64 visible ASCII characters) or, without it, by the client's IP address (from `X-Forwarded-For` only when the peer is in `server.trusted_proxies`). Leases and acknowledgements are per instance, so processes that share an address — behind NAT, or several on one host — must each send their own `X-Tgin-Instance`.

Most bot frameworks point at tgin by changing their Bot API base URL to the gateway, `http://<tgin>:8080/c/<consumer>`: their `getUpdates` then polls this consumer and every other call goes to the bot's Bot API server through tgin ([Framework setup](#framework-setup)).

## Push consumers: the requests tgin sends

Each attempt is a `POST` of the update's JSON to the endpoint `url` with these headers:

| Header | Value |
|---|---|
| `Content-Type` | `application/json` |
| `X-Tgin-Attempt` | The attempt number, from 1. |
| `X-Tgin-Source` | The name of the bot the update came from. |
| `X-Tgin-Update-Type` | The update type, such as `message`. |
| `X-Telegram-Bot-Api-Secret-Token` | The endpoint's `secret_token`, if set (1 to 256 of `A-Za-z0-9_-`, checked at load). |
| `Via` | `1.1 tgin-<16 hex digits>`: the pseudonym of the tgin instance (one process in 1.0), chosen at random when it starts. |

Each request is bounded by the endpoint's `timeout` (default 10 s). The endpoint should answer `2xx` once it has taken the update; see [Delivery semantics](#delivery-semantics) for the other answers. A `2xx` answer may carry a Bot API call, which tgin makes for the endpoint ([Webhook replies](#webhook-replies)).

Every push carries tgin's `Via`, so a push that comes back to the same tgin is refused instead of acted on ([Loop detection](#loop-detection)).

## Webhook ingress

A webhook bot's route is `POST <path>` on the data plane (default path `/webhook/<bot name>`). A request is answered, in this order of checks:

1. `405` for a method other than `POST`;
2. `403` if the client address is not in `ip_allowlist` (`telegram` means 149.154.160.0/20 and 91.108.4.0/22; the client address is the TCP peer's, or the right-most `X-Forwarded-For` address not in `server.trusted_proxies` when the peer is a trusted proxy);
3. `401` without the right `X-Telegram-Bot-Api-Secret-Token` (compared in constant time);
4. `503` with `Retry-After: 5` once ingest has stopped (shutdown), and while the bot is [parked](#moving-a-bot-between-servers);
5. `413` for a body over 1 MiB, `400` for a body the client fails to send;
6. `400` for a body that is not a JSON object with an integer `update_id`;
7. `503` with `Retry-After: 5` when a consumer pushes back, or when ingest stopped while the update was being queued;
8. otherwise `200` with an empty body, also when no rule matches.

Checks 1 to 4 read only the request head; the body is read only for an authorized request. A request still running after 30 s is answered `408`.

`secret_token: auto` (the default) derives the secret from the bot token (HMAC-SHA256 of the token over a fixed context, 43 characters), so it is the same across restarts and replicas. tgin sends it to the Bot API server only with `register: true`; with `register: false`, set `secret_token` to the secret you register yourself, or `none`. `secret_token: none` checks nothing: anyone who can reach the path can post updates.

`register: true` calls `setWebhook` whenever the bot's source starts (at start and when a reload starts or restarts it). A failed call does not stop the bot, as the server may still have the webhook from an earlier run: tgin retries it in the background as the task `register:<bot>`, backing off from 1 s to 5 min (or for the server's `retry_after`, when longer), until it succeeds or the bot stops. A `401`, `403` or `404` from a `cloud` upstream rejects the token for good: tgin logs one error and calls nothing more until a reload changes the bot (its token, upstream or ingress) or tgin restarts. On a `local` upstream only a `403` is final; a `401` or `404` may come from the server itself (a server whose api_id or api_hash Telegram rejects answers `401`), so it is retried.

A long-poll bot and the Bot API server's webhook exclude each other: with `delete_webhook_on_start: true` (the default) a long poll calls `deleteWebhook`, keeping pending updates, before its first poll and again when a poll finds a webhook set (but not again within 60 s of deleting it, which means another process keeps setting it).

## Bot API gateway

The data plane also serves the Bot API to the bots behind tgin. A downstream sets its framework's Bot API base URL to tgin instead of `https://api.telegram.org` ([Framework setup](#framework-setup)) and keeps using the bot's token. tgin forwards each call to the bot's Bot API server (its [upstream](#self-hosted-bot-api-servers): Telegram's, unless the config names another), answers itself the calls that would disturb tgin or the bot's other downstreams, holds the bot's sends to Telegram's flood limits, and absorbs short `429`s. The gateway is part of the data plane, so it needs a `server` block.

### Routes and auth

| Path | Serves |
|---|---|
| `/bot<token>/<method>` | A Bot API method, for any downstream of the bot. |
| `/c/<consumer>/bot<token>/<method>` | The same, on behalf of `<consumer>`, which the bot must route to: `getUpdates` there polls that pull consumer, and `setWebhook` may give that push consumer its endpoint. |
| `/file/bot<token>/<file_path>`, `/c/<consumer>/file/bot<token>/<file_path>` | A file download (the `file_path` of `getFile`), forwarded or served as the bot's upstream says ([File downloads](#file-downloads)). |

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

None of these sends anything to the Bot API server. Everything tgin answers itself on a gateway path has Telegram's shape: `{"ok":false,"error_code":N,"description":"…"}` with the HTTP status equal to `error_code`, or `{"ok":true,"result":…}`. No gateway path, token or upstream URL reaches a log line, a metric or an error body; logs name the bot and the method. A bot whose token is not shaped `<digits>:<secret>` loads with a warning, and the gateway does not serve it.

Whoever holds a bot's token can use the gateway as that bot, as with Telegram, and can poll every pull consumer the bot routes to on `/c/<consumer>/bot<token>/getUpdates`, where the bot token is the credential and the consumer's `auth_token` is not asked for.

### Forwarding

A call that no intercept answers goes to `{url}/bot<token>/<method>`, `url` being the bot's upstream URL (its `api_url` on the shorthand), and a download from a `cloud` upstream to `{url}/file/bot<token>/<file_path>`, keeping the path of `url` (a `local` upstream's files: [File downloads](#file-downloads)). Both bodies stream in both directions and keep their `Content-Length` when they have one, so uploads and downloads of any size pass through in constant memory. tgin reads a body whole only where it must, each with a bound: the JSON or urlencoded body of a method in [`rate_limit.methods`](#rate-limits) (its chat, and a possible resend), up to 64 KiB; for a bot whose `file:` references are refused, any JSON or urlencoded body, up to 64 KiB, a longer one being scanned as it streams ([Local mode and `file:` references](#local-mode-and-file-references)); the parameters of an [intercepted](#intercepts) call, up to 64 KiB; the server's `429` answers, the `getMe` answer that fills tgin's cache and a local-mode `getFile` answer that tgin rewrites, up to 64 KiB each; the server's error answer to a webhook reply, up to 64 KiB; and [webhook replies](#webhook-replies), up to 1 MiB.

Only these headers pass, except one that the message's `Connection` header names:

- to the Bot API server: `Content-Type`, `Content-Length`, `Accept`, `Accept-Encoding` and `User-Agent`, and on file paths also `Range`, `If-None-Match` and `If-Modified-Since`;
- back to the client: `Content-Type`, `Content-Length`, `Content-Encoding`, `Content-Disposition`, `Retry-After`, `Cache-Control`, `ETag`, `Last-Modified`, `Accept-Ranges` and `Content-Range`.

Connecting to the Bot API server may take 5 s; after that no total timeout applies, and a transfer that keeps moving is never cut. While the request body is being sent, the bot's `gateway.upstream_idle_timeout` (`1m`) bounds the time in which nothing moves, whether the client stopped sending or the server stopped reading. Once the body has been sent, the server's response headers must arrive within the bot's response wait: its upstream's `response_timeout` (`1m` for `cloud`, `10m` for `local`), or, on the `api_url` shorthand, its `upstream_idle_timeout`. Each part of the response body must arrive within `upstream_idle_timeout` of the previous one. A request that gets no response is answered `502`, with a description that says whether the server may have run the call:

| Description | When | Did the server run the call? |
|---|---|---|
| `Bad Gateway: the Bot API server is unreachable` | The connect failed or never finished (also for a call without a body); the upload stalled for `upstream_idle_timeout` or broke off; or the body of the server's `429` to a call whose body tgin holds broke off. | No: the server never had the whole request, or flood control refused it. The client may send it again. |
| `Bad Gateway: the Bot API server did not answer; the call may have been executed` | The whole request was sent, then the connection was lost or the response wait ran out; or the body of a `getMe` answer, or of a local-mode `getFile` answer, broke off. | Maybe. tgin never sends it again; a client that does may make the call twice. |

A response body that fails or stalls after its headers aborts the client's connection, except for the answers tgin reads whole before it replies (the server's `429` to a call whose body tgin holds, the `getMe` answer that fills its cache, and a local-mode `getFile` answer), where the client gets the `502` above instead (counted `upstream_error`). Downloads from a `files.url` origin get their own wording ([File downloads](#file-downloads)). The gateway has no circuit breaker: each request fails on its own. Its requests use the `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` environment if set and never follow redirects.

tgin knows the methods of Bot API 10.3. A method it does not know is forwarded like any other and counted with the metric label `other`; only known methods are intercepted.

### Intercepts

| Method | On `/bot<token>/…` | On `/c/<consumer>/bot<token>/…` |
|---|---|---|
| `getUpdates` | `409 Conflict: tgin serves getUpdates per consumer at /c/<consumer>/bot<token>/getUpdates`: tgin polls the Bot API server itself, and a forwarded call would take its updates. | The pull consumer's queue, exactly as `/c/<consumer>/getUpdates` serves it ([Pull consumers](#pull-consumers-the-getupdates-endpoint)), without its `auth_token`. A consumer that is not a pull consumer of this bot: `409 Conflict: <consumer> is not a pull consumer of this bot`. |
| `getMe` | The bot's identity, as the Bot API server sent it to tgin's own `getMe` (at start for bots with an ingress); forwarded until tgin has it, and the first successful answer is kept. | Same. |
| `setWebhook` | `Webhook was set`, or `Webhook was deleted` for an empty `url`; nothing changes. | With `accept_set_webhook: true` on a push consumer, sets its runtime endpoint ([`setWebhook` capture](#setwebhook-capture)); otherwise as on the bot path. |
| `deleteWebhook` | `Webhook was deleted`; nothing changes. | With `accept_set_webhook: true` on a push consumer, removes its runtime endpoint; otherwise as on the bot path. |
| `getWebhookInfo` | `{"url":"","has_custom_certificate":false,"pending_update_count":0}` | `url` is the consumer's runtime endpoint, or `""`; `pending_update_count` counts the consumer's updates queued or leased. |
| `getFile` | Forwarded. On a `local` upstream in local mode with `files`, the server's absolute `file_path` becomes the path under the bot's directory ([File downloads](#file-downloads)). | Same. |
| `logOut`, `close` | `400 Bad Request: logOut and close are disabled by tgin; operators move bots with tgin admin bot logout\|close`, and a warning in the log ([Moving a bot between servers](#moving-a-bot-between-servers)). | Same. |

`drop_pending_updates` on a `deleteWebhook`, or on a `setWebhook` that a consumer accepts, drops the calling bot's queued updates from the calling consumer (as `purge` drops them) when that consumer has `honor_drop_pending: true`, and is ignored otherwise; on the bot path it never drops anything. As Telegram drops only the caller's pending updates, other bots' updates stay: those of another bot feeding the same push consumer, and those a bot left before a reload moved its routes elsewhere.

The answers to the webhook methods follow telegram-bot-api's `Client.cpp`: its descriptions (`Webhook is already set`, `Webhook is already deleted`, `Bad Request: invalid webhook URL specified`, `Bad Request: secret token is too long`, `Bad Request: secret token contains illegal characters`), its order of parameters (the query string first, then a JSON, urlencoded or multipart body's text fields) and its booleans (`true`, `yes` or `1`, in any case). Intercepted calls (except a `getMe` before tgin knows the bot's identity, and `getFile`, which goes on to the server) never reach the Bot API server and count as outcome `local`, so a framework that calls `deleteWebhook` when it starts polling (python-telegram-bot, grammY and Telegraf do), `setWebhook`, `logOut` or `close` cannot remove tgin's webhook, drop the updates the server holds for it or log the bot out. The body of a `setWebhook`, a `deleteWebhook` or a consumer-scoped `getUpdates` must arrive within the bot's `upstream_idle_timeout` (else `408 Request Timeout`) and whole (else `400 Bad Request: the request body ended early`); a body announced over 64 KiB is refused at once (`413 Request Entity Too Large`, or `400` for `getUpdates`, as on `/c/<consumer>/getUpdates`).

### `setWebhook` capture

A push consumer with `accept_set_webhook: true` takes its endpoint from a `setWebhook` on its gateway path, `/c/<consumer>/bot<token>/setWebhook`, so a webhook bot can register itself with tgin as it would with Telegram. Validation allows the flag only on a consumer fed by one bot, and only that bot's token can set the endpoint; such a consumer may have no configured `endpoints` at all.

- The `url` is `http` or `https` (`https` when it has no scheme, as on Telegram); a `secret_token`, 1 to 256 of `A-Za-z0-9_-`, is sent as `X-Telegram-Bot-Api-Secret-Token`. The same call again answers `Webhook is already set`.
- The runtime endpoint comes after the configured ones, with a breaker of its own (the consumer's `breaker` settings) and a 10 s timeout. `/v1/status` lists it with `"runtime": true`, and metrics label it `endpoint="runtime"`.
- `setWebhook` with an empty `url`, or `deleteWebhook`, removes it.
- It survives a reload that keeps the consumer, `accept_set_webhook: true`, the bot feeding the consumer and that bot's token. A reload that changes any of them clears it (logged). Rotating the bot's token is the way to evict an endpoint its holder set; a restart also forgets it. A `setWebhook` or `deleteWebhook` still in flight when its token is rotated changes nothing and answers `401 Unauthorized`. A removed consumer that still drains keeps its endpoint to deliver what it holds, unless the reload changed its bot's token.
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

A call books every quota it needs, or none, each for the instant the call leaves: when one quota delays it, the others are charged for that later slot, so a backlog on `global` does not let one chat's calls bunch up beyond `per_chat`. Each quota admits exactly what its rate and burst allow: `burst` calls at once, then one every `per / count` (with the defaults, 30 at once and then one every 33 ms for `global`; 1 per minute with burst 4 lets 4 go at once and the fifth a minute later). `global` keeps the bot's booked slots as a calendar: a call delayed by its chat or group books the earliest slot `global` allows at or after its start, and the capacity before that slot stays free, so a chat over its quota never holds up the bot's other chats. Its chat is the query string's `chat_id`, else, when tgin holds its body (a JSON or urlencoded body up to 64 KiB), the body's, as telegram-bot-api reads a parameter given in both places; a body that streams unread (a multipart upload, or a longer body) leaves only the query string's. A call whose chat tgin does not know counts against `global` only. `@username` keys ignore case.

What a call beyond the limits gets depends on `mode`:

- `shape` (the default): it waits until its quotas admit it, the longest of their delays, if that is at most `max_delay` (`5s`); a longer wait is answered `429` at once. `max_delay: 0s` behaves like `reject`.
- `reject`: `429` at once.
- `off`: no limits.

tgin's `429` is Telegram's: `{"ok":false,"error_code":429,"description":"Too Many Requests: retry after N","parameters":{"retry_after":N}}` with `Retry-After: N`, N being the wait in whole seconds, rounded up and at most 3600. Delays and refusals count in `tgin_gateway_ratelimited_total`. The quotas belong to the process: several tgin processes serving one bot count separately. They survive a reload that keeps the bot's token and `rate_limit`, and every 60 s the `gateway-sweep` task forgets the chats whose quotas are full again.

### 429 smoothing

When the Bot API server answers `429` to a call whose body tgin holds (a method in `rate_limit.methods`, with a JSON, urlencoded or empty body up to 64 KiB) and its `parameters.retry_after` is at least 1 s and at most `gateway.retry_429.max_wait` (`10s`), tgin waits that long from the `429`, books the rate limits again (unless `mode: off`; a refusal passes the server's `429` on) and sends the call once more. The client gets the second answer and never sees the first. This retry does not depend on `mode`: with `mode: off` tgin still holds the bodies of those methods while `max_wait` is above 0. In every other case (a streamed body, a `429` without `retry_after` or with a longer one, `max_wait: 0s`) the `429` reaches the client as the server sent it. So a call goes to the server at most twice, and a streamed body only once. Each retry that goes counts in `tgin_gateway_retries_total`; one held back because its bot was [parked](#moving-a-bot-between-servers) meanwhile gets the parked `503` instead.

### Webhook replies

A push endpoint can answer an update with a Bot API call, as a webhook can answer Telegram. With `webhook_reply: execute` (the default), a `2xx` answer whose `Content-Type` contains `application/json`, `application/x-www-form-urlencoded` or `multipart/form-data` (in any case) and whose body is not empty is a call: its method is the body's `method` parameter (a JSON member, a form field or a multipart text field). tgin

- refuses it, as Telegram does, when the method is missing or empty, starts with `get`, or is `setWebhook`, `deleteWebhook`, `close` or `logOut`, in any case; it also refuses, on its own, a method name longer than 64 bytes or with a character outside `A-Za-z0-9_`;
- does not execute a body over 1 MiB (logged);
- refuses it, for a bot whose `file:` references are refused, when one of its parameters carries one ([Local mode and `file:` references](#local-mode-and-file-references));
- otherwise applies the bot's rate limits (in `shape` mode the call may wait; a call the limits refuse is skipped; a multipart reply counts against `global` only), then, unless the bot is [parked](#moving-a-bot-between-servers) by then (the reply is refused), POSTs the body unchanged, with its `Content-Type`, to the bot's Bot API server, `{url}/bot<token>/<method>`. Nothing is re-encoded, multipart files included. The call is made once, never retried, and bounded by the upstream's `response_timeout` or 30 s, whichever is shorter (30 s on the `api_url` shorthand). Nobody sees the server's answer, as with Telegram; a failure is logged with the server's `description`, tokens masked.

The reply runs before the update is acknowledged, so under `ordering: per_chat` the chat's next update waits for it. It never fails the delivery: the endpoint has processed the update, so a refused or failed reply still counts the update delivered. tgin never executes the body of a non-`2xx` answer, nor one that fails to arrive whole (logged), and with `webhook_reply: ignore` it reads no answer body. A graceful shutdown waits for a reply in progress. Replies share the delivery's at-least-once guarantee: an update that tgin is stopped by force while its reply runs is not acknowledged, and if the update is delivered again ([Delivery semantics](#delivery-semantics)), so is its reply. Outcomes count in `tgin_webhook_replies_total`.

Frameworks that reply this way: aiogram's `SimpleRequestHandler(handle_in_background=False)` when a handler returns a method object (sent as multipart), grammY with `canUseWebhookReply`, and Telegraf with `webhookReply: true` (its default) for `answerCallbackQuery`, `answerInlineQuery`, `deleteMessage`, `leaveChat` and `sendChatAction`. python-telegram-bot never replies. aiogram's default `handle_in_background=True` answers `{}` to every update, which counts as `refused` (no method); use `webhook_reply: ignore` with it.

### Loop detection

tgin never acts on its own pushes. Every push carries `Via: 1.1 tgin-<pseudonym>`, the pseudonym of the tgin instance (one process in 1.0): 16 hex digits chosen at random when it starts. Both planes answer `508 Loop Detected` to a request whose `Via` lists the instance's own pseudonym (RFC 9110 §7.6.3): the admin plane `{"error":"loop detected"}`, the data plane Telegram's shape, each carrying the instance's `Via`. So an endpoint that points back at tgin (its webhook ingress, a pull endpoint, the gateway, or the admin API) is refused, and the pushing dispatcher, seeing its own `Via` on the `508`, dead-letters the update at once. Only that `508` is final: any other `508` is a transient `5xx`, retried. The refusal is logged as a warning at most once a minute per plane, with the bot from `X-Tgin-Source`. Another tgin instance has another pseudonym, so chained instances still pass updates to each other.

The gateway's calls to the Bot API server and its webhook replies carry the same `Via`, after any `Via` the downstream's request carried. A bot whose upstream URL points at tgin itself therefore gets `508` (counted `upstream_error`) instead of tgin forwarding the call to itself without end.

### Gateway metrics

`tgin_gateway_requests_total{bot,method,outcome}` counts every gateway request of a configured bot once, and every webhook reply that went to the Bot API server. `method` is the method's canonical name (`sendMessage`, whatever case the client used), `other` for a method tgin does not know, and `file` for downloads; a method's series appear on its first request. `outcome` is one of:

| `outcome` | The request |
|---|---|
| `ok` | The Bot API server, or tgin's own file route, answered with a status that is not an error. |
| `rejected` | The server (or tgin's own file route) answered with a `4xx` other than `429`, or tgin refused it for this bot: `404` for a consumer the bot does not route to, `405`, `401` for a token rotated while the request was in flight, a `400` or `413` for a [`file:` reference](#local-mode-and-file-references) or parameters tgin cannot check, or the `503` of a [parked](#moving-a-bot-between-servers) bot. |
| `rate_limited` | It got a `429`: from the bot's rate limits, or the server's own, passed on. |
| `upstream_error` | The server answered with a `5xx`, or its response body failed, or stalled for `upstream_idle_timeout`, after the headers. |
| `unreachable` | No HTTP answer came: the connect failed or never finished, the upload stalled for `upstream_idle_timeout` (the client stopped sending, or the server stopped reading) or broke off, the connection was lost, or no response headers arrived within the response wait. Answered `502` ([Forwarding](#forwarding)). |
| `client_closed` | The client went away mid-request: its body broke off or was malformed (`400 Bad Request: the request body ended early`, which nobody reads), it left while tgin waited for the server's response headers, a rate-limit delay or the server's `retry_after` (no answer; the server may have received the call), or the body of an intercepted call did not arrive within `upstream_idle_timeout` (`408`). |
| `local` | An intercept answered it. |

Requests with an unknown token count only in `tgin_gateway_unauthorized_total`, without labels, so no token ever creates a series; paths that are no gateway path are not counted. `tgin_gateway_upstream_seconds{bot,method}` is the time from sending a request (or a reply) to the server's response headers. `tgin_gateway_ratelimited_total{bot,scope,action}` counts delays and refusals by the rate limits, `scope` `global`, `chat` or `group` (the quota that decided), `action` `delayed` or `rejected`, for requests and webhook replies alike. `tgin_gateway_retries_total{bot}` counts the second sends after the server's `429` that went.

`tgin_webhook_replies_total{consumer,outcome}`:

| `outcome` | The reply |
|---|---|
| `executed` | The Bot API server answered `2xx`. |
| `failed` | The server answered otherwise, or not within the reply's bound ([Webhook replies](#webhook-replies)), or the gateway does not serve the consumer's bot. |
| `refused` | No method, or one that is refused (see above), or it came from a runtime endpoint whose bot's token changed since its `setWebhook`, or it carries a `file:` reference (or parameters tgin cannot check) for a bot whose `file:` references are refused, or its bot is parked. |
| `too_large` | Its body was over 1 MiB. |
| `rate_limited` | The bot's rate limits refused it. |

### Framework setup

Point each downstream at its consumer's gateway path, `http://<tgin>:8080/c/<consumer>`, with the bot's own token. A polling bot then polls the pull consumer; a webhook bot behind a push consumer with `accept_set_webhook: true` registers its URL with tgin; every other call goes to the bot's Bot API server through tgin. The [end-to-end suite](tests/e2e) runs the first four frameworks below, in these versions, with these settings:

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

### Checking against real Telegram

Before a release, a maintainer runs `tests/smoke/real_telegram.py` (Python 3, standard library only) against the real Bot API with a test bot. It starts tgin with [its own config](tests/smoke/real_telegram.yaml) and the [webhook downstream](tests/smoke/hook.py), and reads the token from `TGIN_SMOKE_TOKEN` or from a file given as its argument; it never prints the token. Phase A needs no chat: identity, intercepts, proxied calls, unknown tokens, a capture and metrics. Phase B asks a person to send the bot a message containing `tgin-smoke-reply`, then checks the long poll, the pull consumer, the pushes, a webhook reply, per-chat shaping and a multipart upload with its download. CI never runs it; `--help` lists its options.

With `--self-hosted URL --files-dir DIR`, the same phases run against a self-hosted `telegram-bot-api --local` server through an upstream `kind: local, local_mode: true` whose files tgin serves from `DIR` (`--server-dir` names the directory as the server sees it); the server keeps its API credentials, which the script never reads. It also checks that `getFile` answers relative paths through tgin and every download equals the file on the server's disk, that `file:` references are refused, `tgin_bot_info` and `tgin_upstream_up`, a 53 MiB document up and down byte for byte, and webhook ingress over plain HTTP from the server to `--public-url` (a reload switches the bot to it for a second message, containing `tgin-smoke-hook`, and back). `--migrate` starts the bot on the cloud, asks for confirmation, moves it with `tgin admin bot logout` (the bot cannot log in to the cloud again for 10 minutes) and leaves it on the server. tgin must see the server's directory under the server's own names, which keep the token's `:`; on Windows, where Docker Desktop shows a bind-mounted `:` as U+F03A, run the script in a container on the server's network and files volume (its docstring has the command). `--fake` dry-runs any of this against the [fake Bot API](tests/e2e/fake-telegram) (`--local-mode` for the server), injecting the messages through its control API.

## Self-hosted Bot API servers

A bot can use a self-hosted [telegram-bot-api](https://github.com/tdlib/telegram-bot-api) server instead of `https://api.telegram.org`. tgin long-polls it or takes its webhooks, forwards the gateway's calls to it, serves the bots' files from its disk, refuses requests that would make it read its own files, and moves bots between servers when an operator asks. Wherever this manual says "the Bot API server", it holds for both kinds.

### Upstreams

`upstreams` names the Bot API servers, and a bot names its own with `upstream`:

```yaml
upstreams:
  tba:
    kind: local                              # a self-hosted telegram-bot-api
    url: http://telegram-bot-api:8081
    local_mode: true                         # the server runs with --local
    files:
      dir: /var/lib/telegram-bot-api/files   # its --files-dir, as mounted in tgin

bots:
  - name: shop
    token: ${env:SHOP_TOKEN}
    upstream: tba
    ingress:
      long_poll: {}
    routes:
      - to: [workers]
```

- `kind` is `cloud` (Telegram's Bot API, or a server that keeps its rules) or `local` (a self-hosted `telegram-bot-api`). It decides the file, webhook and timeout rules below.
- `url` is the server's base URL, `http` or `https`. Plain `http` to a host that is not loopback works, with a warning that names the upstream (never its URL): the tokens travel in clear, so keep such a server on a network only tgin reaches. The same warning on an `api_url` adds that a self-hosted server belongs under `upstreams` with `kind: local`: on the shorthand, tgin takes it for the cloud and refuses no [`file:` reference](#local-mode-and-file-references).
- `local_mode: true` (only with `kind: local`) says the server runs with `--local`: `getFile` answers absolute paths on its disk, and it posts webhooks over `http` to any port. tgin refuses `file:` references to every `kind: local` upstream, `local_mode` or not, as the server may run `--local` whatever the config says.
- `files` (only with `kind: local`) says how tgin serves the bots' files: `dir`, with an optional `server_dir`, or `url` ([File downloads](#file-downloads)).
- `response_timeout` bounds the wait for the server's response once a request was sent: `1m` for `cloud` and `10m` for `local` unless set, above 0 and at most `1h` ([Timeouts](#timeouts)).
- Upstream names follow the rules for bot names, except `api_url`, which is reserved. A bot has `upstream` or `api_url`, not both: `api_url` (or neither, for `https://api.telegram.org`) is shorthand for an unnamed `cloud` upstream, which `/v1/config`, `/v1/status` and the metrics call `api_url`.
- Every bot has its own token: a token two bots share is a config error.

### File downloads

| Upstream | `/file/bot<token>/<path>` through tgin | `getFile` through tgin |
|---|---|---|
| `cloud`, `api_url` | Forwarded to `{url}/file/bot<token>/<path>`. | As the server answers it. |
| `local` with `files.dir` | Served from the bot's directory under `dir`. | In local mode, an absolute path under `{server_dir}/<bot's directory>/` becomes the path under that directory; otherwise as the server answers it. |
| `local` with `files.url` | Forwarded to `{files.url}/<token>/<path>`. | In local mode, an absolute path through the bot's own directory becomes the path under it; otherwise as the server answers it. |
| `local` without `files` | `404 Not Found: files are not served for this upstream`. | As the server answers it. |

A server in local mode answers `getFile` with the file's absolute path on its disk, `<files directory>/<bot's directory>/documents/file_1.pdf`, and the server's own `/file` route serves no file then. tgin makes such a path relative again, `documents/file_1.pdf`, so a framework downloads through tgin as it would from Telegram, without the cloud's 20 MB limit. The server names each bot's directory after its token, `:` included (`123456:ABC-DEF…`); only when its file system refuses `:` does it write `~` instead (`123456~ABC-DEF…`). tgin tries both, the `:` form first, and serves a bot only its own directory: a path from another bot's directory is never rewritten (it passes unchanged), and no request reaches a file outside the calling bot's directory. The disk and origin routes (`files.dir`, `files.url`) serve only paths of at least two segments, a directory and a file below it (`documents/file_1.pdf`), as the server stores files (the cloud route forwards the path as it came): the top of a bot's directory, where a server without its own `--files-dir` keeps the bot's TDLib database, is never served. tgin asks the server for a `getFile` answer it rewrites without compression, and reads one of at most 64 KiB; a larger answer passes unchanged.

**From disk** (`files.dir`). `dir` is the server's `--files-dir` as mounted in tgin; read-only is enough. Give the server a `--files-dir` of its own, apart from its working directory, so that tgin never sees the bots' databases. `server_dir` is the same directory as the server sees it, an absolute path: give it when the two differ, as when tgin mounts the volume elsewhere; it defaults to `dir`. tgin needs to read the files: the [server image](docker/telegram-bot-api) has them written 0640 in directories 0750 of its group 10001, so give tgin that group (Compose `group_add: ["10001"]`). The request path is percent-decoded once and must be plain relative segments, at least two: a single segment, an empty, `.` or `..` segment, a `\` or a NUL, or a path whose real location (symbolic links followed) leaves the bot's directory, gets `404 Not Found`, and a file that is not there `404 Not Found: file not found`. A file is sent as `application/octet-stream` with its length, `Last-Modified` and a weak `ETag`; `If-None-Match` (else `If-Modified-Since`) can answer `304`, one byte range `206` (or `416` when it lies past the end), and several ranges get the whole file. Files stream in constant memory, and a download in progress holds the [shutdown](#shutdown)'s drain open as a forwarded one does.

**From an origin** (`files.url`). `url` is an HTTP origin that serves the server's `--files-dir` as it is on disk. tgin asks it for `{files.url}/<token>/<path>`, the token with its `:`, percent-encoded as `%3A`; a server that fell back to `~` names (its file system refuses `:`) needs `files.dir` instead. The path is percent-decoded once, checked as above, and sent encoded again, so an encoded traversal (`..%2F`, `%2e%2e%2f`) gets `404` and never reaches the origin. `upstream_idle_timeout` bounds the origin's answer as well as each pause in it. An origin that cannot be reached gets `502 Bad Gateway: the file server is unreachable`, one that does not answer `502 Bad Gateway: the file server did not answer`; its failures do not count in `tgin_upstream_up`, which is about the Bot API server. A plain-`http` `files.url` to a host that is not loopback loads with a warning, as an upstream URL does. Every request path carries a bot's token, so the origin's access logs hold every bot's token: keep them as you keep the tokens, or turn them off.

**Windows hosts.** tgin on Windows looks only for the `~` form of a bot's directory, as Windows names cannot hold `:`. A Linux server keeps the `:`, and a Docker Desktop bind mount shows it to Windows as U+F03A, so `files.dir` finds no directory of the bot's. Run tgin in a Linux container that mounts the server's files volume instead.

### Local mode and `file:` references

A server in local mode reads a parameter that begins with `file:/` as a file on its own disk, and uploads whatever it can read there, other bots' databases included. So for a bot on a `kind: local` upstream, whatever its `local_mode` says, tgin refuses a call in which any parameter begins with `file:` (case-sensitive; tgin stops one character short of the server's prefix, a margin against newer servers) with `400 Bad Request: local file uploads are disabled by tgin`, unless the bot has `allow_local_files: true`. A bot on a `cloud` upstream, the `api_url` shorthand included, gets no refusal: a self-hosted server configured as `api_url` is not protected, so move it under `upstreams` with `kind: local`. tgin reads every parameter the server reads, decoded as the server decodes it:

- the query string, for every method, `GET` included;
- JSON bodies at any depth, and JSON carried in string parameters (`media`, `stickers`, `results` and the like);
- urlencoded values;
- multipart text fields, parsed as telegram-bot-api's own `HttpReader` parses them, so tgin and the server agree on which part is a file.

JSON and urlencoded bodies up to 64 KiB are read whole and scanned; longer ones are scanned as they stream, and one that announces, or turns out to have, more than 1 MiB, the server's own limit for parameters, gets `413 Request Entity Too Large`. A multipart body streams too. tgin holds back the part of a streamed body that would complete a `file:` reference, or its end, until the scan has cleared it, and drops the connection to the server when the scan refuses it, so the server never gets a whole request; multipart text fields over 1 MiB in all get `413` too. A body that cannot be checked gets `400 Bad Request: can't parse the request parameters`: a malformed multipart body, or a request with more than one `Content-Type`. JSON carried in strings more than four levels deep is refused as a `file:` reference. Refused calls count as outcome `rejected`. A [webhook reply](#webhook-replies) of such a bot is scanned the same way; a refused reply is not made, and counts as `refused`.

The scan refuses some calls that read no file: a text or caption that begins with `file:`; a caption that begins with `[` or `{` and holds a quoted `"file:…"`; a JSON key that begins with `file:`. `allow_local_files: true` turns it off for the bot: set it only when everyone who holds the bot's token may read the server's disk.

### Timeouts

A local server takes uploads of up to 2000 MB and sends each on to Telegram before it answers, so its response can come minutes after the upload ended. The response wait of a bot on an `upstreams` entry is its `response_timeout` (`10m` for `local`); raise it, up to `1h`, above the longest upload the server must pass on. A bot on the `api_url` shorthand, which has no `response_timeout`, waits its `gateway.upstream_idle_timeout`. Webhook replies wait the smaller of `response_timeout` and 30 s. When the wait runs out the client gets `502 Bad Gateway: the Bot API server did not answer; the call may have been executed` ([Forwarding](#forwarding)): the server may still send the file, and a client that sends it again may send it twice.

### Webhooks from a local server

A bot on a `local` upstream with `local_mode: true` may take its updates as webhooks the server posts over plain `http`, to any port: `public_url: http://tgin:8080` works with `register: true`. Its `max_connections` may go up to 100 000, but a value above 100 must not exceed `server.max_connections`, which the data plane holds to: that is a config error. Every other upstream keeps Telegram's rules, checked at load: `https`, port 443, 80, 88 or 8443, and `max_connections` 1 to 100. `ip_allowlist: telegram` names Telegram's networks, which a local server does not post from, so it is an error on a `local` upstream: list the server's networks instead (`ip_allowlist: [10.0.0.0/8]`).

A long poll on a `local` upstream retries a `401` or `404` with a backoff of up to 5 min instead of stopping, as the server answers them when Telegram rejects its own api_id or api_hash; only a `403` stops it. [Registration](#webhook-ingress) follows the same rule.

### Moving a bot between servers

Telegram gives a bot's updates to the one server the bot is logged in on; logged in on two, it is sure to get them all on neither. A bot leaves a server by `logOut` or `close` there, which tgin refuses from downstreams ([Intercepts](#intercepts)). An operator moves it instead:

```console
$ tgin admin bot logout shop --yes --timeout 1m
shop logged out of its Bot API server (api_url); parked until a reload changes its upstream
```

which runs, for the bot:

1. it waits until the bot's Bot API server holds no update for it, its ingest running on: a long poll polls until it gets an empty batch, a webhook bot asks `getWebhookInfo` until `pending_update_count` is 0;
2. it stops the bot's ingest, and parks the bot;
3. it waits until tgin has delivered what it holds of the bot, queued or leased;
4. it closes the gateway to the bot: from now on no call of it leaves for a server, and the move waits for those being sent;
5. it calls `logOut` on the server (`bot close`: `deleteWebhook`, then `close`).

One deadline covers steps 1 to 4, counted from the request, so the time a move waits in the queue counts too: `--timeout`, a duration such as `30s` and at most `1h`, or `reload.drain_timeout` without it, also cut to `1h`. Steps 1 and 3 end a reserve, `min(5 s, timeout / 4)`, before the deadline, which step 4 keeps for the bot's calls already on their way.

- Step 1 not done by then (or a move that waited in the queue past its deadline): the command fails with `504 upstream not drained` and changes nothing: the bot runs on, and nothing was called on the server (the warning in the log names a failing `getWebhookInfo` when that is why).
- Step 3 not done by then: tgin goes on; `left` reports how many updates it still held as the server was called, and they are still delivered.
- Calls still being sent at the deadline (a large upload, say): the command fails with `504 calls in flight`, whose description gives their count and the way out (retry later, or with a longer `--timeout`). The bot is unparked and runs on, and nothing was called on the server.
- The server refuses the call: the command fails with `502`, and the bot is unparked and starts again. When `close` fails after `deleteWebhook`, tgin sets the webhook of a `register: false` bot again, as it was; one with a custom certificate cannot be set again, and the answer says the webhook must be set again by hand. A `register: true` bot registers its webhook again as it starts.
- A move whose requester goes away before step 1 is done (the HTTP client closed, the CLI stopped) is dropped, having changed nothing; after that it goes on.

Without `--yes` the command only prints these steps and exits 64, as it does for a `--timeout` over `1h`; with `--timeout D` it waits at most `D` plus 60 s for the answer, and without one as long as tgin takes.

The admin API is `POST /v1/bots/<name>/logout` or `/close`, with an optional `?timeout=30s` (the config's duration format, without month or year units, at most `1h`; another value gets `400 {"error":"invalid timeout"}`). It answers:

| Status | Body | When |
|---|---|---|
| `200` | `{"bot":"shop","left":0,"upstream":"api_url","result":…}` | Moved; `result` is the server's description of its answer, if any, tokens masked. |
| `404` | `{"error":"unknown bot"}` | No bot of the running config has the name. |
| `409` | `{"error":"draining"}`, `{"error":"parked"}` | tgin is shutting down; the bot is parked already. |
| `502` | `{"error":"upstream refused","description":"…"}` | The server refused the call; `description` is its text, tokens masked. |
| `504` | `{"error":"upstream not drained"}`, `{"error":"calls in flight","description":"…"}` | As above. |

Moves and reloads run one at a time: a move waits for the one in progress, and moves already waiting run before any reload that has not begun, so a move acts on the config in force when it was asked for. A `SIGHUP` is never dropped meanwhile.

**Parked bots.** From step 2, a parked bot runs no long poll, and its webhook route answers `503` with `Retry-After: 5`; `/v1/status` shows it with `"parked": true`, and `tgin admin status` with `, parked` after its state. From step 4, every call of it that tgin would send to a Bot API server or a file origin gets `503 Service Unavailable: the bot is being moved to another Bot API server by tgin` with `Retry-After: 5`, counted `rejected`: method calls, downloads forwarded to the cloud or to a `files.url` origin, and calls that came in earlier and were still waiting on the rate limits or a `429`. Its webhook replies are refused, counted `refused`. A server the bot was logged out of would log it in again on its next call, which is what this prevents. What needs no server goes on: the intercepts (a cached `getMe`, the webhook methods), pull consumers' `getUpdates` of the updates tgin still holds, and files served from disk.

A reload that leaves the upstream's name and URL as they were keeps the bot parked, with a warning; a reload that changes them starts it on the new upstream (logged at info), and its calls go out again. Only removing the bot from the config ends its parking otherwise. While a bot is parked, tgin does not shut down for [every source failing](#shutdown). Parking lives in memory: started again with the old config, tgin starts the bot on its old server, which logs it in there again, so change the config before a restart.

**Runbook.** Between the move and the reload, the bot's downstreams get the `503` above for every call that needs a server. A downstream that retries after `Retry-After` gets through once the reload has started the bot on its new upstream; reload soon after the move.

- **Cloud → local.** `tgin admin bot logout <bot> --yes`, then set the bot's `upstream` to the local server and reload: the server logs the bot in on its first call. The bot cannot log in to the cloud again for 10 minutes.
- **Local → local.** `tgin admin bot close <bot> --yes` on the old server, then change the bot's `upstream` and reload. A server that has run for over 10 minutes refuses `close` with `429` within 10 minutes of the bot's start; the command then fails with `502` and the bot runs on.
- **Local → cloud.** `tgin admin bot logout <bot> --yes` on the local server, then set the bot back to the cloud (its `api_url`, or a `cloud` upstream) and reload. Telegram documents no wait for this direction; if the bot left the cloud less than 10 minutes ago, wait until those 10 minutes are up. A long poll the cloud rejects (`401`, `403` or `404`) stops for good: it needs a reload that changes the bot, or a restart, and if it was the last long-poll source and no webhook bot exists, tgin [exits 2](#command-line-and-exit-codes).

### Running a server

- **A workload of its own.** Run the server as its own container or service; tgin neither starts nor supervises it. Keep its working directory on persistent storage: it holds the bots' sessions and pending updates, and losing it logs every bot out of the server. Give the files their own `--files-dir`, which is what tgin mounts.
- **Never one server per tgin replica.** tgin replicas share the server their bots live on; a server per replica would log each bot in several times.
- **One server per token set.** A bot is logged in on one server, which owns its update queue. Many bots may share a server, or be split across servers with the server's `--filter`.
- **A private network.** The server has no TLS and no authentication beyond the token in the URL: anyone who reaches it with a token controls that bot, and anyone with any valid token makes it log in as that bot with your api_id. Keep it on a network only tgin reaches, publish no port, and keep the downstream bots off that network, so they reach it only through tgin.
- **Never expose the statistics port.** Leave `--http-stat-port` off: its page has no authentication, lists bots by token prefix and changes the log level on request.
- **Local mode trusts its callers.** A `file:` parameter makes it upload any file it can read (tgin refuses them for every `kind: local` upstream unless `allow_local_files`), and a webhook may point it at any address, private ones included: limit its outgoing traffic.
- **Its api_id and api_hash** stay with the server: tgin never needs them. Keep them out of images, command lines and version control.

tgin never logs the server's URL or a bot's directory or file path on it; each bot's directory names, and the `%3A` forms of its token that origin paths carry, are masked in the logs like the token itself.

### Compose example

[examples/self-hosted](examples/self-hosted) runs the server, built from [docker/telegram-bot-api](docker/telegram-bot-api), in local mode next to tgin and one bot: the server shares a network with tgin alone and publishes no port, its files volume is mounted read-only in tgin at the server's own path (`files.dir`), and the bot reaches the server only through tgin's gateway. Its `compose.fake.yaml` runs the same stack against the fake Bot API in local mode, without Telegram.

## Hot reload

**Triggers.** A reload re-reads the config file. It runs on:

- `SIGHUP` (Unix only);
- `POST /v1/reload` on the admin API, or `tgin admin reload`, which answer with the result;
- a change of the config file while `reload.watch` is on (the default). tgin watches the file's directory, waits `reload.debounce` (500 ms) of quiet, and also re-reads the file every 2 s for file systems that send no change events; a change is a change of the file's bytes. When file events cannot be set up at all (an unreadable directory, the system's watch limit reached), tgin logs `file events unavailable (…); polling every 2 s` and keeps re-reading the file. A ConfigMap volume updates mounted files in place, but a file mounted with `subPath` is never updated by Kubernetes, so tgin cannot see the new version. Likewise, a Docker bind mount of the single file keeps the original inode: an edit saved by renaming over the file (most editors, `sed -i`, Ansible) never reaches the container, though reloads still succeed. Mount the directory instead.

Reloads run one at a time, in order, and so do [bot moves](#moving-a-bot-between-servers), with them: moves already waiting run before any reload that has not begun, and a `SIGHUP` is never dropped. While tgin shuts down, `POST /v1/reload` answers `503 {"error":"draining"}` and `SIGHUP` is ignored.

**Rejection.** A config that cannot be read, fails validation or cannot be applied changes nothing: the running config stays, `tgin_config_reloads_total{outcome="rejected"}` grows, the errors are logged on one line, and `POST /v1/reload` answers `422` with `{"errors":[{"path","line","column","message","hint"}, …]}` (the location fields are `null` for an error without one).

**What applies live.** An applied reload raises the generation (`tgin_config_generation`, 1 at start) by one and answers with a report:

```json
{"generation":2,"added":["audit"],"removed":[],"changed":["frontdesk"],
 "sources_added":[],"sources_removed":[],"sources_restarted":["shop"],
 "upstream_changed":[],"restart_required":[]}
```

- Rules and targets apply to the next update.
- A consumer whose settings changed keeps its queue and applies them live; a push consumer whose endpoints, `balance`, `max_in_flight`, `retry` or `breaker` changed gets a new endpoint set with fresh breakers. A reload that only adds bots keeps every push consumer's endpoints, with their breakers and in-flight counts; only the bot names it reports in `X-Tgin-Source` grow.
- A new consumer starts empty. A consumer whose mode changed (pull ↔ push) is a new, empty instance; the old one drains as if removed.
- A removed consumer stops receiving updates at once and drains for `reload.drain_timeout` (30 s); what it still holds then is dropped as `tgin_consumer_dropped_total{reason="reload"}`, and its metric series go. A removed pull consumer stays reachable while it drains, on `/c/<name>/getUpdates` and on its aliases (unless the new config gives the path to something else), with its old `auth_token`, so its clients can fetch and acknowledge what it holds.
- A consumer added back with the same mode while its old instance still drains gets that instance back, with its queue and the new settings; a revived push consumer gets fresh breakers and stays paused if it was paused.
- A bot whose `token` or `ingress` changed, or whose upstream changed its name, URL, `kind` or `local_mode`, is restarted alone; added bots start, removed bots stop. A bot now on another upstream (another name or URL) is also listed in `upstream_changed`; unless the bot was [parked](#moving-a-bot-between-servers) by a move, tgin warns that its updates may be split between the two servers unless it was logged out of the old one first. A parked bot starts on its new upstream (logged at info); one whose upstream's name and URL stay as they were stays parked, with a warning. Webhook paths and pull aliases change without restarting the listeners.
- `log.level`, `admin.token` (a rotated token applies to the next request), `admin.public_metrics`, `server.trusted_proxies`, `reload.*` and `runtime.lame_duck` / `runtime.shutdown_grace` (used when a shutdown begins) apply live.
- A bot's `gateway` settings and `allow_local_files`, and an upstream's `files` and `response_timeout`, apply live, without restarting its source. Its rate-limit quotas keep their state while its token and `rate_limit` stay, and the identity tgin learned from `getMe` while its token stays. A push consumer's `webhook_reply` applies to the next delivery, keeping its endpoints and breakers; `accept_set_webhook` and `honor_drop_pending` apply to the next request. A reload that turns `accept_set_webhook` off, gives the consumer another feeding bot (or none), or changes that bot's token clears the consumer's runtime endpoint ([`setWebhook` capture](#setwebhook-capture)).
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
| `POST /v1/bots/<name>/logout`, `/close` | token | `200 {"bot","left","upstream","result"}`; `400`, `404`, `409`, `502` or `504` ([Moving a bot between servers](#moving-a-bot-between-servers)) |

**Auth.** The token is sent as `Authorization: Bearer <admin.token>` and compared in constant time; a request without it gets `401 {"error":"unauthorized"}` with `WWW-Authenticate: Bearer`. Without `admin.token` the API needs no token, which validation allows only when every `admin.listen` address is a loopback address. An unknown consumer answers `404 {"error":"unknown consumer"}`, an unknown bot `404 {"error":"unknown bot"}`.

**Readiness.** `/readyz` answers `200` when tgin can serve and `503` when it cannot or is shutting down; that status code and the body's `phase` are the contract. The `components` are informative: today `engine`, `admin_plane`, `data_plane` (with a `server` block) and `source:<bot>` for each bot with an ingress, a source not being ready until it has started, also while a reload restarts it. Once shutdown begins, `/readyz` answers `503` with `"phase":"draining"`. A source that later fails does not make tgin unready; watch `tgin_source_state` for that.

**Compatibility.** Within a major version the admin API, the probe bodies and the metrics only grow: a minor release may add fields, enum values (states, phases, label values), metric families and error codes, and what exists keeps its meaning. Clients must ignore what they do not know, as `tgin admin` does.

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
     "updates_total": 120, "errors_total": 0, "upstream": "api_url"}
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
- `sources[]`, one per bot: `ingress` is `long_poll`, `webhook` or `null`; `state` is `starting`, `running`, `backoff`, `failed` or `stopped`. A long poll is `backoff` while it waits out a failed `getMe` or `getUpdates` (its task keeps running); a bot whose source could not be set up is `failed`. `error` (present only when there is one) says why the source backs off or failed, with secrets masked. `upstream` names the bot's upstream (`api_url` for the shorthand), and `"parked": true` is present while the bot is [parked](#moving-a-bot-between-servers) (its state is then `stopped`).
- `consumers[]`: `depth` (waiting to be leased), `leased`, `in_flight` (push deliveries in progress, retries waiting included), `paused`, and for push consumers each endpoint's origin (`scheme://host[:port]`; path, query and credentials are never shown), breaker (`closed`, `open` or `half_open`) and `runtime`: `true` for the endpoint a bot's `setWebhook` set through the gateway (listed after the configured ones), `false` for a configured one.
- `tasks[]`: every supervised task (long-poll sources, push dispatchers, background `getMe` calls, `setWebhook` registrations being retried, as `register:<bot>`, and `gateway-sweep`, which forgets idle rate-limit keys) with the supervisor's view of it: its state and, when it is restarted after a panic or failed, its last error, secrets masked. A long poll that backs off by itself stays `running` here; see `sources[]`.

`GET /v1/config` shows the config as written, with defaults filled in (except a webhook's omitted `path`, which defaults to `/webhook/<bot name>`): every secret is `"[redacted]"`, URL fields (`api_url`, an upstream's `url` and `files.url`, `public_url`, endpoint `url`) show only `scheme://host[:port]` (followed by `/…` when something was left out), and other strings keep their `${…}` references unexpanded with bot tokens and the config's secrets masked as `#####`.

## `tgin admin`

`tgin admin` calls the admin API of a running tgin:

```console
$ tgin admin status
$ tgin admin reload
$ tgin admin consumers
$ tgin admin pause <consumer>     # also: resume, drain, purge
$ tgin admin health
$ tgin admin bot logout <bot> --yes   # also: bot close; --timeout 1m
```

| Option | Environment | Meaning |
|---|---|---|
| `--url URL` | `TGIN_ADMIN_URL` | The admin API, `http` or `https`; default `http://127.0.0.1:9090`. |
| `--token-file PATH` | `TGIN_ADMIN_TOKEN_FILE` | A file holding `admin.token`; trailing line breaks are ignored. |
| `--json` | | Print the API's JSON instead of tables. |

`--url` and `--token-file` go before the subcommand; `--json` may go anywhere after `admin`. The CLI reads the token only from a file — never from an argument or an environment variable — so it does not show up in process listings or shell history. It never uses an HTTP proxy, whatever the proxy environment variables say, so the token goes straight to tgin.

`status` prints the version, generation and phase, then a table of sources (its `UPSTREAM` column names each bot's upstream, and a parked bot's state reads `stopped, parked`) and one of consumers; `consumers` prints the consumer table; `reload` prints the new generation and what changed, bots moved to another upstream and settings that need a restart included; `purge` prints how many updates it dropped; `bot logout` and `bot close` print what they did ([Moving a bot between servers](#moving-a-bot-between-servers)). A request waits at most 60 s (a reload may wait on the Bot API server); `bot logout|close --timeout D` (`D` at most `1h`) waits `D` plus 60 s, and without `--timeout` as long as tgin takes.

`health` asks `GET /readyz`, needs no token (it does not read the token file), waits at most 2 s, prints `ready` or `not ready (…)`, and exits 0 when ready, else 1; when the admin API cannot be reached or does not answer in time, it prints the error on stderr and exits 1. The container image's `HEALTHCHECK` runs it.

Exit codes: 0 on success; 1 when the token file cannot be read, the API cannot be reached or does not answer in time, it answers with an error (printed with the server's `error` or its list of config errors), or `health` finds tgin not ready; 64 for a malformed command line (a `bot logout` or `bot close` `--timeout` over `1h` included), and for `bot logout` or `bot close` without `--yes`, which only print the steps they would take.

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
| `tgin_gateway_requests_total` | counter | `bot`, `method`, `outcome` | Gateway requests of configured bots, and webhook replies sent to the Bot API server; `method` is the canonical method name, `other` or `file`; `outcome` `ok`, `rejected`, `rate_limited`, `upstream_error`, `unreachable`, `client_closed` or `local` ([Gateway metrics](#gateway-metrics)). |
| `tgin_gateway_upstream_seconds` | histogram | `bot`, `method` | From sending a gateway request or webhook reply to the Bot API server's response headers; buckets from 1 ms, doubling, 20 of them (the last at about 524 s). |
| `tgin_gateway_ratelimited_total` | counter | `bot`, `scope`, `action` | Requests and webhook replies the rate limits delayed or refused: `scope` `global`, `chat` or `group`, `action` `delayed` or `rejected`. |
| `tgin_gateway_retries_total` | counter | `bot` | Requests sent again after the Bot API server's `429`. |
| `tgin_gateway_unauthorized_total` | counter | | Gateway requests whose token matches no configured bot. |
| `tgin_webhook_replies_total` | counter | `consumer`, `outcome` | Webhook replies by `outcome` `executed`, `failed`, `refused`, `too_large` or `rate_limited` ([Gateway metrics](#gateway-metrics)). |
| `tgin_bot_info` | gauge | `bot`, `upstream` | 1 for each bot; `upstream` is the name of its upstream, or `api_url` for the shorthand. A reload that changes the upstream's name or URL replaces the series. |
| `tgin_upstream_up` | gauge | `bot` | 1 after any HTTP answer from the bot's Bot API server (to its long poll, `getMe`, `setWebhook` and the like, gateway calls and webhook replies), whatever the status; 0 after a connect failure, a timeout or a lost connection. Absent until the bot's first outcome. A downstream that stalls its own upload, and a `files.url` origin, do not count. Kept across reloads that keep the upstream's name and URL. |

Apart from `tgin_build_info`'s, every label value is a bot or consumer name, an upstream name (in `tgin_bot_info` only), an endpoint index, a method name from tgin's table or a fixed word, so no series carries a URL or a token. A bot's series go when a reload removes it; a consumer's when its drain ends. Metrics follow the admin API's [compatibility rule](#admin-api-v1).

## Logging

Logs go to stdout, as `text` or `json` lines (`log.format`, fixed at start). `log.level` sets the most verbose level and follows reloads; a non-empty `RUST_LOG` overrides it (its directives keep winning after a reload). Text logs are coloured only when stdout is a terminal and `NO_COLOR` is unset or empty. Every line passes through the redactor (see [Security](#security)), panics included.

## Security

**Secrets.** Write secrets as `${env:…}` or `${file:…}` references rather than literally, so the config file can be shared. `${file:…}` suits Docker and Kubernetes secrets mounted as files. An empty secret is an error. Config errors never quote a secret: a malformed reference in a secret or URL field is reported without its text, and YAML errors carry the path, line and column without the source line.

**Redaction.** Every log line is masked before it is written: each secret of every config loaded since start (bot tokens, also with `:` written as `~`, as a self-hosted server may name a bot's directory, and as `%3A` or `%3a`, as a file origin's paths carry it; webhook secrets, `admin.token`, pull `auth_token`s, push `secret_token`s) and anything shaped like a bot token is replaced by `#####`. Secrets shorter than 8 characters are not masked, as they would match ordinary words; use long random secrets. `/v1/status` and `/v1/config` redact as described under [Admin API v1](#admin-api-v1), and metric labels hold no URLs. The gateway never logs a request path, an upstream URL, or a file's path on a self-hosted server, since all of them carry the bot token.

**Auth model.**

| Surface | Protection |
|---|---|
| Admin plane (`/v1/*`, `/metrics`) | `admin.token` as a bearer token; required unless every admin listener is on loopback. `/metrics` can be opened with `admin.public_metrics`. `/livez` and `/readyz` are always open. |
| Pull consumers | Optional `auth_token` per consumer, as a bearer token. Without it, anyone who can reach the data plane can take the consumer's updates. |
| Webhook ingress | `secret_token` (`auto` by default) checked before the body is read; optional `ip_allowlist`; `trusted_proxies` decides whose `X-Forwarded-For` counts. |
| Bot API gateway | The bot token in the path, compared in constant time with every configured bot's token; an unknown token gets `401` and nothing goes to the Bot API server. Its holder acts as the bot, polls the pull consumers the bot routes to, downloads the bot's own files, and, with `accept_set_webhook`, chooses where a push consumer posts ([trust model](#setwebhook-capture)); with `allow_local_files` on a `kind: local` upstream, it reads the server's disk ([`file:` references](#local-mode-and-file-references)). |
| Push endpoints | The endpoint's `secret_token` is sent as `X-Telegram-Bot-Api-Secret-Token`, for the downstream to check. |

Every token compare runs in constant time.

**TLS.** The data plane terminates TLS itself when `server.tls` is set (rustls, HTTP/1.1); the certificate files are read at start (a missing or bad file stops tgin with exit code 1) and again on every reload. The admin plane has no TLS: keep it on loopback or a private network, or put a TLS proxy in front, since the bearer token travels in clear text. Outgoing calls to the Bot API and to push endpoints verify server certificates against the platform's trust store, never follow redirects, and use the `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` environment if set (`tgin admin` never does).

**Telegram webhooks need HTTPS** on 443, 80, 88 or 8443 (Telegram's rule): put tgin behind a TLS-terminating proxy, or give it `server.tls` and a `listen` on one of those ports. A self-hosted server in local mode posts over plain `http` to any port ([Webhooks from a local server](#webhooks-from-a-local-server)).

## Shutdown

`SIGTERM` or `SIGINT` (Ctrl+C, Ctrl+Break, closing the console or a system shutdown on Windows) starts the shutdown, as does every source failing for good. tgin then:

1. **Stops being ready.** `/readyz` answers `503`, reloads are refused.
2. **Lame duck** (`runtime.lame_duck`, 5 s; skipped without a `server` block). Everything keeps working, so load balancers can take tgin out of rotation.
3. **Stops ingest.** Long polls stop (updates not yet admitted are left to the Bot API server); webhooks answer `503` with `Retry-After: 5`.
4. **Drains** for up to `runtime.shutdown_grace` (15 s), until no update is pending and, for 1 s since, no call through the gateway has gone to a Bot API server or file origin or is still under way (waiting for the answer, or streaming it, a download included), and no file served from disk is still streaming: push consumers keep delivering, pull consumers keep serving (a poll on an empty queue answering within 500 ms), and the gateway keeps forwarding the downstreams' calls, so a bot that answers an update after acknowledging it still gets its answer through. Calls the gateway answers itself, polls included, do not hold the drain open; without a data plane the drain ends once nothing is pending.
5. **Closes** the consumer tasks and the data plane, giving requests 1 s; tasks still running then are aborted. Updates still pending are lost: they are logged (`exiting with updates not delivered`) and counted in `tgin_shutdown_abandoned_total`.
6. **Closes the admin plane** last.

With the defaults the whole shutdown takes at most about 22 s; set the orchestrator's stop timeout to 30 s (Kubernetes `terminationGracePeriodSeconds: 30`, `docker run --stop-timeout 30`, Compose `stop_grace_period: 30s`; Docker's default of 10 s cuts the drain short).

A second `SIGTERM`/`SIGINT` (or Ctrl+C, Ctrl+Break) during the shutdown stops at once with exit code 130, losing what is pending. When the shutdown began because every source failed, the first signal is only logged and a second one forces the exit. A signal while tgin is still starting stops what has started and exits 0. Closing the console on Windows, which kills the process about 5 s later, skips the lame duck and caps the drain at 4 s.

**Every source failing.** A long-poll source fails for good when its Bot API server rejects the token: `401`, `403` or `404` from a `cloud` upstream, `403` from a `local` one (whose `401` and `404` are retried, see [Webhooks from a local server](#webhooks-from-a-local-server)). When every bot with an ingress uses long poll, all of their sources have failed and no bot is [parked](#moving-a-bot-between-servers), tgin shuts down as above and exits with code 2. A webhook bot never fails this way, so a config with one never exits 2.

## Deployment

**Rules that follow from the design.**

- **One tgin per long-poll token.** A Bot API server, Telegram's or a self-hosted one, allows one `getUpdates` poller per token; a second gets `409 Conflict`, and tgin backs off 5 s and logs `another process is polling this bot token`. Replace a tgin that polls with stop-then-start (Kubernetes `strategy: Recreate`, `replicas: 1`), not a rolling update that briefly runs two.
- **Queues are per process**, and so are the gateway's rate limits. A pull client must poll the tgin that holds its consumer's updates.
- **Downstream bots go through the gateway.** Bots behind tgin share its token. Through the gateway, their `getUpdates`, `setWebhook`, `deleteWebhook`, `logOut` and `close` are answered by tgin. A bot that calls its Bot API server directly with that token can still take tgin's updates (`getUpdates`), remove tgin's webhook or drop the updates the server holds for it (`deleteWebhook`, as python-telegram-bot, grammY and Telegraf do when they start polling, or aiogram's `delete_webhook(drop_pending_updates=True)`), or log the bot out.
- **A self-hosted Bot API server is a workload of its own**, one per token set and shared by every tgin that serves its bots, on a network only tgin reaches ([Running a server](#running-a-server)).

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
| 0 | Clean shutdown, or a command that succeeded. A shutdown signal ends a run this way, also one during start-up. |
| 1 | The config cannot be read or is invalid, tgin could not start (a listener cannot bind, a certificate cannot be loaded), or a failed `config`/`admin` command (including `tgin admin health` when not ready). |
| 2 | Every source failed for good: the Bot API server rejected each long-poll bot's token, no webhook bot exists and no bot is parked. |
| 64 | Malformed command line (unknown command or option, missing argument, a `tgin admin bot` `--timeout` over `1h`), or `tgin admin bot logout` or `close` without `--yes`. |
| 130 | A second shutdown signal cut the drain short. |

Updates lost at the end of a shutdown do not change the exit code; see `tgin_shutdown_abandoned_total` and the log.

## Further reading

- [docs/config-reference.md](docs/config-reference.md) — every config key, type and default.
- [PERF.md](PERF.md) — benchmarks against the original engine, and their method.
- [CHANGELOG.md](CHANGELOG.md) — changes per release.
- [SECURITY.md](SECURITY.md) — reporting vulnerabilities, verifying releases.
- [examples/simple](examples/simple) — a Compose stack with pull and push consumers whose bots use the gateway.
- [examples/self-hosted](examples/self-hosted) — tgin in front of a self-hosted Bot API server in local mode, with Compose, and against the fake Bot API.
- [docker/telegram-bot-api](docker/telegram-bot-api) — an image of the official Bot API server: its build, flags, volumes and security notes.
- [tests/e2e](tests/e2e) — the end-to-end suite: aiogram, python-telegram-bot, grammY and Telegraf against tgin and a fake Bot API (`cargo xtask e2e`).
