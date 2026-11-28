// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: The collector killed midway through its passes (board 14568), ported from
//! local_cell::sim's `collect` and `stop_collector` steps and its `collectors`, `collected` and
//! `recovering` profiles. Each round has a collector of its own, a node that passes over the hosts'
//! bucket, a pass over one cell or a sweep over every cell, one after another, as the seed says;
//! killed, as the seed says, at any moment of a pass, its tasks dropped with their requests in
//! flight, as a collector's process dies. The collectors of rounds overlap when one is not killed
//! before the next begins, and with the profile's own (`Profile::collector`): a pass that takes a
//! cell fences the one before it, which deletes nothing more. Once healed, sweeps run until they
//! do nothing, and nothing is left that no live cell needs, nor gone that one does.
//!
//! Not ported, as the madsim world has no way to ask for them: the bucket's clock stepped by a
//! `tick` (it is madsim's own, which moves as the simulation sleeps), and `relocate`, a writer's
//! relocation round (doc-27), which cladefsd runs itself at a checkpoint, every 64 segments
//! (`Options::checkpoint_every`), where a writer's rounds here are a few commits of one block each.

use super::{Healed, Round};
use crate::{Blocks, Profile, SEARCHED};
use bucket_cell::collector::Collector;
use bucket_cell::layout::CellId;
use bucket_cell::object_bucket::RoutedBucket;
use cladefs::cladefsd::control::proto::SyncCellRequest;
use futures::StreamExt as _;
use madsim::rand::Rng as _;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

/// The holders of this family's passes begin here, above the attrition's own collector's (counting
/// from 1, and `u64::MAX - 1` and `u64::MAX` at the end) and `collect_until_quiet`'s (`1 << 40`):
/// each pass's differs from every other's, as a pass's must.
const HOLDERS: u64 = 2 << 40;

/// The last octet of the address of the node that makes the last sweeps once healed: the nodes of
/// the rounds' collectors have that of their round, from 1.
const HEALED_AT: u8 = 250;

/// The collector's node at `10.0.1.NUMBER`, which passes over `bucket` as `cladefs-gc` does, under
/// the hosts' KMS: its environment a host's, as far as the bucket goes ([`crate::collector`]'s, at
/// an address of its own, which each of this family's collectors needs).
pub(super) fn collector_node(
    bucket: &Arc<RoutedBucket>,
    number: u8,
) -> madsim::runtime::NodeHandle {
    let handle = madsim::runtime::Handle::current();
    let bucket = bucket.clone();
    let node = handle
        .create_node()
        .name(format!("collector {number}"))
        .ip(IpAddr::V4(Ipv4Addr::new(10, 0, 1, number)));
    node.init(move || {
        let bucket = bucket.clone();
        async move {
            // A host's token the daemon's bucket asks for, which no placement of the hosts' is.
            // And the hosts' KMS master key, which their tenants' KEKs are under ([`crate::master_key`]).
            let vars = [
                ("CLADEFSD_BUCKET", "memory:///cells".to_owned()),
                ("CLADEFSD_TOKEN", "1000".to_owned()),
                ("CLADEFS_MASTER_KEY", crate::master_key()),
            ];
            node_local::env::set_vars(vars);
            let daemon = cladefs::cladefsd::Daemon::new();
            daemon.use_bucket(bucket);
            std::future::pending::<()>().await;
        }
    })
    .build()
}

/// A cell for a pass over one cell: one the bucket lists, as the seed picks, or `own`, the
/// round's, which the listing may not have yet, or fail to reach.
async fn some_cell(bucket: &RoutedBucket, own: CellId) -> CellId {
    let mut cells = vec![own];
    let mut listed = std::pin::pin!(bucket_cell::cell::listed(bucket));
    while let Some(found) = listed.next().await {
        match found {
            Ok(cell) => cells.push(cell),
            Err(refused) => {
                log::info!("collector: listing the cells: {refused}");
                break;
            }
        }
    }
    let at = madsim::rand::thread_rng().gen_range(0..cells.len());
    cells[at]
}

