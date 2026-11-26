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
use super::{Arena, ArenaId, HostSlabs, Node, SLAB, Starved, Wait};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// What a pass frees for an arena at least: four slabs, so that the next allocations find room.
/// What it asks of [`relieve_with`]'s evictor.
pub const RELIEF_BYTES: u64 = 4 * SLAB;

/// Maximum cap-scaled hysteresis headroom added on top of a starved cap's immediate overshoot
/// (board 12315): 1/64 of the cap, clamped to `RELIEF_BYTES..=MAX_RELIEF_HEADROOM`, so large hosts
/// wake relief less often without evicting hot working sets.
const MAX_RELIEF_HEADROOM: u64 = 16 * SLAB;

/// Maximum bytes a single relief pass asks the evictor to free, keeping quarantine batches and VM
/// unmap bursts bounded even after a large cap drop (board 12315).
const MAX_RELIEF_BATCH: u64 = 64 * SLAB;

/// Initial and maximum exponential backoff after a relief pass frees 0 bytes (board 12315),
/// preventing `arena-relief-thread` from spinning under sustained memory pressure when nothing is
/// evictable.
const EMPTY_BACKOFF_FIRST: Duration = Duration::from_millis(1);
const EMPTY_BACKOFF_MAX: Duration = Duration::from_millis(50);

/// Exponential backoff state for 0-byte relief passes (board 12315).
#[derive(Default)]
pub(super) struct Backoff {
    until: Option<Instant>,
    delay: Duration,
}

impl Backoff {
    pub(super) fn clear(&mut self) {
        self.until = None;
        self.delay = Duration::ZERO;
    }

    pub(super) fn record_empty(&mut self, now: Instant) {
        let next = if self.delay.is_zero() {
            EMPTY_BACKOFF_FIRST
        } else {
            self.delay.saturating_mul(2).min(EMPTY_BACKOFF_MAX)
        };
        self.delay = next;
        self.until = Some(now + next);
    }

    pub(super) fn active(&self, now: Instant) -> bool {
        self.until.is_some_and(|until| now < until)
    }
}

/// Which cap relief's current eviction pass is relieving (TASK-161, board 12315).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Relieving {
    Arena(ArenaId),
    Total,
}

/// Periodic governor callback run on the unified eviction thread (board 11703).
pub(super) struct GovernorTick {
    interval: Duration,
    next: Instant,
    tick: Box<dyn FnMut(&mut dyn FnMut()) + Send>,
}

/// The relief/eviction thread's: what wakes it, whether it runs, and its optional periodic
/// governor tick (board 11703).
pub(super) struct Reliever {
    /// Signaled as an allocation asks for relief, a governor attaches, or counted quarantine clears.
    wanted: Signal,
    /// Whether the thread runs: once it has gone, as its evictor panicked, the allocations it
    /// would make room for fail.
    running: AtomicBool,
    /// Optional periodic memory-governor tick driven on the same thread as slab relief.
    governor: Mutex<Option<GovernorTick>>,
}

impl Reliever {
    pub(super) fn stop(&self) {
        self.running.store(false, Ordering::Release);
        self.wanted.signal();
    }

    #[cfg(not(madsim))]
    pub(super) fn signal(&self) {
        self.wanted.signal();
    }

    #[cfg(not(madsim))]
    pub(super) fn wait_timeout(&self, duration: Duration) {
        self.wanted.wait_timeout(duration);
    }
}

/// Guard restoring `node.set_relieving(None)` on scope exit or panic (board 11703).
struct RelievingGuard<'a>(&'a Node);

impl<'a> RelievingGuard<'a> {
    fn enter(node: &'a Node, target: Relieving) -> Self {
        node.set_relieving(Some(target));
        Self(node)
    }
}

impl Drop for RelievingGuard<'_> {
    fn drop(&mut self) {
        self.0.set_relieving(None);
    }
}

