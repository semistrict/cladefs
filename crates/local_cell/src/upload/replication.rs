// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: replicated-cell quorum, fencing, recovery, and peer reconfiguration orchestration
//! (doc-21, TASK-537.3.3–TASK-537.3.6, TASK-678, TASK-681, TASK-683).
//!
//! Separated from segment uploading, checkpointing, and relocation scheduling in `upload.rs`.

use super::{Awaited, Failure, Shared, Taking, Uploader, Waiting, blocking};
use crate::{Dir, Recovering};
use bucket_cell::Writer;
use bucket_cell::keys::Ring;
use bucket_cell::layout::{
    CellId, Configuration, Fsync, Head, Member, Replication, Wish, epoch_of,
};
use futures::future::{BoxFuture, FutureExt as _};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// cladefs: how long a take waits for the members of its new configuration to hold what the one
/// before held, before it fails (doc-21, TASK-537.3.4).
const SEEDING: Duration = Duration::from_secs(60);

/// cladefs: how often a replicated cell's configuration is looked at ([`Shared::keep`]).
const KEEPING: Duration = Duration::from_millis(250);

/// cladefs: how many of [`KEEPING`]'s ticks a keeper waits, after a replacement it had no members
/// for, before it tries again: from 1 s, doubling, to 30 s. Counted in ticks, not by the host's
/// clock, so that the simulation's scheduler, which says when each tick is, decides it (TASK-680).
const BACKOFF_FIRST: u32 = 4;
const BACKOFF_MOST: u32 = 120;

/// cladefs: how many of a replacement's waits for its members, of 10 ms, between its urgings of
/// the uploader, a replacement to f = 0 waiting for the bucket: one [`KEEPING`]'s worth.
const URGE_EVERY: u32 = 25;

/// cladefs: what a host knows of the hosts its replicated cells' configurations are made of
/// (doc-21, TASK-537.3.3), which the daemon's peer network is. What asks the members is a future,
/// which holds no thread as it waits for them (doc-24 step 10).
pub trait Members: Send + Sync + 'static {
    /// This host as a member: its peer address, and its replica directory's incarnation.
    fn me(&self) -> Result<Member, String>;
    /// 2f members for configuration `number` of `cell`, this host aside, and those at `avoid`,
    /// each that answered its incarnation.
    fn pick<'a>(
        &'a self,
        cell: CellId,
        number: u64,
        f: u8,
        avoid: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<Member>, String>>;
    /// A link to `member` of `cell`'s configuration `configuration`.
    fn link(
        &self,
        cell: CellId,
        configuration: &Configuration,
        member: &Member,
    ) -> Arc<dyn crate::replicate::MemberLink>;
    /// Fences `cell` at `epoch` on `member` of `configuration`: the ends of the logs it holds of
    /// it, by their numbers, synced, once its promise is (TASK-537.3.4).
    fn fence<'a>(
        &'a self,
        cell: CellId,
        configuration: &'a Configuration,
        member: &'a Member,
        epoch: u64,
    ) -> BoxFuture<'a, Result<Vec<(u64, u64)>, String>>;
    /// Log `log` that `member` holds of `configuration`, from its start to `end`, where its fence
    /// said it ends: no further, whatever more the member sends; failing if the member stalls
    /// (board 5416).
    fn read<'a>(
        &'a self,
        cell: CellId,
        configuration: &'a Configuration,
        member: &'a Member,
        log: u64,
        end: u64,
    ) -> BoxFuture<'a, Result<Vec<u8>, String>>;
    /// Has `member` drop what it holds of `cell`'s configurations before `configuration`.
    fn drop_before<'a>(
        &'a self,
        cell: CellId,
        configuration: &'a Configuration,
        member: &'a Member,
    ) -> BoxFuture<'a, Result<(), String>>;
    /// Whether these members drive `replicator`, a replicator just made of a cell of this host's
    /// that no task serves yet ([`crate::replicate::Replicator::idle`]): a turn at a time, as the
    /// simulation's scheduler does (TASK-680), keeping it to. Else each of its members is served by
    /// a task of its own, as the daemon's are.
    fn drives(&self, _: &Arc<crate::replicate::Replicator>) -> bool {
        false
    }
    /// cladefs: whether the host at `address` is among those this host's configurations are picked
    /// from now (board 8579): a member the host's peers name no more, `SetPeers` having removed it,
    /// is replaced as one lost is, at once, while it serves still. Every host is, unless these
    /// members say otherwise.
    fn is_peer(&self, _address: &str) -> bool {
        true
    }
}

/// cladefs: a host's [`Members`], as [`super::Options`] holds it.
#[derive(Clone)]
pub struct MembersOf(pub Arc<dyn Members>);

impl std::fmt::Debug for MembersOf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Members")
    }
}

/// cladefs: a replicated cell's replication, as a writer's taking found it: its f, what its
/// guest's fsync waits for, and the number of its active configuration, if it has one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Replicated {
    pub f: u8,
    pub fsync: Fsync,
    pub configuration: Option<u64>,
    /// cladefs: if the taking was forced and fenced fewer members than a take needs (TASK-681): the
    /// epoch of the last commit the bucket held, past which commits told replicated may be lost.
    pub forced_after: Option<u64>,
}

impl Replicated {
    pub(super) fn of(replication: &Replication) -> Self {
        let Replication { wish, configuration } = replication;
        Self {
            f: wish.f,
            fsync: wish.fsync,
            configuration: configuration.as_ref().map(|configuration| configuration.number),
            forced_after: None,
        }
    }
}

