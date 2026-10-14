# Performance and benchmarks

> [!WARNING]
> **These numbers will be re-measured; do not use them for capacity planning.** The charts below come from a benchmark harness with known measurement defects:
>
> - **Closed-loop load.** The load generator does not keep its target rate when the system under test slows down: its ticker delays missed ticks instead of catching up, and webhook sends wait for one of `max_in_flight` slots. The offered load then drops below the target RPS (`rps_actual` in the CSV), and the latency of requests that were never sent goes unmeasured.
> - **Broken direct long-poll baseline.** The fake Bot API in `tgin-bench` reads `getUpdates` parameters only from the query string and keeps every returned update until a later request carries a higher `offset`. The direct aiogram bot sends its parameters in the POST body, so its `offset` never arrives and it is served the same oldest batch again and again. The `longpoll-direct-1` rows measure this bug, not aiogram.
> - **2 s drain cut-off.** After the send phase the generator waits at most 2 s (`--drain-timeout`, `DRAIN_TIMEOUT` in the makefile) for outstanding replies. Anything answered later, such as updates still queued in a tgin long-poll buffer, is counted as lost, so "loss" mixes late delivery with real loss.
> - **10 ms bot sleep.** Every benchmark bot sleeps 10 ms per update before replying (`tests/performance/bot/*.py`). That sleep sets the latency floor and caps each bot's throughput, so the charts mostly show bot capacity, not routing overhead.

## Contents

- [Methodology](#methodology)
- [Webhook — routing overhead](#webhook--routing-overhead)
- [Webhook — scale-out](#webhook--scale-out)
- [Long-poll — routing overhead](#long-poll--routing-overhead)
- [Long-poll — scale-out](#long-poll--scale-out)
- [Reproducing the benchmarks](#reproducing-the-benchmarks)
- [See also](#see-also)

## Methodology

Load testing uses `tgin-bench` (`tests/performance/benchmark`), a Rust load generator that emits synthetic JSON payloads shaped like Telegram `Update` objects and plays the Telegram Bot API for the bots (`getUpdates`, `sendMessage`). A bot replies to each update with `sendMessage`; the time from sending an update to receiving that reply is its latency. Everything runs in Docker Compose so the load generator, tgin and the downstream bots are isolated.

Two architectures are compared:

- **Direct connection** — load generator → single Python aiogram bot.
- **tgin cluster** — load generator → tgin → `N` aiogram bots, distributed via `RoundRobinLB`. Tested with `N ∈ {1, 2, 3, 5, 10}`.

Each scenario runs for both the webhook and the long-poll transport, at 500, 1 000, 2 000, 5 000, 8 000 and 10 000 target RPS.

Per-run metrics (one CSV row per run):

- Target and achieved RPS.
- Loss rate (%): updates without a reply by the end of the drain window.
- Latency: min, mean, median (p50), p95, p99, max. The charts show loss, mean, median and max; the other columns are in the CSV.

## Webhook — routing overhead

_Equal backend count: direct bot vs. a single bot behind tgin._

<table>
  <tr>
    <td><img src="tests/performance/diagram/generated/webhook-overhead-loss.png" alt="Loss rate %"></td>
    <td><img src="tests/performance/diagram/generated/webhook-overhead-median.png" alt="Median latency"></td>
  </tr>
  <tr>
    <td><img src="tests/performance/diagram/generated/webhook-overhead-mean.png" alt="Mean latency"></td>
    <td><img src="tests/performance/diagram/generated/webhook-overhead-max.png" alt="Max latency"></td>
  </tr>
</table>

## Webhook — scale-out

_tgin fanning traffic out across 2, 3, 5, and 10 backends._

<table>
  <tr>
    <td><img src="tests/performance/diagram/generated/webhook-scale-loss.png" alt="Loss rate %"></td>
    <td><img src="tests/performance/diagram/generated/webhook-scale-median.png" alt="Median latency"></td>
  </tr>
  <tr>
    <td><img src="tests/performance/diagram/generated/webhook-scale-mean.png" alt="Mean latency"></td>
    <td><img src="tests/performance/diagram/generated/webhook-scale-max.png" alt="Max latency"></td>
  </tr>
</table>

## Long-poll — routing overhead

_Equal backend count: direct bot vs. a single bot behind tgin. The direct baseline is broken (see the warning above)._

<table>
  <tr>
    <td><img src="tests/performance/diagram/generated/longpoll-overhead-loss.png" alt="Loss rate %"></td>
    <td><img src="tests/performance/diagram/generated/longpoll-overhead-median.png" alt="Median latency"></td>
  </tr>
  <tr>
    <td><img src="tests/performance/diagram/generated/longpoll-overhead-mean.png" alt="Mean latency"></td>
    <td><img src="tests/performance/diagram/generated/longpoll-overhead-max.png" alt="Max latency"></td>
  </tr>
</table>

## Long-poll — scale-out

_tgin buffering and distributing updates across 2, 3, 5, and 10 backends._

<table>
  <tr>
    <td><img src="tests/performance/diagram/generated/longpoll-scale-loss.png" alt="Loss rate %"></td>
    <td><img src="tests/performance/diagram/generated/longpoll-scale-median.png" alt="Median latency"></td>
  </tr>
  <tr>
    <td><img src="tests/performance/diagram/generated/longpoll-scale-mean.png" alt="Mean latency"></td>
    <td><img src="tests/performance/diagram/generated/longpoll-scale-max.png" alt="Max latency"></td>
  </tr>
</table>

## Reproducing the benchmarks

Requires Docker with Compose, `make`, `curl`, `git` and `bash`; the chart step requires [uv](https://docs.astral.sh/uv/). A full sweep runs 12 scenarios × 6 RPS values × 3 runs = 216 runs of 30 s load each, plus container start-up per run: about 3 hours.

```bash
git clone https://github.com/arynyklas/tgin.git
cd tgin/tests/performance

# Run the full RPS matrix (writes results/<run_id>.csv).
# Defaults: DURATION=30 RUNS=3.
./benchmark.sh

# Regenerate the charts under diagram/generated/ from the most recently modified
# results/*.csv (pass CSV paths to `uv run main.py` to choose a specific run).
cd diagram
uv run main.py
```

Per-scenario runs are also exposed through the makefile, e.g. `make webhook-tgin-scale-5 RPS=1000 DURATION=30`. Override full-sweep sampling with `DURATION=<seconds>` and `RUNS=<count>` when needed.

## See also

- [README.md](README.md) — project overview and quick start.
- [DOCS.md](DOCS.md) — full configuration reference.
