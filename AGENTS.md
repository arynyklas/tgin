# Repository Guidelines

`tgin` routes Telegram Bot API updates to downstream bot processes ("NGINX for Telegram bots"). One binary reads a YAML config (`version: 1`), ingests each bot's updates by long poll or webhook, routes every update by the bot's rules into named consumers, and serves them: **pull** consumers through a Telegram-shaped `getUpdates` endpoint, **push** consumers by POSTing to HTTP endpoints. `README.md` and `DOCS.md` are the user documentation; `DOCS.md` is the reference for runtime behaviour (delivery semantics, reload, admin API, metrics, shutdown, exit codes), and `docs/config-reference.md` is generated from the config model.

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

1. **Sources** (`crates/tgin-botapi`): `LongPollSource` polls `getUpdates` and offers each update to a `Sink`, moving its offset past an update only once it is admitted; on backpressure it holds the batch. `webhook_router` serves one `POST <path>` per webhook bot, checks allowlist and secret on the head alone, and answers `503` + `Retry-After` on backpressure. `facts::extract` reads the routing facts from the raw JSON without building a `serde_json::Value`.
2. **Engine** (`crates/tgin-core/src/engine.rs`, `admission.rs`): `Sink::admit` routes with the current `RouteTable` (swapped atomically on reload) and reserves room in every target (`backpressure` targets first, then `drop_oldest`), then commits; one queue lock at a time, nothing awaits.
3. **Consumers** (`crates/tgin-core/src/consumer/`): `PullConsumer::poll` acks by offset, returns held leases, else leases new updates and parks until the timeout; `PushConsumer` runs one dispatcher per consumer that leases up to `max_in_flight` and delivers with retries, per-endpoint breakers and balancing.
4. **App** (`crates/tgin/src`): `app.rs` assembles the engine, the admin plane, the data plane and the sources, and runs the shutdown sequence; `data_server.rs` serves webhook routes and `/c/{consumer}/getUpdates` plus aliases through an `ArcSwap<Router>` that reloads replace; `admin/` is the admin plane (probes, `/metrics`, `/v1/*`) and `tgin admin`; `reload.rs` serializes reloads (SIGHUP, `POST /v1/reload`, file watcher) and applies them through `Engine::apply` (`crates/tgin-core/src/reload.rs`); `sources.rs` starts, stops and restarts bot sources; `migrate/` is `tgin config migrate` (RON, the format of the original tgin, → YAML).

## Workspace Layout

| Path | Role |
| --- | --- |
| `Cargo.toml` | Virtual workspace: `[workspace.package]` (version `1.0.0-dev`, edition 2024, `rust-version = "1.89"`), shared `[workspace.dependencies]`, `[workspace.lints]` (`unsafe_code = "deny"`, clippy `all`), `[profile.release]` (`lto = true`, `codegen-units = 1`) and `[profile.dist]`. `tests/performance/benchmark` is excluded (own `Cargo.lock`). No `default-members`: bare `cargo test` tests every crate. |
| `crates/tgin-runtime/` | Generic building blocks with no tgin domain knowledge: `queue` (`LeaseQueue`: bounded, leases, acks, per-key ordering), `supervisor` (restarting tasks), `lifecycle` (phases, signals, shutdown timing), `http` (hardened HTTP/1.1 server with TLS reload, outbound client, URL-free error rendering), `obs` (tracing with a redacting writer, metrics registry, readiness), `config` (YAML parsing with locations, `${env:…}`/`${file:…}` interpolation, file watcher), `secret` (`Secret`, `Redactor`, constant-time compare), `backoff`, `breaker`. |
| `crates/tgin-core/` | The engine: `config/` (YAML model, validation, resolution, JSON Schema), `rules/` (compiled route table), `engine.rs` + `admission.rs`, `consumer/{pull,push}.rs`, `reload.rs` (config diff and live apply), `metrics.rs` (every metric family and pre-resolved handles). |
| `crates/tgin-botapi/` | Telegram side: `client.rs` (Bot API client), `facts.rs` (fact extraction from raw JSON), `longpoll.rs`, `webhook.rs`, `secret.rs` (derived webhook secret); `benches/` and `tests/alloc_budget.rs` (hot-path gates). |
| `crates/tgin/` | The `tgin` binary (dist builds it via `[package.metadata.dist] dist = true`): `cli.rs`, `app.rs`, `data_server.rs`, `admin/`, `reload.rs`, `sources.rs`, `migrate/`, `exit.rs`. `main.rs` declares mimalloc as the global allocator on musl only. |
| `xtask/` | `cargo xtask schema [--check]` writes `schema/tgin.v1.schema.json`; `cargo xtask config-docs [--check]` writes `docs/config-reference.md` from that schema (`.cargo/config.toml` holds the alias). |
| `schema/tgin.v1.schema.json`, `docs/config-reference.md` | Generated from the doc comments of `crates/tgin-core/src/config/model.rs` (and the hand-written schemas in `config/schema.rs`). Never edit them: change the source, then run both xtask commands. The tests `schema_is_up_to_date` (tgin-core) and `config_reference_is_up_to_date` (xtask), and CI, check them. |
| `tgin.yaml` | Root example config (one long-poll bot, one pull consumer; reads `${env:TOKEN}`). |
| `Dockerfile` | Builder `rust:1.99.0-alpine3.24`, runtime `gcr.io/distroless/static-debian13`, both pinned by digest; `cargo build --release --locked -p tgin` (static musl), `TGIN_COMMIT` from the `REVISION` build arg; uid/gid `65532:65532`; `EXPOSE 8080 9090`; `ENV TGIN_CONFIG=/etc/tgin/tgin.yaml`; exec-form `HEALTHCHECK` running `tgin admin health`; `ENTRYPOINT ["/usr/local/bin/tgin"]`, `CMD []`. |
| `examples/simple/` | Compose stack: one bot, two pull consumers polled by aiogram bots, one push consumer. `tgin.ron` is the same stack as a RON config, kept as `tgin config migrate` input and test fixture. |
| `integrations/pytgin/` | aiogram shim that points `getUpdates` at a tgin URL. |
| `tests/performance/` | Load-test harness (see Testing). `cfgs/v1/*.yaml` are the 1.0 scenario configs, `cfgs/*.ron` the RON configs for the `v0` engine. |

