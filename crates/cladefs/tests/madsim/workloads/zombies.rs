// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: Zombie requests landing late, by BUGGIFY, and the takeovers they race (board 14565),
//! local_cell::sim's `open` (a takeover), `hold` and profiles `takeovers`, `recreated`,
//! `fast_wal_takeovers` and `fast_wal_zombies`, ported.
//!
//! A zombie is a writer taken over whose requests are in flight still: its create of a segment, its
//! swap of the head, its append to a fast WAL segment, which land after its successor's take, fence
//! or sweep. Three things make one here. The bucket holds a write a long while now and then before
//! it goes out (`bucket_cell::object_bucket`'s BUGGIFY sites, which local_cell::sim's `hold` made by
//! parking a taken-over writer's creates until a delete at their key: a segment's create has a site
//! of its own, held often); beside each round's writer, other hosts take its cell at the same
//! placement, at times the seed draws, most of them soon after the round begins, some of them again
//! ([`during`]), rather than only the next round's host once the writer has had its time, as the
//! attrition has it; and, of a fast WAL cell, a rival's host dies as it takes (its successor's
//! recovery stopped midway), and the fast buckets, and the cold one, go out of reach for stretches,
//! so that writers degrade and takes stall between their swap of the head and their flush
//! ([`degraded`]). What a zombie's request lands on is what the attrition's own check reads: every
//! block told uploaded, or told durable by a fast WAL, reads back once healed.

use super::{Healed, Round};
use crate::{Profile, SEARCHED, control, shuffled, token_of};
use bucket_cell::faults::{Odds, Seeded};
use cladefs::cladefsd::control::proto::{CreateCellRequest, DropCellRequest};
use madsim::rand::Rng as _;
use std::sync::Arc;
use std::time::Duration;

/// cladefs: the odds that a round has the cell taken by rivals of its writer's host.
const RIVALED: f64 = 0.85;

/// cladefs: the latest after a round's take that a rival takes the cell, a writer's first writes to
/// its done, as the attrition waits for it (up to 3 s) before the next round's take. Drawn by its
/// powers of two ([`soon`]), so that most rivals come within the writer's first requests.
const RIVAL_WITHIN: Duration = Duration::from_millis(2500);

/// cladefs: how often a rival tries to take the cell, a host's copy of an earlier round dropped
/// first, or the bucket refusing, before it gives up: a rival's take failing is no zombie, not a
/// loss.
const RIVAL_TRIES: usize = 5;

/// cladefs: the odds that a rival, having taken the cell, takes it again a while after, up to three
/// takes in all: each bumps the head's epoch again, past the writer's and the rival's own, whose
/// requests are in flight still (local_cell::sim's many `takeover` steps in a row).
const RIVAL_AGAIN: f64 = 0.3;

/// cladefs: the odds, of a fast WAL cell's round, that a rival's host dies as it takes the cell
/// (`wal_kept_on_take`, `wal_activates_after_flush`: a take stopped after its swap of the head,
/// before its flush), losing its disk now and then, and is started again a while after.
const CRASHED: f64 = 0.35;

/// cladefs: the odds, of a fast WAL cell's round, that its fast buckets go out of reach, or fail,
/// for a stretch ([`degraded`]).
const DEGRADED: f64 = 0.6;

/// cladefs: the odds that the cold bucket is troubled with them, its uploads and acknowledgements
/// behind and takes' flushes failing.
const COLD_TOO: f64 = 0.3;

/// cladefs: how a bucket fails in a stretch of trouble ([`degraded`]): a quarter of its requests
/// fail before or after they land, a quarter go out of reach, for a second and a half each, as a
/// zone's link does, long enough that a writer's append gives up on the fast bucket (degrades) and
/// a take's fence or flush fails midway.
const TROUBLED: Odds = Odds {
    fail_before: 0.25,
    fail_after: 0.25,
    out_of_reach: 0.25,
    out_for: Duration::from_millis(1500),
};

/// cladefs: a time up to just under `most`, drawn by its powers of two, as the seed says, so that
/// short ones are as likely as long ones: a rival in a writer's first request or in its last.
fn soon(most: Duration) -> Duration {
    let most = u64::try_from(most.as_millis()).unwrap().max(1);
    let scale = 1u64 << madsim::rand::thread_rng().gen_range(0..=most.ilog2());
    Duration::from_millis(madsim::rand::thread_rng().gen_range(0..=scale))
}

/// cladefs: what it does beside a round's writer: as the seed says, one or both of the other hosts
/// take the cell at the round's own placement ([`rivals`]), each after a time the seed draws, the
/// writer's host then a zombie whose requests, held by BUGGIFY or in flight, land past the take,
/// fence and sweep; and, of a fast WAL cell, its buckets troubled ([`degraded`]).
pub(crate) fn during(round: &Round<'_>) -> Vec<madsim::task::JoinHandle<()>> {
    // None in the first round, rivals nor troubles, so that its writer's commits are told durable
    // and the run checks something: the rest may fence every writer before it is told anything.
    if round.round == 0 {
        return Vec::new();
    }
    let mut tasks = rivals(round);
    if let Some(fast) = round.fast {
        tasks.extend(degraded(round, fast));
    }
    tasks
}