/// Attaches a periodic governor tick to `slabs`'s unified eviction thread (board 11703).
#[cfg(not(madsim))]
pub(super) fn attach_governor_on(
    slabs: &HostSlabs,
    interval: Duration,
    tick: impl FnMut(&mut dyn FnMut()) + Send + 'static,
) -> bool {
    if cfg!(feature = "shuttle") {
        return false;
    }
    let gov = GovernorTick { interval, next: Instant::now(), tick: Box::new(tick) };
    if let Some(slot) = slabs.reliever.get() {
        let Some(reliever) = slot.as_ref() else {
            return false;
        };
        if !reliever.running.load(Ordering::Acquire) {
            return false;
        }
        *reliever.governor.lock().unwrap() = Some(gov);
        reliever.wanted.signal();
        return true;
    }
    *slabs.pending_governor.lock().unwrap() = Some(gov);
    // Re-check in case `relieve_with_on` initialized `slabs.reliever` concurrently.
    if let Some(slot) = slabs.reliever.get() {
        let Some(reliever) = slot.as_ref() else {
            slabs.pending_governor.lock().unwrap().take();
            return false;
        };
        if let Some(gov) = slabs.pending_governor.lock().unwrap().take() {
            *reliever.governor.lock().unwrap() = Some(gov);
            reliever.wanted.signal();
        }
        return reliever.running.load(Ordering::Acquire);
    }
    true
}

/// Starts the relief of starved arenas on `slabs`, on a thread of its own, which frees runs with
/// `evict` and drives any attached periodic memory governor tick (board 11703). Starts once per
/// [`HostSlabs`]; under Shuttle or madsim no thread starts.
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
        let pending = slabs.pending_governor.lock().unwrap().take();
        let reliever = Arc::new(Reliever {
            wanted: Signal::new(),
            running: AtomicBool::new(true),
            governor: Mutex::new(pending),
        });
        let thread = reliever.clone();
        let spawned =
            super::sync::try_spawn("cladefs-eviction", move || relieve(&thread, weak, evict));
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

/// Computes how many bytes a relief pass asks its evictor to free given the active cap and current
/// overshoot bytes: covers the overshoot plus cap-scaled hysteresis headroom, bounded per pass by
/// [`MAX_RELIEF_BATCH`] (board 12315).
fn relief_bytes_for(cap_bytes: u64, overshoot_bytes: u64) -> u64 {
    let headroom = (cap_bytes / 64).clamp(RELIEF_BYTES, MAX_RELIEF_HEADROOM);
    // Steady-state single-slab starvation (`overshoot_bytes <= SLAB`) already fits inside
    // `headroom`, preserving `RELIEF_BYTES` for small caps.
    overshoot_bytes
        .saturating_sub(SLAB)
        .saturating_add(headroom)
        .max(overshoot_bytes)
        .clamp(RELIEF_BYTES, MAX_RELIEF_BATCH)
}

/// How many bytes a relief pass for `arena`, starved at `starved` for `pages` pages, asks its
/// evictor to free (board 12315).
fn relief_bytes(arena: &Arena, starved: Starved, pages: u16) -> u64 {
    let ps = super::page_size();
    let (cap_pages, overshoot_pages) = match starved {
        Starved::Arena => {
            let state = arena.state.lock().unwrap();
            let need = state.need(pages);
            let cap = state.cap.unwrap_or(0);
            let overshoot = (state.populated + need).saturating_sub(cap);
            (cap, overshoot)
        }
        Starved::Total => {
            let need = arena.state.lock().unwrap().need(pages);
            let cap = arena.node.limit();
            let populated = arena.node.populated.load(Ordering::Relaxed);
            let overshoot = (populated + need).saturating_sub(cap);
            (cap, overshoot)
        }
        Starved::Removed => return 0,
    };
    relief_bytes_for(cap_pages.saturating_mul(ps), overshoot_pages.saturating_mul(ps))
}

