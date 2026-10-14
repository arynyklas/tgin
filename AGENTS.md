# Repository Guidelines

`tgin` is a routing layer between the Telegram Bot API and one-or-more downstream bot instances ("NGINX for Telegram bots"). A single Rust binary loads a RON config, ingests updates via long poll or webhook, and dispatches each update through a tree of routes / load balancers. See `README.md` and `DOCS.md` for end-user documentation; `DOCS.md` is the reference for runtime behaviour (delivery semantics, shutdown, exit codes).

## Architecture & Data Flow

```
Telegram ──► UpdaterComponent (ingress) ──► mpsc<Bytes> (1024) ──► worker pool (dark_threads) ──► RouteableComponent tree ──► downstream bots
                                                                                                        ▲
                                                                          ApiMessage (mpsc) for hot-reconfig, api::Api loop
```

Pipeline stages, all glued together by `src/tgin.rs::Tgin::run_until` (`run_async` = `run_until(shutdown_signal(), ShutdownTiming::default())`):

1. **Ingress** (`src/update/`): each `Box<dyn UpdaterComponent>` runs as a `tokio::spawn`ed task that pushes each update's raw JSON as `bytes::Bytes` into a shared bounded `mpsc::Sender<Bytes>` (`UPDATE_CHANNEL_CAPACITY` = 1024). Updates are never parsed into `serde_json::Value` on the hot path; ingress validates the JSON shape once and the bytes are forwarded verbatim.
   - `LongPollUpdate` polls `https://api.telegram.org/bot<token>/getUpdates` (or the full `url` override) via `reqwest`, with jittered backoff, `Retry-After` on 429, 409 recovery and permanent-failure detection (401/403/404).
   - `WebhookUpdate` registers a POST endpoint on the shared axum router, checks `x-telegram-bot-api-secret-token` in a middleware before the body is read, and optionally calls `setWebhook` at start.
2. **Dispatch**: `spawn_worker_pool` runs `dark_threads` workers over the shared receiver; each runs one update through `route.process` to completion before taking the next.
3. **Routing tree** (`src/route/`, `src/lb/`): a single `Arc<dyn RouteableComponent>` owns all destinations.
   - Leaves: `LongPollRoute` (bounded `Arc<std::sync::Mutex<VecDeque<Bytes>>>` + `Notify`, drained on read by a Telegram-shaped `getUpdates` endpoint), `WebhookRoute` (one POST per update, no retries).
   - Inner nodes: `RoundRobinLB` (atomic cursor → one child per update), `AllLB` (fans out to every child on a `JoinSet` and waits for all).
4. **HTTP plane** (axum): every component implements `Serverable::set_server(Router<Sender<Bytes>>) -> Router<Sender<Bytes>>`; `Tgin` chains the `set_server` calls, merges `observability_routes`, then binds on `0.0.0.0:<server_port>` on the calling task (TLS via `axum_server::tls_rustls` when `ssl` is set). When `api` is enabled, `dynamic_handler` is installed as the fallback so long-poll routes added at runtime resolve through `LONGPOLL_REGISTRY`. `/livez` and `/readyz` are always open; `/status` and `/metrics` are wrapped by `utils::auth::guard` and need `Authorization: Bearer <auth_token>` when `auth_token` is set, as does `/<base_path>/*`. The data plane (webhook ingress, long-poll routes) is never gated.
5. **Management API** (`src/api/`): owns its own `mpsc` pair; HTTP handlers send `ApiMessage::{GetRoutes, AddRoute, RmRoute}` with a `oneshot` reply, and the loop in `run_until` mutates the routing tree (`add_route` / `remove_route` on `Routeable`) and maps `AddRouteError` / `RemoveRouteError` to `ApiError` (409 / 404).
6. **Shutdown**: a coordinator task waits for a `ShutdownCause` (signal, or `AllIngressFailed` from `IngressHealth`), then runs lame duck → cancel `ingest` → `begin_drain` → wait for `backlog_drained` (bounded by `ShutdownTiming::grace`) → close HTTP → cancel `drain`. Without `server_port` it cancels both tokens at once. `main` maps the outcome to exit codes 0 / 1 / 2.

### Core traits (`src/base.rs`, `src/update/base.rs`)

- `Updater` — `async fn start(&self, tx: Sender<Bytes>, shutdown: CancellationToken)`, plus `metrics()` for `/status`.
- `Routeable` — `async fn process(&self, update: Bytes)`; `id()` (`RouteId::Path` / `RouteId::Url` for leaves, `None` for LBs); `add_route` / `remove_route` returning `AddRouteError` / `RemoveRouteError` (defaults: `Conflict` / `NotFound`, overridden by load balancers); `begin_drain()` and `pending()` for shutdown.
- `Serverable` — `async fn set_server(&self, Router<Sender<Bytes>>) -> Router<Sender<Bytes>>`; default returns the router unchanged.
- `Printable` — `print()` for the boot banner, `json_struct()` for `/api/routes`, `/status` and `/metrics`.
- Blanket-impl supertraits combine the above: `UpdaterComponent = Updater + Serverable + Printable`, `RouteableComponent = Routeable + Serverable + Printable`. Implementing the constituents is enough; the blanket impl provides the supertrait.

