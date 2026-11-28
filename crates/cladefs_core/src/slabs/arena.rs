// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! An arena's slots, under its lock (doc-26, The allocator): which of its memory object's slabs
//! are free, spare, a whole part's, a small slab's runs, or being punched; and what holds it to
//! them, its cap and its relief. What is decided here, the memory object is told outside the lock
//! (`slabs::Arena`): a run punched as it is freed is no one's until its punch is done
//! ([`ArenaState::punched`]), so that no allocation takes memory the punch would then take away.

use std::collections::BTreeSet;

/// How many spare slabs an arena keeps: whole slabs freed, populated still, that the next whole
/// allocations take before any slab not populated, so that their memory isn't punched out and
/// allocated again under churn (doc-26: two, as the one 2 MiB spare chunk was).
pub(super) const SPARE_SLABS: usize = 2;

/// A run of an arena's pages: of slab `slab`, `pages` pages from its page `first`. A whole slab's
/// is `first` 0, all of its pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Run {
    pub slab: u32,
    pub first: u16,
    pub pages: u16,
}

/// Why a starved arena gives no run ([`ArenaState::take`]): the cap it is held to, or its removal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Starved {
    /// Its own: runs of its own freed make it room.
    Arena,
    /// The arenas' together, or the memory's: runs of any arena's freed make it room.
    Total,
    /// It was removed (TASK-79.36): nothing makes it room, and its allocations fail.
    Removed,
}

/// Why an arena gave no run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Refused {
    /// It is starved, held to the cap it says.
    Starved(Starved),
    /// Runs being punched would make it room within its own cap: its allocation waits for them,
    /// and tries again ([`super::sync::Rules::allocations_wait_for_punches`]).
    Punching,
}

/// A slab slot of an arena's memory object.
enum Slot {
    /// Punched, or never populated: no one's.
    Free,
    /// Populated, no one's: a whole slab freed, for the next whole allocation.
    Spare,
    /// One part's.
    Whole,
    /// Runs of parts shorter than a slab.
    Small(Small),
    /// Being punched, as a whole slab freed or a spare given back: no one's once that is done.
    Punching,
    /// Its punch failed: kept as used, so that its memory is counted still (doc-26, Failure
    /// modes).
    Leaked,
}

/// A small slab's pages: those allocated, or being punched, set.
#[derive(Clone, Copy)]
struct Small {
    used: [u64; 4],
    /// Its largest run of pages unused, which [`ArenaState::smalls`] orders it by.
    largest: u16,
}

impl Small {
    fn is_used(&self, page: u16) -> bool {
        self.used[usize::from(page / 64)] & (1 << (page % 64)) != 0
    }

