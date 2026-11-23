// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: an ephemeral cell (doc-25, TASK-698.2, board 6905): a child of a version of a cell kept
//! in the bucket, kept on this host's disk alone, never uploaded. Its parent's version is its
//! lower layer, read as every fork reads its origin; its own extents are its upper layer, in its
//! segments, framed in place as `Encryption::Sectors` in `segments/`.
//!
//! - **Its segments** close and frame through the same pipeline as any cell's, always in place as
//!   `Encryption::Sectors` (`segments/<cell>/<n>`, no `kept/` directory or extra `Phase`s). Reads
//!   decrypt sector-by-sector (`Source::Raw`) so freed 4 KiB runs can be punched in place without
//!   invalidating 1 MiB part tags. Its extents are all in segments: none is stored on its own.
//! - **Its local checkpoint**, `local-checkpoint` ([`LocalCheckpoint`]): encoded as an
//!   authenticated [`extent_frame::table`] (the same format as bucket checkpoints, authenticated
//!   under the cell's DEK, readable without keys for CRCs). It holds the cell's own extents
//!   (entries in the table) and, in the footer's `rest`, its parent origin, parent extents freed,
//!   root, next sequence, last commit, spent set, capacity, runs punched by the last checkpoint,
//!   last covered segment number (`through`), and wrapped keys.
//! - **A checkpoint** ([`LocalCell::checkpoint`]) covers closed, framed segments whose commits are
//!   local, in order: their footers applied to the local checkpoint, the local checkpoint written
//!   whole, then the write-ahead log's start moved past them, as an acknowledgement moves it.
//!   Covered segments are acknowledged in `Segments` so freed runs are given back
//!   ([`crate::File::punch`]) once nothing reads them and empty segments are reaped. A crash
//!   between the local checkpoint and the rest is finished by the next opening ([`settle`]),
//!   before its write-ahead log is recovered.
//! - **Recovery** applies the authenticated local checkpoint over its parent's version
//!   ([`EphemeralOpening::parent`]), then the footers recovery finds in `segments/` and `logs/`
//!   after `through` ([`crate::wal::recover`]); closes any open segment in place as
//!   `Encryption::Sectors`; removes any unreferenced segments `<= through`; and re-punches the runs
//!   the last checkpoint gave back.

#[cfg(test)]
mod tests;

use super::*;
use crate::layout::LOCAL_CHECKPOINT;
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
    /// The epoch of the parent's writer that wrote segment `at`.
    pub epoch: u64,
}

/// The non-extent state stored in `TableFooter::rest` of a [`LocalCheckpoint`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Rest {
    parent: Parent,
    freed: Vec<ExtentId>,
    root: Option<Vec<u8>>,
    next_sequence: u64,
    last_commit: Option<u64>,
    spent: Spent,
    capacity: Option<u64>,
    punch: Vec<(u64, u64, u64)>,
    through: Option<u64>,
    keys: Vec<u8>,
}

/// An ephemeral cell's local checkpoint (see the module's docs): its state, as of the last segment
/// a checkpoint covered, over its parent's version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalCheckpoint {
    parent: Parent,
    /// Its own extents, where they are in its segments ([`key`]), sorted by [`ExtentId`].
    own: Vec<(ExtentId, Location)>,
    /// The extents of its parent's version it freed.
    freed: Vec<ExtentId>,
    root: Option<Vec<u8>>,
    next_sequence: u64,
    last_commit: Option<u64>,
    spent: Spent,
    capacity: Option<u64>,
    /// The runs its last checkpoint gave back: of its segments, offsets and lengths.
    punch: Vec<(u64, u64, u64)>,
    /// The last segment a checkpoint covered, if any did.
    through: Option<u64>,
    /// Its keys, as its caller keeps them, wrapped.
    keys: Vec<u8>,
}

impl LocalCheckpoint {
    /// The local checkpoint of a new ephemeral cell, forked from `parent`, whose state at the
    /// version, as its child's, is `state`, its extents its parent's
    /// (`bucket_cell::State::inheriting` makes such); `keys` its keys, wrapped, as its caller keeps
    /// them.
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

