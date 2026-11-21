// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: an ephemeral cell's store (doc-25, TASK-698.2): a child of a version of a cell kept in
//! the bucket, kept on this host's disk alone, never framed nor uploaded. Its parent's version is
//! its lower layer, read as every fork reads its origin; its own extents are its upper layer, in
//! its segments, raw.
//!
//! - **Its segments** close as any store's do, and stay raw ([`Segments`]'s `Raw` phase): read from
//!   their raw files, their logs beside them, as recovery closes again a segment a crash left
//!   torn. Its extents are all in segments: none is stored on its own.
//! - **Its local checkpoint**, `local-checkpoint` ([`LocalCheckpoint`]): its state as of the last segment a checkpoint covered, what it
//!   holds over its parent's version: its own extents, where they are, and those of its parent's
//!   it freed, with its root, its next sequence number, the sequence numbers it spent and its
//!   capacity; the headers of its kept segments' logs, which say how their raw files are
//!   encrypted; the runs its last checkpoint gave back; and its keys, its caller's, wrapped.
//! - **A checkpoint** ([`LocalCell::checkpoint`]) covers the closed segments whose commits are
//!   local, in order: their footers applied to the local checkpoint, the local checkpoint written whole, then their raw
//!   files moved to `kept/` and their logs removed, then the write-ahead log's start moved past
//!   them, as an acknowledgement moves it. What they freed of the cell's own extents is given back
//!   ([`crate::File::punch`]) once nothing reads it: no view, kept extents letting go of them as
//!   for the bucket's. A crash between the local checkpoint and the rest is finished by the next opening
//!   ([`settle`]), before its write-ahead log is recovered, which would take a segment the start
//!   covers for one the bucket acknowledged.
//! - **Recovery** applies the local checkpoint over its parent's version, which the opener gives
//!   ([`EphemeralOpening::parent`]), then the footers recovery folds of the logs of the segments
//!   after it, as it closes them again ([`crate::wal::recover`]); and gives back again the runs the
//!   last checkpoint gave back, a crash maybe cutting it short. A segment's runs are keyed by its
//!   number here, epoch 0 ([`key`]), not by the footers' chain, which recovery stamps anew.

#[cfg(test)]
mod tests;

use super::*;
use crate::layout::{KEPT, LOCAL_CHECKPOINT, kept_path};
use crate::segments::EphemeralSegment;
use extent_frame::spent::Spent;
use serde::{Deserialize, Serialize};

/// The version of a cell kept in the bucket an ephemeral cell was forked from: what its opener
/// reads its parent's state of ([`EphemeralOpening::parent`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parent {
    pub cell: CellId,
    pub version: u64,
    /// The segment of the parent's the version is at: its origin.
    pub at: u64,
}

/// An ephemeral cell's local checkpoint (see the module's docs): its state, as of the last segment a checkpoint
/// covered, over its parent's version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalCheckpoint {
    parent: Parent,
    /// Its own extents, where they are in its kept segments ([`key`]).
    own: Vec<(ExtentId, Location)>,
    /// The extents of its parent's version it freed.
    freed: Vec<ExtentId>,
    root: Option<Vec<u8>>,
    next_sequence: u64,
    last_commit: Option<u64>,
    spent: Spent,
    capacity: Option<u64>,
    /// Its kept segments, by their numbers, with their logs' headers.
    kept: BTreeMap<u64, Vec<u8>>,
    /// The runs its last checkpoint gave back: of its kept segments, offsets and lengths.
    punch: Vec<(u64, u64, u64)>,
    /// The last segment a checkpoint covered, if any did.
    through: Option<u64>,
    /// Its keys, as its caller keeps them, wrapped.
    keys: Vec<u8>,
}

/// What a local checkpoint's file starts with.
const MAGIC: &[u8; 8] = b"cfsbase1";

