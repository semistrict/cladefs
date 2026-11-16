// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A branch's segments (see the crate's docs): those it tracks, each where it is in its life
//! ([`Phase`]), with the bytes of extents' data it holds; the open segment's closing, its footer
//! of what the commits since it opened changed, each footer following the one before; cladefs: a
//! closed segment's data framed, with its footer, off the commit that closed it, its log of the
//! commits in it going with its raw data (TASK-165.9).

use crate::dir::{Dir, File};
use crate::durability::Syncs;
use crate::extents::Extents;
use crate::framed::{Files, Framed, Source, read_parts};
use crate::framing::{self, Look, Looked, Seen};
use crate::held::Hold;
use crate::layout::{
    Change, Data, EXTENTS, LOGS, OPEN_SEGMENTS, Piece, SEGMENTS, SPARE, SegmentId, branch_dir,
    extent_path, framing_path, held_frees, log_path, open_segment_path, parse_number, segment_path,
    spare_path,
};
use crate::raw::{RawEncryption, RawFile};
use crate::store::{Options, Usage};
use crate::timings::{self, Timing, durable, framing as framing_phase};
use crate::wal::{LogHeader, log_header, new_salt};
use anyhow::{Context as _, Error, anyhow, bail, ensure};
use extent_bucket::keys::Ring;
use extent_frame::PART;
use extent_frame::segment::{Chain, Child, Contents, Continued, Footer, VersionCut, epoch_of};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io;
use std::sync::Arc;
use std::time::Instant;
use storage_device::ExtentId;

/// cladefs: the bytes of a sector of an open segment, whose raw file an encrypted store encrypts a
/// sector at a time, AES-256-XTS, the sector its tweak (doc-16, TASK-411.5, [`crate::raw`]):
/// which takes no reading, modifying and writing again only if what reaches the segment is whole
/// sectors, at offsets in it and of lengths that are multiples of this (TASK-411.8).
pub(crate) use crate::raw::SECTOR;

/// cladefs: the error of `len` bytes at `offset` that are not whole sectors ([`SECTOR`]), going to
/// the open segment: a write of an extent stored in segments, `offset` in the extent, as its seal
/// copies it whole, from its start; or what a write, or a seal, takes of the segment, `offset` in
/// it. Refused before anything is written (TASK-411.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{len} bytes at {offset}: not whole sectors of {SECTOR} bytes, as an open segment takes")]
pub struct Unsectored {
    pub offset: u64,
    pub len: u64,
}

impl Unsectored {
    /// Fails unless `len` bytes at `offset` are whole sectors.
    pub(crate) fn check(offset: u64, len: u64) -> Result<(), Self> {
        if offset.is_multiple_of(SECTOR) && len.is_multiple_of(SECTOR) {
            Ok(())
        } else {
            Err(Self { offset, len })
        }
    }
}

/// Where a segment the branch tracks is in its life.
enum Phase<F> {
    /// Taking writes, in its raw file; cladefs: its commits recorded in its log, which the flushes
    /// append to (TASK-165.9).
    Open { raw: Arc<RawFile<F>>, log: Log<F> },
    /// cladefs: filled between commits, taking no more writes, which go to the next: the next commit
    /// seals in place what is one run in it, and closes it, its footer naming those runs as the
    /// commit's, which the segments after it go on with, after what the commits before made in it,
    /// as `window` folds them (TASK-92.41). Read from its raw file until then.
    Full { raw: Arc<RawFile<F>>, log: Log<F>, window: Window },
    /// cladefs: closed by commit `last`, its data being framed off the commit, on a thread of its
    /// own, which ends the framing itself ([`Segments::framed`], TASK-153.8): read from its raw
    /// file, which stays open, until then; its commits' records still appended to its log by the
    /// flushes that make them durable, which wait for no framing (TASK-165.9). cladefs: its framed
    /// file `written`, whole, once the framing wrote it, being made durable then, which the
    /// uploader waits for no more (TASK-384): the bucket's object is what makes its commits
    /// durable, its raw file and log holding them here until it is.
    Framing { raw: Arc<RawFile<F>>, log: Log<F>, last: u64, written: Option<Arc<Framed<F>>> },
    /// cladefs: its framing failed, which failed the store: read from its raw file, which stays.
    Failed { raw: Arc<RawFile<F>>, log: Log<F>, error: String },
    /// Closed, a segment of the write-ahead log until acknowledged in the bucket. Its framed file,
    /// if open, is among those kept open (`Segments::files`).
    Closed,
}

/// cladefs: a segment's log, and the salt of its records, its header's (`wal::log_header`); cladefs:
/// and what MACs them, its raw file's encryption (TASK-411.5).
pub(crate) struct Log<F> {
    pub file: Arc<F>,
    pub salt: u64,
    pub encryption: Arc<RawEncryption>,
}

impl<F> Clone for Log<F> {
    fn clone(&self) -> Self {
        Self { file: self.file.clone(), salt: self.salt, encryption: self.encryption.clone() }
    }
}

/// A segment the branch tracks.
struct Segment<F> {
    phase: Phase<F>,
    /// The bytes of its data.
    len: u64,
    /// The bytes its file takes.
    size: u64,
    /// The bytes of extents' data it holds: pieces of extents not freed.
    live: u64,
    /// The writes to it in progress, and the bytes they reserved: neither live nor dead yet.
    writers: u64,
    pending: u64,
    /// cladefs: the sealed extents the branch holds whose data is in it: what is read in the bucket
    /// once it is acknowledged there, found without going through every extent the branch holds
    /// (TASK-165.8).
    sealed: HashSet<ExtentId>,
    /// cladefs: the bytes of its raw file, from its start, whose writing back has begun, as the
    /// writes to it while open end, and of those, the bytes waited for ([`Segments::write_behind`],
    /// TASK-221).
    written_behind: u64,
    written_back: u64,
    /// cladefs: what its parts' looks ahead saw, for its framing, until it takes them
    /// ([`Segments::see`], TASK-400).
    ahead: Option<Ahead>,
    /// cladefs: its key in the bucket, once its uploader's writer acknowledged it there: it leaves
    /// the store for the host's cache of objects once no extent's data is in it here (TASK-165.8),
    /// if the object there is its framed file, not stamped anew after a takeover (TASK-165.9).
    acknowledged: Option<Option<String>>,
}

impl<F> Segment<F> {
    fn new(phase: Phase<F>, live: u64) -> Self {
        let (len, size, writers, pending) = (0, 0, 0, 0);
        // cladefs: looked at ahead, encrypted (TASK-411.23, TASK-421).
        let ahead = Some(Ahead::default());
        Self {
            phase,
            len,
            size,
            live,
            writers,
            pending,
            sealed: HashSet::new(),
            written_behind: 0,
            written_back: 0,
            ahead,
            acknowledged: None,
        }
    }

    /// A write of `len` bytes at `at` in it is done, `live` of them holding extents' data: returns
    /// whether none is in progress in it any more. cladefs: its parts it made whole wait for a look
    /// ahead, in `seeing` ([`Segments::see`]).
    fn done(&mut self, (at, len): (u64, u64), live: u64, seeing: &mut Vec<u64>) -> bool {
        self.writers -= 1;
        self.pending -= len;
        self.live += live;
        self.ended(at, len, seeing);
        self.writers == 0
    }

    /// cladefs: the `len` bytes at `at` in it are written, by a write that ended or an append: the
    /// parts this made whole, all their bytes written, which change no more, wait for a look ahead,
    /// in `seeing`, if its framing is yet to take its looks (TASK-400).
    fn ended(&mut self, at: u64, len: u64, seeing: &mut Vec<u64>) {
        if let Some(ahead) = &mut self.ahead {
            seeing.extend(ahead.ended(at, len));
        }
    }

    /// Whether reaping lets it go: no extent's data is in it, nor write in progress, and it is
    /// closed and acknowledged in the bucket: the write-ahead log holds the rest (TASK-165.9).
    fn is_empty(&self) -> bool {
        let done = matches!(self.phase, Phase::Closed) && self.acknowledged.is_some();
        done && self.live == 0 && self.writers == 0
    }

    /// cladefs: panics unless what it counts of itself, segment `number`, is what it holds
    /// (TASK-160.16): `pieces` of extents' data, and the sealed extents `sealed`.
    #[cfg(any(test, feature = "audit"))]
    fn audit(&self, number: SegmentId, pieces: &[Piece], sealed: &BTreeSet<ExtentId>) {
        for piece in pieces {
            assert!(piece.offset + piece.len <= self.len, "{piece:?} past {number:?}'s data");
        }
        let live: u64 = pieces.iter().map(|piece| piece.len).sum();
        assert_eq!(self.live, live, "the bytes of extents' data {number:?} holds");
        assert!(self.writers > 0 || self.pending == 0, "{number:?}, pending, unwritten");
        let seated: BTreeSet<ExtentId> = self.sealed.iter().copied().collect();
        assert_eq!(&seated, sealed, "the extents sealed in {number:?}");
        if !matches!(self.phase, Phase::Closed) {
            assert_eq!(self.size, self.len, "{number:?}'s raw file, its data");
        }
    }
}

/// The segments a branch tracks: those its extents' data is in, its own, taking writes or not,
/// and those it inherited that it holds otherwise than the state it shares does, having let
/// extents in them go. It holds the others, untracked, as that state does. Its own segments' files
/// it keeps open itself: the open segment's, and one's being framed, which their phases hold, and
/// closed ones', at most `Options::open_files` but for those reads use, the least lately read
/// closing as more open.
///
/// One segment is open, which writes go to. It closes at a commit, or between commits, its footer
/// what the commits since it opened changed ([`Window`]); and another opens. Its footer follows
/// the one closed before it, as the chain of the footers' writer stamps it. cladefs: filled between
/// commits, it waits, full, for the next commit, which closes it, sealing in place what is one run
/// in it (TASK-92.41).
pub(crate) struct Segments<F> {
    tracked: BTreeMap<SegmentId, Segment<F>>,
    /// cladefs: those reaping lets go of ([`Segment::is_empty`]), but for those kept: so that a commit
    /// looks over them alone, not every segment the branch tracks, thousands as it grows
    /// (TASK-92.16).
    empty: BTreeSet<SegmentId>,
    /// The segment taking writes.
    open: SegmentId,
    /// cladefs: the bytes of its data as of the last commit: those after, the writes since
    /// ([`Segments::filling`]).
    committed_len: u64,
    /// cladefs: those filled since the last commit, oldest first, which the next closes
    /// ([`Phase::Full`]): the first alone may hold commits, those made before it filled.
    full: Vec<SegmentId>,
    /// When it opened.
    opened: Instant,
    /// Whether the next commit closes it, whatever it holds.
    close_wanted: bool,
    /// The forks the next commit makes at its end, which close the open segment.
    forking: Vec<Child>,
    /// cladefs: the versions the next commit cuts at its end, which close the open segment
    /// (TASK-476).
    cutting: Vec<VersionCut>,
    /// What the commits since it opened changed, which its footer records.
    window: Window,
    /// cladefs: the chain of the footers' writer, the writer of the branch in object storage, as the
    /// store opened, which stamps each footer with its place: its number, its writer's epoch
    /// and its floor of the branch's next sequence number. Without one, the footers are of epoch 0,
    /// numbered on from the segment open as the branch was opened, one for each segment closed.
    chain: Chain,
    /// The closed segments' framed files kept open.
    files: Files<SegmentId, F>,
    /// Whether the open segment was written since the last commit, which syncs it.
    written: bool,
    /// cladefs: the raw files of the segments closed since the last commit, written since the one
    /// before, which the next commit's flush syncs (TASK-165.9); and those filled since, which the
    /// commit closing them seals extents in (TASK-92.41).
    closed_unsynced: Vec<Arc<F>>,
    /// Whether a segment closed since the last commit, which syncs the directories of open
    /// segments and of logs, the next's raw file's and log's entries. cladefs: its framed file's,
    /// its framing makes durable (`make_durable`, TASK-310).
    closed_since: bool,
    /// cladefs: the segments whose framings are running, each of which ends itself (TASK-153.8).
    framing: BTreeSet<SegmentId>,
    /// cladefs: the segments opened whose raw files' and logs' entries no sync has made durable
    /// since ([`Segments::unsynced_entries`], TASK-310).
    unsynced_entries: BTreeSet<SegmentId>,
    /// cladefs: the segments framed whose raw files and logs are still there, each with the last
    /// commit its footer names: they go once that commit is durable, so that an opening finding it
    /// not durable closes the segment again from them (`wal::recover`, TASK-92.16, TASK-165.9).
    raw: Vec<(u64, SegmentId)>,
    /// cladefs: the spare logs, by the segments they were the logs of ([`spare_path`]), which the
    /// next segments' logs are, rather than files made anew (TASK-301).
    spares: Vec<SegmentId>,
    /// cladefs: the next segment's raw file and log, prepared off the store's state lock, if they
    /// are, there under their names, and whether a preparation runs ([`Segments::to_prepare`],
    /// TASK-333, TASK-384).
    prepared: Option<Placed<F>>,
    preparing: bool,
    /// cladefs: what the salts of the logs this opening of the branch makes are of ([`new_salt`]).
    lifetime: u64,
    /// cladefs: the store's keys, which its segments are framed and read under (TASK-411.3).
    keys: Arc<Ring>,
    /// cladefs: the parts of segments written whole since the store last took them, to look at
    /// ahead of their framings ([`Segments::take_seeing`], TASK-400).
    seeing: Vec<(SegmentId, u64)>,
}

/// cladefs: how many spare logs a branch keeps ([`Segments::spare`]).
const SPARES: usize = 2;

/// cladefs: a segment the branch let go of, no extent being in it any more, acknowledged in the
/// bucket, with its key there if its file is the object there, which goes into the host's cache
/// (TASK-165.8).
pub(crate) type Reaped = (SegmentId, Option<String>);

impl<F: File> Segments<F> {
    /// The segments of `branch`, opened as of commit `epoch`: those of its write-ahead log,
    /// `tracked`, each with the bytes of extents' data it holds, whose files must be there; and
    /// another, open, `next`, past them, whose raw file and log are made, their entries durable.
    /// Its footer, and those after it, `chain` stamps. At most `open_files` of their files stay
    /// open between reads. cladefs: framed and read under `keys`, the store's (TASK-411.3).
    pub fn open<D: Dir<File = F>>(
        dir: &D,
        (branch, tracked): (u64, BTreeMap<SegmentId, u64>),
        (epoch, next, chain): (u64, u64, Chain),
        (open_files, keys): (usize, Arc<Ring>),
    ) -> Result<Self, Error> {
        let mut segments = BTreeMap::new();
        for (id, live) in tracked {
            ensure!(id.branch == branch, "{id:?} is another branch's");
            let path = segment_path(id);
            let file = dir.open(&path, true).with_context(|| format!("segment {path}"))?;
            // cladefs: its trailer and table, a block, not its footer (TASK-92.16).
            let parts = read_parts(&file, &keys.of(branch))?
                .ok_or_else(|| anyhow!("{path} is not a whole framed file"))?;
            let segment = Segment {
                len: parts.frame.data_len,
                size: parts.frame.size(),
                ..Segment::new(Phase::Closed, live)
            };
            segments.insert(id, segment);
        }
        let open = SegmentId { branch, number: next };
        let logs = dir.list(&branch_dir(LOGS, branch))?;
        let spared =
            logs.iter().filter_map(|name| name.strip_prefix(SPARE)).filter_map(parse_number);
        let mut spares: Vec<_> = spared.map(|number| SegmentId { branch, number }).collect();
        let lifetime = {
            let mut bytes = [0; 24];
            for (at, word) in [branch, epoch, next].into_iter().enumerate() {
                bytes[at * 8..][..8].copy_from_slice(&word.to_le_bytes());
            }
            rapidhash::rapidhash(&bytes)
        };
        let salt = new_salt(lifetime, open);
        let (phase, _) = create_open(dir, open, (&mut spares, &mut None), salt, keys.encrypting())?;
        for kind in [OPEN_SEGMENTS, LOGS] {
            dir.sync_dir(&branch_dir(kind, branch))?;
        }
        segments.insert(open, Segment::new(phase, 0));
        Ok(Self {
            tracked: segments,
            empty: BTreeSet::new(),
            open,
            committed_len: 0,
            full: Vec::new(),
            opened: Instant::now(),
            close_wanted: false,
            forking: Vec::new(),
            cutting: Vec::new(),
            window: Window::starting(branch, epoch + 1),
            chain,
            files: Files::new(open_files),
            written: false,
            closed_unsynced: Vec::new(),
            closed_since: false,
            framing: BTreeSet::new(),
            unsynced_entries: BTreeSet::new(),
            raw: Vec::new(),
            spares,
            prepared: None,
            preparing: false,
            lifetime,
            keys,
            seeing: Vec::new(),
        })
    }

