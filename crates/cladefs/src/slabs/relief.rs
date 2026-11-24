// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The relief of starved arenas (TASK-50.3). An allocation that finds its arena starved asks for
//! relief, and one that can wait waits for it: the relief thread frees runs for each arena asked
//! for since its last pass, and starved still (TASK-79.76), with the evictor the daemon gives it
//! ([`relieve_with`]), the arena's own past its own cap, any arena's past the total's. A pass that
//! frees nothing for an arena starved still exhausts it: its allocations fail rather than wait for
//! ever, until a run of it is freed, a cap changes, or a later pass, which each of them still asks
//! for, frees something. What a pass evicts that is quarantined is owed to the arena it evicted
//! for until it comes back, and no pass evicts for the arena meanwhile (TASK-161): its allocations
//! wait for what is on its way rather than have relief evict the rest.
//!
//! Without relief, nothing makes a starved arena room: its allocations fail, whether or not they
//! may wait.

use super::sync::Signal;
use super::{Arena, ArenaId, HostSlabs, SLAB, Starved, Wait};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

/// What a pass frees for an arena at least: four slabs, so that the next allocations find room.
/// What it asks of [`relieve_with`]'s evictor.
pub const RELIEF_BYTES: u64 = 4 * SLAB;

/// The relief thread's: what wakes it, and whether it runs.
pub(super) struct Reliever {
    /// Signaled as an allocation asks for relief.
    wanted: Signal,
    /// Whether the thread runs: once it has gone, as its evictor panicked, the allocations it
    /// would make room for fail.
    running: AtomicBool,
}

impl Reliever {
    pub(super) fn stop(&self) {
        self.running.store(false, Ordering::Release);
        self.wanted.signal();
    }
}

thread_local! {
    /// On the relief thread, the arena its evictor is evicting for, while it is (TASK-161).
    // Per thread, the relief thread's alone: no simulated host has one (TASK-685.14).
    static RELIEVING: Cell<Option<ArenaId>> = const { Cell::new(None) };
}

/// The arena relief's pass is evicting for, if this is the relief thread, in its evictor: the runs
/// it frees, quarantined, are owed to it until they come back.
pub(super) fn relieving() -> Option<ArenaId> {
    RELIEVING.get()
}

/// Starts the relief of starved arenas on `slabs`, on a thread of its own, which frees runs with
/// `evict`. Starts once per [`HostSlabs`]; under Shuttle or madsim no thread starts.
pub(super) fn relieve_with_on(
    slabs: &Arc<HostSlabs>,
    evict: impl FnMut(Option<ArenaId>, u64) -> u64 + Send + 'static,
) -> std::io::Result<()> {
    let mut started = Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "the arenas' relief has started",
    ));
    let weak = Arc::downgrade(slabs);
    slabs.reliever.get_or_init(|| {
        let reliever = Arc::new(Reliever { wanted: Signal::new(), running: AtomicBool::new(true) });
        let thread = reliever.clone();
        let spawned =
            super::sync::try_spawn("arena-relief-thread", move || relieve(&thread, weak, evict));
        let installed = spawned.as_ref().is_ok_and(|spawned| *spawned).then_some(reliever);
        started = spawned.map(drop);
        installed
    });
    started
}

/// Starts the relief of starved arenas, on a thread of its own, which frees runs with `evict`:
/// given the arena whose runs to free, or none for any arena's, and how many bytes at least, it
/// frees what it may of them and returns the bytes it freed. It must not wait for an allocation
/// that waits for relief, as its own thread relieves them. The runs it frees on its thread that are
/// quarantined are owed to the arena it was called for, which no pass evicts for again until they
/// come back.
///
/// Relief starts once: a later call fails with `AlreadyExists`, whether or not the first started
/// its thread. Under Shuttle no thread starts, and starved arenas' allocations fail, as without
/// relief; nor under madsim.
#[cfg(not(madsim))]
pub fn relieve_with(
    evict: impl FnMut(Option<ArenaId>, u64) -> u64 + Send + 'static,
) -> std::io::Result<()> {
    relieve_with_on(crate::Host::process().slabs(), evict)
}

