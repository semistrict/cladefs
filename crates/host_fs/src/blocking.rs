// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: enforces that slow disk calls never run directly on an async runtime's workers
//! (board 10824; past bugs: boards 5378, 5438, 5440, 605, 604, 580).
//!
//! Sanctioned ways off the workers (`cladefsd::run::off_the_runtime`,
//! `local_cell::upload::blocking`, `bucket_cell::fetch::blocking`) wrap their synchronous disk work
//! in [`may_block`]. In debug and test builds (`debug_assertions`), every disk-touching `host_fs`
//! call checks [`assert_may_block`] and panics if it runs inside a tokio (or madsim) runtime
//! outside a [`may_block`] scope. Release builds compile the check out completely.

#[cfg(debug_assertions)]
thread_local! {
    /// Depth of [`may_block`] scopes currently active on this OS thread.
    // Per thread: tracks whether this OS thread is inside a synchronous blocking scope; under
    // madsim, `may_block` takes a synchronous closure and never yields across `.await`, so no
    // other simulated host runs while the depth is non-zero.
    static MAY_BLOCK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Runs `work` with the current thread marked allowed to block on the host's disk: what the
/// sanctioned ways off an async runtime's workers (`cladefsd::run::off_the_runtime`,
/// `local_cell::upload::blocking`, `bucket_cell::fetch::blocking`) wrap their synchronous disk
/// work in (board 10824).
///
/// Reentrant: nested calls keep blocking allowed until the outermost returns, even if `work`
/// panics. Compiled out of release builds (`not(debug_assertions)`).
#[inline]
pub fn may_block<T>(work: impl FnOnce() -> T) -> T {
    #[cfg(debug_assertions)]
    {
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                MAY_BLOCK.with(|depth| depth.set(depth.get() - 1));
            }
        }
        MAY_BLOCK.with(|depth| depth.set(depth.get() + 1));
        let _guard = Guard;
        work()
    }
    #[cfg(not(debug_assertions))]
    work()
}

/// Asserts, in debug and test builds, that the current thread is either outside an async runtime
/// or inside a [`may_block`] scope. Compiled out of release builds.
#[inline]
#[track_caller]
pub(crate) fn assert_may_block(what: &'static str) {
    #[cfg(debug_assertions)]
    {
        if MAY_BLOCK.with(|depth| depth.get() > 0) {
            return;
        }
        #[cfg(not(madsim))]
        let on_runtime = tokio::runtime::Handle::try_current().is_ok();
        #[cfg(madsim)]
        let on_runtime = madsim::runtime::Handle::try_current().is_ok();
        if on_runtime {
            panic!(
                "host_fs::{what} called on an async runtime thread outside may_block \
                 (run disk I/O through off_the_runtime or upload::blocking; board 10824)"
            );
        }
    }
    #[cfg(not(debug_assertions))]
    let _ = what;
}