    fn set(&mut self, run: std::ops::Range<u16>, used: bool) {
        for page in run {
            let (word, bit) = (usize::from(page / 64), 1u64 << (page % 64));
            match used {
                true => self.used[word] |= bit,
                false => self.used[word] &= !bit,
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.used.iter().all(|word| *word == 0)
    }

    /// Its first run of `pages` unused pages, of its `pps`, and its largest run unused.
    fn fit(&self, pages: u16, pps: u16) -> (Option<u16>, u16) {
        let (mut first, mut largest, mut run, mut start) = (None, 0, 0u16, 0u16);
        for page in 0..pps {
            if self.is_used(page) {
                run = 0;
                continue;
            }
            if run == 0 {
                start = page;
            }
            run += 1;
            largest = largest.max(run);
            if run == pages && first.is_none() {
                first = Some(start);
            }
        }
        (first, largest)
    }
}

/// What relief has of an arena (`slabs::relief`).
#[derive(Default)]
pub(super) struct Relief {
    /// The cap it was held to as its allocations last asked for relief, and the pages they asked
    /// for, until relief's next pass begins for it.
    pub asked: Option<(Starved, u16)>,
    /// Whether relief's last pass for it freed nothing while it was starved: its allocations fail
    /// rather than wait for ever, until a run of it is freed, a cap changes, or a pass, which each
    /// of them still asks for, frees something.
    pub exhausted: bool,
    /// The pages relief's passes for it evicted, and quarantined, which come back once every VM
    /// process they were staged to has made the changes taking them away: room on its way, which
    /// its next pass waits for rather than evict more for it (TASK-161).
    pub owed: u64,
    /// Exponential backoff after a 0-byte relief pass for this arena's own cap, preventing
    /// `arena-relief-thread` from spinning when nothing is evictable (board 12315).
    pub backoff: super::relief::Backoff,
}

/// An arena's slots, and what holds it to them, under its lock.
pub(super) struct ArenaState {
    /// Its slabs' pages: 1 MiB of the host's pages.
    pps: u16,
    slots: Vec<Slot>,
    /// The slots free, taken lowest first, so that its object stays compact.
    free: BTreeSet<u32>,
    /// Its spare slabs.
    spares: Vec<u32>,
    /// Its small slabs with room, by their largest run of pages unused.
    smalls: BTreeSet<(u16, u32)>,
    /// The pages of its object that hold memory: its whole slabs and spares', its small slabs'
    /// runs, those being punched until they are, and those leaked. What the governor counts.
    pub populated: u64,
    /// Of those, the pages being punched.
    pub punching: u64,
    /// Of those, the pages leaked, their punch failed.
    pub leaked: u64,
    /// The most pages it may populate, if capped.
    pub cap: Option<u64>,
    pub relief: Relief,
    /// The pages of its runs freed and quarantined, which come back to it once every VM process
    /// they were staged to has made the changes taking them away: relief exhausts it not while any
    /// is to come back, as each makes it room (TASK-106).
    pub quarantined: u64,
    /// cladefs: of those, the pages held for wedged VM processes, which come back only once they
    /// exit, if ever: relief exhausts it in spite of them (board 8535).
    pub wedged: u64,
    /// Whether it was removed: it gives no run from then on.
    pub retired: bool,
}

/// A run taken ([`ArenaState::take`]): the run; the size its object grows to, in slabs, if it
/// grows; and whether it was a spare.
#[derive(Debug)]
pub(super) struct Taken {
    pub run: Run,
    pub grown: Option<u32>,
    pub spare: bool,
}

/// What a run freed comes to ([`ArenaState::free`]).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Freed {
    /// A spare: populated still, nothing to punch.
    Spare,
    /// To be punched, outside the lock, then told ([`ArenaState::punched`]).
    Punch,
}

impl ArenaState {
    /// An arena of slabs of `pps` pages each, holding none.
    pub(super) fn new(pps: u16) -> Self {
        assert!((1..=256).contains(&pps), "a slab of {pps} pages");
        Self {
            pps,
            slots: Vec::new(),
            free: BTreeSet::new(),
            spares: Vec::new(),
            smalls: BTreeSet::new(),
            populated: 0,
            punching: 0,
            leaked: 0,
            cap: None,
            relief: Relief::default(),
            quarantined: 0,
            wedged: 0,
            retired: false,
        }
    }

    /// Whether `need` pages more keep it within its own cap, as it stands.
    fn within_cap(&self, need: u64) -> bool {
        self.cap.is_none_or(|cap| self.populated + need <= cap)
    }

    /// Why `need` pages more are refused by its own cap, if they are: it waits for runs being
    /// punched, if they would make it room ([`Refused::Punching`]).
    fn refused_by_cap(&self, need: u64, wait_for_punches: bool) -> Option<Refused> {
        if self.within_cap(need) {
            return None;
        }
        let after = self.populated - self.punching + need;
        let punches_make_room = self.cap.is_some_and(|cap| after <= cap);
        Some(match wait_for_punches && punches_make_room {
            true => Refused::Punching,
            false => Refused::Starved(Starved::Arena),
        })
    }

    /// Why it would refuse a run of `pages` now, if it would: what [`ArenaState::take`] would
    /// say, the total's aside.
    pub(super) fn refusal(&self, pages: u16, wait_for_punches: bool) -> Option<Refused> {
        if self.retired {
            return Some(Refused::Starved(Starved::Removed));
        }
        self.refused_by_cap(self.need(pages), wait_for_punches)
    }

