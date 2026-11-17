// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the heap of this crate's test processes, counted as it is allocated ([`Bounded`]), so
//! that a test bounds what the code it runs holds: what the daemon holds of a hostile VM process's
//! (`cladefsd::tests::hostile`, TASK-150.3), or what a store's uploads hold while the bucket takes
//! none of them (`bucket`'s tests, TASK-317). nextest runs each test in a process of its own, so
//! that what one counts is its own.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Counts what the process holds of its heap ([`held`]), and, while a [`LIMIT`] is set, aborts
/// past it, saying the [`CASE`] and where it allocated: as fxfs-posix's corrupt tests bound theirs.
/// Aborted, as what is asked for past it might be asked for without end, the machine's memory
/// with it; and so as soon as it is asked for.
pub(crate) struct Bounded;

static HELD: AtomicUsize = AtomicUsize::new(0);
/// The most it held since [`peak_while`] last looked.
static PEAK: AtomicUsize = AtomicUsize::new(0);
/// The most the heap may hold, or 0, which checks nothing.
pub(crate) static LIMIT: AtomicUsize = AtomicUsize::new(0);
/// The case running, as the test says it, which a heap past its limit names.
pub(crate) static CASE: Mutex<String> = Mutex::new(String::new());

#[global_allocator]
static ALLOCATOR: Bounded = Bounded;

thread_local! {
    /// How many times this thread allocated, or grew an allocation ([`allocations_while`]): of
    /// its own, whatever the process's other threads do meanwhile.
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

/// How many times this thread allocated, or grew an allocation, while `work` ran, with what it
/// returned: so that a test asserts a path allocates nothing (TASK-453).
pub(crate) fn allocations_while<R>(work: impl FnOnce() -> R) -> (u64, R) {
    let count = || ALLOCATIONS.with(Cell::get);
    let before = count();
    let done = work();
    (count() - before, done)
}

/// What the process holds of its heap now.
pub(crate) fn held() -> usize {
    HELD.load(Ordering::Relaxed)
}

impl Bounded {
    fn check(size: usize) {
        // Gone with its thread, as it ends: nothing to count for it then.
        let _ = ALLOCATIONS.try_with(|allocations| allocations.set(allocations.get() + 1));
        let held = HELD.fetch_add(size, Ordering::Relaxed) + size;
        PEAK.fetch_max(held, Ordering::Relaxed);
        let limit = LIMIT.load(Ordering::Relaxed);
        if limit != 0 && held > limit {
            // Nothing below may come back here.
            LIMIT.store(0, Ordering::Relaxed);
            let backtrace = std::backtrace::Backtrace::force_capture();
            let case = CASE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            eprintln!("{held} bytes held, of at most {limit}, in {case}:\n{backtrace}");
            std::process::abort();
        }
    }
}

// SAFETY: this only counts what `System` does.
unsafe impl GlobalAlloc for Bounded {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::check(layout.size());
        // SAFETY: As the caller promises.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        Self::check(layout.size());
        // SAFETY: As the caller promises.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        HELD.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: As the caller promises.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        Self::check(size);
        HELD.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: As the caller promises.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

/// The most the heap held past what it holds now while `work` ran.
pub(crate) fn peak_while(work: impl FnOnce()) -> u64 {
    let before = HELD.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    work();
    PEAK.load(Ordering::Relaxed).saturating_sub(before) as u64
}