/// The relief thread: frees runs with `evict` for each arena asked for since its last pass, starved
/// still at the cap it was asked at, and owed nothing a pass before quarantined, the arena's own at
/// its own cap, any arena's at the total's, and records what it freed, which the arena's
/// allocations waiting then try again with.
fn relieve(
    reliever: &Reliever,
    slabs: Weak<HostSlabs>,
    mut evict: impl FnMut(Option<ArenaId>, u64) -> u64,
) {
    let _gone = Gone(reliever, &slabs);
    loop {
        reliever.wanted.wait();
        if !reliever.running.load(Ordering::Acquire) {
            break;
        }
        let Some(slabs) = slabs.upgrade() else { break };
        let node = slabs.node();
        let asked: Vec<(Arc<Arena>, Starved, u16)> = (node.all().into_iter())
            .filter_map(|arena| {
                let (starved, pages) = arena.state.lock().unwrap().relief.asked.take()?;
                Some((arena, starved, pages))
            })
            .collect();
        for (arena, starved, pages) in asked {
            // An ask made stale, by a pass that relieved the arena since, as its allocations asked
            // again while it ran (TASK-79.76), or by a cap changed, has nothing to relieve: a pass
            // would evict as much again for no allocation. Nor has one made while what a pass
            // evicted for it is quarantined still (TASK-161). Those waiting were woken as the arena
            // changed, or are as what it is owed comes back, and ask again if they must.
            if !wants_relief(&arena, starved, pages) {
                continue;
            }
            let from = match starved {
                Starved::Arena => Some(arena.id),
                Starved::Total => None,
                Starved::Removed => unreachable!("relief asked for {:?}, removed", arena.id),
            };
            RELIEVING.set(Some(arena.id));
            let freed = evict(from, RELIEF_BYTES);
            RELIEVING.set(None);
            relieved(&arena, freed, pages);
            node.pulse();
        }
    }
}

/// Whether relief's pass for `arena`, asked for as it was starved, held to `starved`, for a run of
/// `pages`, evicts for it: it is starved still, held to the same cap, and owed no run a pass before
/// evicted for it, still to come back (TASK-161).
fn wants_relief(arena: &Arena, starved: Starved, pages: u16) -> bool {
    arena.starved(pages) == Some(starved) && arena.state.lock().unwrap().relief.owed == 0
}

/// Relief's pass for `arena`, for a run of `pages`, freed `freed` bytes: something, and it is not
/// exhausted; nothing, and it is, if it is starved still, but while runs that would make it room
/// are quarantined, as they make it room once they come back (TASK-106): its own, starved at its
/// own cap; any arena's, at the total's, which relief evicts any arena's for (TASK-132). cladefs:
/// but those held for wedged VM processes, which come back only once they exit, if ever: they kept
/// a starved arena's allocations waiting for good, where they would fail (board 8535).
fn relieved(arena: &Arena, freed: u64, pages: u16) {
    let starved = arena.starved(pages);
    let coming_back = match starved {
        // Any arena's, a removed one's too.
        Some(Starved::Total) => {
            let node = &arena.node;
            node.quarantined.load(Ordering::Relaxed) > node.wedged.load(Ordering::Relaxed)
        }
        _ => {
            let state = arena.state.lock().unwrap();
            state.quarantined > state.wedged
        }
    };
    let mut state = arena.state.lock().unwrap();
    if freed > 0 {
        state.relief.exhausted = false;
    } else if starved.is_some() && !coming_back {
        state.relief.exhausted = true;
    }
}

/// Marks relief gone as its thread ends, as its evictor panicked: the allocations it would make
/// room for fail, those waiting for it now too.
struct Gone<'a>(&'a Reliever, &'a Weak<HostSlabs>);

impl Drop for Gone<'_> {
    fn drop(&mut self) {
        self.0.running.store(false, Ordering::Release);
        if let Some(slabs) = self.1.upgrade() {
            slabs.pulse_if_init();
        }
    }
}

/// What an allocation that found its arena starved does ([`starved`]).
pub(super) enum Asked {
    /// Waits for relief, and tries again.
    Wait,
    /// Fails: nothing makes it room.
    Fail,
}

/// What an allocation of `arena`'s, of `pages` pages, that found it starved, held to `starved`,
/// does: with relief, it asks for it, and one that may `wait` waits for it, unless the arena is
/// exhausted, or relief gone, when it fails. Without relief it fails.
pub(super) fn starved(arena: &Arena, starved: Starved, pages: u16, wait: Wait) -> Asked {
    let Some(reliever) = arena.node.reliever() else { return Asked::Fail };
    if !reliever.running.load(Ordering::Acquire) {
        return Asked::Fail;
    }
    let exhausted = {
        let mut state = arena.state.lock().unwrap();
        state.relief.asked = Some((starved, pages));
        state.relief.exhausted
    };
    reliever.wanted.signal();
    match exhausted || wait == Wait::No {
        true => Asked::Fail,
        false => Asked::Wait,
    }
}