/// cladefs: a replicated cell's writer's replication, as it is now (doc-21, Degraded;
/// TASK-537.3.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplicationStatus {
    /// The f of its active configuration: less than its head asks for once it dropped to f = 0.
    pub f: u8,
    /// How many members of its active configuration, the writer aside, take what it sends.
    pub members_up: u32,
    /// Whether fewer than f of them do, or its configuration is of f = 0: its commits waiting for
    /// the bucket, uploaded at once.
    pub degraded: bool,
    /// Whether a replacement of its configuration is under way.
    pub replacing: bool,
}

/// cladefs: the members of the configuration a take found that it fenced, each with the ends of the
/// logs it holds ([`Shared::fence`]); and, of a forced take (TASK-681), why they are fewer than a
/// take needs, if they are.
pub(super) struct Fenced {
    pub(super) members: Vec<(Member, Vec<(u64, u64)>)>,
    pub(super) short: Option<String>,
}

/// cladefs: what fencing the configuration a take found returned, carried into recovery once the
/// writer holds the cell.
pub(super) struct FencedTake {
    pub(super) named: Option<Configuration>,
    pub(super) replicated: bool,
    pub(super) old: Option<Configuration>,
    pub(super) fenced: Option<Fenced>,
}

/// cladefs: what recovering a replicated cell's logs produced, carried into starting its
/// replicator once the cell is open.
pub(super) struct ReplicatedTake {
    pub(super) old: Option<Configuration>,
    pub(super) seed: Vec<(u64, Vec<u8>)>,
    pub(super) seeding: crate::replicate::Holding,
    pub(super) short: Option<String>,
    pub(super) forced_after: Option<u64>,
}

impl<D: Dir> Uploader<D> {
    /// cladefs: the replication of the cell, as its writer's taking found its head
    /// (doc-21): none for a cell durable off its host once uploaded alone, or not taken yet.
    pub fn replication(&self) -> Option<Replicated> {
        self.shared.progress.lock().unwrap().taking.and_then(|taking| taking.replication)
    }

    /// cladefs: what wakes those waiting on the uploader, for a replicator to call as more is
    /// replicated (TASK-537.3.3).
    pub fn waker(&self) -> impl Fn() + Send + Sync + 'static + use<D> {
        let changed = self.shared.changed.clone();
        move || {
            changed.notify(usize::MAX);
        }
    }

    /// cladefs: returns once the commits up to `epoch` are replicated or uploaded, as
    /// [`Uploader::replicated`] does: a future, which holds no thread while it waits
    /// (TASK-685.3).
    pub fn replicated(
        &self,
        epoch: u64,
    ) -> impl std::future::Future<Output = Result<(), Failure>> + Send + use<D> {
        self.awaiting(self.waiting_replicated(epoch))
    }

    /// cladefs: what waiting for the commits up to `epoch` replicated or uploaded waits for, if
    /// they aren't ([`Uploader::replicated`]).
    fn waiting_replicated(&self, epoch: u64) -> Result<Option<Waiting>, Failure> {
        let shared = &self.shared;
        let Some(replicator) = self.cell().replicator() else {
            return self.waiting_uploaded(epoch);
        };
        // cladefs: and while a replacement is under way, whose members may take long to catch up
        // (TASK-537.3.5); and while the bucket holds not yet what the cell opened with, which no
        // member need hold, nothing told replicated before (`Replicator::told`, TASK-678).
        let gated = shared.last_uploaded() < replicator.base();
        if replicator.degraded() || replicator.replacing_now() || gated {
            if shared.ask(epoch, Awaited::Uploaded)? {
                return Ok(None);
            }
        } else {
            shared.observe()?;
        }
        Ok(Some(Waiting::Told(replicator, epoch)))
    }

    /// cladefs: the cell's replication status for [`Uploader::status`].
    pub(super) fn replication_status(&self) -> Option<ReplicationStatus> {
        self.cell().replicator().map(|replicator| ReplicationStatus {
            f: u8::try_from(replicator.f()).unwrap_or(u8::MAX),
            members_up: u32::try_from(replicator.members_up()).unwrap_or(u32::MAX),
            degraded: replicator.degraded(),
            replacing: replicator.replacing_now(),
        })
    }
}

impl<D: Dir, B: bucket_cell::Bucket> Shared<D, B> {
    /// cladefs: fences the members of the configuration named in `head` (if `f >= 1`) at this
    /// taking's epoch, once the head is taken and before anything of theirs is read (doc-21,
    /// TASK-537.3.4): no writer of an earlier epoch has a commit replicated after.
    pub(super) async fn fence_for_take(
        &self,
        cell: CellId,
        recovering: &Recovering<D>,
        head: &Head,
    ) -> Result<FencedTake, Failure> {
        let named =
            head.replication.as_ref().and_then(|replication| replication.configuration.clone());
        // cladefs: a replicated cell's, whose writer's copies its cell notes (TASK-683).
        let replicated = head.replication.is_some() || self.options.replication.is_some();
        let old = named.clone().filter(|configuration| configuration.f >= 1);
        // cladefs: or, its placement's takings forced, those of them that answer, however few
        // (TASK-681).
        // Its own copy counted if it is its writer's, its log holding records, or, holding none,
        // its description naming the configuration's writer's epoch (TASK-683).
        let forced = recovering.forced() || self.options.force;
        let fenced = match &old {
            Some(old) => {
                let own = recovering.holds_own_copy(old.epoch);
                Some(self.fence(cell, old, head.epoch, own, forced).await?)
            }
            None => None,
        };
        Ok(FencedTake { named, replicated, old, fenced })
    }

