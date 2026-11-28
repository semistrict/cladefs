// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: Zombie requests landing late, by BUGGIFY, and the takeovers they race (board 14565),
//! local_cell::sim's `open` (a takeover), `hold` and profiles `takeovers`, `recreated`,
//! `fast_wal_takeovers` and `fast_wal_zombies`, ported.
//!
//! A zombie is a writer taken over whose requests are in flight still: its create of a segment, its
//! swap of the head, its append to a fast WAL segment, which land after its successor's take, fence
//! or sweep. Two things make one here. The bucket holds a write a long while now and then before it
//! goes out (`bucket_cell::object_bucket`'s BUGGIFY sites, which local_cell::sim's `hold` made by
//! parking a taken-over writer's creates until a delete at their key); and, beside each round's
//! writer, other hosts take its cell at the same placement, at times the seed draws ([`during`]),
//! rather than only the next round's host once the writer has had its time, as the attrition has it.
//! What a zombie's request lands on is what the attrition's own check reads: every block told
//! uploaded, or told durable by a fast WAL, reads back once healed.

use super::{Healed, Round};
use crate::{Profile, SEARCHED, control, shuffled, token_of};
use cladefs::cladefsd::control::proto::{CreateCellRequest, DropCellRequest};
use std::time::Duration;

/// cladefs: the odds that a round has the cell taken by rivals of its writer's host.
const RIVALED: f64 = 0.7;

/// cladefs: the latest after a round's take that a rival takes the cell, a writer's first writes to
/// its done, as the attrition waits for it (up to 3 s) before the next round's take.
const RIVAL_WITHIN: Duration = Duration::from_millis(2500);

/// cladefs: how often a rival tries to take the cell, a host's copy of an earlier round dropped
/// first, or the bucket refusing, before it gives up: a rival's take failing is no zombie, not a
/// loss.
const RIVAL_TRIES: usize = 5;

/// cladefs: what it does beside a round's writer: as the seed says, one or both of the other hosts
/// take the cell at the round's own placement, each after a time the seed draws, the writer's host
/// then a zombie whose requests, held by BUGGIFY or in flight, land past the take, fence and sweep.
pub(crate) fn during(round: &Round<'_>) -> Vec<madsim::task::JoinHandle<()>> {
    use madsim::rand::Rng as _;
    if !madsim::rand::thread_rng().gen_bool(RIVALED) {
        return Vec::new();
    }
    let others: Vec<usize> =
        shuffled((0..round.nodes.len()).filter(|&host| host != round.host).collect());
    let rivals = madsim::rand::thread_rng().gen_range(1..=others.len());
    let token = token_of(round.round);
    let latest = RIVAL_WITHIN.as_millis() as u64;
    others
        .into_iter()
        .take(rivals)
        .map(|host| {
            let after = Duration::from_millis(madsim::rand::thread_rng().gen_range(0..=latest));
            round.manager.spawn(rival_take(host as u8 + 1, round.cell, token, after))
        })
        .collect()
}

/// cladefs: host `host`'s daemon takes `cell` at placement `token`, as the VM manager asks of it,
/// `after` a while: dropping its copy of an earlier round first if it has one, and trying again a
/// few times while the bucket refuses. A refusal is logged and is not a failure: a placement cut
/// off, or a bucket out of reach, takes nothing, and the round's writer is no zombie then.
async fn rival_take(host: u8, cell: u64, token: u64, after: Duration) {
    madsim::time::sleep(after).await;
    // The control API of a host killed answers not: no rival there.
    let mut daemon = match cladefs::cladefsd::control::connect(control(host)).await {
        Ok(daemon) => daemon,
        Err(err) => return log::info!("host {host}: no rival take of cell {cell}: {err:?}"),
    };
    let taking = CreateCellRequest {
        capacity: 64 << 20,
        token: Some(token),
        cell,
        ..CreateCellRequest::default()
    };
    for _ in 0..RIVAL_TRIES {
        match daemon.create_cell(taking.clone()).await {
            Ok(_) => return log::info!("host {host}: a rival took cell {cell} at token {token}"),
            Err(status) if status.code() == tonic::Code::AlreadyExists => {
                if let Err(err) = daemon.drop_cell(DropCellRequest { tenant: 0, cell }).await {
                    log::info!("host {host}: dropping cell {cell} for a rival: {err:?}");
                }
            }
            Err(status) => log::info!("host {host}: a rival's take of cell {cell}: {status:?}"),
        }
        madsim::time::sleep(Duration::from_millis(200)).await;
    }
    log::info!("host {host}: a rival took no cell {cell} at token {token}");
}

/// cladefs: what it checks once healed: nothing of its own. A zombie's request landing late is a
/// loss only if it takes a block told uploaded (or told durable) away or overwrites it, which the
/// attrition's check of every block read back finds.
pub(crate) async fn healed(_healed: &Healed<'_>) -> Result<(), String> {
    Ok(())
}

/// cladefs: the attrition, zombies in it: rivals taking the cell beside each round's writer, the
/// bucket holding writes now and then (BUGGIFY) so that a zombie's land late. (local_cell::sim's
/// `takeovers`.)
pub(crate) const ZOMBIES: Profile = Profile { zombies: true, ..Profile::PLAIN };