    /// The pages a run of `pages` would populate: none from a spare; its pages otherwise, of a
    /// slot free or a small slab's unused pages, which are punched.
    pub(super) fn need(&self, pages: u16) -> u64 {
        match pages == self.pps && !self.spares.is_empty() {
            true => 0,
            false => u64::from(pages),
        }
    }

    /// Takes a run of `pages` pages, within its cap and, for the pages it populates, the total's,
    /// which `reserve` reserves and says whether it could: a spare, for a whole slab, else the
    /// lowest slot free; for fewer, the first fit of the small slab with the smallest largest run
    /// that holds it, else a free slot made small.
    pub(super) fn take(
        &mut self,
        pages: u16,
        wait_for_punches: bool,
        reserve: impl FnOnce(u64) -> bool,
    ) -> Result<Taken, Refused> {
        assert!((1..=self.pps).contains(&pages), "a run of {pages} pages of {}", self.pps);
        if let Some(refused) = self.refusal(pages, wait_for_punches) {
            return Err(refused);
        }
        let need = self.need(pages);
        if need > 0 && !reserve(need) {
            return Err(Refused::Starved(Starved::Total));
        }
        self.populated += need;
        Ok(self.take_run(pages, true))
    }

    /// cladefs: places a run of `pages` pages (board 14629): takes it as [`ArenaState::take`]
    /// does, but populating none of it, so that neither its cap nor the total's counts it: its
    /// place in the object, reserved for a part to be paged in there later
    /// ([`ArenaState::populate`]). Never a spare, which holds memory.
    pub(super) fn place(&mut self, pages: u16) -> Result<Taken, Refused> {
        assert!((1..=self.pps).contains(&pages), "a run of {pages} pages of {}", self.pps);
        if self.retired {
            return Err(Refused::Starved(Starved::Removed));
        }
        Ok(self.take_run(pages, false))
    }

    /// cladefs: populates `run`, placed ([`ArenaState::place`]), as a part is to be paged in
    /// there: within its cap and, for its pages, the total's, which `reserve` reserves and says
    /// whether it could, as [`ArenaState::take`] would of a run taken now.
    pub(super) fn populate(
        &mut self,
        run: Run,
        wait_for_punches: bool,
        reserve: impl FnOnce(u64) -> bool,
    ) -> Result<(), Refused> {
        if self.retired {
            return Err(Refused::Starved(Starved::Removed));
        }
        let need = u64::from(run.pages);
        if let Some(refused) = self.refused_by_cap(need, wait_for_punches) {
            return Err(refused);
        }
        if !reserve(need) {
            return Err(Refused::Starved(Starved::Total));
        }
        self.populated += need;
        Ok(())
    }

    /// cladefs: `run`, held still, was punched out, if `done`: its pages populate it no more, its
    /// place kept (board 14629); or its punch failed, and they are leaked, counted still. Returns
    /// the pages it populates no more.
    pub(super) fn depopulated(&mut self, run: Run, done: bool) -> u64 {
        let pages = u64::from(run.pages);
        if !done {
            self.leaked += pages;
            return 0;
        }
        self.populated -= pages;
        pages
    }

    /// A run of `pages` pages, its slot marked taken: a spare for a whole slab, if `spares` and it
    /// has one, else the lowest slot free; for fewer, the first fit of the small slab with the
    /// smallest largest run that holds it, else a free slot made small.
    fn take_run(&mut self, pages: u16, spares: bool) -> Taken {
        if pages == self.pps {
            if spares && let Some(slab) = self.spares.pop() {
                self.slots[slab as usize] = Slot::Whole;
                return Taken { run: Run { slab, first: 0, pages }, grown: None, spare: true };
            }
            let (slab, grown) = self.take_slot();
            self.slots[slab as usize] = Slot::Whole;
            return Taken { run: Run { slab, first: 0, pages }, grown, spare: false };
        }
        let fitting = self.smalls.range((pages, 0)..).next().copied();
        let (slab, mut small, grown) = match fitting {
            Some((largest, slab)) => {
                self.smalls.remove(&(largest, slab));
                let Slot::Small(small) = self.slots[slab as usize] else {
                    unreachable!("slab {slab} listed small")
                };
                (slab, small, None)
            }
            None => {
                let (slab, grown) = self.take_slot();
                (slab, Small { used: [0; 4], largest: self.pps }, grown)
            }
        };
        let first = small.fit(pages, self.pps).0.expect("a small slab listed with room");
        small.set(first..first + pages, true);
        small.largest = small.fit(1, self.pps).1;
        self.slots[slab as usize] = Slot::Small(small);
        if small.largest > 0 {
            self.smalls.insert((small.largest, slab));
        }
        Taken { run: Run { slab, first, pages }, grown, spare: false }
    }

