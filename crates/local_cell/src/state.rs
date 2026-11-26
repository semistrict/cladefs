// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! An open cell's state, as its cell holds it ([`State`]), which its lock guards: its extents,
//! epoch and root, and an owner of each part of the rest, which keeps that part's invariants: the
//! room its extents take ([`Space`]), what it changed since its last commit ([`Uncommitted`]), the
//! gate writes pass ([`Gate`]), its segments ([`Segments`]), its extents' own files
//! ([`OwnFiles`]), what its next commit makes unreferenced ([`Unreferenced`]); cladefs: its
//! write-ahead log's segments not yet acknowledged in the bucket ([`Queue`], TASK-165.9) and the
//! frees it keeps ([`KeptExtents`]). cladefs: the state locked
//! ([`Locked`]), which the builds of the tests audit as it lets it go ([`State::audit`],
//! TASK-160.16).

#[cfg(any(test, feature = "audit"))]
use crate::cell::covered;
use crate::cell::{Info, NoSpace, TooSmall};
use crate::dir::{Dir, File};
use crate::durability::Syncs;
use crate::extents::{Entry, Extents};
use crate::framed::{Files, Framed};
use crate::kept::{Kept, KeptExtents};
use crate::layout::{
    Change, Data, EXTENTS, Extent, FileKey, Piece, SegmentId, cell_dir, extent_path,
};
use crate::raw::RawFile;
#[cfg(any(test, feature = "audit"))]
use crate::segments::Recount;
use crate::segments::{Segments, Unframed};
use crate::shared::Shared;
use crate::sync::{MutexGuard, rules};
use crate::wal::Queue;
use anyhow::{Error, bail};
use extent_frame::segment::CellId;
use extent_frame::segment::Location;
use extent_frame::spent::Spent;
#[cfg(any(test, feature = "audit"))]
use std::collections::HashSet;
use std::collections::{BTreeMap, HashMap};
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::time::Instant;
use storage_device::{ExtentId, NoExtent};

/// An open cell's state. See the module's docs.
pub(crate) struct State<F> {
    /// Its extents.
    pub extents: Extents,
    /// The room they take.
    pub space: Space,
    /// The last epoch committed.
    pub epoch: u64,
    /// The root as of the last commit.
    pub root: Vec<u8>,
    pub next_sequence: u64,
    /// cladefs: the sequence numbers of its own extents it opens no more (TASK-79.6).
    pub spent: Spent,
    /// What it changed since the last commit, which the next records.
    pub uncommitted: Uncommitted<F>,
    /// The gate writes pass.
    pub gate: Gate,
    /// The segments it tracks, and the one taking writes.
    pub segments: Segments<F>,
    /// The files of its extents stored on their own.
    pub own_files: OwnFiles<F>,
    /// The files the next commit makes unreferenced.
    pub unreferenced: Unreferenced,
    /// cladefs: its write-ahead log's segments not yet acknowledged in the bucket, which the
    /// uploader takes (TASK-165.9).
    pub wal: Queue,
    /// cladefs: whether it holds a version, which nothing writes (doc-7, Versions).
    pub version: bool,
    /// cladefs: the extents freed whose data stays.
    pub kept: KeptExtents,
    /// cladefs: where the cell is in its life: open, closed, or failed, and why ([`Standing`],
    /// TASK-160.19), which its guards read ([`Guarded`]).
    pub standing: Standing,
    /// cladefs: the segments closed for want of room since the last commit, which the next frames,
    /// with the rule that framings end themselves off alone (TASK-150.2's shape, TASK-153.8).
    pub deferred: Vec<Unframed<F>>,
    /// cladefs: the framings to run, one at a time, in the order their segments closed
    /// (TASK-165.9).
    pub framing: Framing<F>,
    /// cladefs: the cell as the replay of its footers has it, from its opening on, in the builds
    /// of the tests ([`Replay`], TASK-165.9).
    #[cfg(any(test, feature = "audit"))]
    pub replay: Replay,
}

/// cladefs: a cell as the one replay of its segments' footers has it, the bucket's and its
/// write-ahead log's from its cell's opening on, its extents by their lengths: which the state,
/// as of each commit that closes a segment, must hold, the audit asserts (TASK-165.9's AC #5).
#[cfg(any(test, feature = "audit"))]
pub(crate) type Replay = bucket_cell::State<Lengths>;

/// cladefs: the lengths of a cell's extents, as a replay of its footers places them ([`Replay`]).
#[cfg(any(test, feature = "audit"))]
#[derive(Default)]
pub(crate) struct Lengths(pub BTreeMap<ExtentId, u64>);

#[cfg(any(test, feature = "audit"))]
impl bucket_cell::recovery::Places for Lengths {
    fn seal(&mut self, extent: ExtentId, seal: bucket_cell::recovery::Seal) {
        let (bucket_cell::recovery::Seal::Run(at) | bucket_cell::recovery::Seal::Own(at)) = seal;
        self.0.insert(extent, at.len);
    }

    fn free(&mut self, extent: ExtentId) {
        self.0.remove(&extent);
    }
}

/// cladefs: the segments closed whose framings are yet to run, in the order they closed, and
/// whether a framer runs them, one after another (`wal`'s docs, TASK-165.9), with the looks ahead
/// at parts of segments filling between them (TASK-400); and those it framed, written, not yet
/// durable, since when the first of them waits, and whether a thread makes them durable, together
/// (`segments::make_durable`, TASK-310).
pub(crate) struct Framing<F> {
    pub queue: std::collections::VecDeque<Job<F>>,
    pub running: bool,
    /// cladefs: whether the framer's thread reads a part looked at ahead, holding no lock
    /// (`Framer::see`, TASK-423).
    pub looking: bool,
    pub written: Vec<(SegmentId, Arc<Framed<F>>)>,
    pub since: Option<Instant>,
    pub syncing: bool,
}

/// cladefs: what a cell's framer does ([`Framing`]), in order: a look ahead at a part of a segment,
/// written whole, as it fills (`framing::see`, TASK-400); or a segment's framing.
pub(crate) enum Job<F> {
    See(SegmentId, u64),
    Frame(Unframed<F>),
}

impl<F> Default for Framing<F> {
    fn default() -> Self {
        let queue = std::collections::VecDeque::new();
        let written = Vec::new();
        Self { queue, running: false, looking: false, written, since: None, syncing: false }
    }
}

impl<F> Framing<F> {
    /// cladefs: whether a look ahead is queued, or reading its part ([`Framing::looking`]).
    pub fn looks_ahead(&self) -> bool {
        self.looking || self.queue.iter().any(|job| matches!(job, Job::See(..)))
    }