/// The passes of one round's collector, one after another until its node is killed, or the heal
/// aborts it: each, as often as `one_cell` says, over one cell alone, else a sweep over every cell,
/// after a pause as the seed says, as a holder of its own (`HOLDERS`, `round`, `pass`). A pass that
/// fails, as the bucket's faults fail its requests, or leaves its cell to another, says so in the
/// log alone: the next one decides anew.
async fn passes(bucket: Arc<RoutedBucket>, cell: u64, round: u64, one_cell: f64) {
    for pass in 0..=u64::MAX {
        let pause = Duration::from_millis(madsim::rand::thread_rng().gen_range(0..300));
        madsim::time::sleep(pause).await;
        let holder = HOLDERS | (round << 20) | pass;
        let mut collector = Collector::new();
        if madsim::rand::thread_rng().gen_bool(one_cell) {
            let of = some_cell(&bucket, CellId { tenant: 0, id: cell }).await;
            let report = collector.collect_cell(&*bucket, of, holder).await;
            log::info!("collector {round}'s pass {pass} over {of:?}: {report:?}");
        } else {
            let report = collector.collect(&*bucket, holder).await;
            log::info!("collector {round}'s sweep {pass}: {report:?}");
        }
    }
}

/// What the collector killed midway does beside a round's writer: a collector of the round's own
/// (`passes`), on a node of its own, over one cell as often as the seed says; killed, as it says,
/// after a time drawn from 1 ms to 4 s by its power of two, so that some die in the first of a
/// pass's requests and some after many passes. The tasks that begin it and kill it, which the heal
/// aborts: one not killed passes until then, beside the next round's.
pub(crate) fn during(round: &Round<'_>) -> Vec<madsim::task::JoinHandle<()>> {
    let number = u8::try_from(round.round + 1)
        .ok()
        .filter(|&number| number < HEALED_AT)
        .expect("a round numbered below the last sweeps' node's address");
    let (one_cell, kills, dies_after) = {
        let mut rng = madsim::rand::thread_rng();
        let dies_after = Duration::from_micros(1u64 << rng.gen_range(10..22u32));
        (rng.gen_range(0.0..=1.0), rng.gen_bool(0.7), dies_after)
    };
    let node = collector_node(round.bucket, number);
    let id = node.id();
    let passing = node.spawn(passes(round.bucket.clone(), round.cell, round.round, one_cell));
    let mut tasks = vec![passing];
    if kills {
        let n = round.round;
        tasks.push(madsim::task::spawn(async move {
            madsim::time::sleep(dies_after).await;
            node_local::kill(id).await;
            log::info!("round {n}: its collector killed after {dies_after:?}");
        }));
    }
    tasks
}

/// What the collector killed midway checks once healed: sweeps of a collector of its own, one
/// after another until two in a row delete nothing, which finish what the killed passes left;
/// then, with every commit of the cell's host's uploaded, nothing left that no live cell needs
/// (`bucket_cell::testing::uncollected`: the space a pass frees, not only what it keeps, is
/// checked); and every block told uploaded and not freed reads as written, every one freed reads
/// not, and each version, forked, holds what came before it, as attrition's own check had it
/// before these sweeps. The leftover check is not made where zombie requests land late by design
/// (`Profile::zombies`, `Profile::stale`): what lands after the last sweeps is garbage no sweep
/// could have found.
pub(crate) async fn healed(healed: &Healed<'_>) -> Result<(), String> {
    let cell = healed.cell;
    // Nothing of the cell's host's in flight: what its uploader had behind, in the bucket.
    let synced = healed.manager.spawn(async move {
        crate::calling(1, || async move {
            let syncing = SyncCellRequest { tenant: 0, cell };
            crate::connect(1).await.sync_cell(syncing).await
        })
        .await
    });
    synced
        .await
        .unwrap()
        .map(drop)
        .map_err(|errno| format!("syncing cell {cell} before the last sweeps: {errno:?}"))?;
    let node = collector_node(healed.bucket, HEALED_AT);
    let bucket = healed.bucket.clone();
    let sweeping = async move { bucket_cell::testing::collect_until_quiet(&*bucket).await };
    node.spawn(sweeping)
        .await
        .unwrap()
        .map(drop)
        .map_err(|error| format!("the last sweeps of the collector, the bucket whole: {error}"))?;
    if !(healed.profile.zombies || healed.profile.stale) {
        let bucket = healed.bucket.clone();
        let left = node.spawn(async move { bucket_cell::testing::uncollected(&*bucket).await });
        let left = left.await.unwrap();
        if !left.is_empty() {
            return Err(format!("the collector's last sweeps left garbage: {left:?}"));
        }
    }
    // Read again, on the cell's host, as it is now: what the sweeps just made were not the last
    // the attrition's own checks saw.
    let handle = madsim::runtime::Handle::current();
    let host = || handle.get_node(healed.nodes[0].id()).unwrap();
    let uploaded = healed.told.uploaded.lock().unwrap().clone();
    let freed = healed.told.freed.lock().unwrap().clone();
    let sequences: Vec<u64> = uploaded.keys().copied().collect();
    let reading = || crate::lost_in(cell, cell, uploaded.clone(), freed.clone());
    let lost = crate::again_on(host, reading).await?;
    if !lost.is_empty() {
        return Err(format!(
            "after the collector's last sweeps, blocks told uploaded lost or other, or told freed \
             read back: {lost:?} of {sequences:?}"
        ));
    }
    if healed.profile.versions {
        let made = healed.told.versions.lock().unwrap().clone();
        for (version, before) in made {
            let forking = crate::fork_version(1, cell, version);
            let fork = healed.manager.spawn(forking).await.unwrap()?;
            let sequences: Vec<u64> = before.keys().copied().collect();
            let reading = || crate::lost_in(fork, cell, before.clone(), Blocks::new());
            let lost = crate::again_on(host, reading).await?;
            if !lost.is_empty() {
                return Err(format!(
                    "after the collector's last sweeps, version {version}'s fork lost blocks told \
                     uploaded before it: {lost:?} of {sequences:?}"
                ));
            }
        }
    }
    Ok(())
}

