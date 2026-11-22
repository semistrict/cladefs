// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The quarantine of runs freed that a VM process may map still (TASK-79.9, doc-26). A window's
//! second stage in a VM process makes the window's changes as they are sent, after the daemon made
//! them, and they are sent with the window's lock held, so it waits for none: a run whose mapping
//! it took away is reached there until the VM process has made the change. So a run staged to a VM
//! process since it was allocated ([`PartSlab::stage_to`]) is not freed as its holder drops it, but
//! quarantined: handed, with the VM processes it was staged to, to what the daemon gives
//! ([`quarantine_with`]), which frees it once each of them has made the changes it was sent so far,
//! or is dead, by dropping it. A run staged to none, or with no quarantine given, is freed at once,
//! as a window of this process makes each change before it returns.
//!
//! [`PartSlab::stage_to`]: super::PartSlab::stage_to

use super::sync::rules;
use super::{Arena, Held, VmProcessId, relief};
use smallvec::SmallVec;
use std::sync::{Arc, OnceLock};

/// A run freed that VM processes were staged, which goes back to its arena once dropped.
pub struct Quarantined {
    /// The run, freed as this is dropped.
    held: Option<Held>,
    /// The VM processes it was staged to, each once.
    staged: SmallVec<[VmProcessId; 4]>,
    /// The arena relief's pass evicted it for, freed on its thread, which is owed it until it
    /// comes back (TASK-161).
    relieving: Option<Arc<Arena>>,
}

impl Quarantined {
    /// `held`, freed, quarantined for `staged`: counted by its arena until it comes back
    /// (TASK-106), and owed to the arena relief evicts it for, if it does (TASK-161).
    fn new(held: Held, staged: SmallVec<[VmProcessId; 4]>) -> Self {
        let pages = u64::from(held.run.pages);
        held.arena.state.lock().unwrap().quarantined += pages;
        held.arena.node.quarantined.fetch_add(pages, std::sync::atomic::Ordering::Relaxed);
        let relieving = relief::relieving().and_then(|arena| held.arena.node.arena(arena));
        if let Some(arena) = &relieving {
            arena.state.lock().unwrap().relief.owed += pages;
        }
        Self { held: Some(held), staged, relieving }
    }

    /// The VM processes it was staged to, each once: those it waits for.
    pub fn staged(&self) -> &[VmProcessId] {
        &self.staged
    }

    /// Whether the page at `at` is of it.
    pub fn contains(&self, at: super::PhysLocation) -> bool {
        let held = self.held();
        let start = super::Arena::offset(held.run);
        let len = u64::from(held.run.pages) * super::page_size();
        at.arena == held.arena.id && (start..start + len).contains(&at.offset)
    }

    fn held(&self) -> &Held {
        self.held.as_ref().expect("a run quarantined until dropped")
    }
}

impl std::fmt::Debug for Quarantined {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let held = self.held();
        f.debug_struct("Quarantined")
            .field("arena", &held.arena.id)
            .field("run", &held.run)
            .field("staged", &self.staged)
            .finish()
    }
}

impl Drop for Quarantined {
    fn drop(&mut self) {
        let held = self.held.take().expect("a run quarantined once");
        let pages = u64::from(held.run.pages);
        {
            let mut state = held.arena.state.lock().unwrap();
            state.quarantined = (state.quarantined.checked_sub(pages))
                .expect("a run came back from quarantine its arena didn't count");
        }
        held.arena.node.quarantined.fetch_sub(pages, std::sync::atomic::Ordering::Relaxed);
        // The arena relief evicted it for owed none now, its allocations waiting ask again.
        if let Some(arena) = &self.relieving {
            let repaid = {
                let mut state = arena.state.lock().unwrap();
                let owed = &mut state.relief.owed;
                *owed = owed.checked_sub(pages).expect("a run came back relief wasn't owed");
                *owed == 0
            };
            if repaid {
                arena.node.pulse();
            }
        }
        drop(held);
    }
}

/// What quarantines the runs freed, if the daemon gave it.
static QUARANTINE: OnceLock<Box<dyn Fn(Quarantined) + Send + Sync>> = OnceLock::new();

/// Has `quarantine` take the runs freed that were staged to VM processes, from now on: it frees
/// each, dropping it, once each VM process it was staged to has made the changes it was sent so
/// far, which take it away, or is dead. It is called with whatever locks the run's holder drops it
/// under, none of the slabs': it may free it at once, dropping it, as its holder would have. Given
/// once, for the process's life; Shuttle tests give theirs with `quarantine_for_test`.
pub fn quarantine_with(quarantine: impl Fn(Quarantined) + Send + Sync + 'static) {
    let given = QUARANTINE.set(Box::new(quarantine));
    assert!(given.is_ok(), "the quarantine is given once");
}

/// Frees `held`, staged to `staged`: at once, if to none, or with no quarantine given; else into
/// quarantine.
pub(super) fn free(held: Held, staged: SmallVec<[VmProcessId; 4]>) {
    let quarantine = QUARANTINE.get().filter(|_| rules().staged_runs_wait_for_their_stages);
    match quarantine {
        Some(quarantine) if !staged.is_empty() => quarantine(Quarantined::new(held, staged)),
        _ => drop(held),
    }
}

/// The quarantine of the test that holds one ([`quarantine_for_test`]).
#[cfg(all(test, feature = "shuttle"))]
type TestQuarantine = Arc<dyn Fn(Quarantined) + Send + Sync>;

/// The quarantine of the test holding one, if any ([`quarantine_for_test`]).
#[cfg(all(test, feature = "shuttle"))]
static TEST_QUARANTINE: std::sync::Mutex<Option<TestQuarantine>> = std::sync::Mutex::new(None);

/// Has `quarantine` take the runs freed that were staged, as [`quarantine_with`] would, for as
/// long as the test holds what it returns: the process's quarantine, given once for its tests,
/// hands them to the quarantine of the test holding one, and, with none, frees them at once. Tests
/// holding one run one at a time. For the Shuttle tests, whose runs share the process's quarantine.
#[cfg(all(test, feature = "shuttle"))]
pub(crate) fn quarantine_for_test(
    quarantine: impl Fn(Quarantined) + Send + Sync + 'static,
) -> QuarantineForTest {
    use std::sync::PoisonError;
    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static GIVEN: std::sync::Once = std::sync::Once::new();
    let turn = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    GIVEN.call_once(|| {
        quarantine_with(|freed| {
            let test = TEST_QUARANTINE.lock().unwrap_or_else(PoisonError::into_inner).clone();
            match test {
                Some(quarantine) => quarantine(freed),
                None => drop(freed),
            }
        })
    });
    *TEST_QUARANTINE.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(quarantine));
    QuarantineForTest { _turn: turn }
}

/// A test's quarantine ([`quarantine_for_test`]), which it holds until dropped.
#[cfg(all(test, feature = "shuttle"))]
pub(crate) struct QuarantineForTest {
    _turn: std::sync::MutexGuard<'static, ()>,
}

#[cfg(all(test, feature = "shuttle"))]
impl Drop for QuarantineForTest {
    fn drop(&mut self) {
        let test = TEST_QUARANTINE.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        drop(test);
    }
}
