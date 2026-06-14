# Repository Guidelines

`tgin` is a routing layer between the Telegram Bot API and one-or-more downstream bot instances ("NGINX for Telegram bots"). A single Rust binary loads a RON config, ingests updates via long poll or webhook, and dispatches each update through a tree of routes / load balancers. See `README.md` and `DOCS.md` for end-user documentation.

## Architecture & Data Flow

```
Telegram ──► UpdaterComponent (ingress) ──► mpsc<Value> ──► RouteableComponent (egress tree) ──► downstream bots
                                                              ▲
                                                              │ ApiMessage (mpsc) for hot-reconfig
                                                          api::Api
```

Pipeline stages, all glued together by `src/tgin.rs::Tgin::run_async`:

1. **Ingress** (`src/update/`): each `Box<dyn UpdaterComponent>` runs as a `tokio::spawn`ed task that pushes `serde_json::Value` updates into a shared `mpsc::Sender<Value>` (capacity 1_000_000).
   - `LongPollUpdate` polls `https://api.telegram.org/bot<token>/getUpdates` via `reqwest`.
   - `WebhookUpdate` registers an HTTP POST endpoint on the shared axum router and validates `x-telegram-bot-api-secret-token`.
2. **Routing tree** (`src/route/`, `src/lb/`): a single `Arc<dyn RouteableComponent>` owns all destinations.
   - Leaves: `LongPollRoute` (buffers updates in `Arc<Mutex<VecDeque<Value>>>` + `Notify`, served as a Telegram-shaped `getUpdates` endpoint), `WebhookRoute` (POSTs to a downstream URL).
   - Inner nodes: `RoundRobinLB` (atomic cursor → one child per update), `AllLB` (broadcasts via `tokio::spawn` to every child).
3. **HTTP plane** (axum): every component implements `Serverable::set_server(Router) -> Router`; `Tgin` builds one router by chaining `set_server` calls, then binds it on `0.0.0.0:<server_port>` (TLS via `axum_server::tls_rustls` when `ssl` is set). When `api` is enabled, `dynamic_handler` is installed as the fallback so newly-added long-poll routes resolve through `LONGPOLL_REGISTRY`.
4. **Management API** (`src/api/`): owns its own `mpsc` pair; HTTP handlers send `ApiMessage::{GetRoutes, AddRoute, RmRoute}` to the main loop, which mutates the routing tree (`add_route` / `remove_route` on `RouteableComponent`).

The main loop in `Tgin::run_async` is a `tokio::select!` between the update channel and the API channel — without an API, it degrades to a plain `while let Some(update) = rx.recv()`.

### Core traits (`src/base.rs`, `src/update/base.rs`)

- `Updater` — `async fn start(&self, tx: Sender<Value>)`.
- `Routeable` — `async fn process(&self, Value)`, plus default-erroring `add_route` / `remove_route` overridden by load balancers.
- `Serverable` — `async fn set_server(&self, Router) -> Router`; default returns the router unchanged.
- `Printable` — `print()` for the boot banner, `json_struct()` for `/routes` introspection.
- Blanket-impl supertraits combine the above: `UpdaterComponent = Updater + Serverable + Printable`, `RouteableComponent = Routeable + Serverable + Printable`. Implementing the constituents is enough; do **not** implement the supertrait by hand.

All async trait methods use `#[async_trait]`.

## Key Directories

