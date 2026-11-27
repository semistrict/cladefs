// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The cell. See the crate's docs.

use crate::dir::{Dir, File, not_found, read_file};
use crate::durability::{Due, Durability, FileSyncs, Syncs};
use crate::extents::{Entry, Extents, Sparse};
use crate::framed::{Aligned, Framed, Located, Packed, Source, read_aligned};
use crate::framing::{self, Look, Seen};
use crate::held::{HeldParts, Hold};
use crate::kept::{self, Kept, KeptExtents};
use crate::layout::{
    CellFile, Change, DESCRIPTION, Data, Description, EXTENTS, Extent, FileKey, LOGS, OPEN_EXTENTS,
    Piece, Place, SEGMENTS, SegmentId, add_piece, cell_dir, create_cell_dirs, extent_path,
    files_of, list_numbers, log_path, numbered_path, open_extent_path, open_segment_path,
    segment_path, spare_path,
};
use crate::making::Making;
use crate::pool::spawn_or_return;
use crate::raw::{RawEncryption, RawFile};
use crate::replicate::{Flushed, Replicator, log_of, own_frame, own_of};
use crate::seals;
use crate::segments::{self, Behind, LogInit, SECTOR, Segments, Unframed, Unsectored, footer_last};
use crate::shared::SharedLock;
use crate::state::{
    Framing, Gate, Holding, Job, Life, Locked, OwnFiles, Packing, Readable, Reader, Space,
    Standing, State, Uncommitted, Unreferenced, Writable,
};
use crate::sync::{AsyncMutex, AsyncMutexGuard, Event, Mutex, rules};
use crate::timings::{self, Flush, Waits, flush};
use crate::uploads::Closed;
use crate::wal::{
    LOG_HEADER, LogRecord, Queue, Queued, Recovered, ReplicaLog, Runs, SegmentLog, StartFile,
    log_salt,
};
use crate::written;
use anyhow::{Context as _, Error, anyhow, bail, ensure};
use base32_id::Short;
use bucket_cell::cache::Cache;
use bucket_cell::cell::Kind;
use bucket_cell::keys::Ring;
use bucket_cell::recovery::{Places, Seal};
use bucket_cell::trace;
use bucket_cell::wal::{Rebased, Wal};
use extent_frame::segment::{CellId, Chain, Contents, Footer, Location, NewVersion, epoch_of};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};
use storage_device::{ExtentId, OtherCell, SealedExtent};

pub(crate) mod ephemeral;
pub use ephemeral::{EphemeralOpening, LocalCheckpoint, Parent};

/// The error of a write the cell has no room for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the extent store is full")]
pub struct NoSpace;

/// cladefs: the error of a version cut of a cell that is not quiescent: extents written since its
/// last commit, which a version holds not, as its guest committed them not (TASK-476).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a version of a cell holding extents written since its last commit")]
pub struct NotQuiescent;

/// The error of a capacity smaller than what the cell holds: its extents' bytes, and those that
/// writes in progress may add.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the extent store holds {held} bytes, more than {capacity}")]
pub struct TooSmall {
    pub capacity: u64,
    pub held: u64,
}

/// cladefs: the error of a write, or an opening, of an extent of the cell that the cell
/// sealed and freed (TASK-79.6): an extent's ID is its cell's to write once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} was written before: an extent's ID is written once")]
pub struct SpentExtent(pub ExtentId);

/// cladefs: the error of a read of an extent the cell holds whose data is in the bucket, at
/// `location` ([`LocalCell::adopt`]): what the cell's user reads it from.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{extent:?}'s data is in the bucket, at {location:?}")]
pub struct BucketData {
    pub extent: ExtentId,
    pub location: Location,
}

/// The error of I/O at an offset, or of a length, not a multiple of the block size, or empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unaligned I/O: {len} bytes at {offset}")]
pub struct Unaligned {
    pub offset: u64,
    pub len: u64,
}

/// The error of I/O whose end is past the most bytes an extent holds (`Options::max_extent`), or
/// past the largest offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{len} bytes at {offset}: past the most an extent holds")]
pub struct TooLong {
    pub offset: u64,
    pub len: u64,
}

/// cladefs: the error of a write of an extent of the last sequence, past which no cell's next
/// sequence is, nor a spent run's end (board 2762): refused, as no cell mints it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0:?}: the last sequence, which no extent is of")]
pub struct LastSequence(pub ExtentId);

/// How a cell lays its extents out.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// The most bytes an extent holds, doc-4's `max_extent`: no write or read ends past it. A
    /// sealed extent is read, framed and uploaded whole, so that this bounds what each takes of
    /// memory, whatever its writer asks (TASK-79.2).
    pub max_extent: u64,
    /// The size past which a segment takes no more extents: the next commit closes it, and starts
    /// another. cladefs: between commits it takes writes up to `max_extent` past it, then they go
    /// to another, so that no segment holds more than both together (TASK-79.2).
    pub segment_size: u64,
    /// The age past which a segment holding extents' data takes no more: the next commit closes it.
    pub segment_age: Duration,
    /// The size from which an extent is stored in a file of its own: one sealed at that length or
    /// more.
    pub large_extent: u64,
    /// How many segments' files the cell keeps open between reads. cladefs: and as many of its
    /// sealed extents' own files; and the most extents open in raw files of their own, each holding
    /// one open until its commit, past which an extent written large goes to segments
    /// (board 12555).
    pub open_files: usize,
    /// cladefs: the most bytes of runs a flush's commits seal for their records to hold them, and
    /// the flush to sync its logs alone (`wal::LogRecord`, TASK-301): what a guest's journal, or
    /// a database's transaction, commits at a time, not a bulk load's.
    pub held_most: u64,
}

impl Options {
    /// cladefs: the most bytes of data a segment holds: `segment_size`, and a write more, of
    /// `max_extent` at most, before writes go to another. A closed segment is read, framed and
    /// uploaded whole, so that this bounds what each takes of memory, however much is written
    /// between commits (TASK-79.2).
    pub fn segment_limit(&self) -> u64 {
        self.segment_size.saturating_add(self.max_extent)
    }
}

impl Default for Options {
    fn default() -> Self {
        Self {
            max_extent: 64 << 20,
            segment_size: 32 << 20,
            segment_age: Duration::from_secs(60),
            large_extent: 4 << 20,
            open_files: 64,
            held_most: 256 << 10,
        }
    }
}

/// cladefs: the most bytes of runs a flush's commits seal for their records to check them with
/// `Runs::Checked` (reading them back and CRCing them so raw-file sync and log append run at once,
/// TASK-301); above this, reading gigabytes back from disk takes seconds to save one sequential
/// `fsync`, so the flush takes the `Runs::Synced` path instead (board 11380).
const CHECKED_MOST: u64 = 8 << 20;

/// What a cell holds, as doc-4's `INFO` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    /// The most bytes the cell's extents may hold.
    pub capacity: u64,
    /// The bytes its extents hold: each as long as its furthest write.
    pub used: u64,
    /// The bytes it can still take.
    pub available: u64,
    /// One more than the highest sequence number of the cell's extents, freed or not
    /// since it opened; and, as it opened, of those it held. cladefs: those freed before, the
    /// writer's floor is past, each opening's writer minting past its own (doc-7, The head).
    pub next_sequence: u64,
    /// cladefs: of the bytes its extents hold, those of the extents its cell wrote, rather than
    /// inherited from the cells it was forked from, which may share them still (TASK-49).
    pub own: u64,
    /// cladefs: those of the extents it inherited from the cells it was forked from.
    pub inherited: u64,
    /// cladefs: how many extents it holds that its cell wrote (TASK-454): counted as they come
    /// and go, as the bytes are, not by a walk of them.
    pub own_extents: u64,
    /// cladefs: how many it holds that it inherited from the cells it was forked from.
    pub inherited_extents: u64,
}

/// The space a cell's files take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    /// The segments, and how many there are.
    pub segments: u64,
    pub segment_bytes: u64,
    /// The bytes of extents' data in segments.
    pub live_bytes: u64,
    /// The bytes of the extents stored on their own.
    pub own_bytes: u64,
    /// The bytes the cell's segments' files and the extents' own files take: closed segments and
    /// sealed extents compressed (`extent_frame`).
    pub disk_bytes: u64,
    /// cladefs: the bytes of extents freed whose data stays, for the uploader.
    pub kept_bytes: u64,
}

/// cladefs: a cell directory, taken and claimed for its opening, until it closes.
type Opened<D> = Option<(Claim<D>, Arc<TakenDir<D>>)>;

/// An extent store, open. See the crate's docs.
pub struct LocalCell<D: Dir> {
    /// Its directory, which its framings share.
    dir: Arc<D>,
    block_size: u32,
    cell: CellId,
    /// cladefs: its cell's kind, if its description names one (TASK-364, doc-20).
    kind: Option<Kind>,
    /// cladefs: the version of its cell it is at, if it is one (TASK-476).
    version: Option<u64>,
    /// cladefs: the versions its commits cut that its cell's head may not name yet, by the
    /// numbers of the commits cutting them: those of its write-ahead log, and those cut since it
    /// opened, until it learns their commits uploaded ([`LocalCell::local_versions`], TASK-476).
    cuts: Mutex<BTreeMap<u64, NewVersion>>,
    /// cladefs: the URL of the bucket its cell is kept in, if its description names one.
    bucket: Option<String>,
    /// Its directory, taken for this process ([`Dir::take`]), and claimed for its opening
    /// ([`Dir::claim`]): closing it lets go of both, but of the taking what holds it still
    /// ([`LocalCell::hold_dir`]).
    opened: SharedLock<Mutex<Opened<D>>>,
    options: Options,
    /// cladefs: shared with the framings of its segments, each of which ends itself in it, on a
    /// thread of its own (TASK-153.8).
    state: Arc<SharedLock<Mutex<State<D::File>>>>,
    /// Held by a commit.
    committing: AsyncMutex<()>,
    /// When its commits are durable: those ordered that no flush has taken yet, oldest first, for
    /// [`LocalCell::flush`] to make durable, and the last that is.
    durability: SharedLock<Mutex<Durability<Pending<D::File>>>>,
    /// Held by a flush: commits become durable in order. cladefs: and the logs flushes append
    /// records to, of the open segment, and of those closed since whose commits wait (TASK-165.9).
    flushing: AsyncMutex<Logs<D::File>>,
    /// cladefs: the start of its write-ahead log, written as segments are acknowledged, before
    /// their files leave (TASK-165.9).
    start: SharedLock<Mutex<StartFile>>,
    /// cladefs: the host's cache of the bucket's objects, if it has one: what the bucket
    /// acknowledged of the cell's goes into it (TASK-165.8).
    cache: Option<Arc<Cache>>,
    /// cladefs: what holds the parts of its segments packed as they fill, until their framings: the
    /// process's cells', or its own ([`Opening::held`], TASK-400).
    held: Arc<HeldParts>,
    /// Notified when the open segment has no write in progress, and when it closed: what
    /// [`LocalCell::close_now`] and the writes it holds up wait for; cladefs: and as each framing
    /// of a segment ends, which flushes, the uploader, forks and the cell's closing wait for.
    idle: Arc<Event>,
    /// Optional uploader wakeup notified whenever a closed segment finishes writing its framed
    /// file, so closed segments upload immediately without waiting for the open-segment interval.
    segment_written: Arc<std::sync::OnceLock<Arc<tokio::sync::Notify>>>,
    /// cladefs: its keys, which its segments and extents stored on their own are encrypted and
    /// read under (TASK-411.3).
    keys: Arc<Ring>,
    /// cladefs: of a replicated cell's writer, what sends each flush's commits to the members of
    /// its configuration (doc-21, TASK-537.3.3), once it started.
    replicator: std::sync::Mutex<Option<Arc<Replicator>>>,
    /// cladefs: of a fast WAL cell's writer, what stages each flush's commits for appendable zonal
    /// object storage with zero local disk fsyncs (doc-29).
    fast_wal: std::sync::Mutex<Option<Arc<crate::fast_wal::FastWalSink>>>,
    /// cladefs: true when local disk files are cache-only (fast WAL mode, doc-29) so `Framer`
    /// skips local disk `fsync`s/barriers in `segments::make_durable`.
    cache_only: Arc<std::sync::atomic::AtomicBool>,
    /// cladefs: logs of framed segments retained until bucket acknowledgement while replicating so
    /// [`CellLogSource`] can reconstruct evicted replica logs from existing cell files (board 6902).
    kept_logs: std::sync::Mutex<BTreeSet<SegmentId>>,
    /// cladefs: of an ephemeral cell, its local checkpoint as it stands, and what its next
    /// checkpoint covers (doc-25, TASK-698.2): held by a checkpoint, one at a time.
    ephemeral: Option<std::sync::Mutex<ephemeral::Ephemeral>>,
}

/// cladefs: read-only backing source for a replicator's evicted hot chunks (board 6902): streams
/// lagging or replacement peers directly from the cell's existing `logs/`, `open-segments/` or
/// `segments/`, and `extents/` without maintaining a separate `replica-spool/` copy on disk.
struct CellLogSource<D: Dir> {
    dir: Arc<D>,
    cell: CellId,
    keys: Arc<Ring>,
}

impl<D: Dir> crate::replicate::LogSource for CellLogSource<D> {
    fn read(&self, log: u64, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        let segment = SegmentId { cell: self.cell.id, number: log >> 1 };
        let log_file = match self.dir.open(&log_path(segment), false) {
            Ok(file) => file,
            Err(err) if not_found(&err) => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
        let bytes = read_file(&log_file, crate::MAX_LOG_BYTES)?;
        if bytes.len() < crate::wal::LOG_RECORDS as usize {
            return Ok(Vec::new());
        }
        let header = crate::wal::LogHeader::of(&bytes, segment)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad log header"))?;
        let encryption = header.encryption(segment, &self.keys).map_err(io::Error::other)?;
        let records =
            LogRecord::decode(&bytes, segment, Some(&encryption)).map_err(io::Error::other)?;
        let want_end = offset.saturating_add(len);
        let mut out = Vec::new();
        let mut pos = 0u64;
        let push = |out: &mut Vec<u8>, pos: &mut u64, chunk: &[u8]| {
            let chunk_end = pos.saturating_add(chunk.len() as u64);
            if chunk_end > offset && *pos < want_end {
                let from = offset.saturating_sub(*pos) as usize;
                let to = want_end.min(chunk_end).saturating_sub(*pos) as usize;
                out.extend_from_slice(&chunk[from..to]);
            }
            *pos = chunk_end;
        };
        if log & 1 == 1 {
            for record in records {
                for &(sequence, _) in &record.contents.own {
                    if pos >= want_end {
                        return Ok(out);
                    }
                    let path = extent_path(ExtentId { cell: self.cell.id, sequence });
                    let file = match self.dir.open(&path, true) {
                        Ok(file) => file,
                        Err(err) if not_found(&err) => return Ok(out),
                        Err(err) => return Err(err),
                    };
                    let size = file.size()?;
                    let frame_len = crate::replicate::OWN_FRAME_HEADER.saturating_add(size);
                    if pos.saturating_add(frame_len) <= offset {
                        pos = pos.saturating_add(frame_len);
                        continue;
                    }
                    let file_bytes = crate::framed::read_whole(&file)?;
                    let frame = own_frame(sequence, &file_bytes);
                    push(&mut out, &mut pos, &frame);
                }
            }
            return Ok(out);
        }
        let (header_bytes, mut replica) = ReplicaLog::new(
            &bytes[..crate::wal::LOG_RECORDS as usize],
            segment,
            encryption.clone(),
        )
        .map_err(io::Error::other)?;
        push(&mut out, &mut pos, &header_bytes);
        let mut raw: Option<D::File> = None;
        let mut framed: Option<crate::framed::Framed<D::File>> = None;
        for record in records {
            if pos >= want_end {
                break;
            }
            let rec_len = crate::wal::held_framed_len(&record.contents);
            if pos.saturating_add(rec_len) <= offset {
                pos = pos.saturating_add(rec_len);
                replica.advance(rec_len);
                continue;
            }
            let held = match record.runs {
                Runs::Held(runs) => LogRecord { contents: record.contents, runs: Runs::Held(runs) },
                Runs::Checked(_) | Runs::Synced => {
                    let mut runs =
                        Vec::with_capacity(LogRecord::run_bytes(&record.contents) as usize);
                    for &(_, run_offset, run_len) in record.contents.runs() {
                        if raw.is_none() && framed.is_none() {
                            match self.dir.open(&open_segment_path(segment), true) {
                                Ok(file) => raw = Some(file),
                                Err(err) if not_found(&err) => {
                                    let path = segment_path(segment);
                                    let file = self.dir.open(&path, true)?;
                                    let keys = self.keys.of(self.cell.id);
                                    framed = Some(
                                        crate::framed::Framed::open(
                                            file,
                                            &path,
                                            &keys,
                                            crate::framed::unpacked_for(self.dir.host()),
                                        )
                                        .map_err(io::Error::other)?,
                                    );
                                }
                                Err(err) => return Err(err),
                            }
                        }
                        if let Some(raw_file) = &raw {
                            let start = runs.len();
                            runs.resize(start + run_len as usize, 0);
                            raw_file.read_at(&mut runs[start..], run_offset)?;
                        } else if let Some(framed_file) = &framed {
                            let mut buf = framed_file
                                .parts
                                .read(run_offset..run_offset + run_len, |range| {
                                    crate::framed::read_aligned(
                                        &*framed_file.file,
                                        range.start,
                                        (range.end - range.start) as usize,
                                    )
                                    .map(|aligned| aligned.as_slice().to_vec())
                                    .map_err(|err| {
                                        extent_frame::Error::Corrupt(extent_frame::Corrupt(
                                            err.to_string(),
                                        ))
                                    })
                                })
                                .map_err(io::Error::other)?;
                            encryption.encrypt(run_offset, &mut buf);
                            runs.extend_from_slice(&buf);
                        } else {
                            unreachable!()
                        }
                    }
                    LogRecord {
                        contents: record.contents,
                        runs: Runs::Held(crate::wal::Bytes(runs)),
                    }
                }
            };
            let encoded = replica.append([&held]);
            debug_assert_eq!(encoded.len() as u64, rec_len);
            push(&mut out, &mut pos, &encoded);
        }
        Ok(out)
    }
}

/// A cell directory, taken for this process ([`Dir::take`]): let go of once dropped.
struct TakenDir<D: Dir>(Arc<D>);

impl<D: Dir> Drop for TakenDir<D> {
    fn drop(&mut self) {
        self.0.let_go();
    }
}

/// cladefs: a cell directory, claimed for its opening in this process ([`Dir::claim`]): let go of
/// once dropped.
struct Claim<D: Dir>(Arc<D>);

impl<D: Dir> Drop for Claim<D> {
    fn drop(&mut self) {
        self.0.unclaim();
    }
}

/// cladefs: a hold of a cell directory, taken for this process until it is dropped
/// ([`LocalCell::hold_dir`]).
pub struct DirHold<D: Dir> {
    _taken: Arc<TakenDir<D>>,
}

/// What a commit ordered but not yet durable leaves for the flush that makes it durable.
struct Pending<F> {
    /// Commit epoch of this pending commit.
    epoch: u64,
    /// The files whose data it refers to, and the directories whose entries changed, to sync.
    syncs: Syncs<F>,
    /// cladefs: its contents, the local segment whose log its record is to be appended to, and
    /// that segment's bucket segment number on the writer's chain (for fast WAL large-write
    /// pre-upload, doc-29).
    records: Vec<(SegmentId, u64, Contents)>,
    /// The files of the cell's alone it made unreferenced ([`Unreferenced::doomed`]): they go
    /// once it is durable.
    doomed: Vec<String>,
    /// cladefs: the files of the segments acknowledged in the bucket it let go of, with their keys
    /// there if they are the objects there: they leave for the host's cache with its flush
    /// (TASK-165.8).
    leaving: Vec<(String, Option<String>)>,
    /// cladefs: the segments opened whose raw files' and logs' entries its syncs make durable
    /// (`Segments::take_syncs`, TASK-369).
    entries: BTreeSet<SegmentId>,
}

/// cladefs: the logs a flush appends commits' records to, by their segments ([`LocalCell::flush`]).
/// cladefs: what flushes keep between them: the logs they append commits' records to, by their
/// segments, and the raw files a flush holding its commits' runs left unsynced, which the next
/// flush syncing data syncs too (TASK-301): a commit that seals nothing, as one setting the
/// capacity, takes a raw file's sync, which a later commit's data written to it needs.
struct Logs<F> {
    by_segment: BTreeMap<SegmentId, SegmentLog<F>>,
    unsynced: Syncs<F>,
    /// cladefs: of a replicated cell, each segment's replica log and its own log's end, as its
    /// members are sent them (TASK-537.3.3), from the segment's first records after the replicator
    /// started.
    replica: BTreeMap<SegmentId, (ReplicaLog, u64)>,
    /// cladefs: the segments whose logs held records as the replicator started, which no member
    /// holds from their headers: commits recorded in them after are never told replicated before
    /// the bucket holds them ([`Replicator::raise_base`]).
    unreplicated: BTreeSet<SegmentId>,
}

/// A write in progress (see [`LocalCell::begin_write`]): its data goes to its file, by
/// [`Write::write`], or by a caller's own I/O of what [`Write::stored`] gives. Dropping it before
/// it is finished abandons it. cladefs: no caller writes its file otherwise: an encrypted cell's
/// raw files are encrypted, a sector at a time, as they write them (TASK-411.5).
pub struct Write<'a, D: Dir> {
    cell: &'a LocalCell<D>,
    extent: ExtentId,
    /// The extent's opening it writes to ([`Uncommitted::open`]): one its free ends drops it.
    opening: u64,
    extent_offset: u64,
    len: u64,
    file: Arc<RawFile<D::File>>,
    offset: u64,
    /// The segment written to, if it isn't the extent's own file.
    segment: Option<SegmentId>,
    /// The bytes it reserved, which it may add to the cell.
    grows: u64,
    finished: bool,
    /// cladefs: what it holds of its cell's disk, admitted, until it lands (doc-22).
    _pledge: Option<crate::footprint::Pledge>,
}

impl<D: Dir> Write<'_, D> {
    /// Writes `data`, the write's bytes from `at` in it, to its file: encrypted, if the cell is,
    /// whole sectors then, as what the cell takes is (TASK-411.8). cladefs: refused past
    /// the write's end.
    pub fn write(&self, data: &[u8], at: u64) -> Result<(), Error> {
        let len = data.len() as u64;
        ensure!(at + len <= self.len, "{len} bytes at {at} of a write of {}", self.len);
        Ok(self.file.write_at(data, self.offset + at)?)
    }

    /// Records the write, whose data is in the file: reads see it, and the next commit includes
    /// it.
    pub fn finish(mut self) {
        self.finished = true;
        // cladefs: counted (TASK-310).
        written::wrote(written::Kind::Raw, self.len as usize);
        // cladefs: what was written to the open segment, its writing back begun once a MiB or more
        // waits, and waited for once more than WRITTEN_BEHIND_MOST is begun: holding no lock
        // (TASK-221). Failing, it is the flush's sync that fails.
        let Some(Behind { raw, begin: (offset, len), wait }) = self.cell.finish_write(&self) else {
            return;
        };
        if let Err(err) = raw.write_behind(offset, len) {
            log::warn!("extent store: writing back {len} bytes at {offset} of a segment: {err}");
        }
        if let Some((offset, len)) = wait
            && let Err(err) = raw.write_back(offset, len)
        {
            log::warn!("extent store: waiting for {len} bytes at {offset} of a segment: {err}");
        }
    }
}

impl<D: Dir> Drop for Write<'_, D> {
    fn drop(&mut self) {
        if !self.finished {
            self.cell.abandon_write(self);
        }
    }
}

/// Where `entry`'s data is.
fn place_of(entry: Entry<'_>) -> Place {
    let entry = match entry {
        Entry::Own(entry) => entry,
        Entry::Bucket(location) => return Place::Bucket(location.clone()),
    };
    match &entry.data {
        Data::Pieces(pieces) => match pieces[..] {
            [piece] if piece.extent_offset == 0 && piece.len == entry.len => Place::Run {
                cell: piece.segment.cell,
                number: piece.segment.number,
                offset: piece.offset,
            },
            _ => Place::Pieces,
        },
        Data::Own => Place::Own,
        Data::Bucket(location) => Place::Bucket(location.clone()),
    }
}

/// cladefs: ends the framing of segment `number`, `framed` saying how
/// ([`LocalCell::frame_behind`]), in `state`: the segment is read from its framed file from now on,
/// durable, its raw file going once the commits its footer names are durable; or its framing
/// failed, writing the framed file or making it durable, and the cell with it, as it is ahead of
/// its files.
fn framing_ended<F: File>(
    state: &mut State<F>,
    number: SegmentId,
    framed: Result<Arc<Framed<F>>, &Error>,
) {
    match framed {
        Ok(framed) => state.segments.framed(number, framed),
        Err(error) => {
            log::error!("extent store: framing segment {number:?}: {error:#}");
            let error = anyhow!("framing segment {number:?}: {error:#}");
            state.fail(&error);
            state.segments.framing_failed(number, format!("{error:#}"));
        }
    }
}