impl LocalCheckpoint {
    /// The local checkpoint of a new ephemeral cell, forked from `parent`, whose state at the version, as its
    /// child's, is `state`, its extents its parent's (`bucket_cell::State::inheriting` makes such);
    /// `keys` its keys, wrapped, as its caller keeps them.
    pub fn fork(parent: Parent, state: &bucket_cell::State, keys: Vec<u8>) -> Self {
        Self {
            parent,
            own: Vec::new(),
            freed: Vec::new(),
            root: state.root.clone(),
            next_sequence: state.next_sequence,
            last_commit: state.last_commit,
            // The parent's spent are of its own extents, none of the child's.
            spent: Spent::default(),
            capacity: state.capacity,
            kept: BTreeMap::new(),
            punch: Vec::new(),
            through: None,
            keys,
        }
    }

    /// The version it was forked from.
    pub fn parent(&self) -> Parent {
        self.parent
    }

    /// Its keys, wrapped, as its caller gave them.
    pub fn keys(&self) -> &[u8] {
        &self.keys
    }

    fn encode(&self) -> Vec<u8> {
        let body = bucket_cell::layout::encode(self);
        let mut bytes = MAGIC.to_vec();
        bytes.extend(extent_frame::crc(&body).to_le_bytes());
        bytes.extend(body);
        bytes
    }

    fn decode(bytes: &[u8]) -> Result<Self, Error> {
        ensure!(
            bytes.get(..MAGIC.len()) == Some(&MAGIC[..]),
            "not an ephemeral cell's local checkpoint"
        );
        let crc =
            bytes.get(MAGIC.len()..MAGIC.len() + 4).context("a local checkpoint cut short")?;
        let body = &bytes[MAGIC.len() + 4..];
        ensure!(
            u32::from_le_bytes(crc.try_into().unwrap()) == extent_frame::crc(body),
            "an ephemeral cell's local checkpoint fails its CRC"
        );
        Ok(bucket_cell::layout::decode(body)?)
    }

    /// The local checkpoint in `dir`.
    pub fn read<D: Dir>(dir: &D) -> Result<Self, Error> {
        let file = dir
            .open(LOCAL_CHECKPOINT, false)
            .context("opening an ephemeral cell's local checkpoint")?;
        Self::decode(&read_file(&file)?)
    }

    /// Writes it in `dir`, whole, durably, over the one there.
    pub(crate) fn write<D: Dir>(&self, dir: &D) -> Result<(), Error> {
        dir.replace(LOCAL_CHECKPOINT, &self.encode())
            .context("writing an ephemeral cell's local checkpoint")?;
        dir.sync_dir("").context("syncing an ephemeral cell's local checkpoint")?;
        Ok(())
    }
}

/// The key of the runs of segment `number` of `cell`, an ephemeral cell's (see the module's docs).
pub(crate) fn key(cell: CellId, number: u64) -> String {
    bucket_cell::layout::segment(cell, number, 0)
}

/// Finishes in `dir`, an ephemeral cell's store, `cell`'s, a checkpoint a crash cut short (see the
/// module's docs): the raw files of the segments its local checkpoint covers moved to `kept/`, or removed if it
/// keeps none of them, their logs removed, and the write-ahead log's start its local checkpoint's last commit.
/// Before its write-ahead log is recovered, which would take a segment the start covers for one
/// the bucket acknowledged, and remove it.
pub(crate) fn settle<D: Dir>(dir: &D, cell: CellId) -> Result<(), Error> {
    let checkpoint = LocalCheckpoint::read(dir)?;
    if let Some(through) = checkpoint.through {
        let segment = |number| SegmentId { cell: cell.id, number };
        let covered: Vec<u64> = list_numbers(dir, &cell_dir(OPEN_SEGMENTS, cell.id))?
            .into_iter()
            .filter(|&number| number <= through)
            .collect();
        let kept = covered.iter().copied().filter(|number| checkpoint.kept.contains_key(number));
        keep(dir, cell, &kept.map(segment).collect::<Vec<_>>())?;
        let logs = list_numbers(dir, &cell_dir(LOGS, cell.id))?;
        let logs = logs.into_iter().filter(|&number| number <= through);
        let paths = (covered.iter().map(|&number| open_segment_path(segment(number))))
            .chain(logs.map(|number| log_path(segment(number))));
        unlink(dir, cell, paths)?;
    }
    let (mut start, at) = StartFile::read(dir)?;
    if at != checkpoint.last_commit {
        start.write(dir, checkpoint.last_commit)?;
    }
    Ok(())
}

