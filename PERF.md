# Performance and benchmarks

tgin 1.0 (milestone M1) against tgin 0.2 (commit `1af6aaa`), measured on one machine in one session on 2026-10-16. The short version:

- **tgin's own cost is a fraction of a millisecond.** At 10 000 updates per second, the median end-to-end latency through tgin is 0.24–0.28 ms and p99 is 0.52–0.65 ms, against 0.13–0.15 ms and 0.33–0.46 ms for a bot the generator talks to directly.
- **tgin 1.0 matches tgin 0.2 at every point of the matrix** (1 000, 5 000 and 10 000 RPS × {webhook → push, long poll → pull} × {1, 3 downstreams}): delivered throughput is identical (every update answered in every run) and p99 is between 0.88× and 1.11× of 0.2's.
- **tgin 1.0 uses less CPU than 0.2 on the pull path** (long poll, 10 000 RPS: 1.00–1.01 cores against 1.17–1.19 with one bot, 1.27–1.29 against 1.40–1.46 with three) and about the same on the push path.
- The downstream bots are a Rust implementation (`tgin-bench bot`) that answers 50 000–80 000 updates per second, so these numbers measure tgin, not the bots. The earlier aiogram-bot matrix measured queueing in front of saturated Python processes; see [Python bots](#python-bots).

## Contents

- [Methodology](#methodology)
- [Machine](#machine)
- [The gate](#the-gate)
- [Baselines without tgin](#baselines-without-tgin)
- [Charts](#charts)
- [What was fixed to get here](#what-was-fixed-to-get-here)
- [`max_in_flight`](#max_in_flight)
- [Python bots](#python-bots)
- [Known limits of the harness](#known-limits-of-the-harness)
- [Reproducing the benchmarks](#reproducing-the-benchmarks)

## Methodology

Everything runs in Docker Compose (`tests/performance/`) on one bridge network:

- **`tgin-bench`** (Rust, `tests/performance/benchmark`) is the load generator and a fake Telegram Bot API (`getMe`, `deleteWebhook`, `getUpdates`, `sendMessage`). It runs as a server for the container's whole life; `tgin-bench run` asks it for one measured run.
- **The engine under test**: the 0.2 image (RON config, `tests/performance/cfgs/*.ron`) or the 1.0 image (YAML config, `tests/performance/cfgs/v1/*.yaml`), both built by `make image`.
- **The downstream bots**: `tgin-bench bot` (`BOT_IMPL=rust`, the default). In webhook mode it answers 200 at once and sends the reply in the background; in long-poll mode it polls like a framework does (`offset` = last `update_id` + 1, `limit` 100, `timeout` 30 s) and answers each update with `sendMessage`. The aiogram bots are still there under `BOT_IMPL=py`.

Scenarios:

| Scenario | Path of an update |
|---|---|
| `webhook-direct-1` | generator → bot's webhook (no tgin) |
| `webhook-tgin-1`, `webhook-tgin-3` | generator → tgin webhook ingress → push to 1 or 3 bots, round robin |
| `longpoll-direct-1` | bot long-polls the fake Bot API (no tgin) |
| `longpoll-tgin-1`, `longpoll-tgin-3` | tgin long-polls the fake Bot API → 1 or 3 bots poll tgin (1.0: pull consumer `/c/bots/getUpdates`; 0.2: `/botN/getUpdates` routes) |

Each run:

1. **Warm-up.** 30 unmeasured updates must all come back before measuring starts; this proves that tgin and every bot are live.
2. **Open-loop send phase, 30 s.** Update `i` is due at `start + i / rps`. A dedicated thread sleeps until 2 ms before each due time and spins the rest, then hands the update off without waiting for any response. A late send goes out at once and keeps its due time. Webhook sends hold one of 10 000 in-flight permits; when none is free, the send is not issued and counts as `dropped_by_generator`. At most 100 of the issued sends are on the wire at once ([why](#what-was-fixed-to-get-here)); the rest wait for a connection, and that wait counts in their latency.
3. **Latency** runs from the **due time** to the bot's `sendMessage` for that update, so queueing anywhere, including in the generator, counts (no coordinated omission).
4. **Completion wait.** After the send phase the run waits until every sent update is answered or failed, or 30 s pass. What is still missing then is `unreceived`; it never enters the latency figures.

The matrix: 1 000, 5 000 and 10 000 target RPS × {webhook → push, long poll → pull} × {1, 3 bots}, 3 runs per point and engine, the two engines alternating run by run, the direct baselines once per rate and run.

CSV columns (`tests/performance/results/*.csv`):

| Column | Meaning |
|---|---|
| `scenario`, `engine`, `target_rps` | The point. Direct baselines carry the engine label of the run that produced them but involve no tgin. |
| `achieved_rps` | Updates sent ÷ send-phase wall time. Below target when the generator dropped sends or fell behind. Long-poll sends only append to the fake API's queue, so they always reach the target. |
| `delivered_rps` | Updates answered ÷ (last answer − first due time). |
| `p50_ms` … `max_ms` | Latency of answered updates, from the due time. |
| `unreceived` | Sent but not answered by the end of the completion wait. |
| `duplicate_replies` | Further answers to an already answered update (0 in every run). |
| `dropped_by_generator` | Webhook sends never issued because 10 000 requests were already in flight. |
| `lag_p99_ms` | p99 of how late the generator handed updates off. Undisturbed runs are 0.02–0.10 ms. |

Alongside each CSV, `<run_id>.log` holds every run's counter summary (sent, received, errors, lag, completion wait, `getUpdates` calls) and `<run_id>.stats.log` a `docker stats` sample of the whole stack every 5 s, with the start and end of each run marked.

**The gate** (milestone M1's exit criterion), over the medians of 3 valid runs per point and engine:

- **Throughput.** 1.0's `delivered_rps` ≥ 0.2's — except that the requirement is met outright when, in every run of both engines, `unreceived` and `dropped_by_generator` are 0: both engines then delivered every update, and `delivered_rps` differs only by the jitter of the last answer. That is the case at every point below.
- **Latency.** 1.0's `p99_ms` ≤ 1.2 × 0.2's.

**Valid runs.** A run whose generator fell behind its schedule (`lag_p99_ms` above 0.5 ms; undisturbed runs are 0.02–0.10 ms) measures the host, not the engine. Such runs are kept in the CSVs, listed below, and rerun (`./rerun-invalid.sh <csv>`) until each point has 3 valid runs per engine.

## Machine

| | |
|---|---|
| CPU | Intel Core i9-14900KF, 24 cores / 32 threads |
| RAM | 256 GB (Docker VM: 32 CPUs, 62.8 GiB) |
| OS | Windows 11 Pro; Docker Engine 29.3.1 (Docker Desktop 4.68.0) on WSL 2, kernel 6.6.87.2-microsoft-standard-WSL2 |
| Images | 1.0: built from this tree with the repository `Dockerfile` (static musl, mimalloc). 0.2: built from `1af6aaa`. |

No builds or other benchmarks ran during the measurements. Unrelated, idle containers of other projects stayed on the same Docker host.

## The gate

Medians of 3 valid runs, run set [`20261016T0155Z-final`](tests/performance/results/20261016T0155Z-final.csv) (+ [reruns](tests/performance/results/20261016T0155Z-final-rerun.csv)). Both engines answered every update in every run (`unreceived` = 0, `dropped_by_generator` = 0), so the throughput half of the gate is met everywhere; the `delivered` columns are shown for completeness.

| Point | RPS | 0.2 delivered | 1.0 delivered | 0.2 p99 ms | 1.0 p99 ms | 1.0 / 0.2 p99 | Verdict |
|---|---:|---:|---:|---:|---:|---:|---|
| webhook-tgin-1 | 1 000 | 1 000.02 | 1 000.02 | 0.62 | 0.64 | 1.03 | pass |
| webhook-tgin-1 | 5 000 | 4 999.99 | 4 999.98 | 0.47 | 0.49 | 1.04 | pass |
| webhook-tgin-1 | 10 000 | 9 999.96 | 9 999.93 | 0.47 | 0.52 | 1.11 | pass |
| webhook-tgin-3 | 1 000 | 1 000.02 | 1 000.02 | 0.67 | 0.65 | 0.97 | pass |
| webhook-tgin-3 | 5 000 | 4 999.98 | 4 999.99 | 0.47 | 0.49 | 1.04 | pass |
| webhook-tgin-3 | 10 000 | 9 999.96 | 9 999.96 | 0.57 | 0.54 | 0.95 | pass |
| longpoll-tgin-1 | 1 000 | 1 000.02 | 1 000.02 | 0.63 | 0.65 | 1.03 | pass |
| longpoll-tgin-1 | 5 000 | 4 999.99 | 5 000.00 | 0.54 | 0.53 | 0.98 | pass |
| longpoll-tgin-1 | 10 000 | 9 999.93 | 9 999.94 | 0.64 | 0.65 | 1.02 | pass |
| longpoll-tgin-3 | 1 000 | 1 000.02 | 1 000.03 | 0.66 | 0.58 | 0.88 | pass |
| longpoll-tgin-3 | 5 000 | 4 999.99 | 4 999.99 | 0.50 | 0.50 | 1.00 | pass |
| longpoll-tgin-3 | 10 000 | 9 999.92 | 9 999.94 | 0.60 | 0.65 | 1.08 | pass |

Median p50 is 0.18–0.35 ms everywhere, and never differs between the engines by more than 0.05 ms.

No point is excluded from the gate: with the Rust bots every point delivers in full on both engines.

**CPU of the tgin container** (highest `docker stats` sample per run, from `20261016T0155Z-final.stats.log`):

| Point | 10 000 RPS, tgin 0.2 | 10 000 RPS, tgin 1.0 |
|---|---|---|
| webhook-tgin-1 | 151 %, 146 %, 140 % | 188 %, 164 %, 144 % |
| webhook-tgin-3 | 160 %, 163 %, 173 % | 152 %, 168 %, 158 % |
| longpoll-tgin-1 | 118 %, 119 %, 117 % | 101 %, 101 %, 100 % |
| longpoll-tgin-3 | 146 %, 140 %, 145 % | 127 %, 129 %, 129 % |

**Invalid runs** in this set (generator lag above 0.5 ms; each was rerun, and the rerun is in `…-final-rerun.csv`):

| Point | Engine | RPS | `lag_p99_ms` | `p99_ms` of that run |
|---|---|---:|---:|---:|
| webhook-tgin-3 | v1 | 1 000 | 5.272 | 7.86 |
| webhook-tgin-1 | v0 | 1 000 | 4.098 | 6.08 |
| longpoll-tgin-3 | v1 | 1 000 | 2.009 | 4.73 |
| webhook-tgin-1 | v1 | 10 000 | 1.342 | 37.95 |
| webhook-tgin-3 | v0 | 10 000 | 2.245 | 50.85 |

## Baselines without tgin

Medians of 3 valid runs of the same set; the bot answers the generator directly.

| Baseline | RPS | delivered | p50 ms | p99 ms |
|---|---:|---:|---:|---:|
| webhook-direct-1 | 1 000 | 1 000.03 | 0.21 | 0.44 |
| webhook-direct-1 | 5 000 | 5 000.01 | 0.13 | 0.34 |
| webhook-direct-1 | 10 000 | 9 999.97 | 0.13 | 0.33 |
| longpoll-direct-1 | 1 000 | 1 000.03 | 0.20 | 0.44 |
| longpoll-direct-1 | 5 000 | 5 000.00 | 0.13 | 0.38 |
| longpoll-direct-1 | 10 000 | 9 999.96 | 0.15 | 0.46 |

**The Rust bot's capacity** ([`20261016T0330Z-rust-bot-capacity`](tests/performance/results/20261016T0330Z-rust-bot-capacity.csv), 10 s runs, one bot, no tgin):

| RPS | webhook delivered / p50 / p99 | long poll delivered / p50 / p99 |
|---:|---|---|
| 20 000 | 19 999.89 / 0.13 / 0.29 ms | 19 999.59 / 0.19 / 0.45 ms |
| 50 000 | 49 999.57 / 0.13 / 39.68 ms | 49 999.41 / 0.23 / 0.79 ms |
| 80 000 | 79 998.71 / 0.14 / 60.38 ms | 79 997.98 / 0.28 / 19.63 ms |

One bot process answers every update up to 80 000 per second, eight times the highest rate in the matrix, so the bots never set the numbers above. (Beyond 20 000 RPS the tail grows because the generator and the bot together saturate the host, not because updates are lost: nothing is unreceived.)

## Charts

Rendered from [`20261016T0155Z-final-charts.csv`](tests/performance/results/20261016T0155Z-final-charts.csv) (the valid runs of the gate set). Each point is the **mean** of its runs, so a single slow run shows up here while the table above uses medians. Dashed `target` lines mark the requested rate.

### Webhook

<table>
  <tr>
    <td><img src="tests/performance/diagram/generated/webhook-delivered-rps.png" alt="Webhook delivered RPS"></td>
    <td><img src="tests/performance/diagram/generated/webhook-achieved-rps.png" alt="Webhook achieved RPS"></td>
  </tr>
  <tr>
    <td><img src="tests/performance/diagram/generated/webhook-latency-p50.png" alt="Webhook p50 latency"></td>
    <td><img src="tests/performance/diagram/generated/webhook-latency-p99.png" alt="Webhook p99 latency"></td>
  </tr>
  <tr>
    <td><img src="tests/performance/diagram/generated/webhook-latency-p95.png" alt="Webhook p95 latency"></td>
    <td><img src="tests/performance/diagram/generated/webhook-latency-max.png" alt="Webhook max latency"></td>
  </tr>
  <tr>
    <td><img src="tests/performance/diagram/generated/webhook-unreceived.png" alt="Webhook unreceived updates"></td>
    <td></td>
  </tr>
</table>

### Long poll

<table>
  <tr>
    <td><img src="tests/performance/diagram/generated/longpoll-delivered-rps.png" alt="Long-poll delivered RPS"></td>
    <td><img src="tests/performance/diagram/generated/longpoll-achieved-rps.png" alt="Long-poll achieved RPS"></td>
  </tr>
  <tr>
    <td><img src="tests/performance/diagram/generated/longpoll-latency-p50.png" alt="Long-poll p50 latency"></td>
    <td><img src="tests/performance/diagram/generated/longpoll-latency-p99.png" alt="Long-poll p99 latency"></td>
  </tr>
  <tr>
    <td><img src="tests/performance/diagram/generated/longpoll-latency-p95.png" alt="Long-poll p95 latency"></td>
    <td><img src="tests/performance/diagram/generated/longpoll-latency-max.png" alt="Long-poll max latency"></td>
  </tr>
  <tr>
    <td><img src="tests/performance/diagram/generated/longpoll-unreceived.png" alt="Long-poll unreceived updates"></td>
    <td></td>
  </tr>
</table>

## What was fixed to get here

Two defects showed up once the bots stopped being the bottleneck.

**1. Nagle's algorithm on tgin's data plane (engine fix).** A pull response is written as one chunk per update, so a batch leaves tgin as several writes. With Nagle's algorithm on, every write after the first waits for the client's delayed ACK, up to 40 ms on Linux. `longpoll-tgin-1` at 10 000 RPS had p99 74–472 ms against 0.6 ms for 0.2 ([`20261015T2215Z-lp1-diag-v1`](tests/performance/results/20261015T2215Z-lp1-diag-v1.csv)). An instrumented build showed nothing slow inside tgin — not the `getUpdates` poll, the admission, the lease, or the handler ([`…-instrumented`](tests/performance/results/20261015T2225Z-lp1-diag-instrumented-v1.csv)) — while a packet capture showed the delay sitting between two segments of one response. tgin now sets `TCP_NODELAY` on every accepted connection, as 0.2's `axum_server` default acceptor did; with that, the same point is p99 0.65 ms.

**2. Unbounded webhook connections in the generator (harness fix).** Telegram opens at most `max_connections` (≤ 100) connections to a webhook; the generator opened one per in-flight send, up to 10 000. In the runs whose p99 jumped to 30–470 ms — on **both** engines — the kernel counters show the cause: the generator opened thousands of connections at once (`TcpActiveOpens` 2 627 and 3 583 against 60–150 in the clean runs), tgin's listen backlog overflowed (`TcpExtListenOverflows` 2 048 for 0.2 and 3 004 for 1.0, `TcpExtTCPSynRetrans` 1 543 and 2 393), and the sends waited out a SYN retransmit. Clean runs show zeros throughout. The generator now keeps at most 100 requests on the wire; the rest of the in-flight budget waits for a connection, and the wait counts in latency.

## `max_in_flight`

A push consumer's `max_in_flight` (requests in flight over all its endpoints) stays at its default, **32**, in the scenario configs. With the Rust bots the default passes the gate at the webhook points ([`20261016T0131Z-rust-mif32`](tests/performance/results/20261016T0131Z-rust-mif32.csv), medians of 3):

| Point | RPS | 0.2 delivered / p99 | 1.0 delivered / p99 (`max_in_flight` 32) |
|---|---:|---|---|
| webhook-tgin-1 | 5 000 | 5 000.00 / 0.47 ms | 4 999.97 / 0.48 ms |
| webhook-tgin-1 | 10 000 | 9 999.93 / 0.78 ms | 9 999.94 / 0.54 ms |
| webhook-tgin-3 | 5 000 | 4 999.99 / 0.47 ms | 4 999.99 / 0.65 ms |
| webhook-tgin-3 | 10 000 | 9 999.96 / 0.59 ms | 9 999.92 / 0.62 ms |

With a **single-process** downstream (one aiogram bot), the value mattered a great deal: at 5 000 RPS one bot delivered 1 651 RPS with 256, 1 809 with the default 32, and 2 268 with 8 ([`20261015T1945Z-py-mif256-v1`](tests/performance/results/20261015T1945Z-py-mif256-v1.csv), [`…mif64`](tests/performance/results/20261015T1955Z-py-mif64-v1.csv), [`…mif8`](tests/performance/results/20261015T1955Z-py-mif8-v1.csv)). A bot that handles one request at a time is fastest with few requests in flight; tune `max_in_flight` to what the downstream can actually overlap.

## Python bots

`BOT_IMPL=py` runs the aiogram 3 bots instead. One such process answers about 2 000 updates per second on this machine, so every point above 1 000 RPS measures queueing in front of saturated bots rather than tgin. The matrix taken that way is kept for reference:

- [`20261015T1752Z-v0`](tests/performance/results/20261015T1752Z-v0.csv), [`…-v1`](tests/performance/results/20261015T1752Z-v1.csv), [`…-direct`](tests/performance/results/20261015T1752Z-direct.csv): full matrix, 1.0 with default settings.
- [`20261015T2005Z-webhook-mif`](tests/performance/results/20261015T2005Z-webhook-mif-v1.csv) and [`20261015T2055Z-webhook1-1k-mif`](tests/performance/results/20261015T2055Z-webhook1-1k-mif-v1.csv): the webhook points with `max_in_flight` 8 per bot, and three more runs of webhook-tgin-1 at 1 000 RPS.
- [`20261015T2107Z-rust`](tests/performance/results/20261015T2107Z-rust.csv) and [`20261015T2245Z-rust-nodelay`](tests/performance/results/20261015T2245Z-rust-nodelay.csv): the Rust-bot matrix before and after the `TCP_NODELAY` fix, both before the connection cap.

What those runs showed, with the evidence they rest on:

- **With one aiogram bot at 5 000 and 10 000 RPS, both engines are far past the bot's capacity** (the bot container at 110–118 % CPU, tgin at 29–74 %). There 1.0 answered updates that 0.2 dropped: 116 661 against 70 158 of 150 000 with one bot at 5 000 RPS, 275 141 against 200 873 of 300 000 with three bots at 10 000 RPS. Those extra answers are updates that waited in the queue, which raises 1.0's p50 and p99 (36.1 s against 14.5 s at one bot, 5 000 RPS).
- **`longpoll-tgin-3` at 10 000 RPS**: 1.0 answered 4 587 per second against 0.2's 5 230, with the bots at 106–118 % CPU in both cases, i.e. about 12 % fewer answers per second from equally busy bots. The cause was never established; with the Rust bots the same point passes.
- **`longpoll-tgin-3` at 5 000 RPS**: both engines delivered every update with p50 0.85 ms (0.2) against 0.87 ms (1.0), but the tails differed run to run (0.2: 3.21, 4.15, 197.50 ms; 1.0: 55.68, 83.52, 117.44 ms). Three runs per engine could not separate them. With the Rust bots the point is 0.50 ms on both engines.
- **`webhook-tgin-1` at 1 000 RPS** failed on p99 in one set (8.24 ms against 1.46) and passed in a rerun of three runs each (1.47 against 2.33 ms). The bot ran at 45–84 % CPU — not saturated — and the generator did not stall in the failing run (`lag_p99_ms` 0.040). No component was identified as the cause; the point was simply noisy on both engines.

Those sets predate rulings R43 (the throughput rule above) and R45 (invalid runs), and three of them predate the two fixes, so their verdicts are not comparable with the gate table.

## Known limits of the harness

- **Generator CPU.** The send loop spins one core from about 500 RPS up, and the generator container used 1.2–2.4 cores at 5 000–10 000 RPS. It shares the host with tgin and the bots, so a host stall shows up as generator lag; runs above 0.5 ms of `lag_p99_ms` are invalid and are rerun.
- **Long-poll `achieved_rps`** is always the target: a long-poll send only appends to the fake API's queue.
- **`delivered_rps` under overload** is computed up to the last answer, which may come during the 30 s completion wait, so an engine that keeps and later delivers a backlog can report a lower rate than one that loses it. Read it together with `unreceived`.
- **Webhook connections are capped at 100** (see above). A target that answers more slowly than 100 concurrent requests allow will show the wait in its latency rather than in `dropped_by_generator`.
- **Stopping the bench container.** Its PID 1 (`tgin-bench serve`) has no SIGTERM handler, so `docker stop` and `make clean` wait for the stop timeout before killing it.
- **Ctrl-C through `docker compose exec -T`** (as the makefile runs `tgin-bench run`) may not reach the client, so an interrupted run can keep sending until its duration ends.
- Latency is measured over the Docker bridge on a desktop OS. Differences below about 0.05 ms between engines are noise.

## Reproducing the benchmarks

Requires Docker with Compose, `make`, `curl`, `git` and `bash`; the charts need [uv](https://docs.astral.sh/uv/). One pass of the matrix over both engines (6 scenarios × 3 rates × 3 runs of 30 s, plus start-up per run) takes about 70 minutes.

```bash
cd tests/performance

# Both engines, alternating run by run → results/<run_id>.{csv,log,stats.log}
ENGINE="v0 v1" RUN_ID=mine ./benchmark.sh

# Rerun the runs whose generator fell behind (lag_p99_ms > 0.5)
./rerun-invalid.sh results/mine.csv

# One point (ENGINE=v1 and BOT_IMPL=rust are the defaults)
make webhook-tgin-3 RPS=10000 DURATION=30
make longpoll-tgin-3 BOT_IMPL=py RPS=1000 DURATION=30

# Charts into diagram/generated/ (several CSVs may be combined)
cd diagram
uv run main.py ../results/mine.csv
```

`benchmark.sh` takes `ENGINE`, `BOT_IMPL`, `RPS_VALUES`, `SCENARIOS`, `DURATION`, `RUNS`, `RUN_ID`, `RESULT_DIR` and `BOT_SLEEP_MS`; see its header. Without CSV arguments the plotter renders the `results/*.csv` whose run id sorts last.