/// Runs one reactive relief pass for any starved arenas on `node`.
fn relieve_pass(node: &Arc<Node>, evict: &mut impl FnMut(Option<ArenaId>, u64) -> u64) {
    let mut arena_asks = Vec::new();
    let mut total_asks = Vec::new();
    for arena in node.all() {
        let Some((starved, pages)) = arena.state.lock().unwrap().relief.asked.take() else {
            continue;
        };
        match starved {
            Starved::Arena => arena_asks.push((arena, pages)),
            Starved::Total => total_asks.push((arena, pages)),
            Starved::Removed => unreachable!("relief asked for {:?}, removed", arena.id),
        }
    }
    for (arena, pages) in arena_asks {
        // An ask made stale, by a pass that relieved the arena since, as its allocations asked
        // again while it ran (TASK-79.76), or by a cap changed, has nothing to relieve: a pass
        // would evict as much again for no allocation. Nor has one made while what a pass
        // evicted for it is quarantined still (TASK-161), or during empty-pass backoff
        // (board 12315).
        if !wants_relief(&arena, Starved::Arena, pages) {
            continue;
        }
        let target = relief_bytes(&arena, Starved::Arena, pages);
        let freed = {
            let _relieving = RelievingGuard::enter(node, Relieving::Arena(arena.id));
            evict(Some(arena.id), target)
        };
        if relieved_arena(&arena, freed, pages) {
            node.pulse();
        }
    }
    if !total_asks.is_empty() {
        relieve_total(node, &total_asks, evict);
    }
}

/// The unified eviction thread: frees runs with `evict` for each starved arena and drives the
/// periodic memory governor tick when attached (TASK-50.3, board 12315, board 11703).
fn relieve(
    reliever: &Reliever,
    slabs: Weak<HostSlabs>,
    mut evict: impl FnMut(Option<ArenaId>, u64) -> u64,
) {
    let _gone = Gone(reliever, &slabs);
    loop {
        let wait_for = reliever
            .governor
            .lock()
            .unwrap()
            .as_ref()
            .map(|gov| gov.next.saturating_duration_since(Instant::now()));
        match wait_for {
            Some(dur) if dur.is_zero() => {}
            Some(dur) => reliever.wanted.wait_timeout(dur),
            None => reliever.wanted.wait(),
        }
        if !reliever.running.load(Ordering::Acquire) {
            break;
        }
        let Some(slabs) = slabs.upgrade() else { break };
        let node = slabs.node();
        // Once a memory governor is attached, supervise every relief and governor pass under
        // `Holding::work("memory governor")` so any panic on the unified eviction thread aborts
        // the daemon naming `"memory governor"` rather than silently halting both relief and
        // governance (hotbed review 140 finding 521).
        let supervised = reliever.governor.lock().unwrap().is_some();
        let _supervised = supervised.then(|| local_cell::shared::Holding::work("memory governor"));
        relieve_pass(node, &mut evict);
        let due = {
            let mut slot = reliever.governor.lock().unwrap();
            if slot.as_ref().is_some_and(|gov| Instant::now() >= gov.next) {
                slot.take()
            } else {
                None
            }
        };
        if let Some(mut gov) = due {
            let mut service_relief = || relieve_pass(node, &mut evict);
            (gov.tick)(&mut service_relief);
            // Re-check starved arenas immediately after the governor tick returns so an ask that
            // arrived near tick end (or whose `wanted` signal was consumed by `wait_eviction`) is
            // relieved without waiting up to `gov.interval` (hotbed review 139 finding 1).
            relieve_pass(node, &mut evict);
            gov.next = Instant::now() + gov.interval;
            let mut slot = reliever.governor.lock().unwrap();
            if slot.is_none() {
                *slot = Some(gov);
            }
        }
    }
}

/// Relieves all arenas starved at the node-wide total cap in a single pass so `K` starved arenas
/// do not trigger `K` redundant evictions or spin while quarantined runs return (board 12315).
fn relieve_total(
    node: &Arc<Node>,
    asks: &[(Arc<Arena>, u16)],
    evict: &mut impl FnMut(Option<ArenaId>, u64) -> u64,
) {
    let still_starved: Vec<&(Arc<Arena>, u16)> = asks
        .iter()
        .filter(|(arena, pages)| arena.starved(*pages) == Some(Starved::Total))
        .collect();
    let Some((rep, max_pages)) = still_starved.iter().copied().max_by_key(|(_, pages)| *pages)
    else {
        return;
    };
    let now = Instant::now();
    if node.total_owed.load(Ordering::Acquire) > 0 || node.total_backoff_active_at(now) {
        return;
    }
    let target = relief_bytes(rep, Starved::Total, *max_pages);
    let freed = {
        let _relieving = RelievingGuard::enter(node, Relieving::Total);
        evict(None, target)
    };
    if relieved_total(node, asks, freed) {
        node.pulse();
    }
}