    /// cladefs: segment `number`'s framing wrote `framed`, to be made durable.
    pub fn wrote(&mut self, number: SegmentId, framed: Arc<Framed<F>>) {
        self.written.push((number, framed));
        self.since.get_or_insert_with(Instant::now);
    }

    /// cladefs: the segments framed, written, to be made durable together, and since when the first
    /// of them waits: none left.
    pub fn take_written(&mut self) -> (Vec<(SegmentId, Arc<Framed<F>>)>, Instant) {
        let since = self.since.take().unwrap_or_else(Instant::now);
        (std::mem::take(&mut self.written), since)
    }
}

/// Whom a read is for: the cell's own, of what it holds; or of what it holds or keeps, freed, for
/// the bucket ([`LocalCell::read_kept`](crate::LocalCell::read_kept)).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reader {
    Own,
    Kept,
}

/// cladefs: where an extent is in its life, as the cell's state has it, found in one place
/// ([`State::life`], TASK-160.18), which readers and writers decide by. cladefs: its entry borrowed
/// from the layer of the cell's map that holds it ([`Entry`], TASK-453).
pub(crate) enum Life<'a> {
    /// Written since the last commit, which seals it: its entry, as the writes that finished left
    /// it, if one did.
    Open(Option<Entry<'a>>),
    /// Sealed, and held.
    Held(Entry<'a>),
    /// Freed by a commit, and kept: for the bucket ([`Kept`]).
    Kept(&'a Kept),
    /// Sealed once, and freed, and not kept: its number the cell writes no more (TASK-79.6).
    Spent,
    /// Neither opened nor sealed here.
    Unknown,
}

impl<'a> Life<'a> {
    /// Its entry, if the cell holds it: open or sealed.
    pub fn held(&self) -> Option<Entry<'a>> {
        match *self {
            Life::Open(entry) => entry,
            Life::Held(entry) => Some(entry),
            Life::Kept(_) | Life::Spent | Life::Unknown => None,
        }
    }
}

impl<F: File> State<F> {
    /// Runs `change`, which changes the state ahead of the files it writes: failing part way, it
    /// leaves the state other than the files say, and fails the cell, which does nothing more.
    pub fn failing<T>(
        &mut self,
        change: impl FnOnce(&mut Self) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let changed = change(self);
        if let Err(error) = &changed {
            self.fail(error);
        }
        changed
    }

    /// cladefs: fails the cell, for `error`, unless it failed before ([`Standing::fail`]).
    pub fn fail(&mut self, error: &Error) {
        self.standing.fail(error);
    }

    /// cladefs: where `extent` is in its life ([`Life`], TASK-160.18).
    pub fn life(&self, extent: ExtentId) -> Life<'_> {
        let open = self.uncommitted.is_open(extent);
        match self.extents.get(&extent) {
            Some(entry) if open => Life::Open(Some(entry)),
            Some(entry) => Life::Held(entry),
            None if open => Life::Open(None),
            None => match self.kept.get(&extent) {
                Some(kept) => Life::Kept(kept),
                None if self.spent.contains(extent.sequence) => Life::Spent,
                None => Life::Unknown,
            },
        }
    }

    /// `extent`'s entry, which `reader` may read: of what the cell holds, or keeps, if not the
    /// cell's own. Fails with `NoExtent` otherwise.
    pub fn entry_for(&self, extent: ExtentId, reader: Reader) -> Result<Extent, Error> {
        match self.life(extent) {
            Life::Open(Some(entry)) | Life::Held(entry) => Ok(entry.to_extent()),
            Life::Kept(kept) if reader == Reader::Kept => Ok(kept.entry.clone()),
            Life::Open(None) | Life::Kept(_) | Life::Spent | Life::Unknown => {
                bail!(NoExtent(extent))
            }
        }
    }

    /// Lets the data of `extent`, freed, go.
    pub fn forget(&mut self, extent: ExtentId, entry: Extent) {
        match entry.data {
            Data::Pieces(pieces) => {
                for piece in &pieces {
                    self.segments.let_go(piece);
                }
            }
            Data::Own => {
                self.own_files.forget(extent);
                self.unreferenced.doomed.extend(FileKey::Own(extent).paths());
            }
            Data::Bucket(_) => {}
        }
    }

    /// cladefs: segment `segment`, closed, is acknowledged in the bucket, at `key`, and the extents
    /// stored on their own its footer names are at `own` there (TASK-165.8): its extents sealed in
    /// it, and those stored on their own, held or kept, are the bucket's from now on, read there,
    /// and their files leave the cell, for the host's cache if `cached`, its framed file being
    /// the object at `key`. Returns those that leave now, by their paths, with their keys in the
    /// bucket, if into the cache: the extents' own files, those no extent holds nor keeps any more
    /// too, which the segment kept for the bucket (TASK-165.9); the segment's leaves once no
    /// extent's data is in it here, which writes since the last commit may be
    /// ([`Reaped`](crate::segments::Reaped)). cladefs: its data `len` bytes long, which its
    /// density is measured against (doc-27).
    pub fn acknowledge(
        &mut self,
        segment: SegmentId,
        (key, cached): (&str, bool),
        own: &[(ExtentId, Location)],
        len: u64,
    ) -> Vec<(String, Option<String>)> {
        let sealed = self.segments.acknowledge(segment, cached.then_some(key));
        let leaving = self.into_bucket(segment, (key, cached), sealed, own, |_| true);
        self.extents.sized(key, len);
        leaving
    }

    /// cladefs: `sealed`, extents sealed in segment `segment`, one run each, those kept in it that
    /// `named` names, and `own`, stored on their own, are read in the bucket from now on, in the
    /// object at `key`, or where `own` says ([`State::acknowledge`]). Returns the files that leave,
    /// by their paths, with their keys in the bucket, if into the host's cache, as `cached` says.
    fn into_bucket(
        &mut self,
        segment: SegmentId,
        (key, cached): (&str, bool),
        sealed: impl IntoIterator<Item = ExtentId>,
        own: &[(ExtentId, Location)],
        named: impl Fn(&ExtentId) -> bool,
    ) -> Vec<(String, Option<String>)> {
        let in_bucket = |len, piece: &Piece| {
            Data::Bucket(Location { key: key.to_owned(), offset: piece.offset, len })
        };
        let cached_key = |key: &str| cached.then(|| key.to_owned());
        for extent in sealed {
            let entry = self.extents.get(&extent).and_then(Entry::own);
            let entry = entry.expect("an extent sealed in a segment is held");
            let Data::Pieces(pieces) = &entry.data else {
                unreachable!("{extent:?}, sealed, is not")
            };
            let [piece] = pieces[..] else { unreachable!("{extent:?}, sealed, is not in one run") };
            let placed = in_bucket(entry.len, &piece);
            let held = self.extents.place(&extent, placed);
            self.segments.displace(extent, &held);
            self.segments.let_go(&piece);
        }
        let mut leaving = Vec::new();
        for (extent, location) in own {
            let entry = self.extents.get(extent).and_then(Entry::own);
            if entry.is_some_and(|entry| entry.data == Data::Own) {
                self.extents.place(extent, Data::Bucket(location.clone()));
                self.own_files.forget(*extent);
                leaving.push((extent_path(*extent), cached_key(&location.key)));
            } else if self.kept.get(extent).is_none_or(|kept| kept.entry.data != Data::Own) {
                // Freed, and kept no more, or since an opening: the file the bucket needed alone.
                leaving.push((extent_path(*extent), None));
            }
        }
        let own: HashMap<ExtentId, &Location> =
            own.iter().map(|(extent, location)| (*extent, location)).collect();
        // What it keeps of extents freed is read there too.
        let mut kept = Vec::new();
        for (extent, held) in self.kept.entries_mut().filter(|(extent, _)| named(extent)) {
            let placed = match &held.entry.data {
                Data::Pieces(pieces) => match pieces[..] {
                    [piece] if piece.segment == segment => Some(in_bucket(held.entry.len, &piece)),
                    _ => None,
                },
                Data::Own => own.get(extent).map(|location| Data::Bucket((*location).clone())),
                Data::Bucket(_) => None,
            };
            if let Some(placed) = placed {
                kept.push((*extent, std::mem::replace(&mut held.entry.data, placed)));
            }
        }
        for (extent, data) in kept {
            match data {
                Data::Pieces(pieces) => self.segments.let_go(&pieces[0]),
                Data::Own => {
                    self.own_files.forget(extent);
                    leaving.push((extent_path(extent), cached_key(&own[&extent].key)));
                }
                Data::Bucket(_) => unreachable!("kept, placed in the bucket again"),
            }
        }
        leaving
    }

    /// Fails with [`NoSpace`] unless a write may add `grows` bytes, and write over `overwrites`.
    pub fn check_room(&self, grows: u64, overwrites: u64) -> anyhow::Result<()> {
        let wasted = self.uncommitted.wasted() + overwrites;
        self.space.check(grows, wasted)
    }

    /// cladefs: the bucket needs the frees of the commits up to `epoch` no more, acknowledged
    /// there: trims let them go.
    pub fn release_kept(&mut self, epoch: u64) {
        self.kept.release(epoch);
    }

    /// What the cell holds, as doc-4's `INFO` reports it.
    pub fn info(&self) -> Info {
        self.space.info(self.next_sequence, self.uncommitted.wasted())
    }

    /// Lets the kept extents freed by commits up to `published`, the last committed, go that the
    /// uploader needs no more. Those freed since are for the commit that frees them to decide.
    pub fn trim_kept(&mut self, published: u64) {
        for (extent, kept) in self.kept.trim(published) {
            self.forget(extent, kept.entry);
        }
    }
}

