# Repository Guidelines

`tgin` is a routing layer between the Telegram Bot API and one-or-more downstream bot instances ("NGINX for Telegram bots"). A single Rust binary loads a RON config, ingests updates via long poll or webhook, and dispatches each update through a tree of routes / load balancers. See `README.md` and `DOCS.md` for end-user documentation; `DOCS.md` is the reference for runtime behaviour (delivery semantics, shutdown, exit codes).

## Architecture & Data Flow

```
Telegram ──► UpdaterComponent (ingress) ──► mpsc<Bytes> (1024) ──► worker pool (dark_threads) ──► RouteableComponent tree ──► downstream bots
                                                                                                        ▲
                                                                          ApiMessage (mpsc) for hot-reconfig, api::Api loop
```

Pipeline stages, all glued together by `crates/tgin/src/tgin.rs::Tgin::run_until` (`run_async` = `run_until(OsSignals::default(), ShutdownTiming::default())`):

1. **Ingress** (`crates/tgin/src/update/`): each `Box<dyn UpdaterComponent>` runs as a `tokio::spawn`ed task that pushes each update's raw JSON as `bytes::Bytes` into a shared bounded `mpsc::Sender<Bytes>` (`UPDATE_CHANNEL_CAPACITY` = 1024). Updates are never parsed into `serde_json::Value` on the hot path; ingress validates the JSON shape once and the bytes are forwarded verbatim.
   - `LongPollUpdate` polls `https://api.telegram.org/bot<token>/getUpdates` (or the full `url` override) via `reqwest`, with jittered backoff, `Retry-After` on 429, 409 recovery and permanent-failure detection (401/403/404).
   - `WebhookUpdate` registers a POST endpoint on the shared axum router, checks `x-telegram-bot-api-secret-token` in a middleware before the body is read, and optionally calls `setWebhook` at start.
2. **Dispatch**: `spawn_worker_pool` runs `dark_threads` workers over the shared receiver; each runs one update through `route.process` to completion before taking the next.
3. **Routing tree** (`crates/tgin/src/route/`, `crates/tgin/src/lb/`): a single `Arc<dyn RouteableComponent>` owns all destinations.
   - Leaves: `LongPollRoute` (bounded `Arc<std::sync::Mutex<VecDeque<Bytes>>>` + `Notify`, drained on read by a Telegram-shaped `getUpdates` endpoint), `WebhookRoute` (one POST per update, no retries).
   - Inner nodes: `RoundRobinLB` (atomic cursor → one child per update), `AllLB` (fans out to every child on a `JoinSet` and waits for all).
4. **HTTP plane** (axum): every component implements `Serverable::set_server(Router<Sender<Bytes>>) -> Router<Sender<Bytes>>`; `Tgin` chains the `set_server` calls, merges `observability_routes`, then binds on `0.0.0.0:<server_port>` on the calling task (TLS via `axum_server::tls_rustls` when `ssl` is set). When `api` is enabled, `dynamic_handler` is installed as the fallback so long-poll routes added at runtime resolve through `LONGPOLL_REGISTRY`. `/livez` and `/readyz` are always open; `/status` and `/metrics` are wrapped by `utils::auth::guard` and need `Authorization: Bearer <auth_token>` when `auth_token` is set, as does `/<base_path>/*`. The data plane (webhook ingress, long-poll routes) is never gated.
5. **Management API** (`crates/tgin/src/api/`): owns its own `mpsc` pair; HTTP handlers send `ApiMessage::{GetRoutes, AddRoute, RmRoute}` with a `oneshot` reply, and the loop in `run_until` mutates the routing tree (`add_route` / `remove_route` on `Routeable`) and maps `AddRouteError` / `RemoveRouteError` to `ApiError` (409 / 404).
6. **Shutdown**: a coordinator task waits for a `ShutdownCause` from the injected `ShutdownSignals` source (`OsSignals` in production) or `AllIngressFailed` from `IngressHealth`, then runs lame duck → cancel `ingest` → `begin_drain` → wait for `backlog_drained` (bounded by `ShutdownTiming::grace`) → close HTTP → cancel `drain` → at `drain_deadline` cancel `stop` (workers abort in-flight dispatches and exit). Without `server_port` it cancels `ingest` and `drain` at once and `stop` after the grace. A further signal during any of this cancels `forced` and `run_until` returns `ShutdownOutcome::Forced`; otherwise it returns `Completed { abandoned }`. `main` maps the outcome to exit codes 0 / 1 / 2 / 130.