All async trait methods use `#[async_trait]`.

## Key Directories

| Path | Role |
| --- | --- |
| `src/main.rs` | clap CLI (`-f/--file`, default `tgin.ron`; `--version`), `load_config` → `observe::init` → `build_updates` / `build_route` → `Tgin::run`; exit codes 0 / 1 / 2. Declares the musl-only global allocator: `#[global_allocator]` `mimalloc::MiMalloc` under `#[cfg(target_env = "musl")]`, with the `mimalloc` dependency in Cargo.toml's `[target.'cfg(target_env = "musl")'.dependencies]`. |
| `src/tgin.rs` | `Tgin` struct: runtime, bind, worker pool, API loop, shutdown coordinator (`ShutdownTiming`, `ShutdownCause`, `IngressHealth`), `observability_routes`. |
| `src/base.rs` | Trait definitions, `RouteId`, `AddRouteError` / `RemoveRouteError`, component supertraits. |
| `src/config/schema.rs` | RON-deserialized types: `TginConfig`, `UpdateConfig`, `RouteConfig`, `SslConfig`, `ApiConfig`, `RegistrationWebhookConfig`, `LogFormat`. |
| `src/config/setup.rs` | `load_config`, `${VAR}` substitution (regex `\$\{(\w+)\}`; collects every unset variable), `validate`, `effective_auth_token`, `build_updates`, recursive `build_route`. |
| `src/config/error.rs` | `ConfigError` (`FileRead`, `MissingEnvVars`, `Parse`, `Validation`) and `ValidationError`; `main` prints them as one `tgin: …` message and exits 1. |
| `src/update/{longpull,webhook}.rs` | Ingress adapters. **Note the spelling: the module is `longpull`, not `longpoll`.** |
| `src/route/{longpull,webhook}.rs` | Egress routes. Same `longpull` spelling. |
| `src/lb/{roundrobin,all}.rs` | Load balancers, both `ArcSwap<Vec<Arc<dyn RouteableComponent>>>` with RCU add/remove. |
| `src/api/{router,methods,message,schemas}.rs` | Hot-reconfig HTTP API, `ApiMessage`, `ApiError` → status codes. |
| `src/dynamic/handler.rs` | Axum fallback that dispatches to runtime-added long-poll routes via `LONGPOLL_REGISTRY` (`src/dynamic/longpoll_registry.rs`, `std::sync::LazyLock<ArcSwap<HashMap<String, Arc<LongPollRoute>>>>`). |
| `src/utils/defaults.rs` | `TELEGRAM_TOKEN_RE` (`LazyLock<Regex>`, compiled once) for token redaction. |
| `src/utils/http.rs` | `build_shared_client`: the one process-wide `reqwest::Client`. |
| `src/utils/auth.rs` | `secret_eq` (the single constant-time compare, used by the webhook secret check and the bearer gate) and `guard` (optional `Bearer` middleware). |
| `src/observe.rs` | tracing subscriber init, `dispatch_span`, `UpdaterMetrics`, process-global metrics registry, `/status` + `/metrics` handlers. |
| `src/mock/routes.rs` | `MockCallsRoute` test double, gated by `#[cfg(test)]` on `mod mock` in `main.rs`. |
| `tests/performance/` | Docker-compose load-test harness: Rust `tgin-bench` load generator, Python aiogram bots, Python (matplotlib) plotter in `diagram/`. |
| `examples/simple/` | docker-compose example with two long-poll bots and one webhook bot. |
| `integrations/pytgin/` | Python `aiogram` shim that reroutes `getUpdates` through tgin. |

## Important Files