#[cfg(all(test, not(feature = "shuttle")))]
mod tests {
    use super::*;
    use crate::slabs::{
        ArenaId, Quarantined, VmProcessId, alloc, create_arena, node, page_size, quarantine_with,
        remove_arena, set_total_cap,
    };

    /// TASK-132's rule, for an arena removed: an arena starved at the total cap is not exhausted
    /// by a pass that frees nothing while runs of a removed arena's, freed and quarantined, are to
    /// come back, as they make it room once they do. The arenas listed alone were looked at, which a
    /// removed one is not: a tenant's sandbox ending on a full host failed the next one's page-ins
    /// with ENOMEM (the verifying review of board 6204).
    #[test]
    fn removed_arenas_quarantined_runs_exhaust_no_arena() {
        static PARKED: std::sync::Mutex<Vec<Quarantined>> = std::sync::Mutex::new(Vec::new());
        quarantine_with(|freed| PARKED.lock().unwrap().push(freed));
        let whole = (SLAB / page_size()) as u16;
        set_total_cap(Some(2 * SLAB));
        let removed = create_arena().unwrap();
        for _ in 0..2 {
            let slab = alloc(removed, u64::from(whole), Wait::No).unwrap();
            slab.stage_to(VmProcessId(1));
        }
        remove_arena(removed);
        let default = node().arena(ArenaId::DEFAULT).unwrap();
        assert_eq!(default.starved(whole), Some(Starved::Total));
        relieved(&default, 0, whole);
        assert!(!default.state.lock().unwrap().relief.exhausted, "exhausted, its room on its way");
        PARKED.lock().unwrap().clear();
        assert_eq!(default.starved(whole), None, "room once the runs came back");
    }

    /// Board 8535: runs held for a wedged VM process, one killed that has not exited, are no room
    /// on their way: an arena starved at the total cap is exhausted by a pass that frees nothing
    /// while only they are quarantined, its allocations failing rather than waiting for good; and
    /// not while any other is to come back. Red with every run quarantined counted on its way.
    #[test]
    fn runs_held_for_wedged_vm_processes_exhaust_starved_arenas() {
        static PARKED: std::sync::Mutex<Vec<Quarantined>> = std::sync::Mutex::new(Vec::new());
        quarantine_with(|freed| PARKED.lock().unwrap().push(freed));
        let whole = (SLAB / page_size()) as u16;
        set_total_cap(Some(2 * SLAB));
        let arena = create_arena().unwrap();
        let default = node().arena(ArenaId::DEFAULT).unwrap();
        for _ in 0..2 {
            let slab = alloc(arena, u64::from(whole), Wait::No).unwrap();
            slab.stage_to(VmProcessId(1));
            // Freed by relief's pass for the default arena, which is owed it.
            RELIEVING.set(Some(ArenaId::DEFAULT));
            drop(slab);
            RELIEVING.set(None);
        }
        assert_eq!(default.state.lock().unwrap().relief.owed, 2 * u64::from(whole));
        assert_eq!(default.starved(whole), Some(Starved::Total));
        // One of the two wedged: the other is on its way still.
        PARKED.lock().unwrap()[0].wedged();
        relieved(&default, 0, whole);
        assert!(!default.state.lock().unwrap().relief.exhausted, "exhausted, a run on its way");
        // Both, told twice: none is, nor owed to relief, whose passes waited for them.
        PARKED.lock().unwrap().iter().for_each(Quarantined::wedged);
        PARKED.lock().unwrap()[0].wedged();
        assert_eq!(crate::slabs::wedged_bytes(), 2 * SLAB);
        assert_eq!(default.state.lock().unwrap().relief.owed, 0, "owed what never comes");
        assert!(wants_relief(&default, Starved::Total, whole), "no pass for what never comes");
        relieved(&default, 0, whole);
        assert!(default.state.lock().unwrap().relief.exhausted, "waits for runs that never come");
        // Its arena gone, the runs, as they come back, are punched, not kept spare.
        remove_arena(arena);
        PARKED.lock().unwrap().clear();
        assert_eq!((crate::slabs::wedged_bytes(), crate::slabs::quarantined_bytes()), (0, 0));
        assert_eq!(default.starved(whole), None, "room once the runs came back");
    }
}