### Core traits (`crates/tgin/src/base.rs`, `crates/tgin/src/update/base.rs`)

- `Updater` — `async fn start(&self, tx: Sender<Bytes>, shutdown: CancellationToken)`, plus `metrics()` for `/status`.
- `Routeable` — `async fn process(&self, update: Bytes)`; `id()` (`RouteId::Path` / `RouteId::Url` for leaves, `None` for LBs); `add_route` / `remove_route` returning `AddRouteError` / `RemoveRouteError` (defaults: `Conflict` / `NotFound`, overridden by load balancers); `begin_drain()` and `pending()` for shutdown.
- `Serverable` — `async fn set_server(&self, Router<Sender<Bytes>>) -> Router<Sender<Bytes>>`; default returns the router unchanged.
- `Printable` — `print()` for the boot banner, `json_struct()` for `/api/routes`, `/status` and `/metrics`.
- Blanket-impl supertraits combine the above: `UpdaterComponent = Updater + Serverable + Printable`, `RouteableComponent = Routeable + Serverable + Printable`. Implementing the constituents is enough; the blanket impl provides the supertrait.

All async trait methods use `#[async_trait]`.

## Key Directories

| Path | Role |
| --- | --- |
| `Cargo.toml` | Virtual workspace: `[workspace.package]` (version `1.0.0-dev`, `rust-version`), `[workspace.lints]`, release/dist profiles. `tests/performance/benchmark` is excluded (own Cargo.lock). |
| `crates/tgin/` | The `tgin` binary package (edition 2021; dist builds it via `[package.metadata.dist] dist = true`). |
| `crates/tgin-core/`, `crates/tgin-runtime/`, `crates/tgin-botapi/` | Library crates of the new engine (currently empty). |
| `xtask/` | Repository automation, `cargo xtask` (alias in `.cargo/config.toml`). `cargo xtask schema` regenerates `schema/tgin.v1.schema.json` from `tgin_core::config::schema()`; `--check` fails if it is stale. |
| `schema/tgin.v1.schema.json` | Generated JSON Schema of the v1 YAML config (`crates/tgin-core/src/config/`). Never edit by hand; the `schema_is_up_to_date` test and CI check it. |
| `crates/tgin/src/main.rs` | clap CLI (`-f/--file`, default `tgin.ron`; `--version`), `load_config` → `observe::init` → `build_updates` / `build_route` → `Tgin::run`; exit codes 0 / 1 / 2 / 130 (`RunOutcome::exit_code`). Declares the musl-only global allocator: `#[global_allocator]` `mimalloc::MiMalloc` under `#[cfg(target_env = "musl")]`, with the `mimalloc` dependency in `crates/tgin/Cargo.toml`'s `[target.'cfg(target_env = "musl")'.dependencies]`. |
| `crates/tgin/src/tgin.rs` | `Tgin` struct: runtime, bind, worker pool (`WorkerPool`), API loop, shutdown coordinator (`ShutdownTiming`, `ShutdownCause`, `ShutdownSignals` / `OsSignals`, `ShutdownOutcome`, `IngressHealth`), `observability_routes`. |
| `crates/tgin/src/base.rs` | Trait definitions, `RouteId`, `AddRouteError` / `RemoveRouteError`, component supertraits. |
| `crates/tgin/src/config/schema.rs` | RON-deserialized types: `TginConfig`, `UpdateConfig`, `RouteConfig`, `SslConfig`, `ApiConfig`, `RegistrationWebhookConfig`, `LogFormat`. |
| `crates/tgin/src/config/setup.rs` | `load_config`, `${VAR}` substitution (regex `\$\{(\w+)\}`; collects every unset variable), `validate`, `effective_auth_token`, `build_updates`, recursive `build_route`. |
| `crates/tgin/src/config/error.rs` | `ConfigError` (`FileRead`, `MissingEnvVars`, `Parse`, `Validation`) and `ValidationError`; `main` prints them as one `tgin: …` message and exits 1. |
| `crates/tgin/src/update/{longpull,webhook}.rs` | Ingress adapters. **Note the spelling: the module is `longpull`, not `longpoll`.** |
| `crates/tgin/src/route/{longpull,webhook}.rs` | Egress routes. Same `longpull` spelling. |
| `crates/tgin/src/lb/{roundrobin,all}.rs` | Load balancers, both `ArcSwap<Vec<Arc<dyn RouteableComponent>>>` with RCU add/remove. |
| `crates/tgin/src/api/{router,methods,message,schemas}.rs` | Hot-reconfig HTTP API, `ApiMessage`, `ApiError` → status codes. |
| `crates/tgin/src/dynamic/handler.rs` | Axum fallback that dispatches to runtime-added long-poll routes via `LONGPOLL_REGISTRY` (`crates/tgin/src/dynamic/longpoll_registry.rs`, `std::sync::LazyLock<ArcSwap<HashMap<String, Arc<LongPollRoute>>>>`). |
| `crates/tgin/src/utils/defaults.rs` | `redact(&str) -> Cow<str>`: the one token-redaction helper (replaces bot tokens with `#####` via a `LazyLock<Regex>` compiled once). |
| `crates/tgin/src/utils/http.rs` | `build_shared_client`: the one process-wide `reqwest::Client`. |
| `crates/tgin/src/utils/auth.rs` | `secret_eq` (the single constant-time compare, used by the webhook secret check and the bearer gate) and `guard` (optional `Bearer` middleware). |
| `crates/tgin/src/observe.rs` | tracing subscriber init, `dispatch_span`, `UpdaterMetrics`, process-global metrics registry, `/status` + `/metrics` handlers. |
| `crates/tgin/src/mock/routes.rs` | `MockCallsRoute` test double, gated by `#[cfg(test)]` on `mod mock` in `main.rs`. |
| `tests/performance/` | Docker-compose load-test harness: Rust `tgin-bench` (load generator, fake Bot API and the `bot` downstream), Python aiogram bots (`BOT_IMPL=py`), Python (matplotlib) plotter in `diagram/`. |
| `examples/simple/` | docker-compose example with two long-poll bots and one webhook bot. |
| `integrations/pytgin/` | Python `aiogram` shim that reroutes `getUpdates` through tgin. |