    /// The segment taking writes.
    pub fn open_segment(&self) -> SegmentId {
        self.open
    }

    /// Whether the open segment takes `len` bytes more and holds no more than `limit`
    /// (`Options::segment_limit`).
    pub fn fits(&self, len: u64, limit: u64) -> bool {
        self.tracked[&self.open].len + len <= limit
    }

    /// Changes segment `number` as `change` says, keeping count of whether it is empty.
    fn change<T>(&mut self, number: SegmentId, change: impl FnOnce(&mut Segment<F>) -> T) -> T {
        let changed = change(self.tracked.get_mut(&number).unwrap());
        self.note(number);
        changed
    }

    /// Notes whether segment `number`, tracked, is empty ([`Segment::is_empty`]), as it changed.
    fn note(&mut self, number: SegmentId) {
        if self.tracked[&number].is_empty() {
            self.empty.insert(number);
        } else {
            self.empty.remove(&number);
        }
    }

    /// Reserves `len` bytes of the open segment for a write: the segment, its raw file, and where
    /// in it the write goes. The write is in progress until it is done: held
    /// ([`Segments::held`]), or dropped ([`Segments::dropped`]). cladefs: refused, nothing reserved,
    /// unless they are whole sectors ([`Unsectored`], TASK-411.8).
    pub fn reserve(&mut self, len: u64) -> Result<(SegmentId, Arc<RawFile<F>>, u64), Unsectored> {
        let open = self.open;
        Unsectored::check(self.tracked[&open].len, len)?;
        Ok(self.change(open, |segment| {
            let Phase::Open { raw: file, .. } = &segment.phase else {
                unreachable!("the open segment is open")
            };
            let (file, at) = (file.clone(), segment.len);
            segment.len += len;
            segment.size += len;
            segment.writers += 1;
            segment.pending += len;
            (open, file, at)
        }))
    }

    /// A write of `len` bytes at `at` in segment `number` is done, and the segment holds what it
    /// wrote, extents' data: returns whether none is in progress in it any more.
    pub fn held(&mut self, number: SegmentId, (at, len): (u64, u64)) -> bool {
        self.written |= number == self.open;
        self.ended(number, (at, len), len)
    }

    /// A write of `len` bytes at `at` in segment `number` is done, and holds nothing: abandoned, or
    /// to an extent freed since it began; what it wrote is dead space. Returns whether none is in
    /// progress in the segment any more.
    pub fn dropped(&mut self, number: SegmentId, (at, len): (u64, u64)) -> bool {
        self.ended(number, (at, len), 0)
    }

    /// A write of `len` bytes at `at` in segment `number` is done, `live` of them extents' data.
    fn ended(&mut self, number: SegmentId, (at, len): (u64, u64), live: u64) -> bool {
        let mut seeing = Vec::new();
        let idle = self.change(number, |segment| segment.done((at, len), live, &mut seeing));
        self.seeing.extend(seeing.into_iter().map(|part| (number, part)));
        idle
    }

    /// The data of `piece` is in its segment no more: freed, or moved.
    pub fn let_go(&mut self, piece: &Piece) {
        self.change(piece.segment, |segment| segment.live -= piece.len);
    }

    /// Appends `len` bytes of an extent's data, sealed in one run, to the open segment, which
    /// holds them: returns its raw file, and where in it they go. cladefs: written once its caller
    /// says so ([`Segments::appended`]); refused, nothing appended, unless they are whole sectors
    /// ([`Unsectored`], TASK-411.8).
    pub fn append(&mut self, len: u64) -> Result<(Arc<RawFile<F>>, u64), Unsectored> {
        Unsectored::check(self.tracked[&self.open].len, len)?;
        self.written = true;
        Ok(self.change(self.open, |segment| {
            let Phase::Open { raw: file, .. } = &segment.phase else {
                unreachable!("the open segment is open")
            };
            let (file, at) = (file.clone(), segment.len);
            segment.len += len;
            segment.size += len;
            segment.live += len;
            (file, at)
        }))
    }

    /// cladefs: the `len` bytes appended at `at` to segment `number` ([`Segments::append`]) are
    /// written: the parts this made whole wait for a look ahead (TASK-400).
    pub fn appended(&mut self, number: SegmentId, at: u64, len: u64) {
        let mut seeing = Vec::new();
        self.tracked.get_mut(&number).unwrap().ended(at, len, &mut seeing);
        self.seeing.extend(seeing.into_iter().map(|part| (number, part)));
    }

    /// cladefs: what of segment `number`'s raw file to write back, a write to which, while it is open,
    /// ended at `end` in it (TASK-221): what was written since the writing back last began, to
    /// begin writing back, if [`WRITE_BEHIND`] bytes or more; and what was begun more than
    /// [`WRITTEN_BEHIND_MOST`] bytes before `end`, to wait for. Writes end out of order: what one of
    /// those before it hasn't written yet, the sync of the flush after writes.
    pub fn write_behind(&mut self, number: SegmentId, end: u64) -> Option<Behind<F>> {
        let segment = self.tracked.get_mut(&number)?;
        let Phase::Open { raw, .. } = &segment.phase else { return None };
        let from = segment.written_behind;
        if end < from + WRITE_BEHIND {
            return None;
        }
        segment.written_behind = end;
        let back = segment.written_back;
        let wait = end.checked_sub(WRITTEN_BEHIND_MOST).filter(|&to| to > back).map(|to| {
            segment.written_back = to;
            (back, to - back)
        });
        Some(Behind { raw: raw.file().clone(), begin: (from, end - from), wait })
    }

    /// cladefs: the parts of segments written whole since this was last asked, to look at ahead of
    /// their framings, which read them no more if they frame them in place ([`Segments::see`],
    /// TASK-400).
    pub fn take_seeing(&mut self) -> Vec<(SegmentId, u64)> {
        std::mem::take(&mut self.seeing)
    }

    /// cladefs: segment `number`'s raw file, to look at a part of ahead of its framing, if its
    /// framing is yet to take its parts' looks (TASK-400); and the look at its parts seen so far,
    /// which says whether to pack the next ahead too ([`framing::see`]).
    pub fn see(&self, number: SegmentId) -> Option<(Arc<RawFile<F>>, Look)> {
        let segment = self.tracked.get(&number)?;
        let look = segment.ahead.as_ref()?.look;
        match &segment.phase {
            Phase::Open { raw, .. } | Phase::Full { raw, .. } | Phase::Framing { raw, .. } => {
                Some((raw.clone(), look))
            }
            Phase::Failed { .. } | Phase::Closed => None,
        }
    }

    /// cladefs: a look ahead at part `part` of segment `number` saw `seen`, and packed it, `held`, if
    /// it did: for its framing, which takes them ([`Segments::take_seen`]), if it is yet to.
    pub fn saw(&mut self, number: SegmentId, part: u64, seen: Seen, held: Option<Hold>) {
        let ahead = self.tracked.get_mut(&number).and_then(|segment| segment.ahead.as_mut());
        if let Some(ahead) = ahead {
            ahead.saw(part, seen, held);
        }
    }

    /// cladefs: what the looks ahead at segment `number`'s parts took, by part, which its framing
    /// takes as it begins: none are taken after (TASK-400).
    pub fn take_seen(&mut self, number: SegmentId) -> Vec<Option<Looked>> {
        let segment = self.tracked.get_mut(&number);
        segment.and_then(|segment| segment.ahead.take()).map_or_else(Vec::new, |ahead| ahead.looked)
    }

    /// cladefs: segment `number`, closed, is acknowledged in the bucket, its object there at `key`
    /// if its framed file is that object (TASK-165.8): the extents sealed in it, which it returns,
    /// are read there from now on, and it leaves the store once no extent's data is in it here,
    /// the next reaping letting it go as such ([`Reaped`]).
    pub fn acknowledge(&mut self, number: SegmentId, key: Option<&str>) -> Vec<ExtentId> {
        if !self.tracked.contains_key(&number) {
            return Vec::new();
        }
        self.change(number, |segment| {
            segment.acknowledged = Some(key.map(str::to_owned));
            segment.sealed.iter().copied().collect()
        })
    }

    /// cladefs: `extent`, sealed, holds `data` from now on: in the segments, if in any, which say so
    /// once acknowledged in the bucket (TASK-165.8).
    pub fn place(&mut self, extent: ExtentId, data: &Data) {
        self.seat(extent, data, true);
    }

    /// cladefs: `extent`, sealed, holds `data` no more: it was freed, or moved.
    pub fn displace(&mut self, extent: ExtentId, data: &Data) {
        self.seat(extent, data, false);
    }

    /// Notes that `extent`, sealed, is in the segments `data` names, if `seated`, or not.
    fn seat(&mut self, extent: ExtentId, data: &Data, seated: bool) {
        let Data::Pieces(pieces) = data else { return };
        for piece in pieces {
            let sealed = &mut self.tracked.get_mut(&piece.segment).unwrap().sealed;
            if seated {
                sealed.insert(extent);
            } else {
                sealed.remove(&extent);
            }
        }
    }

    /// cladefs: the writes in progress to the segment a closing between commits closes: the first
    /// filled since the last commit, if any, else the open one (`Store::close_now`).
    pub fn closing_writers(&self) -> u64 {
        self.tracked[self.full.first().unwrap_or(&self.open)].writers
    }

    /// cladefs: whether a segment filled since the last commit waits for the next ([`Phase::Full`]).
    pub fn has_full(&self) -> bool {
        !self.full.is_empty()
    }

    /// cladefs: whether the guest is filling the segment a closing between commits would close
    /// (TASK-92.41): one filled since the last commit, or the open one, past [`FILLING`] written
    /// since. Its next commit seals in place what is one run in it; closed first, the commit
    /// copies out all of it, up to a segment and an extent (the shared-disk case's VMs copied a
    /// segment's worth each, closed so mid-burst by the uploader's interval).
    pub fn filling(&self) -> bool {
        let written = self.tracked[&self.open].len.saturating_sub(self.committed_len);
        self.has_full() || written >= FILLING
    }

    /// Has the next commit close the open segment, whatever it holds.
    pub fn want_close(&mut self) {
        self.close_wanted = true;
    }

    /// Has the next commit fork `child` at its end, which its contents name, and close the open
    /// segment, whose footer names the fork (doc-7, Forks): with its owner, and whether it is a
    /// point (doc-7, Points).
    pub fn close_for_fork(&mut self, child: Child) {
        self.close_wanted = true;
        self.forking.push(child);
    }

    /// cladefs: has the next commit cut the version `cut` at its end, which its contents name, and
    /// close the open segment, whose footer names the version (doc-18, TASK-476).
    pub fn close_for_version(&mut self, cut: VersionCut) {
        self.close_wanted = true;
        self.cutting.push(cut);
    }

    /// Whether the open segment holds commits up to `epoch`, the last, which closing it records.
    /// cladefs: or the first full one, which holds those made before it filled, the open segment
    /// none since (TASK-92.41).
    pub fn has_commits(&self, epoch: u64) -> bool {
        match self.full.first() {
            Some(&first) => self.full_window(first).first <= epoch,
            None => self.window.first <= epoch,
        }
    }

    /// cladefs: what the commits made in segment `number`, full, folded ([`Phase::Full`]).
    fn full_window(&self, number: SegmentId) -> &Window {
        match &self.tracked[&number].phase {
            Phase::Full { window, .. } => window,
            _ => unreachable!("{number:?}, filled since the last commit, is full"),
        }
    }

    /// cladefs: whether a commit seals an extent in one run of segment `number` where it is: in the
    /// open segment, or in one filled since the last commit (TASK-92.41).
    pub fn seals_in(&self, number: SegmentId) -> bool {
        number == self.open || self.full.contains(&number)
    }

    /// cladefs: the log of the open segment's commits (TASK-165.9).
    pub fn open_log(&self) -> Log<F> {
        let Phase::Open { log, .. } = &self.tracked[&self.open].phase else {
            unreachable!("the open segment is open")
        };
        log.clone()
    }

    /// cladefs: the log of segment `number`'s commits, open or closed since, which a flush appends
    /// their records to: there until its raw file goes, once it is framed and they are durable
    /// ([`Segments::raw`]).
    pub fn log_file<D: Dir<File = F>>(&self, dir: &D, number: SegmentId) -> Result<Log<F>, Error> {
        match self.tracked.get(&number).map(|segment| &segment.phase) {
            Some(
                Phase::Open { log, .. }
                | Phase::Full { log, .. }
                | Phase::Framing { log, .. }
                | Phase::Failed { log, .. },
            ) => Ok(log.clone()),
            _ => {
                let file = dir.open(&log_path(number), false)?;
                let mut header = [0; crate::wal::LOG_HEADER];
                file.read_cached(&mut header, 0)?;
                let header = LogHeader::of(&header, number)
                    .ok_or_else(|| anyhow!("{number:?}'s log has no header of its own"))?;
                let encryption = header.encryption(number, &self.keys)?;
                Ok(Log { file: Arc::new(file), salt: header.salt, encryption })
            }
        }
    }

    /// cladefs: segment `number`'s raw file, as stored, whose data a commit's record checks
    /// (`wal::LogRecord`): there until it goes, once the segment is framed and its commits durable.
    /// cladefs: encrypted, as its records hold its runs (TASK-411.5).
    pub fn raw_file<D: Dir<File = F>>(&self, dir: &D, number: SegmentId) -> Result<Arc<F>, Error> {
        match self.tracked.get(&number).map(|segment| &segment.phase) {
            Some(
                Phase::Open { raw, .. }
                | Phase::Full { raw, .. }
                | Phase::Framing { raw, .. }
                | Phase::Failed { raw, .. },
            ) => Ok(raw.file().clone()),
            _ => Ok(Arc::new(dir.open(&open_segment_path(number), false)?)),
        }
    }

    /// cladefs: the bytes of segment `number`'s data.
    pub fn len(&self, number: SegmentId) -> u64 {
        self.tracked.get(&number).map_or(0, |segment| segment.len)
    }

    /// cladefs: the bytes of the extents the open segment's commits sealed, in its data and in
    /// files of their own: what of the write-ahead log it holds, which closing it uploads, and not
    /// the writes no commit sealed yet, which no upload takes (TASK-165.9); and the first full
    /// one's (TASK-92.41).
    pub fn open_bytes(&self) -> u64 {
        let full = self.full.first().map(|&first| self.full_window(first));
        let windows = std::iter::once(&self.window).chain(full);
        windows.flat_map(|window| window.sealed.values()).map(|(_, len)| len).sum()
    }