/// cladefs: a cell's writes held back ([`Gate`]) by a waiting commit, or closing between commits,
/// released as its waiting is dropped, should it be, as a cancelled future's is (board 2944).
struct HeldWrites<'a, D: Dir>(Option<&'a LocalCell<D>>);

impl<D: Dir> HeldWrites<'_, D> {
    /// Its waiting done, the hold its waiter's to release.
    fn kept(mut self) {
        self.0 = None;
    }
}

impl<D: Dir> Drop for HeldWrites<'_, D> {
    fn drop(&mut self) {
        if let Some(cell) = self.0 {
            cell.lock().gate.release();
            cell.idle.notify(usize::MAX);
        }
    }
}

/// cladefs: what a cell's segments are framed with, behind what closed them
/// ([`LocalCell::frame_behind`]): its directory, its state, which each framing ends itself in, and
/// what is notified as one does. A framer frames the segments closed, one after another, in the
/// order they closed, each written as it goes (`Unframed::frame`), and hands it to be made durable;
/// another thread makes those framed meanwhile durable together (`segments::make_durable`), and
/// ends their framings: so that no framing waits for another's syncs, which on a disk VMs share
/// took seconds each (TASK-310). Each on a thread of its own, started as there is work for it, and
/// gone once there is none.
struct Framer<D: Dir> {
    dir: Arc<D>,
    cell: u64,
    state: Arc<SharedLock<Mutex<State<D::File>>>>,
    idle: Arc<Event>,
    segment_written: Arc<std::sync::OnceLock<Arc<tokio::sync::Notify>>>,
    held: Arc<HeldParts>,
    cache_only: Arc<std::sync::atomic::AtomicBool>,
}

impl<D: Dir> Clone for Framer<D> {
    fn clone(&self) -> Self {
        let (dir, state, idle) = (self.dir.clone(), self.state.clone(), self.idle.clone());
        Self {
            dir,
            cell: self.cell,
            state,
            idle,
            segment_written: self.segment_written.clone(),
            held: self.held.clone(),
            cache_only: self.cache_only.clone(),
        }
    }
}

impl<D: Dir> Framer<D> {
    fn lock(&self) -> Locked<'_, D::File> {
        Locked::new(self.state.with(Mutex::lock))
    }

    /// The framer's thread: frames the segments queued, one after another, and looks ahead at the
    /// parts queued between, until none is.
    fn frame_queued(&self) {
        let mut state = self.lock();
        while let Some(job) = state.framing.queue.pop_front() {
            let unframed = match job {
                Job::See(number, part) => {
                    state = self.see(state, number, part);
                    continue;
                }
                Job::Frame(unframed) => unframed,
            };
            // cladefs: none once the cell failed, a segment before it among what failed, saying so
            // (TASK-165.9).
            let whole = state.standing.whole();
            let number = unframed.number();
            let ahead = state.segments.take_seen(number);
            drop(state);
            let framed = whole.and_then(|()| unframed.frame(&*self.dir, ahead));
            state = self.lock();
            if self.wrote(&mut state, number, framed) {
                // The host refused a thread to make it durable: on this one, as it would.
                drop(state);
                self.make_written_durable();
                state = self.lock();
            }
        }
        state.framing.running = false;
    }

    /// cladefs: looks ahead at part `part` of segment `number`, written whole, holding no lock, as
    /// its segment fills, as [`Framer::see_here`] does (TASK-400).
    fn see<'a>(
        &'a self,
        mut state: Locked<'a, D::File>,
        number: SegmentId,
        part: u64,
    ) -> Locked<'a, D::File> {
        let Some((raw, look, raw_only)) = Self::to_see(&state, number) else { return state };
        state.framing.looking = true;
        drop(state);
        fail::fail_point!("cell::looking_ahead");
        let seen = self.look(&*raw, part, look, raw_only);
        let mut state = self.lock();
        Self::saw(&mut state, (number, part), seen);
        state.framing.looking = false;
        self.idle.notify(usize::MAX);
        state
    }

    /// cladefs: looks ahead at part `part` of segment `number`, written whole, holding `state`: for
    /// its framing, which reads it no more if it frames the segment in place, nor if it holds it
    /// packed, which this has it do if the segment looks as if it will be framed in a copy; unless
    /// its framing took its parts' looks already, or the cell failed, or closed (TASK-400).
    fn see_here(&self, state: &mut State<D::File>, number: SegmentId, part: u64) {
        if let Some((raw, look, raw_only)) = Self::to_see(state, number) {
            let seen = self.look(&*raw, part, look, raw_only);
            Self::saw(state, (number, part), seen);
        }
    }

    /// cladefs: segment `number`'s raw file, if a look ahead at a part of it is for anything
    /// ([`Framer::see_here`]), the look at its parts seen so far, and whether the cell is
    /// ephemeral (raw-only look-ahead, board 11365).
    fn to_see(
        state: &State<D::File>,
        number: SegmentId,
    ) -> Option<(Arc<RawFile<D::File>>, Look, bool)> {
        let open = state.standing.whole().is_ok() && !state.standing.is_closed();
        open.then(|| state.segments.see(number)).flatten()
    }

    /// cladefs: a look ahead at part `part` of a segment, in its raw file `raw`, whose parts seen
    /// before it look as `look` says ([`framing::look_ahead`]): what it saw, and the part held, if
    /// it is to be packed: by the threads packing ahead, at the host's lowest priority, held
    /// pending until then ([`framing::pack_ahead`]); here and now, on a directory whose I/O is its
    /// callers' alone, as the cell's tests' are, and in the Shuttle tests, whose schedule no thread
    /// of the cell's runs outside of. cladefs: of an encrypted raw file too, its part seen as
    /// stored and packed, and held, in the clear, in memory, until its framing encrypts it
    /// (TASK-411.23, TASK-421). Ephemeral segments (`raw_only`) skip decryption and compression
    /// sampling (board 11365).
    fn look(
        &self,
        raw: &RawFile<D::File>,
        part: u64,
        look: Look,
        raw_only: bool,
    ) -> io::Result<(Seen, Option<Hold>)> {
        if raw_only {
            return Ok((framing::see_raw(raw, part)?, None));
        }
        if !self.dir.concurrent() || cfg!(feature = "shuttle") {
            let (seen, packed) = framing::see(raw, part, look)?;
            return Ok((seen, packed.map(|packed| self.held.hold(packed))));
        }
        let (seen, data) = framing::look_ahead(raw, part, look)?;
        let held = data.map(|data| {
            let (held, pending) = self.held.pend();
            framing::pack_ahead(part, data, pending);
            held
        });
        Ok((seen, held))
    }

    /// cladefs: a look ahead at part `part` of segment `number` saw `seen`, holding the part, or
    /// to, if it is to be packed, or failed: its framing reads the part itself then, which fails as
    /// it may.
    fn saw(
        state: &mut State<D::File>,
        (number, part): (SegmentId, u64),
        seen: io::Result<(Seen, Option<Hold>)>,
    ) {
        match seen {
            Ok((seen, held)) => state.segments.saw(number, part, seen, held),
            Err(err) => log::warn!("extent store: looking at part {part} of {number:?}: {err}"),
        }
    }

    /// Segment `number`'s framing wrote its framed file, `framed`, in `state`: to be made durable
    /// with those framed meanwhile, on a thread that this starts if none runs. Or its writing
    /// failed, which ends its framing. Returns whether the host refused the thread, which the
    /// caller is to be then.
    fn wrote(
        &self,
        state: &mut State<D::File>,
        number: SegmentId,
        framed: Result<Arc<Framed<D::File>>, Error>,
    ) -> bool {
        let framed = match framed {
            Ok(framed) => framed,
            Err(error) => {
                framing_ended(state, number, Err(&error));
                self.idle.notify(usize::MAX);
                return false;
            }
        };
        state.segments.written(number, framed.clone());
        state.framing.wrote(number, framed);
        // cladefs: the uploader, which takes it now (TASK-384).
        self.idle.notify(usize::MAX);
        if let Some(wake) = self.segment_written.get() {
            wake.notify_one();
        }
        if std::mem::replace(&mut state.framing.syncing, true) {
            return false;
        }
        let syncer = self.clone();
        spawn_or_return((), move |()| syncer.make_written_durable()).is_some()
    }

    /// The thread making the segments framed durable: those written since it last did, together,
    /// until none is left, each of whose framings it ends.
    fn make_written_durable(&self) {
        let mut state = self.lock();
        while !state.framing.written.is_empty() {
            let (written, since) = state.framing.take_written();
            let entries =
                state.segments.unsynced_entries(written.iter().map(|(number, _)| *number));
            let logged: Vec<bool> =
                written.iter().map(|(number, _)| state.segments.has_durable_log(*number)).collect();
            let whole = state.standing.whole();
            drop(state);
            let durable =
                whole.and_then(|()| self.make_durable(&written, &entries, &logged, since));
            state = self.lock();
            Self::made_durable(&mut state, written, &entries, &durable);
            self.idle.notify(usize::MAX);
        }
        state.framing.syncing = false;
        self.idle.notify(usize::MAX);
    }

    /// On the caller's thread, holding `state`, as before TASK-41, if the directory's I/O is its
    /// callers' alone, or the host refuses a thread to frame on: frames the segments queued, then
    /// makes them durable together, unless a thread does already, which takes them. No framer runs
    /// meanwhile.
    fn frame_here(&self, state: &mut State<D::File>) {
        while let Some(job) = state.framing.queue.pop_front() {
            let unframed = match job {
                Job::See(number, part) => {
                    self.see_here(state, number, part);
                    continue;
                }
                Job::Frame(unframed) => unframed,
            };
            let number = unframed.number();
            let ahead = state.segments.take_seen(number);
            match state.standing.whole().and_then(|()| unframed.frame(&*self.dir, ahead)) {
                Ok(framed) => {
                    state.segments.written(number, framed.clone());
                    state.framing.wrote(number, framed);
                    if let Some(wake) = self.segment_written.get() {
                        wake.notify_one();
                    }
                }
                Err(error) => framing_ended(state, number, Err(&error)),
            }
        }
        state.framing.running = false;
        if !state.framing.syncing && !state.framing.written.is_empty() {
            let (written, since) = state.framing.take_written();
            let entries =
                state.segments.unsynced_entries(written.iter().map(|(number, _)| *number));
            let logged: Vec<bool> =
                written.iter().map(|(number, _)| state.segments.has_durable_log(*number)).collect();
            let whole = state.standing.whole();
            let durable =
                whole.and_then(|()| self.make_durable(&written, &entries, &logged, since));
            Self::made_durable(state, written, &entries, &durable);
        }
        self.idle.notify(usize::MAX);
    }

    /// Makes `written`, segments framed, the first of them written at `since`, durable together;
    /// and the entries of `entries`, segments opened, too.
    fn make_durable(
        &self,
        written: &[(SegmentId, Arc<Framed<D::File>>)],
        entries: &BTreeSet<SegmentId>,
        logged: &[bool],
        since: Instant,
    ) -> Result<(), Error> {
        let entries = !entries.is_empty();
        let cache_only = self.cache_only.load(std::sync::atomic::Ordering::Acquire);
        segments::make_durable(&*self.dir, self.cell, (written, entries, logged), since, cache_only)
    }

    /// `written`, segments framed, were made durable together, and the entries of `entries`, or
    /// not, as `durable` says: their framings end.
    fn made_durable(
        state: &mut State<D::File>,
        written: Vec<(SegmentId, Arc<Framed<D::File>>)>,
        entries: &BTreeSet<SegmentId>,
        durable: &Result<(), Error>,
    ) {
        if durable.is_ok() {
            state.segments.entries_synced(entries);
        }
        for (number, framed) in written {
            framing_ended(state, number, durable.as_ref().map(|()| framed));
        }
    }
}

/// cladefs: the file a cell directory holds until its maker made it whole, which opening it refuses
/// ([`Unmade`]): written before its description, removed once whole ([`LocalCell::made`]), as an
/// import fills it, so that a maker dying part way leaves no cell that opens (TASK-196).
pub(crate) const UNMADE: &str = "unmade";

/// cladefs: the error of opening a cell whose maker never made it whole ([`UNMADE`], TASK-196).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the cell was never made whole: its maker stopped part way")]
pub struct Unmade;

/// cladefs: what the bucket holds of the cell it opens (doc-14, Opening a cell; TASK-165.9): its
/// state, as recovery found it once its writer took its head, what the writer found taking it
/// ([`bucket_cell::TakenCell`]), the head, which the cell's write-ahead log is put after, and its
/// writer's chain there, which stamps the cell's footers from then on; or, for a version, its state
/// alone, which nothing writes. And the host's cache of the bucket's objects, if it has one, which
/// what the bucket acknowledged of the cell's goes into. cladefs: and the cell's keys, its cell's
/// and its ancestors' DEKs, which its segments and extents stored on their own are encrypted and
/// read under (`bucket_cell::keys`, TASK-411.3): a writer's, which encrypts, but a version's. And
/// what holds the parts of its segments packed as they fill, which the process's cells share, so
/// that what governs the process's memory lets them go as it runs short (`crate::held`, TASK-400);
/// without, one of the cell's own, which nothing makes let go. An encrypted cell holds none
/// (TASK-411.5).
pub struct Opening {
    pub state: bucket_cell::State,
    /// cladefs: the decoded state `state` started from, if it did, whose map of the extents it
    /// shares ([`bucket_cell::Recovered::layer`]): held by the cell while it is open, so that
    /// the cells of its lineage opened after it find it decoded and share it too (TASK-453).
    pub layer: Option<Arc<bucket_cell::State>>,
    pub taken: Option<bucket_cell::TakenCell>,
    pub cache: Option<Arc<Cache>>,
    pub keys: Arc<Ring>,
    pub held: Option<Arc<HeldParts>>,
}

/// cladefs: the DEK cells kept in no bucket are encrypted under ([`Opening::new`], TASK-411.28):
/// the tests' (`bucket_cell::testing::dek`, of which a test's reader holds the ring), known to all,
/// so that tests, benches and simulations make and reopen them in any process. A host's cells are
/// all kept in a bucket, under DEKs their records keep; none is so.
#[cfg(any(test, feature = "unkept"))]
pub const UNKEPT_DEK: [u8; 32] = [0x7e; 32];

impl Opening {
    /// The opening of `cell`, which the bucket holds nothing of: no commit of it there, and a
    /// writer's chain from its first segment, of epoch 0. Its cell's write-ahead log
    /// follows only if it never had anything acknowledged ([`bucket_cell::Error::Diverged`]):
    /// a cell of the tests' own, of a bucket it never reaches. cladefs: encrypted under
    /// [`UNKEPT_DEK`]: every cell is encrypted (TASK-411.28), and one kept nowhere has no record to
    /// keep a DEK of its own; tests', benches' and simulations'.
    #[cfg(any(test, feature = "unkept"))]
    pub fn new(cell: CellId) -> Self {
        let chain = Chain::new(cell, 0, 0, 0);
        let state = bucket_cell::State::new(cell);
        let taken = bucket_cell::TakenCell::of(Default::default(), &state, chain);
        let keys = Ring::writing_under(cell, extent_frame::Secret::from_bytes(&UNKEPT_DEK));
        Self { state, layer: None, taken: Some(taken), cache: None, keys, held: None }
    }
}

/// cladefs: a cell directory, taken for this process and claimed for its opening, its write-ahead
/// log recovered from it ([`LocalCell::recover`]), to be opened as what the bucket holds of its
/// cell says ([`Recovering::open`]): its cell's writer takes the head meanwhile, and recovers it.
pub struct Recovering<D: Dir> {
    dir: Arc<D>,
    taken: Arc<TakenDir<D>>,
    claim: Claim<D>,
    file: CellFile,
    wal: Recovered,
}

impl<D: Dir> Recovering<D> {
    /// The cell.
    pub fn cell(&self) -> CellId {
        self.file.cell
    }

    /// Its directory's host context, if backed by a host directory.
    pub(crate) fn host(&self) -> Option<&Arc<crate::Host>> {
        self.dir.host()
    }

    /// cladefs: the last commit its write-ahead log holds, if it holds any.
    pub fn last_commit_held(&self) -> Option<u64> {
        let segments = self.wal.segments.iter().map(|(footer, _)| &footer.contents);
        let closing = self.wal.closing.values().flatten();
        segments.chain(closing).filter_map(|contents| contents.commits.last().copied()).last()
    }

    /// cladefs: whether it recovered a write-ahead log: segments not yet in its cell's bucket.
    pub fn has_wal(&self) -> bool {
        !self.wal.segments.is_empty() || !self.wal.closing.is_empty()
    }

    /// cladefs: the cell recovered again once what members of its replicated cell's
    /// configuration hold, `logs`, is written as its write-ahead log after `last`, the last commit
    /// its cell's head names ([`crate::replicate::restore`], doc-21, TASK-537.3.4): of a cell
    /// holding none.
    pub fn restored(self, logs: &[(u64, Vec<u8>)], last: Option<u64>) -> Result<Self, Error> {
        ensure!(!self.has_wal(), "restoring members' logs into a cell holding a write-ahead log");
        crate::replicate::restore(&*self.dir, self.file.cell, logs, last)?;
        let Recovering { dir, taken, claim, file, .. } = self;
        let wal = crate::wal::recover(&*dir, file.cell, None)?;
        Ok(Recovering { dir, taken, claim, file, wal })
    }

    /// cladefs: ensures the cell opens its next segment past every segment number present in
    /// `logs`, so a takeover never opens a segment whose log number a seed it spools holds
    /// (board 5398).
    pub fn advance_next_past(&mut self, logs: &[(u64, Vec<u8>)]) {
        if let Some(past) = logs.iter().map(|&(log, _)| (log >> 1).saturating_add(1)).max() {
            self.wal.next = self.wal.next.max(past);
        }
    }

    /// The URL of the bucket its cell is kept in, if its description names one.
    pub fn bucket(&self) -> Option<&str> {
        self.file.bucket.as_deref()
    }

    /// The fencing token of its placement, if it was placed (TASK-39.7).
    pub fn token(&self) -> Option<u64> {
        self.file.token
    }

    /// cladefs: whether its placement's takings of its cell are forced (`PlaceCell`'s `force`,
    /// TASK-681): its placement was placed forced, and is the cell's placement still.
    pub fn forced(&self) -> bool {
        self.file.forced.is_some() && self.file.forced == self.file.token
    }

    /// cladefs: of its forced placement, the epoch of the last commit the bucket held as a
    /// taking of it fenced fewer members than a take needs, if one did
    /// ([`Recovering::note_forced_after`] noted it).
    pub fn forced_after(&self) -> Option<u64> {
        self.file.forced_after.filter(|_| self.forced())
    }

    /// cladefs: notes, durably, in its description, that its taking, of the placement `placement`,
    /// forced, fenced fewer members than a take needs, the bucket holding its commits through epoch
    /// `kept` (TASK-681): what its status says from now on, until another placement holds it. The
    /// placement noted too, as a cell made by a forced take is placed after (`CreateCell`).
    pub fn note_forced_after(&mut self, placement: u64, kept: u64) -> Result<(), Error> {
        self.file.token = Some(placement);
        self.file.forced = Some(placement);
        self.file.forced_after = Some(kept);
        self.dir.replace(DESCRIPTION, self.file.text().as_bytes())?;
        self.dir.sync_dir("")?;
        Ok(())
    }

    /// cladefs: whether it is its writer's copy of what a replicated cell's configuration made at
    /// `epoch` holds (TASK-683): its description names `epoch` among its copies
    /// ([`Recovering::note_writer_copies`]), so that its write-ahead log holds what that writer
    /// synced and the bucket doesn't, all of it, or, empty, nothing the bucket doesn't, the
    /// writer's end counting toward Replicated only once synced (doc-21). Not from its log's
    /// holding records alone: a log dropped and then restored only in part, or a later writer's,
    /// holds records too. A cell made anew names none.
    pub fn holds_own_copy(&self, epoch: u64) -> bool {
        self.file.writer_copies.contains(&epoch)
    }

    /// cladefs: notes, durably, in its description, that it is its writer's copy of the
    /// configurations made at `epochs` (TASK-683), keeping those it was of at `named` or after,
    /// the epoch of the configuration its cell's head names as it is taken, whose copy, and its
    /// later ones', a take may look for still: before the configurations of the epoch it takes
    /// its cell as are named or replicate anything.
    pub fn note_writer_copies(&mut self, epochs: &[u64], named: Option<u64>) -> Result<(), Error> {
        let mut copies: Vec<u64> = (self.file.writer_copies.iter().copied())
            .filter(|&epoch| named.is_some_and(|named| epoch >= named))
            .chain(epochs.iter().copied())
            .collect();
        copies.sort_unstable();
        copies.dedup();
        if copies == self.file.writer_copies {
            return Ok(());
        }
        self.file.writer_copies = copies;
        self.dir.replace(DESCRIPTION, self.file.text().as_bytes())?;
        self.dir.sync_dir("")?;
        Ok(())
    }

    /// cladefs: its cell's kind, if its description names one (TASK-364, doc-20).
    pub fn kind(&self) -> Option<Kind> {
        self.file.kind
    }

    /// cladefs: the version of its cell it is at, if its description names one: its
    /// cell's state at it, which nothing writes (doc-18, TASK-476).
    pub fn version(&self) -> Option<u64> {
        self.file.version
    }

    /// The cell's commit its write-ahead log goes on from: the last the bucket acknowledged of
    /// its commits, if it acknowledged any.
    pub fn start(&self) -> Option<u64> {
        self.wal.start
    }

    /// cladefs: whether `commit`, a cell's last, is its write-ahead log's start or one of its
    /// commits: a cell whose last commit is never moved on from the log, which then diverged
    /// not ([`bucket_cell::Error::Diverged`], TASK-345).
    pub fn holds_commit(&self, commit: Option<u64>) -> bool {
        let wal = self.wal();
        let mut commits = wal.segments().flat_map(|(footer, _)| &footer.contents.commits);
        commit == wal.start() || commit.is_some_and(|commit| commits.any(|&own| own == commit))
    }

    /// cladefs: whether its write-ahead log holds `commit`, a member's last (TASK-678's review): a
    /// commit of `commit`'s writer at or past its place, or its start. A writer makes one sequence,
    /// so a log holding a commit of its past `commit`'s place holds `commit` too. Its segments'
    /// footers name only some of their commits, each segment's last: a log holding `commit` may be
    /// found not to, which costs a take its seed, never its safety.
    pub fn holds_commits(&self, commit: Option<u64>) -> bool {
        use extent_frame::segment::{epoch_of, writer_of};
        let Some(commit) = commit else { return true };
        let segments = self.wal.segments.iter().map(|(footer, _)| &footer.contents);
        let closing = self.wal.closing.values().flatten();
        let mut commits = segments.chain(closing).flat_map(|contents| &contents.commits);
        let of =
            |own: u64| writer_of(own) == writer_of(commit) && epoch_of(own) >= epoch_of(commit);
        self.wal.start.is_some_and(of) || commits.any(|&own| of(own))
    }

    /// The write-ahead log, as its directory holds it: stamped as it was.
    fn wal(&self) -> Wal<u64> {
        Wal::of(self.wal.start, self.wal.segments.iter().cloned())
    }

    /// Whether its write-ahead log holds nothing.
    pub fn is_empty(&self) -> bool {
        self.wal.segments.is_empty()
    }

    /// Whether its write-ahead log follows the cell whose end a writer found as `taken` says:
    /// unless the cell moved on from it ([`bucket_cell::Error::Diverged`]). What a cell asks
    /// of the cell as recovery found it before taking its head, so that a cell plainly stale
    /// fences no writer.
    pub fn follows(&self, taken: &bucket_cell::TakenCell) -> bool {
        self.wal().rebase(taken).is_ok()
    }

    /// Drops its write-ahead log, the commits it holds never acknowledged lost: what `cladefsctl
    /// take` does of a cell whose log diverged (doc-14, Opening a cell). Its start goes with it:
    /// an empty log follows any cell. Durably, before the cell opens past it: a crash after
    /// brings none of it back, whose commits the cell moved on from, to upload after the cell's
    /// own since (TASK-295).
    pub fn drop_wal(&mut self) -> Result<(), Error> {
        // cladefs: its writer's copies, which it holds no more, said so first, durably: a crash
        // while the log goes, or while members' logs are restored into it, must not leave a log
        // that counts as one (TASK-683's review).
        if !self.file.writer_copies.is_empty() {
            self.file.writer_copies.clear();
            self.dir.replace(DESCRIPTION, self.file.text().as_bytes())?;
            self.dir.sync_dir("")?;
        }
        let dir = &*self.dir;
        let cell = self.file.cell;
        // cladefs: those closed again, not framed, their raw files and logs going (TASK-411.9).
        self.wal.drop_closing(dir, cell)?;
        for (footer, number) in std::mem::take(&mut self.wal.segments) {
            let segment = SegmentId { cell: cell.id, number };
            self.remove_files([segment_path(segment)], &footer);
        }
        for kind in [SEGMENTS, EXTENTS] {
            dir.sync_dir(&cell_dir(kind, cell.id))?;
        }
        self.wal.start_file.write(dir, None)?;
        dir.barrier()?;
        self.wal.start = None;
        Ok(())
    }

