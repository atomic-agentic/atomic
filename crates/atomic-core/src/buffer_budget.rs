// SPDX-License-Identifier: Apache-2.0
//! An `ArrayBuffer` allocator with a byte budget, for the compiler worker's
//! isolate and helper isolates. ArrayBuffer and typed-array contents live
//! outside V8's heap, so the heap limit does not bound them; this allocator
//! does, by refusing an allocation that would take an isolate past its
//! budget. V8 then collects garbage as a last resort and either throws a
//! `RangeError` in the isolate or reaches the near-heap-limit callback; the
//! isolate's owner reads [`Budget::refused`] and reports the limit.
#![allow(unsafe_code)]

use deno_core::v8;
use std::alloc::{Layout, alloc, alloc_zeroed, dealloc};
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// The bytes an isolate's ArrayBuffers may hold at once.
pub(crate) struct Budget {
    used: AtomicUsize,
    limit: usize,
    refused: AtomicBool,
}

impl Budget {
    fn reserve(&self, len: usize) -> bool {
        let reserved = self
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(len).filter(|total| *total <= self.limit)
            })
            .is_ok();
        if !reserved {
            self.refused.store(true, Ordering::Release);
        }
        reserved
    }

    /// Whether an allocation was ever refused.
    pub(crate) fn refused(&self) -> bool {
        self.refused.load(Ordering::Acquire)
    }

    fn release(&self, len: usize) {
        self.used.fetch_sub(len, Ordering::AcqRel);
    }
}

// V8 may ask for zero bytes; a zero-sized layout is not allowed, so every
// block is at least one byte. Sixteen bytes matches malloc's alignment, which
// V8 assumes for backing stores.
fn layout(len: usize) -> Option<Layout> {
    Layout::from_size_align(len.max(1), 16).ok()
}

fn allocate_with(budget: &Budget, len: usize, zeroed: bool) -> *mut c_void {
    let Some(layout) = layout(len) else {
        return std::ptr::null_mut();
    };
    if !budget.reserve(len) {
        return std::ptr::null_mut();
    }
    // SAFETY: `layout` has a nonzero size.
    let data = unsafe {
        if zeroed {
            alloc_zeroed(layout)
        } else {
            alloc(layout)
        }
    };
    if data.is_null() {
        budget.release(len);
    }
    data.cast()
}

unsafe extern "C" fn allocate(budget: &Budget, len: usize) -> *mut c_void {
    allocate_with(budget, len, true)
}

unsafe extern "C" fn allocate_uninitialized(budget: &Budget, len: usize) -> *mut c_void {
    allocate_with(budget, len, false)
}

unsafe extern "C" fn free(budget: &Budget, data: *mut c_void, len: usize) {
    if data.is_null() {
        return;
    }
    if let Some(layout) = layout(len) {
        // SAFETY: V8 frees each block once, with the length it was allocated
        // with, so `data` came from `allocate_with` with this same layout.
        unsafe { dealloc(data.cast(), layout) };
        budget.release(len);
    }
}

unsafe extern "C" fn drop_budget(budget: *const Budget) {
    // SAFETY: `handle` was produced by `Arc::into_raw` in `bounded`, and V8
    // calls this once, when it releases the allocator.
    drop(unsafe { Arc::from_raw(budget) });
}

static VTABLE: v8::RustAllocatorVtable<Budget> = v8::RustAllocatorVtable {
    allocate,
    allocate_uninitialized,
    free,
    drop: drop_budget,
};

/// An allocator that refuses to hold more than `limit` bytes at once, and its
/// budget, to see whether it refused.
pub(crate) fn bounded(limit: usize) -> (v8::UniqueRef<v8::Allocator>, Arc<Budget>) {
    let budget = Arc::new(Budget {
        used: AtomicUsize::new(0),
        limit,
        refused: AtomicBool::new(false),
    });
    // SAFETY: the handle is a live `Budget` that the allocator owns (one
    // strong count, released by `drop_budget`), and VTABLE's functions take a
    // `&Budget`.
    let allocator = unsafe { v8::new_rust_allocator(Arc::into_raw(budget.clone()), &VTABLE) };
    (allocator, budget)
}