    /// Records commit `epoch`, which changed `changes`, and set its root and the branch's
    /// capacity, if it set them, `extents` the branch's extents as of it, its extents sealed in
    /// the open segment or in files of their own, `next_sequence` the branch's next sequence
    /// number: its contents, as the chain tags it, fold into the open segment's footer to be
    /// ([`Window`]), with the forks it makes ([`Segments::close_for_fork`]). cladefs: and those it
    /// sealed in one run of a segment filled since the last commit, the runs it goes on with, into
    /// that one's, its continuation ([`Continued`], TASK-92.41). Returns the record of each, by its
    /// segment, in the order of the segments, the open one's last: what each segment's log holds
    /// of the commit.
    pub fn record(
        &mut self,
        epoch: u64,
        changes: &[Change],
        (root, capacity): (Option<&[u8]>, Option<u64>),
        extents: &Extents,
        next_sequence: u64,
    ) -> Result<Vec<(SegmentId, Contents)>, Error> {
        let commit = self.chain.tag(epoch);
        let (mut contents, mut runs) = commit_contents((self.open, &self.full), changes, extents)?;
        let mut records = Vec::with_capacity(self.full.len() + 1);
        for &number in &self.full {
            let extents = runs.remove(&number).unwrap_or_default();
            let record =
                Contents { continued: Some(Continued { commit, extents }), ..Contents::default() };
            let Phase::Full { window, .. } = &mut self.tracked.get_mut(&number).unwrap().phase
            else {
                unreachable!("{number:?}, filled since the last commit, is full")
            };
            window.fold(&record);
            records.push((number, record));
        }
        contents.commits = vec![commit];
        contents.root = root.map(<[u8]>::to_vec);
        contents.capacity = capacity;
        contents.next_sequence = next_sequence;
        contents.forks = std::mem::take(&mut self.forking);
        contents.versions = std::mem::take(&mut self.cutting);
        self.window.fold(&contents);
        self.committed_len = self.tracked[&self.open].len;
        records.push((self.open, contents));
        Ok(records)
    }

    /// Whether the open segment is to close, as `options` say, once no write is in progress in
    /// it: full, old, wanted closed, or at a fork.
    pub fn due(&self, options: &Options) -> bool {
        let open = &self.tracked[&self.open];
        let old = open.len > 0 && self.opened.elapsed() >= options.segment_age;
        open.writers == 0 && (open.len >= options.segment_size || old || self.close_wanted)
    }

    /// Closes the open segment, after commit `epoch`, the branch's next sequence number
    /// `next_sequence`: its footer, of what the commits since it opened changed, stamped by the
    /// chain, after its data, to be framed in a file of its own, as object storage keeps segments;
    /// and opens the next, in `dir`. Nothing changes if it fails.
    pub fn close<D: Dir<File = F>>(
        &mut self,
        dir: &D,
        epoch: u64,
        next_sequence: u64,
    ) -> Result<Unframed<F>, Error> {
        let number = self.open;
        debug_assert!(self.full.is_empty(), "{number:?} closes before those filled before it");
        let next = SegmentId { number: number.number + 1, ..number };
        let salt = new_salt(self.lifetime, next);
        let placing = (&mut self.spares, &mut self.prepared);
        let (phase, synced) = create_open(dir, next, placing, salt, self.keys.encrypting())?;
        // Nothing fails from here on.
        let window =
            std::mem::replace(&mut self.window, Window::starting(number.branch, epoch + 1));
        // What was written to it since the last commit, the next flush syncs, before it appends
        // the records of the commits in it to its log (TASK-165.9): little, written back as it
        // was written (`write_behind`), where the sync wrote it all at once beside the framing,
        // which writes it too, up to 0.8 s under pgbench (GCE r737); and waiting for the framing
        // instead waited for a whole segment's writing, up to 0.55 s (r955, TASK-221).
        let written = std::mem::take(&mut self.written);
        let last = Some(epoch);
        let unframed = self.seal_off(number, (window, last), epoch, next_sequence, written);
        self.tracked.insert(next, Segment::new(phase, 0));
        if !synced {
            self.unsynced_entries.insert(next);
        }
        self.open = next;
        self.committed_len = 0;
        self.opened = Instant::now();
        self.close_wanted = false;
        Ok(unframed)
    }

    /// cladefs: closes the first segment filled since the last commit, holding commits, after commit
    /// `epoch`, the last, between commits, as [`Segments::close`] closes the open one: for an
    /// uploader to take the commits in it without waiting for the next (`Store::close_now`). What
    /// was written to it since, the next commit copies out (TASK-92.41).
    pub fn close_first_full(&mut self, epoch: u64, next_sequence: u64) -> Unframed<F> {
        let number = self.full.remove(0);
        let window = self.take_full(number);
        let last = window.last;
        self.seal_off(number, (window, last), epoch, next_sequence, true)
    }