/// cladefs: where a cell is in its life, one value (TASK-160.19): open; closed
/// ([`LocalCell::close`](crate::LocalCell::close)), when it reads what it holds, and changes
/// nothing; or failed, its state ahead of its files, when it does nothing more, saying why: the
/// first failure's error (TASK-165.9: a framing's failure, which no flush waits for, failed a later
/// call), and whether it was closed too, as a failed cell is closed all the same.
#[derive(Default)]
pub(crate) enum Standing {
    #[default]
    Open,
    Closed,
    Failed {
        cause: String,
        closed: bool,
    },
}

impl Standing {
    /// Fails it, for `error`, unless it failed before.
    pub fn fail(&mut self, error: &Error) {
        let closed = self.is_closed();
        if !self.failed() {
            *self = Standing::Failed { cause: format!("{error:#}"), closed };
        }
    }

    /// Closes it: returns whether it was not closed before, failed or not.
    pub fn close(&mut self) -> bool {
        match self {
            Standing::Open => *self = Standing::Closed,
            Standing::Failed { closed, .. } if !*closed => *closed = true,
            Standing::Closed | Standing::Failed { .. } => return false,
        }
        true
    }

    pub fn failed(&self) -> bool {
        matches!(self, Standing::Failed { .. })
    }

    pub fn is_closed(&self) -> bool {
        matches!(self, Standing::Closed | Standing::Failed { closed: true, .. })
    }

    /// Fails, saying why, if the cell failed.
    pub fn whole(&self) -> Result<(), Error> {
        match self {
            Standing::Failed { cause, .. } => {
                Err(anyhow::anyhow!("the extent store failed: {cause}"))
            }
            Standing::Open | Standing::Closed => Ok(()),
        }
    }
}

/// cladefs: what a [`Guarded`] state holds to, checked as it is taken and again after each of its
/// waits (TASK-160.19).
pub(crate) trait Holds {
    /// Fails unless `state`, of cell `cell`, holds to it.
    fn check<F: File>(state: &State<F>, cell: CellId) -> Result<(), Error>;
}

/// cladefs: a cell not failed: what reading takes ([`Readable`]).
pub(crate) struct Whole;

impl Holds for Whole {
    fn check<F: File>(state: &State<F>, _cell: CellId) -> Result<(), Error> {
        state.standing.whole()
    }
}

/// cladefs: a cell not failed, nor closed, its cell no version: what changing it takes
/// ([`Writable`]).
pub(crate) struct Changeable;

impl Holds for Changeable {
    fn check<F: File>(state: &State<F>, cell: CellId) -> Result<(), Error> {
        state.standing.whole()?;
        anyhow::ensure!(!state.standing.is_closed(), "cell {cell} is closed");
        anyhow::ensure!(!state.version, "cell {cell} is a version, which nothing writes");
        Ok(())
    }
}

/// cladefs: the state, locked, as `H` holds it to (TASK-160.19): made only by checking it, and
/// checked again by each of its waits, which let the lock go meanwhile, so that no caller goes on
/// after waiting as if the cell couldn't have failed, or closed, since (TASK-83). With the rule
/// `recheck_after_waiting` off, as a Shuttle test has it to show it needed, its waits check
/// nothing.
pub(crate) struct Guarded<'a, F: File, H> {
    state: Locked<'a, F>,
    holds: std::marker::PhantomData<H>,
}