- `Cargo.toml` — single binary, edition 2021, `rust-version = "1.88"` (MSRV, checked in CI). `[profile.release]` sets `lto = true` and `codegen-units = 1`; `[profile.dist]` inherits it for release archives.
- `tgin.ron` — root example config; reads `${TOKEN}` and `${TGIN_AUTH_TOKEN}` (it enables `api`).
- `Dockerfile` — BuildKit multi-stage: builder `rust:1.99.0-alpine3.24` and runtime `gcr.io/distroless/static-debian13`, both pinned by digest. Builds a static musl binary, runs as numeric `65532:65532`, no shell in the image. `ENTRYPOINT ["/usr/local/bin/tgin"]`, `CMD ["-f","/etc/tgin/config.ron"]`, OCI labels from build args `VERSION`/`REVISION`/`CREATED`.
- `.github/workflows/ci.yml` — `cargo fmt --check`, `cargo clippy --all-targets -D warnings`, `cargo test` on Linux, macOS and Windows, MSRV `cargo check`, `cargo deny check`, static musl build, Docker build, and clippy + tests for `tests/performance/benchmark`.
- `.github/workflows/release.yml` — generated by dist (cargo-dist 0.32.0) from `dist-workspace.toml`, with hand edits listed in its header. To regenerate, follow the steps in the `allow-dirty` comment in `dist-workspace.toml`, then re-apply the hand edits from the `release.yml` header.
- `.github/workflows/image.yml` — on `v*` semver tags, builds amd64/arm64 images natively, publishes the multi-arch index to `ghcr.io/arynyklas/tgin` and attests it.
- `deny.toml` — cargo-deny policy: advisories (unmaintained, unsound, yanked are errors), license allow-list, bans on `openssl`, `openssl-sys`, `native-tls`, `ring`, crates.io only.
- `.github/dependabot.yml` — weekly grouped updates for cargo, GitHub Actions, Docker, uv and pip.
- `DOCS.md`, `PERF.md`, `CHANGELOG.md`, `SECURITY.md` — user docs, benchmarks, release notes, security policy.

## Development Commands

```bash
# Build
cargo build                 # debug
cargo build --release       # ship build → target/release/tgin

# Run with the shipped config (it enables the management API, so it needs TGIN_AUTH_TOKEN)
export TOKEN=123456:ABC TGIN_AUTH_TOKEN=$(openssl rand -hex 32)
cargo run -- -f tgin.ron
./target/release/tgin -f tgin.ron

# Tests (inline #[cfg(test)] modules; uses tokio + wiremock)
cargo test                          # whole crate
cargo test -p tgin lb::roundrobin   # single module
cargo test test_round_robin_distribution_even -- --nocapture

# What CI runs besides tests
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo deny check

# Docker (the config must be readable by uid 65532)
docker build -t tgin .
docker run --rm -v "$PWD/tgin.ron:/etc/tgin/config.ron:ro" \
  -e TOKEN=... -e TGIN_AUTH_TOKEN="$TGIN_AUTH_TOKEN" -p 3000:3000 tgin

# Example stack (long-poll + webhook bots)
cd examples/simple && TOKEN=... TGIN_AUTH_TOKEN=$(openssl rand -hex 32) docker compose up --build

# Performance harness
cd tests/performance
make build
make webhook-tgin-scale-2 RPS=1000 DURATION=10   # one scenario
./benchmark.sh                           # full RPS matrix → results/<run_id>.csv (~3 h)
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

Variants must match `RouteConfig` / `UpdateConfig` exactly; every config type is `#[serde(deny_unknown_fields)]`, so a misspelt field is a parse error, never a silent default. `validate` collects every semantic error in one pass (e.g. `ValidationError::ApiWithoutAuthToken` for `api` without a non-blank `auth_token`, `InvalidPath`, `DuplicatePath` including the reserved `/status`, `/metrics`, `/livez`, `/readyz`). The HTTP API `POST /<base_path>/route` body uses `"type": "Longpull"` or `"Webhook"` (matching `AddRouteTypeSch`, **not** "Longpoll").

## Code Conventions & Patterns

