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
use super::{Arena, ArenaId, SLAB, Starved, Wait, node};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

/// What a pass frees for an arena at least: four slabs, so that the next allocations find room.
/// What it asks of [`relieve_with`]'s evictor.
pub const RELIEF_BYTES: u64 = 4 * SLAB;

/// The relief thread's: what wakes it, and whether it runs.
struct Reliever {
    /// Signaled as an allocation asks for relief.
    wanted: Signal,
    /// Whether the thread runs: once it has gone, as its evictor panicked, the allocations it
    /// would make room for fail.
    running: AtomicBool,
}

/// The relief thread's, if [`relieve_with`] started it.
// The process's, started once, by whichever simulated host is first (TASK-685.14); under madsim it starts no thread.
static RELIEVER: OnceLock<Option<Arc<Reliever>>> = OnceLock::new();

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

fn reliever() -> Option<&'static Reliever> {
    RELIEVER.get()?.as_deref()
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
pub fn relieve_with(
    evict: impl FnMut(Option<ArenaId>, u64) -> u64 + Send + 'static,
) -> std::io::Result<()> {
    let mut started = Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "the arenas' relief has started",
    ));
    RELIEVER.get_or_init(|| {
        let reliever = Arc::new(Reliever { wanted: Signal::new(), running: AtomicBool::new(true) });
        let thread = reliever.clone();
        let spawned =
            super::sync::try_spawn("arena-relief-thread", move || relieve(&thread, evict));
        let installed = spawned.as_ref().is_ok_and(|spawned| *spawned).then_some(reliever);
        started = spawned.map(drop);
        installed
    });
    started
}

/// The relief thread: frees runs with `evict` for each arena asked for since its last pass, starved
/// still at the cap it was asked at, and owed nothing a pass before quarantined, the arena's own at
/// its own cap, any arena's at the total's, and records what it freed, which the arena's
/// allocations waiting then try again with.
fn relieve(reliever: &Reliever, mut evict: impl FnMut(Option<ArenaId>, u64) -> u64) {
    let _gone = Gone(reliever);
    loop {
        reliever.wanted.wait();
        let node = node();
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
/// own cap; any arena's, at the total's, which relief evicts any arena's for (TASK-132).
fn relieved(arena: &Arena, freed: u64, pages: u16) {
    let starved = arena.starved(pages);
    let coming_back = match starved {
        // Any arena's, a removed one's too.
        Some(Starved::Total) => arena.node.quarantined.load(Ordering::Relaxed) > 0,
        _ => arena.state.lock().unwrap().quarantined > 0,
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
struct Gone<'a>(&'a Reliever);

impl Drop for Gone<'_> {
    fn drop(&mut self) {
        self.0.running.store(false, Ordering::Release);
        node().pulse();
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
    let Some(reliever) = reliever() else { return Asked::Fail };
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
        ArenaId, Quarantined, VmProcessId, alloc, create_arena, page_size, quarantine_with,
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
        let removed = create_arena();
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
}