## Important Files

- `Cargo.toml` — virtual workspace, `rust-version = "1.89"` (MSRV, checked in CI) and version in `[workspace.package]`; the `tgin` binary package stays on edition 2021. `[profile.release]` sets `lto = true` and `codegen-units = 1`; `[profile.dist]` inherits it for release archives.
- `tgin.ron` — root example config; reads `${TOKEN}` and `${TGIN_AUTH_TOKEN}` (it enables `api`).
- `Dockerfile` — BuildKit multi-stage: builder `rust:1.99.0-alpine3.24` and runtime `gcr.io/distroless/static-debian13`, both pinned by digest. Builds a static musl binary, runs as numeric `65532:65532`, no shell in the image. `ENTRYPOINT ["/usr/local/bin/tgin"]`, `CMD ["-f","/etc/tgin/config.ron"]`, OCI labels from build args `VERSION`/`REVISION`/`CREATED`.
- `.github/workflows/ci.yml` — `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -D warnings` (plus the Linux-only `hot_path_ir` bench, which `bench = false` keeps out of `--all-targets`) and `cargo xtask schema --check`, `cargo test --workspace` on Linux, macOS and Windows, MSRV `cargo check --workspace`, `cargo deny check`, static musl build, build-only Docker image on native amd64 and arm64 runners, clippy + tests for `tests/performance/benchmark`, and `bench-gate` (the Callgrind instruction-count gate, `cargo bench -p tgin-botapi --bench hot_path_ir` on a pinned toolchain via `gungraun/setup-gungraun`).
- `crates/tgin-botapi/benches/` and `crates/tgin-botapi/tests/alloc_budget.rs` — hot-path benchmarks over the 1000 updates in `tests/fixtures/updates.jsonl`, routed by `tests/fixtures/hot_path.yaml` (shared setup in `benches/common/mod.rs`). `hot_path.rs` is criterion (`cargo bench -p tgin-botapi -- --quick`). `hot_path_ir.rs` is gungraun under Callgrind (Linux, needs Valgrind and `gungraun-runner`), with a hard `Ir` limit per benchmark set to 1.5× the measured count. `alloc_budget.rs` is the allocation gate: a thread-local counting allocator asserting at most 2 allocations per update on the warm path. The gungraun limits are re-measured (Docker `rust:<version>` with `--security-opt seccomp=unconfined`) whenever the toolchain pinned in `bench-gate` changes: limits and toolchain move together.
- `.github/workflows/release.yml` — generated by dist (cargo-dist 0.32.0) from `dist-workspace.toml`, with hand edits listed in its header. To regenerate, follow the steps in the `allow-dirty` comment in `dist-workspace.toml`, then re-apply the hand edits from the `release.yml` header. The release notes come from the matching `CHANGELOG.md` section; never write credential-shaped text there (e.g. `Bearer` followed by a word), because the Actions runner masks it and drops any job output that contains it.
- `.github/workflows/image.yml` — on `v*` semver tags, builds amd64/arm64 images natively, publishes the multi-arch index to `ghcr.io/arynyklas/tgin` and attests it. `latest` goes only to the highest stable `vX.Y.Z` tag.
- `deny.toml` — cargo-deny policy: advisories (unmaintained, unsound, yanked are errors), license allow-list, bans on `openssl`, `openssl-sys`, `native-tls`, `ring`, crates.io only.
- `.github/dependabot.yml` — weekly grouped updates for cargo, GitHub Actions, Docker, uv and pip.
- `DOCS.md`, `PERF.md`, `CHANGELOG.md`, `SECURITY.md` — user docs, benchmarks, release notes, security policy.

