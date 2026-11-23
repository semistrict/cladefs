// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The slabs under Shuttle (awslabs/shuttle, the `shuttle` feature): an allocation racing its
//! arena's removal, and a run's free; an allocation racing a punch that makes it room; small
//! slabs' runs allocated and freed at once; and a run freed racing the VM process it was staged
//! to unmapping it, and its reuse; in the interleavings Shuttle's schedulers choose. Relief's
//! thread starts under no Shuttle run (`sync::try_spawn`): the daemon's tests, end to end, run it.

use super::sync::{Rules, set_rules};
use super::*;
use crate::shuttle_testing::{config, fails_with, runs};
use shuttle::scheduler::{PctScheduler, RandomScheduler};
use shuttle::{Runner, thread};

/// The schedules a test runs with each scheduler.
const RUNS: usize = 100;

/// Runs `test` in schedules of each scheduler's, in physical memory of a few slabs.
fn check(test: fn()) {
    static CONFIGURED: std::sync::Once = std::sync::Once::new();
    CONFIGURED.call_once(|| configure(16 * SLAB).expect("physical memory is not in use yet"));
    Runner::new(RandomScheduler::new(runs(RUNS)), config()).run(test);
    Runner::new(PctScheduler::new(3, runs(RUNS)), config()).run(test);
}

/// The pages of a slab.
fn whole() -> u64 {
    SLAB / page_size()
}

/// TASK-79.36: an allocation that may wait for a run of an arena, racing the arena's removal,
/// takes one, or fails, as nothing makes a removed arena room. The arena goes once the runs it
/// gave are freed, and its memory with them.
fn allocating_races_removing_the_arena() {
    let arena = create_arena().unwrap();
    let held = alloc(arena, 1, Wait::No).unwrap();
    let allocator = thread::spawn(move || alloc(arena, whole(), Wait::Yes).ok());
    remove_arena(arena);
    let taken = allocator.join().unwrap();
    assert!(arena_fd(arena).is_none(), "the arena stayed");
    let before = arenas_bytes();
    let held_bytes = held.len() + taken.as_ref().map_or(0, PartSlab::len);
    drop((held, taken));
    assert_eq!(arenas_bytes(), before - held_bytes, "a removed arena's runs held memory freed");
}

#[test]
fn allocating_races_removing_the_arena_in_any_order() {
    check(allocating_races_removing_the_arena);
}

/// An allocation that a run being punched makes room for, its memory going back outside the
/// arena's lock, waits for it, and takes a run: here one that may not wait, which would fail at
/// once otherwise, at its arena's cap, with no spare (none kept), and nothing under the cap but
/// the run being punched. Of a small slab, and of a whole one.
fn allocating_races_a_punch() {
    keep_spare_slabs(false);
    let arena = create_arena().unwrap();
    for pages in [whole(), 3] {
        let held = alloc(arena, pages, Wait::No).unwrap();
        set_arena_cap(arena, Some(arena_bytes(arena)));
        let node = node();
        let freer = thread::spawn(move || drop(held));
        // Freed, once its run is being punched, or punched.
        while node.punching.load(Ordering::Relaxed) == 0 && arena_bytes(arena) > 0 {
            thread::yield_now();
        }
        let taken = alloc(arena, pages, Wait::No);
        freer.join().unwrap();
        let taken = taken.expect("a run, as the punch made room");
        drop(taken);
        set_arena_cap(arena, None);
    }
    remove_arena(arena);
    assert_eq!(arena_bytes(arena), 0);
    keep_spare_slabs(true);
}

#[test]
fn allocating_races_a_punch_in_any_order() {
    check(allocating_races_a_punch);
}

/// An allocation that does not wait for a run being punched fails, at its arena's cap, though the
/// run is about to be free.
#[test]
fn needs_allocations_to_wait_for_punches() {
    set_rules(Rules { allocations_wait_for_punches: false, ..Rules::ALL });
    fails_with("a run, as the punch made room", || check(allocating_races_a_punch));
}