/// Whether relief's pass for `arena`, asked for as it was starved, held to `starved`, for a run of
/// `pages`, evicts for it: it is starved still, held to the same cap, owed no run a pass before
/// evicted for it still to come back (TASK-161), and not in empty-pass backoff (board 12315).
fn wants_relief(arena: &Arena, starved: Starved, pages: u16) -> bool {
    if arena.starved(pages) != Some(starved) {
        return false;
    }
    let now = Instant::now();
    match starved {
        Starved::Arena => {
            let state = arena.state.lock().unwrap();
            state.relief.owed == 0 && !state.relief.backoff.active(now)
        }
        Starved::Total => {
            arena.node.total_owed.load(Ordering::Acquire) == 0
                && !arena.node.total_backoff_active_at(now)
        }
        Starved::Removed => false,
    }
}

/// Records the result of a `Starved::Arena` relief pass and returns whether callers waiting on
/// `node.room` should be pulsed (board 12315): pulsing when `freed == 0` while quarantined runs
/// are coming back wakes waiters immediately and spins `arena-relief-thread`.
fn relieved_arena(arena: &Arena, freed: u64, pages: u16) -> bool {
    let starved = arena.starved(pages);
    let mut state = arena.state.lock().unwrap();
    if freed > 0 {
        state.relief.exhausted = false;
        state.relief.backoff.clear();
        return state.relief.owed == 0 || starved.is_none();
    }
    if starved.is_none() {
        state.relief.backoff.clear();
        return true;
    }
    let coming_back = state.quarantined > state.wedged;
    state.relief.backoff.record_empty(Instant::now());
    if !coming_back {
        state.relief.exhausted = true;
        return true;
    }
    false
}

