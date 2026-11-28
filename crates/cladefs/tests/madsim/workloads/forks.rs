// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: versions cut, dropped and forked racing each other (board 14567), ported from
//! local_cell::sim's `version`, `drop_version`, `restore_version`, `unfork` and `delete_base` steps
//! (profiles `forks`, `deleted_bases`, `restores`, `versions`, `children`). The writers cut the
//! versions (`Profile::versions`, as `version` did); beside them each round, through the VM
//! manager's control calls to real daemons, versions are forked on other hosts (`restore_version`),
//! two hosts fork one version at once, a version is dropped while it is forked
//! (`drop_version`), a host is killed as it forks, which its start finishes or deletes (`unfork`),
//! and a base whose forks live is deleted, as they are made (`delete_base`). Once healed, every
//! fork told made reads what its version held, and every fork of a base deleted what it inherited.
//!
//! Not ported: `twins`, two hosts' forks to one child ID, one refused (`IdTaken`): the control API
//! mints a fork's ID, a guest naming none, so that two daemons' forks never race for one.

use super::{Healed, Round};
use crate::{Blocks, Profile, SEARCHED, Told, block_of, control, guest, shuffled, survives};
use cladefs::Errno;
use cladefs::cladefsd::control::proto::cladefsd_client::CladefsdClient;
use cladefs::cladefsd::control::proto::{
    DeleteCellRequest, DropVersionsRequest, ForkCellRequest, SyncCellRequest,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A host's control API's client.
type Client = CladefsdClient<tonic::transport::Channel>;

/// The versions-cut-and-forked profile: the writers making versions, a base made, forked and
/// deleted each round by the attrition itself ([`Profile::bases`]), and this family's steps beside
/// each round's writer, the collector passing meanwhile and at the end.
pub(crate) const FORKS: Profile =
    Profile { forks: true, versions: true, bases: true, collector: true, ..Profile::PLAIN };

/// [`FORKS`], the writers freeing blocks too, which they relocate out of, and checkpoint past
/// their versions: what the versions' own rules (`versions_kept`, `versions_carried`,
/// `versions_acked`) need.
pub(crate) const FREED: Profile = Profile { frees: true, ..FORKS };

/// cladefs: the family's profile (board 14567): versions cut by the writers, forked on other
/// hosts, two forks of one at once, dropped as they are forked, hosts killed as they fork, and
/// bases deleted under forks being made, the collector passing meanwhile: every fork told made
/// reads what its version held, those of deleted bases what they inherited.
#[madsim::test]
async fn versions_cut_dropped_and_forked_keep_what_they_hold() {
    survives(FORKS).await;
}

/// cladefs: [`FORKS`], the writers freeing blocks as well ([`FREED`]).
#[madsim::test]
async fn versions_forked_among_frees_keep_what_they_hold() {
    survives(FREED).await;
}

// The rule searches this family's steps reach (old local_cell::sim's `restores`, `deleted_bases`
// and `versions` profiles' searches).
//
// A fork of a version reads its cell's head after it creates its own record: a version dropped
// first fails it, its pass of the collector not having seen the child.
needs!(forking_needs_versions_read_after_records, forks_check_versions, FORKS);
// A fork's record says it is being made until that read found the version named, so that what
// never was made holds up no pass.
needs!(forking_needs_records_saying_they_are_made, forks_pending, FORKS);
// The collector gives up on the forks being made of versions dropped.
needs!(collection_needs_abandoning_forks, collector_abandons, FORKS);
// A base's cell's pass, the base deleted, keeps what its forks inherited by their references...
needs!(deleted_bases_needs_references, references, FORKS);
// ...and its state at their origins whole, what recovering it reads too.
needs!(deleted_bases_needs_origins_kept, origins_kept, FORKS);
// A pass lists its cell's references with its candidates, missing a fork's made since. Linux's
// alone, as `forking_needs_references_relisted`.
#[cfg(not(target_os = "macos"))]
needs!(forking_needs_references_relisted_here, references_relisted, FORKS);
// A pass keeps, for each version a live cell's head names, the cell's whole state at it.
needs!(versions_needs_their_states_kept, versions_kept, FREED);
// A takeover's swap of a cell's head names the versions it named.
needs!(versions_needs_takeovers_to_carry_them, versions_carried, FREED);
// A writer's head names a version only with the segment cutting it.
needs!(versions_needs_naming_with_their_segments, versions_acked, FREED);
// No search of `clones_refer_first`, `references_held`, `clones_record_first` nor
// `retirements_marked`: local_cell::sim found none of the first two either (`clones_refer_first`
// by collector::tests' direct test, both by specs/bucket's mutations), and found the last two only
// as leaks (`test_leaks!`), which a check of what is left over once every cell is deleted finds,
// not this world's check of what was lost.

/// A fork told made of a version of the writers' cell: what its version held, which it reads.
struct Made {
    /// The host (0-based) its cell is on.
    host: usize,
    /// Its cell ID there.
    child: u64,
    /// The cell whose version it was forked from.
    cell: u64,
    /// The version.
    version: u64,
    /// The blocks told uploaded before the version was begun.
    held: Blocks,
}

/// A fork of a version of a base this family made, the base deleted as it was forked: what it
/// inherits.
struct Heir {
    /// The host (0-based) its cell is on.
    host: usize,
    /// Its cell ID there.
    child: u64,
    /// Its base's cell, whose extent it inherits.
    base: u64,
    /// The block that extent holds.
    block: Vec<u8>,
}

/// What a run's fork steps were told, kept in its [`Told`] (`Told::forks`).
#[derive(Default)]
pub(crate) struct Forks {
    /// The forks of versions of the writers' cell told made.
    made: Mutex<Vec<Made>>,
    /// The children of two forks of one version, made at once on two hosts, both told made.
    pairs: Mutex<Vec<(u64, u64)>>,
    /// The forks of bases, told made, the bases deleted as they were forked.
    heirs: Mutex<Vec<Heir>>,
}

/// How many times a call is tried while its host or the bucket is out of reach.
const ASKED: u32 = 25;

/// How long a call waits to try again.
const AGAIN: Duration = Duration::from_millis(200);

/// Calls `call` on host `host`'s control API until it answers otherwise than the bucket or a peer
/// being out of reach, as [`crate::calling`] does, but giving up after [`ASKED`] tries rather than
/// panicking: these calls run in tasks beside the round's, whose failing is no loss. What it
/// answered; `None` if it never did.
async fn ask<T, F>(host: u8, call: impl Fn(Client) -> F) -> Result<T, Option<Errno>>
where
    F: std::future::Future<Output = Result<T, tonic::Status>>,
{
    for _ in 0..ASKED {
        let daemon = match cladefs::cladefsd::control::connect(control(host)).await {
            Ok(daemon) => daemon,
            Err(err) => {
                log::info!("host {host}'s control API: {err:?}");
                madsim::time::sleep(AGAIN).await;
                continue;
            }
        };
        match call(daemon).await {
            Ok(done) => return Ok(done),
            Err(status) => match cladefs::cladefsd::control::errno(&status) {
                Some(errno) if !matches!(errno, Errno::ENOTCONN | Errno::EIO) => {
                    return Err(Some(errno));
                }
                errno => {
                    log::info!("host {host}: {errno:?} {}", status.message());
                    madsim::time::sleep(AGAIN).await;
                }
            },
        }
    }
    log::info!("host {host} never answered");
    Err(None)
}

/// Forks `version` of `cell` on host `host` (1-based), as the VM manager does: the child's ID.
async fn fork(host: u8, cell: u64, version: u64) -> Result<u64, Option<Errno>> {
    let request = ForkCellRequest { version, cell, ..ForkCellRequest::default() };
    ask(host, move |mut daemon| {
        let request = request.clone();
        async move { daemon.fork_cell(request).await.map(|forked| forked.into_inner().cell) }
    })
    .await
}

/// One of the versions `told` says were made and uploaded, as the seed says, with what it holds;
/// none if no writer made one yet.
fn pick(told: &Told) -> Option<(u64, Blocks)> {
    use madsim::rand::Rng as _;
    let versions = told.versions.lock().unwrap();
    if versions.is_empty() {
        return None;
    }
    let (version, held) = &versions[madsim::rand::thread_rng().gen_range(0..versions.len())];
    Some((*version, held.clone()))
}

/// As [`pick`], and no longer one `told` says was made: the attrition's own last check forks every
/// version in it, which a version about to be dropped is not to be.
fn take(told: &Told) -> Option<(u64, Blocks)> {
    use madsim::rand::Rng as _;
    let mut versions = told.versions.lock().unwrap();
    if versions.is_empty() {
        return None;
    }
    let at = madsim::rand::thread_rng().gen_range(0..versions.len());
    Some(versions.remove(at))
}

/// What a round's fork step does.
#[derive(Clone, Copy, Debug)]
enum Step {
    /// A version forked on another host (`restore_version`).
    Restore,
    /// One version forked on two other hosts at once (`restore_version`, "as many hosts do at
    /// once").
    Pair,
    /// A version dropped through the writer's host as it is forked on another (`drop_version`
    /// racing `restore_version`).
    Dropped,
    /// A version forked on another host, which is killed as it forks, and started again: its
    /// fork finished, or deleted, by its start (`unfork`).
    Killed,
    /// A base made on the writer's host, forked on two others as it is deleted (`delete_base`).
    Base,
}

/// The steps a round draws from.
const STEPS: [Step; 5] = [Step::Restore, Step::Pair, Step::Dropped, Step::Killed, Step::Base];

/// One of a round's fork steps, with what the seed drew for it.
struct Plan {
    /// The cell the writers write.
    cell: u64,
    /// The round.
    round: u64,
    /// The host (0-based) whose writer writes this round.
    writer: usize,
    /// The hosts' nodes, as first made.
    nodes: Vec<madsim::task::NodeId>,
    /// The hosts (0-based) it forks to, other than the writer's, in the order the seed gave.
    targets: Vec<usize>,
    /// What the writers were told.
    told: Arc<Told>,
    /// What the steps are told.
    forks: Arc<Forks>,
    /// How long after the step begins its host is killed, or its base deleted.
    after: Duration,
}

impl Plan {
    /// Does `step`.
    async fn run(self, step: Step) {
        match step {
            Step::Restore => self.restore().await,
            Step::Pair => self.pair().await,
            Step::Dropped => self.dropped().await,
            Step::Killed => self.killed().await,
            Step::Base => self.base().await,
        }
    }

    /// Forks `version`, which held `held`, on host `target` (0-based), noting the fork if it was
    /// told made.
    async fn fork(&self, target: usize, version: u64, held: &Blocks) -> Result<u64, Option<Errno>> {
        let made = fork(target as u8 + 1, self.cell, version).await;
        match &made {
            &Ok(child) => {
                let held = held.clone();
                let fork = Made { host: target, child, cell: self.cell, version, held };
                self.forks.made.lock().unwrap().push(fork);
            }
            Err(errno) => log::info!(
                "round {}: version {version} not forked on host {}: {errno:?}",
                self.round,
                target + 1
            ),
        }
        made
    }

    /// A version forked on another host.
    async fn restore(&self) {
        let Some((version, held)) = pick(&self.told) else { return };
        let _ = self.fork(self.targets[0], version, &held).await;
    }

    /// A version forked on two hosts at once: each told made gets a child of its own.
    async fn pair(&self) {
        let Some((version, held)) = pick(&self.told) else { return };
        let (one, other) = (self.targets[0], self.targets[1]);
        let (one, other) =
            futures::join!(self.fork(one, version, &held), self.fork(other, version, &held));
        if let (Ok(one), Ok(other)) = (one, other) {
            self.forks.pairs.lock().unwrap().push((one, other));
        }
    }

    /// A version dropped through the writer's host while it is forked on another: a fork told
    /// made of it holds what it held, whether the drop came before or after the fork read the
    /// head.
    async fn dropped(&self) {
        let Some((version, held)) = take(&self.told) else { return };
        let cell = self.cell;
        let dropping = ask(self.writer as u8 + 1, move |mut daemon| async move {
            let dropping = DropVersionsRequest { versions: vec![version], tenant: 0, cell };
            daemon.drop_versions(dropping).await.map(drop)
        });
        let forking = self.fork(self.targets[0], version, &held);
        let (forked, dropped) = futures::join!(forking, dropping);
        log::info!(
            "round {}: version {version} dropped as it was forked: {dropped:?}, {forked:?}",
            self.round
        );
    }

    /// A version forked on another host, killed as it forks, and started again a while after: the
    /// fork is finished or deleted by its host's start, and the fork asked for again, answered
    /// once the host is back up. The restart is a task of its own, which the heal doesn't abort:
    /// one aborted between a kill and its start leaves the host down for good.
    async fn killed(&self) {
        let Some((version, held)) = pick(&self.told) else { return };
        let (target, after) = (self.targets[0], self.after);
        let id = self.nodes[target];
        drop(madsim::task::spawn(async move {
            madsim::time::sleep(after).await;
            node_local::restart(id).await;
        }));
        let _ = self.fork(target, version, &held).await;
    }

    /// A base made on the writer's host, a block committed uploaded, a version made of it,
    /// uploaded; that version forked on two other hosts as the base is deleted, as a VM image is
    /// deleted under the VMs restored from it. Each fork told made is noted: it inherits the
    /// block, whether the delete came before or after its fork's record.
    async fn base(&self) {
        let source = self.writer;
        let block = block_of(3000 + self.round, 1);
        let written = block.clone();
        let node = madsim::runtime::Handle::current().get_node(self.nodes[source]).unwrap();
        let made = node.spawn(async move {
            let guest = guest().await;
            let cell = guest.open_new(0, 64 << 20).await.ok()?;
            let extent = cladefs::ExtentId { cell, sequence: 1 };
            guest.write(extent, 0, &written).await.ok()?;
            guest.commit(cladefs::Commit::Uploaded).await.ok()?;
            let version = guest.make_version().await.ok()?;
            guest.close();
            Some((cell, version))
        });
        let Ok(Some((base, version))) = made.await else {
            log::info!("round {}: a base not made", self.round);
            return;
        };
        let syncing = ask(source as u8 + 1, move |mut daemon| async move {
            daemon.sync_cell(SyncCellRequest { tenant: 0, cell: base }).await.map(drop)
        });
        if let Err(errno) = syncing.await {
            log::info!("round {}: base {base} not synced: {errno:?}", self.round);
            return;
        }
        let after = self.after.min(Duration::from_millis(100));
        let deleting = async move {
            madsim::time::sleep(after).await;
            ask(source as u8 + 1, move |mut daemon| async move {
                daemon.delete_cell(DeleteCellRequest { tenant: 0, cell: base }).await.map(drop)
            })
            .await
        };
        let (one, other) = (self.targets[0], self.targets[1]);
        let (one_forked, other_forked, deleted) = futures::join!(
            fork(one as u8 + 1, base, version),
            fork(other as u8 + 1, base, version),
            deleting
        );
        log::info!("round {}: base {base} deleted as it was forked: {deleted:?}", self.round);
        for (host, forked) in [(one, one_forked), (other, other_forked)] {
            match forked {
                Ok(child) => {
                    let heir = Heir { host, child, base, block: block.clone() };
                    self.forks.heirs.lock().unwrap().push(heir);
                }
                Err(errno) => log::info!(
                    "round {}: base {base}'s version not forked on host {}: {errno:?}",
                    self.round,
                    host + 1
                ),
            }
        }
    }
}

/// What this family does beside a round's writer: one or two steps, as the seed says, each some
/// time into the round, in tasks the heal aborts.
pub(crate) fn during(round: &Round<'_>) -> Vec<madsim::task::JoinHandle<()>> {
    use madsim::rand::Rng as _;
    assert!(round.nodes.len() >= 3, "fork steps fork to two hosts other than the writer's");
    let forks = round.told.forks.clone();
    let nodes: Vec<_> = round.nodes.iter().map(|node| node.id()).collect();
    let mut rng = madsim::rand::thread_rng();
    (0..rng.gen_range(1..=2))
        .map(|_| {
            let step = STEPS[rng.gen_range(0..STEPS.len())];
            let begun = Duration::from_millis(rng.gen_range(0..2500));
            let after = Duration::from_millis(rng.gen_range(0..300));
            let others: Vec<usize> = (0..nodes.len()).filter(|&host| host != round.host).collect();
            let plan = Plan {
                cell: round.cell,
                round: round.round,
                writer: round.host,
                nodes: nodes.clone(),
                targets: shuffled(others),
                told: round.told.clone(),
                forks: forks.clone(),
                after,
            };
            round.manager.spawn(async move {
                madsim::time::sleep(begun).await;
                plan.run(step).await;
            })
        })
        .collect()
}

/// The blocks `held` of `extent_cell`'s extents that the cell `opened`, which a guest on this host
/// opens, reads other than as written, by their sequences; or why it didn't open.
async fn read_blocks(opened: u64, extent_cell: u64, held: Blocks) -> Result<Vec<u64>, Errno> {
    let guest = guest().await;
    guest.open_cell(cladefs::CellId { tenant: 0, id: opened }).await?;
    let mut lost = Vec::new();
    for (&sequence, block) in &held {
        let extent = cladefs::ExtentId { cell: extent_cell, sequence };
        if guest.read(extent, 0, 4096).await.as_ref().ok() != Some(block) {
            lost.push(sequence);
        }
    }
    guest.close();
    Ok(lost)
}

/// How many times a read on a host is tried again, its node started anew meanwhile.
const READS: u32 = 100;

/// As [`read_blocks`], on host `host`'s node (0-based), again if the node is started anew
/// meanwhile, as one whose boot failed on a disk fault is (board 5721): its guest's task goes with
/// it, cancelled.
async fn read_on_host(
    nodes: &[madsim::runtime::NodeHandle],
    host: usize,
    opened: u64,
    extent_cell: u64,
    held: &Blocks,
) -> Result<Vec<u64>, Errno> {
    let handle = madsim::runtime::Handle::current();
    for _ in 0..READS {
        let held = held.clone();
        let node = handle.get_node(nodes[host].id()).unwrap();
        match node.spawn(read_blocks(opened, extent_cell, held)).await {
            Ok(read) => return read,
            Err(ended) if ended.is_cancelled() => {
                log::info!("host {}: started again, its guest's read with it", host + 1);
                madsim::time::sleep(AGAIN).await;
            }
            Err(ended) => panic!("host {}'s guest's read: {ended}", host + 1),
        }
    }
    panic!("host {} never read cell {opened}", host + 1)
}

/// What this family checks once healed, the collector's last passes done: two forks of one version
/// made at once have cells of their own; every fork told made reads what its version held, even
/// one whose version was dropped as it was forked; every fork of a base deleted reads the block it
/// inherited. A host whose disk was lost holds no fork told made (a fork is the host's own until
/// uploaded), so that profiles losing disks check the first only.
pub(crate) async fn healed(healed: &Healed<'_>) -> Result<(), String> {
    let forks = healed.told.forks.clone();
    for &(one, other) in forks.pairs.lock().unwrap().iter() {
        if one == other {
            return Err(format!("two forks of one version, at once, were both cell {one}"));
        }
    }
    if healed.profile.lose {
        eprintln!("host disks lost: the forks of versions and of bases are not read");
        return Ok(());
    }
    let made = std::mem::take(&mut *forks.made.lock().unwrap());
    let heirs = std::mem::take(&mut *forks.heirs.lock().unwrap());
    eprintln!(
        "forks of versions told made, each read: {}; forks of bases deleted: {}",
        made.len(),
        heirs.len()
    );
    for Made { host, child, cell, version, held } in &made {
        let sequences: Vec<u64> = held.keys().copied().collect();
        match read_on_host(healed.nodes, *host, *child, *cell, held).await {
            Ok(lost) if lost.is_empty() => {}
            Ok(lost) => {
                return Err(format!(
                    "the fork {child} of version {version} of cell {cell} lost blocks told \
                     uploaded before it: {lost:?} of {sequences:?}"
                ));
            }
            Err(errno) => {
                return Err(format!(
                    "the fork {child} of version {version} of cell {cell}, told made, did not \
                     open: {errno:?}"
                ));
            }
        }
    }
    for Heir { host, child, base, block } in &heirs {
        let held = Blocks::from([(1, block.clone())]);
        match read_on_host(healed.nodes, *host, *child, *base, &held).await {
            Ok(lost) if lost.is_empty() => {}
            Ok(_) => {
                return Err(format!(
                    "the fork {child} of base {base}, deleted as it was forked, read its block \
                     otherwise"
                ));
            }
            Err(errno) => {
                return Err(format!(
                    "the fork {child} of base {base}, deleted as it was forked, did not open: \
                     {errno:?}"
                ));
            }
        }
    }
    Ok(())
}