/// What an ephemeral cell's store opens with, besides its directory ([`Recovering::open_ephemeral`]).
pub struct EphemeralOpening {
    /// Its parent's state at the version it was forked from ([`LocalCheckpoint::parent`]), as decoded once for
    /// every fork of it ([`bucket_cell::Recovered::layer`]): its extents its lower layer.
    pub parent: Arc<bucket_cell::State>,
    /// Its keys: its own DEK, which its segments are encrypted under, and those it inherits.
    pub keys: Arc<Ring>,
    /// The host's cache of the bucket's objects, which its parent's are read through.
    pub cache: Option<Arc<Cache>>,
    /// What holds the parts of segments packed as they fill ([`Opening::held`]).
    pub held: Option<Arc<HeldParts>>,
}

/// An ephemeral cell's own extents and those of its parent's it freed, as a replay of its footers
/// over its local checkpoint places them ([`Ephemeral`]).
#[derive(Clone, Debug, Default)]
struct Based {
    own: BTreeMap<ExtentId, Location>,
    freed: BTreeSet<ExtentId>,
}

impl Places for Based {
    fn seal(&mut self, extent: ExtentId, (Seal::Run(location) | Seal::Own(location)): Seal) {
        self.own.insert(extent, location);
    }

    fn free(&mut self, extent: ExtentId) {
        if self.own.remove(&extent).is_none() {
            self.freed.insert(extent);
        }
    }
}

/// An open ephemeral cell's local checkpoint as it stands (see the module's docs), which its checkpoints move
/// on, one at a time.
pub(crate) struct Ephemeral {
    parent: Parent,
    based: bucket_cell::State<Based>,
    /// Its kept segments, by their numbers, with their logs' headers: those reaped since go.
    kept: BTreeMap<u64, Vec<u8>>,
    through: Option<u64>,
    keys: Vec<u8>,
}

impl Ephemeral {
    /// The local checkpoint `local-checkpoint` of `cell`, as it stands.
    fn of(cell: CellId, checkpoint: &LocalCheckpoint) -> Self {
        let own = checkpoint.own.iter().cloned().collect();
        let freed = checkpoint.freed.iter().copied().collect();
        let based = bucket_cell::State {
            root: checkpoint.root.clone(),
            next_sequence: checkpoint.next_sequence,
            last_commit: checkpoint.last_commit,
            spent: checkpoint.spent.clone(),
            capacity: checkpoint.capacity,
            ..bucket_cell::State::of(cell, Based { own, freed })
        };
        Self {
            parent: checkpoint.parent,
            based,
            kept: checkpoint.kept.clone(),
            through: checkpoint.through,
            keys: checkpoint.keys.clone(),
        }
    }

    /// Its local checkpoint, as it stands, its last checkpoint having given back `punch`.
    fn local_checkpoint(&self, punch: Vec<(u64, u64, u64)>) -> LocalCheckpoint {
        let based = &self.based;
        LocalCheckpoint {
            parent: self.parent,
            own: based.extents.own.iter().map(|(extent, at)| (*extent, at.clone())).collect(),
            freed: based.extents.freed.iter().copied().collect(),
            root: based.root.clone(),
            next_sequence: based.next_sequence,
            last_commit: based.last_commit,
            spent: based.spent.clone(),
            capacity: based.capacity,
            kept: self.kept.clone(),
            punch,
            through: self.through,
            keys: self.keys.clone(),
        }
    }
}

/// How many closed segments wait before a checkpoint covers them, but at a drain: what recovery
/// reads of logs and raw data at most, of a segment's size each, and what a checkpoint writes the
/// local checkpoint for.
pub(crate) const CHECKPOINT_SEGMENTS: usize = 8;

