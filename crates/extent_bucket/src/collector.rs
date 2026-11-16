// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The collector (`cladefs-gc`): the only thing that deletes.
//!
//! cladefs: it collects a cell at a time (doc-7, Garbage collection; TASK-395): a pass over a cell
//! takes the cell's collector lease, lists the cell's prefix, and reads, compacts and deletes what
//! is there alone, beside passes over other cells, which it neither fences nor waits for.
//! Everything it decides by is under the cell's prefix: its branches' objects, their moves, its
//! lease, and the references of forks of its branches in other cells ([`layout::reference`]). What
//! its branches inherited from other cells it reads there, and never deletes: those cells keep it,
//! by the references of the forks that inherited it. A sweep over every cell is a pass over each:
//! one after another ([`Collector::collect`]), or many at once, by processes any number of them
//! ([`crate::sweep`]); the cells are found by delimiter listings, a page at a time, listing none
//! of their objects ([`crate::cell::listed`]).
//!
//! cladefs: a pass holds its cell by the cell's lease (`<C>gc/lease`, [`passed::CollectorLease`],
//! TASK-395.5) for as long as its collector's [`Holding`] says, renewing it as it goes and letting
//! it go as it ends: another pass, of this process or another, finds the cell held and leaves it,
//! as it leaves one whose last pass ended since its sweep began, without waiting ([`Error::Left`]).
//! A pass of a process gone holds its cell until its lease expires: the next sweep after that
//! takes it, fencing it. The lease's time only spreads the work: a pass that takes a cell another
//! still works on fences it as before, and the one fenced deletes nothing.
//!
//! A pass takes its cell's lease, lists every object of the cell, and only then reads what
//! branches need. An object written after the listing is no candidate, which is what makes reading
//! the branches' states after it safe: what a state names was either listed, and is seen as
//! needed, or not, and is left.
//!
//! The live branches are those whose records are not deleted, and the children of forks in live
//! branches' states, of the cell, which get their records only once their forks are acknowledged.
//! What they need is what recovering them reads, the objects holding the extents their states
//! name, the moves the way to those goes through, and each one's segments past its end, which may
//! be uploaded and not yet in it. Segments go only once a checkpoint covers them, so that a reader
//! that finds one missing knows, from the checkpoints, whether it is the branch's end.
//!
//! cladefs: a fork of a branch of the cell to a child in another cell is kept by the fork point: the
//! parent's whole state at the end of the segment it was forked at, the objects and moves holding
//! the extents it names along their ways from where they were sealed, and what recovering it reads
//! (TASK-395.4). It is kept while a reference names it, whatever the child frees of it since: the
//! collector is conservative, reclaiming late rather than reading other cells to find what their
//! branches still hold. A fork point shared by many references, a point cloned many times, is
//! recovered once. The references are listed after the states, by their fork points' prefixes
//! ([`referenced`]). Forks in the live
//! branches' states to other cells keep their fork points too, as a fork's child may lack its
//! reference yet. A cell with no live branch and no reference is reclaimed whole, but its records
//! and its object; and its pass retires the references of its deleted children under their
//! parents' cells, computing them from the records' origins, never listing them: the one delete a
//! pass makes outside its cell. cladefs: its records and object go too, with its heads and lease,
//! if every record of it is a deleted root's or clone's made, of an ID an allocator gave, which
//! no recorder makes again; and a clone's reference retires unmarked ([`Pass::removal`],
//! TASK-455).
//!
//! A fork of a point is its child's record alone (doc-7, Points): its child names what the point
//! names, and a pass that read the point deleted lists the records after that, and so finds every
//! child whose fork can succeed ([`branch::fork_point`]). A child still being made of a point
//! deleted the pass gives up on, first ([`abandon`]): nothing relies on it, and it may name what is
//! gone, which would stop this pass and every other.
//!
//! A state read names no fork deleted before it, a writer's checkpoint dropping those, and the
//! fork's record and its children's may have come after the pass listed the records. So it lists
//! them again after it read the states, and recovers what it finds, again while a state it read
//! may have dropped a fork: while it recovers a branch with a checkpoint of its own. A record made
//! after that names what the pass kept, or what it never listed: what a fork after a state read
//! inherited, that state holds, or a later object; and a fork before it that state names.
//!
//! Objects mostly freed are compacted: their live extents copied to their moves, one object,
//! written whole, before the object goes, a grace period later. Writers record moves in their next
//! checkpoints, after which the moves go too, unless a state's way to its extents still goes through
//! them.
//!
//! cladefs: marking reads metadata alone, which is in the clear in an encrypted cell too, holding
//! no key; compacting copies extents out of objects' data, encrypted there, so a pass compacting
//! one unwraps its cell's KEK through the collector's KMS ([`Collector::kms`]), once for the pass,
//! and its branches' DEKs from their records; and encrypts each one's moves under the key it was
//! encrypted under, with a salt of their own (doc-16, Who holds which key; TASK-411.6). A pass
//! compacting nothing encrypted asks the KMS nothing, nor does a dry run. The KEK not to be had,
//! the tenant's KEK revoked or destroyed, or the collector having no KMS, the pass marks and
//! deletes as ever, compacts what is in the clear, and leaves what is encrypted, saying so
//! ([`Report::locked`]): a revoked tenant's compaction stops, its garbage still goes. It records
//! its listing's digest as any pass does, and what it left ([`Passed::uncompacted`]): a pass after
//! it, the cell unchanged, skips the cell while the tenant's KEK is still not to be had, which the
//! KMS is asked once a sweep, for all of the tenant's cells ([`Heard`], TASK-411.13), and goes
//! over it in full once it is.
//!
//! A branch's writer names its extents where the checkpoint it recovered from, or its own since,
//! said, which the state a pass reads may name further along their moves: a checkpoint another
//! writer made, landing after the writer recovered the branch (one it took the branch from, which
//! knew nothing of it), or the state of the parent a branch inherited them from, which its writer
//! read as it was then. So a state's way to an extent starts where the extent was sealed, its
//! moves all kept, unless the state's checkpoint is that of the writer holding the branch's head,
//! read after the checkpoint was listed: a writer taking the branch after that recovers from that
//! checkpoint or a later one, and the writer that made it took its moves first (TASK-175).
//!
//! A pass reads records, recovers branches, reads moves and compacts objects many at once, and
//! deletes objects in batches where the store deletes so (TASK-395.5); what forks inherited
//! together, their parent's state where they were forked, it reads once and takes once for them
//! all; and a pass reads a record again only once it changed ([`Collector`], TASK-92.7).
//!
//! cladefs: a pass records in its cell's lease, as it ends, what the next pass goes by
//! ([`crate::passed`], TASK-395.6, TASK-395.8): the digest of its listing of the cell, what it
//! deferred to a grace period, and what the cell takes, by kind. A pass whose listing is the one
//! recorded, of the same options, decides what the recorded pass did, everything it decides by
//! being under the cell's prefix: it recovers nothing, and deletes only what was deferred, once due
//! ([`Collector::collect_cell`]). An idle cell costs a pass its lease's read and one listing, a
//! look that writes nothing (TASK-455).
//!
//! cladefs: two passes delete nothing (TASK-264). A mark-only pass ([`Collector::mark_cell`]) is a
//! pass in every respect but that: it takes the cell's lease and compacts, and writes what it would
//! delete to a marks object of the cell's (`<C>gc/marks/<epoch>`, [`layout::Marks`]), which every
//! reader of the cell warns of reaching ([`crate::marks`]), and which the next such pass compares
//! what it finds doomed with: an object doomed before and kept now, the same object still, is a
//! bug, as no correct fork or recovery reaches what no live branch reached. Nothing marked goes, so
//! the bucket grows by what it would have reclaimed, the compacted objects and their moves both,
//! nor does it retire references. A dry run ([`Collector::dry_run_cell`]) writes nothing at all,
//! the lease neither: it says what a pass would delete, less what a pass would delete only for
//! compacting it, a grace period after, and what references it would retire.

use crate::bucket::{self, Bucket, ByteRange, Created, Meta, Version};
use crate::keys::{self, CellKeys, Ring};
use crate::layout::{self, Branch, Cell, Frame, Kind, Location, Marked, Marks, Moves, Origin};
use crate::moves::{Moved, MovesMaps};
use crate::passed::{self, CollectorLease, Decided, Deferred, LeaseTaking, Listed, Passed, Usage};
use crate::record::{self, Record};
use crate::recovery::{self, Checkpoints, Layered, Layers, Recovered, State};
use crate::{Error, branch, rules};
use extent_frame::{Encryption, Keys as _};
use extent_keys::{Secret, kms};
use futures::StreamExt as _;
use futures::future::{BoxFuture, FutureExt as _, Shared};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use storage_device::ExtentId;

/// How many branches' records a pass reads at once.
const RECORDS_AT_ONCE: usize = 32;

/// How many objects' trailers a pass reads at once, to learn their data's lengths.
const TRAILERS_AT_ONCE: usize = 32;

/// How many branches a pass recovers at once, and how many fork points' states.
const RECOVERIES_AT_ONCE: usize = 16;

/// How many of the checkpoints a pass read whole it keeps for the recoveries after: those of forks
/// of a base, recovered one after another, which start from the base's.
const CHECKPOINTS_KEPT: usize = 4;

/// How many objects a pass compacts at once, each read whole: what bounds the memory compacting
/// takes, as many objects' bytes, and their moves'.
const COMPACTIONS_AT_ONCE: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Options {
    /// Objects whose live extents take less than this fraction of their data are compacted.
    pub compact_below: f64,
    /// How long, by the store's clock, a compacted object stays after its moves are written, and
    /// moves after nothing names the object they are of: what spares a reader that looked an
    /// extent up just before a round trip.
    pub grace: u64,
}

/// cladefs: how a collector's passes hold their cells (TASK-395.5): the default holds none, its
/// passes taking their cells whatever another holds, but a pass holding it for a time, as the
/// tests' and the simulation's race, the later fencing the earlier.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Holding {
    /// How long a pass holds its cell, in milliseconds by the store's clock, from when it takes
    /// the lease or last renews it, which it does between its steps; 0: not at all (its lease's
    /// `until` 0), a pass after it taking the cell at once. Long enough for the longest step of a pass over the
    /// largest cell: one that takes longer is taken from, fenced, and deletes nothing.
    pub lease: u64,
    /// When its sweep began, by the store's clock, if it is a sweep's: a pass leaves its cell to
    /// another's that ended since ([`crate::Left::Swept`]), so that processes sweeping at once
    /// split the cells rather than go over each other's again.
    pub since: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// The objects of the cell it deleted.
    pub deleted: usize,
    pub compacted: usize,
    /// cladefs: the references of the cell's deleted children it retired, under their parents' cells
    /// (TASK-395.4), once, whether there or not, as it can't tell without listing other cells; and
    /// those of its own it deleted, marked, written again since their retirements (TASK-395.13).
    pub retired: usize,
    /// cladefs: the bytes of what it deleted, as listed (TASK-395.5).
    pub deleted_bytes: u64,
    /// cladefs: the bytes of what it compacted, as listed: what goes, a grace period after, of the
    /// objects, less what their moves take.
    pub compacted_bytes: u64,
    /// cladefs: the passes that skipped their cells, each unchanged since its last pass, deleting
    /// only what that pass deferred (TASK-395.6).
    pub skipped: usize,
    /// cladefs: the passes that left objects encrypted uncompacted, their cell's KEK not to be had:
    /// the KMS refusing it (its tenant's KEK revoked or destroyed, the KMS failing), or the
    /// collector having no KMS (TASK-411.6). Each still marked, compacted what is in the clear, and
    /// deleted what no live branch needs, warning of the cell and why. And those that skipped a
    /// cell a pass before left so, its tenant's KEK still not to be had (TASK-411.13).
    pub locked: usize,
    /// cladefs: the objects encrypted that those passes would have compacted, or the pass before a
    /// skipping one would have.
    pub uncompacted: usize,
    /// cladefs: the cells it removed whole, dead, their objects counted in `deleted` (TASK-455).
    pub removed: usize,
}

impl std::ops::AddAssign for Report {
    fn add_assign(&mut self, other: Self) {
        self.deleted += other.deleted;
        self.compacted += other.compacted;
        self.retired += other.retired;
        self.deleted_bytes += other.deleted_bytes;
        self.compacted_bytes += other.compacted_bytes;
        self.skipped += other.skipped;
        self.locked += other.locked;
        self.uncompacted += other.uncompacted;
        self.removed += other.removed;
    }
}

/// cladefs: what a mark-only pass over a cell did ([`Collector::mark_cell`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Marking {
    /// What it compacted; it deleted nothing, and retired nothing.
    pub report: Report,
    /// The marks it wrote, at [`layout::marks`] of the cell and their epoch: none if a later pass
    /// took the cell's lease from it, which stopped it as it stops a pass before deleting.
    pub marks: Option<Marks>,
    /// The objects the newest marks of the cell before named that it found kept, the same objects
    /// still: a bug, each warned of.
    pub resurrected: Vec<String>,
}

/// cladefs: what a dry run over a cell found ([`Collector::dry_run_cell`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DryRun {
    /// What a pass would delete, as listed, in the order of their keys; less what it would compact
    /// and so delete a grace period after.
    pub doomed: Vec<Meta>,
    /// What a pass would compact, as listed: what it deletes too, a grace period after, once no state's way to an extent goes through it.
    pub compactable: Vec<Meta>,
    /// The references, under other cells, a pass would retire, which may be gone already.
    pub retired: Vec<String>,
    /// Whether a pass took the cell's collector lease while it ran: what it read may be older than
    /// what that pass, deleting since, read, and what it found is no more than a guess.
    pub fenced: bool,
}

/// One pass of the collector over every cell, one after another, as `holder`, which must differ
/// from every other pass's: of a collector of its own, which remembers nothing of it
/// ([`Collector`]).
pub async fn collect(
    bucket: &impl Bucket,
    holder: u64,
    options: &Options,
) -> Result<Report, Error> {
    Collector::new(*options).collect(bucket, holder).await
}

/// cladefs: one pass of the collector over `cell`, as `holder`, which must differ from every other
/// pass's over the cell: of a collector of its own, which remembers nothing of it.
pub async fn collect_cell(
    bucket: &impl Bucket,
    cell: Cell,
    holder: u64,
    options: &Options,
) -> Result<Report, Error> {
    Collector::new(*options).collect_cell(bucket, cell, holder).await
}

/// The collector. cladefs: it remembers nothing from one pass to the next (TASK-395.4): what a pass
/// read of its cell goes with it, so that a sweep over millions of cells holds no more than the
/// cells it is passing over. A pass reads a record again only once it changed, as it lists the
/// records again (TASK-92.7). What is worth keeping across passes is under the cell's prefix, in
/// its lease, which any collector process reads (TASK-395.6, [`crate::passed`]).
pub struct Collector {
    options: Options,
    /// cladefs: how its passes hold their cells (TASK-395.5).
    holding: Holding,
    /// cladefs: the KMS its passes unwrap their cells' KEKs through, to compact what is encrypted
    /// (TASK-411.6): each pass by a [`keys::CachingKms`] of its own, so that nothing unwrapped
    /// outlives the pass, and a tenant's KEK revoked stops the next. None: they compact only what
    /// is in the clear.
    kms: Option<kms::Shared>,
    /// cladefs: what its passes heard of their cells' tenants' KEKs, shared with the other passes of
    /// its sweep, if given ([`Collector::heard`]); its own each pass otherwise, and each
    /// [`Collector::collect`] (TASK-411.13).
    heard: Option<Arc<Heard>>,
}

/// cladefs: what a sweep's passes heard of their cells' tenants' KEKs from the KMS (TASK-411.13):
/// whether each is to be had, asked once, however many of its passes ask. What a pass goes by over
/// a cell unchanged since a pass that left objects encrypted uncompacted, its cell's KEK not to be
/// had ([`Passed::uncompacted`]): skipping it while the KEK is still not, so that a revoked
/// tenant's idle cells cost a sweep a lease and a listing each, and the KMS one call for all of
/// them; and going over it in full, compacting them, once it is. The KMS is asked by a wrap of a
/// key of no use, which it makes only with the tenant's KEK, revoked, destroyed or failing
/// refusing it as they refuse an unwrap; a KMS that wraps but unwraps not the cell's KEK (another
/// than the one that wrapped it) has the cell passed in full, and left so, each sweep, as before.
/// Without a KMS, none is: the cell is skipped, asking nothing.
#[derive(Default)]
pub struct Heard {
    asked: std::sync::Mutex<BTreeMap<u64, Shared<BoxFuture<'static, bool>>>>,
}

impl Heard {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// Whether `tenant`'s KEK is to be had, as `kms` says, asked once.
    async fn to_be_had(&self, kms: Option<&kms::Shared>, tenant: u64) -> bool {
        let Some(kms) = kms else { return false };
        let asking = (self.asked.lock().unwrap().entry(tenant))
            .or_insert_with(|| {
                let kms = kms.clone();
                async move { kms.wrap_dyn(tenant, &Secret::random()).await.is_ok() }
                    .boxed()
                    .shared()
            })
            .clone();
        asking.await
    }
}

/// cladefs: what a pass over a cell keeps as it reads it, and drops with it.
#[derive(Default)]
struct Remembered {
    /// The records read, by their keys, with the versions they were read at.
    records: BTreeMap<String, (String, Record)>,
    trailers: Trailers,
}

impl Collector {
    pub fn new(options: Options) -> Self {
        Self { options, holding: Holding::default(), kms: None, heard: None }
    }

    /// cladefs: one whose passes hold their cells as `holding` says (TASK-395.5).
    pub fn holding(self, holding: Holding) -> Self {
        Self { holding, ..self }
    }

    /// cladefs: one whose passes compact objects encrypted under their cells' keys, unwrapped
    /// through `kms`, if given (TASK-411.6): a pass unwraps its cell's KEK only if it compacts an
    /// object encrypted, once for all of them; marking, dry runs and passes compacting nothing
    /// encrypted ask it nothing.
    pub fn kms(self, kms: Option<kms::Shared>) -> Self {
        Self { kms, ..self }
    }

    /// cladefs: one whose passes share `heard`, what they heard of their tenants' KEKs, with the
    /// other passes of their sweep (TASK-411.13): the KMS asked once a tenant for all of them.
    pub fn heard(self, heard: Arc<Heard>) -> Self {
        Self { heard: Some(heard), ..self }
    }

    /// cladefs: a pass over every cell of `bucket`, one after another, as `holder`, which must differ
    /// from every other pass's over each ([`Collector::collect_cell`]): the cells found by
    /// delimiter listings, a page at a time ([`crate::cell::listed`]). A pass that fails stops none
    /// of the others; the first error is returned once all ran, what they did then unsaid. A cell
    /// another pass holds it leaves ([`Error::Left`]), as no failure. cladefs: its passes share what
    /// they heard of their tenants' KEKs ([`Heard`]).
    pub async fn collect(&mut self, bucket: &impl Bucket, holder: u64) -> Result<Report, Error> {
        let mut cells = std::pin::pin!(crate::cell::listed(bucket));
        let mut report = Report::default();
        let mut failed = None;
        let heard = self.heard.clone().unwrap_or_default();
        while let Some(cell) = cells.next().await {
            match self.collect_hearing(bucket, cell, holder, &heard).await {
                Ok(done) => report += done,
                Err(Error::Left(_)) => {}
                Err(error) => drop(failed.get_or_insert(error)),
            }
        }
        failed.map_or(Ok(report), Err)
    }

    /// cladefs: one pass over `cell`, as `holder`, which must differ from every other pass's over the
    /// cell: [`Error::Left`] if it leaves the cell to another, as its lease says.
    ///
    /// It takes the cell's lease, reading what the last pass to end recorded there, and lists the
    /// cell. A listing of the digest recorded, its options the same, is decided as it was: the pass
    /// deletes what was deferred to a grace period, once due, and nothing else, recovering nothing
    /// (TASK-395.6). Otherwise it passes in full. It records what it found as it lets the lease go,
    /// in the one write ([`Collector::release`]).
    pub async fn collect_cell(
        &mut self,
        bucket: &impl Bucket,
        cell: Cell,
        holder: u64,
    ) -> Result<Report, Error> {
        let heard = self.heard.clone().unwrap_or_default();
        self.collect_hearing(bucket, cell, holder, &heard).await
    }

    /// cladefs: one pass over `cell`, as [`Collector::collect_cell`] says, hearing of its tenant's
    /// KEK by `heard`.
    async fn collect_hearing(
        &self,
        bucket: &impl Bucket,
        cell: Cell,
        holder: u64,
        heard: &Heard,
    ) -> Result<Report, Error> {
        let found = passed::found(bucket, cell).await;
        let looked = match &found {
            Some(object) => match self.look(bucket, cell, holder, object, heard).await? {
                Looked::Unchanged(report) => return Ok(report),
                Looked::Changed(listed, epoch) => Some((listed, epoch)),
                Looked::Unrecorded => None,
            },
            None => None,
        };
        let Some(mut taken) = self.take(bucket, cell, holder, found).await? else {
            return Ok(Report::default());
        };
        // The look's listing is this pass's if its taking came next, no other between: as if it
        // listed once it took, the passes before it fenced alike, those deleting since their
        // checks doing so either way (TASK-455). Else it lists anew.
        let listed = match looked {
            Some((listed, epoch)) if taken.epoch == epoch + 1 => Some(listed),
            _ => first_listing(bucket, cell).await,
        };
        let recorded = taken.passed.take();
        let collected = self.collect_held(bucket, cell, &taken, recorded, heard, listed).await;
        let ended = match &collected {
            Ok((_, passed)) => PassEnd::Through(passed.clone()),
            Err(_) => PassEnd::Failed,
        };
        self.release(bucket, cell, &taken, ended).await?;
        collected.map(|(report, _)| report)
    }

    /// cladefs: a look at `cell`, whose collector lease is `found`, for a pass of `holder`, before
    /// it takes the lease (TASK-455): the cell left to another pass, as the lease says
    /// ([`Error::Left`]); or, its listing of the digest the last pass in full recorded, of the same
    /// options, with nothing that pass deferred due, nor its tenant's KEK to be had now if it left
    /// objects encrypted uncompacted, the pass that skips it, which writes nothing and deletes
    /// nothing: no other pass need be fenced, and it costs the lease's read and the listing. None:
    /// the pass goes on, taking the lease, with the look's listing and the lease's epoch it read.
    async fn look(
        &self,
        bucket: &impl Bucket,
        cell: Cell,
        holder: u64,
        found: &bucket::Object,
        heard: &Heard,
    ) -> Result<Looked, Error> {
        let lease = CollectorLease::decode(cell, &found.bytes)?;
        if let Some(left) = lease.leaves(holder, self.holding, bucket.now()) {
            return Err(Error::Left(left));
        }
        let epoch = lease.epoch;
        let Some(passed) = lease.passed.filter(|passed| passed.listing.is_some()) else {
            return Ok(Looked::Unrecorded);
        };
        let Some(listed) = first_listing(bucket, cell).await else { return Ok(Looked::Unrecorded) };
        let due = passed.deferred.iter().any(|deferred| deferred.due <= bucket.now());
        if !passed.holds_for(&listed, &self.options) || due {
            return Ok(Looked::Changed(listed, epoch));
        }
        let uncompacted = passed.uncompacted as usize;
        if uncompacted > 0 && heard.to_be_had(self.kms.as_ref(), cell.tenant).await {
            return Ok(Looked::Changed(listed, epoch));
        }
        let locked = usize::from(uncompacted > 0);
        Ok(Looked::Unchanged(Report { skipped: 1, locked, uncompacted, ..Report::default() }))
    }

    /// cladefs: a pass over `cell`, which holds its lease as `taken` says, and what the last pass to
    /// end recorded, `recorded`: what it did, and what it records, if anything. A pass after one
    /// that left objects encrypted uncompacted skips the cell only while its tenant's KEK is still
    /// not to be had, as `heard` says (TASK-411.13).
    async fn collect_held(
        &self,
        bucket: &impl Bucket,
        cell: Cell,
        taken: &LeaseTaking,
        recorded: Option<Passed>,
        heard: &Heard,
        listed: Option<Listed>,
    ) -> Result<(Report, Option<Passed>), Error> {
        if let Some(listed) = &listed
            && let Some(passed) = recorded.filter(|passed| passed.holds_for(listed, &self.options))
            && (passed.uncompacted == 0 || !heard.to_be_had(self.kms.as_ref(), cell.tenant).await)
        {
            return self.skip(bucket, cell, taken, passed, listed).await;
        }
        let mut remembered = Remembered::default();
        let survey = remembered.survey(bucket, cell, taken.epoch, Writes::Some, listed);
        let Some(pass) = survey.await? else {
            return Ok((Report::default(), None));
        };
        let mut report = Report::default();
        if !self.renew(bucket, cell, taken).await? {
            return Ok((report, None));
        }
        let kms = self.kms.as_ref();
        let compacted =
            pass.compact(bucket, &self.options, &mut remembered.trailers, kms, &mut report).await?;
        let (doomed, deferred) = pass.fates(bucket, &self.options).await?;
        // A pass a later one took the lease from stops.
        if !self.holds(bucket, cell, taken).await? {
            return Ok((report, None));
        }
        // Many at once, in batches where the store deletes so, in any order: none of it is what a
        // live branch needs, whatever else goes (TASK-395.5).
        let keys: Vec<&str> = doomed.iter().map(|meta| meta.key.as_str()).collect();
        bucket::delete_all(bucket, &keys).await;
        report.deleted += keys.len();
        report.deleted_bytes += doomed.iter().map(|meta| meta.size).sum::<u64>();
        // Retired for good, marked first, once: its cell, dead, changes no more, and its next
        // passes skip it (TASK-395.14); a clone's made unmarked, its one writer done with it
        // (TASK-455). The cell's own references listed marked, written again since their
        // retirements, deleted (TASK-395.13).
        let (marked, unmarked): (Vec<_>, Vec<_>) = pass.retired().into_iter().partition(|r| r.1);
        let keys = |retired: &[(String, bool)]| -> Vec<String> {
            retired.iter().map(|(key, _)| key.clone()).collect()
        };
        let (marked, unmarked) = (keys(&marked), keys(&unmarked));
        branch::retire(bucket, &marked.iter().map(String::as_str).collect::<Vec<_>>()).await;
        bucket::delete_all(bucket, &unmarked.iter().map(String::as_str).collect::<Vec<_>>()).await;
        let stale: Vec<&str> = pass.recording.stale.iter().map(String::as_str).collect();
        bucket::delete_all(bucket, &stale).await;
        report.retired += marked.len() + unmarked.len() + stale.len();
        // cladefs: a dead cell nothing will need again goes whole, its retirements made: its
        // records after the rest, so that a pass stopping between finds them and goes on, and its
        // lease last (TASK-455). It records nothing: nothing is left to record it in.
        if let Some(removal) = pass.removal(&doomed, &deferred) {
            report.removed += removal.remove(bucket).await;
            report.deleted += removal.len();
            report.deleted_bytes += removal.bytes();
            return Ok((report, None));
        }
        // What the cell takes now: as listed, less what went, with what it compacted to.
        let mut usage = pass.recording.usage.clone();
        doomed.iter().for_each(|meta| usage.remove(&meta.key, meta.size));
        compacted.iter().for_each(|(key, size)| usage.add(key, *size));
        let decided = Decided::by(&self.options);
        // cladefs: what it left encrypted uncompacted went by its KMS, outside the cell: recorded,
        // so that the next skips the cell only while the KEK is still not to be had (TASK-411.13).
        let uncompacted = report.uncompacted as u64;
        let listing = pass.recorded_listing(&doomed, &deferred);
        Ok((report, Some(Passed { listing, decided, deferred, usage, uncompacted })))
    }

    /// cladefs: a pass over `cell`, holding its lease as `taken` says, whose listing, `listed`, is
    /// the one the last pass to end, which recorded `passed`, decided by (TASK-395.6): it decides
    /// as that one did, everything a pass decides by being under the cell's prefix (doc-7, Passes
    /// that skip), whatever process that pass was of. It recovers nothing, and deletes what that
    /// pass deferred to a grace period, once due, as that pass would have, slow; and then records
    /// that a pass is to decide anew, as what goes once those are gone it never worked out: the
    /// moves of an object gone among them. It retires no reference again: that pass's retirements
    /// were marked, and one a writer whose read of the child's record came before writes again
    /// after, the pass over its parent's cell deletes (TASK-395.14). An unchanged dead cell costs
    /// a pass its lease and its listing. What it records: nothing new, if it deleted nothing.
    /// cladefs: it says what that pass left encrypted uncompacted, as left still (TASK-411.13).
    async fn skip(
        &self,
        bucket: &impl Bucket,
        cell: Cell,
        taken: &LeaseTaking,
        passed: Passed,
        listed: &Listed,
    ) -> Result<(Report, Option<Passed>), Error> {
        let uncompacted = passed.uncompacted as usize;
        let locked = usize::from(uncompacted > 0);
        let skipped = Report { skipped: 1, locked, uncompacted, ..Report::default() };
        let now = bucket.now();
        let (due, deferred): (Vec<Deferred>, Vec<Deferred>) =
            passed.deferred.iter().cloned().partition(|deferred| deferred.due <= now);
        if due.is_empty() {
            return Ok((skipped, None));
        }
        if !self.holds(bucket, cell, taken).await? {
            return Ok((skipped, None));
        }
        let keys: Vec<&str> = due.iter().map(|due| due.key.as_str()).collect();
        bucket::delete_all(bucket, &keys).await;
        let mut report = Report { deleted: keys.len(), ..skipped };
        let mut usage = passed.usage.clone();
        for key in keys.iter().copied() {
            if let Some(meta) = listed.objects.get(key) {
                usage.remove(key, meta.size);
                report.deleted_bytes += meta.size;
            }
        }
        Ok((report, Some(Passed { listing: None, deferred, usage, ..passed })))
    }

    /// cladefs: takes `cell`'s lease for a pass of `holder`, holding it as the collector's
    /// [`Holding`] says, at the epoch after the last pass's, fencing that pass, if the cell has
    /// anything a pass goes by ([`passable`]): asked only where it has no lease yet, as a pass of
    /// this format that took one asked it then, and the format of a cell's objects never changes
    /// (AGENTS.md: no migrations). A lease of another format, [`Error::Format`]. Unless another
    /// holds it, unexpired ([`crate::Left::Held`]), or one ended since the sweep began
    /// ([`crate::Left::Swept`]): then it leaves the cell to that pass, writing nothing.
    async fn take(
        &self,
        bucket: &impl Bucket,
        cell: Cell,
        holder: u64,
        found: Option<bucket::Object>,
    ) -> Result<Option<LeaseTaking>, Error> {
        if found.is_none() && !passable(bucket, cell).await? {
            return Ok(None);
        }
        passed::take(bucket, cell, found, holder, self.holding).await.map(Some)
    }

    /// cladefs: whether the pass that took `cell`'s lease as `taken` says holds it still, renewing it
    /// for as long again as a pass holds it, if it does: what a pass checks before it deletes.
    async fn holds(
        &self,
        bucket: &impl Bucket,
        cell: Cell,
        taken: &LeaseTaking,
    ) -> Result<bool, Error> {
        match self.holding.lease {
            0 => Ok(!fenced(bucket, cell, taken.epoch).await?),
            _ => self.renew(bucket, cell, taken).await,
        }
    }

    /// cladefs: renews `cell`'s lease for the pass that took it as `taken` says, for as long again as
    /// a pass holds it, if it holds it still: whether it does. A collector holding cells for no
    /// time writes nothing, and goes on: it checks before it deletes ([`Collector::holds`]).
    async fn renew(
        &self,
        bucket: &impl Bucket,
        cell: Cell,
        taken: &LeaseTaking,
    ) -> Result<bool, Error> {
        if self.holding.lease == 0 {
            return Ok(true);
        }
        let until = bucket.now().saturating_add(self.holding.lease);
        passed::change(bucket, cell, taken, |found| CollectorLease { until, ..found }).await
    }

    /// cladefs: lets `cell`'s lease go, which the pass that took it as `taken` says holds, if it
    /// does, as the pass `ended`: recording what it went through to, if anything, in the same
    /// write. Other passes take the cell at once after it, and a sweep that began before a pass
    /// went through leaves the cell to it. A collector holding cells for no time writes only what a
    /// pass records: its passes hold nothing.
    async fn release(
        &self,
        bucket: &impl Bucket,
        cell: Cell,
        taken: &LeaseTaking,
        ended: PassEnd,
    ) -> Result<(), Error> {
        let holds = self.holding.lease != 0;
        let (went_through, passed) = match ended {
            PassEnd::Through(passed) => (true, passed),
            PassEnd::Failed => (false, None),
        };
        if !holds && passed.is_none() {
            return Ok(());
        }
        let now = bucket.now();
        let release = |found: CollectorLease| {
            let ended = if holds && went_through { now } else { found.ended };
            let until = if holds { 0 } else { found.until };
            let passed = passed.clone().or(found.passed);
            CollectorLease { until, ended, passed, ..found }
        };
        passed::change(bucket, cell, taken, release).await.map(drop)
    }

    /// cladefs: one mark-only pass over `cell`, as `holder`, which must differ from every other
    /// pass's over the cell: a pass but for deleting, which writes what it would delete to its
    /// marks instead (TASK-264), and retires no reference. It never skips its cell, nor records
    /// what it found: what it would delete it writes to its marks.
    ///
    /// It takes the cell's collector lease, as a pass that deletes does (doc-7, Garbage collection,
    /// step 1). Its marks are ordered by it, each pass's epoch its own and later than every pass's
    /// over the cell before, so that readers, and the next mark-only pass, find the newest by their
    /// keys, and no two passes' marks share a key. A pass that deletes, which it then fences,
    /// deletes nothing, and the next deletes what it would have: a pass fenced is late, never
    /// wrong. And it stops, as a pass that deletes does (TASK-180), where it finds lost what a live
    /// branch needs once a pass that deletes took the lease from it, deleting since what it read
    /// live; a pass not holding the lease could not tell that from a bug. Fenced, it writes no
    /// marks, as a pass fenced deletes nothing: what it read may be older than what a later pass
    /// did. Compacting, which writes only create-only moves that other passes' stand beside, needs
    /// the lease no more than before.
    pub async fn mark_cell(
        &mut self,
        bucket: &impl Bucket,
        cell: Cell,
        holder: u64,
    ) -> Result<Marking, Error> {
        let found = passed::found(bucket, cell).await;
        let Some(taken) = self.take(bucket, cell, holder, found).await? else {
            return Ok(Marking::default());
        };
        let marked = self.mark_held(bucket, cell, &taken).await;
        let ended = if marked.is_ok() { PassEnd::Through(None) } else { PassEnd::Failed };
        self.release(bucket, cell, &taken, ended).await?;
        marked
    }

    /// cladefs: a mark-only pass over `cell`, which holds its lease as `taken` says.
    async fn mark_held(
        &self,
        bucket: &impl Bucket,
        cell: Cell,
        taken: &LeaseTaking,
    ) -> Result<Marking, Error> {
        let epoch = taken.epoch;
        let listed = first_listing(bucket, cell).await;
        let mut remembered = Remembered::default();
        let Some(pass) = remembered.survey(bucket, cell, epoch, Writes::Some, listed).await? else {
            return Ok(Marking::default());
        };
        let mut report = Report::default();
        if !self.renew(bucket, cell, taken).await? {
            return Ok(Marking::default());
        }
        let (trailers, kms) = (&mut remembered.trailers, self.kms.as_ref());
        pass.compact(bucket, &self.options, trailers, kms, &mut report).await?;
        let doomed = pass.doomed(bucket, &self.options).await?;
        if !self.holds(bucket, cell, taken).await? {
            return Ok(Marking { report, ..Marking::default() });
        }
        let resurrected =
            resurrected(bucket, cell, epoch, &self.options, &pass.objects, &doomed).await;
        let marked = doomed.into_iter().map(Marked::of).collect();
        let marks = Marks { epoch, grace: self.options.grace, marked };
        // Readers watch the cells' marks only while the bucket says it is marked: first.
        bucket::create(bucket, layout::MARKING, b"marking".to_vec()).await;
        // The epoch is this pass's taking of the lease alone: nothing else is at its key.
        bucket::create(bucket, &layout::marks(cell, epoch), layout::encode(&marks)).await;
        Ok(Marking { report, marks: Some(marks), resurrected })
    }

    /// cladefs: what a pass over `cell` would delete, writing nothing (TASK-264): not taking the
    /// cell's collector lease, which would fence a pass that deletes, running meanwhile, out of its
    /// deletes. It compacts nothing, and so finds doomed none of what a pass would delete only once
    /// it compacted it, a grace period after: no more than a pass would delete, and less. What a
    /// pass would compact, it says apart ([`DryRun::compactable`]), and what references it would
    /// retire ([`DryRun::retired`]). It goes by the lease's epoch as it starts, as a pass by the
    /// epoch it takes, to tell whether a pass that deletes ran meanwhile ([`DryRun::fenced`]).
    /// Where a pass would give up on a fork of a point deleted, writing its record, it takes the
    /// fork as given up on, as a pass mostly finds it. It goes over the cell in full, whatever the
    /// last pass recorded.
    pub async fn dry_run_cell(&self, bucket: &impl Bucket, cell: Cell) -> Result<DryRun, Error> {
        let epoch = passed::epoch(bucket, cell).await?;
        let listed = first_listing(bucket, cell).await;
        let mut remembered = Remembered::default();
        let Some(pass) = remembered.survey(bucket, cell, epoch, Writes::None, listed).await? else {
            return Ok(DryRun { fenced: true, ..DryRun::default() });
        };
        let doomed = pass.doomed(bucket, &self.options).await?.into_iter().cloned().collect();
        let compactable = pass.compactable(bucket, &self.options, &mut remembered.trailers).await?;
        let compactable = compactable.into_iter().map(|object| object.meta.clone()).collect();
        let mut retired: Vec<String> = pass.retired().into_iter().map(|(key, _)| key).collect();
        retired.extend(pass.recording.stale.iter().cloned());
        Ok(DryRun { doomed, compactable, retired, fenced: fenced(bucket, cell, epoch).await? })
    }
}

/// cladefs: what a look at a cell found, before its pass took the lease ([`Collector::look`]).
enum Looked {
    /// The cell as the last pass in full recorded it, nothing due: the pass that skips it,
    /// writing nothing.
    Unchanged(Report),
    /// Changed, or something due: its listing, and the lease's epoch the look read.
    Changed(Listed, u64),
    /// The last pass recorded no listing: nothing to look at.
    Unrecorded,
}

/// cladefs: how a pass over a cell ended, which its letting go of the cell's lease writes
/// ([`Collector::release`]).
enum PassEnd {
    /// It went through, deleting what it found to, recording this, if anything.
    Through(Option<Passed>),
    /// It failed: the next pass decides anew.
    Failed,
}

impl Remembered {
    /// What a pass over `cell` of the cell's collector lease's `epoch` finds: every object of the
    /// cell `listed` before it read anything else ([`first_listing`]), and what the live branches
    /// and the fork points referred to need of them; none if a later pass took the lease from it as
    /// it found what a live branch needs lost (TASK-180). It writes what giving up on forks needs,
    /// if `writes` says so.
    async fn survey(
        &mut self,
        bucket: &impl Bucket,
        cell: Cell,
        epoch: u64,
        writes: Writes,
        listed: Option<Listed>,
    ) -> Result<Option<Pass>, Error> {
        let early = listed.is_some();
        // Without the rule that they are listed after the states, with the candidates, and never
        // again, as a test shows it needed.
        let listed_first = match rules().references_relisted {
            true => None,
            false => Some(referenced(bucket, cell).await),
        };
        // What the listing of the cell holds serves the first listing of the records, and of each
        // branch's checkpoints, that the pass makes (TASK-395.16).
        let objects = listed.as_ref().map(|listed| &listed.objects);
        let checkpoints = Checkpoints::new(bucket, CHECKPOINTS_KEPT).listed(objects);
        let mut records = self.records(bucket, cell, objects, &BTreeMap::new()).await?;
        let mut live = Live::default();
        let read =
            match self.read(bucket, cell, &checkpoints, &mut records, &mut live, writes).await {
                Ok(()) => {
                    let referenced = match listed_first {
                        Some(listed) => listed,
                        None => referenced(bucket, cell).await,
                    };
                    let by = Points::Referred;
                    keep_points(bucket, &checkpoints, &mut live, &referenced, by)
                        .await
                        .map(|()| referenced)
                }
                Err(error) => Err(error),
            };
        let referenced = match read {
            Ok(referenced) => referenced,
            // Fenced, it read what the pass that fenced it, deleting since, may have found no live
            // branch needs: a point deleted, read live before, and its fork being made, read after
            // (TASK-180). It stops, as it would before deleting.
            Err(Error::Lost(_))
                if rules().passes_fenced_stop && fenced(bucket, cell, epoch).await? =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        drop(checkpoints);
        // What a pass listing the cell as it did decides as it did, unless it went by more than the
        // cell: a point of another cell's record, read for a clone being made.
        let forking_elsewhere = (records.iter()).any(|(_, record)| {
            !record.deleted && record.forking().is_some_and(|point| point.cell != cell)
        });
        let settled = early && !live.unsettled && !forking_elsewhere;
        let listed = match listed {
            Some(listed) => listed,
            None => passed::list(bucket, cell).await,
        };
        let Listed { objects, digest, usage, stale, references } = listed;
        self.trailers.retain(&objects);
        let recording = Recording { digest: settled.then_some(digest), usage, stale, references };
        let found = (objects, records, live, referenced, recording);
        Ok(Some(Pass::new(bucket, (cell, epoch), found).await?))
    }

    /// Reads the states of `cell`'s live branches into `live`, and of the fork points of their forks
    /// to other cells, then lists `records` again, going on with the new ones, while a state it
    /// read is from a checkpoint, until no new one appears. A pass fenced as it found what a live
    /// branch needs lost stops, where its caller sees it.
    async fn read(
        &mut self,
        bucket: &impl Bucket,
        cell: Cell,
        checkpoints: &Checkpoints<'_, impl Bucket>,
        records: &mut BTreeMap<Branch, Record>,
        live: &mut Live,
        writes: Writes,
    ) -> Result<(), Error> {
        loop {
            if rules().collector_abandons {
                abandon(bucket, records, writes).await?;
            }
            let (checkpointed, points) =
                recover_live(bucket, cell, checkpoints, records, live, writes).await?;
            keep_points(bucket, checkpoints, live, &points, Points::Forked).await?;
            if !checkpointed || !rules().records_relisted {
                return Ok(());
            }
            let again = self.records(bucket, cell, None, records).await?;
            let found = again.keys().any(|branch| !records.contains_key(branch));
            records.extend(again);
            if !found {
                return Ok(());
            }
        }
    }

    /// The records of `cell`'s branches, read many at once, but those read before at the versions
    /// listed: listed until a listing finds none of a branch not in `before`, the records the pass
    /// read before, nor any not read by the listings before it. A deleted branch's children got
    /// theirs before it was marked deleted, so listing them again after reading them finds those of
    /// any it read deleted. cladefs: the first listing is `listed`, the pass's listing of the whole
    /// cell, if it has one, taken before it read anything (TASK-395.16): a LIST less, as the model's
    /// pass reads its records as it lists its candidates (Bucket.tla, `GcStart`); the listings
    /// after are of the records alone.
    async fn records(
        &mut self,
        bucket: &impl Bucket,
        cell: Cell,
        listed: Option<&BTreeMap<String, Meta>>,
        before: &BTreeMap<Branch, Record>,
    ) -> Result<BTreeMap<Branch, Record>, Error> {
        let mut records = BTreeMap::new();
        let mut first = listed.map(|objects| bucket::under(objects, &layout::records(cell)));
        loop {
            let listing = match first.take() {
                Some(listed) => listed,
                None => bucket::list(bucket, &layout::records(cell)).await,
            };
            let listed = (listing.into_iter()).filter_map(|meta| match layout::kind(&meta.key) {
                Kind::Record { branch } if !records.contains_key(&branch) => Some((branch, meta)),
                _ => None,
            });
            let known = &self.records;
            let reads = futures::stream::iter(listed)
                .map(|(branch, meta)| async move {
                    (branch, read_record(bucket, known, branch, &meta).await, meta.key)
                })
                .buffered(RECORDS_AT_ONCE)
                .collect::<Vec<_>>()
                .await;
            let mut new = false;
            for (branch, read, key) in reads {
                let Some((version, record)) = read? else { continue };
                match version {
                    Some(version) => self.records.insert(key, (version, record.clone())),
                    None => self.records.remove(&key),
                };
                records.insert(branch, record);
                new |= !before.contains_key(&branch);
            }
            if !new {
                // Those gone, which the collector deleted, it forgets.
                self.records.retain(|key, _| match layout::kind(key) {
                    Kind::Record { branch } => records.contains_key(&branch),
                    _ => false,
                });
                return Ok(records);
            }
        }
    }
}

/// cladefs: whether a pass over `cell` has anything to go by, checked before it takes the cell's
/// lease, which would be the cell's one object otherwise: the format of the cell's object, a GET,
/// which a pass of another format refuses ([`Error::Format`]) before writing anything. A cell with
/// none, its branches made without one, its first record's format; and, holding no record, whether
/// it holds anything (a reference, what a dead cell left).
async fn passable(bucket: &impl Bucket, cell: Cell) -> Result<bool, Error> {
    if crate::cell::read(bucket, cell).await?.is_some() {
        return Ok(true);
    }
    let listed = bucket::list(bucket, &layout::records(cell)).await;
    for meta in &listed {
        let Kind::Record { branch } = layout::kind(&meta.key) else { continue };
        if let Some(object) = bucket::get(bucket, &meta.key).await {
            Record::decode(branch, &object.bytes)?;
            return Ok(true);
        }
    }
    let listing = bucket::list_delimited(bucket, &cell.prefix()).await;
    Ok(!listing.objects.is_empty() || !listing.prefixes.is_empty())
}

/// cladefs: every object of `cell`, listed before a pass reads anything else of it, its candidates
/// (rule `candidates_first`). Without the rule, as a test shows it needed, none: the pass lists the
/// cell after the states, and no pass skips by a listing ([`Collector::collect_cell`]).
async fn first_listing(bucket: &impl Bucket, cell: Cell) -> Option<Listed> {
    match rules().candidates_first {
        true => Some(passed::list(bucket, cell).await),
        false => None,
    }
}

/// cladefs: whether a pass writes to the bucket what giving up on forks needs: a dry run writes
/// nothing (TASK-264).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Writes {
    Some,
    None,
}

/// cladefs: the objects the newest marks of `cell` before those of `epoch` named, that `objects`,
/// listed by the mark-only pass of `epoch`, holds still, the same objects, and that `doomed`, what
/// it found no live branch needs, does not: each warned of, loudly. No correct fork or recovery
/// reaches what no live branch reached, so each is a bug in the collector, which a pass deleting
/// would have lost. Marks of a shorter grace period than `options`' are compared with none: what a
/// compacted object needs to go, its moves aged, takes longer by a longer one. Marks it can't read,
/// it warns of.
async fn resurrected(
    bucket: &impl Bucket,
    cell: Cell,
    epoch: u64,
    options: &Options,
    objects: &BTreeMap<String, Meta>,
    doomed: &[&Meta],
) -> Vec<String> {
    let listed = bucket::list(bucket, &layout::marks_of(cell)).await;
    let before = listed.iter().rev().find(
        |meta| matches!(layout::kind(&meta.key), Kind::Marks { epoch: marked, .. } if marked < epoch),
    );
    let Some(object) = (match before {
        Some(before) => bucket::get(bucket, &before.key).await,
        None => None,
    }) else {
        return Vec::new();
    };
    let marks: Marks = match layout::decode(&object.bytes) {
        Ok(marks) => marks,
        Err(error) => {
            let key = &object.meta.key;
            log::warn!(key, error:%; "cladefs-gc: {key}, the marks before, unread: {error}");
            return Vec::new();
        }
    };
    if marks.grace < options.grace {
        return Vec::new();
    }
    let doomed: BTreeSet<&str> = doomed.iter().map(|meta| meta.key.as_str()).collect();
    let mut resurrected = Vec::new();
    for marked in &marks.marked {
        let key = marked.key.as_str();
        if doomed.contains(key) || !objects.get(key).is_some_and(|meta| marked.is(meta)) {
            continue;
        }
        let (kind, before) = (layout::kind(key), marks.epoch);
        log::warn!(
            key, kind:?, marks = before, epoch;
            "cladefs-gc: RESURRECTED {key}: the mark-only pass {before:x} found no live branch needs \
             it, and the pass {epoch:x} finds one does: a bug in the collector, which deleting \
             would have lost it to"
        );
        resurrected.push(marked.key.clone());
    }
    resurrected
}

/// Whether a later pass over `cell` took the cell's collector lease from the pass that took it at
/// `epoch`.
async fn fenced(bucket: &impl Bucket, cell: Cell, epoch: u64) -> Result<bool, Error> {
    Ok(passed::epoch(bucket, cell).await? != epoch)
}

/// Gives up on the forks being made of points that were deleted, as `records` say, which it says
/// are deleted then, or made, if that came first ([`branch::abandon`]); without `writes`, takes them
/// as deleted, writing nothing. cladefs: a point of another cell's than the pass's, it reads where it
/// is.
async fn abandon(
    bucket: &impl Bucket,
    records: &mut BTreeMap<Branch, Record>,
    writes: Writes,
) -> Result<(), Error> {
    let forking: Vec<(Branch, Branch)> = (records.iter())
        .filter(|(_, record)| !record.deleted)
        .filter_map(|(&child, record)| Some((child, record.forking()?)))
        .collect();
    for (child, point) in forking {
        let gone = match records.get(&point) {
            Some(record) => record.deleted,
            None if point.cell == child.cell => true,
            None => gone(bucket, point).await?,
        };
        if !gone {
            continue;
        }
        let record = match writes {
            Writes::Some => branch::abandon(bucket, child).await?,
            Writes::None => Record { deleted: true, ..records[&child].clone() },
        };
        records.insert(child, record);
    }
    Ok(())
}

/// cladefs: whether `point`, of another cell's, is gone: its record deleted, or none.
async fn gone(bucket: &impl Bucket, point: Branch) -> Result<bool, Error> {
    Ok(branch::record(bucket, point).await?.is_none_or(|record| record.deleted))
}

/// The record of `branch` at the key of `meta`, which a listing gave, if there is one, and the
/// version read: as `known` has it, if it read it at the version listed; or read now, unless it is
/// of another format ([`Error::Format`]), which stops the pass: what that branch needs, it can't
/// read.
async fn read_record(
    bucket: &impl Bucket,
    known: &BTreeMap<String, (String, Record)>,
    branch: Branch,
    meta: &Meta,
) -> Result<Option<(Option<String>, Record)>, Error> {
    if let Some(listed) = &meta.version.e_tag
        && let Some((version, record)) = known.get(&meta.key)
        && version == listed
    {
        return Ok(Some((Some(version.clone()), record.clone())));
    }
    let Some(object) = bucket::get(bucket, &meta.key).await else { return Ok(None) };
    Ok(Some((object.meta.version.e_tag, Record::decode(branch, &object.bytes)?)))
}

/// cladefs: the fork points the references under `cell` name, by delimiter listings: the parents
/// with references, then each one's fork points, none of their children (doc-7, The layout). Once:
/// a listing's pages are read one after another, but what it shows is a fork point's prefix, not a
/// reference, and a clone's reference and its point's own share it, `<C>refs/<origin>/<at>/`, the
/// clone's written before the point's is retired; so the prefix holds a key all along, and a
/// listing reaching it at any time shows it (a listing of the references whole would not: the
/// clone's behind its cursor, the point's ahead, retired).
async fn referenced(bucket: &impl Bucket, cell: Cell) -> BTreeSet<Origin> {
    let mut points = BTreeSet::new();
    for prefix in bucket::list_delimited(bucket, &layout::references(cell)).await.prefixes {
        let Some(parent) = layout::referenced_parent(cell, &prefix) else { continue };
        for at in bucket::list_delimited(bucket, &prefix).await.prefixes {
            if let Some(at) = layout::referenced_at(parent, &at) {
                points.insert(Origin { parent, at });
            }
        }
    }
    points
}

/// cladefs: what keeps a fork point a pass keeps ([`keep_points`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Points {
    /// A fork of a live branch's state to a child in another cell, which may have no reference
    /// yet: its state lost is what the fork's child needs lost, which stops the pass.
    Forked,
    /// A reference: its state lost stops the pass if a child named under it lives, its record, read
    /// then, naming the fork point as its origin; else nothing was lost, the children deleted,
    /// being made of a point deleted, claims, or having no record.
    Referred,
}

/// cladefs: keeps in `live` the states of the fork points `points` it keeps none of yet, each
/// recovered once, however many forks name it, several at once ([`Live::keep_point`]): the
/// extents each names, and what recovering it read (doc-7, A pass over a cell, step 5). Those of
/// other cells are read there.
async fn keep_points(
    bucket: &impl Bucket,
    checkpoints: &Checkpoints<'_, impl Bucket>,
    live: &mut Live,
    points: &BTreeSet<Origin>,
    by: Points,
) -> Result<(), Error> {
    let unkept: Vec<Origin> =
        points.iter().filter(|&origin| live.points.insert(*origin)).copied().collect();
    let recoveries = futures::stream::iter(unkept)
        .map(|origin| async move {
            (origin, recovery::recover_at(checkpoints, origin.parent, origin.at).await)
        })
        .buffer_unordered(RECOVERIES_AT_ONCE)
        .collect::<Vec<_>>()
        .await;
    for (origin, recovered) in recoveries {
        match recovered {
            Ok((state, touched)) => live.keep_point(&state, touched),
            Err(Error::Lost(lost)) if by == Points::Referred => {
                if let Some(child) = needing(bucket, origin).await? {
                    return Err(Error::Lost(format!("{lost}, which {child} lives on")));
                }
                // Decided by its children's records, in other cells.
                live.unsettled = true;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// cladefs: a child the references of the fork point `origin` name that lives on it: its record not
/// deleted, a fork's or a point's, naming `origin` as its origin. Its references listed, and their
/// children's records read, in their cells: asked only of a fork point whose state is lost.
async fn needing(bucket: &impl Bucket, origin: Origin) -> Result<Option<Branch>, Error> {
    let listed = bucket::list(bucket, &layout::fork_point(origin.parent, origin.at)).await;
    for meta in listed {
        let Kind::Reference { child, .. } = layout::kind(&meta.key) else { continue };
        let Some(record) = branch::record(bucket, child).await? else { continue };
        let lives = match record.kind {
            record::Kind::Fork(from) | record::Kind::Clone(from) | record::Kind::Point(from) => {
                from == origin
            }
            _ => false,
        };
        if lives && !record.deleted {
            return Ok(Some(child));
        }
    }
    Ok(None)
}

/// Recovers the live branches of `cell` `live` has not yet, several at once, folding them into it:
/// those whose records are not deleted, and the children of forks in their states that have no
/// records yet, of the cell. Returns whether one has a checkpoint of its own, which a fork deleted
/// may have gone from; and the fork points of the forks in their states to children of other cells,
/// whose states the pass keeps, as those children may have no references yet. Forks of a branch at
/// one segment are recovered together, those without checkpoints of their own starting from the
/// branch's state there, read once for them ([`Checkpoints`]); and the states are folded into what
/// the pass needs of them as they are recovered, what forks inherited together once for them all,
/// rather than each fork's copy of it ([`Layers`]).
///
/// A branch is recovered once by each origin it is named by: a child its parent's state names, that
/// fork refused, has its record from another branch whose fork to its ID came first, a restore's,
/// which listing the records again may find only after the child was recovered by the state; what
/// the record's origin needs is what the child does (TASK-173).
///
/// A failed fork's claim no writer has, whose recovery finds what it names lost, it gives up,
/// writing its record so if `writes` says so; and so a fork of a point still being made, its point
/// deleted, read again then (TASK-395.4): what the pass read of it before, the point live, may be
/// older than what recovering it read.
async fn recover_live(
    bucket: &impl Bucket,
    cell: Cell,
    checkpoints: &Checkpoints<'_, impl Bucket>,
    records: &BTreeMap<Branch, Record>,
    live: &mut Live,
    writes: Writes,
) -> Result<(bool, BTreeSet<Origin>), Error> {
    let mut checkpointed = false;
    let mut points = BTreeSet::new();
    // Forks made at the same segment next to each other, in the same rounds.
    let mut queue: BTreeSet<(Option<Origin>, Branch)> = records
        .iter()
        .filter(|(_, record)| !record.deleted)
        .map(|(&branch, record)| (record.origin(), branch))
        .filter(|&(origin, branch)| !live.recovered(branch, origin))
        .collect();
    while !queue.is_empty() {
        // Each once by each origin, queued by its record, or by its parent's state, or both.
        let round: Vec<_> = std::iter::from_fn(|| queue.pop_first())
            .filter(|&(origin, branch)| live.take(branch, origin))
            .take(RECOVERIES_AT_ONCE)
            .collect();
        let recoveries = round.into_iter().map(|(origin, branch)| async move {
            // Recovered from its head, read after the listing: what its writer may name
            // ([`behind`]), and what it may yet name ([`Ended::ahead`]).
            (branch, recovery::recover_layers(checkpoints, branch, origin).await)
        });
        let mut inherited = InheritedStates::default();
        for (branch, recovered) in futures::future::join_all(recoveries).await {
            let recovered = match recovered {
                Ok(recovered) => recovered,
                Err(Error::Lost(lost)) => match lost_is_ok(bucket, branch, writes).await? {
                    // Decided by what it read again, a point's record in another cell among it.
                    true => {
                        live.unsettled = true;
                        continue;
                    }
                    false => return Err(Error::Lost(lost)),
                },
                Err(error) => return Err(error),
            };
            // A child with a record is queued by it, as it says: one whose ID another branch took
            // as the fork was made has another origin than its parent's state says (TASK-79.82).
            // cladefs: one of another cell's, its fork point kept, which its reference, if it has one
            // yet, keeps too: the child's cell's pass recovers it (TASK-395.4).
            for fork in &recovered.state.forks {
                let origin = Origin { parent: branch, at: fork.at };
                if !rules().forks_from_states {
                    continue;
                }
                if fork.child.cell != cell {
                    points.insert(origin);
                    continue;
                }
                if records.contains_key(&fork.child) || live.recovered(fork.child, Some(origin)) {
                    continue;
                }
                queue.insert((Some(origin), fork.child));
            }
            checkpointed |= recovered.checkpoint().is_some();
            live.add(recovered, &mut inherited);
        }
        live.inherit(inherited);
    }
    Ok((checkpointed, points))
}

/// Whether recovering `branch` finding what it needs lost lost nothing, as its record, read again,
/// says: deleted since it was read, what it needed may be gone; a failed fork's claim no writer
/// has, which nothing ever read, given up on; cladefs: a fork of a point still being made, its point
/// deleted, given up on (TASK-395.4), unless it was made first, as [`abandon`] would have: the rule
/// `collector_abandons` both. Gives up on them, writing their
/// records so, if `writes` says so.
async fn lost_is_ok(bucket: &impl Bucket, branch: Branch, writes: Writes) -> Result<bool, Error> {
    let Some(record) = branch::record(bucket, branch).await? else { return Ok(true) };
    if record.deleted {
        return Ok(true);
    }
    if branch::lost_is_ok(bucket, branch, &record).await? {
        if writes == Writes::Some {
            branch::abandon_claim(bucket, branch).await?;
        }
        return Ok(true);
    }
    let Some(point) = record.forking().filter(|_| rules().collector_abandons) else {
        return Ok(false);
    };
    if !gone(bucket, point).await? {
        return Ok(false);
    }
    match writes {
        Writes::Some => Ok(branch::abandon(bucket, branch).await?.deleted),
        Writes::None => Ok(true),
    }
}

/// Whether the writer of the branch `recovered` is of may name the extents it holds anywhere on
/// their ways from where they were sealed, before the moves its state followed: unless its state
/// is from a checkpoint its head names of the writer holding the head. The extents it inherited,
/// its writer may name so whatever this says ([`Live::inherit`]).
fn behind(recovered: &Recovered<Layers>) -> bool {
    let epoch = recovered.epoch();
    rules().ways_from_seals && recovered.checkpoint().is_none_or(|(_, by)| by != epoch)
}

/// What a pass needs of the live branches' states, folded in as they are recovered.
#[derive(Default)]
struct Live {
    /// Those recovered, or being, by the origins they were recovered from: each once by each.
    taken: BTreeSet<(Branch, Option<Origin>)>,
    /// Where each recovery of each ended.
    branches: BTreeMap<Branch, Vec<Ended>>,
    /// What recovering them read.
    touched: BTreeSet<String>,
    /// Where the extents they name are: each place once, however many of them name it there.
    located: BTreeMap<ExtentId, BTreeSet<Location>>,
    /// The moves they name extents in whose writers may name them anywhere on their ways from
    /// where they were sealed ([`behind`]).
    behind: BTreeSet<String>,
    /// cladefs: whether each object [`Live::behind_at`] was asked of is moves, by its key: each
    /// key parsed once, not once an extent in it (TASK-395.12). Parsing them took most of a pass
    /// over a cell whose branch was forked at many points, each a state of every extent of it.
    moves: rustc_hash::FxHashMap<String, bool>,
    /// cladefs: the fork points whose states it keeps, each once ([`Live::keep_point`]).
    points: BTreeSet<Origin>,
    /// cladefs: whether what it found went by more than the cell holds, which may change with nothing
    /// of the cell's changing: a recovery that found what it needs lost, and then nothing lost; a
    /// fork point found lost, its children's records read (TASK-395.6).
    unsettled: bool,
}

/// What deciding what goes needs of a live branch's state, its extents but: its head, as its
/// recovery read it after the listing (TASK-407).
struct Ended {
    /// The number of segments it names.
    end: u64,
    /// Its epoch: of the writer holding it, or none's.
    epoch: u64,
    /// Its newest checkpoint, by its number and its writer's epoch.
    checkpoint: Option<(u64, u64)>,
}

impl Ended {
    /// Whether the head may yet name the object of `kind`, which it names not (rule
    /// `collector_epochs`): of its epoch, or a later one, a segment or an extent of its own at or
    /// past its end, or a checkpoint newer than its own. An object of an earlier epoch, its
    /// writer's swaps all fail: no head ever names it.
    fn ahead(&self, kind: Kind<'_>) -> bool {
        rules().collector_epochs
            && match kind {
                Kind::Segment { segment, epoch, .. } | Kind::Extent { segment, epoch, .. } => {
                    epoch >= self.epoch && segment >= self.end
                }
                Kind::Checkpoint { segment, epoch, .. } => {
                    epoch >= self.epoch && self.checkpoint.is_none_or(|at| (segment, epoch) > at)
                }
                _ => false,
            }
    }
}

/// The states forks inherited, as their recoveries read them, each with the extents of it every
/// fork inheriting it holds no longer where it inherited it: what no fork of a round recovered
/// needs of it.
#[derive(Default)]
struct InheritedStates {
    states: Vec<(Arc<State>, BTreeSet<ExtentId>)>,
}

impl Live {
    /// Whether `branch` was recovered, or is being, from `origin`: or from any, without the rule
    /// that a branch is recovered by each origin it is named by (TASK-173).
    fn recovered(&self, branch: Branch, origin: Option<Origin>) -> bool {
        match rules().recoveries_by_origins {
            true => self.taken.contains(&(branch, origin)),
            false => self.taken.range((branch, None)..).next().is_some_and(|&(b, _)| b == branch),
        }
    }

    /// Takes the recovery of `branch` from `origin`, unless it was taken: returns whether it was
    /// not.
    fn take(&mut self, branch: Branch, origin: Option<Origin>) -> bool {
        !self.recovered(branch, origin) && self.taken.insert((branch, origin))
    }

    /// Folds in `recovered`, a live branch's: its own layer, and what it inherited into
    /// `inherited`, its round's, which [`Live::inherit`] folds in once for all its forks.
    fn add(&mut self, recovered: Recovered<Layers>, inherited: &mut InheritedStates) {
        let behind = behind(&recovered);
        let ended = Ended {
            end: recovered.end(),
            epoch: recovered.epoch(),
            checkpoint: recovered.checkpoint(),
        };
        let Recovered { state, touched, .. } = recovered;
        let State { branch, extents, .. } = state;
        let Layered { inherited: from, own, shadowed } = extents;
        self.touched.extend(touched);
        for (extent, location) in own {
            if behind {
                self.behind_at(&location);
            }
            self.located.entry(extent).or_default().insert(location);
        }
        if let Some(from) = from {
            match inherited.states.iter_mut().find(|(state, _)| Arc::ptr_eq(state, &from)) {
                Some((_, unheld)) => unheld.retain(|extent| shadowed.contains(extent)),
                None => inherited.states.push((from, shadowed)),
            }
        }
        self.branches.entry(branch).or_default().push(ended);
    }

    /// Folds in what forks inherited, `inherited`: each state once, but the extents of it none of
    /// them holds where they inherited them. Their writers may name those anywhere on their ways
    /// from where they were sealed: each recovered the parent's state as it was then.
    fn inherit(&mut self, inherited: InheritedStates) {
        let behind = rules().ways_from_seals;
        for (state, unheld) in inherited.states {
            for (extent, location) in &state.extents {
                if unheld.contains(extent) {
                    continue;
                }
                if behind {
                    self.behind_at(location);
                }
                let located = self.located.entry(*extent).or_default();
                if !located.contains(location) {
                    located.insert(location.clone());
                }
            }
        }
    }

    /// cladefs: keeps `state`, a fork point's, whole, for the forks of it to other cells, which
    /// `touched` was read to recover (TASK-395.4): the extents it names, along their ways from where
    /// they were sealed, as a child with no checkpoint of its own names them where it read them;
    /// and what recovering it read, through which such a child recovers, unless the rule that it is
    /// kept is off, as a test shows it needed. What its forks freed of it since is no matter: the
    /// collector is conservative.
    fn keep_point(&mut self, state: &State, touched: BTreeSet<String>) {
        if rules().fork_points_kept {
            self.touched.extend(touched);
        }
        let behind = rules().ways_from_seals;
        for (extent, location) in &state.extents {
            if behind {
                self.behind_at(location);
            }
            let located = self.located.entry(*extent).or_default();
            if !located.contains(location) {
                located.insert(location.clone());
            }
        }
    }

    /// Notes that a writer may name the extent at `location` anywhere on its way there from where
    /// it was sealed: in moves, if it is in some.
    fn behind_at(&mut self, location: &Location) {
        let key = &location.key;
        let moves = match self.moves.get(key) {
            Some(&moves) => moves,
            None => {
                let moves = matches!(layout::kind(key), Kind::Moves { .. });
                self.moves.insert(key.clone(), moves);
                moves
            }
        };
        if moves && !self.behind.contains(key) {
            self.behind.insert(key.clone());
        }
    }
}

/// Whether `extents`, live in an object of `len` bytes of data, take less than `options`' fraction
/// of it.
fn below(extents: &BTreeMap<ExtentId, Location>, options: &Options, len: u64) -> bool {
    let used: u64 = extents.values().map(|location| location.len).sum();
    (used as f64) < options.compact_below * len as f64
}

/// The trailers of the objects a pass may compact, by their keys, with the versions and sizes they
/// were listed at: read once each, as they never change, and forgotten once they are gone. The live
/// extents of an object are measured against its data, as its trailer says, which its listed size,
/// compressed (TASK-29), may be far below: before, a mostly dead segment whose data compressed was
/// never compacted (TASK-293). cladefs: and its trailer's encryption says whether compacting it
/// needs its key, read keyless (TASK-411.6).
#[derive(Default)]
struct Trailers(BTreeMap<String, (Version, u64, Frame)>);

impl Trailers {
    /// The frame of the object `meta` lists, as its trailer says, if it is still there.
    fn known(&self, meta: &Meta) -> Option<&Frame> {
        let (version, size, frame) = self.0.get(&meta.key)?;
        (*version == meta.version && *size == meta.size).then_some(frame)
    }

    /// Learns the trailers of the objects `metas` list, those not known yet, many at once.
    async fn learn(&mut self, bucket: &impl Bucket, metas: &[&Meta]) -> Result<(), Error> {
        // The reads' futures made first, as in `bucket::delete_all`.
        let unknown: Vec<&Meta> =
            metas.iter().copied().filter(|meta| self.known(meta).is_none()).collect();
        let reads: Vec<_> = unknown.into_iter().map(|meta| learned(bucket, meta)).collect();
        let reads = futures::stream::iter(reads)
            .buffer_unordered(TRAILERS_AT_ONCE)
            .collect::<Vec<_>>()
            .await;
        for (meta, frame) in reads {
            if let Some(frame) = frame? {
                self.0.insert(meta.key.clone(), (meta.version.clone(), meta.size, frame));
            }
        }
        Ok(())
    }

    /// Forgets those of objects no longer listed as they were.
    fn retain(&mut self, objects: &BTreeMap<String, Meta>) {
        self.0.retain(|key, (version, size, _)| {
            objects.get(key).is_some_and(|meta| meta.version == *version && meta.size == *size)
        });
    }
}

/// `meta`, and the frame of the object it lists ([`trailer`]).
async fn learned<'a>(
    bucket: &impl Bucket,
    meta: &'a Meta,
) -> (&'a Meta, Result<Option<Frame>, Error>) {
    (meta, trailer(bucket, meta).await)
}

/// The frame of the object `meta` lists, as its trailer says, if it is still there.
async fn trailer(bucket: &impl Bucket, meta: &Meta) -> Result<Option<Frame>, Error> {
    let range = ByteRange::Suffix(layout::TRAILER);
    let Some(trailer) = bucket::get_range(bucket, &meta.key, range).await? else {
        return Ok(None);
    };
    let frame = Frame::of_trailer(&trailer)?;
    if frame.size() != meta.size {
        let (key, size) = (&meta.key, meta.size);
        return Err(Error::Corrupt(format!(
            "{key}: {size} bytes, {} by its trailer",
            frame.size()
        )));
    }
    Ok(Some(frame))
}

/// An object a pass compacts ([`Pass::compactable`]): as listed, its live extents, and how it is
/// encrypted, as its trailer says.
struct Compactable<'a> {
    meta: &'a Meta,
    extents: &'a BTreeMap<ExtentId, Location>,
    encryption: Encryption,
}

/// Compacts the object at `key`, of which `extents` are live: copies them to its moves by the pass
/// of epoch `pass`, at a key of the pass's (TASK-407.1), unless it is gone. The moves' key and
/// size, if it did: those there first, if the pass created them before. cladefs:
/// it reads the object under `ring`'s keys of its branch, refusing one in the clear of a branch
/// encrypted, and encrypts its moves under the key the object was encrypted under, the DEK of the
/// same generation, with a salt of their own (TASK-411.6): every reader following the moves read
/// the object under that key, where a store opened before a newer generation was added holds none
/// of it. An object in the clear's moves are in the clear.
async fn compact_object(
    bucket: &impl Bucket,
    key: &str,
    extents: &BTreeMap<ExtentId, Location>,
    (ring, pass): (&Ring, u64),
) -> Result<Option<(String, u64)>, Error> {
    let moves = layout::moves(key, pass)?;
    let Some(object) = bucket::get(bucket, key).await else { return Ok(None) };
    let keys = keys::of_key(ring, key);
    // Its data, checked: compacting copies none that is corrupt under new CRCs.
    let object_data = layout::data_under(&object.bytes, &keys).map_err(|err| err.reading(key))?;
    let encryption = match Frame::of(&object.bytes)?.encryption.keyed() {
        Some((id, _)) => Some((keys.dek(id).ok_or(Error::NoKey(id))?, id)),
        None => None,
    };
    let mut data = Vec::new();
    let mut entries = Vec::new();
    for (&extent, location) in extents {
        let range = location.offset as usize..(location.offset + location.len) as usize;
        let bytes = object_data
            .get(range)
            .ok_or_else(|| Error::Corrupt(format!("{extent:?} past the end of {key}")))?;
        let to = Location { key: moves.clone(), offset: data.len() as u64, len: location.len };
        entries.push((extent, to));
        data.extend_from_slice(bytes);
    }
    // Its own moves of it, created before, stand.
    let footer = Moves { of: key.to_owned(), extents: entries };
    let bytes = layout::with_footer_under(&data, &footer, encryption);
    let size = match bucket::create(bucket, &moves, bytes.clone()).await {
        Created::Created => bytes.len() as u64,
        Created::Exists(stood) => stood.meta.size,
    };
    Ok(Some((moves, size)))
}

/// cladefs: compacts the object `compactable` is, under `ring`, by the pass of epoch `pass`, as
/// [`compact_object`] does: its
/// moves' key and size, and the object's size as listed, if it did. A function of its own, not a
/// closure's future, so that a pass's future is seen to be `Send` (TASK-395.15: a sweep spawning
/// its passes).
async fn compact_listed(
    bucket: &impl Bucket,
    compactable: &Compactable<'_>,
    (ring, pass): (&Ring, u64),
) -> Result<Option<((String, u64), u64)>, Error> {
    let Compactable { meta, extents, .. } = compactable;
    let moves = compact_object(bucket, &meta.key, extents, (ring, pass)).await?;
    Ok(moves.map(|moves| (moves, meta.size)))
}

/// cladefs: the keys of the moves of the object at `key` that `objects`, a listing of its cell,
/// holds, every pass's, in the order of their passes (TASK-407.1).
fn moves_listed<'a>(
    objects: &'a BTreeMap<String, Meta>,
    key: &str,
) -> Result<Vec<&'a String>, Error> {
    let prefix = layout::moves_of(key)?;
    Ok((objects.range(prefix.clone()..))
        .map(|(moves, _)| moves)
        .take_while(|moves| moves.starts_with(&prefix))
        .collect())
}

/// What becomes of an object a pass listed ([`Pass::fates`]).
enum Fate {
    Stays,
    Goes,
    /// It goes once the store's clock reads this.
    GoesAt(u64),
}

impl Fate {
    /// Goes at `at`, if it is; stays otherwise.
    fn at(at: Option<u64>) -> Self {
        at.map_or(Self::Stays, Self::GoesAt)
    }
}

/// What a pass over a cell found: every object of the cell listed, and what the live branches and
/// the fork points it keeps need of them.
struct Pass {
    cell: Cell,
    /// cladefs: its epoch, its taking of the cell's collector lease, which its moves' keys name
    /// (TASK-407.1).
    epoch: u64,
    objects: BTreeMap<String, Meta>,
    records: BTreeMap<Branch, Record>,
    /// Where each recovery of each live branch ended.
    branches: BTreeMap<Branch, Vec<Ended>>,
    /// What recovering the live branches and the fork points read.
    touched: BTreeSet<String>,
    /// The objects holding the extents the live branches and the fork points name, and those
    /// extents in them.
    named: BTreeMap<String, BTreeMap<ExtentId, Location>>,
    /// The moves the way to those extents goes through, whose maps are needed even once their
    /// data moved on: from where the states name them, or, where a writer may name them before
    /// that, from where they were sealed.
    hops: BTreeSet<String>,
    /// cladefs: the fork points the references under the cell name, as listed after the states.
    referenced: BTreeSet<Origin>,
    /// cladefs: what it records of its listing (TASK-395.6, TASK-395.8).
    recording: Recording,
    /// cladefs: the branches of its deleted roots' and clones' records whose IDs an allocator gave
    /// ([`crate::objectid::given`], TASK-455).
    given: BTreeSet<Branch>,
}

/// cladefs: what a pass records of its listing of its cell, as it ends ([`Passed`]).
struct Recording {
    /// The listing's digest, if what the pass decides goes by the cell alone, and the listing came
    /// before what it read ([`Passed::listing`]).
    digest: Option<[u8; 32]>,
    /// What the cell took, as listed.
    usage: Usage,
    /// The cell's own references listed marked ([`Listed::stale`]).
    stale: Vec<String>,
    /// How many references it listed ([`Listed::references`]).
    references: usize,
}

impl Pass {
    /// The pass over `cell` of `epoch`, which listed `objects`, and found `records`, what the live
    /// branches and the fork points need (`live`), the fork points `referenced`, and what it
    /// records as it ends: its ways to the extents they name worked out.
    async fn new(
        bucket: &impl Bucket,
        (cell, epoch): (Cell, u64),
        found: (
            BTreeMap<String, Meta>,
            BTreeMap<Branch, Record>,
            Live,
            BTreeSet<Origin>,
            Recording,
        ),
    ) -> Result<Self, Error> {
        let (objects, records, live, referenced, recording) = found;
        let Live { branches, touched, located, behind, .. } = live;
        // Whose IDs an allocator gave, of those a removal or an unmarked retirement asks of: a GET
        // each, once.
        let mut given = BTreeSet::new();
        for (&branch, record) in &records {
            let asked = matches!(record.kind, record::Kind::Root | record::Kind::Clone(_));
            if record.deleted && asked && crate::objectid::given(bucket, branch.id).await {
                given.insert(branch);
            }
        }
        let mut named: BTreeMap<String, BTreeMap<ExtentId, Location>> = BTreeMap::new();
        // The moves on the ways there from where the extents were sealed, those named among them:
        // each moves is at a key of the moves of what it is of.
        let mut hops = BTreeSet::new();
        for key in &behind {
            let mut key = key.clone();
            while let Some(from) = layout::moved_from(&key) {
                if objects.contains_key(&key) {
                    hops.insert(key);
                }
                key = from;
            }
        }
        // Each place once, however many branches name it, followed along its moves a hop at a
        // time: the moves the ways reach at a hop read at once, as the listing says which there
        // are (TASK-395.5). cladefs: a place in another cell, and its moves, are no candidates of
        // this one's, which its listing doesn't hold (TASK-395.4).
        let mut maps = MovesMaps::default();
        let mut ways: Vec<(ExtentId, Location)> = (located.iter())
            .flat_map(|(&extent, locations)| locations.iter().map(move |at| (extent, at.clone())))
            .collect();
        while !ways.is_empty() {
            let mut onward = Vec::new();
            for (extent, location) in ways {
                if objects.contains_key(&location.key) {
                    named.entry(location.key.clone()).or_default().insert(extent, location.clone());
                }
                // Every pass's moves of it listed, the way going through the latest that has the
                // extent, as readers follow it (TASK-407.1).
                let moves = moves_listed(&objects, &location.key)?;
                if !moves.is_empty() {
                    hops.extend(moves.iter().map(|&key| key.clone()));
                    let moves = moves.into_iter().cloned().collect::<Vec<_>>();
                    onward.push((extent, location, moves));
                }
            }
            let listed = onward.iter().map(|(_, at, moves)| (at.key.clone(), moves.clone()));
            maps.read_listed(bucket, listed).await?;
            ways = (onward.into_iter())
                .filter_map(|(extent, location, _)| match maps.moved_read(extent, &location.key) {
                    Moved::To(to) => Some((extent, to)),
                    Moved::No | Moved::Dropped => None,
                })
                .collect();
        }
        Ok(Self {
            cell,
            epoch,
            objects,
            records,
            branches,
            touched,
            named,
            hops,
            referenced,
            recording,
            given,
        })
    }

    /// cladefs: whether the object of `kind`, which no live state names, a live branch's head may yet
    /// name, by what its key declares ([`Ended::ahead`], TASK-407): by any of the branch's
    /// recoveries, of several origins (TASK-173).
    fn ahead(&self, kind: Kind<'_>) -> bool {
        let branch = match kind {
            Kind::Segment { branch, .. }
            | Kind::Extent { branch, .. }
            | Kind::Checkpoint { branch, .. } => branch,
            _ => return false,
        };
        self.branches.get(&branch).into_iter().flatten().any(|ended| ended.ahead(kind))
    }

    /// The objects mostly freed, as the lengths of their data in their trailers say
    /// ([`Trailers`]): those [`Pass::compact`] compacts, each with its live extents, which take
    /// less than `options`' fraction of it, and its encryption.
    async fn compactable(
        &self,
        bucket: &impl Bucket,
        options: &Options,
        trailers: &mut Trailers,
    ) -> Result<Vec<Compactable<'_>>, Error> {
        let mut ours = Vec::new();
        for (key, extents) in &self.named {
            let collectable = match layout::kind(key) {
                kind @ Kind::Segment { .. } => !self.touched.contains(key) && !self.ahead(kind),
                Kind::Moves { .. } => true,
                _ => false,
            };
            if collectable && moves_listed(&self.objects, key)?.is_empty() {
                ours.push((&self.objects[key], extents));
            }
        }
        trailers.learn(bucket, &ours.iter().map(|(meta, _)| *meta).collect::<Vec<_>>()).await?;
        // Those gone since they were listed, whose trailers it could not read, are not compacted.
        Ok((ours.into_iter())
            .filter_map(|(meta, extents)| {
                let frame = trailers.known(meta)?;
                let encryption = frame.encryption;
                below(extents, options, frame.data_len).then_some(Compactable {
                    meta,
                    extents,
                    encryption,
                })
            })
            .collect())
    }

    /// Compacts the objects mostly freed: copies the live extents of each to its moves.
    /// [`COMPACTIONS_AT_ONCE`] at once, in any order (TASK-395.5): each writes the moves of an
    /// object of its own, create-only, which no other compaction of the pass reads, as an object
    /// is compacted only where no moves of it were listed, and moves only where they were, never
    /// both of one way. They all end before the pass works out what goes, as one at a time: what
    /// goes is of the listing before, which none of their moves is in. The moves' keys and sizes.
    /// cladefs: those encrypted under their branches' DEKs, unwrapped under the cell's KEK through
    /// `kms` once for the pass, and only if one is to be compacted ([`Pass::compacting`],
    /// TASK-411.6).
    async fn compact(
        &self,
        bucket: &impl Bucket,
        options: &Options,
        trailers: &mut Trailers,
        kms: Option<&kms::Shared>,
        report: &mut Report,
    ) -> Result<Vec<(String, u64)>, Error> {
        let mut compactable = self.compactable(bucket, options, trailers).await?;
        let ring = self.compacting(bucket, kms, &mut compactable, report).await?;
        // The compactions' futures made first, as in `bucket::delete_all`.
        let compactions: Vec<_> = compactable
            .iter()
            .map(|object| compact_listed(bucket, object, (&ring, self.epoch)))
            .collect();
        let compactions = futures::stream::iter(compactions).buffer_unordered(COMPACTIONS_AT_ONCE);
        let mut compactions = std::pin::pin!(compactions);
        let mut compacted = Vec::new();
        while let Some(done) = compactions.next().await {
            if let Some((moves, size)) = done? {
                report.compacted += 1;
                report.compacted_bytes += size;
                compacted.push(moves);
            }
        }
        Ok(compacted)
    }

    /// cladefs: the keys `compactable`'s objects are compacted under ([`keys::compacting`],
    /// TASK-411.6): which of their branches are encrypted, as the pass's records of them say, whose
    /// objects in the clear are refused; and, if any of the objects is encrypted, those branches' DEKs,
    /// unwrapped under the cell's KEK by a [`keys::CachingKms`] of this pass's over `kms`, each
    /// generation of the KEK once: the pass's one call of the KMS, however many objects it
    /// compacts. None of them encrypted, it asks nothing. The KEK not to be had, `kms` refusing it (a
    /// tenant's KEK revoked or destroyed, the KMS failing) or none given, the objects encrypted are
    /// left uncompacted, counted in `report` and warned of, and the pass goes on with those in the
    /// clear.
    async fn compacting(
        &self,
        bucket: &impl Bucket,
        kms: Option<&kms::Shared>,
        compactable: &mut Vec<Compactable<'_>>,
        report: &mut Report,
    ) -> Result<Arc<Ring>, Error> {
        let branches: BTreeSet<u64> =
            compactable.iter().filter_map(|object| keys::branch_of(&object.meta.key)).collect();
        let records: Vec<(Branch, &Record)> = (self.records.iter())
            .filter(|(branch, _)| branches.contains(&branch.id))
            .map(|(&branch, record)| (branch, record))
            .collect();
        let encrypted =
            compactable.iter().filter(|object| object.encryption.keyed().is_some()).count();
        if encrypted == 0 {
            return keys::compacting(records, None).await;
        }
        let cell = self.cell;
        let kms = kms.map(|kms| keys::CachingKms::new(kms.clone()));
        let cell_keys = match crate::cell::read(bucket, cell).await? {
            Some(object) => CellKeys::of(cell, &object, kms),
            None => CellKeys::none(cell, kms),
        };
        match keys::compacting(records.iter().copied(), Some(&cell_keys)).await {
            Err(Error::Kms(why)) => {
                log::warn!(
                    cell:%, why:%;
                    "cladefs-gc: cell {cell}: {encrypted} objects encrypted left uncompacted: {why}"
                );
                report.locked += 1;
                report.uncompacted += encrypted;
                compactable.retain(|object| object.encryption.keyed().is_none());
                keys::compacting(records, None).await
            }
            ring => ring,
        }
    }

    /// What goes: everything listed that the live branches and the fork points kept do not need, in
    /// the order of its keys ([`Pass::fates`]).
    /// cladefs: the cell's removal too, if it goes whole ([`Pass::removal`], TASK-455).
    async fn doomed(&self, bucket: &impl Bucket, options: &Options) -> Result<Vec<&Meta>, Error> {
        let (mut doomed, deferred) = self.fates(bucket, options).await?;
        if let Some(removal) = self.removal(&doomed, &deferred) {
            doomed.extend(removal.metas());
            doomed.sort_by(|a, b| a.key.cmp(&b.key));
        }
        Ok(doomed)
    }

    /// What goes, as [`Pass::doomed`] says; and what would but that it was written too recently,
    /// or what it was compacted to was, with when it may go (TASK-395.6), in the order of its keys.
    /// cladefs: a cell with no live branch and no reference, all of it but its records and its object,
    /// and the collector's own, goes.
    async fn fates(
        &self,
        bucket: &impl Bucket,
        options: &Options,
    ) -> Result<(Vec<&Meta>, Vec<Deferred>), Error> {
        let now = bucket.now();
        let due = |meta: &Meta| meta.modified.saturating_add(options.grace);
        // When the object at `key` may go for having been compacted: a grace period after its
        // latest moves were written, if any were listed.
        let compacted = |key: &str| {
            let moves = moves_listed(&self.objects, key).ok()?;
            moves.last().map(|moves| due(&self.objects[*moves]))
        };
        let goes = |goes: bool| if goes { Fate::Goes } else { Fate::Stays };
        let (mut doomed, mut deferred) = (Vec::new(), Vec::new());
        for (key, meta) in &self.objects {
            let named = self.named.contains_key(key);
            let kind = layout::kind(key);
            let fate = match kind {
                Kind::Checkpoint { .. } => goes(!self.touched.contains(key) && !self.ahead(kind)),
                // Once a checkpoint covers it, if its extents are dead or moved; but one its head
                // may yet name ([`Pass::ahead`]).
                Kind::Segment { .. } => {
                    if self.ahead(kind) || self.touched.contains(key) {
                        Fate::Stays
                    } else if named {
                        Fate::at(compacted(key))
                    } else {
                        Fate::Goes
                    }
                }
                // Once no segment will name it.
                Kind::Extent { .. } => goes(!named && !self.ahead(kind)),
                // Moves go once no state's way goes through them, and they are compacted in turn,
                // or what they are of is gone and they hold no extent a state names.
                Kind::Moves { cell, of, .. } => {
                    if self.hops.contains(key) {
                        Fate::Stays
                    } else if named {
                        Fate::at(compacted(key))
                    } else if self.objects.contains_key(&format!("{}{of}", cell.prefix())) {
                        Fate::Stays
                    } else {
                        Fate::GoesAt(due(meta))
                    }
                }
                // A record stays, a deleted branch's too (TASK-174), and every head: a deleted
                // branch's, final, fences its writers, which no swap of theirs passes. cladefs: but
                // with a dead cell that goes whole ([`Pass::removal`], TASK-455).
                Kind::Record { .. } => Fate::Stays,
                // cladefs: but one of a branch of no record listed that is no fence ([`fence`]): a
                // writer's that took a branch by a read of its record from before its deletion, its
                // cell removed since (TASK-455). Without the rule, as a test shows it needed, any
                // in a cell of no record, a deletion's fence among them.
                Kind::Head { branch } => match rules().fences_kept {
                    true => {
                        goes(!self.records.contains_key(&branch) && !fence(bucket, branch).await?)
                    }
                    false => goes(self.records.is_empty()),
                },
                // The collector's own: its lease, and its marks, which a mark-only pass never
                // deletes, and an operator removes (TASK-264); and references, which the passes of
                // their children's cells retire (TASK-395.4), and their marks, kept for good
                // (TASK-395.13). cladefs: a cell's object, which says who holds it, stays as its
                // records do, and a block of object IDs taken for good (TASK-364).
                Kind::CollectorLease { .. }
                | Kind::Marks { .. }
                | Kind::Marking
                | Kind::Reference { .. }
                | Kind::Retired { .. }
                | Kind::Cell { .. }
                | Kind::ObjectIds { .. }
                | Kind::Unknown => Fate::Stays,
            };
            match fate {
                Fate::Stays => {}
                Fate::Goes => doomed.push(meta),
                Fate::GoesAt(at) if at <= now => doomed.push(meta),
                Fate::GoesAt(due) => deferred.push(Deferred { key: key.clone(), due }),
            }
        }
        Ok((doomed, deferred))
    }

    /// cladefs: the references the pass retires, under other cells (doc-7, References; TASK-395.4):
    /// once its cell holds no live branch and no reference of its own, those of its deleted
    /// children of origins in other cells, each at the key its record's origin gives, never listed.
    /// Nothing anywhere then inherits from them: a fork of one of them in the cell would have a
    /// record there, and one in another cell a reference there. Without the rule that the cell's
    /// references hold them, as a test shows it needed, once each child is deleted. cladefs: each
    /// with whether it is marked first: but a clone's made, of an ID an allocator gave
    /// ([`Record::retires_unmarked`], TASK-455).
    fn retired(&self) -> Vec<(String, bool)> {
        if rules().references_held && !self.dead() {
            return Vec::new();
        }
        (self.records.iter())
            .filter(|(_, record)| record.deleted)
            .filter_map(|(&child, record)| {
                let origin = record.origin().filter(|origin| origin.parent.cell != self.cell)?;
                let marked = !record.retires_unmarked(self.given.contains(&child));
                Some((layout::reference(origin.parent, origin.at, child), marked))
            })
            .collect()
    }

    /// cladefs: whether its cell holds no live branch and no reference of its own.
    fn dead(&self) -> bool {
        self.records.values().all(|record| record.deleted)
            && self.branches.is_empty()
            && self.referenced.is_empty()
    }

    /// cladefs: whether every reference it listed is one it deletes, stale ([`Listed::stale`]).
    fn references_all_stale(&self) -> bool {
        self.recording.references == self.recording.stale.len()
    }

    /// cladefs: what removes the cell whole, if it goes (TASK-455): dead, deferring nothing, its
    /// listed references all stale, and every object it listed that `doomed` leaves one of a cell
    /// nothing will need again: its records, each [`Record::removable`] (a root's or a clone's
    /// made, deleted, of an ID an allocator gave), its branches' heads, its object, and its
    /// lease. A mark under it (of a reference retired, which a writer may write again; or a
    /// mark-only pass's, an operator's to remove), what it defers, a fence ([`fence`]), and
    /// anything else keep it.
    /// A cell of no record loses its lease alone, once nothing but its lease is left: one whose
    /// removal stopped before its end; its object, if it has one, may be of a cell being made,
    /// its record yet to come, and keeps it.
    fn removal<'a>(&'a self, doomed: &[&Meta], deferred: &[Deferred]) -> Option<Removal<'a>> {
        if !self.dead() || !deferred.is_empty() || !self.references_all_stale() {
            return None;
        }
        let doomed: BTreeSet<&str> = doomed.iter().map(|meta| meta.key.as_str()).collect();
        let mut removal = Removal::default();
        for (key, meta) in &self.objects {
            if doomed.contains(key.as_str()) {
                continue;
            }
            match layout::kind(key) {
                Kind::Record { branch } => {
                    let given = self.given.contains(&branch);
                    self.records.get(&branch).filter(|record| record.removable(given))?;
                    removal.records.push(meta);
                }
                // A head of a branch of no record listed that `doomed` leaves is a fence
                // ([`fence`]), which keeps it; without the rule, it goes with the rest.
                Kind::Head { branch }
                    if !self.records.is_empty()
                        && (self.records.contains_key(&branch) || !rules().fences_kept) =>
                {
                    removal.rest.push(meta);
                }
                Kind::Cell { .. } if !self.records.is_empty() => removal.rest.push(meta),
                Kind::CollectorLease { .. } => removal.lease = Some(meta),
                _ => return None,
            }
        }
        Some(removal)
    }

    /// cladefs: the listing's digest it records, if any ([`Passed::listing`]): of what its listing
    /// leaves once its deletes land, if its cell is dead and it defers nothing, as a pass in full
    /// over that finds nothing more to do (TASK-455): so that a dead cell it does not remove is not
    /// passed over in full twice, the second for the first's deletes, retiring its references
    /// again. What it leaves must be what a dead cell keeps, its records, heads, object, lease and
    /// marks: moves left, of an object it deletes, the next pass deletes (or defers), and goes in
    /// full for. Else of its listing.
    fn recorded_listing(&self, doomed: &[&Meta], deferred: &[Deferred]) -> Option<[u8; 32]> {
        let digest = self.recording.digest?;
        if !self.dead() || !deferred.is_empty() || !self.references_all_stale() {
            return Some(digest);
        }
        let doomed: BTreeSet<&str> = doomed.iter().map(|meta| meta.key.as_str()).collect();
        let left: Vec<&Meta> =
            self.objects.values().filter(|meta| !doomed.contains(meta.key.as_str())).collect();
        let kept = left.iter().all(|meta| {
            matches!(
                layout::kind(&meta.key),
                Kind::Record { .. }
                    | Kind::Head { .. }
                    | Kind::Cell { .. }
                    | Kind::CollectorLease { .. }
                    | Kind::Marks { .. }
                    | Kind::Retired { .. }
            )
        });
        Some(if kept { passed::digest(self.cell, left) } else { digest })
    }
}

/// cladefs: whether `branch`'s head, its record not listed by the pass, is to stay (rule
/// `fences_kept`, TASK-455's review): final, a deletion's fence, of a fork's child whose record is
/// yet to come, the deletion's own or its parent's slow writer's (TASK-79.78), which a writer
/// taking the branch once it is recorded must find; or the branch has a record after all, listed
/// after the cell, a listing of many pages having missed it. Any other is a stale taker's, of a
/// branch removed with its cell, whose record never comes again (rule `ids_given`): garbage. Two
/// GETs at most, of a head no record was listed with, which only those leave. Gone since, it is
/// not to stay.
async fn fence(bucket: &impl Bucket, branch: Branch) -> Result<bool, Error> {
    let Some(head) = crate::head::get(bucket, branch).await? else { return Ok(false) };
    Ok(head.epoch == layout::Head::FINAL || branch::record(bucket, branch).await?.is_some())
}

/// cladefs: what a pass removing its cell whole deletes ([`Pass::removal`], TASK-455).
#[derive(Default)]
struct Removal<'a> {
    /// Its heads and its object.
    rest: Vec<&'a Meta>,
    records: Vec<&'a Meta>,
    lease: Option<&'a Meta>,
}

impl<'a> Removal<'a> {
    /// Every object it deletes.
    fn metas(&self) -> impl Iterator<Item = &'a Meta> + '_ {
        self.rest.iter().chain(&self.records).chain(&self.lease).copied()
    }

    fn len(&self) -> usize {
        self.metas().count()
    }

    fn bytes(&self) -> u64 {
        self.metas().map(|meta| meta.size).sum()
    }

    /// Deletes it: the records after the rest, so that a pass stopping between finds the cell as
    /// this one did, and the lease last, so that one stopping before finds the cell's lease and
    /// what is left, which it removes. Returns the cells it removed: 1, or 0 if it removed only the
    /// lease of a cell of no record. A pass that took the lease meanwhile finds it gone, fenced,
    /// and deletes nothing more: what is left, the next removes.
    async fn remove(&self, bucket: &impl Bucket) -> usize {
        let keys = |metas: &[&Meta]| -> Vec<String> {
            metas.iter().map(|meta| meta.key.clone()).collect()
        };
        for keys in [keys(&self.rest), keys(&self.records)] {
            bucket::delete_all(bucket, &keys.iter().map(String::as_str).collect::<Vec<_>>()).await;
        }
        if let Some(lease) = self.lease {
            bucket::delete(bucket, &lease.key).await;
        }
        usize::from(!self.records.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::{Commit, SegmentBuilder};
    use crate::bucket::{ByteRange, Created, Error as BucketError, Object, PutMode, Version};
    use crate::layout::test_branch;
    use crate::layout::{Child, child};
    use crate::object_bucket::{ObjectBucket, Request};
    use crate::testing::{Direct, SimBucket};
    use crate::writer::Writer;
    use extent_frame::KeyId;
    use extent_keys::kms::LocalKms;
    use extent_keys::testing::Counted;
    use futures::executor::block_on;
    use object_store::memory::InMemory;
    use std::future::Future;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// No compaction, and compacted objects kept long.
    const KEEP: Options = Options { compact_below: 0.0, grace: 3_600_000 };

    fn store() -> Arc<ObjectBucket> {
        Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""))
    }

    /// `commit` in a segment of its own, uploaded and acknowledged.
    async fn commit<B: Bucket>(writer: &Writer<B>, commit: Commit) {
        let objects = SegmentBuilder::from(commit).close(&mut writer.chain()).frame();
        let segment = writer.add_segment(objects).unwrap();
        writer.upload(segment).await.unwrap();
        writer.acknowledge().await.unwrap();
    }

    /// The keys of every object of the bucket but the collector's leases.
    async fn keys(bucket: &impl Bucket) -> Vec<String> {
        (bucket::list(bucket, "").await.into_iter())
            .map(|meta| meta.key)
            .filter(|key| !matches!(layout::kind(key), Kind::CollectorLease { .. }))
            .collect()
    }

    /// The extents a writer stored on their own and freed go; one uploaded ahead of the segment
    /// that will name it stays, as do those its state names, and its record.
    #[test]
    fn extents_minted_and_freed_go_and_those_ahead_of_their_segments_stay() {
        block_on(async {
            let bucket = store();
            branch::create(&*bucket, test_branch(1), 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap();
            let first = writer.chain().next_sequence();
            let (freed, kept) = (ExtentId::new(1, first), ExtentId::new(1, first + 1));
            let own = vec![(freed.sequence, b"freed".to_vec()), (kept.sequence, b"kept".to_vec())];
            commit(&writer, Commit { tag: 1, own, ..Commit::default() }).await;
            let gone = writer.location(freed).unwrap().key;
            commit(&writer, Commit { tag: 2, frees: vec![freed], ..Commit::default() }).await;
            let (segment, next) = (writer.chain().next_segment(), writer.chain().next_sequence());
            let ahead =
                layout::extent(layout::TEST_CELL, writer.epoch(), segment, ExtentId::new(1, next));
            let bytes = layout::with_footer(b"ahead", &());
            assert!(matches!(bucket::create(&*bucket, &ahead, bytes).await, Created::Created));

            let before = keys(&*bucket).await;
            let report = collect(&*bucket, 1, &KEEP).await.unwrap();
            assert_eq!(report, Report { deleted: 1, compacted: 0, retired: 0, ..report });
            let left: Vec<_> = before.into_iter().filter(|key| *key != gone).collect();
            assert_eq!(keys(&*bucket).await, left);
            assert!(left.contains(&ahead) && left.contains(&layout::record(test_branch(1))));
        });
    }

    /// TASK-141.2: a pass finding a branch of a newer format stops, deleting nothing, though what
    /// the other branches freed would go: what that branch needs, it can't read.
    #[test]
    fn passes_stop_at_branches_of_newer_formats() {
        block_on(async {
            let bucket = store();
            branch::create(&*bucket, test_branch(1), 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap();
            let freed = ExtentId::new(1, writer.chain().next_sequence());
            let own = vec![(freed.sequence, b"freed".to_vec())];
            commit(&writer, Commit { tag: 1, own, ..Commit::default() }).await;
            commit(&writer, Commit { tag: 2, frees: vec![freed], ..Commit::default() }).await;
            let mut newer = (layout::FORMAT + 1).to_le_bytes().to_vec();
            newer.extend_from_slice(b"what a later format holds");
            let created = bucket::create(&*bucket, &layout::record(test_branch(2)), newer).await;
            assert!(matches!(created, Created::Created));

            let before = keys(&*bucket).await;
            let refused = collect(&*bucket, 1, &KEEP).await;
            assert_eq!(
                refused,
                Err(Error::Format { branch: test_branch(2), format: layout::FORMAT + 1 })
            );
            assert_eq!(keys(&*bucket).await, before);
        });
    }

    /// An extent stored on its own, uploaded ahead of the segment that names it, stays, though
    /// the branch's segments landed say its next sequence number is past it: as when the guest
    /// commits extents out of the order it minted them, or a store closes a segment between
    /// commits while extents are open (TASK-79.3): of the head's epoch, its segment at or past the
    /// head's end, the head may yet name it (TASK-407). Its segment acknowledged, the branch reads
    /// it.
    #[test]
    fn extents_ahead_of_their_segments_stay_past_the_next_sequence_number() {
        assert_eq!(block_on(extent_ahead_of_its_segment_read()), Ok(b"late".to_vec()));
    }

    /// What reading an extent stored on its own gives, uploaded ahead of the segment naming it,
    /// committed after one past it in sequence, once a pass ran and then the segment landed.
    async fn extent_ahead_of_its_segment_read() -> Result<Vec<u8>, Error> {
        let bucket = store();
        branch::create(&*bucket, test_branch(1), 0).await.unwrap();
        let writer = Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap();
        let first = writer.chain().next_sequence();
        let (late, early) = (ExtentId::new(1, first), ExtentId::new(1, first + 1));
        let extents = vec![(early, b"early".to_vec())];
        commit(&writer, Commit { tag: 1, extents, ..Commit::default() }).await;
        assert_eq!(writer.chain().next_sequence(), first + 2);
        // The writer's next segment, which names `late`, its object uploaded first.
        let own = vec![(late.sequence, b"late".to_vec())];
        let objects = SegmentBuilder::from(Commit { tag: 2, own, ..Commit::default() })
            .close(&mut writer.chain())
            .frame();
        let [(key, bytes)] = &objects.own[..] else { panic!("{} objects", objects.own.len()) };
        let created = bucket::create(&*bucket, key, bytes.read().unwrap()).await;
        assert!(matches!(created, Created::Created));

        collect(&*bucket, 1, &KEEP).await.unwrap();
        let segment = writer.add_segment(objects).unwrap();
        writer.upload(segment).await.unwrap();
        writer.acknowledge().await.unwrap();
        let recovered = recovery::recover(&*bucket, test_branch(1), None).await.unwrap();
        let location = &recovered.state.extents[&late];
        crate::read(&bucket, late, location).await
    }

    /// An object compacted is compacted once: its moves stand while it waits out the grace
    /// period. A live branch's record stays.
    #[test]
    fn objects_are_compacted_once() {
        block_on(async {
            let bucket = store();
            branch::create(&*bucket, test_branch(1), 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap();
            let first = writer.chain().next_sequence();
            let (small, large) = (ExtentId::new(1, first), ExtentId::new(1, first + 1));
            let large_bytes: Vec<u8> = (0..20_000u32).map(|i| (i * 7 % 251) as u8).collect();
            let extents = vec![(small, b"small".to_vec()), (large, large_bytes)];
            commit(&writer, Commit { tag: 1, extents, ..Commit::default() }).await;
            let segment = writer.location(small).unwrap().key;
            commit(&writer, Commit { tag: 2, frees: vec![large], ..Commit::default() }).await;
            writer.checkpoint().await.unwrap();

            let compacting = Options { compact_below: 0.5, ..KEEP };
            let report = collect(&*bucket, 1, &compacting).await.unwrap();
            assert_eq!(report.compacted, 1);
            let after = keys(&*bucket).await;
            assert!(after.contains(&segment) && moves_in(&after, &segment).is_some());
            assert!(after.contains(&layout::record(test_branch(1))));
            let report = collect(&*bucket, 2, &compacting).await.unwrap();
            assert_eq!(report, Report::default());
            assert_eq!(keys(&*bucket).await, after);
            let location = writer.location(small).unwrap();
            assert_eq!(crate::read(&bucket, small, &location).await.unwrap(), b"small");
        });
    }

    /// TASK-293: a segment whose data compresses, its live extent a third of its data but more than
    /// half its stored bytes, is compacted: the collector measures what is live against its data,
    /// not its stored size, as it did before, leaving it uncompacted for as long as the extent
    /// lived.
    #[test]
    fn compressed_objects_are_compacted_by_their_data() {
        block_on(async {
            let bucket = store();
            branch::create(&*bucket, test_branch(1), 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap();
            let first = writer.chain().next_sequence();
            let (kept, freed) = (ExtentId::new(1, first), ExtentId::new(1, first + 1));
            let extents = vec![(kept, vec![b'k'; 4096]), (freed, vec![b'f'; 8192])];
            commit(&writer, Commit { tag: 1, extents, ..Commit::default() }).await;
            let segment = writer.location(kept).unwrap().key;
            commit(&writer, Commit { tag: 2, frees: vec![freed], ..Commit::default() }).await;
            writer.checkpoint().await.unwrap();
            let stored = bucket.head(&segment).await.unwrap().size;
            assert!(stored < 2 * 4096, "{segment}: {stored} bytes stored");

            let compacting = Options { compact_below: 0.5, ..KEEP };
            let report = collect(&*bucket, 1, &compacting).await.unwrap();
            assert_eq!(report.compacted, 1);
            let after = keys(&*bucket).await;
            assert!(after.contains(&segment) && moves_in(&after, &segment).is_some());
            let location = writer.location(kept).unwrap();
            assert_eq!(crate::read(&bucket, kept, &location).await.unwrap(), vec![b'k'; 4096]);
        });
    }

    /// cladefs: an object a byte was flipped in, in storage, is compacted to no moves, where its
    /// extents would be under CRCs of their own: the pass fails, and the object stays, as its
    /// extents' reads fail (TASK-153.11). In a part stored compressed, the kept extent's bytes
    /// compressing, and in one stored raw.
    #[test]
    fn objects_with_flipped_bytes_are_not_compacted() {
        for compressible in [true, false] {
            block_on(async {
                let bucket = store();
                branch::create(&*bucket, test_branch(1), 0).await.unwrap();
                let writer = Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap();
                let first = writer.chain().next_sequence();
                let (kept, freed) = (ExtentId::new(1, first), ExtentId::new(1, first + 1));
                // The kept extent's bytes compress, or not; the freed one's don't, from a xorshift
                // generator, so that the part is stored raw where the kept one's don't either.
                let mut state = 0x9e37_79b9_7f4a_7c15_u64;
                let mut bytes = |len: u32, compressible: bool| -> Vec<u8> {
                    (0..len)
                        .map(|i| match compressible {
                            true => (i * 7 % 251) as u8,
                            false => {
                                state ^= state << 13;
                                state ^= state >> 7;
                                state ^= state << 17;
                                state as u8
                            }
                        })
                        .collect()
                };
                let extents =
                    vec![(kept, bytes(4096, compressible)), (freed, bytes(20_000, false))];
                commit(&writer, Commit { tag: 1, extents, ..Commit::default() }).await;
                let segment = writer.location(kept).unwrap().key;
                commit(&writer, Commit { tag: 2, frees: vec![freed], ..Commit::default() }).await;
                writer.checkpoint().await.unwrap();
                let mut bytes = bucket::get(&*bucket, &segment).await.unwrap().bytes;
                let stored =
                    extent_frame::Parts::of(&bytes, &extent_frame::NoKeys).unwrap().stored[0];
                let encoding = match compressible {
                    true => extent_frame::Encoding::Zstd,
                    false => extent_frame::Encoding::Raw,
                };
                assert_eq!(stored.encoding, encoding);
                bytes[stored.offset as usize + 9] ^= 0x10;
                bucket.put(&segment, bytes.into(), PutMode::Overwrite).await.unwrap();

                let compacting = Options { compact_below: 0.5, ..KEEP };
                let corrupt = Error::Corrupt("an object's part, by its CRC".into());
                assert_eq!(collect(&*bucket, 1, &compacting).await, Err(corrupt));
                let after = keys(&*bucket).await;
                assert!(after.contains(&segment) && moves_in(&after, &segment).is_none());
                let location = writer.location(kept).unwrap();
                let read = crate::read(&bucket, kept, &location).await;
                let corrupt = format!("part 0 of {segment}, by its CRC");
                assert_eq!(read, Err(Error::Corrupt(corrupt)));
            });
        }
    }

    /// TASK-175, as specs/bucket's NoWaysFromSeals finds it since the head (TASK-407): a fork's
    /// writer recovers its parent's state at the fork, naming an extent where it was sealed; a
    /// pass compacts that segment, and the parent's writer checkpoints at the fork point, naming
    /// the moves, which the fork's recovery reads since. The collector compacts the moves in turn
    /// and deletes what it can, and the fork's writer still reads the extent, the moves on its way
    /// from where it was sealed kept. (Before the head, a writer taken over checkpointing late did
    /// it alone: found by the simulation, seed 677 of its `recreated` on task-165-3.)
    #[test]
    fn writers_read_past_checkpoints_landing_after_they_took_over() {
        assert_eq!(block_on(read_past_a_late_checkpoint()), Ok(b"kept".to_vec()));
    }

    /// The collector keeping the moves on the way to an extent only from where the states it reads
    /// name it: the fork's writer reads it where neither the object nor its moves are.
    #[test]
    fn writers_reading_past_checkpoints_landing_after_they_took_over_need_ways_from_seals() {
        crate::set_rules(crate::Rules { ways_from_seals: false, ..crate::Rules::ALL });
        let read = block_on(read_past_a_late_checkpoint());
        crate::set_rules(crate::Rules::ALL);
        assert!(matches!(read, Err(Error::Lost(_))), "{read:?}");
    }

    /// What the fork's writer reads of the extent [`writers_read_past_checkpoints_landing_after_they_took_over`]
    /// keeps: its parent's writer checkpointing at the fork point, the collector having moved it,
    /// after the fork's writer recovered the parent's state there from the checkpoint before; the
    /// moves then compacted in turn, and passes run until they delete nothing more.
    async fn read_past_a_late_checkpoint() -> Result<Vec<u8>, Error> {
        let bucket = store();
        let (parent, fork) = (test_branch(1), test_branch(2));
        branch::create(&*bucket, parent, 0).await.unwrap();
        let before = Writer::open(bucket.clone(), parent, 10, 0).await.unwrap();
        let first = before.chain().next_sequence();
        let [kept, moved, freed] = [0, 1, 2].map(|i| ExtentId::new(1, first + i));
        let extents = vec![
            (kept, b"kept".to_vec()),
            (moved, vec![b'm'; 20_000]),
            (freed, vec![b'f'; 20_000]),
        ];
        commit(&before, Commit { tag: 1, extents, ..Commit::default() }).await;
        let sealed = before.location(kept).unwrap().key;
        before.checkpoint().await.unwrap();
        // The fork, freeing what makes the segment mostly freed.
        let forking =
            SegmentBuilder::from(Commit { tag: 2, frees: vec![freed], ..Commit::default() });
        let at =
            before.add_segment(forking.fork(child(fork), &mut before.chain()).frame()).unwrap();
        before.upload(at).await.unwrap();
        before.acknowledge().await.unwrap();
        // The fork's writer recovers its parent's state at the fork, from the checkpoint before.
        let writer = Writer::open(bucket.clone(), fork, 11, 0).await.unwrap();
        assert_eq!(writer.location(kept).unwrap().key, sealed);
        let compacting = Options { compact_below: 0.6, ..KEEP };
        assert_eq!(collect(&*bucket, 1, &compacting).await.unwrap().compacted, 1);
        // The parent's writer checkpoints at the fork point, following the moves.
        assert_eq!(before.checkpoint().await, Ok(Some(at)));
        let tag = writer.chain().tag(1);
        commit(&before, Commit { tag: 3, frees: vec![moved], ..Commit::default() }).await;
        commit(&writer, Commit { tag, frees: vec![moved], ..Commit::default() }).await;
        collect_all(&*bucket).await;
        let left = keys(&*bucket).await;
        assert!(
            !left.contains(&sealed)
                && left.iter().any(|key| {
                    layout::moved_from(key).and_then(|from| layout::moved_from(&from))
                        == Some(sealed.clone())
                }),
            "{left:#?}"
        );
        crate::read(&bucket, kept, &writer.location(kept).unwrap()).await
    }

    /// TASK-175: a writer names the extents its checkpoint moves where they went before the
    /// checkpoint lands: passes of the collector running as the head's swap naming it takes
    /// effect, before it is answered, trust the checkpoint of the writer holding the head, compact
    /// the moves in turn and delete the moves before, and the writer still reads the extent.
    #[test]
    fn writers_name_what_their_checkpoints_move_before_they_land() {
        assert_eq!(block_on(read_as_a_checkpoint_lands()), Ok(b"kept".to_vec()));
    }

    /// A writer naming them so once the head's swap naming its checkpoint is answered: as it takes
    /// effect, it names the extent where neither the object nor its moves are.
    #[test]
    fn writers_naming_what_their_checkpoints_move_need_moves_taken_first() {
        crate::set_rules(crate::Rules { moves_taken_first: false, ..crate::Rules::ALL });
        let read = block_on(read_as_a_checkpoint_lands());
        crate::set_rules(crate::Rules::ALL);
        assert!(matches!(read, Err(Error::Lost(_))), "{read:?}");
    }

    /// What the writer reads of the extent [`writers_name_what_their_checkpoints_move_before_they_land`]
    /// keeps, where it names it as its checkpoint recording the extent's moves lands, passes of
    /// the collector having run until they delete nothing more.
    async fn read_as_a_checkpoint_lands() -> Result<Vec<u8>, Error> {
        let direct = Arc::new(Direct(SimBucket::new()));
        let bucket = Arc::new(Hooked::new(direct.clone(), Hook::Swapped));
        branch::create(&*bucket, test_branch(1), 0).await.unwrap();
        let writer = Arc::new(Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap());
        let Compacted { kept, moved, .. } = compacted(&*bucket, &writer).await;
        commit(&writer, Commit { tag: 3, frees: vec![moved], ..Commit::default() }).await;
        let named = Arc::new(Mutex::new(None));
        let (passing, naming, watched) = (direct.clone(), named.clone(), Arc::downgrade(&writer));
        *bucket.then.lock().unwrap() = Some(Box::new(move || {
            collect_all(&*passing).now_or_never().unwrap();
            *naming.lock().unwrap() = watched.upgrade().unwrap().location(kept);
        }));
        writer.checkpoint().await.unwrap();
        let location = named.lock().unwrap().clone().unwrap();
        crate::read(&direct, kept, &location).await
    }

    /// What [`compacted`] made.
    struct Compacted {
        /// An extent the collector moved, which stays.
        kept: ExtentId,
        /// One it moved with it, freed next.
        moved: ExtentId,
    }

    /// Has `writer` seal three extents in a segment, then free one and checkpoint, and the
    /// collector compact the segment, moving the two others.
    async fn compacted<B: Bucket>(bucket: &B, writer: &Writer<B>) -> Compacted {
        let first = writer.chain().next_sequence();
        let [kept, moved, freed] = [0, 1, 2].map(|i| ExtentId::new(1, first + i));
        let extents = vec![
            (kept, b"kept".to_vec()),
            (moved, vec![b'm'; 20_000]),
            (freed, vec![b'f'; 20_000]),
        ];
        commit(writer, Commit { tag: 1, extents, ..Commit::default() }).await;
        commit(writer, Commit { tag: 2, frees: vec![freed], ..Commit::default() }).await;
        writer.checkpoint().await.unwrap();
        let compacting = Options { compact_below: 0.6, ..KEEP };
        assert_eq!(collect(bucket, 1, &compacting).await.unwrap().compacted, 1);
        Compacted { kept, moved }
    }

    /// Runs passes of the collector, compacting objects less than 60% live and deleting what they
    /// compacted at once, until one does nothing.
    async fn collect_all(bucket: &impl Bucket) {
        let now = Options { compact_below: 0.6, grace: 0 };
        let mut holder = 2;
        while collect(bucket, holder, &now).await.unwrap() != Report::default() {
            holder += 1;
        }
    }

    /// TASK-177: a fork's recovery that listed no checkpoint of the fork's own, going to its
    /// parent's state where it was forked, finds the parent's record gone, the parent deleted and
    /// the collector having dropped its record once the fork's own checkpoint came: it starts again
    /// from that checkpoint. Found by the simulation, seed 210 of its `forks` at SIM_SOAK=100.
    /// Records are kept since (TASK-174): with them, the recovery finds the parent's segments gone
    /// instead, and starts again as for any segment gone; this is what it does should a record go.
    #[test]
    fn forks_recover_past_parents_records_their_own_checkpoints_let_go() {
        crate::set_rules(crate::Rules { records_kept: false, ..crate::Rules::ALL });
        let recovered = block_on(recover_as_a_fork_checkpoints());
        crate::set_rules(crate::Rules::ALL);
        assert_eq!(recovered, Ok(vec![ExtentId::new(1, 1 << 40)]));
        assert_eq!(block_on(recover_as_a_fork_checkpoints()), Ok(vec![ExtentId::new(1, 1 << 40)]));
    }

    /// The extents a fork's recovery finds, its parent deleted, the fork's writer checkpointing and
    /// a pass of the collector running as the recovery reads the parent's record.
    async fn recover_as_a_fork_checkpoints() -> Result<Vec<ExtentId>, Error> {
        use crate::layout::{Contents, Origin, Segment};
        use crate::writer::Objects;
        let direct = Arc::new(Direct(SimBucket::new()));
        let bucket =
            Arc::new(Hooked::new(direct.clone(), Hook::Getting(layout::record(test_branch(1)), 1)));
        branch::create(&*bucket, test_branch(1), 0).await.unwrap();
        let parent = Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap();
        let extent = ExtentId::new(1, parent.chain().next_sequence());
        let extents = vec![(extent, b"held".to_vec())];
        commit(&parent, Commit { tag: 1, extents, ..Commit::default() }).await;
        let footer = parent
            .chain()
            .close(Contents { forks: vec![child(test_branch(2))], ..Contents::default() });
        let segment = Segment { data: Vec::new(), footer: footer.clone() }.encode().into();
        let at = parent.add_segment(Objects { footer, segment, own: Vec::new() }).unwrap();
        parent.upload(at).await.unwrap();
        parent.acknowledge().await.unwrap();
        branch::delete(&*bucket, test_branch(1), 11, u64::MAX).await.unwrap();
        let forked = Arc::new(Writer::open(bucket.clone(), test_branch(2), 12, 0).await.unwrap());
        let passing = direct.clone();
        *bucket.then.lock().unwrap() = Some(Box::new(move || {
            forked.checkpoint().now_or_never().unwrap().unwrap();
            collect(&*passing, 1, &KEEP).now_or_never().unwrap().unwrap();
        }));
        let recovered = recovery::recover(
            &*bucket,
            test_branch(2),
            Some(Origin { parent: test_branch(1), at }),
        )
        .await?;
        Ok(recovered.state.extents.keys().copied().collect())
    }

    /// TASK-178: a deletion that finds a branch live, as a placement cut off does once the latest
    /// placement's deletion ended the head, recovers it to give its forks' children their records;
    /// that other deletion finishing meanwhile, and a pass reclaiming what recovering the branch
    /// needed, a fork's, its parent's segments up to the fork among it, the deletion is done, as
    /// the other gave the children their records. Found by the simulation, seed 3953 of its
    /// `stale_placements` at SIM_SOAK=100, on main too.
    #[test]
    fn deletions_finished_by_others_meanwhile_are_done() {
        assert_eq!(block_on(delete_as_another_deletion_finishes()), Ok(()));
    }

    /// What deleting a fork gives, another deletion of it finishing and a pass of the collector
    /// running as it lists the fork's checkpoints, recovering it.
    async fn delete_as_another_deletion_finishes() -> Result<(), Error> {
        use crate::layout::{Contents, Segment};
        use crate::writer::Objects;
        let direct = Arc::new(Direct(SimBucket::new()));
        branch::create(&*direct, test_branch(1), 0).await.unwrap();
        let parent = Writer::open(direct.clone(), test_branch(1), 10, 0).await.unwrap();
        let extent = ExtentId::new(1, parent.chain().next_sequence());
        let extents = vec![(extent, b"held".to_vec())];
        commit(&parent, Commit { tag: 1, extents, ..Commit::default() }).await;
        let footer = parent
            .chain()
            .close(Contents { forks: vec![child(test_branch(2))], ..Contents::default() });
        let segment = Segment { data: Vec::new(), footer: footer.clone() }.encode().into();
        let at = parent.add_segment(Objects { footer, segment, own: Vec::new() }).unwrap();
        parent.upload(at).await.unwrap();
        parent.acknowledge().await.unwrap();
        commit(&parent, Commit { tag: 2, frees: vec![extent], ..Commit::default() }).await;
        parent.checkpoint().await.unwrap();
        Writer::open(direct.clone(), test_branch(2), 11, 0)
            .await
            .unwrap()
            .checkpoint()
            .await
            .unwrap();
        let passing = direct.clone();
        let bucket = Meanwhile {
            bucket: direct,
            branch: test_branch(2),
            meanwhile: std::sync::Mutex::new(Some(move || {
                branch::delete(&*passing, test_branch(2), 13, u64::MAX)
                    .now_or_never()
                    .unwrap()
                    .unwrap();
                collect(&*passing, 1, &KEEP).now_or_never().unwrap().unwrap();
            })),
        };
        branch::delete(&bucket, test_branch(2), 12, u64::MAX).await
    }

    /// TASK-407: a writer taken over, whose head another taking swapped since, acknowledges
    /// nothing: its swap fails, though its segment landed and its successor committed and
    /// checkpointed. The branch holds the successor's commit; the writer's segment, which no head
    /// names, nor ever will, a pass deletes.
    #[test]
    fn writers_taken_over_acknowledge_nothing() {
        let (acknowledged, last, kept) = block_on(acknowledge_once_taken_over());
        assert_eq!((acknowledged, last, kept), (Err(Error::Fenced), Some(2), false));
    }

    /// The head written whatever is there: the writer taken over writes it over its successor's,
    /// and the branch holds its commit, not the successor's, acknowledged (specs/bucket's
    /// NoHeadSwap).
    #[test]
    fn writers_taken_over_acknowledging_nothing_need_head_swap() {
        crate::set_rules(crate::Rules { head_swap: false, ..crate::Rules::ALL });
        let (acknowledged, last, _) = block_on(acknowledge_once_taken_over());
        crate::set_rules(crate::Rules::ALL);
        assert_eq!((acknowledged, last), (Ok(Some(0)), Some(1)));
    }

    /// What a writer taken over gets of the acknowledgement of its commit 1, its segment uploaded
    /// before its successor took the branch, committed 2 and checkpointed; the branch's last
    /// commit, after a pass; and whether the pass kept the writer's segment.
    async fn acknowledge_once_taken_over() -> (Result<Option<u64>, Error>, Option<u64>, bool) {
        let bucket = store();
        branch::create(&*bucket, test_branch(1), 0).await.unwrap();
        let taken = Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap();
        let objects = SegmentBuilder::from(Commit { tag: 1, ..Commit::default() })
            .close(&mut taken.chain())
            .frame();
        let segment = taken.add_segment(objects).unwrap();
        taken.upload(segment).await.unwrap();
        let successor = Writer::open(bucket.clone(), test_branch(1), 11, 0).await.unwrap();
        commit(&successor, Commit { tag: 2, ..Commit::default() }).await;
        successor.checkpoint().await.unwrap();
        let acknowledged = taken.acknowledge().await;
        collect(&*bucket, 1, &KEEP).await.unwrap();
        let key = layout::segment(test_branch(1), segment, taken.epoch());
        let kept = keys(&*bucket).await.contains(&key);
        let recovered = recovery::recover(&*bucket, test_branch(1), None).await;
        (acknowledged, recovered.ok().and_then(|recovered| recovered.state.last_commit), kept)
    }

    /// TASK-407: a pass between a writer's upload of its segment and its acknowledgement keeps the
    /// segment, no state naming it: of the head's epoch, at its end, the head may yet name it.
    #[test]
    fn segments_uploaded_not_yet_acknowledged_are_kept() {
        assert_eq!(block_on(acknowledge_past_a_pass()), Some(1));
    }

    /// The collector deleting what no head names yet: the writer's swap names a segment gone, and
    /// the branch is lost (specs/bucket's NoCollectorEpochs).
    #[test]
    fn segments_kept_until_acknowledged_need_collector_epochs() {
        crate::set_rules(crate::Rules { collector_epochs: false, ..crate::Rules::ALL });
        let last = block_on(acknowledge_past_a_pass());
        crate::set_rules(crate::Rules::ALL);
        assert_eq!(last, None);
    }

    /// The last commit of branch 1, whose writer acknowledged its commit 1 after a pass ran
    /// between its upload and its acknowledgement: none if the branch is lost.
    async fn acknowledge_past_a_pass() -> Option<u64> {
        let bucket = store();
        branch::create(&*bucket, test_branch(1), 0).await.unwrap();
        let writer = Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap();
        let objects = SegmentBuilder::from(Commit { tag: 1, ..Commit::default() })
            .close(&mut writer.chain())
            .frame();
        let segment = writer.add_segment(objects).unwrap();
        writer.upload(segment).await.unwrap();
        collect(&*bucket, 1, &KEEP).await.unwrap();
        writer.acknowledge().await.unwrap();
        let recovered = recovery::recover(&*bucket, test_branch(1), None).await;
        recovered.ok().and_then(|recovered| recovered.state.last_commit)
    }

    /// TASK-407: two writers' segments of one number land at two keys, each naming its writer's
    /// epoch: the one taken over lands its segment, never acknowledged, and its successor's, of the
    /// same number, lands beside it, acknowledged.
    #[test]
    fn writers_segments_of_one_number_land_at_their_own_keys() {
        assert_eq!(block_on(upload_where_a_writer_taken_over_did()), (Ok(()), Some(2)));
    }

    /// Keys naming no epoch: the successor's segment finds the one taken over's at its key, and
    /// its writer stops, a key written twice (specs/bucket's NoEpochKeys, whose writers overwrite).
    #[test]
    fn writers_segments_of_one_number_landing_apart_need_epoch_keys() {
        crate::set_rules(crate::Rules { epoch_keys: false, ..crate::Rules::ALL });
        let (uploaded, last) = block_on(upload_where_a_writer_taken_over_did());
        // Their one key, of no epoch.
        let key = layout::segment(test_branch(1), 0, 2);
        crate::set_rules(crate::Rules::ALL);
        let corrupt = Error::Corrupt(format!("{key} holds another segment"));
        assert_eq!((uploaded, last), (Err(corrupt), None));
    }

    /// What the successor of a writer taken over gets of the upload of its commit 2 at segment 0,
    /// where the writer taken over uploaded its commit 1, never acknowledged; and the branch's last
    /// commit, once it acknowledged what it could.
    async fn upload_where_a_writer_taken_over_did() -> (Result<(), Error>, Option<u64>) {
        let bucket = store();
        branch::create(&*bucket, test_branch(1), 0).await.unwrap();
        let taken = Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap();
        let objects = SegmentBuilder::from(Commit { tag: 1, ..Commit::default() })
            .close(&mut taken.chain())
            .frame();
        let segment = taken.add_segment(objects).unwrap();
        taken.upload(segment).await.unwrap();
        let successor = Writer::open(bucket.clone(), test_branch(1), 11, 0).await.unwrap();
        let objects = SegmentBuilder::from(Commit { tag: 2, ..Commit::default() })
            .close(&mut successor.chain())
            .frame();
        let segment = successor.add_segment(objects).unwrap();
        let uploaded = successor.upload(segment).await;
        let _ = successor.acknowledge().await;
        let recovered = recovery::recover(&*bucket, test_branch(1), None).await;
        (uploaded, recovered.ok().and_then(|recovered| recovered.state.last_commit))
    }

    /// TASK-180: a pass that read a point live, its records listed again finding a fork being made
    /// of it after another pass took the collector's lease, the point deleted and what it named
    /// reclaimed meanwhile, finds what the fork names lost: fenced, it stops, deleting nothing, as
    /// it would before deleting. Found by the simulation, seed 13797 of its `restores` at
    /// SIM_SOAK=100. TASK-395.4: it reads the point's record again then, finds it deleted, and gives
    /// the fork up, as the pass that fenced it would have, rather than failing: fenced, it deletes
    /// nothing all the same.
    #[test]
    fn passes_fenced_finding_forks_of_points_lost_stop() {
        assert_eq!(block_on(pass_fenced_as_a_point_goes(Raced::Fork)), Ok(Report::default()));
    }

    /// TASK-395.4: a pass that finds a fork of a point being made lost, its point read again
    /// deleted, gives the fork up, whether or not fenced: not live, it lost nothing, as the
    /// collector's recoveries, unlike specs/bucket's, read the bucket at many times (doc-7, For the
    /// code, beyond the model). Its record says so after.
    #[test]
    fn passes_finding_forks_of_points_lost_give_them_up_once_their_points_are_deleted() {
        crate::set_rules(crate::Rules { passes_fenced_stop: false, ..crate::Rules::ALL });
        let passed = block_on(pass_fenced_as_a_point_goes(Raced::Fork));
        crate::set_rules(crate::Rules::ALL);
        assert_eq!(passed, Ok(Report::default()));
    }

    /// TASK-180, since TASK-395.4 gives up on such a fork: a pass finding a failed fork's claim lost,
    /// one a writer has, landed with what it names reclaimed by a pass that fenced it, stops,
    /// deleting nothing, as it would before deleting.
    #[test]
    fn passes_fenced_finding_claims_lost_stop() {
        assert_eq!(block_on(pass_fenced_as_a_point_goes(Raced::Claim)), Ok(Report::default()));
    }

    /// A pass failing on what it finds lost, fenced or not: it fails.
    #[test]
    fn passes_fenced_finding_forks_of_points_lost_stopping_need_passes_fenced_stop() {
        crate::set_rules(crate::Rules { passes_fenced_stop: false, ..crate::Rules::ALL });
        let passed = block_on(pass_fenced_as_a_point_goes(Raced::Claim));
        crate::set_rules(crate::Rules::ALL);
        let lost = Error::Lost("branch 1/1/4: what its recovery starts from keeps going".into());
        assert_eq!(passed, Err(lost));
    }

    /// What lands as [`pass_fenced_as_a_point_goes`] lists the records a second time.
    #[derive(Clone, Copy)]
    enum Raced {
        /// A fork of the point being made, which finds it deleted.
        Fork,
        /// A failed fork's claim, of the point's origin, which a writer takes.
        Claim,
    }

    /// What a pass gives, a point of branch 1 deleted, a later pass reclaiming what it named, and
    /// a fork of the point being made to branch 4, or a claim of 4 landing, as the pass lists the
    /// records a second time, having read the point's before.
    async fn pass_fenced_as_a_point_goes(raced: Raced) -> Result<Report, Error> {
        use crate::layout::{Contents, Origin, Segment};
        use crate::writer::Objects;
        let direct = Arc::new(Direct(SimBucket::new()));
        branch::create(&*direct, test_branch(1), 0).await.unwrap();
        let writer = Writer::open(direct.clone(), test_branch(1), 10, 0).await.unwrap();
        let extent = ExtentId::new(1, writer.chain().next_sequence());
        let extents = vec![(extent, b"held".to_vec())];
        commit(&writer, Commit { tag: 1, extents, ..Commit::default() }).await;
        let point = Child { branch: test_branch(3), owner: 0x33, point: true };
        let footer = writer.chain().close(Contents { forks: vec![point], ..Contents::default() });
        let segment = Segment { data: Vec::new(), footer: footer.clone() }.encode().into();
        let at = writer.add_segment(Objects { footer, segment, own: Vec::new() }).unwrap();
        writer.upload(at).await.unwrap();
        writer.acknowledge().await.unwrap();
        commit(&writer, Commit { tag: 2, frees: vec![extent], ..Commit::default() }).await;
        writer.checkpoint().await.unwrap();
        let bucket =
            Hooked::new(direct.clone(), Hook::Listing(layout::records(layout::TEST_CELL), 2));
        let raced_on = direct.clone();
        *bucket.then.lock().unwrap() = Some(Box::new(move || {
            let direct = raced_on;
            let origin = Origin { parent: test_branch(1), at };
            branch::delete(&*direct, test_branch(3), 11, u64::MAX).now_or_never().unwrap().unwrap();
            collect(&*direct, 2, &KEEP).now_or_never().unwrap().unwrap();
            let left = keys(&*direct).now_or_never().unwrap();
            assert!(
                !left.contains(&layout::segment(test_branch(1), at, 1)),
                "the point's state stayed"
            );
            match raced {
                Raced::Fork => {
                    let forked =
                        branch::fork_point(&*direct, test_branch(4), test_branch(3), origin, 0x44)
                            .now_or_never()
                            .unwrap();
                    assert_eq!(forked, Err(Error::Deleted));
                }
                Raced::Claim => {
                    let claim = crate::record::Kind::Claim(origin).of(0x44);
                    let key = layout::record(test_branch(4));
                    let created = bucket::create(&*direct, &key, layout::encode(&claim));
                    assert!(matches!(created.now_or_never().unwrap(), Created::Created));
                    let taking = crate::head::take(&*direct, test_branch(4), 12, 0);
                    taking.now_or_never().unwrap().unwrap();
                }
            }
        }));
        let passed = collect(&bucket, 1, &KEEP).await;
        if let (Raced::Fork, Ok(_)) = (raced, &passed) {
            let record = branch::record(&*direct, test_branch(4)).await.unwrap().unwrap();
            assert!(record.deleted && record.forking().is_some(), "{record:?}: given up");
        }
        passed
    }

    /// TASK-173: a pass that recovers a child by its parent's state, the child having no record as
    /// it listed them, finds on listing them again the child's record of another origin: a restore
    /// of a point took the child's ID before the parent's writer recorded its fork, which was
    /// refused. It recovers the child again, by its record's origin, and keeps what that needs, the
    /// parent's segments up to the point, which its checkpoint since covers. Found by
    /// specs/bucket's TLA+ (TASK-165.2), the model BucketPointsDeep.
    #[test]
    fn children_restored_from_points_keep_their_origins_though_their_ids_were_forked_to() {
        let report = block_on(pass_as_a_child_is_restored());
        let counts = report.map(|report| (report.deleted, report.compacted, report.retired));
        // The refused fork's segment, its checkpoint covering it.
        assert_eq!(counts, Ok((1, 0, 0)));
    }

    /// A pass recovering each branch once, by its parent's state: it deletes what the restore's
    /// child needs, which fails every pass after, unless the restore's reference keeps it.
    #[test]
    fn children_restored_from_points_keeping_their_origins_need_recoveries_by_origins() {
        // TASK-395.1: the restore's reference keeps its origin's state at the fork point too: with
        // references gone, as specs/bucket's NoRecoveriesByOrigins has it.
        let rules =
            crate::Rules { recoveries_by_origins: false, references: false, ..crate::Rules::ALL };
        crate::set_rules(rules);
        let passed = block_on(pass_as_a_child_is_restored());
        crate::set_rules(crate::Rules::ALL);
        let lost = Error::Lost("branch 1/1/3: what its recovery starts from keeps going".into());
        assert_eq!(passed, Err(lost));
    }

    /// What a pass gives after one that, having listed the records, recovers branch 1 as its writer
    /// forks point 2, a restore of the point takes child 3, and the writer forks to 3, refused, the
    /// point is deleted and the writer checkpoints, dropping it.
    async fn pass_as_a_child_is_restored() -> Result<Report, Error> {
        use crate::layout::{Contents, Segment};
        use crate::writer::Objects;
        const PARENT: u64 = 1;
        const POINT: u64 = 2;
        const CHILD: u64 = 3;
        const EARLIER: u64 = 4;
        /// Forks `writer`'s branch to `child`: at the segment it returns, acknowledged.
        async fn fork(writer: &Writer<Direct>, child: Child) -> u64 {
            let footer =
                writer.chain().close(Contents { forks: vec![child], ..Contents::default() });
            let framed = Segment { data: Vec::new(), footer: footer.clone() }.encode().into();
            let segment =
                writer.add_segment(Objects { footer, segment: framed, own: Vec::new() }).unwrap();
            writer.upload(segment).await.unwrap();
            writer.acknowledge().await.unwrap();
            segment
        }
        let direct = Arc::new(Direct(SimBucket::new()));
        branch::create(&*direct, test_branch(PARENT), 0).await.unwrap();
        let writer = Writer::open(direct.clone(), test_branch(PARENT), 10, 0).await.unwrap();
        // A fork, recorded before the pass lists the records, whose recovery reads the parent's
        // checkpoints first, as the cell's listing shows them: the restore's child's, after,
        // lists them anew (TASK-395.16), as the spec reads them. Then a commit: what the restore's
        // child needs that a pass may delete.
        fork(&writer, Child { branch: test_branch(EARLIER), owner: 0x44, point: false }).await;
        commit(&writer, Commit { tag: 1, ..Commit::default() }).await;
        let restoring = direct.clone();
        let meanwhile = move || {
            let work = async {
                let point = Child { branch: test_branch(POINT), owner: 0x22, point: true };
                let at = fork(&writer, point).await;
                let origin = Origin { parent: test_branch(PARENT), at };
                branch::fork_point(
                    &*restoring,
                    test_branch(CHILD),
                    test_branch(POINT),
                    origin,
                    0x33,
                )
                .await
                .unwrap();
                fork(&writer, Child { branch: test_branch(CHILD), owner: 0x34, point: false })
                    .await;
                assert!(writer.refused(test_branch(CHILD)));
                branch::delete(&*restoring, test_branch(POINT), 20, 0).await.unwrap();
                writer.checkpoint().await.unwrap();
            };
            work.now_or_never().expect("a direct bucket answers at once");
        };
        let during = Meanwhile {
            bucket: direct.clone(),
            branch: test_branch(PARENT),
            meanwhile: Mutex::new(Some(meanwhile)),
        };
        let now = Options { compact_below: 0.0, grace: 0 };
        collect(&during, 1, &now).await?;
        collect(&*direct, 2, &now).await
    }

    /// TASK-174: a root that took the ID of a fork's child, and was deleted, keeps its record:
    /// the fork's parent's writer, which acknowledged the fork before the parent's deletion ended
    /// its head and was slow to record the child, finds it deleted once a pass ran, and makes
    /// none. Found by specs/bucket's TLA+ (TASK-165.2), the model BucketForks.
    #[test]
    fn deleted_roots_keep_their_records_from_slow_writers_forking_to_their_ids() {
        let (record, passed) = block_on(record_a_fork_to_a_deleted_root());
        let deleted = Record { deleted: true, ..crate::record::Kind::Root.of(0x21) };
        // The cell dead, its roots' IDs named, no allocator's: kept, and the second pass skips
        // it, unchanged since the first's deletes, which that one recorded (TASK-455).
        let skipped = Report { skipped: 1, ..Report::default() };
        assert_eq!((record, passed), (Some(deleted), Ok(skipped)));
    }

    /// The collector removing the dead cell whole, whatever its records (TASK-455): the slow writer
    /// makes the child's record again, a fork's child's, naming its parent's segments, which went
    /// with the cell, and the next pass fails.
    #[test]
    fn deleted_roots_keeping_their_records_need_records_kept() {
        crate::set_rules(crate::Rules { records_kept: false, ..crate::Rules::ALL });
        let (record, passed) = block_on(record_a_fork_to_a_deleted_root());
        crate::set_rules(crate::Rules::ALL);
        let made = crate::record::Kind::Fork(Origin { parent: test_branch(1), at: 0 }).of(0x22);
        assert_eq!(
            (record, passed),
            (Some(made), Err(Error::Lost(format!("segment 0 of {}", test_branch(1)))))
        );
    }

    /// TASK-455: a dead cell a pass does not remove, of an ID no allocator gave, its segments
    /// compacted before its branch's deletion: the first pass in full deletes the segments, and
    /// their moves, whose objects it listed, the next, which goes in full for that, as the first
    /// recorded its listing, not what its deletes left (the simulation found the moves kept for
    /// good when it did); then passes skip it, and nothing is left but its record, head and lease.
    #[test]
    fn dead_cells_kept_lose_the_moves_of_what_their_first_pass_deleted() {
        let sim = SimBucket::new();
        block_on(async {
            let bucket = Arc::new(Direct(sim.clone()));
            mostly_freed(bucket.clone()).await;
            let compacting = Options { compact_below: 0.5, grace: 0 };
            collect(&*bucket, 1, &compacting).await.unwrap();
            let moved = keys(&*bucket).await;
            let moves = |keys: &[String]| -> usize {
                keys.iter().filter(|key| matches!(layout::kind(key), Kind::Moves { .. })).count()
            };
            assert_eq!(moves(&moved), SEGMENTS as usize);
            branch::delete(&*bucket, test_branch(1), 11, 0).await.unwrap();
            let first = collect(&*bucket, 2, &compacting).await.unwrap();
            assert_eq!(moves(&keys(&*bucket).await), SEGMENTS as usize, "{first:?}");
            let second = collect(&*bucket, 3, &compacting).await.unwrap();
            assert_eq!((second.deleted, second.skipped), (SEGMENTS as usize, 0));
            let third = collect(&*bucket, 4, &compacting).await.unwrap();
            assert_eq!(third, Report { skipped: 1, ..Report::default() });
            let left = keys(&*bucket).await;
            let expected = vec![layout::record(test_branch(1)), layout::head(test_branch(1))];
            let mut left = left;
            left.retain(|key| !matches!(layout::kind(key), Kind::Cell { .. }));
            assert_eq!(left, expected);
            assert_eq!(crate::testing::uncollected(&*bucket, 0.5).await, Vec::<String>::new());
        });
    }

    /// TASK-455's AC #3: a base's cell keeps nothing of the clones of its point made and gone, of
    /// IDs an allocator gave: each clone's reference retired unmarked, and its cell removed whole,
    /// so that the base's cell holds the point's reference alone, as it did before any, and its
    /// pass lists as much whatever the clones were. Before, a mark a clone, for good.
    #[test]
    fn bases_keep_nothing_of_clones_made_and_gone() {
        let (few, report) = block_on(base_after_clones(3));
        assert_eq!((report.removed, report.retired), (3, 3));
        let (many, report) = block_on(base_after_clones(30));
        assert_eq!((report.removed, report.retired), (30, 30));
        assert_eq!(few, many);
        let kinds: Vec<&str> = few.iter().map(|key| layout::kind(key).name()).collect();
        assert_eq!(kinds.iter().filter(|kind| **kind == "reference").count(), 1, "{few:#?}");
        assert!(!kinds.contains(&"retired"), "{few:#?}");
    }

    /// The objects under the base's cell once `clones` clones of a point of it, of IDs an
    /// allocator gave, each a cell of its own, were made and deleted and passes ran until quiet,
    /// and what those did; nothing left of the clones' cells.
    async fn base_after_clones(clones: u64) -> (Vec<String>, Report) {
        use crate::layout::Origin;
        let bucket = Arc::new(Direct(SimBucket::new()));
        let (base, point) = (celled(1, 1), celled(2, 2));
        branch::create(&*bucket, base, 0).await.unwrap();
        let writer = Writer::open(bucket.clone(), base, 10, 0).await.unwrap();
        let extent = ExtentId::new(1, writer.chain().next_sequence());
        let made = Child { branch: point, owner: 0x22, point: true };
        let at = commit_and_fork(&writer, vec![(extent, vec![1; 64])], made).await;
        writer.checkpoint().await.unwrap();
        let origin = Origin { parent: base, at };
        let ids = crate::objectid::Allocator::new();
        let mut cells = Vec::new();
        for n in 0..clones {
            let clone = Branch {
                cell: layout::Cell { tenant: 1, id: 0x100 + n },
                id: ids.next(&*bucket).await,
            };
            branch::fork_point(&*bucket, clone, point, origin, 0x40 + n).await.unwrap();
            branch::delete(&*bucket, clone, 20, u64::MAX).await.unwrap();
            cells.push(clone.cell);
        }
        let options = Options { compact_below: 0.0, grace: 0 };
        let report = crate::testing::collect_until_quiet(&*bucket, &options).await.unwrap();
        for cell in cells {
            assert_eq!(bucket::list(&*bucket, &cell.prefix()).await, Vec::new(), "{cell}");
        }
        assert_eq!(crate::testing::uncollected(&*bucket, 0.0).await, Vec::<String>::new());
        let listed = bucket::list(&*bucket, &base.cell.prefix()).await;
        (listed.into_iter().map(|meta| meta.key).collect(), report)
    }

    /// The rule that a dead cell of roots goes whole only if their IDs are an allocator's: one of a
    /// root of a guest's ID taken for given, the slow writer makes the fork's child's record again,
    /// naming its parent's segments, gone with the cell, and the next pass fails (TASK-455).
    #[test]
    fn deleted_roots_keeping_their_records_need_ids_given() {
        crate::set_rules(crate::Rules { ids_given: false, ..crate::Rules::ALL });
        let (record, passed) = block_on(record_a_fork_to_a_deleted_root());
        crate::set_rules(crate::Rules::ALL);
        let made = crate::record::Kind::Fork(Origin { parent: test_branch(1), at: 0 }).of(0x22);
        assert_eq!(
            (record, passed),
            (Some(made), Err(Error::Lost(format!("segment 0 of {}", test_branch(1)))))
        );
    }

    /// Child 2's record, and what a pass gives, once the writer of branch 1 forked to 2, which a
    /// root took and was deleted, and recorded the fork after branch 1's deletion and a pass.
    async fn record_a_fork_to_a_deleted_root() -> (Option<Record>, Result<Report, Error>) {
        use crate::layout::{Contents, Segment};
        use crate::writer::Objects;
        const PARENT: u64 = 1;
        const CHILD: u64 = 2;
        let direct = Arc::new(Direct(SimBucket::new()));
        let (slow, open) =
            Gated::new(direct.clone(), Gate::Put(layout::record(test_branch(CHILD))));
        let slow = Arc::new(slow);
        branch::create(&*direct, test_branch(PARENT), 0).await.unwrap();
        let writer = Writer::open(slow, test_branch(PARENT), 10, 0).await.unwrap();
        branch::create(&*direct, test_branch(CHILD), 0x21).await.unwrap();
        branch::delete(&*direct, test_branch(CHILD), 11, 0).await.unwrap();
        let forked = Child { branch: test_branch(CHILD), owner: 0x22, point: false };
        let footer = writer.chain().close(Contents { forks: vec![forked], ..Contents::default() });
        let segment = Segment { data: Vec::new(), footer: footer.clone() }.encode().into();
        let at = writer.add_segment(Objects { footer, segment, own: Vec::new() }).unwrap();
        writer.upload(at).await.unwrap();
        // The head read, the fork's child's record waits.
        let mut acknowledging = Box::pin(writer.acknowledge());
        assert!((&mut acknowledging).now_or_never().is_none());
        branch::delete(&*direct, test_branch(PARENT), 12, 0).await.unwrap();
        let now = Options { compact_below: 0.0, grace: 0 };
        collect(&*direct, 1, &now).await.unwrap();
        open.send(()).unwrap();
        assert_eq!((&mut acknowledging).now_or_never(), Some(Ok(Some(at))));
        let record = branch::record(&*direct, test_branch(CHILD)).await.unwrap();
        (record, collect(&*direct, 2, &now).await)
    }

    /// TASK-455's review: a fork's child whose deletion, asked before its parent's writer recorded
    /// it, ended its head and stopped before its record, its cell holding nothing else: a pass
    /// keeps the head, final, a fence, and once the parent's writer records the child, no writer
    /// takes it.
    #[test]
    fn deletions_fences_stand_until_their_records() {
        let fenced = block_on(take_a_child_whose_deletion_stopped_before_its_record());
        assert_eq!(fenced, (layout::Head::FINAL, Err(Error::Deleted)));
    }

    /// Without the rule, the pass deletes the head, and a writer takes the child once it is
    /// recorded, which the deletion, going on, marks deleted under it: specs/bucket's
    /// NoFencesKept.
    #[test]
    fn deletions_fences_need_fences_kept() {
        crate::set_rules(crate::Rules { fences_kept: false, ..crate::Rules::ALL });
        let fenced = block_on(take_a_child_whose_deletion_stopped_before_its_record());
        crate::set_rules(crate::Rules::ALL);
        assert_eq!(fenced, (0, Ok(())));
    }

    /// The epoch of the head of child 2, in a cell of its own, after a pass, and what a writer's
    /// taking of it gives once recorded: branch 1's writer forked to it, and stalls recording it,
    /// as its deletion ends its head and stops before its record (the TASK-79.78 race).
    async fn take_a_child_whose_deletion_stopped_before_its_record() -> (u64, Result<(), Error>) {
        use crate::layout::{Contents, Segment};
        use crate::writer::Objects;
        let direct = Arc::new(Direct(SimBucket::new()));
        let (parent, child) = (celled(1, 1), celled(2, 2));
        let (slow, open) = Gated::new(direct.clone(), Gate::Put(layout::record(child)));
        branch::create(&*direct, parent, 0).await.unwrap();
        let writer = Writer::open(Arc::new(slow), parent, 10, 0).await.unwrap();
        let forked = Child { branch: child, owner: 0x22, point: false };
        let footer = writer.chain().close(Contents { forks: vec![forked], ..Contents::default() });
        let segment = Segment { data: Vec::new(), footer: footer.clone() }.encode().into();
        let at = writer.add_segment(Objects { footer, segment, own: Vec::new() }).unwrap();
        writer.upload(at).await.unwrap();
        // The fork acknowledged, the child's record waits.
        let mut acknowledging = Box::pin(writer.acknowledge());
        assert!((&mut acknowledging).now_or_never().is_none());
        let scenario = fail::FailScenario::setup();
        fail::cfg("branch::head_ended", "return").unwrap();
        let deleting = branch::delete(&*direct, child, 20, 0).await;
        scenario.teardown();
        assert_eq!(deleting, Err(Error::Read("failpoint branch::head_ended".to_string())));
        let now = Options { compact_below: 0.0, grace: 0 };
        collect(&*direct, 1, &now).await.unwrap();
        let head = crate::head::epoch(&*direct, child).await.unwrap();
        open.send(()).unwrap();
        assert_eq!((&mut acknowledging).now_or_never(), Some(Ok(Some(at))));
        let recorded = branch::record(&*direct, child).await.unwrap();
        assert_eq!(recorded, Some(crate::record::Kind::Fork(Origin { parent, at }).of(0x22)));
        (head, Writer::open(direct.clone(), child, 30, 0).await.map(drop))
    }

    /// TASK-455's review: a clone a store resumed, its host stalling before its reference, made,
    /// deleted and its cell removed, the host's reference landing after: its retirement marked, the
    /// base's next pass deletes it, and nothing is left; the host's fork fails, its record gone.
    #[test]
    fn late_references_of_resumed_clones_go() {
        let (left, late, marks, _) = block_on(left_by_a_late_host_of_a_resumed_clone());
        assert_eq!((left, late, marks), (Vec::new(), Err(Error::NoBranch), 1));
    }

    /// Without the rule, the clone resumed retires its reference unmarked, as one its host made
    /// alone, and the host's reference, written after, stays for good: specs/bucket's
    /// NoResumesMarked.
    #[test]
    fn late_references_of_resumed_clones_need_resumes_marked() {
        crate::set_rules(crate::Rules { resumes_marked: false, ..crate::Rules::ALL });
        let (left, late, marks, reference) = block_on(left_by_a_late_host_of_a_resumed_clone());
        crate::set_rules(crate::Rules::ALL);
        assert_eq!(
            (left, late, marks),
            (vec![format!("garbage left: {reference}")], Err(Error::NoBranch), 0)
        );
    }

    /// A point of branch 1's state at its segment `at` (its writer's), branch 2, both in cells of
    /// their own: the writer, which sealed `extent` before the fork, and the point's origin.
    async fn pointed<B: Bucket>(bucket: Arc<B>) -> (Writer<B>, ExtentId, Origin) {
        let (origin_branch, point) = (celled(1, 1), celled(2, 2));
        branch::create(&*bucket, origin_branch, 0).await.unwrap();
        let writer = Writer::open(bucket, origin_branch, 10, 0).await.unwrap();
        let extent = ExtentId::new(1, writer.chain().next_sequence());
        let made = Child { branch: point, owner: 0x22, point: true };
        let at = commit_and_fork(&writer, vec![(extent, b"held".to_vec())], made).await;
        (writer, extent, Origin { parent: origin_branch, at })
    }

    /// What [`crate::testing::uncollected`] finds, what the host's fork gave and how many marks
    /// the base's cell holds, once a clone of a point of the base, of an ID an allocator gave, was
    /// begun by its host, which stalled before its reference; a store resumed it; it was deleted,
    /// and passes ran until quiet, which removed its cell; and the host's reference landed, and
    /// passes ran again. And the reference's key.
    async fn left_by_a_late_host_of_a_resumed_clone()
    -> (Vec<String>, Result<(), Error>, usize, String) {
        let direct = Arc::new(Direct(SimBucket::new()));
        let (_writer, _, origin) = pointed(direct.clone()).await;
        let id = crate::objectid::Allocator::new().next(&*direct).await;
        let clone = Branch { cell: layout::Cell { tenant: 1, id: 3 }, id };
        let reference = layout::reference(origin.parent, origin.at, clone);
        let (slow, open) = Gated::new(direct.clone(), Gate::Put(reference.clone()));
        let mut forking = Box::pin(branch::fork_point(&slow, clone, celled(2, 2), origin, 0x33));
        assert!((&mut forking).now_or_never().is_none());
        branch::resume_point_fork(&*direct, clone).await.unwrap();
        branch::delete(&*direct, clone, 20, u64::MAX).await.unwrap();
        let options = Options { compact_below: 0.0, grace: 0 };
        crate::testing::collect_until_quiet(&*direct, &options).await.unwrap();
        assert_eq!(bucket::list(&*direct, &clone.cell.prefix()).await, Vec::new());
        open.send(()).unwrap();
        let late = forking.await;
        crate::testing::collect_until_quiet(&*direct, &options).await.unwrap();
        let listed = bucket::list(&*direct, &origin.parent.cell.prefix()).await;
        let marks = listed.iter().filter(|meta| layout::kind(&meta.key).name() == "retired");
        let marks = marks.count();
        (crate::testing::uncollected(&*direct, 0.0).await, late, marks, reference)
    }

    /// TASK-455's review: a clone whose host died after its record, before its reference, which a
    /// store resumed: the resume writes its reference, which keeps the base's state at the fork
    /// point once the base freed it and the point was deleted, and the clone holds what it did.
    #[test]
    fn clones_resumed_keep_their_references() {
        let (referred, held) = block_on(clone_resumed_after_its_host_died());
        assert_eq!((referred, held), (true, Ok(1)));
    }

    /// Without the rule, the resume makes the clone with no reference, and the base's passes,
    /// the point gone, reclaim what the clone recovers from: specs/bucket's NoResumesRefer.
    #[test]
    fn clones_resumed_need_resumes_refer() {
        crate::set_rules(crate::Rules { resumes_refer: false, ..crate::Rules::ALL });
        let (referred, held) = block_on(clone_resumed_after_its_host_died());
        crate::set_rules(crate::Rules::ALL);
        // The base's checkpoint at the fork point, which the clone recovers from.
        let lost = layout::checkpoint(celled(1, 1), 1, 1);
        assert_eq!((referred, held), (false, Err(Error::Lost(lost))));
    }

    /// Whether a clone of a point of the base has its reference once its host died after its
    /// record and a store resumed it; and how many of the extents it names, recovered, are where
    /// it says, once the point was deleted, the base freed what it held and checkpointed, and
    /// passes ran until quiet.
    async fn clone_resumed_after_its_host_died() -> (bool, Result<usize, Error>) {
        let direct = Arc::new(Direct(SimBucket::new()));
        let (writer, extent, origin) = pointed(direct.clone()).await;
        let clone = celled(3, 3);
        let reference = layout::reference(origin.parent, origin.at, clone);
        let (slow, never) = Gated::new(direct.clone(), Gate::Put(reference.clone()));
        let mut forking = Box::pin(branch::fork_point(&slow, clone, celled(2, 2), origin, 0x33));
        assert!((&mut forking).now_or_never().is_none());
        // The host dies there.
        drop(forking);
        drop(never);
        branch::resume_point_fork(&*direct, clone).await.unwrap();
        let referred = bucket::get(&*direct, &reference).await.is_some();
        // The point deleted, the base's checkpoint after drops its fork to it.
        branch::delete(&*direct, celled(2, 2), 20, 0).await.unwrap();
        commit(&writer, Commit { tag: 3, frees: vec![extent], ..Commit::default() }).await;
        writer.checkpoint().await.unwrap();
        let options = Options { compact_below: 0.0, grace: 0 };
        crate::testing::collect_until_quiet(&*direct, &options).await.unwrap();
        let recovered = crate::recovery::recover(&*direct, clone, Some(origin)).await;
        let held = match recovered {
            Ok(recovered) => {
                let mut held = 0;
                for location in recovered.state.extents.values() {
                    held += usize::from(bucket::get(&*direct, &location.key).await.is_some());
                }
                Ok(held)
            }
            Err(error) => Err(error),
        };
        (referred, held)
    }

    /// TASK-183: two deletions of a branch at once, a fork's segment of its writer's in flight,
    /// never acknowledged. The segment lands, at its own key, which no head names: both deletions
    /// recover the branch from the head the first ended, which nothing swaps after, and find no
    /// fork: a pass after lets go of nothing a child needs, as no fork's child has a record. Found
    /// by specs/bucket's TLA+ (TASK-165.2), the model BucketForks once a deletion recovered in two
    /// steps; the head (TASK-407) made the deletions' fences needless.
    #[test]
    fn deletions_at_once_find_no_forks_landing_between_them() {
        // The first pass, after both deletions, deleted all of the branch, its cell dead, its
        // root's ID named: the second skips it (TASK-455).
        let landed = (Ok(()), None, Ok(Report { skipped: 1, ..Report::default() }));
        assert_eq!(block_on(delete_twice_as_a_fork_lands()), landed);
    }

    /// What the upload of branch 1's fork to child 2 gives, landing between two deletions'
    /// recoveries of 1: the first's, which then waits to mark 1 deleted, and the second's, which
    /// then waits to record the forks it found; the first marks 1 deleted, a pass runs, and the
    /// second goes on. The child's record, and what a pass after gives.
    async fn delete_twice_as_a_fork_lands()
    -> (Result<(), Error>, Option<Record>, Result<Report, Error>) {
        use crate::layout::{Contents, Segment};
        use crate::writer::Objects;
        const PARENT: u64 = 1;
        const CHILD: u64 = 2;
        let direct = Arc::new(Direct(SimBucket::new()));
        branch::create(&*direct, test_branch(PARENT), 0).await.unwrap();
        let writer = Writer::open(direct.clone(), test_branch(PARENT), 10, 0).await.unwrap();
        commit(&writer, Commit { tag: 1, ..Commit::default() }).await;
        let footer = writer
            .chain()
            .close(Contents { forks: vec![child(test_branch(CHILD))], ..Contents::default() });
        let segment = Segment { data: Vec::new(), footer: footer.clone() }.encode().into();
        let at = writer.add_segment(Objects { footer, segment, own: Vec::new() }).unwrap();
        // The first deletion, recovered, waits to mark the branch deleted.
        let (marking, mark) =
            Gated::new(direct.clone(), Gate::Put(layout::record(test_branch(PARENT))));
        let marking = Arc::new(marking);
        let mut first =
            Box::pin(async move { branch::delete(&*marking, test_branch(PARENT), 11, 0).await });
        assert!((&mut first).now_or_never().is_none());
        let landed = writer.upload(at).await;
        // The second, recovered, waits to record the fork's child, if it found the fork.
        let (recording, record) =
            Gated::new(direct.clone(), Gate::Put(layout::record(test_branch(CHILD))));
        let recording = Arc::new(recording);
        let mut second =
            Box::pin(async move { branch::delete(&*recording, test_branch(PARENT), 12, 0).await });
        let second_done = (&mut second).now_or_never();
        mark.send(()).unwrap();
        first.await.unwrap();
        let now = Options { compact_below: 0.0, grace: 0 };
        collect(&*direct, 1, &now).await.unwrap();
        // Done already, finding no fork, the second deletion dropped its gate.
        let _ = record.send(());
        let deleted = match second_done {
            Some(deleted) => deleted,
            None => second.await,
        };
        assert_eq!(deleted, Ok(()));
        let record = branch::record(&*direct, test_branch(CHILD)).await.unwrap();
        (landed, record, collect(&*direct, 2, &now).await)
    }

    /// TASK-380, under the head (TASK-407): two deletions of a branch at once, its writer's
    /// segment forking to a child in flight, the extent of its own the segment's commit sealed
    /// uploaded ahead of it, and the branch's segments to an earlier fork kept by that fork's
    /// child. The first deletion ends the head and marks the branch deleted; a pass deletes the
    /// extent, which no live branch needs; the segment lands, at its own key, which no head names,
    /// and the second deletion, which read the head live before the first ended it, finds it ended
    /// as it ends it, and recovers from it: it records no child.
    #[test]
    fn deletions_at_once_record_no_forks_landing_after_the_first() {
        assert_eq!(block_on(delete_twice_as_a_fork_lands_past_a_pass()), (Ok(()), None, vec![]));
    }

    /// What the second of two deletions of branch 1 gives, its writer's segment forking to child
    /// 3, with an extent of its own, landing after a pass deleted what the first left, child 2 of
    /// an earlier fork keeping the branch's segments to that fork; child 3's record, and the keys
    /// of the extents its state names that are gone.
    async fn delete_twice_as_a_fork_lands_past_a_pass()
    -> (Result<(), Error>, Option<Record>, Vec<String>) {
        use crate::layout::{Contents, Segment};
        use crate::writer::Objects;
        const PARENT: u64 = 1;
        const KEPT: u64 = 2;
        const CHILD: u64 = 3;
        let direct = Arc::new(Direct(SimBucket::new()));
        branch::create(&*direct, test_branch(PARENT), 0).await.unwrap();
        // The writer, whose create of its segment 1 waits.
        let at = 1;
        let (writing, landing) =
            Gated::new(direct.clone(), Gate::Put(layout::segment(test_branch(PARENT), at, 1)));
        let writer = Writer::open(Arc::new(writing), test_branch(PARENT), 10, 0).await.unwrap();
        let footer = writer
            .chain()
            .close(Contents { forks: vec![child(test_branch(KEPT))], ..Contents::default() });
        let segment = Segment { data: Vec::new(), footer: footer.clone() }.encode().into();
        let kept = writer.add_segment(Objects { footer, segment, own: Vec::new() }).unwrap();
        writer.upload(kept).await.unwrap();
        writer.acknowledge().await.unwrap();
        // A commit sealing an extent on its own, then a fork to the child, in one segment.
        let own = vec![(writer.chain().next_sequence(), b"own".to_vec())];
        let forking = SegmentBuilder::from(Commit { tag: 1, own, ..Commit::default() })
            .fork(child(test_branch(CHILD)), &mut writer.chain())
            .frame();
        assert_eq!(writer.add_segment(forking).unwrap(), at);
        let mut uploading = Box::pin(writer.upload(at));
        assert!((&mut uploading).now_or_never().is_none());
        // The second deletion, the head read live, waits to end it.
        let (gated, open) =
            Gated::new(direct.clone(), Gate::Put(layout::head(test_branch(PARENT))));
        let gated = Arc::new(gated);
        let mut second =
            Box::pin(async move { branch::delete(&*gated, test_branch(PARENT), 12, 0).await });
        assert!((&mut second).now_or_never().is_none());
        branch::delete(&*direct, test_branch(PARENT), 11, 0).await.unwrap();
        let now = Options { compact_below: 0.0, grace: 0 };
        collect(&*direct, 1, &now).await.unwrap();
        landing.send(()).unwrap();
        assert_eq!((&mut uploading).now_or_never(), Some(Ok(())));
        open.send(()).unwrap();
        let deleted = second.await;
        let record = branch::record(&*direct, test_branch(CHILD)).await.unwrap();
        let mut gone = Vec::new();
        if let Some(record) = &record {
            let state = recovery::recover(&*direct, test_branch(CHILD), record.origin())
                .await
                .unwrap()
                .state;
            for location in state.extents.values() {
                if bucket::get(&*direct, &location.key).await.is_none() {
                    gone.push(location.key.clone());
                }
            }
        }
        (deleted, record, gone)
    }

    /// A bucket whose first request its gate names waits until the gate opens: what [`Gated::new`]
    /// gives with it opens it.
    struct Gated {
        bucket: Arc<Direct>,
        gate: Gate,
        closed: Mutex<Option<futures::channel::oneshot::Receiver<()>>>,
    }

    /// The request a [`Gated`] bucket holds.
    enum Gate {
        /// The put of an object at this key.
        Put(String),
    }

    impl Gated {
        fn new(bucket: Arc<Direct>, gate: Gate) -> (Self, futures::channel::oneshot::Sender<()>) {
            let (open, closed) = futures::channel::oneshot::channel();
            (Self { bucket, gate, closed: Mutex::new(Some(closed)) }, open)
        }

        /// What the request waits for, if `now` and the gate is yet to hold one.
        fn wait(&self, now: bool) -> Option<futures::channel::oneshot::Receiver<()>> {
            now.then(|| self.closed.lock().unwrap().take()).flatten()
        }
    }

    impl Bucket for Gated {
        fn get(&self, key: &str) -> impl Future<Output = Result<Object, BucketError>> + Send {
            self.bucket.get(key)
        }

        fn get_range(
            &self,
            key: &str,
            range: ByteRange,
        ) -> impl Future<Output = Result<Vec<u8>, BucketError>> + Send {
            self.bucket.get_range(key, range)
        }

        fn head(&self, key: &str) -> impl Future<Output = Result<Meta, BucketError>> + Send {
            self.bucket.head(key)
        }

        fn list(
            &self,
            prefix: &str,
        ) -> impl Future<Output = Result<Vec<Meta>, BucketError>> + Send {
            self.bucket.list(prefix)
        }

        fn list_delimited(
            &self,
            prefix: &str,
            page: crate::bucket::Page<'_>,
        ) -> impl Future<Output = Result<crate::bucket::Listing, crate::bucket::Error>> + Send
        {
            self.bucket.list_delimited(prefix, page)
        }

        fn put(
            &self,
            key: &str,
            bytes: bytes::Bytes,
            mode: PutMode,
        ) -> impl Future<Output = Result<Version, BucketError>> + Send {
            let gate = self.wait(matches!(&self.gate, Gate::Put(at) if at == key));
            async move {
                if let Some(gate) = gate {
                    gate.await.unwrap();
                }
                self.bucket.put(key, bytes, mode).await
            }
        }

        fn delete(&self, key: &str) -> impl Future<Output = Result<(), BucketError>> + Send {
            self.bucket.delete(key)
        }

        fn now(&self) -> u64 {
            self.bucket.now()
        }
    }

    /// A bucket that runs `then` once, at the request its hook names, counted from when `then` was
    /// set.
    struct Hooked {
        bucket: Arc<Direct>,
        hook: Hook,
        then: Mutex<Option<Box<dyn FnOnce() + Send>>>,
        /// How many gets of [`Hook::Getting`]'s key, or listings of [`Hook::Listing`]'s prefix, it
        /// answered with `then` set.
        asked: Mutex<usize>,
        /// The key whose put, once sent, is the last request it answers: its client dies then,
        /// every request after it never answered.
        dies_after: Mutex<Option<String>>,
        dead: AtomicBool,
    }

    /// Where [`Hooked`] runs what it runs.
    enum Hook {
        /// As a swap of the head takes effect, before it is answered: a checkpoint's, the hook set
        /// as it is written.
        Swapped,
        /// Before the get of the object at this key of this number, from 1.
        Getting(String, usize),
        /// After the first get of the object at this key, before it is answered.
        Got(String),
        /// After the get of the object at this key of this number, from 1, before it is answered.
        Gotten(String, usize),
        /// Before the listing of this prefix of this number, from 1.
        Listing(String, usize),
        /// Before the put of a key under this prefix of this number, from 1.
        Putting(String, usize),
    }

    impl Hooked {
        fn new(bucket: Arc<Direct>, hook: Hook) -> Self {
            Self {
                bucket,
                hook,
                then: Mutex::default(),
                asked: Mutex::default(),
                dies_after: Mutex::default(),
                dead: AtomicBool::new(false),
            }
        }

        /// `request`, answered unless its client died ([`Hooked::dies_after`]).
        fn answered<T>(
            &self,
            request: impl Future<Output = T> + Send,
        ) -> impl Future<Output = T> + Send {
            let dead = self.dead.load(Ordering::SeqCst);
            async move {
                if dead {
                    futures::future::pending::<()>().await;
                }
                request.await
            }
        }

        /// What it runs, if `now` and it has yet to.
        fn then(&self, now: bool) -> Option<Box<dyn FnOnce() + Send>> {
            now.then(|| self.then.lock().unwrap().take()).flatten()
        }

        /// What it runs, if this request, one of those it counts, is the `nth` with `then` set.
        fn nth(&self, counted: bool, nth: usize) -> Option<Box<dyn FnOnce() + Send>> {
            if !counted || self.then.lock().unwrap().is_none() {
                return None;
            }
            let mut asked = self.asked.lock().unwrap();
            *asked += 1;
            let now = *asked == nth;
            drop(asked);
            self.then(now)
        }
    }

    impl Bucket for Hooked {
        fn get(&self, key: &str) -> impl Future<Output = Result<Object, BucketError>> + Send {
            if let Hook::Getting(at, nth) = &self.hook
                && let Some(then) = self.nth(at == key, *nth)
            {
                then();
            }
            let then = match &self.hook {
                Hook::Got(at) => self.then(at == key),
                Hook::Gotten(at, nth) => self.nth(at == key, *nth),
                _ => None,
            };
            let get = self.answered(self.bucket.get(key));
            async move {
                let got = get.await;
                if let Some(then) = then {
                    then();
                }
                got
            }
        }

        fn get_range(
            &self,
            key: &str,
            range: ByteRange,
        ) -> impl Future<Output = Result<Vec<u8>, BucketError>> + Send {
            self.answered(self.bucket.get_range(key, range))
        }

        fn head(&self, key: &str) -> impl Future<Output = Result<Meta, BucketError>> + Send {
            self.answered(self.bucket.head(key))
        }

        fn list(
            &self,
            prefix: &str,
        ) -> impl Future<Output = Result<Vec<Meta>, BucketError>> + Send {
            if let Hook::Listing(at, nth) = &self.hook
                && let Some(then) = self.nth(at == prefix, *nth)
            {
                then();
            }
            self.answered(self.bucket.list(prefix))
        }

        fn list_delimited(
            &self,
            prefix: &str,
            page: crate::bucket::Page<'_>,
        ) -> impl Future<Output = Result<crate::bucket::Listing, crate::bucket::Error>> + Send
        {
            self.answered(self.bucket.list_delimited(prefix, page))
        }

        fn put(
            &self,
            key: &str,
            bytes: bytes::Bytes,
            mode: PutMode,
        ) -> impl Future<Output = Result<Version, BucketError>> + Send {
            if let Hook::Putting(at, nth) = &self.hook
                && let Some(then) = self.nth(key.starts_with(at.as_str()), *nth)
            {
                then();
            }
            let swap = matches!(layout::kind(key), Kind::Head { .. });
            let then = self.then(swap && matches!(self.hook, Hook::Swapped));
            let put = self.answered(self.bucket.put(key, bytes, mode));
            if self.dies_after.lock().unwrap().as_deref() == Some(key) {
                self.dead.store(true, Ordering::SeqCst);
            }
            async move {
                let put = put.await;
                if let Some(then) = then {
                    then();
                }
                put
            }
        }

        fn delete(&self, key: &str) -> impl Future<Output = Result<(), BucketError>> + Send {
            self.answered(self.bucket.delete(key))
        }

        fn now(&self) -> u64 {
            self.bucket.now()
        }
    }

    /// TASK-407.1: a pass that read an object to compact it, then stalled, creates its moves of it
    /// late, after later passes compacted it, and, its extents freed, deleted it and their own
    /// moves of it: at a key of its own, which no pass wrote before (specs/bucket's
    /// NoMovesByPass), as a key is written once.
    #[test]
    fn passes_creating_moves_late_create_them_at_keys_of_their_own() {
        let (late, written) = block_on(moves_created_late());
        assert!(!written.contains(&late), "{late} written twice: {written:#?}");
        assert!(matches!(layout::kind(&late), Kind::Moves { pass: 1, .. }), "{late}");
    }

    /// Keyed by the object alone, the stalled pass's moves land at the key the later passes wrote
    /// their moves at, and deleted: a key written twice, of other bytes, which a reader's cache may
    /// hold the first's parts of.
    #[test]
    fn passes_creating_moves_late_need_moves_by_pass() {
        crate::set_rules(crate::Rules { moves_by_pass: false, ..crate::Rules::ALL });
        let (late, written) = block_on(moves_created_late());
        crate::set_rules(crate::Rules::ALL);
        assert!(written.contains(&late), "{late}: {written:#?}");
    }

    /// The key of the moves a pass of epoch 1 creates of a segment it read before it stalled, as
    /// the later passes of epochs 2 to 4 compacted it, its extents then freed, and deleted it and
    /// their moves of it; and every moves key those passes wrote.
    async fn moves_created_late() -> (String, BTreeSet<String>) {
        let direct = Arc::new(Direct(SimBucket::new()));
        branch::create(&*direct, test_branch(1), 0).await.unwrap();
        let writer = Arc::new(Writer::open(direct.clone(), test_branch(1), 10, 0).await.unwrap());
        let first = writer.chain().next_sequence();
        let (small, large) = (ExtentId::new(1, first), ExtentId::new(1, first + 1));
        let extents = vec![(small, b"small".to_vec()), (large, vec![7; 20_000])];
        commit(&writer, Commit { tag: 1, extents, ..Commit::default() }).await;
        let at = writer.location(small).unwrap();
        commit(&writer, Commit { tag: 2, frees: vec![large], ..Commit::default() }).await;
        writer.checkpoint().await.unwrap();
        // The stalled pass's epoch, 1, a pass's that compacted nothing.
        collect(&*direct, 1, &KEEP).await.unwrap();
        let segment = at.key.clone();
        let hooked = Hooked::new(direct.clone(), Hook::Got(segment.clone()));
        let written = Arc::new(Mutex::new(BTreeSet::new()));
        let (going, noted, rules) = (direct.clone(), written.clone(), crate::rules());
        *hooked.then.lock().unwrap() = Some(Box::new(move || {
            let went = std::thread::spawn(move || {
                // The test's rules, a thread's.
                crate::set_rules(rules);
                block_on(async {
                    let now = Options { compact_below: 0.5, grace: 0 };
                    let moves = |keys: Vec<String>| -> Vec<String> {
                        keys.into_iter()
                            .filter(|key| matches!(layout::kind(key), Kind::Moves { .. }))
                            .collect()
                    };
                    assert_eq!(collect(&*going, 2, &now).await.unwrap().compacted, 1);
                    noted.lock().unwrap().extend(moves(keys(&*going).await));
                    commit(&writer, Commit { tag: 3, frees: vec![small], ..Commit::default() })
                        .await;
                    writer.checkpoint().await.unwrap();
                    collect(&*going, 3, &now).await.unwrap();
                    collect(&*going, 4, &now).await.unwrap();
                    let left = keys(&*going).await;
                    assert!(
                        !left.contains(&segment) && moves(left.clone()).is_empty(),
                        "{left:#?}"
                    );
                })
            });
            went.join().unwrap();
        }));
        let found = BTreeMap::from([(small, at.clone())]);
        let late = compact_object(&hooked, &at.key, &found, (&Ring::clear(), 1)).await;
        let (late, _) = late.unwrap().unwrap();
        let written = written.lock().unwrap().clone();
        (late, written)
    }

    /// Moves whose extents are all freed stay as long as the object they are of: a reader that
    /// finds it gone looks its extents up in them. They go after it.
    #[test]
    fn moves_outlive_what_they_are_of() {
        block_on(async {
            let bucket = store();
            branch::create(&*bucket, test_branch(1), 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap();
            let first = writer.chain().next_sequence();
            let (small, large) = (ExtentId::new(1, first), ExtentId::new(1, first + 1));
            let extents = vec![(small, b"small".to_vec()), (large, vec![7; 20_000])];
            commit(&writer, Commit { tag: 1, extents, ..Commit::default() }).await;
            let segment = writer.location(small).unwrap().key;
            commit(&writer, Commit { tag: 2, frees: vec![large], ..Commit::default() }).await;
            writer.checkpoint().await.unwrap();
            let now = Options { compact_below: 0.5, grace: 0 };
            assert_eq!(collect(&*bucket, 1, &now).await.unwrap().compacted, 1);

            commit(&writer, Commit { tag: 3, frees: vec![small], ..Commit::default() }).await;
            writer.checkpoint().await.unwrap();
            collect(&*bucket, 2, &now).await.unwrap();
            let left = keys(&*bucket).await;
            assert!(!left.contains(&segment) && moves_in(&left, &segment).is_some());
            collect(&*bucket, 3, &now).await.unwrap();
            assert_eq!(moves_in(&keys(&*bucket).await, &segment), None);
        });
    }

    /// What a pass asked, round by round, as [`SimBucket::served_in_rounds`] serves it: the reads
    /// of branches' records, the reads of checkpoints whole, and the deletes, how many in each
    /// round that asked for any.
    fn asked(rounds: &[Vec<String>]) -> [Vec<usize>; 3] {
        let counted = |asking: &dyn Fn(&str) -> bool| -> Vec<usize> {
            let counts = rounds.iter().map(|round| round.iter().filter(|r| asking(r)).count());
            counts.filter(|&count| count > 0).collect()
        };
        let whole = |asked: &str| asked.split(' ').count() == 2;
        let at = |asked: &str, kind: &str| {
            asked.strip_prefix("get ").is_some_and(|key| key.contains(&format!("/{kind}/")))
        };
        [
            counted(&|asked| at(asked, "branches")),
            counted(&|asked| at(asked, "checkpoints") && whole(asked)),
            counted(&|asked| asked.starts_with("delete ")),
        ]
    }

    /// A base, branch 1: a segment of extents, some stored on their own, which the next frees,
    /// then a segment forking it to `forks` children, and a checkpoint at it, the children's
    /// recoveries' start; the last `deleted` children deleted. Its requests served by `bucket`.
    fn forked_base(bucket: &Arc<SimBucket>, forks: u64, deleted: u64) {
        use crate::layout::{Chain, Contents, Segment};
        use crate::writer::Objects;
        let (writer, _) = bucket.served(async {
            branch::create(&**bucket, test_branch(1), 0).await.unwrap();
            Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap()
        });
        let first = writer.chain().next_sequence();
        let extents = (0..20).map(|n| (ExtentId::new(1, first + n), vec![n as u8; 8])).collect();
        let own: Vec<_> = (20..23).map(|n| (first + n, vec![n as u8; 16])).collect();
        let frees = own.iter().map(|(sequence, _)| ExtentId::new(1, *sequence)).collect();
        let children = (2..2 + forks + deleted).map(|id| child(test_branch(id))).collect();
        let segments: [Box<dyn FnOnce(&mut Chain) -> Objects>; 3] = [
            Box::new(|chain| {
                let commit = Commit { tag: 1, extents, own, ..Commit::default() };
                SegmentBuilder::from(commit).close(chain).frame()
            }),
            Box::new(|chain| {
                SegmentBuilder::from(Commit { tag: 2, frees, ..Commit::default() })
                    .close(chain)
                    .frame()
            }),
            Box::new(|chain| {
                let footer = chain.close(Contents { forks: children, ..Contents::default() });
                let segment = Segment { data: Vec::new(), footer: footer.clone() };
                Objects { footer, segment: segment.encode().into(), own: Vec::new() }
            }),
        ];
        for close in segments {
            let segment = writer.add_segment(close(&mut writer.chain())).unwrap();
            bucket.served(writer.upload(segment)).0.unwrap();
            bucket.served(writer.acknowledge()).0.unwrap();
        }
        bucket.served(writer.checkpoint()).0.unwrap();
        for child in 2 + forks..2 + forks + deleted {
            bucket.served(branch::delete(&**bucket, test_branch(child), 20, 0)).0.unwrap();
        }
    }

    /// TASK-92.7: a pass of the collector over a base and its forks reads the branches' records
    /// many at once, the checkpoint the base's recovery and its forks' start from once, and
    /// deletes what goes many at once. TASK-395.4: a second pass of the same collector reads every
    /// record again, as the first: a collector keeps nothing of a cell from one pass to the next,
    /// so that a sweep over every cell holds no more than the cell it passes over. The first pass
    /// reads one record first, its format, before taking the cell's lease; the passes after find
    /// the lease (TASK-395.6), which a pass of this format took. None skips the cell, each finding
    /// it changed by the one before: deleting, or a branch deleted.
    #[test]
    fn passes_read_records_and_checkpoints_once_and_delete_at_once() {
        let bucket = SimBucket::new();
        forked_base(&bucket, 6, 2);
        let mut collector = Collector::new(KEEP);
        let mut passes = Vec::new();
        for holder in 1..=2 {
            let (report, rounds) = bucket.served_in_rounds(collector.collect(&*bucket, holder));
            report.unwrap();
            passes.push(asked(&rounds));
        }
        // Its record, its children's, the deleted ones' too; its checkpoint; the segments the
        // checkpoint covers that hold no extent its branches hold, and its extents freed. Heads
        // stay, the deleted children's too, and a deletion writes no segment (TASK-407).
        let first = [vec![1, 9], vec![1], vec![5]];
        assert_eq!(passes, [first, [vec![9], vec![1], vec![]]]);

        // The fork deleted: nothing of it goes that its parent's state needs not, and it held
        // nothing of its own.
        bucket.served(branch::delete(&*bucket, test_branch(2), 20, 0)).0.unwrap();
        let (report, rounds) = bucket.served_in_rounds(collector.collect(&*bucket, 3));
        report.unwrap();
        assert_eq!(asked(&rounds), [vec![9], vec![1], vec![]]);
    }

    /// TASK-395.5: how many segments [`mostly_freed`] makes.
    const SEGMENTS: u64 = 5;

    /// Branch 1, [`SEGMENTS`] segments of it each holding a small extent it keeps and a large one
    /// it freed after, and a checkpoint past them: what a pass compacts, a moves each, and the
    /// next, the moves aged, deletes. The segments' keys.
    async fn mostly_freed<B: Bucket>(bucket: Arc<B>) -> Vec<String> {
        branch::create(&*bucket, test_branch(1), 0).await.unwrap();
        let writer = Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap();
        let (mut segments, mut frees) = (Vec::new(), Vec::new());
        for n in 0..SEGMENTS {
            let first = writer.chain().next_sequence();
            let (small, large) = (ExtentId::new(1, first), ExtentId::new(1, first + 1));
            let large_bytes: Vec<u8> = (0..20_000u32).map(|i| (i * 7 % 251) as u8).collect();
            let extents = vec![(small, b"small".to_vec()), (large, large_bytes)];
            commit(&writer, Commit { tag: n + 1, extents, ..Commit::default() }).await;
            segments.push(writer.location(small).unwrap().key);
            frees.push(large);
        }
        commit(&writer, Commit { tag: SEGMENTS + 1, frees, ..Commit::default() }).await;
        writer.checkpoint().await.unwrap();
        segments
    }

    /// TASK-395.5: a pass compacts the objects mostly freed at once, reading them whole in one
    /// round, and the next reads their moves' footers in one round and deletes them in one: no
    /// request waits on another of its kind. Before, each compaction and each moves' footer was a
    /// round of its own.
    #[test]
    fn passes_compact_and_read_moves_at_once() {
        let bucket = SimBucket::new();
        let (segments, _) = bucket.served(mostly_freed(bucket.clone()));
        let mut collector = Collector::new(Options { compact_below: 0.5, grace: 0 });
        // How many of the requests `asking` each round asked, of the rounds asking any.
        let counted = |rounds: &[Vec<String>], asking: &dyn Fn(&str) -> bool| -> Vec<u64> {
            let counts = rounds.iter().map(|round| round.iter().filter(|r| asking(r)).count());
            counts.filter(|&count| count > 0).map(|count| count as u64).collect()
        };
        let (report, rounds) = bucket.served_in_rounds(collector.collect(&*bucket, 1));
        assert_eq!(report.unwrap().compacted, SEGMENTS as usize);
        let whole = |asked: &str| segments.iter().any(|segment| asked == format!("get {segment}"));
        assert_eq!(counted(&rounds, &whole), [SEGMENTS]);

        let (report, rounds) = bucket.served_in_rounds(collector.collect(&*bucket, 2));
        let report = report.unwrap();
        assert_eq!(
            report,
            Report { deleted: SEGMENTS as usize, compacted: 0, retired: 0, ..report }
        );
        // A footer is read by its range, once its trailer, the object's end, says where it is.
        let footers = |asked: &str| {
            let moves = |segment: &String| format!("get {}", layout::moves_of(segment).unwrap());
            let of_moves = segments.iter().any(|segment| asked.starts_with(&moves(segment)));
            of_moves && !asked.contains(" last ")
        };
        assert_eq!(counted(&rounds, &footers), [SEGMENTS]);
        assert_eq!(counted(&rounds, &|asked| asked.starts_with("delete ")), [SEGMENTS]);
        let left = bucket.keys();
        assert_eq!(segments.iter().filter(|segment| left.contains(segment)).count(), 0);
    }

    /// TASK-395.5: over S3, a pass deletes what goes in one DeleteObjects, no DELETE: the S3
    /// server and the store count one batch, of the segments [`mostly_freed`] makes, which the
    /// pass before compacted.
    #[test]
    fn passes_delete_in_batches_over_s3() {
        use crate::object_bucket::Request;
        use crate::s3_server::{BUCKET, S3Server};
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let server = S3Server::start(None);
            let url = format!("s3://{BUCKET}/collector");
            let client = &crate::client::Client::DEFAULT;
            let (objects, prefix) =
                crate::object_bucket::object_store_with(&url, server.options(), client).unwrap();
            let bucket = Arc::new(ObjectBucket::new(objects, &prefix));
            let segments = mostly_freed(bucket.clone()).await;
            let mut collector = Collector::new(Options { compact_below: 0.5, grace: 0 });
            let report = collector.collect(&*bucket, 1).await.unwrap();
            assert_eq!(report.compacted, SEGMENTS as usize);

            let (served, stats) = (server.requests(), bucket.stats());
            let report = collector.collect(&*bucket, 2).await.unwrap();
            assert_eq!(
                report,
                Report { deleted: SEGMENTS as usize, compacted: 0, retired: 0, ..report }
            );
            let (served_after, stats_after) = (server.requests(), bucket.stats());
            let served =
                |kind| served_after.get(kind).unwrap_or(&0) - served.get(kind).unwrap_or(&0);
            let counted = |request| stats_after.requests(request) - stats.requests(request);
            assert_eq!((served("delete-objects"), served("delete")), (1, 0));
            assert_eq!((counted(Request::DeleteBatch), counted(Request::Delete)), (1, 0));
            let left = keys(&*bucket).await;
            assert_eq!(segments.iter().filter(|segment| left.contains(segment)).count(), 0);
        });
    }

    /// TASK-92.7: what forks inherited together, taken once for them all, goes only once none of
    /// them holds it, whichever round of the pass recovered them. A base's three extents stored
    /// on their own, which the base frees after forking more children than a pass recovers at
    /// once: all of them free the first; the first child, which the first round recovers, keeps
    /// the second; and the last, which the last round recovers, the third. The first goes; the
    /// second and the third stay.
    #[test]
    fn what_forks_inherited_goes_once_none_holds_it() {
        use crate::layout::{Contents, Segment};
        use crate::writer::Objects;
        let bucket = store();
        let children = 2..2 + RECOVERIES_AT_ONCE as u64 + 1;
        let (extents, held_in) = block_on(async {
            branch::create(&*bucket, test_branch(1), 0).await.unwrap();
            let base = Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap();
            let sequence = base.chain().next_sequence();
            let extents: Vec<_> = (0..3).map(|n| ExtentId::new(1, sequence + n)).collect();
            let own = extents.iter().map(|extent| (extent.sequence, b"held".to_vec())).collect();
            let held_in = base.chain().next_segment();
            commit(&base, Commit { tag: 1, own, ..Commit::default() }).await;
            let forks = Contents {
                forks: children.clone().map(|id| child(test_branch(id))).collect(),
                ..Contents::default()
            };
            let footer = base.chain().close(forks);
            let framed = Segment { data: Vec::new(), footer: footer.clone() }.encode().into();
            let segment =
                base.add_segment(Objects { footer, segment: framed, own: Vec::new() }).unwrap();
            base.upload(segment).await.unwrap();
            base.acknowledge().await.unwrap();
            assert_eq!(base.checkpoint().await, Ok(Some(segment)));
            commit(&base, Commit { tag: 2, frees: extents.clone(), ..Commit::default() }).await;
            base.checkpoint().await.unwrap();
            for child in children.clone() {
                let writer =
                    Writer::open(bucket.clone(), test_branch(child), 10 + child, 0).await.unwrap();
                let held = match child {
                    _ if child == children.start => Some(extents[1]),
                    _ if child == children.end - 1 => Some(extents[2]),
                    _ => None,
                };
                let frees = extents.iter().copied().filter(|&extent| Some(extent) != held);
                let commit_frees = Commit { tag: 1, frees: frees.collect(), ..Commit::default() };
                commit(&writer, commit_frees).await;
            }
            (extents, held_in)
        });
        let kept = |keys: &[String]| -> Vec<bool> {
            extents
                .iter()
                .map(|&extent| {
                    keys.contains(&layout::extent(layout::TEST_CELL, 1, held_in, extent))
                })
                .collect()
        };
        assert_eq!(kept(&block_on(keys(&*bucket))), [true, true, true]);
        block_on(collect(&*bucket, 1, &KEEP)).unwrap();
        assert_eq!(kept(&block_on(keys(&*bucket))), [false, true, true]);
    }

    /// TASK-79.85: a fork no host holds any more, as one whose host left neither its handle nor
    /// its deletion, is a live branch to the collector, which keeps what it inherited for it. An
    /// operator finds it among the live branches, and deletes it, whatever placement last took
    /// it: the next pass frees what only it held.
    #[test]
    fn branches_no_host_holds_keep_what_they_inherited_until_deleted() {
        use crate::layout::{Contents, Segment};
        use crate::writer::Objects;
        let bucket = store();
        let (extent, held_in) = block_on(async {
            branch::create(&*bucket, test_branch(1), 0).await.unwrap();
            let base = Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap();
            let extent = ExtentId::new(1, base.chain().next_sequence());
            let held_in = base.chain().next_segment();
            let own = vec![(extent.sequence, b"held".to_vec())];
            commit(&base, Commit { tag: 1, own, ..Commit::default() }).await;
            let forks = Contents { forks: vec![child(test_branch(2))], ..Contents::default() };
            let footer = base.chain().close(forks);
            let framed = Segment { data: Vec::new(), footer: footer.clone() }.encode().into();
            let segment =
                base.add_segment(Objects { footer, segment: framed, own: Vec::new() }).unwrap();
            base.upload(segment).await.unwrap();
            base.acknowledge().await.unwrap();
            assert_eq!(base.checkpoint().await, Ok(Some(segment)));
            commit(&base, Commit { tag: 2, frees: vec![extent], ..Commit::default() }).await;
            base.checkpoint().await.unwrap();
            // Its host took it, at a placement later than any since, and went.
            crate::head::take(&*bucket, test_branch(2), 11, 7).await.unwrap();
            (extent, held_in)
        });
        let kept = || {
            block_on(keys(&*bucket)).contains(&layout::extent(
                layout::TEST_CELL,
                1,
                held_in,
                extent,
            ))
        };
        block_on(collect(&*bucket, 1, &KEEP)).unwrap();
        assert!(kept(), "what the fork inherited went while it is live");

        let live = block_on(branch::live(&*bucket, layout::TEST_CELL)).unwrap();
        assert_eq!(live.keys().copied().collect::<Vec<_>>(), [test_branch(1), test_branch(2)]);
        block_on(branch::delete(&*bucket, test_branch(2), 12, u64::MAX)).unwrap();
        block_on(collect(&*bucket, 2, &KEEP)).unwrap();
        assert!(!kept(), "what the fork inherited stayed once it was deleted");
        let live = block_on(branch::live(&*bucket, layout::TEST_CELL)).unwrap();
        assert_eq!(live.keys().copied().collect::<Vec<_>>(), [test_branch(1)]);
    }

    /// TASK-79.82: a fork's child recovers from the origin its record names, not from a parent
    /// whose state names it too, a fork to its ID refused: what it inherited is kept, though the
    /// parent it was forked from frees it. A round of recoveries past the first, with more branches
    /// than one takes, takes the lowest origin first, here the refused fork's.
    #[test]
    fn forks_keep_what_their_records_origin_holds_whoever_else_forked_to_their_id() {
        use crate::layout::{Contents, Origin, Segment};
        use crate::writer::Objects;
        const REFUSED: u64 = 1;
        const PARENT: u64 = 2;
        const CHILD: u64 = 3;
        /// Forks `writer`'s branch to `CHILD`, of `owner`: at the segment it returns.
        async fn fork(writer: &Writer<ObjectBucket>, owner: u64) -> u64 {
            let forked = Child { branch: test_branch(CHILD), owner, point: false };
            let forks = Contents { forks: vec![forked], ..Contents::default() };
            let footer = writer.chain().close(forks);
            let framed = Segment { data: Vec::new(), footer: footer.clone() }.encode().into();
            let segment =
                writer.add_segment(Objects { footer, segment: framed, own: Vec::new() }).unwrap();
            writer.upload(segment).await.unwrap();
            writer.acknowledge().await.unwrap();
            segment
        }
        let bucket = store();
        let (extent, held_in) = block_on(async {
            for branch in [REFUSED, PARENT].into_iter().chain(10..30) {
                branch::create(&*bucket, test_branch(branch), 0).await.unwrap();
            }
            let parent = Writer::open(bucket.clone(), test_branch(PARENT), 10, 0).await.unwrap();
            let extent = ExtentId::new(PARENT, parent.chain().next_sequence());
            let held_in = parent.chain().next_segment();
            let own = vec![(extent.sequence, b"held".to_vec())];
            commit(&parent, Commit { tag: 1, own, ..Commit::default() }).await;
            let at = fork(&parent, 0x32).await;
            parent.checkpoint().await.unwrap();
            commit(&parent, Commit { tag: 2, frees: vec![extent], ..Commit::default() }).await;
            parent.checkpoint().await.unwrap();
            let refused = Writer::open(bucket.clone(), test_branch(REFUSED), 11, 0).await.unwrap();
            fork(&refused, 0x31).await;
            assert!(refused.refused(test_branch(CHILD)));
            refused.checkpoint().await.unwrap();
            let record = branch::record(&*bucket, test_branch(CHILD)).await.unwrap().unwrap();
            assert_eq!(record.origin(), Some(Origin { parent: test_branch(PARENT), at }));
            (extent, held_in)
        });
        block_on(collect(&*bucket, 1, &KEEP)).unwrap();
        let key = layout::extent(layout::TEST_CELL, 1, held_in, extent);
        assert!(block_on(keys(&*bucket)).contains(&key), "what the child inherited went");
    }

    /// TASK-92.7: a benchmark of a pass over a base of 100,000 extents and 100 forks of it, none
    /// with a checkpoint of its own: how long the pass takes, and the most memory the process
    /// held. `cargo nextest run -p extent_bucket --run-ignored only bench_passes_over_many_forks
    /// --no-capture`, in a process of its own, which nextest gives each test.
    #[test]
    #[ignore = "a benchmark (TASK-92.7): --run-ignored only, alone"]
    fn bench_passes_over_many_forks() {
        use crate::layout::{Contents, Segment};
        use crate::writer::Objects;
        const EXTENTS: u64 = 100_000;
        const FORKS: u64 = 100;
        let bucket = Arc::new(Direct(SimBucket::new()));
        block_on(async {
            branch::create(&*bucket, test_branch(1), 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap();
            let first = writer.chain().next_sequence();
            let extents =
                (0..EXTENTS).map(|n| (ExtentId::new(1, first + n), n.to_le_bytes().to_vec()));
            let commit = Commit { tag: 1, extents: extents.collect(), ..Commit::default() };
            let objects = SegmentBuilder::from(commit).close(&mut writer.chain()).frame();
            let segment = writer.add_segment(objects).unwrap();
            writer.upload(segment).await.unwrap();
            writer.acknowledge().await.unwrap();
            let footer = writer.chain().close(Contents {
                forks: (2..2 + FORKS).map(|id| child(test_branch(id))).collect(),
                ..Contents::default()
            });
            let segment = Segment { data: Vec::new(), footer: footer.clone() };
            let objects = Objects { footer, segment: segment.encode().into(), own: Vec::new() };
            let segment = writer.add_segment(objects).unwrap();
            writer.upload(segment).await.unwrap();
            writer.acknowledge().await.unwrap();
            writer.checkpoint().await.unwrap();
        });
        let started = std::time::Instant::now();
        let report = block_on(collect(&*bucket, 1, &KEEP)).unwrap();
        let took = started.elapsed();
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: a getrusage of this process, into `usage`.
        assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) }, 0);
        // In bytes on macOS, in KiB on Linux.
        let most =
            if cfg!(target_os = "macos") { usage.ru_maxrss >> 20 } else { usage.ru_maxrss >> 10 };
        println!(
            "a pass over a base of {EXTENTS} extents and {FORKS} forks: {took:?}, deleting {}; \
             at most {most} MiB held",
            report.deleted
        );
    }

    /// A bucket that marks branch `child`'s record deleted just before the first read of `key`.
    struct DeletedAt {
        bucket: Direct,
        key: String,
        child: Branch,
        done: AtomicBool,
    }

    impl Bucket for DeletedAt {
        fn get(&self, key: &str) -> impl Future<Output = Result<Object, BucketError>> + Send {
            if key == self.key && !self.done.swap(true, Ordering::Relaxed) {
                let record = Record { deleted: true, ..crate::record::Kind::Own.of(0) };
                let (key, bytes) = (layout::record(self.child), layout::encode(&record));
                self.bucket
                    .put(&key, bytes.into(), PutMode::Overwrite)
                    .now_or_never()
                    .unwrap()
                    .unwrap();
            }
            self.bucket.get(key)
        }

        fn get_range(
            &self,
            key: &str,
            range: ByteRange,
        ) -> impl Future<Output = Result<Vec<u8>, BucketError>> + Send {
            self.bucket.get_range(key, range)
        }

        fn head(&self, key: &str) -> impl Future<Output = Result<Meta, BucketError>> + Send {
            self.bucket.head(key)
        }

        fn list(
            &self,
            prefix: &str,
        ) -> impl Future<Output = Result<Vec<Meta>, BucketError>> + Send {
            self.bucket.list(prefix)
        }

        fn list_delimited(
            &self,
            prefix: &str,
            page: crate::bucket::Page<'_>,
        ) -> impl Future<Output = Result<crate::bucket::Listing, BucketError>> + Send {
            self.bucket.list_delimited(prefix, page)
        }

        fn put(
            &self,
            key: &str,
            bytes: bytes::Bytes,
            mode: PutMode,
        ) -> impl Future<Output = Result<Version, BucketError>> + Send {
            self.bucket.put(key, bytes, mode)
        }

        fn delete(&self, key: &str) -> impl Future<Output = Result<(), BucketError>> + Send {
            self.bucket.delete(key)
        }

        fn now(&self) -> u64 {
            self.bucket.now()
        }
    }

    /// A bucket that runs `meanwhile` once, as a pass first reads the head of `branch`: having
    /// listed the records, as it starts to read that branch's state.
    struct Meanwhile<F> {
        bucket: Arc<Direct>,
        branch: Branch,
        meanwhile: std::sync::Mutex<Option<F>>,
    }

    impl<F: FnOnce() + Send + 'static> Bucket for Meanwhile<F> {
        /// Of the branch's head, read by its recovery: `meanwhile` first, the first time.
        fn get(&self, key: &str) -> impl Future<Output = Result<Object, BucketError>> + Send {
            if key == layout::head(self.branch)
                && let Some(meanwhile) = self.meanwhile.lock().unwrap().take()
            {
                meanwhile();
            }
            self.bucket.get(key)
        }

        fn get_range(
            &self,
            key: &str,
            range: ByteRange,
        ) -> impl Future<Output = Result<Vec<u8>, BucketError>> + Send {
            self.bucket.get_range(key, range)
        }

        fn head(&self, key: &str) -> impl Future<Output = Result<Meta, BucketError>> + Send {
            self.bucket.head(key)
        }

        fn list(
            &self,
            prefix: &str,
        ) -> impl Future<Output = Result<Vec<Meta>, BucketError>> + Send {
            self.bucket.list(prefix)
        }

        fn list_delimited(
            &self,
            prefix: &str,
            page: crate::bucket::Page<'_>,
        ) -> impl Future<Output = Result<crate::bucket::Listing, crate::bucket::Error>> + Send
        {
            self.bucket.list_delimited(prefix, page)
        }

        fn put(
            &self,
            key: &str,
            bytes: bytes::Bytes,
            mode: PutMode,
        ) -> impl Future<Output = Result<Version, BucketError>> + Send {
            self.bucket.put(key, bytes, mode)
        }

        fn delete(&self, key: &str) -> impl Future<Output = Result<(), BucketError>> + Send {
            self.bucket.delete(key)
        }

        fn now(&self) -> u64 {
            self.bucket.now()
        }
    }

    /// TASK-158: everything between a pass's listing the records and its reading a branch's state:
    /// a snapshot of it, a restore of that, the point deleted, what the point held freed, and a
    /// checkpoint, which drops the fork deleted. The state the pass reads names none of it, and it
    /// listed neither record: only listing them again finds the restored branch, which keeps what
    /// it inherited. Found by the simulation, seed 5310 of its `restores`.
    #[test]
    fn forks_of_points_made_and_gone_within_a_pass_keep_what_they_hold() {
        assert_eq!(
            read_what_a_fork_of_a_point_made_and_gone_within_a_pass_holds(),
            Ok(b"held".to_vec())
        );
    }

    /// The pass not listing the records again after it read the states: it never finds the
    /// restored branch, and reclaims what it inherited. The simulation's
    /// `collection_needs_records_listed_after_states` searches seeds for the same, which a change
    /// of the trajectories moves (TASK-175's moved it from seed 5310 to 64216 of `restores`).
    /// TASK-395.1: since every clone writes a reference, listed after the states, which finds it as
    /// well, the rule shows needed only with references gone too, as specs/bucket's
    /// NoRecordsRelisted has it.
    #[test]
    fn forks_of_points_made_and_gone_within_a_pass_keeping_what_they_hold_need_records_relisted() {
        let rules =
            crate::Rules { records_relisted: false, references: false, ..crate::Rules::ALL };
        crate::set_rules(rules);
        let read = read_what_a_fork_of_a_point_made_and_gone_within_a_pass_holds();
        crate::set_rules(crate::Rules::ALL);
        assert!(matches!(read, Err(Error::Lost(_))), "{read:?}");
    }

    /// What the restored branch of [`forks_of_points_made_and_gone_within_a_pass_keep_what_they_hold`]
    /// reads of the extent it inherited, once the pass is done.
    fn read_what_a_fork_of_a_point_made_and_gone_within_a_pass_holds() -> Result<Vec<u8>, Error> {
        let sim = SimBucket::new();
        let direct = Arc::new(Direct(sim.clone()));
        const POINT: u64 = 2;
        const RESTORED: u64 = 3;
        let writer = block_on(async {
            branch::create(&*direct, test_branch(1), 0).await.unwrap();
            Arc::new(Writer::open(direct.clone(), test_branch(1), 10, 0).await.unwrap())
        });
        let extent = ExtentId::new(1, writer.chain().next_sequence());
        let extents = vec![(extent, b"held".to_vec())];
        block_on(async {
            let batch = SegmentBuilder::from(Commit { tag: 1, extents, ..Commit::default() });
            let segment = writer.add_segment(batch.close(&mut writer.chain()).frame()).unwrap();
            writer.upload(segment).await.unwrap();
            writer.acknowledge().await.unwrap();
            writer.checkpoint().await.unwrap();
        });
        let meanwhile = {
            let (direct, writer) = (direct.clone(), writer.clone());
            move || {
                let work = async {
                    let batch = SegmentBuilder::from(Commit { tag: 2, ..Commit::default() });
                    let segment = writer
                        .add_segment(
                            batch
                                .fork(
                                    Child { branch: test_branch(POINT), owner: 0, point: true },
                                    &mut writer.chain(),
                                )
                                .frame(),
                        )
                        .unwrap();
                    writer.upload(segment).await.unwrap();
                    writer.acknowledge().await.unwrap();
                    let origin = Origin { parent: test_branch(1), at: segment };
                    branch::fork_point(
                        &*direct,
                        test_branch(RESTORED),
                        test_branch(POINT),
                        origin,
                        0,
                    )
                    .await
                    .unwrap();
                    branch::delete(&*direct, test_branch(POINT), 20, 0).await.unwrap();
                    let frees = vec![extent];
                    let batch = SegmentBuilder::from(Commit { tag: 3, frees, ..Commit::default() });
                    let segment =
                        writer.add_segment(batch.close(&mut writer.chain()).frame()).unwrap();
                    writer.upload(segment).await.unwrap();
                    writer.acknowledge().await.unwrap();
                    writer.checkpoint().await.unwrap();
                };
                work.now_or_never().expect("a direct bucket answers at once");
            }
        };
        let during = Meanwhile {
            bucket: direct.clone(),
            branch: test_branch(1),
            meanwhile: std::sync::Mutex::new(Some(meanwhile)),
        };
        let now = Options { compact_below: 0.0, grace: 0 };
        block_on(collect(&during, 1, &now)).unwrap();
        block_on(async {
            let record = branch::record(&*direct, test_branch(RESTORED)).await.unwrap();
            let recovered =
                recovery::recover(&*direct, test_branch(RESTORED), record.unwrap().origin())
                    .await?;
            crate::read(&direct, extent, &recovered.state.extents[&extent]).await
        })
    }

    /// TASK-411.2: compacting an object encrypted under a key the ring given doesn't hold copies
    /// none of it, its data unread, and fails naming the key, leaving the bucket as it was (a pass
    /// compacts under its cell's keys, TASK-411.6, or leaves it); its twin in the clear is
    /// compacted, its live extent copied to its moves.
    #[test]
    fn objects_encrypted_are_not_compacted_without_their_keys() {
        let id = extent_frame::KeyId { branch: 1, generation: 0 };
        let dek = extent_frame::Secret::from_bytes(&[3; 32]);
        let key = layout::segment(test_branch(1), 0, 1);
        let data: Vec<u8> = (0..3 * 4096).map(|i| (i % 251) as u8).collect();
        let live = ExtentId::new(1, 2);
        let extents =
            BTreeMap::from([(live, Location { key: key.clone(), offset: 4096, len: 4096 })]);
        let footer = layout::encode(&());
        block_on(async {
            let encrypted = Direct(SimBucket::new());
            let object = extent_frame::framed(&data, &footer, Some((&dek, id)));
            assert!(matches!(bucket::create(&encrypted, &key, object).await, Created::Created));
            let before = held(&encrypted).await;
            let compacted = compact_object(&encrypted, &key, &extents, (&Ring::clear(), 1)).await;
            assert_eq!(compacted, Err(Error::NoKey(id)));
            assert_eq!(held(&encrypted).await, before);

            let clear = Direct(SimBucket::new());
            let object = extent_frame::framed(&data, &footer, None);
            assert!(matches!(bucket::create(&clear, &key, object).await, Created::Created));
            let compacted = compact_object(&clear, &key, &extents, (&Ring::clear(), 1)).await;
            let moves = layout::moves(&key, 1).unwrap();
            let bytes = bucket::get(&clear, &moves).await.unwrap().bytes;
            assert_eq!(compacted, Ok(Some((moves, bytes.len() as u64))));
            assert_eq!(layout::data(&bytes).unwrap(), &data[4096..8192]);
        });
    }

    /// A live branch whose recovery finds what it needs gone, here its parent's segment, which no
    /// head names, fails the pass, which deletes nothing; one deleted meanwhile is live no longer,
    /// and the pass goes on.
    #[test]
    fn branches_lost_fail_the_pass_unless_deleted() {
        // Branch 2, forked from 1, which has no record; and a segment of no branch.
        let origin = Origin { parent: test_branch(1), at: 0 };
        let stray = layout::segment(test_branch(3), 0, 1);
        let sim = SimBucket::new();
        let direct = Direct(sim.clone());
        block_on(async {
            branch::record_fork(&direct, child(test_branch(2)), origin).await.unwrap();
            let bytes = layout::with_footer(b"", &());
            assert!(matches!(bucket::create(&direct, &stray, bytes).await, Created::Created));
        });
        let before = block_on(keys(&direct));
        let lost = Error::Lost("segment 0 of 1/1/1".to_owned());
        assert_eq!(block_on(collect(&direct, 1, &KEEP)), Err(lost));
        assert_eq!(block_on(keys(&direct)), before);

        let deleting = DeletedAt {
            bucket: Direct(sim),
            key: layout::head(test_branch(1)),
            child: test_branch(2),
            done: AtomicBool::new(false),
        };
        let report = block_on(collect(&deleting, 2, &KEEP)).unwrap();
        assert_eq!(report, Report { deleted: 1, compacted: 0, retired: 0, ..report });
        assert!(!block_on(keys(&deleting)).contains(&stray));
    }

    /// TASK-264: what the bucket holds, every object, the collector's lease included: its key, its
    /// version, size and time, and its bytes.
    async fn held(bucket: &impl Bucket) -> Vec<(Meta, Vec<u8>)> {
        let mut held = Vec::new();
        for meta in bucket::list(bucket, "").await {
            let bytes = bucket::get(bucket, &meta.key).await.unwrap().bytes;
            held.push((meta, bytes));
        }
        held
    }

    /// TASK-264: branch 1, of a pass already, so that the collector's lease is there, then of
    /// garbage: its fence, a segment of a small extent and a large, a segment freeing the large,
    /// all covered by a checkpoint, and an extent stored on its own, freed. The segment of the
    /// small extent is mostly freed: a pass compacting below half compacts it.
    async fn garbage(bucket: &Arc<ObjectBucket>) {
        branch::create(&**bucket, test_branch(1), 0).await.unwrap();
        let writer = Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap();
        collect(&**bucket, 1, &KEEP).await.unwrap();
        let first = writer.chain().next_sequence();
        let (small, large, own) =
            (ExtentId::new(1, first), ExtentId::new(1, first + 1), ExtentId::new(1, first + 2));
        let large_bytes: Vec<u8> = (0..20_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let extents = vec![(small, b"small".to_vec()), (large, large_bytes)];
        let owned = vec![(own.sequence, b"own".to_vec())];
        commit(&writer, Commit { tag: 1, extents, own: owned, ..Commit::default() }).await;
        let frees = vec![large, own];
        commit(&writer, Commit { tag: 2, frees, ..Commit::default() }).await;
        writer.checkpoint().await.unwrap();
    }

    /// The keys `garbage` makes that a pass deletes: the segment freeing, and the extent stored on
    /// its own; not the segment of the small extent, which is live.
    fn garbage_doomed() -> Vec<String> {
        let own = layout::extent(
            layout::TEST_CELL,
            1,
            0,
            ExtentId::new(1, (1 << crate::EPOCH_SHIFT) + 2),
        );
        vec![own, layout::segment(test_branch(1), 1, 1)]
    }

    /// TASK-264: a dry run writes nothing, not even the collector's lease, and compacts nothing; it
    /// finds doomed what a pass compacting nothing deletes, and the object such a pass compacting
    /// below half compacts.
    #[test]
    fn dry_runs_write_nothing_and_find_what_passes_delete() {
        block_on(async {
            let bucket = store();
            garbage(&bucket).await;
            let before = held(&*bucket).await;
            let options = Options { compact_below: 0.5, grace: 0 };
            let dry =
                Collector::new(options).dry_run_cell(&*bucket, layout::TEST_CELL).await.unwrap();
            assert_eq!(held(&*bucket).await, before);
            let doomed: Vec<String> = dry.doomed.iter().map(|meta| meta.key.clone()).collect();
            assert_eq!(doomed, garbage_doomed());
            let compactable: Vec<String> =
                dry.compactable.into_iter().map(|meta| meta.key).collect();
            assert_eq!(compactable, [layout::segment(test_branch(1), 0, 1)]);
            assert!(!dry.fenced);

            let keys_before = keys(&*bucket).await;
            let now = Options { compact_below: 0.0, grace: 0 };
            let report = collect(&*bucket, 2, &now).await.unwrap();
            assert_eq!(report, Report { deleted: 2, compacted: 0, retired: 0, ..report });
            let after = keys(&*bucket).await;
            let deleted: Vec<String> =
                keys_before.into_iter().filter(|key| !after.contains(key)).collect();
            assert_eq!(deleted, doomed);
        });
    }

    /// TASK-264: a dry run a pass ran during, which took the collector's lease and deleted, says it
    /// was fenced.
    #[test]
    fn dry_runs_a_pass_ran_during_say_so() {
        block_on(async {
            let direct = Arc::new(Direct(SimBucket::new()));
            branch::create(&*direct, test_branch(1), 0).await.unwrap();
            // As the dry run reads the lease again, at its end.
            let during = Hooked::new(
                direct.clone(),
                Hook::Getting(layout::collector_lease(layout::TEST_CELL), 2),
            );
            let passing = direct.clone();
            *during.then.lock().unwrap() = Some(Box::new(move || {
                let pass = collect(&*passing, 9, &KEEP).now_or_never();
                assert_eq!(pass, Some(Ok(Report::default())));
            }));
            let dry = Collector::new(KEEP).dry_run_cell(&during, layout::TEST_CELL).await.unwrap();
            assert_eq!(dry, DryRun { fenced: true, ..DryRun::default() });
        });
    }

    /// TASK-264: a mark-only pass compacts as a pass does, deletes nothing, and marks exactly what
    /// a pass deletes; the next marks the object compacted too, its moves aged, and nothing found
    /// kept that was marked. Marks are never collected, nor marked: a pass deleting leaves them.
    #[test]
    fn mark_only_passes_compact_and_mark_what_passes_delete() {
        let compacting = Options { compact_below: 0.5, grace: 0 };
        let deleted = block_on(async {
            let bucket = store();
            garbage(&bucket).await;
            let before = keys(&*bucket).await;
            let report = collect(&*bucket, 2, &compacting).await.unwrap();
            assert_eq!(report, Report { deleted: 2, compacted: 1, retired: 0, ..report });
            let after = keys(&*bucket).await;
            before.into_iter().filter(|key| !after.contains(key)).collect::<Vec<_>>()
        });
        assert_eq!(deleted, garbage_doomed());
        block_on(async {
            let bucket = store();
            garbage(&bucket).await;
            let before = held(&*bucket).await;
            let mut collector = Collector::new(compacting);
            let marking = collector.mark_cell(&*bucket, layout::TEST_CELL, 2).await.unwrap();
            assert_eq!(
                marking.report,
                Report { deleted: 0, compacted: 1, retired: 0, ..marking.report }
            );
            assert_eq!(marking.resurrected, Vec::<String>::new());
            let marks = marking.marks.unwrap();
            let listed: BTreeMap<String, Meta> =
                before.iter().map(|(meta, _)| (meta.key.clone(), meta.clone())).collect();
            let expected: Vec<Marked> =
                deleted.iter().map(|key| Marked::of(&listed[key])).collect();
            assert_eq!(marks, Marks { epoch: 2, grace: 0, marked: expected });
            let stored =
                bucket::get(&*bucket, &layout::marks(layout::TEST_CELL, 2)).await.unwrap().bytes;
            assert_eq!(layout::decode::<Marks>(&stored), Ok(marks));
            // Nothing deleted: what was there is, but the collector's lease, taken again.
            let after = held(&*bucket).await;
            let kept = |held: &[(Meta, Vec<u8>)]| -> Vec<(Meta, Vec<u8>)> {
                held.iter()
                    .filter(|(meta, _)| meta.key != layout::collector_lease(layout::TEST_CELL))
                    .cloned()
                    .collect()
            };
            let mut more: Vec<String> = after.iter().map(|(meta, _)| meta.key.clone()).collect();
            more.retain(|key| !listed.contains_key(key));
            let moved = layout::moves(&layout::segment(test_branch(1), 0, 1), 2).unwrap();
            // And the word that the bucket is marked, which readers go by (TASK-395.4's review).
            let marking = layout::MARKING.to_owned();
            assert_eq!(more, [marking, layout::marks(layout::TEST_CELL, 2), moved]);
            let left: Vec<_> = kept(&after)
                .into_iter()
                .filter(|(meta, _)| listed.contains_key(&meta.key))
                .collect();
            assert_eq!(left, kept(&before));

            let next = collector.mark_cell(&*bucket, layout::TEST_CELL, 3).await.unwrap();
            assert_eq!(next.report, Report::default());
            assert_eq!(next.resurrected, Vec::<String>::new());
            let marked: Vec<String> =
                next.marks.unwrap().marked.into_iter().map(|marked| marked.key).collect();
            let mut expected = garbage_doomed();
            expected.push(layout::segment(test_branch(1), 0, 1));
            expected.sort();
            assert_eq!(marked, expected);

            let report = collect(&*bucket, 4, &compacting).await.unwrap();
            assert_eq!(report, Report { deleted: 3, compacted: 0, retired: 0, ..report });
            let marks: Vec<String> = (keys(&*bucket).await.into_iter())
                .filter(|key| matches!(layout::kind(key), Kind::Marks { .. }))
                .collect();
            assert_eq!(
                marks,
                [layout::marks(layout::TEST_CELL, 2), layout::marks(layout::TEST_CELL, 3)]
            );
        });
    }

    /// TASK-264: a pass wrong, deleting what no head names yet (rule `collector_epochs`, TASK-407),
    /// marks an extent uploaded ahead of the segment that names it. The segment
    /// landing, a reader starting after the pass, which reads the marks first, warns as it reads
    /// the extent, each read of it; and the next mark-only pass, right, warns that it finds needed
    /// what was marked, the same object still.
    #[test]
    fn readers_and_mark_only_passes_warn_of_what_a_wrong_pass_marked() {
        let log = crate::marks::captured();
        let memory: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let bucket = Arc::new(ObjectBucket::new(memory.clone(), ""));
        let (late, key) = block_on(async {
            branch::create(&*bucket, test_branch(1), 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), test_branch(1), 10, 0).await.unwrap();
            let first = writer.chain().next_sequence();
            let (late, early) = (ExtentId::new(1, first), ExtentId::new(1, first + 1));
            let extents = vec![(early, b"early".to_vec())];
            commit(&writer, Commit { tag: 1, extents, ..Commit::default() }).await;
            // The writer's next segment, which names `late`, its object uploaded first.
            let own = vec![(late.sequence, b"late".to_vec())];
            let objects = SegmentBuilder::from(Commit { tag: 2, own, ..Commit::default() })
                .close(&mut writer.chain())
                .frame();
            let [(key, bytes)] = &objects.own[..] else { panic!("{}", objects.own.len()) };
            let created = bucket::create(&*bucket, key, bytes.read().unwrap()).await;
            assert!(matches!(created, Created::Created));

            // A wrong pass, deleting what no head names yet (rule `collector_epochs`).
            crate::set_rules(crate::Rules { collector_epochs: false, ..crate::Rules::ALL });
            let marking = Collector::new(KEEP).mark_cell(&*bucket, layout::TEST_CELL, 1).await;
            crate::set_rules(crate::Rules::ALL);
            let marked: Vec<String> =
                marking.unwrap().marks.unwrap().marked.into_iter().map(|m| m.key).collect();
            assert_eq!(marked, [key.as_str()]);
            let key = key.clone();
            let segment = writer.add_segment(objects).unwrap();
            writer.upload(segment).await.unwrap();
            writer.acknowledge().await.unwrap();
            (late, key)
        });
        let warnings = || -> Vec<String> {
            let lines = log.lock().unwrap();
            lines.iter().filter(|line| line.starts_with("WARN")).cloned().collect()
        };
        assert_eq!(warnings(), Vec::<String>::new());

        let reader = Arc::new(ObjectBucket::new(memory.clone(), ""));
        let read = block_on(async {
            let recovered = recovery::recover(&*reader, test_branch(1), None).await.unwrap();
            crate::read(&reader, late, &recovered.state.extents[&late]).await
        });
        assert_eq!(read, Ok(b"late".to_vec()));
        let kind = layout::kind(&key);
        let saying = "a reader reached what the collector's newest mark-only pass found no live \
                      branch needs, and would have deleted";
        let marks = layout::marks(layout::TEST_CELL, 1);
        let reached = |request: &str| {
            format!(
                "WARN extent_bucket: {request} {key}: {saying} ({marks}) key={key} kind={kind:?} \
                 request={request} marks=1"
            )
        };
        // Its tail, then its data.
        let read_twice = [reached("get-range"), reached("get-range")];
        assert_eq!(warnings(), read_twice);

        let next =
            block_on(Collector::new(KEEP).mark_cell(&*bucket, layout::TEST_CELL, 2)).unwrap();
        assert_eq!(next.resurrected, [key.as_str()]);
        assert_eq!(next.marks.unwrap().marked, Vec::<Marked>::new());
        let resurrected = format!(
            "WARN cladefs-gc: RESURRECTED {key}: the mark-only pass 1 found no live branch needs it, \
             and the pass 2 finds one does: a bug in the collector, which deleting would have lost \
             it to key={key} kind={kind:?} marks=1 epoch=2"
        );
        let [tail, data] = read_twice;
        assert_eq!(warnings(), [tail, data, resurrected]);
    }

    // cladefs: the collector per cell, and fork references (TASK-395.2, .3, .4).

    /// Branch `id` of tenant 1's cell `cell`: each of these tests' branches a cell of its own, as
    /// hosts make them.
    /// TASK-407.1: the key of the latest pass's moves of the object at `key` among `keys`, if any.
    fn moves_in(keys: &[String], key: &str) -> Option<String> {
        let prefix = layout::moves_of(key).unwrap();
        keys.iter().filter(|listed| listed.starts_with(&prefix)).next_back().cloned()
    }

    /// TASK-407.1: the key of the latest pass's moves of the object at `key` in `bucket`, if any.
    async fn moves_at(bucket: &impl Bucket, key: &str) -> Option<String> {
        bucket::list(bucket, &layout::moves_of(key).unwrap()).await.pop().map(|meta| meta.key)
    }

    fn celled(cell: u64, id: u64) -> Branch {
        Branch { cell: layout::Cell { tenant: 1, id: cell }, id }
    }

    /// The child of a writable fork to `branch`, its owner its ID.
    fn forked(branch: Branch) -> Child {
        Child { branch, owner: 0x100 | branch.id, point: false }
    }

    /// Has `writer` commit `extents` in a segment, then fork to `child` in the next, uploaded and
    /// acknowledged, which records the child: the fork's segment.
    async fn commit_and_fork<B: Bucket>(
        writer: &Writer<B>,
        extents: Vec<(ExtentId, Vec<u8>)>,
        child: Child,
    ) -> u64 {
        commit(writer, Commit { tag: 1, extents, ..Commit::default() }).await;
        let forking = SegmentBuilder::from(Commit { tag: 2, ..Commit::default() });
        let at = writer.add_segment(forking.fork(child, &mut writer.chain()).frame()).unwrap();
        writer.upload(at).await.unwrap();
        writer.acknowledge().await.unwrap();
        at
    }

    /// The position in `asked`, requests as [`SimBucket::served`] gives them, of the first that is
    /// `request`.
    fn position(asked: &[String], request: &str) -> usize {
        asked
            .iter()
            .position(|asked| asked == request)
            .unwrap_or_else(|| panic!("{request}: {asked:#?}"))
    }

    /// TASK-395.3: a writable fork's child of another cell gets its reference, under its parent's
    /// cell, before its record, the writer writing both; of bytes unique to the fork, its owner's.
    /// A deletion recording the fork again, finding the child's record, its own, writes none: the
    /// writer's came first (TASK-395.14). A child of the parent's own cell gets none: that cell's
    /// pass reads its record.
    #[test]
    fn writable_forks_refer_before_their_childrens_records() {
        let bucket = SimBucket::new();
        let (parent, other, own) = (celled(1, 1), celled(2, 2), celled(1, 3));
        let (writer, _) = bucket.served(async {
            branch::create(&*bucket, parent, 0).await.unwrap();
            Writer::open(bucket.clone(), parent, 10, 0).await.unwrap()
        });
        let extent = ExtentId::new(1, writer.chain().next_sequence());
        let (at, asked) = bucket.served(commit_and_fork(
            &writer,
            vec![(extent, b"held".to_vec())],
            forked(other),
        ));
        let reference = layout::reference(parent, at, other);
        let referred = position(&asked, &format!("put {reference}"));
        assert!(
            referred < position(&asked, &format!("put {}", layout::record(other))),
            "{asked:#?}"
        );
        let bytes = bucket.served(bucket::get(&*bucket, &reference)).0.unwrap().bytes;
        assert_eq!(bytes, layout::encode(&layout::Reference::of(forked(other).owner)));

        let (at, asked) = bucket.served(commit_and_fork(&writer, Vec::new(), forked(own)));
        let refers = |asked: &[String]| -> Vec<String> {
            let put = |asked: &&String| asked.starts_with("put ") && asked.contains("/refs/");
            asked.iter().filter(put).cloned().collect()
        };
        assert_eq!(refers(&asked), Vec::<String>::new(), "within the cell, at {at}");

        // The parent's deletion records the forks of its state again: recorded, their references
        // stand.
        let (deleted, asked) = bucket.served(branch::delete(&*bucket, parent, 20, 0));
        deleted.unwrap();
        assert_eq!(refers(&asked), Vec::<String>::new());
        let references: Vec<String> =
            bucket.keys().into_iter().filter(|key| key.contains("/refs/")).collect();
        assert_eq!(references, [reference]);
    }

    /// TASK-395.3, TASK-395.10: a clone of a point writes its reference, under the cell of the
    /// point's origin, after its record and before it reads the point's; whatever its cell, the
    /// origin's own too.
    #[test]
    fn clones_refer_between_their_records_and_their_points_read() {
        assert_eq!(clones_requests(), ["record", "reference", "read"]);
    }

    /// Without the rule, the reference comes first, as before TASK-395.10.
    #[test]
    fn clones_refer_before_their_records_without_clones_record_first() {
        crate::set_rules(crate::Rules { clones_record_first: false, ..crate::Rules::ALL });
        let asked = clones_requests();
        crate::set_rules(crate::Rules::ALL);
        assert_eq!(asked, ["reference", "record", "read"]);
    }

    /// The order of a clone's first requests: its record's create, its reference's, the read of
    /// its point's record; the same for a clone in another cell than its origin's and in that one.
    fn clones_requests() -> Vec<&'static str> {
        use crate::layout::Origin;
        let bucket = SimBucket::new();
        let (origin, point) = (Origin { parent: celled(1, 1), at: 3 }, celled(2, 2));
        // The point's record, of an origin no bucket here has: what the clones read of it.
        let point_record = layout::encode(&crate::record::Kind::Point(origin).of(0x22));
        bucket.served(bucket::create(&*bucket, &layout::record(point), point_record));
        let mut orders = Vec::new();
        for clone in [celled(3, 3), celled(1, 4)] {
            let (forked, asked) =
                bucket.served(branch::fork_point(&*bucket, clone, point, origin, 0x33));
            forked.unwrap();
            let reference = layout::reference(origin.parent, origin.at, clone);
            let mut steps = [
                (position(&asked, &format!("put {reference}")), "reference"),
                (position(&asked, &format!("put {}", layout::record(clone))), "record"),
                (position(&asked, &format!("get {}", layout::record(point))), "read"),
            ];
            steps.sort();
            orders.push(steps.map(|(_, step)| step).to_vec());
            let bytes = bucket.served(bucket::get(&*bucket, &reference)).0.unwrap().bytes;
            assert_eq!(bytes, layout::encode(&layout::Reference::of(0x33)));
        }
        assert_eq!(orders[0], orders[1]);
        orders.swap_remove(0)
    }

    /// What reading `extent` gives `branch`, recovered from its record's origin.
    async fn read_inherited<B: Bucket>(
        bucket: &Arc<B>,
        branch: Branch,
        extent: ExtentId,
    ) -> Result<Vec<u8>, Error> {
        let record = branch::record(&**bucket, branch).await?.ok_or(Error::NoBranch)?;
        let recovered = recovery::recover(&**bucket, branch, record.origin()).await?;
        let location = recovered.state.extents.get(&extent).cloned();
        let location = location.ok_or_else(|| Error::Lost(format!("{extent:?} of {branch}")))?;
        crate::read(bucket, extent, &location).await
    }

    /// Sweeps over every cell, deleting what is free at once, until two in a row delete nothing:
    /// failing as a pass finds what a live branch needs lost.
    async fn sweep(bucket: &impl Bucket) -> Result<(), Error> {
        let now = Options { compact_below: 0.0, grace: 0 };
        crate::testing::collect_until_quiet(bucket, &now).await.map(drop)
    }

    /// TASK-395.4: a fork to another cell keeps its parent's state at the fork point in the
    /// parent's cell, by its reference, the parent deleted and having freed what the child
    /// inherited, its checkpoint covering the segments the child, with no checkpoint of its own,
    /// recovers through.
    #[test]
    fn fork_points_keep_what_children_in_other_cells_inherit() {
        assert_eq!(block_on(read_forked_from_a_deleted_parent()), Ok(b"held".to_vec()));
    }

    /// Without references, the parent's cell's pass reclaims what the child inherited: it reads its
    /// parent deleted, and lists nothing of the child's cell.
    #[test]
    fn fork_points_kept_for_children_in_other_cells_need_references() {
        crate::set_rules(crate::Rules { references: false, ..crate::Rules::ALL });
        let read = block_on(read_forked_from_a_deleted_parent());
        crate::set_rules(crate::Rules::ALL);
        assert!(matches!(read, Err(Error::Lost(_))), "{read:?}");
    }

    /// Keeping the extents of a fork point's state alone, not what recovering it reads: the child,
    /// with no checkpoint of its own, recovers through the parent's segments, which the parent's
    /// checkpoint covers, gone.
    #[test]
    fn fork_points_kept_for_children_in_other_cells_need_what_recovering_them_reads() {
        crate::set_rules(crate::Rules { fork_points_kept: false, ..crate::Rules::ALL });
        let read = block_on(read_forked_from_a_deleted_parent());
        crate::set_rules(crate::Rules::ALL);
        assert!(matches!(read, Err(Error::Lost(_))), "{read:?}");
    }

    /// What a fork to another cell reads of what it inherited, its parent having freed it,
    /// checkpointed and been deleted, sweeps having run until they did nothing more: read by a
    /// reader of another host's, whose bucket kept none of what it read decoded (TASK-407).
    async fn read_forked_from_a_deleted_parent() -> Result<Vec<u8>, Error> {
        let objects: Arc<InMemory> = Arc::new(InMemory::new());
        let bucket = Arc::new(ObjectBucket::new(objects.clone(), ""));
        let (parent, child) = (celled(1, 1), celled(2, 2));
        branch::create(&*bucket, parent, 0).await.unwrap();
        let writer = Writer::open(bucket.clone(), parent, 10, 0).await.unwrap();
        let extent = ExtentId::new(1, writer.chain().next_sequence());
        commit_and_fork(&writer, vec![(extent, b"held".to_vec())], forked(child)).await;
        commit(&writer, Commit { tag: 3, frees: vec![extent], ..Commit::default() }).await;
        writer.checkpoint().await.unwrap();
        drop(writer);
        branch::delete(&*bucket, parent, 20, 0).await.unwrap();
        sweep(&*bucket).await?;
        read_inherited(&Arc::new(ObjectBucket::new(objects, "")), child, extent).await
    }

    /// TASK-395.3, TASK-395.4: a chain of forks across cells, a root's, its child's, its
    /// grandchild's: the child deleted, its cell keeps its own state at the grandchild's fork
    /// point, and its reference under the root's cell stays, the child's cell holding the
    /// grandchild's; the root frees and checkpoints, and the grandchild, with no checkpoint, reads
    /// what it inherited through both. The grandchild deleted too, the passes retire the
    /// references down the chain, and the cells of the dead are reclaimed whole but their records,
    /// heads
    /// and the marks of the references retired (TASK-395.13): no reference is left, and the root's
    /// cell holds its own alone.
    #[test]
    fn references_hold_while_their_childrens_cells_hold_references() {
        let (read, bucket) = block_on(read_through_a_deleted_child());
        assert_eq!(read, Ok(b"held".to_vec()));
        block_on(async {
            let grandchild = celled(3, 3);
            branch::delete(&*bucket, grandchild, 22, 0).await.unwrap();
            sweep(&*bucket).await.unwrap();
        });
        let left = block_on(keys(&*bucket));
        let references: Vec<&String> = left.iter().filter(|key| key.contains("/refs/")).collect();
        assert_eq!(references, Vec::<&String>::new());
        for cell in [2, 3] {
            let prefix = layout::Cell { tenant: 1, id: cell }.prefix();
            let kept: Vec<&String> = (left.iter())
                .filter(|key| key.starts_with(&prefix))
                .filter(|key| {
                    !matches!(
                        layout::kind(key),
                        Kind::Record { .. }
                            | Kind::Head { .. }
                            | Kind::Marks { .. }
                            | Kind::Retired { .. }
                    )
                })
                .collect();
            assert_eq!(kept, Vec::<&String>::new(), "cell {cell}, dead, reclaimed whole");
        }
    }

    /// The child's reference retired once the child is deleted, though its cell holds the
    /// grandchild's: the root's cell's pass reclaims what the grandchild inherited through it.
    #[test]
    fn references_held_by_their_childrens_cells_need_references_held() {
        crate::set_rules(crate::Rules { references_held: false, ..crate::Rules::ALL });
        let (read, _) = block_on(read_through_a_deleted_child());
        crate::set_rules(crate::Rules::ALL);
        assert!(matches!(read, Err(Error::Lost(_))), "{read:?}");
    }

    /// What a grandchild, forked to cell 3 from a child forked to cell 2 from a root in cell 1,
    /// reads of what it inherited from the root: the child deleted, the root having freed it and
    /// checkpointed, sweeps having run until they did nothing more. With the bucket.
    async fn read_through_a_deleted_child() -> (Result<Vec<u8>, Error>, Arc<ObjectBucket>) {
        let bucket = store();
        let (root, child, grandchild) = (celled(1, 1), celled(2, 2), celled(3, 3));
        branch::create(&*bucket, root, 0).await.unwrap();
        let writer = Writer::open(bucket.clone(), root, 10, 0).await.unwrap();
        let extent = ExtentId::new(1, writer.chain().next_sequence());
        commit_and_fork(&writer, vec![(extent, b"held".to_vec())], forked(child)).await;
        let forking = Writer::open(bucket.clone(), child, 11, 0).await.unwrap();
        commit_and_fork(&forking, Vec::new(), forked(grandchild)).await;
        drop(forking);
        branch::delete(&*bucket, child, 21, 0).await.unwrap();
        sweep(&*bucket).await.unwrap();
        commit(&writer, Commit { tag: 3, frees: vec![extent], ..Commit::default() }).await;
        writer.checkpoint().await.unwrap();
        let read = match sweep(&*bucket).await {
            Ok(()) => read_inherited(&bucket, grandchild, extent).await,
            Err(error) => Err(error),
        };
        (read, bucket)
    }

    /// TASK-395.3: a deletion that records the child of a fork across cells, its writer having
    /// recorded none, dying after its head named the fork's segment, writes its reference before
    /// it marks the parent deleted: a pass over the parent's cell between the two keeps the fork
    /// point, by the parent's state; after, by the reference.
    #[test]
    fn deletions_refer_before_they_mark_parents_deleted() {
        assert_eq!(block_on(read_as_a_deletion_refers()), Ok(b"held".to_vec()));
    }

    /// The references after the parent marked deleted, as specs/bucket's NoReferencesFirst has it:
    /// a pass over the parent's cell between the two reads it deleted, and finds no reference.
    #[test]
    fn deletions_referring_before_they_mark_parents_deleted_need_references_first() {
        crate::set_rules(crate::Rules { references_first: false, ..crate::Rules::ALL });
        let read = block_on(read_as_a_deletion_refers());
        crate::set_rules(crate::Rules::ALL);
        assert!(matches!(read, Err(Error::Lost(_))), "{read:?}");
    }

    /// What the child reads of what it inherited, its parent's deletion recording it, sweeps
    /// running as the deletion writes the reference.
    async fn read_as_a_deletion_refers() -> Result<Vec<u8>, Error> {
        let direct = Arc::new(Direct(SimBucket::new()));
        let (parent, child) = (celled(1, 1), celled(2, 2));
        branch::create(&*direct, parent, 0).await.unwrap();
        // Its writer's write of the fork's reference waits, for ever: it dies there.
        let at = 1;
        let (dying, _never) =
            Gated::new(direct.clone(), Gate::Put(layout::reference(parent, at, child)));
        let writer = Writer::open(Arc::new(dying), parent, 10, 0).await.unwrap();
        let extent = ExtentId::new(1, writer.chain().next_sequence());
        commit(
            &writer,
            Commit { tag: 1, extents: vec![(extent, b"held".to_vec())], ..Commit::default() },
        )
        .await;
        // The fork's segment, acknowledged, its head naming it; its writer records nothing.
        let forking = SegmentBuilder::from(Commit { tag: 2, ..Commit::default() });
        let forked_at =
            writer.add_segment(forking.fork(forked(child), &mut writer.chain()).frame()).unwrap();
        assert_eq!(forked_at, at);
        writer.upload(at).await.unwrap();
        let mut acknowledging = Box::pin(writer.acknowledge());
        assert!((&mut acknowledging).now_or_never().is_none(), "its writer dies recording");
        drop(acknowledging);
        drop(writer);
        let bucket = Hooked::new(direct.clone(), Hook::Putting(layout::references(parent.cell), 1));
        let sweeping = direct.clone();
        *bucket.then.lock().unwrap() = Some(Box::new(move || {
            let now = Options { compact_below: 0.0, grace: 0 };
            let pass = collect_cell(&*sweeping, parent.cell, 1, &now);
            pass.now_or_never().unwrap().unwrap();
        }));
        branch::delete(&bucket, parent, 20, 0).await.unwrap();
        assert!(bucket.then.lock().unwrap().is_none(), "the pass ran");
        sweep(&*direct).await?;
        read_inherited(&direct, child, extent).await
    }

    /// TASK-395.3: a clone writes its reference before it reads its point: a clone that read the
    /// point live, the point and its origin deleted and swept away as it finishes, the point's
    /// reference retired, keeps what it inherited, by its reference.
    #[test]
    fn clones_keep_what_they_inherit_as_their_points_go() {
        assert_eq!(block_on(read_as_a_point_goes_under_a_clone()), Ok(b"held".to_vec()));
    }

    /// The clone's reference written once it is made, as specs/bucket's NoForksReferFirst has
    /// it: the sweeps between find neither the point nor a reference.
    #[test]
    fn clones_keeping_what_they_inherit_as_their_points_go_need_clones_refer_first() {
        crate::set_rules(crate::Rules { clones_refer_first: false, ..crate::Rules::ALL });
        let read = block_on(read_as_a_point_goes_under_a_clone());
        crate::set_rules(crate::Rules::ALL);
        assert!(matches!(read, Err(Error::Lost(_))), "{read:?}");
    }

    /// What a clone in cell 3 of a point in cell 2 of a branch in cell 1 reads of what it inherited,
    /// the point and the branch deleted, and passes run over their cells, the point's then the
    /// branch's, as the clone makes its record made, having read the point live. Not the clone's:
    /// its pass would give the clone up, its point deleted, which loses nothing.
    async fn read_as_a_point_goes_under_a_clone() -> Result<Vec<u8>, Error> {
        use crate::layout::Origin;
        let direct = Arc::new(Direct(SimBucket::new()));
        let (origin_branch, point, clone) = (celled(1, 1), celled(2, 2), celled(3, 3));
        branch::create(&*direct, origin_branch, 0).await.unwrap();
        let writer = Writer::open(direct.clone(), origin_branch, 10, 0).await.unwrap();
        let extent = ExtentId::new(1, writer.chain().next_sequence());
        let made = Child { branch: point, owner: 0x22, point: true };
        let at = commit_and_fork(&writer, vec![(extent, b"held".to_vec())], made).await;
        drop(writer);
        let origin = Origin { parent: origin_branch, at };
        // Before the update that makes the fork, its record's second put.
        let bucket = Hooked::new(direct.clone(), Hook::Putting(layout::record(clone), 2));
        let sweeping = direct.clone();
        *bucket.then.lock().unwrap() = Some(Box::new(move || {
            let now = Options { compact_below: 0.0, grace: 0 };
            let deleted = async {
                branch::delete(&*sweeping, point, 20, 0).await.unwrap();
                branch::delete(&*sweeping, origin_branch, 21, 0).await.unwrap();
                collect_cell(&*sweeping, point.cell, 1, &now).await.unwrap();
                collect_cell(&*sweeping, origin_branch.cell, 1, &now).await.unwrap();
            };
            deleted.now_or_never().unwrap();
        }));
        branch::fork_point(&bucket, clone, point, origin, 0x33).await?;
        assert!(bucket.then.lock().unwrap().is_none(), "the sweep ran");
        sweep(&*direct).await?;
        read_inherited(&direct, clone, extent).await
    }

    /// Cells numbered from 0x100, `count` of them, each a branch of its own, `garbage` freed in it
    /// and checkpointed past: what a pass over each deletes.
    async fn garbage_cells(bucket: &Arc<SimBucket>, count: u64) {
        for cell in 0x100..0x100 + count {
            let branch = celled(cell, cell);
            branch::create(&**bucket, branch, 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), branch, 10, 0).await.unwrap();
            let extent = ExtentId::new(cell, writer.chain().next_sequence());
            let extents = vec![(extent, b"garbage".to_vec())];
            commit(&writer, Commit { tag: 1, extents, ..Commit::default() }).await;
            commit(&writer, Commit { tag: 2, frees: vec![extent], ..Commit::default() }).await;
            writer.checkpoint().await.unwrap();
        }
    }

    /// TASK-395.4: a pass over a cell, its one branch a fork of a branch in another cell, deleted,
    /// in a bucket of `others` cells more: the requests it makes, and the objects it deleted.
    fn pass_over_a_dead_fork(others: u64) -> (Vec<String>, Report, Branch, Branch, u64) {
        let bucket = SimBucket::new();
        let (parent, child) = (celled(1, 1), celled(2, 2));
        let at = bucket
            .served(async {
                garbage_cells(&bucket, others).await;
                branch::create(&*bucket, parent, 0).await.unwrap();
                let writer = Writer::open(bucket.clone(), parent, 10, 0).await.unwrap();
                let extent = ExtentId::new(1, writer.chain().next_sequence());
                let at = commit_and_fork(&writer, vec![(extent, vec![1; 64])], forked(child)).await;
                let forking = Writer::open(bucket.clone(), child, 11, 0).await.unwrap();
                let own = ExtentId::new(2, forking.chain().next_sequence());
                commit(
                    &forking,
                    Commit { tag: 3, extents: vec![(own, vec![2; 64])], ..Commit::default() },
                )
                .await;
                forking.checkpoint().await.unwrap();
                drop(forking);
                branch::delete(&*bucket, child, 21, 0).await.unwrap();
                at
            })
            .0;
        let options = Options { compact_below: 0.0, grace: 0 };
        let (report, asked) = bucket.served(collect_cell(&*bucket, child.cell, 7, &options));
        (asked, report.unwrap(), parent, child, at)
    }

    /// TASK-395.4's AC #1: a pass over a cell lists nothing outside the cell's prefix, writes and
    /// deletes nothing outside it but the one reference it retires, of its deleted child's fork
    /// across cells, and its mark (TASK-395.13); and what it asks is the same in a bucket of many
    /// more cells: it grows with the cell, not the bucket.
    #[test]
    fn passes_over_a_cell_list_and_delete_within_it_alone() {
        let (asked, report, parent, child, at) = pass_over_a_dead_fork(3);
        let prefix = child.cell.prefix();
        let retired = layout::reference(parent, at, child);
        let mark = layout::retired(parent, at, child);
        for request in &asked {
            let (verb, key) = request.split_once(' ').unwrap();
            let key = key.split(' ').next().unwrap();
            match verb {
                "list" | "list-delimited" => assert!(key.starts_with(&prefix), "{request}"),
                "put" => assert!(key.starts_with(&prefix) || key == mark, "{request}"),
                "delete" => assert!(key.starts_with(&prefix) || key == retired, "{request}"),
                _ => {}
            }
        }
        let lists = asked.iter().filter(|asked| asked.starts_with("list")).count();
        assert!(lists > 0, "{asked:#?}");
        assert!(asked.contains(&format!("delete {retired}")), "{asked:#?}");
        let (marked, deleted) = (
            position(&asked, &format!("put {mark}")),
            position(&asked, &format!("delete {retired}")),
        );
        assert!(marked < deleted, "marked first: {asked:#?}");
        assert_eq!(report.retired, 1);
        assert!(report.deleted > 0, "{report:?}");
        let (many, many_report, ..) = pass_over_a_dead_fork(30);
        assert_eq!(many_report, report);
        assert_eq!(many.len(), asked.len(), "{asked:#?}\n{many:#?}");
    }

    /// TASK-395.16: the LISTs of a pass in full over a cell, as doc-7 accounts for them (A pass
    /// over a cell, The LISTs a pass makes): the cell once, whose listing serves the first of the
    /// records; the records once more after reading them (until none new); the records after the
    /// states (`records_relisted`); the references by delimiter last (`references_relisted`). A
    /// branch's recovery lists nothing: its head names its checkpoint and its segments (TASK-407).
    /// Before: 7 LISTs a branch, the records listed twice more and each branch's checkpoints once
    /// more. The passes counted are the second over each cell, in full as the first deleted,
    /// holding its lease, so that what the first asks before taking the lease, finding none
    /// (`passable`), is no part of them.
    #[test]
    fn passes_list_their_cell_once_and_what_the_spec_relists() {
        let bucket = SimBucket::new();
        let (one, three) = (celled(0x100, 0x100).cell, celled(0x200, 0x201).cell);
        bucket.served(async {
            garbage_cells(&bucket, 1).await;
            for id in 0x201..=0x203 {
                let branch = celled(0x200, id);
                branch::create(&*bucket, branch, 0).await.unwrap();
                let writer = Writer::open(bucket.clone(), branch, 10, 0).await.unwrap();
                let extent = ExtentId::new(id, writer.chain().next_sequence());
                let extents = vec![(extent, b"garbage".to_vec())];
                commit(&writer, Commit { tag: 1, extents, ..Commit::default() }).await;
                commit(&writer, Commit { tag: 2, frees: vec![extent], ..Commit::default() }).await;
                writer.checkpoint().await.unwrap();
            }
        });
        let options = Options { compact_below: 0.0, grace: 0 };
        let lists = |cell: Cell| -> Vec<String> {
            let (first, _) = bucket.served(collect_cell(&*bucket, cell, 1, &options));
            assert!(first.unwrap().deleted > 0, "the first pass deletes, changing the cell");
            let (second, asked) = bucket.served(collect_cell(&*bucket, cell, 2, &options));
            assert_eq!(second.unwrap().skipped, 0, "{asked:#?}");
            asked.into_iter().filter(|asked| asked.starts_with("list")).collect()
        };
        let listed = |cell: Cell| -> Vec<String> {
            vec![
                format!("list {}", cell.prefix()),
                format!("list {}", layout::records(cell)),
                format!("list {}", layout::records(cell)),
                format!("list-delimited {}", layout::references(cell)),
            ]
        };
        assert_eq!(lists(one), listed(one));
        // However many branches: their recoveries list nothing.
        assert_eq!(lists(three), listed(three));
    }

    /// TASK-395.4's AC #2: a fork point many references name, a point cloned many times, is
    /// recovered once a pass: the pass over its cell asks the same, its references listed by
    /// delimiter, however many clones there are.
    #[test]
    fn fork_points_shared_by_many_references_are_recovered_once() {
        use crate::layout::Origin;
        let asked = |clones: u64| {
            let bucket = SimBucket::new();
            let (origin_branch, point) = (celled(1, 1), celled(2, 2));
            let at = bucket
                .served(async {
                    branch::create(&*bucket, origin_branch, 0).await.unwrap();
                    let writer = Writer::open(bucket.clone(), origin_branch, 10, 0).await.unwrap();
                    let extent = ExtentId::new(1, writer.chain().next_sequence());
                    let made = Child { branch: point, owner: 0x22, point: true };
                    let at = commit_and_fork(&writer, vec![(extent, vec![1; 64])], made).await;
                    commit(&writer, Commit { tag: 3, frees: vec![extent], ..Commit::default() })
                        .await;
                    writer.checkpoint().await.unwrap();
                    let origin = Origin { parent: origin_branch, at };
                    for clone in 0..clones {
                        let clone = celled(0x10 + clone, 0x10 + clone);
                        branch::fork_point(&*bucket, clone, point, origin, clone.id).await.unwrap();
                    }
                    at
                })
                .0;
            let options = Options { compact_below: 0.0, grace: 0 };
            let (report, asked) =
                bucket.served(collect_cell(&*bucket, origin_branch.cell, 7, &options));
            report.unwrap();
            // The fork point's state, the checkpoint at it the point's recording wrote.
            let point = format!("get {}", layout::checkpoint(origin_branch, at, 1));
            let read = asked.iter().filter(|asked| **asked == point).count();
            (asked.len(), read)
        };
        let (one, read) = asked(1);
        assert_eq!(read, 1, "the fork point's checkpoint");
        assert_eq!(asked(8), (one, read));
    }

    /// TASK-395.12: a pass over a cell whose branch was forked at many points to other cells, a VM
    /// snapshotted many times, lists none of the branch's checkpoints: its recovery reads what its
    /// head names (TASK-407), and the listing of the cell serves each fork point's; and it reads
    /// each fork point's checkpoint, which recording the fork wrote, once. Before, it listed them
    /// once after the branch's segments (`newest_checkpoint`), and before that once more a fork
    /// point.
    #[test]
    fn passes_list_a_branchs_checkpoints_once_however_many_points_it_was_forked_at() {
        let parent = celled(1, 1);
        let asked = |points: u64| {
            let bucket = SimBucket::new();
            let (ats, _) = bucket.served(async {
                branch::create(&*bucket, parent, 0).await.unwrap();
                let writer = Writer::open(bucket.clone(), parent, 10, 0).await.unwrap();
                let mut ats = Vec::new();
                for n in 0..points {
                    let extent = ExtentId::new(1, writer.chain().next_sequence());
                    let point = Child { branch: celled(0x10 + n, 1), owner: 0x22, point: true };
                    ats.push(commit_and_fork(&writer, vec![(extent, vec![1; 64])], point).await);
                }
                // Written on, its own checkpoint past the last fork point's.
                commit(&writer, Commit { tag: 3, ..Commit::default() }).await;
                writer.checkpoint().await.unwrap();
                ats
            });
            let options = Options { compact_below: 0.0, grace: 0 };
            let (report, asked) = bucket.served(collect_cell(&*bucket, parent.cell, 7, &options));
            report.unwrap();
            let count = |request: String| asked.iter().filter(|asked| **asked == request).count();
            let listed = count(format!("list {}", layout::checkpoints(parent)));
            let read = |&at: &u64| count(format!("get {}", layout::checkpoint(parent, at, 1)));
            let read: Vec<usize> = ats.iter().map(read).collect();
            (listed, read)
        };
        assert_eq!(asked(1), (0, vec![1]));
        assert_eq!(asked(6), (0, vec![1; 6]));
    }

    /// TASK-407: the checkpoint at a fork point to another cell, which the fork's child reads by
    /// its key alone, of the epoch of the fork's segment, stands through passes over the parent's
    /// cell, though a later writer checkpointed at the same segment, at its own key, which the
    /// pass's recovery of the fork point reads instead. Before, the pass kept the later writer's
    /// alone, deleting the child's, which then failed to open (the simulation's `everything`).
    #[test]
    fn fork_points_to_other_cells_keep_their_checkpoints_past_later_writers() {
        use crate::layout::Origin;
        let bucket = store();
        let (parent, child) = (celled(1, 1), celled(2, 2));
        let at = block_on(async {
            branch::create(&*bucket, parent, 0).await.unwrap();
            let first = Writer::open(bucket.clone(), parent, 10, 0).await.unwrap();
            let extent = ExtentId::new(1, first.chain().next_sequence());
            let at = commit_and_fork(&first, vec![(extent, vec![1; 64])], forked(child)).await;
            // A later writer checkpoints at the fork's segment, at a key of its epoch.
            let second = Writer::open(bucket.clone(), parent, 11, 0).await.unwrap();
            assert_eq!(second.checkpoint().await.unwrap(), Some(at));
            at
        });
        let held = block_on(keys(&*bucket));
        let (forks, later) = (layout::checkpoint(parent, at, 1), layout::checkpoint(parent, at, 2));
        assert!(held.contains(&forks) && held.contains(&later), "{held:#?}");
        let options = Options { compact_below: 0.0, grace: 0 };
        for pass in 1..=2 {
            block_on(collect_cell(&*bucket, parent.cell, pass, &options)).unwrap();
        }
        assert!(block_on(keys(&*bucket)).contains(&forks));
        let origin = Some(Origin { parent, at });
        assert!(block_on(recovery::recover(&*bucket, child, origin)).is_ok());
    }

    /// TASK-395.4: a dry run over a dead cell says what reference a pass would retire, deleting it
    /// not; a mark-only pass retires none either; a pass does.
    #[test]
    fn dry_runs_and_mark_only_passes_retire_nothing() {
        let bucket = store();
        let (parent, child) = (celled(1, 1), celled(2, 2));
        let at = block_on(async {
            branch::create(&*bucket, parent, 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), parent, 10, 0).await.unwrap();
            let at = commit_and_fork(&writer, Vec::new(), forked(child)).await;
            branch::delete(&*bucket, child, 21, 0).await.unwrap();
            at
        });
        let reference = layout::reference(parent, at, child);
        let mut collector = Collector::new(KEEP);
        let dry = block_on(collector.dry_run_cell(&*bucket, child.cell)).unwrap();
        assert_eq!(dry.retired, std::slice::from_ref(&reference));
        let marking = block_on(collector.mark_cell(&*bucket, child.cell, 1)).unwrap();
        assert_eq!(marking.report.retired, 0);
        assert!(block_on(keys(&*bucket)).contains(&reference));
        let report = block_on(collector.collect_cell(&*bucket, child.cell, 2)).unwrap();
        assert_eq!(report.retired, 1);
        assert!(!block_on(keys(&*bucket)).contains(&reference));
    }

    /// TASK-395.4's AC #1, a live fork's cell (TASK-395.4's review): a pass over the cell of a
    /// fork of a branch in another cell, the fork live with no checkpoint of its own, lists nothing
    /// outside its cell, reading its parent's state by the checkpoint at the fork point alone,
    /// which recording the fork wrote; whether the parent had a checkpoint before the fork, or
    /// none. The fork reads what it inherited.
    #[test]
    fn passes_over_a_live_forks_cell_list_nothing_of_its_parents() {
        for checkpointed in [true, false] {
            let bucket = SimBucket::new();
            let (parent, child) = (celled(1, 1), celled(2, 2));
            let (extent, at) = bucket
                .served(async {
                    branch::create(&*bucket, parent, 0).await.unwrap();
                    let writer = Writer::open(bucket.clone(), parent, 10, 0).await.unwrap();
                    let extent = ExtentId::new(1, writer.chain().next_sequence());
                    if checkpointed {
                        commit(&writer, Commit { tag: 9, ..Commit::default() }).await;
                        writer.checkpoint().await.unwrap();
                    }
                    let held = vec![(extent, b"held".to_vec())];
                    (extent, commit_and_fork(&writer, held, forked(child)).await)
                })
                .0;
            let options = Options { compact_below: 0.0, grace: 0 };
            let (report, asked) = bucket.served(collect_cell(&*bucket, child.cell, 7, &options));
            report.unwrap();
            let prefix = child.cell.prefix();
            let outside: Vec<&String> = (asked.iter())
                .filter(|asked| asked.starts_with("list"))
                .filter(|asked| !asked.split(' ').nth(1).unwrap().starts_with(&prefix))
                .collect();
            assert_eq!(outside, Vec::<&String>::new(), "checkpointed {checkpointed}");
            let point = format!("get {}", layout::checkpoint(parent, at, 1));
            assert!(asked.contains(&point), "{asked:#?}");
            let direct = Arc::new(Direct(bucket.clone()));
            assert_eq!(block_on(read_inherited(&direct, child, extent)), Ok(b"held".to_vec()));
        }
    }

    /// TASK-395.10: a clone refused by a record of its child's ID there before, of another origin,
    /// deleted with none, or of its own origin, writes no reference, as it writes it after its own
    /// record.
    #[test]
    fn clones_refused_by_records_there_write_no_references() {
        assert_eq!(references_left_by_refused_clones(), [false, false, false]);
    }

    /// TASK-395.4's review: without the rule, the reference written first, a clone so refused
    /// takes it back, which no record's origin names; one refused by a record of its own origin
    /// leaves it, that record's to retire. Its host dying first left it (TASK-395.10).
    #[test]
    fn clones_refused_by_records_there_take_their_references_back_without_clones_record_first() {
        crate::set_rules(crate::Rules { clones_record_first: false, ..crate::Rules::ALL });
        let left = references_left_by_refused_clones();
        crate::set_rules(crate::Rules::ALL);
        assert_eq!(left, [false, false, true]);
    }

    /// Whether a clone refused by each of three records of its child's ID there before, of
    /// another origin, deleted with none, and of the clone's own origin, leaves its reference.
    fn references_left_by_refused_clones() -> [bool; 3] {
        use crate::layout::Origin;
        use crate::record::Kind as Made;
        let bucket = Direct(SimBucket::new());
        let (origin, point) = (Origin { parent: celled(1, 1), at: 3 }, celled(2, 2));
        let other = Origin { parent: celled(1, 1), at: 4 };
        let record = layout::record(point);
        block_on(bucket::create(&bucket, &record, layout::encode(&Made::Point(origin).of(0x22))));
        let found = |made: crate::record::Record| layout::encode(&made);
        let cases = [
            (celled(3, 3), found(Made::Fork(other).of(0x44)), Error::IdTaken(celled(3, 3))),
            (
                celled(4, 4),
                found(crate::record::Record { deleted: true, ..Made::Own.of(0x45) }),
                Error::Deleted,
            ),
            (celled(5, 5), found(Made::Fork(origin).of(0x46)), Error::IdTaken(celled(5, 5))),
        ];
        cases.map(|(clone, there, refused)| {
            block_on(bucket::create(&bucket, &layout::record(clone), there));
            let forked = block_on(branch::fork_point(&bucket, clone, point, origin, 0x33));
            assert_eq!(forked, Err(refused), "{clone}");
            let reference = layout::reference(origin.parent, origin.at, clone);
            block_on(bucket::get(&bucket, &reference)).is_some()
        })
    }

    /// TASK-395.4's review: a pass over a cell holding nothing takes no lease there, which would be
    /// its one object; one over a cell of another format's records refuses before taking it.
    #[test]
    fn passes_take_no_lease_of_cells_empty_or_of_other_formats() {
        let bucket = Direct(SimBucket::new());
        let (empty, older) = (layout::Cell { tenant: 1, id: 7 }, layout::Cell { tenant: 1, id: 8 });
        let options = Options { compact_below: 0.0, grace: 0 };
        assert_eq!(block_on(collect_cell(&bucket, empty, 1, &options)), Ok(Report::default()));
        let branch = Branch { cell: older, id: 9 };
        let mut format8 = 8u32.to_le_bytes().to_vec();
        format8.extend_from_slice(b"what format 8 held");
        block_on(bucket::create(&bucket, &layout::record(branch), format8));
        let refused = block_on(collect_cell(&bucket, older, 1, &options));
        assert_eq!(refused, Err(Error::Format { branch, format: 8 }));
        // A cell's object of format 8, whatever its records', refused as such.
        let celled = layout::Cell { tenant: 1, id: 9 };
        block_on(bucket::create(&bucket, &layout::cell(celled), 8u32.to_le_bytes().to_vec()));
        let refused = block_on(collect_cell(&bucket, celled, 1, &options));
        let branch = Branch { cell: celled, id: 0 };
        assert_eq!(refused, Err(Error::Format { branch, format: 8 }));
        let leases: Vec<String> = (bucket.0.keys().into_iter())
            .filter(|key| matches!(layout::kind(key), Kind::CollectorLease { .. }))
            .collect();
        assert_eq!(leases, Vec::<String>::new());
    }

    /// TASK-395.4's review, found by the simulation (seed 11 of `restores`): a fork across cells
    /// recorded again, by a later writer of its parent or the parent's deletion, after the parent
    /// checkpointed past the fork point and passes reclaimed its segments up to there, the
    /// checkpoint at the fork point kept: it records nothing more, recovering nothing of the
    /// parent there, which it could not. Before, recording it again recovered the parent at the
    /// fork point from its segments, gone, and failed the writer's acknowledgement.
    #[test]
    fn forks_across_cells_recorded_again_recover_nothing_of_their_parents() {
        block_on(async {
            let bucket = store();
            let (parent, child) = (celled(1, 1), celled(2, 2));
            branch::create(&*bucket, parent, 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), parent, 10, 0).await.unwrap();
            let extent = ExtentId::new(1, writer.chain().next_sequence());
            let at =
                commit_and_fork(&writer, vec![(extent, b"held".to_vec())], forked(child)).await;
            commit(&writer, Commit { tag: 3, frees: vec![extent], ..Commit::default() }).await;
            writer.checkpoint().await.unwrap();
            sweep(&*bucket).await.unwrap();
            let left = keys(&*bucket).await;
            assert!(!left.contains(&layout::segment(parent, at, 1)), "the fork's segment went");
            assert!(left.contains(&layout::checkpoint(parent, at, 1)), "the fork point kept");
            let origin = crate::layout::Origin { parent, at };
            assert_eq!(branch::record_fork(&*bucket, forked(child), origin).await, Ok(()));
            // Its checkpoint at the fork point gone too, as nothing keeps it for a record of the
            // fork's owner no reference keeps, a failed fork's claim landing late (seed 11's).
            let point = layout::checkpoint(parent, at, 1);
            bucket::delete_all(&*bucket, &[point.as_str()]).await;
            assert_eq!(branch::record_fork(&*bucket, forked(child), origin).await, Ok(()));
            drop(writer);
            assert_eq!(branch::delete(&*bucket, parent, 20, 0).await, Ok(()));
        });
    }

    /// TASK-395.4's review (sim seed 143): a failed fork to another cell whose parent's state at
    /// the fork point is gone is deleted from the first, not claimed: no reader recovers it.
    #[test]
    fn failed_forks_across_cells_of_a_state_gone_are_deleted_from_the_first() {
        block_on(async {
            let bucket = store();
            let (parent, child, failed) = (celled(1, 1), celled(2, 2), celled(2, 3));
            branch::create(&*bucket, parent, 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), parent, 10, 0).await.unwrap();
            let extent = ExtentId::new(1, writer.chain().next_sequence());
            let at =
                commit_and_fork(&writer, vec![(extent, b"held".to_vec())], forked(child)).await;
            commit(&writer, Commit { tag: 3, frees: vec![extent], ..Commit::default() }).await;
            branch::delete(&*bucket, child, 30, u64::MAX).await.unwrap();
            writer.checkpoint().await.unwrap();
            sweep(&*bucket).await.unwrap();
            let left = keys(&*bucket).await;
            assert!(!left.contains(&layout::segment(parent, at, 1)), "the fork point let go of");
            assert!(!left.contains(&layout::checkpoint(parent, at, 1)), "the fork point let go of");
            let origin = crate::layout::Origin { parent, at };
            let owner = forked(failed).owner;
            let deleted = branch::delete_fork(
                &*bucket,
                failed,
                crate::record::Kind::Fork(origin),
                owner,
                30,
                0,
            );
            assert_eq!(deleted.await, Ok(true));
            let recorded = branch::record(&*bucket, failed).await.unwrap().unwrap();
            assert_eq!((recorded.deleted, recorded.origin()), (true, Some(origin)));
            drop(writer);
            assert_eq!(branch::delete(&*bucket, parent, 20, 0).await, Ok(()));
        });
    }

    /// TASK-395.11, found by the simulation (restores, seed 11, then 2545 once the seeds moved): a
    /// writer of the parent records an acknowledged fork across cells, its read finding the child
    /// with no record; before it writes the parent's checkpoint at the fork point, the child is
    /// deleted, its host's fork failing, the parent's next writer checkpoints past the fork point,
    /// dropping the fork of a child deleted, and a sweep lets the fork point go. The parent's state
    /// there gone, the writer reads the record again: deleted, it writes nothing, no reference, no
    /// checkpoint, no record, nor fails. Before, it recorded the child deleted, which the record
    /// there refused, and before TASK-395.4's review it failed the writer.
    #[test]
    fn forks_recorded_after_their_children_went_record_nothing() {
        block_on(async {
            let direct = Arc::new(Direct(SimBucket::new()));
            let (parent, child) = (celled(1, 1), celled(2, 2));
            let bucket = Arc::new(Hooked::new(direct.clone(), Hook::Got(layout::record(child))));
            branch::create(&*direct, parent, 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), parent, 10, 0).await.unwrap();
            let extent = ExtentId::new(1, writer.chain().next_sequence());
            let extents = vec![(extent, b"held".to_vec())];
            commit(&writer, Commit { tag: 1, extents, ..Commit::default() }).await;
            let forking = SegmentBuilder::from(Commit { tag: 2, ..Commit::default() });
            let at = writer
                .add_segment(forking.fork(forked(child), &mut writer.chain()).frame())
                .unwrap();
            writer.upload(at).await.unwrap();
            let going = direct.clone();
            *bucket.then.lock().unwrap() = Some(Box::new(move || {
                let went = std::thread::spawn(move || {
                    block_on(async {
                        branch::delete(&*going, child, 30, u64::MAX).await.unwrap();
                        let next = Writer::open(going.clone(), parent, 11, 0).await.unwrap();
                        let freeing = Commit { tag: 3, frees: vec![extent], ..Commit::default() };
                        commit(&next, freeing).await;
                        next.checkpoint().await.unwrap();
                        sweep(&*going).await.unwrap();
                    })
                });
                went.join().unwrap();
            }));
            assert_eq!(writer.acknowledge().await.map(drop), Ok(()));
            assert!(bucket.then.lock().unwrap().is_none(), "the child went");
            let left = keys(&*direct).await;
            assert!(!left.contains(&layout::segment(parent, at, 1)), "the fork point let go of");
            assert!(!left.contains(&layout::checkpoint(parent, at, 1)), "{left:#?}");
            assert!(!left.contains(&layout::reference(parent, at, child)), "{left:#?}");
            let record = branch::record(&*direct, child).await.unwrap().unwrap();
            assert_eq!((record.deleted, record.origin()), (true, None));
        });
    }

    /// TASK-395.10: a writable fork across cells whose child's ID another branch takes between the
    /// read of the child's record and its create, its reference written, takes the reference back;
    /// and one left by a writer dying between its create refused and that, the parent's deletion
    /// takes back, recording the fork again. No pass would retire either: no record names it.
    #[test]
    fn forks_whose_childrens_ids_are_taken_take_their_references_back() {
        assert_eq!(block_on(references_left_by_taken_ids()), [false, false]);
    }

    /// Without the rule, the references stay, for ever: specs/bucket's NoReferencesTakenBack.
    #[test]
    fn forks_whose_childrens_ids_are_taken_need_references_taken_back() {
        crate::set_rules(crate::Rules { references_taken_back: false, ..crate::Rules::ALL });
        let left = block_on(references_left_by_taken_ids());
        crate::set_rules(crate::Rules::ALL);
        assert_eq!(left, [true, true]);
    }

    /// Whether references are left of two forks of a branch to other cells whose children's IDs
    /// other branches took: one taken as its writer writes the reference, one whose writer died
    /// before it recorded it, a reference left, then the branch deleted; and the passes after.
    async fn references_left_by_taken_ids() -> [bool; 2] {
        let direct = Arc::new(Direct(SimBucket::new()));
        let (parent, child, late) = (celled(1, 1), celled(2, 2), celled(3, 3));
        branch::create(&*direct, parent, 0).await.unwrap();
        let hook = Hook::Putting(layout::references(parent.cell), 1);
        let bucket = Arc::new(Hooked::new(direct.clone(), hook));
        let writer = Writer::open(bucket.clone(), parent, 10, 0).await.unwrap();
        let taking = direct.clone();
        *bucket.then.lock().unwrap() = Some(Box::new(move || {
            branch::create(&*taking, child, 0x99).now_or_never().unwrap().unwrap();
        }));
        let at = commit_and_fork(&writer, Vec::new(), forked(child)).await;
        assert!(bucket.then.lock().unwrap().is_none(), "the ID taken");
        drop(writer);
        // The next writer's fork, acknowledged, its head naming it, the writer dying as it writes
        // the fork's reference, before it records the child.
        let late_at = at + 1;
        let (dying, _never) =
            Gated::new(direct.clone(), Gate::Put(layout::reference(parent, late_at, late)));
        let writer = Writer::open(Arc::new(dying), parent, 11, 0).await.unwrap();
        let forking = SegmentBuilder::from(Commit { tag: 4, ..Commit::default() });
        let forked_at =
            writer.add_segment(forking.fork(forked(late), &mut writer.chain()).frame()).unwrap();
        assert_eq!(forked_at, late_at);
        writer.upload(late_at).await.unwrap();
        let mut acknowledging = Box::pin(writer.acknowledge());
        assert!((&mut acknowledging).now_or_never().is_none(), "its writer dies recording");
        drop(acknowledging);
        drop(writer);
        branch::create(&*direct, late, 0x98).await.unwrap();
        let reference = layout::reference(parent, late_at, late);
        let bytes = layout::encode(&layout::Reference::of(forked(late).owner));
        bucket::create(&*direct, &reference, bytes).await;
        assert_eq!(branch::delete(&*direct, parent, 20, 0).await, Ok(()));
        sweep(&*direct).await.unwrap();
        let left = keys(&*direct).await;
        [layout::reference(parent, at, child), reference].map(|key| left.contains(&key))
    }

    /// TASK-395.14, the simulation's restores, seed 53: a recorder of a fork across cells (the
    /// parent's writer, its next, its deletion) reads the child's record live, its own; the child
    /// is then deleted, and its cell's pass retires the reference. The recorder writes no reference:
    /// the record's maker wrote it first. Before, it wrote it again after its retirement, which the
    /// dead cell's passes, skipping it, retired again only by sending every retirement again, every
    /// pass.
    #[test]
    fn recorders_finding_their_childs_record_write_no_reference() {
        block_on(async {
            let direct = Arc::new(Direct(SimBucket::new()));
            let (parent, child) = (celled(1, 1), celled(2, 2));
            branch::create(&*direct, parent, 0).await.unwrap();
            let writer = Writer::open(direct.clone(), parent, 10, 0).await.unwrap();
            let at = commit_and_fork(&writer, Vec::new(), forked(child)).await;
            let reference = layout::reference(parent, at, child);
            assert!(keys(&*direct).await.contains(&reference));
            let bucket = Hooked::new(direct.clone(), Hook::Got(layout::record(child)));
            let going = direct.clone();
            *bucket.then.lock().unwrap() = Some(Box::new(move || {
                let went = std::thread::spawn(move || {
                    block_on(async {
                        branch::delete(&*going, child, 30, u64::MAX).await.unwrap();
                        let mut collector =
                            Collector::new(Options { compact_below: 0.0, grace: 0 });
                        let report = collector.collect_cell(&*going, child.cell, 1).await;
                        assert_eq!(report.unwrap().retired, 1);
                    })
                });
                went.join().unwrap();
            }));
            let origin = layout::Origin { parent, at };
            assert_eq!(branch::record_fork(&bucket, forked(child), origin).await, Ok(()));
            assert!(bucket.then.lock().unwrap().is_none(), "the child went");
            assert!(!keys(&*direct).await.contains(&reference));
        });
    }

    /// TASK-395.13 and TASK-395.14: a writer of the parent recording a fork across cells reads the
    /// child's record, twice (TASK-395.11), finding none; before it writes the reference, either
    /// the child's ID is taken by a root and another recorder takes the reference back, the writer
    /// dying once it wrote it, before its create (`taken`); or another recorder records the fork,
    /// the child is deleted and its cell's passes retire the reference, until they skip the cell,
    /// dead and unchanged, the writer going on. Its
    /// reference written after, no record will name it, and no recorder nor pass of the child's
    /// cell looks at it again: the pass over the parent's cell deletes it, by its mark, whatever
    /// else of the parent it keeps. Whether it is left once sweeps ran, and what else
    /// [`crate::testing::uncollected`] finds left.
    async fn left_by_a_stale_recorder(taken: bool) -> (bool, Vec<String>) {
        let direct = Arc::new(Direct(SimBucket::new()));
        let (parent, child) = (celled(1, 1), celled(2, 2));
        branch::create(&*direct, parent, 0).await.unwrap();
        let hook = Hook::Gotten(layout::record(child), 2);
        let bucket = Arc::new(Hooked::new(direct.clone(), hook));
        let writer = Writer::open(bucket.clone(), parent, 10, 0).await.unwrap();
        commit(&writer, Commit { tag: 1, ..Commit::default() }).await;
        let forking = SegmentBuilder::from(Commit { tag: 2, ..Commit::default() });
        let at =
            writer.add_segment(forking.fork(forked(child), &mut writer.chain()).frame()).unwrap();
        writer.upload(at).await.unwrap();
        let origin = layout::Origin { parent, at };
        let reference = layout::reference(parent, at, child);
        let other = direct.clone();
        // The other recorder and the pass, on a thread of their own, by this one's rules.
        let rules = crate::rules();
        *bucket.then.lock().unwrap() = Some(Box::new(move || {
            let went = std::thread::spawn(move || {
                crate::set_rules(rules);
                block_on(async {
                    if taken {
                        branch::create(&*other, child, 0x99).await.unwrap();
                        let refused = branch::record_fork(&*other, forked(child), origin).await;
                        assert_eq!(refused, Err(Error::IdTaken(child)));
                        return;
                    }
                    branch::record_fork(&*other, forked(child), origin).await.unwrap();
                    branch::delete(&*other, child, 30, u64::MAX).await.unwrap();
                    // Until the child's cell settles, its passes skipping it.
                    sweep(&*other).await.unwrap();
                    let reference = layout::reference(parent, at, child);
                    assert!(!keys(&*other).await.contains(&reference), "retired");
                })
            });
            went.join().unwrap();
        }));
        if taken {
            *bucket.dies_after.lock().unwrap() = Some(reference.clone());
            assert!(writer.acknowledge().now_or_never().is_none(), "the writer died");
        } else {
            assert_eq!(writer.acknowledge().await.map(drop), Ok(()));
        }
        drop(writer);
        assert!(bucket.then.lock().unwrap().is_none(), "the child's record came");
        assert!(keys(&*direct).await.contains(&reference), "written after");
        sweep(&*direct).await.unwrap();
        let left = keys(&*direct).await.contains(&reference);
        (left, crate::testing::uncollected(&*direct, 0.0).await)
    }

    /// The reference such a stale recorder writes goes with the pass over its parent's cell.
    #[test]
    fn stale_recorders_references_go_by_their_marks() {
        for taken in [true, false] {
            let left = block_on(left_by_a_stale_recorder(taken));
            assert_eq!(left, (false, Vec::new()), "taken: {taken}");
        }
    }

    /// Without the rule, they stay, for ever: specs/bucket's NoRetirementsMarked.
    #[test]
    fn stale_recorders_references_need_retirements_marked() {
        crate::set_rules(crate::Rules { retirements_marked: false, ..crate::Rules::ALL });
        let left = [true, false].map(|taken| block_on(left_by_a_stale_recorder(taken)).0);
        crate::set_rules(crate::Rules::ALL);
        assert_eq!(left, [true, true]);
    }

    /// TASK-395.10: a clone's host dying at each step of the clone, before its reference, before
    /// its read of the point, before it makes its record made, its directory then deleted
    /// knowing nothing of the child, as a host's left unmade does: once the point and its origin
    /// are deleted and passes ran, nothing is left of either, nor of the clone but its record,
    /// deleted, which names the origin its reference was retired by.
    #[test]
    fn clones_whose_hosts_die_leave_nothing_once_their_points_go() {
        for (step, nth) in [("reference", 1), ("read", 1), ("record", 2)] {
            let (left, references) = left_by_a_clone_dying_before(step, nth);
            assert_eq!((left, references), (Vec::<String>::new(), 0), "before its {step}");
        }
    }

    /// Without the rule, a host dying between the reference and the record leaves the reference,
    /// which no record names, for ever, with what it keeps: specs/bucket's NoForksRecordFirst.
    #[test]
    fn clones_whose_hosts_die_need_clones_record_first() {
        crate::set_rules(crate::Rules { clones_record_first: false, ..crate::Rules::ALL });
        let (left, references) = left_by_a_clone_dying_before("record", 1);
        crate::set_rules(crate::Rules::ALL);
        assert_eq!(references, 1, "{left:#?}");
        assert!(left.iter().any(|left| left.contains("/refs/")), "{left:#?}");
    }

    /// What [`crate::testing::uncollected`] finds left, and how many references, of a clone of a
    /// point whose host died before the `nth` request of its `step` (its record's put, its
    /// reference's, the read of its point's record), the point and its origin then deleted and
    /// passes run.
    fn left_by_a_clone_dying_before(step: &str, nth: usize) -> (Vec<String>, usize) {
        use crate::layout::Origin;
        let bucket = SimBucket::new();
        let (origin_branch, point, clone) = (celled(1, 1), celled(2, 2), celled(3, 3));
        let origin = bucket
            .served(async {
                branch::create(&*bucket, origin_branch, 0).await.unwrap();
                let writer = Writer::open(bucket.clone(), origin_branch, 10, 0).await.unwrap();
                let extent = ExtentId::new(1, writer.chain().next_sequence());
                let made = Child { branch: point, owner: 0x22, point: true };
                let at = commit_and_fork(&writer, vec![(extent, b"held".to_vec())], made).await;
                Origin { parent: origin_branch, at }
            })
            .0;
        let request = match step {
            "reference" => format!("put {}", layout::reference(origin.parent, origin.at, clone)),
            "read" => format!("get {}", layout::record(point)),
            _ => format!("put {}", layout::record(clone)),
        };
        let forking = branch::fork_point(&*bucket, clone, point, origin, 0x33);
        let (asked, died) = bucket.died_before(forking, (&request, nth));
        assert!(died, "{step} {nth}: {asked:#?}");
        let direct = Direct(bucket.clone());
        block_on(async {
            branch::delete(&direct, point, 20, 0).await.unwrap();
            branch::delete(&direct, origin_branch, 21, 0).await.unwrap();
            sweep(&direct).await.unwrap();
            let left = crate::testing::uncollected(&direct, 0.0).await;
            let references =
                keys(&direct).await.iter().filter(|key| key.contains("/refs/")).count();
            (left, references)
        })
    }

    /// TASK-395.7: a clone of a shared point into another tenant's cell writes its reference under
    /// the cell of the point's origin, the point's tenant's, as any clone does; the point and its
    /// origin deleted, passes keep what the clones inherited, which they read; once the clones go,
    /// their references are retired, and nothing of any is left but records.
    #[test]
    fn clones_of_shared_points_in_other_tenants_keep_what_they_inherit_by_references() {
        use crate::layout::Origin;
        block_on(async {
            let direct = Arc::new(Direct(SimBucket::new()));
            let (origin_branch, point) = (celled(1, 1), celled(2, 2));
            let theirs = |id| Branch { cell: layout::Cell { tenant: 2, id }, id };
            branch::create(&*direct, origin_branch, 0).await.unwrap();
            let writer = Writer::open(direct.clone(), origin_branch, 10, 0).await.unwrap();
            let extent = ExtentId::new(1, writer.chain().next_sequence());
            let made = Child { branch: point, owner: 0x22, point: true };
            let at = commit_and_fork(&writer, vec![(extent, b"held".to_vec())], made).await;
            drop(writer);
            let origin = Origin { parent: origin_branch, at };
            branch::share(&*direct, point, true).await.unwrap();
            let clones = [theirs(3), theirs(4)];
            for clone in clones {
                branch::fork_point(&*direct, clone, point, origin, clone.id).await.unwrap();
                let reference = layout::reference(origin_branch, at, clone);
                assert!(reference.starts_with(&origin_branch.cell.prefix()), "{reference}");
                assert!(keys(&*direct).await.contains(&reference), "{reference}");
            }
            branch::delete(&*direct, point, 20, 0).await.unwrap();
            branch::delete(&*direct, origin_branch, 21, 0).await.unwrap();
            for _ in 0..2 {
                sweep(&*direct).await.unwrap();
                for clone in clones {
                    let read = read_inherited(&direct, clone, extent).await;
                    assert_eq!(read, Ok(b"held".to_vec()), "{clone}");
                }
            }
            for clone in clones {
                branch::delete(&*direct, clone, 30, u64::MAX).await.unwrap();
            }
            sweep(&*direct).await.unwrap();
            let left = keys(&*direct).await;
            assert!(!left.iter().any(|key| key.contains("/refs/")), "{left:#?}");
            assert_eq!(crate::testing::uncollected(&*direct, 0.0).await, Vec::<String>::new());
        });
    }

    // cladefs: passes that skip cells unchanged since the last pass, and what passes record of the
    // cells (TASK-395.6, TASK-395.8).

    /// The requests of a sweep, `asked`, as [`SimBucket::served`] gives them, by the cell of their
    /// keys; those of no cell's (the listings finding the cells) left out.
    fn by_cell(asked: &[String]) -> BTreeMap<Cell, Vec<String>> {
        let mut cells: BTreeMap<Cell, Vec<String>> = BTreeMap::new();
        for request in asked {
            let key = request.split(' ').nth(1).unwrap();
            if let Some(cell) = layout::cell_of(key) {
                cells.entry(cell).or_default().push(request.clone());
            }
        }
        cells
    }

    /// What a pass over `cell` asks that skips it, deleting nothing: its lease, read, and a listing
    /// of the cell; it writes nothing, taking no lease (TASK-455).
    fn idle(cell: Cell) -> Vec<String> {
        let lease = layout::collector_lease(cell);
        vec![format!("get {lease}"), format!("list {}", cell.prefix())]
    }

    /// TASK-395.6's AC #1 and #3: a sweep over cells unchanged since their last passes asks of each
    /// its lease and one listing, nothing more; a write to any of them, a segment, a reference, a
    /// record, a head a writer takes over (an object written again at its key), a branch deleted,
    /// makes the next pass over it go over it in full, recovering it, and the one after skip it
    /// again once it changes it no more, the other cells skipped all along.
    #[test]
    fn idle_cells_cost_a_listing_until_written() {
        let bucket = SimBucket::new();
        let (parent, child) = (celled(1, 1), celled(2, 2));
        let writer = bucket
            .served(async {
                garbage_cells(&bucket, 4).await;
                branch::create(&*bucket, parent, 0).await.unwrap();
                let writer = Writer::open(bucket.clone(), parent, 10, 0).await.unwrap();
                let extent = ExtentId::new(1, writer.chain().next_sequence());
                commit_and_fork(&writer, vec![(extent, vec![1; 64])], forked(child)).await;
                writer.checkpoint().await.unwrap();
                writer
            })
            .0;
        let options = Options { compact_below: 0.0, grace: 0 };
        bucket.served(crate::testing::collect_until_quiet(&*bucket, &options)).0.unwrap();
        let cells: Vec<Cell> = bucket.served(crate::cell::listed(&*bucket).collect()).0;
        assert_eq!(cells.len(), 6, "{cells:?}");
        let mut collector = Collector::new(options);
        let mut holder = 0x10;
        let mut sweep = |collector: &mut Collector| {
            holder += 1;
            let (report, asked) = bucket.served(collector.collect(&*bucket, holder));
            report.unwrap();
            by_cell(&asked)
        };
        let all_idle: BTreeMap<Cell, Vec<String>> =
            cells.iter().map(|&cell| (cell, idle(cell))).collect();
        assert_eq!(sweep(&mut collector), all_idle);

        // Each write, the cell it is in, and what the pass over it in full asks that a pass skipping
        // it never does.
        let garbage = |cell: u64| celled(cell, cell);
        // At the garbage branch's checkpoint, its state there whole: a fork point found lost would
        // have its pass read its children's records, in other cells, and skip it never.
        let at = (bucket.keys().iter())
            .find_map(|key| match layout::kind(key) {
                Kind::Checkpoint { branch, segment, .. } if branch == garbage(0x100) => {
                    Some(segment)
                }
                _ => None,
            })
            .unwrap();
        let reference = layout::reference(garbage(0x100), at, celled(0x200, 0x200));
        let made = celled(0x101, 0x301);
        let taken = garbage(0x103);
        let writes: [(&str, Cell, String); 5] = [
            ("segment", parent.cell, format!("get {}", layout::head(parent))),
            (
                "reference",
                garbage(0x100).cell,
                format!("list-delimited {}", layout::references(garbage(0x100).cell)),
            ),
            ("record", made.cell, format!("get {}", layout::record(made))),
            ("takeover", taken.cell, format!("get {}", layout::head(taken))),
            ("deletion", garbage(0x102).cell, format!("get {}", layout::record(garbage(0x102)))),
        ];
        for (write, cell, recovered) in writes {
            match write {
                "segment" => {
                    bucket.served(commit(&writer, Commit { tag: 7, ..Commit::default() }));
                }
                // A clone's first step, its host dying after it (TASK-395.10).
                "reference" => {
                    let bytes = layout::encode(&layout::Reference::of(0x200));
                    bucket.served(bucket::create(&*bucket, &reference, bytes));
                }
                "record" => {
                    bucket.served(branch::create(&*bucket, made, 0)).0.unwrap();
                }
                // A writer's first step taking the branch over: its head, of the epoch after.
                "takeover" => {
                    bucket.served(crate::head::take(&*bucket, taken, 0x77, 0)).0.unwrap();
                }
                _ => {
                    bucket.served(branch::delete(&*bucket, garbage(0x102), 20, 0)).0.unwrap();
                }
            }
            let asked = sweep(&mut collector);
            assert!(asked[&cell].contains(&recovered), "{write}: {:#?}", asked[&cell]);
            let others = |cells: &BTreeMap<Cell, Vec<String>>| {
                let mut cells = cells.clone();
                cells.remove(&cell);
                cells
            };
            assert_eq!(others(&asked), others(&all_idle), "{write}");
            // Until it changes the cell no more: a deletion's pass deletes, the cell dead, and
            // records what its deletes leave, which the one after it finds (TASK-455).
            assert_eq!(sweep(&mut collector), all_idle, "{write}");
        }
        // The fork point the reference names, its cell's pass keeps (TASK-395.10's space).
        let kept = bucket.keys();
        assert!(
            kept.contains(&reference) && kept.contains(&layout::checkpoint(garbage(0x100), at, 1))
        );
    }

    /// TASK-395.6's AC #1: what a pass deferred to a grace period, compacted objects whose moves
    /// are too recent, a pass skipping the cell deletes once due, asking nothing more than its
    /// lease and listing, its taking of the lease, its listing serving it, the lease came next
    /// (TASK-455), and the lease again, to fence its deletes and record them; the pass after
    /// goes over the cell in full, as what goes once those are gone none worked out; then passes
    /// skip it again.
    #[test]
    fn passes_skipping_delete_what_was_deferred_once_due() {
        let bucket = SimBucket::new();
        let (segments, _) = bucket.served(mostly_freed(bucket.clone()));
        let cell = test_branch(1).cell;
        let options = Options { compact_below: 0.5, grace: 1000 };
        let mut collector = Collector::new(options);
        let mut holder = 0;
        let mut pass = |collector: &mut Collector| {
            holder += 1;
            let (report, asked) = bucket.served(collector.collect_cell(&*bucket, cell, holder));
            (report.unwrap(), asked)
        };
        let skipped = Report { skipped: 1, ..Report::default() };
        let (report, _) = pass(&mut collector);
        assert_eq!(report.compacted, SEGMENTS as usize);
        // In full, the cell's listing changed by the moves: the segments deferred.
        let (report, asked) = pass(&mut collector);
        assert_eq!(report, Report::default());
        assert!(asked.len() > idle(cell).len(), "{asked:#?}");
        let (report, asked) = pass(&mut collector);
        assert_eq!((report, asked), (skipped, idle(cell)));
        bucket.advance(999);
        let (report, asked) = pass(&mut collector);
        assert_eq!((report, asked), (skipped, idle(cell)));

        bucket.advance(1);
        let (report, asked) = pass(&mut collector);
        assert_eq!(report, Report { deleted: SEGMENTS as usize, skipped: 1, ..report });
        let lease = layout::collector_lease(cell);
        let mut expected = idle(cell);
        expected.extend([format!("put {lease}"), format!("get {lease}")]);
        expected.extend(segments.iter().map(|segment| format!("delete {segment}")));
        expected.extend([format!("get {lease}"), format!("put {lease}")]);
        let mut sorted = asked.clone();
        sorted[4..4 + SEGMENTS as usize].sort();
        assert_eq!(sorted, expected);
        let left = bucket.keys();
        assert_eq!(segments.iter().filter(|segment| left.contains(segment)).count(), 0);
        let (report, asked) = pass(&mut collector);
        assert_eq!(report, Report::default());
        assert!(asked.len() > idle(cell).len(), "{asked:#?}");
        let (report, asked) = pass(&mut collector);
        assert_eq!((report, asked), (skipped, idle(cell)));
    }

    /// TASK-395.6: a pass skips nothing it went by more than the cell for. A clone being made in the
    /// cell, of a point in another, its pass reads the point's record for: deleted then, with
    /// nothing of the cell's changed, the next pass gives the clone up, as a pass in full does. The
    /// cell dead, the pass in full retires the clone's reference, marking it, and passes that skip
    /// the cell after ask its lease and listing alone (TASK-395.14): the reference written again,
    /// by a writer whose read came before, the pass over the parent's cell deletes, by its mark.
    /// And passes of other options, or that only mark, go over the cell in full.
    #[test]
    fn passes_skip_only_what_the_cell_alone_decides() {
        use crate::layout::Origin;
        let bucket = SimBucket::new();
        let (origin_branch, point, clone) = (celled(1, 1), celled(2, 2), celled(3, 3));
        let at = bucket
            .served(async {
                branch::create(&*bucket, origin_branch, 0).await.unwrap();
                let writer = Writer::open(bucket.clone(), origin_branch, 10, 0).await.unwrap();
                let extent = ExtentId::new(1, writer.chain().next_sequence());
                let made = Child { branch: point, owner: 0x22, point: true };
                commit_and_fork(&writer, vec![(extent, vec![1; 64])], made).await
            })
            .0;
        let origin = Origin { parent: origin_branch, at };
        // The clone's record, being made: its host died before it was.
        let forking = crate::record::Kind::Forking { origin, point }.of(0x33);
        let reference = layout::reference(origin_branch, at, clone);
        bucket.served(async {
            let bytes = layout::encode(&layout::Reference::of(0x33));
            bucket::create(&*bucket, &reference, bytes).await;
            bucket::create(&*bucket, &layout::record(clone), layout::encode(&forking)).await;
        });
        let options = Options { compact_below: 0.0, grace: 0 };
        let mut collector = Collector::new(options);
        let pass = |collector: &mut Collector, holder| {
            let (report, asked) =
                bucket.served(collector.collect_cell(&*bucket, clone.cell, holder));
            (report.unwrap(), asked)
        };
        for holder in 1..=2 {
            let (_, asked) = pass(&mut collector, holder);
            let read = format!("get {}", layout::record(point));
            assert!(asked.contains(&read), "{holder}: {asked:#?}");
        }
        bucket.served(branch::delete(&*bucket, point, 20, 0)).0.unwrap();
        pass(&mut collector, 3);
        let record = bucket.served(branch::record(&*bucket, clone)).0.unwrap().unwrap();
        assert!(record.deleted, "{record:?}");

        // Settled: the clone given up on, the cell dead, a pass in full retires the clone's
        // reference, marked; the passes after skip the cell, asking nothing more of it.
        let (report, _) = pass(&mut collector, 4);
        assert_eq!(report.retired, 1);
        let mark = layout::retired(origin_branch, at, clone);
        assert!(!bucket.keys().contains(&reference));
        assert!(bucket.keys().contains(&mark));
        let skipped = Report { skipped: 1, ..Report::default() };
        let (report, asked) = pass(&mut collector, 5);
        assert_eq!((report, asked), (skipped, idle(clone.cell)));
        // Written again, as a writer whose read came before writes it (the simulation's restores,
        // seed 53): the dead cell's passes skip it still; its parent's cell's pass deletes it.
        bucket.served(async {
            let bytes = layout::encode(&layout::Reference::of(0x33));
            bucket::create(&*bucket, &reference, bytes).await;
        });
        let (report, asked) = pass(&mut collector, 6);
        assert_eq!((report, asked), (skipped, idle(clone.cell)));
        assert!(bucket.keys().contains(&reference));
        let (report, _) = bucket.served(collector.collect_cell(&*bucket, origin_branch.cell, 9));
        assert_eq!(report.unwrap().retired, 1);
        assert!(!bucket.keys().contains(&reference));
        assert!(bucket.keys().contains(&mark));
        let mut other = Collector::new(Options { compact_below: 0.5, grace: 0 });
        let (report, asked) = bucket.served(other.collect_cell(&*bucket, clone.cell, 7));
        report.unwrap();
        assert!(asked.len() > idle(clone.cell).len() + 2, "{asked:#?}");
        let (marking, asked) = bucket.served(collector.mark_cell(&*bucket, clone.cell, 8));
        marking.unwrap();
        assert!(asked.len() > idle(clone.cell).len(), "{asked:#?}");
    }

    /// The usage `cell`'s collector lease records, and what a listing of the cell finds now, of
    /// each kind.
    async fn usages(
        bucket: &impl Bucket,
        cell: Cell,
    ) -> (passed::Usage, BTreeMap<String, passed::Sum>) {
        let lease = passed::read(bucket, cell).await.unwrap().unwrap();
        let mut listed: BTreeMap<String, passed::Sum> = BTreeMap::new();
        for meta in bucket::list(bucket, &cell.prefix()).await {
            let sum = listed.entry(layout::kind(&meta.key).name().to_owned()).or_default();
            *sum += passed::Sum { objects: 1, bytes: meta.size };
        }
        (lease.passed.unwrap().usage, listed)
    }

    /// TASK-395.8's AC #1 and #3: each cell's pass records the cell's objects and bytes, by kind,
    /// with when it listed them, which match a listing of the cell after it: after writes, after
    /// compacting and deleting what was compacted, and after a branch's deletion, its cell
    /// reclaimed and the reference it held of its parent's cell retired.
    #[test]
    fn passes_record_what_their_cells_take() {
        let sim = SimBucket::new();
        block_on(async {
            let bucket = Arc::new(Direct(sim.clone()));
            let segments = mostly_freed(bucket.clone()).await;
            let compacting = test_branch(1).cell;
            let (parent, child) = (celled(5, 7), celled(2, 2));
            branch::create(&*bucket, parent, 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), parent, 10, 0).await.unwrap();
            let extent = ExtentId::new(7, writer.chain().next_sequence());
            commit_and_fork(&writer, vec![(extent, vec![1; 64])], forked(child)).await;
            let forking = Writer::open(bucket.clone(), child, 11, 0).await.unwrap();
            let own = ExtentId::new(2, forking.chain().next_sequence());
            let extents = vec![(own, vec![2; 64])];
            commit(&forking, Commit { tag: 3, extents, ..Commit::default() }).await;
            forking.checkpoint().await.unwrap();
            let cells = [compacting, parent.cell, child.cell];

            // Each cell's usage as its listing, measured since `before` if `changed` holds it, and
            // before that otherwise: passes since skipped it.
            let check = |step: &'static str, before: u64, changed: &'static [usize]| {
                let bucket = bucket.clone();
                async move {
                    for (index, cell) in cells.into_iter().enumerate() {
                        let (usage, listed) = usages(&*bucket, cell).await;
                        assert_eq!(usage.kinds, listed, "{step}: cell {cell}");
                        let (measured, now) = (usage.measured, bucket.now());
                        match changed.contains(&index) {
                            true => {
                                assert!(before <= measured && measured <= now, "{step} {cell}")
                            }
                            false => assert!(measured < before, "{step}: cell {cell}"),
                        }
                    }
                }
            };
            let options = Options { compact_below: 0.5, grace: 0 };
            let before = bucket.now();
            let report = crate::testing::collect_until_quiet(&*bucket, &options).await.unwrap();
            assert_eq!(report.compacted, SEGMENTS as usize);
            check("written and compacted", before, &[0, 1, 2]).await;
            let left = keys(&*bucket).await;
            assert_eq!(segments.iter().filter(|segment| left.contains(segment)).count(), 0);
            let (usage, _) = usages(&*bucket, compacting).await;
            assert_eq!(usage.kinds["moves"].objects, SEGMENTS);

            drop(forking);
            // The bucket's clock past every measurement before.
            sim.advance(1);
            let before = bucket.now();
            branch::delete(&*bucket, child, 21, 0).await.unwrap();
            // Retired by the pass over the dead cell in full; not by the ones after, which skip it
            // (TASK-395.14), as that one recorded what its deletes left (TASK-455).
            let report = crate::testing::collect_until_quiet(&*bucket, &options).await.unwrap();
            assert_eq!(report.retired, 1);
            check("deleted", before, &[1, 2]).await;
            let (usage, _) = usages(&*bucket, parent.cell).await;
            assert!(!usage.kinds.contains_key("reference"), "{usage:?}");
            let (usage, _) = usages(&*bucket, child.cell).await;
            assert!(!usage.kinds.contains_key("segment"), "{usage:?}");
        });
    }

    /// TASK-411.6: what the tests of compacting encrypted cells share: a bucket; a KMS holding the
    /// KEK of [`layout::TEST_CELL`]'s tenant, counting its calls; and a writer of branch 1 of the
    /// cell, encrypted, its record's DEK wrapped under the cell's KEK, holding the ring its store
    /// holds.
    struct Encrypted {
        bucket: Arc<ObjectBucket>,
        counted: Arc<Counted<LocalKms>>,
        kms: kms::Shared,
        writer: Writer<ObjectBucket>,
        ring: Arc<Ring>,
        /// The cell's keys, as its maker holds them.
        cell: CellKeys,
        _dir: tempfile::TempDir,
    }

    /// The segments [`Encrypted::mostly_freed`] writes.
    const ENCRYPTED_SEGMENTS: u64 = 3;

    impl Encrypted {
        /// The cell made by a host with a KEK it wrapped, its branch 1 given a DEK by its writer,
        /// as `extent_store` makes them; the KMS's calls since counted from none.
        async fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let local = LocalKms::open(dir.path().join("kms")).unwrap();
            local.create(layout::TEST_CELL.tenant).unwrap();
            let counted = Arc::new(Counted::new(local));
            let kms: kms::Shared = counted.clone();
            let host = keys::CachingKms::new(kms.clone());
            let bucket = store();
            let kek = keys::new_cell_kek(Some(&host), layout::TEST_CELL.tenant).await.unwrap();
            let making = (true, kek.as_ref().map(|(_, kek)| kek));
            let vdisk = crate::cell::Vdisk { branch: 1, kind: crate::cell::Kind::Fxfs };
            let (_, object) =
                crate::cell::hold(&*bucket, layout::TEST_CELL, (5, 5), making, Some(("a", vdisk)))
                    .await
                    .unwrap();
            assert_eq!(object.keks.len(), 1);
            let cell = keys::held(Some(&host), layout::TEST_CELL, object, kek);
            let branch = test_branch(1);
            branch::create(&*bucket, branch, 0).await.unwrap();
            let record = branch::record(&*bucket, branch).await.unwrap().unwrap();
            let record = keys::with_dek(&*bucket, &cell, branch, record).await.unwrap();
            let ring = keys::ring(&*bucket, &cell, branch, &record, true).await.unwrap();
            let writer =
                Writer::open_keyed(bucket.clone(), branch, (10, 0), ring.clone()).await.unwrap();
            counted.reset();
            Self { bucket, counted, kms, writer, ring, cell, _dir: dir }
        }

        /// The KMS's wraps and unwraps, since [`Encrypted::new`] or the last reset.
        fn calls(&self) -> (u64, u64) {
            (self.counted.wraps(), self.counted.unwraps())
        }

        /// A collector of `options` unwrapping through the KMS.
        fn collector(&self, options: Options) -> Collector {
            Collector::new(options).kms(Some(self.kms.clone()))
        }

        /// `commit`, in a segment of its own encrypted under the branch's DEK, uploaded and
        /// acknowledged.
        async fn commit(&self, commit: Commit) {
            let objects = SegmentBuilder::from(commit).close(&mut self.writer.chain());
            let segment =
                self.writer.add_segment(objects.frame_under(self.ring.encrypting())).unwrap();
            self.writer.upload(segment).await.unwrap();
            self.writer.acknowledge().await.unwrap();
        }

        /// [`ENCRYPTED_SEGMENTS`] segments, each of an extent kept, of [`kept`]'s data, and a
        /// larger one freed after, then a checkpoint past them: what a pass compacting below a half
        /// compacts.
        /// The extents kept, each with its segment's key.
        async fn mostly_freed(&self) -> Vec<(ExtentId, String)> {
            let (mut kept_at, mut frees) = (Vec::new(), Vec::new());
            for n in 0..ENCRYPTED_SEGMENTS {
                let first = self.writer.chain().next_sequence();
                let (small, large) = (ExtentId::new(1, first), ExtentId::new(1, first + 1));
                let extents = vec![(small, kept(n)), (large, vec![b'f'; 20_000])];
                self.commit(Commit { tag: n + 1, extents, ..Commit::default() }).await;
                kept_at.push((small, self.writer.location(small).unwrap().key));
                frees.push(large);
            }
            self.commit(Commit { tag: ENCRYPTED_SEGMENTS + 1, frees, ..Commit::default() }).await;
            self.writer.checkpoint().await.unwrap();
            kept_at
        }

        /// A commit more, of an extent of its own, and a checkpoint of it: where the writer names
        /// the extents the collector moved from then on.
        async fn checkpointed(&self) {
            let extent = ExtentId::new(1, self.writer.chain().next_sequence());
            let extents = vec![(extent, b"more".to_vec())];
            self.commit(Commit { tag: ENCRYPTED_SEGMENTS + 2, extents, ..Commit::default() }).await;
            self.writer.checkpoint().await.unwrap();
        }
    }

    /// The data of the extent kept in segment `n` of [`Encrypted::mostly_freed`]'s: of no bytes
    /// repeated, so that a window of it is found nowhere else.
    fn kept(n: u64) -> Vec<u8> {
        let mut state = (n + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        (0..4096)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    /// `extent`'s data at `location`, read by a host's fetcher holding `ring`, as a host serving
    /// the branch's store holds its keys; none, if `ring` is clear.
    async fn read_holding(
        bucket: &Arc<ObjectBucket>,
        ring: &Arc<Ring>,
        extent: ExtentId,
        location: &Location,
    ) -> Result<Vec<u8>, Error> {
        let memory = crate::fetch::Memory::new();
        let _held = memory.keys().hold(ring.clone());
        let budget = crate::fetch::Budget::new(crate::fetch::Limits::UNBOUNDED);
        let fetcher = crate::fetch::Fetcher::new(bucket.clone(), budget, memory.clone(), None);
        let demand = crate::fetch::Priority::Demand;
        fetcher.read(extent, location, 0, location.len, demand).await
    }

    /// The encryption of the object at `key`, as its trailer says.
    async fn encryption_at(bucket: &impl Bucket, key: &str) -> Encryption {
        let bytes = bucket::get(bucket, key).await.unwrap().bytes;
        Frame::of(&bytes).unwrap().encryption
    }

    /// TASK-411.6, AC #1, #2: a pass compacting an encrypted cell's objects mostly freed unwraps
    /// the cell's KEK through the KMS once for them all, and encrypts each one's moves under the
    /// DEK its segment was encrypted under, of the same generation, with a salt of their own,
    /// holding none of their data in the clear. A reader holding the branch's keys reads the
    /// extents moved, from where their writer named them, their segments gone, and from the moves
    /// its next checkpoint names; one holding none reads none of them. Passes that compact nothing
    /// (none below their fraction, or nothing left to compact) and dry runs ask the KMS nothing.
    #[test]
    fn encrypted_cells_are_compacted_under_their_keys_unwrapped_once_a_pass() {
        block_on(async {
            let encrypted = Encrypted::new().await;
            let kept_at = encrypted.mostly_freed().await;
            let dek = KeyId { branch: 1, generation: keys::FIRST };
            let compacting = Options { compact_below: 0.5, grace: 0 };

            // Marking alone, compacting nothing, and a dry run: no call.
            let report = encrypted.collector(KEEP).collect(&*encrypted.bucket, 1).await.unwrap();
            assert_eq!((report.compacted, report.locked), (0, 0));
            let dry = encrypted.collector(compacting);
            let dry = dry.dry_run_cell(&*encrypted.bucket, layout::TEST_CELL).await.unwrap();
            assert_eq!(dry.compactable.len(), ENCRYPTED_SEGMENTS as usize);
            assert_eq!(encrypted.calls(), (0, 0));

            // Compacting them: one unwrap for the three.
            let mut collector = encrypted.collector(compacting);
            let report = collector.collect(&*encrypted.bucket, 2).await.unwrap();
            assert_eq!(
                (report.compacted, report.locked, report.uncompacted),
                (ENCRYPTED_SEGMENTS as usize, 0, 0)
            );
            assert_eq!(encrypted.calls(), (0, 1));
            let objects = held(&*encrypted.bucket).await;
            for (n, (_, segment)) in kept_at.iter().enumerate() {
                let moves = moves_at(&*encrypted.bucket, segment).await.unwrap();
                let (
                    Encryption::Encrypted { key, salt },
                    Encryption::Encrypted { key: of, salt: of_salt },
                ) = (
                    encryption_at(&*encrypted.bucket, &moves).await,
                    encryption_at(&*encrypted.bucket, segment).await,
                )
                else {
                    panic!("{moves} or {segment} not encrypted");
                };
                assert_eq!((key, of), (dek, dek));
                assert_ne!(salt, of_salt);
                let data = kept(n as u64);
                for (meta, bytes) in &objects {
                    let found = bytes.windows(64).any(|window| window == &data[..64]);
                    assert!(!found, "{} holds extent {n}'s data in the clear", meta.key);
                }
            }

            // The next deletes the segments compacted, compacting nothing: no call more.
            let report = collector.collect(&*encrypted.bucket, 3).await.unwrap();
            assert_eq!((report.compacted, report.locked), (0, 0));
            let left = keys(&*encrypted.bucket).await;
            assert!(kept_at.iter().all(|(_, segment)| !left.contains(segment)), "{left:?}");
            assert_eq!(encrypted.calls(), (0, 1));

            // Read where their writer named them, followed to their moves; then where its next
            // checkpoint names them.
            for (n, (extent, segment)) in kept_at.iter().enumerate() {
                let at = encrypted.writer.location(*extent).unwrap();
                assert_eq!((at.key.as_str(), at.len), (segment.as_str(), 4096));
                let read = read_holding(&encrypted.bucket, &encrypted.ring, *extent, &at).await;
                assert_eq!(read, Ok(kept(n as u64)));
                let keyless = read_holding(&encrypted.bucket, &Ring::clear(), *extent, &at).await;
                assert_eq!(keyless, Err(Error::NoKey(dek)));
            }
            encrypted.checkpointed().await;
            for (n, (extent, segment)) in kept_at.iter().enumerate() {
                let at = encrypted.writer.location(*extent).unwrap();
                assert_eq!(Some(at.key.clone()), moves_at(&*encrypted.bucket, segment).await);
                let read = read_holding(&encrypted.bucket, &encrypted.ring, *extent, &at).await;
                assert_eq!(read, Ok(kept(n as u64)));
            }
            assert_eq!(encrypted.calls(), (0, 1));
        });
    }

    /// TASK-411.6: an object encrypted under a DEK's generation older than its record's newest is
    /// compacted to moves encrypted under its own: a store opened before the newer was added, which
    /// holds none of it, reads the extents moved.
    #[test]
    fn moves_are_encrypted_under_the_generation_their_objects_were() {
        block_on(async {
            let encrypted = Encrypted::new().await;
            let kept_at = encrypted.mostly_freed().await;
            let branch = test_branch(1);
            let added =
                keys::add_generation(&*encrypted.bucket, &encrypted.cell, branch).await.unwrap();
            assert_eq!(added, keys::FIRST + 1);
            assert_eq!(encrypted.ring.generations(1), [keys::FIRST]);
            let mut collector = encrypted.collector(Options { compact_below: 0.5, grace: 0 });
            let report = collector.collect(&*encrypted.bucket, 1).await.unwrap();
            assert_eq!(report.compacted, ENCRYPTED_SEGMENTS as usize);
            collector.collect(&*encrypted.bucket, 2).await.unwrap();
            for (n, (extent, segment)) in kept_at.iter().enumerate() {
                let moves = moves_at(&*encrypted.bucket, segment).await.unwrap();
                let encryption = encryption_at(&*encrypted.bucket, &moves).await;
                let first = KeyId { branch: 1, generation: keys::FIRST };
                assert!(
                    matches!(encryption, Encryption::Encrypted { key, .. } if key == first),
                    "{encryption:?}"
                );
                let at = encrypted.writer.location(*extent).unwrap();
                let read = read_holding(&encrypted.bucket, &encrypted.ring, *extent, &at).await;
                assert_eq!(read, Ok(kept(n as u64)));
            }
        });
    }

    /// TASK-411.6, AC #1's twin: a cell in the clear is compacted as before, its moves in the
    /// clear, by a collector holding a KMS it asks nothing.
    #[test]
    fn cells_in_the_clear_are_compacted_asking_the_kms_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let counted = Arc::new(Counted::new(LocalKms::open(dir.path().join("kms")).unwrap()));
        let kms: kms::Shared = counted.clone();
        let bucket = SimBucket::new();
        let (segments, _) = bucket.served(mostly_freed(bucket.clone()));
        let mut collector = Collector::new(Options { compact_below: 0.5, grace: 0 }).kms(Some(kms));
        let (report, _) = bucket.served(collector.collect(&*bucket, 1));
        let report = report.unwrap();
        assert_eq!((report.compacted, report.locked), (SEGMENTS as usize, 0));
        assert_eq!((counted.wraps(), counted.unwraps()), (0, 0));
        for segment in &segments {
            let moves = bucket.served(moves_at(&*bucket, segment)).0.unwrap();
            let (encryption, _) = bucket.served(encryption_at(&*bucket, &moves));
            assert_eq!(encryption, Encryption::None, "{moves}");
        }
    }

    /// TASK-411.6, AC #3: a pass over a cell whose tenant's KEK is revoked marks it and deletes
    /// what no live branch needs, but compacts none of what is encrypted, saying so in its report,
    /// as one of a collector with no KMS does, which asks none. TASK-411.13, AC #1, #2: it records
    /// its listing's digest and what it left, so that a pass after it, the cell unchanged, skips
    /// it, asking its lease and a listing, and the KMS one wrap, refused, while the KEK is still
    /// not to be had (no call at all without a KMS), saying what is left; a write to the cell has
    /// the next go over it in full; and the KEK given back, the next compacts.
    #[test]
    fn cells_of_revoked_tenants_are_marked_and_swept_but_not_compacted() {
        block_on(async {
            let encrypted = Encrypted::new().await;
            let kept_at = encrypted.mostly_freed().await;
            let compacting = Options { compact_below: 0.5, grace: 0 };
            let locked = |report: Report| (report.compacted, report.locked, report.uncompacted);
            let segments = ENCRYPTED_SEGMENTS as usize;
            // A pass's report, and the requests it made, by kind.
            let bucket = &encrypted.bucket;
            let pass = |collector: Collector, holder| async move {
                let mut collector = collector;
                let before = bucket.stats().requests;
                let report = collector.collect_cell(&**bucket, layout::TEST_CELL, holder);
                let report = report.await.unwrap();
                let after = bucket.stats().requests;
                let made = (Request::ALL.into_iter())
                    .filter_map(|request| {
                        let made = after[request as usize] - before[request as usize];
                        (made > 0).then_some((request, made))
                    })
                    .collect::<Vec<_>>();
                (report, made)
            };
            // Its lease read, its listing: a look, writing nothing (TASK-455).
            let idle = vec![(Request::Get, 1), (Request::List, 1)];

            // No KMS: no call, the encrypted left.
            let before = keys(&*encrypted.bucket).await;
            let (report, _) = pass(Collector::new(compacting), 1).await;
            assert_eq!(locked(report), (0, 1, segments));
            assert_eq!(encrypted.calls(), (0, 0));
            let after = keys(&*encrypted.bucket).await;
            assert_eq!(report.deleted, before.len() - after.len());
            assert_eq!(report.deleted, 1);
            assert!(after.iter().all(|key| !matches!(layout::kind(key), Kind::Moves { .. })));
            // The next in full, the cell changed by what went; then skipped, asking nothing more.
            let (report, made) = pass(Collector::new(compacting), 2).await;
            assert_eq!((locked(report), report.skipped, report.deleted), ((0, 1, segments), 0, 0));
            assert!(made.len() > idle.len(), "{made:?}");
            let (report, made) = pass(Collector::new(compacting), 3).await;
            let skipped =
                Report { skipped: 1, locked: 1, uncompacted: segments, ..Report::default() };
            assert_eq!((report, made), (skipped, idle.clone()));
            assert_eq!(encrypted.calls(), (0, 0));

            // Revoked: skipped still, each pass asking the KMS one wrap, refused.
            encrypted.counted.inner().revoke(layout::TEST_CELL.tenant).unwrap();
            for holder in [4, 5] {
                let (report, made) = pass(encrypted.collector(compacting), holder).await;
                assert_eq!((report, made), (skipped, idle.clone()));
            }
            assert_eq!(encrypted.calls(), (2, 0));
            assert_eq!(keys(&*encrypted.bucket).await, after);
            // Written to: in full, its unwrap refused, left again, deleting what the write left
            // behind; the next in full too, the cell changed by that; then skipped again.
            encrypted.checkpointed().await;
            let full = |report: Report| (locked(report), report.skipped, report.deleted);
            let (report, made) = pass(encrypted.collector(compacting), 6).await;
            assert_eq!(full(report), ((0, 1, segments), 0, 1));
            assert!(made.len() > idle.len(), "{made:?}");
            let (report, _) = pass(encrypted.collector(compacting), 7).await;
            assert_eq!(full(report), ((0, 1, segments), 0, 0));
            assert_eq!(encrypted.calls(), (2, 2));
            let (report, made) = pass(encrypted.collector(compacting), 8).await;
            assert_eq!((report, made), (skipped, idle.clone()));
            assert_eq!(encrypted.calls(), (3, 2));

            // Given back: the KMS wraps, the cell gone over in full, compacted.
            encrypted.counted.inner().unrevoke(layout::TEST_CELL.tenant).unwrap();
            let (report, _) = pass(encrypted.collector(compacting), 9).await;
            assert_eq!(full(report), ((segments, 0, 0), 0, 0));
            assert_eq!(encrypted.calls(), (4, 3));
            for (n, (extent, _)) in kept_at.iter().enumerate() {
                let at = encrypted.writer.location(*extent).unwrap();
                let read = read_holding(&encrypted.bucket, &encrypted.ring, *extent, &at).await;
                assert_eq!(read, Ok(kept(n as u64)));
            }
            // Compacted, the next deletes what was, recording nothing left; then skipped, asking
            // the KMS nothing.
            let (report, _) = pass(encrypted.collector(compacting), 10).await;
            assert_eq!(full(report), ((0, 0, 0), 0, segments));
            let (report, _) = pass(encrypted.collector(compacting), 11).await;
            assert_eq!(full(report), ((0, 0, 0), 0, 0));
            let (report, made) = pass(encrypted.collector(compacting), 12).await;
            assert_eq!((report, made), (Report { skipped: 1, ..Report::default() }, idle));
            assert_eq!(encrypted.calls(), (4, 3));
        });
    }

    /// TASK-411.13: what a sweep's passes heard of a tenant's KEK, asked of the KMS once however
    /// many ask, each tenant apart; none without a KMS.
    #[test]
    fn tenants_keks_are_asked_for_once_a_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let local = LocalKms::open(dir.path().join("kms")).unwrap();
        local.create(1).unwrap();
        local.create(2).unwrap();
        local.revoke(2).unwrap();
        let counted = Arc::new(Counted::new(local));
        let kms: kms::Shared = counted.clone();
        let heard = Heard::new();
        let asked = |tenant| block_on(heard.to_be_had(Some(&kms), tenant));
        assert_eq!(
            [asked(1), asked(1), asked(2), asked(2), asked(3)],
            [true, true, false, false, false]
        );
        assert_eq!((counted.wraps(), counted.unwraps()), (3, 0));
        assert!(!block_on(Heard::new().to_be_had(None, 1)));
        assert_eq!(counted.wraps(), 3);
    }

    /// TASK-411.6, AC #4: the moves of an encrypted object, copied whole to the key of another's
    /// moves of its branch, are refused there, by readers holding the branch's keys and by those
    /// holding none: their footer, which its MAC covers, names the object they are the moves of
    /// (TASK-411.3). Their footer made to name the other, framed anew in the clear, which needs no
    /// key, is a downgrade, refused by readers holding the keys.
    #[test]
    fn encrypted_moves_copied_to_another_key_of_their_branch_are_refused() {
        block_on(async {
            let encrypted = Encrypted::new().await;
            let kept_at = encrypted.mostly_freed().await;
            let mut collector = encrypted.collector(Options { compact_below: 0.5, grace: 0 });
            collector.collect(&*encrypted.bucket, 1).await.unwrap();
            collector.collect(&*encrypted.bucket, 2).await.unwrap();
            encrypted.checkpointed().await;
            let [(_, of_one), (other, of_other), ..] = &kept_at[..] else { panic!() };
            let moves_one = moves_at(&*encrypted.bucket, of_one).await.unwrap();
            let moves_other = moves_at(&*encrypted.bucket, of_other).await.unwrap();
            let at = encrypted.writer.location(*other).unwrap();
            assert_eq!(at.key, moves_other);
            let read = read_holding(&encrypted.bucket, &encrypted.ring, *other, &at).await;
            assert_eq!(read, Ok(kept(1)));

            let copied = bucket::get(&*encrypted.bucket, &moves_one).await.unwrap().bytes;
            let put = encrypted.bucket.put(&moves_other, copied.clone().into(), PutMode::Overwrite);
            put.await.unwrap();
            let refused = Error::Corrupt(format!("{moves_other} holds the moves of {of_one}"));
            for ring in [encrypted.ring.clone(), Ring::clear()] {
                let read = read_holding(&encrypted.bucket, &ring, *other, &at).await;
                assert_eq!(read, Err(refused.clone()));
            }

            // Made to name the other and its extent, framed in the clear.
            let view = keys::of_key(&encrypted.ring, &moves_one);
            let data = layout::data_under(&copied, &view).unwrap();
            let mut footer: Moves = layout::footer_under(&copied, &view).unwrap();
            assert_eq!(footer.of, *of_one);
            footer.of = of_other.clone();
            for (extent, to) in &mut footer.extents {
                (*extent, to.key) = (*other, moves_other.clone());
            }
            let forged = layout::with_footer(&data, &footer);
            encrypted.bucket.put(&moves_other, forged.into(), PutMode::Overwrite).await.unwrap();
            let read = read_holding(&encrypted.bucket, &encrypted.ring, *other, &at).await;
            assert_eq!(read, Err(Error::Downgrade(moves_other.clone())));
        });
    }
}
