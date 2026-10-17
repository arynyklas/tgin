# Repository Guidelines

`tgin` routes Telegram Bot API updates to downstream bot processes ("NGINX for Telegram bots"). One binary reads a YAML config (`version: 1`), ingests each bot's updates by long poll or webhook, routes every update by the bot's rules into named consumers, and serves them: **pull** consumers through a Telegram-shaped `getUpdates` endpoint, **push** consumers by POSTing to HTTP endpoints. The data plane also serves the Bot API itself to the downstream bots (the **gateway**): it proxies their calls to Telegram under per-bot rate limits and answers locally the calls that would disturb tgin. `README.md` and `DOCS.md` are the user documentation; `DOCS.md` is the reference for runtime behaviour (delivery semantics, the gateway, reload, admin API, metrics, shutdown, exit codes), and `docs/config-reference.md` is generated from the config model.

## Architecture & Data Flow

```
Telegram ─► source (long poll task, or webhook route on the data plane)
              │ extract facts once (update type, chat, user, text, command)
              ▼
           Engine::admit ─► RouteTable (rules, compiled) ─► reserve in every target queue ─► commit
                                                                   │
                         pull: LeaseQueue ◄─ getUpdates polls ─────┤
                         push: LeaseQueue ─► dispatcher ─► HTTP endpoints (retries, breakers)
```

1. **Sources** (`crates/tgin-botapi`): `LongPollSource` polls `getUpdates` and offers each update to a `Sink`, moving its offset past an update only once it is admitted; on backpressure it holds the batch. `webhook_router` serves one `POST <path>` per webhook bot, checks allowlist and secret on the head alone, and answers `503` + `Retry-After` on backpressure. `facts::extract` reads the routing facts from the raw JSON without building a `serde_json::Value`. `pull.rs` serves `getUpdates` from a pull consumer's queue (for `data_server.rs`'s routes and the gateway's `/c/<consumer>/bot<token>/getUpdates`); `gateway/intercept.rs` answers `getUpdates`, `getMe`, `setWebhook`, `deleteWebhook`, `getWebhookInfo`, `logOut` and `close` before the proxy, so none reaches Telegram as a downstream sent it.
2. **Engine** (`crates/tgin-core/src/engine.rs`, `admission.rs`): `Sink::admit` routes with the current `RouteTable` (swapped atomically on reload) and reserves room in every target (`backpressure` targets first, then `drop_oldest`), then commits; one queue lock at a time, nothing awaits. `Engine::set_runtime_endpoint` gives a push consumer with `accept_set_webhook` the endpoint its bot's `setWebhook` named (serialized with reloads, which clear it once that bot no longer feeds the consumer); a push consumer with no endpoint at all leases nothing.
3. **Consumers** (`crates/tgin-core/src/consumer/`): `PullConsumer::poll` acks by offset, returns held leases, else leases new updates and parks until the timeout; `PushConsumer` runs one dispatcher per consumer that leases up to `max_in_flight` and delivers with retries, per-endpoint breakers and balancing. With `webhook_reply: execute`, a delivery reads a method call from a 2xx answer (`crates/tgin-core/src/reply.rs`, at most 1 MiB) and awaits the `ReplyExecutor` before it acks the update; the executor is late-bound through `EngineRuntime.reply`, which the app sets to the gateway once it is built.
4. **App** (`crates/tgin/src`): `app.rs` assembles the engine, the gateway, the admin plane, the data plane and the sources, and runs the shutdown sequence; it also supervises `gateway-sweep`, which every 60 s drops the rate-limit keys back to a fresh key's state; `data_server.rs` serves webhook routes and `/c/{consumer}/getUpdates` plus aliases through an `ArcSwap<Router>` that reloads replace, with the Bot API gateway (`tgin_botapi::gateway::Gateway`) as every router's fallback; `admin/` is the admin plane (probes, `/metrics`, `/v1/*`) and `tgin admin`; `reload.rs` serializes reloads (SIGHUP, `POST /v1/reload`, file watcher) and applies them through `Engine::apply` (`crates/tgin-core/src/reload.rs`), then `Gateway::set_bots`, then the sources; `sources.rs` starts, stops and restarts bot sources and records each `getMe` identity in the gateway.

