//! The hot path's allocation budget: splitting `getUpdates` bodies,
//! extracting facts and admitting into the engine allocates at most twice per
//! update once warm.
//!
//! This binary installs the shared counting global allocator
//! (`support/counting_alloc.rs`). Its allocation counter is thread-local,
//! so libtest's other threads do not disturb a measurement, and the
//! engine's runtime is never driven, so its tasks never run on this thread
//! during one.

#[path = "../benches/common/mod.rs"]
mod common;
mod support {
    pub mod counting_alloc;
}

use bytes::Bytes;

use common::HotPath;
use support::counting_alloc::{Counting, thread_allocations};

#[global_allocator]
static GLOBAL: Counting = Counting;

/// The allocations this thread makes while `f` runs.
fn allocations<R>(f: impl FnOnce() -> R) -> (u64, R) {
    let before = thread_allocations();
    let out = f();
    (thread_allocations() - before, out)
}

/// Most allocations per update on the warm hot path.
const BUDGET_PER_UPDATE: f64 = 2.0;
/// Rounds over the fixture before the measured one.
const WARM_UP_ROUNDS: usize = 3;
/// The updates each consumer holds after one round over a fixture: 1751
/// placements of 1000 updates.
const PENDING: [(&str, usize); 3] = [("workers", 813), ("analytics", 818), ("hooks", 120)];

#[test]
fn counter_sees_this_threads_allocations() {
    let (n, boxed) = allocations(|| Box::new(7_u64));
    assert_eq!(n, 1);
    let (n, ()) = allocations(|| drop(boxed));
    assert_eq!(n, 0, "deallocations are not counted");
    let (n, v) = allocations(|| {
        let mut v = Vec::<u8>::with_capacity(1);
        v.extend_from_slice(&[1, 2, 3, 4]);
        v
    });
    assert_eq!((n, v.len()), (2, 4), "a reallocation counts");
}

/// Allocations per update while the warm hot path ingests `updates` (one
/// update per line) in `getUpdates` bodies of 100.
fn per_update(updates: &str) -> f64 {
    let hot = HotPath::new();
    let raw = common::bodies(updates);
    let count = updates.lines().filter(|l| !l.is_empty()).count();
    assert_eq!(count, 1000, "the fixture holds 1000 updates");

    let round = || {
        // A fresh body per poll, as the HTTP client returns one; copying it
        // in is the network's cost, not the hot path's.
        let bodies: Vec<Bytes> = raw.iter().map(|b| Bytes::from(b.clone())).collect();
        let (n, admitted) = allocations(|| bodies.iter().map(|b| hot.ingest(b)).sum::<usize>());
        assert_eq!(admitted, count, "every update is admitted");
        // The same routing for every fixture, so their budgets compare.
        assert_eq!(hot.pending_by_consumer(), PENDING, "updates per consumer");
        drop(bodies);
        hot.drain();
        n
    };
    for _ in 0..WARM_UP_ROUNDS {
        round();
    }
    let n = round();
    let per_update = n as f64 / count as f64;
    eprintln!("hot path: {n} allocations for {count} updates ({per_update:.3} per update)");
    per_update
}

#[test]
fn hot_path_allocations_per_update() {
    let per_update = per_update(common::UPDATES);
    assert!(
        per_update <= BUDGET_PER_UPDATE,
        "{per_update:.3} allocations per update, budget {BUDGET_PER_UPDATE}"
    );
}

/// Every text, caption and callback data non-ASCII, which Telegram sends
/// `\u`-escaped: what a Russian- or Kazakh-language bot ingests.
#[test]
fn hot_path_allocations_per_update_escaped_text() {
    let per_update = per_update(common::UPDATES_ESCAPED);
    assert!(
        per_update <= BUDGET_PER_UPDATE,
        "{per_update:.3} allocations per update, budget {BUDGET_PER_UPDATE}"
    );
}