    /// The lowest slot free, or a new one, which grows its object to the slots it then holds.
    fn take_slot(&mut self) -> (u32, Option<u32>) {
        if let Some(slab) = self.free.pop_first() {
            return (slab, None);
        }
        self.slots.push(Slot::Free);
        let slots = self.slots.len() as u32;
        (slots - 1, Some(slots))
    }

    /// `run`, which it gave, is freed: a whole slab is kept as a spare if `keep`, it has fewer
    /// than [`SPARE_SLABS`] and it is within its cap; else it is to be punched, and is no one's
    /// until it is ([`ArenaState::punched`]). cladefs: one not `populated` (placed, or
    /// depopulated, board 14629) is punched, in case anything put pages there, but makes no room:
    /// a spare never, nor counted as being punched.
    pub(super) fn free(&mut self, run: Run, keep: bool, populated: bool) -> Freed {
        self.relief.exhausted = false;
        self.relief.backoff.clear();
        let slot = &mut self.slots[run.slab as usize];
        if run.pages == self.pps {
            assert!(matches!(slot, Slot::Whole), "slab {} freed, not whole", run.slab);
            let room = self.cap.is_none_or(|cap| self.populated <= cap);
            if populated && keep && !self.retired && room && self.spares.len() < SPARE_SLABS {
                *slot = Slot::Spare;
                self.spares.push(run.slab);
                return Freed::Spare;
            }
            *slot = Slot::Punching;
        } else {
            let Slot::Small(small) = slot else { panic!("slab {} freed, not small", run.slab) };
            assert!(
                (run.first..run.first + run.pages).all(|page| small.is_used(page)),
                "{run:?} freed, not allocated"
            );
        }
        if populated {
            self.punching += u64::from(run.pages);
        }
        Freed::Punch
    }

    /// A spare, given back, if it has one: to be punched, as [`ArenaState::free`] says one is.
    pub(super) fn release_spare(&mut self) -> Option<Run> {
        let slab = self.spares.pop()?;
        self.slots[slab as usize] = Slot::Punching;
        self.punching += u64::from(self.pps);
        Some(Run { slab, first: 0, pages: self.pps })
    }

    /// Its spares, given back, as [`ArenaState::release_spare`] gives one.
    pub(super) fn release_spares(&mut self) -> Vec<Run> {
        std::iter::from_fn(|| self.release_spare()).collect()
    }

    /// `run`, being punched, was, if `done`: its pages no one's, and populated no more; or its
    /// punch failed, and they are leaked, kept as used and counted (doc-26, Failure modes).
    /// Returns the pages it populates no more. cladefs: of a run freed not `populated`
    /// ([`ArenaState::free`]), none: its pages were counted by nothing.
    pub(super) fn punched(&mut self, run: Run, done: bool, populated: bool) -> u64 {
        let pages = u64::from(run.pages);
        let counted = if populated { pages } else { 0 };
        self.punching -= counted;
        if !done {
            self.leaked += counted;
        }
        let slot = &mut self.slots[run.slab as usize];
        if run.pages == self.pps {
            assert!(matches!(slot, Slot::Punching), "slab {} punched, not punching", run.slab);
            if !done {
                *slot = Slot::Leaked;
                return 0;
            }
            *slot = Slot::Free;
            self.free.insert(run.slab);
            self.populated -= counted;
            return counted;
        }
        let Slot::Small(small) = slot else { panic!("slab {} punched, not small", run.slab) };
        if !done {
            return 0;
        }
        self.smalls.remove(&(small.largest, run.slab));
        small.set(run.first..run.first + run.pages, false);
        small.largest = small.fit(1, self.pps).1;
        if small.is_empty() {
            *slot = Slot::Free;
            self.free.insert(run.slab);
        } else {
            self.smalls.insert((small.largest, run.slab));
        }
        self.populated -= counted;
        counted
    }