    /// cladefs: reads the longest authentic copies of the fenced members' logs, restores the local
    /// write-ahead log if members are ahead, and notes writer copies and any forced-take cut-off
    /// in the cell's description (TASK-537.3.4, TASK-678, TASK-681, TASK-683).
    pub(super) async fn recover_replicated(
        &self,
        cell: CellId,
        writer: &Arc<Writer<B>>,
        recovering: Recovering<D>,
        placement: u64,
        fenced_take: FencedTake,
    ) -> Result<(Recovering<D>, ReplicatedTake), Failure> {
        let FencedTake { named, replicated, old, fenced } = fenced_take;
        let keys = writer.keys().clone();
        // cladefs: what the fenced members hold, the longest copy of each log, after the head's
        // last commit: the cell's write-ahead log, if it holds none, and what the new
        // configuration's members are given before it is active (TASK-537.3.4).
        let (seed, seeding, recovering, short, restored) = match (&old, fenced) {
            (Some(old), Some(Fenced { members: fenced, short })) => {
                let (logs, mut seeding) = self.read_longest(cell, old, &fenced, &keys).await?;
                let last = writer.taken().last_commit;
                let seed = match crate::replicate::trimmed(cell, &logs, last, Some(&keys)) {
                    Ok(seed) => seed,
                    // Forced, what the members that answered hold, going not on from the head, is
                    // given up as what the others held is (TASK-681).
                    Err(err) if short.is_some() => {
                        log::warn!("cladefs: {cell:?} taken by force: its members' logs: {err:#}");
                        Vec::new()
                    }
                    Err(err) => return Err(err.into()),
                };
                // What it holds of the members' logs from here: the seed alone, counted until its
                // replicator holds it (board 8453, Codex's review 56).
                drop(logs);
                seeding.hold_only(seed.iter().map(|(_, bytes)| bytes.len() as u64).sum());
                // Its own write-ahead log, if it holds one, recovered unless the members' go past
                // it: a later writer's, or this writer's beyond its own sync (TASK-537.3.4's
                // review).
                let own = recovering.last_commit_held();
                let members = crate::replicate::last_commit(cell, &seed);
                let ahead = crate::replicate::members_ahead(own, members);
                // Its own kept, the members' logs are given the new configuration only if they
                // hold at least one complete commit of its history (board 5398: a seed holding
                // only an incomplete `continued` start of a multi-segment commit has `members ==
                // None` and must not be spooled onto a fresh store's segment 0; and any seed that
                // IS kept advances `recovering`'s next segment number past it below).
                let seed = match ahead || (members.is_some() && recovering.holds_commits(members)) {
                    true => seed,
                    false => {
                        if members.is_some() {
                            log::info!(
                                "cladefs: {cell:?}: its members' logs, through {members:?}, are \
                                 an earlier writer's than its own, through {own:?}: given up"
                            );
                        }
                        seeding.hold_only(0);
                        Vec::new()
                    }
                };
                let mut recovering = match ahead {
                    false => recovering,
                    true => blocking(|| -> Result<_, Failure> {
                        #[cfg(test)]
                        {
                            *self.recovered_from.lock().unwrap() = Some(old.clone());
                        }
                        let mut recovering = recovering;
                        if recovering.has_wal() {
                            log::warn!(
                                "cladefs: {cell:?}: its members hold past its own log ({own:?} < \
                                 {members:?}): recovered from them"
                            );
                            recovering.drop_wal()?;
                        }
                        Ok(recovering.restored(&seed, last)?)
                    })?,
                };
                // Never open a segment whose log number a seed we spool holds (board 5398).
                recovering.advance_next_past(&seed);
                (seed, seeding, recovering, short, ahead)
            }
            _ => (Vec::new(), crate::replicate::Holding::none(), recovering, None, false),
        };
        // cladefs: taken by force from too few members: what may be lost noted in the cell's
        // description, which its status says from now on, while this placement holds it; or, of an
        // earlier taking of this forced placement, what that one noted (TASK-681).
        let mut recovering = recovering;
        // cladefs: the copies it is noted, before a configuration of its writer's is named or
        // replicates (TASK-683): that writer's; and, its log restored from members of the
        // configuration named, the longest copies of every log a quorum fenced, that one's, all
        // of it but taken by force. Those it was before of the one named or later kept, unless its
        // log was dropped, which let them go.
        if replicated {
            let mut copies = vec![writer.epoch()];
            if let (true, None, Some(old)) = (restored, &short, &old) {
                copies.push(old.epoch);
            }
            let named = named.as_ref().map(|configuration| configuration.epoch);
            blocking(|| recovering.note_writer_copies(&copies, named))?;
        }
        let forced_after = match &short {
            Some(_) => {
                let kept = writer.taken().last_commit.map_or(0, epoch_of);
                blocking(|| recovering.note_forced_after(placement, kept))?;
                Some(kept)
            }
            None => recovering.forced_after(),
        };
        Ok((recovering, ReplicatedTake { old, seed, seeding, short, forced_after }))
    }