    /// cladefs: closes the segments filled since the last commit, commit `epoch`, which sealed in
    /// them what was one run in each ([`Segments::record`]), in the order they filled: their
    /// footers name the runs the commit goes on with past them, which the open segment ends, after
    /// what the commits before made in them (TASK-92.41).
    pub fn close_full(&mut self, epoch: u64, next_sequence: u64) -> Vec<Unframed<F>> {
        let full = std::mem::take(&mut self.full);
        if !full.is_empty() {
            SPANNING.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let windows: Vec<_> = full.iter().map(|&number| (number, self.take_full(number))).collect();
        (windows.into_iter())
            .map(|(number, window)| {
                let last = window.last;
                self.seal_off(number, (window, last), epoch, next_sequence, true)
            })
            .collect()
    }

    /// cladefs: what the commits made in segment `number`, full, folded, which it holds no more,
    /// closing.
    fn take_full(&mut self, number: SegmentId) -> Window {
        let phase = &mut self.tracked.get_mut(&number).unwrap().phase;
        match phase {
            Phase::Full { window, .. } => std::mem::take(window),
            _ => unreachable!("{number:?}, filled since the last commit, is full"),
        }
    }

    /// cladefs: segment `number`, open or full, closes after commit `epoch`, the last whose data is
    /// in it ([`Phase::Framing`]): its footer, of what `window` folds, naming `last`, its last
    /// commit, if any, stamped by the chain, the branch's next sequence number `next_sequence`;
    /// its raw file synced by the next flush, if `written` since the last commit. Its data, unless no extent's
    /// is in it, nor write in progress, which is framed then as none, its footer alone: a segment
    /// filled whose extents a commit copied out, or freed.
    fn seal_off(
        &mut self,
        number: SegmentId,
        (window, last): (Window, Option<u64>),
        epoch: u64,
        next_sequence: u64,
        written: bool,
    ) -> Unframed<F> {
        let commits = last.map(|last| vec![self.chain.tag(last)]).unwrap_or_default();
        let footer = self.chain.close(window.contents(commits, next_sequence));
        let segment = self.tracked.get_mut(&number).unwrap();
        let (raw, log) = match &segment.phase {
            Phase::Open { raw, log } | Phase::Full { raw, log, .. } => (raw.clone(), log.clone()),
            _ => unreachable!("{number:?}, closing, is open or full"),
        };
        segment.phase = Phase::Framing { raw: raw.clone(), log, last: epoch, written: None };
        debug_assert!(
            segment.live > 0 || footer.contents.runs().next().is_none(),
            "{number:?} names runs, holding none"
        );
        let len = if segment.live > 0 || segment.writers > 0 { segment.len } else { 0 };
        // Written to in place, its framing would change what the frame says of it.
        let quiet = segment.writers == 0;
        if written {
            self.closed_unsynced.push(raw.file().clone());
        }
        self.closed_since = true;
        Unframed { number, raw, len, footer, quiet, keys: self.keys.clone() }
    }

    /// cladefs: the open segment, full, takes no more writes, which go to the next, opened in `dir`
    /// after commit `epoch`, the last: it waits for the next commit, which seals what is one run in
    /// it there, and closes it ([`Phase::Full`], TASK-92.41). Writes in progress in it go on, and
    /// reads read it from its raw file. A guest writing more than a segment between commits had
    /// each segment it filled set aside, holding no commit, or closed, holding some, and the commit
    /// copy out of them every extent written since, each into a file of its own past the open
    /// segment's room: what it wrote written twice, its daemon writing some 2.3 times what its
    /// guests did (TASK-79.2, TASK-310). Nothing changes if it fails.
    pub fn fill<D: Dir<File = F>>(&mut self, dir: &D, epoch: u64) -> Result<(), Error> {
        let number = self.open;
        let next = SegmentId { number: number.number + 1, ..number };
        let salt = new_salt(self.lifetime, next);
        let placing = (&mut self.spares, &mut self.prepared);
        let (phase, synced) = create_open(dir, next, placing, salt, self.keys.encrypting())?;
        let window =
            std::mem::replace(&mut self.window, Window::starting(number.branch, epoch + 1));
        let segment = self.tracked.get_mut(&number).unwrap();
        let Phase::Open { raw, log } = &segment.phase else {
            unreachable!("the open segment is open")
        };
        segment.phase = Phase::Full { raw: raw.clone(), log: log.clone(), window };
        self.full.push(number);
        self.tracked.insert(next, Segment::new(phase, 0));
        if !synced {
            self.unsynced_entries.insert(next);
        }
        self.open = next;
        self.committed_len = 0;
        self.opened = Instant::now();
        // What is in it, the commit closing it syncs (`seal_off`); the next's entry with those of
        // segments closed.
        self.written = false;
        self.closed_since = true;
        Ok(())
    }

    /// cladefs: segment `number`, closed and being framed, is in `framed` from now on, durable, its
    /// raw file no more, which goes once the commits its footer names are ([`Segments::raw`]).
    pub fn framed(&mut self, number: SegmentId, framed: Arc<Framed<F>>) {
        let mut closed = 0;
        self.change(number, |segment| {
            let Phase::Framing { last, .. } = segment.phase else {
                unreachable!("{number:?}, framed, was being framed")
            };
            segment.size = framed.size();
            segment.phase = Phase::Closed;
            closed = last;
        });
        self.files.insert(number, framed);
        self.raw.push((closed, number));
        self.framing.remove(&number);
    }

    /// cladefs: segment `number`, being framed, has its framed file `framed` written, whole, being
    /// made durable from now on, which the uploader takes it without waiting for (TASK-384): read
    /// from its raw file still, as its framed file is not yet named its own.
    pub fn written(&mut self, number: SegmentId, framed: Arc<Framed<F>>) {
        let segment = self.tracked.get_mut(&number).expect("a segment being framed is tracked");
        let Phase::Framing { written, .. } = &mut segment.phase else {
            unreachable!("{number:?}, written, was being framed")
        };
        *written = Some(framed);
    }

    /// cladefs: whether segment `number`'s framing is writing its framed file still: the uploader
    /// takes it once it wrote it, not waiting for it to be made durable (TASK-384).
    pub fn is_writing(&self, number: SegmentId) -> bool {
        matches!(
            self.tracked.get(&number),
            Some(Segment { phase: Phase::Framing { written: None, .. }, .. })
        )
    }

    /// cladefs: segment `number`'s framed file, which the uploader uploads: one written, being made
    /// durable (TASK-384), or a closed one's.
    pub fn framed_file<D: Dir<File = F>>(
        &mut self,
        dir: &D,
        number: SegmentId,
    ) -> Result<Arc<Framed<F>>, Error> {
        if let Some(Segment { phase: Phase::Framing { written: Some(framed), .. }, .. }) =
            self.tracked.get(&number)
        {
            return Ok(framed.clone());
        }
        match self.source(dir, number)? {
            Source::Framed(framed) => Ok(framed),
            Source::Raw(..) => bail!("{number:?} is not framed"),
        }
    }

    /// cladefs: segment `number`'s framing failed, for `error`, which failed the store: it is read
    /// from its raw file, which stays.
    pub fn framing_failed(&mut self, number: SegmentId, error: String) {
        let segment = self.tracked.get_mut(&number).expect("a segment being framed is tracked");
        let Phase::Framing { raw, log, .. } = &segment.phase else {
            unreachable!("{number:?}, whose framing failed, was being framed")
        };
        segment.phase = Phase::Failed { raw: raw.clone(), log: log.clone(), error };
        self.framing.remove(&number);
    }

    /// cladefs: segment `number`, closed, is being framed from now on, by a framing that ends itself
    /// ([`Segments::framed`], [`Segments::framing_failed`]).
    pub fn framing_begun(&mut self, number: SegmentId) {
        debug_assert!(self.is_framing(number), "{number:?}, being framed, was closed");
        self.framing.insert(number);
    }

    /// cladefs: those of `framed`, segments being made durable, whose raw files' and logs' entries no
    /// sync has made durable yet, which the making syncs first, before their framed files' entries
    /// ([`make_durable`]): theirs alone, as a segment opened after them, the next's, needs none of
    /// its own durable for theirs (TASK-369); and the flush syncing the directories after a
    /// segment opened marks its entries durable ([`Segments::take_syncs`]), so that each is synced
    /// once, mostly by that flush, long before its segment closes. An opening reads a framed file whose raw file and log are gone
    /// unchecked, whole, and one whose raw file or log is gone as durable, as those go once it is
    /// (`wal::recover`); one whose entries were never durable, a crash may keep the framed file's
    /// entry and lose those, the framed file torn, read as whole, or following one lost (TASK-310's
    /// crash rounds). The flush after a segment closes syncs the next one's entries, a commit made
    /// in it durable then; nothing waits for that flush to frame a segment. Each framing synced the
    /// directory of raw files after its framed file, making the raw files' entries of the segments
    /// opened since durable before the next framing made theirs; then each synced them first if
    /// none had, on a disk VMs share 0.7-3.8 s, the slowest part of a framing left.
    pub fn unsynced_entries(
        &self,
        framed: impl IntoIterator<Item = SegmentId>,
    ) -> BTreeSet<SegmentId> {
        framed.into_iter().filter(|number| self.unsynced_entries.contains(number)).collect()
    }

    /// cladefs: the entries of `synced`, segments opened, are durable now.
    pub fn entries_synced(&mut self, synced: &BTreeSet<SegmentId>) {
        self.unsynced_entries.retain(|number| !synced.contains(number));
    }

    /// cladefs: where segment `number` stands for a commit sealing the extents in it, for why it
    /// copies them ([`crate::seals`], TASK-92.41).
    pub fn standing(&self, number: SegmentId) -> crate::seals::Standing {
        use crate::seals::Standing;
        match self.tracked.get(&number).map(|segment| &segment.phase) {
            _ if number == self.open => Standing::Open,
            Some(Phase::Full { .. }) => Standing::Full,
            Some(Phase::Framing { .. } | Phase::Failed { .. }) => Standing::Framing,
            Some(Phase::Open { .. }) | Some(Phase::Closed) | None => Standing::Closed,
        }
    }

    /// cladefs: whether segment `number` is being framed.
    pub fn is_framing(&self, number: SegmentId) -> bool {
        matches!(self.tracked.get(&number), Some(Segment { phase: Phase::Framing { .. }, .. }))
    }

    /// cladefs: why segment `number`'s framing failed, if it did.
    pub fn framing_failure(&self, number: SegmentId) -> Option<&str> {
        match self.tracked.get(&number) {
            Some(Segment { phase: Phase::Failed { error, .. }, .. }) => Some(error),
            _ => None,
        }
    }

    /// cladefs: the segments whose framings are running.
    pub fn framings(&self) -> &BTreeSet<SegmentId> {
        &self.framing
    }

    /// cladefs: the paths of the raw files, and logs, of the segments framed whose footers name no
    /// commit past `durable`, the last durable, which may go: an opening reads their framed files
    /// (TASK-153.8, TASK-165.9).
    pub fn raw(&mut self, durable: u64) -> Vec<SegmentId> {
        let (going, staying) = std::mem::take(&mut self.raw)
            .into_iter()
            .partition::<Vec<_>, _>(|(last, _)| *last <= durable);
        self.raw = staying;
        going.into_iter().map(|(_, number)| number).collect()
    }

    /// cladefs: whether `segment` has its log still: open, being framed, or framed and its raw file
    /// and log not gone yet, as its commits aren't durable.
    pub fn has_log(&self, segment: SegmentId) -> bool {
        let framed = self.raw.iter().any(|(_, number)| *number == segment);
        let phase = self.tracked.get(&segment).map(|segment| &segment.phase);
        framed
            || matches!(
                phase,
                Some(
                    Phase::Open { .. }
                        | Phase::Full { .. }
                        | Phase::Framing { .. }
                        | Phase::Failed { .. }
                )
            )
    }

    /// cladefs: what to prepare of the next segment's log, if anything: none if it is prepared, or
    /// being, that segment's; a spare to prepare it from, if any (TASK-333).
    pub fn to_prepare(&mut self) -> Option<Preparation> {
        let next = SegmentId { number: self.open.number + 1, ..self.open };
        if self.preparing || self.prepared.is_some() {
            // One prepared is the next segment's: the opening takes it, or gives it back a spare.
            debug_assert!(self.prepared.as_ref().is_none_or(|ready| ready.segment == next));
            return None;
        }
        self.preparing = true;
        let spare = self.spares.pop();
        let encryption = new_encryption(self.keys.encrypting());
        Some(Preparation { segment: next, spare, salt: new_salt(self.lifetime, next), encryption })
    }

    /// cladefs: a preparation's log is made, as `prepared` says: if it is still the next segment's,
    /// placed in `dir`, there under its name, beside the next segment's raw file, the spare raw
    /// file renamed, or a file made anew: changes to directories, which sync nothing, here,
    /// holding the state, so that no opening of the segment makes them meanwhile (TASK-384).
    /// Returns what of the preparation is left: the entries of the segment placed, to make durable
    /// ([`Segments::placed_synced`]), the preparation running until then; or, one prepared for a
    /// segment opened meanwhile, with a log of its own, the next's to prepare then, of the log
    /// prepared, the preparation going on; or, one failed, the spare it took a spare again, if the
    /// branch keeps another ([`SPARES`]), else its name to remove, the preparation ended.
    pub fn prepared<D: Dir<File = F>>(
        &mut self,
        dir: &D,
        prepared: Result<Prepared<F>, (Option<SegmentId>, Error)>,
    ) -> Preparing {
        let next = SegmentId { number: self.open.number + 1, ..self.open };
        let spare = match prepared {
            Ok(prepared) if prepared.segment == next => {
                let spare = prepared.spare;
                match self.place_prepared(dir, prepared) {
                    Ok(placed) => {
                        self.prepared = Some(placed);
                        return Preparing::Placed(next);
                    }
                    Err(error) => {
                        log::warn!("extent store: placing {next:?}'s raw file and log: {error:#}");
                        spare
                    }
                }
            }
            // cladefs: its log the next's, the preparation going on: the opening began none, this one
            // running, and ended here, it left the next segment nothing prepared until another
            // flush or closing, the last flush's having found it running (TASK-404).
            Ok(stale) => {
                // cladefs: the next's raw file encrypted under a salt of its own, drawn anew
                // (TASK-411.5).
                let (salt, encryption) =
                    (new_salt(self.lifetime, next), new_encryption(self.keys.encrypting()));
                let spare = Some(stale.spare);
                return Preparing::Again(Preparation { segment: next, spare, salt, encryption });
            }
            Err((spare, error)) => {
                log::warn!("extent store: preparing {next:?}'s log: {error:#}");
                self.preparing = false;
                let Some(spare) = spare else { return Preparing::Ended(None) };
                spare
            }
        };
        self.preparing = false;
        if self.wants_spare() {
            self.spares.push(spare);
            return Preparing::Ended(None);
        }
        Preparing::Ended(Some(spare))
    }

    /// cladefs: places `prepared`, the next segment's log, prepared, there under its name in `dir`,
    /// beside the segment's raw file, made anew. Fails leaving the log a spare, under its spare's
    /// name, and no raw file of the segment's.
    fn place_prepared<D: Dir<File = F>>(
        &self,
        dir: &D,
        prepared: Prepared<F>,
    ) -> Result<Placed<F>, Error> {
        let Prepared { segment, spare, file, salt, encryption } = prepared;
        let raw = dir.create(&open_segment_path(segment), true)?;
        let (from, to) = (spare_path(spare), log_path(segment));
        if let Err(error) = dir.rename(&from, &to) {
            let path = open_segment_path(segment);
            if let Err(err) = dir.remove(&path) {
                log::warn!("extent store: removing {path}: {err}");
            }
            return Err(Error::new(error).context(format!("renaming {from} {to}")));
        }
        Ok(Placed { segment, raw, log: file, salt, encryption, synced: false })
    }

    /// cladefs: the entries of segment `segment`, placed ([`Segments::prepared`]), were made durable,
    /// or not, as `synced` says, which ends the preparation: the segment, opened meanwhile, needs
    /// no flush to sync them, nor, opening later, does it (TASK-384). Returns the next's
    /// preparation, if the segment opened meanwhile, the preparation going on with it: the opening
    /// began none, this one running (TASK-404).
    pub fn placed_synced(&mut self, segment: SegmentId, synced: bool) -> Option<Preparation> {
        self.preparing = false;
        if synced {
            match &mut self.prepared {
                Some(placed) if placed.segment == segment => placed.synced = true,
                _ => {
                    self.unsynced_entries.remove(&segment);
                }
            }
        }
        self.to_prepare()
    }

    /// cladefs: the next segment, if its raw file and log are placed ahead of it (TASK-384).
    pub fn placed(&self) -> Option<SegmentId> {
        self.prepared.as_ref().map(|placed| placed.segment)
    }

    /// cladefs: whether the next segment's log is being prepared ([`Segments::to_prepare`]).
    pub fn preparing(&self) -> bool {
        self.preparing
    }

    /// cladefs: whether the branch keeps another spare log ([`SPARES`]).
    pub fn wants_spare(&self) -> bool {
        self.spares.len() < SPARES
    }

    /// cladefs: the log of `segment`, gone, is a spare now, at [`spare_path`].
    pub fn spare(&mut self, segment: SegmentId) {
        self.spares.push(segment);
    }

    /// The file of segment `number`, opened in `dir` if it isn't: the raw file of the open segment,
    /// or of one filled or being framed; a closed one's framed file.
    pub fn source<D: Dir<File = F>>(
        &mut self,
        dir: &D,
        number: SegmentId,
    ) -> Result<Source<F>, Error> {
        let segment = self.tracked.get(&number).ok_or_else(|| anyhow!("no segment {number:?}"))?;
        match &segment.phase {
            Phase::Open { raw, .. }
            | Phase::Full { raw, .. }
            | Phase::Framing { raw, .. }
            | Phase::Failed { raw, .. } => {
                return Ok(Source::Raw(raw.clone()));
            }
            Phase::Closed => {}
        }
        if let Some(framed) = self.files.get(number) {
            return Ok(Source::Framed(framed));
        }
        let path = segment_path(number);
        let keys = self.keys.of(number.branch);
        let framed = Arc::new(Framed::open(dir.open(&path, true)?, &path, &keys)?);
        Ok(Source::Framed(self.files.insert(number, framed)))
    }

    /// Adds to `syncs` what the next commit's flush syncs of the segments': the open segment's
    /// file, if it was written since the last commit, keeping its pages in the host's page cache,
    /// as they go with it once it is framed and removed (TASK-92.23); and if one closed since, the
    /// directories of open segments and of their logs. cladefs: not that of framed ones, whose
    /// framings make their entries durable (`make_durable`): the commit needs none of them, and
    /// each directory's sync was a journal commit of the host's file system's and a flush of its
    /// disk's cache more for the commits' durability to wait for (TASK-310). Returns the segments
    /// whose entries those directories' syncs make durable, the flush to mark them so once it
    /// synced ([`Segments::entries_synced`]): a making of segments framed durable synced them
    /// again, a journal commit more, before which a disk VMs share kept flushes' syncs waiting
    /// seconds (TASK-369).
    pub fn take_syncs(&mut self, syncs: &mut Syncs<F>) -> BTreeSet<SegmentId> {
        if std::mem::take(&mut self.written) {
            let Phase::Open { raw, .. } = &self.tracked[&self.open].phase else {
                unreachable!("the open segment is open")
            };
            syncs.cached(raw.file().clone());
        }
        for raw in self.closed_unsynced.drain(..) {
            syncs.cached(raw);
        }
        // cladefs: none if the segments opened since had theirs made durable as they were placed
        // (TASK-384).
        if !std::mem::take(&mut self.closed_since) || self.unsynced_entries.is_empty() {
            return BTreeSet::new();
        }
        for kind in [OPEN_SEGMENTS, LOGS] {
            syncs.dir(branch_dir(kind, self.open.branch));
        }
        self.unsynced_entries.clone()
    }

    /// Lets go of the segments no extent's data is in any more, closed and acknowledged in the
    /// bucket, which no write is in progress in, and returns them.
    pub fn reap(&mut self) -> Vec<Reaped> {
        let dead: Vec<SegmentId> = self.empty.iter().copied().collect();
        let mut reaped = Vec::with_capacity(dead.len());
        for number in dead {
            self.empty.remove(&number);
            let segment = self.tracked.remove(&number).unwrap();
            self.files.remove(number);
            let key = segment.acknowledged.expect("a segment reaped is acknowledged");
            reaped.push((number, key));
        }
        reaped
    }

    /// What the segments take: how many there are, the bytes of their data, of extents' data, and
    /// of their files; none of what the extents stored on their own take.
    pub fn usage(&self) -> Usage {
        let all = || self.tracked.values();
        Usage {
            segments: all().count() as u64,
            segment_bytes: all().map(|segment| segment.len).sum(),
            live_bytes: all().map(|segment| segment.live).sum(),
            own_bytes: 0,
            disk_bytes: all().map(|segment| segment.size).sum(),
            kept_bytes: 0,
            kept_for_views: 0,
        }
    }

    /// cladefs: panics unless what it counts of the segments it tracks is what `recount` says they
    /// hold, as the branch holds them; and it tracks every segment the branch holds extents in
    /// (TASK-160.16).
    #[cfg(any(test, feature = "audit"))]
    pub fn audit(&self, recount: &Recount) {
        let none = (Vec::new(), BTreeSet::new());
        for (number, segment) in &self.tracked {
            let (pieces, sealed) = recount.held.get(number).unwrap_or(&none);
            assert_eq!(self.empty.contains(number), segment.is_empty(), "{number:?}, empty");
            segment.audit(*number, pieces, sealed);
        }
        assert!(self.empty.iter().all(|number| self.tracked.contains_key(number)));
        for number in recount.held.keys() {
            assert!(self.tracked.contains_key(number), "{number:?}, held, untracked");
        }
    }
}

/// cladefs: what each segment holds, recounted from what the branch holds, for [`Segments::audit`]
/// (TASK-160.16): the pieces of extents' data in it, of the extents the branch holds, open or
/// sealed, and of those it keeps, freed; and the extents sealed in it.
#[cfg(any(test, feature = "audit"))]
#[derive(Default)]
pub(crate) struct Recount {
    held: BTreeMap<SegmentId, (Vec<Piece>, BTreeSet<ExtentId>)>,
}

#[cfg(any(test, feature = "audit"))]
use crate::layout::Extent;

#[cfg(any(test, feature = "audit"))]
impl Recount {
    /// The branch holds `extent` as `entry` says, sealed if `sealed`, or open.
    pub fn holds(&mut self, extent: ExtentId, entry: &Extent, sealed: bool) {
        let Data::Pieces(pieces) = &entry.data else { return };
        for piece in pieces {
            let (held, seated) = self.held.entry(piece.segment).or_default();
            held.push(*piece);
            if sealed {
                seated.insert(extent);
            }
        }
    }

    /// The branch keeps `entry`'s data, freed.
    pub fn keeps(&mut self, entry: &Extent) {
        let Data::Pieces(pieces) = &entry.data else { return };
        for piece in pieces {
            self.held.entry(piece.segment).or_default().0.push(*piece);
        }
    }
}

/// cladefs: a segment just closed, whose data is yet to be framed, with `footer`
/// ([`Unframed::frame`]): off the commit that closed it, on a thread of its own, whose framing
/// ends itself (TASK-153.8).
pub(crate) struct Unframed<F> {
    number: SegmentId,
    raw: Arc<RawFile<F>>,
    len: u64,
    pub footer: Footer,
    /// cladefs: whether no write was in progress in it as it closed, none to come: what framing it
    /// in place needs, which leaves its data where it is (TASK-310).
    quiet: bool,
    /// cladefs: the store's keys, whose newest DEK of its branch it is encrypted under, if any
    /// (TASK-411.3).
    keys: Arc<Ring>,
}

impl<F: File> Unframed<F> {
    /// The segment.
    pub fn number(&self) -> SegmentId {
        self.number
    }

