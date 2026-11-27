// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The collector (`cladefs-gc`): the only thing that deletes.
//!
//! cladefs: it collects a cell at a time (doc-7, Garbage collection; TASK-395): a pass over a cell
//! takes the cell's collector lease, lists the cell's prefix, and reads and deletes what is there
//! alone, beside passes over other cells, which it neither fences nor waits for. Everything it
//! decides by is under the cell's prefix: its cells' objects, its lease, and the references of
//! forks of its cells in other cells ([`layout::reference`]). What its cells inherited from other
//! cells it reads there, and never deletes: those cells keep it, by the references of the forks
//! that inherited it. A sweep over every cell is a pass over each: one after another
//! ([`Collector::collect`]), or many at once, by processes any number of them ([`crate::sweep`]);
//! the cells are found by delimiter listings, a page at a time, listing none of their objects
//! ([`crate::cell::listed`]).
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
//! cells need. An object written after the listing is no candidate, which is what makes reading
//! the cells' states after it safe: what a state names was either listed, and is seen as
//! needed, or not, and is left.
//!
//! The live cells are those whose records are not deleted, and the children of forks in live
//! cells' states, of the cell, which get their records only once their forks are acknowledged.
//! What they need is what recovering them reads, the objects holding the extents their states
//! name, and each one's segments past its end, which may
//! be uploaded and not yet in it. Segments go only once a checkpoint covers them, so that a reader
//! that finds one missing knows, from the checkpoints, whether it is the cell's end.
//!
//! cladefs: a fork of the cell to a child in another cell is kept by the origin: the
//! parent's whole state at the end of the segment it was forked at, the objects holding the
//! extents it names, and what recovering it reads
//! (TASK-395.4). It is kept while a reference names it, whatever the child frees of it since: the
//! collector is conservative, reclaiming late rather than reading other cells to find what their
//! cells still hold. An origin shared by many references, a version cloned many times, is
//! recovered once. The references are listed after the states, by their origins' prefixes
//! ([`referenced`]). Forks in the live
//! cells' states to other cells keep their origins too, as a fork's child may lack its
//! reference yet. A cell with no live cell and no reference is reclaimed whole, but its records
//! and its object; and its pass retires the references of its deleted children under their
//! parents' cells, computing them from the records' origins, never listing them: the one delete a
//! pass makes outside its cell. cladefs: its records and object go too, with its head and lease,
//! if every record of it is a deleted root's or clone's made, of an ID an allocator gave, which
//! no recorder makes again; and a clone's reference retires unmarked ([`Pass::removal`],
//! TASK-455).
//!
//! A fork of a version is its child's record alone (doc-20): its child names what the version
//! names, and a pass that read the version dropped lists the records after that, and so finds
//! every child whose fork can succeed ([`lifecycle::fork_version`]). A child still being made of a
//! version dropped the pass gives up on, first ([`abandon`]): nothing relies on it, and it may name
//! what is gone, which would stop this pass and every other.
//!
//! A state read names no fork deleted before it, a writer's checkpoint dropping those, and the
//! fork's record and its children's may have come after the pass listed the records. So it lists
//! them again after it read the states, and recovers what it finds, again while a state it read
//! may have dropped a fork: while it recovers a cell with a checkpoint of its own. A record made
//! after that names what the pass kept, or what it never listed: what a fork after a state read
//! inherited, that state holds, or a later object; and a fork before it that state names.
//!
//! cladefs: it compacts nothing, and holds no key (doc-27): writers relocate out of their cells'
//! sparse segments themselves, in commits of their own, and what a pass keeps and deletes it
//! decides by metadata alone, authenticated, not encrypted.
//!
//! A pass reads records and recovers cells many at once, and deletes objects in batches where the
//! backend deletes so (TASK-395.5); what forks inherited together, their parent's state where they
//! were forked, it reads once and takes once for them all; and a pass reads a record again only
//! once it changed ([`Collector`], TASK-92.7).
//!
//! cladefs: a pass records in its cell's lease, as it ends, what the next pass goes by
//! ([`crate::passed`], TASK-395.6, TASK-395.8): the digest of its listing of the cell, and what the
//! cell takes, by kind. A pass whose listing is the one recorded decides what the recorded pass
//! did, everything it decides by being under the cell's prefix: it recovers nothing, and deletes
//! nothing ([`Collector::collect_cell`]). An idle cell costs a pass its lease's read and one
//! listing, a look that writes nothing (TASK-455).
//!
//! cladefs: a cell holding the moves the collector made in formats before 28 is refused
//! ([`Error::Format`], [`Kind::Former`]): nothing of this format reads them.
//!
//! cladefs: a dry run deletes nothing (TASK-264): it ([`Collector::dry_run_cell`]) writes nothing
//! at all, the lease neither, and says what a pass would delete, and what references it would
//! retire. The mark-only passes that wrote what they would delete to the bucket, which every
//! reader of a cell watched, went pre-production (board 10545): a dry run's logs say as much,
//! touching neither the bucket nor its readers' path.

use crate::bucket::{self, Bucket, Meta};
use crate::gc_metrics::{CellSample, ObjectSample, Observer};
use crate::layout::Head;
use crate::layout::{self, CellId, Kind, Origin};
use crate::passed::{self, CollectorLease, LeaseTaking, Listed, Passed, Usage};
use crate::record::Made;
use crate::recovery::{self, Checkpoints};
use crate::{Error, lifecycle, rules};
use futures::StreamExt as _;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// How many cells a pass recovers at once, and how many origins' states.
#[cfg(not(target_arch = "wasm32"))]
const RECOVERIES_AT_ONCE: usize = 16;
#[cfg(target_arch = "wasm32")]
const RECOVERIES_AT_ONCE: usize = 1;

/// cladefs: how a collector's passes hold their cells (TASK-395.5): the default holds none, its
/// passes taking their cells whatever another holds, but a pass holding it for a time, as the
/// tests' and the simulation's race, the later fencing the earlier.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Holding {
    /// How long a pass holds its cell, in milliseconds by the backend's clock, from when it takes
    /// the lease or last renews it, which it does between its steps; 0: not at all (its lease's
    /// `until` 0), a pass after it taking the cell at once. Long enough for the longest step of a
    /// pass over the largest cell: one that takes longer is taken from, fenced, and deletes
    /// nothing.
    pub lease: u64,
    /// When its sweep began, by the backend's clock, if it is a sweep's: a pass leaves its cell to
    /// another's that ended since ([`crate::Left::Swept`]), so that processes sweeping at once
    /// split the cells rather than go over each other's again.
    pub since: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// The objects of the cell it deleted.
    pub deleted: usize,
    /// cladefs: the references of the cell's deleted children it retired, under their parents'
    /// cells (TASK-395.4), once, whether there or not, as it can't tell without listing other
    /// cells; and those of its own it deleted, marked, written again since their retirements
    /// (TASK-395.13).
    pub retired: usize,
    /// cladefs: the bytes of what it deleted, as listed (TASK-395.5).
    pub deleted_bytes: u64,
    /// cladefs: the passes that skipped their cells, each unchanged since its last pass
    /// (TASK-395.6).
    pub skipped: usize,
    /// cladefs: the cells it removed whole, dead, their objects counted in `deleted` (TASK-455).
    pub removed: usize,
}

impl std::ops::AddAssign for Report {
    fn add_assign(&mut self, other: Self) {
        self.deleted += other.deleted;
        self.retired += other.retired;
        self.deleted_bytes += other.deleted_bytes;
        self.skipped += other.skipped;
        self.removed += other.removed;
    }
}

/// cladefs: what a sweep's passes may delete together (board 8529): so many objects, and so many
/// bytes of them as listed, each bounded or not. A pass takes from it what it is about to delete,
/// all of it or none, before it deletes any of it ([`Collector::budget`]): the objects of its cell
/// and their bytes, its dead cell's head if it removes it, and the references it retires, of no
/// bytes; not its own lease, nor what moves rather than goes: the references a mirror it resumes
/// copies to another bucket before it retires them, and the copies elsewhere of those it retires
/// (`mirror::Here::retire`), counted once as the references they are. Once one take is refused, every take after it is, so that what a sweep
/// deletes stays within its bounds whatever its passes find doomed: a bug, a bucket listing short.
/// A pass refused deletes nothing, as one fenced deletes nothing, and records nothing new: late,
/// never wrong, the next sweep's pass decides anew.
#[derive(Debug)]
pub struct Budget {
    left: std::sync::Mutex<Spending>,
}

/// What a [`Budget`] has left, and the takes it refused.
#[derive(Debug)]
struct Spending {
    objects: Option<u64>,
    bytes: Option<u64>,
    refused: usize,
}

impl Budget {
    /// One of `objects` objects and `bytes` bytes at most, each unbounded if none.
    pub fn new(objects: Option<u64>, bytes: Option<u64>) -> Self {
        Self { left: std::sync::Mutex::new(Spending { objects, bytes, refused: 0 }) }
    }

    /// Takes `objects` objects of `bytes` bytes, if it has them left and refused no take before:
    /// whether it did. Nothing it always gives.
    fn take(&self, objects: u64, bytes: u64) -> bool {
        if objects == 0 && bytes == 0 {
            return true;
        }
        let mut left = self.left.lock().unwrap();
        let fits = |left: Option<u64>, wanted: u64| left.is_none_or(|left| wanted <= left);
        if left.refused == 0 && fits(left.objects, objects) && fits(left.bytes, bytes) {
            left.objects = left.objects.map(|left| left - objects);
            left.bytes = left.bytes.map(|left| left - bytes);
            return true;
        }
        left.refused += 1;
        false
    }

    /// The takes it refused: the passes that left what they would delete to a later sweep.
    pub fn refused(&self) -> usize {
        self.left.lock().unwrap().refused
    }
}

/// cladefs: what a dry run over a cell found ([`Collector::dry_run_cell`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DryRun {
    /// What a pass would delete, as listed, in the order of their keys.
    pub doomed: Vec<Meta>,
    /// The references, under other cells, a pass would retire, which may be gone already.
    pub retired: Vec<String>,
    /// Whether a pass took the cell's collector lease while it ran: what it read may be older than
    /// what that pass, deleting since, read, and what it found is no more than a guess.
    pub fenced: bool,
}

/// One pass of the collector over every cell, one after another, as `holder`, which must differ
/// from every other pass's: of a collector of its own, which remembers nothing of it
/// ([`Collector`]).
#[cfg(any(test, feature = "testing"))]
pub async fn collect(bucket: &impl Bucket, holder: u64) -> Result<Report, Error> {
    Collector::new().collect(bucket, holder).await
}

/// cladefs: [`collect`], of a bucket whose own bucket is `own` (TASK-633.8): its copies of other
/// buckets' cells passed over as such ([`Collector::collect_with_own`]).
#[cfg(any(test, feature = "testing"))]
pub async fn collect_with_own(
    bucket: &impl Bucket,
    own: &impl Bucket,
    holder: u64,
) -> Result<Report, Error> {
    Collector::new().collect_with_own(bucket, Some(own), holder).await
}

/// cladefs: one pass of the collector over `cell`, as `holder`, which must differ from every other
/// pass's over the cell: of a collector of its own, which remembers nothing of it; tests', as
/// [`collect`] is.
#[cfg(any(test, feature = "testing"))]
pub async fn collect_cell(
    bucket: &impl Bucket,
    cell: CellId,
    holder: u64,
) -> Result<Report, Error> {
    Collector::new().collect_cell(bucket, cell, holder).await
}

/// The collector. cladefs: it remembers nothing from one pass to the next (TASK-395.4): what a pass
/// read of its cell goes with it, so that a sweep over millions of cells holds no more than the
/// cells it is passing over. A pass reads a record again only once it changed, as it lists the
/// records again (TASK-92.7). What is worth keeping across passes is under the cell's prefix, in
/// its lease, which any collector process reads (TASK-395.6, [`crate::passed`]).
pub struct Collector {
    /// cladefs: how its passes hold their cells (TASK-395.5).
    holding: Holding,
    /// cladefs: what its passes may delete, shared with the other passes of their sweep, if
    /// bounded (board 8529).
    budget: Option<Arc<Budget>>,
    /// cladefs: tenants known to be shredded (from `events`), whose live cells are ended and whose
    /// read evidence and orphaned ephemeral references are reclaimed.
    shredded: Arc<BTreeSet<u64>>,
    /// Optional streaming metrics observer notified once per completed cell pass (zero extra I/O).
    observer: Option<Arc<dyn Observer>>,
}

impl Default for Collector {
    fn default() -> Self {
        Self {
            holding: Holding::default(),
            budget: None,
            shredded: Arc::new(BTreeSet::new()),
            observer: None,
        }
    }
}

/// cladefs: what a pass over a cell keeps as it reads it, and drops with it.
#[derive(Default)]
struct Remembered {
    /// The heads read, each with the version it was read at, by key.
    heads: BTreeMap<String, (String, Head)>,
}

impl Collector {
    pub fn new() -> Self {
        Self::default()
    }

    /// cladefs: one whose passes hold their cells as `holding` says (TASK-395.5).
    pub fn holding(self, holding: Holding) -> Self {
        Self { holding, ..self }
    }

    /// cladefs: one whose passes take what they delete from `budget`, if given, and delete
    /// nothing past it (board 8529).
    pub fn budget(self, budget: Option<Arc<Budget>>) -> Self {
        Self { budget, ..self }
    }

    /// cladefs: one whose passes treat `shredded` tenants as shredded, ending their live cells and
    /// reclaiming their read evidence (board 2940) and orphaned ephemeral references.
    pub fn shredded(self, shredded: Arc<BTreeSet<u64>>) -> Self {
        Self { shredded, ..self }
    }

    /// Attaches an [`Observer`] notified once per completed cell pass with [`CellSample`] metadata
    /// gathered from the pass's existing listings and reads (zero extra bucket requests).
    pub fn observer(self, observer: Option<Arc<dyn Observer>>) -> Self {
        Self { observer, ..self }
    }

    /// cladefs: whether a pass may delete `objects` objects of `bytes` bytes, as its budget says,
    /// taking them from it if so (board 8529): always, without one.
    fn may_delete(&self, objects: usize, bytes: u64) -> bool {
        self.budget.as_ref().is_none_or(|budget| budget.take(objects as u64, bytes))
    }

    /// cladefs: a pass over every cell of `bucket`, one after another, as `holder`, which must
    /// differ from every other pass's over each ([`Collector::collect_cell`]): the cells found by
    /// delimiter listings, a page at a time ([`crate::cell::listed`]). A pass that fails stops none
    /// of the others; the first error is returned once all ran, what they did then unsaid. A cell
    /// another pass holds it leaves ([`Error::Left`]), as no failure.
    pub async fn collect<B: Bucket>(&mut self, bucket: &B, holder: u64) -> Result<Report, Error> {
        self.collect_with_own(bucket, None::<&B>, holder).await
    }

    /// cladefs: as [`Collector::collect`], `bucket` routed, `own` its own bucket, unrouted, if
    /// given (TASK-633.8): a cell listed here of another bucket, by its ID, is a copy of that
    /// cell's mirrored versions, passed over as such ([`Collector::collect_copy`]).
    pub async fn collect_with_own<B: Bucket, O: Bucket>(
        &mut self,
        bucket: &B,
        own: Option<&O>,
        holder: u64,
    ) -> Result<Report, Error> {
        #[cfg(feature = "full")]
        let number = match own {
            Some(own) => crate::identity::get(own).await?.map(|identity| identity.number),
            None => None,
        };
        #[cfg(not(feature = "full"))]
        let _ = own;
        if let Some(log) = crate::events::get_authoritative(bucket).await? {
            let mut merged = (*self.shredded).clone();
            merged.extend(log.shredded_set());
            self.shredded = Arc::new(merged);
        }
        let mut cells = std::pin::pin!(crate::cell::listed(bucket));
        let mut report = Report::default();
        let mut failed = None;
        while let Some(cell) = cells.next().await {
            // cladefs: the listing refused, the sweep stops, failed (board 5861).
            let cell = match cell {
                Ok(cell) => cell,
                Err(refused) => {
                    failed = failed.or(Some(refused.into()));
                    break;
                }
            };
            #[cfg(feature = "full")]
            if let (Some(own), Some(number)) = (own, number)
                && layout::routed_to(&cell.prefix()).is_some_and(|of| of != number)
            {
                match self.collect_copy(bucket, own, number, cell).await {
                    Ok(done) => report += done,
                    Err(error) => drop(failed.get_or_insert(error)),
                }
                continue;
            }
            match self.collect_cell(bucket, cell, holder).await {
                Ok(done) => report += done,
                Err(Error::Left(_)) => {}
                Err(error) => drop(failed.get_or_insert(error)),
            }
        }
        failed.map_or(Ok(report), Err)
    }

    /// cladefs: a pass over the copy of `cell`, a cell of another bucket, in this one, `own`, its
    /// own bucket, numbered `number` (doc-23, Mirrors; TASK-633.8): what mirrors of `cell`'s
    /// versions copied here, their pins and markers, and the references of children of this
    /// bucket that refer here. `bucket`, routed, reads `cell`'s head in its own bucket and the
    /// children's heads here.
    ///
    /// It reads `cell`'s head first, then lists the copy (`copyRelists`), so that a mirror or fork
    /// it races either shows in the listing or reads the head this read. A reference whose child
    /// has no head, one naming another origin, or one deleted and holding no references of its own
    /// (`copyChecksDead`, board 10446) is retired here; one of a deleted child that still holds
    /// its own children's references stands until those are gone (rule `references_held`). The
    /// copy is kept whole while `cell` lives (`copiesKeptWhileAlive`), a mirror is under way (a
    /// pin), or a child refers here: a live cell cuts versions after this read, whose mirrors take
    /// the objects they share with the copy as copied, and once the cell is deleted it cuts none.
    /// Else its mirrors' holds in `cell`'s bucket go first, then its markers (`markerGoesFirst`),
    /// so that no read is sent here for a version whose objects go, then its objects, then the
    /// marks of references retired here: the cell's prefix here left empty. No digest is recorded:
    /// what it keeps turns on `cell`'s head, in another bucket. A head not to be read deletes
    /// nothing.
    ///
    /// A pin unrefreshed for [`crate::mirror::PIN_STALE`], its mirror's host dead, keeps nothing:
    /// its mirror is begun again here, taking it over, and the copy kept this pass whatever that
    /// mirror came to.
    #[cfg(feature = "full")]
    pub async fn collect_copy(
        &self,
        bucket: &impl Bucket,
        own: &impl Bucket,
        number: u16,
        cell: CellId,
    ) -> Result<Report, Error> {
        let mut report = Report::default();
        let alive = crate::head::get(bucket, cell).await?.is_some_and(|head| !head.deleted());
        let listed = bucket::list(own, &cell.prefix()).await?;
        let (mut pinned, mut referring) = (false, Vec::new());
        // Stale pins grouped by version `at`, each carrying its key (v33 per-run or legacy
        // v31/v32) so the pass resumes the mirror once per version and then reaps the stale pins.
        let mut stale: BTreeMap<u64, Vec<&str>> = BTreeMap::new();
        // The versions of the copy, by what is listed of them: whose mirrors' holds go with it.
        let mut ats = BTreeSet::new();
        let marks: BTreeSet<&str> = (listed.iter())
            .filter(|meta| matches!(layout::kind(&meta.key), Kind::Retired { .. }))
            .map(|meta| meta.key.as_str())
            .collect();
        for meta in &listed {
            match layout::kind(&meta.key) {
                Kind::Mirror { at, .. }
                | Kind::Pin { at, .. }
                | Kind::Reference { at, .. }
                | Kind::Retired { at, .. }
                | Kind::Checkpoint { segment: at, .. } => drop(ats.insert(at)),
                _ => {}
            }
            match layout::kind(&meta.key) {
                // cladefs: keys of formats before this one, refused as a pass over the cell does.
                Kind::Former { cell, format } => return Err(Error::Format { cell, format }),
                Kind::Pin { at, .. } if crate::mirror::stale(own, meta) => {
                    stale.entry(at).or_default().push(meta.key.as_str());
                }
                Kind::Pin { .. } => pinned = true,
                Kind::Reference { at, child, .. } => referring.push((at, child, meta.key.as_str())),
                _ => {}
            }
        }
        let mut standing = false;
        let mut gone = Vec::new();
        for (at, child, key) in referring {
            let marked = layout::retired_of(key).is_some_and(|mark| marks.contains(mark.as_str()));
            let lives = !marked
                && match crate::head::get(bucket, child).await? {
                    Some(head) if head.origin().is_some_and(|o| (o.parent, o.at) == (cell, at)) => {
                        !head.deleted()
                            || (rules().references_held && has_references(bucket, child).await?)
                    }
                    _ => false,
                };
            match lives {
                true => standing = true,
                false => gone.push(key),
            }
        }
        // Nothing of it needed, it goes whole: all holds in A for this bucket of every version of
        // the copy (found via prefix LIST, covering both v33 per-run holds and legacy v31/v32
        // single-key holds, board 11699) go first, then its markers here, then what they named,
        // then the marks.
        let whole = !(alive || pinned || !stale.is_empty() || standing);
        let mut holds = Vec::new();
        if whole {
            for &at in &ats {
                if let Some(meta) =
                    bucket::head(bucket, &layout::legacy_hold(cell, at, number)).await?
                {
                    holds.push(meta.key);
                }
                for meta in bucket::list(bucket, &layout::holds(cell, at, number)).await? {
                    if matches!(
                        layout::kind(&meta.key),
                        Kind::Hold { parent, at: held_at, bucket: of }
                            if parent == cell && held_at == at && of == number
                    ) {
                        holds.push(meta.key);
                    }
                }
            }
        }
        let holds: Vec<&str> = holds.iter().map(String::as_str).collect();
        let of = |kind: fn(&Kind) -> bool| -> Vec<&Meta> {
            listed.iter().filter(|meta| kind(&layout::kind(&meta.key))).collect()
        };
        let markers = of(|kind| matches!(kind, Kind::Mirror { .. }));
        let copied = of(|kind| {
            matches!(
                kind,
                Kind::Segment { .. }
                    | Kind::Extent { .. }
                    | Kind::Checkpoint { .. }
                    // cladefs: a fork's read evidence written here, routed nowhere else
                    // (TASK-702.2).
                    | Kind::Evidence { .. }
            )
        });
        let written: Vec<String> = gone.iter().filter_map(|key| layout::retired_of(key)).collect();
        let marks: BTreeSet<&str> =
            marks.into_iter().chain(written.iter().map(String::as_str)).collect();
        // cladefs: all it deletes taken from its sweep's budget at once, or nothing deleted (board
        // 8529): the references gone, and, if it goes whole, the rest. Not what a mirror it
        // resumes moves here, which copies each reference here before it retires it there.
        let (objects, bytes) = match whole {
            true => (
                gone.len() + holds.len() + markers.len() + copied.len() + marks.len(),
                markers.iter().chain(&copied).map(|meta| meta.size).sum(),
            ),
            false => (gone.len(), 0),
        };
        if !self.may_delete(objects, bytes) {
            return Ok(report);
        }
        if !gone.is_empty() {
            lifecycle::retire(own, &gone).await?;
            report.retired += gone.len();
        }
        for (&at, stale_pins) in &stale {
            let resumed = crate::mirror::mirror(bucket, own, number, cell, at).await?;
            log::info!("collector: {cell}'s mirror at {at:x}, its pin stale, again: {resumed:?}");
            // Reap stale per-run (and legacy) pins once resumed (board 11699): when the version
            // is marked, leave matching holds in A for GcUnhold; when unmarked, delete each stale
            // pin's matching hold in A first, then the stale pin in B so a crash between leaves
            // the pin for the next pass.
            let reap_holds = match resumed {
                crate::mirror::Mirrored::UnderWay => continue,
                crate::mirror::Mirrored::Done { retired, .. } => {
                    report.retired += retired;
                    false
                }
                crate::mirror::Mirrored::Unnamed => true,
                crate::mirror::Mirrored::GaveUp(_) => {
                    match bucket::get(own, &layout::mirror(cell, at)).await? {
                        Some(object) => !layout::Mirror::decode(&object.bytes)?.is_marked(),
                        None => true,
                    }
                }
            };
            if reap_holds {
                let stale_holds: Vec<String> =
                    stale_pins.iter().filter_map(|key| layout::hold_of_pin(key, number)).collect();
                let stale_holds: Vec<&str> = stale_holds.iter().map(String::as_str).collect();
                bucket::delete_all(bucket, &stale_holds).await?;
            }
            bucket::delete_all(own, stale_pins).await?;
        }
        if !whole {
            self.observe_copy(own, cell, &listed, false, &[]);
            return Ok(report);
        }
        bucket::delete_all(bucket, &holds).await?;
        for metas in [&markers, &copied] {
            let keys: Vec<&str> = metas.iter().map(|meta| meta.key.as_str()).collect();
            bucket::delete_all(own, &keys).await?;
        }
        bucket::delete_all(own, &marks.into_iter().collect::<Vec<_>>()).await?;
        // What it deleted of the copy, not the marks, its own, which a pass over a child here may
        // have written as this one ran.
        let mut deleted_metas = Vec::new();
        for metas in [markers, copied] {
            report.deleted += metas.len();
            report.deleted_bytes += metas.iter().map(|meta| meta.size).sum::<u64>();
            if self.observer.is_some() {
                deleted_metas.extend(metas);
            }
        }
        self.observe_copy(own, cell, &listed, true, &deleted_metas);
        Ok(report)
    }