    /// cladefs: finishes starting replication once the cell is open: records any forced-take
    /// cut-off, activates the initial configuration if replicated, and arms the keeper
    /// (doc-21, TASK-537.3.3, TASK-537.3.5, TASK-681).
    pub(super) async fn start_replication(
        &self,
        cell: CellId,
        writer: &Arc<Writer<B>>,
        taken: ReplicatedTake,
    ) -> Result<(), Failure> {
        let ReplicatedTake { old, seed, seeding, short, forced_after } = taken;
        // cladefs: taken by force from too few members: what may be lost said, and kept in its
        // status (TASK-681).
        // Kept for sure: what the bucket holds. Past it, what the members that answered held, and
        // its own write-ahead log, which may be an earlier writer's, past what a later one told
        // replicated on the members that answered not (TASK-680's simulation, seed 129).
        if let (Some(short), Some(kept)) = (&short, forced_after) {
            log::warn!(
                "cladefs: {cell:?} taken by force: {short}: the bucket holds its commits through \
                 {kept}; commits told replicated past it that only the members that answered not \
                 held may be lost"
            );
        }
        if let Some(kept) = forced_after {
            self.forced_after(kept);
        }
        // cladefs: a replicated cell's commits replicated from now on (doc-21, TASK-537.3.3).
        // cladefs: and, of a cell whose head names a configuration, replicated at its f at least,
        // whatever this taking asked: its head goes on naming a live configuration, whose
        // members hold what the one before held, until its writer drops it to f = 0 once the
        // bucket holds all it told (TASK-537.3.5; TASK-537.3.4's review).
        if let Some(mut replication) = writer.taken().head.replication.clone() {
            let wish = replication.wish;
            let before = old.as_ref().map_or(0, |old| old.f);
            replication.wish.f = replication.wish.f.max(before);
            if replication.wish.f >= 1 {
                let (configuration, stale) =
                    self.replicate(writer, replication, old, (seed, seeding)).await?;
                *self.keeping.lock().unwrap() = Some((wish, configuration, stale));
            }
        }
        Ok(())
    }

    /// cladefs: spawns the keeper task that keeps a replicated cell's configuration whole beside
    /// the uploader (TASK-537.3.5, board 8507).
    pub(super) fn spawn_keeper(self: &Arc<Self>, writer: &Arc<Writer<B>>) {
        let keeping = self.keeping.lock().unwrap().take();
        if let Some((wish, configuration, stale)) = keeping {
            let (done, kept) = futures::channel::oneshot::channel::<()>();
            *self.kept.lock().unwrap() = Some(kept);
            let keeper = self.clone().keep(writer.clone(), wish, (configuration, stale));
            let shared = self.clone();
            (self.spawn)(
                async move {
                    // cladefs: its panic the uploader's, which fails and stops, rather than go on
                    // with its configuration kept no more (board 8507).
                    if let Err(panic) = std::panic::AssertUnwindSafe(keeper).catch_unwind().await {
                        shared.panicked("its keeper", &*panic);
                    }
                    // Said once the keeper, and what it held, went.
                    drop(shared);
                    drop(done);
                }
                .boxed(),
            );
        }
    }

    /// cladefs: `writer`, having taken a replicated cell asking for `replication`, has its commits
    /// replicated from now on (doc-21, TASK-537.3.3, TASK-537.3.4): a configuration of this host
    /// and 2f members picked from its peers, given `seed`, what the configuration `old` before held
    /// after the head's last commit, and activated by a swap of the head once f of them hold it, so
    /// that what `old` held replicated stays so; then the cell's replicator started, of the
    /// commits after those it opened with, on no member of the new configuration but the seed's.
    /// The configuration before's members drop what they held of it, if they answer.
    ///
    /// A cell that opened holding more than `old`'s members gave, its own write-ahead log's
    /// commits on its own host (a retake on the writer's disk, the member holding the rest out of
    /// reach), may hold commits told replicated that no member of the new configuration is given:
    /// it is activated only by the acknowledgement that names them all, in the same swap
    /// (`Writer::configure_past`), the head naming `old`, fenced, until then (TASK-678; the spec's
    /// installOwn, its mutation InstallMembersOnly). Returns the configuration, and whether the
    /// head names one before it still ([`Shared::keep`]'s `stale`). The seed's bytes are counted
    /// by `seeding` until the replicator holds them, which counts them then (board 8453).
    async fn replicate(
        &self,
        writer: &Arc<Writer<B>>,
        replication: Replication,
        old: Option<Configuration>,
        (seed, seeding): (Vec<(u64, Vec<u8>)>, crate::replicate::Holding),
    ) -> Result<(Configuration, bool), Failure> {
        let Replication { wish, configuration } = replication;
        let members = self.members()?;
        let open_cell = self.cell().clone();
        let cell = open_cell.description().cell;
        let before = configuration.clone();
        let number = configuration.map_or(1, |configuration| configuration.number + 1);
        let me = members.me().map_err(Failure::msg)?;
        let picked = (me, members.pick(cell, number, wish.f, &[]).await);
        let base = open_cell.last_commit()?;
        let epoch = writer.epoch();
        // The last commit the head names, which every configuration's members go on from.
        let named = writer.taken().last_commit.map(epoch_of);
        let (me, others) = match picked {
            (me, Ok(others)) => (me, others),
            // cladefs: no members to be had: the cell serves degraded, at f = 0, its commits
            // waiting for the bucket, until its keeper has members (Codex's review of
            // TASK-537.3.5): a take of a cell its writer dropped to f = 0 needs no peers. And a
            // take that recovered what the configuration before held, which it can give no
            // configuration of its own, a host of three lost (TASK-537.3.6): the head goes on
            // naming that configuration, fenced, its members holding what was told, until the
            // acknowledgement that names all the cell recovered names its own, of f = 0, in the
            // same swap (`Writer::configure_past`; the spec's aloneSwitch).
            (me, Err(err)) => {
                log::warn!(
                    "cladefs: {cell:?} degraded: no configuration of f {} to be had, its \
                     commits waiting for the bucket: {err}",
                    wish.f
                );
                let source = Some(open_cell.replica_source());
                let replicator = self.replicator(&members, 0, base, source, Vec::new());
                open_cell.replicate_to(replicator).await;
                return Ok(match before {
                    Some(before) if before.f == 0 => (before, false),
                    // The writer's own, of f = 0, which the head names once it names all the
                    // cell recovered: at once if the bucket holds it all.
                    Some(before) => {
                        let number = before.number + 1;
                        let alone = Configuration { number, epoch, f: 0, members: vec![me] };
                        match named.is_some_and(|named| named >= base) {
                            true => writer.configure(alone.clone()).await?,
                            false => writer.configure_past(base, alone.clone()),
                        }
                        (alone, true)
                    }
                    // A cell the head names no configuration of: its keeper's replacement the
                    // first.
                    None => (Configuration { number: 0, epoch, f: 0, members: vec![me] }, false),
                });
            }
        };
        let mut all = vec![me];
        all.extend(others.iter().cloned());
        let configuration = Configuration { number, epoch, f: wish.f, members: all };
        let links =
            others.iter().map(|member| members.link(cell, &configuration, member)).collect();
        let source = Some(open_cell.replica_source());
        let replicator = self.replicator(&members, usize::from(wish.f), base, source, links);
        // What `old`'s members held, which they give the new configuration's: through the seed's
        // last commit, or the head's. The cell holding more, its own log's, of `old`'s writer's
        // (TASK-678): the head names the new configuration once it names those too.
        let given = crate::replicate::last_commit(cell, &seed).map(epoch_of).max(named);
        if bucket_cell::rules().installs_own && old.is_some() && given.unwrap_or(0) < base {
            log::info!(
                "cladefs: {cell:?} replicated in configuration {number} with {:?} once the bucket \
                 holds its commits through {base}, which its own log held past what the members \
                 of configuration {} gave ({given:?}): until then the head names that one",
                configuration.members.iter().map(|member| &member.address).collect::<Vec<_>>(),
                old.as_ref().map_or(0, |old| old.number),
            );
            writer.configure_past(base, configuration.clone());
            open_cell.replicate_to(replicator).await;
            return Ok((configuration, true));
        }
        if !seed.is_empty() {
            replicator.seed(base, seed);
            drop(seeding);
            let started = std::time::Instant::now();
            while replicator.replicated() < base {
                if replicator.fenced().is_some() || started.elapsed() > SEEDING {
                    return Err(Failure::msg(format!(
                        "the new configuration's members took not what the one before held \
                         within {SEEDING:?}: {:?}",
                        replicator.down()
                    )));
                }
                (self.clock)(Duration::from_millis(10)).await;
            }
        }
        writer.configure(configuration.clone()).await?;
        open_cell.replicate_to(replicator).await;
        self.configured(number);
        log::info!(
            "cladefs: {cell:?} replicated in configuration {number} with {:?}",
            configuration.members.iter().map(|member| &member.address).collect::<Vec<_>>()
        );
        if let Some(old) = old {
            self.drop_before(cell, &old, &configuration).await;
        }
        Ok((configuration, false))
    }

