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
    /// Counts `size` held, then allocates it with `allocate`: counted first, as a request past
    /// the limit aborts before it is asked for; taken back if `allocate` is refused (null), as the
    /// system then holds nothing of it.
    fn counted(size: usize, allocate: impl FnOnce() -> *mut u8) -> *mut u8 {
        Self::check(size);
        let pointer = allocate();
        if pointer.is_null() {
            HELD.fetch_sub(size, Ordering::Relaxed);
        }
        pointer
    }

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

// SAFETY: each method is `System`'s, with the caller's arguments, the contract `GlobalAlloc` asks
// of the caller passed on unchanged; what it adds, counting, touches no memory but atomics, and
// can't unwind: HELD counts what `System` holds, refusals taken back, so it stays below
// `usize::MAX`, and past the limit it aborts.
unsafe impl GlobalAlloc for Bounded {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's `layout`, of nonzero size, as `GlobalAlloc::alloc` asks of it.
        Self::counted(layout.size(), || unsafe { System.alloc(layout) })
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as `alloc`.
        Self::counted(layout.size(), || unsafe { System.alloc_zeroed(layout) })
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        HELD.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: the caller's `pointer`, allocated by this allocator (`System`, as each method
        // delegates to it) with `layout`, as `GlobalAlloc::dealloc` asks of it.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: the caller's `pointer` and `layout`, as `dealloc`, and `size`, nonzero and
        // not overflowing `isize` once rounded to `layout`'s alignment, as `GlobalAlloc::realloc`
        // asks of it.
        let grown = Self::counted(size, || unsafe { System.realloc(pointer, layout, size) });
        // Refused, the old allocation is still held, as `realloc` leaves it.
        if !grown.is_null() {
            HELD.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        grown
    }
}

/// The most the heap held past what it holds now while `work` ran.
pub(crate) fn peak_while(work: impl FnOnce()) -> u64 {
    let before = HELD.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    work();
    PEAK.load(Ordering::Relaxed).saturating_sub(before) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the system refuses to allocate, the heap doesn't hold: counted, it stayed held,
    /// and each refusal brought the count nearer a limit and to overflow (TASK-444.13).
    #[test]
    fn a_refused_allocation_holds_nothing() {
        let before = held();
        let mut bytes = Vec::<u8>::new();
        assert!(bytes.try_reserve_exact(isize::MAX as usize).is_err());
        assert_eq!(held(), before);
    }

    /// A growth the system refuses leaves what was held held: it was counted as the new size
    /// held and the old one freed.
    #[test]
    fn a_refused_growth_holds_what_it_held() {
        let mut bytes = vec![0u8; 4096];
        let before = held();
        assert!(bytes.try_reserve_exact(isize::MAX as usize - bytes.len()).is_err());
        assert_eq!(held(), before);
    }
}
