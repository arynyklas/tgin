# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Until 1.0, minor releases (0.x.0) may contain breaking changes.

## [Unreleased]

## [0.2.0] - 2026-10-14

### Security

- Bot tokens no longer leak into logs or startup banners. HTTP client errors
  are logged without their request URL (which carries the token), and the
  `WebhookRoute` banner redacts its URL like the other banners.
- `WebhookUpdate` now sends its `secret_token` to `setWebhook` when it
  registers the webhook, so Telegram includes the
  `X-Telegram-Bot-Api-Secret-Token` header that tgin checks. Previously a
  configured secret was never registered. `secret_token` must be 1-256
  characters of `A-Za-z0-9_-`, as Telegram requires.
- tgin warns at startup for every `WebhookUpdate` without a `secret_token`:
  anyone who can reach that path can inject forged updates.
- The management API can no longer run without authentication: enabling `api`
  requires `auth_token`, and every `/api/*` request must send
  `Authorization: Bearer <auth_token>`.

### Fixed

- Shutdown no longer loses updates that were already acknowledged to
  Telegram. On a shutdown signal tgin reports not-ready on `/readyz`, keeps
  accepting connections for a short lame-duck period (5 s), stops ingesting,
  answers webhook ingress with `503` and `Retry-After`, and keeps serving
  buffered updates to long-poll routes until they are drained or a 15 s grace
  period ends. Parked long polls return an empty batch right away.
- Long-poll ingress recovers from `409 Conflict` instead of retrying it as a
  transient error. If a webhook is active, tgin deletes it (at most once per
  60 s) and resumes polling; if another process polls the same token, tgin
  backs off and logs that only one poller per token can run.
- tgin exits with code 2 when every long-poll ingress has failed permanently
  and there is no webhook ingress, instead of running with nothing to route.
- Long-poll routes clamp `getUpdates` parameters to Telegram's bounds: `limit`
  defaults to 100 and is clamped to 1-100, `timeout` is capped at 50 s.
- HTTP paths that the router would panic on or treat as captures (no leading
  `/`, `{` or `}`, a segment starting with `:` or `*`, or `api.base_path` set
  to `/`) are rejected by config validation and by the management API (`400`).

### Changed

- **Breaking:** unknown fields in the configuration are rejected at startup.
  A misspelled key such as `server_prot` used to be silently ignored.
- **Breaking:** enabling the management API (`api`) requires a non-blank
  `auth_token`. The shipped example configs read it from `TGIN_AUTH_TOKEN`.
- **Breaking:** the minimum supported Rust version is now 1.88
  (`rust-version = "1.88"`). The `icu_*` 2.3 crates pulled in by `url`/`idna`
  require it.
- **Breaking:** the container image is a static musl binary on distroless and
  runs as the numeric user and group `65532:65532`. Mounted configuration
  files (and TLS keys) must be readable by uid 65532.
- Linux musl builds (the static release archives and the container image) use
  mimalloc as the global allocator; musl's allocator is slow under
  multi-threaded load.
- The `examples/simple` compose stack publishes tgin on `127.0.0.1` only.

### Added

- `GET /livez` (always `200`) and `GET /readyz` (`200`, or `503` while
  draining) probes. They stay unauthenticated when `auth_token` is set.
- Graceful shutdown on Windows console events: Ctrl+C, Ctrl+Break, console
  close and system shutdown.
- `tgin --version`.
- Multi-arch (amd64, arm64) container image at `ghcr.io/arynyklas/tgin`,
  tagged `<version>`, `<major>.<minor>` and `latest`, with a build provenance
  attestation.
- Release binaries for Linux (glibc and static musl), macOS and Windows on
  x86_64 and aarch64, with shell and PowerShell installers, SHA-256 checksums,
  a CycloneDX SBOM, embedded dependency metadata (`cargo auditable`) and
  build provenance attestations.

### Dependencies

- All Rust dependencies moved to their latest releases, including axum 0.8,
  axum-server 0.8, reqwest 0.13 and rand 0.10. reqwest is built with rustls
  only, so OpenSSL, native-tls and ring are out of the build graph; one
  aws-lc-rs provider is shared by the HTTP client and the TLS listener.
  `once_cell` was replaced by `std::sync::LazyLock`.
- Known advisories against the lockfile went from 11 affected packages to 0.
- The benchmark harness, examples and Docker base images were updated as well.

### Docs

- README, DOCS, AGENTS and PERF now describe the actual behaviour: delivery
  semantics, shutdown and exit codes, configuration strictness, probes,
  container usage and the known limits of the benchmark numbers.
- Added `SECURITY.md` (supported versions, private reporting, verifying
  release attestations) and this changelog.

[Unreleased]: https://github.com/arynyklas/tgin/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/arynyklas/tgin/releases/tag/v0.2.0