    /// cladefs: a replicator of a configuration of `f`, its members reached by `links`, started
    /// after the commit of epoch `base`, reading evicted logs from `source`
    /// ([`crate::replicate::Replicator`]): its members served by threads of their own, unless
    /// `members` drive it.
    fn replicator(
        &self,
        members: &Arc<dyn Members>,
        f: usize,
        base: u64,
        source: Option<Arc<dyn crate::replicate::LogSource>>,
        links: Vec<Arc<dyn crate::replicate::MemberLink>>,
    ) -> Arc<crate::replicate::Replicator> {
        let changed = self.changed.clone();
        let waker = move || {
            changed.notify(usize::MAX);
        };
        let replicator =
            crate::replicate::Replicator::idle_on(self.host.clone(), f, base, source, links, waker);
        if !members.drives(&replicator) {
            replicator.run_tasks();
        }
        replicator
    }

    /// cladefs: has the members of `old`, but this host, drop what they hold of `cell`'s
    /// configurations before `configuration`, best effort: a member that does not answer sweeps
    /// them later (doc-21, Sweeping stale replica logs).
    async fn drop_before(&self, cell: CellId, old: &Configuration, configuration: &Configuration) {
        let Ok(members) = self.members() else { return };
        for member in old.members.iter().skip(1) {
            if let Err(err) = members.drop_before(cell, configuration, member).await {
                log::info!("cladefs: {} keeps {cell:?}'s logs for now: {err}", member.address);
            }
        }
    }