    fn encode(&self, encrypting: (&extent_frame::Secret, extent_frame::KeyId)) -> Vec<u8> {
        let mut own = self.own.clone();
        own.sort_unstable_by_key(|(id, _)| *id);
        let rest = Rest {
            parent: self.parent,
            freed: self.freed.clone(),
            root: self.root.clone(),
            next_sequence: self.next_sequence,
            last_commit: self.last_commit,
            spent: self.spent.clone(),
            capacity: self.capacity,
            punch: self.punch.clone(),
            through: self.through,
            keys: self.keys.clone(),
        };
        let entries = own.iter().map(|(id, at)| (*id, at.key.as_str(), at.offset, at.len));
        extent_frame::table::encode(entries, bucket_cell::layout::encode(&rest), &[], encrypting)
    }

    fn decode(bytes: &[u8], keys: &dyn extent_frame::Keys) -> Result<Self, Error> {
        let extent_frame::table::Decoded { footer, entries, .. } =
            extent_frame::table::decode(bytes, keys)
                .context("decoding an ephemeral cell's local checkpoint")?;
        let Rest {
            parent,
            freed,
            root,
            next_sequence,
            last_commit,
            spent,
            capacity,
            punch,
            through,
            keys,
        } = bucket_cell::layout::decode(&footer.rest)
            .context("decoding an ephemeral cell's local checkpoint state")?;
        let own = entries
            .into_iter()
            .map(|entry| {
                (entry.id, Location { key: entry.key, offset: entry.offset, len: entry.len })
            })
            .collect();
        Ok(Self {
            parent,
            own,
            freed,
            root,
            next_sequence,
            last_commit,
            spent,
            capacity,
            punch,
            through,
            keys,
        })
    }

    /// The local checkpoint in `dir`, checked by its CRCs alone (no key required).
    pub fn read<D: Dir>(dir: &D) -> Result<Self, Error> {
        Self::read_under(dir, &extent_frame::NoKeys)
    }

    /// The local checkpoint in `dir`, authenticated under `keys`.
    pub(crate) fn read_under<D: Dir>(
        dir: &D,
        keys: &dyn extent_frame::Keys,
    ) -> Result<Self, Error> {
        let file = dir
            .open(LOCAL_CHECKPOINT, false)
            .context("opening an ephemeral cell's local checkpoint")?;
        Self::decode(&read_file(&file)?, keys)
    }

    /// Writes it in `dir`, whole, durably, authenticated under `encrypting`, over the one there.
    pub(crate) fn write<D: Dir>(
        &self,
        dir: &D,
        encrypting: (&extent_frame::Secret, extent_frame::KeyId),
    ) -> Result<(), Error> {
        dir.replace(LOCAL_CHECKPOINT, &self.encode(encrypting))
            .context("writing an ephemeral cell's local checkpoint")?;
        dir.sync_dir("").context("syncing an ephemeral cell's local checkpoint")?;
        Ok(())
    }
}

/// The key of the runs of segment `number` of `cell`, an ephemeral cell's (see the module's docs).
pub(crate) fn key(cell: CellId, number: u64) -> String {
    bucket_cell::layout::segment(cell, number, 0)
}

/// Parses the segment number out of an ephemeral segment [`key`] for `cell`.
fn number_of(cell: CellId, segment_key: &str) -> Option<u64> {
    let rest = segment_key.strip_prefix(&bucket_cell::layout::segments(cell))?;
    let (_, number) = rest.split_once('.')?;
    base32_id::parse_padded(number)
}

