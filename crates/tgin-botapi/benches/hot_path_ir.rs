//! Instruction counts of the hot path under Callgrind, each gated by a hard
//! limit: `cargo bench -p tgin-botapi --bench hot_path_ir`. Linux only; it
//! needs Valgrind and `gungraun-runner` at the version of `gungraun` in
//! `Cargo.lock`. Elsewhere it builds as an empty `main`.
//!
//! Every benchmark runs one warm round over the 1000 fixture updates, as
//! `benches/hot_path.rs` and `tests/alloc_budget.rs` do. Each limit is 1.5
//! times the count measured in Docker `rust:1.99.0` (Valgrind 3.24.0, run
//! with `--security-opt seccomp=unconfined`), the highest of four runs; the
//! measured value is next to it. Runs differ by under 1% with the random
//! hash seeds of the route table's and the queues' sets and maps.

#[cfg(target_os = "linux")]
mod common;

#[cfg(target_os = "linux")]
use std::hint::black_box;

#[cfg(target_os = "linux")]
use bytes::Bytes;
#[cfg(target_os = "linux")]
use gungraun::{
    Callgrind, EventKind, LibraryBenchmarkConfig, library_benchmark, library_benchmark_group, main,
};
#[cfg(target_os = "linux")]
use smallvec::SmallVec;
#[cfg(target_os = "linux")]
use tgin_core::envelope::Envelope;
#[cfg(target_os = "linux")]
use tgin_core::facts::Facts;
#[cfg(target_os = "linux")]
use tgin_core::rules::MatchScratch;
#[cfg(target_os = "linux")]
use tgin_runtime::queue::{HolderId, LeaseQueue};

#[cfg(target_os = "linux")]
use common::HotPath;

/// Instructions for `facts::extract` over the 1000 updates.
#[cfg(target_os = "linux")]
const EXTRACT_IR: u64 = 18_152_688; // measured: 12_101_792
/// Instructions for `rules::route` over the 1000 updates.
#[cfg(target_os = "linux")]
const ROUTE_IR: u64 = 2_316_710; // measured: 1_544_473
/// Instructions for `queue::commit_lease_ack` over the 1000 updates.
#[cfg(target_os = "linux")]
const QUEUE_IR: u64 = 10_607_196; // measured: 7_071_464
/// Instructions for `engine::admit` over the 1000 updates.
#[cfg(target_os = "linux")]
const ADMIT_IR: u64 = 9_392_940; // measured: 6_261_960

/// Callgrind failing the run when the benchmark executes more than `ir`
/// instructions.
#[cfg(target_os = "linux")]
fn limit(ir: u64) -> LibraryBenchmarkConfig {
    let mut config = LibraryBenchmarkConfig::default();
    config.tool(Callgrind::default().hard_limits([(EventKind::Ir, ir)]));
    config
}

/// The fixture updates, with the facts each one routes on. The payloads are
/// leaked so that the facts can borrow them for the rest of the process.
#[cfg(target_os = "linux")]
fn fixture() -> (&'static [Bytes], Vec<(i64, Facts<'static>)>) {
    let payloads: &'static [Bytes] = common::payloads().leak();
    (payloads, common::facts(payloads))
}

#[cfg(target_os = "linux")]
fn extract_setup() -> Vec<Bytes> {
    let payloads = common::payloads();
    for p in &payloads {
        black_box(tgin_botapi::extract(p).expect("valid"));
    }
    payloads
}

#[cfg(target_os = "linux")]
#[library_benchmark]
#[bench::updates(setup = extract_setup, config = limit(EXTRACT_IR))]
fn extract(payloads: Vec<Bytes>) -> usize {
    let n = payloads
        .iter()
        .filter(|p| black_box(tgin_botapi::extract(black_box(p))).is_ok())
        .count();
    // Keeps freeing the fixture out of the count.
    std::mem::forget(payloads);
    n
}

#[cfg(target_os = "linux")]
struct Routing {
    hot: HotPath,
    facts: Vec<(i64, Facts<'static>)>,
    scratch: MatchScratch,
}

#[cfg(target_os = "linux")]
fn route_all(r: &mut Routing) -> usize {
    let state = r.hot.engine.state();
    let mut ids = SmallVec::new();
    r.facts
        .iter()
        .filter(|(_, f)| {
            black_box(state.table.route(r.hot.source, f, &mut ids, &mut r.scratch)).is_some()
        })
        .count()
}

#[cfg(target_os = "linux")]
fn route_setup() -> Routing {
    let (_, facts) = fixture();
    let mut r = Routing {
        hot: HotPath::new(),
        facts,
        scratch: MatchScratch::default(),
    };
    route_all(&mut r);
    r
}

#[cfg(target_os = "linux")]
#[library_benchmark]
#[bench::updates(setup = route_setup, config = limit(ROUTE_IR))]
fn route(mut r: Routing) -> usize {
    let n = route_all(&mut r);
    std::mem::forget(r);
    n
}

#[cfg(target_os = "linux")]
struct Queueing {
    queue: LeaseQueue<Envelope>,
    holder: HolderId,
    envelopes: Vec<Envelope>,
}

#[cfg(target_os = "linux")]
fn queue_setup() -> Queueing {
    let hot = HotPath::new();
    let q = Queueing {
        queue: common::queue(),
        holder: HolderId("bench".into()),
        envelopes: common::payloads()
            .iter()
            .map(|p| common::envelope(hot.source, p))
            .collect(),
    };
    common::commit_lease_ack(&q.queue, &q.holder, &q.envelopes);
    q
}

#[cfg(target_os = "linux")]
#[library_benchmark]
#[bench::updates(setup = queue_setup, config = limit(QUEUE_IR))]
fn commit_lease_ack(q: Queueing) -> usize {
    common::commit_lease_ack(&q.queue, &q.holder, black_box(&q.envelopes));
    let left = q.queue.stats().available;
    std::mem::forget(q);
    left
}

#[cfg(target_os = "linux")]
struct Admitting {
    hot: HotPath,
    payloads: &'static [Bytes],
    facts: Vec<(i64, Facts<'static>)>,
}

#[cfg(target_os = "linux")]
fn admit_setup() -> Admitting {
    let (payloads, facts) = fixture();
    let a = Admitting {
        hot: HotPath::new(),
        payloads,
        facts,
    };
    a.hot.admit(a.payloads, &a.facts);
    a.hot.drain();
    a
}

#[cfg(target_os = "linux")]
#[library_benchmark]
#[bench::updates(setup = admit_setup, config = limit(ADMIT_IR))]
fn admit(a: Admitting) -> usize {
    let n = a.hot.admit(a.payloads, black_box(&a.facts));
    std::mem::forget(a);
    n
}

#[cfg(target_os = "linux")]
library_benchmark_group!(name = facts, benchmarks = extract);
#[cfg(target_os = "linux")]
library_benchmark_group!(name = rules, benchmarks = route);
#[cfg(target_os = "linux")]
library_benchmark_group!(name = queue, benchmarks = commit_lease_ack);
#[cfg(target_os = "linux")]
library_benchmark_group!(name = engine, benchmarks = admit);

#[cfg(target_os = "linux")]
main!(library_benchmark_groups = [facts, rules, queue, engine]);

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("hot_path_ir: Callgrind benchmarks run on Linux only");
}