/// cladefs: the state of a cell not failed ([`Whole`]).
pub(crate) type Readable<'a, F> = Guarded<'a, F, Whole>;

/// cladefs: the state of a cell to change ([`Changeable`]).
pub(crate) type Writable<'a, F> = Guarded<'a, F, Changeable>;

impl<'a, F: File, H: Holds> Guarded<'a, F, H> {
    /// `state`, of cell `cell`, if it holds to `H`.
    pub fn check(state: Locked<'a, F>, cell: CellId) -> Result<Self, Error> {
        H::check(&state, cell)?;
        Ok(Self { state, holds: std::marker::PhantomData })
    }

    /// The state, the check given up: what a wait lets go of, to lock again
    /// (`LocalCell::lock_when`, board 2944).
    pub fn into_locked(self) -> Locked<'a, F> {
        self.state
    }

    /// `state`, of cell `cell`, after a wait let it go: checked again, as it holds to `H`, unless
    /// `recheck_after_waiting` is off, as a Shuttle test has it to show it needed.
    pub fn rechecked(state: Locked<'a, F>, cell: CellId) -> Result<Self, Error> {
        match rules().recheck_after_waiting {
            true => Self::check(state, cell),
            false => Ok(Self { state, holds: std::marker::PhantomData }),
        }
    }
}

impl<F: File, H> Deref for Guarded<'_, F, H> {
    type Target = State<F>;

    fn deref(&self) -> &State<F> {
        &self.state
    }
}

impl<F: File, H> DerefMut for Guarded<'_, F, H> {
    fn deref_mut(&mut self) -> &mut State<F> {
        &mut self.state
    }
}

/// cladefs: the state, locked, which the builds of the tests audit as they let it go, waiting or
/// done ([`State::audit`], TASK-160.16): so that whatever changed it, its counts are found right
/// before anything else reads them. Unless the cell failed, which changes nothing more, its
/// state other than its files say; or the thread is panicking, which another panic would abort.
pub(crate) struct Locked<'a, F: File> {
    /// None only as it waits ([`Locked::wait`]).
    guard: Option<StateGuard<'a, F>>,
}

/// cladefs: the state's lock held, shared beyond one cell, as the cells a base is attached to share
/// it, which a panic holding aborts the extent daemon (`crate::shared`, TASK-149.24).
pub(crate) type StateGuard<'a, F> = Shared<MutexGuard<'a, State<F>>>;

impl<'a, F: File> Locked<'a, F> {
    pub fn new(guard: StateGuard<'a, F>) -> Self {
        Self { guard: Some(guard) }
    }
}

impl<F: File> Deref for Locked<'_, F> {
    type Target = State<F>;

    fn deref(&self) -> &State<F> {
        self.guard.as_ref().expect("the state is locked")
    }
}

impl<F: File> DerefMut for Locked<'_, F> {
    fn deref_mut(&mut self) -> &mut State<F> {
        self.guard.as_mut().expect("the state is locked")
    }
}

impl<F: File> Drop for Locked<'_, F> {
    fn drop(&mut self) {
        if let Some(guard) = self.guard.take() {
            drop(audited(guard));
        }
    }
}

/// cladefs: the most extents a cell may hold for the audit to recount its state ([`Locked`]): it
/// goes through them all each time the state is let go, which took a test of cladefs's, writing a
/// base of 32,768 extents one at a time and forking it a hundred times, past an hour; the cells
/// of the crate's own tests hold hundreds at most (TASK-160.16).
#[cfg(any(test, feature = "audit"))]
const AUDITED_EXTENTS: usize = 4096;

/// cladefs: `guard`'s state, audited, in the builds of the tests ([`Locked`]), if its cell holds
/// [`AUDITED_EXTENTS`] at most. An audit that fails panics once it has let the state go,
/// unpoisoned, saying what it found after [`AUDIT_FAILED`]: the cell's drop, closing its cell,
/// locks it again, which a lock poisoned would panic in, aborting as the audit's panic unwinds.
#[cfg(any(test, feature = "audit"))]
fn audited<F: File>(guard: StateGuard<'_, F>) -> StateGuard<'_, F> {
    if guard.standing.failed() || std::thread::panicking() || guard.extents.len() > AUDITED_EXTENTS
    {
        return guard;
    }
    let audit = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| guard.audit()));
    match audit {
        Ok(()) => guard,
        Err(panic) => {
            drop(guard);
            let found = (panic.downcast_ref::<String>().map(String::as_str))
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("a panic");
            panic!("{AUDIT_FAILED}: {found}")
        }
    }
}

/// cladefs: what a failed audit's panic begins with, whatever it found (TASK-160.16).
#[cfg(any(test, feature = "audit"))]
pub const AUDIT_FAILED: &str = "the extent store's audit found its counts drifted";