    /// cladefs: the bytes of its data, framed.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Frames it in its framed file in `dir`, and returns that, written, not yet durable, under a
    /// name of its own until it is ([`framing_path`]): what [`make_durable`] makes durable, with
    /// its entry, together with others framed meanwhile, syncing nothing itself (TASK-310). Its raw
    /// file may go once it is, and the commits its footer names are too. cladefs: the files of the
    /// extents stored on their own its footer names are durable first, so that a framed file a
    /// crash leaves whole names none torn (TASK-165.9). `ahead`, what looks ahead at its parts
    /// took, by part, as it filled ([`Segments::take_seen`], TASK-400).
    pub fn frame<D: Dir<File = F>>(
        self,
        dir: &D,
        ahead: Vec<Option<Looked>>,
    ) -> Result<Arc<Framed<F>>, Error> {
        // cladefs: timed, phase by phase (TASK-165.9), with its waits to run (TASK-336).
        let mut timing = Timing::since(Instant::now());
        // The extents stored on their own it names, durable before it is, as a flush syncs them,
        // several at once: one at a time, each a journal commit of the host's file system's, they
        // held up for as long the syncs of the flushes of cladefsd's turns, which wait on the same
        // files, up to 0.9 s under pgbench (GCE r730, TASK-165.9).
        let mut own = Syncs::default();
        for (extent, _) in self.footer.own() {
            own.file(Arc::new(dir.open(&extent_path(extent), true)?));
            own.dir(branch_dir(EXTENTS, self.number.branch));
        }
        own.sync(dir)?;
        timing.took(framing_phase::OWN);
        let footer = self.footer.encode();
        let (path, raw_path) = (framing_path(self.number), open_segment_path(self.number));
        let raw = (raw_path.as_str(), &self.raw);
        let data = (self.len, self.quiet, ahead);
        let encryption = self.keys.encrypting();
        let (framed, how) =
            frame_segment(dir, &path, raw, data, (&footer, encryption), &mut timing)?;
        timings::framed(self.number, how, timing.end());
        Ok(framed)
    }
}

/// cladefs: makes `framed`, segments of `branch` framed since the last such call, whose framings
/// wrote them, the oldest written at `since`, durable together: each framed file synced, several
/// at once, then renamed its segment's ([`framing_path`]), and the directory of framed files, their
/// entries, synced once for them all (TASK-310); first, if `entries`, the directories of raw files
/// and logs, for the segments opened since they were last synced ([`Segments::unsynced_entries`]),
/// so that no framed file's entry is durable before its raw file's and log's. Each framing synced
/// its own file, then the directories of framed and of raw files, one after another: three syncs,
/// each a journal commit of the host's file system and a flush of its disk's cache, whose queue a
/// disk VMs share keeps full, 2-4 s each with eight VMs writing at once, the framings after it
/// waiting (TASK-92.41's A/B on ubu1). The segments' framings end once it returns: what waits for
/// their framed files to be durable waits for that (the uploader, a closing between commits, a
/// branch's closing), and their raw files and logs go no sooner, as before. A crash meanwhile may
/// keep any of the framed files whole, each torn or not as the others are, under its own name or
/// its segment's: an opening removes the first, and reads one whole only while those before it
/// are whole too, checking each whose raw file is still there, and closes again from its raw file
/// and log any after the first that isn't (`wal::recover`).
pub(crate) fn make_durable<D: Dir>(
    dir: &D,
    branch: u64,
    (framed, entries): (&[(SegmentId, Arc<Framed<D::File>>)], bool),
    since: Instant,
) -> Result<(), Error> {
    // Where the segments are framed, written, not yet durable: their framings still running.
    fail::fail_point!("segments::making_durable");
    let mut timing = Timing::since(since);
    timing.took(durable::WAIT);
    if entries {
        let mut opened = Syncs::default();
        for kind in [OPEN_SEGMENTS, LOGS] {
            opened.dir(branch_dir(kind, branch));
        }
        opened.sync_dirs(dir)?;
    }
    timing.took(durable::ENTRIES);
    // cladefs: each written back a piece at a time, waiting for each, then synced: so that its
    // sync, and the syncs of the daemon's turns, which the host's journal makes wait for what was
    // begun before them, find little of it to write at once, as a framed segment's 32 MiB synced
    // at once kept them waiting up to a second (GCE r728, TASK-165.9). Its framing waited for
    // each piece itself, 1.0-2.7 s a framing behind a disk VMs share (TASK-401), and the uploader
    // waited for the framing: it reads the framed file through the host's page cache now, which
    // holds what the framing wrote ([`crate::read_object`]).
    let mut syncs = Syncs::default();
    for (_, framed) in framed {
        write_back_by_pieces(&*framed.file)?;
        syncs.file(framed.file.clone());
    }
    syncs.dir(branch_dir(SEGMENTS, branch));
    syncs.sync_files(dir)?;
    timing.took(durable::FILES);
    for &(number, _) in framed {
        let (from, to) = (framing_path(number), segment_path(number));
        dir.rename(&from, &to).with_context(|| format!("renaming {from} {to}"))?;
    }
    syncs.sync_dirs(dir)?;
    dir.barrier()?;
    timing.took(durable::DIRS);
    timings::made_durable(branch, framed.len(), timing.end());
    Ok(())
}

/// cladefs: writes `file`, a framed file, back to the host's disk a piece of [`WRITTEN_BACK`] bytes
/// at a time, waiting for each ([`make_durable`]): what of it is written back already, an in-place
/// framing's raw data, the guest's writes wrote back, it waits for no more.
fn write_back_by_pieces<F: File>(file: &F) -> io::Result<()> {
    let size = file.size()?;
    for at in (0..size).step_by(WRITTEN_BACK as usize) {
        file.write_back(at, (size - at).min(WRITTEN_BACK))?;
    }
    Ok(())
}

/// cladefs: the bytes written to the open segment since the last commit from which the guest is
/// filling it ([`Segments::filling`]): more than the flush of a daemon's turn writes at once
/// ([`WRITTEN_BEHIND_MOST`]), a bulk write's, not a journal's or a database's between its commits.
pub(crate) const FILLING: u64 = 8 << 20;

/// cladefs: the bytes of the open segment's raw file whose writing back begins at once, written
/// ([`Segments::write_behind`], TASK-221).
pub(crate) const WRITE_BEHIND: u64 = 1 << 20;

/// cladefs: the most bytes of the open segment's raw file whose writing back began that its writes
/// go on past, not waiting for it ([`Segments::write_behind`]): what the sync of a daemon's turn
/// writes of what its guest wrote, at most, but for the writes in progress then. A guest's burst
/// outran the host's disk, up to 300 MB a second under pgbench against some 200, and its writing
/// back, begun but not waited for, filled the disk's queue as waiting in the host's page cache did:
/// the syncs of the turns after waited behind it, up to 0.6 s (GCE r956, TASK-221). Waiting, its
/// writes go on as fast as the disk takes them.
pub(crate) const WRITTEN_BEHIND_MOST: u64 = 8 << 20;

/// cladefs: what of the open segment's raw file to write back, a write to it done
/// ([`Segments::write_behind`]): a range to begin writing back, and one to wait for, if any.
pub(crate) struct Behind<F> {
    pub raw: Arc<F>,
    pub begin: (u64, u64),
    pub wait: Option<(u64, u64)>,
}

/// cladefs: the bytes of a framed file [`make_durable`] writes back at a time, waiting for each,
/// before it syncs it ([`File::write_back`]): what a sync behind it, a daemon's turn's, waits for at
/// most, as the host's disk writes them.
pub(crate) const WRITTEN_BACK: u64 = 1 << 20;

/// cladefs: what a segment's looks ahead at its parts took as it filled, for its framing, which reads
/// them no more if it frames the segment in place, or holds them packed (TASK-400): of each part,
/// the bytes of it written, each by a write ended or an append, and once all are, what a look at it
/// took, if one did; and the look at all of those seen, which says whether the segment will be
/// framed in place, as its framing's does ([`Look::saves_little`]).
#[derive(Default)]
struct Ahead {
    written: Vec<u64>,
    looked: Vec<Option<Looked>>,
    look: Look,
}

impl Ahead {
    /// The `len` bytes at `at` are written: the parts this made whole, all their bytes written. A
    /// segment's writes write each of its bytes once, its reservations never overlapping, nor its
    /// appends.
    fn ended(&mut self, at: u64, len: u64) -> impl Iterator<Item = u64> + use<> {
        let (end, mut whole) = (at + len, Vec::new());
        for part in at / PART..end.div_ceil(PART) {
            let (from, to) = (at.max(part * PART), end.min((part + 1) * PART));
            let part_at = part as usize;
            if self.written.len() <= part_at {
                self.written.resize(part_at + 1, 0);
            }
            self.written[part_at] += to - from;
            debug_assert!(self.written[part_at] <= PART, "part {part} written twice");
            if self.written[part_at] == PART {
                whole.push(part);
            }
        }
        whole.into_iter()
    }

    /// A look at part `part` saw `seen`, and packed it, `held`, if it did. Once the parts seen
    /// say the segment will be framed in place, it holds none of them packed: its framing reads
    /// them not, nor any part, but for those not seen ahead.
    fn saw(&mut self, part: u64, seen: Seen, held: Option<Hold>) {
        let part = part as usize;
        if self.looked.len() <= part {
            self.looked.resize_with(part + 1, || None);
        }
        self.look = self.look.plus(seen.look);
        let held = match self.look.saves_little() {
            true => {
                self.looked.iter_mut().flatten().for_each(|looked| looked.held = None);
                None
            }
            false => held,
        };
        self.looked[part] = Some(Looked { seen, held });
    }
}

/// cladefs: frames the `len` bytes of a closed segment's data, which its raw file `raw`, at
/// `raw_path`, holds, with `footer`, into the framed file at `path` in `dir`: counted in
/// [`segment_closes`], and timed in `timing`, reading, compressing and writing (TASK-336); and
/// returns how ([`How`]). In place, if `quiet`, no write in progress in it, and compressing its
/// parts saves less than an eighth of its data ([`framing::IN_PLACE_SAVING`]), each stored raw
/// then, as an object of such parts holds them as they are, where they are: its raw file, its tail
/// appended, is its framed file, by another name ([`Dir::link`]), its data written once, as its
/// guest wrote it (TASK-310). A segment's data was written twice, raw, which the host's disk wrote
/// back as the guest wrote it, and framed: as much again as its guest wrote, the disk's other
/// writers waiting behind it, on a disk VMs share. Or else in a file of its own, as before. Its
/// parts looked at `ahead`, as it filled ([`Segments::take_seen`]), it reads only to frame it in a
/// copy, and not those packed then, held still (TASK-400). cladefs: encrypted, if `encrypting` gives a
/// DEK, its branch's newest: in a copy, its parts compressed and encrypted with AES-GCM (TASK-411.3);
/// in place as in the clear, its raw file, encrypted, AES-256-XTS by sector, the object of its
/// sectors as they are, each part tagged ([`extent_frame::Encryption::Sectors`], TASK-411.23). Framed in
/// a copy, an encrypted segment wrote every byte twice, the guests' writes 1.9 times over on
/// fxfs-guest-sandboxes against 1.16 in the clear. Its parts looked at ahead are seen as stored,
/// and held packed in the clear until its framing encrypts them ([`framing`], TASK-421).
fn frame_segment<D: Dir>(
    dir: &D,
    path: &str,
    (raw_path, raw): (&str, &Arc<RawFile<D::File>>),
    (len, quiet, ahead): (u64, bool, Vec<Option<Looked>>),
    (footer, encrypting): (&[u8], extent_bucket::layout::EncryptionKey<'_>),
    timing: &mut Timing<4>,
) -> Result<(Arc<Framed<D::File>>, How), Error> {
    let started = Instant::now();
    // cladefs: its parts read, each in a buffer kept for the framings after; checksummed and looked
    // at if it may be framed in place (TASK-336), but for those seen ahead, and those packed then
    // (TASK-400).
    // cladefs: in a copy, its parts encrypted with AES-GCM (TASK-411.2, TASK-411.3).
    let mut packer = extent_frame::Packer::new(encrypting);
    // cladefs: in place, whole sectors of its raw file, its object their sectors under the raw
    // file's key (TASK-411.23).
    let sectors = raw.sectors();
    let in_place_maybe = quiet && len > 0 && len.is_multiple_of(SECTOR);
    let mut parts = framing::read(&**raw, len, in_place_maybe, ahead)?;
    // cladefs: in place at once if a look at its parts says compressing them saves too little,
    // compressing no more of them: compressing every part to tell was most of what framings took
    // with eight VMs framing at once, up to 0.9 s each; else by them all (TASK-310). The parts
    // packed ahead, held, held no more either way.
    let look = parts.iter().fold(Look::default(), |look, part| look.plus(part.seen.look));
    let sampled = in_place_maybe && look.saves_little();
    if !sampled {
        framing::fill(&**raw, len, &mut parts)?;
    }
    let bytes = |of: fn(&framing::Read) -> bool| -> u64 {
        parts.iter().filter(|part| of(part)).map(|part| part.len).sum()
    };
    let (read, held) = (bytes(|part| part.data.is_some()), bytes(|part| part.packed.is_some()));
    FRAMING_READ.fetch_add(read, std::sync::atomic::Ordering::Relaxed);
    timing.took(framing_phase::READ);
    let mut packed = Vec::new();
    if !sampled {
        // On threads of their own, whose waits to run count in this one's stead.
        let handing = timing.waits.handing();
        let waited = framing::pack(&mut parts, packer.key());
        timing.waits.handed(handing, waited);
        for part in &parts {
            let (stored, how) = part.stored();
            packer.push_stored(&mut packed, part.len, stored, how);
        }
    }
    let in_place =
        sampled || (in_place_maybe && packer.end() > len - len / framing::IN_PLACE_SAVING);
    if in_place {
        packer = extent_frame::Packer::in_place(sectors.clone());
        let raw_parts = parts.iter().zip(0..).map(|(read, part)| {
            debug_assert_eq!(read.seen.part.len, (len - part * PART).min(PART), "part {part}");
            read.seen.part
        });
        packer.push_raw_parts(raw_parts);
    }
    drop(parts);
    let end = packer.end();
    let (tail, parts) = packer.finish_parts(footer);
    timing.took(framing_phase::COMPRESS);
    let framed = match in_place {
        true => frame_in_place(dir, (raw_path, raw.file()), path, end, &tail)?,
        false => None,
    };
    let how = How { in_place: framed.is_some(), len, read, held };
    let framed = match framed {
        Some(file) => Framed::new(file, parts),
        None => {
            let file = dir.create(path, true)?;
            // Where the framed file is in its directory, not yet written: whole only once its
            // framing ends, which its store says ([`Store::framing`](crate::Store::framing),
            // TASK-189).
            fail::fail_point!("segments::framing");
            // cladefs: written, its writing back left to the making durable after, which writes
            // it back a piece at a time before it syncs it ([`make_durable`], TASK-401).
            write_framed(&file, &packed, 0)?;
            write_framed(&file, &tail, end)?;
            Framed::new(Arc::new(file), parts)
        }
    };
    timing.took(framing_phase::WRITE);
    SEGMENT_CLOSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = started.elapsed().as_nanos() as u64;
    SEGMENT_CLOSE_NS.fetch_add(nanos, std::sync::atomic::Ordering::Relaxed);
    Ok((Arc::new(framed), how))
}

/// cladefs: how a segment was framed, as its framing's warning says it (TASK-400): in place or in a
/// copy, of how many bytes of data, and how many of them it read, those a look ahead saw not; and
/// how many it took packed ahead, held.
pub(crate) struct How {
    in_place: bool,
    len: u64,
    read: u64,
    held: u64,
}

impl std::fmt::Display for How {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mib = |bytes: u64| bytes as f64 / (1 << 20) as f64;
        let how = if self.in_place { "in place" } else { "in a copy" };
        let (len, read, held) = (mib(self.len), mib(self.read), mib(self.held));
        write!(f, "{how}, {len:.1} MiB, {read:.1} MiB read, {held:.1} MiB held")
    }
}

/// cladefs: writes `bytes` at `offset` in `file`, a segment's framed file, or its tail: counted, as
/// framed ([`crate::written`]).
fn write_framed<F: File>(file: &F, bytes: &[u8], offset: u64) -> io::Result<()> {
    file.write_at(bytes, offset)?;
    crate::written::wrote(crate::written::Kind::Framed, bytes.len());
    Ok(())
}

/// cladefs: frames a segment's data, `raw`, its raw file at `raw_path`, its parts stored raw, in
/// place ([`frame_segment`]), the file as it stores them, encrypted or not: `tail` appended at `end`,
/// the end of its data, in the clear as any object's tail is (TASK-411.23), which the making
/// durable after writes back (TASK-401: written back here, a tail of a few KiB waited 1.5 s
/// behind the disk's queue); and the file linked at `path`, its framed file's. Its
/// raw path goes once the commits its footer names are durable, as a raw file does
/// ([`Segments::raw`]): an opening finding the framed file torn closes the segment again from its
/// raw file and log, as one framed in a copy. None if `dir` links no files.
fn frame_in_place<D: Dir>(
    dir: &D,
    (raw_path, raw): (&str, &Arc<D::File>),
    path: &str,
    end: u64,
    tail: &[u8],
) -> Result<Option<Arc<D::File>>, Error> {
    write_framed(&**raw, tail, end)?;
    match dir.link(raw_path, path) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::Unsupported => return Ok(None),
        Err(err) => return Err(Error::new(err).context(format!("linking {raw_path} at {path}"))),
    }
    // As a framed file in its directory, not yet synced.
    fail::fail_point!("segments::framing");
    IN_PLACE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(Some(raw.clone()))
}