/// cladefs: [`ZOMBIES`] with frees, versions and the collector's passes, which delete what a
/// zombie's late create recreates: a segment a pass deleted, created again holding other extents
/// than readers look for there. (local_cell::sim's `recreated`.)
pub(crate) const RECREATED: Profile = Profile { zombies: true, ..Profile::COLLECTED };

/// cladefs: [`ZOMBIES`] of a fast WAL cell, hosts killed losing their disks at times: takes of
/// either zone fence and recover the fast WAL while a zombie's create or append is in flight, or
/// held, past the fence, the roll and the sweep. (local_cell::sim's `fast_wal_takeovers` and
/// `fast_wal_zombies`.)
pub(crate) const FAST_ZOMBIES: Profile =
    Profile { zombies: true, fast_wal: true, lose: true, ..Profile::PLAIN };

/// cladefs: board 14565: the attrition with rivals taking the cell as its writers write, and writes
/// held: every block told uploaded reads back once healed. (local_cell::sim's
/// `takeovers_while_uploading`.)
#[madsim::test]
async fn uploaded_commits_survive_zombie_writers() {
    crate::survives(ZOMBIES).await;
}

/// cladefs: [`RECREATED`]: zombies' late creates over the collector's passes, every block told
/// uploaded and not freed reads back as written. (local_cell::sim's
/// `stale_creates_landing_past_deletions` and `takeovers_racing_collection`.)
#[madsim::test]
async fn zombie_creates_landing_past_collection_keep_what_is_live() {
    crate::survives(RECREATED).await;
}

/// cladefs: [`FAST_ZOMBIES`]: every block told durable by a fast WAL reads back once healed, though
/// its writers were taken over with their appends in flight or held. (local_cell::sim's
/// `fast_wal_takeovers_fence_writers_in_flight` and `fast_wal_zombies_land_on_tombstones`.)
#[madsim::test]
async fn fast_wal_commits_survive_zombie_writers() {
    crate::survives(FAST_ZOMBIES).await;
}

// The rule searches local_cell::sim had of these profiles, each rule turned off until a seed loses
// what a guest was told.
//
// A writer acknowledging what it uploaded by swapping the head, with the condition that it is its own
// taking still (TASK-407): written whatever is there, a writer taken over writes it over its
// successor's, whose blocks told uploaded the head then names not.
needs!(takeovers_needs_head_swaps, head_swap, ZOMBIES);
// Keys naming their writers' epochs (TASK-407): two writers' segments of one number at one key.
needs!(takeovers_needs_epoch_keys, epoch_keys, ZOMBIES);
// Extents minted after a takeover, numbered past the epoch's (case 10).
needs!(takeovers_needs_epoch_sequences, epoch_sequences, ZOMBIES);
// The collector keeping what a live cell's head may yet name (TASK-407): deleted, a writer's swap
// names what is gone, a zombie's late create recreating it with other extents (TASK-79.18).
needs!(recreated_collection_needs_epochs, collector_epochs, RECREATED);

// specs/fast-wal's rules, each turned off: some seed loses a commit told durable, finds a large
// extent missing, or a take that never ends.
needs!(fast_wal_takes_needs_fences, wal_fences, FAST_ZOMBIES);
needs!(fast_wal_appends_needs_offsets, wal_offset_checked, FAST_ZOMBIES);
needs!(fast_wal_rolls_needs_their_markers, wal_rolls_before_next, FAST_ZOMBIES);
needs!(fast_wal_takes_needs_to_follow_rolls, wal_fence_follows_roll, FAST_ZOMBIES);
needs!(fast_wal_takes_needs_the_old_wal_named, wal_kept_on_take, FAST_ZOMBIES);
needs!(fast_wal_takes_needs_to_flush_first, wal_activates_after_flush, FAST_ZOMBIES);
needs!(fast_wal_batches_needs_their_large_extents, wal_after_large, FAST_ZOMBIES);
needs!(fast_wal_collectors_needs_the_wal_epoch, protect_wal_epoch, FAST_ZOMBIES);
// Of local_cell::sim's `fast_wals()`, without takeovers' hold: the fast WAL's own attrition.
needs!(fast_wal_segments_needs_room_for_fences, wal_rolls_before_limit, Profile::FAST_WAL);
needs!(fast_wal_reclaims_needs_acknowledgements, wal_reclaims_after_ack, Profile::FAST_WAL);
needs!(fast_wal_reclaims_needs_tombstones, wal_rolled_tombstones, Profile::FAST_WAL);
needs!(fast_wal_degraded_writers_needs_contiguous_batches, wal_contiguous, Profile::FAST_WAL);
// `wal_sweep_tombstones` (SweepDeletesFence) is `fast_wal::tests::sweeps_need_tombstones`'s in
// local_cell::sim: its case needs a retried create in flight across a whole take and its sweep, which
// no seed of 5,000 of `fast_wal_zombies()` reached. A held append, a BUGGIFY's, may reach it here.
needs!(fast_wal_sweeps_needs_tombstones, wal_sweep_tombstones, FAST_ZOMBIES);