impl<D: Dir> Recovering<D> {
    /// Whether it is an ephemeral cell's store (doc-25, TASK-698.2), which opens as one
    /// ([`Recovering::open_ephemeral`]).
    pub fn is_ephemeral(&self) -> bool {
        self.file.ephemeral
    }

    /// Its local checkpoint, of an ephemeral cell's store: what its opener reads its parent's state and its
    /// keys of.
    pub fn local_checkpoint(&self) -> Result<LocalCheckpoint, Error> {
        ensure!(self.file.ephemeral, "cell {}'s store is no ephemeral cell's", self.file.cell);
        LocalCheckpoint::read(&*self.dir)
    }

    /// Opens an ephemeral cell's store as `opening` says (see the module's docs): its local checkpoint over its
    /// parent's version, then its segments' logs after it, closed again; another segment opens,
    /// past them. Its extents are all in segments: `options`' large extents are none.
    pub fn open_ephemeral(
        self,
        mut options: Options,
        opening: EphemeralOpening,
    ) -> Result<LocalCell<D>, Error> {
        let Self { dir, taken, claim, file, wal: recovered } = self;
        let CellFile { block_size, cell, bucket, capacity, kind, version, ephemeral, .. } = file;
        ensure!(ephemeral, "cell {cell}'s store is no ephemeral cell's");
        ensure!(version.is_none(), "an ephemeral cell's store is at no version");
        options.large_extent = u64::MAX;
        let EphemeralOpening { parent, keys, cache, held } = opening;
        let checkpoint = LocalCheckpoint::read(&*dir)?;
        let framed = recovered.segments.iter().find(|(_, n)| !recovered.closing.contains_key(n));
        ensure!(framed.is_none(), "ephemeral cell {cell}'s store holds a framed segment");
        let segment = |number| SegmentId { cell: cell.id, number };
        // Its segments closed again hold their commits' runs, which their logs alone may have held.
        recovered.restore_raw(&*dir, cell, &keys)?;

        // Its extents: its parent's version's, its local checkpoint's over them, its log's over those.
        let mut by_key = HashMap::new();
        for &number in checkpoint.kept.keys() {
            by_key.insert(key(cell, number), segment(number));
        }
        for (_, number) in &recovered.segments {
            by_key.insert(key(cell, *number), segment(*number));
        }
        let lower = Extents::over(parent.extents.clone(), Some(parent.clone()));
        let mut replay = bucket_cell::State {
            root: checkpoint.root.clone(),
            next_sequence: checkpoint.next_sequence,
            last_commit: checkpoint.last_commit,
            spent: checkpoint.spent.clone(),
            capacity: checkpoint.capacity,
            ..bucket_cell::State::of(cell, Local { extents: lower, segments: by_key })
        };
        for &extent in &checkpoint.freed {
            replay.extents.free(extent);
        }
        for (extent, location) in &checkpoint.own {
            ensure!(
                replay.extents.segments.contains_key(&location.key),
                "{extent:?} is in no segment cell {cell}'s local checkpoint keeps"
            );
            replay.extents.seal(*extent, Seal::Run(location.clone()));
        }
        for (footer, number) in &recovered.segments {
            replay.apply_contents(*number, 0, &footer.contents);
        }
        ensure!(replay.begun.is_none(), "cell {cell}'s log ends inside a commit");
        let epoch = replay.last_commit.map_or(0, epoch_of);
        let restate = replay.capacity.is_none();
        let capacity = replay.capacity.unwrap_or(capacity);
        let bucket_cell::State { extents: Local { extents, .. }, root, spent, .. } = replay;
        let own = extents.lens().filter(|(extent, _)| extent.cell == cell.id);
        let next_sequence = own.map(|(extent, _)| extent.sequence + 1).max().unwrap_or(0);
        let root = root.unwrap_or_default();
        remove_unheld(&*dir, cell.id, &extents, &BTreeSet::new())?;

        // Its segments: those it keeps, and those closed raw after them, with the bytes of their
        // extents' data each holds.
        let mut tracked = BTreeMap::new();
        for (&number, header) in &checkpoint.kept {
            // One reaped after the local checkpoint named it, no extent in it, is gone, or goes.
            let path = kept_path(segment(number));
            match dir.open(&path, true) {
                Err(err) if not_found(&err) => continue,
                opened => drop(opened.with_context(|| format!("opening {path}"))?),
            }
            tracked.insert(segment(number), (0, EphemeralSegment::Kept { header: header.clone() }));
        }
        for (_, number) in &recovered.segments {
            tracked.insert(segment(*number), (0, EphemeralSegment::Raw));
        }
        let holding = Holding::of(cell.id, extents.lens());
        for (extent, entry) in extents.own() {
            match &entry.data {
                Data::Pieces(pieces) => {
                    for piece in pieces {
                        let held = tracked.get_mut(&piece.segment);
                        let held = held.with_context(|| format!("{extent:?} in no segment"))?;
                        held.0 += piece.len;
                    }
                }
                Data::Own => bail!("{extent:?} of ephemeral cell {cell} is stored on its own"),
                Data::Bucket(_) => {}
            }
        }
        let chain = Chain::new(cell, 0, recovered.next, 0);
        let opened = (epoch, recovered.next, chain);
        let files = (options.open_files, keys.clone());
        let mut segments = Segments::open_ephemeral(&*dir, (cell.id, tracked), opened, files)?;
        for (extent, entry) in extents.own() {
            segments.place(*extent, &entry.data);
        }
        let queued = (recovered.segments.iter()).map(|(footer, number)| {
            let bytes = segments.len(segment(*number));
            Queued::new(*number, footer.clone(), false, bytes)
        });
        let queue = Queue::new(recovered.start, queued.collect::<Vec<_>>());
        // What its last checkpoint gave back, again: a crash may have cut it short.
        for &(number, offset, len) in &checkpoint.punch {
            if let Some(raw) = segments.kept_file(segment(number)) {
                punch(&*raw, segment(number), offset, len);
            }
        }
        let assembled = Assembled {
            options,
            extents,
            capacity,
            restate,
            holding,
            epoch,
            root,
            next_sequence,
            spent,
            segments,
            own_sizes: HashMap::new(),
            queue,
            version: false,
            cuts: BTreeMap::new(),
            start_file: recovered.start_file,
            cache,
            held,
            keys,
            ephemeral: Some(Ephemeral::of(cell, &checkpoint)),
        };
        Ok(LocalCell::assemble(
            (dir, taken, claim),
            (block_size, cell, bucket, kind, version),
            assembled,
        ))
    }
}