    /// Removes the files at `paths`, and those of the extents stored on their own `footer` names.
    fn remove_files(&self, paths: impl IntoIterator<Item = String>, footer: &Footer) {
        let own = footer.own().map(|(extent, _)| extent_path(extent));
        for path in paths.into_iter().chain(own) {
            if let Err(err) = self.dir.remove(&path)
                && !not_found(&err)
            {
                log::warn!("extent store: failed to remove {path}: {err}");
            }
        }
    }

    /// Opens the cell as `opening` says the bucket holds its cell (doc-14, Opening a cell):
    /// its write-ahead log put after the cell's end, by its commits, those of its segments that
    /// landed acknowledged, their files into the host's cache, and the rest stamped anew on the
    /// writer's chain ([`Wal::rebase`]), its start written first; or refused, if the cell moved
    /// on from it ([`bucket_cell::Error::Diverged`]). The cell's state is the bucket's, its
    /// extents there, with the log's segments' footers applied over it, their extents here; and
    /// another segment opens, past them. A version's cell holds its state alone, and no log.
    pub fn open(self, options: Options, opening: Opening) -> Result<LocalCell<D>, Error> {
        ensure!(!self.file.ephemeral, "an ephemeral cell opens as one (open_ephemeral)");
        let Self { dir, taken, claim, file, wal: mut recovered } = self;
        let CellFile { block_size, cell, bucket, capacity, kind, version, .. } = file;
        let Opening { state, layer, taken: found, cache, keys, held } = opening;
        ensure!(state.cell == cell, "cell {cell} opened as {}", state.cell);
        // cladefs: the segments its recovery closed again framed, first, under its keys, as nothing
        // before them is (TASK-411.9).
        recovered.close(&*dir, cell, &keys, false)?;
        // cladefs: what the bucket acknowledged goes into the host's cache by a rename, which can't
        // cross filesystems: a cell elsewhere than any of the cache's JBOD disks is refused, not
        // left to read every segment back from the bucket (TASK-359).
        if let (Some(cache), Some(here)) = (&cache, dir.disk().and_then(|disk| disk.dev())) {
            ensure!(
                cache.has_disk(here),
                "the cell is on another filesystem than the host's cache at {}: the cell's \
                 segments go into the cache by renaming them",
                cache.dirs_display()
            );
        }
        // Stamped as the directory holds them: those the rebase stamps anew upload so.
        let stamped: BTreeMap<u64, Footer> =
            recovered.segments.iter().map(|(footer, number)| (*number, footer.clone())).collect();
        let mut wal = Wal::of(recovered.start, recovered.segments);
        let mut start_file = recovered.start_file;
        let (Rebased { landed }, chain) = match &found {
            Some(found) => {
                let rebased = wal.rebase(found)?;
                start_file.write(&*dir, wal.start())?;
                // Its footers from now on follow those stamped anew.
                let mut chain = found.chain;
                for (footer, _) in wal.segments() {
                    chain = chain.admit(footer).expect("a log rebased follows the writer's chain");
                }
                (rebased, chain)
            }
            None => {
                ensure!(wal.is_empty(), "the cell at version {cell} holds commits of its own");
                (Rebased { landed: Vec::new() }, Chain::new(cell, 0, 0, 0))
            }
        };
        // What the head names is the bucket's: into the host's cache, as an acknowledgement's
        // files go.
        let mut leaving = Vec::new();
        for (footer, number) in landed {
            let segment = SegmentId { cell: cell.id, number };
            let key = bucket_cell::layout::segment(cell, footer.segment, footer.epoch);
            leaving.push((segment_path(segment), Some(key)));
            leaving.extend(footer.own().map(|(extent, at)| (extent_path(extent), Some(at.key))));
        }
        leave(&*dir, cache.as_deref(), leaving);

        // The cell: the bucket's state, the log's footers applied over it. cladefs: the bucket's
        // map of its extents its layer, shared, not copied, the log's its own over it (TASK-453).
        let mut replay = state.placed(|extents| Local {
            extents: Extents::over(extents, layer, cell),
            segments: HashMap::new(),
        });
        let mut named_own = BTreeSet::new();
        for (footer, number) in wal.segments() {
            let key = bucket_cell::layout::segment(cell, footer.segment, footer.epoch);
            replay.extents.segments.insert(key, SegmentId { cell: cell.id, number: *number });
            replay.apply_contents(footer.segment, footer.epoch, &footer.contents);
            named_own.extend(footer.own().map(|(extent, _)| extent));
        }
        // cladefs: a log ends with no commit unended (`wal::recover`, TASK-92.41).
        ensure!(replay.begun.is_none(), "cell {cell}'s log ends inside a commit");
        let head = wal.head().or(replay.last_commit);
        let epoch = head.map_or(0, epoch_of);
        let restate = replay.capacity.is_none() && found.is_some();
        let capacity = replay.capacity.unwrap_or(capacity);
        let bucket_cell::State { extents: Local { mut extents, .. }, root, spent, lengths, .. } =
            replay;
        // cladefs: the lengths of its segments' data the bucket's state knows: what their density
        // is measured against, with no request (doc-27).
        for (key, len) in &lengths {
            extents.sized(key, *len);
        }
        // Past every extent of its own the cell holds: fxfs looks for what a crash left below
        // it (`Allocator::open`). Not the footers' next sequence, which their writers stamped
        // past their own floors (`Chain::close`), the next floor being the uploader's to say.
        let own = extents.lens().filter(|(extent, _)| extent.cell == cell.id);
        // cladefs: the bucket's state is authenticated, and its extents a cell's, of no last
        // sequence ([`LastSequence`]): saturating all the same, as no arithmetic on what was read
        // wraps (board 2762).
        let next_sequence =
            own.map(|(extent, _)| extent.sequence.saturating_add(1)).max().unwrap_or(0);
        let root = root.unwrap_or_default();
        remove_unheld(&*dir, cell.id, &extents, &named_own)?;

        // The segments it holds extents' data in, with the bytes of it each holds, and its own
        // extents' files. They must be there.
        let mut tracked: BTreeMap<SegmentId, (u64, bool)> = BTreeMap::new();
        for (_, number) in wal.segments() {
            tracked.insert(SegmentId { cell: cell.id, number: *number }, (0, false));
        }
        let holding = Holding::of(cell.id, extents.lens());
        let mut own_sizes = HashMap::new();
        // What of their data is here is in its own layer: the bucket's layer's is in the bucket.
        for (extent, entry) in extents.own() {
            match &entry.data {
                Data::Pieces(pieces) => {
                    for piece in pieces {
                        tracked.entry(piece.segment).or_insert((0, false)).0 += piece.len;
                    }
                }
                Data::Own => {
                    let path = extent_path(*extent);
                    let file = dir.open(&path, true);
                    let file = file.with_context(|| format!("extent {extent:?}'s file"))?;
                    own_sizes.insert(*extent, file.size()?);
                }
                Data::Bucket(_) => {}
            }
        }
        let opened = (epoch, recovered.next, chain);
        let files = (options.open_files, keys.clone());
        let mut segments = Segments::open(&*dir, (cell.id, tracked), opened, files, false)?;
        // cladefs: the sealed extents in each, read in the bucket once it is acknowledged there
        // (TASK-165.8).
        for (extent, entry) in extents.own() {
            segments.place(*extent, &entry.data);
        }
        let mut queued = Vec::new();
        for (footer, number) in wal.segments() {
            let restamped = stamped.get(number) != Some(footer);
            let own: u64 = footer.contents.own.iter().map(|(_, len)| len).sum();
            let bytes = segments.len(SegmentId { cell: cell.id, number: *number }) + own;
            queued.push(Queued::new(*number, footer.clone(), restamped, bytes));
        }
        // cladefs: the versions its log's segments cut, which its cell's head names once their
        // segments are acknowledged (TASK-476).
        let cuts: BTreeMap<u64, NewVersion> = (wal.segments())
            .filter_map(|(footer, _)| {
                let last = footer.contents.commits.last()?;
                Some(footer.contents.versions.iter().map(move |cut| (epoch_of(*last), *cut)))
            })
            .flatten()
            .collect();
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
            own_sizes,
            queue: Queue::new(wal.start(), queued),
            version: found.is_none(),
            cuts,
            start_file,
            cache,
            held,
            keys,
            ephemeral: None,
        };
        Ok(LocalCell::assemble(
            (dir, taken, claim),
            (block_size, cell, bucket, kind, version),
            assembled,
        ))
    }
}

/// cladefs: what an opening found of a cell, from which it is put together
/// ([`LocalCell::assemble`]): a cell kept in the bucket's ([`Recovering::open`]), or an ephemeral
/// cell's (`Recovering::open_ephemeral`, TASK-698.2).
struct Assembled<D: Dir> {
    options: Options,
    extents: Extents,
    capacity: u64,
    /// Whether the cell said no capacity, its cell's then, which its next commit sets in it.
    restate: bool,
    holding: Holding,
    epoch: u64,
    root: Vec<u8>,
    next_sequence: u64,
    spent: extent_frame::spent::Spent,
    segments: Segments<D::File>,
    own_sizes: HashMap<ExtentId, u64>,
    queue: Queue,
    version: bool,
    cuts: BTreeMap<u64, NewVersion>,
    start_file: StartFile,
    cache: Option<Arc<Cache>>,
    held: Option<Arc<HeldParts>>,
    keys: Arc<Ring>,
    ephemeral: Option<ephemeral::Ephemeral>,
}

impl<D: Dir> LocalCell<D> {
    /// cladefs: the cell `assembled` says an opening found, in `dir`, taken and claimed, of `cell`
    /// described so (TASK-698.2's refactor of [`Recovering::open`]).
    fn assemble(
        (dir, taken, claim): (Arc<D>, Arc<TakenDir<D>>, Claim<D>),
        (block_size, cell, bucket, kind, version): (
            u32,
            CellId,
            Option<String>,
            Option<Kind>,
            Option<u64>,
        ),
        assembled: Assembled<D>,
    ) -> Self {
        let Assembled {
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
            own_sizes,
            queue,
            version: at_version,
            cuts,
            start_file,
            cache,
            held,
            keys,
            ephemeral,
        } = assembled;
        let mut uncommitted = Uncommitted::default();
        // cladefs: a cell saying no capacity has the cell's, which its next commit sets in it
        // (TASK-165.3).
        if restate {
            uncommitted.resize(capacity);
        }
        #[cfg(any(test, feature = "audit"))]
        let replay = {
            let lengths = extents.lens().collect();
            let mut replay = crate::state::Replay::of(cell, crate::state::Lengths(lengths));
            (replay.root, replay.spent) = (Some(root.clone()), spent.clone());
            replay.capacity = (!restate).then_some(capacity);
            replay
        };
        LocalCell {
            dir,
            block_size,
            cell,
            bucket,
            kind,
            version,
            cuts: Mutex::new(cuts),
            opened: SharedLock::new("store opening", Mutex::new(Some((claim, taken)))),
            options,
            state: Arc::new(SharedLock::new(
                "store state",
                Mutex::new(State {
                    extents,
                    space: Space::new(capacity, holding),
                    epoch,
                    root,
                    next_sequence,
                    spent,
                    uncommitted,
                    gate: Gate::default(),
                    segments,
                    own_files: OwnFiles::new(
                        cell.id,
                        own_sizes,
                        (options.open_files, keys.clone()),
                    ),
                    unreferenced: Unreferenced::default(),
                    wal: queue,
                    version: at_version,
                    kept: KeptExtents::default(),
                    standing: Standing::Open,
                    deferred: Vec::new(),
                    framing: Framing::default(),
                    #[cfg(any(test, feature = "audit"))]
                    replay,
                }),
            )),
            committing: AsyncMutex::new("store commit", ()),
            durability: SharedLock::new("store durability", Mutex::new(Durability::new(epoch))),
            flushing: AsyncMutex::new(
                "store flush",
                Logs {
                    by_segment: BTreeMap::new(),
                    unsynced: Syncs::default(),
                    replica: BTreeMap::new(),
                    unreplicated: BTreeSet::new(),
                },
            ),
            start: SharedLock::new("store log start", Mutex::new(start_file)),
            cache,
            held: held.unwrap_or_default(),
            idle: Arc::new(Event::new()),
            segment_written: Arc::new(std::sync::OnceLock::new()),
            keys,
            replicator: std::sync::Mutex::new(None),
            fast_wal: std::sync::Mutex::new(None),
            cache_only: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            kept_logs: std::sync::Mutex::new(BTreeSet::new()),
            ephemeral: ephemeral.map(std::sync::Mutex::new),
        }
    }
}

/// cladefs: a cell's extents as a replay of the footers of its write-ahead log places them over
/// the bucket's ([`Recovering::open`]): sealed in one run of a segment of the log, which
/// `segments` holds by its key in the bucket, or in files of their own. By the run's key, not the
/// segment applied: a commit spanning segments seals its runs in those before (TASK-92.41).
struct Local {
    extents: Extents,
    segments: HashMap<String, SegmentId>,
}

impl Places for Local {
    fn seal(&mut self, extent: ExtentId, seal: Seal) {
        let entry = match seal {
            Seal::Run(Location { key, offset, len }) => {
                let segment = self.segments[&key];
                let piece = Piece { segment, offset, extent_offset: 0, len };
                Extent { len, data: Data::Pieces(vec![piece]) }
            }
            Seal::Own(location) => Extent { len: location.len, data: Data::Own },
        };
        self.extents.insert(extent, entry);
    }

    fn free(&mut self, extent: ExtentId) {
        self.extents.remove(&extent);
    }
}

/// cladefs: what a relocation did ([`LocalCell::relocate`]): the bytes it relocated, and the
/// commit that names them where they went, if it relocated any: one that changes nothing the
/// guest reads, its state the commit's before it but where they are (doc-27).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Relocated {
    pub bytes: u64,
    pub commit: Option<u64>,
}

/// cladefs: a relocation that failed after it relocated what it says ([`LocalCell::relocate`]),
/// committed: the context of its error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PartlyRelocated(pub Relocated);

impl std::fmt::Display for PartlyRelocated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "relocating, {} bytes relocated", self.0.bytes)
    }
}

/// cladefs: an extent of a cell's own segments in the bucket, read by its writer to relocate it
/// ([`LocalCell::relocate`], doc-27): where it was read, and its bytes, as the guest reads them.
#[derive(Clone, Debug)]
pub struct Relocating {
    pub extent: ExtentId,
    pub from: Location,
    pub data: Vec<u8>,
}

/// cladefs: the footer of `extent`'s own file, the object it is in the bucket: it names the extent,
/// so that one copied to another's key is refused (`bucket_cell::layout::own_extent`,
/// TASK-411.3).
fn own_footer(extent: ExtentId) -> Vec<u8> {
    bucket_cell::layout::own_extent_footer(extent)
}

/// cladefs: the files at the paths of `leaving` leave a cell directory `dir`, the objects in the
/// bucket at their keys, if given: into the host's cache, `cache`, if it takes them, the file
/// renamed; or removed (TASK-165.8). What fails to leave is logged, and removed, the bucket
/// holding it.
fn leave<D: Dir>(dir: &D, cache: Option<&Cache>, leaving: Vec<(String, Option<String>)>) {
    for (path, key) in leaving {
        let adopted = match (cache, key) {
            (Some(cache), Some(key)) => {
                dir.give(&path, &mut |from| cache.adopt(&key, from)).unwrap_or_else(|err| {
                    log::warn!("extent store: {path} into the cache, as {key}: {err}");
                    false
                })
            }
            _ => false,
        };
        if !adopted
            && let Err(err) = dir.remove(&path)
            && !not_found(&err)
        {
            log::warn!("extent store: failed to remove {path}: {err}");
        }
    }
}

impl<D: Dir> LocalCell<D> {
    /// cladefs: makes a new cell directory in `dir`, which holds no cell, as `description`
    /// describes, its description naming `bucket`, the URL of the bucket its cell is kept in, if
    /// given ([`LocalCell::bucket`]), and `kind`, its cell's kind, if given ([`LocalCell::kind`],
    /// TASK-364); unmade until its maker says it is whole ([`LocalCell::made`]). The description is
    /// made last: until it is durable, the directory is no cell either. Its cell is the bucket's,
    /// which opening it takes ([`LocalCell::recover`]).
    pub fn make(
        dir: &D,
        description: Description,
        bucket: Option<&str>,
        kind: Option<Kind>,
    ) -> Result<(), Error> {
        Self::make_copy(dir, description, bucket, kind, None)
    }

    /// cladefs: makes a new cell's directory in `dir`, as [`LocalCell::make`] does, a copy of the
    /// version `version` of its cell, if given: its cell's state at the version, which nothing
    /// writes, as a cell at a version holds the version's (doc-18, TASK-476).
    pub fn make_copy(
        dir: &D,
        description: Description,
        bucket: Option<&str>,
        kind: Option<Kind>,
        version: Option<u64>,
    ) -> Result<(), Error> {
        Self::make_with(dir, description, bucket, kind, version, |_| Ok(()))
    }

    /// cladefs: makes a new cell's directory in `dir` as [`LocalCell::make_copy`] does, `extra`
    /// writing what else it holds into its making first, before its description, made durable
    /// with it ([`Making`], board 7420).
    pub fn make_with(
        dir: &D,
        description: Description,
        bucket: Option<&str>,
        kind: Option<Kind>,
        version: Option<u64>,
        extra: impl FnOnce(&mut Making<'_, D>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        Self::make_described(dir, description, bucket, kind, (version, false), extra)
    }

    /// cladefs: makes a new cell's directory in `dir`, as [`LocalCell::make_copy`] does, its
    /// description saying `version` and whether its cell is ephemeral, as `described` says;
    /// `extra` writes what else it holds first, before its description (TASK-698.2), into its
    /// making, which makes it durable with the description at once ([`Making`], board 7420).
    fn make_described(
        dir: &D,
        description: Description,
        bucket: Option<&str>,
        kind: Option<Kind>,
        (version, ephemeral): (Option<u64>, bool),
        extra: impl FnOnce(&mut Making<'_, D>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let Description { block_size, capacity, cell } = description;
        ensure!(
            block_size.is_power_of_two() && block_size as usize <= crate::framed::ALIGNMENT,
            "bad block size {block_size}"
        );
        ensure!(capacity >= block_size as u64, "the cell is smaller than a block");
        // cladefs: what a cell made takes, within its disk's allowance and its tenant's limits
        // (doc-22, TASK-539.8).
        let _pledge = dir.footprint().map(|footprint| footprint.admit_made()).transpose()?;
        dir.take().context("taking the cell directory")?;
        let made = || -> Result<(), Error> {
            create_cell_dirs(dir, cell.id)?;
            dir.create(UNMADE, false)?.sync()?;
            // Unmade from here on, durably: what follows is synced at once, its description too.
            let mut making = Making::new(dir);
            extra(&mut making)?;
            let file = dir.create(DESCRIPTION, false)?;
            let bucket = bucket.map(str::to_owned);
            let described = CellFile {
                block_size,
                cell,
                bucket,
                capacity,
                token: None,
                forced: None,
                forced_after: None,
                writer_copies: Vec::new(),
                kind,
                version,
                ephemeral,
            };
            let text = described.text();
            file.write_at(text.as_bytes(), 0)?;
            making.file(DESCRIPTION, file);
            making.dir_entries("");
            making.sync()?;
            dir.barrier()?;
            Ok(())
        };
        let made = made();
        dir.let_go();
        made
    }

    /// cladefs: the cell is whole, as its maker made it: the file saying it is not goes, durably
    /// ([`UNMADE`], TASK-196). Its next openings open it.
    pub fn made(&self) -> Result<(), Error> {
        match self.dir.remove(UNMADE) {
            Err(err) if !not_found(&err) => return Err(err.into()),
            _ => {}
        }
        self.dir.sync_dir("")?;
        Ok(self.dir.barrier()?)
    }

    /// cladefs: makes a new cell in `dir` as [`LocalCell::make`] does, whole, kept nowhere, and
    /// opens it: one whose cell the bucket holds nothing of ([`Opening::new`]), as the cell's own
    /// tests' are.
    #[cfg(any(test, feature = "unkept"))]
    pub fn create(dir: D, description: Description, options: Options) -> Result<Self, Error> {
        Self::make(&dir, description, None, None)?;
        let cell = Self::recover_unmade(dir)?.open(options, Opening::new(description.cell))?;
        cell.made()?;
        Ok(cell)
    }

    /// cladefs: opens the cell in `dir`, whose cell the bucket holds nothing of
    /// ([`Opening::new`]), as the cell's own tests' is: refused if its write-ahead log had
    /// anything acknowledged.
    #[cfg(any(test, feature = "unkept"))]
    pub fn open(dir: D, options: Options) -> Result<Self, Error> {
        let recovering = Self::recover(dir)?;
        let cell = recovering.cell();
        recovering.open(options, Opening::new(cell))
    }

    /// Takes the cell in `dir` for this process ([`Dir::take`]), and claims it for an opening
    /// ([`Dir::claim`]), and recovers its write-ahead log ([`crate::wal`]), to be opened as the
    /// bucket holds its cell ([`Recovering::open`]). Fails while another process has it open, or
    /// this one; and with [`Unmade`] if its maker never made it whole (TASK-196).
    pub fn recover(dir: D) -> Result<Recovering<D>, Error> {
        match dir.open(UNMADE, false) {
            // cladefs: unmade on the disk, but whole here, its durability deferred (board 7420).
            Ok(_) if dir.made_deferred() => {}
            Ok(_) => return Err(Unmade.into()),
            Err(err) if not_found(&err) => {}
            Err(err) => return Err(err.into()),
        }
        Self::recover_unmade(dir)
    }

    /// Recovers the cell in `dir` as [`LocalCell::recover`] does, unmade or not: its maker's
    /// Reads only the cell's description file without claiming the directory or parsing its local
    /// WAL, returning its `(CellId, bucket_url)` (doc-29, Hotbed review 118).
    pub fn peek_bucket(dir: &D) -> Result<(CellId, Option<String>), Error> {
        let file = dir.open(DESCRIPTION, false).context("opening the cell's description")?;
        let text = String::from_utf8(read_file(&file, crate::MAX_DESCRIPTION_BYTES)?)
            .context("reading the description")?;
        let parsed = CellFile::parse(&text)?;
        Ok((parsed.cell, parsed.bucket))
    }

    /// Takes and claims `dir`, reads its description, and drops all local WAL files without
    /// parsing them (doc-29, Hotbed review 118): used only after verifying the cold bucket head
    /// has an active Fast WAL so a damaged local cache never blocks remote recovery.
    pub fn recover_dropping_wal(dir: D) -> Result<Recovering<D>, Error> {
        dir.take().context("taking the cell directory")?;
        let dir = Arc::new(dir);
        let taken = Arc::new(TakenDir(dir.clone()));
        dir.claim().context("taking the cell directory")?;
        let claim = Claim(dir.clone());
        let file = {
            let file = dir.open(DESCRIPTION, false).context("opening the cell's description")?;
            let text = String::from_utf8(read_file(&file, crate::MAX_DESCRIPTION_BYTES)?)
                .context("reading the description")?;
            CellFile::parse(&text)?
        };
        ensure!(!file.ephemeral, "ephemeral cells do not use fast WAL remote recovery");
        let wal = crate::wal::drop_all(&*dir, file.cell)?;
        Ok(Recovering { dir, taken, claim, file, wal })
    }

    /// opening.
    pub fn recover_unmade(dir: D) -> Result<Recovering<D>, Error> {
        dir.take().context("taking the cell directory")?;
        let dir = Arc::new(dir);
        // Dropped, as an opening that fails drops them, they let the directory go.
        let taken = Arc::new(TakenDir(dir.clone()));
        dir.claim().context("taking the cell directory")?;
        let claim = Claim(dir.clone());
        let file = {
            let file = dir.open(DESCRIPTION, false).context("opening the cell's description")?;
            let text = String::from_utf8(read_file(&file, crate::MAX_DESCRIPTION_BYTES)?)
                .context("reading the description")?;
            CellFile::parse(&text)?
        };
        // cladefs: an ephemeral cell's checkpoint cut short finished first (TASK-698.2, board 6905).
        let ephemeral =
            if file.ephemeral { Some(ephemeral::settle(&*dir, file.cell)?) } else { None };
        let wal = crate::wal::recover(&*dir, file.cell, ephemeral)?;
        Ok(Recovering { dir, taken, claim, file, wal })
    }

    /// cladefs: the sequence numbers of the cell's own extents it freed once sealed, which it
    /// writes no more ([`SpentExtent`], TASK-79.6).
    pub fn spent(&self) -> Result<extent_frame::spent::Spent, Error> {
        Ok(self.state()?.spent.clone())
    }

    /// Where `extent`'s data is, if the cell holds it.
    pub fn place(&self, extent: ExtentId) -> Result<Option<Place>, Error> {
        Ok(self.state()?.extents.get(&extent).map(place_of))
    }

    /// The extents the cell holds, with their lengths and where they are, in order.
    pub fn places(&self) -> Result<Vec<(ExtentId, u64, Place)>, Error> {
        let state = self.state()?;
        let mut extents: Vec<_> = state
            .extents
            .iter()
            .map(|(extent, entry)| (extent, entry.len(), place_of(entry)))
            .collect();
        extents.sort_by_key(|(extent, ..)| *extent);
        Ok(extents)
    }

    /// cladefs: the URL of the bucket its cell is kept in, as its description names it: every
    /// cell in a host's directory names one ([`crate::create_host`]); none, one of memory's
    /// (TASK-165.6).
    pub fn bucket(&self) -> Option<&str> {
        self.bucket.as_deref()
    }

    /// cladefs: its cell's kind, as its description names it (TASK-364, doc-20).
    pub fn kind(&self) -> Option<Kind> {
        self.kind
    }

    /// cladefs: its keys, its cell's and its ancestors' DEKs, which it encrypts and reads under
    /// (TASK-411.3): every cell's encrypted (TASK-411.28).
    pub fn keys(&self) -> &Arc<Ring> {
        &self.keys
    }

    /// cladefs: holds the cell directory taken for this process until what it returns is dropped,
    /// as the cell open does: for what outlives the cell's closing and still writes in the
    /// directory, as its uploader does as it stops (TASK-129). Fails once it is closed.
    pub fn hold_dir(&self) -> Result<DirHold<D>, Error> {
        let opened = self.opened.with(Mutex::lock);
        let (_, taken) = opened.as_ref().ok_or_else(|| anyhow!("cell {} is closed", self.cell))?;
        Ok(DirHold { _taken: taken.clone() })
    }

    /// cladefs: commits, durably, a commit whose segment's footer names the version `cut` cut at
    /// its end, a name for the cell's state as of the commit, which the acknowledgement naming the
    /// segment names in the cell's head (doc-18, TASK-476). Its segment closes at the commit. If
    /// `quiescent`, the cell must be, no extent written since its last commit
    /// ([`LocalCell::has_open`]), so that the version holds all its guest wrote, as a VM snapshot's
    /// must: refused otherwise ([`NotQuiescent`]). A fork's need not: it holds what the guest
    /// committed. Returns the commit's number: the version is local until the bucket acknowledges
    /// it ([`LocalCell::local_versions`]).
    pub async fn commit_version(&self, cut: NewVersion, quiescent: bool) -> Result<u64, Error> {
        let committing = self.committing.lock().await;
        if quiescent && self.has_open()? {
            bail!(NotQuiescent);
        }
        self.writable()?.segments.close_for_version(cut);
        self.commit_locked(&committing, None).await?;
        let epoch = self.lock().epoch;
        self.cuts.lock().unwrap().insert(epoch, cut);
        failpoint!("version::committed");
        self.flush().await?;
        Ok(epoch)
    }

    /// cladefs: the versions its commits cut whose commits are later than `uploaded`, the number of
    /// the last commit the bucket acknowledged, each by its commit's number: local, its cell's
    /// head naming them not yet (TASK-476). Those uploaded it forgets.
    pub fn local_versions(&self, uploaded: u64) -> Vec<(u64, NewVersion)> {
        let mut cuts = self.cuts.lock().unwrap();
        cuts.retain(|&commit, _| commit > uploaded);
        cuts.iter().map(|(&commit, &cut)| (commit, cut)).collect()
    }

    /// cladefs: forgets the local versions `ids` names: their commits stay, and the bucket's head
    /// names them as they are acknowledged, which their dropping then drops (TASK-476).
    pub fn forget_versions(&self, ids: &BTreeSet<u64>) {
        self.cuts.lock().unwrap().retain(|_, cut| !ids.contains(&cut.id));
    }

    /// cladefs: the version of its cell it is at, if it is one (TASK-476).
    pub fn version(&self) -> Option<u64> {
        self.version
    }

    /// The cell directory.
    pub fn dir(&self) -> &D {
        &self.dir
    }

    /// cladefs: what holds the parts of its segments packed as they fill ([`Opening::held`],
    /// TASK-400).
    pub fn held_parts(&self) -> &Arc<HeldParts> {
        &self.held
    }

    /// The cell, as its user sees it.
    pub fn description(&self) -> Description {
        Description {
            block_size: self.block_size,
            capacity: self.lock().space.capacity(),
            cell: self.cell,
        }
    }

    /// The state, locked, failed or not: audited as it is let go, in the builds of the tests
    /// ([`Locked`], TASK-160.16).
    fn lock(&self) -> Locked<'_, D::File> {
        Locked::new(self.state.with(Mutex::lock))
    }

    /// The state, to read: unless the cell failed ([`Readable`], TASK-160.19).
    fn state(&self) -> Result<Readable<'_, D::File>, Error> {
        Readable::check(self.lock(), self.cell)
    }

