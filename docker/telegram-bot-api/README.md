# telegram-bot-api image

The official [Telegram Bot API server](https://github.com/tdlib/telegram-bot-api),
built from source by this repository, for running tgin against a self-hosted
server. `examples/self-hosted` shows it next to tgin.

| | |
|---|---|
| Upstream commit | [`e3e9dd8e5b3d7ab8537cd5a10dc31d5ffa8f82d1`](https://github.com/tdlib/telegram-bot-api/commit/e3e9dd8e5b3d7ab8537cd5a10dc31d5ffa8f82d1) (2026-08-25), with `td` at `bc9c263e2bfee06aaab41e82db51a103376030bc` |
| Bot API version | 10.3 |
| Base | `alpine:3.24.2`, pinned by digest, for both stages |
| User | `10001:10001` (`telegram-bot-api`) |
| Entrypoint | `telegram-bot-api`, no default arguments |
| Volume | `/var/lib/telegram-bot-api`, also the working directory |
| Port | 8081 (`EXPOSE` only; never publish it) |
| License | Boost Software License 1.0 (upstream) |

The labels carry the same facts: `org.opencontainers.image.revision` is the
upstream commit and `org.opencontainers.image.version` the Bot API version.
The image is not published; CI (`.github/workflows/bot-api-image.yml`) builds
it on amd64 and arm64 with an SBOM and provenance.

## Build

```sh
docker build -t tgin-telegram-bot-api:local docker/telegram-bot-api
docker run --rm tgin-telegram-bot-api:local --version   # Bot API 10.3
```

The builder fetches the upstream commit by its full SHA and checks it out;
the `td` submodule comes at the commit upstream pins. The build fails if the
binary reports a Bot API version other than `BOT_API_VERSION`.

| Build argument | Default | Meaning |
|---|---|---|
| `TELEGRAM_BOT_API_COMMIT` | the commit above | Full SHA of `tdlib/telegram-bot-api` to build |
| `BOT_API_VERSION` | `10.3` | The version that commit reports; checked |
| `BUILD_JOBS` | one per CPU | Parallel compile jobs. TDLib needs a few GB of memory per job: lower it on a small machine |

A cold build takes a while: TDLib is a large C++ code base.

**Updating:** upstream has no tags or releases; each Bot API release is an
"Update version to X" commit. Pick the commit, set both
`TELEGRAM_BOT_API_COMMIT` defaults (builder and runtime stage) and both
`BOT_API_VERSION` defaults to it, and update the table above. Dependabot
updates the Alpine digest.

## Run

The server needs an api_id and api_hash from <https://my.telegram.org>. The
image has none: pass `--api-id`/`--api-hash`, or set `TELEGRAM_API_ID` and
`TELEGRAM_API_HASH`. Without them the server exits with "You must provide
valid api-id and api-hash".

```sh
docker run -d --name telegram-bot-api --network private-net \
  -e TELEGRAM_API_ID -e TELEGRAM_API_HASH \
  -v bot-api-state:/var/lib/telegram-bot-api \
  -v bot-api-files:/var/lib/telegram-bot-api/files \
  tgin-telegram-bot-api:local \
  --local --dir=/var/lib/telegram-bot-api --files-dir=/var/lib/telegram-bot-api/files
```

Arguments after the image name replace the (empty) default, so give every
flag you need. `docker run --rm tgin-telegram-bot-api:local --help` lists all
of them.

| Flag | Default | Notes |
|---|---|---|
| `--api-id`, `--api-hash` | `TELEGRAM_API_ID`, `TELEGRAM_API_HASH` | Required. Prefer the variables: flags show up in the host's `ps`. Both show up in `docker inspect`, so limit who can reach the Docker API |
| `--local` | off | Local mode: uploads up to 2000 MB, downloads of any size, `getFile` answers an absolute path on the server, `file:` uploads read the server's own files, and webhooks may use plain HTTP, any port and private addresses |
| `-d`, `--dir` | `./` (the volume) | Working directory: each bot's TDLib database (its Telegram session) in `<dir>/<token>/`, the update queue `tqueue.binlog` and `webhooks_db.binlog` |
| `-f`, `--files-dir` | `--dir` | Each bot's files in `<files-dir>/<token>/`. Point it at its own volume so that tgin can read files without seeing the databases |
| `-t`, `--temp-dir` | the system temporary directory | Where uploads are spooled; a relative path is under `--files-dir`. It must hold the largest upload |
| `--http-ip-address` | `0.0.0.0` | Bind address of the Bot API listener |
| `-p`, `--http-port` | `8081` | Port of the Bot API listener |
| `-s`, `--http-stat-port` | off | Statistics page. Leave it off, or bind it to `127.0.0.1` with `--http-stat-ip-address` |
| `--http-stat-ip-address` | `0.0.0.0` | Bind address of the statistics page |
| `--max-webhook-connections` | 40, 100 in local mode | Default `max_connections` of a webhook |
| `--filter` | all bots | `<remainder>/<modulo>`: serve only bots whose id gives that remainder, to split bots across servers |
| `-v`, `--verbosity` | `0` | Log verbosity; logs go to stderr unless `--log` names a file |
| `--proxy` | none | HTTP proxy for outgoing webhook requests |

The image runs as `10001:10001`; `-u`/`-g` (switch user) are not needed.

A bot's directory is named after its full token, `:` included, as in
`123456:AAE…/`. The server switches to `~` (`123456~AAE…/`) only when its
startup check finds that the filesystem rejects `:` in names; bots on the
test DC get a `:T` suffix. Treat these names as secrets: they are the tokens.

TDLib creates the bots' directories with mode 0750 and their files with
0640 (under the default umask 0022), and the image creates `files` with
0750 and the working directory with 0700. A process that reads the files
volume (tgin) therefore needs only group 10001 (`group_add`), not the
server's uid; the databases stay private to the server.

### Volumes

| Path | Holds | Who mounts it |
|---|---|---|
| `/var/lib/telegram-bot-api` (`--dir`) | TDLib databases (bot sessions), pending updates, webhook settings | The server only. It survives restarts: losing it logs every bot out of this server and drops its pending updates |
| `/var/lib/telegram-bot-api/files` (`--files-dir`) | Downloaded and uploaded files | The server read-write; tgin read-only, at the same path |

A new named volume takes its owner (10001) and mode from the image, which
creates both directories. A bind mount must be owned by 10001, with group
read access on the files directory.

## Security

The server has no authentication beyond the bot token in the URL, and no
TLS.

- **Never expose the Bot API port.** Anyone who reaches it with a token
  controls that bot: reads its updates, sends as it, logs it out. Anyone who
  reaches it with any valid token makes the server log in as that bot with
  your api_id. Keep it on a private network that only tgin can reach; do not
  publish the port.
- **Never expose the statistics port.** It is off by default. When enabled
  it is unauthenticated, lists bots by token prefix and lets callers change
  the log verbosity.
- **Local mode reads files.** A `file:///path` value in a request makes the
  server read and upload any file it can read, including other bots'
  databases. Only trusted callers may reach a local-mode server.
- **Local mode sends webhooks anywhere**, including private addresses: a
  caller that can set a webhook can make the server post to your internal
  network. Limit its egress.
- **One server per token set.** A bot must be logged in on one server only:
  the server owns its update queue, and two servers for one token split and
  lose updates. Many bots can share a server, or be split across servers
  with `--filter`; never run one server per tgin replica.
- The api_id and api_hash belong to your Telegram account: keep them out of
  images, command lines and version control.
