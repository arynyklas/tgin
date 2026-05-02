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

- A single direct aiogram bot saturates around 2,000 RPS and is unusable at 5,000–10,000 RPS (95–98% loss, multi-second tail).
- A 10-worker tgin webhook cluster sustains 10,000 RPS with 0% packet loss, ~11.5 ms mean latency, and p99 around 13 ms. 5 workers also absorb the same load at 0% loss, but mean latency rises to ~31 ms (p99 ~126 ms).
- In long-poll mode tgin acts as an in-memory buffer: spikes are queued, not dropped, so packet loss stays at 0% while latency rises temporarily as the backlog drains.

## Methodology

Load testing uses `tgin-bench`, a Rust load generator that emits synthetic JSON payloads matching Telegram `Update` objects. Everything runs in Docker so the load generator, tgin, and downstream bots are isolated and reproducible.

Two architectures are compared:

- **Direct connection** — load generator → single Python aiogram bot.
- **tgin cluster** — load generator → tgin → `N` aiogram workers, distributed via `RoundRobinLB`. Tested with `N ∈ {1, 2, 3, 4, 5, 10}`.

Per-run metrics:

- Requests per second (RPS).
- Packet loss rate (%).
- Latency: mean, median, p99, max.

Each scenario is run for both the webhook and the long-poll transport.

## Results summary

A single aiogram instance hits a hard ceiling. At 2 000 RPS packet loss starts climbing; at 5 000 and 10 000 RPS the direct setup falls apart, with 95–98% loss and response times beyond 9 seconds.

Putting tgin in front of even a single worker improves resilience. With 5 workers the cluster sustains 10 000 RPS at 0% loss.

In webhook mode, 10 workers at 10 000 RPS hold mean latency around 11.5 ms with p99 around 13 ms; 5 workers also absorb the same load at 0% loss, but mean rises to ~31 ms (p99 ~126 ms). In long-poll mode tgin buffers updates during spikes: latency rises temporarily as the backlog drains, but no updates are lost once the cluster is wide enough.

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

A full benchmark sweep takes about 30 minutes.

```bash
git clone https://github.com/arynyklas/tgin.git
cd tgin/tests/performance

# Run the full RPS matrix (writes results/<run_id>.csv).
. ./benchmark.sh

# Regenerate the charts under diagram/generated/.
cd diagram
uv run main.py
```

Per-scenario runs are also exposed through the makefile, e.g. `make webhook-tgin-scale-5 RPS=1000 DURATION=10`.

## See also

- [README.md](README.md) — project overview and quick start.
- [DOCS.md](DOCS.md) — full configuration reference.
