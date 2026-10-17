# tgin documentation

This is the manual of tgin 1.0. The key-by-key reference of the config file is [docs/config-reference.md](docs/config-reference.md), generated from the config model.

## Contents

- [Concepts](#concepts)
- [Configuration](#configuration)
- [Delivery semantics](#delivery-semantics)
- [Pull consumers: the `getUpdates` endpoint](#pull-consumers-the-getupdates-endpoint)
- [Push consumers: the requests tgin sends](#push-consumers-the-requests-tgin-sends)
- [Webhook ingress](#webhook-ingress)
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
  - **pull**: downstream bots fetch updates from tgin with a Telegram-shaped `getUpdates` call on `/c/<consumer>/getUpdates` (and on any alias path). Several processes may poll one pull consumer; they compete for its updates. A pull consumer serves exactly one bot: two bots routing to the same pull consumer is a config error.
  - **push**: tgin posts each update to one or more HTTP endpoints, with retries and a circuit breaker per endpoint. Several bots may route to one push consumer; the `X-Tgin-Source` header names the bot.
  Each consumer has its own bounded queue (`capacity` updates, `capacity_bytes` bytes).
- **Rules** (`bots[].routes`). Each bot's rules are tried in order; the first rule whose `match` holds decides where the update goes, and a rule without `match` matches everything. An update no rule matches is dropped and counted in `tgin_routes_unmatched_total`; `tgin config validate` warns when a bot's last rule has a `match`.
- **Targets** (`to`). Every entry of a rule's `to` receives the update. An entry is a consumer name, `{split: {strategy, among, weights?}}` (exactly one child receives it: `round_robin`, `hash_chat` — sticky by chat, else by user, else round robin — or smooth `weighted` round robin) or `{all: […]}` (every child receives it). Targets nest. A consumer reached twice through one rule's targets receives the update once.

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
| `bots` | Bots with `name`, `token`, `api_url`, `ingress`, `routes` | `api_url: https://api.telegram.org` |
| `consumers` | Pull and push consumers by name | see below |

Names of bots and consumers are 1 to 63 lowercase letters, digits and `-`, starting with a letter or digit.

**Long poll** (`ingress.long_poll`): `timeout` (`30s`, at least `1s`, at most 50 s is sent), `limit` (`100`, 1 to 100), `allowed_updates`, `delete_webhook_on_start` (`true`). tgin sends `allowed_updates` with every `getUpdates`: the configured list, or `[]` when it is omitted, which is Telegram's default (every update type except `chat_member`, `message_reaction` and `message_reaction_count`). A list Telegram stored for the token from an earlier client never decides what arrives; list those three types to receive them.

**Webhook** (`ingress.webhook`): `path` (default `/webhook/<bot name>`), `public_url` (the public `https` base URL; `setWebhook` registers `public_url` followed by `path`), `register` (`false`; `true` calls `setWebhook` when the bot's source starts and needs `public_url`), `secret_token` (`auto`; a configured secret is 1 to 256 ASCII letters, digits, `_` or `-`, as Telegram requires), `ip_allowlist` (`none`), and Telegram's `allowed_updates` (sent with every `setWebhook`, `[]` when omitted, as for long polls), `max_connections`, `drop_pending_updates`, `ip_address` and `certificate_file` for `setWebhook`.

**Tokens and paths.** A bot token may use only ASCII letters, digits, `:`, `_` and `-`; a token with anything else (a space, quotes, a second trailing newline in a token file) is a config error, at start and on reload alike. Webhook paths and pull aliases are matched byte for byte, so they must reach tgin unchanged inside a URL: no `?`, `#`, spaces, control or non-ASCII characters (write them percent-encoded), no `.` or `..` segments and no `\`.

**Pull consumer** (`pull`): `capacity` (`10000`), `capacity_bytes` (`64MiB`, at least `1MiB`), `on_full` (`backpressure`), `lease_timeout` (`2m`, at least `1s`: leases are expired by a check that runs once a second), `ordering` (`none`), `auth_token`, `aliases`.

**Push consumer** (`push`): `endpoints` (each `url`, `secret_token`, `timeout` `10s`; at least one, unless `accept_set_webhook: true`), `balance` (`round_robin`, `hash_chat` or `least_in_flight`), `max_in_flight` (`32`), `ordering` (`none`), `capacity` (`10000`), `capacity_bytes` (`64MiB`), `on_full` (`backpressure`), `retry` (`initial` `1s`, `max` `1m`, `max_attempts` `10`, `max_age` `1h`), `breaker` (`failure_threshold` `5`, `open_for` `30s`). An endpoint `timeout`, `retry.initial`, `retry.max`, `retry.max_age`, `breaker.open_for` and `server.header_read_timeout` of 0 would disable or break what they configure, and are config errors; `runtime.lame_duck`, `runtime.shutdown_grace` and `reload.drain_timeout` may be 0 (no lame duck, no drain).

`max_in_flight` counts deliveries in progress over all of a consumer's endpoints, retries waiting included. With downstreams that handle requests concurrently, the default, 32, delivered every update at every webhook point of the benchmarks. A downstream that handles one request at a time (a single bot process) can deliver more with a lower value: in benchmarks from an earlier session, before the load generator's 100-connection cap, one aiogram process offered 5 000 updates/s took 1 809 updates/s with 32 and 2 268–2 280 with 8 ([PERF.md → `max_in_flight`](PERF.md#max_in_flight)).

**Runtime endpoint.** A push consumer with `accept_set_webhook: true` (allowed only on a consumer fed by one bot) takes an endpoint from that bot's `setWebhook` on the consumer's gateway path, `/c/<consumer>/bot<token>/setWebhook`: an `http` or `https` `url` (`https` when it has no scheme, as on Telegram) with an optional `secret_token`, sent as `X-Telegram-Bot-Api-Secret-Token`. Whoever holds the bot's token then chooses where tgin posts the consumer's updates, from inside your network, so enable it only for trusted downstreams; tgin refuses its own pushes (see [Push consumers](#push-consumers-the-requests-tgin-sends)), so the endpoint cannot be tgin itself. It sits after the configured endpoints, with its own breaker (the consumer's `breaker`) and the default 10 s timeout; `setWebhook` with an empty `url` or `deleteWebhook` removes it. It survives a reload that keeps the consumer, `accept_set_webhook: true`, the bot feeding it and that bot's token; a reload that changes any of those clears it (logged), so rotating a leaked token evicts what its holder set, and a restart forgets it. A `setWebhook` or `deleteWebhook` still in flight when its token is rotated changes nothing and answers `401 Unauthorized`; the bodies of these requests (and of a consumer-scoped `getUpdates`) must arrive within the bot's `gateway.upstream_idle_timeout`, or the request gets `408 Request Timeout`. A push consumer with no endpoint at all leases nothing: its updates wait in the queue, bounded by `capacity` and `on_full`, and `retry.max_age` still counts from admission. `drop_pending_updates=true` on `deleteWebhook` (or on a captured `setWebhook`) purges the calling consumer only, and only with `honor_drop_pending: true`.

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

- Parameters come from the query string and, when there is a body, from a JSON (`Content-Type: application/json`) or form body, whose values win. `offset` (default 0), `limit` (default 100, clamped to 1 … 100) and `timeout` in seconds (default 0, clamped to 0 … 50) are read; `allowed_updates` and anything else is ignored (routing is the config's). `multipart/form-data` is refused.
- The answer is `{"ok":true,"result":[…]}` with the updates exactly as Telegram sent them. Errors look like the Bot API's: `404 {"ok":false,"error_code":404,"description":"Not Found"}` for an unknown consumer, `401 … "Unauthorized"` without the right token, `400 … "Bad Request: …"` for bad parameters.
- With `auth_token` set, a poll must send `Authorization: Bearer <auth_token>`.
- The polling **instance** is named by the `X-Tgin-Instance` header (1 to 64 visible ASCII characters) or, without it, by the client's IP address (from `X-Forwarded-For` only when the peer is in `server.trusted_proxies`). Leases and acknowledgements are per instance, so processes that share an address — behind NAT, or several on one host — must each send their own `X-Tgin-Instance`.

Most bot frameworks can be pointed at tgin by replacing Telegram's `getUpdates` URL with `http://<tgin>:8080/c/<consumer>/getUpdates`; [integrations/pytgin](integrations/pytgin) does this for aiogram.

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

Each request is bounded by the endpoint's `timeout` (default 10 s). The endpoint should answer `2xx` once it has taken the update; see [Delivery semantics](#delivery-semantics) for the other answers.

**tgin never acts on its own pushes.** Both planes answer `508 Loop Detected` to a request whose `Via` lists this process's pseudonym (RFC 9110 §7.6.3): the admin plane `{"error":"loop detected"}`, the data plane Telegram's shape, each with the process's own `Via` header. So an endpoint that points back at tgin (its webhook ingress, a pull endpoint, the gateway, or the admin API) is refused, and the pushing dispatcher, seeing its own `Via` on the `508`, dead-letters the update at once. The refusal is logged as a warning at most once a minute per plane, with the bot from `X-Tgin-Source`. Another tgin process has another pseudonym, so chained instances still pass updates to each other.

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
- `tasks[]`: every supervised task (long-poll sources, push dispatchers, background `getMe` calls) with the supervisor's view of it: its state and, when it is restarted after a panic or failed, its last error, secrets masked. A long poll that backs off by itself stays `running` here; see `sources[]`.

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

Apart from `tgin_build_info`'s, every label value is a bot or consumer name, an endpoint index or a fixed word, so no series carries a URL or a token. A bot's series go when a reload removes it; a consumer's when its drain ends.

## Logging

Logs go to stdout, as `text` or `json` lines (`log.format`, fixed at start). `log.level` sets the most verbose level and follows reloads; a non-empty `RUST_LOG` overrides it (its directives keep winning after a reload). Text logs are coloured only when stdout is a terminal and `NO_COLOR` is unset or empty. Every line passes through the redactor (see [Security](#security)), panics included.

## Security

**Secrets.** Write secrets as `${env:…}` or `${file:…}` references rather than literally, so the config file can be shared. `${file:…}` suits Docker and Kubernetes secrets mounted as files. An empty secret is an error. Config errors never quote a secret: a malformed reference in a secret or URL field is reported without its text, and YAML errors carry the path, line and column without the source line.

**Redaction.** Every log line is masked before it is written: each secret of every config loaded since start (bot tokens, webhook secrets, `admin.token`, pull `auth_token`s, push `secret_token`s) and anything shaped like a bot token is replaced by `#####`. Secrets shorter than 8 characters are not masked, as they would match ordinary words; use long random secrets. `/v1/status` and `/v1/config` redact as described under [Admin API v1](#admin-api-v1), and metric labels hold no URLs.

**Auth model.**

| Surface | Protection |
|---|---|
| Admin plane (`/v1/*`, `/metrics`) | `admin.token` as a bearer token; required unless every admin listener is on loopback. `/metrics` can be opened with `admin.public_metrics`. `/livez` and `/readyz` are always open. |
| Pull consumers | Optional `auth_token` per consumer, as a bearer token. Without it, anyone who can reach the data plane can take the consumer's updates. |
| Webhook ingress | `secret_token` (`auto` by default) checked before the body is read; optional `ip_allowlist`; `trusted_proxies` decides whose `X-Forwarded-For` counts. |
| Push endpoints | The endpoint's `secret_token` is sent as `X-Telegram-Bot-Api-Secret-Token`, for the downstream to check. |

Every token compare runs in constant time.

**TLS.** The data plane terminates TLS itself when `server.tls` is set (rustls, HTTP/1.1); the certificate files are read at start (a missing or bad file stops tgin with exit code 1) and again on every reload. The admin plane has no TLS: keep it on loopback or a private network, or put a TLS proxy in front, since the bearer token travels in clear text. Outgoing calls to the Bot API and to push endpoints verify server certificates against the platform's trust store, never follow redirects, and use the `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` environment if set (`tgin admin` never does).

**Telegram webhooks need HTTPS** on 443, 80, 88 or 8443 (Telegram's rule): put tgin behind a TLS-terminating proxy, or give it `server.tls` and a `listen` on one of those ports.

## Shutdown

`SIGTERM` or `SIGINT` (Ctrl+C, Ctrl+Break, closing the console or a system shutdown on Windows) starts the shutdown, as does every source failing for good. tgin then:

1. **Stops being ready.** `/readyz` answers `503`, reloads are refused.
2. **Lame duck** (`runtime.lame_duck`, 5 s; skipped without a `server` block). Everything keeps working, so load balancers can take tgin out of rotation.
3. **Stops ingest.** Long polls stop (updates not yet admitted are left to Telegram); webhooks answer `503` with `Retry-After: 5`.
4. **Drains** for up to `runtime.shutdown_grace` (15 s), until no update is pending: push consumers keep delivering, and pull consumers keep serving, a poll on an empty queue answering within 500 ms.
5. **Closes** the consumer tasks and the data plane, giving requests 1 s; tasks still running then are aborted. Updates still pending are lost: they are logged (`exiting with updates not delivered`) and counted in `tgin_shutdown_abandoned_total`.
6. **Closes the admin plane** last.

With the defaults the whole shutdown takes at most about 22 s; set the orchestrator's stop timeout to 30 s (Kubernetes `terminationGracePeriodSeconds: 30`, `docker run --stop-timeout 30`, Compose `stop_grace_period: 30s`; Docker's default of 10 s cuts the drain short).

A second `SIGTERM`/`SIGINT` (or Ctrl+C, Ctrl+Break) during the shutdown stops at once with exit code 130, losing what is pending. When the shutdown began because every source failed, the first signal is only logged and a second one forces the exit. A signal while tgin is still starting stops what has started and exits 0. Closing the console on Windows, which kills the process about 5 s later, skips the lame duck and caps the drain at 4 s.

**Every source failing.** A long-poll source fails for good when Telegram rejects its token (`401`, `403` or `404`). When every bot with an ingress uses long poll and all of their sources have failed, tgin shuts down as above and exits with code 2. A webhook bot never fails this way, so a config with one never exits 2.

## Deployment

**Rules that follow from the design.**

- **One tgin per long-poll token.** Telegram allows one `getUpdates` poller per token; a second gets `409 Conflict`, and tgin backs off 5 s and logs `another process is polling this bot token`. Replace a tgin that polls with stop-then-start (Kubernetes `strategy: Recreate`, `replicas: 1`), not a rolling update that briefly runs two.
- **Queues are per process.** A pull client must poll the tgin that holds its consumer's updates.
- **Downstream bots must not manage the webhook.** Bots behind tgin usually share its token to send messages. A framework that calls `deleteWebhook` or `setWebhook` on that token (aiogram's `delete_webhook(drop_pending_updates=True)`, the polling start-up of python-telegram-bot and grammY) can remove tgin's webhook or drop the updates Telegram holds for it.

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
- [examples/simple](examples/simple) — a Compose stack with pull and push consumers.
- [integrations/pytgin](integrations/pytgin) — an aiogram shim that points `getUpdates` at tgin.