/// cladefs: the rivals of the round's writer: none, or one or both of the other hosts, taking its
/// cell as the seed says, one of them, of a fast WAL cell, killed as it does.
fn rivals(round: &Round<'_>) -> Vec<madsim::task::JoinHandle<()>> {
    if !madsim::rand::thread_rng().gen_bool(RIVALED) {
        return Vec::new();
    }
    let others: Vec<usize> =
        shuffled((0..round.nodes.len()).filter(|&host| host != round.host).collect());
    let rivals = madsim::rand::thread_rng().gen_range(1..=others.len());
    let token = token_of(round.round);
    others
        .into_iter()
        .take(rivals)
        .map(|host| {
            let after = soon(RIVAL_WITHIN);
            let takes = match madsim::rand::thread_rng().gen_bool(RIVAL_AGAIN) {
                true => madsim::rand::thread_rng().gen_range(2..=3),
                false => 1,
            };
            if round.profile.fast_wal && madsim::rand::thread_rng().gen_bool(CRASHED) {
                crash(round, host, after + soon(Duration::from_millis(800)));
            }
            round.manager.spawn(rival_take(host as u8 + 1, round.cell, token, after, takes))
        })
        .collect()
}

/// cladefs: host `host` (0-based) killed `after` a while, as its process dies, mid-take if `after`
/// falls in a rival's: its disk lost now and then, as the seed says, and started again up to two
/// seconds after. The task is its own, which the heal doesn't abort: one aborted between a kill
/// and its start leaves the host down for good (as `workloads::forks`' kills).
fn crash(round: &Round<'_>, host: usize, after: Duration) {
    let id = round.nodes[host].id();
    let loses = round.profile.lose && madsim::rand::thread_rng().gen_bool(0.5);
    let down = Duration::from_millis(madsim::rand::thread_rng().gen_range(0..2000));
    drop(madsim::task::spawn(async move {
        madsim::time::sleep(after).await;
        node_local::kill(id).await;
        if loses {
            log::info!("host {}: its disk lost, as it took a cell", host + 1);
            host_fs::lose(id);
        }
        madsim::time::sleep(down).await;
        node_local::restart(id).await;
    }));
}

/// cladefs: a stretch of trouble for a fast WAL cell's buckets, as the seed says: after a time, one
/// or both zones' fast buckets (and the cold one now and then) fail and go out of reach
/// ([`TROUBLED`]) for up to four seconds, then fail as every round's do again. A writer whose
/// append fails then degrades, its commits waiting for the cold bucket's head (`wal_contiguous`); a
/// take whose fence, flush or swap fails midway leaves a head naming what it left
/// (`wal_kept_on_take`, `wal_activates_after_flush`), and is stretched past the collector's passes
/// (`protect_wal_epoch`) and the uploads of large extents (`wal_after_large`). The heal aborts it
/// and sets every bucket whole.
fn degraded(round: &Round<'_>, fast: &crate::FastBuckets) -> Option<madsim::task::JoinHandle<()>> {
    if !madsim::rand::thread_rng().gen_bool(DEGRADED) {
        return None;
    }
    let mut troubled: Vec<Arc<Seeded>> = Vec::new();
    for faults in &fast.faults {
        if madsim::rand::thread_rng().gen_bool(0.5) {
            troubled.push(faults.clone());
        }
    }
    if troubled.is_empty() {
        let zone = madsim::rand::thread_rng().gen_range(0..fast.faults.len());
        troubled.push(fast.faults[zone].clone());
    }
    if madsim::rand::thread_rng().gen_bool(COLD_TOO) {
        troubled.push(round.faults.clone());
    }
    let after = soon(RIVAL_WITHIN);
    let stretch = Duration::from_millis(madsim::rand::thread_rng().gen_range(300..=4000));
    let round = round.round;
    Some(madsim::task::spawn(async move {
        madsim::time::sleep(after).await;
        log::info!("round {round}: {} buckets troubled for {stretch:?}", troubled.len());
        for faults in &troubled {
            faults.set(TROUBLED);
        }
        madsim::time::sleep(stretch).await;
        for faults in &troubled {
            faults.set(crate::ODDS);
        }
    }))
}