/// Finishes in `dir`, an ephemeral cell, `cell`'s, a checkpoint a crash cut short (see the module's
/// docs): removes any lingering `open-segments/` and `logs/` covered by `checkpoint.through`, and
/// moves the write-ahead log's start to `checkpoint.last_commit` if behind. Returns
/// `checkpoint.through` for [`crate::wal::recover`].
pub(crate) fn settle<D: Dir>(dir: &D, cell: CellId) -> Result<Option<u64>, Error> {
    let checkpoint = LocalCheckpoint::read(dir)?;
    if let Some(through) = checkpoint.through {
        let segment = |number| SegmentId { cell: cell.id, number };
        let open = list_numbers(dir, &cell_dir(OPEN_SEGMENTS, cell.id))?
            .into_iter()
            .filter(|&number| number <= through)
            .map(|number| open_segment_path(segment(number)));
        let logs = list_numbers(dir, &cell_dir(LOGS, cell.id))?
            .into_iter()
            .filter(|&number| number <= through)
            .map(|number| log_path(segment(number)));
        let mut removed = false;
        for path in open.chain(logs) {
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
    }
    let (mut start, at) = StartFile::read(dir)?;
    if at != checkpoint.last_commit {
        start.write(dir, checkpoint.last_commit)?;
    }
    Ok(checkpoint.through)
}

/// What an ephemeral cell opens with, besides its directory ([`Recovering::open_ephemeral`]).
pub struct EphemeralOpening {
    /// Its parent's state at the version it was forked from ([`LocalCheckpoint::parent`]), as
    /// decoded once for every fork of it ([`bucket_cell::Recovered::layer`]): its extents its lower
    /// layer.
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

/// An open ephemeral cell's local checkpoint as it stands (see the module's docs), which its
/// checkpoints move on, one at a time.
pub(crate) struct Ephemeral {
    parent: Parent,
    based: bucket_cell::State<Based>,
    through: Option<u64>,
    keys: Vec<u8>,
}

impl Ephemeral {
    /// The local checkpoint `checkpoint` of `cell`, as it stands.
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
            punch,
            through: self.through,
            keys: self.keys.clone(),
        }
    }
}

/// How many closed segments wait before a checkpoint covers them, but at a drain: what recovery
/// reads of segments and logs after the local checkpoint at most.
pub(crate) const CHECKPOINT_SEGMENTS: usize = 8;

impl<D: Dir> Recovering<D> {
    /// Whether it is an ephemeral cell (doc-25, TASK-698.2), which opens as one
    /// ([`Recovering::open_ephemeral`]).
    pub fn is_ephemeral(&self) -> bool {
        self.file.ephemeral
    }

    /// Its local checkpoint, of an ephemeral cell: what its opener reads its parent's state and its
    /// keys of.
    pub fn local_checkpoint(&self) -> Result<LocalCheckpoint, Error> {
        ensure!(self.file.ephemeral, "cell {} is no ephemeral cell's", self.file.cell);
        LocalCheckpoint::read(&*self.dir)
    }