/// cladefs: `guard`, audited not, in the other builds.
#[cfg(not(any(test, feature = "audit")))]
fn audited<F: File>(guard: StateGuard<'_, F>) -> StateGuard<'_, F> {
    guard
}

/// cladefs: the audit of the state's counts, in the builds of the tests (TASK-160.16).
#[cfg(any(test, feature = "audit"))]
impl<F: File> State<F> {
    /// cladefs: the replay ([`Replay`]) applies `footer`, of a segment just closed; and, if the
    /// state holds nothing open nor changed since its last commit, which the footer names last,
    /// panics unless the state is what the replay says: its extents and their lengths, its root,
    /// what it spent and its capacity (TASK-165.9's AC #5).
    pub fn closed(&mut self, footer: &extent_frame::segment::Footer) {
        self.replay.apply_contents(footer.segment, footer.epoch, &footer.contents);
        // cladefs: nor a commit begun in the footers, which the open segment's ends (TASK-92.41).
        if !self.uncommitted.is_empty() || self.replay.begun.is_some() {
            return;
        }
        let lengths: BTreeMap<ExtentId, u64> = self.extents.lens().collect();
        assert_eq!(lengths, self.replay.extents.0, "the extents, as the footers say");
        let root = self.replay.root.as_deref().unwrap_or_default();
        assert_eq!(self.root, root, "the root, as the footers say");
        assert_eq!(self.spent, self.replay.spent, "the IDs spent, as the footers say");
        if let Some(capacity) = self.replay.capacity {
            assert_eq!(self.space.capacity(), capacity, "the capacity, as the footers say");
        }
    }

    /// Recounts each count the state keeps on as it changes from what it holds, and panics,
    /// saying which, unless they agree: the bytes its extents hold ([`Space`]); what writes since
    /// the last commit wrote over ([`Uncommitted`]); what each segment it tracks holds
    /// ([`Segments`]); what its extents' own files take ([`OwnFiles`]); and the frees it keeps
    /// ([`KeptExtents`]). Recounted from its extents, open and sealed, those it keeps, and the
    /// segments it tracks. Kept by hand, as each change adds to them or takes from them, a count
    /// drifting from what it counts showed only as a write refused room it had, or given room it
    /// had not (TASK-79.2, TASK-79.105). What writes in
    /// progress take, and what writes since the last commit let go of, which nothing it holds
    /// says, it checks only as far as what it holds bounds them.
    pub fn audit(&self) {
        let cell = self.segments.open_segment().cell;
        self.space.audit(cell, self.extents.lens(), self.gate.is_writing());
        self.extents.audit();
        self.uncommitted.audit(&self.extents);
        self.kept.audit();
        let own =
            |extent: &ExtentId, entry: &Extent| extent.cell == cell && entry.data == Data::Own;
        let mut recount = Recount::default();
        let mut own_files = HashSet::new();
        for (extent, entry) in self.extents.iter() {
            let entry = entry.into_cow();
            let sealed = !self.uncommitted.is_open(extent);
            recount.holds(extent, &entry, sealed);
            if sealed && own(&extent, &entry) {
                own_files.insert(extent);
            }
        }
        for (extent, kept) in self.kept.iter() {
            recount.keeps(&kept.entry);
            if own(extent, &kept.entry) {
                own_files.insert(*extent);
            }
        }
        self.segments.audit(&recount);
        self.own_files.audit(&own_files);
    }
}

/// The room a cell's extents take: the most they may hold, as its record says
/// ([`LocalCell::set_capacity`](crate::LocalCell::set_capacity)), the bytes they hold, and those
/// writes in progress may add. A write takes room only if what they hold and may add fits.
/// cladefs: and with what writes since the
/// last commit wasted past the capacity's worth, which counts as used too (TASK-79.2): what they
/// wrote over, in segments, and what frees of extents open let go of, and writes dropped, of what
/// they wrote. That is its writer's own, which no commit held, and which its next commit takes
/// back: so that a writer that never commits takes twice the capacity of the host's disk at most,
/// rewriting, or writing extents and freeing them. Past it, the writes a commit comes with are
/// refused too (doc-4's `WRITE_COMMIT`'s, and those of the extents a DAX guest maps writably, which
/// its commits write first), so that a writer past it may be unable to commit: a guest's filesystem
/// comes nowhere near it, Fxfs freeing an extent only once a flush has committed it, and writing
/// over little between commits. What the commits leave in the cell's segments that no extent is in,
/// freed, takes none of the room, however much: the segments leave the cell once acknowledged in
/// the bucket, whose collector takes it back (TASK-165.8). Counted as used past the capacity's
/// worth, it found a guest's cell full with a tenth of it used (`ENOSPC`), which the guest's
/// filesystem did not survive.
pub(crate) struct Space {
    capacity: u64,
    /// The extents it holds and the bytes they hold: of them all, and of its own.
    holding: Holding,
    /// The bytes writes in progress may add.
    reserved: u64,
}

/// cladefs: how many extents a cell holds, and the bytes they hold, as [`Space`] keeps them on
/// as they change: of all of them, and of those the cell wrote (TASK-49), the rest inherited
/// from the cells it was forked from. Kept on so that reading them walks nothing: a fork's map
/// holds its base's extents too, hundreds of thousands of them (TASK-92.17, TASK-454).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Holding {
    all: Held,
    own: Held,
}

/// cladefs: a number of extents and the bytes they hold ([`Holding`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Held {
    extents: u64,
    bytes: u64,
}

impl Holding {
    /// What `extents`, those of `cell` and those it inherited, by their lengths, hold: what an
    /// opening counts, and the audit recounts.
    pub fn of(cell: u64, extents: impl IntoIterator<Item = (ExtentId, u64)>) -> Self {
        let mut holding = Self::default();
        for (extent, len) in extents {
            holding.entered(extent.cell == cell);
            holding.grew(len, extent.cell == cell);
        }
        holding
    }

    /// Like [`Holding::of`], rejecting overflowing extent counts or byte sums with an error rather
    /// than panicking or wrapping on untrusted on-disk metadata (board 11649).
    pub fn try_of(
        cell: u64,
        extents: impl IntoIterator<Item = (ExtentId, u64)>,
    ) -> anyhow::Result<Self> {
        let mut holding = Self::default();
        for (extent, len) in extents {
            let own = extent.cell == cell;
            for held in holding.counts(own) {
                held.extents = held
                    .extents
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("extent count overflow"))?;
                held.bytes = held
                    .bytes
                    .checked_add(len)
                    .ok_or_else(|| anyhow::anyhow!("held extent length overflow"))?;
            }
        }
        Ok(holding)
    }

    /// The counts of all of them, and of the cell's own if `own`.
    fn counts(&mut self, own: bool) -> impl Iterator<Item = &mut Held> {
        std::iter::once(&mut self.all).chain(own.then_some(&mut self.own))
    }

    /// An extent is held, holding nothing yet: the cell's own if `own`.
    fn entered(&mut self, own: bool) {
        self.counts(own).for_each(|held| held.extents += 1);
    }

    /// An extent held holds `len` bytes more.
    fn grew(&mut self, len: u64, own: bool) {
        self.counts(own).for_each(|held| held.bytes += len);
    }

    /// An extent of `len` bytes is held no more.
    fn removed(&mut self, len: u64, own: bool) {
        self.counts(own).for_each(|held| {
            held.extents -= 1;
            held.bytes -= len;
        });
    }
}

impl Space {
    /// The room of a cell of `capacity` bytes whose extents hold what `holding` says.
    pub fn new(capacity: u64, holding: Holding) -> Self {
        Self { capacity, holding, reserved: 0 }
    }

    /// The most bytes its extents may hold.
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Fails with [`NoSpace`], saying the numbers (TASK-79.105), unless a write may add `grows`
    /// bytes, writes since the last commit, this one's included, wasting `wasted` bytes.
    pub fn check(&self, grows: u64, wasted: u64) -> anyhow::Result<()> {
        let used = self.holding.all.bytes + self.past_slack(wasted);
        if used + self.reserved + grows > self.capacity {
            return Err(anyhow::Error::new(NoSpace).context(format!(
                "{used} bytes used, {} reserved and {grows} written past the capacity's {}",
                self.reserved, self.capacity
            )));
        }
        Ok(())
    }

