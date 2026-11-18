// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the heap of this crate's unit test processes, counted as it is allocated ([`held`]), so
//! that a test bounds what the code it runs holds: what a map of extents made of another copies
//! (`recovery::tests::changes_bound_what_maps_made_of_others_copy`, TASK-453). nextest runs each
//! test in a process of its own, so that what one counts is its own.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Counts what the process holds of its heap ([`held`]).
struct Counted;

static HELD: AtomicUsize = AtomicUsize::new(0);

#[global_allocator]
static ALLOCATOR: Counted = Counted;

/// What the process holds of its heap now.
pub(crate) fn held() -> usize {
    HELD.load(Ordering::Relaxed)
}

// SAFETY: this only counts what `System` does.
unsafe impl GlobalAlloc for Counted {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        HELD.fetch_add(layout.size(), Ordering::Relaxed);
        // SAFETY: As the caller promises.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        HELD.fetch_add(layout.size(), Ordering::Relaxed);
        // SAFETY: As the caller promises.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        HELD.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: As the caller promises.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        HELD.fetch_add(size, Ordering::Relaxed);
        HELD.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: As the caller promises.
        unsafe { System.realloc(pointer, layout, size) }
    }
}