    /// Opens an ephemeral cell as `opening` says (see the module's docs): its authenticated local
    /// checkpoint over its parent's version, then its segments after it (closing any open one in
    /// place as `Encryption::Sectors`); another segment opens, past them. Its extents are all in
    /// segments: `options`' large extents are none.
    pub fn open_ephemeral(
        self,
        mut options: Options,
        opening: EphemeralOpening,
    ) -> Result<LocalCell<D>, Error> {
        let Self { dir, taken, claim, file, wal: mut recovered } = self;
        let CellFile { block_size, cell, bucket, capacity, kind, version, ephemeral, .. } = file;
        ensure!(ephemeral, "cell {cell} is no ephemeral cell's");
        ensure!(version.is_none(), "an ephemeral cell is at no version");
        options.large_extent = u64::MAX;
        let EphemeralOpening { parent, keys, cache, held } = opening;
        // Authenticate the local checkpoint under the cell's own DEK.
        let checkpoint = LocalCheckpoint::read_under(&*dir, &keys.of(cell.id))?;
        let segment = |number| SegmentId { cell: cell.id, number };
        // Close any segments left open at a crash in place as Encryption::Sectors.
        recovered.close(&*dir, cell, &keys, true)?;

        // Its extents: its parent's version's, its local checkpoint's over them, its WAL's over
        // those.
        let mut by_key = HashMap::new();
        let mut covered_segments = BTreeSet::new();
        for (_, location) in &checkpoint.own {
            let number = number_of(cell, &location.key).with_context(|| {
                format!("bad segment key {:?} in cell {cell}'s local checkpoint", location.key)
            })?;
            by_key.insert(location.key.clone(), segment(number));
            covered_segments.insert(number);
        }
        for (_, number) in &recovered.segments {
            by_key.insert(key(cell, *number), segment(*number));
        }
        let lower = Extents::over(parent.extents.clone(), Some(parent.clone()), cell);
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

        // Remove any unreferenced segments <= through that a crash left behind after reaping, and
        // drop every unreferenced covered segment from `covered_segments` even if an earlier
        // reopen already unlinked its file before a second crash/kill (board 6905).
        if let Some(through) = checkpoint.through {
            let referenced: BTreeSet<u64> = extents
                .own()
                .flat_map(|(_, entry)| match &entry.data {
                    Data::Pieces(pieces) => pieces.as_slice(),
                    _ => &[],
                })
                .map(|piece| piece.segment.number)
                .collect();
            covered_segments.retain(|number| referenced.contains(number));
            for number in list_numbers(&*dir, &cell_dir(SEGMENTS, cell.id))? {
                if number <= through && !referenced.contains(&number) {
                    let path = segment_path(segment(number));
                    if let Err(err) = dir.remove(&path)
                        && !not_found(&err)
                    {
                        log::warn!("extent store: removing reaped {path}: {err}");
                    }
                }
            }
        }
        remove_unheld(&*dir, cell.id, &extents, &BTreeSet::new())?;

        // Its segments: acknowledged ones covered by the local checkpoint, and unacknowledged
        // ones from the WAL after it.
        let mut tracked: BTreeMap<SegmentId, (u64, bool)> = BTreeMap::new();
        for number in covered_segments {
            tracked.insert(segment(number), (0, true));
        }
        for (_, number) in &recovered.segments {
            tracked.insert(segment(*number), (0, false));
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
        let mut segments = Segments::open(&*dir, (cell.id, tracked), opened, files, true)?;
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
            let file = segments.closed_file(&*dir, segment(number));
            punch(file, segment(number), offset, len);
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

/// Gives back the `len` bytes at `offset` of `file`, segment `segment`'s: logged, not failed, if
/// the host can't, as the space alone is lost.
/// Gives back `len` bytes at `offset` of `segment`, `file` its file if it is closed: a failure,
/// opening it or punching it, said, the bytes staying taken (the punch a disk's room alone).
fn punch<F: File>(file: Result<Option<Arc<F>>, Error>, segment: SegmentId, offset: u64, len: u64) {
    let punched = file.and_then(|file| match file {
        Some(file) => Ok(file.punch(offset, len)?),
        None => Ok(()),
    });
    if let Err(err) = punched {
        log::warn!("extent store: giving back {len} bytes at {offset} of {segment:?}: {err:#}");
    }
}

impl<D: Dir> LocalCell<D> {
    /// cladefs: makes a new ephemeral cell in `dir` (doc-25, TASK-698.2, board 6905), as
    /// [`LocalCell::make`] does, its description naming `bucket`, its parent's, which its parent's
    /// extents are read in, and `kind`, its cell's; and `checkpoint`, its local checkpoint,
    /// authenticated under `encrypting`. Unmade until its maker says it is whole
    /// ([`LocalCell::made`]).
    pub fn make_ephemeral(
        dir: &D,
        description: Description,
        bucket: Option<&str>,
        kind: Option<Kind>,
        checkpoint: &LocalCheckpoint,
        encrypting: (&extent_frame::Secret, extent_frame::KeyId),
        extra: impl FnOnce(&mut crate::Making<'_, D>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        Self::make_described(dir, description, bucket, kind, (None, true), |making| {
            extra(making)?;
            let dir = making.dir();
            checkpoint.write(dir, encrypting)?;
            StartFile::read(dir)?.0.write(dir, checkpoint.last_commit)
        })
    }

    /// cladefs: whether its cell is ephemeral (doc-25, TASK-698.2).
    pub fn is_ephemeral(&self) -> bool {
        self.ephemeral.is_some()
    }

    /// cladefs: of an ephemeral cell, covers the closed, framed segments whose commits are local,
    /// in order, if there are [`CHECKPOINT_SEGMENTS`] of them, or any if `drain` (see the module's
    /// docs): returns whether it covered any. Nothing, of a cell kept in the bucket. A failure
    /// fails the cell, as what it holds may be ahead of its files.
    pub async fn checkpoint(&self, drain: bool) -> Result<bool, Error> {
        let Some(ephemeral) = &self.ephemeral else { return Ok(false) };
        let durable = self.durability.with(Mutex::lock).local();
        let wait_through = {
            let state = self.state()?;
            let covered = state.wal.covering(durable);
            if covered.is_empty() || (!drain && covered.len() < CHECKPOINT_SEGMENTS) {
                return Ok(false);
            }
            covered.last().map(|(number, _)| *number)
        };
        if drain {
            self.wait_framed().await?;
        } else if let Some(last) = wait_through {
            self.wait_framed_through(last).await?;
        }
        // cladefs: the flush's lock first, as a flush takes it, then the cell's own: the segments
        // a checkpoint covers leave the logs a flush holds (board 2944).
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
        let covered = {
            let state = self.state()?;
            let mut covered = state.wal.covering(durable);
            if let Some(at) =
                covered.iter().position(|(number, _)| state.segments.is_framing(segment(*number)))
            {
                covered.truncate(at);
                while covered.last().is_some_and(|(_, footer)| footer.contents.continued.is_some())
                {
                    covered.pop();
                }
            }
            if covered.is_empty() || (!drain && covered.len() < CHECKPOINT_SEGMENTS) {
                return Ok(false);
            }
            covered
        };
        // The local checkpoint, its footers applied; what they let go of among its own extents'
        // runs, each where it was, to give back.
        let before = ephemeral.based.extents.own.clone();
        for (number, footer) in &covered {
            ephemeral.based.apply_contents(*number, 0, &footer.contents);
        }
        ensure!(ephemeral.based.begun.is_none(), "a checkpoint of cell {cell} inside a commit");
        let last = covered.last().map(|(number, _)| *number).expect("covers some");
        ephemeral.through = Some(last);
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
            .filter_map(|(_, at)| Some((number_of(cell, &at.key)?, at.offset, at.len)))
            .collect();

        // All covered segments are already framed durably in `segments/<cell>/<n>`.
        ephemeral.local_checkpoint(given_back).write(&*self.dir, self.keys.encrypting())?;
        self.start.with(Mutex::lock).write(&*self.dir, ephemeral.based.last_commit)?;
        for (number, _) in &covered {
            logs.by_segment.remove(&segment(*number));
        }

        // Acknowledge covered segments so freed runs are punched once released, and empty
        // segments are reaped uniformly with bucket cells.
        let (reaped, punches, raw) = {
            let mut state = self.state()?;
            let state = &mut *state;
            for (number, _) in &covered {
                state.segments.acknowledge(segment(*number), None);
            }
            for (number, _) in &covered {
                let whole = state.wal.acknowledge(*number);
                let whole =
                    whole.with_context(|| format!("{:?} first no more", segment(*number)))?;
                for queued in &whole {
                    state.release_kept(footer_last(&queued.footer));
                }
            }
            state.trim_kept(state.epoch);
            let reaped = state.segments.reap();
            let punches = state.segments.take_punches();
            let raw = state.segments.raw(durable);
            (reaped, punches, raw)
        };
        self.remove_raw(raw);
        self.give_back(punches);
        for (number, _) in reaped {
            let path = segment_path(number);
            if let Err(err) = self.dir.remove(&path)
                && !not_found(&err)
            {
                log::warn!("extent store: failed to remove {path}: {err}");
            }
        }
        Ok(true)
    }

    /// cladefs: gives back `punches`, runs of acknowledged ephemeral segments that nothing reads
    /// any more ([`Segments::take_punches`], TASK-698.2, board 6905).
    pub(crate) fn give_back(&self, punches: Vec<(SegmentId, u64, u64)>) {
        for (number, offset, len) in punches {
            let file = self.lock().segments.closed_file(&*self.dir, number);
            punch(file, number, offset, len);
        }
    }
}