    /// cladefs: of `wasted`, what is past the capacity's worth, which counts as used (TASK-79.2).
    fn past_slack(&self, wasted: u64) -> u64 {
        wasted.saturating_sub(self.capacity)
    }

    /// Reserves `grows` bytes for a write in progress, which may add them.
    pub fn reserve(&mut self, grows: u64) {
        self.reserved += grows;
    }

    /// Lets the `grows` bytes a write reserved go: it is done.
    pub fn unreserve(&mut self, grows: u64) {
        self.reserved -= grows;
    }

    /// cladefs: an extent is held, holding nothing yet, the cell's own if `own` (TASK-454).
    pub fn entered(&mut self, own: bool) {
        self.holding.entered(own);
    }

    /// An extent held holds `len` bytes more, of the cell's own if `own`.
    pub fn grew(&mut self, len: u64, own: bool) {
        self.holding.grew(len, own);
    }

    /// An extent of `len` bytes, of the cell's own if `own`, is held no more.
    pub fn removed(&mut self, len: u64, own: bool) {
        self.holding.removed(len, own);
    }

    /// Fails with [`TooSmall`] unless `capacity` holds what the extents hold, with what writes in
    /// progress may add.
    pub fn check_capacity(&self, capacity: u64) -> Result<(), TooSmall> {
        let held = self.holding.all.bytes + self.reserved;
        if capacity < held {
            return Err(TooSmall { capacity, held });
        }
        Ok(())
    }

    pub fn set_capacity(&mut self, capacity: u64) {
        self.capacity = capacity;
    }

    /// What the cell holds, as doc-4's `INFO` reports it: `next_sequence` its next sequence
    /// number, and writes since the last commit wasting `wasted` bytes, what is past the
    /// capacity's worth of which room is not available either.
    pub fn info(&self, next_sequence: u64, wasted: u64) -> Info {
        let Holding { all, own } = self.holding;
        let used = all.bytes;
        let unavailable = used + self.past_slack(wasted) + self.reserved;
        Info {
            capacity: self.capacity,
            used,
            available: self.capacity.saturating_sub(unavailable),
            next_sequence,
            own: own.bytes,
            inherited: all.bytes - own.bytes,
            own_extents: own.extents,
            inherited_extents: all.extents - own.extents,
        }
    }

    /// cladefs: panics unless it counts `held`, the extents of `cell`, and what they hold, of them
    /// its own (TASK-454); and, unless `writing`, writes in progress reserving none (TASK-160.16).
    #[cfg(any(test, feature = "audit"))]
    fn audit(&self, cell: u64, held: impl Iterator<Item = (ExtentId, u64)>, writing: bool) {
        let recounted = Holding::of(cell, held);
        assert_eq!(self.holding, recounted, "the bytes the extents hold, own, and their counts");
        assert!(writing || self.reserved == 0, "{} bytes reserved, no write in progress", {
            self.reserved
        });
    }
}

/// Where an open extent's data goes: segments, or a raw file of its own.
pub(crate) enum Packing<F> {
    Segment,
    Own(Arc<RawFile<F>>),
}

impl<F> Clone for Packing<F> {
    fn clone(&self) -> Self {
        match self {
            Self::Segment => Self::Segment,
            Self::Own(file) => Self::Own(file.clone()),
        }
    }
}

/// What a cell changed since its last commit, which the next records: its changes, and the
/// extents written since, which are open, with where their data goes, until it seals them.
pub(crate) struct Uncommitted<F> {
    open: HashMap<ExtentId, Open<F>>,
    /// cladefs: how many of them are in raw files of their own ([`Packing::Own`]), each a file
    /// open: what `LocalCell` bounds by `Options::open_files` (board 12555).
    own: usize,
    /// The extents opened so far, which numbers each opening: an extent freed while open, and
    /// written again, is opened anew, and the writes to it before are another opening's.
    openings: u64,
    changes: Vec<Change>,
    /// cladefs: the bytes the open extents' writes wrote over, in segments (TASK-79.2).
    overwritten: u64,
    /// cladefs: the bytes writes since the last commit wrote that no extent holds any more, which
    /// no commit held: of the extents freed while open, in segments, what they wrote over too, or
    /// in raw files of their own; and of the writes dropped, in segments (TASK-79.2).
    let_go: u64,
    /// cladefs: the capacity set since the last commit, which the next sets in the cell's state
    /// as object storage keeps it, its segment's footer saying so (TASK-165.3).
    capacity: Option<u64>,
    /// cladefs: the bytes of the extents open, each as far as its writes that finished took it,
    /// holes and all: what the next commit, sealing them, may copy (TASK-539.8).
    open_bytes: u64,
}

/// An extent open: where its data goes, and its opening's number, which the writes to it carry;
/// cladefs: and the bytes its writes wrote over, in segments, which its pieces hold until it is
/// sealed, or freed.
struct Open<F> {
    packing: Packing<F>,
    opening: u64,
    overwritten: u64,
}

impl<F> Default for Uncommitted<F> {
    fn default() -> Self {
        let (open, changes, overwritten, let_go) = (HashMap::new(), Vec::new(), 0, 0);
        let (own, openings, capacity, open_bytes) = (0, 0, None, 0);
        Self { open, own, openings, changes, overwritten, let_go, capacity, open_bytes }
    }
}

/// cladefs: the extents open let go of in the order they were opened, as
/// [`Uncommitted::let_go_open`] lets them go.
impl<F> Drop for Uncommitted<F> {
    fn drop(&mut self) {
        self.let_go_open();
    }
}

impl<F> Uncommitted<F> {
    /// cladefs: lets go of the extents open in the order they were opened: the last holder of an
    /// open extent's raw file writes its dirty pages back as it goes, and so in an order the cell's
    /// writes decide, not the map's, which differs from one run to the next, as
    /// [`Uncommitted::open_extents`] orders them: so that where a crash cuts the writes back off,
    /// and what it leaves, do not (TASK-675).
    fn let_go_open(&mut self) {
        let mut open: Vec<Open<F>> = self.open.drain().map(|(_, open)| open).collect();
        self.own = 0;
        open.sort_unstable_by_key(|open| open.opening);
        // A vector's elements drop in order, first to last.
        drop(open);
    }

    /// Where `extent`'s data goes, if it is open.
    pub fn packing(&self, extent: ExtentId) -> Option<&Packing<F>> {
        self.open.get(&extent).map(|open| &open.packing)
    }

