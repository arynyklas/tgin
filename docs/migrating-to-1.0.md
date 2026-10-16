# Migrating from tgin 0.2 to 1.0

tgin 1.0 is a rewrite. The config file is YAML instead of RON, the routing tree became rules and named consumers, the HTTP endpoints split into a data plane and an admin plane, and the metrics were renamed. This guide lists every change an operator of 0.2 has to act on. The manual is [DOCS.md](../DOCS.md); the config keys are in [config-reference.md](config-reference.md).

## Contents

- [At a glance](#at-a-glance)
- [Convert the config: `tgin config migrate`](#convert-the-config-tgin-config-migrate)
- [What migrate keeps from 0.2 on purpose](#what-migrate-keeps-from-02-on-purpose)
- [Ports, paths and probes](#ports-paths-and-probes)
- [Delivery semantics](#delivery-semantics)
- [Auth model](#auth-model)
- [Metrics](#metrics)
- [Command line and exit codes](#command-line-and-exit-codes)
- [Docker and Kubernetes](#docker-and-kubernetes)

## At a glance

| tgin 0.2 | tgin 1.0 |
|---|---|
| `tgin -f tgin.ron` (RON, `${VAR}` substituted everywhere in the file) | `tgin -c tgin.yaml`, `TGIN_CONFIG`, or `tgin.yaml` in the working directory (YAML; `${env:VAR}` and `${file:PATH}` only in secrets, URLs and paths) |
| `updates: [LongPollUpdate(…), WebhookUpdate(…)]` | `bots:` with a `name`, `token` and `ingress` each |
| `route:` tree of `RoundRobinLB`, `AllLB`, `LongPollRoute`, `WebhookRoute` | per-bot `routes:` rules whose `to:` targets name `consumers`; `split` and `all` replace the load balancers |
| `LongPollRoute(path: …)` | a `pull` consumer served on `/c/<name>/getUpdates` and on its `aliases` |
| `WebhookRoute(url: …)` | a `push` consumer with `endpoints` |
| one port (`server_port`) for everything | data plane `server.listen` (default `0.0.0.0:8080`) and admin plane `admin.listen` (default `127.0.0.1:9090`) |
| `/status`, `/metrics`, `/livez`, `/readyz` on the data port | `/v1/status`, `/metrics`, `/livez`, `/readyz` on the admin plane |
| `api` (add and remove routes over HTTP) | removed; edit the config file, which tgin reloads on change |
| `auth_token` | `admin.token`, plus an optional `auth_token` per pull consumer |
| `dark_threads` | `runtime.worker_threads` |
| `log_level`, `log_format` (`Compact`, `Json`) | `log.level`, `log.format` (`text`, `json`) |

## Convert the config: `tgin config migrate`

```bash
tgin config migrate tgin.ron -o tgin.yaml     # or without -o/--out: YAML on stdout
tgin config validate tgin.yaml
```

- It never reads the environment: each 0.2 placeholder `${VAR}` becomes the reference `${env:VAR}`, and literal values are copied as written. A placeholder outside a string (0.2 substituted it into the RON text) is an error, as 1.0 expands references only inside strings.
- Warnings go to stderr and into a comment at the top of the YAML; errors and warnings name fields and positions, never config values.
- Exit codes: **0** converted; **3** converted, with warnings to review; **1** not converted (the file cannot be read or parsed, or it uses something 1.0 cannot express; nothing is written); **64** malformed command line.

What it produces, for each part of a 0.2 config:

- `dark_threads` → `runtime.worker_threads`; `log_level`/`log_format` → `log` (`off`, which 1.0 lacks, becomes `error` with a warning).
- `server_port: Some(P)` → `server.listen: ["0.0.0.0:P"]`, so the data plane keeps your port; `ssl` → `server.tls`.
- `auth_token` → `admin.token`; `api` is dropped with a warning.
- Each entry of `updates` → a bot named `bot-1`, `bot-2`, …:
  - `LongPollUpdate` → `ingress.long_poll` (`long_poll_timeout` and `long_poll_limit` carried over, a timeout of 0 becoming `1s` with a warning; `default_timeout_sleep` and `error_timeout_sleep` dropped with a warning). A custom `url` becomes `api_url`, and must end in `/bot<token>/getUpdates` with the bot's token.
  - `WebhookUpdate` → `ingress.webhook` with the same `path`. With `registration`, `public_url` (from `public_ip`) and `register: true`, and the bot token from `set_webhook_url` or else `registration.token`; a `set_webhook_url` that is not `<api>/bot<token>/setWebhook` is an error, since 1.0 builds that URL from `api_url`. Without `registration` the 0.2 config has no token for the bot, so migrate writes `${env:TGIN_BOT_<n>_TOKEN}` and warns.
- The `route` tree is copied into each bot as one rule, `routes: [{to: [<tree>]}]`:
  - `RoundRobinLB` → `split` with `strategy: round_robin`; `AllLB` → `all`;
  - `LongPollRoute` → a pull consumer named `pull-<path>` (letters and digits of the path, lowercased, other runs as `-`), with `capacity` from `max_buffered_updates`, and the 0.2 path as an alias;
  - `WebhookRoute` → a push consumer named `push-<host>[-<port>]` after the URL's host and port only, never its path (a secret path segment would otherwise show up in logs, metrics and `/v1/status`). A `request_timeout_ms` other than 10 000 becomes the endpoint `timeout`.
  - A path that holds a placeholder or a bot token, or a URL that holds a placeholder, names its consumer by number instead (`pull-<n>`, `push-<n>`), and a name already taken gets `-2`, `-3`, … appended.
  - In 1.0 a pull consumer serves one bot, so a config with several `updates` whose tree contains a `LongPollRoute` is not converted: migrate one bot at a time and merge the results with distinct consumer names.

The repository's 0.2 example converts to:

```yaml
# Converted from a tgin 0.2 config by `tgin config migrate`.
# Warnings:
# - `api` was dropped: tgin 1.0 has no route-management API; edit the routes in this file instead, which tgin reloads on change

version: 1
runtime:
  worker_threads: 6
log:
  level: info
  format: text
server:
  listen:
    - "0.0.0.0:3000"
admin:
  # tgin 0.2's auth_token. The status and metrics endpoints moved from the data port to admin.listen (default 127.0.0.1:9090).
  token: "${env:TGIN_AUTH_TOKEN}"
bots:
  - name: bot-1
    token: "${env:TOKEN}"
    ingress:
      long_poll: {}
    routes:
      - to:
          - split:
              strategy: round_robin
              among:
                - pull-bot1-getupdates
                - push-1
                - pull-bot2-getupdates
consumers:
  pull-bot1-getupdates:
    pull:
      capacity: 10000
      on_full: drop_oldest
      aliases:
        - "/bot1/getUpdates"
  push-1:
    push:
      endpoints:
        - url: "${env:WH_ENDPOINT}"
      # tgin 0.2 made one delivery attempt and dropped what its downstream did not take, never pausing the source; delete on_full and retry for tgin 1.0's backpressure and retries.
      on_full: drop_oldest
      retry:
        max_attempts: 1
  pull-bot2-getupdates:
    pull:
      capacity: 10000
      on_full: drop_oldest
      aliases:
        - "/bot2/getUpdates"
```

## What migrate keeps from 0.2 on purpose

A converted config behaves like 0.2 where the defaults of 1.0 would change what reaches your bots. Each of these is a line you can delete to get the 1.0 behaviour:

- **Push consumers get `on_full: drop_oldest` and `retry: {max_attempts: 1}`.** tgin 0.2 made one delivery attempt per webhook route and never held back its source for a failed downstream. With the 1.0 defaults (`backpressure`, 10 attempts within an hour), a downstream that is down fills its queue and then stalls its bot's source, and with it every other consumer of that bot. Delete both lines for retries with backoff and backpressure.
- **Pull consumers get `on_full: drop_oldest`**, as 0.2's long-poll buffers evicted their oldest updates when full. Delete it to make a full queue hold back the source instead.
- **A 0.2 webhook without a secret becomes `secret_token: none`**, with a warning: anyone who can reach the path can post updates. Delete the line to use `auto` (a secret derived from the bot token, sent to Telegram by `register: true`), or set your own.
- **Relative TLS paths** are copied as written, with a warning: 0.2 resolved them against its working directory, 1.0 against the YAML file's directory. Make them absolute or move the files.

## Ports, paths and probes

- **The data plane** (webhooks and pull consumers) listens on `server.listen`: `0.0.0.0:8080` when the `server` block names no address, your old port in a migrated config. Without a `server` block there is no data plane.
- **The admin plane** listens on `admin.listen`, `127.0.0.1:9090` by default. `/livez`, `/readyz` and `/metrics` moved there, and `/status` became `/v1/status` with a new shape ([DOCS.md → Admin API v1](../DOCS.md#admin-api-v1)). A non-loopback `admin.listen` requires `admin.token`.
- **Kubernetes probes** cannot reach `127.0.0.1` from outside the pod: either set `admin.listen: ["0.0.0.0:9090"]` with `admin.token` and probe `/livez` and `/readyz` on port 9090 (they need no token), or keep the default and use exec probes running `tgin admin health`. See [DOCS.md → Kubernetes](../DOCS.md#kubernetes).
- **0.2 pull paths keep working.** Paths such as `/bot1/getUpdates` become `aliases` of the pull consumer, next to the new `/c/<name>/getUpdates`. A configured path may not be `/c`, `/v1`, `/mtproto`, `/livez`, `/readyz` or `/metrics`, or lie below one of them (whole segments only: `/metricsbot/hook` is free), and may not start with a bot-token-shaped segment (`/bot<digits>:…` or `/file/bot<digits>:…`), which tgin reserves; other `/bot…` paths are free. `tgin config migrate` reports a 0.2 path that starts with a literal token as an error, and warns about `bot` followed by a placeholder, since it does not read the environment.
- **Paths are matched as sent.** A webhook path or alias with `?`, `#`, spaces, control or non-ASCII characters, `.`/`..` segments or `\` is rejected: a URL would change it and every request would miss the route.
- **Relative file paths** in the config (TLS certificate and key, the webhook `certificate_file`, `${file:…}` references) resolve against the config file's directory, not the working directory.

## Delivery semantics

- **Pull consumers lease and acknowledge.** A 0.2 long-poll route removed an update from its buffer when it handed it out. In 1.0 a poll leases updates to the polling instance, the next poll's `offset` acknowledges them, a re-poll gets back what the instance still holds, and a lease not acknowledged within `lease_timeout` (2 minutes) goes back to the queue for another instance. Clients must send `offset` as with Telegram (frameworks do): a client that always sends `offset=0` receives the same updates again and again. Instances that share an IP address must send their own `X-Tgin-Instance` header ([DOCS.md → Pull consumers](../DOCS.md#pull-consumers-the-getupdates-endpoint)).
- **Push consumers retry.** A 0.2 webhook route made one attempt. 1.0 retries `408`, `429`, `5xx` and network errors with backoff, dead-letters other `4xx` and every `3xx` at once (it never follows redirects), and keeps a circuit breaker per endpoint ([DOCS.md → Delivery semantics](../DOCS.md#delivery-semantics)). Migrated push consumers keep 0.2's single attempt until you delete `retry`.
- **Queues are bounded** per consumer, in updates and bytes. With `on_full: backpressure` (the 1.0 default) a full queue holds back its bot's source; with `drop_oldest` it drops its oldest updates. An update larger than a consumer's `capacity_bytes` is dropped for that consumer.
- **`all` is independent per consumer.** 0.2's `AllLB` waited for every child; in 1.0 each consumer of an `all` gets its own copy in its own queue.
- **Round-robin splits keep a position per bot.** A migrated config copies the 0.2 tree into each bot's rule, so each bot's split takes turns on its own; 0.2 had one position shared by every bot's updates.
- **Webhooks and long polls.** 0.2 deleted a webhook only after a `409 Conflict` on `getUpdates`. 1.0's `long_poll.delete_webhook_on_start: true` (the default) calls `deleteWebhook`, keeping pending updates, before the first poll and whenever a poll finds a webhook set. Set it to `false` to leave the webhook alone; the source then waits until something else deletes it.
- **`allowed_updates` is always sent.** 0.2 never sent it with `getUpdates` or `setWebhook`, so a bot received whatever list Telegram had stored for its token from an earlier client (an aiogram `allowed_updates=…`, say). 1.0 sends the configured list, or `[]` when the config has none, which is Telegram's default: every type except `chat_member`, `message_reaction` and `message_reaction_count`. A migrated bot that relied on a stored list that included those types gets them again only once you list them under `allowed_updates`; one whose stored list was narrower now receives every default type, which its rules route or drop.
- **Bot tokens and webhook secrets are checked at load.** A token with anything but ASCII letters, digits, `:`, `_` and `-` (a stray space or quotes, an extra newline in a token file) and a webhook `secret_token` outside Telegram's `[A-Za-z0-9_-]{1,256}` are config errors.
- **Ordering.** `ordering: per_chat` (new) delivers one update per chat at a time.

## Auth model

| | tgin 0.2 | tgin 1.0 |
|---|---|---|
| Status and metrics | behind `auth_token` (when set) on the data port | behind `admin.token` on the admin plane; `/metrics` can be opened with `admin.public_metrics` |
| Management API | `api`, required `auth_token` | removed; the admin API v1 (`/v1/…`) needs `admin.token` unless every admin listener is on loopback |
| Probes | open | open (`/livez`, `/readyz` on the admin plane) |
| Pull clients | open | open, or `Authorization: Bearer <auth_token>` per consumer |
| Webhook ingress | optional `secret_token` | `secret_token: auto` by default, plus optional `ip_allowlist` and `trusted_proxies` |

`tgin admin` reads the admin token only from a file (`--token-file` or `TGIN_ADMIN_TOKEN_FILE`).

## Metrics

Labels changed with the names: 0.2 labelled series with redacted URLs and paths (`source`, `route`); 1.0 labels them with bot and consumer names, an endpoint's index, and fixed words.

| tgin 0.2 | tgin 1.0 | Notes |
|---|---|---|
| `tgin_updater_updates_received_total{source}` | `tgin_source_updates_total{source}` | `source` is the bot name. |
| `tgin_updater_poll_failures_total{source}` | `tgin_source_errors_total{source,reason}` | Sum over `reason`; 1.0 also counts refused and unreadable webhook requests. |
| `tgin_updater_permanent_failure{source}` | `tgin_source_state{source,state="failed"}` | 1 while failed. |
| `tgin_route_queue_depth{route}` | `tgin_consumer_queue_depth{consumer}` + `tgin_consumer_leased{consumer}` | 1.0 splits waiting and leased updates. |
| `tgin_route_queue_dropped_total{route}` | `tgin_consumer_dropped_total{consumer,reason="overflow"}` | Other reasons: `dead_letter`, `reload`, `purge`, `oversized`. |
| `tgin_route_served_total{route}` | — | 1.0 counts acknowledgements instead: `tgin_delivery_latency_seconds_count{consumer}`. |
| `tgin_route_dispatched_total{route}` | `tgin_delivery_attempts_total{consumer,endpoint,outcome}` | Sum over `outcome`; retries count as attempts. |
| `tgin_route_http_responses_total{route,result="success"}` | `tgin_delivery_attempts_total{…,outcome="delivered"}` | |
| `tgin_route_http_responses_total{route,result="failure"\|"error"}` | `tgin_delivery_attempts_total{…,outcome="retry"\|"dead_letter"}` | 1.0 tells transient from permanent failures, not responses from network errors. |
| `tgin_lb_dropped_empty_total` | — | 1.0 has no runtime route removal; updates no rule matches are `tgin_routes_unmatched_total{source}`. |

New in 1.0: `tgin_build_info`, `tgin_source_dropped_total`, `tgin_consumer_in_flight`, `tgin_consumer_backpressure_seconds_total`, `tgin_delivery_latency_seconds`, `tgin_config_reloads_total`, `tgin_config_generation` and `tgin_shutdown_abandoned_total` ([DOCS.md → Metrics](../DOCS.md#metrics)).

## Command line and exit codes

- `tgin -f tgin.ron` exits 1 and prints `tgin 1.0 reads YAML; convert with: tgin config migrate tgin.ron -o tgin.yaml`. Use `tgin -c tgin.yaml` (or `tgin run -c …`, or `TGIN_CONFIG`).
- New subcommands: `tgin config validate|print|schema|migrate`, `tgin admin …`, `tgin version`.
- **Exit code 64** (new) is a malformed command line; 0.2 exited 2 for it, as clap does by default. Code 2 now means only that every source failed for good: Telegram rejected the token of every long-poll bot, and there is no webhook bot. In 0.2 it meant that at least one long-poll ingress had failed.
- **Exit code 3** exists only for `tgin config migrate`: converted with warnings.
- 0, 1 and 130 keep their meaning: clean shutdown, start-up failure, second signal during the drain.

## Docker and Kubernetes

- **Config path.** The image reads `TGIN_CONFIG=/etc/tgin/tgin.yaml`. The 0.2 default arguments `-f /etc/tgin/config.ron` are gone: mount the converted YAML at `/etc/tgin/tgin.yaml`, or pass `-c <path>`. An old `docker run IMAGE -f …` now exits 1 with the migrate hint.
- **Ports.** The image declares `8080` and `9090` instead of `3000`. A migrated config keeps its old data port; publish whatever `server.listen` says.
- **Health check.** The image has a `HEALTHCHECK` (0.2 had none, since the image has no shell): exec-form `tgin admin health`, which asks `/readyz` on `http://127.0.0.1:9090`. Set `TGIN_ADMIN_URL` if `admin.listen` moves the admin plane.
- **Admin token.** Mount it as a secret file, reference it as `admin.token: ${file:/run/secrets/…}`, and set `TGIN_ADMIN_TOKEN_FILE` to the same file, so `docker exec <container> tgin admin status` works ([DOCS.md → Docker](../DOCS.md#docker)).
- **Kubernetes.** Move HTTP probes from the data port to the admin plane, or switch to exec probes (see [Ports, paths and probes](#ports-paths-and-probes)). Mount the config's ConfigMap as a directory, not with `subPath`, so hot reload sees changes. Keep `terminationGracePeriodSeconds: 30` and, for long-poll bots, `replicas: 1` with `strategy: Recreate`.