## Development Commands

```bash
# Build
cargo build --workspace                 # debug, every crate
cargo build -p tgin --release           # ship build → target/release/tgin

# Run with the shipped config (it enables the management API, so it needs TGIN_AUTH_TOKEN)
export TOKEN=123456:ABC TGIN_AUTH_TOKEN=$(openssl rand -hex 32)
cargo run -p tgin -- -f tgin.ron
./target/release/tgin -f tgin.ron

# Tests (inline #[cfg(test)] modules; uses tokio + wiremock)
cargo test --workspace              # every crate
cargo test -p tgin lb::roundrobin   # single module
cargo test -p tgin test_round_robin_distribution_even -- --nocapture

# What CI runs besides tests
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo deny check

# Docker (the config must be readable by uid 65532)
docker build -t tgin .
docker run --rm -v "$PWD/tgin.ron:/etc/tgin/config.ron:ro" \
  -e TOKEN=... -e TGIN_AUTH_TOKEN="$TGIN_AUTH_TOKEN" -p 3000:3000 tgin

# Example stack (long-poll + webhook bots)
cd examples/simple && TOKEN=... TGIN_ADMIN_TOKEN=$(openssl rand -hex 32) docker compose up --build

# Performance harness (ENGINE=v1 default or v0 = tgin 0.2 from 1af6aaa;
# BOT_IMPL=rust default, py = the aiogram bots)
cd tests/performance
make build && make image ENGINE=v1
make webhook-tgin-3 RPS=10000 DURATION=30            # one scenario
ENGINE="v0 v1" ./benchmark.sh      # both engines, 6 scenarios × 3 rates × 3 runs (~70 min)
./rerun-invalid.sh results/<run_id>.csv              # rerun runs with lag_p99_ms > 0.5
```

There is no `rustfmt.toml`, `clippy.toml` or `rust-toolchain*` file: CI uses stock `rustfmt` and `clippy` on stable, with clippy warnings as errors.

## Configuration

`tgin.ron` is RON. `${VAR}` placeholders are substituted from env before parse; every unset variable is collected into `ConfigError::MissingEnvVars`, printed once, and the process exits 1. Skeleton:

```ron
(
    dark_threads: 6,                                 // tokio worker threads and dispatch workers (default 4)
    server_port: Some(3000),                         // None disables the HTTP listener
    ssl: Some(SslConfig(cert: "...", key: "...")),   // optional rustls
    api: Some(ApiConfig(base_path: "/api")),         // optional management API
    auth_token: Some("${TGIN_AUTH_TOKEN}"),          // required when `api` is set
    updates: [ LongPollUpdate(token: "${TOKEN}") ],
    route: RoundRobinLB(routes: [
        LongPollRoute(path: "/bot1/getUpdates"),
        WebhookRoute(url: "http://127.0.0.1:8080/bot2"),
    ]),
)
```

