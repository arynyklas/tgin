# Performance and Benchmarks

## Methodology

Load testing was conducted using `tgin-bench`, a custom benchmarking tool written in Rust. All tests were executed in a Dockerized environment to simulate high-load scenarios. The tool generated synthetic JSON payloads matching the structure of Telegram API Update objects.

Two main architectures:

Direct Connection: Requests were sent directly to a single Python bot instance built with aiogram.
TGIN Cluster: Requests were sent to the TGIN load balancer, which distributed traffic among N worker instances (tested with 2, 3, 4, 5, and 10 workers) using round-robin.
The following metrics were measured: requests per second (RPS), packet loss rate in percent, and latency (mean, median, p99, and max).

## Results Summary

A single aiogram instance shows a clear performance ceiling. At 2000 RPS, packet loss begins to increase significantly. At 5000 and 10000 RPS, the direct setup becomes unstable, with packet loss reaching 95–98% and response times exceeding 9 seconds.

In contrast, deploying TGIN immediately improves system resilience. Even a single TGIN worker handles load more gracefully than the direct setup. With 5 workers, the cluster sustains 10000 RPS with 0.00% packet loss.

In Webhook mode, 5 workers at 10000 RPS maintain a mean latency of approximately 11 ms, with p99 latency below 25 ms.

In Longpoll mode, TGIN functions as an in-memory buffer: during traffic spikes, incoming requests are queued rather than dropped. This ensures zero data loss, with latency increasing temporarily as the backlog is processed.


### Webhook — TGIN routing overhead
_Equal backend count: direct bot vs. a single bot behind tgin._

![Loss Rate %](tests/performance/diagram/generated/webhook-overhead-loss.png)
![Median Latency Ms](tests/performance/diagram/generated/webhook-overhead-median.png)
![Mean Latency Ms](tests/performance/diagram/generated/webhook-overhead-mean.png)
![Max Latency Ms](tests/performance/diagram/generated/webhook-overhead-max.png)

### Webhook — TGIN scale-out
_tgin fanning traffic out across 2, 3, 5, and 10 backends._

![Loss Rate %](tests/performance/diagram/generated/webhook-scale-loss.png)
![Median Latency Ms](tests/performance/diagram/generated/webhook-scale-median.png)
![Mean Latency Ms](tests/performance/diagram/generated/webhook-scale-mean.png)
![Max Latency Ms](tests/performance/diagram/generated/webhook-scale-max.png)

### Long-poll — TGIN routing overhead
_Equal backend count: direct bot vs. a single bot behind tgin._

![Loss Rate %](tests/performance/diagram/generated/longpoll-overhead-loss.png)
![Median Latency Ms](tests/performance/diagram/generated/longpoll-overhead-median.png)
![Mean Latency Ms](tests/performance/diagram/generated/longpoll-overhead-mean.png)
![Max Latency Ms](tests/performance/diagram/generated/longpoll-overhead-max.png)

### Long-poll — TGIN scale-out
_tgin buffering and distributing updates across 2, 3, 5, and 10 backends._

![Loss Rate %](tests/performance/diagram/generated/longpoll-scale-loss.png)
![Median Latency Ms](tests/performance/diagram/generated/longpoll-scale-median.png)
![Mean Latency Ms](tests/performance/diagram/generated/longpoll-scale-mean.png)
![Max Latency Ms](tests/performance/diagram/generated/longpoll-scale-max.png)


## How to setup
#### running the full benchmark takes about 30 minutes

```
# Clone the repository
git clone https://github.com/chesnokpeter/tgin.git
cd tgin/tests/performance

# Run benchmark
. ./benchmark.sh


# Update diagram's
cd diagram
uv run main.py
```