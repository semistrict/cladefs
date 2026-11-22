// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What the slabs' users cannot see, which these tests reach inside for: the memory an arena's
//! object holds, as the host counts it. Physical memory is the process's: nextest runs each test
//! in a process of its own.

use super::*;

/// The memory `arena`'s object holds, as the host counts it: its blocks.
#[cfg(target_os = "linux")]
fn held(arena: ArenaId) -> u64 {
    node().arena(arena).expect("the arena").object.held().expect("the object's blocks")
}

/// doc-26: what the governor counts of an arena ([`arena_bytes`]) is the memory its object holds,
/// exactly, as runs are allocated and written, freed and punched, whole slabs kept as spares and
/// given back: no page counted that holds no memory, none holding memory uncounted.
#[cfg(target_os = "linux")]
#[test]
fn arenas_count_the_memory_their_objects_hold() {
    let ps = page_size();
    let arena = create_arena();
    let check = |when: &str| assert_eq!(arena_bytes(arena), held(arena), "{when}");
    let written = |pages: u64, byte: u8| {
        let run = alloc(arena, pages, Wait::No).unwrap();
        run.write(0, &vec![byte; (pages * ps) as usize]).unwrap();
        run
    };
    let wholes: Vec<PartSlab> = (1..=3).map(|byte| written(SLAB / ps, byte)).collect();
    let smalls: Vec<PartSlab> = [1, 3, 7, 2].into_iter().map(|pages| written(pages, 9)).collect();
    check("written");
    assert_eq!(arena_bytes(arena), 3 * SLAB + 13 * ps);
    drop(wholes);
    check("whole slabs freed, two kept spare");
    assert_eq!(arena_bytes(arena), 2 * SLAB + 13 * ps);
    let mut smalls = smalls.into_iter();
    drop(smalls.next());
    check("a small run freed");
    drop(smalls);
    check("every small run freed");
    let again = written(SLAB / ps, 4);
    check("a spare taken");
    assert_eq!(arena_bytes(arena), 2 * SLAB);
    drop(again);
    release_spares(Some(arena));
    check("spares given back");
    assert_eq!(arena_bytes(arena), 0);
    assert_eq!(arenas_bytes(), 0);
}

/// An arena at its cap holding nothing but spares gives them back for a run they can't serve, one
/// shorter than a slab, rather than ask relief, which has nothing to evict, and fail: its short
/// parts were unreadable until something else gave the spares back (Codex's review of board 6204).
/// A whole slab takes a spare, within the cap.
#[test]
fn capped_arenas_give_their_spares_back_for_shorter_runs() {
    let arena = create_arena();
    set_arena_cap(arena, Some(SLAB));
    drop(alloc(arena, SLAB / page_size(), Wait::No).unwrap());
    assert_eq!(arena_bytes(arena), SLAB, "a spare kept");
    let whole = alloc(arena, SLAB / page_size(), Wait::No).expect("the spare");
    drop(whole);
    let short = alloc(arena, 1, Wait::No).expect("a run of a page, the spare given back");
    assert_eq!(arena_bytes(arena), page_size());
    drop(short);
}

/// doc-26: a run staged to VM processes forgets those gone as it is staged to another, so that it
/// is staged to as many as were connected at once, however many came and went: a public base's
/// parts, mapped by connection after connection, kept every ID they were ever staged to, scanned at
/// each map (Codex's review of board 6208). No ID is another's: one reused had a run staged to a VM
/// process gone wait for the next, another tenant's frozen one perhaps (the verifying review).
#[test]
fn runs_staged_forget_vm_processes_gone() {
    let run = alloc(ArenaId::DEFAULT, 1, Wait::No).unwrap();
    let (gone, connected) = (VmProcessId::connected(), VmProcessId::connected());
    run.stage_to(gone);
    gone.gone();
    let next = VmProcessId::connected();
    assert_ne!(next, gone, "an ID taken again");
    run.stage_to(connected);
    run.stage_to(next);
    assert_eq!(run.staged(), [connected, next]);
}
