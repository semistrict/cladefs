// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What the slabs synchronize with: their locks and condition variables, std's, or, with the
//! `shuttle` feature, Shuttle's, whose scheduler then chooses every interleaving of a test's
//! threads (`slabs::shuttle_tests`); the waits made of them, [`Pulse`] and [`Signal`]; and the
//! rules their concurrency relies on ([`Rules`]), which the Shuttle tests turn off one at a time
//! to see a test fail: a test that never fails proves nothing.

#[cfg(feature = "shuttle")]
pub(super) use shuttle::sync::{Condvar, Mutex};
#[cfg(not(feature = "shuttle"))]
pub(super) use std::sync::{Condvar, Mutex};

/// The rules the slabs' concurrency relies on, each of which a Shuttle test shows to be needed by
/// turning it off (`slabs::shuttle_tests`).
#[derive(Clone, Copy, Debug)]
pub struct Rules {
    /// An allocation that runs being punched would make room for, as their memory goes back
    /// outside the arena's lock, waits for them, rather than ask relief to evict more, or fail.
    pub allocations_wait_for_punches: bool,
    /// A run a VM process was staged since it was allocated, freed, waits in quarantine for the
    /// VM processes it was staged to (doc-26, spec rule waitsForStaged), rather than being free at
    /// once.
    pub staged_runs_wait_for_their_stages: bool,
}

impl Rules {
    pub const ALL: Self =
        Self { allocations_wait_for_punches: true, staged_runs_wait_for_their_stages: true };
}

/// The rules in force: all of them.
#[cfg(not(feature = "shuttle"))]
pub fn rules() -> Rules {
    Rules::ALL
}

#[cfg(feature = "shuttle")]
static RULES: std::sync::Mutex<Rules> = std::sync::Mutex::new(Rules::ALL);

/// The rules in force: all of them, but in a Shuttle test that turned one off.
#[cfg(feature = "shuttle")]
pub fn rules() -> Rules {
    *RULES.lock().unwrap()
}

/// Sets the rules in force, for the Shuttle tests that turn one off.
#[cfg(all(test, feature = "shuttle"))]
pub fn set_rules(rules: Rules) {
    *RULES.lock().unwrap() = rules;
}

/// Starts the slabs' thread `name`, running `f`: the arenas' relief, which acts as allocations ask.
/// Under Shuttle none starts, as Shuttle's threads are its tests' alone; nor under madsim, which
/// runs one thread (doc-24). Fails as the host fails to start it.
pub(super) fn try_spawn<F>(name: &str, f: F) -> std::io::Result<bool>
where
    F: FnOnce() + Send + 'static,
{
    if cfg!(feature = "shuttle") || cfg!(madsim) {
        return Ok(false);
    }
    std::thread::Builder::new().name(name.into()).spawn(f).map(|_| true)
}

/// Wakes every waiter as it pulses: a waiter that read the count of pulses ([`Pulse::count`])
/// before what it waits for could change waits for the next ([`Pulse::wait_past`]), and misses
/// none.
pub(super) struct Pulse {
    count: Mutex<u64>,
    cond: Condvar,
}

impl Pulse {
    pub(super) fn new() -> Self {
        Self { count: Mutex::new(0), cond: Condvar::new() }
    }

    /// The pulses so far, for [`Pulse::wait_past`].
    pub(super) fn count(&self) -> u64 {
        *self.count.lock().unwrap()
    }

    /// Wakes every waiter.
    pub(super) fn pulse(&self) {
        *self.count.lock().unwrap() += 1;
        self.cond.notify_all();
    }

    /// Waits for a pulse after the `count`th.
    pub(super) fn wait_past(&self, count: u64) {
        let mut now = self.count.lock().unwrap();
        while *now == count {
            now = self.cond.wait(now).unwrap();
        }
    }
}

/// An event that each signal releases one wait of, after which it is unsignaled again.
pub(super) struct Signal {
    signaled: Mutex<bool>,
    cond: Condvar,
}

impl Signal {
    pub(super) fn new() -> Self {
        Self { signaled: Mutex::new(false), cond: Condvar::new() }
    }

    /// Signals it, releasing a wait, now or the next.
    pub(super) fn signal(&self) {
        *self.signaled.lock().unwrap() = true;
        self.cond.notify_one();
    }

    /// Waits for it to be signaled, and unsignals it.
    pub(super) fn wait(&self) {
        let mut signaled = self.signaled.lock().unwrap();
        while !*signaled {
            signaled = self.cond.wait(signaled).unwrap();
        }
        *signaled = false;
    }

    /// Waits for it to be signaled or for `duration` to elapse, unsignaling if signaled.
    #[cfg(not(feature = "shuttle"))]
    pub(super) fn wait_timeout(&self, duration: std::time::Duration) {
        let mut signaled = self.signaled.lock().unwrap();
        if !*signaled && !duration.is_zero() {
            let (guard, _) = self.cond.wait_timeout(signaled, duration).unwrap();
            signaled = guard;
        }
        *signaled = false;
    }

    /// Under Shuttle no real OS eviction thread spawns; falls back to [`Self::wait`].
    #[cfg(feature = "shuttle")]
    pub(super) fn wait_timeout(&self, _duration: std::time::Duration) {
        self.wait();
    }
}