    /// The state, to change: unless the cell failed, or the cell is closed, or a version
    /// ([`Writable`]), checked again by its waits, which let it go meanwhile (TASK-83).
    fn writable(&self) -> Result<Writable<'_, D::File>, Error> {
        Writable::check(self.lock(), self.cell)
    }

    /// cladefs: the state, locked, once `ready` says so of it: waited for holding no thread, as
    /// what it waits for, done on another thread or task, says it changed (`idle`, board 2944).
    /// It listens before it looks, so that a change after its look wakes it.
    async fn lock_when(&self, ready: impl Fn(&State<D::File>) -> bool) -> Locked<'_, D::File> {
        loop {
            let changed = self.idle.listen();
            // In a scope of its own, which ends before the await: a guard dropped there, not
            // merely dropped, is held across no await, which the future's sending needs.
            {
                let state = self.lock();
                if ready(&state) {
                    return state;
                }
            }
            changed.await;
        }
    }

    /// cladefs: the state, locked, once `busy` says nothing of it, new writes held back meanwhile
    /// ([`Gate`]) by its caller, which held them, its own state let go: as a commit, or a closing
    /// between commits, waits for the writes in progress, waiting as [`LocalCell::lock_when`] does.
    /// Its waiting dropped, as a future cancelled is, the hold goes with it. Its caller then lets
    /// the writes go on. No guard given it, so that its future is the caller's to send.
    async fn lock_holding_writes(
        &self,
        busy: impl Fn(&State<D::File>) -> bool,
    ) -> Locked<'_, D::File> {
        let held = HeldWrites(Some(self));
        let state = self.lock_when(|state| !busy(state)).await;
        held.kept();
        state
    }

    /// cladefs: says the cell is closing, as [`LocalCell::close`] does first, or what closes it
    /// before it, its uploader's last uploads (TASK-644): an opening of the cell in this process
    /// meanwhile waits for it, rather than fail as at a second opening (TASK-648), as does what
    /// waits for its close ([`crate::closed`]).
    pub fn closing(&self) {
        if let Some((claim, _)) = self.opened.with(Mutex::lock).as_ref() {
            claim.0.closing();
        }
    }

    /// Closes the cell, once the commits only ordered are durable, those a flush in flight is
    /// making durable among them: another opening may follow, and this one reads what it holds, as
    /// caches of its extents may still, and changes nothing.
    pub async fn close(&self) -> Result<(), Error> {
        self.closing();
        fail::fail_point!("cell::closing");
        // After a flush in flight: its commits are durable, and the files they made unreferenced
        // gone, before the cell may be opened again.
        let mut flushing = match rules().close_waits_for_flushes {
            true => Some(self.flushing.lock().await),
            false => None,
        };
        let flushed = match &mut flushing {
            Some(_) if !self.flush_pending() => Ok(()),
            Some(flushing) => {
                crate::upload::blocking(|| self.flush_locked(flushing, &mut Flush::default()))
            }
            // With the rule off, as if the commits a flush in flight took were durable.
            None if !self.durability.with(Mutex::lock).queued() => Ok(()),
            None => self.flush().await,
        };
        drop(flushing);
        // cladefs: its durability deferred, and no flush to make it durable, made durable as it
        // closes, whether or not its flush failed, as a fork's making would have: whole on the
        // disk for its next opening (board 7420).
        let settled = crate::upload::blocking(|| self.dir.settle());
        let flushed = match (flushed, settled) {
            (Ok(()), settled) => {
                settled.context("making the cell durable as it closes, its durability deferred")
            }
            (Err(error), Err(settled)) => {
                log::error!(
                    "extent store: making cell {} durable as it closes, its flush failed: {settled}",
                    self.cell
                );
                Err(error)
            }
            (flushed, Ok(())) => flushed,
        };
        // cladefs: an ephemeral cell's closed segments covered, all of them, its local checkpoint
        // what its next opening starts from (TASK-698.2).
        let flushed = match flushed {
            Ok(()) if self.ephemeral.is_some() && !self.lock().standing.is_closed() => {
                self.checkpoint(true).await.map(drop)
            }
            flushed => flushed,
        };
        if !self.lock().standing.is_closed() {
            // cladefs: once no segment is being framed, whatever closed it, a commit no flush took
            // or one that failed among them, which another opening of the cell would frame
            // again meanwhile from its raw file (TASK-41.1, TASK-79.99): each framing ends itself,
            // and the segments say which are running (TASK-153.8). The opening keeps their framed
            // files, if whole, or frames their raw files anew.
            if rules().close_waits_for_framings {
                drop(self.lock_when(|state| state.segments.framings().is_empty()).await);
            }
            self.lock().standing.close();
            // cladefs: and for a look ahead reading its part, holding no lock (TASK-400), which
            // read on after close returned, maybe once the raw files below were gone, while a cell
            // opened on the directory meanwhile counted its I/O as that cell's own (TASK-426).
            // Closed, the cell begins none of those queued ([`Framer::to_see`]). Their raw files,
            // of commits durable, go as a flush's would.
            let raw = {
                let mut state = self.lock_when(|state| !state.framing.looking).await;
                match state.standing.failed() {
                    false => state.segments.raw(self.durability.with(Mutex::lock).local()),
                    true => Vec::new(),
                }
            };
            crate::upload::blocking(|| self.remove_raw(raw));
            // cladefs: clear open reserve while still holding the directory claim so a closed
            // LocalCell kept alive by a reader never wipes a reopened store's reserve on drop
            // (board 5442).
            let mut opened = self.opened.with(Mutex::lock);
            if opened.is_some() {
                if let Some(footprint) = self.dir.footprint() {
                    footprint.set_open(0);
                }
                drop(opened.take());
            }
        }
        flushed
    }

    /// Fails unless I/O of `len` bytes at `offset` in an extent is aligned to blocks, and ends
    /// within the most an extent holds.
    pub fn check_io(&self, offset: u64, len: u64) -> Result<(), Error> {
        let block_size = self.block_size as u64;
        if !offset.is_multiple_of(block_size) || !len.is_multiple_of(block_size) || len == 0 {
            bail!(Unaligned { offset, len });
        }
        if offset.checked_add(len).is_none_or(|end| end > self.options.max_extent) {
            bail!(TooLong { offset, len });
        }
        Ok(())
    }

    /// A segment's file, opened if it isn't: the open segment's raw, a closed one's framed.
    fn segment_file(
        &self,
        state: &mut State<D::File>,
        number: SegmentId,
    ) -> Result<Source<D::File>, Error> {
        state.segments.source(&*self.dir, number)
    }

    /// The file `extent`'s data is in, stored on its own: raw while it is open, framed once sealed.
    fn own_source(
        &self,
        state: &mut State<D::File>,
        extent: ExtentId,
    ) -> Result<Source<D::File>, Error> {
        match state.uncommitted.packing(extent) {
            Some(Packing::Own(file)) => Ok(Source::Raw(file.clone())),
            _ => Ok(Source::Framed(state.own_files.file(&*self.dir, extent)?)),
        }
    }

    /// Whether a commit, or a closing between commits, waits for the writes in progress, holding
    /// new ones back ([`LocalCell::begin_write`]).
    pub fn holding_writes(&self) -> bool {
        self.lock().gate.is_held()
    }

    /// Begins writing `len` bytes at `offset` in `extent`, which must be the cell's and
    /// open: not yet written, or written since the last commit. The caller writes the data to the
    /// returned write's file, and finishes it.
    pub async fn begin_write(
        &self,
        extent: ExtentId,
        offset: u64,
        len: u64,
    ) -> Result<Write<'_, D>, Error> {
        // Not while a commit, or a closing between commits, waits for the writes in progress.
        loop {
            if let Some(write) =
                crate::upload::blocking(|| self.try_begin_write(extent, offset, len))?
            {
                return Ok(write);
            }
            self.wait_until_writes_resume().await;
        }
    }

    /// cladefs: begins a write as [`LocalCell::begin_write`] does, or, while a commit, or a closing
    /// between commits, holds writes back, returns `None` at once: what `begin_write` awaits the
    /// end of ([`LocalCell::wait_until_writes_resume`]), holding no thread, as the writes in
    /// progress that the commit waits for may need the thread to end (board 820, 2944).
    pub fn try_begin_write(
        &self,
        extent: ExtentId,
        offset: u64,
        len: u64,
    ) -> Result<Option<Write<'_, D>>, Error> {
        self.check_io(offset, len)?;
        if extent.cell != self.cell.id {
            bail!(OtherCell(extent));
        }
        if extent.sequence == u64::MAX {
            bail!(LastSequence(extent));
        }
        let state = self.writable()?;
        if state.gate.is_held() {
            return Ok(None);
        }
        self.begin_write_locked(state, extent, offset, len).map(Some)
    }

    /// cladefs: awaits, holding no thread, the time no commit, nor closing between commits, holds
    /// writes back ([`LocalCell::try_begin_write`]).
    pub async fn wait_until_writes_resume(&self) {
        drop(self.lock_when(|state| !(state.gate.is_held())).await);
    }

    /// cladefs: fails unless `extent`, the cell's, whose life in `state` is `life`, may be opened,
    /// as a write opens it: unless it is open, or never sealed by the cell, held or freed
    /// ([`SpentExtent`], TASK-79.6). One kept, freed, is spent if it was the cell's own.
    fn check_opens(state: &State<D::File>, life: &Life<'_>, extent: ExtentId) -> Result<(), Error> {
        match life {
            Life::Open(_) | Life::Unknown => Ok(()),
            Life::Held(_) => bail!(SealedExtent(extent)),
            Life::Spent => bail!(SpentExtent(extent)),
            Life::Kept(_) if state.spent.contains(extent.sequence) => bail!(SpentExtent(extent)),
            Life::Kept(_) => Ok(()),
        }
    }

    /// Begins a write, as [`LocalCell::begin_write`] says, with `state`, writable, held, and no
    /// commit holding writes back.
    fn begin_write_locked(
        &self,
        mut state: Writable<'_, D::File>,
        extent: ExtentId,
        offset: u64,
        len: u64,
    ) -> Result<Write<'_, D>, Error> {
        let end = offset + len;
        let (grows, to_segment, overwrites) = {
            let life = state.life(extent);
            Self::check_opens(&state, &life, extent)?;
            let current = life.held();
            let grows = end.saturating_sub(current.map_or(0, Entry::len));
            let to_segment = match state.uncommitted.packing(extent) {
                Some(packing) => matches!(packing, Packing::Segment),
                None => !self.opens_own(&state.uncommitted, end),
            };
            // cladefs: what it writes over, in a segment, takes room too (TASK-79.2), as the writes
            // that finished left the extent: not its holes (TASK-160.16).
            let overwrites = match current.and_then(Entry::own).map(|entry| &entry.data) {
                Some(Data::Pieces(pieces)) if to_segment => covered(pieces, offset, end),
                _ => 0,
            };
            (grows, to_segment, overwrites)
        };
        // cladefs: whole sectors, which the open segment takes, and its seal, copying it whole from
        // its start, appends there (TASK-411.8): refused before the extent opens. cladefs: and
        // which large extents' raw files take, encrypted a sector at a time (TASK-411.5).
        Unsectored::check(offset, len)?;
        state.check_room(grows, overwrites)?;
        // cladefs: within its disk's allowance and its tenant's limits, with room left for what the
        // cell's own work adds for it, which nothing refuses (doc-22, TASK-539.8).
        let pledge =
            self.dir.footprint().map(|footprint| footprint.admit(len, grows)).transpose()?;
        if to_segment {
            self.make_room(&mut state, len)?;
        }
        let (packing, opening) = match state.uncommitted.opening(extent) {
            Some(opened) => opened,
            None => {
                let packing = if to_segment {
                    Packing::Segment
                } else {
                    // Raw until it is sealed, when it is framed in its own file.
                    Packing::Own(Arc::new(self.create_raw(extent)?))
                };
                let opening = state.uncommitted.open(extent, packing.clone());
                (packing, opening)
            }
        };
        let (file, file_offset, segment) = match packing {
            Packing::Own(file) => (file, offset, None),
            Packing::Segment => {
                let (number, file, at) = state.segments.reserve(len)?;
                (file, at, Some(number))
            }
        };
        state.space.reserve(grows);
        state.gate.pass();
        Ok(Write {
            cell: self,
            extent,
            opening,
            extent_offset: offset,
            len,
            file,
            offset: file_offset,
            segment,
            grows,
            finished: false,
            _pledge: pledge,
        })
    }

    /// Whether an extent opening, by a write ending at `end`, goes to a raw file of its own: if it
    /// reaches `large_extent`. cladefs: and fewer than `open_files` of those open, `uncommitted`'s,
    /// are, each holding a file open until its commit (board 12555, hotbed question 559): past
    /// them, it goes to segments, as one written there and grown large does, which its commit
    /// copies into its own file. Before, a VM process writing a block at the end of as many extents
    /// as it liked, never committing, held a file open for each, unbounded by the cell's options,
    /// and its daemon's limit of open files ran out under every other cell (`EMFILE`).
    fn opens_own(&self, uncommitted: &Uncommitted<D::File>, end: u64) -> bool {
        end >= self.options.large_extent && uncommitted.own() < self.options.open_files
    }

    /// The raw file of `extent`, not open, made for its writes until it is sealed. cladefs: one
    /// left at its path holds nothing the extent does, as no open extent's writes go to it: a
    /// free's, of the extent while it was open, which the next commit's flush removes. It makes
    /// way, so that the extent may be written anew before that flush (TASK-79.5). cladefs:
    /// encrypted under its cell's newest DEK, from a salt of its own kept in memory alone, as the
    /// file never outlives the process: an opening removes those left (TASK-411.5).
    fn create_raw(&self, extent: ExtentId) -> Result<RawFile<D::File>, Error> {
        let (dir, path) = (&*self.dir, open_extent_path(extent));
        let created = match dir.create(&path, true) {
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                dir.remove(&path).and_then(|()| dir.create(&path, true))
            }
            created => created,
        };
        let (dek, key) = self.keys.try_encrypting()?;
        let encryption = Arc::new(RawEncryption::new(&dek, key));
        Ok(RawFile::new(created.with_context(|| format!("creating {path}"))?, encryption))
    }

    /// cladefs: makes room in the open segment for a write of `len` bytes, if it has none: writes
    /// go to the next (TASK-79.2). The open segment, full, takes no more, and waits for the next
    /// commit, which seals in place what is one run in it and closes it (`Segments::fill`,
    /// TASK-92.41): holding the state alone, so that the write waits neither for a commit in
    /// progress, nor for the writes in the segment, which go on, nor for a framing. Before, it was
    /// closed then, holding commits, or set aside, and the next commit copied out of it every
    /// extent written since.
    fn make_room(&self, state: &mut State<D::File>, len: u64) -> Result<(), Error> {
        if state.segments.fits(len, self.options.segment_limit()) {
            return Ok(());
        }
        state.segments.fill(&*self.dir, state.epoch)?;
        Ok(())
    }

    /// cladefs: what frames the cell's segments off its commits ([`Framer`]).
    fn framer(&self) -> Framer<D> {
        let (dir, state, idle) = (self.dir.clone(), self.state.clone(), self.idle.clone());
        Framer {
            dir,
            cell: self.cell.id,
            state,
            idle,
            segment_written: self.segment_written.clone(),
            held: self.held.clone(),
            cache_only: self.cache_only.clone(),
        }
    }

    /// Registers an uploader wakeup notified whenever a closed segment finishes writing its
    /// framed file.
    pub(crate) fn wake_on_segment_written(&self, wake: Arc<tokio::sync::Notify>) {
        // Registered once when the cell's uploader opens it.
        let _ = self.segment_written.set(wake);
    }

    /// Records `write`, done; returns what of the open segment's raw file to write back, if
    /// anything ([`Segments::write_behind`]).
    fn finish_write(&self, write: &Write<'_, D>) -> Option<Behind<D::File>> {
        let mut state = self.lock();
        let state = &mut *state;
        // A write to an extent freed since it began is dropped, as though it finished before the
        // free: the extent, if written again since, is another opening's, whose data may go
        // elsewhere, and which its bytes are no part of (TASK-79.1). A commit waits for the writes
        // in progress before it seals what they write to: a free, not a seal, ended the opening.
        if rules().drop_writes_to_freed_extents
            && !state.uncommitted.is_open_in(write.extent, write.opening)
        {
            self.drop_write(state, write);
            return None;
        }
        state.space.unreserve(write.grows);
        let extent = write.extent;
        let end = write.extent_offset + write.len;
        // A write's extent is its cell's.
        let entry = state.extents.written(extent, || {
            state.space.entered(true);
            Extent {
                len: 0,
                data: if write.segment.is_some() { Data::Pieces(Vec::new()) } else { Data::Own },
            }
        });
        let grew = end.saturating_sub(entry.len);
        if grew > 0 {
            state.space.grew(grew, true);
            entry.len = end;
            state.uncommitted.grew(grew);
        }
        match (write.segment, &mut entry.data) {
            (Some(number), Data::Pieces(pieces)) => {
                let piece = Piece {
                    segment: number,
                    offset: write.offset,
                    extent_offset: write.extent_offset,
                    len: write.len,
                };
                // cladefs: what the extent held of where it wrote: not its holes, which writes past
                // its end grew it by, and which it writes over nothing in (TASK-160.16).
                let over = covered(pieces, write.extent_offset, end);
                add_piece(pieces, piece);
                if state.segments.held(number, (write.offset, write.len)) {
                    self.idle.notify(usize::MAX);
                }
                state.uncommitted.wrote_over(extent, over);
                state.uncommitted.push(Change::Write { extent, piece });
            }
            // Its file is the cell's to claim once it is sealed, and framed.
            (None, Data::Own) => {
                state.uncommitted.push(Change::WriteOwn { extent, len: entry.len });
            }
            _ => unreachable!("an extent's data is in segments or its own file"),
        }
        // Not the last sequence, which writes refuse ([`LastSequence`]).
        state.next_sequence = state.next_sequence.max(extent.sequence.saturating_add(1));
        self.count_open(state);
        self.written(state);
        self.see_ahead(state);
        let number = write.segment?;
        state.segments.write_behind(number, write.offset + write.len)
    }

    /// cladefs: has its footprint count its extents open as `state` holds them, which its reserve
    /// covers the sealing of (doc-22, TASK-539.8).
    fn count_open(&self, state: &State<D::File>) {
        if let Some(footprint) = self.dir.footprint() {
            footprint.set_open(state.uncommitted.open_bytes());
        }
    }

    fn abandon_write(&self, write: &Write<'_, D>) {
        self.drop_write(&mut self.lock(), write);
    }

    /// `write` is done, and holds nothing: abandoned, or to an extent freed since it began. What it
    /// wrote to a segment, if anything, is dead space; cladefs: which its writer wasted, until the
    /// next commit (TASK-79.2).
    fn drop_write(&self, state: &mut State<D::File>, write: &Write<'_, D>) {
        state.space.unreserve(write.grows);
        if let Some(number) = write.segment {
            state.uncommitted.let_go(write.len);
            if state.segments.dropped(number, (write.offset, write.len)) {
                self.idle.notify(usize::MAX);
            }
        }
        self.written(state);
        self.see_ahead(state);
    }

    /// A write in progress is done, finished or abandoned: a commit waiting for the last goes on.
    fn written(&self, state: &mut State<D::File>) {
        if state.gate.done() {
            self.idle.notify(usize::MAX);
        }
    }

    /// Where the `len` bytes at `offset` in `extent` are: in files, or zeros, where nothing was
    /// written, which is anything past the extent's end.
    pub fn locate(
        &self,
        extent: ExtentId,
        offset: u64,
        len: u64,
    ) -> Result<Vec<Located<D::File>>, Error> {
        self.locate_entry(extent, offset, len, Reader::Own)
    }

    /// Where the bytes are, as [`LocalCell::locate`] says, of an extent `reader` may read.
    fn locate_entry(
        &self,
        extent: ExtentId,
        offset: u64,
        len: u64,
        reader: Reader,
    ) -> Result<Vec<Located<D::File>>, Error> {
        self.check_io(offset, len)?;
        let mut state = self.state()?;
        let entry = state.entry_for(extent, reader)?;
        self.parts_locked(&mut state, extent, &entry, offset, len)
    }

    /// Where the `len` bytes at `offset` in `extent`, `entry` its entry, are, holding the state.
    fn parts_locked(
        &self,
        state: &mut State<D::File>,
        extent: ExtentId,
        entry: &Extent,
        offset: u64,
        len: u64,
    ) -> Result<Vec<Located<D::File>>, Error> {
        let end = offset + len;
        let held_end = end.min(entry.len).max(offset);
        let unpacked = crate::framed::unpacked_for(self.dir.host());
        let mut parts = Vec::new();
        match &entry.data {
            Data::Own => {
                if held_end > offset {
                    let source = self.own_source(state, extent)?;
                    source.locate(offset, held_end - offset, &unpacked, &mut parts)?;
                }
            }
            Data::Bucket(location) => bail!(BucketData { extent, location: location.clone() }),
            Data::Pieces(pieces) => {
                for (start, end, source) in paint(pieces, offset, held_end) {
                    match source {
                        None => parts.push(Located::Zeros(end - start)),
                        Some((number, at)) => {
                            let source = self.segment_file(state, number)?;
                            source.locate(at, end - start, &unpacked, &mut parts)?;
                        }
                    }
                }
            }
        }
        if end > held_end {
            parts.push(Located::Zeros(end - held_end));
        }
        Ok(parts)
    }

    /// Reads `parts` into `buf`, as long as they are.
    fn read_parts(&self, parts: Vec<Located<D::File>>, buf: &mut [u8]) -> Result<(), Error> {
        let mut at = 0;
        for part in parts {
            let len = part.len() as usize;
            let out = &mut buf[at..at + len];
            match part {
                Located::Data { file, offset, len } => {
                    let mut data = Aligned::new(len as usize);
                    file.read_at(data.as_mut_slice(), offset)?;
                    out.copy_from_slice(data.as_slice());
                }
                Located::Packed { part, skip, .. } => {
                    let bytes = self.read_packed(&part)?;
                    out.copy_from_slice(&bytes[skip as usize..skip as usize + len]);
                }
                Located::Unpacked { bytes, skip, .. } => {
                    out.copy_from_slice(&bytes[skip as usize..skip as usize + len]);
                }
                Located::Zeros(_) => out.fill(0),
            }
            at += len;
        }
        Ok(())
    }

    /// A part of a framed file, read, checked and unpacked.
    pub fn read_packed(
        &self,
        part: &Packed<D::File>,
    ) -> Result<bucket_cell::fetch::PartBytes, Error> {
        let stored = read_aligned(&*part.file, part.stored().offset, part.stored().len as usize)?;
        self.unpack(part, stored)
    }

    /// A part of a framed file, unpacked from `stored`, its stored bytes and any after them, read
    /// from its file, checked against its CRC; reads of it find it unpacked for a while.
    pub fn unpack(
        &self,
        part: &Packed<D::File>,
        stored: Aligned,
    ) -> Result<bucket_cell::fetch::PartBytes, Error> {
        let unpacked = crate::framed::unpacked_for(self.dir.host());
        part.unpack(stored, &unpacked)
    }

    /// Frees `extent`, which may be open, if the cell holds it.
    pub fn free(&self, extent: ExtentId) -> Result<(), Error> {
        let mut state = self.writable()?;
        let state = &mut *state;
        let packing = state.uncommitted.remove(extent);
        if let Some(Packing::Own(_)) = packing {
            // Its raw file, which it is not sealed from, or a write abandoned created.
            state.unreferenced.doomed.push(open_extent_path(extent));
        }
        let Some(entry) = state.extents.remove(&extent) else { return Ok(()) };
        if packing.is_some() {
            state.uncommitted.shrank(entry.len);
            self.count_open(state);
        }
        // cladefs: sealed, the cell's own is written no more (TASK-79.6).
        if packing.is_none() && extent.cell == self.cell.id {
            state.spent.insert(extent.sequence);
        }
        state.space.removed(entry.len, extent.cell == self.cell.id);
        state.uncommitted.push(Change::Free(extent));
        // A sealed one's data stays until the uploader has what it needs of it. cladefs: an open
        // one's goes at once:
        // no commit held it, so that no reader needs it, and what it took of segments is dead
        // space, counted so (TASK-79.2). Of a file of its own it has none, which its seal would
        // have made, but its raw file, which goes with the next commit: nothing else of it to let
        // go of, which would take the file the extent's next opening seals at that path
        // (TASK-79.1). What it took of either its writer wasted, until the next commit
        // (TASK-79.2).
        if packing.is_none() {
            state.segments.displace(extent, &entry.data);
            // cladefs: for the bucket, until the commit that frees it is acknowledged there, as the
            // segment holding its seal is by then (TASK-165.9).
            let uploader = rules().keep_frees;
            state.kept.insert(extent, Kept { entry, freed: kept::PENDING, uploader });
            return Ok(());
        }
        match &entry.data {
            Data::Pieces(pieces) => {
                for piece in pieces {
                    state.segments.let_go(piece);
                    state.uncommitted.let_go(piece.len);
                }
            }
            Data::Own => state.uncommitted.let_go(entry.len),
            Data::Bucket(_) => unreachable!("an open extent's data is in segments or a raw file"),
        }
        Ok(())
    }

    /// cladefs: reads `buf.len()` bytes at `offset` in `extent`, held, or freed and kept: for
    /// the bucket, until the commit that freed it is acknowledged there (TASK-165.9).
    pub fn read_kept(&self, extent: ExtentId, offset: u64, buf: &mut [u8]) -> Result<(), Error> {
        ensure!(!self.keys.is_revoked(), "cell keys revoked");
        self.read_into_entry(extent, offset, buf, Reader::Kept)
    }

    /// Commits what was written and freed since the last commit, and `root`, if given, as the
    /// cell's root, durably: a barrier, after which the cell recovers at least this. It seals
    /// the extents written before it.
    pub async fn commit(&self, root: Option<&[u8]>) -> Result<(), Error> {
        self.commit_ordered(root).await?;
        self.flush().await
    }

    /// cladefs: commits as [`LocalCell::commit`] does, but without waiting for durability: a
    /// barrier, which orders what came before it before what comes after, and seals the extents
    /// written before it. A crash keeps a prefix of the commits, which [`LocalCell::flush`] makes
    /// durable in order: doc-4's `COMMIT` without `COMMIT_UPLOADED`.
    pub async fn commit_ordered(&self, root: Option<&[u8]>) -> Result<(), Error> {
        let committing = self.committing.lock().await;
        self.commit_locked(&committing, root).await
    }

    /// Commits, as [`LocalCell::commit_ordered`] does, holding `committing`.
    async fn commit_locked(
        &self,
        committing: &AsyncMutexGuard<'_, ()>,
        root: Option<&[u8]>,
    ) -> Result<(), Error> {
        let mut state = self.between_writes(committing).await?;
        // From its first seal on, the state runs ahead of the files: a commit failing part way
        // fails the cell.
        let committed =
            crate::upload::blocking(|| state.failing(|state| self.commit_in(state, root)));
        committed.context("committing the extent store")
    }

    /// The state, to change, for a commit, holding `_committing`, once the writes in progress are
    /// done: a commit includes them, which may be to extents it seals, what they write in them,
    /// not after; and it closes the segments filled since the last commit, which they may write
    /// to. New ones wait for it, checked again after (TASK-83).
    async fn between_writes(
        &self,
        _committing: &AsyncMutexGuard<'_, ()>,
    ) -> Result<Writable<'_, D::File>, Error> {
        Ok(match rules().commit_waits_for_writes {
            true => {
                // Held, then let go of before any await: a future holding the state is no task's
                // to send. New writes wait on the gate meanwhile, and no other commit runs.
                let writing = {
                    let mut locked = self.writable()?.into_locked();
                    locked.gate.hold();
                    locked.gate.is_writing()
                };
                let mut locked = match writing {
                    true => self.lock_holding_writes(|state| state.gate.is_writing()).await,
                    false => self.lock(),
                };
                locked.gate.release();
                self.idle.notify(usize::MAX);
                Writable::rechecked(locked, self.cell)?
            }
            false => self.writable()?,
        })
    }

    /// cladefs: relocates `relocating`, extents its writer read from the cell's own segments in
    /// the bucket, sparse, into its open segment, in a commit of its own (doc-27, Writers
    /// compact): a relocation, which the uploader uploads, and the bucket acknowledges, as any
    /// commit. The guest asks for nothing and sees nothing: no root set, nothing freed, no
    /// sequence number minted, and the extents it has open stay open, its frees since its last
    /// commit for its next, which a relocation's commit neither seals nor makes (a seal would
    /// refuse its next write to them). Those of `relocating` the cell holds no more where they
    /// were read, freed or relocated since, are passed over. Durable once it returns, as a commit
    /// ([`LocalCell::commit`]) is. Returns the bytes it relocated: none, and no commit, if it
    /// relocated none.
    ///
    /// A relocation is optional (doc-27): it changes nothing of the guest's segments' life, and
    /// nothing it fails at fails the cell. It fills no segment, relocating only what the open one
    /// has room for, and closes none, so that the guest's next commit seals in place what the
    /// guest wrote in one run (TASK-92.41): while the guest has filled one since its last commit,
    /// or a run of an extent it has open ends where the copy would go, which the guest's next
    /// write to it goes on with, it relocates nothing. A copy that fails is dead space, as a write dropped is; what it
    /// copied before is committed, and it fails, saying so.
    /// Refused, the cell unchanged, unless each one's bytes are as many as where it was read holds.
    pub async fn relocate(&self, relocating: Vec<Relocating>) -> Result<Relocated, Error> {
        for Relocating { extent, from, data } in &relocating {
            let read = data.len() as u64;
            ensure!(read == from.len, "{extent:?} relocated with {read} bytes of {}", from.len);
        }
        // cladefs: within its disk's allowance and its tenant's limits, as a guest's write is,
        // room left for what the cell's own work adds for it (doc-22; Codex's review of
        // fa1912379): refused, it relocates nothing, held until its copies land.
        let bytes = relocating.iter().map(|relocating| relocating.from.len).sum();
        let _pledge =
            self.dir.footprint().map(|footprint| footprint.admit(bytes, 0)).transpose()?;
        let (relocated, failed) = {
            let committing = self.committing.lock().await;
            let mut state = self.between_writes(&committing).await?;
            // Its commit's record on, the state runs ahead of the files, as a commit's does.
            let relocated = crate::upload::blocking(|| {
                state.failing(|state| self.relocate_in(state, relocating))
            });
            relocated.context("relocating the extent store's extents")?
        };
        // Durable, as the uploader takes only what is.
        if relocated.commit.is_some() {
            self.flush().await?;
        }
        match failed {
            Some(failed) => Err(failed.context(PartlyRelocated(relocated))),
            None => Ok(relocated),
        }
    }

    /// cladefs: the steps of a relocation's commit ([`LocalCell::relocate`]): each extent the cell
    /// holds still where it was read, copied into one run of the open segment, then the commit,
    /// which names them there, its root unchanged.
    fn relocate_in(
        &self,
        state: &mut State<D::File>,
        relocating: Vec<Relocating>,
    ) -> Result<(Relocated, Option<Error>), Error> {
        if state.segments.has_full() || self.run_goes_on(state) {
            return Ok((Relocated::default(), None));
        }
        let (mut changes, mut relocated, mut failed) = (Vec::new(), 0, None);
        for Relocating { extent, from, data } in relocating {
            let held = match state.life(extent) {
                Life::Held(entry) => entry.into_cow().data == Data::Bucket(from.clone()),
                Life::Open(_) | Life::Kept(_) | Life::Spent | Life::Unknown => false,
            };
            if !held {
                continue;
            }
            if !state.segments.fits(from.len, self.options.segment_limit()) {
                break;
            }
            let mut aligned = Aligned::new(data.len());
            aligned.as_mut_slice().copy_from_slice(&data);
            let piece = match self.copy_relocated(state, &aligned) {
                Ok(piece) => piece,
                Err(error) => {
                    failed = Some(error.context(format!("relocating {extent:?}")));
                    break;
                }
            };
            changes.push(Change::Move { extent, pieces: vec![piece] });
            let data = Data::Pieces(vec![piece]);
            state.segments.place(extent, &data);
            state.extents.place(&extent, data);
            relocated += from.len;
        }
        if changes.is_empty() {
            return Ok((Relocated::default(), failed));
        }
        let epoch = state.epoch + 1;
        state.trim_kept(epoch);
        let next_sequence = state.next_sequence;
        let records = state.segments.record_moves(
            epoch,
            &changes,
            &state.extents,
            next_sequence,
            relocated,
        )?;
        let bucket_seg = state.segments.next_bucket_segment();
        let records =
            records.into_iter().map(|(seg, contents)| (seg, bucket_seg, contents)).collect();
        self.queue_flush(state, epoch, records, Vec::new());
        Ok((Relocated { bytes: relocated, commit: Some(epoch) }, failed))
    }

    /// cladefs: whether an extent the guest has open ends its run at the open segment's end, where
    /// its next write goes on with it, joining it (`Piece::joined`, TASK-92.1): a relocation's
    /// copy there would split it, and the guest's next commit copy the extent out (the verifying
    /// review of 8cf431e95). A relocation yields to it, as it is optional.
    fn run_goes_on(&self, state: &State<D::File>) -> bool {
        let open = state.segments.open_segment();
        let end = state.segments.len(open);
        let ends_there = |extent: ExtentId| match state.extents.get(&extent).and_then(Entry::own) {
            Some(Extent { data: Data::Pieces(pieces), .. }) => {
                pieces.iter().any(|piece| piece.segment == open && piece.offset + piece.len == end)
            }
            _ => false,
        };
        state.uncommitted.open_extents().into_iter().any(ends_there)
    }

    /// cladefs: copies `data`, an extent's whole, a relocation's, into one run of the open
    /// segment, which has room for it ([`LocalCell::relocate`]): returns the run. A write that
    /// fails leaves dead space where it went, as a write dropped does, which the cell's writer
    /// wasted until the next commit (TASK-79.2).
    fn copy_relocated(&self, state: &mut State<D::File>, data: &Aligned) -> Result<Piece, Error> {
        let len = data.as_slice().len() as u64;
        let open = state.segments.open_segment();
        let (file, at) = state.segments.append(len)?;
        let piece = Piece { segment: open, offset: at, extent_offset: 0, len };
        // A test's: the copy's write failing, as a disk's would.
        let write = || -> Result<(), Error> {
            failpoint!("relocate::write");
            Ok(file.write_at(data.as_slice(), at)?)
        };
        if let Err(error) = write() {
            state.segments.let_go(&piece);
            state.uncommitted.let_go(len);
            return Err(error.into());
        }
        state.segments.appended(open, at, len);
        written::wrote(written::Kind::Relocated, data.as_slice().len());
        self.see_ahead(state);
        Ok(piece)
    }

    /// The steps of the next commit, which sets `root`, if any, in their order: each takes what
    /// those before it leave.
    /// cladefs: its seal, and what it writes of it, each in a span of its own, `commit.seal` and
    /// `commit.write` (`bucket_cell::trace`, TASK-258).
    fn commit_in(&self, state: &mut State<D::File>, root: Option<&[u8]>) -> Result<(), Error> {
        let epoch = state.epoch + 1;
        let seal = trace::Span::start("commit.seal");
        seal.run(|| self.seal_open(state, &seal))?;
        drop(seal);
        let write = trace::Span::start("commit.write");
        write.set_count(trace::COMMIT, epoch);
        write.run(|| self.write_commit(state, epoch, root))
    }

    /// Writes commit `epoch`, which sets `root`, if any, its open extents sealed: lets go of the
    /// frees the uploader needs no more, records it in the open segment, and queues its flush.
    fn write_commit(
        &self,
        state: &mut State<D::File>,
        epoch: u64,
        root: Option<&[u8]>,
    ) -> Result<(), Error> {
        // cladefs: the frees it makes are its, by its number, from now on: kept as freed by a
        // commit to come until one makes them, whichever commits come between (a relocation's,
        // doc-27).
        for extent in state.uncommitted.frees().collect::<Vec<_>>() {
            state.kept.freed_by(extent, epoch);
        }
        state.trim_kept(epoch);
        let (changes, capacity) = (state.uncommitted.take(), state.uncommitted.take_capacity());
        self.count_open(state);
        // The open segment's footer records them, before the segment closes; cladefs: and its log,
        // unless it closes with them (TASK-165.9).
        let changed = (root, capacity);
        let records =
            state.segments.record(epoch, &changes, changed, &state.extents, state.next_sequence)?;
        if let Some(root) = root {
            state.root = root.to_vec();
        }
        self.committed(state, epoch, records)
    }

    /// cladefs: what follows commit `epoch`, whose contents are `records`, by the segments it
    /// spans, in the state: the segments filled since the last commit closed and framed, which it
    /// sealed in, the open segment closed if it is due, the segments it let go of, and what its
    /// flush makes durable, queued.
    fn committed(
        &self,
        state: &mut State<D::File>,
        epoch: u64,
        records: Vec<(SegmentId, Contents)>,
    ) -> Result<(), Error> {
        // cladefs: with the rule off, the segments the last commit closed are framed only now, by a
        // commit that may never come (TASK-150.2).
        let mut closed = std::mem::take(&mut state.deferred);
        let mut closed_bucket_segs: Vec<(SegmentId, u64)> = Vec::new();
        // cladefs: those filled since the last commit, in the order they filled, before the open
        // one, as the chain stamps them (TASK-92.41).
        for unframed in state.segments.close_full(epoch, state.next_sequence) {
            closed_bucket_segs.push((unframed.number(), unframed.footer.segment));
            self.enqueue(state, &unframed);
            match rules().framings_end_themselves {
                true => closed.push(unframed),
                false => state.deferred.push(unframed),
            }
        }
        // cladefs: framed together, made durable together once framed (TASK-310), whether or not
        // the open segment's closing fails.
        let due = self.close_if_due(state, epoch).map(|due| {
            if let Some(unframed) = due {
                closed_bucket_segs.push((unframed.number(), unframed.footer.segment));
                closed.push(unframed);
            }
        });
        let open_bucket_seg = state.segments.next_bucket_segment();
        let records: Vec<(SegmentId, u64, Contents)> = records
            .into_iter()
            .map(|(seg, contents)| {
                let bucket_seg = closed_bucket_segs
                    .iter()
                    .find(|&&(id, _)| id == seg)
                    .map_or(open_bucket_seg, |&(_, bs)| bs);
                (seg, bucket_seg, contents)
            })
            .collect();
        self.frame_behind(state, closed);
        due?;
        let Unreferenced { doomed } = state.unreferenced.take();
        self.queue_flush(state, epoch, records, doomed);
        Ok(())
    }

    /// cladefs: commit `epoch`, whose contents are `records`, by the segments it spans, is the
    /// cell's from now on, its flush queued, which makes it durable, then removes `doomed`, the
    /// files it makes unreferenced ([`LocalCell::committed`]).
    fn queue_flush(
        &self,
        state: &mut State<D::File>,
        epoch: u64,
        records: Vec<(SegmentId, u64, Contents)>,
        doomed: Vec<String>,
    ) {
        // What its flush syncs, after the closing, whose directories its flush must sync: the
        // next segment's raw file's and log's entries durable before a commit made in it.
        let mut syncs = Syncs::default();
        state.own_files.take_syncs(&mut syncs);
        let entries = state.segments.take_syncs(&mut syncs);
        // The segments none of its extents' data is in any more, acknowledged in the bucket: their
        // files go into the host's cache once it is durable (TASK-165.8).
        let leaving = (state.segments.reap().into_iter())
            .map(|(number, key)| (segment_path(number), key))
            .collect();
        state.epoch = epoch;
        // Queued while the state is locked, so that commits queue in their order.
        let pending = Pending { epoch, syncs, records, doomed, leaving, entries };
        self.durability.with(Mutex::lock).push(epoch, pending);
    }

    /// Seals the extents open, as a commit does, each in one run, or a file of its own if large
    /// (doc-7, Segments); cladefs: saying on `span` how many, and the bytes it copied.
    fn seal_open(&self, state: &mut State<D::File>, span: &trace::Span) -> Result<(), Error> {
        let sealing = state.uncommitted.open_extents();
        // cladefs: what sealing copied, which the commit's flush syncs: said once past 8 MiB, which
        // the flush of cladefsd's turn writes at once (TASK-221).
        let (mut copies, mut copied, mut largest) = (0, 0, 0);
        // cladefs: why it copies what it copies, with CLADEFS_SEAL_STATS set (TASK-92.41).
        let mut why = seals::enabled().then(seals::Sealing::default);
        for &extent in &sealing {
            if let Some(why) = &mut why
                && let Some((sealed, len, pieces)) = self.why_sealed(state, extent)
            {
                why.sealed(sealed, len, pieces);
            }
            let bytes = self.seal(state, extent)?;
            copies += u64::from(bytes > 0);
            copied += bytes;
            largest = largest.max(bytes);
        }
        if copied > 8 << 20 {
            log::warn!(
                "extent store: cell {}'s commit sealed {} extents, copying {copies} of them, \
                 {} KiB, the largest {} KiB",
                self.cell,
                sealing.len(),
                copied >> 10,
                largest >> 10
            );
        }
        if let Some(why) = why {
            why.log(self.cell.id, state.epoch);
        }
        span.set_count(trace::EXTENTS, sealing.len() as u64);
        span.set_count(trace::BYTES, copied);
        Ok(())
    }

    /// cladefs: why sealing `extent` copies it, or not, its length and how many pieces it is in;
    /// none if sealing does nothing with it (TASK-92.41).
    fn why_sealed(
        &self,
        state: &State<D::File>,
        extent: ExtentId,
    ) -> Option<(seals::Why, u64, usize)> {
        let entry = state.extents.get(&extent)?.own()?;
        match (&entry.data, state.uncommitted.packing(extent)) {
            (Data::Own, Some(Packing::Own(_))) => Some((seals::Why::Own, entry.len, 1)),
            (Data::Pieces(pieces), _) => {
                let large = entry.len >= self.options.large_extent;
                let standing = |segment| state.segments.standing(segment);
                let why = seals::Why::of_pieces(pieces, entry.len, standing, large);
                Some((why, entry.len, pieces.len()))
            }
            _ => None,
        }
    }

    /// Closes the open segment after commit `epoch`, once no write is in progress in it, if it is
    /// due: full, old, wanted closed, or at a fork; writes go to a new one. Returns it, to be
    /// framed off the commit ([`LocalCell::frame_behind`]).
    fn close_if_due(
        &self,
        state: &mut State<D::File>,
        epoch: u64,
    ) -> Result<Option<Unframed<D::File>>, Error> {
        if !state.segments.due(&self.options) {
            return Ok(None);
        }
        self.close_open_segment(state, epoch).map(Some)
    }

    /// Seals `extent`, which a commit seals, in one run in the open segment, or in a file of its
    /// own, framed, if it is large, copying its data there unless it is. cladefs: or where it is,
    /// if one run of a segment filled since the last commit, which the commit goes on with
    /// (`Segments::fill`, TASK-92.41); a copy past the open segment's room filling it, and going in
    /// the next. Returns the bytes it copied, if it copied them.
    fn seal(&self, state: &mut State<D::File>, extent: ExtentId) -> Result<u64, Error> {
        let Some(entry) = state.extents.get(&extent) else {
            // cladefs: opened by a write abandoned, none since: its raw file, if it has one, holds
            // nothing, and goes now, as the commit waited for the writes in progress; not with the
            // commit's flush, which the extent's next write, making another, may come before
            // (TASK-79.5). Opening the cell removes it, failing here.
            if let Some(Packing::Own(_)) = state.uncommitted.packing(extent) {
                let path = open_extent_path(extent);
                if let Err(err) = self.dir.remove(&path) {
                    log::warn!("extent store: failed to remove {path}: {err}");
                }
            }
            return Ok(0);
        };
        // In the bucket: encrypted there.
        let Some(entry) = entry.own().cloned() else { return Ok(0) };
        let large = entry.len >= self.options.large_extent;
        match (&entry.data, state.uncommitted.packing(extent)) {
            (Data::Bucket(_), _) => return Ok(0),
            // Written to a raw file of its own, which it is framed from, or in.
            (Data::Own, Some(Packing::Own(raw))) => {
                let raw = raw.clone();
                return self.seal_own(state, extent, raw, entry.len);
            }
            (Data::Pieces(pieces), _) if !large => {
                let one_run = matches!(pieces[..], [piece] if state.segments.seals_in(piece.segment)
                    && piece.extent_offset == 0
                    && piece.len == entry.len);
                if one_run {
                    state.segments.place(extent, &entry.data);
                    return Ok(0);
                }
            }
            _ => {}
        }
        let Data::Pieces(pieces) = &entry.data else {
            bail!("{extent:?}, open, is in neither segments nor a file of its own")
        };
        let mut data = Aligned::new(entry.len as usize);
        let parts = self.parts_locked(state, extent, &entry, 0, entry.len)?;
        self.read_parts(parts, data.as_mut_slice())?;
        // Its old places go.
        for piece in pieces {
            state.segments.let_go(piece);
        }
        let data_now = if large {
            let data = data.as_slice();
            self.write_own(state, extent, entry.len, |at, buf| {
                buf.copy_from_slice(&data[at as usize..at as usize + buf.len()]);
                Ok(())
            })?;
            state.uncommitted.push(Change::MoveOwn { extent });
            Data::Own
        } else {
            let pieces = vec![self.copy_to_segment(state, &data, written::Kind::Copied)?];
            state.uncommitted.push(Change::Move { extent, pieces: pieces.clone() });
            Data::Pieces(pieces)
        };
        state.segments.place(extent, &data_now);
        state.extents.place(&extent, data_now);
        Ok(entry.len)
    }

    /// Copies `data`, an extent's whole, into one run of the open segment, written as `kind`:
    /// cladefs: in the next, the open one full, which the commit goes on with past it
    /// (TASK-92.41). Returns the run.
    fn copy_to_segment(
        &self,
        state: &mut State<D::File>,
        data: &Aligned,
        kind: written::Kind,
    ) -> Result<Piece, Error> {
        let len = data.as_slice().len() as u64;
        if !state.segments.fits(len, self.options.segment_limit()) {
            state.segments.fill(&*self.dir, state.epoch)?;
        }
        let open = state.segments.open_segment();
        let (file, at) = state.segments.append(len)?;
        file.write_at(data.as_slice(), at)?;
        written::wrote(kind, data.as_slice().len());
        state.segments.appended(open, at, len);
        self.see_ahead(state);
        Ok(Piece { segment: open, offset: at, extent_offset: 0, len })
    }

    /// cladefs: seals `extent`, open in `raw`, a raw file of its own, of its `len` bytes, in its
    /// own file, framed: in place, if none of its parts compresses, as an object of such parts
    /// holds them as they are, where they are (`extent_frame::Packer::in_place`), its raw file that
    /// file then, its tail appended, renamed; or else in a file of its own, as
    /// [`LocalCell::write_own`] frames one, the parts before the first that compresses copied
    /// there. A large extent's data was written twice, in its raw file, which the host's disk wrote
    /// back as its guest wrote it, and in the framed file: as much again as its guest wrote, its
    /// framings and the disk's other writers waiting behind it (TASK-310). In place, its bytes
    /// don't change, which a read of the raw file in progress reads. Returns the bytes it copied:
    /// none in place. cladefs: an encrypted cell's in place too, its raw file's XTS sectors the
    /// object's parts, each tagged as it is read, stored, then decrypted to be packed
    /// (TASK-411.23); its parts before the first that compresses are not its copy's, encrypted with
    /// AES-GCM: framed anew, whole, in a copy then.
    fn seal_own(
        &self,
        state: &mut State<D::File>,
        extent: ExtentId,
        raw: Arc<RawFile<D::File>>,
        len: u64,
    ) -> Result<u64, Error> {
        let (raw_path, path) = (open_extent_path(extent), extent_path(extent));
        let (dek, key) = self.keys.try_encrypting()?;
        let mut packer = extent_frame::Packer::new((&dek, key));
        let sectors = raw.sectors();
        // cladefs: in place, whole sectors of its raw file, under the raw file's key (TASK-411.23).
        let in_place = len.is_multiple_of(SECTOR);
        let mut raw_parts = Vec::new();
        let mut data = Aligned::new(len.min(OWN_CHUNK) as usize);
        // The framed file, once a part compresses.
        let mut framed: Option<D::File> = None;
        for at in (0..len).step_by(OWN_CHUNK as usize) {
            let buf = &mut data.as_mut_slice()[..(len - at).min(OWN_CHUNK) as usize];
            if in_place && framed.is_none() {
                raw.file().read_at(buf, at)?;
                let parts =
                    (buf.chunks(extent_frame::PART as usize).zip(at / extent_frame::PART..))
                        .map(|(part, index)| extent_frame::RawPart::of(index, part, sectors));
                raw_parts.extend(parts);
                raw.decrypt(at, buf);
            } else {
                raw.read_at(buf, at)?;
            }
            let end = packer.end();
            let concurrent = self.dir.concurrent();
            let packed = packer.push_with(buf, |first, parts, key| {
                crate::framing::pack_parts(first, parts, key, concurrent)
            });
            // As they are, where they are, as those before.
            if framed.is_none() && in_place && packer.stored_raw() {
                continue;
            }
            let file = match &mut framed {
                Some(file) => file,
                // Its raw parts before are no parts of a cell at a version's, encrypted with
                // AES-GCM: framed anew, whole.
                None if end > 0 => {
                    self.write_own(state, extent, len, |at, buf| raw.read_at(buf, at))?;
                    state.unreferenced.doomed.push(raw_path);
                    return Ok(len);
                }
                None => {
                    let file =
                        self.dir.create(&path, true).with_context(|| format!("creating {path}"))?;
                    framed.insert(file)
                }
            };
            file.write_at(&packed, end)?;
            written::wrote(written::Kind::Own, packed.len());
        }
        let (file, copied, (tail, parts)) = match framed {
            Some(file) => {
                let end = packer.end();
                let finished = packer.finish_parts(&own_footer(extent));
                file.write_at(&finished.0, end)?;
                state.unreferenced.doomed.push(raw_path);
                (Arc::new(file), len, finished)
            }
            None => {
                let mut in_place = extent_frame::Packer::in_place(raw.sectors().clone());
                in_place.push_raw_parts(raw_parts);
                let end = in_place.end();
                let finished = in_place.finish_parts(&own_footer(extent));
                let file = raw.file().clone();
                file.write_at(&finished.0, end)?;
                self.dir
                    .rename(&raw_path, &path)
                    .with_context(|| format!("renaming {raw_path}"))?;
                (file, 0, finished)
            }
        };
        written::wrote(written::Kind::Own, tail.len());
        state.own_files.sealed(
            extent,
            Arc::new(Framed::new(file, parts, crate::framed::unpacked_for(self.dir.host()))),
        );
        Ok(copied)
    }

    /// Writes `extent`'s own file, framed, of its `len` bytes, which `read` reads, by ranges, into
    /// buffers: so many parts at a time.
    fn write_own(
        &self,
        state: &mut State<D::File>,
        extent: ExtentId,
        len: u64,
        mut read: impl FnMut(u64, &mut [u8]) -> io::Result<()>,
    ) -> Result<(), Error> {
        let path = extent_path(extent);
        let file = self.dir.create(&path, true).with_context(|| format!("creating {path}"))?;
        let (dek, key) = self.keys.try_encrypting()?;
        let mut packer = extent_frame::Packer::new((&dek, key));
        let mut data = Aligned::new(len.min(OWN_CHUNK) as usize);
        for at in (0..len).step_by(OWN_CHUNK as usize) {
            let buf = &mut data.as_mut_slice()[..(len - at).min(OWN_CHUNK) as usize];
            read(at, buf)?;
            let end = packer.end();
            let concurrent = self.dir.concurrent();
            let packed = packer.push_with(buf, |first, parts, key| {
                crate::framing::pack_parts(first, parts, key, concurrent)
            });
            file.write_at(&packed, end)?;
            written::wrote(written::Kind::Own, packed.len());
        }
        let end = packer.end();
        let (tail, parts) = packer.finish_parts(&own_footer(extent));
        file.write_at(&tail, end)?;
        written::wrote(written::Kind::Own, tail.len());
        let framed = Arc::new(Framed::new(
            Arc::new(file),
            parts,
            crate::framed::unpacked_for(self.dir.host()),
        ));
        state.own_files.sealed(extent, framed);
        Ok(())
    }

    /// Closes the open segment, after commit `epoch`: its footer, of what the commits since it
    /// opened changed, stamped by the chain of the uploader's writer, if any, after its data, which
    /// is to be framed in a file of its own, as object storage keeps segments; and opens another.
    fn close_open_segment(
        &self,
        state: &mut State<D::File>,
        epoch: u64,
    ) -> Result<Unframed<D::File>, Error> {
        let unframed = state.segments.close(&*self.dir, epoch, state.next_sequence)?;
        self.enqueue(state, &unframed);
        Ok(unframed)
    }

    /// cladefs: `unframed`, a segment just closed, is a segment of the write-ahead log from now on,
    /// until acknowledged (TASK-165.9), holding the bytes of its data framed, and of the extents
    /// stored on their own its footer names.
    fn enqueue(&self, state: &mut State<D::File>, unframed: &Unframed<D::File>) {
        #[cfg(any(test, feature = "audit"))]
        state.closed(&unframed.footer);
        let own: u64 = unframed.footer.contents.own.iter().map(|(_, len)| len).sum();
        let bytes = unframed.len() + own;
        state.wal.push(unframed.number().number, unframed.footer.clone(), bytes);
    }

    /// Has the next commit close the open segment, whatever it holds: for its data to be uploaded
    /// with its footer.
    pub fn close_segment(&self) -> Result<(), Error> {
        self.writable()?.segments.want_close();
        Ok(())
    }

    /// cladefs: closes the open segment now, between commits, once no write is in progress in it,
    /// if commits made since it opened, or forks: for its uploader to upload what the commits made
    /// so far without waiting for the next. Writes wait for the closing alone: it is framed, and
    /// made durable, by a framing that ends itself, as a commit's closing is
    /// ([`LocalCell::frame_behind`]), waited for holding no lock, so that the cell serves its
    /// writes, reads and commits meanwhile, which read the segment from its raw file (TASK-41.1).
    /// Its framed file is durable when this returns; its raw file goes once the commits its footer
    /// names are durable, as a commit's closing's does: so that opening the cell reads no segment
    /// whose raw file is gone (TASK-92.16). Returns whether it closed.
    pub async fn close_now(&self) -> Result<bool, Error> {
        self.close_now_unless(false).await
    }

    /// cladefs: closes as [`LocalCell::close_now`] does, unless the guest is filling the segment it
    /// would close (`Segments::filling`): its next commit seals in place what is one run in it,
    /// where closed now that commit copies all it wrote since out of it (TASK-92.41). What an
    /// uploader's interval does; a commit waiting for its upload closes it whatever it holds.
    /// Returns whether it closed.
    pub async fn close_now_unless_filling(&self) -> Result<bool, Error> {
        self.close_now_unless(true).await
    }

    /// cladefs: closes as [`LocalCell::close_now`] does, `unless_filling` as
    /// [`LocalCell::close_now_unless_filling`] does, for the uploader: until its framing wrote its
    /// framed file, which it uploads, not until the framing made it durable, which its raw file
    /// and log stand for meanwhile, until the commits its footer names are (TASK-384). A durable
    /// commit waited, one after another, for the segment's framed file synced, the directory of
    /// framed files after it, each a commit of the host's journal, before its upload began.
    /// Returns whether it closed.
    pub async fn close_for_upload(&self, unless_filling: bool) -> Result<bool, Error> {
        let closed = self.close_between_commits(unless_filling).await?;
        let Some(number) = closed else { return Ok(false) };
        self.wait_for_writings([number]).await.context("closing the open segment")?;
        Ok(true)
    }

    /// cladefs: [`LocalCell::close_now`], unless `filling` and the guest is filling the segment.
    async fn close_now_unless(&self, filling: bool) -> Result<bool, Error> {
        let Some(number) = self.close_between_commits(filling).await? else { return Ok(false) };
        let state = self.lock_when(|state| !(state.segments.is_framing(number))).await;
        match state.segments.framing_failure(number) {
            Some(error) => bail!("closing the open segment: framing it: {error}"),
            None => Ok(true),
        }
    }

    /// cladefs: closes the open segment, between commits, as [`LocalCell::close_now`] does, framing
    /// it behind, and returns it: none if no commit came since it opened, nor fork, or the cell is
    /// closed. cladefs: or the first segment filled since the last commit, holding commits, which
    /// would wait for the next otherwise (`Segments::fill`, TASK-92.41).
    async fn close_between_commits(
        &self,
        unless_filling: bool,
    ) -> Result<Option<SegmentId>, Error> {
        let _committing = self.committing.lock().await;
        let busy = |state: &State<D::File>| {
            rules().close_waits_for_writers && state.segments.closing_writers() > 0
        };
        // The state looked at, and new writes held back, as a commit holds them
        // (`LocalCell::commit_locked`), then let go of before any await.
        let waiting = {
            let state = self.state()?;
            // Closed, it changes nothing: its last segment closes when it is next opened.
            if state.standing.is_closed() || !state.segments.has_commits(state.epoch) {
                return Ok(None);
            }
            if unless_filling && state.segments.filling() {
                return Ok(None);
            }
            let mut locked = state.into_locked();
            locked.gate.hold();
            busy(&locked)
        };
        let mut locked = match waiting {
            true => self.lock_holding_writes(busy).await,
            false => self.lock(),
        };
        locked.gate.release();
        self.idle.notify(usize::MAX);
        let mut state = Readable::rechecked(locked, self.cell)?;
        // cladefs: and closed while it waited, as before (TASK-83).
        if state.standing.is_closed() {
            return Ok(None);
        }
        let epoch = state.epoch;
        let unframed = crate::upload::blocking(|| {
            state.failing(|state| match state.segments.has_full() {
                true => {
                    let unframed = state.segments.close_first_full(epoch, state.next_sequence);
                    self.enqueue(state, &unframed);
                    Ok(unframed)
                }
                false => self.close_open_segment(state, epoch),
            })
        });
        let unframed = unframed.context("closing the open segment")?;
        let number = unframed.number();
        crate::upload::blocking(|| self.frame_behind(&mut state, [unframed]));
        Ok(Some(number))
    }

    /// cladefs: frames `closed`, segments just closed, which `state` holds, off what closed them:
    /// a commit, a write finding no room, a closing between commits or a fork's commit, alike. Its
    /// framing writes it ([`Unframed::frame`]) and makes it durable, with the segments framed
    /// meanwhile (`segments::make_durable`, TASK-310), then ends itself, holding the state: the
    /// segment read from its framed file from now on, its raw file going once the commits its
    /// footer names are durable ([`Segments::framed`]); or the cell failed. Whatever comes next,
    /// or never does, only waits for that, as the flush of the commit that closed it, a fork, the
    /// cell's closing and [`LocalCell::close_now`] do (TASK-153.8): it ends whether or not a commit
    /// comes after (TASK-150.2), and a closing waits for every one, being counted
    /// ([`Segments::framings`], TASK-79.99). On a thread of its own, which would otherwise hold up
    /// every write and commit behind it for as long as reading, compressing, writing and syncing a
    /// segment takes (TASK-41: 67 ms under pgbench, the cell's commits' time); on the caller's,
    /// holding them up as before TASK-41, if the directory's I/O is its callers' alone
    /// ([`Dir::concurrent`], TASK-79.79), or if the host refuses a thread, rather than panic
    /// through the cell's locks, which the caller holds (TASK-79.80).
    fn frame_behind(
        &self,
        state: &mut State<D::File>,
        closed: impl IntoIterator<Item = Unframed<D::File>>,
    ) {
        // cladefs: one framer at a time, in the order the segments closed (`wal`'s docs,
        // TASK-165.9); each segment made durable once framed, with those framed meanwhile, by
        // another thread ([`Framer`], TASK-310).
        for unframed in closed {
            state.segments.framing_begun(unframed.number());
            state.framing.queue.push_back(Job::Frame(unframed));
        }
        self.run_framer(state);
    }

    /// cladefs: has the framer look ahead at the parts of segments written whole since it last did,
    /// as they fill, behind the framings queued before them, as it frames them
    /// ([`LocalCell::frame_behind`]): its framings in place read them no more (TASK-400). A
    /// segment's framing read all of its data back as it closed, seconds after its guest wrote it,
    /// from the host's disk once the host had let its pages go, its 16 GiB at its limit in the long
    /// mode, 0.7-5.7 s with eight VMs and their bucket keeping the disk busy.
    fn see_ahead(&self, state: &mut State<D::File>) {
        let seeing = state.segments.take_seeing();
        if seeing.is_empty() {
            return;
        }
        let jobs = seeing.into_iter().map(|(number, part)| Job::See(number, part));
        state.framing.queue.extend(jobs);
        self.run_framer(state);
    }

    /// cladefs: runs the framer, if none runs and it has work queued ([`LocalCell::frame_behind`]).
    fn run_framer(&self, state: &mut State<D::File>) {
        if state.framing.queue.is_empty() || std::mem::replace(&mut state.framing.running, true) {
            return;
        }
        let framer = self.framer();
        if self.dir.concurrent() {
            let thread = framer.clone();
            if spawn_or_return((), move |()| thread.frame_queued()).is_none() {
                return;
            }
        }
        framer.frame_here(state);
    }

    /// cladefs: removes the files at `paths`, as nothing refers to them: logging what it can't,
    /// which opening the cell removes.
    fn remove(&self, paths: impl IntoIterator<Item = String>) {
        for path in paths {
            if let Err(err) = self.dir.remove(&path)
                && !not_found(&err)
            {
                log::warn!("extent store: failed to remove {path}: {err}");
            }
        }
    }

    /// cladefs: the numbers of the cell's segments being framed behind what closed them
    /// ([`LocalCell::frame_behind`]): their framed files, in the cell directory already, are whole
    /// only once their framings end, their raw files holding their data meanwhile. What reads the
    /// directory beside the cell, as a test does, reads no other framed file (TASK-189).
    pub fn framing(&self) -> Vec<u64> {
        let state = self.lock();
        let framing = state.segments.framings().iter();
        let ours = framing.filter(|segment| segment.cell == self.cell.id);
        ours.map(|segment| segment.number).collect()
    }

    /// cladefs: the last commit whose segments the uploader may take: the last durable. A segment
    /// of commits a crash may lose here, acknowledged in the bucket, would leave the cell past the
    /// write-ahead log the cell opens with, which its opening reads as diverged (`ESTALE`), the
    /// commits its own (TASK-165.9). The uploader flushes before it takes them, but for commits
    /// made between.
    fn uploadable(&self) -> u64 {
        self.durability.with(Mutex::lock).local()
    }

    /// cladefs: waits, holding no lock, for the framings of segment `number` and those before it to
    /// end, their framed files durable, which the uploader waits for no more before it uploads
    /// them (TASK-384): fails if one failed.
    async fn wait_framed_through(&self, number: u64) -> Result<(), Error> {
        let framing =
            |segment: &SegmentId| segment.cell == self.cell.id && segment.number <= number;
        let state = self.lock_when(|state| !(state.segments.framings().iter().any(framing))).await;
        let segment = SegmentId { cell: self.cell.id, number };
        match state.segments.framing_failure(segment) {
            Some(error) => bail!("framing segment {}: {error}", Short(number)),
            None => Ok(()),
        }
    }

    /// cladefs: waits, holding no lock, for every framing running to end, its framed file durable,
    /// or failed, which it fails then: what reads the directory beside the cell, as a test does,
    /// finds the segments framed then named their own, their raw files and logs going with the next
    /// flush, if their commits are durable. The uploader waits for no more than their framed files
    /// written (`LocalCell::closed`, TASK-384).
    pub async fn wait_framed(&self) -> Result<(), Error> {
        let framing = self.state()?.segments.framings().clone();
        // And the thread making them durable done with them.
        let state = self
            .lock_when(|state| {
                !(framing.iter().any(|&number| state.segments.is_framing(number))
                    || state.framing.syncing)
            })
            .await;
        match framing.iter().find_map(|&number| state.segments.framing_failure(number)) {
            Some(error) => bail!("framing a segment: {error}"),
            None => Ok(()),
        }
    }

    /// cladefs: waits, holding no lock, until no look ahead at a part written whole is queued or
    /// reading it ([`LocalCell::see_ahead`]): the framer reads each, as its segment fills, beside
    /// what wrote it and after, whatever comes next (TASK-400). What measures the directory's I/O
    /// across an operation, as a test does, counts the operation's alone then, for none is queued
    /// but by writes (TASK-423). Packing a part looked at ahead, on the threads packing ahead,
    /// reads no file, and may run on.
    pub async fn wait_looked_ahead(&self) {
        drop(self.lock_when(|state| !(state.framing.looks_ahead())).await);
    }

    /// cladefs: waits, holding no lock, until the cell does nothing to its directory off its
    /// callers' threads: no segment is being framed, nor made durable, its framed file renamed its
    /// own (`segments::make_durable`); and no look ahead is queued or reading
    /// ([`LocalCell::wait_looked_ahead`]). What reads the directory beside the cell, as a test
    /// does, finds then every file it lists still there under the name listed, as after
    /// [`LocalCell::close`], until its callers, or its uploader, do something more (TASK-463).
    /// Fails if the cell failed.
    pub async fn wait_idle(&self) -> Result<(), Error> {
        let idle = |state: &State<D::File>| {
            state.segments.framings().is_empty()
                && !state.framing.syncing
                && !state.framing.looks_ahead()
        };
        drop(self.lock_when(idle).await);
        self.state().map(drop)
    }

    /// cladefs: the segments of the write-ahead log not yet taken, oldest first, `limit` of them at
    /// most, for the uploader, which has them until it says they are acknowledged
    /// ([`LocalCell::acknowledged`]): the rest it takes later, their files not opened meanwhile.
    /// Only those whose commits are durable ([`LocalCell::uploadable`]). Those being framed it
    /// waits for, holding no lock: each framing ends itself, whatever comes next, which may be
    /// nothing, as after a write that closed one for want of room (TASK-150.2, TASK-153.8).
    /// cladefs: until it wrote their framed files, not until it made them durable, which their raw
    /// files and logs stand for meanwhile, until the commits their footers name are (TASK-384).
    pub async fn closed(&self, limit: usize) -> Result<Vec<Closed<D::File>>, Error> {
        let to_wait: Vec<SegmentId> = {
            let state = self.state()?;
            let durable = self.uploadable();
            let segments = &state.segments;
            let deferred = &state.deferred;
            state
                .wal
                .pending_writing(durable, limit, |number| {
                    // When Shuttle tests turn `framings_end_themselves` off (board 13424),
                    // segments in `state.deferred` have not started framing yet; waiting on them
                    // would deadlock instead of surfacing the missing segment to the caller.
                    let segment = SegmentId { cell: self.cell.id, number };
                    segments.is_writing(segment) && !deferred.iter().any(|u| u.number() == segment)
                })
                .into_iter()
                .map(|number| SegmentId { cell: self.cell.id, number })
                .collect()
        };
        if !to_wait.is_empty() {
            self.wait_for_writings(to_wait).await?;
        }
        self.take_closed(limit)
    }

    /// Takes up to `limit` untaken durable segments whose framed files are already written without
    /// waiting on later segments still queued in the framer, so streaming uploads overlap with
    /// framing instead of stalling behind queued framings.
    pub(crate) async fn closed_ready(&self, limit: usize) -> Result<Vec<Closed<D::File>>, Error> {
        self.take_closed(limit)
    }

    fn take_closed(&self, limit: usize) -> Result<Vec<Closed<D::File>>, Error> {
        crate::upload::blocking(|| {
            let mut state = self.state()?;
            let state = &mut *state;
            // cladefs: and framed: its framed file is what is uploaded.
            let segments = &state.segments;
            let writing = |number| segments.is_writing(SegmentId { cell: self.cell.id, number });
            let taken: Vec<(u64, Footer, bool, u64)> =
                (state.wal.take(self.uploadable(), writing, limit))
                    .into_iter()
                    .map(|queued| {
                        (queued.number, queued.footer.clone(), queued.restamped, queued.data_len())
                    })
                    .collect();
            let mut closed = Vec::with_capacity(taken.len());
            for (number, footer, restamped, data_len) in taken {
                let segment = SegmentId { cell: self.cell.id, number };
                let file = state.segments.framed_file(&*self.dir, segment)?.file.clone();
                let mut own = Vec::with_capacity(footer.contents.own.len());
                for (extent, location) in footer.own() {
                    let path = extent_path(extent);
                    let size = match state.own_files.size(extent) {
                        Some(size) => size,
                        None => state.own_files.file(&*self.dir, extent)?.size(),
                    };
                    let dir = self.dir.clone();
                    own.push(crate::uploads::ClosedOwn::new(location.key, size, move || {
                        let file = dir.open(&path, true)?;
                        crate::framed::read_object(&file)
                    }));
                }
                let keys = self.keys.clone();
                closed.push(Closed { number, footer, file, own, restamped, keys, data_len });
            }
            Ok(closed)
        })
    }

    /// cladefs: segment `number`, which [`LocalCell::closed`] gave, the first of the write-ahead
    /// log, is acknowledged in the bucket: the extents sealed in it, and those stored on their own
    /// its footer names, are read there from now on ([`BucketData`]), and their files leave the
    /// cell for the host's cache, or go: the extents' own files, and the segment's, once no
    /// extent's data is in it here (TASK-165.8). The frees of its commits are the bucket's to keep.
    /// The log's start, its last commit from now on, is durable first, so that the cell opened
    /// again puts what is left of the log after the cell (TASK-165.9).
    ///
    /// cladefs: once its framing ended, its framed file durable, which its upload waited for no
    /// more, but for its framed file written (TASK-384): its upload and the syncs making it
    /// durable here go on at once, the bucket's round trip mostly the longer; and once it is
    /// acknowledged, its files leave as before, its framed file named its own, into the host's
    /// cache, where reads of its extents in the bucket find it, and its raw file and log go.
    pub async fn acknowledged(&self, number: u64) -> Result<(), Error> {
        self.acknowledged_batch(&[number]).await
    }

    /// Acknowledges a contiguous prefix `numbers` of the write-ahead log with a single durable
    /// WAL start-file update instead of one `fsync` per segment.
    pub async fn acknowledged_batch(&self, numbers: &[u64]) -> Result<(), Error> {
        let Some(&last) = numbers.last() else {
            return Ok(());
        };
        self.wait_framed_through(last).await?;
        // cladefs: the members' logs of it, and before it, go: the bucket holds them (doc-21).
        if let Some(replicator) = self.replicator() {
            replicator.acknowledged(last);
        }
        // cladefs: writes and syncs the WAL start once for the batch, removes raw files and logs,
        // and moves acknowledged segments to the part cache off the bucket runtime's workers
        // (board 10824).
        crate::upload::blocking(|| {
            let mut start = self.start.with(Mutex::lock);
            let after = {
                let state = self.state()?;
                match state.wal.start_after(numbers) {
                    Some(after) => after,
                    None => bail!(
                        "segment {} is not the next of the write-ahead log",
                        Short(numbers[0])
                    ),
                }
            };
            // cladefs: failing, it leaves the record before it whole, which the cell goes on with:
            // the uploader tries again (TASK-539.12). Before, a full disk failed the cell here.
            // Fast WAL cells treat local disk as cache-only (doc-29, Hotbed review 118): skip
            // writing and syncing the local WAL start file altogether.
            if self.fast_wal().is_none() {
                start.write(&*self.dir, after).context("recording an acknowledgement")?;
            }
            let (leaving, raw) = {
                let mut state = self.state()?;
                let state = &mut *state;
                let mut leaving = Vec::new();
                for &number in numbers {
                    // cladefs: those whose acknowledgements are whole, of a commit spanning
                    // segments all once its last is acknowledged (`Queue::acknowledge`, TASK-338).
                    let whole = state.wal.acknowledge(number).expect("the first, as found");
                    for queued in &whole {
                        state.release_kept(footer_last(&queued.footer));
                    }
                    for queued in whole {
                        let footer = &queued.footer;
                        let key =
                            bucket_cell::layout::segment(self.cell, footer.segment, footer.epoch);
                        let own: Vec<_> = footer.own().collect();
                        let segment = SegmentId { cell: self.cell.id, number: queued.number };
                        let at = (key.as_str(), !queued.restamped);
                        leaving.extend(state.acknowledge(segment, at, &own, queued.data_len()));
                    }
                }
                state.trim_kept(state.epoch);
                // Those no extent's data is in here any more leave now.
                let reaped = state.segments.reap().into_iter();
                leaving.extend(reaped.map(|(number, key)| (segment_path(number), key)));
                // cladefs: and the raw files and logs of the segments framed whose commits are
                // durable, as those acknowledged are, which no flush may come to remove
                // (TASK-165.9).
                let raw = state.segments.raw(self.durability.with(Mutex::lock).local());
                (leaving, raw)
            };
            self.remove_raw(raw);
            let done_logs: Vec<SegmentId> = {
                let mut kept = self.kept_logs.lock().unwrap();
                let rest = kept.split_off(&SegmentId { cell: self.cell.id, number: last + 1 });
                std::mem::replace(&mut *kept, rest).into_iter().collect()
            };
            for segment in done_logs {
                self.recycle_or_remove_log(segment);
            }
            leave(&*self.dir, self.cache.as_deref(), leaving);
            Ok::<_, Error>(())
        })?;
        // cladefs: the logs that went let go of at once, as no flush may follow (board 14305).
        self.let_logs_go(&mut *self.flushing.lock().await);
        Ok(())
    }

    /// cladefs: lets go of the logs `logs` appends to of segments whose logs went (`Segments::raw`,
    /// a spare's or removed), and their replica logs: kept until the next flush before, which an
    /// idle cell never makes, the last spare it renamed held open for good (board 14305). Its
    /// caller holds the flush's lock, which is taken before the cell's state lock.
    fn let_logs_go(&self, logs: &mut Logs<D::File>) {
        let state = self.lock();
        logs.by_segment.retain(|segment, _| state.segments.has_log(*segment));
        logs.replica.retain(|segment, _| state.segments.has_log(*segment));
    }

    /// cladefs: makes the commits ordered so far durable, in order, as few syncs as they need
    /// together: their data, then their records in the logs of the segments they were made in,
    /// then the files they made unreferenced go; and the raw files and logs of the segments framed
    /// whose commits are durable. It waits for no framing (TASK-165.9).
    ///
    /// cladefs: in a span of its own, `store.flush` (`bucket_cell::trace`, TASK-258): a durable
    /// commit's, or of the commits ordered before, which a daemon's turn makes durable.
    pub async fn flush(&self) -> Result<(), Error> {
        let span = trace::Span::start("store.flush");
        span.set_cell(self.cell);
        span.run_async(async {
            // cladefs: timed, phase by phase (TASK-165.9).
            let asked = Instant::now();
            // cladefs: and its waits to run (TASK-336).
            let waits = Waits::start();
            let mut logs = self.flushing.lock().await;
            let mut timed = Flush::default();
            timed.phases.took(flush::WAIT, asked);
            // Its syncs on the disk, holding the thread: handed its other tasks meanwhile.
            let flushed = crate::upload::blocking(|| self.flush_locked(&mut logs, &mut timed));
            drop(logs);
            timed.phases.runnable_us = waits.us();
            timings::flushed(self.cell.id, &timed);
            span.set_count(trace::COMMITS, timed.commits as u64);
            flushed?;
            // cladefs: an ephemeral cell's closed segments covered as they add up, and what they
            // let go of given back (TASK-698.2).
            if self.ephemeral.is_some() {
                self.checkpoint(false).await?;
                let punches = self.state()?.segments.take_punches();
                self.give_back(punches);
            }
            Ok(())
        })
        .await
    }

    /// [`LocalCell::flush`], holding `flushing`, and so `logs`, those of the open segment and of
    /// the segments closed since that the last flush left; timing it in `timed`.
    fn flush_locked(&self, logs: &mut Logs<D::File>, timed: &mut Flush) -> Result<(), Error> {
        // A cell that failed makes nothing more durable.
        drop(self.state()?);
        // cladefs: fast WAL cells treat local disk as cache-only (doc-29): skip all local disk
        // fsyncs/barriers and stage commits straight to the fast WAL sink in commit order.
        if let Some(sink) = self.fast_wal() {
            let batch = self.durability.with(Mutex::lock).take();
            let durable = match batch {
                None => self.durability.with(Mutex::lock).local(),
                Some(Due { last, commits }) => {
                    timed.commits = commits.len();
                    let staged = match self.stage_fast_wal(&commits) {
                        Ok(staged) => staged,
                        Err(error) => {
                            let error = error.context("staging commits for fast WAL");
                            self.lock().fail(&error);
                            return Err(error);
                        }
                    };
                    sink.push(staged, last);
                    self.durability
                        .with(Mutex::lock)
                        .finish(last, self.dir.host().map(Arc::as_ref));
                    let entries =
                        commits.iter().flat_map(|pending| &pending.entries).copied().collect();
                    self.lock().segments.entries_synced(&entries);
                    let mut leaving = Vec::new();
                    for pending in commits {
                        self.remove(pending.doomed);
                        leaving.extend(pending.leaving);
                    }
                    leave(&*self.dir, self.cache.as_deref(), leaving);
                    last
                }
            };
            let raw = self.state()?.segments.raw(durable);
            self.remove_raw(raw);
            self.let_logs_go(logs);
            return Ok(());
        }
        // cladefs: its durability deferred, all of it made durable first, before anything of the
        // flush: no commit is durable before its cell is (board 7420). Failing, the cell fails, as
        // any flush's failure fails it.
        if let Err(err) = self.dir.settle() {
            let error = Error::new(err).context("making the cell durable, its durability deferred");
            self.lock().fail(&error);
            return Err(error);
        }
        let batch = self.durability.with(Mutex::lock).take();
        let durable = match batch {
            None => self.durability.with(Mutex::lock).local(),
            Some(Due { last, mut commits }) => {
                let records = match self.make_durable(logs, &mut commits, timed, last) {
                    Ok(records) => records,
                    Err(error) => {
                        let error = error.context("committing the extent store");
                        self.lock().fail(&error);
                        return Err(error);
                    }
                };
                self.durability.with(Mutex::lock).finish(last, self.dir.host().map(Arc::as_ref));
                drop(records);
                // cladefs: the entries of the segments opened its syncs made durable: no making of
                // them durable syncs them again (TASK-369).
                let entries =
                    commits.iter().flat_map(|pending| &pending.entries).copied().collect();
                self.lock().segments.entries_synced(&entries);
                // The files the commits let go of go, once they are durable; cladefs: those of the
                // bucket's, into the host's cache (TASK-165.8).
                let started = Instant::now();
                let mut leaving = Vec::new();
                for pending in commits {
                    self.remove(pending.doomed);
                    leaving.extend(pending.leaving);
                }
                leave(&*self.dir, self.cache.as_deref(), leaving);
                timed.phases.took(flush::GONE, started);
                last
            }
        };
        // cladefs: and the raw files and logs of the segments framed, whose footers name commits
        // durable now (TASK-153.8).
        let started = Instant::now();
        let raw = self.state()?.segments.raw(durable);
        self.remove_raw(raw);
        self.let_logs_go(logs);
        timed.phases.took(flush::RAW, started);
        Ok(())
    }

    /// cladefs: of a replicated cell's writer, sends the commits of a flush, local now, through
    /// `last`, `records` the records it appended, to the members of its configuration (doc-21,
    /// TASK-537.3.3): each segment's records held whole, in its replica log
    /// ([`ReplicaLog`]), started with its local log's header, and the extents they stored on their
    /// own, in its own log ([`own_frame`]). Nothing, of a cell not replicating.
    fn replicate(
        &self,
        logs: &mut Logs<D::File>,
        commits: &[Pending<D::File>],
        records: &[(SegmentId, LogRecord)],
        last: u64,
    ) -> Result<(), Error> {
        let Some(replicator) = self.replicator.lock().unwrap().clone() else { return Ok(()) };
        // Held whole, as a replica's records are: the flush's own, if it held them; else read back.
        let read_back;
        let held: &[(SegmentId, LogRecord)] =
            match records.iter().all(|(_, record)| matches!(record.runs, Runs::Held(_))) {
                true => records,
                false => {
                    read_back = self.records(commits, Runs::Held(Default::default()))?;
                    &read_back
                }
            };
        let mut flushed = Flushed { epoch: last, appends: Vec::new() };
        let mut at = 0;
        while at < held.len() {
            let segment = held[at].0;
            let run = held[at..].iter().take_while(|(of, _)| *of == segment).count();
            let records = &held[at..at + run];
            at += run;
            if logs.unreplicated.contains(&segment) {
                replicator.raise_base(last);
                continue;
            }
            let mut started = Vec::new();
            if !logs.replica.contains_key(&segment) {
                let log = self.segment_log(logs, segment)?;
                let header = log.header().context("reading a log's header")?;
                let (bytes, replica) = ReplicaLog::new(&header, segment, log.encryption().clone())?;
                started = bytes;
                logs.replica.insert(segment, (replica, 0));
            }
            let (replica, own_end) = logs.replica.get_mut(&segment).unwrap();
            let offset = replica.end() - if started.is_empty() { 0 } else { started.len() as u64 };
            started.extend(replica.append(records.iter().map(|(_, record)| record)));
            flushed.appends.push((log_of(segment.number), offset, Arc::from(started)));
            let mut own = Vec::new();
            for (_, record) in records {
                for &(sequence, _) in &record.contents.own {
                    let extent = storage_device::ExtentId { cell: self.cell.id, sequence };
                    let path = extent_path(extent);
                    let file =
                        self.dir.open(&path, true).with_context(|| format!("opening {path}"))?;
                    let bytes = crate::framed::read_whole(&file)
                        .with_context(|| format!("reading {path}"))?;
                    own.extend(own_frame(sequence, &bytes));
                }
            }
            if !own.is_empty() {
                let len = own.len() as u64;
                flushed.appends.push((own_of(segment.number), *own_end, Arc::from(own)));
                *own_end += len;
            }
        }
        replicator.flushed(flushed);
        Ok(())
    }

    /// cladefs: the log of `segment` a flush appends to: kept while the segment has one, made at
    /// its first records.
    fn segment_log<'a>(
        &self,
        logs: &'a mut Logs<D::File>,
        segment: SegmentId,
    ) -> Result<&'a mut SegmentLog<D::File>, Error> {
        Ok(match logs.by_segment.entry(segment) {
            std::collections::btree_map::Entry::Occupied(log) => log.into_mut(),
            std::collections::btree_map::Entry::Vacant(vacant) => {
                let log = self.lock().segments.log_file(&*self.dir, segment)?;
                vacant.insert(SegmentLog::new(log.file, log.salt, log.encryption))
            }
        })
    }

    /// cladefs: sends a flush's `records`, as it appends them, before its syncs, to the members of
    /// a replicated cell's configuration ([`LocalCell::replicate`]): their round trips overlap its
    /// own sync (doc-21), and no waiter is told a commit replicated before its own copy is local,
    /// as every waiter flushes first. A failure to read what it sends breaks the replicator, whose
    /// commits then wait for the bucket, and fails nothing else: the flush is the cell's own
    /// (Codex's review of TASK-537.3.3).
    fn send(
        &self,
        logs: &mut Logs<D::File>,
        commits: &[Pending<D::File>],
        records: &[(SegmentId, LogRecord)],
        last: u64,
    ) {
        if let Err(err) = self.replicate(logs, commits, records, last)
            && let Some(replicator) = self.replicator()
        {
            replicator.break_with(format!("{err:#}"));
        }
    }

    /// cladefs: has `replicator` send the commits of every flush from now on to the members of the
    /// cell's configuration (doc-21, TASK-537.3.3): as the cell opens after its writer's taking,
    /// every segment before closed, so that what it sends of each segment starts at its log's
    /// header. The epoch of the cell's last commit, local, is what none of them holds
    /// ([`Replicator::base`]).
    pub async fn replicate_to(&self, replicator: Arc<Replicator>) {
        let mut logs = self.flushing.lock().await;
        let started = logs.by_segment.iter().filter(|(_, log)| log.holds_records());
        logs.unreplicated = started.map(|(&segment, _)| segment).collect();
        *self.replicator.lock().unwrap() = Some(replicator);
    }

    /// cladefs: replicates no more, its replicator stopped and any logs retained for lagging peers
    /// released: its cell's head names a configuration of f = 0, as it asks (TASK-537.3.5).
    pub async fn unreplicate(&self) {
        let mut logs = self.flushing.lock().await;
        logs.replica.clear();
        logs.unreplicated.clear();
        if let Some(replicator) = self.replicator.lock().unwrap().take() {
            replicator.stop();
        }
        let kept: Vec<SegmentId> =
            std::mem::take(&mut *self.kept_logs.lock().unwrap()).into_iter().collect();
        for segment in kept {
            self.recycle_or_remove_log(segment);
        }
    }

    /// cladefs: a read-only [`crate::replicate::LogSource`] backed by this cell's existing
    /// `logs/`, `open-segments/` or `segments/`, and `extents/` for lagging/replacement peer
    /// catch-up when bytes are evicted from the replicator's hot window (board 6902).
    pub fn replica_source(&self) -> Arc<dyn crate::replicate::LogSource> {
        Arc::new(CellLogSource { dir: self.dir.clone(), cell: self.cell, keys: self.keys.clone() })
    }

    /// cladefs: its replicator, if it replicates.
    pub fn replicator(&self) -> Option<Arc<Replicator>> {
        self.replicator.lock().unwrap().clone()
    }

    /// cladefs: attaches `sink` so commits are staged for the fast WAL with zero local disk
    /// fsyncs (doc-29).
    pub fn fast_wal_to(&self, sink: Arc<crate::fast_wal::FastWalSink>) {
        self.cache_only.store(true, std::sync::atomic::Ordering::Release);
        *self.fast_wal.lock().unwrap() = Some(sink);
    }

    /// cladefs: its fast WAL sink, if the cell uses fast WAL commit mode (doc-29).
    pub fn fast_wal(&self) -> Option<Arc<crate::fast_wal::FastWalSink>> {
        self.fast_wal.lock().unwrap().clone()
    }

    /// cladefs: reads plaintext bytes of a sealed run at `(offset, len)` in `segment` (whether
    /// still open/raw or already closed/framed) for staging into the fast WAL (doc-29).
    fn read_run_plaintext(
        &self,
        segment: SegmentId,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, Error> {
        let source = self.lock().segments.source(&*self.dir, segment)?;
        match source {
            Source::Raw(raw) => {
                let mut buf = vec![0u8; len as usize];
                raw.read_at(&mut buf, offset)?;
                Ok(buf)
            }
            Source::Framed(framed) => {
                let bytes = framed.parts.read(offset..offset + len, |range| {
                    crate::framed::read_aligned(
                        &*framed.file,
                        range.start,
                        (range.end - range.start) as usize,
                    )
                    .map(|aligned| aligned.as_slice().to_vec())
                    .map_err(|err| {
                        extent_frame::Error::Corrupt(extent_frame::Corrupt(err.to_string()))
                    })
                })?;
                Ok(bytes)
            }
        }
    }

    /// cladefs: extracts plaintext inline extents and pre-framed large own-extents for a batch of
    /// pending commits to hand to [`crate::fast_wal::FastWalSink`] (doc-29).
    fn stage_fast_wal(
        &self,
        commits: &[Pending<D::File>],
    ) -> Result<Vec<crate::fast_wal::StagedCommit>, Error> {
        let mut staged = Vec::with_capacity(commits.len());
        for pending in commits {
            let mut inline_extents = Vec::new();
            let mut large_extents = Vec::new();
            let mut frees = Vec::new();
            let mut root = None;
            let mut capacity = None;
            let mut next_sequence = 0u64;
            let mut versions = Vec::new();
            let mut tag = None;

            for (segment, bucket_segment, contents) in &pending.records {
                if let Some(&commit_tag) = contents.commits.last() {
                    tag = Some(commit_tag);
                }
                for &(extent, offset, len) in contents.runs() {
                    let bytes = self.read_run_plaintext(*segment, offset, len)?;
                    inline_extents.push((extent, bytes));
                }
                for &(sequence, logical_len) in &contents.own {
                    let extent = ExtentId { cell: self.cell.id, sequence };
                    let path = extent_path(extent);
                    let file =
                        self.dir.open(&path, true).with_context(|| format!("opening {path}"))?;
                    // Validate frame structure and every part's stored CRC before staging for
                    // Fast WAL pre-upload so damaged own-extent data can never be acknowledged
                    // (board 12295).
                    let framed_bytes = crate::framed::read_object(&file)
                        .with_context(|| format!("reading {path}"))?;
                    large_extents.push(crate::fast_wal::StagedLargeExtent {
                        sequence,
                        logical_len,
                        bucket_segment: *bucket_segment,
                        framed_bytes,
                    });
                }
                frees.extend_from_slice(&contents.frees);
                if contents.root.is_some() {
                    root = contents.root.clone();
                }
                if contents.capacity.is_some() {
                    capacity = contents.capacity;
                }
                next_sequence = next_sequence.max(contents.next_sequence);
                versions.extend_from_slice(&contents.versions);
            }

            // Every commit's records end with one naming it (`Segments::record`).
            let Some(tag) = tag else {
                bail!("commit {} has no record naming it", pending.epoch);
            };
            staged.push(crate::fast_wal::StagedCommit {
                epoch: pending.epoch,
                tag,
                inline_extents,
                large_extents,
                frees,
                root,
                capacity,
                next_sequence,
                versions,
            });
        }
        Ok(staged)
    }

    /// cladefs: removes the raw files and logs of `raw`, segments framed, whose framed files, and
    /// their entries, an opening reads their commits from now: their framings made them durable
    /// before they ended (`segments::make_durable`), so that it syncs nothing, a flush's syncs
    /// waiting behind the host's disk enough (TASK-310). A log the cell wants as a spare is
    /// renamed one, rather than removed ([`spare_path`], TASK-301): the log of a segment to come,
    /// written in place, its blocks there.
    fn remove_raw(&self, raw: Vec<SegmentId>) {
        self.remove(raw.iter().map(|segment| open_segment_path(*segment)));
        let replicating = self.replicator().is_some();
        for segment in raw {
            if replicating {
                self.kept_logs.lock().unwrap().insert(segment);
                continue;
            }
            self.recycle_or_remove_log(segment);
        }
    }

    /// cladefs: keeps `segment`'s log as a spare if wanted, else removes it.
    fn recycle_or_remove_log(&self, segment: SegmentId) {
        let (log, spare) = (log_path(segment), spare_path(segment));
        // A log without a header of its own holds records no salt tells from a later one's.
        if self.lock().segments.wants_spare()
            && let Ok(file) = self.dir.open(&log, false)
            && let mut header = [0; LOG_HEADER]
            && file.read_at(&mut header, 0).is_ok()
            && log_salt(&header, segment).is_some()
        {
            match self.dir.rename(&log, &spare) {
                Ok(()) => {
                    self.lock().segments.spare(segment);
                    return;
                }
                Err(err) => log::warn!("extent store: keeping {log} as a spare: {err}"),
            }
        }
        self.remove([log]);
    }

    /// cladefs: creates lazily, off the cell's state lock, the log files for any segments named
    /// by `commits` not yet in `logs.by_segment` (board 11709, board 1770). Adds `LOGS` to `syncs`
    /// when any log file was created and returns those segments so [`Self::make_durable`] marks
    /// their directory entries durable after `dir.barrier()`.
    fn ensure_logs(
        &self,
        logs: &mut Logs<D::File>,
        commits: &[Pending<D::File>],
        syncs: &mut Syncs<D::File>,
    ) -> Result<Vec<SegmentId>, Error> {
        let mut created = Vec::new();
        for &(segment, _, _) in commits.iter().flat_map(|pending| &pending.records) {
            if logs.by_segment.contains_key(&segment) {
                continue;
            }
            let init = self.lock().segments.take_log_init(segment);
            match init {
                LogInit::Existing(log) => {
                    logs.by_segment
                        .insert(segment, SegmentLog::new(log.file, log.salt, log.encryption));
                }
                LogInit::Reopen { salt, encryption } => {
                    let file = Arc::new(self.dir.open(&log_path(segment), false)?);
                    logs.by_segment.insert(segment, SegmentLog::new(file, salt, encryption));
                }
                LogInit::Create { salt, encryption, spare } => {
                    let file =
                        segments::create_log_file(&*self.dir, segment, salt, &encryption, spare)?;
                    self.lock().segments.install_log(segment, Arc::clone(&file));
                    logs.by_segment.insert(segment, SegmentLog::new(file, salt, encryption));
                    created.push(segment);
                }
                LogInit::Missing => {
                    let log = self.lock().segments.log_file(&*self.dir, segment)?;
                    logs.by_segment
                        .insert(segment, SegmentLog::new(log.file, log.salt, log.encryption));
                }
            }
        }
        if !created.is_empty() {
            syncs.dir(cell_dir(LOGS, self.cell.id));
        }
        Ok(created)
    }

    /// Makes `commits`, a flush's, durable, in order: their data, and their records in the logs of
    /// the segments they were made in, open, or closed since; but for the files they made
    /// unreferenced. cladefs: it waits for no framing, those of segments the uploader closes
    /// between commits among them, which held up the turns a daemon makes its guests' commits
    /// durable in (TASK-165.9): an opening closes a segment whose framed file isn't whole again
    /// from its log. cladefs: in one sync's time, not two (TASK-301, [`LogRecord`]): commits that
    /// sealed little (`Options::held_most`) hold their runs in their records, and only the logs are
    /// synced, as a database's write-ahead log is; others' records check their runs, synced at once
    /// with them. Data first, then records, if the commits stored extents on their own, which no
    /// record checks. At once, the records go first in a directory that does one thing at a time,
    /// as crash tests' do: a crash between keeps records whose data it lost.
    fn make_durable(
        &self,
        logs: &mut Logs<D::File>,
        commits: &mut [Pending<D::File>],
        timed: &mut Flush,
        last: u64,
    ) -> Result<Vec<(SegmentId, LogRecord)>, Error> {
        let started = Instant::now();
        timed.commits = commits.len();
        let mut syncs = Syncs::default();
        for pending in commits.iter_mut() {
            syncs.extend(std::mem::take(&mut pending.syncs));
        }
        let created_logs =
            self.ensure_logs(logs, commits, &mut syncs).context("appending commits' records")?;
        let sealed: u64 = commits
            .iter()
            .flat_map(|pending| &pending.records)
            .map(|(_, _, contents)| LogRecord::run_bytes(contents))
            .sum();
        let records = if syncs.holds_own() || sealed > CHECKED_MOST.max(self.options.held_most) {
            syncs.extend(std::mem::take(&mut logs.unsynced));
            timed.synced = syncs.sync_files(&*self.dir)?;
            let synced = timed.phases.took(flush::DATA, started);
            syncs.sync_dirs(&*self.dir)?;
            let synced = timed.phases.took(flush::DIRS, synced);
            let records = self.records(commits, Runs::Synced)?;
            self.send(logs, commits, &records, last);
            self.append_records(logs, &records)?;
            self.dir.barrier()?;
            timed.phases.took(flush::LOGS, synced);
            records
        } else {
            let hold = sealed <= self.options.held_most;
            // The raw files it leaves unsynced, the next flush syncing data syncs.
            match hold {
                true => logs.unsynced.carry(syncs.take_cached()),
                false => syncs.extend(std::mem::take(&mut logs.unsynced)),
            }
            let runs =
                if hold { Runs::Held(Default::default()) } else { Runs::Checked(Vec::new()) };
            let records =
                self.records(commits, runs).context("reading back the runs commits sealed")?;
            self.send(logs, commits, &records, last);
            let recorded = timed.phases.took(flush::DATA, started);
            // What is synced beside the records: the directories alone, if they hold the runs.
            let sync_data = || -> Result<FileSyncs, Error> {
                let synced =
                    if hold { FileSyncs::default() } else { syncs.sync_files(&*self.dir)? };
                syncs.sync_dirs(&*self.dir)?;
                Ok(synced)
            };
            // Nothing beside the records, as a commit's when the segment holds the runs: no thread.
            timed.synced = match syncs.holds_any(!hold) {
                true => self.at_once(|| self.append_records(logs, &records), sync_data)?,
                false => self.append_records(logs, &records).map(|()| FileSyncs::default())?,
            };
            self.dir.barrier()?;
            timed.phases.took(flush::LOGS, recorded);
            records
        };
        // cladefs: records and data are durable in local logs/segments now, so any hot chunks sent
        // before append may be evicted past HOT_BYTES and served via CellLogSource (board 6902).
        if let Some(replicator) = self.replicator() {
            replicator.persisted();
        }
        // cladefs: the logs of segments whose logs went: no commit made in them is still to come.
        // Those of segments closed since stay, with where their records end, which their files'
        // sizes don't say of spares (TASK-301): a flush may yet append commits made in them before
        // they closed, as one taking those ordered as the uploader closed the segment does.
        let mut state = self.lock();
        if !created_logs.is_empty() {
            state.segments.log_entries_synced(&created_logs);
        }
        logs.by_segment.retain(|segment, _| state.segments.has_log(*segment));
        logs.replica.retain(|segment, _| state.segments.has_log(*segment));
        Ok(records)
    }

    /// cladefs: runs `first`, and `then`, at once, `then` on a thread of the pool's, if one is
    /// free, as one of the helpers every flush of the process shares (`durability::HELPERS`); or in
    /// turn, on this thread, if none is, or the directory does one thing at a time.
    fn at_once<T: Send + 'static>(
        &self,
        first: impl FnOnce() -> Result<(), Error>,
        then: impl Fn() -> Result<T, Error> + Sync,
    ) -> Result<T, Error> {
        let helper = crate::durability::Helpers::take(1);
        if !self.dir.concurrent() || helper.count() == 0 {
            first()?;
            return then();
        }
        // `then`, run once: by the helper, if it begins first, or after `first`.
        let taken = std::sync::atomic::AtomicBool::new(false);
        let once = || (!taken.swap(true, std::sync::atomic::Ordering::AcqRel)).then(&then);
        let ((done, mine), theirs) = crate::pool::help(1, &once, || (first(), once()));
        let other = mine.or(theirs.into_iter().flatten().next()).expect("`then`, run once");
        done.and(other)
    }

    /// cladefs: the records of `commits`, a flush's, by the segments they were made in, in order:
    /// each the commit's contents, and what `runs` says of the runs it sealed ([`LogRecord`]);
    /// cladefs: of a commit spanning segments, one in each, in their order (TASK-92.41).
    fn records(
        &self,
        commits: &[Pending<D::File>],
        runs: Runs,
    ) -> Result<Vec<(SegmentId, LogRecord)>, Error> {
        let mut raw: Option<(SegmentId, Arc<D::File>)> = None;
        let mut records = Vec::with_capacity(commits.len());
        let spanned = commits.iter().flat_map(|pending| &pending.records);
        for (segment, _, contents) in spanned {
            let record = match runs {
                Runs::Synced => LogRecord { contents: contents.clone(), runs: Runs::Synced },
                Runs::Checked(_) | Runs::Held(_) => {
                    let file = match &raw {
                        Some((at, file)) if at == segment => file.clone(),
                        _ => {
                            let file = self.lock().segments.raw_file(&*self.dir, *segment)?;
                            raw = Some((*segment, file.clone()));
                            file
                        }
                    };
                    LogRecord::of(contents.clone(), &*file, matches!(runs, Runs::Held(_)))?
                }
            };
            records.push((*segment, record));
        }
        Ok(records)
    }

    /// cladefs: appends `records` to the logs of their segments, each segment's at once, and syncs
    /// them.
    fn append_records(
        &self,
        logs: &mut Logs<D::File>,
        records: &[(SegmentId, LogRecord)],
    ) -> Result<(), Error> {
        let mut at = 0;
        while at < records.len() {
            let segment = records[at].0;
            let run = records[at..].iter().take_while(|(of, _)| *of == segment).count();
            let log = self.segment_log(logs, segment)?;
            // Salted as its log's (`wal::LOG_MAGIC`); cladefs: MACed (TASK-411.5).
            let appended = records[at..at + run].iter().map(|(_, record)| record);
            log.append(appended).context("appending commits' records")?;
            at += run;
        }
        Ok(())
    }

    /// cladefs: waits, holding no lock, for the framings of `segments` to write their framed files,
    /// or end, each of which ends itself ([`LocalCell::frame_behind`]): fails if one failed. Not
    /// for them to make the framed files durable, which the uploader waits for no more (TASK-384).
    async fn wait_for_writings(
        &self,
        segments: impl IntoIterator<Item = SegmentId>,
    ) -> Result<(), Error> {
        let segments: Vec<SegmentId> = segments.into_iter().collect();
        let state = self
            .lock_when(|state| !(segments.iter().any(|&number| state.segments.is_writing(number))))
            .await;
        match segments.iter().find_map(|&number| state.segments.framing_failure(number)) {
            Some(error) => bail!("framing a segment: {error}"),
            None => Ok(()),
        }
    }

    /// cladefs: whether commits ordered are not durable yet, which [`LocalCell::flush`] makes
    /// durable: waiting for a flush to take them, or taken by one in flight, which a flush waits
    /// for. Those taken counted not, a daemon's turn finding none took commits for durable that
    /// weren't, a flush of the uploader's in flight (TASK-165.9).
    pub fn flush_pending(&self) -> bool {
        self.durability.with(Mutex::lock).pending()
    }

    /// Reads `buf.len()` bytes at `offset` in `extent`, as `locate` finds them.
    fn read_into(&self, extent: ExtentId, offset: u64, buf: &mut [u8]) -> Result<(), Error> {
        self.read_into_entry(extent, offset, buf, Reader::Own)
    }

    fn read_into_entry(
        &self,
        extent: ExtentId,
        offset: u64,
        buf: &mut [u8],
        reader: Reader,
    ) -> Result<(), Error> {
        let parts = self.locate_entry(extent, offset, buf.len() as u64, reader)?;
        self.read_parts(parts, buf)
    }

    /// Reads `buf.len()` bytes at `offset` in `extent`. Bytes never written read as zeros.
    pub fn read(&self, extent: ExtentId, offset: u64, buf: &mut [u8]) -> Result<(), Error> {
        ensure!(!self.keys.is_revoked(), "cell keys revoked");
        self.read_into(extent, offset, buf)
    }

    /// Writes `data` at `offset` in `extent` (see `begin_write`).
    pub async fn write(&self, extent: ExtentId, offset: u64, data: &[u8]) -> Result<(), Error> {
        let fill = |at: u64, buf: &mut [u8]| buf.copy_from_slice(&data[at as usize..][..buf.len()]);
        self.write_with(extent, offset, data.len() as u64, data.len(), fill).await
    }

    /// cladefs: writes `len` bytes at `offset` in `extent`, as [`LocalCell::write`] does, one
    /// write, but `piece` bytes at a time at most, each of which `fill(at, buf)` fills with the
    /// bytes at `at` of them: so that what a write holds of memory is a piece, whatever its length,
    /// which a VM process names (TASK-149.17). Its pieces are whole sectors, one at least, as what
    /// reaches the open segment is ([`SECTOR`], TASK-411.8).
    pub async fn write_with(
        &self,
        extent: ExtentId,
        offset: u64,
        len: u64,
        piece: usize,
        mut fill: impl FnMut(u64, &mut [u8]),
    ) -> Result<(), Error> {
        let write = self.begin_write(extent, offset, len).await?;
        let piece = (piece as u64 / SECTOR).max(1) * SECTOR;
        crate::upload::blocking(|| -> Result<(), Error> {
            let mut aligned = Aligned::new(len.min(piece) as usize);
            let mut at = 0;
            while at < len {
                let buf = &mut aligned.as_mut_slice()[..(len - at).min(piece) as usize];
                fill(at, buf);
                write.write(buf, at)?;
                at += buf.len() as u64;
            }
            write.finish();
            Ok(())
        })
    }

    /// Sets the most bytes the cell's extents may hold, durably: cladefs: a commit of its own,
    /// which seals nothing, and takes none of what changed since the last commit, the next's, so
    /// that the extents a guest has open stay open (TASK-165.9); ordered with the others, as its
    /// segment's footer says (TASK-165.3). It may grow it, or shrink it no lower than what the
    /// extents hold, with what writes in progress may add ([`TooSmall`]).
    pub async fn set_capacity(&self, capacity: u64) -> Result<(), Error> {
        let committing = self.committing.lock().await;
        {
            let mut state = self.writable()?;
            let block_size = self.block_size as u64;
            ensure!(capacity >= block_size, "a capacity of {capacity} is less than a block");
            state.space.check_capacity(capacity)?;
            let committed = crate::upload::blocking(|| {
                state.failing(|state| {
                    state.space.set_capacity(capacity);
                    state.uncommitted.take_capacity();
                    let epoch = state.epoch + 1;
                    state.trim_kept(epoch);
                    let (changed, next_sequence) = ((None, Some(capacity)), state.next_sequence);
                    let records = state.segments.record(
                        epoch,
                        &[],
                        changed,
                        &state.extents,
                        next_sequence,
                    )?;
                    self.committed(state, epoch, records)
                })
            });
            committed.context("committing the capacity")?;
        }
        drop(committing);
        self.flush().await
    }

    /// cladefs: the bytes of its write-ahead log not yet acknowledged in the bucket, those the open
    /// segment's commits sealed included: what the uploader's lag bounds (TASK-165.9).
    pub fn wal_bytes(&self) -> Result<u64, Error> {
        let state = self.state()?;
        Ok(state.wal.bytes() + state.segments.open_bytes())
    }

    /// The root as of the last commit: empty if none set one.
    pub fn root(&self) -> Result<Vec<u8>, Error> {
        Ok(self.state()?.root.clone())
    }

    pub fn info(&self) -> Result<Info, Error> {
        Ok(self.state()?.info())
    }

    /// cladefs: the cell's own segments in the bucket that are sparse, but those `pinned` says a
    /// kept state reads, most dead bytes first: what its writer relocates out of (doc-27,
    /// Choosing what to relocate).
    /// Their live bytes counted the first time it is asked, walking every extent the cell holds.
    pub fn sparse(&self, pinned: impl Fn(&str) -> bool) -> Result<Vec<Sparse>, Error> {
        Ok(self.state()?.extents.sparse(pinned))
    }

    /// cladefs: how many bytes a relocation now would take ([`LocalCell::relocate`]): what the open
    /// segment has room for, none while the guest has filled a segment since its last commit or a
    /// run of an extent it has open ends where a copy would go, as a relocation then relocates
    /// nothing: what a round reads no more than (the verifying review of d33a6c360).
    pub fn relocation_room(&self) -> Result<u64, Error> {
        let state = self.state()?;
        if state.segments.has_full() || self.run_goes_on(&state) {
            return Ok(0);
        }
        let open = state.segments.len(state.segments.open_segment());
        Ok(self.options.segment_limit().saturating_sub(open))
    }

    /// cladefs: the extents the cell holds in each of its segments in the bucket `keys` names,
    /// where they are, by offset: what its writer reads to relocate out of them (doc-27).
    pub fn held_in(
        &self,
        keys: &HashSet<String>,
    ) -> Result<HashMap<String, Vec<(ExtentId, Location)>>, Error> {
        Ok(self.state()?.extents.held_in(keys))
    }

    pub fn usage(&self) -> Result<Usage, Error> {
        let state = self.state()?;
        let segments = state.segments.usage();
        Ok(Usage {
            own_bytes: (state.extents.own())
                .filter(|(_, entry)| entry.data == Data::Own)
                .map(|(_, entry)| entry.len)
                .sum(),
            disk_bytes: segments.disk_bytes + state.own_files.bytes(),
            kept_bytes: state.kept.bytes(),
            ..segments
        })
    }

    /// `extent`'s length, and whether it is open (written since the last commit), if the cell
    /// holds it.
    pub fn extent(&self, extent: ExtentId) -> Result<Option<(u64, bool)>, Error> {
        let state = self.state()?;
        let life = state.life(extent);
        let open = matches!(life, Life::Open(_));
        Ok(life.held().map(|entry| (entry.len(), open)))
    }

    /// cladefs: whether an extent is open: written since the last commit, which seals it.
    pub fn has_open(&self) -> Result<bool, Error> {
        Ok(self.state()?.uncommitted.has_open())
    }

    /// cladefs: whether its map of the extents the bucket holds of its cell, its bucket's layer, is
    /// `other`'s, shared, one map, not two alike (TASK-453).
    #[cfg(feature = "testing")]
    pub fn shares_extents_with<E: Dir>(&self, other: &LocalCell<E>) -> Result<bool, Error> {
        Ok(self.state()?.extents.shares_bucket_with(&other.state()?.extents))
    }

    /// The extents the cell holds, with their lengths, in order.
    pub fn extents(&self) -> Result<Vec<(ExtentId, u64)>, Error> {
        let state = self.state()?;
        let mut extents: Vec<_> = state.extents.lens().collect();
        extents.sort();
        Ok(extents)
    }

    /// The number of the last commit.
    pub fn last_commit(&self) -> Result<u64, Error> {
        Ok(self.state()?.epoch)
    }
}