    /// Where `extent`'s data goes, and its opening's number, if it is open.
    pub fn opening(&self, extent: ExtentId) -> Option<(Packing<F>, u64)> {
        self.open.get(&extent).map(|open| (open.packing.clone(), open.opening))
    }

    /// Whether `extent` is open.
    pub fn is_open(&self, extent: ExtentId) -> bool {
        self.open.contains_key(&extent)
    }

    /// Whether `extent` is open still in its opening `opening`: neither freed since, nor sealed.
    pub fn is_open_in(&self, extent: ExtentId, opening: u64) -> bool {
        self.open.get(&extent).is_some_and(|open| open.opening == opening)
    }

    /// Whether an extent is open.
    pub fn has_open(&self) -> bool {
        !self.open.is_empty()
    }

    /// cladefs: whether nothing changed since the last commit: no extent open, no free.
    #[cfg(any(test, feature = "audit"))]
    pub fn is_empty(&self) -> bool {
        self.open.is_empty() && self.changes.is_empty() && self.capacity.is_none()
    }

    /// cladefs: how many extents open are in raw files of their own, each holding one open
    /// (board 12555).
    pub fn own(&self) -> usize {
        self.own
    }

    /// Opens `extent`, not open, whose data goes as `packing` says: returns the opening's number.
    pub fn open(&mut self, extent: ExtentId, packing: Packing<F>) -> u64 {
        self.openings += 1;
        let opening = self.openings;
        self.own += usize::from(matches!(packing, Packing::Own(_)));
        let replaced = self.open.insert(extent, Open { packing, opening, overwritten: 0 });
        debug_assert!(replaced.is_none(), "{extent:?} opened while open");
        opening
    }

    /// cladefs: a write to `extent`, open, wrote over `bytes` of what it held, in segments
    /// (TASK-79.2).
    pub fn wrote_over(&mut self, extent: ExtentId, bytes: u64) {
        if let Some(open) = self.open.get_mut(&extent) {
            open.overwritten += bytes;
            self.overwritten += bytes;
        }
    }

    /// cladefs: `bytes` that writes since the last commit wrote are no extent's any more: of an
    /// extent freed while open, or of a write dropped (TASK-79.2).
    pub fn let_go(&mut self, bytes: u64) {
        self.let_go += bytes;
    }

    /// cladefs: the bytes writes since the last commit wasted: what the open extents' writes wrote
    /// over, and what those freed, and writes dropped, let go of (TASK-79.2). The next commit takes
    /// them back: as dead space, which leaves with the segments once acknowledged in the bucket
    /// (TASK-165.8); and of raw files, which its flush removes.
    pub fn wasted(&self) -> u64 {
        self.overwritten + self.let_go
    }

    /// `extent`, freed, is open no more: where its data went, if it was. cladefs: what its writes
    /// wrote over is let go of with the rest of what they wrote, which its free lets go of; and its
    /// raw file, if it has one, which the next commit's flush removes.
    pub fn remove(&mut self, extent: ExtentId) -> Option<Packing<F>> {
        let open = self.open.remove(&extent)?;
        self.overwritten -= open.overwritten;
        self.own -= usize::from(matches!(open.packing, Packing::Own(_)));
        Some(open.packing)
    }

    /// cladefs: the bytes of the extents open, each as far as it goes ([`Uncommitted::grew`]).
    pub fn open_bytes(&self) -> u64 {
        self.open_bytes
    }

    /// cladefs: an extent open grew by `bytes`, a write finishing past its end.
    pub fn grew(&mut self, bytes: u64) {
        self.open_bytes += bytes;
    }

    /// cladefs: an extent open, of `bytes`, was freed.
    pub fn shrank(&mut self, bytes: u64) {
        self.open_bytes -= bytes;
    }

    /// The extents open, which the next commit seals, in the order they were opened. cladefs: an
    /// order the cell's writes decide, not the map's, which differs from one run to the next,
    /// so that where a commit seals each, and what its record says, do not (TASK-79.79).
    pub fn open_extents(&self) -> Vec<ExtentId> {
        let mut open: Vec<_> =
            self.open.iter().map(|(extent, open)| (open.opening, *extent)).collect();
        open.sort_unstable();
        open.into_iter().map(|(_, extent)| extent).collect()
    }

    pub fn push(&mut self, change: Change) {
        self.changes.push(change);
    }

    /// cladefs: the extents freed since the last commit, which the next frees.
    pub fn frees(&self) -> impl Iterator<Item = ExtentId> + '_ {
        self.changes.iter().filter_map(|change| match change {
            Change::Free(extent) => Some(*extent),
            _ => None,
        })
    }

    /// cladefs: the cell's capacity is `capacity`, as of the next commit.
    pub fn resize(&mut self, capacity: u64) {
        self.capacity = Some(capacity);
    }

    /// cladefs: the capacity set since the last commit, if any, for the commit, which sets it.
    pub fn take_capacity(&mut self) -> Option<u64> {
        self.capacity.take()
    }

    /// The changes since the last commit, for the commit, which seals the extents open.
    pub fn take(&mut self) -> Vec<Change> {
        self.let_go_open();
        self.overwritten = 0;
        self.let_go = 0;
        self.open_bytes = 0;
        std::mem::take(&mut self.changes)
    }

    /// cladefs: panics unless what it counts of the open extents, which `extents` holds as the
    /// writes that finished left them, is theirs (TASK-160.16): what their writes wrote over, in
    /// segments, the bytes of their pieces past what the extent holds of them. What writes since
    /// the last commit let go of, which nothing it holds says, it can't recount.
    #[cfg(any(test, feature = "audit"))]
    fn audit(&self, extents: &Extents) {
        let overwritten: u64 = self.open.values().map(|open| open.overwritten).sum();
        assert_eq!(self.overwritten, overwritten, "written over");
        let own = self.open.values().filter(|open| matches!(open.packing, Packing::Own(_)));
        assert_eq!(self.own, own.count(), "the extents open in raw files of their own");
        for (extent, open) in &self.open {
            assert!(open.opening <= self.openings, "{extent:?} opened past the openings");
            let entry = extents.get(extent).map(Entry::into_cow);
            let data = entry.as_deref().map(|entry| (entry.len, &entry.data));
            match (&open.packing, data) {
                (Packing::Segment, Some((len, Data::Pieces(pieces)))) => {
                    let written: u64 = pieces.iter().map(|piece| piece.len).sum();
                    let held = covered(pieces, 0, len);
                    assert_eq!(open.overwritten + held, written, "{extent:?}'s writes, open");
                }
                (Packing::Own(_), Some((_, Data::Own)))
                | (Packing::Segment | Packing::Own(_), None) => {
                    assert_eq!(open.overwritten, 0, "{extent:?}, in a raw file, wrote over");
                }
                (_, data) => panic!("{extent:?}, open, holds {data:?} where it is not placed"),
            }
        }
    }
}