/// Gives back the `len` bytes at `offset` of `raw`, segment `segment`'s, kept: logged, not failed,
/// if the host can't, as the space alone is lost.
fn punch<F: File>(raw: &F, segment: SegmentId, offset: u64, len: u64) {
    if let Err(err) = raw.punch(offset, len) {
        log::warn!("extent store: giving back {len} bytes at {offset} of {segment:?}: {err}");
    }
}

impl<D: Dir> LocalCell<D> {
    /// cladefs: makes a new ephemeral cell's store in `dir` (doc-25, TASK-698.2), as
    /// [`LocalCell::make`] does, its description naming `bucket`, its parent's, which its parent's
    /// extents are read in, and `kind`, its cell's; and `local-checkpoint`, its local checkpoint, its parent's version's
    /// state at the fork. Unmade until its maker says it is whole ([`LocalCell::made`]).
    pub fn make_ephemeral(
        dir: &D,
        description: Description,
        bucket: Option<&str>,
        kind: Option<Kind>,
        checkpoint: &LocalCheckpoint,
    ) -> Result<(), Error> {
        Self::make_described(dir, description, bucket, kind, (None, true), |dir| {
            create_dir(dir, KEPT)?;
            create_dir(dir, &cell_dir(KEPT, description.cell.id))?;
            checkpoint.write(dir)?;
            StartFile::read(dir)?.0.write(dir, checkpoint.last_commit)
        })
    }

