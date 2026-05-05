# Performance and benchmarks

## Contents

- [TL;DR](#tldr)
- [Methodology](#methodology)
- [Results summary](#results-summary)
- [Webhook — routing overhead](#webhook--routing-overhead)
- [Webhook — scale-out](#webhook--scale-out)
- [Long-poll — routing overhead](#long-poll--routing-overhead)
- [Long-poll — scale-out](#long-poll--scale-out)
- [Reproducing the benchmarks](#reproducing-the-benchmarks)
- [See also](#see-also)

## TL;DR

- A single direct aiogram bot is still clean at 2,000 RPS but collapses from 5,000 RPS upward (97–98% loss, multi-second p99 latency).
- A 10-worker tgin webhook cluster sustains 10,000 RPS at 0% loss with mean ~11.6 ms and p99 ~15 ms. 5 workers also hold 0% loss at the same RPS, but the cluster queues: mean climbs to ~410 ms with p99 around 990 ms.
- In long-poll mode tgin buffers spikes in a bounded route queue. With 5 or more workers loss stays at 0% at 10,000 RPS while latency rises temporarily as the backlog drains; narrower clusters fill the buffer and start dropping (3 workers see ~53% loss at 10,000 RPS, 1 worker drops ~91% from 5,000 RPS upward).

## Methodology

Load testing uses `tgin-bench`, a Rust load generator that emits synthetic JSON payloads matching Telegram `Update` objects. Everything runs in Docker so the load generator, tgin, and downstream bots are isolated and reproducible.

Two architectures are compared:

- **Direct connection** — load generator → single Python aiogram bot.
- **tgin cluster** — load generator → tgin → `N` aiogram workers, distributed via `RoundRobinLB`. Tested with `N ∈ {1, 2, 3, 5, 10}`.

Per-run metrics:

- Requests per second (RPS).
- Packet loss rate (%).
- Latency: mean, median (p50), p99, max. The repository's plotted charts show loss, mean, median, and max; p99 is recorded in the CSV and cited in the prose.

Each scenario is run for both the webhook and the long-poll transport.

## Results summary

A single aiogram instance hits a hard ceiling. At 2 000 RPS the direct setup is still clean; from 5 000 RPS upward it collapses with 97–98% loss and p99 latency in the tens of seconds.

Putting tgin in front of even a single worker drops 5 000–10 000 RPS loss from ~97% to ~7%, and a 5-worker webhook cluster sustains 10 000 RPS at 0% loss.

In webhook mode, 10 workers at 10 000 RPS hold mean latency around 11.6 ms with p99 around 15 ms; 5 workers also absorb the same load at 0% loss, but the cluster queues — mean rises to ~410 ms with p99 around 990 ms. In long-poll mode tgin buffers updates during spikes in a bounded route queue: latency rises temporarily as the backlog drains, and once the cluster is wide enough (≥5 workers at 10 000 RPS) no updates are lost. Narrower long-poll clusters fill the buffer and start dropping.

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

_Equal backend count: direct bot vs. a single bot behind tgin._

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

A full benchmark sweep runs three 30-second samples for each scenario by default.

```bash
git clone https://github.com/arynyklas/tgin.git
cd tgin/tests/performance

# Run the full RPS matrix (writes results/<run_id>.csv).
# Defaults: DURATION=30 RUNS=3.
. ./benchmark.sh

# Regenerate the charts under diagram/generated/.
cd diagram
uv run main.py
```

Per-scenario runs are also exposed through the makefile, e.g. `make webhook-tgin-scale-5 RPS=1000 DURATION=30`. Override full-sweep sampling with `DURATION=<seconds>` and `RUNS=<count>` when needed.

## See also

- [README.md](README.md) — project overview and quick start.
- [DOCS.md](DOCS.md) — full configuration reference.