    /// Whether its page `page` of slab `slab` is free: neither held nor quarantined, a spare's or
    /// being punched. `None` if its object holds no such slab.
    pub(super) fn is_free(&self, slab: u32, page: u16) -> Option<bool> {
        Some(match self.slots.get(slab as usize)? {
            Slot::Free | Slot::Spare | Slot::Punching => true,
            Slot::Whole | Slot::Leaked => false,
            Slot::Small(small) => !small.is_used(page),
        })
    }

    /// How many spares it has.
    pub(super) fn spares(&self) -> usize {
        self.spares.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A slab of the tests' arenas: 1 MiB of 4 KiB pages.
    const PPS: u16 = 256;

    fn take(arena: &mut ArenaState, pages: u16) -> Run {
        arena.take(pages, true, |_| true).expect("a run").run
    }

    /// Frees `run` and punches it, as an arena's owner does.
    fn free(arena: &mut ArenaState, run: Run) {
        if arena.free(run, true, true) == Freed::Punch {
            arena.punched(run, true, true);
        }
    }

    /// cladefs: a run placed (board 14629) populates nothing, nor takes a spare, which holds
    /// memory; populated, its pages count, within its cap; depopulated, they count no more, its
    /// place kept; freed unpopulated, punched, it gives no room back.
    #[test]
    fn placed_runs_count_once_populated() {
        let mut arena = ArenaState::new(PPS);
        arena.cap = Some(u64::from(PPS) + 8);
        // A spare, which a placement passes over.
        let whole = take(&mut arena, PPS);
        assert_eq!(arena.free(whole, true, true), Freed::Spare);
        assert_eq!(arena.populated, u64::from(PPS));

        let placed = arena.place(PPS).expect("a place").run;
        assert_ne!(placed.slab, whole.slab, "a spare taken by a placement");
        assert_eq!(arena.populated, u64::from(PPS), "a placement counted");
        // Past its cap, with the spare's memory counted: refused, until the spare goes.
        assert!(matches!(
            arena.populate(placed, false, |_| true),
            Err(Refused::Starved(Starved::Arena))
        ));
        let spare = arena.release_spare().expect("the spare");
        assert_eq!(arena.punched(spare, true, true), u64::from(PPS));
        arena.populate(placed, false, |_| true).expect("room for it");
        assert_eq!(arena.populated, u64::from(PPS));

        assert_eq!(arena.depopulated(placed, true), u64::from(PPS));
        assert_eq!(arena.populated, 0);
        assert_eq!(arena.is_free(placed.slab, 0), Some(false), "its place kept");
        // Freed unpopulated: punched, counting nothing.
        assert_eq!(arena.free(placed, true, false), Freed::Punch);
        assert_eq!(arena.punching, 0);
        assert_eq!(arena.punched(placed, true, false), 0);
        assert_eq!(arena.is_free(placed.slab, 0), Some(true));
        assert_eq!(arena.populated, 0);

        // A small run placed, populated, freed populated: counted, then given back.
        let small = arena.place(8).expect("a place").run;
        arena.populate(small, false, |_| true).expect("room");
        assert_eq!(arena.populated, 8);
        free(&mut arena, small);
        assert_eq!(arena.populated, 0);
    }

    /// Runs shorter than a slab share small slabs, first fit in the one whose largest run unused
    /// is the smallest that holds them: pages freed among pages in use are taken by runs that fit
    /// them, rather than a larger run split (TASK-131.1's rule, kept). A small slab whose runs are
    /// all freed is free, and taken again.
    #[test]
    fn small_runs_fit_the_tightest_slab() {
        let mut arena = ArenaState::new(PPS);
        let runs: Vec<Run> = (0..64).map(|_| take(&mut arena, 4)).collect();
        assert!(runs.iter().all(|run| run.slab == 0), "{runs:?}");
        assert_eq!(runs[5], Run { slab: 0, first: 20, pages: 4 });
        // A hole of 4 pages among those used, and a slab of its own for a run of 200.
        free(&mut arena, runs[5]);
        let large = take(&mut arena, 200);
        assert_eq!(large, Run { slab: 1, first: 0, pages: 200 });
        // A run of 3 fits the hole, of 4, rather than slab 1's 56 pages unused.
        assert_eq!(take(&mut arena, 3), Run { slab: 0, first: 20, pages: 3 });
        assert_eq!(take(&mut arena, 2), Run { slab: 1, first: 200, pages: 2 });
        assert_eq!(arena.populated, 64 * 4 - 4 + 3 + 200 + 2);
        free(&mut arena, large);
        assert_eq!(arena.is_free(1, 0), Some(true));
        assert_eq!(arena.is_free(1, 200), Some(false));
        for run in runs.iter().filter(|run| run.first != 20) {
            free(&mut arena, *run);
        }
        assert_eq!(arena.is_free(0, 20), Some(false), "the run of 3 holds slab 0");
        assert_eq!(arena.is_free(0, 0), Some(true));
        assert_eq!(arena.populated, 3 + 2);
    }

    /// A whole slab freed is kept as a spare, populated still, up to two, which the next whole
    /// slabs take before any slot free; beyond them it is punched, and its slot free. Spares
    /// given back are punched.
    #[test]
    fn whole_slabs_freed_are_kept_spare_up_to_two() {
        let mut arena = ArenaState::new(PPS);
        let runs: Vec<Run> = (0..3).map(|_| take(&mut arena, PPS)).collect();
        assert_eq!(arena.populated, 3 * u64::from(PPS));
        for run in &runs {
            free(&mut arena, *run);
        }
        assert_eq!(arena.spares(), 2);
        assert_eq!(arena.populated, 2 * u64::from(PPS), "the third punched");
        let mut reserved = 0;
        let taken = (arena.take(PPS, true, |need| {
            reserved += need;
            true
        }))
        .unwrap();
        assert_eq!((taken.run.slab, taken.grown, taken.spare), (1, None, true));
        assert_eq!(reserved, 0, "a spare, populated already");
        let released = arena.release_spares();
        assert_eq!(released.len(), 1);
        for run in released {
            assert_eq!(arena.punched(run, true, true), u64::from(PPS));
        }
        assert_eq!(arena.populated, u64::from(PPS));
        assert_eq!(take(&mut arena, PPS).slab, 0, "the lowest slot free");
    }

    /// An arena at its cap with runs being punched that would make it room waits for them
    /// ([`Refused::Punching`]), rather than be starved; without them, or past what they free, it
    /// is starved at its own cap. A punch that fails leaks its pages, counted still.
    #[test]
    fn caps_wait_for_punches_that_make_room() {
        let mut arena = ArenaState::new(PPS);
        arena.cap = Some(2 * u64::from(PPS));
        let (first, second) = (take(&mut arena, PPS), take(&mut arena, PPS));
        assert_eq!(arena.refusal(1, true), Some(Refused::Starved(Starved::Arena)));
        assert_eq!(arena.free(first, false, true), Freed::Punch);
        assert_eq!(arena.refusal(PPS, true), Some(Refused::Punching));
        assert_eq!(arena.refusal(PPS, false), Some(Refused::Starved(Starved::Arena)));
        assert_eq!(arena.punched(first, false, true), 0, "leaked");
        assert_eq!((arena.populated, arena.leaked), (2 * u64::from(PPS), u64::from(PPS)));
        assert_eq!(arena.refusal(1, true), Some(Refused::Starved(Starved::Arena)));
        free(&mut arena, second);
        assert_eq!(arena.refusal(PPS, true), None);
        arena.retired = true;
        assert_eq!(arena.refusal(1, true), Some(Refused::Starved(Starved::Removed)));
    }
}