    /// cladefs: keeps the configuration of a replicated cell, `configuration` as its taking
    /// activated, what its head asks for, `wish`, until the uploader stops, or its writer is fenced
    /// (doc-21, Replacing a member and Degraded; TASK-537.3.5):
    /// - a member refusing the writer, or down for [`super::Options::replace_after`], is replaced,
    ///   by a configuration of `wish`'s f, picked from the host's peers, the refusing aside;
    /// - its commits uploaded at once, rather than at the interval, while degraded: fewer than f
    ///   members take what it sends, their waiters left to the bucket;
    /// - degraded, and no replacement to be had, it drops to f = 0, so that a take of the cell
    ///   needs no member of a configuration lost, and back up as members come back;
    /// - a configuration of another f than `wish`'s, as a taking asking for f = 0 of a cell
    ///   replicated keeps until the bucket holds what it told, is replaced by one of `wish`'s;
    /// - `stale`, the head naming the configuration of the writer before, which the taking could
    ///   give no configuration of its own (TASK-537.3.6): nothing replaced until the
    ///   acknowledgement naming what the taking recovered names the writer's own
    ///   (`Writer::configure_past`).
    async fn keep(
        self: Arc<Self>,
        writer: Arc<Writer<B>>,
        wish: Wish,
        (mut configuration, mut stale): (Configuration, bool),
    ) {
        let cell = writer.cell();
        let mut degraded = None::<std::time::Instant>;
        // Ticks to wait before trying a replacement again, and those left to wait.
        let (mut backoff, mut skip) = (0u32, 0u32);
        // Each attempt's configuration a number of its own, one given up never reused (the
        // review's: a member of both would hold what it never dropped of the first under the
        // second's key); and the members that refused one, not picked again at once.
        let mut number = configuration.number + 1;
        let mut avoid = Vec::<String>::new();
        loop {
            // Its tick, or the uploader stopping, which it holds up no longer (TASK-701).
            let stopping = std::pin::pin!(self.until_stopping());
            futures::future::select((self.clock)(KEEPING), stopping).await;
            if self.stopping() {
                return;
            }
            let Some(replicator) = self.cell().replicator() else { return };
            if replicator.fenced().is_some() {
                return;
            }
            skip = skip.saturating_sub(1);
            if replicator.degraded() {
                if degraded.is_none() {
                    log::warn!(
                        "cladefs: {cell:?} degraded: {} of the {} members of configuration {} \
                         up, its commits waiting for the bucket: {:?}",
                        replicator.members_up(),
                        2 * usize::from(configuration.f),
                        configuration.number,
                        replicator.down()
                    );
                }
                degraded.get_or_insert_with(std::time::Instant::now);
                self.eager();
            } else if degraded.take().is_some() {
                log::info!("cladefs: {cell:?} degraded no more");
            }
            // Alone, its own configuration is named once the bucket holds what it recovered: no
            // replacement before (the spec's aloneSwitch).
            if stale {
                match writer.switching() {
                    // What it waits for uploaded at once (TASK-678).
                    true => {
                        self.eager();
                        continue;
                    }
                    false => stale = false,
                }
                // What StatCell says of it, the head naming it now (Codex's review).
                self.configured(configuration.number);
                if configuration.f == 0 && wish.f == 0 {
                    self.cell().unreplicate().await;
                    log::info!("cladefs: {cell:?} replicated no more, as its head asks");
                    return;
                }
            }
            let replace_after = self.options.replace_after;
            // cladefs: members the host's peers name no more, replaced while they serve still,
            // rather than once they are gone for `replace_after` (board 8579).
            let removed = self.removed(&configuration);
            let replace =
                configuration.f != wish.f || replicator.lost(replace_after) || !removed.is_empty();
            if !replace || skip > 0 {
                continue;
            }
            if !removed.is_empty() {
                log::info!(
                    "cladefs: {cell:?}: configuration {}'s members {removed:?} are no peers of \
                     this host's any more: replacing them",
                    configuration.number
                );
            }
            let refusing = replicator.refusing().into_iter();
            let refusing =
                refusing.filter_map(|m| Some(configuration.members.get(m + 1)?.address.clone()));
            for address in refusing.chain(removed) {
                if !avoid.contains(&address) {
                    avoid.push(address);
                }
            }
            number = number.max(configuration.number + 1);
            let replacing = (&configuration, &mut number, wish.f);
            let mut new = self.replace(&writer, &replicator, replacing, &mut avoid).await;
            // Degraded, and no member to be had: f = 0, once the bucket holds what it told.
            let long = degraded.is_some_and(|since| since.elapsed() >= replace_after);
            if matches!(new, Ok(None)) && wish.f >= 1 && configuration.f >= 1 && long {
                let replacing = (&configuration, &mut number, 0);
                new = self.replace(&writer, &replicator, replacing, &mut avoid).await;
            }
            match new {
                Ok(Some(new)) => {
                    configuration = new;
                    backoff = 0;
                    avoid.clear();
                    if configuration.f == 0 && wish.f == 0 {
                        self.cell().unreplicate().await;
                        log::info!("cladefs: {cell:?} replicated no more, as its head asks");
                        return;
                    }
                }
                Ok(None) => {
                    backoff = (backoff * 2).clamp(BACKOFF_FIRST, BACKOFF_MOST);
                    skip = backoff;
                }
                // Its writer stopped, the uploader with it.
                Err(err) => {
                    log::error!("cladefs: {cell:?}: replacing its configuration: {err}");
                    return;
                }
            }
        }
    }

    /// cladefs: the members of `configuration`, its writer aside, that the host's members are
    /// picked from no more ([`Members::is_peer`], board 8579).
    fn removed(&self, configuration: &Configuration) -> Vec<String> {
        // No members: a configuration of no peers, none to say removed.
        let Ok(members) = self.members() else { return Vec::new() };
        let others = configuration.members.iter().skip(1);
        others
            .filter(|member| !members.is_peer(&member.address))
            .map(|m| m.address.clone())
            .collect()
    }

    /// cladefs: the head names the writer's configuration `number`, as its status says from now.
    fn configured(&self, number: u64) {
        if let Some(Taking { replication: Some(replicated), .. }) =
            self.progress.lock().unwrap().taking.as_mut()
        {
            replicated.configuration = Some(number);
        }
    }

    /// cladefs: the writer's taking was forced, fewer members fenced than a take needs, and
    /// the bucket holding its commits through epoch `kept`, as its status says from now (TASK-681).
    fn forced_after(&self, kept: u64) {
        if let Some(Taking { replication: Some(replicated), .. }) =
            self.progress.lock().unwrap().taking.as_mut()
        {
            replicated.forced_after = Some(kept);
        }
    }

    /// cladefs: the configuration whose members' logs its taking recovered the cell from, if it
    /// did ([`Shared::recovered_from`]'s field).
    #[cfg(test)]
    pub(crate) fn recovered_from(&self) -> Option<Configuration> {
        self.recovered_from.lock().unwrap().clone()
    }

    /// cladefs: whether the uploader stops, or stopped.
    fn stopping(&self) -> bool {
        let progress = self.progress.lock().unwrap();
        progress.stop || progress.failed.is_some()
    }

    /// cladefs: returns once the uploader is stopping ([`Shared::stopping`]), holding no thread.
    async fn until_stopping(&self) {
        loop {
            let changed = self.changed.listen();
            if self.stopping() {
                return;
            }
            changed.await;
        }
    }

