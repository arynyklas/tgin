//! A counting global allocator for test binaries: the workspace's one
//! `unsafe` exception. It forwards to [`System`] and keeps two records:
//! - each thread's allocations, zeroed allocations and reallocations, which
//!   `crates/tgin-botapi/tests/alloc_budget.rs` counts on the hot path (a
//!   thread-local, so libtest's other threads do not disturb it);
//! - the process's live heap bytes and their peak, which
//!   `crates/tgin/tests/gateway_bounded.rs` reads to bound what the gateway
//!   buffers while it streams.
//!
//! A binary includes this file as a module and installs [`Counting`] with
//! its own `#[global_allocator]` line. Each uses only part of it.
#![allow(unsafe_code, dead_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicIsize, Ordering};

/// Forwards to [`System`], counting as described in the module docs.
pub struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicIsize = AtomicIsize::new(0);

/// The allocations, zeroed allocations and reallocations this thread has
/// made.
pub fn thread_allocations() -> u64 {
    ALLOCATIONS.with(Cell::get)
}

/// Live heap bytes now; the peak starts again from here.
pub fn reset_peak() -> isize {
    let live = LIVE.load(Ordering::SeqCst);
    PEAK.store(live, Ordering::SeqCst);
    live
}

/// The most live heap bytes since [`reset_peak`].
pub fn peak() -> isize {
    PEAK.load(Ordering::SeqCst)
}

fn count() {
    // `try_with`: the allocator may run while the thread's locals are torn down.
    let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
}

fn grew(by: usize) {
    let by = isize::try_from(by).unwrap_or(isize::MAX);
    let now = LIVE.fetch_add(by, Ordering::Relaxed) + by;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

fn shrank(by: usize) {
    LIVE.fetch_sub(isize::try_from(by).unwrap_or(isize::MAX), Ordering::Relaxed);
}

// SAFETY: every method forwards its arguments unchanged to `System`, which
// upholds the `GlobalAlloc` contract; the bookkeeping touches only a
// const-initialized thread-local `Cell` and static atomics, which never
// allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: the caller upholds `alloc`'s contract (non-zero size).
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            grew(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: the caller upholds `alloc_zeroed`'s contract.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            grew(layout.size());
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: the caller upholds `realloc`'s contract: `ptr` came from this
        // allocator (hence from `System`) with `layout`.
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            grew(new_size);
            shrank(layout.size());
        }
        new
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `dealloc`'s contract: `ptr` came from this
        // allocator (hence from `System`) with `layout`.
        unsafe { System.dealloc(ptr, layout) };
        shrank(layout.size());
    }
}