### Bot API gateway

`tgin_botapi::gateway::Gateway` is every data-plane router's fallback (`data_server.rs::router`), so configured webhook paths, `/c/{consumer}/getUpdates` and aliases win. A request flows through `Gateway::handle` (`gateway/mod.rs`): `path::parse` (no gateway path → Telegram's JSON 404) → `Table::find` (`secret_eq` against every configured token, no early exit; unknown → 401) → `Engine::routes_to` for `/c/<consumer>/…` → GET/POST only → `intercept::Call::route` (the locally answered methods) → `proxy::forward`. The app builds the gateway after the engine and before the sources; `Gateway::set_bots` runs at start and on every reload after `Engine::apply`, and every `getMe` site in `sources.rs` calls `Gateway::set_identity`.

| File (`crates/tgin-botapi/src/gateway/`) | Role |
| --- | --- |
| `mod.rs` | `Gateway`, `GatewayBot`, the bot table (`ArcSwap<Table>`, swapped whole by `set_bots`; an entry keeps its client, identity and limiter while its token and settings stay), `read_bounded` (the one bounded body reader). |
| `path.rs` | `GatewayPath::parse`: `[/c/<consumer>]/bot<token>/<method>` and `[/c/<consumer>]/file/bot<token>/<file_path>`, the token percent-decoded once and checked with `tgin_core::config::gateway_token` (the single token grammar). |
| `methods.rs` | The Bot API method table (`METHODS`, sorted by lowercased name; `lookup` ignores ASCII case) and `MethodSet` (`send*`-style patterns). |
| `intercept.rs` | `getUpdates` (scoped: `pull::poll` on the bot's pull consumer; bot path: 409), `getMe` (cached identity, else proxied and learned), `setWebhook`/`deleteWebhook` (runtime endpoint through `Engine::set_runtime_endpoint`, `drop_pending_updates` through `Engine::drop_pending`, both bound to the authenticating token), `getWebhookInfo`, `logOut`, `close`; descriptions and parameter order from telegram-bot-api's `Client.cpp`. |
| `proxy.rs` | The streaming proxy: header allowlists both ways, `Upload`/`Download` wrappers that keep `Content-Length` and stamp progress (`upstream_idle_timeout` = no progress in either direction), `prepare` (buffers a covered method's JSON/form body ≤ 64 KiB and reads its chat), `Call::admit` (the limits), `flood` (the one resend after a 429), `Tally` (counts each request exactly once). |
| `limit.rs` | `BotLimiter`: global, per-chat and per-group GCRA quotas over `tgin_runtime::limit`, booked all or none; `sweep_limiters` for the app's `gateway-sweep` task. |
| `reply.rs` | The webhook-reply executor (`impl ReplyExecutor`, installed through `Gateway::reply_executor`, a `Weak<Gateway>` so the engine never keeps the gateway alive): Telegram's refusal rule plus the 64-byte `[A-Za-z0-9_]` method rule, the bot's limits, the body posted verbatim once with a 30 s bound. |
| `params.rs`, `chat_key.rs` | Request parameters (query, JSON top-level scalars, urlencoded, multipart text fields via multer) and the rate-limit chat key; the only parameter readers on the gateway path. |
| `errors.rs` | Telegram-shaped answers written by hand in Telegram's key order (serde_json would sort the keys). |
| `metrics.rs` | Every gateway metric family, registered once at start; per-bot handles, method handles resolved on first use and indexed by method. |

Rules on the gateway path: never log, trace or report a request path or upstream URL (both carry the token; log the bot and the method label); read a body whole only where one of these needs it, with its bound: covered send bodies ≤ 64 KiB (`proxy.rs`), Telegram's 429 bodies ≤ 64 KiB (`proxy.rs`), intercepted-method parameters ≤ 64 KiB and the `getMe` answer that fills the identity cache ≤ 64 KiB (`intercept.rs`), Telegram's error answer to a failed webhook reply ≤ 64 KiB (`reply.rs`), and webhook replies ≤ 1 MiB. The gateway reads its bodies with `read_bounded`; webhook replies are read in tgin-core's push consumer (`PushConsumer::reply`, `MAX_REPLY_BYTES` in `crates/tgin-core/src/reply.rs`). Every answer tgin writes goes through `errors.rs`; compare methods ignoring ASCII case and label metrics with the table's canonical name. `crates/tgin/tests/gateway_bounded.rs` pins the constant memory of 200 MiB uploads and downloads.

**Two client profiles.** Every outbound client comes from `tgin_runtime::http::client` (no redirects; environment proxies unless `no_proxy`). The **control profile** (`ClientOptions::default()`: 5 s connect, 10 s total) is the app's shared client: the sources' Bot API calls (a long poll extends its request's timeout to the poll's `timeout` plus a grace), `setWebhook` registration and push deliveries (each endpoint's `timeout` per request). `tgin admin` builds its own with `no_proxy` and its 60 s or 2 s bound. The **gateway profile** (one client per gateway bot: 5 s connect, `request_timeout: None`) has no total timeout, because a total timeout would cut slow uploads and downloads that are still moving; the proxy bounds idleness itself, and webhook-reply calls set a 30 s timeout per request. Do not add a reqwest `read_timeout` to the gateway profile: in reqwest 0.13 it is one fixed timer until the response headers, so it would cut an upload that is still flowing.

**Maintaining the method table on a Bot API release.** Compare `METHODS` in `gateway/methods.rs` with the method list at <https://core.telegram.org/bots/api>: add and remove names keeping the order by ASCII-lowercased name, update the array length, the count in `method_table_sorted_unique`, the version in the table's doc comment and DOCS.md's "tgin knows the methods of Bot API …" sentence. Add new methods that send a message to `send_methods_present`, and check that the default `rate_limit.methods` (`crates/tgin-core/src/config/defaults.rs`, `send*`, `copyMessage*`, `forwardMessage*`) still covers them. A new method telegram-bot-api answers itself (its `Client.cpp` local methods), or one that would disturb tgin or another downstream, belongs in `intercept.rs`. A method missing from the table still works: it is forwarded with the metric label `other`, and only known methods are intercepted. The fake (`tests/e2e/fake-telegram/src/api.rs`) answers unknown methods 404; extend it when a scenario needs a new one.

## Workspace Layout

| Path | Role |
| --- | --- |
| `Cargo.toml` | Virtual workspace: `[workspace.package]` (version `1.0.0-dev`, edition 2024, `rust-version = "1.89"`), shared `[workspace.dependencies]`, `[workspace.lints]` (`unsafe_code = "deny"`, clippy `all`), `[profile.release]` (`lto = true`, `codegen-units = 1`) and `[profile.dist]`. `tests/performance/benchmark` is excluded (own `Cargo.lock`). No `default-members`: bare `cargo test` tests every crate. |
| `crates/tgin-runtime/` | Generic building blocks with no tgin domain knowledge: `queue` (`LeaseQueue`: bounded, leases, acks, per-key ordering), `supervisor` (restarting tasks), `lifecycle` (phases, signals, shutdown timing), `http` (hardened HTTP/1.1 server with TLS reload, outbound client, URL-free error rendering), `obs` (tracing with a redacting writer, metrics registry, readiness), `config` (YAML parsing with locations, `${env:…}`/`${file:…}` interpolation, file watcher), `secret` (`Secret`, `Redactor`, constant-time compare), `limit` (`KeyedLimiter`: sharded keyed GCRA that books future slots, with exact `cancel`), `backoff`, `breaker`. |
| `crates/tgin-core/` | The engine: `config/` (YAML model, validation, resolution, JSON Schema), `rules/` (compiled route table), `engine.rs` + `admission.rs`, `consumer/{pull,push}.rs`, `reload.rs` (config diff and live apply), `metrics.rs` (every metric family and pre-resolved handles). |
| `crates/tgin-botapi/` | Telegram side: `client.rs` (Bot API client), `facts.rs` (fact extraction from raw JSON), `longpoll.rs`, `webhook.rs`, `pull.rs` (`getUpdates` on a pull consumer, for the data plane and the gateway), `secret.rs` (derived webhook secret), `gateway/` (the Bot API gateway, see [Bot API gateway](#bot-api-gateway)); `benches/` and `tests/alloc_budget.rs` (hot-path gates). |
| `crates/tgin/` | The `tgin` binary (dist builds it via `[package.metadata.dist] dist = true`): `cli.rs`, `app.rs`, `data_server.rs`, `admin/`, `reload.rs`, `sources.rs`, `exit.rs`. `main.rs` declares mimalloc as the global allocator on musl only. |
| `xtask/` | `cargo xtask schema [--check]` writes `schema/tgin.v1.schema.json`; `cargo xtask config-docs [--check]` writes `docs/config-reference.md` from that schema (`.cargo/config.toml` holds the alias). |
| `schema/tgin.v1.schema.json`, `docs/config-reference.md` | Generated from the doc comments of `crates/tgin-core/src/config/model.rs` (and the hand-written schemas in `config/schema.rs`). Never edit them: change the source, then run both xtask commands. The tests `schema_is_up_to_date` (tgin-core) and `config_reference_is_up_to_date` (xtask), and CI, check them. |
| `tgin.yaml` | Root example config (one long-poll bot, one pull consumer; reads `${env:TOKEN}`). |
| `Dockerfile` | Builder `rust:1.99.0-alpine3.24`, runtime `gcr.io/distroless/static-debian13`, both pinned by digest; `cargo build --release --locked -p tgin` (static musl), `TGIN_COMMIT` from the `REVISION` build arg; uid/gid `65532:65532`; `EXPOSE 8080 9090`; `ENV TGIN_CONFIG=/etc/tgin/tgin.yaml`; exec-form `HEALTHCHECK` running `tgin admin health`; `ENTRYPOINT ["/usr/local/bin/tgin"]`, `CMD []`. |
| `examples/simple/` | Compose stack: one bot, two pull consumers polled by aiogram bots and one push consumer whose aiogram bot answers with webhook replies; every bot uses the gateway base URL `http://tgin:8080/c/<consumer>`. |
| `tests/e2e/` | The end-to-end suite (`cargo xtask e2e`, Docker): `compose.yaml` runs the fake, tgin (`tgin.yaml`: eight long-poll bots, one consumer each) and eight bots, a polling and a webhook one per framework, from `bots/python` (aiogram, python-telegram-bot; uv) and `bots/node` (grammY, Telegraf; npm), all pinned. |
| `tests/e2e/fake-telegram/` | Crate `tgin-fake-telegram` (workspace member, lib + bin `fake-telegram`, own `Dockerfile` with the repo root as context): a fake Bot API with per-token state, long polling with telegram-bot-api's conflict rules, webhook delivery with replies, files and multipart, and a control API under `/fake/` (`POST /fake/updates/{token}` injects an update, `GET /fake/calls?token=&method=&kind=` is the call journal, `POST /fake/faults` makes the next calls fail, `GET /fake/health`). |
| `tests/smoke/` | `real_telegram.py`: the manual real-Telegram check run before a release (Python stdlib; `real_telegram.yaml` is its tgin config template, `hook.py` its webhook downstream). Never in CI. |
| `tests/performance/` | Load-test harness (see Testing). `cfgs/v1/*.yaml` are the 1.0 scenario configs, `cfgs/upstream/` the original engine's configs for `ENGINE=upstream`. |

### Crate boundaries

Dependencies point one way: `tgin-runtime` ← `tgin-core` ← `tgin-botapi` ← `tgin` (and `xtask` → `tgin-core`; `tgin-fake-telegram` depends on no tgin crate, so the fake never shares code with what it tests). `tgin-runtime` knows nothing of Telegram or of the config model; `tgin-core` knows nothing of the Bot API's HTTP shapes (sources reach it only through the `Sink` trait and `Facts`); `tgin-botapi` does not know the app. Put code in the lowest crate whose concepts it needs; never add an upward dependency.

## Important Files

- `.github/workflows/ci.yml` — every check listed under [CI checklist](#ci-checklist).
- `.github/workflows/release.yml` — generated by dist (cargo-dist 0.32.0) from `dist-workspace.toml` (8 targets), with hand edits listed in its header. To regenerate, follow the steps in the `allow-dirty` comment in `dist-workspace.toml`, then re-apply the hand edits. Dry run: `dist plan --tag v1.0.0-dev` (the tag must match the package version). The release notes come from the matching `CHANGELOG.md` section; never write credential-shaped text there (e.g. `Bearer` followed by a word): the Actions runner masks it and drops any job output that contains it.
- `.github/workflows/image.yml` — on `v*` semver tags, builds amd64/arm64 images natively, publishes the multi-arch index to `ghcr.io/arynyklas/tgin` and attests it; `latest` goes only to the highest stable `vX.Y.Z`.
- `deny.toml` — cargo-deny: advisories, license allow-list, bans on `openssl`, `openssl-sys`, `native-tls`, `ring`; crates.io only.
- `crates/tgin-botapi/benches/` and `crates/tgin-botapi/tests/alloc_budget.rs` — hot-path benchmarks over the 1000 updates in `crates/tgin-botapi/tests/fixtures/updates.jsonl`, routed by `crates/tgin-botapi/tests/fixtures/hot_path.yaml` (shared setup in `crates/tgin-botapi/benches/common/mod.rs`). `hot_path.rs` is criterion (`cargo bench -p tgin-botapi -- --quick`). `hot_path_ir.rs` is gungraun under Callgrind (Linux, needs Valgrind and `gungraun-runner`), with a hard `Ir` limit per benchmark set to 1.5× the measured count. `alloc_budget.rs` is the allocation gate: the shared counting allocator (`crates/tgin-botapi/tests/support/counting_alloc.rs`, per-thread counter) asserting at most 2 allocations per update on the warm path, over `updates.jsonl` and over `updates_escaped.jsonl` (the same updates with every text, caption and callback data non-ASCII, `\u`-escaped as Telegram sends it). The gungraun limits are re-measured (Docker `rust:<version>` with `--security-opt seccomp=unconfined`) whenever the toolchain pinned in `bench-gate` changes: limits and toolchain move together.
- `DOCS.md`, `PERF.md`, `CHANGELOG.md`, `SECURITY.md` — user docs, benchmarks, release notes, security policy.

## Development Commands

```bash
# Build
cargo build --workspace                  # debug, every crate
cargo build --release -p tgin            # → target/release/tgin

# Run (reads ./tgin.yaml; or -c PATH / TGIN_CONFIG)
TOKEN=123456:ABC cargo run -p tgin
TOKEN=123456:ABC cargo run -p tgin -- config validate tgin.yaml
cargo run -p tgin -- admin status        # against a running tgin

# Tests
cargo test --workspace                   # every crate
cargo test -p tgin-core --test engine    # one integration test file
cargo test -p tgin-runtime queue::       # one module's unit tests

# Generated files
cargo xtask schema && cargo xtask config-docs

# Docker (mount the config's directory, readable by uid 65532; a single-file
# mount would never see an edit saved by rename)
docker build -t tgin .
docker run --rm -v "$PWD/config:/etc/tgin:ro" -e TOKEN=... -p 8080:8080 tgin

# Example stack
cd examples/simple && TOKEN=... TGIN_ADMIN_TOKEN=$(openssl rand -hex 32) docker compose up --build

# End-to-end suite (Docker; builds the images, runs the scenarios, tears down)
cargo xtask e2e            # --keep leaves the stack up

# Manual real-Telegram check before a release (a test bot's token; never in CI)
cargo build -p tgin && TGIN_SMOKE_TOKEN=... python tests/smoke/real_telegram.py

# Performance harness (ENGINE=v1 default, or upstream = the original engine at 4f0971a;
# BOT_IMPL=rust default, py = the aiogram bots)
cd tests/performance
make build && make image ENGINE=v1 && make image ENGINE=upstream
make webhook-tgin-3 RPS=10000 DURATION=30            # one scenario
ENGINE="upstream v1" ./benchmark.sh   # both engines, 6 scenarios × 3 rates × 3 runs (~70 min)
./rerun-invalid.sh results/<run_id>.csv              # rerun runs with lag_p99_ms > 0.5
```

There is no `rustfmt.toml`, `clippy.toml` or `rust-toolchain*` file: CI uses stock `rustfmt` and `clippy` on stable, warnings as errors; `bench-gate` pins its own toolchain.

## Code Conventions & Patterns

### Hot path

The ingest path (getUpdates body → `split_updates` → `extract` → `Engine::admit` → queue commit) is gated by `alloc_budget.rs` (≤ 2 allocations per update on both fixtures) and, with queue commit, lease and ack, by `bench-gate` (instruction counts). Egress (`data_server.rs::batch_response`, push delivery) is not gated. Along these paths:

- **Payloads stay `Bytes`.** An update is its raw JSON from ingress to egress, never re-serialized; a pull response is assembled from the payloads as body chunks without copying (`data_server.rs::batch_response`). Facts are extracted once, borrowing from the payload, or for escaped text and callback data from a decode buffer the caller reuses (one per `getUpdates` batch). Ingress copies each `getUpdates` body and each single-frame webhook body once, to its exact size, so queued updates never keep hyper's read buffers alive; the updates of one `getUpdates` body share it.
- **Metrics handles are pre-resolved.** `CoreMetrics::source(name)` / `::consumer(name)` resolve every label combination once; the hot path increments handles and never hashes labels. A metric has one owner (e.g. only `Engine::admit` counts `tgin_routes_unmatched_total`).
- **No awaits and one lock at a time in admission.** Reserve, then commit; cancel reservations on refusal.
- **Rules are compiled.** `RouteTable::compile` does the work at load time; `route` writes into reusable scratch and does not allocate.
- **Swap, don't lock, for config-derived state.** The engine state, route table, data-plane router, running config and redactor sit in `ArcSwap`s that reloads replace.

### Secrets and redaction

- Secrets are `Secret<T>` (never shown by `Debug`/`Display`/serde); config URLs and paths are `Sensitive<T>`, used through `.expose()` and rendered masked (URLs as their origin only). Log through `tracing`, whose writer passes every line through the current `Redactor`; never print config text or URLs to stderr directly.
- Render `reqwest` errors with `tgin_runtime::http::describe_error` (no URL). Config errors never quote secret values or source lines.
- Every constant-time compare goes through `tgin_runtime::secret::secret_eq`.
- Outbound HTTP clients come from `tgin_runtime::http::client`, which never follows redirects; `tgin admin` asks it for `no_proxy`, so the admin token never passes a proxy.
- tgin never acts on its own pushes: every push carries `Via: 1.1 <pseudonym>` (`tgin_runtime::http::Via`, random per process), and both planes' routers are wrapped in `tgin_runtime::http::refuse_loops`, which answers `508 Loop Detected` to a request carrying it. Keep any new listener or router behind it: with `accept_set_webhook`, a bot's token holder chooses push destinations.

### Config model

`crates/tgin-core/src/config/model.rs` keeps line and column for every error, so it uses no `#[serde(untagged)]`, no internally tagged enums and no `flatten` (they make serde-saphyr buffer and lose locations); string-or-object fields have hand-written visitors and schemas (`schema.rs`). Every field is either applied live by a reload or listed in `restart_required` (`crates/tgin-core/src/reload.rs`). Doc comments are the user-facing descriptions: they become the JSON Schema and `docs/config-reference.md`.

### Errors and tasks

- Errors are typed (`thiserror`); `expect` only for invariants. Exit codes live in `crates/tgin/src/exit.rs`.
- Long-running tasks run under `tgin_runtime::supervisor::Supervisor` (restart with backoff on `Exit::Retry` or panic, `Failed` on `Exit::Fatal`); graceful stops use `CancellationToken`s, never the supervisor's abort token.

### Unsafe

`unsafe_code` is denied workspace-wide. The only exception is the test-only counting allocator in `crates/tgin-botapi/tests/support/counting_alloc.rs` (`#![allow(unsafe_code)]`), which forwards to `System` with a `// SAFETY:` comment on each block; `crates/tgin-botapi/tests/alloc_budget.rs` and `crates/tgin/tests/gateway_bounded.rs` include it (the latter by `#[path]`), each with its own `#[global_allocator]` line. New unsafe code needs the same: a test-only or proven need, a scoped `allow`, and a `SAFETY` comment per block.

### Style

Imports follow the grouping of the file you edit; `cargo fmt` sorts within groups. Comments say why, not what.

## Testing & QA

- **Unit tests** are inline (`#[cfg(test)] mod tests`); **integration tests** live in each crate's `tests/`: `tgin-core/tests/{config,engine,push}.rs`, `tgin-botapi/tests/{longpoll,webhook,alloc_budget}.rs`, `tgin-runtime/tests/{http,watch}.rs`, `tgin/tests/{e2e,admin,panic_hook,gateway,gateway_bounded,gateway_intercept,webhook_reply}.rs` (`gateway_bounded` measures the whole process's heap, so it is a binary of its own).
- **Real boundaries.** HTTP peers are real servers (`wiremock`, or axum on `127.0.0.1:0`); config tests use `MapEnv` instead of mutating the process environment.
- **Paused clock.** Timer-driven logic (queue leases, supervisor backoff, engine drains, long-poll backoff) is tested on a paused clock (`#[tokio::test(start_paused = true)]` or `tokio::time::pause()`), so tests take no wall time and do not flake on slow machines; use real time only where real I/O has to make progress, with bounded waits. A paused clock jumps to the next timer whenever the runtime waits for a socket; where a test over real sockets asserts exact instants (the gateway's rate limits), `crates/tgin/tests/gateway.rs::freeze` holds that jump back with a blocking task, so time moves only by `tokio::time::advance`, and its bounded waits run on real time.
- **Child processes bind port 0.** `crates/tgin/tests/support/mod.rs::spawn_listening` starts the `tgin` binary with a config whose listeners use port 0 and reads the admin plane's bound address from the `admin plane listening` log line; it removes `RUST_LOG` from the child's environment (it would hide that info line). Never pick a port by binding and releasing it: another socket can take it before the child binds.
- **Signals.** Unix tests deliver real signals to a child process with `kill`; other app tests feed signals through `Signals::from_channel`.
- **Property tests** (`proptest`) cover the queue's invariants (`tgin-runtime/src/queue.rs`) and fact extraction against a `serde_json::Value` reference (`tgin-botapi/src/facts.rs`).
- **Performance / load tests** are not run by `cargo test`. They live under `tests/performance/` and require Docker, `make`, `curl` and `git` — see `tests/performance/makefile` and `benchmark.sh`. `ENGINE` selects the engine (`v1` default; `upstream` = the original engine, chesnokpeter/tgin at `UPSTREAM_REF` `4f0971a`, built with its own Dockerfile, its long-poll routes added through its management API; or both: `ENGINE="upstream v1"` alternates them run by run); `BOT_IMPL` selects the downstream (`rust` default = `tgin-bench bot`, 50 000+ updates/s; `py` = the aiogram bots, about 600–2 400/s per process depending on the engine and `max_in_flight`, which saturate before tgin does). A full pass over both engines is 6 scenarios × 3 RPS values (1 000, 5 000, 10 000) × 3 runs of 30 s plus container start-up, about 70 min; the generator is open loop and measures latency from each update's scheduled time. Results land in `tests/performance/results/<run_id>.csv` (13 columns, see PERF.md) next to `<run_id>.log` (per-run counters) and `<run_id>.stats.log` (`docker stats`). A run with `lag_p99_ms` above 0.5 measured the host, not the engine: `./rerun-invalid.sh results/<run_id>.csv` reruns those. Charts come from `tests/performance/diagram/main.py` (`uv run main.py [CSV ...]`); without arguments it plots the `results/*.csv` whose run id sorts last, and it deletes every PNG in `diagram/generated/` before rendering. Run the matrix on an otherwise idle machine (no cargo builds); PERF.md lists the machine, the upstream/v1 results and the harness's known limits.
- **End-to-end suite** (`cargo xtask e2e [--keep]`, `xtask/src/e2e.rs`; CI job `e2e` on Linux). It runs `tests/e2e/compose.yaml` (project `tgin-e2e`), waits for every service's health check, and checks seven scenarios through the fake's call journal and tgin's `/metrics`: polling echo, restart confirm, files (a 100 000-byte document up and down), webhook delivery (captured and configured endpoints), webhook replies, the transparent 429 retry and per-chat shaping. Every check polls every 100 ms for at most 30 s (no fixed sleeps), and every exact count is checked again after the last scenario. On failure it prints the journal's tail and the compose logs; it always runs `docker compose down -v` unless `--keep`. The polling bots have fixed addresses in `10.231.77.0/24`, because tgin tells pollers apart by client address and the restart scenario needs a restarted bot to be the same poller. The fake tokens live in `compose.yaml` and in the runner (`e2e::tests::tokens_match_compose` keeps them in step). A new gateway behaviour that a framework exercises gets a scenario here; the fake follows telegram-bot-api (`Client.cpp`) wherever tgin depends on its behaviour.
- **Real-Telegram smoke** (`tests/smoke/real_telegram.py`), before a release and never in CI: it starts a built `tgin` (`--tgin`, default `target/debug/tgin`) with `real_telegram.yaml` against the real Bot API, a test bot's token from `TGIN_SMOKE_TOKEN` or a token file given as its argument. Phase A needs no chat; phase B prints instructions for a person to message the bot (`tgin-smoke-reply`) and checks long poll, pull, push, the webhook reply, shaping and a multipart round trip. It refuses to start when the bot has a webhook or pending updates, never prints the token, and scans tgin's log for it at the end. `--api-url` points it at another Bot API (the fake, for a dry run).
- Exercise the actual change before claiming it works: run the touched crate's tests, and for HTTP-facing changes a binary (`examples/simple`, the e2e suite, or `tgin` against the fake Bot API).

### CI checklist

What `.github/workflows/ci.yml` runs; run the same locally before pushing:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p tgin-botapi --bench hot_path_ir --locked -- -D warnings   # `bench = false` keeps it out of --all-targets
cargo xtask schema --check
cargo xtask config-docs --check
cargo test --workspace --locked                                           # CI: Linux, macOS, Windows
cargo check --workspace --locked                                          # with the MSRV toolchain (1.89)
cargo deny --locked check
cargo build --release --locked --target x86_64-unknown-linux-musl         # musl job; the Docker build runs cargo build --release --locked -p tgin
```

CI also builds the Docker image on native amd64 and arm64 runners, runs clippy and tests for `tests/performance/benchmark`, runs `bench-gate`: `cargo bench -p tgin-botapi --bench hot_path_ir` on a pinned toolchain under Callgrind (`gungraun/setup-gungraun`), failing past any benchmark's `Ir` limit, and runs the `e2e` job (`cargo xtask e2e`, which needs Docker with Compose).

## Runtime / Tooling

- **Rust** edition 2024 in every crate, MSRV 1.89 (checked in CI), no toolchain pin. Tokio multi-threaded runtime with `runtime.worker_threads` workers (`auto` = one per CPU). Linux musl builds use mimalloc.
- **HTTP / TLS stack is fixed**: tokio, axum 0.8 + axum-server 0.8 (HTTP/1.1 only, rustls with ALPN `http/1.1`), reqwest 0.13 (`default-features = false`, rustls); one aws-lc-rs crypto provider. OpenSSL, native-tls and ring are banned in `deny.toml`.
- **Config**: YAML through serde-saphyr; JSON Schema through schemars.
- **Python and Node**: `examples/simple/`, `tests/e2e/bots/python/` and `tests/performance/diagram/` are `uv` projects with a committed `uv.lock`; `tests/e2e/bots/node/` is an npm project with a committed `package-lock.json` (`npm ci`); the perf bots (`tests/performance/bot/`) install `requirements.txt` with pip; `tests/smoke/` uses the Python standard library only.
- No package-manager wrappers and no root `Makefile`: work through `cargo` (the perf harness has its own `makefile`).