/// The bytes of an extent's data framed at once in its own file ([`LocalCell::write_own`],
/// [`LocalCell::seal_own`]).
pub(crate) const OWN_CHUNK: u64 = 16 * extent_frame::PART;

/// Removes the files of extents stored on their own of `cell`'s in `dir` that neither `extents`
/// holds nor a footer of its write-ahead log names, `named`: what commits that weren't durable
/// wrote, or that were to go once the commits that let them go were; and the raw files of extents
/// open, which no commit made durable refers to, and which are framed once sealed. Its write-ahead
/// log's recovery removed what of segments it holds not ([`crate::wal::recover`]).
fn remove_unheld<D: Dir>(
    dir: &D,
    cell: u64,
    extents: &Extents,
    named: &BTreeSet<ExtentId>,
) -> Result<(), Error> {
    let held = files_of(extents.own());
    for (kind, own) in [(EXTENTS, true), (OPEN_EXTENTS, false)] {
        for number in list_numbers(dir, &cell_dir(kind, cell))? {
            let extent = ExtentId::new(cell, number);
            if own && (held.contains(&FileKey::Own(extent)) || named.contains(&extent)) {
                continue;
            }
            let path = numbered_path(kind, cell, number);
            dir.remove(&path).with_context(|| format!("removing {path}"))?;
        }
    }
    dir.barrier()?;
    Ok(())
}

