// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A cell crashes at random versions of random workloads, and recovers to a prefix of its commits:
//! exactly the extents and root of an epoch at least as late as the last commit made durable, by
//! a durable commit or a flush of commits only ordered; cladefs: as its open segment closes between
//! commits too, as an uploader has it close.
//!
//! cladefs: a round is a function of its seed (TASK-79.79): its cell does its I/O on the test's
//! thread alone, none on threads of its own ([`MemDir::serial`]), so that the operation its file
//! system fails from, and what the crash keeps, are the same each time it is run. The Shuttle tests
//! run the same rounds ([`rounds`]) with the cell's threads, which frame the segments commits
//! close, and sync a flush's files at once, while the workload goes on: on Shuttle's schedules,
//! seeded by the rounds, so that each is still a function of its seed (tests/shuttle/crash.rs).

#[path = "../common/host_runtime.rs"]
mod host_runtime;
mod kept;
mod rounds;

use kept::{CUT_ROUNDS, KEPT_ROUNDS, cut, kept};
use local_cell::MemDir;
use rounds::{First, ROUNDS, recovering, recovering_after, same, twice};

/// cladefs: each round runs twice, at once, and its two runs' crashes leave the same directories,
/// and they end with the same: a round is a function of its seed alone, so that a round that failed
/// fails again, and a round that passed says what it seems (TASK-79.79).
#[tokio::test(flavor = "multi_thread")]
async fn recovers_a_prefix_of_its_commits() {
    let mut crashes_mid_commit = 0;
    for round in 0..ROUNDS {
        let ((first, mid_commit), (second, _)) =
            twice(async || recovering(round, MemDir::serial()).await).await;
        same(round, &first, &second);
        crashes_mid_commit += mid_commit as u64;
    }
    // The crashes hit commits as well as what came between them.
    assert!(crashes_mid_commit > ROUNDS / 10, "{crashes_mid_commit} crashes in commits");
    // cladefs: and the workloads' commits spanned segments, sealing extents in those filled between
    // commits (TASK-92.41): with segments that never filled, none did.
    let spanning = local_cell::commits_spanning_segments();
    assert!(spanning > ROUNDS, "{spanning} commits spanned segments in {ROUNDS} rounds");
}

/// Board 5744: a cell killed in a random workload, its host up, recovers what its writes left,
/// durable or not, and keeps it through a power loss after it carried on and flushed.
#[tokio::test(flavor = "multi_thread")]
async fn keeps_what_it_recovered_after_a_kill_through_a_power_loss() {
    for round in 0..ROUNDS {
        recovering_after(round, MemDir::serial(), First::Kill).await;
    }
}

/// cladefs: a cell keeping its cell in a bucket, crashing as its uploader uploads, recovers from
/// the bucket alone at least what its uploader acknowledged, and from its disk at least what it
/// made durable (TASK-165.9's AC #4); the host's collector passing over the bucket all along, cut
/// in a pass as it crashes, or before, and a full pass after (TASK-291.2); then passes alone,
/// which leave nothing they should have collected (TASK-390).
#[tokio::test(flavor = "multi_thread")]
async fn recovers_from_its_bucket_with_and_without_its_disk() {
    let (mut uploaded, mut ahead, mut cut, mut deleted) = (0, 0, 0, 0);
    for round in 0..KEPT_ROUNDS {
        let found = kept(round).await;
        uploaded += found.acknowledged as u64;
        ahead += found.ahead as u64;
        cut += found.cut as u64;
        deleted += found.deleted as u64;
    }
    // The crashes hit uploads under way, and cells whose disks held more than the bucket; the
    // collector's cuts, passes under way; and its passes deleted what the cells had uploaded.
    // cladefs: each round waits for an upload early in its workload, so that most
    // acknowledge a commit whatever the build's speed (board 1738): 82 to 88 of 100 did on the
    // Mac, debug or optimized.
    assert!(uploaded > KEPT_ROUNDS / 2, "{uploaded} rounds acknowledged a commit");
    assert!(ahead > KEPT_ROUNDS / 10, "{ahead} rounds held commits the bucket lacked");
    assert!(cut > KEPT_ROUNDS / 4, "{cut} rounds cut a pass before the crash");
    assert!(deleted > KEPT_ROUNDS / 3, "{deleted} rounds' passes deleted");
    // cladefs: and commits spanning segments uploaded, acknowledged with the segments ending them,
    // and recovered (TASK-92.41): each round's waited-for commit spans segments by construction,
    // a burst's (board 1852), and its workload makes more: 188 to 211 in 100 rounds on the Mac.
    let spanning = local_cell::commits_spanning_segments();
    assert!(spanning > KEPT_ROUNDS, "{spanning} commits spanned segments in {KEPT_ROUNDS} rounds");
}

/// cladefs: a cell's opening cut by a crash at each of its writes in turn leaves what opens again
/// to at least what the cell made durable (TASK-165.9).
#[tokio::test(flavor = "multi_thread")]
async fn openings_cut_at_each_write_open_again() {
    let mut writes = 0;
    for round in 0..CUT_ROUNDS {
        writes += cut(round).await;
    }
    // The openings write: a segment closed again, the WAL's start, a restated capacity.
    assert!(writes > CUT_ROUNDS, "{writes} writes in {CUT_ROUNDS} openings");
}