### Crate boundaries

Dependencies point one way: `tgin-runtime` ← `tgin-core` ← `tgin-botapi` ← `tgin` (and `xtask` → `tgin-core`). `tgin-runtime` knows nothing of Telegram or of the config model; `tgin-core` knows nothing of the Bot API's HTTP shapes (sources reach it only through the `Sink` trait and `Facts`); `tgin-botapi` does not know the app. Put code in the lowest crate whose concepts it needs; never add an upward dependency.

## Important Files

- `.github/workflows/ci.yml` — every check listed under [CI checklist](#ci-checklist).
- `.github/workflows/release.yml` — generated by dist (cargo-dist 0.32.0) from `dist-workspace.toml` (8 targets), with hand edits listed in its header. To regenerate, follow the steps in the `allow-dirty` comment in `dist-workspace.toml`, then re-apply the hand edits. Dry run: `dist plan --tag v1.0.0-dev` (the tag must match the package version). The release notes come from the matching `CHANGELOG.md` section; never write credential-shaped text there (e.g. `Bearer` followed by a word): the Actions runner masks it and drops any job output that contains it.
- `.github/workflows/image.yml` — on `v*` semver tags, builds amd64/arm64 images natively, publishes the multi-arch index to `ghcr.io/arynyklas/tgin` and attests it; `latest` goes only to the highest stable `vX.Y.Z`.
- `deny.toml` — cargo-deny: advisories, license allow-list, bans on `openssl`, `openssl-sys`, `native-tls`, `ring`; crates.io only.
- `crates/tgin-botapi/benches/` and `crates/tgin-botapi/tests/alloc_budget.rs` — hot-path benchmarks over the 1000 updates in `crates/tgin-botapi/tests/fixtures/updates.jsonl`, routed by `crates/tgin-botapi/tests/fixtures/hot_path.yaml` (shared setup in `crates/tgin-botapi/benches/common/mod.rs`). `hot_path.rs` is criterion (`cargo bench -p tgin-botapi -- --quick`). `hot_path_ir.rs` is gungraun under Callgrind (Linux, needs Valgrind and `gungraun-runner`), with a hard `Ir` limit per benchmark set to 1.5× the measured count. `alloc_budget.rs` is the allocation gate: a thread-local counting allocator asserting at most 2 allocations per update on the warm path. The gungraun limits are re-measured (Docker `rust:<version>` with `--security-opt seccomp=unconfined`) whenever the toolchain pinned in `bench-gate` changes: limits and toolchain move together.
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

# Performance harness (ENGINE=v1 default or v0 = 1af6aaa, the last commit before the engine rewrite;
# BOT_IMPL=rust default, py = the aiogram bots)
cd tests/performance
make build && make image ENGINE=v1
make webhook-tgin-3 RPS=10000 DURATION=30            # one scenario
ENGINE="v0 v1" ./benchmark.sh      # both engines, 6 scenarios × 3 rates × 3 runs (~70 min)
./rerun-invalid.sh results/<run_id>.csv              # rerun runs with lag_p99_ms > 0.5
```

There is no `rustfmt.toml`, `clippy.toml` or `rust-toolchain*` file: CI uses stock `rustfmt` and `clippy` on stable, warnings as errors; `bench-gate` pins its own toolchain.

## Code Conventions & Patterns

### Hot path

The ingest path (getUpdates body → `split_updates` → `extract` → `Engine::admit` → queue commit) is gated by `alloc_budget.rs` (≤ 2 allocations per update on the fixture) and, with queue commit, lease and ack, by `bench-gate` (instruction counts). Egress (`data_server.rs::batch_response`, push delivery) is not gated. Along these paths:

- **Payloads stay `Bytes`.** An update is its raw JSON from ingress to egress, never re-serialized; a pull response is assembled from the payloads as body chunks without copying (`data_server.rs::batch_response`). Facts are extracted once, borrowing from the payload. Ingress copies each `getUpdates` body and each single-frame webhook body once, to its exact size, so queued updates never keep hyper's read buffers alive; the updates of one `getUpdates` body share it.
- **Metrics handles are pre-resolved.** `CoreMetrics::source(name)` / `::consumer(name)` resolve every label combination once; the hot path increments handles and never hashes labels. A metric has one owner (e.g. only `Engine::admit` counts `tgin_routes_unmatched_total`).
- **No awaits and one lock at a time in admission.** Reserve, then commit; cancel reservations on refusal.
- **Rules are compiled.** `RouteTable::compile` does the work at load time; `route` writes into reusable scratch and does not allocate.
- **Swap, don't lock, for config-derived state.** The engine state, route table, data-plane router, running config and redactor sit in `ArcSwap`s that reloads replace.

### Secrets and redaction

- Secrets are `Secret<T>` (never shown by `Debug`/`Display`/serde); config URLs and paths are `Sensitive<T>`, used through `.expose()` and rendered masked (URLs as their origin only). Log through `tracing`, whose writer passes every line through the current `Redactor`; never print config text or URLs to stderr directly.
- Render `reqwest` errors with `tgin_runtime::http::describe_error` (no URL). Config errors never quote secret values or source lines.
- Every constant-time compare goes through `tgin_runtime::secret::secret_eq`.
- Outbound HTTP clients come from `tgin_runtime::http::client`, which never follows redirects; `tgin admin` asks it for `no_proxy`, so the admin token never passes a proxy.

### Config model

`crates/tgin-core/src/config/model.rs` keeps line and column for every error, so it uses no `#[serde(untagged)]`, no internally tagged enums and no `flatten` (they make serde-saphyr buffer and lose locations); string-or-object fields have hand-written visitors and schemas (`schema.rs`). Every field is either applied live by a reload or listed in `restart_required` (`crates/tgin-core/src/reload.rs`). Doc comments are the user-facing descriptions: they become the JSON Schema and `docs/config-reference.md`.

### Errors and tasks

- Errors are typed (`thiserror`); `expect` only for invariants. Exit codes live in `crates/tgin/src/exit.rs`.
- Long-running tasks run under `tgin_runtime::supervisor::Supervisor` (restart with backoff on `Exit::Retry` or panic, `Failed` on `Exit::Fatal`); graceful stops use `CancellationToken`s, never the supervisor's abort token.

### Unsafe

`unsafe_code` is denied workspace-wide. The only exception is `crates/tgin-botapi/tests/alloc_budget.rs` (`#![allow(unsafe_code)]`), whose counting `GlobalAlloc` forwards to `System` with a `// SAFETY:` comment on each block. New unsafe code needs the same: a test-only or proven need, a scoped `allow`, and a `SAFETY` comment per block.

### Style

Imports follow the grouping of the file you edit; `cargo fmt` sorts within groups. Comments say why, not what.

## Testing & QA

- **Unit tests** are inline (`#[cfg(test)] mod tests`); **integration tests** live in each crate's `tests/`: `tgin-core/tests/{config,engine,push}.rs`, `tgin-botapi/tests/{longpoll,webhook,alloc_budget}.rs`, `tgin-runtime/tests/{http,watch}.rs`, `tgin/tests/{e2e,admin,migrate,panic_hook}.rs`.
- **Real boundaries.** HTTP peers are real servers (`wiremock`, or axum on `127.0.0.1:0`); config tests use `MapEnv` instead of mutating the process environment.
- **Paused clock.** Timer-driven logic (queue leases, supervisor backoff, engine drains, long-poll backoff) is tested on a paused clock (`#[tokio::test(start_paused = true)]` or `tokio::time::pause()`), so tests take no wall time and do not flake on slow machines; use real time only where real I/O has to make progress, with bounded waits.
- **Child processes bind port 0.** `crates/tgin/tests/support/mod.rs::spawn_listening` starts the `tgin` binary with a config whose listeners use port 0 and reads the admin plane's bound address from the `admin plane listening` log line; it removes `RUST_LOG` from the child's environment (it would hide that info line). Never pick a port by binding and releasing it: another socket can take it before the child binds.
- **Signals.** Unix tests deliver real signals to a child process with `kill`; other app tests feed signals through `Signals::from_channel`.
- **Property tests** (`proptest`) cover the queue's invariants (`tgin-runtime/src/queue.rs`) and fact extraction against a `serde_json::Value` reference (`tgin-botapi/src/facts.rs`).
- **Performance / load tests** are not run by `cargo test`. They live under `tests/performance/` and require Docker, `make`, `curl` and `git` — see `tests/performance/makefile` and `benchmark.sh`. `ENGINE` selects the engine (`v1` default, `v0` = built from `1af6aaa`, this project's last commit before the engine rewrite, or both: `ENGINE="v0 v1"` alternates them run by run); `BOT_IMPL` selects the downstream (`rust` default = `tgin-bench bot`, 50 000+ updates/s; `py` = the aiogram bots, ~2 000/s, which saturate before tgin does). A full pass over both engines is 6 scenarios × 3 RPS values (1 000, 5 000, 10 000) × 3 runs of 30 s plus container start-up, about 70 min; the generator is open loop and measures latency from each update's scheduled time. Results land in `tests/performance/results/<run_id>.csv` (13 columns, see PERF.md) next to `<run_id>.log` (per-run counters) and `<run_id>.stats.log` (`docker stats`). A run with `lag_p99_ms` above 0.5 measured the host, not the engine: `./rerun-invalid.sh results/<run_id>.csv` reruns those. Charts come from `tests/performance/diagram/main.py` (`uv run main.py [CSV ...]`); without arguments it plots the `results/*.csv` whose run id sorts last, and it deletes every PNG in `diagram/generated/` before rendering. Run the matrix on an otherwise idle machine (no cargo builds); PERF.md lists the machine, the v0/v1 gate and the harness's known limits.
- Exercise the actual change before claiming it works: run the touched crate's tests, and for HTTP-facing changes a binary (`examples/simple`, or `tgin` against a fake Bot API).

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

CI also builds the Docker image on native amd64 and arm64 runners, runs clippy and tests for `tests/performance/benchmark`, and runs `bench-gate`: `cargo bench -p tgin-botapi --bench hot_path_ir` on a pinned toolchain under Callgrind (`gungraun/setup-gungraun`), failing past any benchmark's `Ir` limit.

## Runtime / Tooling

- **Rust** edition 2024 in every crate, MSRV 1.89 (checked in CI), no toolchain pin. Tokio multi-threaded runtime with `runtime.worker_threads` workers (`auto` = one per CPU). Linux musl builds use mimalloc.
- **HTTP / TLS stack is fixed**: tokio, axum 0.8 + axum-server 0.8 (HTTP/1.1 only, rustls with ALPN `http/1.1`), reqwest 0.13 (`default-features = false`, rustls); one aws-lc-rs crypto provider. OpenSSL, native-tls and ring are banned in `deny.toml`.
- **Config**: YAML through serde-saphyr; JSON Schema through schemars. RON (`ron`) is used only by `tgin config migrate` to read RON configs.
- **Python**: `examples/simple/` and `tests/performance/diagram/` are `uv` projects with a committed `uv.lock`; the perf bots (`tests/performance/bot/`) install `requirements.txt` with pip; `integrations/pytgin/` is a setuptools package without a lock file.
- No package-manager wrappers and no root `Makefile`: work through `cargo` (the perf harness has its own `makefile`).
