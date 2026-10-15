# Performance and benchmarks

tgin 1.0 (milestone M1, commit `536518e`) against tgin 0.2 (commit `1af6aaa`), measured on one machine in one session on 2026-10-15. The short version:

- **Below the bots' capacity, tgin adds well under a millisecond.** At 1 000 RPS every scenario delivers every update; median latency through either engine is 0.6–1.0 ms, against 0.5–0.6 ms without tgin.
- **The gate holds at 7 of 12 points** (webhook with three bots at every rate, webhook with one bot at 5 000 and 10 000 RPS, both long-poll scenarios at 1 000 RPS). Webhook with three bots at 10 000 RPS: 6 042 against 5 570 delivered RPS, p99 1.8 s against 2.3 s.
- **Every failing point has bots at or beyond their capacity.** One aiogram process answers about 2 000 updates per second here; beyond that, latency is time spent in a queue. tgin 1.0 keeps updates that 0.2 loses (long poll, three bots, 10 000 RPS: 275 141 updates answered against 200 873), and the late ones raise its p99. Webhook with one bot at 1 000 RPS is noisy for both engines and fails on one set of runs and passes on another. Details in [Gate](#gate).

## Contents

- [Methodology](#methodology)
- [Machine](#machine)
- [Gate](#gate)
- [Baselines without tgin](#baselines-without-tgin)
- [Charts](#charts)
- [Known limits of the harness](#known-limits-of-the-harness)
- [Reproducing the benchmarks](#reproducing-the-benchmarks)

## Methodology

Everything runs in Docker Compose (`tests/performance/`) on one bridge network:

- **`tgin-bench`** (Rust, `tests/performance/benchmark`) is both the load generator and a fake Telegram Bot API (`getMe`, `deleteWebhook`, `getUpdates`, `sendMessage`). It runs as a server for the container's whole life; `tgin-bench run` asks it for one measured run.
- **The engine under test**: the 0.2 image (RON config, `tests/performance/cfgs/*.ron`) or the 1.0 image (YAML config, `tests/performance/cfgs/v1/*.yaml`).
- **Bots**: Python aiogram 3 processes (`tests/performance/bot`) that answer every update with `sendMessage` and do no other work (`BOT_SLEEP_MS=0`).

Scenarios:

| Scenario | Path of an update |
|---|---|
| `webhook-direct-1` | generator → bot's webhook (no tgin) |
| `webhook-tgin-1`, `webhook-tgin-3` | generator → tgin webhook → push to 1 or 3 bots, round robin |
| `longpoll-direct-1` | bot long-polls the fake Bot API (no tgin) |
| `longpoll-tgin-1`, `longpoll-tgin-3` | tgin long-polls the fake Bot API → 1 or 3 bots poll tgin (1.0: pull consumer `/c/bots/getUpdates`; 0.2: `/botN/getUpdates` routes, round robin) |

Each run:

1. **Warm-up.** 30 unmeasured updates must all come back before measuring starts; this proves that tgin and every bot are live.
2. **Open-loop send phase, 30 s.** Update `i` is due at `start + i / rps`. A dedicated thread sleeps until 2 ms before each due time and spins the rest, then hands the update off without waiting for any response. A late send goes out at once and keeps its due time. Webhook sends hold one of 10 000 in-flight permits; when none is free, the send is not issued and counts as `dropped_by_generator`.
3. **Latency** runs from the **due time** to the bot's `sendMessage` for that update, so queueing anywhere, including in the generator, counts (no coordinated omission).
4. **Completion wait.** After the send phase the run waits until every sent update is answered or failed, or 30 s pass. What is still missing then is `unreceived`; it never enters the latency figures.

The matrix: 1 000, 5 000 and 10 000 target RPS × {webhook → push, long poll → pull} × {1, 3 bots}, 3 runs per point and engine, the two engines alternating run by run. The direct baselines ran 3 times per rate.

CSV columns (`tests/performance/results/*.csv`):

| Column | Meaning |
|---|---|
| `scenario`, `engine`, `target_rps` | The point. Direct baselines carry `engine=v1` but involve no tgin. |
| `achieved_rps` | Updates sent ÷ send-phase wall time. Below target when the generator dropped sends (webhook) or fell behind. Long-poll sends only append to the fake API's queue, so they always reach the target. |
| `delivered_rps` | Updates answered ÷ (last answer − first due time). Under overload it includes the completion wait, see [limits](#known-limits-of-the-harness). |
| `p50_ms` … `max_ms` | Latency of answered updates. |
| `unreceived` | Sent but not answered by the end of the completion wait. For 1.0 webhooks this includes updates tgin refused with `503` (backpressure). |
| `duplicate_replies` | Further answers to an already answered update (0 in every run). |
| `dropped_by_generator` | Webhook sends never issued because 10 000 requests were already in flight; 0.2 holds requests open when it is overloaded, so its overload shows up here. |
| `lag_p99_ms` | p99 of how late the generator handed updates off. Above ~1 ms means the host stalled the generator during the run. |

**The gate.** At each point, over the medians of 3 runs: 1.0's `delivered_rps` ≥ 0.2's, and 1.0's `p99_ms` ≤ 1.2 × 0.2's.

**One configuration change.** The webhook scenarios set `max_in_flight` on the 1.0 push consumer to 8 requests per bot (8 for one bot, 24 for three) instead of the default 32. A single aiogram process answers fastest at low concurrency. With the default 32, one bot delivered 1 809 RPS at 5 000 RPS target (0.2: 2 181); with 256 it delivered 1 651; with 8, 2 268. A user running one single-process bot behind tgin would make the same change. The long-poll scenarios use the defaults.

## Machine

| | |
|---|---|
| CPU | Intel Core i9-14900KF, 24 cores / 32 threads |
| RAM | 256 GB (Docker VM: 32 CPUs, 62.8 GiB) |
| OS | Windows 11 Pro; Docker Desktop 29.3.1 on WSL 2, kernel 6.6.87.2-microsoft-standard-WSL2 |
| Images | 1.0: `tgin:m1`, built from `536518e` with the repository `Dockerfile` (static musl, mimalloc). 0.2: `tgin-bench:v0`, built from `1af6aaa`. |

No builds or other benchmarks ran during the measurements. Unrelated, idle database containers from other projects kept running on the same Docker host.

## Gate

Medians of 3 runs. Long poll: run set `20261015T1752Z` (defaults). Webhook: run set `20261015T2005Z-webhook-mif` (`max_in_flight` 8 per bot), both engines measured again in that set.

| Point | RPS | 0.2 delivered | 1.0 delivered | 0.2 p99 ms | 1.0 p99 ms | 1.0 / 0.2 p99 | 0.2 unreceived + dropped | 1.0 unreceived + dropped | Verdict |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| webhook-tgin-1 | 1 000 | 1 000.01 | 1 000.01 | 1.46 | 8.24 | 5.64 | 0 | 0 | **fail** (p99), see note 1 |
| webhook-tgin-1 | 5 000 | 2 120.83 | 2 268.31 | 5 632.00 | 4 718.59 | 0.84 | 75 986 | 71 999 | pass |
| webhook-tgin-1 | 10 000 | 2 173.92 | 2 197.86 | 5 410.81 | 4 825.09 | 0.89 | 224 052 | 223 543 | pass |
| webhook-tgin-3 | 1 000 | 999.99 | 1 000.00 | 1.79 | 1.76 | 0.98 | 0 | 0 | pass |
| webhook-tgin-3 | 5 000 | 4 999.86 | 4 999.87 | 83.45 | 88.70 | 1.06 | 0 | 0 | pass |
| webhook-tgin-3 | 10 000 | 5 569.82 | 6 042.45 | 2 330.62 | 1 763.33 | 0.76 | 122 134 | 109 443 | pass |
| longpoll-tgin-1 | 1 000 | 1 000.01 | 1 000.01 | 2.31 | 1.75 | 0.76 | 0 | 0 | pass |
| longpoll-tgin-1 | 5 000 | 1 878.66 | 1 944.50 | 14 499.84 | 36 143.10 | 2.49 | 79 842 | 33 339 | **fail** (p99), note 2 |
| longpoll-tgin-1 | 10 000 | 1 838.82 | 1 957.18 | 13 631.49 | 47 611.90 | 3.49 | 229 609 | 182 568 | **fail** (p99), note 2 |
| longpoll-tgin-3 | 1 000 | 1 000.00 | 1 000.01 | 1.78 | 1.59 | 0.89 | 0 | 0 | pass |
| longpoll-tgin-3 | 5 000 | 4 999.88 | 4 999.92 | 4.15 | 83.52 | 20.13 | 0 | 0 | **fail** (p99), note 3 |
| longpoll-tgin-3 | 10 000 | 5 230.18 | 4 586.58 | 15 630.33 | 32 358.40 | 2.07 | 99 127 | 24 859 | **fail** (delivered, p99), note 2 |

With the default `max_in_flight` (set `20261015T1752Z`), webhook-tgin-1 at 1 000 RPS passed (p99 1.53 against 4.86 ms), and at 5 000 and 10 000 RPS it failed on delivered RPS (1 809 against 2 181; 1 783 against 2 132); webhook-tgin-3 at 5 000 RPS missed by 0.2 delivered RPS (4 999.69 against 4 999.90).

Notes on the failing points. In the long-poll failures the bots are the bottleneck: each bot container sat at 110–118 % CPU (one Python core plus aiohttp's threads) in `docker stats`, while tgin used 2–79 %; the generator kept its schedule (`lag_p99_ms` ≤ 0.08 ms) and dropped nothing.

1. **webhook-tgin-1, 1 000 RPS.** One bot at 75–99 % CPU is close to its knee, and the point is noisy for both engines. Of the three 1.0 runs in the gate set, one had `lag_p99_ms` 9.0 ms (the host stalled the generator; delivered 754 RPS, p99 9.9 s) and one p99 8.24 ms. Three more runs of each engine (set `20261015T2055Z-webhook1-1k-mif`) gave medians of 1.47 ms (1.0) against 2.33 ms (0.2), a pass; there 0.2 had a run with p99 2 071 ms and `lag_p99_ms` 7.8. Over all six runs the medians are 5.18 ms (1.0) against 2.23 ms (0.2).
2. **Long poll beyond the bots' capacity (one bot at 5 000 and 10 000, three bots at 10 000).** 0.2 loses updates that 1.0 keeps: at 10 000 RPS with three bots, 1.0 answered 275 141 of 300 000 updates and 0.2 200 873; with one bot at 5 000 RPS, 116 661 against 70 158. The extra updates are old ones that waited in the queue, which is what raises 1.0's p50 and p99. The same effect lowers `delivered_rps` for three bots at 10 000 RPS: 1.0 keeps answering through the 30 s completion wait, so its rate is computed over a longer window (about 60 s against 38 s). tgin itself was not busy: 21 % CPU for 1.0 against 78 % for 0.2 (three bots, 10 000 RPS). 1.0's pull consumer stops fetching from Telegram when its queue is full (`on_full: backpressure`), so the backlog waits upstream instead of being dropped.
3. **longpoll-tgin-3, 5 000 RPS.** Three bots at 114–117 % CPU, exactly at capacity; both engines delivered every update with nearly the same p50 (0.85 against 0.87 ms) and one `getUpdates` call per update. The tail is unstable for both: 0.2's runs had p99 3.21, 4.15 and 197.50 ms; 1.0's 55.68, 83.52 and 117.44 ms. At capacity, a short stall in any bot queues updates behind it; three runs per engine do not separate the two engines' tails.

No code change to tgin is indicated: wherever the bots have headroom (every three-bot point at 1 000 RPS, three webhook bots at 5 000 RPS) 1.0 matches or beats 0.2, and tgin 1.0 never used more than 1.2 cores.

Every run, including the failed points, is in the raw CSVs:

- [`20261015T1752Z-v0.csv`](tests/performance/results/20261015T1752Z-v0.csv), [`20261015T1752Z-v1.csv`](tests/performance/results/20261015T1752Z-v1.csv), [`20261015T1752Z-direct.csv`](tests/performance/results/20261015T1752Z-direct.csv): the full matrix with default 1.0 settings.
- [`20261015T2005Z-webhook-mif-v0.csv`](tests/performance/results/20261015T2005Z-webhook-mif-v0.csv), [`20261015T2005Z-webhook-mif-v1.csv`](tests/performance/results/20261015T2005Z-webhook-mif-v1.csv): the webhook points again, 1.0 with `max_in_flight` 8 per bot.
- [`20261015T2055Z-webhook1-1k-mif-v0.csv`](tests/performance/results/20261015T2055Z-webhook1-1k-mif-v0.csv), [`20261015T2055Z-webhook1-1k-mif-v1.csv`](tests/performance/results/20261015T2055Z-webhook1-1k-mif-v1.csv): webhook-tgin-1 at 1 000 RPS, 3 more runs per engine.
- [`20261015T2100Z-charts.csv`](tests/performance/results/20261015T2100Z-charts.csv): the rows the charts and the gate table use (long poll and direct from the first set, webhook from the second).

## Baselines without tgin

Medians of 3 runs. One bot without tgin does worse than one bot behind tgin under overload: the generator holds up to 10 000 requests open against the bot's webhook, and aiohttp slows down under that many connections; behind tgin the bot sees at most `max_in_flight` of them.

| Baseline | RPS | delivered | p50 ms | p99 ms | unreceived + dropped |
|---|---:|---:|---:|---:|---:|
| webhook-direct-1 | 1 000 | 1 000.02 | 0.52 | 1.57 | 0 |
| webhook-direct-1 | 5 000 | 795.97 | 29 949.95 | 33 325.06 | 111 866 |
| webhook-direct-1 | 10 000 | 928.47 | 30 048.26 | 35 323.90 | 261 155 |
| longpoll-direct-1 | 1 000 | 1 000.02 | 0.58 | 1.97 | 0 |
| longpoll-direct-1 | 5 000 | 1 393.85 | 20 299.78 | 43 712.51 | 66 373 |
| longpoll-direct-1 | 10 000 | 1 400.84 | 25 296.90 | 51 281.92 | 216 144 |

## Charts

Rendered from `20261015T2100Z-charts.csv`. Each line is the mean of the runs at that rate (the gate table uses medians). Dashed `target` lines mark the requested rate.

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

## Known limits of the harness

- **The bots are the ceiling.** One aiogram process answers about 2 000 updates per second here. Above that, the numbers describe queueing in front of saturated bots, not tgin's own capacity; tgin 1.0 used at most 1.2 cores in any run.
- **Generator CPU.** The send loop spins one core from about 500 RPS up, and the generator container used 1.2–2.4 cores at 5 000–10 000 RPS in the tgin scenarios. In `webhook-direct-1` at 5 000 RPS and above it used 33–36 cores (10 000 requests held open against one slow bot), so those two baseline points measure the generator as much as the bot. It shares the host with tgin and the bots. A host stall shows as `lag_p99_ms` above ~1 ms; such runs are kept in the CSVs and called out above.
- **`delivered_rps` under overload** is computed up to the last answer, which may come during the 30 s completion wait. An engine that keeps and later delivers a backlog gets a longer window and a lower rate than one that loses it; read it together with `unreceived`.
- **Long-poll `achieved_rps`** is always the target: a long-poll send only appends to the fake API's queue.
- **Stopping the bench container.** Its PID 1 (`tgin-bench serve`) has no SIGTERM handler, so `docker stop` and `make clean` wait for the stop timeout before killing it.
- **Ctrl-C through `docker compose exec -T`** (as the makefile runs `tgin-bench run`) may not reach the client, so an interrupted run can keep sending until its duration ends.
- Timer resolution: the send loop is accurate to microseconds (`lag_p99_ms` 0.03–0.08 ms in undisturbed runs), but latency is measured over the Docker bridge on a desktop OS; sub-millisecond differences between engines are within noise.

## Reproducing the benchmarks

Requires Docker with Compose, `make`, `curl`, `git` and `bash`; the charts need [uv](https://docs.astral.sh/uv/). A matrix of one engine (6 scenarios × 3 rates × 3 runs of 30 s, plus start-up per run) takes about 45 minutes.

```bash
cd tests/performance

# The 0.2 baseline image (from git, no checkout needed) and the 1.0 image:
make image ENGINE=v0
make image ENGINE=v1

# One engine's matrix → results/<run_id>.csv:
ENGINE=v0 RUN_ID=mine-v0 ./benchmark.sh
ENGINE=v1 RUN_ID=mine-v1 ./benchmark.sh

# One point:
make webhook-tgin-3 ENGINE=v1 RPS=5000 DURATION=30

# Charts into diagram/generated/ (several CSVs may be combined):
cd diagram
uv run main.py ../results/mine-v0.csv ../results/mine-v1.csv
```

`benchmark.sh` takes `ENGINE`, `RPS_VALUES`, `SCENARIOS`, `DURATION`, `RUNS`, `RUN_ID`, `RESULT_DIR` and `BOT_SLEEP_MS`; see its header. Without CSV arguments the plotter renders the `results/*.csv` whose run id sorts last.