/// The collector killed midway: the profile of the collector's rules (`COLLECTED`: frees leaving
/// objects sparse, versions kept, passes meanwhile and at the end), and a collector of each round
/// killed at a moment of its pass, as the seed says, passes over one cell and sweeps overlapping
/// (local_cell::sim's `collectors`, `collected` and `recovering`).
pub(crate) const COLLECTOR_DIES: Profile = Profile { collector_dies: true, ..Profile::COLLECTED };

/// cladefs: the collector killed midway, restores of versions made and interrupted beside it
/// (`workloads::forks`): a record being made of a version dropped is given up on
/// (`collector_abandons`).
pub(crate) const COLLECTOR_DIES_FORKING: Profile = Profile { forks: true, ..COLLECTOR_DIES };

/// cladefs: the collector killed midway, a fast WAL cell taken over by hosts of either zone, their
/// disks lost at times: the collector keeps the large extents of the epoch the active fast WAL is
/// of, past a take's bump of the head's (`protect_wal_epoch`). No frees or versions, as
/// `Profile::FAST_WAL` has none.
pub(crate) const COLLECTOR_DIES_FAST_WAL: Profile =
    Profile { collector_dies: true, collector: true, ..Profile::FAST_WAL };

/// cladefs: doc-24, board 14568: the attrition profile, its writers freeing blocks and making
/// versions as they go, a collector of each round passing over the bucket, one cell or every cell,
/// killed midway as the seed says, the collectors of rounds racing each other and the profile's own
/// and fencing each other: every block told uploaded and not freed reads back as written, none
/// freed does, each version holds what came before it, and once healed the sweeps leave no
/// garbage.
#[madsim::test]
async fn collectors_killed_midway_keep_what_is_live() {
    crate::survives(COLLECTOR_DIES).await;
}

/// cladefs: board 14568: [`collectors_killed_midway_keep_what_is_live`], a fast WAL cell taken
/// over across zones, hosts losing their disks: what the collector deletes of a cell's objects,
/// killed midway or not, is no large extent the fast WAL names.
#[madsim::test]
async fn collectors_killed_midway_keep_what_the_fast_wal_names() {
    crate::survives(COLLECTOR_DIES_FAST_WAL).await;
}

// The collector deletes, of a live cell, objects of the head's epoch it may yet name (also
// `collection_needs_epochs`, over plain passes): with passes killed midway too.
needs!(dying_collectors_needs_epochs, collector_epochs, COLLECTOR_DIES);
// `candidates_first` has no search, as `collection_needs_epochs`' comment says: specs/bucket's
// NoCandidatesFirst shows it needed.
// A pass of the collector gives up on the restores of versions dropped, which a pass killed midway
// leaves for the next. Depends on `workloads::forks` making restores that a version's drop
// interrupts; not measured yet.
needs!(dying_collectors_needs_abandoning_restores, collector_abandons, COLLECTOR_DIES_FORKING);
// The collector keeps the large extents of the fast WAL's active epoch past a take's bump of the
// head's, until the taker flushes. Not measured yet (local_cell::sim's search needed zombie
// creates held to find it).
needs!(dying_collectors_needs_the_wal_epoch, protect_wal_epoch, COLLECTOR_DIES_FAST_WAL);