    /// cladefs: whether its cell is ephemeral (doc-25, TASK-698.2).
    pub fn is_ephemeral(&self) -> bool {
        self.ephemeral.is_some()
    }

    /// cladefs: of an ephemeral cell, covers the closed segments whose commits are local, in order, if
    /// there are [`CHECKPOINT_SEGMENTS`] of them, or any if `drain` (see the module's docs): returns
    /// whether it covered any. Nothing, of a cell kept in the bucket. A failure fails the store, as
    /// what it holds may be ahead of its files.
    pub async fn checkpoint(&self, drain: bool) -> Result<bool, Error> {
        let Some(ephemeral) = &self.ephemeral else { return Ok(false) };
        // cladefs: the flush's lock first, as a flush takes it, then the cell's own: the segments
        // a checkpoint covers leave the logs a flush holds (TASK-685.17).
        let mut logs = self.flushing.lock().await;
        let mut ephemeral = ephemeral.lock().unwrap();
        let checkpointed =
            crate::upload::blocking(|| self.checkpoint_locked(&mut ephemeral, &mut logs, drain));
        if let Err(error) = &checkpointed {
            self.lock().fail(error);
        }
        checkpointed
    }

    /// [`LocalCell::checkpoint`], holding `ephemeral`.
    fn checkpoint_locked(
        &self,
        ephemeral: &mut Ephemeral,
        logs: &mut Logs<D::File>,
        drain: bool,
    ) -> Result<bool, Error> {
        let cell = self.cell;
        let segment = |number| SegmentId { cell: cell.id, number };
        let durable = self.durability.with(Mutex::lock).local();
        let (covered, headers) = {
            let state = self.state()?;
            let covered = state.wal.covering(durable);
            if covered.is_empty() || (!drain && covered.len() < CHECKPOINT_SEGMENTS) {
                return Ok(false);
            }
            let headers: Vec<_> = (covered.iter())
                .map(|(number, _)| (*number, state.segments.raw_header(segment(*number))))
                .collect();
            (covered, headers)
        };
        // The local checkpoint, its footers applied; what they let go of among its own extents' runs, each
        // where it was, to give back.
        let before = ephemeral.based.extents.own.clone();
        for (number, footer) in &covered {
            ephemeral.based.apply_contents(*number, 0, &footer.contents);
        }
        ensure!(ephemeral.based.begun.is_none(), "a checkpoint of cell {cell} inside a commit");
        for (number, header) in headers {
            let header =
                header.with_context(|| format!("{:?} is not closed raw", segment(number)))?;
            ephemeral.kept.insert(number, header);
        }
        let last = covered.last().map(|(number, _)| *number).expect("covers some");
        ephemeral.through = Some(last);
        let numbers: HashMap<String, u64> =
            ephemeral.kept.keys().map(|&number| (key(cell, number), number)).collect();
        let sealed: Vec<(ExtentId, Location)> = (covered.iter())
            .flat_map(|(number, footer)| {
                let key = key(cell, *number);
                let continued = footer.contents.continued.iter().flat_map(|c| &c.extents);
                (footer.contents.extents.iter().chain(continued)).map(
                    move |&(extent, offset, len)| {
                        (extent, Location { key: key.clone(), offset, len })
                    },
                )
            })
            .collect();
        let after = &ephemeral.based.extents.own;
        let given_back: Vec<(u64, u64, u64)> = (before.into_iter().chain(sealed))
            .filter(|(extent, at)| after.get(extent) != Some(at))
            .filter_map(|(_, at)| Some((*numbers.get(&at.key)?, at.offset, at.len)))
            .collect();
        // Their raw files hold their commits' runs, which a flush that sealed little made durable in
        // their logs alone, which go.
        for (number, _) in &covered {
            let raw = self.lock().segments.raw_file(&*self.dir, segment(*number))?;
            raw.sync().with_context(|| format!("syncing {:?}", segment(*number)))?;
        }
        ephemeral.local_checkpoint(given_back).write(&*self.dir)?;

        // Its segments kept: their raw files in `kept/`, then their raw files' old names and their
        // logs gone, durably; then its start.
        let numbers: Vec<SegmentId> = covered.iter().map(|(number, _)| segment(*number)).collect();
        keep(&*self.dir, cell, &numbers)?;
        let gone = numbers.iter().flat_map(|&number| [open_segment_path(number), log_path(number)]);
        unlink(&*self.dir, cell, gone)?;
        self.start.with(Mutex::lock).write(&*self.dir, ephemeral.based.last_commit)?;
        for (number, _) in &covered {
            logs.by_segment.remove(&segment(*number));
        }

        // What they freed lets go of their runs once views need them no more, which gives them
        // back ([`Segments::kept`]).
        let (reaped, punches) = {
            let mut state = self.state()?;
            let state = &mut *state;
            for (number, _) in &covered {
                state.segments.kept(segment(*number));
            }
            for (number, _) in &covered {
                let whole = state.wal.acknowledge(*number);
                let whole =
                    whole.with_context(|| format!("{:?} first no more", segment(*number)))?;
                for queued in &whole {
                    state.release_kept(footer_last(&queued.footer));
                }
            }
            state.trim_kept(&self.views.keeping(), state.epoch);
            state.segments.reap();
            (state.segments.take_kept_reaped(), state.segments.take_punches())
        };
        self.give_back(punches);
        for number in reaped {
            ephemeral.kept.remove(&number.number);
            let path = kept_path(number);
            if let Err(err) = self.dir.remove(&path)
                && !not_found(&err)
            {
                log::warn!("extent store: failed to remove {path}: {err}");
            }
        }
        Ok(true)
    }