    /// cladefs: has the uploader close and upload what the cell holds at once, if it holds
    /// commits not uploaded: as a replicated cell's commits wait for the bucket (doc-21, Degraded).
    fn eager(&self) {
        let Ok(last) = self.cell().last_commit() else { return };
        if last > self.last_uploaded() {
            bucket_cell::host_disk::Urge::urge(self);
        }
    }

    /// cladefs: replaces `writer`'s configuration `from` with `number`, of `f`, `number` moved past
    /// it once its members are sent anything (doc-21, Replacing a member; TASK-537.3.5): this host
    /// and 2f members picked from its peers, those at `avoid` aside, made at its epoch; given every
    /// log the replicator spools, and every flush from now, its commits counted on both meanwhile
    /// (the joint rule); swapped into the head once f of its members hold all the guest was told
    /// replicated, or, for f = 0, once the bucket does; then counted alone. The new configuration,
    /// or none if none was to be had, or it took not what it needed within [`SEEDING`], given up,
    /// those of its members that refused the writer added to `avoid`. Fails if the head's swap
    /// does: the writer stops, the replacement's joint rule kept, as the head may name either.
    async fn replace(
        &self,
        writer: &Arc<Writer<B>>,
        replicator: &Arc<crate::replicate::Replicator>,
        (from, next, f): (&Configuration, &mut u64, u8),
        avoid: &mut Vec<String>,
    ) -> Result<Option<Configuration>, Failure> {
        let members = self.members()?;
        let cell = writer.cell();
        let number = *next;
        let picked = async {
            let me = members.me()?;
            let others = match f {
                0 => Vec::new(),
                f => members.pick(cell, number, f, &avoid[..]).await?,
            };
            Ok::<_, String>((me, others))
        };
        let picked = picked.await;
        let (me, others) = match picked {
            Ok(picked) => picked,
            Err(err) => {
                log::warn!(
                    "cladefs: {cell:?}: no configuration {number} of f {f} to be had: {err}"
                );
                return Ok(None);
            }
        };
        let mut all = vec![me];
        all.extend(others.iter().cloned());
        let configuration = Configuration { number, epoch: writer.epoch(), f, members: all };
        let links = others.iter().map(|member| members.link(cell, &configuration, member));
        replicator.replacing(usize::from(f), links.collect());
        *next = number + 1;
        let started = std::time::Instant::now();
        let mut waits = 0u32;
        while !replicator.installed(self.last_uploaded()) {
            if self.stopping() || replicator.fenced().is_some() || started.elapsed() > SEEDING {
                let refusing = replicator.abandon().into_iter();
                avoid.extend(refusing.filter_map(|m| Some(others.get(m)?.address.clone())));
                log::warn!(
                    "cladefs: {cell:?}: configuration {number} of f {f} given up: its members \
                     took not what the guest was told replicated within {SEEDING:?}: {:?}",
                    replicator.down()
                );
                return Ok(None);
            }
            // Of f = 0, what was told replicated uploaded at once.
            // cladefs: in a simulation, at any wait (doc-24).
            if f == 0 && (waits.is_multiple_of(URGE_EVERY) || bucket_cell::buggify!()) {
                self.eager();
            }
            waits += 1;
            (self.clock)(Duration::from_millis(10)).await;
        }
        writer.configure(configuration.clone()).await?;
        replicator.swapped();
        self.configured(number);
        log::info!(
            "cladefs: {cell:?} replicated in configuration {number}, of f {f}, with {:?}, \
             replacing {}",
            configuration.members.iter().map(|member| &member.address).collect::<Vec<_>>(),
            from.number
        );
        self.drop_before(cell, from, &configuration).await;
        Ok(Some(configuration))
    }

    /// cladefs: the host's members, which a replicated cell needs.
    fn members(&self) -> Result<Arc<dyn Members>, Failure> {
        let members = self.options.members.clone().ok_or_else(|| {
            Failure::msg(
                "a replicated cell needs the daemon's peers and replica directory \
                 (CLADEFSD_PEER_ADDRESS, CLADEFSD_PEERS, CLADEFSD_REPLICAS)",
            )
        })?;
        Ok(members.0)
    }

    /// cladefs: fences `cell` at `epoch` on the members of `old`, the configuration its head named
    /// as it was taken (doc-21, TASK-537.3.4): f+1 members other than its writer must answer, so
    /// that no ack quorum of `old` is left unfenced, the writer's own copy, which nothing fences,
    /// counting only if this host is the writer and holds it (`own`: its cell noted as the
    /// writer's copy of `old`, TASK-683). Each
    /// that answered, with the ends of the logs it holds. `forced` (TASK-681), fewer may: why they
    /// are too few, then.
    async fn fence(
        &self,
        cell: CellId,
        old: &Configuration,
        epoch: u64,
        own: bool,
        forced: bool,
    ) -> Result<Fenced, Failure> {
        let members = self.members()?;
        let me = members.me().map_err(Failure::msg)?;
        let mine = own && old.members.first() == Some(&me);
        let needed = usize::from(old.f) + 1 - usize::from(mine);
        let mut fenced = Vec::new();
        let mut refused = Vec::new();
        for member in old.members.iter().skip(1) {
            match members.fence(cell, old, member, epoch).await {
                Ok(ends) => fenced.push((member.clone(), ends)),
                Err(err) => refused.push(format!("{}: {err}", member.address)),
            }
        }
        if fenced.len() < needed {
            let short = format!(
                "{} of the {needed} members of configuration {} a take must fence answered: {}",
                fenced.len(),
                old.number,
                refused.join("; ")
            );
            return match forced {
                true => Ok(Fenced { members: fenced, short: Some(short) }),
                false => Err(Failure::msg(short)),
            };
        }
        Ok(Fenced { members: fenced, short: None })
    }

