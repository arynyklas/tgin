//! Wall-clock benchmarks of the hot path over the 1000 fixture updates:
//! `cargo bench -p tgin-botapi --bench hot_path`.

mod common;

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use smallvec::SmallVec;
use tgin_botapi::extract;
use tgin_core::rules::MatchScratch;
use tgin_runtime::queue::HolderId;

use common::HotPath;

fn hot_path(c: &mut Criterion) {
    let hot = HotPath::new();
    let payloads = common::payloads();
    let facts = common::facts(&payloads);
    let updates = Throughput::Elements(payloads.len() as u64);

    let mut g = c.benchmark_group("facts");
    g.throughput(updates.clone());
    g.bench_function("extract", |b| {
        b.iter(|| {
            for p in &payloads {
                black_box(extract(black_box(p)).expect("valid"));
            }
        });
    });
    g.finish();

    let mut g = c.benchmark_group("rules");
    g.throughput(updates.clone());
    let state = hot.engine.state();
    let mut ids = SmallVec::new();
    let mut scratch = MatchScratch::default();
    g.bench_function("route", |b| {
        b.iter(|| {
            for (_, f) in &facts {
                black_box(state.table.route(hot.source, f, &mut ids, &mut scratch));
            }
        });
    });
    g.finish();

    let mut g = c.benchmark_group("queue");
    g.throughput(updates.clone());
    let queue = common::queue();
    let holder = HolderId("bench".into());
    let envelopes: Vec<_> = payloads
        .iter()
        .map(|p| common::envelope(hot.source, p))
        .collect();
    g.bench_function("commit_lease_ack", |b| {
        b.iter(|| common::commit_lease_ack(&queue, &holder, black_box(&envelopes)));
    });
    g.finish();

    let mut g = c.benchmark_group("engine");
    g.throughput(updates);
    g.bench_function("admit", |b| {
        // Draining between rounds is not part of the measurement.
        b.iter_custom(|rounds| {
            let mut spent = Duration::ZERO;
            for _ in 0..rounds {
                let start = Instant::now();
                black_box(hot.admit(&payloads, &facts));
                spent += start.elapsed();
                hot.drain();
            }
            spent
        });
    });
    g.finish();
}

criterion_group!(benches, hot_path);
criterion_main!(benches);
