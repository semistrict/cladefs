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
//! Beyond those, for the rule searches (below) that the writers' own versions and one fork at a time
//! never reach, the family's profiles draw other steps ([`Mix`]): a cell of the family's own cut into
//! versions in a burst, a takeover of it racing the uploads that name them ([`Plan::cut`]); versions
//! dropped alone and forked long after, once the collector passed ([`Plan::drop_alone`],
//! [`Plan::late`]); and a fork's host killed as it records its fork, losing its disk, so that no start
//! finishes or deletes the fork ([`Plan::abandoned`]), its version dropped meanwhile. Once healed,
//! sweeps of the collector settle ([`settled`]) and each version of the family's own cells is checked.
//!
//! Not ported: `twins`, two hosts' forks to one child ID, one refused (`IdTaken`): the control API
//! mints a fork's ID, a guest naming none (`ForkCellRequest` has no child ID field), so that two
//! daemons' forks never race for one.

use super::{Healed, Round};
use crate::{
    Blocks, Profile, SEARCHED, Told, block_of, control, guest, shuffled, survives, token_of,
};
use cladefs::Errno;
use cladefs::cladefsd::control::proto::cladefsd_client::CladefsdClient;
use cladefs::cladefsd::control::proto::{
    CreateCellRequest, DeleteCellRequest, DropCellRequest, DropVersionsRequest, ForkCellRequest,
    SyncCellRequest,
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
/// their versions: what `versions_kept` needs. (`versions_carried` and `versions_acked` are
/// searched in [`CUTTING`]'s cells: the attrition's own check of the writers' versions panics at a
/// version it cannot fork, which is no finding.)
pub(crate) const FREED: Profile = Profile { frees: true, ..FORKS };

/// [`FREED`], no base made and deleted by the attrition itself, which [`Mix::of`] reads as: versions
/// dropped alone ([`Plan::drop_alone`]), and forked long after ([`Plan::late`]), once the collector
/// passed and deleted the extents the blocks freed since left it; and dropped as they are forked
/// ([`Plan::dropped`]), the forks' records held back by BUGGIFY past the collector's passes: so that
/// a fork that is not refused for its version dropped (`forks_check_versions`) reads what is gone.
pub(crate) const FORKED_LATE: Profile = Profile { bases: false, ..FREED };

/// [`FREED`], hosts losing their disks as they are killed ([`Profile::lose`], which [`Mix::of`] reads
/// as: forks whose hosts are killed as they record them, and lose their disks, so that no start finishes
/// or deletes them, their versions dropped meanwhile ([`Plan::abandoned`])): a record left being made of a
/// version dropped, the collector's passes to give up on it (`forks_pending`, `collector_abandons`).
/// The forks' reads are not made, as a profile losing disks has them not ([`healed`]).
pub(crate) const ABANDONING: Profile = Profile { lose: true, ..FREED };

/// The collector passing, the writers cutting no versions: the family cuts its own, in cells of its
/// own, which a takeover races ([`Plan::cut`]; what [`Mix::of`] reads a profile without versions as).
/// The writers cut none, so that the attrition's own check forks none: a version lost is this
/// family's finding, as an error, which the attrition's check, which panics at a fork refused, would
/// make no finding of (`versions_carried`, `versions_acked`).
pub(crate) const CUTTING: Profile = Profile { forks: true, collector: true, ..Profile::PLAIN };

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

/// cladefs: [`FORKED_LATE`]: versions dropped alone, forked long after, the collector passing between:
/// every fork told made reads what its version held, a version dropped being refused.
#[madsim::test]
async fn versions_dropped_alone_refuse_late_forks() {
    survives(FORKED_LATE).await;
}

/// cladefs: [`ABANDONING`]: forks of versions dropped, their hosts killed as they record them and
/// losing their disks: the collector's last sweeps settle, given up on what never was made.
#[madsim::test]
async fn forks_whose_hosts_died_are_given_up_on() {
    survives(ABANDONING).await;
}

/// cladefs: [`CUTTING`]: cells of the family's own cut into versions in bursts, taken over as their
/// uploads name them: every version told uploaded, and every version a head names, forks and reads what
/// came before it.
#[madsim::test]
async fn versions_cut_in_bursts_survive_takeovers() {
    survives(CUTTING).await;
}

// The rule searches this family's steps reach (old local_cell::sim's `restores`, `deleted_bases`
// and `versions` profiles' searches).
//
// A fork of a version reads its cell's head after it creates its own record: a version dropped
// first fails it. Measured over versions dropped alone and forked after the collector passed: with
// the rule off, the fork is made of what it deleted since the version was dropped, its extents among
// it (the writers' frees leave them garbage), and reads it not.
needs!(forking_needs_versions_read_after_records, forks_check_versions, FORKED_LATE);
// A fork's record says it is being made until that read found the version named, so that what
// never was made holds up no pass. A fork whose host died after its record, before its reference,
// and lost its disk, its version dropped, holds nothing; a record that says it is made is recovered
// by the next pass, and the state it names, no reference keeping it, is gone: a pass that fails, for
// good.
needs!(forking_needs_records_saying_they_are_made, forks_pending, ABANDONING);
// The collector gives up on the forks being made of versions dropped, as that pass finds one.
needs!(collection_needs_abandoning_forks, collector_abandons, ABANDONING);
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
// A takeover's swap of a cell's head names the versions it named: cells of the family's own, versions
// told uploaded, taken over, name none with the rule off (the attrition's own cell does the same,
// but its check panics at a version it cannot fork, which finds nothing: see `CUTTING`).
needs!(versions_needs_takeovers_to_carry_them, versions_carried, CUTTING);
// A writer's head names a version only with the segment cutting it: versions cut in a burst of local
// commits, one segment's acknowledgement naming those of later segments, a takeover or a fork
// between that and theirs finding the head naming a version at a segment it does not hold.
needs!(versions_needs_naming_with_their_segments, versions_acked, CUTTING);
// No search of `clones_refer_first`, `references_held`, `clones_record_first` nor
// `retirements_marked`: local_cell::sim found none of the first two either (`clones_refer_first`
// by collector::tests' direct test, both by specs/bucket's mutations), and found the last two only
// as leaks (`test_leaks!`), which a check of what is left over once every cell is deleted finds,
// not this world's check of what was lost.

/// A fork told made of a version of the writers' cell, or of a cell of the family's own: what its
/// version held, which it reads.
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

/// A version cut in a cell of the family's own ([`Plan::cut`]).
struct Cut {
    /// Its cell.
    cell: u64,
    /// The version.
    version: u64,
    /// The blocks written, and committed locally, before it was cut: what a fork of it holds.
    held: Blocks,
    /// Whether a commit told uploaded came after it, which names it in its cell's head: it is told
    /// made then, and every takeover after carries it.
    uploaded: bool,
}

/// What a run's fork steps were told, kept in its [`Told`] (`Told::forks`).
#[derive(Default)]
pub(crate) struct Forks {
    /// The forks of versions told made.
    made: Mutex<Vec<Made>>,
    /// The children of two forks of one version, made at once on two hosts, both told made.
    pairs: Mutex<Vec<(u64, u64)>>,
    /// The forks of bases, told made, the bases deleted as they were forked.
    heirs: Mutex<Vec<Heir>>,
    /// The versions cut in cells of the family's own, each cell made by [`Plan::cut`].
    cuts: Mutex<Vec<Cut>>,
    /// The versions of the writers' cell dropped, as `(cell, version, held)`, forked since or not.
    dropped: Mutex<Vec<(u64, u64, Blocks)>>,
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
    // A host started again answers once its daemon boots, which may take as long as the madsim
    // world gives any (`crate::STARTED_WITHIN`): its refusals meanwhile count against that, not
    // against the calls' tries.
    let booting = madsim::time::Instant::now();
    let mut tries = 0;
    while tries < ASKED {
        let daemon = match cladefs::cladefsd::control::connect(control(host)).await {
            Ok(daemon) => daemon,
            Err(err) if booting.elapsed() < crate::STARTED_WITHIN => {
                log::info!("host {host}'s control API: {err:?}");
                madsim::time::sleep(AGAIN).await;
                continue;
            }
            Err(err) => {
                log::info!("host {host}'s control API, past its boot: {err:?}");
                break;
            }
        };
        tries += 1;
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

/// Takes `cell` onto host `host` (1-based) at placement `token`, as the VM manager does, dropping
/// the host's copy of an earlier take first, and trying again while the bucket refuses, a few times:
/// whether it took it. A take that fails is no loss: the cell was not taken.
async fn take_over(host: u8, cell: u64, token: u64) -> bool {
    let taking = CreateCellRequest {
        capacity: 64 << 20,
        token: Some(token),
        cell,
        ..CreateCellRequest::default()
    };
    for _ in 0..ASKED {
        match cladefs::cladefsd::control::connect(control(host)).await {
            Err(err) => log::info!("host {host}'s control API: {err:?}"),
            Ok(mut daemon) => match daemon.create_cell(taking.clone()).await {
                Ok(_) => return true,
                Err(status) if status.code() == tonic::Code::AlreadyExists => {
                    if let Err(err) = daemon.drop_cell(DropCellRequest { tenant: 0, cell }).await {
                        log::info!("host {host}: dropping cell {cell} for a take: {err:?}");
                    }
                }
                Err(status) => log::info!("host {host}: taking cell {cell}: {status:?}"),
            },
        }
        madsim::time::sleep(AGAIN).await;
    }
    log::info!("host {host} took no cell {cell} at token {token}");
    false
}

/// One of the versions `told` says were made and uploaded, as the seed says, with what it holds,
/// and no longer one `told` says was made: the attrition's own last check forks every version in it,
/// which a version about to be dropped is not to be. None if no writer made one yet.
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
    /// A version dropped through the writer's host, and forked by none now: [`Step::Late`]'s.
    DropAlone,
    /// A version dropped by an earlier step forked on another host, the collector having passed
    /// since: refused, as a version no head names is (`forks_check_versions`).
    Late,
    /// A cell of the family's own, cut into versions in a burst on one host, taken over by another
    /// as its uploads name them (`version`, `takeover`).
    Cut,
    /// A version dropped as it is forked on another host, which is killed as it forks, loses its
    /// disk, and is started again: the fork, its record perhaps made and its reference not, is
    /// nobody's but the collector's (`forgotten_restores`).
    Abandoned,
}

/// What a profile's rounds draw their fork steps from, as its fields say (the family's profiles
/// differ in fields the family reads by their meaning, as each says: [`FORKED_LATE`], [`ABANDONING`],
/// [`CUTTING`]; the swarm's mixes get the one of their fields).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mix {
    /// [`FORKS`] and [`FREED`]: the writers' versions forked, dropped and killed, bases deleted.
    Whole,
    /// [`FORKED_LATE`]: versions dropped alone, and forked long after.
    Late,
    /// [`ABANDONING`]: forks whose hosts died and lost their disks as they recorded them.
    Abandoning,
    /// [`CUTTING`]: cells of the family's own cut into versions.
    Cutting,
}

impl Mix {
    /// The mix `profile` asks for: hosts losing their disks, else the writers cutting no versions,
    /// else no base made by the attrition itself.
    fn of(profile: &Profile) -> Self {
        match (profile.lose, profile.versions, profile.bases) {
            (true, ..) => Self::Abandoning,
            (false, false, _) => Self::Cutting,
            (false, true, false) => Self::Late,
            (false, true, true) => Self::Whole,
        }
    }

    /// The steps a round draws from, some of them more often than others.
    fn steps(self) -> &'static [Step] {
        match self {
            Self::Whole => &[Step::Restore, Step::Pair, Step::Dropped, Step::Killed, Step::Base],
            // Versions dropped as they are forked too, the forks' records held back by BUGGIFY past
            // the collector's passes (`bucket_cell::lifecycle::fork_version_keyed`).
            Self::Late => &[
                Step::DropAlone,
                Step::DropAlone,
                Step::Late,
                Step::Late,
                Step::Dropped,
                Step::Dropped,
                Step::Restore,
                Step::Killed,
            ],
            Self::Abandoning => &[
                Step::Abandoned,
                Step::Abandoned,
                Step::Abandoned,
                Step::DropAlone,
                Step::Late,
                Step::Restore,
                Step::Killed,
            ],
            Self::Cutting => {
                &[Step::Cut, Step::Cut, Step::Cut, Step::Restore, Step::Pair, Step::Killed]
            }
        }
    }
}

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
            Step::DropAlone => self.drop_alone().await,
            Step::Late => self.late().await,
            Step::Cut => self.cut().await,
            Step::Abandoned => self.abandoned().await,
        }
    }

    /// Forks `version` of `cell`, which held `held`, on host `target` (0-based), noting the fork if
    /// it was told made.
    async fn fork(
        &self,
        target: usize,
        cell: u64,
        version: u64,
        held: &Blocks,
    ) -> Result<u64, Option<Errno>> {
        let made = fork(target as u8 + 1, cell, version).await;
        match &made {
            &Ok(child) => {
                let held = held.clone();
                let fork = Made { host: target, child, cell, version, held };
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

    /// One of the versions the writers were told made and uploaded, or cut in a cell of the family's
    /// own, as the seed says: its cell, itself, and what it holds; none if there is none yet.
    fn pick(&self) -> Option<(u64, u64, Blocks)> {
        use madsim::rand::Rng as _;
        let told = self.told.versions.lock().unwrap();
        let cuts = self.forks.cuts.lock().unwrap();
        let total = told.len() + cuts.len();
        if total == 0 {
            return None;
        }
        let at = madsim::rand::thread_rng().gen_range(0..total);
        Some(match told.get(at) {
            Some((version, held)) => (self.cell, *version, held.clone()),
            None => {
                let cut = &cuts[at - told.len()];
                (cut.cell, cut.version, cut.held.clone())
            }
        })
    }

    /// Drops `version` of the writers' cell through the writer's host; or, once a later round took
    /// the cell elsewhere, its copy there stale or fenced, or gone, through the host that holds it
    /// now, as a VM manager asks the host it placed the cell on: the last host's refusal if none
    /// dropped it.
    async fn drop_version(&self, version: u64) -> Result<(), Option<Errno>> {
        let cell = self.cell;
        let hosts = self.nodes.len();
        let mut dropped = Err(None);
        for host in (0..hosts).map(|at| (self.writer + at) % hosts) {
            dropped = ask(host as u8 + 1, move |mut daemon| async move {
                let dropping = DropVersionsRequest { versions: vec![version], tenant: 0, cell };
                daemon.drop_versions(dropping).await.map(drop)
            })
            .await;
            if dropped.is_ok() {
                break;
            }
        }
        dropped
    }

    /// A version forked on another host.
    async fn restore(&self) {
        let Some((cell, version, held)) = self.pick() else { return };
        let _ = self.fork(self.targets[0], cell, version, &held).await;
    }

    /// A version forked on two hosts at once: each told made gets a child of its own.
    async fn pair(&self) {
        let Some((cell, version, held)) = self.pick() else { return };
        let (one, other) = (self.targets[0], self.targets[1]);
        let (one, other) = futures::join!(
            self.fork(one, cell, version, &held),
            self.fork(other, cell, version, &held)
        );
        if let (Ok(one), Ok(other)) = (one, other) {
            self.forks.pairs.lock().unwrap().push((one, other));
        }
    }

    /// A version dropped through the writer's host while it is forked on another: a fork told
    /// made of it holds what it held, whether the drop came before or after the fork read the
    /// head.
    async fn dropped(&self) {
        let Some((version, held)) = take(&self.told) else { return };
        self.forks.dropped.lock().unwrap().push((self.cell, version, held.clone()));
        let dropping = self.drop_version(version);
        let forking = self.fork(self.targets[0], self.cell, version, &held);
        let (forked, dropped) = futures::join!(forking, dropping);
        log::info!(
            "round {}: version {version} dropped as it was forked: {dropped:?}, {forked:?}",
            self.round
        );
    }

    /// A version dropped through the writer's host, and forked by no step now. The writers go on
    /// freeing the blocks it held, and the collector's passes delete what only it kept: a fork of it
    /// made after ([`Plan::late`]) finds that gone, unless its head's read refuses it.
    async fn drop_alone(&self) {
        let Some((version, held)) = take(&self.told) else { return };
        self.forks.dropped.lock().unwrap().push((self.cell, version, held));
        let dropped = self.drop_version(version).await;
        log::info!("round {}: version {version} dropped: {dropped:?}", self.round);
    }

    /// A version an earlier step dropped, forked on another host. Refused, for the version is named
    /// no more; one that is made, told made, holds what the version did, which the check reads.
    async fn late(&self) {
        use madsim::rand::Rng as _;
        let dropped = {
            let dropped = self.forks.dropped.lock().unwrap();
            if dropped.is_empty() {
                return;
            }
            dropped[madsim::rand::thread_rng().gen_range(0..dropped.len())].clone()
        };
        let (cell, version, held) = dropped;
        let _ = self.fork(self.targets[0], cell, version, &held).await;
    }

    /// A version forked on another host, killed as it forks, and started again a while after: the
    /// fork is finished or deleted by its host's start, and the fork asked for again, answered
    /// once the host is back up. The restart is a task of its own, which the heal doesn't abort:
    /// one aborted between a kill and its start leaves the host down for good.
    async fn killed(&self) {
        let Some((cell, version, held)) = self.pick() else { return };
        let (target, after) = (self.targets[0], self.after);
        let id = self.nodes[target];
        drop(madsim::task::spawn(async move {
            madsim::time::sleep(after).await;
            node_local::restart(id).await;
        }));
        let _ = self.fork(target, cell, version, &held).await;
    }

    /// A version dropped through the writer's host while it is forked on another, whose host is
    /// killed a little after the fork begins, anywhere in its few requests (1 ms to 260 ms, by the
    /// seed's power of two), loses its disk, as a host replaced does, and is started again: no
    /// directory of its own remembers the fork, so no start finishes or deletes it. If it was killed
    /// after its record and before its reference, the record is all there is of it, the version
    /// dropped and nothing keeping what it names: the collector's alone to give up on
    /// (`forks_pending`, `collector_abandons`). The fork asked for again, answered once the host is up,
    /// is another fork, with a child of its own.
    async fn abandoned(&self) {
        use madsim::rand::Rng as _;
        let Some((version, held)) = take(&self.told) else { return };
        self.forks.dropped.lock().unwrap().push((self.cell, version, held.clone()));
        let (target, id) = (self.targets[0], self.nodes[self.targets[0]]);
        let (dies, back, drops) = {
            let mut rng = madsim::rand::thread_rng();
            let dies = Duration::from_micros(1u64 << rng.gen_range(10..19u32));
            (dies, Duration::from_millis(rng.gen_range(200..1500)), rng.gen_range(0..300))
        };
        drop(madsim::task::spawn(async move {
            madsim::time::sleep(dies).await;
            node_local::kill(id).await;
            host_fs::lose(id);
            log::info!("host {}: killed as it forked, its disk lost", target + 1);
            madsim::time::sleep(back).await;
            node_local::restart(id).await;
        }));
        let dropping = async {
            madsim::time::sleep(Duration::from_millis(drops)).await;
            self.drop_version(version).await
        };
        let forking = self.fork(target, self.cell, version, &held);
        let (forked, dropped) = futures::join!(forking, dropping);
        log::info!(
            "round {}: version {version} dropped as its fork's host died: {dropped:?}, {forked:?}",
            self.round
        );
    }

    /// A cell of the family's own, made on a host other than the writer's, written and cut into
    /// versions in a burst ([`burst`]) while another host takes it over, a time after, as the seed
    /// says, by a power of two from 1 ms to 2 s: before the first version is named in the cell's head,
    /// between one segment's acknowledgement and another's, or once all are, so that the takeover
    /// must carry them (`versions_carried`, `versions_acked`). The versions are checked once healed
    /// ([`settled`]).
    async fn cut(&self) {
        use madsim::rand::Rng as _;
        let (source, taker) = (self.targets[0], self.targets[1]);
        let (made, cell) = futures::channel::oneshot::channel();
        let node = madsim::runtime::Handle::current().get_node(self.nodes[source]).unwrap();
        let cutting = node.spawn(burst(self.round, self.forks.clone(), made));
        let token = token_of(self.round) + 100;
        let after = Duration::from_micros(1u64 << madsim::rand::thread_rng().gen_range(10..21u32));
        let taking = async move {
            let Ok(cell) = cell.await else { return };
            madsim::time::sleep(after).await;
            let took = take_over(taker as u8 + 1, cell, token).await;
            log::info!("host {}: took cell {cell} at {token}: {took}", taker + 1);
        };
        let (cut, ()) = futures::join!(cutting, taking);
        if let Err(ended) = cut {
            log::info!("round {}: host {}'s burst ended: {ended}", self.round, source + 1);
        }
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

/// On this host, a cell of the family's own made and cut into versions in a burst, as a VM's
/// snapshots are: a few blocks, each written and committed locally, not waiting for the bucket, one or
/// two versions cut after each, back to back, so that several segments are cut before the uploader
/// acknowledges the first, a pause between, as the seed says, and now and then a commit told
/// uploaded, which names the versions cut so far in the cell's head. Each version is noted as it is
/// cut, told made once a commit told uploaded came after it. Its cell's ID is sent as soon as it is
/// made, for a takeover to race the rest. The guest stays open a while after, as a VM's does.
async fn burst(round: u64, forks: Arc<Forks>, made: futures::channel::oneshot::Sender<u64>) {
    use madsim::rand::Rng as _;
    let guest = guest().await;
    let Ok(cell) = guest.open_new(0, 64 << 20).await else { return };
    let _ = made.send(cell);
    let Ok(first) = guest.next_sequence().await else { return };
    let blocks = madsim::rand::thread_rng().gen_range(2..=5u64);
    let mut held = Blocks::new();
    // The versions this burst cut, by their places in `forks.cuts`.
    let mut cut: Vec<usize> = Vec::new();
    for sequence in first..first + blocks {
        let block = block_of(4000 + round, sequence);
        let extent = cladefs::ExtentId { cell, sequence };
        if guest.write(extent, 0, &block).await.is_err() {
            break;
        }
        if guest.commit(cladefs::Commit::Local).await.is_err() {
            break;
        }
        held.insert(sequence, block);
        let versions = madsim::rand::thread_rng().gen_range(1..=2);
        for _ in 0..versions {
            match guest.make_version().await {
                Ok(version) => {
                    let mut cuts = forks.cuts.lock().unwrap();
                    cuts.push(Cut { cell, version, held: held.clone(), uploaded: false });
                    cut.push(cuts.len() - 1);
                }
                Err(errno) => log::info!("round {round}: a version refused: {errno:?}"),
            }
        }
        if madsim::rand::thread_rng().gen_bool(0.3) {
            uploaded(&guest, &forks, &cut).await;
        }
        let pause = Duration::from_millis(madsim::rand::thread_rng().gen_range(0..200));
        madsim::time::sleep(pause).await;
    }
    if madsim::rand::thread_rng().gen_bool(0.5) {
        uploaded(&guest, &forks, &cut).await;
    }
    let lingers = Duration::from_micros(1u64 << madsim::rand::thread_rng().gen_range(10..21u32));
    madsim::time::sleep(lingers).await;
    guest.close();
}

/// Commits `guest`'s cell told uploaded, and notes the versions `cut` names, those cut before it
/// began, told made if it was told done.
async fn uploaded(guest: &cladefs::cladefsd::guest::Guest, forks: &Forks, cut: &[usize]) {
    if guest.commit(cladefs::Commit::Uploaded).await.is_err() {
        return;
    }
    let mut cuts = forks.cuts.lock().unwrap();
    for &at in cut {
        if let Some(cut) = cuts.get_mut(at) {
            cut.uploaded = true;
        }
    }
}

/// What this family does beside a round's writer: one or two steps (up to three, of a profile
/// losing disks), as the seed says, each some time into the round, in tasks the heal aborts.
pub(crate) fn during(round: &Round<'_>) -> Vec<madsim::task::JoinHandle<()>> {
    use madsim::rand::Rng as _;
    assert!(round.nodes.len() >= 3, "fork steps fork to two hosts other than the writer's");
    let forks = round.told.forks.clone();
    let nodes: Vec<_> = round.nodes.iter().map(|node| node.id()).collect();
    let (mix, mut rng) = (Mix::of(round.profile), madsim::rand::thread_rng());
    let steps = mix.steps();
    let most = if mix == Mix::Abandoning { 3 } else { 2 };
    (0..rng.gen_range(1..=most))
        .map(|_| {
            let step = steps[rng.gen_range(0..steps.len())];
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

/// What this family checks of the buckets whole again, beyond the forks' reads, when its profile asks
/// for more than the writers' own versions ([`Mix`]):
///
/// - sweeps of the collector, one after another until two in a row delete nothing, which fail as a
///   pass fails: a record of a fork that never was made, which nothing gave up on, names what is
///   gone, and no pass recovers it (`forks_pending`, `collector_abandons`);
/// - each version cut in a cell of the family's own ([`Plan::cut`]): one told made, by a commit told
///   uploaded after it, is named in its cell's head, which every takeover carries
///   (`versions_carried`); and one the head names, told or not, forks and reads what was written
///   before it, as a head names a version only at a segment it holds (`versions_acked`).
async fn settled(healed: &Healed<'_>, forks: &Forks) -> Result<(), String> {
    // A collector's node of its own, numbered past the collector family's rounds' (`collector_node`).
    let node = super::collector::collector_node(healed.bucket, 200);
    if healed.profile.collector {
        let bucket = healed.bucket.clone();
        let sweeping = async move { bucket_cell::testing::collect_until_quiet(&*bucket).await };
        node.spawn(sweeping).await.unwrap().map(drop).map_err(|error| {
            format!("the last sweeps of the collector, the bucket whole, failed: {error}")
        })?;
    }
    let cuts = std::mem::take(&mut *forks.cuts.lock().unwrap());
    eprintln!("versions cut in the family's own cells, each checked: {}", cuts.len());
    for Cut { cell, version, held, uploaded } in cuts {
        let bucket = healed.bucket.clone();
        let id = bucket_cell::layout::CellId { tenant: 0, id: cell };
        let reading = async move { bucket_cell::head::get(&*bucket, id).await };
        let head = node
            .spawn(reading)
            .await
            .unwrap()
            .map_err(|error| format!("cell {cell}'s head, the bucket whole: {error}"))?;
        let named = head.is_some_and(|head| head.version(version).is_some());
        if !named {
            if uploaded {
                return Err(format!(
                    "version {version} of cell {cell}, told made by a commit told uploaded after \
                     it, is named by its cell's head no more"
                ));
            }
            continue;
        }
        // From the VM manager's node, as every call of the hosts' control APIs: the simulation's
        // own node is on no network.
        let forking = healed.manager.spawn(fork(1, cell, version));
        let child = forking.await.unwrap().map_err(|errno| {
            format!(
                "version {version} of cell {cell}, which its head names, was not forked: {errno:?}"
            )
        })?;
        match read_on_host(healed.nodes, 0, child, cell, &held).await {
            Ok(lost) if lost.is_empty() => {}
            Ok(lost) => {
                return Err(format!(
                    "the fork {child} of version {version} of cell {cell}, which its head names, \
                     lost blocks written before it: {lost:?} of {:?}",
                    held.keys().collect::<Vec<_>>()
                ));
            }
            Err(errno) => {
                return Err(format!(
                    "the fork {child} of version {version} of cell {cell}, which its head names, \
                     did not open: {errno:?}"
                ));
            }
        }
    }
    Ok(())
}

/// What this family checks once healed, the collector's last passes done: two forks of one version
/// made at once have cells of their own; the collector's sweeps settle and the family's own versions
/// are as their heads name them ([`settled`], where the profile asks for more than [`FORKS`]'s steps);
/// every fork told made reads what its version held, even one whose version was dropped as it was
/// forked; every fork of a base deleted reads the block it inherited. A host whose disk was lost holds
/// no fork told made (a fork is the host's own until uploaded), so that profiles losing disks check
/// the first, and the sweeps and versions, only.
pub(crate) async fn healed(healed: &Healed<'_>) -> Result<(), String> {
    let forks = healed.told.forks.clone();
    for &(one, other) in forks.pairs.lock().unwrap().iter() {
        if one == other {
            return Err(format!("two forks of one version, at once, were both cell {one}"));
        }
    }
    if Mix::of(healed.profile) != Mix::Whole {
        settled(healed, &forks).await?;
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