/// Records the result of a `Starved::Total` relief pass across all `asks` and returns whether
/// callers waiting on `node.room` should be pulsed (board 12315).
fn relieved_total(node: &Node, asks: &[(Arc<Arena>, u16)], freed: u64) -> bool {
    if freed > 0 {
        node.total_exhausted.store(false, Ordering::Release);
        node.clear_total_backoff();
        for (arena, _) in asks {
            let mut state = arena.state.lock().unwrap();
            state.relief.exhausted = false;
            state.relief.backoff.clear();
        }
        return node.total_owed.load(Ordering::Acquire) == 0
            || asks.iter().any(|(arena, pages)| arena.starved(*pages).is_none());
    }
    let any_starved = asks.iter().any(|(arena, pages)| arena.starved(*pages).is_some());
    if !any_starved {
        node.clear_total_backoff();
        return true;
    }
    let coming_back =
        || node.quarantined.load(Ordering::Acquire) > node.wedged.load(Ordering::Acquire);
    let now = Instant::now();
    if !coming_back() {
        node.record_total_empty_backoff(now);
        node.total_exhausted.store(true, Ordering::Release);
        for (arena, _) in asks {
            arena.state.lock().unwrap().relief.exhausted = true;
        }
        return true;
    }
    node.record_total_empty_backoff(now);
    // Re-check after recording backoff in case the last quarantined slab returned concurrently.
    if !coming_back() {
        node.clear_total_backoff();
        node.total_exhausted.store(true, Ordering::Release);
        for (arena, _) in asks {
            arena.state.lock().unwrap().relief.exhausted = true;
        }
        return true;
    }
    false
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
    let now = Instant::now();
    let (exhausted, should_signal) = match starved {
        Starved::Arena => {
            let mut state = arena.state.lock().unwrap();
            let should_signal = state.relief.owed == 0 && !state.relief.backoff.active(now);
            if should_signal {
                state.relief.asked = Some((starved, pages));
            }
            (state.relief.exhausted, should_signal)
        }
        Starved::Total => {
            let should_signal = arena.node.total_owed.load(Ordering::Acquire) == 0
                && !arena.node.total_backoff_active_at(now);
            let mut state = arena.state.lock().unwrap();
            if should_signal {
                state.relief.asked = Some((starved, pages));
            }
            let exhausted =
                state.relief.exhausted || arena.node.total_exhausted.load(Ordering::Acquire);
            (exhausted, should_signal)
        }
        Starved::Removed => return Asked::Fail,
    };
    if should_signal {
        reliever.wanted.signal();
    }
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
        assert!(!relieved_total(node(), &[(default.clone(), whole)], 0));
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
            // Freed by relief's pass for the total cap, which is owed it.
            let _relieving = RelievingGuard::enter(node(), Relieving::Total);
            drop(slab);
        }
        assert_eq!(node().total_owed.load(Ordering::Acquire), 2 * u64::from(whole));
        assert_eq!(default.starved(whole), Some(Starved::Total));
        // One of the two wedged: the other is on its way still.
        PARKED.lock().unwrap()[0].wedged();
        assert!(!relieved_total(node(), &[(default.clone(), whole)], 0));
        assert!(!default.state.lock().unwrap().relief.exhausted, "exhausted, a run on its way");
        // Both, told twice: none is, nor owed to relief, whose passes waited for them.
        PARKED.lock().unwrap().iter().for_each(Quarantined::wedged);
        PARKED.lock().unwrap()[0].wedged();
        assert_eq!(crate::slabs::wedged_bytes(), 2 * SLAB);
        assert_eq!(node().total_owed.load(Ordering::Acquire), 0, "owed what never comes");
        assert!(wants_relief(&default, Starved::Total, whole), "no pass for what never comes");
        assert!(relieved_total(node(), &[(default.clone(), whole)], 0));
        assert!(default.state.lock().unwrap().relief.exhausted, "waits for runs that never come");
        // Its arena gone, the runs, as they come back, are punched, not kept spare.
        remove_arena(arena);
        PARKED.lock().unwrap().clear();
        assert_eq!((crate::slabs::wedged_bytes(), crate::slabs::quarantined_bytes()), (0, 0));
        assert_eq!(default.starved(whole), None, "room once the runs came back");
    }

    /// Board 12315: a 0-byte relief pass while quarantined runs are coming back must NOT pulse
    /// waiters and spin `arena-relief-thread`, and waiters must still wake promptly once the
    /// quarantined runs return.
    #[test]
    fn zero_byte_relief_with_quarantined_runs_does_not_spin() {
        use std::sync::atomic::AtomicUsize;

        static PARKED: std::sync::Mutex<Vec<Quarantined>> = std::sync::Mutex::new(Vec::new());
        quarantine_with(|freed| PARKED.lock().unwrap().push(freed));
        let whole = (SLAB / page_size()) as u16;
        set_total_cap(Some(SLAB));

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_evict = calls.clone();
        relieve_with(move |_, _| {
            calls_evict.fetch_add(1, Ordering::Relaxed);
            0
        })
        .unwrap();

        let slab = alloc(ArenaId::DEFAULT, u64::from(whole), Wait::No).unwrap();
        slab.stage_to(VmProcessId(1));
        drop(slab);
        assert_eq!(PARKED.lock().unwrap().len(), 1);

        let waiter =
            std::thread::spawn(move || alloc(ArenaId::DEFAULT, u64::from(whole), Wait::Yes));
        std::thread::sleep(Duration::from_millis(40));
        let passes = calls.load(Ordering::Relaxed);
        assert!(
            passes <= 2,
            "arena-relief-thread spun while waiting on quarantine: {passes} passes"
        );
        assert!(!waiter.is_finished(), "waiter failed despite quarantined run coming back");

        PARKED.lock().unwrap().clear();
        let acquired =
            waiter.join().unwrap().expect("waiter should succeed once quarantine clears");
        drop(acquired);
    }

    /// Board 12315: multiple arenas starved at `Starved::Total` share `total_owed` and trigger
    /// a single eviction pass rather than one per arena.
    #[test]
    fn multiple_arenas_starved_at_total_cap_deduplicate_relief() {
        use std::sync::atomic::AtomicUsize;

        static PARKED: std::sync::Mutex<Vec<Quarantined>> = std::sync::Mutex::new(Vec::new());
        quarantine_with(|freed| PARKED.lock().unwrap().push(freed));
        let whole = (SLAB / page_size()) as u16;
        set_total_cap(Some(2 * SLAB));

        let holder = create_arena().unwrap();
        let slabs_held: Arc<std::sync::Mutex<Vec<crate::slabs::PartSlab>>> =
            Arc::new(std::sync::Mutex::new(
                (0..2)
                    .map(|_| {
                        let s = alloc(holder, u64::from(whole), Wait::No).unwrap();
                        s.stage_to(VmProcessId(1));
                        s
                    })
                    .collect(),
            ));

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_evict = calls.clone();
        let evictable = slabs_held.clone();
        relieve_with(move |_, _| {
            calls_evict.fetch_add(1, Ordering::Relaxed);
            evictable.lock().unwrap().pop().map_or(0, |s| s.len())
        })
        .unwrap();

        let arenas: Vec<_> = (0..4).map(|_| create_arena().unwrap()).collect();
        let waiters: Vec<_> = arenas
            .iter()
            .map(|&a| std::thread::spawn(move || alloc(a, u64::from(whole), Wait::Yes)))
            .collect();

        let deadline = Instant::now() + Duration::from_secs(5);
        while calls.load(Ordering::Relaxed) == 0 {
            assert!(Instant::now() < deadline, "relief never ran");
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "multiple arenas starved at Starved::Total triggered redundant relief passes"
        );
        assert_eq!(slabs_held.lock().unwrap().len(), 1, "second slab evicted while first was owed");

        // Release remaining slab + quarantine so all waiters finish cleanly.
        slabs_held.lock().unwrap().clear();
        PARKED.lock().unwrap().clear();
        set_total_cap(None);
        for waiter in waiters {
            drop(waiter.join().unwrap().unwrap());
        }
    }

    /// Board 12315: repeated retries against an exhausted cap back off exponentially instead of
    /// spinning `arena-relief-thread`, and recover immediately when a slab is freed.
    #[test]
    fn exhausted_relief_backs_off_and_recovers_on_free() {
        use std::sync::atomic::AtomicUsize;

        let whole = (SLAB / page_size()) as u16;
        set_total_cap(Some(SLAB));
        let held = alloc(ArenaId::DEFAULT, u64::from(whole), Wait::No).unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_evict = calls.clone();
        relieve_with(move |_, _| {
            calls_evict.fetch_add(1, Ordering::Relaxed);
            0
        })
        .unwrap();

        let end = Instant::now() + Duration::from_millis(30);
        let mut attempts = 0usize;
        while Instant::now() < end {
            assert_eq!(
                alloc(ArenaId::DEFAULT, u64::from(whole), Wait::Yes).err(),
                Some(crate::slabs::NoMemory)
            );
            attempts += 1;
            std::thread::yield_now();
        }
        let passes = calls.load(Ordering::Relaxed);
        assert!(attempts > 10, "expected many retry attempts, got {attempts}");
        assert!(
            passes <= 10,
            "exhausted relief spun instead of backing off: {passes} passes for {attempts} attempts"
        );

        // Freeing the held slab clears backoff and exhaustion immediately.
        drop(held);
        let acquired = alloc(ArenaId::DEFAULT, u64::from(whole), Wait::Yes)
            .expect("allocation should succeed immediately once a slab is freed");
        drop(acquired);
    }

    /// Board 12315: relief batch size scales with overshoot and cap-proportional hysteresis
    /// headroom, preserving `RELIEF_BYTES` for small caps and bounding bursts at `MAX_RELIEF_BATCH`.
    #[test]
    fn relief_bytes_scales_with_overshoot_and_headroom() {
        assert_eq!(relief_bytes_for(4 * SLAB, SLAB), RELIEF_BYTES);
        assert_eq!(relief_bytes_for(256 * SLAB, SLAB), RELIEF_BYTES);
        assert_eq!(relief_bytes_for(512 * SLAB, SLAB), 8 * SLAB);
        assert_eq!(relief_bytes_for(1024 * SLAB, SLAB), MAX_RELIEF_HEADROOM);
        assert_eq!(relief_bytes_for(4096 * SLAB, SLAB), MAX_RELIEF_HEADROOM);
        assert_eq!(relief_bytes_for(1024 * SLAB, 30 * SLAB), 29 * SLAB + MAX_RELIEF_HEADROOM);
        assert_eq!(relief_bytes_for(4096 * SLAB, 200 * SLAB), MAX_RELIEF_BATCH);
    }

    /// Hotbed review 139 finding 1: a slow governor tick that calls `service_relief()` between
    /// steps unblocks a starved `alloc(Wait::Yes)` promptly instead of blocking it for the full
    /// tick duration, and a relief ask whose `wanted` signal was consumed by `wait_eviction` at
    /// tick end is still serviced immediately without sleeping for the governor interval.
    #[cfg(not(madsim))]
    #[test]
    fn governor_tick_services_starved_relief_and_post_tick_asks_promptly() {
        use std::sync::atomic::AtomicBool;

        let whole = (SLAB / page_size()) as u16;
        let slabs = HostSlabs::new();
        slabs.set_total_cap(Some(SLAB));
        let held = Arc::new(Mutex::new(vec![
            slabs.alloc(ArenaId::DEFAULT, u64::from(whole), Wait::No).unwrap(),
        ]));
        let held_evict = Arc::clone(&held);
        slabs
            .relieve_with(move |_, _| held_evict.lock().unwrap().pop().map_or(0, |slab| slab.len()))
            .unwrap();

        let in_tick = Arc::new(AtomicBool::new(false));
        let in_tick_cb = Arc::clone(&in_tick);
        let ran = Arc::new(AtomicBool::new(false));
        let ran_cb = Arc::clone(&ran);
        let slabs_cb = Arc::clone(&slabs);
        assert!(slabs.attach_governor(Duration::from_secs(5), move |service_relief| {
            if ran_cb.swap(true, Ordering::AcqRel) {
                return;
            }
            in_tick_cb.store(true, Ordering::Release);
            // Simulate a multi-step governor pass (400 ms total) calling `service_relief` between
            // steps, then consuming `wanted` once via `wait_eviction` right before returning.
            for _ in 0..8 {
                std::thread::sleep(Duration::from_millis(50));
                service_relief();
            }
            slabs_cb.wait_eviction(Duration::from_millis(20));
            in_tick_cb.store(false, Ordering::Release);
        }));

        let wait_start = Instant::now();
        while !in_tick.load(Ordering::Acquire) {
            assert!(wait_start.elapsed() < Duration::from_secs(2), "governor tick never started");
            std::thread::sleep(Duration::from_millis(1));
        }
        // Starved alloc arriving mid-tick must be serviced by `service_relief()` well before the
        // 400 ms tick finishes.
        let started = Instant::now();
        let slab1 = slabs.alloc(ArenaId::DEFAULT, u64::from(whole), Wait::Yes).unwrap();
        let mid_tick_elapsed = started.elapsed();
        assert!(
            mid_tick_elapsed < Duration::from_millis(250),
            "mid-tick relief waited too long: {mid_tick_elapsed:?}"
        );

        // Hold `slab1` back and starve again right as the tick calls `wait_eviction` and returns:
        // post-tick `relieve_pass` must service it immediately instead of sleeping 5 s.
        held.lock().unwrap().push(slab1);
        let started = Instant::now();
        let slab2 = slabs.alloc(ArenaId::DEFAULT, u64::from(whole), Wait::Yes).unwrap();
        let post_tick_elapsed = started.elapsed();
        assert!(
            post_tick_elapsed < Duration::from_secs(1),
            "post-tick relief stalled for governor interval: {post_tick_elapsed:?}"
        );
        drop(slab2);
        held.lock().unwrap().clear();
    }
}