/// The gate writes pass ([`LocalCell::begin_write`](crate::LocalCell::begin_write)): held while a
/// commit, or a closing between commits ([`LocalCell::close_now`](crate::LocalCell::close_now)),
/// waits for the writes in progress, which new ones wait for; and the writes in progress, to
/// segments or extents' own files: the extents a commit seals hold what they write.
#[derive(Default)]
pub(crate) struct Gate {
    held: bool,
    writing: u64,
}

impl Gate {
    pub fn is_held(&self) -> bool {
        self.held
    }

    /// Holds new writes back.
    pub fn hold(&mut self) {
        self.held = true;
    }

    /// Lets new writes pass.
    pub fn release(&mut self) {
        self.held = false;
    }

    /// A write passes, and is in progress.
    pub fn pass(&mut self) {
        self.writing += 1;
    }

    /// A write in progress is done, finished or abandoned: returns whether it was the last.
    pub fn done(&mut self) -> bool {
        self.writing -= 1;
        self.writing == 0
    }

    /// Whether writes are in progress.
    pub fn is_writing(&self) -> bool {
        self.writing > 0
    }
}

/// The files of a cell's extents stored on their own, sealed: those open, at most
/// `Options::open_files` but for those reads use, the least lately read closing as more open; the
/// bytes each takes; those written since the last commit, which its flush syncs, with their
/// directory.
pub(crate) struct OwnFiles<F> {
    open: Files<ExtentId, F>,
    sizes: HashMap<ExtentId, u64>,
    /// cladefs: in order, which their flush syncs them in, rather than the order of a map that
    /// differs from one run to the next (TASK-79.79).
    unsynced: BTreeMap<ExtentId, Arc<F>>,
    /// Their directory, the cell's, and whether its entries changed since the last commit.
    dir: String,
    written: bool,
    /// cladefs: the cell's keys, which its files are read under (TASK-411.3).
    keys: Arc<bucket_cell::keys::Ring>,
}

impl<F: File> OwnFiles<F> {
    /// `cell`'s, whose files take `sizes`, of which `limit` stay open, read under `keys`.
    pub fn new(
        cell: u64,
        sizes: HashMap<ExtentId, u64>,
        (limit, keys): (usize, Arc<bucket_cell::keys::Ring>),
    ) -> Self {
        let (dir, open, unsynced) = (cell_dir(EXTENTS, cell), Files::new(limit), BTreeMap::new());
        Self { open, sizes, unsynced, dir, written: false, keys }
    }

    /// `extent`'s file, opened in `dir` if it isn't.
    pub fn file<D: Dir<File = F>>(
        &mut self,
        dir: &D,
        extent: ExtentId,
    ) -> Result<Arc<Framed<F>>, Error> {
        if let Some(file) = self.open.get(extent) {
            return Ok(file);
        }
        let path = extent_path(extent);
        let keys = self.keys.of(extent.cell);
        let file = Arc::new(Framed::open(
            dir.open(&path, true)?,
            &path,
            &keys,
            crate::framed::unpacked_for(dir.host()),
        )?);
        Ok(self.open.insert(extent, file))
    }

    /// `extent`'s file is `framed`, just written: its commit's flush syncs it, and its directory.
    pub fn sealed(&mut self, extent: ExtentId, framed: Arc<Framed<F>>) {
        self.sizes.insert(extent, framed.size());
        self.unsynced.insert(extent, framed.file.clone());
        self.open.insert(extent, framed);
        self.written = true;
    }

    /// `extent`'s file goes: the extent was freed.
    pub fn forget(&mut self, extent: ExtentId) {
        self.open.remove(extent);
        self.sizes.remove(&extent);
        self.unsynced.remove(&extent);
    }

    /// Adds to `syncs` what the next commit's flush syncs of them: the files written since the
    /// last commit, and their directory.
    pub fn take_syncs(&mut self, syncs: &mut Syncs<F>) {
        for file in std::mem::take(&mut self.unsynced).into_values() {
            syncs.file(file);
        }
        if std::mem::take(&mut self.written) {
            syncs.dir(self.dir.clone());
        }
    }

    /// The bytes they take.
    pub fn bytes(&self) -> u64 {
        self.sizes.values().sum()
    }

    /// cladefs: panics unless they are the files of `sealed`, the cell's own extents sealed in
    /// files of their own, held or kept (TASK-160.16).
    #[cfg(any(test, feature = "audit"))]
    pub fn audit(&self, sealed: &HashSet<ExtentId>) {
        let files: HashSet<ExtentId> = self.sizes.keys().copied().collect();
        assert_eq!(&files, sealed, "the files of the extents sealed on their own");
    }
}

/// What a cell's next commit makes unreferenced: files of the cell's, which it removes once it
/// is durable.
#[derive(Default)]
pub(crate) struct Unreferenced {
    /// Raw files whose extents were sealed from them, or freed, and files of extents' data no
    /// extent is in any more: they wait for the cell's commits, in its queue of them, and are
    /// forgotten with a failed cell's, as its next opening removes what nothing refers to.
    pub doomed: Vec<String>,
}

impl Unreferenced {
    /// Those of the commit: from now on, the next's.
    pub fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// cladefs: a cell's standing: closing says whether it was open; failing keeps its first cause,
    /// and whether it was closed, closed or failed in either order; a failed cell refuses, saying
    /// why, and closes all the same, once (TASK-160.19).
    #[test]
    fn standings_fail_and_close_in_either_order() {
        let mut standing = Standing::default();
        assert!(standing.whole().is_ok() && !standing.is_closed());
        assert!(standing.close());
        assert!(!standing.close());
        standing.fail(&anyhow::anyhow!("the disk failed"));
        standing.fail(&anyhow::anyhow!("later"));
        assert!(standing.is_closed() && standing.failed());
        let refused = standing.whole().unwrap_err().to_string();
        assert_eq!(refused, "the extent store failed: the disk failed");

        let mut standing = Standing::default();
        standing.fail(&anyhow::anyhow!("the disk failed"));
        assert!(!standing.is_closed());
        assert!(standing.close());
        assert!(!standing.close());
        assert!(standing.is_closed() && standing.failed());
    }
}