/// What commit `changes` made, as the footer of `segment`, the open segment, names it: the extents
/// it sealed or moved, which the branch holds as of it, `extents`, each in one run of `segment`
/// or in a file of its own, the branch's, by its sequence number; and of its frees, those of
/// extents a commit held, which the store spent, if its own: so that the footer's readers spend
/// all it frees, as the store did (TASK-160.15). Those of extents written in it, open, it frees
/// not: no commit held them, nor names them, and the store writes them again. Those held
/// elsewhere, in the bucket, it names not. The rest of its contents are its caller's. cladefs: and
/// apart, by their segments, the runs of those it sealed in one run of a segment of `full`, filled
/// since the last commit, which it goes on with past them (TASK-92.41).
fn commit_contents(
    (segment, full): (SegmentId, &[SegmentId]),
    changes: &[Change],
    extents: &Extents,
) -> Result<(Contents, BTreeMap<SegmentId, Vec<Run>>), Error> {
    let held = held_frees(changes);
    let (mut touched, mut frees) = (BTreeSet::new(), BTreeSet::new());
    for change in changes {
        match change {
            Change::Write { extent, .. }
            | Change::WriteOwn { extent, .. }
            | Change::Move { extent, .. }
            | Change::MoveOwn { extent } => {
                touched.insert(*extent);
            }
            Change::Free(extent) => {
                touched.remove(extent);
                if held.contains(extent) {
                    frees.insert(*extent);
                }
            }
        }
    }
    let mut contents = Contents { frees: frees.into_iter().collect(), ..Contents::default() };
    let mut continued: BTreeMap<SegmentId, Vec<Run>> = BTreeMap::new();
    for extent in touched {
        let entry = extents.get(&extent).expect("touched and held").into_cow();
        match &entry.data {
            Data::Pieces(pieces) => {
                let [piece] = pieces[..] else { bail!("{extent:?} is not sealed in one run") };
                let run = (extent, piece.offset, entry.len);
                if piece.segment == segment {
                    contents.extents.push(run);
                } else {
                    ensure!(
                        full.contains(&piece.segment),
                        "{extent:?} is sealed in another segment"
                    );
                    continued.entry(piece.segment).or_default().push(run);
                }
            }
            Data::Own => {
                ensure!(extent.branch == segment.branch, "{extent:?} is another branch's");
                contents.own.push((extent.sequence, entry.len));
            }
            Data::Bucket(_) => {}
        }
    }
    Ok((contents, continued))
}

/// cladefs: a run of a segment's data an extent is sealed in: the extent, the run's offset and its
/// length, as footers name them.
type Run = (ExtentId, u64, u64);

/// What the commits since a segment opened made, as its footer says: their contents
/// ([`commit_contents`]) folded, each over those before, so that applying it to the branch's state
/// as of any of those commits gives its state as of the last. cladefs: one fold, of the commits as
/// they are made and of the records of a segment's log of them alike (TASK-165.9).
#[derive(Default)]
struct Window {
    /// Its first commit's epoch.
    first: u64,
    /// cladefs: its last commit's, if it has one: the commit its footer names (TASK-92.41).
    last: Option<u64>,
    /// The branch whose extents are stored on their own.
    branch: u64,
    /// The extents its commits sealed, which the branch holds as of its last, where they sealed
    /// them: in a run of its segment's data, at an offset, of a length, or on their own, of a
    /// length.
    sealed: BTreeMap<ExtentId, (Option<u64>, u64)>,
    /// The extents they freed.
    frees: BTreeSet<ExtentId>,
    /// The root and capacity they set last, if any did, and the forks at their ends.
    root: Option<Vec<u8>>,
    capacity: Option<u64>,
    forks: Vec<Child>,
    /// cladefs: the versions cut at their ends (TASK-476).
    versions: Vec<VersionCut>,
    /// cladefs: the runs of the commit its segment's footer goes on with, which the segments after
    /// it end, if any: the last thing it folds (TASK-92.41).
    continued: Option<Continued>,
}

impl Window {
    fn starting(branch: u64, first: u64) -> Self {
        Self { first, branch, ..Self::default() }
    }

    /// Folds `commit`'s contents over those of the commits before it: its frees take out what
    /// they sealed, and are the footer's; what it seals is no free of the footer's, though freed
    /// before, as readers apply its frees after its extents (TASK-79.1).
    fn fold(&mut self, commit: &Contents) {
        for extent in &commit.frees {
            self.sealed.remove(extent);
            self.frees.insert(*extent);
        }
        let own = commit
            .own
            .iter()
            .map(|&(sequence, len)| (ExtentId::new(self.branch, sequence), (None, len)));
        let runs =
            commit.extents.iter().map(|&(extent, offset, len)| (extent, (Some(offset), len)));
        for (extent, sealed) in runs.chain(own).collect::<Vec<_>>() {
            self.frees.remove(&extent);
            self.sealed.insert(extent, sealed);
        }
        if commit.root.is_some() {
            self.root.clone_from(&commit.root);
        }
        if commit.capacity.is_some() {
            self.capacity = commit.capacity;
        }
        self.forks.extend_from_slice(&commit.forks);
        self.versions.extend_from_slice(&commit.versions);
        if let Some(&last) = commit.commits.last() {
            self.last = Some(epoch_of(last));
        }
        if let Some(continued) = &commit.continued {
            debug_assert!(self.continued.is_none(), "a segment goes on with two commits");
            self.continued = Some(continued.clone());
        }
    }

    /// The footer's contents, of `commits`, naming `next_sequence`.
    fn contents(self, commits: Vec<u64>, next_sequence: u64) -> Contents {
        let (mut extents, mut own) = (Vec::new(), Vec::new());
        for (extent, (offset, len)) in self.sealed {
            match offset {
                Some(offset) => extents.push((extent, offset, len)),
                None => own.push((extent.sequence, len)),
            }
        }
        Contents {
            commits,
            extents,
            own,
            frees: self.frees.into_iter().collect(),
            root: self.root,
            next_sequence,
            forks: self.forks,
            capacity: self.capacity,
            continued: self.continued,
            versions: self.versions,
        }
    }
}

/// cladefs: the contents of a segment of `branch`'s whose commits' contents are `commits`, in order,
/// as its footer names them: their last commit, and the largest next sequence number they name
/// (TASK-165.9). The one fold of a segment's commits ([`Window`]), of a log of them as of the
/// commits as they are made.
pub(crate) fn fold(branch: u64, commits: &[Contents]) -> Contents {
    let mut window = Window::starting(branch, 0);
    for commit in commits {
        window.fold(commit);
    }
    // cladefs: the last a commit's record names: a segment's last record may go on with a commit,
    // naming none (TASK-92.41).
    let last = commits.iter().rev().find(|commit| !commit.commits.is_empty());
    let last = last.map_or(Vec::new(), |last| last.commits.clone());
    let next_sequence = commits.iter().map(|commit| commit.next_sequence).max().unwrap_or(0);
    window.contents(last, next_sequence)
}

/// cladefs: a segment's log made ahead of the segment, off the store's state lock: a spare log,
/// `spare`'s by its name, or a file made anew under `spare`'s name, its header `segment`'s, its
/// records salted `salt`, synced ([`Preparation::prepare`], TASK-333). Placed, if the segment is
/// still the next, renamed its log, beside its raw file ([`Segments::prepared`]).
pub(crate) struct Prepared<F> {
    segment: SegmentId,
    spare: SegmentId,
    file: F,
    salt: u64,
    encryption: Arc<RawEncryption>,
}

/// cladefs: the next segment's raw file and log, made ahead of it, there under their names, which
/// its opening takes, changing no directory ([`create_open`]); their entries made durable off the
/// store's state lock, `synced` once they are, as no commit then waits for (TASK-384). An opening
/// that renamed them, or made them, there, left their entries to the commits' next flush to sync,
/// each a commit of the host's journal that a durable commit waited for, one per segment opened.
pub(crate) struct Placed<F> {
    segment: SegmentId,
    raw: F,
    log: F,
    salt: u64,
    encryption: Arc<RawEncryption>,
    synced: bool,
}

/// cladefs: what of a preparation is left once its log is made ([`Segments::prepared`]).
pub(crate) enum Preparing {
    /// The segment's raw file and log placed, their entries to make durable, the preparation
    /// running until then ([`Segments::placed_synced`]).
    Placed(SegmentId),
    /// Its segment opened meanwhile, with a log of its own: the next's to prepare, of the log it
    /// made, the preparation running on (TASK-404).
    Again(Preparation),
    /// The preparation ended, a spare log's name to remove, if it left one the branch keeps not.
    Ended(Option<SegmentId>),
}

/// cladefs: the bit set in the number naming a log a preparation made anew ([`Preparation::prepare`]),
/// past every segment's number.
const MADE: u64 = 1 << 63;

/// cladefs: what to prepare of segment `segment`'s log ([`Segments::to_prepare`]): from `spare`'s
/// spare log, if any, else a file made anew under a spare's name of its own ([`MADE`]), salted
/// `salt`; cladefs: encrypted by `encryption`, with its raw file, if the store is encrypted
/// (TASK-411.5).
pub(crate) struct Preparation {
    segment: SegmentId,
    spare: Option<SegmentId>,
    salt: u64,
    encryption: Arc<RawEncryption>,
}

impl Preparation {
    /// Prepares the log in `dir`: its header written, then synced, under a spare's name, so that
    /// a crash leaves a spare, whatever its header says, which a log made of it writes over. Its
    /// sync waits behind the host's file system's journal, on a disk VMs share 0.1-1.4 s: it held
    /// the store's state lock in `create_open`, a write filling the open segment, every write and
    /// read of the store and its flushes waiting (TASK-333). Fails with the spare it took.
    pub fn prepare<D: Dir>(self, dir: &D) -> Result<Prepared<D::File>, (Option<SegmentId>, Error)> {
        let Self { segment, spare, salt, encryption } = self;
        // Where it runs, off the store's state lock, the segment's opening free to come first.
        fail::fail_point!("segments::preparing");
        // A file made anew under a name no segment's spare log has: a stale one, its segment
        // opened with another log, its spare later, would take the name, the file a preparation
        // held then gone from under it.
        let made = SegmentId { number: segment.number | MADE, ..segment };
        let named = spare.unwrap_or(made);
        let path = spare_path(named);
        let make = || -> io::Result<D::File> {
            let log = match spare {
                Some(_) => dir.open(&path, false)?,
                None => dir.create(&path, false)?,
            };
            write_header(&log, salt, segment, &encryption)?;
            Ok(log)
        };
        match make() {
            Ok(file) => Ok(Prepared { segment, spare: named, file, salt, encryption }),
            Err(error) => Err((spare, Error::new(error).context(format!("preparing {path}")))),
        }
    }
}

/// cladefs: writes the header of `segment`'s log, its records salted `salt`, to `log`, and syncs it
/// (`wal::log_header`), MACed by `encryption`.
fn write_header<F: File>(
    log: &F,
    salt: u64,
    segment: SegmentId,
    encryption: &RawEncryption,
) -> io::Result<()> {
    let header = log_header(salt, segment, encryption);
    log.write_at(&header, 0)?;
    crate::written::wrote(crate::written::Kind::Log, header.len());
    log.sync_cached()
}

/// cladefs: the phase of a new open segment, `segment`, whose raw file and log it makes in `dir`,
/// and whether their entries are durable: the raw file and log placed for it, if any, its log's
/// header durable, their entries too once their preparation synced them (TASK-333, TASK-384);
/// else a raw file made anew, and its log a spare, or a file made anew, its header written and
/// synced here.
fn create_open<D: Dir>(
    dir: &D,
    segment: SegmentId,
    (spares, placed): (&mut Vec<SegmentId>, &mut Option<Placed<D::File>>),
    salt: u64,
    encrypting: extent_bucket::layout::EncryptionKey<'_>,
) -> Result<(Phase<D::File>, bool), Error> {
    if let Some(ready) = placed.take_if(|ready| ready.segment == segment && ready.salt == salt) {
        let raw = Arc::new(RawFile::new(ready.raw, ready.encryption.clone()));
        let log = Log { file: Arc::new(ready.log), salt, encryption: ready.encryption };
        return Ok((Phase::Open { raw, log }, ready.synced));
    }
    // cladefs: encrypted, as every store is, under its branch's newest DEK (TASK-411.5).
    let encryption = new_encryption(encrypting);
    let raw =
        Arc::new(RawFile::new(dir.create(&open_segment_path(segment), true)?, encryption.clone()));
    // cladefs: its log a spare, if there is one, its header written over the one there (TASK-301):
    // durable before any record is appended, and before the spare is renamed its log: renamed
    // first, a crash, or a failure, before the header was durable left the spare's header, and the
    // records it salts, its last segment's, under this segment's name, which an opening took for
    // this segment's commits, their runs past its raw data (TASK-310's crash round 270).
    let path = log_path(segment);
    let headed = |log: D::File| -> io::Result<D::File> {
        write_header(&log, salt, segment, &encryption)?;
        Ok(log)
    };
    let reused = spares.pop().map(|spare| {
        let spare = spare_path(spare);
        let log = headed(dir.open(&spare, false)?)?;
        dir.rename(&spare, &path)?;
        Ok::<_, io::Error>(log)
    });
    let log = match reused {
        Some(Ok(log)) => log,
        Some(Err(error)) => {
            log::warn!("extent store: reusing a spare log for {segment:?}: {error}");
            headed(dir.create(&path, false)?)?
        }
        None => headed(dir.create(&path, false)?)?,
    };
    Ok((Phase::Open { raw, log: Log { file: Arc::new(log), salt, encryption } }, false))
}

/// cladefs: the encryption of a new segment's raw file and log, under `encrypting`'s DEK, its branch's
/// newest, from a salt of their own (TASK-411.5).
fn new_encryption((dek, key): extent_bucket::layout::EncryptionKey<'_>) -> Arc<RawEncryption> {
    Arc::new(RawEncryption::new(dek, key))
}

/// The store's epoch of the last commit of the segment whose footer is `footer`. cladefs: the one it
/// goes on with, if any, past those it ends, its data that commit's too: it uploads once that
/// commit is durable, and is acknowledged once it is (TASK-92.41).
pub(crate) fn footer_last(footer: &Footer) -> u64 {
    let contents = &footer.contents;
    let continued = contents.continued.as_ref().map(|continued| continued.commit);
    continued.or(contents.commits.last().copied()).map_or(0, epoch_of)
}

/// The footer of a closed segment, `bytes`: none if the segment closed with none, as one whose
/// footer named commits not durable, which opening its branch closes again.
pub(crate) fn segment_footer(bytes: &[u8]) -> Result<Option<Footer>, Error> {
    if bytes.is_empty() {
        return Ok(None);
    }
    Ok(Some(Footer::decode(bytes).context("a segment's footer")?))
}

/// cladefs: the segments this process's stores closed, and the nanoseconds closing them took.
static SEGMENT_CLOSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static SEGMENT_CLOSE_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// cladefs: of those, the segments framed in place ([`frame_in_place`], TASK-310).
static IN_PLACE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// cladefs: the bytes of segments' data their framings read (TASK-400).
static FRAMING_READ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// cladefs: the bytes of segments' data this process's stores' framings read since it started, and
/// the bytes looks ahead at their parts read as they filled, which framings in place read no more
/// (TASK-400).
pub fn framing_reads() -> (u64, u64) {
    let read = FRAMING_READ.load(std::sync::atomic::Ordering::Relaxed);
    (read, framing::seen_ahead())
}

/// cladefs: how many segments this process's stores framed in place since it started, none of their
/// parts compressing, their data written once (TASK-310).
pub fn segments_framed_in_place() -> u64 {
    IN_PLACE.load(std::sync::atomic::Ordering::Relaxed)
}

/// cladefs: the commits this process's stores made that spanned segments, closing segments filled
/// since the commit before ([`Segments::close_full`], TASK-92.41).
static SPANNING: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// cladefs: how many commits this process's stores made since it started that spanned segments
/// (TASK-92.41): what tests of random workloads count, to see that they make such commits.
pub fn commits_spanning_segments() -> u64 {
    SPANNING.load(std::sync::atomic::Ordering::Relaxed)
}