    /// cladefs: of each log the members `fenced` hold of `old`, the longest copy, read from the
    /// member holding it: every commit a guest was told replicated is on one of them, and each
    /// member's logs are prefixes of the writer's (doc-21, TASK-537.3.4). Of a segment's records,
    /// the longest copy `keys`, the cell's, find authentic, cut to what of it is whole: what a
    /// member answers is unauthenticated, and a longer copy garbled, or forged, would otherwise
    /// hide an honest one (TASK-684). A copy found not authentic is passed over; so is one that
    /// can't be read, a member stalled or gone, if another as long is read (board 5416): else it
    /// fails the take, as it may be the only one holding commits told replicated, as does one that
    /// reads shorter than its member's fence said; and so does a log none of whose copies is
    /// authentic, as a log missing would look like a writer's base, given up
    /// (`replicate::trimmed`). Each copy is read to its fence's end, no further. A copy its
    /// member's fence says ends at nothing holds nothing, as one it never made: a member makes a
    /// log's file at its first append, which may fail after (board 5914: its write failing,
    /// ENOSPC, it held the file empty, which, no header of its own, was no authentic copy, and
    /// every take failed). What it holds of the copies, the longest of each log and the one being
    /// read, from its asking, is counted with the hot replication buffers, which the daemon's
    /// memory governor governs (`replicate::Holding`, board 8453), and returned with the logs, still
    /// counted for as long as the caller holds it; each member's ends are bounded where its answer
    /// entered (`cladefsd::peers`), and a read stops at its end.
    async fn read_longest(
        &self,
        cell: CellId,
        old: &Configuration,
        fenced: &[(Member, Vec<(u64, u64)>)],
        keys: &Arc<Ring>,
    ) -> Result<(Vec<(u64, Vec<u8>)>, crate::replicate::Holding), Failure> {
        // Each log's copies, by their members' fences' ends, the longest first.
        let mut copies: BTreeMap<u64, Vec<(u64, Member)>> = BTreeMap::new();
        for (member, ends) in fenced {
            for &(log, end) in ends.iter().filter(|&&(_, end)| end > 0) {
                copies.entry(log).or_default().push((end, member.clone()));
            }
        }
        for held in copies.values_mut() {
            held.sort_by_key(|copy| std::cmp::Reverse(copy.0));
        }
        let members = self.members()?;
        let mut logs = Vec::with_capacity(copies.len());
        let mut holding =
            crate::replicate::Holding::on(self.host.as_ref().map(|h| Arc::clone(&h.hot_budget)));
        {
            for (log, held) in copies {
                let segment = crate::layout::SegmentId { cell: cell.id, number: log >> 1 };
                let (mut best, mut refused) = (None::<Vec<u8>>, Vec::new());
                // The longest copy read whole, and the longest that could not be read, with why.
                let (mut read, mut unread) = (0, None::<(u64, String)>);
                for (end, member) in held {
                    if best.as_ref().is_some_and(|best| best.len() as u64 >= end) {
                        break;
                    }
                    // Held from its asking, to its end, where the read stops; let go of below as
                    // far as it is not the best copy.
                    holding.hold(end);
                    let mut bytes = match members.read(cell, old, &member, log, end).await {
                        Ok(bytes) => bytes,
                        Err(err) => {
                            holding.release(end);
                            let err =
                                format!("{} reading log {log} of {cell:?}: {err}", member.address);
                            log::warn!("cladefs: {err}: passed over if another copy as long reads");
                            unread = unread.or(Some((end, err)));
                            continue;
                        }
                    };
                    bytes.truncate(usize::try_from(end).unwrap_or(usize::MAX));
                    if (bytes.len() as u64) < end {
                        return Err(Failure::msg(format!(
                            "{} gave {} bytes of log {log}, fenced at {end}",
                            member.address,
                            bytes.len()
                        )));
                    }
                    read = read.max(end);
                    // A segment's records, as far as they are whole and authentic; its own
                    // extents' files, whose records name them, as they are.
                    if log & 1 == 0 {
                        match crate::wal::replica_whole(&bytes, segment, keys) {
                            // Cut to what is whole, its allocation with it: what it holds is
                            // what is counted (Codex's review 56).
                            Ok(whole) if whole < bytes.len() => {
                                bytes.truncate(whole);
                                bytes.shrink_to_fit();
                            }
                            Ok(_) => {}
                            Err(err) => {
                                holding.release(end);
                                log::warn!(
                                    "cladefs: {}'s log {log} of {cell:?}: {err:#}: passed over",
                                    member.address
                                );
                                refused.push(format!("{}: {err:#}", member.address));
                                continue;
                            }
                        }
                    }
                    if best.as_ref().is_none_or(|best| bytes.len() > best.len()) {
                        holding.release(end - bytes.len() as u64);
                        if let Some(was) = best.replace(bytes) {
                            holding.release(was.len() as u64);
                        }
                    } else {
                        holding.release(end);
                    }
                }
                // A copy that could not be read, longer than every copy read: the commits it alone
                // may hold are not to be had.
                if let Some((end, err)) = unread.filter(|(end, _)| *end > read) {
                    return Err(Failure::msg(format!(
                        "{err}, fenced at {end}: no other copy as long"
                    )));
                }
                match best {
                    Some(bytes) => logs.push((log, bytes)),
                    None => {
                        return Err(Failure::msg(format!(
                            "no member's copy of log {log} of {cell:?} is authentic: {}",
                            refused.join("; ")
                        )));
                    }
                }
            }
        }
        Ok((logs, holding))
    }
}