- **Composition over inheritance.** New ingress adapter = a struct implementing `Updater + Serverable + Printable`; new route = `Routeable + Serverable + Printable`. The blanket impl wires it into `*Component`. Default trait methods (e.g. `Serverable::set_server` returning the router unchanged) make stubs cheap.
- **Async everywhere via `async_trait`.** Match the pattern on these traits; the cheap accessors `Routeable::{id, begin_drain, pending}` and `Updater::metrics` are deliberately sync.
- **Payloads are `Bytes`.** An update is one valid JSON value as raw bytes, from ingress to egress; cloning is a ref-count bump. New ingress adapters must validate JSON shape before sending into the channel (see the `Routeable::process` doc comment).
- **Shared state**: `ArcSwap<Vec<Arc<dyn RouteableComponent>>>` for LB children, `Arc<std::sync::Mutex<VecDeque<Bytes>>>` + `Arc<Notify>` for the long-poll buffer (never held across `.await`), `AtomicUsize` for the round-robin cursor, `std::sync::LazyLock<ArcSwap<HashMap<...>>>` (`arc-swap`) for `LONGPOLL_REGISTRY`, `CancellationToken` (`tokio-util`) for shutdown. Reach for the matching primitive rather than introducing a new pattern.
- **Update flow is one direction.** Producers own a `Sender<Bytes>`; the routing tree lives behind `Arc<dyn RouteableComponent>` and is `process`-called from the worker pool. Don't bypass the channel.
- **Errors on the egress path are logged and counted, never retried** (`WebhookRoute` records success / failure / error and moves on). Preserve this unless explicitly changing the contract — downstreams must be resilient. A slow downstream still holds its worker for up to `request_timeout_ms`.
- **Token redaction**: when logging or printing anything that may carry a Telegram token, use `utils::defaults::TELEGRAM_TOKEN_RE`, and log `reqwest` errors via `err.without_url()`. This includes `json_struct` outputs (they feed `/status`, `/metrics` and `/api/routes`), so `WebhookRoute::json_struct` redacts its `url`.
- **Constant-time secret compares go through `utils::auth::secret_eq`.** The optional control-plane `Bearer` gate is `utils::auth::guard`; a blank `auth_token` is normalized to `None` by `config::setup::effective_auth_token` (auth disabled) with a startup warning, never reaching the gate.
- **One HTTP client.** Every outbound call uses the `reqwest::Client` from `utils::http::build_shared_client` and sets its own per-request timeout.
- **`build_route` recurses** through `RouteConfig::{RoundRobinLB,AllLB}` — keep it the single construction site for the routing tree.
- **Naming quirk**: long-poll modules and types are spelled `longpull` (`src/update/longpull.rs`, `src/route/longpull.rs`, `AddRouteType::Longpull`, API `"type":"Longpull"`). User-facing docs say "long poll" / "LongPoll"; keep the in-code spelling unless doing a deliberate, repo-wide rename.
- **Errors are typed.** Config loading returns `ConfigError` instead of panicking; route mutations return `AddRouteError` / `RemoveRouteError`; API handlers return `ApiError`. `expect` is reserved for invariants (poisoned mutex, static JSON, client construction). The crate has no `unsafe`.
- **Imports**: follow the grouping of the file you are editing; `cargo fmt` sorts within groups.

## Testing & QA

- **Unit tests are inline.** Each module ends with `#[cfg(test)] mod tests { ... }`; async tests use `#[tokio::test]`. Examples: `src/lb/roundrobin.rs`, `src/lb/all.rs`, `src/route/longpull.rs`, `src/route/webhook.rs`, `src/update/{longpull,webhook}.rs`, and the shutdown tests in `src/tgin.rs`.
- **Test doubles** live in `src/mock/routes.rs` (`MockCallsRoute` records every received update as `Bytes` in `Arc<tokio::sync::Mutex<Vec<Bytes>>>`; `get_calls` parses them). The `mod mock` declaration in `src/main.rs` is `#[cfg(test)]`-gated, so production builds don't see it.
- **HTTP mocking**: `wiremock` for outbound assertions (see `src/route/webhook.rs::tests::test_process_sends_correct_post_request`). `tower::ServiceExt` and `http_body_util` are available via dev-deps for in-process axum requests.
- **Dependencies must not be mocked at the boundary** the system actually crosses — prefer real `wiremock` HTTP servers and real `tokio::sync::mpsc` channels over hand-rolled fakes.
- **Performance / load tests** are not run by `cargo test`. They live under `tests/performance/` and require Docker, `make`, `curl` and `git` — see `tests/performance/makefile` and `benchmark.sh`. A full sweep is 12 scenarios × 6 RPS values × 3 runs (216 runs of 30 s load plus container start-up), about 3 h. Results land in `tests/performance/results/<run_id>.csv` and are plotted by `tests/performance/diagram/main.py` (`uv run main.py`), which picks the newest CSV under `results/`. PERF.md lists the harness's known measurement defects.
- Always exercise the actual change before claiming it passes: run the specific `cargo test <name>` for the touched module, and for HTTP-affecting changes spin up `examples/simple` or a perf scenario.

## Runtime / Tooling

- **Rust** edition 2021, MSRV 1.88, no `rust-toolchain` pin. Tokio multi-threaded runtime with `dark_threads` workers. Linux musl builds use mimalloc as the global allocator (see `src/main.rs`).
- **Async runtime is fixed**: tokio + axum 0.8 + axum-server 0.8 (TLS via rustls) + reqwest 0.13 (`default-features = false`, rustls only). One aws-lc-rs crypto provider serves both; the platform verifier checks server certificates. OpenSSL, native-tls and ring are banned in `deny.toml`. Keep to this HTTP / TLS stack.
- **Config format**: RON 0.12 only. JSON-shaped types in `src/api/schemas.rs` use `serde` + `serde_json`.
- **Python**: `examples/simple/` and `tests/performance/diagram/` are `uv` projects with a committed `uv.lock`; the perf bots (`tests/performance/bot/`) install `requirements.txt` with pip in their Dockerfile; `integrations/pytgin/` is a setuptools package without a lock file.
- No package manager wrappers and no `Makefile` at the repo root — interact with the project through `cargo` directly (the perf harness has its own `makefile`).