    /// cladefs: one pass over `cell`, as `holder`, which must differ from every other pass's over
    /// the cell: [`Error::Left`] if it leaves the cell to another, as its lease says.
    ///
    /// It takes the cell's lease, reading what the last pass to end recorded there, and lists the
    /// cell. A listing of the digest recorded is decided as it was: the pass deletes nothing,
    /// recovering nothing (TASK-395.6). Otherwise it passes in full. It records what it found as
    /// it lets the lease go, in the one write ([`Collector::release`]).
    pub async fn collect_cell(
        &self,
        bucket: &impl Bucket,
        cell: CellId,
        holder: u64,
    ) -> Result<Report, Error> {
        if cell.tenant != layout::DEFAULT_TENANT && self.shredded.contains(&cell.tenant) {
            lifecycle::delete_existing(bucket, cell, holder, u64::MAX).await?;
        }
        let found = passed::found(bucket, cell).await?;
        let looked = match &found {
            Some(object) => match self.look(bucket, cell, holder, object).await? {
                Looked::Unchanged(report) => return Ok(report),
                Looked::Changed(listed, epoch, ephemeral) => Some((listed, epoch, ephemeral)),
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
        let (listed, ephemeral) = match looked {
            Some((listed, epoch, ephemeral)) if taken.epoch == epoch + 1 => {
                (Some(listed), ephemeral)
            }
            _ => (first_listing(bucket, cell).await?, EphemeralCheck::Unchecked),
        };
        let recorded = taken.passed.take();
        let collected = self.collect_held(bucket, cell, &taken, recorded, listed, ephemeral).await;
        let ended = match &collected {
            Ok((_, passed)) => PassEnd::Through(passed.clone()),
            // The pass's failure, its caller's below: its lease says it ended.
            Err(_) => PassEnd::Failed,
        };
        self.release(bucket, cell, &taken, ended).await?;
        collected.map(|(report, _)| report)
    }

    /// cladefs: a look at `cell`, whose collector lease is `found`, for a pass of `holder`, before
    /// it takes the lease (TASK-455): the cell left to another pass, as the lease says
    /// ([`Error::Left`]); or, its listing of the digest the last pass in full recorded, the pass
    /// that skips it, which writes nothing and deletes nothing but the read evidence past each
    /// tenant's newest of each version, which nothing relies on ([`Collector::trim_evidence`], TASK-702.2): no
    /// other pass need be fenced, and it costs the lease's read and the listing. None: the pass
    /// goes on, taking the lease, with the look's listing and the lease's epoch it read.
    async fn look(
        &self,
        bucket: &impl Bucket,
        cell: CellId,
        holder: u64,
        found: &bucket::Object,
    ) -> Result<Looked, Error> {
        let lease = CollectorLease::decode(cell, &found.bytes)?;
        if let Some(left) = lease.leaves(holder, self.holding, bucket.now()) {
            return Err(Error::Left(left));
        }
        let epoch = lease.epoch;
        let Some(passed) = lease.passed.filter(|passed| passed.listing.is_some()) else {
            return Ok(Looked::Unrecorded);
        };
        let Some(listed) = first_listing(bucket, cell).await? else {
            return Ok(Looked::Unrecorded);
        };
        if !passed.holds_for(&listed) {
            return Ok(Looked::Changed(listed, epoch, EphemeralCheck::Unchecked));
        }
        if let Some(decommissioned) =
            self.check_retired_ephemeral(bucket, cell, &listed.objects).await?
        {
            return Ok(Looked::Changed(listed, epoch, EphemeralCheck::Retired(decommissioned)));
        }
        let skipped = Report { skipped: 1, ..Report::default() };
        Ok(Looked::Unchanged(
            self.trim_evidence(bucket, cell, Some(lease.format), &listed, skipped).await?,
        ))
    }

    /// cladefs: checks whether `objects` holds any ephemeral reference under `cell` that should
    /// now be retired because its child's tenant was shredded or its host's incarnation has a
    /// decommissioned tombstone in `cell`'s bucket (board 9968, hotbed question 544): both live
    /// outside `cell`'s prefix, so an unchanged cell listing digest must not skip retiring them.
    /// Returns `Some(decommissioned)` (the subset of non-shredded incarnations under `cell` that
    /// have tombstones, reused by `collect_held` without repeating bucket lookups; hotbed review
    /// 120 finding 455) when at least one ephemeral reference must be retired, or `None` if all
    /// ephemeral references under `cell` remain live.
    async fn check_retired_ephemeral(
        &self,
        bucket: &impl Bucket,
        cell: CellId,
        objects: &BTreeMap<String, Meta>,
    ) -> Result<Option<BTreeSet<u64>>, bucket::Refused> {
        let mut any_shredded = false;
        let mut check_incarnations = BTreeSet::new();
        for key in objects.keys() {
            if let Kind::Ephemeral { parent, incarnation, child, .. } = layout::kind(key)
                && parent == cell
            {
                if child.tenant != layout::DEFAULT_TENANT && self.shredded.contains(&child.tenant) {
                    any_shredded = true;
                } else {
                    check_incarnations.insert(incarnation);
                }
            }
        }
        let decommissioned = decommissioned_incarnations(bucket, cell, &check_incarnations).await?;
        if any_shredded || !decommissioned.is_empty() { Ok(Some(decommissioned)) } else { Ok(None) }
    }

    /// cladefs: orphaned ephemeral references under `cell` whose child belongs to a tenant in
    /// `self.shredded` or whose host incarnation has a decommissioned tombstone in `cell`'s bucket
    /// (board 9968, hotbed question 544), paired with their ephemeral index keys. Reuses
    /// `prechecked` from `look()` when present (hotbed review 120 finding 455); otherwise makes
    /// zero requests when `objects` holds no ephemeral references, one HEAD for a single distinct
    /// incarnation, or one delimited listing of `ephemeral/<bucket>/` for multiple incarnations.
    async fn retired_ephemeral<'a>(
        &self,
        bucket: &impl Bucket,
        cell: CellId,
        objects: &'a BTreeMap<String, Meta>,
        prechecked: Option<&BTreeSet<u64>>,
    ) -> Result<Vec<(&'a str, String)>, bucket::Refused> {
        let mut ephemeral = Vec::new();
        let mut check_incarnations = BTreeSet::new();
        for key in objects.keys() {
            if let Kind::Ephemeral { parent, at, incarnation, child } = layout::kind(key)
                && parent == cell
            {
                let shredded =
                    child.tenant != layout::DEFAULT_TENANT && self.shredded.contains(&child.tenant);
                if !shredded && prechecked.is_none() {
                    check_incarnations.insert(incarnation);
                }
                ephemeral.push((key.as_str(), at, incarnation, child, shredded));
            }
        }
        if ephemeral.is_empty() {
            return Ok(Vec::new());
        }
        let fetched;
        let decommissioned = match prechecked {
            Some(known) => known,
            None => {
                fetched = decommissioned_incarnations(bucket, cell, &check_incarnations).await?;
                &fetched
            }
        };
        Ok(ephemeral
            .into_iter()
            .filter(|(_, _, incarnation, _, shredded)| {
                *shredded || decommissioned.contains(incarnation)
            })
            .map(|(ref_key, at, incarnation, child, _)| {
                (ref_key, layout::ephemeral_index(incarnation, cell, at, child))
            })
            .collect())
    }

    /// cladefs: a pass over `cell`, which holds its lease as `taken` says, and what the last pass
    /// to end recorded, `recorded`: what it did, and what it records, if anything. Its listing the
    /// one that pass decided by, it decides as that one did, everything a pass decides by being
    /// under the cell's prefix (doc-7, Passes that skip; TASK-395.6), whatever process that pass
    /// was of: it recovers nothing, deletes nothing but read evidence nothing relies on
    /// ([`trim_evidence`]), and records nothing new. It retires no reference again: that pass's
    /// retirements were marked (TASK-395.14). An unchanged dead cell costs a pass its lease and its
    /// listing.
    async fn collect_held(
        &self,
        bucket: &impl Bucket,
        cell: CellId,
        taken: &LeaseTaking,
        recorded: Option<Passed>,
        listed: Option<Listed>,
        ephemeral: EphemeralCheck,
    ) -> Result<(Report, Option<Passed>), Error> {
        let prechecked = match ephemeral {
            EphemeralCheck::Retired(decommissioned) => Some(decommissioned),
            EphemeralCheck::Unchecked => {
                if let Some(listed) = &listed
                    && recorded.as_ref().is_some_and(|passed| passed.holds_for(listed))
                {
                    match self.check_retired_ephemeral(bucket, cell, &listed.objects).await? {
                        None => {
                            let skipped = Report { skipped: 1, ..Report::default() };
                            return Ok((
                                self.trim_evidence(
                                    bucket,
                                    cell,
                                    Some(layout::FORMAT.current),
                                    listed,
                                    skipped,
                                )
                                .await?,
                                None,
                            ));
                        }
                        Some(decommissioned) => Some(decommissioned),
                    }
                } else {
                    None
                }
            }
        };
        let mut remembered = Remembered::default();
        let survey = remembered.survey(bucket, cell, taken.epoch, Writes::Some, listed, false);
        let Some(pass) = survey.await? else {
            return Ok((Report::default(), None));
        };
        let mut report = Report::default();
        if !self.renew(bucket, cell, taken).await? {
            return Ok((report, None));
        }
        let doomed = pass.fates(bucket, &self.shredded)?;
        let retired_ephemeral =
            self.retired_ephemeral(bucket, cell, &pass.objects, prechecked.as_ref()).await?;
        // A pass a later one took the lease from stops.
        if !self.holds(bucket, cell, taken).await? {
            return Ok((report, None));
        }
        let retired = pass.retired();
        let removal = pass.removal(&doomed);
        // cladefs: past its sweep's budget, it deletes nothing, as one fenced, and records nothing
        // new, so that the next pass decides anew (board 8529): what it would delete, the head of
        // its cell if it goes whole, and the references it would retire.
        let head = removal.as_ref().and_then(|removal| removal.head);
        let objects =
            doomed.len() + usize::from(head.is_some()) + retired.len() + retired_ephemeral.len();
        let objects = objects + pass.recording.stale.len();
        let bytes = doomed.iter().copied().chain(head).map(|meta| meta.size).sum();
        if !self.may_delete(objects, bytes) {
            return Ok((report, None));
        }
        // Many at once, in batches where the backend deletes so, in any order: none of it is what a
        // live cell needs, whatever else goes (TASK-395.5).
        let keys: Vec<&str> = doomed.iter().map(|meta| meta.key.as_str()).collect();
        bucket::delete_all(bucket, &keys).await?;
        report.deleted += keys.len();
        report.deleted_bytes += doomed.iter().map(|meta| meta.size).sum::<u64>();
        // Retired for good, marked first, once: its cell, dead, changes no more, and its next
        // passes skip it (TASK-395.14); a clone's made unmarked, its one writer done with it
        // (TASK-455). The cell's own references listed marked, written again since their
        // retirements, deleted (TASK-395.13).
        let (marked, unmarked): (Vec<_>, Vec<_>) = retired.into_iter().partition(|r| r.1);
        let keys = |retired: &[(String, bool)]| -> Vec<String> {
            retired.iter().map(|(key, _)| key.clone()).collect()
        };
        let (marked, unmarked) = (keys(&marked), keys(&unmarked));
        lifecycle::retire(bucket, &marked.iter().map(String::as_str).collect::<Vec<_>>()).await?;
        bucket::delete_all(bucket, &unmarked.iter().map(String::as_str).collect::<Vec<_>>())
            .await?;
        // cladefs: and here, under cells of other buckets, where a mirror moved them, or forks
        // wrote them, those cells' versions mirrored here (TASK-633.8): the copy's pass keeps a
        // reference of a child deleted standing, until this one, its cell dead, retires it, counted
        // apart from its original there.
        if let Some(here) = bucket.mirrors() {
            let all: Vec<&str> = marked.iter().chain(&unmarked).map(String::as_str).collect();
            report.retired += here.retire(&all).await?;
        }
        let stale: Vec<&str> = pass.recording.stale.iter().map(String::as_str).collect();
        bucket::delete_all(bucket, &stale).await?;
        if !retired_ephemeral.is_empty() {
            let ref_keys: Vec<&str> =
                retired_ephemeral.iter().map(|(ref_key, _)| *ref_key).collect();
            let idx_keys: Vec<&str> =
                retired_ephemeral.iter().map(|(_, idx_key)| idx_key.as_str()).collect();
            bucket::delete_all(bucket, &ref_keys).await?;
            bucket::delete_all(bucket, &idx_keys).await?;
        }
        report.retired += marked.len() + unmarked.len() + stale.len() + retired_ephemeral.len();
        // cladefs: a dead cell nothing will need again goes whole, its retirements made: its head,
        // then its lease (TASK-455), the collector's own, which the report counts not, as it counts
        // not its taking. It records nothing: nothing is left to record it in.
        if let Some(removal) = removal {
            report.removed += removal.remove(bucket).await?;
            report.deleted += usize::from(removal.head.is_some());
            report.deleted_bytes += removal.head.map_or(0, |meta| meta.size);
            self.observe_pass(
                bucket,
                cell,
                Some(layout::FORMAT.current),
                &pass,
                &doomed,
                &retired_ephemeral,
                Some(&removal),
            );
            return Ok((report, None));
        }
        self.observe_pass(
            bucket,
            cell,
            Some(layout::FORMAT.current),
            &pass,
            &doomed,
            &retired_ephemeral,
            None,
        );
        // What the cell takes now: as listed, less what went.
        let mut usage = pass.recording.usage.clone();
        doomed.iter().for_each(|meta| usage.remove(&meta.key, meta.size));
        for &(ref_key, _) in &retired_ephemeral {
            if let Some(meta) = pass.objects.get(ref_key) {
                usage.remove(&meta.key, meta.size);
            }
        }
        let listing =
            if retired_ephemeral.is_empty() { pass.recorded_listing(&doomed) } else { None };
        Ok((report, Some(Passed { listing, usage })))
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
        cell: CellId,
        holder: u64,
        found: Option<bucket::Object>,
    ) -> Result<Option<LeaseTaking>, Error> {
        if found.is_none() && !passable(bucket, cell).await? {
            return Ok(None);
        }
        passed::take(bucket, cell, found, holder, self.holding).await.map(Some)
    }

    /// cladefs: whether the pass that took `cell`'s lease as `taken` says holds it still, renewing
    /// it for as long again as a pass holds it, if it does: what a pass checks before it deletes.
    async fn holds(
        &self,
        bucket: &impl Bucket,
        cell: CellId,
        taken: &LeaseTaking,
    ) -> Result<bool, Error> {
        match self.holding.lease {
            0 => Ok(!fenced(bucket, cell, taken.epoch).await?),
            _ => self.renew(bucket, cell, taken).await,
        }
    }

    /// cladefs: renews `cell`'s lease for the pass that took it as `taken` says, for as long again
    /// as a pass holds it, if it holds it still: whether it does. A collector holding cells for no
    /// time writes nothing, and goes on: it checks before it deletes ([`Collector::holds`]).
    async fn renew(
        &self,
        bucket: &impl Bucket,
        cell: CellId,
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
        cell: CellId,
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

    /// cladefs: what a pass over `cell` would delete, writing nothing (TASK-264): not taking the
    /// cell's collector lease, which would fence a pass that deletes, running meanwhile, out of its
    /// deletes; and what references it would retire ([`DryRun::retired`]). It goes by the lease's
    /// epoch as it starts, as a pass by the epoch it takes, to tell whether a pass that deletes ran
    /// meanwhile ([`DryRun::fenced`]). Where a pass would give up on a fork of a version deleted,
    /// writing its record, it takes the fork as given up on, as a pass mostly finds it. It goes
    /// over the cell in full, whatever the last pass recorded.
    pub async fn dry_run_cell(&self, bucket: &impl Bucket, cell: CellId) -> Result<DryRun, Error> {
        let epoch = passed::epoch(bucket, cell).await?;
        let listed = first_listing(bucket, cell).await?;
        let mut remembered = Remembered::default();
        let shredded_cell =
            cell.tenant != layout::DEFAULT_TENANT && self.shredded.contains(&cell.tenant);
        let Some(pass) =
            remembered.survey(bucket, cell, epoch, Writes::None, listed, shredded_cell).await?
        else {
            return Ok(DryRun { fenced: true, ..DryRun::default() });
        };
        let doomed_refs = pass.doomed(bucket, &self.shredded)?;
        self.observe_pass(bucket, cell, None, &pass, &doomed_refs, &[], None);
        let doomed = doomed_refs.into_iter().cloned().collect();
        let mut retired: Vec<String> = pass.retired().into_iter().map(|(key, _)| key).collect();
        retired.extend(pass.recording.stale.iter().cloned());
        retired.extend(
            self.retired_ephemeral(bucket, cell, &pass.objects, None)
                .await?
                .into_iter()
                .map(|(ref_key, _)| ref_key.to_owned()),
        );
        Ok(DryRun { doomed, retired, fenced: fenced(bucket, cell, epoch).await? })
    }
}

/// cladefs: the subset of `incarnations` that have a decommissioned tombstone in `cell`'s bucket
/// (board 9968, hotbed review 120 finding 455): zero requests when empty, one HEAD for a single
/// incarnation, or one delimited listing of `ephemeral/<bucket>/` for multiple incarnations so a
/// cell forked by many hosts never pays serial per-host HEADs.
async fn decommissioned_incarnations(
    bucket: &impl Bucket,
    cell: CellId,
    incarnations: &BTreeSet<u64>,
) -> Result<BTreeSet<u64>, bucket::Refused> {
    let mut found = BTreeSet::new();
    match incarnations.len() {
        0 => {}
        1 => {
            let &incarnation = incarnations.iter().next().expect("len == 1");
            if lifecycle::decommissioned(bucket, incarnation, cell).await? {
                found.insert(incarnation);
            }
        }
        _ => {
            let number = layout::ephemeral_bucket(cell);
            let listed = bucket::list_delimited(bucket, &layout::ephemeral_hosts(number)).await?;
            for meta in &listed.objects {
                if let Some((of, incarnation)) = layout::decommissioned_of(&meta.key)
                    && of == number
                    && incarnations.contains(&incarnation)
                {
                    found.insert(incarnation);
                }
            }
        }
    }
    Ok(found)
}

/// cladefs: whether `Collector::look` already ran the out-of-cell ephemeral retirement check for
/// its listing (hotbed review 120 finding 455).
enum EphemeralCheck {
    /// `look()` did not run an ephemeral retirement check for this listing.
    Unchecked,
    /// `look()` found at least one ephemeral reference to retire and already resolved which of
    /// the cell's non-shredded incarnations are decommissioned.
    Retired(BTreeSet<u64>),
}

/// cladefs: what a look at a cell found, before its pass took the lease ([`Collector::look`]).
enum Looked {
    /// The cell as the last pass in full recorded it: the pass that skips it, writing nothing.
    Unchanged(Report),
    /// Changed: its listing, the lease's epoch the look read, and any prechecked decommissioned
    /// incarnations from `look()`.
    Changed(Listed, u64, EphemeralCheck),
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
    /// cell `listed` before it read anything else ([`first_listing`]), and what the live cells
    /// and the origins referred to need of them; none if a later pass took the lease from it as
    /// it found what a live cell needs lost (TASK-180). It writes what giving up on forks needs,
    /// if `writes` says so.
    async fn survey(
        &mut self,
        bucket: &impl Bucket,
        cell: CellId,
        epoch: u64,
        writes: Writes,
        listed: Option<Listed>,
        shredded_cell: bool,
    ) -> Result<Option<Pass>, Error> {
        let early = listed.is_some();
        // Without the rule that they are listed after the states, with the candidates, and never
        // again, as a test shows it needed.
        let listed_first = match rules().references_relisted {
            true => None,
            false => Some(referenced(bucket, cell).await?),
        };
        // What the listing of the cell holds serves the first listing of the records, and of each
        // cell's checkpoints, that the pass makes (TASK-395.16).
        let objects = listed.as_ref().map(|listed| &listed.objects);
        let checkpoints = Checkpoints::new(bucket, 0).listed(objects);
        let mut record = self.record(bucket, cell, objects).await?;
        if shredded_cell
            && let Some(head) = &mut record
            && !head.deleted()
        {
            head.epoch = Head::FINAL;
            head.token = u64::MAX;
            head.versions.clear();
        }
        let mut live = Live::default();
        let read = self.read(bucket, &checkpoints, cell, &mut record, &mut live, writes);
        let read = match read.await {
            Ok(()) => {
                let referenced = match listed_first {
                    Some(listed) => listed,
                    None => referenced(bucket, cell).await?,
                };
                let by = KeptBy::Referred;
                keep_origins(bucket, &checkpoints, &mut live, &referenced, by)
                    .await
                    .map(|()| referenced)
            }
            Err(error) => Err(error),
        };
        let referenced = match read {
            Ok(referenced) => referenced,
            // Fenced, it read what the pass that fenced it, deleting since, may have found no live
            // cell needs: a version deleted, read live before, and its fork being made, read after
            // (TASK-180). It stops, as it would before deleting.
            Err(Error::Lost(_)) if fenced(bucket, cell, epoch).await? => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        drop(checkpoints);
        // What a pass listing the cell as it did decides as it did, unless it went by more than the
        // cell: a version of another cell's record, or a version's cell's head, read for a clone
        // being made.
        let forking_elsewhere = record.as_ref().is_some_and(|record| {
            !record.deleted() && forked_from(record).is_some_and(|from| from != cell)
        });
        let settled = early && !live.unsettled && !forking_elsewhere;
        let listed = match listed {
            Some(listed) => listed,
            None => passed::list(bucket, cell).await?,
        };
        let Listed { objects, digest, usage, stale, references } = listed;
        let recording = Recording { digest: settled.then_some(digest), usage, stale, references };
        let found = (objects, record, live, referenced, recording);
        Ok(Some(Pass::new(cell, found)))
    }

    /// Reads the state of `cell`, its head `record`, live, into `live`, and those of its versions.
    /// A pass fenced as it found what a live cell needs lost stops, where its caller sees it.
    async fn read(
        &mut self,
        bucket: &impl Bucket,
        checkpoints: &Checkpoints<'_, impl Bucket>,
        cell: CellId,
        record: &mut Option<Head>,
        live: &mut Live,
        writes: Writes,
    ) -> Result<(), Error> {
        if rules().collector_abandons {
            abandon(bucket, cell, record, writes).await?;
        }
        let origins =
            recover_live(bucket, checkpoints, cell, record.as_ref(), live, writes).await?;
        keep_origins(bucket, checkpoints, live, &origins, KeptBy::Version).await
    }

    /// `cell`'s record, if it has one, read: as the pass's listing of the whole cell, `listed`,
    /// has it, if it has one, taken before it read anything (TASK-395.16), a read less where it
    /// lists the version read before; or read now.
    async fn record(
        &mut self,
        bucket: &impl Bucket,
        cell: CellId,
        listed: Option<&BTreeMap<String, Meta>>,
    ) -> Result<Option<Head>, Error> {
        // cladefs: its head, as listed, read unless it is the version read before.
        let read = match listed {
            Some(objects) => {
                let key = layout::head(cell);
                match objects.get(&key) {
                    None => None,
                    Some(meta) => {
                        let known = self.heads.get(&key);
                        let read = read_record(bucket, known, cell, meta).await?;
                        match read {
                            Some((Some(version), head)) => {
                                self.heads.insert(key, (version, head.clone()));
                                Some(head)
                            }
                            Some((None, head)) => {
                                self.heads.remove(&key);
                                Some(head)
                            }
                            None => None,
                        }
                    }
                }
            }
            None => crate::head::get(bucket, cell).await?,
        };
        Ok(read)
    }
}

/// cladefs: whether a pass over `cell` has anything to go by, checked before it takes the cell's
/// lease, which would be the cell's one object otherwise: the format of the cell's object, a GET,
/// which a pass of another format refuses ([`Error::Format`]) before writing anything. A cell with
/// none, its cells made without one, its first record's format; and, holding no record, whether
/// it holds anything (a reference, what a dead cell left).
async fn passable(bucket: &impl Bucket, cell: CellId) -> Result<bool, Error> {
    // cladefs: its head, which a cell has from its making on, until its removal: a GET.
    if let Some(object) = bucket::get(bucket, &layout::head(cell)).await? {
        Head::decode(cell, &object.bytes)?;
        return Ok(true);
    }
    let listing = bucket::list_delimited(bucket, &cell.prefix()).await?;
    Ok(!listing.objects.is_empty() || !listing.prefixes.is_empty())
}

/// cladefs: every object of `cell`, listed before a pass reads anything else of it, its candidates
/// (rule `candidates_first`). Without the rule, as specs/bucket's NoCandidatesFirst shows it
/// needed, none: the pass lists the cell after the states, and no pass skips by a listing ([`Collector::collect_cell`]).
async fn first_listing(bucket: &impl Bucket, cell: CellId) -> Result<Option<Listed>, Error> {
    Ok(match rules().candidates_first {
        true => Some(passed::list(bucket, cell).await?),
        false => None,
    })
}

impl Collector {
    /// cladefs: `report`, of a pass over `cell` that skips it, its listing `listed`, with the read
    /// evidence it deletes counted: all but each tenant's newest of each version
    /// ([`crate::evidence::doomed`], TASK-702.2), which a cell's listing leaves out of its digest,
    /// so that forks' hosts writing it as they open the cell's versions leave it skipped. It needs
    /// no lease, nothing relying on evidence. Past its sweep's budget, it deletes none (board
    /// 8529): the next pass that skips the cell does.
    async fn trim_evidence(
        &self,
        bucket: &impl Bucket,
        cell: CellId,
        lease_format: Option<u32>,
        listed: &Listed,
        report: Report,
    ) -> Result<Report, Error> {
        let evidence = layout::evidence_of_cell(cell);
        let under = bucket::under(&listed.objects, &evidence);
        let doomed = crate::evidence::doomed(cell, &under, None, &self.shredded, bucket.now());
        let deleted_bytes = doomed.iter().map(|meta| meta.size).sum::<u64>();
        if !self.may_delete(doomed.len(), deleted_bytes) {
            self.observe_skipped(bucket, cell, lease_format, listed, &[]);
            return Ok(report);
        }
        let keys: Vec<&str> = doomed.iter().map(|meta| meta.key.as_str()).collect();
        bucket::delete_all(bucket, &keys).await?;
        self.observe_skipped(bucket, cell, lease_format, listed, &doomed);
        Ok(Report {
            deleted: report.deleted + keys.len(),
            deleted_bytes: report.deleted_bytes + deleted_bytes,
            ..report
        })
    }

    fn observe_skipped(
        &self,
        bucket: &impl Bucket,
        cell: CellId,
        lease_format: Option<u32>,
        listed: &Listed,
        trimmed: &[&Meta],
    ) {
        let Some(observer) = &self.observer else { return };
        let (kept, deleted) = split_samples(listed.objects.values(), trimmed, false);
        observer.observe(&CellSample {
            cell,
            foreign_copy: false,
            skipped: true,
            removed: false,
            now: bucket.now(),
            head_format: None,
            lease_format,
            live: None,
            versions: None,
            is_fork: None,
            replicated: None,
            references: listed.references as u64,
            kept: &kept,
            deleted: &deleted,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn observe_pass(
        &self,
        bucket: &impl Bucket,
        cell: CellId,
        lease_format: Option<u32>,
        pass: &Pass,
        doomed: &[&Meta],
        retired_ephemeral: &[(&str, String)],
        removal: Option<&Removal<'_>>,
    ) {
        let Some(observer) = &self.observer else { return };
        let mut deleted_keys: BTreeSet<&str> = doomed.iter().map(|m| m.key.as_str()).collect();
        for &(ref_key, _) in retired_ephemeral {
            deleted_keys.insert(ref_key);
        }
        if let Some(rem) = removal {
            if let Some(head) = rem.head {
                deleted_keys.insert(head.key.as_str());
            }
            if let Some(lease) = rem.lease {
                deleted_keys.insert(lease.key.as_str());
            }
        }
        let kept: Vec<ObjectSample> = if removal.is_some() {
            Vec::new()
        } else {
            pass.objects
                .values()
                .filter(|m| !deleted_keys.contains(m.key.as_str()))
                .map(object_sample)
                .collect()
        };
        let mut deleted: Vec<ObjectSample> = doomed.iter().copied().map(object_sample).collect();
        for &(ref_key, _) in retired_ephemeral {
            if let Some(meta) = pass.objects.get(ref_key) {
                deleted.push(object_sample(meta));
            }
        }
        if let Some(rem) = removal
            && let Some(head) = rem.head
        {
            deleted.push(object_sample(head));
        }
        let head = pass.record.as_ref();
        observer.observe(&CellSample {
            cell,
            foreign_copy: false,
            skipped: false,
            removed: removal.is_some(),
            now: bucket.now(),
            head_format: head.map(|h| h.format),
            lease_format,
            live: head.map(|h| !h.deleted()),
            versions: head.map(|h| h.versions.len() as u64),
            is_fork: head.map(|h| h.origin().is_some()),
            replicated: head.map(|h| h.replication.is_some()),
            references: pass.recording.references as u64,
            kept: &kept,
            deleted: &deleted,
        });
    }

    #[cfg(feature = "full")]
    fn observe_copy(
        &self,
        bucket: &impl Bucket,
        cell: CellId,
        listed: &[Meta],
        removed: bool,
        deleted_metas: &[&Meta],
    ) {
        let Some(observer) = &self.observer else { return };
        let (kept, deleted) = split_samples(listed.iter(), deleted_metas, removed);
        observer.observe(&CellSample {
            cell,
            foreign_copy: true,
            skipped: false,
            removed,
            now: bucket.now(),
            head_format: None,
            lease_format: None,
            live: None,
            versions: None,
            is_fork: None,
            replicated: None,
            references: 0,
            kept: &kept,
            deleted: &deleted,
        });
    }
}

fn split_samples<'a>(
    all: impl IntoIterator<Item = &'a Meta>,
    deleted_metas: &[&Meta],
    removed: bool,
) -> (Vec<ObjectSample>, Vec<ObjectSample>) {
    let deleted_keys: BTreeSet<&str> = deleted_metas.iter().map(|m| m.key.as_str()).collect();
    let kept = if removed {
        Vec::new()
    } else {
        all.into_iter()
            .filter(|m| !deleted_keys.contains(m.key.as_str()))
            .map(object_sample)
            .collect()
    };
    let deleted = deleted_metas.iter().copied().map(object_sample).collect();
    (kept, deleted)
}

fn object_sample(meta: &Meta) -> ObjectSample {
    ObjectSample { kind: layout::kind(&meta.key).name(), size: meta.size, modified: meta.modified }
}

/// cladefs: whether a pass writes to the bucket what giving up on forks needs: a dry run writes
/// nothing (TASK-264).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Writes {
    Some,
    None,
}

/// Whether a later pass over `cell` took the cell's collector lease from the pass that took it at
/// `epoch`.
async fn fenced(bucket: &impl Bucket, cell: CellId, epoch: u64) -> Result<bool, Error> {
    Ok(passed::epoch(bucket, cell).await? != epoch)
}

/// Gives up on `cell`, its head `record`, if it is a fork being made of a version that is gone,
/// which it says is deleted then, or made, if that came first ([`lifecycle::abandon`]); without
/// `writes`, takes it as deleted, writing nothing (TASK-476).
async fn abandon(
    bucket: &impl Bucket,
    cell: CellId,
    record: &mut Option<Head>,
    writes: Writes,
) -> Result<(), Error> {
    let Some(head) = record.as_ref().filter(|head| !head.deleted()) else { return Ok(()) };
    let (Some(origin), Some(id)) = (head.origin(), head.forking()) else { return Ok(()) };
    if !version_gone(bucket, origin, id).await? {
        return Ok(());
    }
    let abandoned = match writes {
        Writes::Some => lifecycle::abandon(bucket, cell).await?,
        Writes::None => Head { epoch: Head::FINAL, ..head.clone() },
    };
    *record = Some(abandoned);
    Ok(())
}

/// cladefs: whether the version `id` of the cell `origin` names, at the segment it names, is gone:
/// that cell's head names it no more, dropped by its writer or ended by a deletion (TASK-476).
async fn version_gone(bucket: &impl Bucket, origin: Origin, id: u64) -> Result<bool, Error> {
    let head = crate::head::get(bucket, origin.parent).await?;
    let named = head.as_ref().and_then(|head| head.version(id));
    Ok(named.is_none_or(|version| version.at != origin.at))
}

/// cladefs: the cell whose head a fork being made reads as it is made, `record`'s: its version's.
fn forked_from(record: &Head) -> Option<CellId> {
    record.forking()?;
    Some(record.origin()?.parent)
}

/// The record of `cell` at the key of `meta`, which a listing gave, if there is one, and the
/// version read: as `known` has it, if it read it at the version listed; or read now, unless it is
/// of another format ([`Error::Format`]), which stops the pass: what that cell needs, it can't
/// read.
async fn read_record(
    bucket: &impl Bucket,
    known: Option<&(String, Head)>,
    cell: CellId,
    meta: &Meta,
) -> Result<Option<(Option<String>, Head)>, Error> {
    if let Some(listed) = &meta.version.e_tag
        && let Some((version, record)) = known
        && version == listed
    {
        return Ok(Some((Some(version.clone()), record.clone())));
    }
    let Some(object) = bucket::get(bucket, &meta.key).await? else { return Ok(None) };
    Ok(Some((object.meta.version.e_tag, Head::decode(cell, &object.bytes)?)))
}

/// cladefs: whether `child`'s cell holds any reference of its own under [`layout::references`]
/// (children, ephemeral cells, mirror pins or holds): delimited pages, read after `child`'s head
/// was read deleted (`forksReferFirst`, `mirrorHolds`, `copyChecksDead`, board 10446), stopping at
/// the first non-empty page and following `next` across any empty page a cloud store returns
/// (hotbed review 94). None: `child` is dead, and its reference in a mirror's copy is retired here
/// even if its own pass already ran and this copy's marks were deleted before a late write of the
/// reference landed.
#[cfg(feature = "full")]
async fn has_references(bucket: &impl Bucket, child: CellId) -> Result<bool, bucket::Refused> {
    let prefix = layout::references(child);
    let mut after = None;
    loop {
        let page = bucket::Page { after: after.as_deref(), ..bucket::Page::FIRST };
        let listing = bucket::list_delimited_page(bucket, &prefix, page).await?;
        if !listing.prefixes.is_empty() || !listing.objects.is_empty() {
            return Ok(true);
        }
        let Some(next) = listing.next else { return Ok(false) };
        after = Some(next);
    }
}

/// cladefs: the origins the references under `cell` name, by a delimiter listing: its origins,
/// none of their children (doc-7, The layout). What a listing shows is an origin's prefix, not a
/// reference, and a clone's reference and its version's own share it, `<C>refs/<at>/`, the clone's
/// written before the version's is retired; so the prefix holds a key all along, and a listing
/// reaching it at any time shows it (a listing of the references whole would not: the clone's
/// behind its cursor, the version's ahead, retired).
async fn referenced(bucket: &impl Bucket, cell: CellId) -> Result<BTreeSet<(CellId, u64)>, Error> {
    let listed = bucket::list_delimited(bucket, &layout::references(cell)).await?.prefixes;
    let ats = listed.iter().filter_map(|at| layout::referenced_at(cell, at));
    Ok(ats.map(|at| (cell, at)).collect())
}

/// cladefs: what keeps an origin a pass keeps ([`keep_origins`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeptBy {
    /// A version a live cell's head names: its state lost is what the head needs lost, which
    /// stops the pass.
    Version,
    /// A reference: its state lost stops the pass if a child named under it lives, its record, read
    /// then, naming the origin as its origin; else nothing was lost, the children deleted,
    /// being made of a version deleted, claims, or having no record.
    Referred,
}

/// cladefs: keeps in `live` the states of the origins `origins` it keeps none of yet, each
/// recovered once, however many forks name it, several at once: the objects holding the extents
/// each names, and what recovering it read (doc-7, A pass over a cell, step 5). Those of other
/// cells are read there. A cell's are recovered in one walk where they share a checkpoint, each
/// footer read once for them all (`recovery::keys_at`, TASK-476): a cell's versions are up to 256
/// states of one history. A walk finding one lost, each is recovered alone, to tell which.
async fn keep_origins(
    bucket: &impl Bucket,
    checkpoints: &Checkpoints<'_, impl Bucket>,
    live: &mut Live,
    origins: &BTreeSet<(CellId, u64)>,
    by: KeptBy,
) -> Result<(), Error> {
    let mut unkept: BTreeMap<CellId, BTreeSet<u64>> = BTreeMap::new();
    for &(parent, at) in origins.iter().filter(|&origin| live.origins.insert(*origin)) {
        unkept.entry(parent).or_default().insert(at);
    }
    let walks = futures::stream::iter(unkept)
        .map(|(parent, ats)| async move {
            let walked = recovery::keys_at(checkpoints, parent, &ats).await;
            (parent, ats, walked)
        })
        .buffer_unordered(RECOVERIES_AT_ONCE)
        .collect::<Vec<_>>()
        .await;
    for (parent, ats, walked) in walks {
        match walked {
            Ok((named, touched)) => live.keep(named, touched),
            Err(Error::Lost(_)) if by == KeptBy::Referred => {
                for at in ats {
                    keep_referred(bucket, checkpoints, live, (parent, at)).await?;
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// cladefs: keeps in `live` the state of the origin `(parent, at)` a reference names, recovered
/// alone: if it is lost, a child named under it living on it stops the pass; with none, nothing
/// was lost, the children deleted, being made of a version deleted, claims, or having no record.
async fn keep_referred(
    bucket: &impl Bucket,
    checkpoints: &Checkpoints<'_, impl Bucket>,
    live: &mut Live,
    (parent, at): (CellId, u64),
) -> Result<(), Error> {
    let ats = BTreeSet::from([at]);
    match recovery::keys_at(checkpoints, parent, &ats).await {
        Ok((named, touched)) => live.keep(named, touched),
        Err(Error::Lost(_)) => {
            let Some(child) = needing(bucket, (parent, at)).await? else {
                // Decided by its children's records, in other cells.
                live.unsettled = true;
                return Ok(());
            };
            // Recovered again, after the child's record was read: its fork checkpointed the
            // state at its version before it wrote its reference and the record
            // (`lifecycle::record_fork`), so this finds it, listing anew. What the first found
            // lost may be another fork at the same segment number, a writer's that a
            // takeover's dropped, the takeover's writer forking the child there since, after
            // the first read the head (TASK-462): references name an origin by its segment
            // number alone.
            match recovery::keys_at(checkpoints, parent, &ats).await {
                Ok((named, touched)) => live.keep(named, touched),
                Err(Error::Lost(lost)) => {
                    return Err(Error::Lost(format!("{lost}, which {child} lives on")));
                }
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

/// cladefs: a child the references of the origin `(parent, at)` name that lives on it: its record
/// not deleted, a fork's, naming `(parent, at)` as its origin. Its references listed, and their
/// children's records read, in their cells: asked only of an origin whose state is lost.
async fn needing(
    bucket: &impl Bucket,
    (parent, at): (CellId, u64),
) -> Result<Option<CellId>, Error> {
    let listed = bucket::list(bucket, &layout::origin(parent, at)).await?;
    for meta in listed {
        let Kind::Reference { child, .. } = layout::kind(&meta.key) else { continue };
        let Some(record) = crate::head::get(bucket, child).await? else { continue };
        let lives =
            matches!(record.made, Made::Clone(from) if (from.parent, from.at) == (parent, at));
        if lives && !record.deleted() {
            return Ok(Some(child));
        }
    }
    Ok(None)
}

/// Recovers `cell`, its head `record`, live, keys-only ([`recovery::recover_keys`]), folding it
/// into `live`. Returns the origins of its versions, whose states the pass keeps (rule
/// `versions_kept`, TASK-476). A fork of a version still being made, its version deleted, whose
/// recovery finds what it names lost, it gives up on, writing its record so if `writes` says so
/// (TASK-395.4): what the pass read of it before, the version live, may be older than what
/// recovering it read.
async fn recover_live(
    bucket: &impl Bucket,
    checkpoints: &Checkpoints<'_, impl Bucket>,
    cell: CellId,
    record: Option<&Head>,
    live: &mut Live,
    writes: Writes,
) -> Result<BTreeSet<(CellId, u64)>, Error> {
    let mut origins = BTreeSet::new();
    let Some(record) = record.filter(|record| !record.deleted()) else { return Ok(origins) };
    // Recovered from its head, read after the listing: what its writer may name ([`behind`]), and
    // what it may yet name ([`Ended::ahead`]).
    let recovered = match recovery::recover_keys(checkpoints, cell, record.origin()).await {
        Ok(recovered) => recovered,
        Err(Error::Lost(lost)) => match lost_is_ok(bucket, cell, record, writes).await? {
            // Decided by what it read again, a version's record in another cell among it.
            true => {
                live.unsettled = true;
                return Ok(origins);
            }
            false => return Err(Error::Lost(lost)),
        },
        Err(error) => return Err(error),
    };
    if rules().versions_kept
        && let Some(head) = &recovered.head
    {
        origins.extend(head.versions.iter().map(|version| (cell, version.at)));
    }
    live.add(cell, recovered);
    Ok(origins)
}

/// Whether recovering `cell` finding what it needs lost lost nothing, as its record, read again,
/// says: deleted since it was read, what it needed may be gone; cladefs: a fork of a version still
/// being made, its version deleted, given up on (TASK-395.4), unless it was made first, as
/// [`abandon`] would have (rule `collector_abandons`): writing its record so, if `writes` says
/// so. cladefs: a fork being made as the pass read it, `read`, made since: what the pass read is
/// stale, its version checkpointed at its origin before it was made (TASK-653); decided again by
/// the next pass, as one still being made is. It held nothing of its own when listed.
async fn lost_is_ok(
    bucket: &impl Bucket,
    cell: CellId,
    read: &Head,
    writes: Writes,
) -> Result<bool, Error> {
    let Some(record) = crate::head::get(bucket, cell).await? else { return Ok(true) };
    if record.deleted() {
        return Ok(true);
    }
    let made_since = |head: &Head| read.forking().is_some() && head.forking().is_none();
    if made_since(&record) {
        return Ok(true);
    }
    let Some(id) = record.forking().filter(|_| rules().collector_abandons) else {
        return Ok(false);
    };
    let Some(origin) = record.origin() else { return Ok(false) };
    let gone = version_gone(bucket, origin, id).await?;
    // cladefs: a restore of a version into another cell, being made, recovers through its version's
    // cell's checkpoint at the version, by its key, which the restore writes after its reference,
    // before it makes the restore (`lifecycle::fork_version`): none there yet, it holds nothing,
    // nothing relies on it, and its reference keeps what it would inherit in that cell. Lost
    // nothing; decided again by the next pass (TASK-476).
    if !gone {
        return Ok(true);
    }
    match writes {
        // cladefs: made since, its version checkpointed at its origin before (TASK-653): what this
        // pass read of it, a fork being made, is stale; decided again by the next pass, as one
        // still being made is. It held nothing of its own when listed.
        Writes::Some => {
            let head = lifecycle::abandon(bucket, cell).await?;
            Ok(head.deleted() || made_since(&head))
        }
        Writes::None => Ok(true),
    }
}

/// What a pass needs of the live cells' states, folded in as they are recovered.
#[derive(Default)]
struct Live {
    /// Where each recovery of each ended.
    cells: BTreeMap<CellId, Vec<Ended>>,
    /// What recovering them read.
    touched: BTreeSet<String>,
    /// The objects the extents they name are in: each once, however many of them name extents
    /// there (TASK-395.12: a pass over a cell forked at many versions, each a state of every extent
    /// of it, looks each extent's key up, cloning it once).
    named: rustc_hash::FxHashSet<String>,
    /// cladefs: the origins whose states it keeps, each once ([`Live::keep_origin`]).
    origins: BTreeSet<(CellId, u64)>,
    /// cladefs: whether what it found went by more than the cell holds, which may change with
    /// nothing of the cell's changing: a recovery that found what it needs lost, and then nothing
    /// lost; a origin found lost, its children's records read (TASK-395.6).
    unsettled: bool,
}

/// What deciding what goes needs of a live cell's state, its extents but: its head, as its
/// recovery read it after the listing (TASK-407).
struct Ended {
    /// The number of segments it names.
    end: u64,
    /// Its epoch: of the writer holding it, or none's.
    epoch: u64,
    /// Its newest checkpoint, by its number and its writer's epoch.
    checkpoint: Option<(u64, u64)>,
    /// cladefs: the active fast WAL epoch named in `head.fast_wal.active` (doc-29): large own
    /// extents of this epoch at or past `end` stay protected until the taker flushes the fast WAL
    /// and switches `active` (rule `protect_wal_epoch`).
    wal_epoch: Option<u64>,
}

impl Ended {
    /// Whether the head may yet name the object of `kind`, which it names not (rule
    /// `collector_epochs`): of its epoch, or a later one, a segment or an extent of its own at or
    /// past its end, or a checkpoint newer than its own. An object of an earlier epoch, its
    /// writer's swaps all fail: no head ever names it, except large own extents of the active
    /// fast WAL epoch (`protect_wal_epoch`, doc-29).
    fn ahead(&self, kind: Kind) -> bool {
        rules().collector_epochs
            && match kind {
                Kind::Segment { segment, epoch, .. } => epoch >= self.epoch && segment >= self.end,
                Kind::Extent { segment, epoch, .. } => {
                    (epoch >= self.epoch
                        || (rules().protect_wal_epoch && Some(epoch) == self.wal_epoch))
                        && segment >= self.end
                }
                Kind::Checkpoint { segment, epoch, .. } => {
                    epoch >= self.epoch && self.checkpoint.is_none_or(|at| (segment, epoch) > at)
                }
                _ => false,
            }
    }
}

impl Live {
    fn active_wal_epoch(head: Option<&Head>) -> Option<u64> {
        head.and_then(|head| head.fast_wal.as_ref())
            .and_then(|fast_wal| fast_wal.active.as_ref())
            .map(|active| active.epoch)
    }

    /// Folds in `recovered`, a live cell's: where its recovery ended, what it read, and the
    /// objects its extents are in.
    fn add(&mut self, cell: CellId, recovered: recovery::RecoveredKeys) {
        let ended = Ended {
            end: recovered.end(),
            epoch: recovered.epoch(),
            checkpoint: recovered.checkpoint(),
            wal_epoch: Self::active_wal_epoch(recovered.head.as_ref()),
        };
        self.touched.extend(recovered.touched);
        self.named.extend(recovered.named);
        self.cells.entry(cell).or_default().push(ended);
    }

    /// cladefs: keeps an origin's state, for the forks of it to other cells, which `touched` was
    /// read to recover (TASK-395.4): the objects `named`, holding the extents it names, as a child
    /// with no checkpoint of its own names them where it read them; and what recovering it read,
    /// through which such a child recovers, unless the rule that it is kept is off, as a test
    /// shows it needed. What its forks freed of it since is no matter: the collector is
    /// conservative.
    fn keep(&mut self, named: rustc_hash::FxHashSet<String>, touched: BTreeSet<String>) {
        self.keep_read(touched);
        self.named.extend(named);
    }

    /// cladefs: keeps `touched`, what recovering origins' states read, unless the rule that it
    /// is kept is off, as a test shows it needed.
    fn keep_read(&mut self, touched: BTreeSet<String>) {
        if rules().origins_kept {
            self.touched.extend(touched);
        }
    }
}

/// What becomes of an object a pass listed ([`Pass::fates`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fate {
    Stays,
    Goes,
}

impl Fate {
    fn of(goes: bool) -> Self {
        if goes { Self::Goes } else { Self::Stays }
    }
}

/// What a pass over a cell found: every object of the cell listed, and what the live cells and
/// the origins it keeps need of them.
struct Pass {
    cell: CellId,
    objects: BTreeMap<String, Meta>,
    /// cladefs: the cell's head, if it has one (doc-20: a cell is one vdisk, its head its one
    /// record).
    record: Option<Head>,
    /// Where each recovery of each live cell ended.
    cells: BTreeMap<CellId, Vec<Ended>>,
    /// What recovering the live cells and the origins read.
    touched: BTreeSet<String>,
    /// The objects holding the extents the live cells and the origins name.
    named: rustc_hash::FxHashSet<String>,
    /// cladefs: the origins the references under the cell name, as listed after the states.
    referenced: BTreeSet<(CellId, u64)>,
    /// cladefs: what it records of its listing (TASK-395.6, TASK-395.8).
    recording: Recording,
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
    /// The pass over `cell`, which listed `objects`, and found its head `record`, what the live cells and
    /// the origins need (`live`), the origins `referenced`, and what it records as it ends.
    fn new(
        cell: CellId,
        found: (BTreeMap<String, Meta>, Option<Head>, Live, BTreeSet<(CellId, u64)>, Recording),
    ) -> Self {
        let (objects, record, live, referenced, recording) = found;
        let Live { cells, touched, named, .. } = live;
        Self { cell, objects, record, cells, touched, named, referenced, recording }
    }

    /// cladefs: whether the object of `kind`, which no live state names, a live cell's head may yet
    /// name, by what its key declares ([`Ended::ahead`], TASK-407): by any of the cell's
    /// recoveries, of several origins (TASK-173).
    fn ahead(&self, kind: Kind) -> bool {
        let cell = match kind {
            Kind::Segment { cell, .. }
            | Kind::Extent { cell, .. }
            | Kind::Checkpoint { cell, .. } => cell,
            _ => return false,
        };
        self.cells.get(&cell).into_iter().flatten().any(|ended| ended.ahead(kind))
    }

    /// What goes: everything listed that the live cells and the origins kept do not need, in
    /// the order of its keys ([`Pass::fates`]).
    /// cladefs: the cell's removal too, if it goes whole ([`Pass::removal`], TASK-455).
    fn doomed(&self, bucket: &impl Bucket, shredded: &BTreeSet<u64>) -> Result<Vec<&Meta>, Error> {
        let mut doomed = self.fates(bucket, shredded)?;
        if let Some(removal) = self.removal(&doomed) {
            doomed.extend(removal.head.iter().chain(&removal.lease).copied());
            doomed.sort_by(|a, b| a.key.cmp(&b.key));
        }
        Ok(doomed)
    }

    /// What goes, as [`Pass::doomed`] says, in the order of its keys. cladefs: a cell with no live
    /// cell and no reference, all of it but its records and its object, and the collector's own,
    /// goes. A cell holding moves, of formats before 28, is refused ([`Error::Format`]).
    fn fates(&self, bucket: &impl Bucket, shredded: &BTreeSet<u64>) -> Result<Vec<&Meta>, Error> {
        let now = bucket.now();
        let goes = Fate::of;
        // cladefs: the read evidence that goes (TASK-702.2): of versions the head names no more,
        // all of a dead cell's, all of shredded tenants' (board 2940), and all but each tenant's
        // newest of each.
        let named: BTreeSet<u64> = (self.record.as_ref().filter(|head| !head.deleted()))
            .map(|head| head.versions.iter().map(|version| version.at).collect())
            .unwrap_or_default();
        let evidence = bucket::under(&self.objects, &layout::evidence_of_cell(self.cell));
        let evidence: BTreeSet<String> =
            (crate::evidence::doomed(self.cell, &evidence, Some(&named), shredded, now)
                .into_iter())
            .map(|meta| meta.key.clone())
            .collect();
        let mut doomed = Vec::new();
        for (key, meta) in &self.objects {
            let named = self.named.contains(key);
            let kind = layout::kind(key);
            let fate = match kind {
                Kind::Checkpoint { .. } => goes(!self.touched.contains(key) && !self.ahead(kind)),
                // Once a checkpoint covers it, if its extents are dead, or relocated by its cell's
                // writer (doc-27); but one its head may yet name ([`Pass::ahead`]).
                Kind::Segment { .. } => {
                    goes(!named && !self.touched.contains(key) && !self.ahead(kind))
                }
                // Once no segment will name it.
                Kind::Extent { .. } => goes(!named && !self.ahead(kind)),
                // cladefs: the head slots of formats before 29, which nothing of this one reads.
                Kind::Former { cell, format } => return Err(Error::Format { cell, format }),
                // A head stays, a deleted cell's too, final, fencing its writers, which no swap of
                // theirs passes (TASK-174); but with a dead cell that goes whole
                // ([`Pass::removal`], TASK-455). The collector's own: its lease; and references, which the passes of their children's cells retire (TASK-395.4), and
                // their marks, kept for good (TASK-395.13). cladefs: a block of object IDs taken
                // for good (TASK-364), and the bucket's identity (TASK-633.9); a tenant's KEK
                // object, no cell's, which no pass lists, for good too (TASK-411.27).
                Kind::Head { .. }
                | Kind::CollectorLease { .. }
                | Kind::Reference { .. }
                | Kind::Retired { .. }
                // cladefs: a mirror's pin and marker, under another bucket's cell's prefix, which a
                // pass over that cell's copy decides ([`Collector::collect_copy`], TASK-633.8); and
                // its hold, under the cell's here, which that pass deletes.
                | Kind::Pin { .. }
                | Kind::Hold { .. }
                // cladefs: an ephemeral cell's reference, and its index entry: its host's to delete
                // (doc-25, TASK-698.3), or retired by `Collector::retired_ephemeral` when its
                // child's tenant is shredded or its host's incarnation is decommissioned (board
                // 9968, hotbed question 544); and a host's tombstone, kept for good (board 8519).
                | Kind::Ephemeral { .. }
                | Kind::EphemeralIndex { .. }
                | Kind::Decommissioned { .. }
                | Kind::Mirror { .. }
                | Kind::ObjectIds { .. }
                | Kind::Identity
                | Kind::Buckets
                | Kind::Kek { .. }
                | Kind::Wal { .. }
                | Kind::Unknown => Fate::Stays,
                Kind::Evidence { .. } => goes(evidence.contains(key)),
            };
            if fate == Fate::Goes {
                doomed.push(meta);
            }
        }
        Ok(doomed)
    }

    /// cladefs: the references the pass retires, under other cells (doc-7, References; TASK-395.4):
    /// once its cell holds no live cell and no reference of its own, its own, its head deleted, of
    /// an origin in another cell, at the key its head's origin gives, never listed.
    /// Nothing anywhere then inherits from them: a fork of one of them in the cell would have a
    /// record there, and one in another cell a reference there. Without the rule that the cell's
    /// references hold them, as a test shows it needed, once each child is deleted. cladefs: each
    /// with whether it is marked first: but a clone's made, of an ID an allocator gave
    /// ([`Record::retires_unmarked`], TASK-455).
    fn retired(&self) -> Vec<(String, bool)> {
        if rules().references_held && !self.dead() {
            return Vec::new();
        }
        let Some(record) = self.record.as_ref().filter(|record| record.deleted()) else {
            return Vec::new();
        };
        let Some(origin) = record.origin().filter(|origin| origin.parent != self.cell) else {
            return Vec::new();
        };
        let marked = !record.retires_unmarked();
        vec![(layout::reference(origin.parent, origin.at, self.cell), marked)]
    }

    /// cladefs: whether its cell holds no live cell and no reference of its own.
    fn dead(&self) -> bool {
        self.record.as_ref().is_none_or(|record| record.deleted())
            && self.cells.is_empty()
            && self.referenced.is_empty()
    }

    /// cladefs: whether every reference it listed is one it deletes, stale ([`Listed::stale`]).
    fn references_all_stale(&self) -> bool {
        self.recording.references == self.recording.stale.len()
    }

    /// cladefs: what removes the cell whole, if it goes (TASK-455): dead, its listed references
    /// all stale, and every object it listed that `doomed` leaves one of a cell
    /// nothing will need again: its head, [`Head::removable`] (a root's or a clone's made,
    /// deleted, of an ID an allocator gave), and its lease. A mark under it (of a reference
    /// retired, which a writer may write again), and anything else keep it. A cell of no head loses its lease alone, once
    /// nothing but its lease is left: one whose removal stopped before its end.
    fn removal<'a>(&'a self, doomed: &[&Meta]) -> Option<Removal<'a>> {
        if !self.dead() || !self.references_all_stale() {
            return None;
        }
        let doomed: BTreeSet<&str> = doomed.iter().map(|meta| meta.key.as_str()).collect();
        let mut removal = Removal::default();
        for (key, meta) in &self.objects {
            if doomed.contains(key.as_str()) {
                continue;
            }
            match layout::kind(key) {
                Kind::Head { cell } => {
                    let own = |record: &&Head| cell == self.cell && record.removable();
                    self.record.as_ref().filter(own)?;
                    removal.head = Some(meta);
                }
                Kind::CollectorLease { .. } => removal.lease = Some(meta),
                _ => return None,
            }
        }
        Some(removal)
    }

    /// cladefs: the listing's digest it records, if any ([`Passed::listing`]): of what its listing
    /// leaves once its deletes land, if its cell is dead, as a pass in full over that finds nothing
    /// more to do (TASK-455): so that a dead cell it does not remove is not passed over in full
    /// twice, the second for the first's deletes, retiring its references again. What it leaves
    /// must be what a dead cell keeps, its head, lease and marks of retired references. Else of its
    /// listing.
    fn recorded_listing(&self, doomed: &[&Meta]) -> Option<[u8; 32]> {
        let digest = self.recording.digest?;
        if !self.dead() || !self.references_all_stale() {
            return Some(digest);
        }
        let doomed: BTreeSet<&str> = doomed.iter().map(|meta| meta.key.as_str()).collect();
        let left: Vec<&Meta> =
            self.objects.values().filter(|meta| !doomed.contains(meta.key.as_str())).collect();
        let kept = left.iter().all(|meta| {
            matches!(
                layout::kind(&meta.key),
                Kind::Head { .. } | Kind::CollectorLease { .. } | Kind::Retired { .. }
            )
        });
        Some(if kept { passed::digest(self.cell, left) } else { digest })
    }
}

/// cladefs: what a pass removing its cell whole deletes ([`Pass::removal`], TASK-455).
#[derive(Default)]
struct Removal<'a> {
    head: Option<&'a Meta>,
    lease: Option<&'a Meta>,
}

impl Removal<'_> {
    /// Deletes it: the head, then the lease last, so that a pass stopping between finds the
    /// cell's lease and what is left, which it removes. Returns the cells it removed: 1, or 0 if it
    /// removed only the lease of a cell of no head. A pass that took the lease meanwhile finds it
    /// gone, fenced, and deletes nothing more: what is left, the next removes.
    async fn remove(&self, bucket: &impl Bucket) -> Result<usize, bucket::Refused> {
        for meta in self.head.iter().chain(&self.lease) {
            bucket::delete(bucket, &meta.key).await?;
        }
        Ok(usize::from(self.head.is_some()))
    }
}

#[cfg(all(test, feature = "full"))]
mod tests {
    use super::*;
    use crate::batch::{Commit, SegmentBuilder};
    use crate::bucket::{ByteRange, Created, Error as BucketError, Object, PutMode, Version};
    use crate::layout::Child;
    use crate::layout::test_cell;
    use crate::object_bucket::{ObjectBucket, Request};
    use crate::testing::{Direct, SimBucket};
    use crate::writer::Writer;
    use futures::FutureExt as _;
    use futures::TryStreamExt as _;
    use futures::executor::block_on;
    use object_store::memory::InMemory;
    use std::future::Future;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use storage_device::ExtentId;

    fn memory_bucket() -> Arc<ObjectBucket> {
        Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""))
    }

    /// `commit` in a segment of its own, uploaded and acknowledged.
    async fn commit<B: Bucket>(writer: &Writer<B>, commit: Commit) {
        let objects = SegmentBuilder::from(commit)
            .close(&mut writer.chain())
            .frame(writer.keys().encrypting());
        let segment = writer.add_segment(objects).unwrap();
        writer.upload(segment).await.unwrap();
        writer.acknowledge().await.unwrap();
    }

    /// The keys of every object of the bucket but the collector's leases.
    async fn keys(bucket: &impl Bucket) -> Vec<String> {
        (bucket::list(bucket, "").await.unwrap().into_iter())
            .map(|meta| meta.key)
            .filter(|key| !matches!(layout::kind(key), Kind::CollectorLease { .. }))
            .collect()
    }

    /// The extents a writer stored on their own and freed go; one uploaded ahead of the segment
    /// that will name it stays, as do those its state names, and its head.
    #[test]
    fn extents_minted_and_freed_go_and_those_ahead_of_their_segments_stay() {
        block_on(async {
            let bucket = memory_bucket();
            lifecycle::create(&*bucket, test_cell(1), 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), test_cell(1), 10, 0).await.unwrap();
            let first = writer.chain().next_sequence();
            let (freed, kept) = (ExtentId::new(1, first), ExtentId::new(1, first + 1));
            let own = vec![(freed.sequence, b"freed".to_vec()), (kept.sequence, b"kept".to_vec())];
            commit(&writer, Commit { tag: 1, own, ..Commit::default() }).await;
            let gone = writer.location(freed).unwrap().key;
            commit(&writer, Commit { tag: 2, frees: vec![freed], ..Commit::default() }).await;
            let (segment, next) = (writer.chain().next_segment(), writer.chain().next_sequence());
            let ahead =
                layout::extent(layout::TEST_CELL, writer.epoch(), segment, ExtentId::new(1, next));
            let bytes = layout::with_footer(
                b"ahead",
                &(),
                crate::testing::encrypting(layout::test_cell(1)),
            );
            let created = bucket::create(&*bucket, &ahead, bytes).await.unwrap();
            assert!(matches!(created, Created::Created(_)));

            let before = keys(&*bucket).await;
            let report = collect(&*bucket, 1).await.unwrap();
            assert_eq!(report, Report { deleted: 1, retired: 0, ..report });
            let left: Vec<_> = before.into_iter().filter(|key| *key != gone).collect();
            assert_eq!(keys(&*bucket).await, left);
            assert!(left.contains(&ahead) && left.contains(&layout::head(test_cell(1))));
        });
    }

    /// An extent stored on its own, uploaded ahead of the segment that names it, stays, though
    /// the cell's segments landed say its next sequence number is past it: as when the guest
    /// commits extents out of the order it minted them, or the cell closes a segment between
    /// commits while extents are open (TASK-79.3): of the head's epoch, its segment at or past the
    /// head's end, the head may yet name it (TASK-407). Its segment acknowledged, the cell reads
    /// it.
    #[test]
    fn extents_ahead_of_their_segments_stay_past_the_next_sequence_number() {
        assert_eq!(block_on(extent_ahead_of_its_segment_read()), Ok(b"late".to_vec()));
    }

    /// What reading an extent stored on its own gives, uploaded ahead of the segment naming it,
    /// committed after one past it in sequence, once a pass ran and then the segment landed.
    async fn extent_ahead_of_its_segment_read() -> Result<Vec<u8>, Error> {
        let bucket = memory_bucket();
        lifecycle::create(&*bucket, test_cell(1), 0).await.unwrap();
        let writer = Writer::open(bucket.clone(), test_cell(1), 10, 0).await.unwrap();
        let first = writer.chain().next_sequence();
        let (late, early) = (ExtentId::new(1, first), ExtentId::new(1, first + 1));
        let extents = vec![(early, b"early".to_vec())];
        commit(&writer, Commit { tag: 1, extents, ..Commit::default() }).await;
        assert_eq!(writer.chain().next_sequence(), first + 2);
        // The writer's next segment, which names `late`, its object uploaded first.
        let own = vec![(late.sequence, b"late".to_vec())];
        let objects = SegmentBuilder::from(Commit { tag: 2, own, ..Commit::default() })
            .close(&mut writer.chain())
            .frame(writer.keys().encrypting());
        let [(key, bytes)] = &objects.own[..] else { panic!("{} objects", objects.own.len()) };
        let created = bucket::create(&*bucket, key, bytes.read().unwrap()).await.unwrap();
        assert!(matches!(created, Created::Created(_)));

        collect(&*bucket, 1).await.unwrap();
        let segment = writer.add_segment(objects).unwrap();
        writer.upload(segment).await.unwrap();
        writer.acknowledge().await.unwrap();
        let recovered = recovery::recover(&*bucket, test_cell(1), None).await.unwrap();
        let location = &recovered.state.extents[&late];
        crate::testing::read(&bucket, late, location).await
    }

    /// TASK-407: a writer taken over, whose head another taking swapped since, acknowledges
    /// nothing: its swap fails, though its segment landed and its successor committed and
    /// checkpointed. The cell holds the successor's commit; the writer's segment, which no head
    /// names, nor ever will, a pass deletes.
    #[test]
    fn writers_taken_over_acknowledge_nothing() {
        let (acknowledged, last, kept) = block_on(acknowledge_once_taken_over(0));
        assert_eq!((acknowledged, last, kept), (Err(Error::Fenced), Some(2), false));
    }

    /// The head written whatever is there: the writer taken over writes it over its successor's,
    /// and the cell holds its commits, not the successor's, acknowledged (specs/bucket's
    /// NoHeadSwap). cladefs: its swaps after its acknowledgement too, three, its checkpoint's swap
    /// among them.
    #[test]
    fn writers_taken_over_acknowledging_nothing_need_head_swap() {
        crate::set_rules(crate::Rules { head_swap: false, ..crate::Rules::ALL });
        let (acknowledged, last, _) = block_on(acknowledge_once_taken_over(3));
        crate::set_rules(crate::Rules::ALL);
        assert_eq!((acknowledged, last), (Ok(Some(0)), Some(5)));
    }

    /// What a writer taken over gets of the acknowledgement of its commit 1, its segment uploaded
    /// before its successor took the cell, committed 2 and checkpointed; the cell's last
    /// commit, after a pass; and whether the pass kept the writer's segment.
    /// cladefs: and `more` commits of the writer taken over after, each acknowledged.
    async fn acknowledge_once_taken_over(
        more: u64,
    ) -> (Result<Option<u64>, Error>, Option<u64>, bool) {
        let bucket = memory_bucket();
        lifecycle::create(&*bucket, test_cell(1), 0).await.unwrap();
        let taken = Writer::open(bucket.clone(), test_cell(1), 10, 0).await.unwrap();
        let objects = SegmentBuilder::from(Commit { tag: 1, ..Commit::default() })
            .close(&mut taken.chain())
            .frame(taken.keys().encrypting());
        let segment = taken.add_segment(objects).unwrap();
        taken.upload(segment).await.unwrap();
        let successor = Writer::open(bucket.clone(), test_cell(1), 11, 0).await.unwrap();
        commit(&successor, Commit { tag: 2, ..Commit::default() }).await;
        successor.checkpoint().await.unwrap();
        let acknowledged = taken.acknowledge().await;
        for tag in 3..3 + more {
            commit(&taken, Commit { tag, ..Commit::default() }).await;
        }
        collect(&*bucket, 1).await.unwrap();
        let key = layout::segment(test_cell(1), segment, taken.epoch());
        let kept = keys(&*bucket).await.contains(&key);
        let recovered = recovery::recover(&*bucket, test_cell(1), None).await;
        (acknowledged, recovered.ok().and_then(|recovered| recovered.state.last_commit), kept)
    }

    /// TASK-407: a pass between a writer's upload of its segment and its acknowledgement keeps the
    /// segment, no state naming it: of the head's epoch, at its end, the head may yet name it.
    #[test]
    fn segments_uploaded_not_yet_acknowledged_are_kept() {
        assert_eq!(block_on(acknowledge_past_a_pass()), Some(1));
    }

    /// The collector deleting what no head names yet: the writer's swap names a segment gone, and
    /// the cell is lost (specs/bucket's NoCollectorEpochs).
    #[test]
    fn segments_kept_until_acknowledged_need_collector_epochs() {
        crate::set_rules(crate::Rules { collector_epochs: false, ..crate::Rules::ALL });
        let last = block_on(acknowledge_past_a_pass());
        crate::set_rules(crate::Rules::ALL);
        assert_eq!(last, None);
    }

    /// The last commit of cell 1, whose writer acknowledged its commit 1 after a pass ran
    /// between its upload and its acknowledgement: none if the cell is lost.
    async fn acknowledge_past_a_pass() -> Option<u64> {
        let bucket = memory_bucket();
        lifecycle::create(&*bucket, test_cell(1), 0).await.unwrap();
        let writer = Writer::open(bucket.clone(), test_cell(1), 10, 0).await.unwrap();
        let objects = SegmentBuilder::from(Commit { tag: 1, ..Commit::default() })
            .close(&mut writer.chain())
            .frame(writer.keys().encrypting());
        let segment = writer.add_segment(objects).unwrap();
        writer.upload(segment).await.unwrap();
        collect(&*bucket, 1).await.unwrap();
        writer.acknowledge().await.unwrap();
        let recovered = recovery::recover(&*bucket, test_cell(1), None).await;
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
        let key = layout::segment(test_cell(1), 0, 2);
        crate::set_rules(crate::Rules::ALL);
        let corrupt = Error::Corrupt(format!("{key} holds another segment"));
        assert_eq!((uploaded, last), (Err(corrupt), None));
    }

    /// What the successor of a writer taken over gets of the upload of its commit 2 at segment 0,
    /// where the writer taken over uploaded its commit 1, never acknowledged; and the cell's last
    /// commit, once it acknowledged what it could.
    async fn upload_where_a_writer_taken_over_did() -> (Result<(), Error>, Option<u64>) {
        let bucket = memory_bucket();
        lifecycle::create(&*bucket, test_cell(1), 0).await.unwrap();
        let taken = Writer::open(bucket.clone(), test_cell(1), 10, 0).await.unwrap();
        let objects = SegmentBuilder::from(Commit { tag: 1, ..Commit::default() })
            .close(&mut taken.chain())
            .frame(taken.keys().encrypting());
        let segment = taken.add_segment(objects).unwrap();
        taken.upload(segment).await.unwrap();
        let successor = Writer::open(bucket.clone(), test_cell(1), 11, 0).await.unwrap();
        let objects = SegmentBuilder::from(Commit { tag: 2, ..Commit::default() })
            .close(&mut successor.chain())
            .frame(successor.keys().encrypting());
        let segment = successor.add_segment(objects).unwrap();
        let uploaded = successor.upload(segment).await;
        let _ = successor.acknowledge().await;
        let recovered = recovery::recover(&*bucket, test_cell(1), None).await;
        (uploaded, recovered.ok().and_then(|recovered| recovered.state.last_commit))
    }

    /// TASK-455's AC #3: a base's cell keeps nothing of the clones of its version made and gone,
    /// of IDs an allocator gave: each clone's reference retired unmarked, and its cell removed
    /// whole, so that the base's cell holds no reference, as before any, and its pass lists as much
    /// whatever the clones were. Before, a mark a clone, for good.
    #[test]
    fn bases_keep_nothing_of_clones_made_and_gone() {
        let (few, report) = block_on(base_after_clones(3));
        assert_eq!((report.removed, report.retired), (3, 3));
        let (many, report) = block_on(base_after_clones(30));
        assert_eq!((report.removed, report.retired), (30, 30));
        assert_eq!(few, many);
        let kinds: Vec<&str> = few.iter().map(|key| layout::kind(key).name()).collect();
        assert_eq!(kinds.iter().filter(|kind| **kind == "reference").count(), 0, "{few:#?}");
        assert!(!kinds.contains(&"retired"), "{few:#?}");
    }

    /// The objects under the base's cell once `clones` clones of a version of it, of IDs an
    /// allocator gave, each a cell of its own, were made and deleted and passes ran until quiet,
    /// and what those did; nothing left of the clones' cells.
    async fn base_after_clones(clones: u64) -> (Vec<String>, Report) {
        use crate::layout::Origin;
        use crate::testing::{VERSION, fork_version, versioned_by};
        let bucket = Arc::new(Direct(SimBucket::new()));
        let base = test_cell(1);
        lifecycle::create(&*bucket, base, 0).await.unwrap();
        let writer = Writer::open(bucket.clone(), base, 10, 0).await.unwrap();
        let extent = ExtentId::new(1, writer.chain().next_sequence());
        let at = versioned_by(&writer, vec![(extent, vec![1; 64])], VERSION).await;
        writer.checkpoint().await.unwrap();
        let origin = Origin { parent: base, at, epoch: writer.epoch() };
        let ids = crate::objectid::Allocator::new();
        let mut cells = Vec::new();
        for n in 0..clones {
            let clone = CellId { tenant: 1, id: ids.next(&*bucket).await.unwrap() };
            fork_version(&*bucket, clone, VERSION, origin, 0x40 + n).await.unwrap();
            lifecycle::delete(&*bucket, clone, 20, u64::MAX).await.unwrap();
            cells.push(clone);
        }
        let report = crate::testing::collect_until_quiet(&*bucket).await.unwrap();
        for cell in cells {
            assert_eq!(bucket::list(&*bucket, &cell.prefix()).await.unwrap(), Vec::new(), "{cell}");
        }
        assert_eq!(crate::testing::uncollected(&*bucket).await, Vec::<String>::new());
        let listed = bucket::list(&*bucket, &base.prefix()).await.unwrap();
        (listed.into_iter().map(|meta| meta.key).collect(), report)
    }

    /// A bucket that runs `then` once, at the request its hook names, counted from when `then` was
    /// set.
    struct Hooked {
        bucket: Arc<Direct>,
        hook: Hook,
        then: Mutex<Option<Box<dyn FnOnce() + Send>>>,
        /// How many gets of [`Hook::Getting`]'s key it answered with `then` set.
        asked: Mutex<usize>,
        /// The key whose put, once sent, is the last request it answers: its client dies then,
        /// every request after it never answered.
        dies_after: Mutex<Option<String>>,
        dead: AtomicBool,
    }

    /// Where [`Hooked`] runs what it runs.
    enum Hook {
        /// Before the get of the object at this key of this number, from 1.
        Getting(String, usize),
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
            self.answered(self.bucket.get(key))
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
            let put = self.answered(self.bucket.put(key, bytes, mode));
            if self.dies_after.lock().unwrap().as_deref() == Some(key) {
                self.dead.store(true, Ordering::SeqCst);
            }
            put
        }

        fn delete(&self, key: &str) -> impl Future<Output = Result<(), BucketError>> + Send {
            self.answered(self.bucket.delete(key))
        }

        fn now(&self) -> u64 {
            self.bucket.now()
        }
    }

    /// TASK-395.5: over S3, a pass deletes what goes in one DeleteObjects, no DELETE: the server
    /// and the bucket count one batch, of the segments a cell freed and checkpointed past.
    #[test]
    fn passes_delete_in_batches_over_s3() {
        use crate::s3_server::{BUCKET, S3Server};
        const SEGMENTS: u64 = 5;
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let server = S3Server::start(None);
            let url = format!("s3://{BUCKET}/collector");
            let client = &crate::client::Client::DEFAULT;
            let (objects, prefix) =
                crate::object_bucket::object_store_with(&url, server.options(), client).unwrap();
            let bucket = Arc::new(ObjectBucket::new(objects, &prefix));
            // Segments each holding an extent, then one freeing them all, checkpointed past: each
            // a pass deletes.
            lifecycle::create(&*bucket, test_cell(1), 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), test_cell(1), 10, 0).await.unwrap();
            let first = writer.chain().next_sequence();
            let mut freed = Vec::new();
            for n in 0..SEGMENTS {
                let extent = ExtentId::new(1, first + n);
                let extents = vec![(extent, vec![n as u8; 100])];
                commit(&writer, Commit { tag: n + 1, extents, ..Commit::default() }).await;
                freed.push(extent);
            }
            let segments: Vec<String> =
                freed.iter().map(|&extent| writer.location(extent).unwrap().key).collect();
            let frees = Commit { tag: SEGMENTS + 1, frees: freed, ..Commit::default() };
            commit(&writer, frees).await;
            writer.checkpoint().await.unwrap();

            let (served, stats) = (server.requests(), bucket.stats());
            let report = collect(&*bucket, 1).await.unwrap();
            assert_eq!(report, Report { deleted: SEGMENTS as usize + 1, retired: 0, ..report });
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

    /// TASK-79.85: a fork no host holds any more, as one whose host left neither its handle nor
    /// its deletion, is a live cell to the collector, which keeps what it inherited for it. An
    /// operator finds it among the live cells, and deletes it, whatever placement last took
    /// it: the next pass frees what only it held.
    #[test]
    fn cells_no_host_holds_keep_what_they_inherited_until_deleted() {
        let bucket = memory_bucket();
        let (extent, held_in) = block_on(async {
            lifecycle::create(&*bucket, test_cell(1), 0).await.unwrap();
            let base = Writer::open(bucket.clone(), test_cell(1), 10, 0).await.unwrap();
            let extent = ExtentId::new(1, base.chain().next_sequence());
            let held_in = base.chain().next_segment();
            let own = vec![(extent.sequence, b"held".to_vec())];
            commit(&base, Commit { tag: 1, own, ..Commit::default() }).await;
            let at = commit_and_fork(&base, Vec::new(), forked(test_cell(2))).await;
            assert_eq!(base.checkpoint().await, Ok(Some(at)));
            commit(&base, Commit { tag: 2, frees: vec![extent], ..Commit::default() }).await;
            base.checkpoint().await.unwrap();
            // Its host took it, at a placement later than any since, and went.
            crate::testing::take(&*bucket, test_cell(2), 11, 7).await.unwrap();
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
        block_on(collect(&*bucket, 1)).unwrap();
        assert!(kept(), "what the fork inherited went while it is live");

        let live = |cell| block_on(crate::head::live(&*bucket, cell)).unwrap().is_some();
        assert_eq!((live(test_cell(1)), live(test_cell(2))), (true, true));
        block_on(lifecycle::delete(&*bucket, test_cell(2), 12, u64::MAX)).unwrap();
        // The pass over the child's cell retires its reference; the next over its parent's frees.
        block_on(collect(&*bucket, 2)).unwrap();
        block_on(collect(&*bucket, 3)).unwrap();
        assert!(!kept(), "what the fork inherited stayed once it was deleted");
        assert_eq!((live(test_cell(1)), live(test_cell(2))), (true, false));
    }

    /// A bucket that marks cell `child`'s record deleted just before the first read of `key`.
    struct DeletedAt {
        bucket: Direct,
        key: String,
        child: CellId,
        done: AtomicBool,
    }

    impl DeletedAt {
        /// Deletes the child, the first time `key` is read, whole or by a range of it.
        fn reading(&self, key: &str) {
            if key == self.key && !self.done.swap(true, Ordering::Relaxed) {
                // cladefs: as a deletion writes it.
                let record = Head { epoch: Head::FINAL, ..Head::of(Made::Own, 0) };
                let key = layout::head(self.child);
                let bytes = layout::encode(&record);
                self.bucket
                    .put(&key, bytes.into(), PutMode::Overwrite)
                    .now_or_never()
                    .unwrap()
                    .unwrap();
            }
        }
    }

    impl Bucket for DeletedAt {
        fn get(&self, key: &str) -> impl Future<Output = Result<Object, BucketError>> + Send {
            self.reading(key);
            self.bucket.get(key)
        }

        fn get_range(
            &self,
            key: &str,
            range: ByteRange,
        ) -> impl Future<Output = Result<Vec<u8>, BucketError>> + Send {
            self.reading(key);
            self.bucket.get_range(key, range)
        }

        crate::bucket::delegate_bucket_methods!(except_reads bucket);
    }

    /// A bucket that runs `meanwhile` once, as a pass first reads the head of `cell`: having
    /// listed the records, as it starts to read that cell's state.
    struct Meanwhile<F> {
        bucket: Arc<Direct>,
        cell: CellId,
        meanwhile: std::sync::Mutex<Option<F>>,
    }

    impl<F: FnOnce() + Send + 'static> Bucket for Meanwhile<F> {
        /// Of the cell's head, read by its recovery: `meanwhile` first, the first time.
        fn get(&self, key: &str) -> impl Future<Output = Result<Object, BucketError>> + Send {
            if key == layout::head(self.cell)
                && let Some(meanwhile) = self.meanwhile.lock().unwrap().take()
            {
                meanwhile();
            }
            self.bucket.get(key)
        }

        crate::bucket::delegate_bucket_methods!(except_get bucket);
    }

    /// TASK-158: everything between a pass's listing the records and its reading a cell's state:
    /// a version of it, a restore of that, the version dropped, what it held freed, and a
    /// checkpoint. The state the pass reads names none of it, and it
    /// listed neither record: only listing them again finds the restored cell, which keeps what
    /// it inherited. Found by the simulation, seed 5310 of its `restores`.
    #[test]
    fn forks_of_versions_made_and_gone_within_a_pass_keep_what_they_hold() {
        assert_eq!(
            read_what_a_fork_of_a_version_made_and_gone_within_a_pass_holds(),
            Ok(b"held".to_vec())
        );
    }

    /// What the restored cell of
    /// [`forks_of_versions_made_and_gone_within_a_pass_keep_what_they_hold`] reads of the extent it
    /// inherited, once the pass is done.
    fn read_what_a_fork_of_a_version_made_and_gone_within_a_pass_holds() -> Result<Vec<u8>, Error> {
        let sim = SimBucket::new();
        let direct = Arc::new(Direct(sim.clone()));
        const RESTORED: u64 = 3;
        let writer = block_on(async {
            lifecycle::create(&*direct, test_cell(1), 0).await.unwrap();
            Arc::new(Writer::open(direct.clone(), test_cell(1), 10, 0).await.unwrap())
        });
        let extent = ExtentId::new(1, writer.chain().next_sequence());
        let extents = vec![(extent, b"held".to_vec())];
        block_on(async {
            let batch = SegmentBuilder::from(Commit { tag: 1, extents, ..Commit::default() });
            let segment = writer
                .add_segment(batch.close(&mut writer.chain()).frame(writer.keys().encrypting()))
                .unwrap();
            writer.upload(segment).await.unwrap();
            writer.acknowledge().await.unwrap();
            writer.checkpoint().await.unwrap();
        });
        let meanwhile = {
            let (direct, writer) = (direct.clone(), writer.clone());
            move || {
                let work = async {
                    let version = crate::testing::VERSION;
                    let segment = crate::testing::versioned_by(&writer, Vec::new(), version).await;
                    let origin =
                        Origin { parent: test_cell(1), at: segment, epoch: writer.epoch() };
                    crate::testing::fork_version(&*direct, test_cell(RESTORED), version, origin, 0)
                        .await
                        .unwrap();
                    writer.drop_versions(&BTreeSet::from([version])).await.unwrap();
                    let frees = vec![extent];
                    let batch = SegmentBuilder::from(Commit { tag: 3, frees, ..Commit::default() });
                    let segment = writer
                        .add_segment(
                            batch.close(&mut writer.chain()).frame(writer.keys().encrypting()),
                        )
                        .unwrap();
                    writer.upload(segment).await.unwrap();
                    writer.acknowledge().await.unwrap();
                    writer.checkpoint().await.unwrap();
                };
                work.now_or_never().expect("a direct bucket answers at once");
            }
        };
        let during = Meanwhile {
            bucket: direct.clone(),
            cell: test_cell(1),
            meanwhile: std::sync::Mutex::new(Some(meanwhile)),
        };
        block_on(collect(&during, 1)).unwrap();
        block_on(async {
            let record = crate::head::get(&*direct, test_cell(RESTORED)).await.unwrap();
            let recovered =
                recovery::recover(&*direct, test_cell(RESTORED), record.unwrap().origin()).await?;
            crate::testing::read(&direct, extent, &recovered.state.extents[&extent]).await
        })
    }

    /// A live cell whose recovery finds what it needs gone, here its parent's segment, which no
    /// head names, fails the pass, which deletes nothing; one deleted meanwhile is live no longer,
    /// and the pass goes on.
    #[test]
    fn cells_lost_fail_the_pass_unless_deleted() {
        // CellId 2, forked from 1, which has no record; and a segment of no cell.
        let origin = Origin { parent: test_cell(1), at: 0, epoch: 1 };
        let stray = layout::segment(test_cell(2), 0, 1);
        let sim = SimBucket::new();
        let direct = Direct(sim.clone());
        block_on(async {
            let forked = Head::of(Made::Clone(origin), 0);
            crate::testing::make_head(&direct, test_cell(2), forked).await.unwrap();
            let bytes =
                layout::with_footer(b"", &(), crate::testing::encrypting(layout::test_cell(1)));
            assert!(matches!(
                bucket::create(&direct, &stray, bytes).await.unwrap(),
                Created::Created(_)
            ));
        });
        let before = block_on(keys(&direct));
        let checkpoint = layout::checkpoint(origin.parent, origin.at, origin.epoch);
        let lost = Error::Lost(checkpoint.clone());
        assert_eq!(block_on(collect(&direct, 1)), Err(lost));
        assert_eq!(block_on(keys(&direct)), before);

        let deleting = DeletedAt {
            bucket: Direct(sim),
            key: checkpoint,
            child: test_cell(2),
            done: AtomicBool::new(false),
        };
        let report = block_on(collect(&deleting, 2)).unwrap();
        assert_eq!(report, Report { deleted: 1, retired: 0, ..report });
        assert!(!block_on(keys(&deleting)).contains(&stray));
    }

    /// TASK-264: a dry run a pass ran during, which took the collector's lease and deleted, says it
    /// was fenced.
    #[test]
    fn dry_runs_a_pass_ran_during_say_so() {
        block_on(async {
            let direct = Arc::new(Direct(SimBucket::new()));
            lifecycle::create(&*direct, test_cell(1), 0).await.unwrap();
            // As the dry run reads the lease again, at its end.
            let during = Hooked::new(
                direct.clone(),
                Hook::Getting(layout::collector_lease(layout::TEST_CELL), 2),
            );
            let passing = direct.clone();
            *during.then.lock().unwrap() = Some(Box::new(move || {
                let pass = collect(&*passing, 9).now_or_never();
                assert_eq!(pass, Some(Ok(Report::default())));
            }));
            let dry = Collector::new().dry_run_cell(&during, layout::TEST_CELL).await.unwrap();
            assert_eq!(dry, DryRun { fenced: true, ..DryRun::default() });
        });
    }

    // cladefs: the collector per cell, and fork references (TASK-395.2, .3, .4).

    /// The child of a fork to `cell`, its owner its ID.
    fn forked(cell: CellId) -> Child {
        Child { cell, owner: 0x100 | cell.id }
    }

    /// Has `writer` commit `extents` in a segment, then a version in the next, uploaded and
    /// acknowledged, forked to `child`, and dropped: the version's segment, the child's origin.
    async fn commit_and_fork<B: Bucket + 'static>(
        writer: &Writer<B>,
        extents: Vec<(ExtentId, Vec<u8>)>,
        child: Child,
    ) -> u64 {
        commit(writer, Commit { tag: 1, extents, ..Commit::default() }).await;
        let id = 0x100 + child.cell.id;
        let at = crate::testing::versioned_by(writer, Vec::new(), id).await;
        let origin = Origin { parent: writer.cell(), at, epoch: writer.epoch() };
        crate::testing::fork_version(writer.bucket(), child.cell, id, origin, child.owner)
            .await
            .unwrap();
        writer.drop_versions(&BTreeSet::from([id])).await.unwrap();
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

    /// TASK-395.3, TASK-395.10: a clone of a version writes its reference, under the cell of the
    /// version's origin, after its record and before it reads the version's; whatever its cell, the
    /// origin's own too.
    #[test]
    fn clones_refer_between_their_records_and_their_versions_read() {
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
    /// its version's cell's head; the same for a clone in another cell than its origin's and in
    /// that one.
    fn clones_requests() -> Vec<&'static str> {
        use crate::layout::{Head, Origin, Version};
        use crate::testing::{VERSION, fork_version};
        let bucket = SimBucket::new();
        let origin = Origin { parent: test_cell(1), at: 3, epoch: 1 };
        // The head naming the version, of a cell no bucket here has otherwise: what the clones
        // read of it. A clone into another cell finds its checkpoint missing, and gives up.
        let version = Version { id: VERSION, at: origin.at, epoch: origin.epoch, shared: false };
        let head = Head {
            epoch: 1,
            holder: 1,
            end: origin.at + 1,
            runs: vec![(0, 1)],
            versions: vec![version],
            ..Head::of(Made::Root, 1)
        };
        let key = layout::head(origin.parent);
        bucket.served(bucket::create(&*bucket, &key, layout::encode(&head))).0.unwrap();
        let mut orders = Vec::new();
        for clone in [test_cell(4), test_cell(3)] {
            let (_, asked) = bucket.served(fork_version(&*bucket, clone, VERSION, origin, 0x33));
            let reference = layout::reference(origin.parent, origin.at, clone);
            let mut steps = [
                (position(&asked, &format!("put {reference}")), "reference"),
                (position(&asked, &format!("put {}", layout::head(clone))), "record"),
                (position(&asked, &format!("get {key}")), "read"),
            ];
            steps.sort();
            orders.push(steps.map(|(_, step)| step).to_vec());
            let bytes = bucket.served(bucket::get(&*bucket, &reference)).0.unwrap().unwrap().bytes;
            assert_eq!(bytes, layout::encode(&layout::Reference::of(0x33)));
        }
        assert_eq!(orders[0], orders[1]);
        orders.swap_remove(0)
    }

    /// What reading `extent` gives `cell`, recovered from its record's origin.
    async fn read_inherited<B: Bucket>(
        bucket: &Arc<B>,
        cell: CellId,
        extent: ExtentId,
    ) -> Result<Vec<u8>, Error> {
        let record = crate::head::get(&**bucket, cell).await?.ok_or(Error::NoCell)?;
        let recovered = recovery::recover(&**bucket, cell, record.origin()).await?;
        let location = recovered.state.extents.get(&extent).cloned();
        let location = location.ok_or_else(|| Error::Lost(format!("{extent:?} of {cell}")))?;
        crate::testing::read(bucket, extent, &location).await
    }

    /// Sweeps over every cell, deleting what is free at once, until two in a row delete nothing:
    /// failing as a pass finds what a live cell needs lost.
    async fn sweep(bucket: &impl Bucket) -> Result<(), Error> {
        crate::testing::collect_until_quiet(bucket).await.map(drop)
    }

    /// TASK-395.4: a fork to another cell keeps its parent's state at the origin in the
    /// parent's cell, by its reference, the parent deleted and having freed what the child
    /// inherited, its checkpoint covering the segments the child, with no checkpoint of its own,
    /// recovers through.
    #[test]
    fn fork_versions_keep_what_children_in_other_cells_inherit() {
        assert_eq!(block_on(read_forked_from_a_deleted_parent()), Ok(b"held".to_vec()));
    }

    /// Without references, the parent's cell's pass reclaims what the child inherited: it reads its
    /// parent deleted, and lists nothing of the child's cell.
    #[test]
    fn fork_versions_kept_for_children_in_other_cells_need_references() {
        crate::set_rules(crate::Rules { references: false, ..crate::Rules::ALL });
        let read = block_on(read_forked_from_a_deleted_parent());
        crate::set_rules(crate::Rules::ALL);
        assert!(matches!(read, Err(Error::Lost(_))), "{read:?}");
    }

    /// Keeping the extents of an origin's state alone, not what recovering it reads: the child,
    /// with no checkpoint of its own, recovers through the parent's segments, which the parent's
    /// checkpoint covers, gone.
    #[test]
    fn fork_versions_kept_for_children_in_other_cells_need_what_recovering_them_reads() {
        crate::set_rules(crate::Rules { origins_kept: false, ..crate::Rules::ALL });
        let read = block_on(read_forked_from_a_deleted_parent());
        crate::set_rules(crate::Rules::ALL);
        assert!(matches!(read, Err(Error::Lost(_))), "{read:?}");
    }

    /// TASK-476: a version keeps its cell's state at it, which its cell freed since and
    /// checkpointed past, the checkpoint covering the segments recovering the version reads; once
    /// its writer drops it, the next sweep reclaims it.
    #[test]
    fn versions_keep_what_they_name_until_dropped() {
        assert_eq!(block_on(read_a_version(Dropped::No)), Ok(b"held".to_vec()));
        let read = block_on(read_a_version(Dropped::Yes));
        assert!(matches!(read, Err(Error::Lost(_))), "{read:?}");
    }

    /// Without the rule `versions_kept`, a pass keeps the cell's state alone, and reclaims what
    /// its version names.
    #[test]
    fn versions_keeping_what_they_name_need_versions_kept() {
        crate::set_rules(crate::Rules { versions_kept: false, ..crate::Rules::ALL });
        let read = block_on(read_a_version(Dropped::No));
        crate::set_rules(crate::Rules::ALL);
        assert!(matches!(read, Err(Error::Lost(_))), "{read:?}");
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Dropped {
        No,
        Yes,
    }

    /// cladefs: an ephemeral cell's reference keeps its version's state, its cell having dropped
    /// the version, freed what it named and checkpointed past it, as a child's reference does
    /// (doc-25, TASK-698.3); deleted by its host, or by its host's decommissioning, the next sweep
    /// reclaims it. Its index entry goes with it.
    #[test]
    fn ephemeral_references_keep_their_versions_until_their_hosts_let_go() {
        for (gone, read) in [
            (Unreferred::No, Ok(b"held".to_vec())),
            (Unreferred::ByItsHost, Err(())),
            (Unreferred::Decommissioned, Err(())),
        ] {
            let (found, indexes) = block_on(read_a_version_ephemerally(gone));
            assert_eq!(found.map_err(drop), read, "{gone:?}");
            assert_eq!(indexes, usize::from(gone == Unreferred::No), "{gone:?}");
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Unreferred {
        No,
        ByItsHost,
        Decommissioned,
    }

    /// What a version of a cell reads, as [`read_a_version`] reads it, an ephemeral cell forked
    /// from it, the version dropped, then its reference deleted as `gone` says; and how many index
    /// entries of the ephemeral cells' host are left.
    async fn read_a_version_ephemerally(gone: Unreferred) -> (Result<Vec<u8>, Error>, usize) {
        let objects: Arc<InMemory> = Arc::new(InMemory::new());
        let bucket = Arc::new(ObjectBucket::new(objects.clone(), ""));
        let cell = test_cell(1);
        lifecycle::create(&*bucket, cell, 0).await.unwrap();
        let writer = Writer::open(bucket.clone(), cell, 10, 0).await.unwrap();
        let extent = ExtentId::new(1, writer.chain().next_sequence());
        commit(
            &writer,
            Commit { tag: 1, extents: vec![(extent, b"held".to_vec())], ..Commit::default() },
        )
        .await;
        let cut = layout::NewVersion { id: 0xa, keep_last: None };
        let cutting = SegmentBuilder::from(Commit { tag: 2, ..Commit::default() });
        let at = writer
            .add_segment(cutting.cut(cut, &mut writer.chain()).frame(writer.keys().encrypting()))
            .unwrap();
        writer.upload(at).await.unwrap();
        writer.acknowledge().await.unwrap();
        let (origin, child, incarnation) =
            (Origin { parent: cell, at, epoch: writer.epoch() }, test_cell(9), 0x2a);
        let keys = crate::keys::Parents::Held(writer.keys());
        lifecycle::fork_ephemeral(&*bucket, child, 0xa, origin, (incarnation, keys)).await.unwrap();
        // The cell's checkpoint at the version, which its openings start from (TASK-698.8).
        let checkpoints = crate::bucket::list(&*bucket, &layout::checkpoints(cell)).await.unwrap();
        let at_version = |meta: &crate::bucket::Meta| matches!(layout::kind(&meta.key), layout::Kind::Checkpoint { segment, .. } if segment == at);
        assert!(checkpoints.iter().any(at_version), "no checkpoint at the version");
        commit(&writer, Commit { tag: 3, frees: vec![extent], ..Commit::default() }).await;
        writer.checkpoint().await.unwrap();
        writer.drop_versions(&BTreeSet::from([0xa])).await.unwrap();
        // A fork of the version dropped is refused, and leaves no reference.
        let keys = crate::keys::Parents::Held(writer.keys());
        let refused =
            lifecycle::fork_ephemeral(&*bucket, test_cell(8), 0xa, origin, (incarnation, keys));
        assert_eq!(refused.await, Err(Error::Deleted));
        match gone {
            Unreferred::No => {}
            Unreferred::ByItsHost => {
                lifecycle::unrefer_ephemeral(&*bucket, origin, incarnation, child).await.unwrap()
            }
            Unreferred::Decommissioned => {
                let buckets = lifecycle::ephemeral_buckets(&*bucket).await.unwrap();
                let decommissioned = lifecycle::decommission(&*bucket, &buckets, incarnation, 10);
                let decommissioned = decommissioned.await.unwrap();
                assert_eq!(decommissioned, lifecycle::Decommissioned { retired: 1, left: 0 });
            }
        }
        let buckets = lifecycle::ephemeral_buckets(&*bucket).await.unwrap();
        let indexes = lifecycle::ephemeral_references(&*bucket, &buckets, incarnation);
        let indexes = indexes.await.unwrap().len();
        if let Err(error) = sweep(&*bucket).await {
            return (Err(error), indexes);
        }
        let reader = Arc::new(ObjectBucket::new(objects, ""));
        let checkpoints = Checkpoints::new(&*reader, 0);
        let read = async {
            let (state, _) = recovery::recover_at(&checkpoints, cell, at).await?;
            let location = state.extents.get(&extent).cloned();
            let location = location.ok_or_else(|| Error::Lost(format!("{extent:?} of {cell}")))?;
            crate::testing::read(&reader, extent, &location).await
        };
        (read.await, indexes)
    }

    /// What a version of a cell reads of an extent the cell sealed before it and freed after,
    /// the cell checkpointed past it, sweeps having run until they did nothing more, the version
    /// dropped first if `dropped` says so: read by a reader of another host's.
    async fn read_a_version(dropped: Dropped) -> Result<Vec<u8>, Error> {
        let objects: Arc<InMemory> = Arc::new(InMemory::new());
        let bucket = Arc::new(ObjectBucket::new(objects.clone(), ""));
        let cell = test_cell(1);
        lifecycle::create(&*bucket, cell, 0).await.unwrap();
        let writer = Writer::open(bucket.clone(), cell, 10, 0).await.unwrap();
        let extent = ExtentId::new(1, writer.chain().next_sequence());
        commit(
            &writer,
            Commit { tag: 1, extents: vec![(extent, b"held".to_vec())], ..Commit::default() },
        )
        .await;
        let cut = layout::NewVersion { id: 0xa, keep_last: None };
        let cutting = SegmentBuilder::from(Commit { tag: 2, ..Commit::default() });
        let at = writer
            .add_segment(cutting.cut(cut, &mut writer.chain()).frame(writer.keys().encrypting()))
            .unwrap();
        writer.upload(at).await.unwrap();
        writer.acknowledge().await.unwrap();
        commit(&writer, Commit { tag: 3, frees: vec![extent], ..Commit::default() }).await;
        writer.checkpoint().await.unwrap();
        if dropped == Dropped::Yes {
            writer.drop_versions(&BTreeSet::from([0xa])).await.unwrap();
        }
        sweep(&*bucket).await?;
        let reader = Arc::new(ObjectBucket::new(objects, ""));
        let checkpoints = Checkpoints::new(&*reader, 0);
        let (state, _) = recovery::recover_at(&checkpoints, cell, at).await?;
        let location = state.extents.get(&extent).cloned();
        let location = location.ok_or_else(|| Error::Lost(format!("{extent:?} of {cell}")))?;
        crate::testing::read(&reader, extent, &location).await
    }

    /// TASK-702.2: a version's read evidence is kept to each tenant's newest
    /// [`crate::evidence::KEPT`], by every pass, one that skips the cell too, its writes changing
    /// no listing's digest; and goes whole with the version.
    #[test]
    fn versions_evidence_goes_past_each_tenants_newest_and_with_them() {
        block_on(async {
            let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
            let cell = test_cell(1);
            lifecycle::create(&*bucket, cell, 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), cell, 10, 0).await.unwrap();
            let cut = layout::NewVersion { id: 0xa, keep_last: None };
            let cutting = SegmentBuilder::from(Commit { tag: 1, ..Commit::default() });
            let at = writer
                .add_segment(
                    cutting.cut(cut, &mut writer.chain()).frame(writer.keys().encrypting()),
                )
                .unwrap();
            writer.upload(at).await.unwrap();
            writer.acknowledge().await.unwrap();
            let epoch = writer.epoch();
            let of =
                |tenant| crate::evidence::Of { origin: Origin { parent: cell, at, epoch }, tenant };
            let write = |tenant, count| {
                let (bucket, writer) = (bucket.clone(), &writer);
                async move {
                    for _ in 0..count {
                        let encrypting = writer.keys().encrypting_as(cell.id).unwrap();
                        crate::evidence::write(&*bucket, of(tenant), &[], encrypting)
                            .await
                            .unwrap();
                    }
                }
            };
            let count = |tenant| {
                let bucket = bucket.clone();
                async move { crate::bucket::list(&*bucket, &of(tenant).prefix()).await.unwrap().len() }
            };
            let kept = crate::evidence::KEPT;
            let mut collector = Collector::new();
            let mut sweep = 0;
            let mut collect = async || {
                sweep += 1;
                collector.collect(&*bucket, sweep).await.unwrap()
            };
            collect().await;
            assert_eq!(collect().await.skipped, 1);
            write(5, kept + 2).await;
            write(6, 1).await;
            let report = collect().await;
            assert_eq!((report.skipped, report.deleted), (1, 2), "{report:?}");
            assert_eq!((count(5).await, count(6).await), (kept, 1));
            assert_eq!(collect().await.skipped, 1);
            writer.drop_versions(&BTreeSet::from([0xa])).await.unwrap();
            let report = collect().await;
            assert_eq!((report.skipped, report.deleted), (0, kept + 1), "{report:?}");
            assert_eq!((count(5).await, count(6).await), (0, 0));
        });
    }

    /// TASK-476: a pass over a cell of many versions reads each segment's footer twice at most,
    /// once for the cell's state and once for all its versions' states, walked to at once
    /// (`recovery::keys_at`), not once a version; and each version's checkpoint at its key, by its
    /// tail, a ranged GET, which none has. 16 versions cut after a commit each, no checkpoint: 33 segments, whose
    /// footers a pass recovering each version alone read 321 times, in 52 GETs besides
    /// (`versions-pass-bench` shows how that grew).
    #[test]
    fn passes_read_a_cells_footers_once_for_all_its_versions() {
        let (segments, ranged, gets) = block_on(pass_over_versions(16));
        assert_eq!(segments, 33);
        // cladefs: its three reads of the cell's head; the walk to its versions reads none again,
        // going by the one its recovery read (board 8543).
        assert_eq!((ranged, gets), (2 * segments + 16, 5));
    }

    /// A pass over a cell with `versions` versions, each cut after a commit of an
    /// extent: how many segments it has, and the ranged GETs, which read footers, and the GETs the
    /// pass made.
    async fn pass_over_versions(versions: u64) -> (u64, u64, u64) {
        let bucket = memory_bucket();
        // Formatted, as a host's bucket is.
        crate::identity::format(&*bucket, &Default::default()).await.unwrap();
        let cell = test_cell(1);
        lifecycle::create(&*bucket, cell, 0).await.unwrap();
        let writer = Writer::open(bucket.clone(), cell, 10, 0).await.unwrap();
        let first = writer.chain().next_sequence();
        let extent = |n| ExtentId::new(cell.id, first + n);
        commit(
            &writer,
            Commit { tag: 1, extents: vec![(extent(0), vec![1])], ..Commit::default() },
        )
        .await;
        for version in 0..versions {
            let (tag, written) = (2 + 2 * version, extent(1 + version));
            let frees = vec![extent(version)];
            let extents = vec![(written, vec![1])];
            commit(&writer, Commit { tag, extents, frees, ..Commit::default() }).await;
            let cut = layout::NewVersion { id: 1 + version, keep_last: None };
            let cutting = SegmentBuilder::from(Commit { tag: tag + 1, ..Commit::default() });
            let objects = cutting.cut(cut, &mut writer.chain()).frame(writer.keys().encrypting());
            let at = writer.add_segment(objects).unwrap();
            writer.upload(at).await.unwrap();
            writer.acknowledge().await.unwrap();
        }
        assert_eq!(writer.versions().await.len() as u64, versions);
        let segments = writer.chain().next_segment();
        let before = bucket.stats().requests;
        let dry = Collector::new().dry_run_cell(&*bucket, cell).await.unwrap();
        assert!(!dry.fenced);
        let after = bucket.stats().requests;
        let made = |request: Request| after[request as usize] - before[request as usize];
        (segments, made(Request::GetRange), made(Request::Get))
    }

    /// TASK-476: of a cell's versions, walked to at once, a later one keeps what the cell
    /// sealed after the first and freed after it, as the first keeps what it names: what a walk
    /// names of the states after its first is what changed since (`recovery::walk_keys`).
    #[test]
    fn later_versions_keep_what_changed_since_the_first() {
        assert_eq!(block_on(read_the_second_version()), Ok(b"later".to_vec()));
    }

    /// What the second of two versions of a cell reads of an extent the cell sealed on its own
    /// between them and freed after both, the cell checkpointed past them, sweeps having run until
    /// they did nothing more: read by a reader of another host's.
    async fn read_the_second_version() -> Result<Vec<u8>, Error> {
        let objects: Arc<InMemory> = Arc::new(InMemory::new());
        let bucket = Arc::new(ObjectBucket::new(objects.clone(), ""));
        let cell = test_cell(1);
        lifecycle::create(&*bucket, cell, 0).await.unwrap();
        let writer = Writer::open(bucket.clone(), cell, 10, 0).await.unwrap();
        let first = writer.chain().next_sequence();
        let (held, later) = (ExtentId::new(1, first), ExtentId::new(1, first + 1));
        let cut = async |tag, id| {
            let cut = layout::NewVersion { id, keep_last: None };
            let cutting = SegmentBuilder::from(Commit { tag, ..Commit::default() });
            let objects = cutting.cut(cut, &mut writer.chain()).frame(writer.keys().encrypting());
            let at = writer.add_segment(objects).unwrap();
            writer.upload(at).await.unwrap();
            writer.acknowledge().await.unwrap();
            at
        };
        let extents = vec![(held, b"held".to_vec())];
        commit(&writer, Commit { tag: 1, extents, ..Commit::default() }).await;
        cut(2, 0xa).await;
        // On its own, in an object no recovery reads: kept only by the states naming it.
        let own = vec![(later.sequence, b"later".to_vec())];
        commit(&writer, Commit { tag: 3, own, ..Commit::default() }).await;
        let at = cut(4, 0xb).await;
        commit(&writer, Commit { tag: 5, frees: vec![held, later], ..Commit::default() }).await;
        writer.checkpoint().await.unwrap();
        sweep(&*bucket).await?;
        let reader = Arc::new(ObjectBucket::new(objects, ""));
        let checkpoints = Checkpoints::new(&*reader, 0);
        let (state, _) = recovery::recover_at(&checkpoints, cell, at).await?;
        let location = state.extents.get(&later).cloned();
        let location = location.ok_or_else(|| Error::Lost(format!("{later:?} of {cell}")))?;
        crate::testing::read(&reader, later, &location).await
    }

    /// TASK-476: a pass gives up on a restore of a version being made, its host dead, once the
    /// version's cell's head names it no more: its record deleted, it keeps nothing, and the
    /// pass, recovering it, fails not.
    #[test]
    fn restores_being_made_of_versions_dropped_are_given_up_on() {
        let (report, record) = block_on(sweep_past_a_restore_of_a_dropped_version());
        assert_eq!(report.map(drop), Ok(()));
        assert!(record.deleted(), "{record:?}: given up");
    }

    /// Without the rule `collector_abandons`, the pass recovers that restore, through what a pass
    /// before reclaimed of the version's cell, and fails.
    #[test]
    fn restores_being_made_of_versions_dropped_given_up_on_need_collector_abandons() {
        crate::set_rules(crate::Rules { collector_abandons: false, ..crate::Rules::ALL });
        let (report, record) = block_on(sweep_past_a_restore_of_a_dropped_version());
        crate::set_rules(crate::Rules::ALL);
        assert!(matches!(report, Err(Error::Lost(_))), "{report:?}");
        assert!(!record.deleted(), "{record:?}");
    }

    /// A restore of a version left being made, its host dead before its reference, the version then
    /// dropped and its cell checkpointed past it, swept: what the sweeps reported, and the
    /// restore's record after.
    async fn sweep_past_a_restore_of_a_dropped_version() -> (Result<(), Error>, Head) {
        let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
        let (cell, clone) = (test_cell(1), test_cell(2));
        lifecycle::create(&*bucket, cell, 0).await.unwrap();
        let writer = Writer::open(bucket.clone(), cell, 10, 0).await.unwrap();
        let cut = layout::NewVersion { id: 0xa, keep_last: None };
        let cutting = SegmentBuilder::from(Commit { tag: 1, ..Commit::default() });
        let at = writer
            .add_segment(cutting.cut(cut, &mut writer.chain()).frame(writer.keys().encrypting()))
            .unwrap();
        writer.upload(at).await.unwrap();
        writer.acknowledge().await.unwrap();
        let origin = Origin { parent: cell, at, epoch: writer.epoch() };
        let forking = Head::of(Made::Forking { origin, version: 0xa }, 0x33);
        crate::testing::make_head(&*bucket, clone, forking).await.unwrap();
        writer.drop_versions(&BTreeSet::from([0xa])).await.unwrap();
        commit(&writer, Commit { tag: 2, ..Commit::default() }).await;
        writer.checkpoint().await.unwrap();
        let swept = sweep(&*bucket).await;
        let record = crate::head::get(&*bucket, clone).await.unwrap().unwrap();
        (swept, record)
    }

    /// What a fork to another cell reads of what it inherited, its parent having freed it,
    /// checkpointed and been deleted, sweeps having run until they did nothing more: read by a
    /// reader of another host's, whose bucket kept none of what it read decoded (TASK-407).
    async fn read_forked_from_a_deleted_parent() -> Result<Vec<u8>, Error> {
        let objects: Arc<InMemory> = Arc::new(InMemory::new());
        let bucket = Arc::new(ObjectBucket::new(objects.clone(), ""));
        let (parent, child) = (test_cell(1), test_cell(2));
        lifecycle::create(&*bucket, parent, 0).await.unwrap();
        let writer = Writer::open(bucket.clone(), parent, 10, 0).await.unwrap();
        let extent = ExtentId::new(1, writer.chain().next_sequence());
        commit_and_fork(&writer, vec![(extent, b"held".to_vec())], forked(child)).await;
        commit(&writer, Commit { tag: 3, frees: vec![extent], ..Commit::default() }).await;
        writer.checkpoint().await.unwrap();
        drop(writer);
        lifecycle::delete(&*bucket, parent, 20, 0).await.unwrap();
        sweep(&*bucket).await?;
        read_inherited(&Arc::new(ObjectBucket::new(objects, "")), child, extent).await
    }

    /// TASK-395.3, TASK-395.4: a chain of forks across cells, a root's, its child's, its
    /// grandchild's: the child deleted, its cell keeps its own state at the grandchild's fork
    /// version, and its reference under the root's cell stays, the child's cell holding the
    /// grandchild's; the root frees and checkpoints, and the grandchild, with no checkpoint, reads
    /// what it inherited through both. The grandchild deleted too, the passes retire the
    /// references down the chain, and the cells of the dead are reclaimed whole but their heads and
    /// the retired markers of their references (TASK-395.13): no reference is left, and the
    /// root's cell holds its own alone.
    #[test]
    fn references_hold_while_their_childrens_cells_hold_references() {
        let (read, bucket) = block_on(read_through_a_deleted_child());
        assert_eq!(read, Ok(b"held".to_vec()));
        block_on(async {
            let grandchild = test_cell(3);
            lifecycle::delete(&*bucket, grandchild, 22, 0).await.unwrap();
            sweep(&*bucket).await.unwrap();
        });
        let left = block_on(keys(&*bucket));
        let references: Vec<&String> = left.iter().filter(|key| key.contains("/refs/")).collect();
        assert_eq!(references, Vec::<&String>::new());
        for cell in [2, 3] {
            let prefix = layout::CellId { tenant: 1, id: cell }.prefix();
            let kept: Vec<&String> = (left.iter())
                .filter(|key| key.starts_with(&prefix))
                .filter(|key| {
                    !matches!(layout::kind(key), Kind::Head { .. } | Kind::Retired { .. })
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
        let bucket = memory_bucket();
        let (root, child, grandchild) = (test_cell(1), test_cell(2), test_cell(3));
        lifecycle::create(&*bucket, root, 0).await.unwrap();
        let writer = Writer::open(bucket.clone(), root, 10, 0).await.unwrap();
        let extent = ExtentId::new(1, writer.chain().next_sequence());
        commit_and_fork(&writer, vec![(extent, b"held".to_vec())], forked(child)).await;
        let forking = Writer::open(bucket.clone(), child, 11, 0).await.unwrap();
        commit_and_fork(&forking, Vec::new(), forked(grandchild)).await;
        drop(forking);
        lifecycle::delete(&*bucket, child, 21, 0).await.unwrap();
        sweep(&*bucket).await.unwrap();
        commit(&writer, Commit { tag: 3, frees: vec![extent], ..Commit::default() }).await;
        writer.checkpoint().await.unwrap();
        let read = match sweep(&*bucket).await {
            Ok(()) => read_inherited(&bucket, grandchild, extent).await,
            Err(error) => Err(error),
        };
        (read, bucket)
    }

    /// TASK-395.3: a clone writes its reference before it reads its version's cell's head: a
    /// clone that read the version live, its cell deleted and swept away as it finishes, keeps
    /// what it inherited, by its reference.
    #[test]
    fn clones_keep_what_they_inherit_as_their_versions_go() {
        assert_eq!(block_on(read_as_a_version_goes_under_a_clone()), Ok(b"held".to_vec()));
    }

    /// The clone's reference written once it is made, as specs/bucket's NoForksReferFirst has
    /// it: the sweeps between find neither the version nor a reference.
    #[test]
    fn clones_keeping_what_they_inherit_as_their_versions_go_need_clones_refer_first() {
        crate::set_rules(crate::Rules { clones_refer_first: false, ..crate::Rules::ALL });
        let read = block_on(read_as_a_version_goes_under_a_clone());
        crate::set_rules(crate::Rules::ALL);
        assert!(matches!(read, Err(Error::Lost(_))), "{read:?}");
    }

    /// What a clone in cell 3 of a version of a cell in cell 1 reads of what it inherited, the
    /// cell deleted, and a pass run over its cell, as the clone makes its record made, having
    /// read the version live. Not the clone's: its pass would give the clone up, its version gone,
    /// which loses nothing.
    async fn read_as_a_version_goes_under_a_clone() -> Result<Vec<u8>, Error> {
        use crate::layout::Origin;
        let direct = Arc::new(Direct(SimBucket::new()));
        use crate::testing::{VERSION, fork_version, versioned_by};
        let (origin_cell, clone) = (test_cell(1), test_cell(3));
        lifecycle::create(&*direct, origin_cell, 0).await.unwrap();
        let writer = Writer::open(direct.clone(), origin_cell, 10, 0).await.unwrap();
        let extent = ExtentId::new(1, writer.chain().next_sequence());
        let at = versioned_by(&writer, vec![(extent, b"held".to_vec())], VERSION).await;
        let epoch = writer.epoch();
        drop(writer);
        let origin = Origin { parent: origin_cell, at, epoch };
        // Before the update that makes the fork, its record's second put.
        let bucket = Hooked::new(direct.clone(), Hook::Putting(layout::head(clone), 2));
        let sweeping = direct.clone();
        *bucket.then.lock().unwrap() = Some(Box::new(move || {
            let deleted = async {
                lifecycle::delete(&*sweeping, origin_cell, 21, 0).await.unwrap();
                collect_cell(&*sweeping, origin_cell, 1).await.unwrap();
            };
            deleted.now_or_never().unwrap();
        }));
        fork_version(&bucket, clone, VERSION, origin, 0x33).await?;
        assert!(bucket.then.lock().unwrap().is_none(), "the sweep ran");
        sweep(&*direct).await?;
        read_inherited(&direct, clone, extent).await
    }

    /// Cells numbered from 0x100, `count` of them, each a cell of its own, `garbage` freed in it
    /// and checkpointed past: what a pass over each deletes.
    async fn garbage_cells(bucket: &Arc<SimBucket>, count: u64) {
        for cell in 0x100..0x100 + count {
            let cell = test_cell(cell);
            lifecycle::create(&**bucket, cell, 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), cell, 10, 0).await.unwrap();
            let extent = ExtentId::new(cell.id, writer.chain().next_sequence());
            let extents = vec![(extent, b"garbage".to_vec())];
            commit(&writer, Commit { tag: 1, extents, ..Commit::default() }).await;
            commit(&writer, Commit { tag: 2, frees: vec![extent], ..Commit::default() }).await;
            writer.checkpoint().await.unwrap();
        }
    }

    /// TASK-395.4: a pass over a cell, its one cell a fork of a cell in another cell, deleted,
    /// in a bucket of `others` cells more: the requests it makes, and the objects it deleted.
    fn pass_over_a_dead_fork(others: u64) -> (Vec<String>, Report, CellId, CellId, u64) {
        let bucket = SimBucket::new();
        let (parent, child) = (test_cell(1), test_cell(2));
        let at = bucket
            .served(async {
                garbage_cells(&bucket, others).await;
                lifecycle::create(&*bucket, parent, 0).await.unwrap();
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
                lifecycle::delete(&*bucket, child, 21, 0).await.unwrap();
                at
            })
            .0;
        let (report, asked) = bucket.served(collect_cell(&*bucket, child, 7));
        (asked, report.unwrap(), parent, child, at)
    }

    /// TASK-395.4's AC #1: a pass over a cell lists nothing outside the cell's prefix, writes and
    /// deletes nothing outside it but the one reference it retires, of its deleted child's fork
    /// across cells, and its mark (TASK-395.13); and what it asks is the same in a bucket of many
    /// more cells: it grows with the cell, not the bucket.
    #[test]
    fn passes_over_a_cell_list_and_delete_within_it_alone() {
        let (asked, report, parent, child, at) = pass_over_a_dead_fork(3);
        let prefix = child.prefix();
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
    /// cell's recovery lists nothing: its head names its checkpoint and its segments (TASK-407).
    /// Before: 7 LISTs a cell, the records listed twice more and each cell's checkpoints once
    /// more. The passes counted are the second over each cell, in full as the first deleted,
    /// holding its lease, so that what the first asks before taking the lease, finding none
    /// (`passable`), is no part of them.
    #[test]
    fn passes_list_their_cell_once_and_what_the_spec_relists() {
        let bucket = SimBucket::new();
        let (one, three) = (test_cell(0x100), test_cell(0x201));
        bucket.served(async {
            garbage_cells(&bucket, 1).await;
            for id in 0x201..=0x203 {
                let cell = test_cell(id);
                lifecycle::create(&*bucket, cell, 0).await.unwrap();
                let writer = Writer::open(bucket.clone(), cell, 10, 0).await.unwrap();
                let extent = ExtentId::new(id, writer.chain().next_sequence());
                let extents = vec![(extent, b"garbage".to_vec())];
                commit(&writer, Commit { tag: 1, extents, ..Commit::default() }).await;
                commit(&writer, Commit { tag: 2, frees: vec![extent], ..Commit::default() }).await;
                writer.checkpoint().await.unwrap();
            }
        });
        let lists = |cell: CellId| -> Vec<String> {
            let (first, _) = bucket.served(collect_cell(&*bucket, cell, 1));
            assert!(first.unwrap().deleted > 0, "the first pass deletes, changing the cell");
            let (second, asked) = bucket.served(collect_cell(&*bucket, cell, 2));
            assert_eq!(second.unwrap().skipped, 0, "{asked:#?}");
            asked.into_iter().filter(|asked| asked.starts_with("list")).collect()
        };
        let listed = |cell: CellId| -> Vec<String> {
            vec![
                format!("list {}", cell.prefix()),
                format!("list-delimited {}", layout::references(cell)),
            ]
        };
        assert_eq!(lists(one), listed(one));
        // However many cells: their recoveries list nothing.
        assert_eq!(lists(three), listed(three));
    }

    /// TASK-395.4's AC #2: an origin many references name, a version cloned many times, is
    /// recovered once a pass: the pass over its cell asks the same, its references listed by
    /// delimiter, however many clones there are.
    #[test]
    fn fork_versions_shared_by_many_references_are_recovered_once() {
        use crate::layout::Origin;
        let asked = |clones: u64| {
            let bucket = SimBucket::new();
            use crate::testing::{VERSION, fork_version, versioned_by};
            let origin_cell = test_cell(1);
            let at = bucket
                .served(async {
                    lifecycle::create(&*bucket, origin_cell, 0).await.unwrap();
                    let writer = Writer::open(bucket.clone(), origin_cell, 10, 0).await.unwrap();
                    let extent = ExtentId::new(1, writer.chain().next_sequence());
                    let at = versioned_by(&writer, vec![(extent, vec![1; 64])], VERSION).await;
                    commit(&writer, Commit { tag: 3, frees: vec![extent], ..Commit::default() })
                        .await;
                    writer.checkpoint().await.unwrap();
                    let origin = Origin { parent: origin_cell, at, epoch: writer.epoch() };
                    for clone in 0..clones {
                        let clone = test_cell(0x10 + clone);
                        fork_version(&*bucket, clone, VERSION, origin, clone.id).await.unwrap();
                    }
                    at
                })
                .0;
            let (report, asked) = bucket.served(collect_cell(&*bucket, origin_cell, 7));
            report.unwrap();
            // The origin's state, the checkpoint at it the first clone into another cell wrote:
            // by its tail, never whole.
            let checkpoint = layout::checkpoint(origin_cell, at, 1);
            let whole = format!("get {checkpoint}");
            assert!(!asked.contains(&whole), "{asked:#?}");
            let tail = format!("get {checkpoint} last {}", crate::object::TAIL);
            let read = asked.iter().filter(|asked| **asked == tail).count();
            (asked.len(), read)
        };
        let (one, read) = asked(1);
        assert_eq!(read, 1, "the origin's checkpoint's tail");
        assert_eq!(asked(8), (one, read));
    }

    /// TASK-395.12: a pass over a cell whose cell was forked at many versions to other cells, a VM
    /// snapshotted many times, lists none of the cell's checkpoints: its recovery reads what its
    /// head names (TASK-407), and the listing of the cell serves each origin's; and it reads each
    /// origin's checkpoint, which the restore into another cell wrote, once. Before, it listed them
    /// once after the cell's segments (`newest_checkpoint`), and before that once more a fork
    /// version.
    #[test]
    fn passes_list_a_cells_checkpoints_once_however_many_versions_it_was_forked_at() {
        let parent = test_cell(1);
        let asked = |versions: u64| {
            let bucket = SimBucket::new();
            let (ats, _) = bucket.served(async {
                lifecycle::create(&*bucket, parent, 0).await.unwrap();
                let writer = Writer::open(bucket.clone(), parent, 10, 0).await.unwrap();
                let mut ats = Vec::new();
                for n in 0..versions {
                    let extent = ExtentId::new(1, writer.chain().next_sequence());
                    let id = 0x10 + n;
                    let at = crate::testing::versioned_by(&writer, vec![(extent, vec![1; 64])], id)
                        .await;
                    let origin = layout::Origin { parent, at, epoch: writer.epoch() };
                    crate::testing::fork_version(&*bucket, test_cell(id), id, origin, 0x22)
                        .await
                        .unwrap();
                    ats.push(at);
                }
                // Written on, its own checkpoint past the last origin's.
                commit(&writer, Commit { tag: 3, ..Commit::default() }).await;
                writer.checkpoint().await.unwrap();
                ats
            });
            let (report, asked) = bucket.served(collect_cell(&*bucket, parent, 7));
            report.unwrap();
            let count = |request: String| asked.iter().filter(|asked| **asked == request).count();
            let listed = count(format!("list {}", layout::checkpoints(parent)));
            // By its tail, never whole.
            let read = |&at: &u64| {
                let checkpoint = layout::checkpoint(parent, at, 1);
                assert_eq!(count(format!("get {checkpoint}")), 0, "{asked:#?}");
                count(format!("get {checkpoint} last {}", crate::object::TAIL))
            };
            let read: Vec<usize> = ats.iter().map(read).collect();
            (listed, read)
        };
        assert_eq!(asked(1), (0, vec![1]));
        assert_eq!(asked(6), (0, vec![1; 6]));
    }

    /// TASK-407: the checkpoint at an origin to another cell, which the fork's child reads by
    /// its key alone, of the epoch of the fork's segment, stands through passes over the parent's
    /// cell, though a later writer checkpointed at the same segment, at its own key, which the
    /// pass's recovery of the origin reads instead. Before, the pass kept the later writer's
    /// alone, deleting the child's, which then failed to open (the simulation's `everything`).
    #[test]
    fn fork_versions_to_other_cells_keep_their_checkpoints_past_later_writers() {
        use crate::layout::Origin;
        let bucket = memory_bucket();
        let (parent, child) = (test_cell(1), test_cell(2));
        let at = block_on(async {
            lifecycle::create(&*bucket, parent, 0).await.unwrap();
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
        for pass in 1..=2 {
            block_on(collect_cell(&*bucket, parent, pass)).unwrap();
        }
        assert!(block_on(keys(&*bucket)).contains(&forks));
        let origin = Some(Origin { parent, at, epoch: 1 });
        assert!(block_on(recovery::recover(&*bucket, child, origin)).is_ok());
    }

    /// TASK-395.4: a dry run over a dead cell says what reference a pass would retire, deleting it
    /// not; a pass does.
    #[test]
    fn dry_runs_retire_nothing() {
        let bucket = memory_bucket();
        let (parent, child) = (test_cell(1), test_cell(2));
        let at = block_on(async {
            lifecycle::create(&*bucket, parent, 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), parent, 10, 0).await.unwrap();
            let at = commit_and_fork(&writer, Vec::new(), forked(child)).await;
            lifecycle::delete(&*bucket, child, 21, 0).await.unwrap();
            at
        });
        let reference = layout::reference(parent, at, child);
        let collector = Collector::new();
        let dry = block_on(collector.dry_run_cell(&*bucket, child)).unwrap();
        assert_eq!(dry.retired, std::slice::from_ref(&reference));
        assert!(block_on(keys(&*bucket)).contains(&reference));
        let report = block_on(collector.collect_cell(&*bucket, child, 2)).unwrap();
        assert_eq!(report.retired, 1);
        assert!(!block_on(keys(&*bucket)).contains(&reference));
    }

    /// TASK-395.4's AC #1, a live fork's cell (TASK-395.4's review): a pass over the cell of a
    /// fork of a cell in another cell, the fork live with no checkpoint of its own, lists nothing
    /// outside its cell, reading its parent's state by the checkpoint at the origin alone,
    /// which recording the fork wrote; whether the parent had a checkpoint before the fork, or
    /// none. The fork reads what it inherited.
    #[test]
    fn passes_over_a_live_forks_cell_list_nothing_of_its_parents() {
        for checkpointed in [true, false] {
            let bucket = SimBucket::new();
            let (parent, child) = (test_cell(1), test_cell(2));
            let ((extent, at, epoch), _) = bucket.served(async {
                lifecycle::create(&*bucket, parent, 0).await.unwrap();
                let writer = Writer::open(bucket.clone(), parent, 10, 0).await.unwrap();
                let extent = ExtentId::new(1, writer.chain().next_sequence());
                if checkpointed {
                    commit(&writer, Commit { tag: 9, ..Commit::default() }).await;
                    writer.checkpoint().await.unwrap();
                }
                let held = vec![(extent, b"held".to_vec())];
                (extent, commit_and_fork(&writer, held, forked(child)).await, writer.epoch())
            });
            let (report, asked) = bucket.served(collect_cell(&*bucket, child, 7));
            report.unwrap();
            let prefix = child.prefix();
            let outside: Vec<&String> = (asked.iter())
                .filter(|asked| asked.starts_with("list"))
                .filter(|asked| !asked.split(' ').nth(1).unwrap().starts_with(&prefix))
                .collect();
            assert_eq!(outside, Vec::<&String>::new(), "checkpointed {checkpointed}");
            let checkpoint = layout::checkpoint(parent, at, epoch);
            let tail = format!("get {checkpoint} last {}", crate::object::TAIL);
            assert!(asked.contains(&tail), "{asked:#?}");
            let direct = Arc::new(Direct(bucket.clone()));
            assert_eq!(block_on(read_inherited(&direct, child, extent)), Ok(b"held".to_vec()));
        }
    }

    /// TASK-395.10: a clone refused by a record of its child's ID there before, of another origin,
    /// deleted with none, or of its own origin, writes no reference, as it writes it after its own
    /// record. Each another's, its ID taken: a deleted one too, which is no version deleted
    /// (TASK-449: no look at the record comes first any more, which refused it so).
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
        use crate::testing::{VERSION, fork_version};
        let bucket = Direct(SimBucket::new());
        let origin = Origin { parent: test_cell(1), at: 3, epoch: 1 };
        let other = Origin { parent: test_cell(1), at: 4, epoch: 1 };
        let found = |made: Head| made;
        let cases = [
            (test_cell(3), found(Head::of(Made::Clone(other), 0x44)), Error::IdTaken(test_cell(3))),
            (
                test_cell(4),
                found(Head { epoch: Head::FINAL, ..Head::of(Made::Own, 0x45) }),
                Error::IdTaken(test_cell(4)),
            ),
            (
                test_cell(5),
                found(Head::of(Made::Clone(origin), 0x46)),
                Error::IdTaken(test_cell(5)),
            ),
        ];
        cases.map(|(clone, there, refused)| {
            block_on(crate::testing::make_head(&bucket, clone, there)).unwrap();
            let forked = block_on(fork_version(&bucket, clone, VERSION, origin, 0x33));
            assert_eq!(forked, Err(refused), "{clone}");
            let reference = layout::reference(origin.parent, origin.at, clone);
            block_on(bucket::get(&bucket, &reference)).unwrap().is_some()
        })
    }

    /// TASK-395.4's review: a pass over a cell holding nothing takes no lease there, which would be
    /// its one object; one over a cell of another format's head refuses before taking it.
    #[test]
    fn passes_take_no_lease_of_cells_empty_or_of_other_formats() {
        let bucket = Direct(SimBucket::new());
        let (empty, older) =
            (layout::CellId { tenant: 1, id: 7 }, layout::CellId { tenant: 1, id: 8 });
        assert_eq!(block_on(collect_cell(&bucket, empty, 1)), Ok(Report::default()));
        let cell = older;
        let mut format8 = 8u32.to_le_bytes().to_vec();
        format8.extend_from_slice(b"what format 8 held");
        block_on(bucket::create(&bucket, &layout::head(cell), format8)).unwrap();
        let refused = block_on(collect_cell(&bucket, older, 1));
        assert_eq!(refused, Err(Error::Format { cell, format: 8 }));
        let leases: Vec<String> = (bucket.0.keys().into_iter())
            .filter(|key| matches!(layout::kind(key), Kind::CollectorLease { .. }))
            .collect();
        assert_eq!(leases, Vec::<String>::new());
    }

    /// doc-27: a cell holding the head slots of format 28, which nothing of this format
    /// reads, is refused, and nothing of it deleted.
    #[test]
    fn cells_holding_head_slots_of_format_28_are_refused() {
        let bucket = memory_bucket();
        let cell = test_cell(1);
        block_on(async {
            lifecycle::create(&*bucket, cell, 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), cell, 10, 0).await.unwrap();
            let extent = ExtentId::new(1, writer.chain().next_sequence());
            let extents = vec![(extent, b"held".to_vec())];
            commit(&writer, Commit { tag: 1, extents, ..Commit::default() }).await;
            let slot = format!("{}head/0000000000003", cell.prefix());
            assert_eq!(layout::kind(&slot), Kind::Former { cell, format: 28 });
            let created = bucket::create(&*bucket, &slot, b"format 28's head slot".to_vec());
            assert!(matches!(created.await.unwrap(), Created::Created(_)));
        });
        let before = block_on(keys(&*bucket));
        assert_eq!(block_on(collect(&*bucket, 1)), Err(Error::Format { cell, format: 28 }));
        assert_eq!(block_on(keys(&*bucket)), before);
    }

    /// TASK-395.10: a clone's host dying at each step of the clone, before its reference, before
    /// its read of the version, before it makes its record made, its directory then deleted
    /// knowing nothing of the child, as a host's left unmade does: once the version and its origin
    /// are deleted and passes ran, nothing is left of either, nor of the clone but its record,
    /// deleted, which names the origin its reference was retired by.
    #[test]
    fn clones_whose_hosts_die_leave_nothing_once_their_versions_go() {
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
    /// version whose host died before the `nth` request of its `step` (its record's put, its
    /// reference's, the read of its version's cell's head), that cell then deleted and passes
    /// run.
    fn left_by_a_clone_dying_before(step: &str, nth: usize) -> (Vec<String>, usize) {
        use crate::layout::Origin;
        let bucket = SimBucket::new();
        use crate::testing::{VERSION, fork_version, versioned_by};
        let (origin_cell, clone) = (test_cell(1), test_cell(3));
        let origin = bucket
            .served(async {
                lifecycle::create(&*bucket, origin_cell, 0).await.unwrap();
                let writer = Writer::open(bucket.clone(), origin_cell, 10, 0).await.unwrap();
                let extent = ExtentId::new(1, writer.chain().next_sequence());
                let at = versioned_by(&writer, vec![(extent, b"held".to_vec())], VERSION).await;
                Origin { parent: origin_cell, at, epoch: writer.epoch() }
            })
            .0;
        let request = match step {
            "reference" => format!("put {}", layout::reference(origin.parent, origin.at, clone)),
            "read" => format!("get {}", layout::head(origin_cell)),
            _ => format!("put {}", layout::head(clone)),
        };
        let forking = fork_version(&*bucket, clone, VERSION, origin, 0x33);
        let (asked, died) = bucket.died_before(forking, (&request, nth));
        assert!(died, "{step} {nth}: {asked:#?}");
        let direct = Direct(bucket.clone());
        block_on(async {
            lifecycle::delete(&direct, origin_cell, 21, 0).await.unwrap();
            sweep(&direct).await.unwrap();
            let left = crate::testing::uncollected(&direct).await;
            let references =
                keys(&direct).await.iter().filter(|key| key.contains("/refs/")).count();
            (left, references)
        })
    }

    /// TASK-395.7: a clone of a shared version into another tenant's cell writes its reference
    /// under the cell of the version's cell, its tenant's, as any clone does; that cell deleted,
    /// passes keep what the clones inherited, which they read; once the clones go, their references
    /// are retired, and nothing of any is left but heads.
    #[test]
    fn clones_of_shared_versions_in_other_tenants_keep_what_they_inherit_by_references() {
        use crate::testing::{VERSION, fork_version, versioned_by};
        block_on(async {
            let direct = Arc::new(Direct(SimBucket::new()));
            let origin_cell = test_cell(1);
            let theirs = |id| CellId { tenant: 2, id };
            lifecycle::create(&*direct, origin_cell, 0).await.unwrap();
            let writer = Writer::open(direct.clone(), origin_cell, 10, 0).await.unwrap();
            let extent = ExtentId::new(1, writer.chain().next_sequence());
            let at = versioned_by(&writer, vec![(extent, b"held".to_vec())], VERSION).await;
            writer.share_version(VERSION, true).await.unwrap();
            let origin = Origin { parent: origin_cell, at, epoch: writer.epoch() };
            drop(writer);
            let clones = [theirs(3), theirs(4)];
            for clone in clones {
                fork_version(&*direct, clone, VERSION, origin, clone.id).await.unwrap();
                let reference = layout::reference(origin_cell, at, clone);
                assert!(reference.starts_with(&origin_cell.prefix()), "{reference}");
                assert!(keys(&*direct).await.contains(&reference), "{reference}");
            }
            lifecycle::delete(&*direct, origin_cell, 21, 0).await.unwrap();
            for _ in 0..2 {
                sweep(&*direct).await.unwrap();
                for clone in clones {
                    let read = read_inherited(&direct, clone, extent).await;
                    assert_eq!(read, Ok(b"held".to_vec()), "{clone}");
                }
            }
            for clone in clones {
                lifecycle::delete(&*direct, clone, 30, u64::MAX).await.unwrap();
            }
            sweep(&*direct).await.unwrap();
            let left = keys(&*direct).await;
            assert!(!left.iter().any(|key| key.contains("/refs/")), "{left:#?}");
            assert_eq!(crate::testing::uncollected(&*direct).await, Vec::<String>::new());
        });
    }

    // cladefs: passes that skip cells unchanged since the last pass, and what passes record of the
    // cells (TASK-395.6, TASK-395.8).

    /// The requests of a sweep, `asked`, as [`SimBucket::served`] gives them, by the cell of their
    /// keys; those of no cell's (the listings finding the cells) left out.
    fn by_cell(asked: &[String]) -> BTreeMap<CellId, Vec<String>> {
        let mut cells: BTreeMap<CellId, Vec<String>> = BTreeMap::new();
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
    fn idle(cell: CellId) -> Vec<String> {
        let lease = layout::collector_lease(cell);
        vec![format!("get {lease}"), format!("list {}", cell.prefix())]
    }

    /// TASK-395.6's AC #1 and #3: a sweep over cells unchanged since their last passes asks of each
    /// its lease and one listing, nothing more; a write to any of them, a segment, a reference, a
    /// head a writer takes over (an object written again at its key), a cell deleted,
    /// makes the next pass over it go over it in full, recovering it, and the one after skip it
    /// again once it changes it no more, the other cells skipped all along.
    #[test]
    fn idle_cells_cost_a_listing_until_written() {
        let bucket = SimBucket::new();
        let (parent, child) = (test_cell(1), test_cell(2));
        let writer = bucket
            .served(async {
                garbage_cells(&bucket, 4).await;
                lifecycle::create(&*bucket, parent, 0).await.unwrap();
                let writer = Writer::open(bucket.clone(), parent, 10, 0).await.unwrap();
                let extent = ExtentId::new(1, writer.chain().next_sequence());
                commit_and_fork(&writer, vec![(extent, vec![1; 64])], forked(child)).await;
                writer.checkpoint().await.unwrap();
                writer
            })
            .0;
        bucket.served(crate::testing::collect_until_quiet(&*bucket)).0.unwrap();
        let cells: Vec<CellId> =
            bucket.served(crate::cell::listed(&*bucket).try_collect()).0.unwrap();
        assert_eq!(cells.len(), 6, "{cells:?}");
        let mut collector = Collector::new();
        let mut holder = 0x10;
        let mut sweep = |collector: &mut Collector| {
            holder += 1;
            let (report, asked) = bucket.served(collector.collect(&*bucket, holder));
            report.unwrap();
            by_cell(&asked)
        };
        let all_idle: BTreeMap<CellId, Vec<String>> =
            cells.iter().map(|&cell| (cell, idle(cell))).collect();
        assert_eq!(sweep(&mut collector), all_idle);

        // Each write, the cell it is in, and what the pass over it in full asks that a pass
        // skipping it never does.
        let garbage = |cell: u64| test_cell(cell);
        // At the garbage cell's checkpoint, its state there whole: an origin found lost would
        // have its pass read its children's records, in other cells, and skip it never.
        let at = (bucket.keys().iter())
            .find_map(|key| match layout::kind(key) {
                Kind::Checkpoint { cell, segment, .. } if cell == garbage(0x100) => Some(segment),
                _ => None,
            })
            .unwrap();
        let reference = layout::reference(garbage(0x100), at, test_cell(0x200));
        let taken = garbage(0x103);
        let writes: [(&str, CellId, String); 4] = [
            ("segment", parent, format!("get {}", layout::head(parent))),
            (
                "reference",
                garbage(0x100),
                format!("list-delimited {}", layout::references(garbage(0x100))),
            ),
            ("takeover", taken, format!("get {}", layout::head(taken))),
            ("deletion", garbage(0x102), format!("get {}", layout::head(garbage(0x102)))),
        ];
        for (write, cell, recovered) in writes {
            match write {
                "segment" => {
                    bucket.served(commit(&writer, Commit { tag: 7, ..Commit::default() }));
                }
                // A clone's first step, its host dying after it (TASK-395.10).
                "reference" => {
                    let bytes = layout::encode(&layout::Reference::of(0x200));
                    bucket.served(bucket::create(&*bucket, &reference, bytes)).0.unwrap();
                }
                // A writer's first step taking the cell over: its head, of the epoch after.
                "takeover" => {
                    bucket.served(crate::testing::take(&*bucket, taken, 0x77, 0)).0.unwrap();
                }
                _ => {
                    bucket.served(lifecycle::delete(&*bucket, garbage(0x102), 20, 0)).0.unwrap();
                }
            }
            let asked = sweep(&mut collector);
            assert!(asked[&cell].contains(&recovered), "{write}: {:#?}", asked[&cell]);
            let others = |cells: &BTreeMap<CellId, Vec<String>>| {
                let mut cells = cells.clone();
                cells.remove(&cell);
                cells
            };
            assert_eq!(others(&asked), others(&all_idle), "{write}");
            // Until it changes the cell no more; a deletion's pass removes the dead cell whole
            // (TASK-455).
            let quiet = match write {
                "deletion" => others(&all_idle),
                _ => all_idle.clone(),
            };
            assert_eq!(sweep(&mut collector), quiet, "{write}");
        }
        // The origin the reference names, its cell's pass keeps (TASK-395.10's space).
        let kept = bucket.keys();
        assert!(
            kept.contains(&reference) && kept.contains(&layout::checkpoint(garbage(0x100), at, 1))
        );
    }

    /// TASK-395.6: a pass skips nothing it went by more than the cell for. A clone being made in
    /// the cell, of a version in another, its pass reads the version's record for: deleted then,
    /// with nothing of the cell's changed, the next pass gives the clone up, as a pass in full
    /// does. The cell dead, the pass in full retires the clone's reference, marking it, and passes
    /// that skip the cell after ask its lease and listing alone (TASK-395.14): the reference
    /// written again, by a writer whose read came before, the pass over the parent's cell deletes,
    /// by its mark. Another collector, which remembers nothing, skips it too, by what the cell's
    /// lease recorded.
    #[test]
    fn passes_skip_only_what_the_cell_alone_decides() {
        use crate::layout::Origin;
        let bucket = SimBucket::new();
        use crate::testing::{VERSION, versioned_by};
        let (origin_cell, clone) = (test_cell(1), test_cell(3));
        let (at, writer) = bucket
            .served(async {
                lifecycle::create(&*bucket, origin_cell, 0).await.unwrap();
                let writer = Writer::open(bucket.clone(), origin_cell, 10, 0).await.unwrap();
                let extent = ExtentId::new(1, writer.chain().next_sequence());
                (versioned_by(&writer, vec![(extent, vec![1; 64])], VERSION).await, writer)
            })
            .0;
        let origin = Origin { parent: origin_cell, at, epoch: writer.epoch() };
        // The clone's record, being made: its host died before it was.
        let forking = Head::of(Made::Forking { origin, version: VERSION }, 0x33);
        let reference = layout::reference(origin_cell, at, clone);
        bucket.served(async {
            let bytes = layout::encode(&layout::Reference::of(0x33));
            bucket::create(&*bucket, &reference, bytes).await.unwrap();
            crate::testing::make_head(&*bucket, clone, forking).await.unwrap();
        });
        let mut collector = Collector::new();
        let pass = |collector: &mut Collector, holder| {
            let (report, asked) = bucket.served(collector.collect_cell(&*bucket, clone, holder));
            (report.unwrap(), asked)
        };
        for holder in 1..=2 {
            let (_, asked) = pass(&mut collector, holder);
            let read = format!("get {}", layout::head(origin_cell));
            assert!(asked.contains(&read), "{holder}: {asked:#?}");
        }
        bucket.served(writer.drop_versions(&BTreeSet::from([VERSION]))).0.unwrap();
        pass(&mut collector, 3);
        let record = bucket.served(crate::head::get(&*bucket, clone)).0.unwrap().unwrap();
        assert!(record.deleted(), "{record:?}");

        // Settled: the clone given up on, the cell dead, a pass in full retires the clone's
        // reference, marked; the passes after skip the cell, asking nothing more of it.
        let (report, _) = pass(&mut collector, 4);
        assert_eq!(report.retired, 1);
        let mark = layout::retired(origin_cell, at, clone);
        assert!(!bucket.keys().contains(&reference));
        assert!(bucket.keys().contains(&mark));
        let skipped = Report { skipped: 1, ..Report::default() };
        let (report, asked) = pass(&mut collector, 5);
        assert_eq!((report, asked), (skipped, idle(clone)));
        // Written again, as a writer whose read came before writes it (the simulation's restores,
        // seed 53): the dead cell's passes skip it still; its parent's cell's pass deletes it.
        bucket.served(async {
            let bytes = layout::encode(&layout::Reference::of(0x33));
            bucket::create(&*bucket, &reference, bytes).await.unwrap();
        });
        let (report, asked) = pass(&mut collector, 6);
        assert_eq!((report, asked), (skipped, idle(clone)));
        assert!(bucket.keys().contains(&reference));
        let (report, _) = bucket.served(collector.collect_cell(&*bucket, origin_cell, 9));
        assert_eq!(report.unwrap().retired, 1);
        assert!(!bucket.keys().contains(&reference));
        assert!(bucket.keys().contains(&mark));
        let other = Collector::new();
        let (report, asked) = bucket.served(other.collect_cell(&*bucket, clone, 7));
        assert_eq!((report.unwrap(), asked), (skipped, idle(clone)));
    }

    /// Collector reaps a shredded tenant's live cells, read evidence (board 2940), and orphaned
    /// ephemeral references once `TenantShredded` is published to `events`, while preserving
    /// cross-tenant forks, `tenants/<T>/kek` tombstones, and `events` itself.
    #[test]
    fn collector_reaps_shredded_tenant_while_preserving_cross_tenant_forks() {
        let bucket = memory_bucket();
        block_on(async {
            // Tenant 5 creates a shared base cell with one version, and an unshared cell.
            let shared = CellId { tenant: 5, id: (1 << 48) | 1 };
            let unshared = CellId { tenant: 5, id: (1 << 48) | 2 };
            // Tenant 6 creates a base cell (whose read evidence & ephemeral ref tenant 5 holds)
            // and a cross-tenant fork of tenant 5's shared version.
            let other_base = CellId { tenant: 6, id: (1 << 48) | 3 };
            let cross_fork = CellId { tenant: 6, id: (1 << 48) | 4 };

            for cell in [shared, unshared, other_base] {
                lifecycle::create(&*bucket, cell, 1).await.unwrap();
            }

            let w_shared = Writer::open(bucket.clone(), shared, 10, 1).await.unwrap();
            let batch = SegmentBuilder::from(Commit {
                tag: 1,
                extents: vec![(ExtentId::new(shared.id, 1), b"shared-data".to_vec())],
                ..Commit::default()
            });
            let cut = layout::NewVersion { id: 0xaa, keep_last: None };
            let at = w_shared
                .add_segment(
                    batch.cut(cut, &mut w_shared.chain()).frame(w_shared.keys().encrypting()),
                )
                .unwrap();
            w_shared.upload(at).await.unwrap();
            w_shared.acknowledge().await.unwrap();
            w_shared.share_version(0xaa, true).await.unwrap();
            let shared_origin = Origin { parent: shared, at, epoch: w_shared.epoch() };

            // Cross-tenant fork into tenant 6 from tenant 5's shared version.
            let kms = crate::keys::testing::host_kms();
            lifecycle::fork_version(
                &*bucket,
                cross_fork,
                0xaa,
                shared_origin,
                cross_fork.id,
                crate::keys::Parents::Kms(&kms, None),
            )
            .await
            .unwrap();

            // Unshared cell owned by tenant 5 with data.
            let w_unshared = Writer::open(bucket.clone(), unshared, 11, 1).await.unwrap();
            commit(
                &w_unshared,
                Commit {
                    tag: 1,
                    extents: vec![(ExtentId::new(unshared.id, 1), b"victim-data".to_vec())],
                    ..Commit::default()
                },
            )
            .await;

            // Other tenant's base cell with read evidence written by tenant 5 and an ephemeral
            // reference owned by tenant 5.
            let w_other = Writer::open(bucket.clone(), other_base, 12, 1).await.unwrap();
            let other_at = {
                let batch = SegmentBuilder::from(Commit {
                    tag: 1,
                    extents: vec![(ExtentId::new(other_base.id, 1), b"other-data".to_vec())],
                    ..Commit::default()
                });
                let cut = layout::NewVersion { id: 0xbb, keep_last: None };
                let seg = w_other
                    .add_segment(
                        batch.cut(cut, &mut w_other.chain()).frame(w_other.keys().encrypting()),
                    )
                    .unwrap();
                w_other.upload(seg).await.unwrap();
                w_other.acknowledge().await.unwrap();
                seg
            };
            let other_origin = Origin { parent: other_base, at: other_at, epoch: w_other.epoch() };
            let ev_key = crate::evidence::write(
                &*bucket,
                crate::evidence::Of { origin: other_origin, tenant: 5 },
                &[crate::evidence::Unit {
                    extent: ExtentId::new(other_base.id, 1),
                    mib: 0,
                    first: 0,
                }],
                w_other.keys().encrypting(),
            )
            .await
            .unwrap();
            let eph_child = CellId { tenant: 5, id: (1 << 48) | 99 };
            lifecycle::refer_ephemeral(&*bucket, other_origin, 0x77, eph_child).await.unwrap();

            // Shred tenant 5: tombstone KEK and publish TenantShredded to events.
            crate::bucket_kms::destroy(&*bucket, 5).await.unwrap();
            crate::events::publish(&*bucket, crate::events::Publish::TenantShredded(5))
                .await
                .unwrap();

            // Run collector: should end tenant 5's cells, remove unshared cell whole, delete
            // tenant 5's evidence under other_base, retire tenant 5's ephemeral ref, keep
            // cross_fork readable, and preserve both `tenants/<5>/kek` and `events`.
            let mut collector = Collector::new();
            let report = collector.collect(&*bucket, 1).await.unwrap();
            assert!(report.deleted > 0, "{report:?}");
            assert!(report.removed >= 1, "unshared cell removed: {report:?}");
            assert!(report.retired >= 1, "ephemeral ref retired: {report:?}");

            let all_keys = keys(&*bucket).await;
            assert!(all_keys.contains(&layout::kek(5)), "tombstone must remain");
            assert!(all_keys.contains(&layout::EVENTS.to_owned()), "events must remain");
            assert!(!all_keys.contains(&ev_key), "shredded tenant evidence deleted");
            assert!(
                !all_keys.iter().any(|k| k.starts_with(&unshared.prefix())),
                "unshared cell completely removed"
            );
            assert!(
                !all_keys
                    .contains(&layout::ephemeral_reference(other_base, other_at, 0x77, eph_child)),
                "shredded tenant ephemeral ref deleted"
            );

            // Tenant 5's shared cell head is ended, yet tenant 6's cross-tenant fork still
            // recovers and reads its inherited extent cleanly!
            let shared_head = crate::head::get(&*bucket, shared).await.unwrap().unwrap();
            assert!(shared_head.deleted());
            let state = crate::recovery::recover(&*bucket, cross_fork, Some(shared_origin))
                .await
                .unwrap()
                .state;
            assert!(state.extents.contains_key(&ExtentId::new(shared.id, 1)));
        });
    }

    /// cladefs: a pass over a cell whose head names a checkpoint and nothing after it reads the
    /// checkpoint's tail alone, never the checkpoint whole, and deletes what no state names: a
    /// stray segment.
    #[test]
    fn passes_read_a_checkpoint_nothing_since_shadows_by_its_tail_alone() {
        block_on(async {
            let sim = SimBucket::new();
            let direct = Arc::new(Direct(sim.clone()));
            let cell = test_cell(1);
            lifecycle::create(&*direct, cell, 0).await.unwrap();
            let writer = Writer::open(direct.clone(), cell, 10, 0).await.unwrap();
            let mut extents = Vec::new();
            for seq in 1..=64u64 {
                extents.push((ExtentId::new(1, seq), vec![seq as u8; 64]));
            }
            commit(&writer, Commit { tag: 1, extents, ..Commit::default() }).await;
            let cp_seg = writer.checkpoint().await.unwrap().unwrap();
            let cp_key = layout::checkpoint(cell, cp_seg, writer.epoch());
            let live = writer.location(ExtentId::new(1, 1)).unwrap().key;

            // Of epoch 0, before any writer's (the writer's is 1): no head names it, ever.
            assert_eq!(writer.epoch(), 1);
            let stray = layout::segment(cell, 999, 0);
            let bytes =
                layout::with_footer(b"", &(), crate::testing::encrypting(layout::test_cell(1)));
            direct.put(&stray, bytes.into(), PutMode::Overwrite).await.unwrap();

            let (report, asked) = sim.served(Collector::new().collect_cell(&*direct, cell, 1));
            assert_eq!(report.unwrap().deleted, 1);
            let left = keys(&*direct).await;
            assert!(!left.contains(&stray), "{left:?}");
            assert!(left.contains(&live) && left.contains(&cp_key), "{left:?}");
            assert!(
                !asked.iter().any(|req| req == &format!("get {cp_key}")),
                "a pass GETs the checkpoint {cp_key} whole: {asked:?}"
            );
        });
    }

    /// cladefs: a pass over a cell whose checkpoint spans several 1 MiB parts
    /// (`extent_frame::PART`), its 64 KiB table blocks straddling their seams, and whose segment
    /// after it frees every extent of an older segment, deletes that segment and keeps the rest.
    #[test]
    fn passes_reclaim_what_segments_free_of_multipart_checkpoints() {
        block_on(async {
            let direct = Arc::new(Direct(SimBucket::new()));
            let cell = test_cell(1);
            lifecycle::create(&*direct, cell, 0).await.unwrap();
            let writer = Writer::open(direct.clone(), cell, 10, 0).await.unwrap();

            // Segment 1: extents that will ALL be freed after the checkpoint.
            let freed_extents: Vec<ExtentId> =
                (1..=16u64).map(|seq| ExtentId::new(1, seq)).collect();
            let seg1_extents: Vec<_> =
                freed_extents.iter().map(|&id| (id, vec![0xAA; 16])).collect();
            commit(&writer, Commit { tag: 1, extents: seg1_extents, ..Commit::default() }).await;
            let freed_seg_key = writer.location(freed_extents[0]).unwrap().key;

            // Segment 2: enough extents (~45,000, ~2.1 MiB table uncompressed) so the checkpoint
            // spans multiple 1 MiB `extent_frame::PART`s and table blocks straddle part seams.
            let mut kept_extents = Vec::with_capacity(45_000);
            for seq in 17..=45_016u64 {
                kept_extents.push((ExtentId::new(1, seq), vec![(seq & 0xff) as u8; 1]));
            }
            commit(&writer, Commit { tag: 2, extents: kept_extents, ..Commit::default() }).await;
            let kept_seg_key = writer.location(ExtentId::new(1, 17)).unwrap().key;

            let cp_seg = writer.checkpoint().await.unwrap().unwrap();
            let cp_key = layout::checkpoint(cell, cp_seg, writer.epoch());
            let cp_obj = bucket::get(&*direct, &cp_key).await.unwrap().unwrap();
            let cp_parts = extent_frame::Parts::of(&cp_obj.bytes, &extent_frame::NoKeys).unwrap();
            assert!(
                cp_parts.frame.data_len > extent_frame::PART,
                "expected multi-part checkpoint (>1 MiB), got {} bytes",
                cp_parts.frame.data_len
            );

            // Tail segment frees every extent of segment 1 while leaving segment 2 alive.
            commit(&writer, Commit { tag: 3, frees: freed_extents, ..Commit::default() }).await;

            Collector::new().collect_cell(&*direct, cell, 1).await.unwrap();
            let left = keys(&*direct).await;
            assert!(!left.contains(&freed_seg_key), "freed segment should be reclaimed");
            assert!(left.contains(&kept_seg_key), "live segment must remain");
            assert!(left.contains(&cp_key), "the head's checkpoint must remain");
        });
    }

    /// cladefs: a pass over a cell of versions, one of them dropped with a fork of it into another
    /// cell, which has no checkpoint of its own, keeps what each state still named needs: as a full
    /// recovery of each, after the pass, finds every extent it names in an object still there.
    #[test]
    fn passes_keep_what_versions_and_forks_of_dropped_versions_name() {
        block_on(async {
            let direct = Arc::new(Direct(SimBucket::new()));
            let parent = test_cell(1);
            let child = test_cell(2);
            lifecycle::create(&*direct, parent, 0).await.unwrap();
            let writer = Writer::open(direct.clone(), parent, 10, 0).await.unwrap();

            let ext_v1 = ExtentId::new(1, 1);
            let ext_v2 = ExtentId::new(1, 2);
            let ext_live = ExtentId::new(1, 3);
            commit(
                &writer,
                Commit {
                    tag: 1,
                    extents: vec![(ext_v1, b"v1".to_vec()), (ext_live, b"live".to_vec())],
                    ..Commit::default()
                },
            )
            .await;
            let v1_at = crate::testing::versioned_by(&writer, Vec::new(), 0x10).await;
            writer.checkpoint().await.unwrap();

            // Fork child from v1, then drop v1, free ext_v1, add ext_v2 and cut v2.
            let origin = Origin { parent, at: v1_at, epoch: writer.epoch() };
            crate::testing::fork_version(&*direct, child, 0x10, origin, 0x200).await.unwrap();
            writer.drop_versions(&BTreeSet::from([0x10])).await.unwrap();

            commit(
                &writer,
                Commit {
                    tag: 2,
                    extents: vec![(ext_v2, b"v2".to_vec())],
                    frees: vec![ext_v1],
                    ..Commit::default()
                },
            )
            .await;
            let v2_at = crate::testing::versioned_by(&writer, Vec::new(), 0x20).await;
            writer.checkpoint().await.unwrap();
            commit(&writer, Commit { tag: 3, frees: vec![ext_v2], ..Commit::default() }).await;

            Collector::new().collect(&*direct, 1).await.unwrap();
            let left = keys(&*direct).await;
            let forked = recovery::recover(&*direct, child, Some(origin)).await.unwrap().state;
            let live = recovery::recover(&*direct, parent, None).await.unwrap().state;
            let checkpoints = Checkpoints::new(&*direct, 0);
            let (v2, _) = recovery::recover_at(&checkpoints, parent, v2_at).await.unwrap();
            let named = [(&forked, ext_v1), (&forked, ext_live), (&live, ext_live), (&*v2, ext_v2)];
            for (state, extent) in named {
                let location = state.extents.get(&extent).expect("the state names it");
                assert!(left.contains(&location.key), "{extent:?} at {location:?} gone: {left:?}");
            }
        });
    }

    /// cladefs: a pass over a cell whose head names the cell itself as its origin, with no
    /// checkpoint of its own, refuses it as corrupt, deleting nothing (board 14525): no writer
    /// makes such a head, and recovering what it inherits would recurse without end.
    #[test]
    fn passes_refuse_heads_naming_their_own_cell_as_origin() {
        block_on(async {
            let direct = Direct(SimBucket::new());
            let cell = test_cell(1);
            let origin = Origin { parent: cell, at: 0, epoch: 0 };
            crate::testing::make_head(&direct, cell, Head::of(Made::Clone(origin), 0)).await;
            let stray = layout::segment(cell, 999, 999);
            let bytes = layout::with_footer(b"", &(), crate::testing::encrypting(cell));
            direct.put(&stray, bytes.into(), PutMode::Overwrite).await.unwrap();

            let refused = Collector::new().collect_cell(&direct, cell, 1).await;
            let said = format!("{cell}'s head names the cell itself as its origin");
            assert_eq!(refused, Err(Error::Corrupt(said)));
            assert!(keys(&direct).await.contains(&stray), "a refused pass deleted");
        });
    }

    /// cladefs: a pass reading a checkpoint's parts, as a segment since frees an extent it names,
    /// checks each block's CRC and refuses corrupt data rather than trusting the footer's keys.
    #[test]
    fn passes_refuse_corrupt_checkpoint_blocks() {
        block_on(async {
            let direct = Arc::new(Direct(SimBucket::new()));
            let cell = test_cell(1);
            lifecycle::create(&*direct, cell, 0).await.unwrap();
            let writer = Writer::open(direct.clone(), cell, 10, 0).await.unwrap();
            let e1 = ExtentId::new(1, 1);
            let e2 = ExtentId::new(1, 2);
            commit(
                &writer,
                Commit {
                    tag: 1,
                    extents: vec![(e1, b"one".to_vec()), (e2, b"two".to_vec())],
                    ..Commit::default()
                },
            )
            .await;
            let cp_seg = writer.checkpoint().await.unwrap().unwrap();
            commit(&writer, Commit { tag: 2, frees: vec![e1], ..Commit::default() }).await;

            // Corrupt the first byte of the checkpoint's part 0 so its CRC fails.
            let cp_key = layout::checkpoint(cell, cp_seg, writer.epoch());
            let mut bytes = bucket::get(&*direct, &cp_key).await.unwrap().unwrap().bytes;
            bytes[0] ^= 0xff;
            direct.put(&cp_key, bytes.into(), PutMode::Overwrite).await.unwrap();

            let err = Collector::new().collect_cell(&*direct, cell, 1).await.unwrap_err();
            assert!(matches!(err, Error::Corrupt(_)), "expected Corrupt, got {err:?}");
        });
    }

    /// Board 9968 (hotbed question 544): an in-flight `ephemeral.<incarnation>...` reference PUT
    /// from a lost host that lands *after* `lifecycle::decommission` deleted its index entry is
    /// retired by the collector using the incarnation's tombstone in that bucket, unpinning the
    /// parent's dropped version, while leaving non-decommissioned hosts' ephemeral references and
    /// their pinned versions intact.
    #[test]
    fn late_ephemeral_reference_put_after_decommission_is_retired_by_collector() {
        use crate::testing::Fault;
        use std::task::{Context, Poll};

        let sim = SimBucket::new();
        let direct = Arc::new(Direct(sim.clone()));
        let parent = test_cell(1);
        let lost_inc = 0x1b;
        let live_inc = 0x1d;

        let (origin_lost, origin_live, ext_lost, ext_live) = block_on(async {
            lifecycle::create(&*direct, parent, 0).await.unwrap();
            let writer = Writer::open(direct.clone(), parent, 10, 0).await.unwrap();
            let first = writer.chain().next_sequence();
            let ext_lost = ExtentId::new(1, first);
            let ext_live = ExtentId::new(1, first + 1);

            // Version 0xa (will be pinned only by the late ref from `lost_inc`).
            commit(
                &writer,
                Commit {
                    tag: 1,
                    extents: vec![(ext_lost, b"lost-host-version".to_vec())],
                    ..Commit::default()
                },
            )
            .await;
            let at_lost = crate::testing::versioned_by(&writer, Vec::new(), 0xa).await;
            recovery::checkpoint_at(
                &*direct,
                parent,
                at_lost,
                crate::keys::Parents::Held(writer.keys()),
            )
            .await
            .unwrap();

            // Version 0xb (pinned by live non-decommissioned host `live_inc`).
            commit(
                &writer,
                Commit {
                    tag: 2,
                    extents: vec![(ext_live, b"live-host-version".to_vec())],
                    frees: vec![ext_lost],
                    ..Commit::default()
                },
            )
            .await;
            let at_live = crate::testing::versioned_by(&writer, Vec::new(), 0xb).await;
            recovery::checkpoint_at(
                &*direct,
                parent,
                at_live,
                crate::keys::Parents::Held(writer.keys()),
            )
            .await
            .unwrap();

            commit(&writer, Commit { tag: 3, frees: vec![ext_live], ..Commit::default() }).await;
            writer.checkpoint().await.unwrap();
            writer.drop_versions(&BTreeSet::from([0xa, 0xb])).await.unwrap();

            let origin_lost = Origin { parent, at: at_lost, epoch: writer.epoch() };
            let origin_live = Origin { parent, at: at_live, epoch: writer.epoch() };
            lifecycle::refer_ephemeral(&*direct, origin_live, live_inc, test_cell(20))
                .await
                .unwrap();
            (origin_lost, origin_live, ext_lost, ext_live)
        });

        let lost_child = test_cell(10);
        let lost_ref =
            layout::ephemeral_reference(origin_lost.parent, origin_lost.at, lost_inc, lost_child);
        let live_ref = layout::ephemeral_reference(
            origin_live.parent,
            origin_live.at,
            live_inc,
            test_cell(20),
        );

        // Start `refer_ephemeral` for `lost_inc`, let its index PUT land, hold its reference PUT
        // in flight in `sim`'s queue, and drop the future (host lost its disk).
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        {
            let mut refer =
                Box::pin(lifecycle::refer_ephemeral(&*sim, origin_lost, lost_inc, lost_child));
            assert!(refer.as_mut().poll(&mut cx).is_pending());
            assert_eq!(sim.waiting(), 1, "index entry PUT queued");
            sim.complete(0, Fault::None);
            assert!(refer.as_mut().poll(&mut cx).is_pending());
            assert_eq!(sim.waiting(), 1, "reference PUT queued");
            assert!(sim.describe(0).contains(&lost_ref), "{}", sim.describe(0));
            // Drop `refer`: the host died while the reference PUT stayed in flight.
        }

        // Decommission `lost_inc` while keeping the in-flight reference PUT held in `sim`'s queue.
        let buckets = BTreeSet::from([crate::identity::NAMED]);
        let mut dec = Box::pin(lifecycle::decommission(&*sim, &buckets, lost_inc, 100));
        let dec_result = loop {
            if let Poll::Ready(res) = dec.as_mut().poll(&mut cx) {
                break res.unwrap();
            }
            let next = (0..sim.waiting())
                .find(|&i| !sim.describe(i).contains(&format!("put {lost_ref}")))
                .expect("decommission should progress while reference PUT is held");
            sim.complete(next, Fault::None);
        };
        assert_eq!(dec_result, lifecycle::Decommissioned { retired: 1, left: 0 });

        // Now the delayed reference PUT lands with no index entry left.
        assert_eq!(sim.waiting(), 1);
        assert!(sim.describe(0).contains(&format!("put {lost_ref}")));
        sim.complete(0, Fault::None);

        // Confirm the orphaned reference stands and a second `decommission` cannot find it.
        block_on(async {
            assert_eq!(
                lifecycle::decommission(&*direct, &buckets, lost_inc, 100).await.unwrap(),
                lifecycle::Decommissioned { retired: 0, left: 0 }
            );
            let all_keys = keys(&*direct).await;
            assert!(all_keys.contains(&lost_ref), "orphaned ref present before collector pass");
            assert!(all_keys.contains(&live_ref), "live host ref present");

            // Dry run reports the orphaned decommissioned ref and does not touch the bucket or
            // the live host's ref.
            let dry = Collector::new().dry_run_cell(&*direct, parent).await.unwrap();
            assert!(dry.retired.contains(&lost_ref), "dry_run reports orphaned ref: {dry:?}");
            assert!(!dry.retired.contains(&live_ref), "dry_run keeps live ref: {dry:?}");
            assert!(keys(&*direct).await.contains(&lost_ref), "dry_run writes nothing");

            // First collector pass retires the orphaned reference (recording no skip digest so the
            // next pass reclaims the now-unpinned version).
            let pass1 = Collector::new().collect_cell(&*direct, parent, 1).await.unwrap();
            assert_eq!(pass1.retired, 1, "first pass retires orphaned ref: {pass1:?}");
            let after1 = keys(&*direct).await;
            assert!(!after1.contains(&lost_ref), "orphaned ref retired");
            assert!(after1.contains(&live_ref), "live host ref kept");

            // Second pass reclaims the dropped version `origin_lost` that only `lost_ref` pinned.
            let pass2 = Collector::new().collect_cell(&*direct, parent, 2).await.unwrap();
            assert!(pass2.deleted > 0, "second pass reclaims unpinned version: {pass2:?}");

            // `origin_lost` is now reclaimed; `origin_live` remains readable.
            let checkpoints = Checkpoints::new(&*direct, 0);
            let lost_read = recovery::recover_at(&checkpoints, parent, origin_lost.at).await;
            let lost_missing = match lost_read {
                Err(Error::Lost(_)) => true,
                Ok((state, _)) => match state.extents.get(&ext_lost) {
                    Some(loc) => crate::testing::read(&direct, ext_lost, loc).await.is_err(),
                    None => true,
                },
                Err(other) => panic!("unexpected error reading lost version: {other:?}"),
            };
            assert!(lost_missing, "unpinned version's extent should be reclaimed");

            let (live_state, _) =
                recovery::recover_at(&checkpoints, parent, origin_live.at).await.unwrap();
            let live_loc = live_state.extents.get(&ext_live).expect("live extent kept");
            assert_eq!(
                crate::testing::read(&direct, ext_live, live_loc).await.unwrap(),
                b"live-host-version"
            );
        });
    }

    /// Board 9968 blind-spot regression: a cell whose collector lease already recorded a matching
    /// listing digest before a host incarnation was decommissioned must NOT be skipped by `look()`
    /// or `collect_held()` once that incarnation's tombstone appears outside the cell's prefix.
    #[test]
    fn unchanged_cell_digest_does_not_skip_decommissioned_ephemeral_reference() {
        let direct = Arc::new(Direct(SimBucket::new()));
        let parent = test_cell(1);
        let inc = 0x2f;

        block_on(async {
            lifecycle::create(&*direct, parent, 0).await.unwrap();
            let writer = Writer::open(direct.clone(), parent, 10, 0).await.unwrap();
            let ext = ExtentId::new(1, writer.chain().next_sequence());
            commit(
                &writer,
                Commit { tag: 1, extents: vec![(ext, b"pinned".to_vec())], ..Commit::default() },
            )
            .await;
            let at = crate::testing::versioned_by(&writer, Vec::new(), 0xc).await;
            recovery::checkpoint_at(
                &*direct,
                parent,
                at,
                crate::keys::Parents::Held(writer.keys()),
            )
            .await
            .unwrap();
            commit(&writer, Commit { tag: 2, frees: vec![ext], ..Commit::default() }).await;
            writer.checkpoint().await.unwrap();
            writer.drop_versions(&BTreeSet::from([0xc])).await.unwrap();

            let child = test_cell(30);
            let ref_key = layout::ephemeral_reference(parent, at, inc, child);
            // Write only the reference (as after a late PUT) BEFORE any tombstone exists.
            bucket::create(&*direct, &ref_key, Vec::new()).await.unwrap();

            // Settle any garbage unrelated to the pinned version so the lease records the clean
            // digest, then verify the next pass skips while the host is still live.
            let mut holder = 1;
            loop {
                let report = Collector::new().collect_cell(&*direct, parent, holder).await.unwrap();
                holder += 1;
                assert_eq!(report.retired, 0);
                if report.deleted == 0 {
                    break;
                }
            }
            let skipped = Collector::new().collect_cell(&*direct, parent, holder).await.unwrap();
            holder += 1;
            assert_eq!((skipped.skipped, skipped.retired), (1, 0), "skipped while host live");

            // Decommission `inc` (tombstone lives outside `parent`'s prefix; cell listing digest
            // is completely unchanged).
            let buckets = BTreeSet::from([crate::identity::NAMED]);
            lifecycle::decommission(&*direct, &buckets, inc, 100).await.unwrap();

            // Next pass must NOT skip: it detects the tombstone and retires the orphaned reference.
            let retired = Collector::new().collect_cell(&*direct, parent, holder).await.unwrap();
            holder += 1;
            assert_eq!(
                (retired.skipped, retired.retired),
                (0, 1),
                "retired despite unchanged digest"
            );
            assert!(!keys(&*direct).await.contains(&ref_key), "orphaned ref deleted");

            // Following pass reclaims the now-unpinned dropped version.
            let reclaimed = Collector::new().collect_cell(&*direct, parent, holder).await.unwrap();
            assert!(reclaimed.deleted > 0, "unpinned version reclaimed: {reclaimed:?}");
        });
    }

    /// Hotbed review 120 finding 455: a cell holding ephemeral references from many distinct host
    /// incarnations checks tombstones with a single delimited listing of `ephemeral/<bucket>/`
    /// (zero serial HEADs), and reuses `look()`'s result in `collect_held()` instead of querying
    /// tombstones twice.
    #[test]
    fn skipped_pass_over_many_ephemeral_incarnations_uses_one_delimited_listing() {
        let bucket = memory_bucket();
        let parent = test_cell(1);
        const HOSTS: u64 = 25;

        block_on(async {
            lifecycle::create(&*bucket, parent, 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), parent, 10, 0).await.unwrap();
            let ext = ExtentId::new(1, writer.chain().next_sequence());
            commit(
                &writer,
                Commit { tag: 1, extents: vec![(ext, b"shared".to_vec())], ..Commit::default() },
            )
            .await;
            let at = crate::testing::versioned_by(&writer, Vec::new(), 0xd).await;
            recovery::checkpoint_at(
                &*bucket,
                parent,
                at,
                crate::keys::Parents::Held(writer.keys()),
            )
            .await
            .unwrap();
            let origin = Origin { parent, at, epoch: writer.epoch() };

            for i in 0..HOSTS {
                let inc = 0x101 + 2 * i;
                lifecycle::refer_ephemeral(&*bucket, origin, inc, test_cell(100 + i))
                    .await
                    .unwrap();
            }

            // Settle the cell so the collector lease holds the clean digest.
            let mut holder = 1;
            loop {
                let report = Collector::new().collect_cell(&*bucket, parent, holder).await.unwrap();
                holder += 1;
                if report.deleted == 0 {
                    break;
                }
            }

            // A skipped pass over 25 distinct incarnations uses 0 HEADs and 1 delimited listing.
            let before = bucket.stats();
            let skipped = Collector::new().collect_cell(&*bucket, parent, holder).await.unwrap();
            holder += 1;
            let after = bucket.stats();
            assert_eq!((skipped.skipped, skipped.retired), (1, 0));
            assert_eq!(
                after.requests(Request::Head) - before.requests(Request::Head),
                0,
                "no serial per-incarnation HEADs on skip"
            );
            assert_eq!(
                after.requests(Request::ListDelimited) - before.requests(Request::ListDelimited),
                1,
                "one delimited listing of ephemeral/<bucket>/"
            );

            // Decommission one incarnation (leaving its ref behind as after a late PUT), then run
            // a pass: `look()` does 1 delimited listing and `collect_held()` reuses it without a
            // second tombstone query (only `referenced()` adds 1 delimited listing of `refs/`).
            let doomed_inc = 0x101;
            let doomed_ref = layout::ephemeral_reference(parent, at, doomed_inc, test_cell(100));
            let buckets = BTreeSet::from([crate::identity::NAMED]);
            lifecycle::decommission(&*bucket, &buckets, doomed_inc, 100).await.unwrap();
            bucket::create(&*bucket, &doomed_ref, Vec::new()).await.unwrap();

            let before = bucket.stats();
            let retired = Collector::new().collect_cell(&*bucket, parent, holder).await.unwrap();
            let after = bucket.stats();
            assert_eq!((retired.skipped, retired.retired), (0, 1));
            assert_eq!(
                after.requests(Request::Head) - before.requests(Request::Head),
                0,
                "no serial HEADs when retiring"
            );
            assert_eq!(
                after.requests(Request::ListDelimited) - before.requests(Request::ListDelimited),
                2,
                "1 delimited listing for tombstones in look() (reused) + 1 for refs/"
            );
            assert!(!keys(&*bucket).await.contains(&doomed_ref));
        });
    }
}