/// cladefs: how many segments this process's stores closed since it started, and the nanoseconds
/// framing them took: their data read, framed and written, off the commits that closed them, and
/// off the store's locks for those closed between commits
/// ([`Store::close_now`](crate::Store::close_now)'s).
pub fn segment_closes() -> (u64, u64) {
    let load =
        |count: &std::sync::atomic::AtomicU64| count.load(std::sync::atomic::Ordering::Relaxed);
    (load(&SEGMENT_CLOSES), load(&SEGMENT_CLOSE_NS))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dir::read_file;
    use crate::framed::read_whole;
    use crate::layout::create_branch_dirs;
    use crate::{HeldParts, MemDir, MemFile};

    const BLOCK: u64 = SECTOR;
    const BRANCH: extent_bucket::layout::Branch = extent_bucket::layout::test_branch(7);

    fn segment(number: u64) -> SegmentId {
        SegmentId { branch: BRANCH.id, number }
    }

    /// The segments of a branch with none closed, `open_files` of whose files stay open.
    fn opened(dir: &MemDir, open_files: usize) -> Segments<MemFile> {
        create_branch_dirs(dir, BRANCH.id).unwrap();
        let chain = Chain::new(BRANCH, 0, 0, 0);
        Segments::open(
            dir,
            (BRANCH.id, BTreeMap::new()),
            (0, 0, chain),
            (open_files, extent_bucket::testing::ring([BRANCH])),
        )
        .unwrap()
    }

    /// `unframed`, framed and made durable, as a store's framings make it ([`make_durable`]).
    fn framed_durably(dir: &MemDir, unframed: Unframed<MemFile>) -> Arc<Framed<MemFile>> {
        let number = unframed.number();
        let framed = unframed.frame(dir, Vec::new()).unwrap();
        make_durable(dir, BRANCH.id, (&[(number, framed.clone())], false), Instant::now()).unwrap();
        framed
    }

    /// Writes a block of `byte`s to the open segment, which holds it, and returns its piece.
    fn write(segments: &mut Segments<MemFile>, byte: u8) -> Piece {
        let (number, file, at) = segments.reserve(BLOCK).unwrap();
        file.write_at(&[byte; BLOCK as usize], at).unwrap();
        segments.held(number, (at, BLOCK));
        Piece { segment: number, offset: at, extent_offset: 0, len: BLOCK }
    }

    /// Segment `number`'s data, as its file holds it.
    /// TASK-400: looks ahead at `seeing`, parts of segments written whole, as the store's framer
    /// does, holding in `held` those it packs.
    fn look_ahead(
        segments: &mut Segments<MemFile>,
        held: &Arc<HeldParts>,
        seeing: Vec<(SegmentId, u64)>,
    ) {
        for (number, part) in seeing {
            let (raw, look) = segments.see(number).expect("a part to look at");
            let (seen, packed) = framing::see(&*raw, part, look).unwrap();
            segments.saw(number, part, seen, packed.map(|packed| held.hold(packed)));
        }
    }

    fn data(segments: &mut Segments<MemFile>, dir: &MemDir, number: SegmentId) -> Vec<u8> {
        match segments.source(dir, number).unwrap() {
            Source::Raw(file) => read_file(&*file).unwrap(),
            Source::Framed(framed) => extent_frame::data(
                &read_whole(&*framed.file).unwrap(),
                &extent_bucket::testing::ring([BRANCH]).of(BRANCH.id),
            )
            .unwrap(),
        }
    }

    /// A closed segment is read from its raw file while it is being framed, which its phase keeps
    /// open, and from its framed file once framed, which closes once idle: as more files are open
    /// than allowed, and no read uses it. The open segment's never does.
    #[test]
    fn closing_frames_through_framing_to_closed() {
        let dir = MemDir::new();
        let mut segments = opened(&dir, 1);
        write(&mut segments, 1);
        let Source::Raw(raw) = segments.source(&dir, segment(0)).unwrap() else {
            panic!("the open segment is read framed")
        };
        let unframed = segments.close(&dir, 1, 0).unwrap();
        // Two files open, the most one: but that being framed stays open.
        assert!(segments.is_framing(segment(0)));
        let Source::Raw(read) = segments.source(&dir, segment(0)).unwrap() else {
            panic!("a segment being framed is read framed")
        };
        assert!(Arc::ptr_eq(&read, &raw));
        drop((read, raw));

        let framed = framed_durably(&dir, unframed);
        segments.framed(segment(0), framed.clone());
        assert!(!segments.is_framing(segment(0)));
        let Source::Framed(read) = segments.source(&dir, segment(0)).unwrap() else {
            panic!("a segment framed is read raw")
        };
        assert!(Arc::ptr_eq(&read, &framed));
        let reading = framed.file.clone();
        let first = Arc::downgrade(&framed);
        drop((read, framed));

        // Read meanwhile, it stays open as the next closes.
        write(&mut segments, 2);
        let unframed = segments.close(&dir, 2, 0).unwrap();
        assert!(first.upgrade().is_some());
        drop(reading);
        segments.framed(segment(1), framed_durably(&dir, unframed));

        // Idle, it closed as the next was framed, the least lately read; that being framed and the
        // open one stay open.
        write(&mut segments, 3);
        let unframed = segments.close(&dir, 3, 0).unwrap();
        assert!(first.upgrade().is_none());
        assert!(matches!(segments.source(&dir, segment(2)).unwrap(), Source::Raw(_)));
        assert!(matches!(segments.source(&dir, segment(3)).unwrap(), Source::Raw(_)));
        // Opened again, framed.
        assert!(matches!(segments.source(&dir, segment(0)).unwrap(), Source::Framed(_)));
        let first_bytes = [0, 1, 2].map(|number| data(&mut segments, &dir, segment(number))[0]);
        assert_eq!(first_bytes, [1, 2, 3]);
        drop(unframed);
    }

    /// cladefs: the open segment takes whole sectors only: what is not is refused, the segment
    /// unchanged, the next write going where it would have (TASK-411.8).
    #[test]
    fn open_segments_take_whole_sectors() {
        let dir = MemDir::new();
        let mut segments = opened(&dir, 64);
        let refused = segments.reserve(SECTOR / 8).map(|_| ()).unwrap_err();
        assert_eq!(refused, Unsectored { offset: 0, len: 512 });
        let refused = segments.append(SECTOR + 1).map(|_| ()).unwrap_err();
        assert_eq!(refused, Unsectored { offset: 0, len: 4097 });
        assert_eq!(write(&mut segments, 1).offset, 0);
        assert_eq!(segments.append(SECTOR).unwrap().1, SECTOR);
        assert_eq!(segments.reserve(SECTOR).unwrap().2, 2 * SECTOR);
    }

    /// cladefs: TASK-400: a segment's parts packed ahead are held while the parts seen say it will be
    /// framed in a copy, and let go of all at once when they say it will be framed in place: a part
    /// of text held, then, noise outweighing it, none, nor any after; its framing takes no hold.
    #[test]
    fn parts_held_go_once_the_segment_looks_in_place() {
        let dir = MemDir::new();
        let mut segments = opened(&dir, 64);
        let held = HeldParts::new();
        let bytes = |seed: u64, text: bool| -> Vec<u8> {
            let mut state = (seed + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15);
            let words = (0..PART / 8).map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state
            });
            let noise = words.flat_map(u64::to_le_bytes);
            noise.map(|byte| if text { b'a' + byte % 16 } else { byte }).collect()
        };
        let mut held_bytes = Vec::new();
        for part in 0..12 {
            let (number, file, at) = segments.reserve(PART).unwrap();
            file.write_at(&bytes(part, part == 0), at).unwrap();
            segments.held(number, (at, PART));
            let seeing = segments.take_seeing();
            look_ahead(&mut segments, &held, seeing);
            held_bytes.push(held.bytes());
        }
        assert!(held_bytes[0] > 0 && held_bytes[0] < PART, "the text, compressed: {held_bytes:?}");
        assert!(held_bytes[6] > held_bytes[0], "noise held raw, the parts seen saving enough");
        assert_eq!(held_bytes[11], 0, "{held_bytes:?}");
        let ahead = segments.take_seen(segment(0));
        assert_eq!(ahead.len(), 12);
        assert!(ahead.iter().flatten().all(|looked| looked.held.is_none()));
    }

    /// cladefs: TASK-400: each part of a segment is looked at ahead once its last write ended, the
    /// writes to it ending in any order, or appended; its framing takes what was seen, after which
    /// nothing is, and frames it in place reading only the parts not seen, its last, part written,
    /// its data the same as framed from its raw file alone. Before, a framing read its whole raw
    /// file back as it closed.
    #[test]
    fn framings_in_place_read_only_the_parts_not_seen_ahead() {
        let dir = MemDir::new();
        let mut segments = opened(&dir, 64);
        // Bytes that don't compress, of the place they go.
        let noise = |at: u64, len: u64| -> Vec<u8> {
            let mut state = (at + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15);
            let words = (0..len / 8).map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state
            });
            words.flat_map(u64::to_le_bytes).collect()
        };
        let reserved = |segments: &mut Segments<MemFile>, len| {
            let (number, file, at) = segments.reserve(len).unwrap();
            file.write_at(&noise(at, len), at).unwrap();
            (number, at, len)
        };
        let (half, whole) = (PART / 2, PART);
        // Part 0 in two writes, the later ending first; part 1 in one; part 2 appended; part 3 a
        // block, never whole.
        let first = reserved(&mut segments, half);
        let second = reserved(&mut segments, half);
        segments.held(second.0, (second.1, second.2));
        assert_eq!(segments.take_seeing(), [], "part 0 half written");
        segments.held(first.0, (first.1, first.2));
        let one = reserved(&mut segments, whole);
        segments.held(one.0, (one.1, one.2));
        let (file, at) = segments.append(whole).unwrap();
        file.write_at(&noise(at, whole), at).unwrap();
        segments.appended(segment(0), at, whole);
        let last = reserved(&mut segments, BLOCK);
        segments.held(last.0, (last.1, last.2));
        let seeing = segments.take_seeing();
        assert_eq!(seeing, [(segment(0), 0), (segment(0), 1), (segment(0), 2)]);
        let held = HeldParts::new();
        look_ahead(&mut segments, &held, seeing);
        assert_eq!(held.bytes(), 0, "noise, framed in place, held not");
        let unframed = segments.close(&dir, 1, 0).unwrap();
        let raw_data = data(&mut segments, &dir, segment(0));
        let seen = segments.take_seen(segment(0));
        assert_eq!(seen.iter().map(Option::is_some).collect::<Vec<_>>(), [true; 3]);
        assert!(segments.see(segment(0)).is_none(), "its framing took what was seen");
        // What the framing read, not the audit's reading again of what was seen (`framing::read`).
        let before = (framing_reads().0, segments_framed_in_place());
        let framed = unframed.frame(&dir, seen).unwrap();
        assert_eq!(framing_reads().0 - before.0, BLOCK, "the last part alone read");
        assert_eq!(segments_framed_in_place() - before.1, 1, "framed in place");
        make_durable(&dir, BRANCH.id, (&[(segment(0), framed.clone())], false), Instant::now())
            .unwrap();
        segments.framed(segment(0), framed);
        assert_eq!(data(&mut segments, &dir, segment(0)), raw_data, "framed as it was written");
    }

    /// The segments of an encrypted branch, under `dek`, with none closed, and its keys.
    fn encrypted_opened(dir: &MemDir, dek: [u8; 32]) -> (Segments<MemFile>, Arc<Ring>) {
        create_branch_dirs(dir, BRANCH.id).unwrap();
        let chain = Chain::new(BRANCH, 0, 0, 0);
        let keys =
            Ring::encrypting_for_tests(BRANCH.id, 1, extent_frame::Secret::from_bytes(&dek), []);
        let tracked = (BRANCH.id, BTreeMap::new());
        let segments = Segments::open(dir, tracked, (0, 0, chain), (64, keys.clone())).unwrap();
        (segments, keys)
    }

    /// `len` bytes of xorshift64's, from `seed`: noise, or, if `text`, letters at random, sixteen
    /// of them, which compress by about half.
    fn bytes_of(seed: u64, len: u64, text: bool) -> Vec<u8> {
        let mut state = (seed + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let words = (0..len / 8).map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        });
        let noise = words.flat_map(u64::to_le_bytes);
        noise.map(|byte| if text { b'a' + byte % 16 } else { byte }).collect()
    }

    /// Whether any file of `dir` holds a sector's first 32 bytes of `data`, in the clear.
    fn in_the_clear(dir: &MemDir, data: &[u8]) -> Vec<String> {
        let pieces: std::collections::HashSet<&[u8]> =
            data.chunks(SECTOR as usize).map(|sector| &sector[..32]).collect();
        (dir.files().into_iter())
            .filter(|(_, bytes)| bytes.windows(32).any(|window| pieces.contains(window)))
            .map(|(path, _)| path)
            .collect()
    }

    /// cladefs: TASK-400 in an encrypted store (TASK-411.23, TASK-421): an encrypted segment's parts that
    /// compress, written whole or appended, are looked at ahead, packed and held, in the clear, in
    /// memory; its framing in a copy reads none of them back, and encrypts them under its object's
    /// key, the bytes it framed opening to its data under the branch's key. No file holds its
    /// data in the clear.
    #[test]
    fn encrypted_segments_parts_are_held_ahead_and_encrypted_framing() {
        let dir = MemDir::new();
        let (mut segments, keys) = encrypted_opened(&dir, [3; 32]);
        let text = bytes_of(1, 3 * PART, true);
        let (number, file, at) = segments.reserve(2 * PART).unwrap();
        file.write_at(&text[..2 * PART as usize], at).unwrap();
        segments.held(number, (at, 2 * PART));
        let (file, at) = segments.append(PART).unwrap();
        file.write_at(&text[2 * PART as usize..], at).unwrap();
        segments.appended(number, at, PART);
        let seeing = segments.take_seeing();
        assert_eq!(seeing, [(number, 0), (number, 1), (number, 2)]);
        let held = HeldParts::new();
        look_ahead(&mut segments, &held, seeing);
        let holding = held.bytes();
        assert!(holding > PART && holding < 2 * PART, "compressed, held: {holding}");
        let unframed = segments.close(&dir, 1, 0).unwrap();
        let ahead = segments.take_seen(number);
        assert_eq!(
            ahead.iter().map(|looked| looked.as_ref().unwrap().held.is_some()).collect::<Vec<_>>(),
            [true; 3]
        );
        let before = (framing_reads().0, segments_framed_in_place());
        let framed = unframed.frame(&dir, ahead).unwrap();
        assert_eq!(framing_reads().0 - before.0, 0, "none read back");
        assert_eq!(segments_framed_in_place() - before.1, 0, "framed in a copy");
        assert_eq!(held.bytes(), 0, "taken");
        let object = read_whole(&*framed.file).unwrap();
        assert!(object.len() < 2 * PART as usize, "compressed: {} bytes", object.len());
        let encryption = extent_frame::Frame::of(&object).unwrap().encryption;
        assert!(
            matches!(encryption, extent_frame::Encryption::Encrypted { key, .. } if key == extent_frame::KeyId { branch: BRANCH.id, generation: 1 }),
            "{encryption:?}"
        );
        assert_eq!(extent_frame::data(&object, &keys.of(BRANCH.id)).unwrap(), text);
        assert_eq!(in_the_clear(&dir, &text), Vec::<String>::new());
    }

    /// cladefs: TASK-411.23: an encrypted segment of noise is framed in place as a clear one is (TASK-310,
    /// TASK-400): its parts seen ahead as stored, the last alone read, its raw file its framed file,
    /// nothing of its data written again but its tail; the object of its raw file's XTS sectors,
    /// under the raw file's DEK and salt, which opens to its data under the branch's key, to none
    /// without it, and holds none of it in the clear.
    #[test]
    fn encrypted_segments_of_noise_are_framed_in_place() {
        let dir = MemDir::new();
        let (mut segments, keys) = encrypted_opened(&dir, [4; 32]);
        let noise = bytes_of(2, 3 * PART + BLOCK, false);
        let (number, file, at) = segments.reserve(3 * PART).unwrap();
        file.write_at(&noise[..3 * PART as usize], at).unwrap();
        segments.held(number, (at, 3 * PART));
        let (number, file, at) = segments.reserve(BLOCK).unwrap();
        file.write_at(&noise[3 * PART as usize..], at).unwrap();
        segments.held(number, (at, BLOCK));
        let raw = file.file().clone();
        let salt = file.sectors().salt();
        let held = HeldParts::new();
        let seeing = segments.take_seeing();
        look_ahead(&mut segments, &held, seeing);
        assert_eq!(held.bytes(), 0, "noise, held not");
        let unframed = segments.close(&dir, 1, 0).unwrap();
        let seen = segments.take_seen(number);
        let before = (framing_reads().0, segments_framed_in_place(), crate::written().framed);
        let framed = unframed.frame(&dir, seen).unwrap();
        assert_eq!(framing_reads().0 - before.0, BLOCK, "the last part alone read");
        assert_eq!(segments_framed_in_place() - before.1, 1, "framed in place");
        assert!(Arc::ptr_eq(&framed.file, &raw), "its raw file, its framed file");
        let object = read_whole(&*framed.file).unwrap();
        let tail = object.len() as u64 - noise.len() as u64;
        assert_eq!(crate::written().framed - before.2, tail, "its tail alone written");
        let frame = extent_frame::Frame::of(&object).unwrap();
        let key = extent_frame::KeyId { branch: BRANCH.id, generation: 1 };
        assert_eq!(frame.encryption, extent_frame::Encryption::Sectors { key, salt });
        assert_eq!(extent_frame::data(&object, &keys.of(BRANCH.id)).unwrap(), noise);
        let locked = extent_frame::data(&object, &extent_frame::NoKeys);
        assert_eq!(locked, Err(extent_frame::Error::NoKey(key)));
        assert_eq!(in_the_clear(&dir, &noise), Vec::<String>::new());
        make_durable(&dir, BRANCH.id, (&[(number, framed.clone())], false), Instant::now())
            .unwrap();
        segments.framed(number, framed);
        let Source::Framed(read) = segments.source(&dir, number).unwrap() else { panic!() };
        let parts = &read.parts;
        let at = PART + 5;
        let range = parts.read(at..at + 100, |range| {
            let mut bytes = vec![0; (range.end - range.start) as usize];
            read.file.read_at(&mut bytes, range.start).unwrap();
            Ok::<_, extent_frame::Error>(bytes)
        });
        assert_eq!(range.unwrap(), &noise[at as usize..at as usize + 100]);
    }

    /// Footers are stamped by the writer's chain the segments opened with, numbered on from it,
    /// each following the one before, of its writer's epoch, the next sequence number they name
    /// its floor at least; the segments are numbered on from the number they opened with, whatever
    /// the chain's (TASK-165.9).
    #[test]
    fn footers_chain_from_their_writers_chain() {
        use extent_frame::segment::tag;
        let dir = MemDir::new();
        create_branch_dirs(&dir, BRANCH.id).unwrap();
        let chain = Chain::new(BRANCH, 3, 5, 100);
        let segments = Segments::open(
            &dir,
            (BRANCH.id, BTreeMap::new()),
            (0, 2, chain),
            (64, extent_bucket::testing::ring([BRANCH])),
        );
        let mut segments = segments.unwrap();
        assert_eq!(segments.open_segment(), segment(2));
        let close = |segments: &mut Segments<MemFile>, epoch| {
            let footer = segments.close(&dir, epoch, 7).unwrap().footer;
            (footer.segment, footer.epoch, footer.contents.commits, footer.contents.next_sequence)
        };
        assert_eq!(close(&mut segments, 3), (5, 3, vec![tag(3, 3)], 100));
        assert_eq!(close(&mut segments, 4), (6, 3, vec![tag(3, 4)], 100));
        assert_eq!(segments.open_segment(), segment(4));
    }

    /// Reaping lets go of the closed segments acknowledged in the bucket no extent's data is in any
    /// more, which no write is in progress in: not the open segment, nor one being framed, nor one
    /// the write-ahead log holds, not yet acknowledged (TASK-165.9).
    #[test]
    fn reaping_keeps_open_framing_and_unacknowledged_segments() {
        let dir = MemDir::new();
        let mut segments = opened(&dir, 64);
        // Written, its write abandoned: no extent is in it.
        let (number, _, at) = segments.reserve(BLOCK).unwrap();
        segments.dropped(number, (at, BLOCK));
        let empty = segments.close(&dir, 1, 0).unwrap();
        // Holding an extent.
        let held = write(&mut segments, 1);
        let holding = segments.close(&dir, 2, 0).unwrap();
        // A write in progress, which a closing between commits did not wait for.
        let (writing, _, writing_at) = segments.reserve(BLOCK).unwrap();
        let unwritten = segments.close(&dir, 3, 0).unwrap();
        let reap = |segments: &mut Segments<MemFile>| segments.reap();
        for (number, unframed) in [(0, empty), (1, holding), (2, unwritten)] {
            segments.framed(segment(number), framed_durably(&dir, unframed));
        }
        // Not acknowledged, they stay.
        assert_eq!(reap(&mut segments), []);
        for number in 0..3 {
            let key = (number != 1).then_some("key");
            segments.acknowledge(segment(number), key);
        }
        assert_eq!(reap(&mut segments), [(segment(0), Some("key".to_owned()))]);
        segments.let_go(&held);
        segments.dropped(writing, (writing_at, BLOCK));
        let reaped = reap(&mut segments);
        assert_eq!(reaped, [(segment(1), None), (segment(2), Some("key".to_owned()))]);
        for number in 0..3 {
            assert!(!segments.tracked.contains_key(&segment(number)), "{number} tracked still");
        }
        assert!(segments.tracked.contains_key(&segments.open_segment()));
    }

    /// cladefs: a segment filled between commits takes no more writes, which go to the next, reads
    /// from its raw file, and is never reaped while it waits; the next commit seals in place what is
    /// one run in it, and closes it, its footer naming those runs as the commit's, which the open
    /// segment ends, after the commits made in it before it filled; its raw file synced by the
    /// commit's flush. One no extent's data is in is framed with no data, its footer alone
    /// (TASK-92.41; it was set aside, holding no commit, and reaped, TASK-79.2).
    #[test]
    fn segments_filled_wait_for_the_next_commit() {
        use crate::layout::Extent;
        use extent_frame::segment::tag;
        let dir = MemDir::new();
        let mut segments = opened(&dir, 64);
        let (first, second) = (ExtentId::new(BRANCH.id, 1), ExtentId::new(BRANCH.id, 2));
        let mut extents = Extents::default();
        let mut commit = |segments: &mut Segments<MemFile>, epoch, extent, piece: Piece| {
            let data = Data::Pieces(vec![piece]);
            extents.insert(extent, Extent { len: BLOCK, data: data.clone() });
            let changes = [Change::Write { extent, piece }];
            let records = segments.record(epoch, &changes, (None, None), &extents, epoch + 1);
            segments.place(extent, &data);
            records.unwrap()
        };
        let piece = write(&mut segments, 1);
        commit(&mut segments, 1, first, piece);
        let written = write(&mut segments, 2);
        segments.fill(&dir, 1).unwrap();
        assert!(segments.has_commits(1), "the commit made in it before it filled");
        segments.fill(&dir, 1).unwrap();
        assert_eq!(segments.open_segment(), segment(2));
        let Source::Raw(raw) = segments.source(&dir, segment(0)).unwrap() else {
            panic!("a segment filled is read framed")
        };
        assert_eq!(data(&mut segments, &dir, segment(0))[BLOCK as usize], 2);
        assert_eq!(write(&mut segments, 3).segment, segment(2));
        assert_eq!(Arc::strong_count(&raw), 2, "its phase holds its raw file");
        drop(raw);
        assert_eq!(segments.reap().len(), 0);
        // The next commit seals the second extent where it is, in the first segment filled.
        let records = commit(&mut segments, 2, second, written);
        let continued = |runs| Some(Continued { commit: tag(0, 2), extents: runs });
        let going_on = |runs| Contents { continued: continued(runs), ..Contents::default() };
        let ended = Contents { commits: vec![tag(0, 2)], next_sequence: 3, ..Contents::default() };
        let expected = vec![
            (segment(0), going_on(vec![(second, BLOCK, BLOCK)])),
            (segment(1), going_on(vec![])),
            (segment(2), ended),
        ];
        assert_eq!(records, expected);
        let closed = segments.close_full(2, 3);
        let footers: Vec<_> = closed.iter().map(|closed| closed.footer.contents.clone()).collect();
        let held = Contents {
            commits: vec![tag(0, 1)],
            extents: vec![(first, 0, BLOCK)],
            next_sequence: 3,
            ..going_on(vec![(second, BLOCK, BLOCK)])
        };
        assert_eq!(footers, [held, Contents { next_sequence: 3, ..going_on(vec![]) }]);
        assert_eq!(closed.iter().map(Unframed::len).collect::<Vec<_>>(), [2 * BLOCK, 0]);
        let mut syncs = Syncs::default();
        segments.take_syncs(&mut syncs);
        assert_eq!(syncs.files().1.len(), 3, "the raw files of the two filled and the open one");
        for unframed in closed {
            let number = unframed.number();
            segments.framed(number, framed_durably(&dir, unframed));
        }
        assert_eq!(data(&mut segments, &dir, segment(0))[BLOCK as usize], 2);
    }

    /// cladefs: the next segment's log prepared off the state lock, a filling opens the next with
    /// it, syncing nothing; one prepared for a segment opened meanwhile, the opening having made
    /// its own, goes on as the next's, as does one whose segment opened before its entries were
    /// durable, no opening beginning another while it runs (TASK-404); and every log's header is
    /// its segment's (TASK-333). Before, the filling wrote and synced the header, holding the
    /// store's state lock; and the preparation ended there, leaving the next segment nothing
    /// prepared until another flush or closing began one.
    #[test]
    fn segments_open_with_their_logs_prepared_syncing_nothing() {
        let dir = MemDir::new();
        let mut segments = opened(&dir, 64);
        let headed = |number: u64| {
            let log = dir.open(&log_path(segment(number)), false).unwrap();
            let mut header = [0; crate::wal::LOG_HEADER];
            log.read_cached(&mut header, 0).unwrap();
            crate::wal::log_salt(&header, segment(number)).is_some()
        };
        // Made, then placed, its entries made durable, as the store's opener does: what of the
        // preparation goes on.
        let prepare = |segments: &mut Segments<MemFile>, preparation: Preparation| match segments
            .prepared(&dir, preparation.prepare(&dir))
        {
            Preparing::Placed(placed) => segments.placed_synced(placed, true),
            Preparing::Again(next) => Some(next),
            Preparing::Ended(stale) => panic!("the preparation ended, {stale:?} to remove"),
        };
        let preparation = segments.to_prepare().expect("the next's log to prepare");
        assert!(segments.to_prepare().is_none(), "one at a time");
        assert!(prepare(&mut segments, preparation).is_none(), "segment 1's placed");
        assert!(segments.to_prepare().is_none(), "the next's prepared");
        write(&mut segments, 1);
        let before = dir.io().synced_files;
        segments.fill(&dir, 1).unwrap();
        assert_eq!(dir.io().synced_files - before, 0, "a filling syncs nothing");
        assert!(headed(1), "segment 1's log, its header its own");
        // Its next's being prepared, a filling opens it with a log of its own, its header synced.
        let preparation = segments.to_prepare().expect("segment 2's log to prepare");
        write(&mut segments, 2);
        let before = dir.io().synced_files;
        segments.fill(&dir, 1).unwrap();
        assert_eq!(dir.io().synced_files - before, 1, "the header's sync, nothing prepared");
        assert!(headed(2), "segment 2's log, its header its own");
        assert!(segments.to_prepare().is_none(), "segment 2's preparing still");
        // Late, it goes on as segment 3's, of the log it made.
        let again = prepare(&mut segments, preparation).expect("segment 3's log to prepare");
        let made = SegmentId { number: 2 | MADE, ..segment(2) };
        assert_eq!((again.segment, again.spare), (segment(3), Some(made)));
        assert!(segments.to_prepare().is_none(), "one at a time");
        assert!(prepare(&mut segments, again).is_none(), "segment 3's placed");
        assert!(segments.spares.is_empty(), "no spare left of segment 2's");
        write(&mut segments, 3);
        segments.fill(&dir, 1).unwrap();
        assert!(headed(3), "segment 3's log, of segment 2's late, its header its own");
        // Placed, its segment opened before its entries were durable: it goes on as the next's.
        let preparation = segments.to_prepare().expect("segment 4's log to prepare");
        let Preparing::Placed(placed) = segments.prepared(&dir, preparation.prepare(&dir)) else {
            panic!("segment 4's placed");
        };
        write(&mut segments, 4);
        segments.fill(&dir, 1).unwrap();
        assert!(headed(4), "segment 4's log, placed, its header its own");
        assert!(segments.to_prepare().is_none(), "segment 4's preparing still");
        let again = segments.placed_synced(placed, true).expect("segment 5's log to prepare");
        assert_eq!(again.segment, segment(5));
        assert!(prepare(&mut segments, again).is_none(), "segment 5's placed");
        assert_eq!(segments.placed(), Some(segment(5)));
    }

    /// cladefs: a segment opened has its entries unsynced until a sync that began after it opened
    /// syncs them, a commit's flush's after it opened, or a making of it durable; one opened since
    /// stays so; and a making of segments durable syncs those of theirs alone (TASK-310,
    /// TASK-369).
    #[test]
    fn entries_stay_unsynced_until_synced_after_they_were_made() {
        let dir = MemDir::new();
        let mut segments = opened(&dir, 64);
        let all = || [0, 1, 2, 3].map(segment);
        write(&mut segments, 1);
        drop(segments.close(&dir, 1, 0).unwrap());
        assert_eq!(segments.unsynced_entries(all()), BTreeSet::from([segment(1)]));
        // The flush after it opened syncs them: the commit's syncs say so.
        let synced = segments.take_syncs(&mut Syncs::default());
        assert_eq!(synced, BTreeSet::from([segment(1)]));
        assert_eq!(
            segments.take_syncs(&mut Syncs::default()),
            BTreeSet::new(),
            "none closed since"
        );
        write(&mut segments, 2);
        drop(segments.close(&dir, 2, 0).unwrap());
        segments.entries_synced(&synced);
        assert_eq!(segments.unsynced_entries(all()), BTreeSet::from([segment(2)]));
        assert_eq!(segments.unsynced_entries([segment(1)]), BTreeSet::new(), "its own alone");
    }

    /// A commit syncs the open segment's file if it was written since the last commit, keeping
    /// its pages cached (TASK-92.23), and the directories of open segments and of logs if one
    /// closed, but not of framed ones, which framings sync (TASK-310); cladefs: and the raw file of
    /// one closed, if written since, which its commits' records name the data of (TASK-165.9).
    #[test]
    fn only_written_open_segments_are_synced() {
        let dir = MemDir::new();
        let mut segments = opened(&dir, 64);
        let syncs = |segments: &mut Segments<MemFile>| {
            let mut syncs = Syncs::default();
            segments.take_syncs(&mut syncs);
            let (files, cached) = syncs.files();
            ((files.len(), cached.len()), syncs.dirs().iter().cloned().collect::<Vec<_>>())
        };
        assert_eq!(syncs(&mut segments), ((0, 0), vec![]));
        write(&mut segments, 1);
        assert_eq!(syncs(&mut segments), ((0, 1), vec![]));
        assert_eq!(syncs(&mut segments), ((0, 0), vec![]));
        write(&mut segments, 2);
        segments.close(&dir, 1, 0).unwrap();
        let dirs = vec![branch_dir(LOGS, BRANCH.id), branch_dir(OPEN_SEGMENTS, BRANCH.id)];
        assert_eq!(syncs(&mut segments), ((0, 1), dirs));
        segments.append(BLOCK).unwrap();
        assert_eq!(syncs(&mut segments), ((0, 1), vec![]));
    }
}