| Path | Role |
| --- | --- |
| `src/main.rs` | clap CLI (`-f/--file`, default `tgin.ron`), wires `load_config` → `build_updates` → `build_route` → `Tgin::run`. |
| `src/tgin.rs` | `Tgin` struct, runtime + axum bind + main select-loop. |
| `src/base.rs` | Trait definitions and component supertraits. |
| `src/config/schema.rs` | RON-deserialized types: `TginConfig`, `UpdateConfig`, `RouteConfig`, `SslConfig`, `ApiConfig`, `RegistrationWebhookConfig`. |
| `src/config/setup.rs` | `load_config`, `${VAR}` env substitution (regex `\$\{(\w+)\}`, panics on missing var), `build_updates`, recursive `build_route`. |
| `src/update/{longpull,webhook}.rs` | Ingress adapters. **Note the spelling: the module is `longpull`, not `longpoll`.** |
| `src/route/{longpull,webhook}.rs` | Egress routes. Same `longpull` spelling. |
| `src/lb/{roundrobin,all}.rs` | Load balancers, both `RwLock<Vec<Arc<dyn RouteableComponent>>>` + dynamic add/remove. |
| `src/api/{router,methods,message,schemas}.rs` | Hot-reconfig HTTP API + `ApiMessage` enum. |
| `src/dynamic/handler.rs` | Axum fallback that dispatches to dynamically-added long-poll routes via `LONGPOLL_REGISTRY` (`src/dynamic/longpoll_registry.rs`, `Lazy<ArcSwap<HashMap<String, Arc<LongPollRoute>>>>`). |
| `src/utils/defaults.rs` | `TELEGRAM_TOKEN_REGEX` for log redaction. |
| `src/mock/routes.rs` | `MockCallsRoute` test double, gated by `#[cfg(test)]` on `mod mock` in `main.rs`. |
| `tests/performance/` | Docker-compose load-test harness (Rust bench tool, Python aiogram bots, Go plotter). |
| `examples/simple/` | Working docker-compose example with two long-poll bots and one webhook bot. |
| `integrations/pytgin/` | Python `aiogram` shim that reroutes `getUpdates` through tgin. |

## Important Files

- `Cargo.toml` — single binary, edition 2021, no custom features/profiles.
- `tgin.ron` — root example config, demonstrates `${TOKEN}` substitution.
- `Dockerfile` — multi-stage `rust:slim-bookworm` → `debian:bookworm-slim`, runs as `tginuser`, `CMD ["tgin","-f","/etc/tgin/config.ron"]`.
- `DOCS.md` — full configuration reference and HTTP API table.
- `PERF.md` — benchmark methodology and latest numbers.

## Development Commands

```bash
# Build
cargo build                 # debug
cargo build --release       # ship build → target/release/tgin

# Run with a config file
cargo run -- -f tgin.ron
./target/release/tgin -f tgin.ron

# Tests (inline #[cfg(test)] modules; uses tokio + wiremock)
cargo test                          # whole crate
cargo test -p tgin lb::roundrobin   # single module
cargo test test_round_robin_distribution_even -- --nocapture

# Docker
docker build -t tgin .
docker run --rm -v "$PWD/tgin.ron:/etc/tgin/config.ron" -e TOKEN=... -p 3000:3000 tgin

# Example stack (long-poll + webhook bots)
cd examples/simple && TOKEN=... docker compose up --build

# Performance harness
cd tests/performance
make build
make webhook-tgin-scale-2 RPS=1000 DURATION=10   # one scenario
./benchmark.sh                           # full RPS matrix → results/<run_id>.csv (~30 min)
```

The repo has no `rustfmt.toml`, `clippy.toml`, `rust-toolchain*`, or CI config. Use stock `cargo fmt` and `cargo clippy --all-targets` defaults.

## Configuration

`tgin.ron` is RON. `${VAR}` placeholders are substituted from env before parse — a missing var panics in `substitute_env_vars`. Skeleton:

```ron
(
    dark_threads: 6,                                 // tokio worker threads (default 4)
    server_port: Some(3000),                         // None disables all HTTP ingress
    ssl: Some(SslConfig(cert: "...", key: "...")),   // optional rustls
    api: Some(ApiConfig(base_path: "/api")),         // optional management API
    updates: [ LongPollUpdate(token: "${TOKEN}") ],
    route: RoundRobinLB(routes: [
        LongPollRoute(path: "/bot1/getUpdates"),
        WebhookRoute(url: "http://127.0.0.1:8080/bot2"),
    ]),
)
```

Variants must match `RouteConfig` / `UpdateConfig` exactly. The HTTP API `POST /<base_path>/route` body uses `"type": "Longpull"` or `"Webhook"` (matching `AddRouteTypeSch`, **not** "Longpoll").

## Code Conventions & Patterns