// cladefs: commits only ordered are made durable before the cell goes, which would otherwise
// lose them though nothing crashed.
impl<D: Dir> Drop for LocalCell<D> {
    fn drop(&mut self) {
        // cladefs: dropped unclosed, closed here: at once, as a close that waits for nothing goes,
        // as one does where the cell's work runs on its caller's thread (madsim's); or waiting on
        // the dropping thread for what it waits on, an outermost entry's (board 2944).
        let mut closing = std::pin::pin!(self.close());
        let waker = std::task::Waker::noop();
        let closed = match closing.as_mut().poll(&mut std::task::Context::from_waker(waker)) {
            std::task::Poll::Ready(closed) => closed,
            std::task::Poll::Pending => {
                crate::blocks!("a cell dropped unclosed, its close waiting");
                crate::sync::block_on(closing)
            }
        };
        if let Err(err) = closed {
            log::error!("extent store: making the last commits durable: {err:#}");
        }
        // cladefs: what it had open, uncommitted, no commit will seal (doc-22); clear only if
        // this cell still holds its directory claim so a closed cell dropped after reopen never
        // zeroes the reopened cell's open reserve (board 5442).
        let opened = self.opened.with(Mutex::lock);
        if opened.is_some()
            && let Some(footprint) = self.dir.footprint()
        {
            footprint.set_open(0);
        }
    }
}

