# Live gate: tgin-mtproto on the owner's account

Manual checks of tgin's own MTProto engine (sender, update manager,
account supervisor) on the owner's real Telegram account. Nothing here runs
in CI. The driver is the dev example
`crates/tgin-mtproto/examples/live_gate.rs`; it is not part of tgin.

## Identifiers

The owner's identifiers never go into the repository. This runbook names
them by two variables, which the example reads from the environment, else
from `C:/Users/user/.tgin-secrets/m3-gate.env` (`--gate-env` names another
file); the soak's containers get them from the same file:

| Variable | What |
|---|---|
| `TGIN_GATE_USER` | the account's Telegram user id, also the Bot API chat id of its private chat with the test bot |
| `TGIN_GATE_CHANNEL` | the test channel's Bot API id (`-100…`) |

The test bot is the bot whose token is in `bot-token`.

## Rules

- The account is the owner's main account (user `TGIN_GATE_USER`). Every
  live step is a production step.
- Live runs go only from the Windows host binary, one process at a time
  (the session store's lock refuses a second one). The one exception is
  the soak's account container, which then has the session to itself: no
  host run, no `whoami`, nothing else on the session until the soak is
  stopped.
- Test traffic comes only from the test bot: silent
  (`disable_notification`), and deleted once the account has seen it.
  Targets: the bot's private chat (Bot API chat `TGIN_GATE_USER`) and the
  test channel `TGIN_GATE_CHANNEL` (never delete the channel).
- At most 30 messages per kill run and at most 5 runs per check. A
  FLOOD_WAIT is an environment note, never a failure; every sender here
  waits it out.
- Content (probe tags, message ids) is recorded only for the private chat
  and the test channel. Every other chat yields counts, constructor names,
  box kinds and pts, in logs, reports and saved files alike.
- Never print or commit the api hash, the bot token or any part of it, the
  phone, auth keys or session files.

## Paths

| What | Where |
|---|---|
| Session | `C:/Users/user/.tgin-secrets/m3-account/session` (the directory `m3-account` has no inherited ACL: the user, SYSTEM and Administrators only) |
| api id | `C:/Users/user/.tgin-secrets/telegram-api.env` (`TELEGRAM_API_ID`) |
| Bot token | `C:/Users/user/.tgin-secrets/bot-token` |
| Phone (for `whoami`) | `C:/Users/user/.tgin-secrets/m3-account.env` (`TGIN_SPIKE_PHONE`) |
| Identifiers | `C:/Users/user/.tgin-secrets/m3-gate.env` (`TGIN_GATE_USER`, `TGIN_GATE_CHANNEL`) |
| Host check output | `%TEMP%/tgin-live-gate/<check>/` (journals and the account's logs; counts, tags and ids of the two test chats only) |

## Host checks

Build once, from the repository root:

```
cargo build --release -p tgin-mtproto --example live_gate
set G=target\release\examples\live_gate.exe
```

| Check | Command | PASS |
|---|---|---|
| Import | `%G% import` (from `…/m3-spike/session`; refuses a target that already holds an authorization) | the rows read back identical |
| Who am I | `%G% whoami` | `user_id` = `TGIN_GATE_USER`, phone matches, `home_dc=2`, `layer=230` |
| Kill runs | `%G% kill-runs --runs 5` (`--first-run N` to number a later batch) | every probe admitted at least once, in both boxes |
| Backpressure | `%G% backpressure` (40 probes) | every probe recovered; dropped `full` grows by exactly the refused unsequenced updates |
| Catch-up | `%G% catch-up` | the live probe arrives within 10 s of the restart while channels still catch up |

How the kill runs kill: each run starts three account processes in turn.
The first aborts (`std::process::abort`) inside the sink, right after it
has fsynced a probe batch to its journal and before the ingest commits that
batch's state. The second arms, at admission, a thread that aborts the
process after a random delay in [0, 2.5 × the median `commit_state` time
measured on the same disk), while the ingest goes on to merge the batch's
peers and commit its state; afterwards the supervisor reads the stored
state and says whether the abort landed before or after the commit. The
third runs to the end and is stopped through a stop file.

## The soak (7 days)

Four containers from `tests/smoke/live_gate/compose.yaml`:

| Container | Runs | Network |
|---|---|---|
| `tgin-soak-account` | `live_gate soak`: the account under its supervisor, the session bind-mounted | `tgin-soak-net` (the one the cuts disconnect) |
| `tgin-soak-heartbeat` | `live_gate heartbeat`: a silent bot message every 5 min to the private chat and the test channel; deletes each once the account saw it (matched by sequence number, since the private chat's ids differ between the bot and the account; unseen ones after 6 h) | `tgin-soak-bot` |
| `tgin-soak-baseline` | `live_gate baseline`: the same binary and runtime without an account, for the footprint comparison | none |
| `tgin-soak-chaos` | `chaos.sh` with the Docker CLI (`chaos.Dockerfile`, base image pinned by digest) on the host's socket: induced restarts and network cuts | none |

The chaos container holds the host's Docker socket, which is full control
of the Docker daemon: through it, it can read the live session (for
example `docker cp` from `tgin-soak-account`, or a new container mounting
the session directory). Treat it as having read access to the account's
auth key, keep its image pinned, and run nothing else in it. (The soak
started on 2026-10-19 runs a chaos image built before the pin, on the same
Docker 29.7.2 CLI from a private mirror; a rebuild uses the pinned base.)

All four restart `unless-stopped`, so they come back after a Docker Desktop
or host restart. The chaos schedule counts from the first start (kept in
the volume as `chaos-t0`; the last event carried out is `chaos-done`): a
hard restart at 15 min (`docker kill -s KILL`, then `docker start`: the
account gets no SIGTERM), then a restart every 6 h, alternately graceful
(`docker restart -t 10`: SIGTERM, the account closes its session) and
hard; network cuts of 90 s at 30 min and of 600 s at 40 min, then one
every 6 h from 3 h, alternately 90 s and 600 s; no more chaos after 7 days.
The account exits with code 3 when it has seen no heartbeat for 20 min, so
a wedged stream also gets a restart.

Everything lands in the volume `tgin-soak-data`, counts and timings only,
except the two test chats' message ids:

| File | Rows |
|---|---|
| `events.csv` | `unix_ms,event,detail`: `start`, `state` (each state entered), `stop` (SIGTERM), `wedge-exit`, `closed` |
| `status.csv` | every minute: state, updates admitted, dropped undecodable and full, catching up (common, channels), batches, updates by box (common, qts, channel, unsequenced), heartbeats seen, test-chat ids |
| `footprint.csv`, `baseline.csv` | every minute: resident kB, private (anon) kB, threads, CPU ms, fds, sockets |
| `sent.csv`, `seen.csv`, `deleted.csv`, `heartbeat-errors.csv` | heartbeats: sent (`unix_ms,chat,message_id,seq`), seen by the account (`…,latency_ms`), deleted (`seen` or `expired`), send errors |
| `ids.csv` | `unix_ms,chat,message_id,constructor,pts,origin` of every new message the account admitted in the private chat and the test channel |
| `chaos.csv` | `unix_s,kind,detail`: `chaos-start`, `restart-hard`, `restart-graceful`, `cut-start`, `cut-end`, `account-state` (restart count, status, start time, on change), `started` (the account was not running when the chaos container started), `skipped`, `window-end`, `error` |

Container logs (`docker logs tgin-soak-account`) hold the account's own
log lines: names, states, error codes, counts.

### Start

Nothing may hold the session: no host `live_gate`, no tgin on it.

```
docker compose -f tests/smoke/live_gate/compose.yaml build
docker compose -f tests/smoke/live_gate/compose.yaml up -d
docker ps --filter name=tgin-soak- --format "{{.Names}}\t{{.Status}}"
```

### Read the evidence

The report, for the first 24 h from the account's first start (or the
whole week with `--hours 168`; `--from-ms` moves the window):

```
docker run --rm -v tgin-soak-data:/data tgin-live-gate:soak soak-report --data /data --hours 24
docker run --rm -v tgin-soak-data:/data tgin-live-gate:soak soak-report --data /data --hours 168
```

It prints, for the window: heartbeats sent, seen and missing per chat
(those sent in the last 10 min are not counted as missing yet) with
latency p50/p95/max; deletions; account events and states entered; every
chaos event; the last status row; the account's and the baseline's
footprint (resident and private MB first/last/max, threads, sockets, fds,
CPU seconds and share of one core); and the number of distinct message ids
received in each test chat.

Live views: `docker logs --tail 50 tgin-soak-account`,
`docker logs tgin-soak-chaos`, and
`docker run --rm -v tgin-soak-data:/data --entrypoint "" tgin-live-gate:chaos tail -n 20 /data/chaos.csv`.

### Received message ids (comparison with another session)

`ids.csv` records every new message the account admitted in the private
chat and the test channel. To list the distinct ids of the first 24 h, one
`<chat> <id>` per line (`private` or `channel`):

```
docker run --rm -v tgin-soak-data:/data tgin-live-gate:soak soak-report --data /data --hours 24 --ids-out /data/ids-24h.txt
docker run --rm -v tgin-soak-data:/data --entrypoint "" tgin-live-gate:chaos cat /data/ids-24h.txt > ids-24h.txt
```

The private chat's ids are the account's own message ids, the same in
every session of the account; the channel's are the channel's. A reference
session of the same account sees the private chat through
`updates.getDifference` from a cursor taken at the soak's start (for the
soak started 2026-10-19T22:06:35Z: `pts 3268746, qts 0, date 1792447602`,
taken at 22:06:42Z through the controller's own session); the
channel's messages come through the channel's own history
(`messages.getHistory` on `TGIN_GATE_CHANNEL` over the same 24 h), since a
channel's posts are not in the common difference. The heartbeats are
deleted once seen, so the reference must read updates (new and deleted
messages), not only today's history. Compare ids only.

### Stop cleanly

```
docker compose -f tests/smoke/live_gate/compose.yaml stop
docker compose -f tests/smoke/live_gate/compose.yaml down
```

`stop` sends SIGTERM (10 s grace): the account closes its session and
writes `stop` and `closed` to `events.csv`. `down` removes the containers
and networks and keeps the volume. Only then may a host run use the
session again (`live_gate whoami` is the first one). Read the report
before removing the evidence with `docker volume rm tgin-soak-data`.