Variants must match `RouteConfig` / `UpdateConfig` exactly; every config type is `#[serde(deny_unknown_fields)]`, so a misspelt field is a parse error, never a silent default. `validate` collects every semantic error in one pass (e.g. `ValidationError::ApiWithoutAuthToken` for `api` without a non-blank `auth_token`, `RequiresServerPort` for a `WebhookUpdate`, `LongPollRoute` or `api` without `server_port`, `InvalidPath`, `DuplicatePath` including the reserved `/status`, `/metrics`, `/livez`, `/readyz`). The HTTP API `POST /<base_path>/route` body uses `"type": "Longpull"` or `"Webhook"` (matching `AddRouteTypeSch`, **not** "Longpoll").

## Code Conventions & Patterns

- **Composition over inheritance.** New ingress adapter = a struct implementing `Updater + Serverable + Printable`; new route = `Routeable + Serverable + Printable`. The blanket impl wires it into `*Component`. Default trait methods (e.g. `Serverable::set_server` returning the router unchanged) make stubs cheap.
- **Async everywhere via `async_trait`.** Match the pattern on these traits; the cheap accessors `Routeable::{id, begin_drain, pending}` and `Updater::metrics` are deliberately sync.
- **Payloads are `Bytes`.** An update is one valid JSON value as raw bytes, from ingress to egress; cloning is a ref-count bump. New ingress adapters must validate JSON shape before sending into the channel (see the `Routeable::process` doc comment).
- **Shared state**: `ArcSwap<Vec<Arc<dyn RouteableComponent>>>` for LB children, `Arc<std::sync::Mutex<VecDeque<Bytes>>>` + `Arc<Notify>` for the long-poll buffer (never held across `.await`), `AtomicUsize` for the round-robin cursor, `std::sync::LazyLock<ArcSwap<HashMap<...>>>` (`arc-swap`) for `LONGPOLL_REGISTRY`, `CancellationToken` (`tokio-util`) for shutdown. Reach for the matching primitive rather than introducing a new pattern.
- **Update flow is one direction.** Producers own a `Sender<Bytes>`; the routing tree lives behind `Arc<dyn RouteableComponent>` and is `process`-called from the worker pool. Don't bypass the channel.
- **Errors on the egress path are logged and counted, never retried** (`WebhookRoute` records success / failure / error and moves on). Preserve this unless explicitly changing the contract — downstreams must be resilient. A slow downstream still holds its worker for up to `request_timeout_ms`.
- **Token redaction**: when logging, printing or returning anything that may carry a Telegram token — URLs, and also configured paths (`WebhookUpdate.path`, `LongPollRoute.path`), which may embed one — wrap it in `utils::defaults::redact`, and log `reqwest` errors via `err.without_url()`. This includes `json_struct` outputs (they feed `/status`, `/metrics` and `/api/routes`), so `WebhookRoute::json_struct` redacts its `url` and `LongPollRoute::json_struct` its `path`.
- **Constant-time secret compares go through `utils::auth::secret_eq`.** The optional control-plane `Bearer` gate is `utils::auth::guard`; a blank `auth_token` is normalized to `None` by `config::setup::effective_auth_token` (auth disabled) with a startup warning, never reaching the gate.
- **One HTTP client.** Every outbound call uses the `reqwest::Client` from `utils::http::build_shared_client` and sets its own per-request timeout.
- **`build_route` recurses** through `RouteConfig::{RoundRobinLB,AllLB}` — keep it the single construction site for the routing tree.
- **Naming quirk**: long-poll modules and types are spelled `longpull` (`crates/tgin/src/update/longpull.rs`, `crates/tgin/src/route/longpull.rs`, `AddRouteType::Longpull`, API `"type":"Longpull"`). User-facing docs say "long poll" / "LongPoll"; keep the in-code spelling unless doing a deliberate, repo-wide rename.
- **Errors are typed.** Config loading returns `ConfigError` instead of panicking; route mutations return `AddRouteError` / `RemoveRouteError`; API handlers return `ApiError`. `expect` is reserved for invariants (poisoned mutex, static JSON, client construction). The crate has no `unsafe`.
- **Imports**: follow the grouping of the file you are editing; `cargo fmt` sorts within groups.