    /// cladefs: gives back `punches`, runs of kept segments that nothing reads any more
    /// ([`Segments::take_punches`], TASK-698.2).
    pub(crate) fn give_back(&self, punches: Vec<(SegmentId, u64, u64)>) {
        for (number, offset, len) in punches {
            let raw = self.lock().segments.kept_file(number);
            if let Some(raw) = raw {
                punch(&*raw, number, offset, len);
            }
        }
    }
}

/// Gives each of `segments`, `cell`'s, its name in `kept/`, its raw file's other name, in
/// `open-segments/`, left, unless it has one there: durably, before any old name goes, as a crash
/// may keep each directory's entries apart.
fn keep<D: Dir>(dir: &D, cell: CellId, segments: &[SegmentId]) -> Result<(), Error> {
    for &segment in segments {
        let (from, to) = (open_segment_path(segment), kept_path(segment));
        match dir.open(&to, true) {
            Ok(_) => continue,
            Err(err) if not_found(&err) => {}
            Err(err) => return Err(Error::new(err).context(format!("opening {to}"))),
        }
        dir.link(&from, &to).with_context(|| format!("keeping {from}"))?;
    }
    if !segments.is_empty() {
        dir.sync_dir(&cell_dir(KEPT, cell.id))?;
    }
    Ok(())
}

/// Removes the files at `paths`, `cell`'s, unless gone, durably.
fn unlink<D: Dir>(
    dir: &D,
    cell: CellId,
    paths: impl IntoIterator<Item = String>,
) -> Result<(), Error> {
    let mut removed = false;
    for path in paths {
        match dir.remove(&path) {
            Err(err) if not_found(&err) => {}
            gone => gone.with_context(|| format!("removing {path}"))?,
        }
        removed = true;
    }
    if removed {
        for kind in [OPEN_SEGMENTS, LOGS] {
            dir.sync_dir(&cell_dir(kind, cell.id))?;
        }
    }
    Ok(())
}

/// Makes the directory at `path` in `dir`, unless it is there.
fn create_dir<D: Dir>(dir: &D, path: &str) -> Result<(), Error> {
    match dir.create_dir(path) {
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        made => made.with_context(|| format!("creating {path}")),
    }
}