/// Runs of small slabs allocated and freed on two threads at once never overlap: each run's pages
/// are its own while it is held, each page written its holder's mark reads it back, and, all
/// freed and spares given back, the arena holds no memory.
fn small_runs_race() {
    let arena = create_arena().unwrap();
    let workers: Vec<_> = (0..2u8)
        .map(|worker| {
            thread::spawn(move || {
                for (round, pages) in [2u64, 5, 1].into_iter().enumerate() {
                    let run = alloc(arena, pages, Wait::No).unwrap();
                    let mark = vec![worker * 16 + round as u8 + 1; run.len() as usize];
                    run.write(0, &mark).unwrap();
                    thread::yield_now();
                    let mut read = vec![0; run.len() as usize];
                    run.read(0, &mut read).unwrap();
                    assert_eq!(read, mark, "{run:?}'s pages another run's");
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    release_spares(Some(arena));
    assert_eq!(arena_bytes(arena), 0, "memory held, its runs freed");
    remove_arena(arena);
}

#[test]
fn small_runs_race_in_any_order() {
    check(small_runs_race);
}

/// A VM process's second stage, as the extent daemon's connection to one is: it maps runs as they
/// are staged, but makes the changes taking them away later, on a thread of its own
/// ([`LaggingStage::make`]), as the VM process makes them: what a guest reads through it meanwhile
/// is what the runs hold.
#[derive(Default)]
struct LaggingStage(shuttle::sync::Mutex<Lagging>);

#[derive(Default)]
struct Lagging {
    /// What the guest reaches: where each page's memory is.
    mapped: BTreeMap<u64, PhysLocation>,
    /// The unmaps sent, not made yet, and how many were sent, and made.
    unmaps: std::collections::VecDeque<u64>,
    sent: u64,
    made: u64,
    /// The runs freed, quarantined until the unmaps sent before are made.
    parked: Vec<(u64, Quarantined)>,
}

/// The VM process of [`LaggingStage`].
const VM: VmProcessId = VmProcessId(7);

impl LaggingStage {
    /// Maps `slab`'s first page at `offset`, staged to it.
    fn map(&self, offset: u64, slab: &PartSlab) {
        slab.stage_to(VM);
        self.0.lock().unwrap().mapped.insert(offset, slab.location());
    }

    /// Sends the unmap of the page at `offset`, which it makes later.
    fn unmap(&self, offset: u64) {
        let mut stage = self.0.lock().unwrap();
        stage.unmaps.push_back(offset);
        stage.sent += 1;
    }

    /// Makes the oldest unmap not made yet, and lets go of the runs that waited for it: whether
    /// there was one.
    fn make(&self) -> bool {
        let released: Vec<_> = {
            let mut stage = self.0.lock().unwrap();
            let Some(offset) = stage.unmaps.pop_front() else { return false };
            stage.mapped.remove(&offset);
            stage.made += 1;
            let made = stage.made;
            stage.parked.extract_if(.., |(sent, _)| *sent <= made).collect()
        };
        // Freed without the stage's lock.
        drop(released);
        true
    }

    /// Quarantines `freed`, staged to it, until the unmaps sent so far are made.
    fn park(&self, freed: Quarantined) {
        assert_eq!(freed.staged(), [VM], "quarantined for another VM process");
        let mut stage = self.0.lock().unwrap();
        if stage.made < stage.sent {
            let sent = stage.sent;
            stage.parked.push((sent, freed));
            return;
        }
        drop(stage);
        drop(freed);
    }

    /// The first byte of what the guest reads at `offset` through the stage, if anything is
    /// mapped there, read as its VM process maps the page.
    fn read(&self, offset: u64) -> Option<u8> {
        let stage = self.0.lock().unwrap();
        let page = stage.mapped.get(&offset)?;
        Some(read_byte(*page))
    }
}

/// The first byte of the page at `location`, read through its arena's object, as a VM process
/// mapping it reads it.
fn read_byte(location: PhysLocation) -> u8 {
    // An arena's memory, a test's read of it: outside the simulation.
    use std::os::unix::fs::FileExt;
    let fd = arena_fd(location.arena).expect("the page's arena");
    let file = std::fs::File::from(fd.try_clone().expect("the arena's object, again"));
    let mut byte = [0];
    file.read_exact_at(&mut byte, location.offset).expect("the page read");
    byte[0]
}

/// The stage of the schedule running, which the quarantine parks the runs freed on.
static LAGGING: std::sync::Mutex<Option<Arc<LaggingStage>>> = std::sync::Mutex::new(None);

/// The quarantine of the tests of [`freeing_races_the_second_stage`], which parks the runs freed
/// on the stage of the schedule running, held for as long as the test runs.
fn lagging_quarantine() -> quarantine::QuarantineForTest {
    quarantine::quarantine_for_test(|freed| {
        LAGGING.lock().unwrap().as_ref().expect("a schedule's stage").park(freed)
    })
}

/// What a run freed of [`freeing_races_the_second_stage`] holds, and what its next holder writes.
const FREED: u8 = 0xf7;
const REUSED: u8 = 0x0e;

/// TASK-79.9, doc-26: a run a VM process was staged, which makes the changes it is sent after they
/// return, freed, is no other allocation's until the VM process has taken it away: a guest reading
/// it through the stage meanwhile reads what it held, never what its next holder writes. A page
/// staged through a [`LaggingStage`], the unmap sent and the run dropped, and another allocated
/// and written, as its VM process makes the unmap and its guest reads.
fn freeing_races_the_second_stage() {
    let stage = Arc::new(LaggingStage::default());
    *LAGGING.lock().unwrap() = Some(stage.clone());
    let arena = create_arena().unwrap();
    // A run of the small slab the next is of, which keeps it small as the next is freed, and so
    // that run's pages the next allocated.
    let keeper = alloc(arena, 1, Wait::No).unwrap();
    let run = alloc(arena, 1, Wait::No).unwrap();
    run.write(0, &vec![FREED; page_size() as usize]).unwrap();
    stage.map(0, &run);
    assert_eq!(stage.read(0), Some(FREED));

    let vmm = {
        let stage = stage.clone();
        thread::spawn(move || {
            while !stage.make() {
                thread::yield_now();
            }
        })
    };
    let guest = {
        let stage = stage.clone();
        thread::spawn(move || {
            for _ in 0..3 {
                if let Some(read) = stage.read(0) {
                    assert_eq!(read, FREED, "the guest read a run another holds");
                }
                thread::yield_now();
            }
        })
    };
    stage.unmap(0);
    drop(run);
    let next = alloc(arena, 1, Wait::No).unwrap();
    next.write(0, &vec![REUSED; page_size() as usize]).unwrap();
    guest.join().unwrap();
    vmm.join().unwrap();
    assert!(stage.0.lock().unwrap().parked.is_empty(), "runs quarantined once the unmap was made");
    LAGGING.lock().unwrap().take();
    drop((keeper, next));
    remove_arena(arena);
}

#[test]
fn freeing_races_the_second_stage_in_any_order() {
    let _quarantine = lagging_quarantine();
    check(freeing_races_the_second_stage);
}

/// A run freed at once, though the VM process it was staged to may map it still, is another's that
/// the guest reads.
#[test]
fn needs_staged_runs_to_wait_for_their_stages() {
    set_rules(Rules { staged_runs_wait_for_their_stages: false, ..Rules::ALL });
    let _quarantine = lagging_quarantine();
    fails_with("the guest read a run another holds", || check(freeing_races_the_second_stage));
}
