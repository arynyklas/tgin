//! The hot path's allocation budget: splitting `getUpdates` bodies,
//! extracting facts and admitting into the engine allocates at most twice per
//! update once warm.
//!
//! This binary installs a counting global allocator. Its counter is
//! thread-local, so libtest's other threads do not disturb a measurement, and
//! the engine's runtime is never driven, so its tasks never run on this
//! thread during one.
#![allow(unsafe_code)]

#[path = "../benches/common/mod.rs"]
mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use bytes::Bytes;

use common::HotPath;

/// Forwards to [`System`], counting the calling thread's allocations,
/// zeroed allocations and reallocations.
struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

fn count() {
    // `try_with`: the allocator may run while the thread's locals are torn down.
    let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
}

// SAFETY: every method forwards its arguments unchanged to `System`, which
// upholds the `GlobalAlloc` contract; counting touches only a const-initialized
// thread-local `Cell`, which never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: the caller upholds `alloc`'s contract (non-zero size).
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: the caller upholds `alloc_zeroed`'s contract.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: the caller upholds `realloc`'s contract: `ptr` came from this
        // allocator (hence from `System`) with `layout`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `dealloc`'s contract: `ptr` came from this
        // allocator (hence from `System`) with `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// The allocations this thread makes while `f` runs.
fn allocations<R>(f: impl FnOnce() -> R) -> (u64, R) {
    let before = ALLOCATIONS.with(Cell::get);
    let out = f();
    (ALLOCATIONS.with(Cell::get) - before, out)
}

/// Most allocations per update on the warm hot path.
const BUDGET_PER_UPDATE: f64 = 2.0;
/// Rounds over the fixture before the measured one.
const WARM_UP_ROUNDS: usize = 3;

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

#[test]
fn hot_path_allocations_per_update() {
    let hot = HotPath::new();
    let raw = common::bodies();
    let updates = common::payloads().len();
    assert_eq!(updates, 1000, "the fixture holds 1000 updates");

    let round = || {
        // A fresh body per poll, as the HTTP client returns one; copying it
        // in is the network's cost, not the hot path's.
        let bodies: Vec<Bytes> = raw.iter().map(|b| Bytes::from(b.clone())).collect();
        let (n, admitted) = allocations(|| bodies.iter().map(|b| hot.ingest(b)).sum::<usize>());
        assert_eq!(admitted, updates, "every update is admitted");
        drop(bodies);
        hot.drain();
        n
    };
    for _ in 0..WARM_UP_ROUNDS {
        round();
    }
    let n = round();

    let per_update = n as f64 / updates as f64;
    eprintln!("hot path: {n} allocations for {updates} updates ({per_update:.3} per update)");
    assert!(
        per_update <= BUDGET_PER_UPDATE,
        "{per_update:.3} allocations per update, budget {BUDGET_PER_UPDATE}"
    );
}