## Testing & QA

- **Unit tests are inline.** Each module ends with `#[cfg(test)] mod tests { ... }`; async tests use `#[tokio::test]`. Examples: `crates/tgin/src/lb/roundrobin.rs`, `crates/tgin/src/lb/all.rs`, `crates/tgin/src/route/longpull.rs`, `crates/tgin/src/route/webhook.rs`, `crates/tgin/src/update/{longpull,webhook}.rs`, and the shutdown tests in `crates/tgin/src/tgin.rs`.
- **Test doubles** live in `crates/tgin/src/mock/routes.rs` (`MockCallsRoute` records every received update as `Bytes` in `Arc<tokio::sync::Mutex<Vec<Bytes>>>`; `get_calls` parses them). The `mod mock` declaration in `crates/tgin/src/main.rs` is `#[cfg(test)]`-gated, so production builds don't see it.
- **HTTP mocking**: `wiremock` for outbound assertions (see `crates/tgin/src/route/webhook.rs::tests::test_process_sends_correct_post_request`). `tower::ServiceExt` and `http_body_util` are available via dev-deps for in-process axum requests.
- **Dependencies must not be mocked at the boundary** the system actually crosses — prefer real `wiremock` HTTP servers and real `tokio::sync::mpsc` channels over hand-rolled fakes.
- **Performance / load tests** are not run by `cargo test`. They live under `tests/performance/` and require Docker, `make`, `curl` and `git` — see `tests/performance/makefile` and `benchmark.sh`. `ENGINE` selects the engine (`v1` default, `v0` = 0.2 built from `1af6aaa`, or both: `ENGINE="v0 v1"` alternates them run by run); `BOT_IMPL` selects the downstream (`rust` default = `tgin-bench bot`, 50 000+ updates/s; `py` = the aiogram bots, ~2 000/s, which saturate before tgin does). A full pass over both engines is 6 scenarios × 3 RPS values (1 000, 5 000, 10 000) × 3 runs of 30 s plus container start-up, about 70 min; the generator is open loop and measures latency from each update's scheduled time. Results land in `tests/performance/results/<run_id>.csv` (13 columns, see PERF.md) next to `<run_id>.log` (per-run counters) and `<run_id>.stats.log` (`docker stats`). A run with `lag_p99_ms` above 0.5 measured the host, not the engine: `./rerun-invalid.sh results/<run_id>.csv` reruns those. Charts come from `tests/performance/diagram/main.py` (`uv run main.py [CSV ...]`); without arguments it plots the `results/*.csv` whose run id sorts last, and it deletes every PNG in `diagram/generated/` before rendering. Run the matrix on an otherwise idle machine (no cargo builds); PERF.md lists the machine, the v0/v1 gate and the harness's known limits.
- Always exercise the actual change before claiming it passes: run the specific `cargo test -p tgin <name>` for the touched module, and for HTTP-affecting changes spin up `examples/simple` or a perf scenario.

## Runtime / Tooling

- **Rust** workspace edition 2024 (the `tgin` package stays on 2021), MSRV 1.89, no `rust-toolchain` pin. Tokio multi-threaded runtime with `dark_threads` workers. Linux musl builds use mimalloc as the global allocator (see `crates/tgin/src/main.rs`).
- **Async runtime is fixed**: tokio + axum 0.8 + axum-server 0.8 (TLS via rustls) + reqwest 0.13 (`default-features = false`, rustls only). One aws-lc-rs crypto provider serves both; the platform verifier checks server certificates. OpenSSL, native-tls and ring are banned in `deny.toml`. Keep to this HTTP / TLS stack.
- **Config format**: RON 0.12 only. JSON-shaped types in `crates/tgin/src/api/schemas.rs` use `serde` + `serde_json`.
- **Python**: `examples/simple/` and `tests/performance/diagram/` are `uv` projects with a committed `uv.lock`; the perf bots (`tests/performance/bot/`) install `requirements.txt` with pip in their Dockerfile; `integrations/pytgin/` is a setuptools package without a lock file.
- No package manager wrappers and no `Makefile` at the repo root — interact with the project through `cargo` directly (the perf harness has its own `makefile`).