- **Composition over inheritance.** New ingress adapter = a struct implementing `Updater + Serverable + Printable`; new route = `Routeable + Serverable + Printable`. The blanket impl wires it into `*Component`. Default trait methods (e.g. `Serverable::set_server` returning the router unchanged) make stubs cheap.
- **Async everywhere via `async_trait`.** Match the pattern; do not introduce sync trait methods on these traits.
- **Shared state**: `ArcSwap<Vec<Arc<dyn RouteableComponent>>>` for mutable route lists, `Arc<Mutex<VecDeque<Value>>>` + `Arc<Notify>` for the long-poll buffer, `AtomicUsize` for round-robin cursor, `Lazy<ArcSwap<HashMap<...>>>` (`once_cell` + `arc-swap`) for `LONGPOLL_REGISTRY`. Reach for the matching primitive rather than introducing a new pattern.
- **Update flow is one direction.** Producers own `Sender<Value>`; the routing tree lives behind `Arc<dyn RouteableComponent>` and is `process`-called from the main loop. Don't bypass the channel.
- **Errors on the egress path are swallowed after best-effort logging** (`WebhookRoute` ignores HTTP failures so one bad downstream can't kill the dispatcher). Preserve this behavior unless explicitly changing the contract — downstreams must be resilient.
- **Token redaction**: when logging or printing anything that may carry a Telegram token, use `utils::defaults::TELEGRAM_TOKEN_REGEX`.
- **`build_route` recurses** through `RouteConfig::{RoundRobinLB,AllLB}` — keep it the single construction site for the routing tree.
- **Naming quirk**: long-poll modules and types are spelled `longpull` (`src/update/longpull.rs`, `src/route/longpull.rs`, `AddRouteType::Longpull`, API `"type":"Longpull"`). User-facing docs say "long poll" / "LongPoll"; do not "fix" the in-code spelling without a deliberate, repo-wide rename.
- **No `unsafe`**, no custom panics outside config loading. Surface failures as `Result<(), ()>` on the `Routeable` mutation methods (the unit-error is intentional — the API logs and continues).
- **Imports** are grouped `crate::` first, then external, with blank lines between groups (see `src/tgin.rs`).

## Testing & QA

- **Unit tests are inline.** Each module ends with `#[cfg(test)] mod tests { ... }`; async tests use `#[tokio::test]`. Examples: `src/lb/roundrobin.rs`, `src/lb/all.rs`, `src/route/longpull.rs`, `src/route/webhook.rs`, `src/update/{longpull,webhook}.rs`.
- **Test doubles** live in `src/mock/routes.rs` (`MockCallsRoute` records every received update in `Arc<Mutex<Vec<Value>>>`). The `mod mock` declaration in `src/main.rs` is `#[cfg(test)]`-gated, so production builds don't see it.
- **HTTP mocking**: `wiremock` for outbound assertions (see `src/route/webhook.rs::tests::test_process_sends_correct_post_request`). `tower::ServiceExt` and `http_body_util` are available via dev-deps for in-process axum requests.
- **Dependencies must not be mocked at the boundary** the system actually crosses — prefer real `wiremock` HTTP servers and real `tokio::sync::mpsc` channels over hand-rolled fakes.
- **Performance / load tests** are not run by `cargo test`. They live under `tests/performance/` and require Docker — see `tests/performance/makefile` and `benchmark.sh`. Results land in `tests/performance/results/<run_id>.csv` and are plotted by `tests/performance/diagram/main.py` (via `uv run main.py`), which auto-picks the newest CSV under `results/`.
- Always exercise the actual change before claiming it passes: run the specific `cargo test <name>` for the touched module, and for HTTP-affecting changes spin up `examples/simple` or a perf scenario.

## Runtime / Tooling

- **Rust** edition 2021, stable toolchain (no `rust-toolchain` pin). Tokio multi-threaded runtime with `dark_threads` workers.
- **Async runtime is fixed**: tokio + axum 0.7 + axum-server 0.8 (TLS via rustls) + reqwest 0.11 (rustls). Do not introduce alternative HTTP / TLS stacks.
- **Config format**: RON 0.12 only. JSON-shaped types in `src/api/schemas.rs` use `serde` + `serde_json`.
- **Python integration** (`integrations/pytgin/`, `examples/simple/`): managed with `uv`, not pip.
- No package manager wrappers, no `Makefile` at the repo root — interact with the project through `cargo` directly.
