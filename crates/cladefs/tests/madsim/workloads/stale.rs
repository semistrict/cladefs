// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: Writers and placements cut off running on unaware (board 14566), local_cell::sim's
//! `stale_checkpoint`, `stale_delete` and `reclaim` steps, done with real daemons: a host cut off
//! from the VM manager, or paused, while the cell is taken on another host at a later token, its
//! daemon running on unaware (its writer, if it has one, uploading with its old token); then a
//! stale VM manager (a node of its own, whose view is the earlier placement's) asks its control API
//! to sync the cell (a late checkpoint), to delete it (the delete is fenced by the token its copy
//! of the cell was placed at, which `DeleteCellRequest` does not carry: `delete_host_on`'s) or to
//! take it back with an old token. What must fail does, and what was told uploaded reads back
//! (attrition's own check) though the late checkpoints and uploads landed.

use super::{Healed, Round};
use crate::{Profile, SEARCHED, Told};
use cladefs::cladefsd::control::proto::{
    CreateCellRequest, DeleteCellRequest, DropCellRequest, StatCellRequest, SyncCellRequest,
};
use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// cladefs: the odds that a round has a host cut off from the VM manager and asked by a stale one.
const EPISODE_ODDS: f64 = 0.6;

/// cladefs: how long a host is cut off or paused at most, running on unaware.
const FROZEN_FOR: Duration = Duration::from_secs(4);

/// cladefs: how long the stale VM manager's ask may take: past it, the host stays cut off from the
/// VM manager no longer, whose takes wait for it ([`crate::take`] tries again for 40 s, and
/// [`crate::connect`] for 60).
const ASK_WITHIN: Duration = Duration::from_secs(10);

/// cladefs: what a run's episodes share, kept in its [`Told`] (`Told::stale`).
#[derive(Default)]
pub(crate) struct Episodes {
    /// The hosts (0-based) an episode has cut off from the VM manager now: no other cuts them.
    cut: Mutex<BTreeSet<usize>>,
}

/// cladefs: a host cut off from the VM manager (its link from there lost, so that no take of the
/// VM manager's lands on it meanwhile: its placement stays the earlier one) and, as drawn, paused
/// too. Ends as it is dropped: when its task ends, or is aborted at the heal.
struct Episode {
    told: Arc<Told>,
    sim: Arc<madsim::net::NetSim>,
    handle: madsim::runtime::Handle,
    manager: madsim::task::NodeId,
    /// The host, 0-based.
    host: usize,
    node: madsim::task::NodeId,
    paused: bool,
}

impl Episode {
    /// Cuts host `host`, whose node is `node`, off from `manager`, and pauses it if `paused`.
    fn begin(
        told: Arc<Told>,
        manager: madsim::task::NodeId,
        host: usize,
        node: madsim::task::NodeId,
        paused: bool,
    ) -> Self {
        let (sim, handle) = (madsim::net::NetSim::current(), madsim::runtime::Handle::current());
        sim.clog_link(manager, node);
        if paused {
            handle.pause(node);
        }
        Self { told, sim, handle, manager, host, node, paused }
    }

    /// Has the host run again, if it was paused: still cut off.
    fn thaw(&mut self) {
        if std::mem::take(&mut self.paused) {
            self.handle.resume(self.node);
        }
    }
}

impl Drop for Episode {
    fn drop(&mut self) {
        self.thaw();
        self.sim.unclog_link(self.manager, self.node);
        self.told.stale.cut.lock().unwrap().remove(&self.host);
    }
}

/// cladefs: what a stale VM manager asks of the host cut off.
#[derive(Clone, Copy, Debug)]
enum Ask {
    /// To sync the cell: its copy's late checkpoint, and the uploads its writer ran on with
    /// (`stale_checkpoint`).
    Sync,
    /// To delete the cell, as the placement its copy there was made at (`stale_delete`).
    Delete,
    /// To take the cell, at `token`, an earlier placement's than the cell's latest (`reclaim`).
    TakeBack { token: u64 },
}

/// cladefs: what a stale VM manager's ask of host `host` found: what a placement cut off did that
/// none may, if anything. An error is what is meant to come of it (or the bucket's faults, or a
/// connection open on the host's copy): logged.
async fn ask(host: u8, cell: u64, ask: Ask) -> Option<String> {
    let mut daemon = match cladefs::cladefsd::control::connect(crate::control(host)).await {
        Ok(daemon) => daemon,
        Err(err) => {
            log::info!(
                "host {host}, cut off: not connected to ask {ask:?} of cell {cell}: {err:?}"
            );
            return None;
        }
    };
    match ask {
        Ask::Sync => {
            let synced = daemon.sync_cell(SyncCellRequest { tenant: 0, cell }).await;
            log::info!("host {host}, cut off: synced cell {cell}: {:?}", synced.map(drop));
            None
        }
        Ask::Delete => match daemon.delete_cell(DeleteCellRequest { tenant: 0, cell }).await {
            Ok(_) => Some(format!("a placement cut off deleted cell {cell} on host {host}")),
            Err(status) => {
                log::info!("host {host}, cut off: deleting cell {cell}: {status}");
                None
            }
        },
        Ask::TakeBack { token } => {
            let taking = CreateCellRequest {
                capacity: 64 << 20,
                token: Some(token),
                cell,
                ..CreateCellRequest::default()
            };
            let mut taken = daemon.create_cell(taking.clone()).await;
            // Its copy of an earlier placement there: dropped first, as the VM manager's takes do
            // (`crate::take`), so that the bucket's fence is what refuses the take.
            if matches!(&taken, Err(status) if status.code() == tonic::Code::AlreadyExists) {
                if let Err(status) = daemon.drop_cell(DropCellRequest { tenant: 0, cell }).await {
                    log::info!("host {host}, cut off: dropping its copy of cell {cell}: {status}");
                    return None;
                }
                taken = daemon.create_cell(taking).await;
            }
            match taken {
                Ok(_) => Some(format!(
                    "a placement cut off took cell {cell} back on host {host} with token {token}"
                )),
                Err(status) => {
                    log::info!(
                        "host {host}, cut off: taking cell {cell} back at {token}: {status}"
                    );
                    None
                }
            }
        }
    }
}