/// cladefs: host `host`'s daemon takes `cell` at placement `token`, as the VM manager asks of it,
/// `after` a while, `takes` times in all, the later ones a while after the one before: dropping its
/// copy of an earlier round first if it has one, and trying again a few times while the bucket
/// refuses. A refusal is logged and is not a failure: a placement cut off, or a bucket out of
/// reach, takes nothing, and the round's writer is no zombie then.
async fn rival_take(host: u8, cell: u64, token: u64, after: Duration, takes: usize) {
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
    for taken in 0..takes {
        if taken > 0 {
            madsim::time::sleep(soon(RIVAL_WITHIN)).await;
        }
        let mut done = false;
        for _ in 0..RIVAL_TRIES {
            match daemon.create_cell(taking.clone()).await {
                Ok(_) => {
                    log::info!("host {host}: a rival took cell {cell} at token {token}");
                    done = true;
                    break;
                }
                Err(status) if status.code() == tonic::Code::AlreadyExists => {
                    if let Err(err) = daemon.drop_cell(DropCellRequest { tenant: 0, cell }).await {
                        log::info!("host {host}: dropping cell {cell} for a rival: {err:?}");
                    }
                }
                Err(status) => {
                    log::info!("host {host}: a rival's take of cell {cell}: {status:?}");
                }
            }
            madsim::time::sleep(Duration::from_millis(200)).await;
        }
        if !done {
            return log::info!("host {host}: a rival took no cell {cell} at token {token}");
        }
    }
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
/// held, past the fence, the roll and the sweep, a rival's host dying as it takes, the fast
/// buckets failing and out of reach for stretches. (local_cell::sim's `fast_wal_takeovers` and
/// `fast_wal_zombies`.)
pub(crate) const FAST_ZOMBIES: Profile =
    Profile { zombies: true, fast_wal: true, lose: true, ..Profile::PLAIN };

/// cladefs: [`FAST_ZOMBIES`] with the collector's passes, often, over the cold bucket: one that
/// falls between a take's swap of the head, bumping its epoch, and its flush deletes the large
/// extents the fast WAL names of the epoch before (`protect_wal_epoch`).
pub(crate) const FAST_COLLECTED: Profile = Profile { collector: true, ..FAST_ZOMBIES };

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
/// its writers were taken over with their appends in flight or held, their takers' hosts dying
/// midway, and their buckets troubled. (local_cell::sim's
/// `fast_wal_takeovers_fence_writers_in_flight` and `fast_wal_zombies_land_on_tombstones`.)
#[madsim::test]
async fn fast_wal_commits_survive_zombie_writers() {
    crate::survives(FAST_ZOMBIES).await;
}

/// cladefs: [`FAST_COLLECTED`]: every block told durable by a fast WAL reads back once healed,
/// the collector passing over the cold bucket as its cell's takes stall.
#[madsim::test]
async fn fast_wal_commits_survive_zombie_writers_and_collection() {
    crate::survives(FAST_COLLECTED).await;
}

// The rule searches local_cell::sim had of these profiles, each rule turned off until a seed loses
// what a guest was told.
//
// A writer acknowledging what it uploaded by swapping the head, with the condition that it is its own
// taking still (TASK-407): written whatever is there, a writer taken over writes it over its
// successor's, whose blocks told uploaded the head then names not.
needs!(takeovers_needs_head_swaps, head_swap, ZOMBIES);
// Keys naming their writers' epochs (TASK-407): two writers' segments of one number at one key, a
// zombie's, held by the segment creates' own BUGGIFY site, landing before its successor's.
needs!(takeovers_needs_epoch_keys, epoch_keys, ZOMBIES);
// Extents minted after a takeover, numbered past the epoch's (case 10): rivals taking again and
// again, so that writers of many epochs mint past each other's.
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
// A take's swap of the head naming no fast WAL, the take stopped before its flush (its host dying,
// its buckets out of reach): the next take finds none to fence and recover.
needs!(fast_wal_takes_needs_the_old_wal_named, wal_kept_on_take, FAST_ZOMBIES);
needs!(fast_wal_takes_needs_to_flush_first, wal_activates_after_flush, FAST_ZOMBIES);
// A batch appended before its large extents landed in the cold bucket, a take recovering it between
// (a stretch of the cold bucket's trouble holding the uploads).
needs!(fast_wal_batches_needs_their_large_extents, wal_after_large, FAST_ZOMBIES);
// The collector's passes over a take stalled between its swap and its flush.
needs!(fast_wal_collectors_needs_the_wal_epoch, protect_wal_epoch, FAST_COLLECTED);
// Of local_cell::sim's `fast_wals()`, without takeovers' hold: the fast WAL's own attrition. Its
// segments' room for fences is the fast buckets' part limit, a BUGGIFY site of
// `bucket_cell::object_bucket` as S3 Express's MaxParts is, in a fifth of the seeds.
needs!(fast_wal_segments_needs_room_for_fences, wal_rolls_before_limit, Profile::FAST_WAL);
needs!(fast_wal_reclaims_needs_acknowledgements, wal_reclaims_after_ack, Profile::FAST_WAL);
needs!(fast_wal_reclaims_needs_tombstones, wal_rolled_tombstones, Profile::FAST_WAL);
// A writer's append failing, dropped, and a later batch's durability covering it: its fast bucket
// out of reach for a stretch ([`degraded`]), the commits of the batches after it in flight (a
// write that stages many commits before one waits, which `write_round` needs to make).
needs!(fast_wal_degraded_writers_needs_contiguous_batches, wal_contiguous, FAST_ZOMBIES);
// `wal_sweep_tombstones` (SweepDeletesFence) is `fast_wal::tests::sweeps_need_tombstones`'s in
// local_cell::sim: its case needs a retried create in flight across a whole take and its sweep, which
// no seed of 5,000 of `fast_wal_zombies()` reached. A held append, a BUGGIFY's, may reach it here.
needs!(fast_wal_sweeps_needs_tombstones, wal_sweep_tombstones, FAST_ZOMBIES);