/// cladefs: the bytes of `start..end` of an extent that `pieces` hold: what a write there writes
/// over, in segments (TASK-79.2, TASK-160.16).
pub(crate) fn covered(pieces: &[Piece], start: u64, end: u64) -> u64 {
    let spans = paint(pieces, start, end).into_iter();
    spans.filter(|(_, _, source)| source.is_some()).map(|(start, end, _)| end - start).sum()
}

/// A part of an extent: its start and end, and the segment and offset of its data, or `None` where
/// nothing was written.
type Span = (u64, u64, Option<(SegmentId, u64)>);

/// The parts of `start..end` of an extent that `pieces` hold, later pieces over earlier ones.
fn paint(pieces: &[Piece], start: u64, end: u64) -> Vec<Span> {
    let mut parts = if start < end { vec![(start, end, None)] } else { Vec::new() };
    for piece in pieces {
        let (from, to) =
            (piece.extent_offset.max(start), (piece.extent_offset + piece.len).min(end));
        if from >= to {
            continue;
        }
        let mut painted = Vec::with_capacity(parts.len() + 2);
        for (a, b, source) in parts {
            if b <= from || a >= to {
                painted.push((a, b, source));
                continue;
            }
            if a < from {
                painted.push((a, from, source));
            }
            let (s, e) = (a.max(from), b.min(to));
            painted.push((s, e, Some((piece.segment, piece.offset + (s - piece.extent_offset)))));
            if b > to {
                painted.push((to, b, source.map(|(number, at)| (number, at + (to - a)))));
            }
        }
        parts = painted;
    }
    // Neighbours whose data is contiguous are one part.
    let mut merged: Vec<Span> = Vec::with_capacity(parts.len());
    for (start, end, source) in parts {
        if let Some((last_start, last_end, last_source)) = merged.last_mut() {
            let contiguous = match (*last_source, source) {
                (None, None) => true,
                (Some((number, at)), Some(next)) => {
                    next == (number, at + (*last_end - *last_start))
                }
                _ => false,
            };
            if contiguous {
                *last_end = end;
                continue;
            }
        }
        merged.push((start, end, source));
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(feature = "shuttle"))]
    #[test]
    fn failed_relocation_does_not_complete_a_part_for_looking_ahead() {
        let _scenario = fail::FailScenario::setup();
        let description = Description {
            block_size: 4096,
            capacity: 2 * extent_frame::PART,
            cell: bucket_cell::layout::test_cell(1),
        };
        let cell =
            LocalCell::create(crate::MemDir::new(), description, Options::default()).unwrap();
        let mut state = cell.lock();
        fail::cfg("relocate::write", "1*return").unwrap();
        let error = cell
            .copy_relocated(&mut state, &Aligned::new(extent_frame::PART as usize))
            .unwrap_err();
        assert_eq!(error.to_string(), "failpoint relocate::write");
        assert!(state.segments.take_seeing().is_empty());
    }

    /// The cell's own tests under Shuttle (the `shuttle` feature), which only they can run:
    /// `cargo nextest run -p local_cell --features shuttle --lib shuttle_tests::`.
    #[cfg(feature = "shuttle")]
    mod shuttle_tests {
        use super::*;

        /// cladefs: a segment a commit closes while the host refuses threads is framed on the
        /// committing thread, rather than panic, as `thread::spawn` does, through the locks the
        /// commit holds, which every later call would find poisoned: the commit goes on, and its
        /// flush, which syncs on its own thread too, and the cell's next commits; and the cell,
        /// opened again, holds what they committed (TASK-79.80). Under Shuttle, the only build in
        /// which a test can have the host refuse threads (`refuse_threads`), on random schedules:
        /// Shuttle's PCT scheduler fails a test that exercises no concurrency, as this one, its
        /// segment framed on the committing thread, does not.
        #[test]
        fn a_host_refusing_threads_frames_on_the_committing_thread() {
            let mut config = shuttle::Config::new();
            config.stack_size = 4 << 20;
            let scheduler = shuttle::scheduler::RandomScheduler::new(10);
            shuttle::Runner::new(scheduler, config).run(|| {
                let dir = crate::MemDir::new();
                let description = Description {
                    block_size: 4096,
                    capacity: 1 << 20,
                    cell: bucket_cell::layout::test_cell(1),
                };
                let cell = LocalCell::create(dir.clone(), description, Options::default()).unwrap();
                let extent = |sequence| ExtentId::new(1, sequence);
                shuttle::future::block_on(cell.write(extent(0), 0, &[1; 4096])).unwrap();
                cell.close_segment().unwrap();
                crate::sync::refuse_threads(true);
                let committed = shuttle::future::block_on(cell.commit(None));
                crate::sync::refuse_threads(false);
                committed.unwrap();
                let framed = dir.list(&cell_dir(crate::layout::SEGMENTS, 1)).unwrap();
                assert_eq!(framed, ["0000000000000"]);
                shuttle::future::block_on(cell.write(extent(1), 0, &[2; 4096])).unwrap();
                shuttle::future::block_on(cell.commit(None)).unwrap();
                drop(cell);

                let cell = LocalCell::open(dir, Options::default()).unwrap();
                let mut data = [0; 4096];
                for (sequence, byte) in [(0, 1), (1, 2)] {
                    cell.read(extent(sequence), 0, &mut data).unwrap();
                    assert_eq!(data, [byte; 4096], "extent {sequence}");
                }
            });
        }
    }

    /// cladefs: the cell's futures are `Send`, as the daemon's tasks awaiting them are, which tokio
    /// moves between its threads (board 2944): a guard of the cell's lock held across an await
    /// makes one not, as `close`'s and `wait_framed`'s were. Made, never polled.
    #[test]
    fn the_cells_futures_are_send() {
        fn send<T: Send>(_: T) {}
        let description = Description {
            block_size: 4096,
            capacity: 1 << 20,
            cell: bucket_cell::layout::test_cell(1),
        };
        let cell =
            LocalCell::create(crate::MemDir::new(), description, Options::default()).unwrap();
        let extent = ExtentId::new(1, 1);
        send(cell.begin_write(extent, 0, 4096));
        send(cell.write(extent, 0, &[1; 4096]));
        send(cell.write_with(extent, 0, 4096, 4096, |_, _| {}));
        send(cell.wait_until_writes_resume());
        send(cell.commit(None));
        send(cell.commit_ordered(None));
        send(cell.commit_version(NewVersion { id: 1, keep_last: None }, true));
        send(cell.set_capacity(2 << 20));
        send(cell.flush());
        send(cell.close_now());
        send(cell.close_now_unless_filling());
        send(cell.close_for_upload(true));
        send(cell.wait_framed());
        send(cell.wait_looked_ahead());
        send(cell.wait_idle());
        send(cell.closed(1));
        send(cell.acknowledged(0));
        send(cell.unreplicate());
        send(cell.checkpoint(true));
        send(cell.close());
    }

    /// cladefs: a count drifted from what it counts, as a change that missed it leaves it, is found
    /// as the state is next let go, by any call: here the bytes its extents hold (TASK-160.16).
    #[tokio::test(flavor = "multi_thread")]
    #[should_panic(expected = "the bytes the extents hold")]
    async fn audits_find_counts_drifted() {
        let description = Description {
            block_size: 4096,
            capacity: 1 << 20,
            cell: bucket_cell::layout::test_cell(1),
        };
        let cell =
            LocalCell::create(crate::MemDir::new(), description, Options::default()).unwrap();
        cell.write(ExtentId::new(1, 1), 0, &[1; 4096]).await.unwrap();
        cell.state.with(Mutex::lock).space.grew(4096, true);
        cell.last_commit().unwrap();
    }

    /// cladefs: and here how many extents it holds: one counted that no map holds (TASK-454).
    #[tokio::test(flavor = "multi_thread")]
    #[should_panic(expected = "their counts")]
    async fn audits_find_extent_counts_drifted() {
        let description = Description {
            block_size: 4096,
            capacity: 1 << 20,
            cell: bucket_cell::layout::test_cell(1),
        };
        let cell =
            LocalCell::create(crate::MemDir::new(), description, Options::default()).unwrap();
        cell.write(ExtentId::new(1, 1), 0, &[1; 4096]).await.unwrap();
        cell.state.with(Mutex::lock).space.entered(true);
        cell.last_commit().unwrap();
    }

    #[test]
    fn painting() {
        let piece = |segment, offset, extent_offset, len| Piece {
            segment: SegmentId { cell: 1, number: segment },
            offset,
            extent_offset,
            len,
        };
        let at = |segment, offset| Some((SegmentId { cell: 1, number: segment }, offset));
        // Nothing written.
        assert_eq!(paint(&[], 0, 8), vec![(0, 8, None)]);
        // A piece in the middle, and one over its end.
        let pieces = [piece(1, 100, 2, 4), piece(2, 200, 5, 2)];
        assert_eq!(
            paint(&pieces, 0, 8),
            vec![(0, 2, None), (2, 5, at(1, 100)), (5, 7, at(2, 200)), (7, 8, None)]
        );
        // A later piece within an earlier one splits it.
        let pieces = [piece(1, 100, 0, 8), piece(2, 200, 3, 2)];
        assert_eq!(
            paint(&pieces, 1, 7),
            vec![(1, 3, at(1, 101)), (3, 5, at(2, 200)), (5, 7, at(1, 105))]
        );
    }
}