/// cladefs: what it does beside a round's writer (`profile.stale`), as the seed says: a host that
/// is not the round's, which an earlier round's was (or the next round's), cut off from the VM
/// manager, and paused or not, a while, running on unaware of the take this round made; then a
/// stale VM manager, a node of its own, asks its control API to sync the cell, delete it or take it
/// back with an earlier token ([`Ask`]). The host is cut off from the VM manager alone, until the
/// ask is answered: none of the VM manager's takes land on it meanwhile, so that its copy's
/// placement is earlier than the cell's latest one when the ask lands (a delete by a copy placed
/// latest would succeed, and rightly). At most one episode cuts a host at a time.
pub(crate) fn during(round: &Round<'_>) -> Vec<madsim::task::JoinHandle<()>> {
    use madsim::rand::Rng as _;
    let mut rng = madsim::rand::thread_rng();
    let draws = (rng.gen_bool(EPISODE_ODDS), rng.gen_bool(0.75), rng.gen_bool(0.5));
    let frozen = Duration::from_millis(rng.gen_range(0..FROZEN_FOR.as_millis() as u64));
    let asking = match rng.gen_range(0..3) {
        0 => Ask::Sync,
        1 => Ask::Delete,
        // Below the token this round's take made, the latest so far (`crate::attrition`: `round +
        // 10`).
        _ => Ask::TakeBack { token: rng.gen_range(1..crate::token_of(round.round)) },
    };
    let (episode, earlier, paused) = draws;
    if !episode {
        return Vec::new();
    }
    let hosts = round.nodes.len();
    let victim = match earlier {
        true => (round.host + hosts - 1) % hosts,
        false => (round.host + 1) % hosts,
    };
    if !round.told.stale.cut.lock().unwrap().insert(victim) {
        return Vec::new();
    }
    let node = round.nodes[victim].id();
    let told = round.told.clone();
    let mut cut = Episode::begin(told.clone(), round.manager.id(), victim, node, paused);
    let caller = madsim::runtime::Handle::current()
        .create_node()
        .name(format!("stale caller {}", round.round))
        .ip(IpAddr::V4(Ipv4Addr::new(10, 0, 2, 1 + round.round as u8)))
        .build();
    let (host, cell) = (victim as u8 + 1, round.cell);
    let task = caller.spawn(async move {
        madsim::time::sleep(frozen).await;
        cut.thaw();
        match madsim::time::timeout(ASK_WITHIN, ask(host, cell, asking)).await {
            Ok(Some(lost)) => crate::violated(&told, lost),
            Ok(None) => {}
            Err(_) => log::info!("host {host}, cut off: {asking:?} of cell {cell} unanswered"),
        }
        // Mended, and running again, as it ends.
        drop(cut);
    });
    vec![task]
}

/// cladefs: what it checks once healed: that the cell lives, its latest placement's host answering
/// for it. What an ask found a placement cut off do that none may failed the run already
/// ([`crate::found`]).
pub(crate) async fn healed(healed: &Healed<'_>) -> Result<(), String> {
    let cell = healed.cell;
    let stat = healed.manager.spawn(async move {
        let stating = || async move {
            crate::connect(1).await.stat_cell(StatCellRequest { tenant: 0, cell }).await
        };
        crate::calling(1, stating).await
    });
    match stat.await.unwrap() {
        Ok(stat) if stat.get_ref().capacity == 64 << 20 => Ok(()),
        Ok(stat) => Err(format!(
            "cell {cell}, after placements cut off asked, has another capacity: {}",
            stat.get_ref().capacity
        )),
        Err(errno) => Err(format!("cell {cell}, after placements cut off asked: {errno:?}")),
    }
}

/// cladefs: writers and placements cut off, running on unaware, asked late by stale VM managers
/// (local_cell::sim's `stale_placements`).
pub(crate) const STALE: Profile = Profile { stale: true, ..Profile::PLAIN };

/// cladefs: [`STALE`], the collector passing meanwhile, deleting what a late checkpoint or upload
/// covers (local_cell::sim's `late_checkpoints`, TASK-175).
pub(crate) const LATE: Profile = Profile { stale: true, collector: true, ..Profile::PLAIN };

/// Local_cell::sim's `stale_placements_take_nothing_back`: placements cut off, asked late to sync,
/// delete and take back with their earlier tokens, while the writers go on and are taken over in
/// turn: every ask fails, and every block told uploaded reads back as written.
#[madsim::test]
async fn stale_placements_take_nothing_back() {
    crate::survives(STALE).await;
}

/// Local_cell::sim's `late_checkpoints_landing_past_takeovers`: writers cut off or paused, taken
/// over meanwhile, their checkpoints and uploads landing late, past where the next writer recovered
/// from, while the collector deletes what they cover.
#[madsim::test]
async fn late_checkpoints_landing_past_takeovers() {
    crate::survives(LATE).await;
}

// A placement cut off takes a cell back with an earlier token than the cell's latest placement's:
// the bucket's head has it, and its writer fenced by the earlier one.
needs!(fencing_needs_tokens, tokens, STALE);
// A placement cut off deletes a cell with the token it was placed at, earlier than the cell's
// latest placement's.
needs!(deletes_needs_tokens, delete_tokens, STALE);
