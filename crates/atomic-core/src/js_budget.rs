// SPDX-License-Identifier: Apache-2.0
//! Memory quotas for one-shot ECMAScript child processes. Boa 0.22 allocates
//! ArrayBuffer backing stores through aligned-vec with ConstAlign<64>. Count
//! all allocations of that alignment conservatively, including native ones.
//! Counts start at process startup so frees and reallocations on any thread
//! release exactly the amount charged. Only compiler/helper children arm limits.
#![allow(unsafe_code)]
use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
};

pub(crate) const HEAP_EXIT: i32 = crate::child_exit::HEAP;
pub(crate) const BUFFER_EXIT: i32 = crate::child_exit::BUFFERS;
pub(crate) const DEADLINE_EXIT: i32 = crate::child_exit::DEADLINE;
// Account before main, so even allocations freed after dispatch have a matching charge.
// Ordinary CLI processes never execute guest code in-process.
static COUNTING: AtomicBool = AtomicBool::new(true);
pub(crate) fn ordinary_process() {
    COUNTING.store(false, Relaxed);
}
struct Counter {
    live: AtomicUsize,
    ceiling: AtomicUsize,
    status: i32,
}
impl Counter {
    const fn new(status: i32) -> Self {
        Self {
            live: AtomicUsize::new(0),
            ceiling: AtomicUsize::new(usize::MAX),
            status,
        }
    }
    fn charge(&self, size: usize) {
        if !COUNTING.load(Relaxed) {
            return;
        }
        let old = self.live.fetch_add(size, Relaxed);
        if old
            .checked_add(size)
            .is_none_or(|n| n > self.ceiling.load(Relaxed))
        {
            // SAFETY: _exit neither allocates nor runs destructors in a process
            // that has exhausted its budget. The parent decodes this status.
            unsafe { libc::_exit(self.status) }
        }
    }
    fn release(&self, size: usize) {
        if COUNTING.load(Relaxed) {
            self.live.fetch_sub(size, Relaxed);
        }
    }
    fn limit(&self, extra: usize) {
        self.ceiling
            .store(self.live.load(Relaxed).saturating_add(extra), Relaxed);
    }
}
static HEAP: Counter = Counter::new(HEAP_EXIT);
static BUFFERS: Counter = Counter::new(BUFFER_EXIT);

pub(crate) fn enter(heap: usize, buffers: usize) {
    assert!(
        COUNTING.load(Relaxed),
        "engine budgets require a dedicated child"
    );
    HEAP.limit(heap);
    BUFFERS.limit(buffers);
}
fn counter(layout: Layout) -> &'static Counter {
    if layout.align() == 64 {
        &BUFFERS
    } else {
        &HEAP
    }
}
struct Meter;
#[global_allocator]
static ALLOCATOR: Meter = Meter;
// SAFETY: all memory comes from System, with exactly the caller's layout.
// Accounting uses only atomics and the allocation-free exit above.
unsafe impl GlobalAlloc for Meter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let c = counter(layout);
        c.charge(layout.size());
        let p = unsafe { System.alloc(layout) };
        if p.is_null() {
            c.release(layout.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let c = counter(layout);
        c.charge(layout.size());
        let p = unsafe { System.alloc_zeroed(layout) };
        if p.is_null() {
            c.release(layout.size());
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        counter(layout).release(layout.size());
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let c = counter(layout);
        let growth = size.saturating_sub(layout.size());
        c.charge(growth);
        let p = unsafe { System.realloc(ptr, layout, size) };
        c.release(if p.is_null() {
            growth
        } else {
            layout.size().saturating_sub(size)
        });
        p
    }
}
