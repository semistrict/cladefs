// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a replicated cell's writer's side (doc-21, TASK-537.3.3): each flush's commits sent to
//! the members of its configuration, and the commits replicated once f of them, with the writer's
//! own copy f+1, hold them synced.
//!
//! What a member holds of the writer's is two logs a segment, by their numbers in its directory:
//! the segment's replica log, [`log_of`] (`wal::ReplicaLog`: the local log's header, then every
//! record held whole), and the extents its commits stored on their own, [`own_of`], each framed
//! ([`own_frame`]). A taker writes them back as the segment's log and own extents' files beside an
//! empty raw file, and recovers the segment as an opening recovers its own.
//!
//! A replicator starts as its cell opens after a taking, whose opening closed every segment
//! before (`wal::recover`), so that each segment it streams it streams from its log's header. The
//! commits before it started are on no member: none is told replicated before the bucket holds
//! them ([`Replicator::base`]), so that what a guest is told replicated is a prefix of the cell's
//! commits, each in the bucket or on f+1 copies.
//!
//! Each member is sent what it lacks, by a task of its own, through a [`MemberLink`], in pieces of
//! [`PIECE`] at most (or, in the simulation, one turn at a time as its scheduler says,
//! [`Replicator::idle`] and [`Replicator::drive`]), and answers the end of the log it holds,
//! synced; one answering less than it was sent (an append past its end, as a member that lost what
//! it had) is sent again from its end. A member out of reach is tried again, from 50 ms apart,
//! doubling, to 5 s; one that refuses the writer (another incarnation, no replica directory) is
//! sent nothing more, until a replacement (TASK-537.3.5); one that says a later epoch fenced the
//! cell fences the writer: no commit of it is told replicated after.
//!
//! For hot replication (peers caught up), each flush's `Arc<[u8]>` batch is handed directly to the
//! `2f` peer tasks in memory (zero spool disk writes, zero spool disk reads; board 6902) and
//! dropped once live members pass it or total hot bytes exceed [`HOT_BYTES`]. For lagging peers or
//! configuration replacement/catch-up, bytes evicted from the hot window are streamed directly
//! from the cell's existing `logs/`, `open-segments/`/`segments/`, and `extents/` via
//! [`LogSource`].

use crate::untrusted::Cursor;
use event_listener::Event;
use futures::FutureExt as _;
use futures::future::BoxFuture;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

/// The most of a log a member is sent at once.
pub const PIECE: usize = 4 << 20;

/// Maximum evictable hot bytes retained in memory per replicator before lagging/replacement peers
/// fall back to reading from [`LogSource`] (board 6902).
const HOT_BYTES: u64 = 16 << 20;

/// Node-wide accounting and governor eviction hook for replication hot buffers (board 6902):
/// counts every byte held across all live [`Replicator`] instances (including pinned take-time
/// seeds) and lets the daemon's memory governor cap and reclaim persisted buffers under pressure.
pub(crate) struct HotBudget {
    total: AtomicU64,
    limit: AtomicU64,
    replicators: Mutex<Vec<Weak<Shared>>>,
}

impl HotBudget {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            total: AtomicU64::new(0),
            limit: AtomicU64::new(u64::MAX),
            replicators: Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    pub(crate) fn size_to(&self, limit: u64) {
        self.limit.store(limit, Ordering::Relaxed);
    }

    pub(crate) fn evict_to(&self, target: u64) -> u64 {
        if self.total.load(Ordering::Relaxed) <= target {
            return 0;
        }
        let live: Vec<Arc<Shared>> = {
            let mut list = self.replicators.lock().unwrap();
            list.retain(|weak| weak.strong_count() > 0);
            list.iter().filter_map(Weak::upgrade).collect()
        };
        let mut evicted = 0u64;
        loop {
            let mut progress = false;
            for shared in &live {
                if self.total.load(Ordering::Relaxed) <= target {
                    return evicted;
                }
                let mut state = shared.state.lock().unwrap();
                if state.has_source {
                    let freed = state.evict_one_persisted();
                    if freed > 0 {
                        evicted += freed;
                        progress = true;
                    }
                }
            }
            if !progress {
                return evicted;
            }
        }
    }
}

/// Total bytes held in hot replication buffers across all live replicators on `host`,
/// including pinned take-time seeds (board 6902).
pub fn hot_bytes_on(host: &crate::Host) -> u64 {
    host.hot_budget.bytes()
}

/// Total bytes held in hot replication buffers on the process's default host outside `madsim`.
#[cfg(not(madsim))]
pub fn hot_bytes() -> u64 {
    hot_bytes_on(crate::Host::process())
}

/// cladefs: what a take holds of its fenced members' logs as it reads them (board 8453), counted
/// with the hot replication buffers ([`hot_bytes_on`]), which the daemon's memory governor counts
/// and makes room for, from when a read is asked for to when the take is done with it: a member's
/// copy is the take's to hold whole, as it checks it whole, so nothing of it is evicted, but the
/// governor shrinks what else it governs for it (one memory budget).
pub(crate) struct Holding {
    bytes: u64,
    budget: Option<Arc<HotBudget>>,
}

impl Holding {
    /// Holds nothing, counted against `budget` when given.
    pub(crate) fn on(budget: Option<Arc<HotBudget>>) -> Self {
        Self { bytes: 0, budget }
    }

    /// Holds nothing, uncounted.
    pub(crate) fn none() -> Self {
        Self { bytes: 0, budget: None }
    }

    /// Holds `bytes` more.
    pub(crate) fn hold(&mut self, bytes: u64) {
        if let Some(budget) = &self.budget {
            budget.total.fetch_add(bytes, Ordering::Relaxed);
        }
        self.bytes += bytes;
    }

    /// Holds `bytes` fewer, of those it holds.
    pub(crate) fn release(&mut self, bytes: u64) {
        let bytes = bytes.min(self.bytes);
        if let Some(budget) = &self.budget {
            budget.total.fetch_sub(bytes, Ordering::Relaxed);
        }
        self.bytes -= bytes;
    }

    /// Holds `bytes`, more or fewer than it held.
    pub(crate) fn hold_only(&mut self, bytes: u64) {
        match bytes.checked_sub(self.bytes) {
            Some(more) => self.hold(more),
            None => self.release(self.bytes - bytes),
        }
    }
}

impl Drop for Holding {
    fn drop(&mut self) {
        if let Some(budget) = &self.budget {
            budget.total.fetch_sub(self.bytes, Ordering::Relaxed);
        }
    }
}

/// Sets `host`'s soft cap on hot replication buffers (called by the daemon's memory governor):
/// new flushes past this cap evict persisted chunks as soon as they hit local disk.
pub fn size_hot_to_on(host: &crate::Host, limit: u64) {
    host.hot_budget.size_to(limit);
}

/// Sets the process's default host's soft cap on hot replication buffers outside `madsim`.
#[cfg(not(madsim))]
pub fn size_hot_to(limit: u64) {
    size_hot_to_on(crate::Host::process(), limit);
}

/// Evicts persisted, non-pinned hot replication chunks across live replicators on `host` until
/// total hot bytes are at most `target` (or only unpersisted/pinned chunks remain), returning
/// bytes freed.
pub fn evict_hot_to_on(host: &crate::Host, target: u64) -> u64 {
    host.hot_budget.evict_to(target)
}

/// Evicts persisted, non-pinned hot replication chunks on the process's default host outside
/// `madsim`.
#[cfg(not(madsim))]
pub fn evict_hot_to(target: u64) -> u64 {
    evict_hot_to_on(crate::Host::process(), target)
}

/// Seed for `own_frame` rapidhash integrity checks: fixed for good, as the replica directory is
/// frozen (doc-28, board 10046).
const OWN_FRAME_SEED: u64 = 0x6678_6461_7832_0003;

/// Header bytes of an `own_frame`: its version (u32), its sequence (u64), its file's length (u64),
/// its hash (u64).
pub(crate) const OWN_FRAME_HEADER: u64 = 28;

/// The log a member keeps segment `segment`'s replica log in.
pub fn log_of(segment: u64) -> u64 {
    segment << 1
}

/// The log a member keeps the extents segment `segment`'s commits stored on their own in.
pub fn own_of(segment: u64) -> u64 {
    (segment << 1) | 1
}

fn own_frame_hash(version: u32, sequence: u64, file: &[u8]) -> u64 {
    let mut header = [0u8; 20];
    header[..4].copy_from_slice(&version.to_le_bytes());
    header[4..12].copy_from_slice(&sequence.to_le_bytes());
    header[12..].copy_from_slice(&(file.len() as u64).to_le_bytes());
    let h = rapidhash::rapidhash_seeded(&header, OWN_FRAME_SEED);
    rapidhash::rapidhash_seeded(file, h) | 1
}

/// An extent stored on its own as its segment's own log holds it: the replica directory's version
/// (u32, [`crate::replica::FORMAT`], doc-28), its sequence (u64), its file's length (u64), its
/// rapidhash, of all of them and the file (u64, nonzero so zero-padding never matches),
/// little-endian, then the file's bytes, the bucket's object byte for byte: an extent frame, which
/// says its own version.
pub fn own_frame(sequence: u64, file: &[u8]) -> Vec<u8> {
    let version = crate::replica::FORMAT.current;
    let mut frame = Vec::with_capacity(OWN_FRAME_HEADER as usize + file.len());
    frame.extend_from_slice(&version.to_le_bytes());
    frame.extend_from_slice(&sequence.to_le_bytes());
    frame.extend_from_slice(&(file.len() as u64).to_le_bytes());
    frame.extend_from_slice(&own_frame_hash(version, sequence, file).to_le_bytes());
    frame.extend_from_slice(file);
    frame
}

/// The frames whole and hash-verified at the start of `bytes`, an own log's, each sequence and
/// file, and where the last ends. A member's bytes, untrusted: read through [`Cursor`], a frame
/// cut short, corrupted, or of a length past what is left ending them (board 5408, board 6902).
/// A frame whole of a version this build doesn't read is refused, never taken for the log's torn
/// end, which would forget what its member acked (doc-28).
#[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
fn parse_own(bytes: &[u8]) -> Result<(Vec<(u64, &[u8])>, usize), anyhow::Error> {
    use anyhow::Context as _;
    let mut cursor = Cursor::new(bytes);
    let (mut frames, mut end) = (Vec::new(), 0usize);
    // A length past what this host addresses: past what is left, which ends the frames too.
    while let Some(version) = cursor.u32()
        && let Some(sequence) = cursor.u64()
        && let Some(len) = cursor.u64().and_then(|len| usize::try_from(len).ok())
        && let Some(expected) = cursor.u64()
        && let Some(file) = cursor.take(len)
    {
        if own_frame_hash(version, sequence, file) != expected {
            break;
        }
        crate::replica::FORMAT
            .check(version)
            .with_context(|| format!("an own log's frame at {end}"))?;
        let Some(next) =
            end.checked_add(OWN_FRAME_HEADER as usize).and_then(|n| n.checked_add(len))
        else {
            break;
        };
        frames.push((sequence, file));
        end = next;
    }
    Ok((frames, end))
}

/// The extents an own log's bytes hold, whole and hash-verified: each sequence and file
/// ([`parse_own`]).
pub fn own_frames(bytes: &[u8]) -> Result<Vec<(u64, Vec<u8>)>, anyhow::Error> {
    let (frames, _) = parse_own(bytes)?;
    Ok(frames.into_iter().map(|(sequence, file)| (sequence, file.to_vec())).collect())
}

/// Byte offset past the last whole, hash-verified frame in an own log (board 6902).
pub(crate) fn own_frames_end(bytes: &[u8]) -> Result<usize, anyhow::Error> {
    parse_own(bytes).map(|(_, end)| end)
}

/// Authenticates an odd own-extent log `bytes` for `cell` under `keys` against the `required`
/// `(sequence -> data_len)` own extents named by the segment's authenticated even records log,
/// returning the byte offset past the last whole authentic frame (board 5406).
pub(crate) fn own_whole_authentic(
    bytes: &[u8],
    cell: extent_frame::segment::CellId,
    keys: &bucket_cell::keys::Ring,
    required: &BTreeMap<u64, u64>,
) -> Result<usize, anyhow::Error> {
    use anyhow::Context as _;
    let (frames, end) = parse_own(bytes)?;
    if end == 0 && !bytes.is_empty() {
        anyhow::bail!("own-extent log has no whole valid frame ({} bytes)", bytes.len());
    }
    let cell_keys = keys.of(cell.id);
    let mut seen: BTreeMap<u64, u64> = BTreeMap::new();
    for (sequence, file) in frames {
        let data_len = extent_frame::check_under(file, &cell_keys)
            .with_context(|| format!("own extent {sequence} failed frame authentication"))?;
        seen.insert(sequence, data_len);
    }
    for (&sequence, &expected_len) in required {
        match seen.get(&sequence) {
            Some(&got_len) if got_len == expected_len => {}
            Some(&got_len) => anyhow::bail!(
                "own extent {sequence} has length {got_len}, authenticated records expect {expected_len}"
            ),
            None => anyhow::bail!(
                "own extent {sequence} (length {expected_len}) missing from own-extent log"
            ),
        }
    }
    Ok(end)
}

/// What of `logs`, what members hold of a replicated cell `cell`'s writer's logs by their numbers,
/// goes on from `last`, the last commit the cell's head names (TASK-537.3.4): the segments after
/// the last whose records name a commit through `last`, the head acknowledging whole segments, so
/// that a member's copy of one it named, whole or cut short, its drop never come, is the bucket's
/// already (TASK-537.3.5's review); and of those, from the first holding any record.
///
/// None if the first commit they hold does not follow `last`, and `keys`, the cell's, find their
/// logs authentic (TASK-680, found by the simulation): members are sent a segment's logs before
/// the next's, and drop only what the head names, so what is between is the writer's base, the
/// commits it opened with ([`Replicator::base`]), which its members were never given and the
/// bucket held not yet; and it tells nothing replicated before the bucket holds its base, so
/// nothing they hold was told replicated. (A take's own commits it gives its configuration's
/// members as their logs' first, or the head names the configuration before until the bucket holds
/// them, TASK-678.) Refused without `keys`, or if a log is not authentic: a forged record past the
/// head's last would otherwise make a take give up commits told replicated (TASK-680's review).
///
/// Refused too if a segment at or before the last naming a commit through `last` names one past
/// it, as no writer's logs do, but forged ones may. (Not the head's count of segments: a writer's
/// local segments are numbered as its own, the bucket's as the cell's.)
pub fn trimmed(
    cell: extent_frame::segment::CellId,
    logs: &[(u64, Vec<u8>)],
    last: Option<u64>,
    keys: Option<&bucket_cell::keys::Ring>,
) -> Result<Vec<(u64, Vec<u8>)>, anyhow::Error> {
    use crate::layout::SegmentId;
    use anyhow::Context as _;
    use extent_frame::segment::epoch_of;
    let segment = |number: u64| SegmentId { cell: cell.id, number };
    let through = |commit: u64| last.is_some_and(|last| epoch_of(commit) <= epoch_of(last));
    // Each segment's records, by the commits each names.
    let records: BTreeMap<u64, Vec<Vec<u64>>> = (logs.iter().filter(|(log, _)| log & 1 == 0))
        .map(|(log, bytes)| (log >> 1, crate::wal::replica_records(bytes, segment(log >> 1))))
        .collect();
    let named = (records.iter())
        .filter(|(_, records)| records.iter().flatten().any(|&commit| through(commit)))
        .map(|(&number, _)| number)
        .max();
    // What a member sends is read here holding no key, unauthenticated (doc-21): a forged record
    // naming an old commit must not make honest logs after it look named (the verifying review
    // of TASK-537.3.6). Honest, every commit of a segment named is through `last`, the head
    // acknowledging whole segments; else refused, as recovery refuses a forged record it would
    // restore, failing its MAC.
    if let Some(named) = named {
        let past = (records.range(..=named))
            .flat_map(|(&number, records)| records.iter().flatten().map(move |c| (number, *c)))
            .find(|&(_, commit)| !through(commit));
        if let Some((number, commit)) = past {
            anyhow::bail!(
                "the members' logs of {cell:?} name commit {commit} in segment {number}, past \
                 the head's last, {last:?}, at or before segment {named}, which names a commit \
                 through it: not one writer's logs"
            );
        }
    }
    let after = |number: u64| named.is_none_or(|named| number > named);
    let first = (records.iter())
        .find(|&(&number, records)| after(number) && !records.is_empty())
        .map(|(&number, _)| number);
    let Some(first) = first else { return Ok(Vec::new()) };
    let kept: Vec<(u64, Vec<u8>)> =
        logs.iter().filter(|(log, _)| log >> 1 >= first).cloned().collect();
    let commits = records.range(first..).flat_map(|(_, records)| records.iter().flatten());
    let first_commit = commits.copied().next();
    let follows = match (last, first_commit) {
        (_, None) => true,
        (None, Some(commit)) => epoch_of(commit) <= 1,
        (Some(last), Some(commit)) => epoch_of(commit) == epoch_of(last) + 1,
    };
    if !follows {
        let Some(keys) = keys else {
            anyhow::bail!(
                "the members' logs of {cell:?} from segment {first} begin at commit \
                 {first_commit:?}, not going on from the head's last, {last:?}, and no key \
                 authenticates them"
            );
        };
        for (log, bytes) in logs.iter().filter(|(log, _)| log & 1 == 0) {
            crate::wal::replica_authentic(bytes, segment(log >> 1), keys).with_context(|| {
                format!(
                    "the members' logs of {cell:?} begin at commit {first_commit:?}, not going \
                     on from the head's last, {last:?}"
                )
            })?;
        }
        log::info!(
            "cladefs: the members' logs of {cell:?} from segment {first} begin at commit \
             {first_commit:?}, past the head's last, {last:?}: their writer's base, which the \
             bucket held not, was between, none of it told replicated: given up"
        );
        return Ok(Vec::new());
    }
    Ok(kept)
}

/// The last commit `logs`, what members hold of a replicated cell `cell`'s writer's logs, name.
pub fn last_commit(cell: extent_frame::segment::CellId, logs: &[(u64, Vec<u8>)]) -> Option<u64> {
    let segment = |log: u64| crate::layout::SegmentId { cell: cell.id, number: log >> 1 };
    let mut last = None;
    for (log, bytes) in logs.iter().filter(|(log, _)| log & 1 == 0) {
        last = crate::wal::replica_commits(bytes, segment(*log)).last().copied().or(last);
    }
    last
}

/// Whether what members hold, ending at the commit `members`, goes past a cell's own write-ahead
/// log, ending at `own` (TASK-537.3.4's review): a later writer's commits, or the same writer's
/// beyond what its own log holds (sent before its own sync, which a crash cut). Then the members'
/// logs, not its own, are what the cell recovers: they hold every commit a guest was told
/// replicated, and a member holding a flush holds every flush before it. Else its own log holds
/// all they do, and Local commits besides.
pub fn members_ahead(own: Option<u64>, members: Option<u64>) -> bool {
    use extent_frame::segment::{epoch_of, writer_of};
    match (own, members) {
        (_, None) => false,
        (None, Some(_)) => true,
        (Some(own), Some(members)) => match writer_of(members).cmp(&writer_of(own)) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => epoch_of(members) > epoch_of(own),
        },
    }
}

/// Writes what a member holds of a replicated cell's writer's logs, `logs` by their numbers in its
/// directory, into `dir`, a cell directory of `cell`, made and holding no write-ahead log, as
/// its write-ahead log after the commit `last`, the last the cell's head names (TASK-537.3.4's
/// take): what of them goes on from it ([`trimmed`]), each segment's replica log as the segment's
/// log beside an empty raw file, each extent stored on its own as its file, and the log's start,
/// `last`; an opening then recovers the segments as it recovers its own, their records restoring
/// their runs.
pub fn restore<D: crate::Dir>(
    dir: &D,
    cell: extent_frame::segment::CellId,
    logs: &[(u64, Vec<u8>)],
    last: Option<u64>,
) -> Result<(), anyhow::Error> {
    use crate::layout::{SegmentId, extent_path, log_path, open_segment_path};
    use anyhow::Context as _;
    let segment = |log: u64| SegmentId { cell: cell.id, number: log >> 1 };
    let kept = trimmed(cell, logs, last, None)?;
    let write = |path: &str, bytes: &[u8], data: bool| -> Result<(), anyhow::Error> {
        let file = dir.create(path, data).with_context(|| format!("making {path}"))?;
        if !bytes.is_empty() {
            crate::File::write_at(&file, bytes, 0).with_context(|| format!("writing {path}"))?;
        }
        crate::File::sync(&file).with_context(|| format!("syncing {path}"))
    };
    let write_own = |path: &str, bytes: &[u8]| -> Result<(), anyhow::Error> {
        match dir.create(path, true) {
            Ok(file) => {
                if !bytes.is_empty() {
                    crate::File::write_at(&file, bytes, 0)
                        .with_context(|| format!("writing {path}"))?;
                }
                crate::File::sync(&file).with_context(|| format!("syncing {path}"))
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                // board 5402: a crash between syncing an own extent + sending to members and
                // appending the local WAL record leaves `extents/<cell>/<seq>` on disk while
                // members are ahead. Reuse it if its bytes match, otherwise replace the orphan.
                let existing = dir.open(path, true).with_context(|| format!("opening {path}"))?;
                let size =
                    crate::File::size(&existing).with_context(|| format!("sizing {path}"))?;
                if size == bytes.len() as u64 {
                    let mut got = vec![0u8; bytes.len()];
                    if !got.is_empty() {
                        crate::File::read_at(&existing, &mut got, 0)
                            .with_context(|| format!("reading {path}"))?;
                    }
                    if got == bytes {
                        return crate::File::sync(&existing)
                            .with_context(|| format!("syncing {path}"));
                    }
                }
                drop(existing);
                dir.remove(path).with_context(|| format!("removing stale {path}"))?;
                let file = dir.create(path, true).with_context(|| format!("making {path}"))?;
                if !bytes.is_empty() {
                    crate::File::write_at(&file, bytes, 0)
                        .with_context(|| format!("writing {path}"))?;
                }
                crate::File::sync(&file).with_context(|| format!("syncing {path}"))
            }
            Err(err) => Err(anyhow::Error::from(err).context(format!("making {path}"))),
        }
    };
    for (log, bytes) in &kept {
        match log & 1 {
            0 => {
                write(&log_path(segment(*log)), bytes, false)?;
                write(&open_segment_path(segment(*log)), &[], true)?;
            }
            _ => {
                for (sequence, file) in own_frames(bytes)? {
                    let extent = storage_device::ExtentId { cell: cell.id, sequence };
                    write_own(&extent_path(extent), &file)?;
                }
            }
        }
    }
    let (mut file, _) = crate::wal::StartFile::read(dir)?;
    file.write(dir, last)?;
    let kinds = [crate::layout::LOGS, crate::layout::OPEN_SEGMENTS, crate::layout::EXTENTS];
    let paths = kinds.map(|kind| crate::layout::cell_dir(kind, cell.id));
    for path in std::iter::once(String::new()).chain(paths) {
        if let Err(err) = dir.sync_dir(&path) {
            if err.kind() != std::io::ErrorKind::NotFound {
                return Err(anyhow::Error::from(err).context(format!("syncing {path}")));
            }
        }
    }
    Ok(())
}

/// Reads bytes of a replicated log on catch-up from the cell's existing `logs/`,
/// `open-segments/`/`segments/`, and `extents/` when evicted from the hot in-memory window
/// (board 6902).
pub trait LogSource: Send + Sync + 'static {
    /// Reads up to `len` bytes at `offset` of `log`.
    fn read(&self, log: u64, offset: u64, len: u64) -> std::io::Result<Vec<u8>>;
}

/// How the writer reaches a member of its configuration: each call a future, which holds no thread
/// as it waits for the member (doc-24 step 10). The simulation's answer at once
/// ([`Replicator::drive`]).
pub trait MemberLink: Send + Sync + 'static {
    /// Appends `bytes` at `offset` of the member's log `log`: the log's end there, synced.
    fn append(
        &self,
        log: u64,
        offset: u64,
        bytes: Vec<u8>,
    ) -> BoxFuture<'_, Result<u64, LinkError>>;
    /// Drops the member's logs to `through`, whose segments the head names.
    fn drop_through(&self, through: u64) -> BoxFuture<'_, Result<(), LinkError>>;
}

/// Why a member did not take what it was sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkError {
    /// A take of a later epoch fenced the cell: the writer is fenced.
    Fenced(String),
    /// The member refuses the writer for good: another incarnation, no replica directory.
    Refused(String),
    /// Out of reach, or failing: tried again.
    Down(String),
}

/// What a flush sends: the epoch of its last commit, and what it appends to each log.
#[derive(Clone, Debug, Default)]
pub struct Flushed {
    pub epoch: u64,
    /// Each append: the log, the offset, and the shared bytes.
    pub appends: Vec<(u64, u64, Arc<[u8]>)>,
}

/// A replicated cell's writer's replicator: what its members hold, and what is replicated.
pub struct Replicator {
    shared: Arc<Shared>,
    /// Its host, whose bucket runtime runs its member tasks when started.
    host: Option<Arc<crate::Host>>,
    /// Each member's link, by its place among [`State::members`].
    links: Mutex<Vec<Arc<dyn MemberLink>>>,
    /// Whether each member is served by a task of its own ([`Replicator::run_tasks`]), or
    /// driven, a turn at a time ([`Replicator::drive`]).
    threaded: std::sync::atomic::AtomicBool,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

/// What a turn of a member's did ([`turn`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Turn {
    /// Nothing to send it, for now.
    Idle,
    /// It was sent something, and answered.
    Sent,
    /// What it was to be sent went meanwhile: to choose again.
    Again,
    /// It is out of reach: to try again, later.
    Retry,
    /// It is sent nothing more: retired, refusing, the writer fenced, or the replicator stopped
    /// or broken.
    Stop,
}

struct Shared {
    /// The epoch of the last commit on no member: before it started, or recorded in a segment
    /// whose log it started in the middle of.
    base: std::sync::atomic::AtomicU64,
    /// Catch-up reader backed by the cell's existing files, if any (none in memory-only unit tests).
    source: Option<Arc<dyn LogSource>>,
    state: Mutex<State>,
    /// Said as its state changes, to the members' tasks waiting for something to send.
    changed: Event,
    /// Called once more is replicated, or the writer is fenced, holding no lock.
    notify: Box<dyn Fn() + Send + Sync>,
}

/// A batch held in the hot in-memory replication window until live members pass it or it is
/// evicted to keep memory bounded by [`HOT_BYTES`].
struct HotChunk {
    offset: u64,
    bytes: Arc<[u8]>,
    /// Whether the batch has been written to the cell's local `logs/` file on disk (so [`LogSource`]
    /// can reconstruct it if evicted).
    persisted: bool,
    /// Pinned in memory until acknowledged (used for take-time seeds whose local `logs/` were
    /// closed during recovery).
    pinned: bool,
}

impl HotChunk {
    fn end(&self) -> u64 {
        self.offset + self.bytes.len() as u64
    }
}

/// A configuration's members, among a replicator's: its f, and where they are in
/// [`State::members`].
#[derive(Clone, Debug)]
struct Group {
    f: usize,
    members: std::ops::Range<usize>,
}

struct State {
    /// Its host's hot-buffer budget.
    budget: Arc<HotBudget>,
    /// Each log's length, until its segment is in the bucket.
    logs: BTreeMap<u64, u64>,
    /// Hot in-memory batches per log, ordered by offset.
    hot: BTreeMap<u64, VecDeque<HotChunk>>,
    /// Total bytes held in `hot` across all logs (evictable + unpersisted + pinned).
    hot_bytes: u64,
    /// Whether a [`LogSource`] backs evicted chunks.
    has_source: bool,
    /// The logs dropped, to this one, their segments in the bucket.
    dropped: Option<u64>,
    /// The epoch of the last flush's last commit.
    last: u64,
    /// Every member of the configurations it served, by their order in them, those of a
    /// configuration gone retired.
    members: Vec<Member>,
    /// The configuration whose quorum counts, as the head names it.
    active: Option<Group>,
    /// The configuration replacing it (TASK-537.3.5), until its swap of the head lands: a commit is
    /// replicated meanwhile only on quorums of both, as a taker may find either in the head
    /// (doc-21's joint rule).
    pending: Option<Group>,
    replicated: u64,
    fenced: Option<String>,
    /// Why catch-up reading failed, if it did: nothing more is sent.
    broken: Option<String>,
    stopping: bool,
}

#[derive(Clone, Debug, Default)]
struct Member {
    /// The ends of its logs it answered, synced.
    ends: BTreeMap<u64, u64>,
    /// Each flush's epoch and the ends of the logs it appended to, those it holds not yet.
    marks: VecDeque<(u64, Vec<(u64, u64)>)>,
    /// The epoch of the last flush it holds whole.
    holds: u64,
    /// The logs dropped on it, to this one.
    dropped: Option<u64>,
    /// Why it holds nothing more, if it is down or refused, and since when.
    down: Option<(String, std::time::Instant)>,
    refused: bool,
    /// Of a configuration replaced: sent nothing more, counting for nothing.
    retired: bool,
}

impl Drop for State {
    fn drop(&mut self) {
        self.clear_hot();
    }
}

impl State {
    fn new(budget: Arc<HotBudget>) -> Self {
        Self {
            budget,
            logs: BTreeMap::new(),
            hot: BTreeMap::new(),
            hot_bytes: 0,
            has_source: false,
            dropped: None,
            last: 0,
            members: Vec::new(),
            active: None,
            pending: None,
            replicated: 0,
            fenced: None,
            broken: None,
            stopping: false,
        }
    }

    fn add_hot(&mut self, len: u64) {
        if len > 0 {
            self.hot_bytes += len;
            self.budget.total.fetch_add(len, Ordering::Relaxed);
        }
    }

    fn sub_hot(&mut self, len: u64) {
        if len > 0 {
            self.hot_bytes -= len;
            self.budget.total.fetch_sub(len, Ordering::Relaxed);
        }
    }

    fn clear_hot(&mut self) {
        self.hot.clear();
        if self.hot_bytes > 0 {
            let len = std::mem::take(&mut self.hot_bytes);
            self.budget.total.fetch_sub(len, Ordering::Relaxed);
        }
    }

    fn evict_one_persisted(&mut self) -> u64 {
        let evict_log = self.hot.iter().find_map(|(&log, queue)| {
            queue.front().is_some_and(|c| c.persisted && !c.pinned).then_some(log)
        });
        let Some(log) = evict_log else { return 0 };
        let queue = self.hot.get_mut(&log).unwrap();
        let chunk = queue.pop_front().unwrap();
        let len = chunk.bytes.len() as u64;
        if queue.is_empty() {
            self.hot.remove(&log);
        }
        self.sub_hot(len);
        len
    }

    /// Whether member `m` holds a mark's `ends`: every log it appended to, to its end, or dropped,
    /// in the bucket.
    fn holds(&self, m: usize, ends: &[(u64, u64)]) -> bool {
        ends.iter().all(|&(log, end)| {
            self.dropped.is_some_and(|dropped| log <= dropped)
                || self.members[m].ends.get(&log).is_some_and(|&held| held >= end)
        })
    }

    /// The epoch of the last commit f members of `group` hold, the writer's own copy the f+1-th:
    /// none, for a configuration of f = 0, which holds no quorum (doc-21; TLC's zeroNoQuorum).
    fn quorum(&self, group: &Group) -> Option<u64> {
        if group.f == 0 {
            return None;
        }
        let mut holds: Vec<u64> =
            self.members[group.members.clone()].iter().map(|member| member.holds).collect();
        holds.sort_unstable_by(|a, b| b.cmp(a));
        Some(holds.get(group.f - 1).copied().unwrap_or(0))
    }

    /// Moves each member past the marks it holds: whether the replicated epoch moved. It moves
    /// to what the active configuration's quorum holds, and, while one replaces it, the
    /// replacement's quorum too.
    fn advance(&mut self) -> bool {
        for m in 0..self.members.len() {
            while let Some((epoch, ends)) = self.members[m].marks.front() {
                if !self.holds(m, ends) {
                    break;
                }
                let epoch = *epoch;
                let member = &mut self.members[m];
                member.marks.pop_front();
                member.holds = epoch;
            }
        }
        let groups = self.active.iter().chain(&self.pending);
        let quorums: Option<Vec<u64>> = groups.map(|group| self.quorum(group)).collect();
        let Some(replicated) = quorums.and_then(|quorums| quorums.into_iter().min()) else {
            return false;
        };
        let moved = replicated > self.replicated;
        self.replicated = self.replicated.max(replicated);
        moved
    }

    /// What member `m` is to be sent next: its logs' drop, or an append from its end.
    fn next(&self, m: usize) -> Option<Sending> {
        let member = &self.members[m];
        if member.refused || member.retired || self.fenced.is_some() || self.broken.is_some() {
            return None;
        }
        if self.dropped > member.dropped {
            return self.dropped.map(Sending::Drop);
        }
        // Each segment's own extents before its records: a member holding a record holds the
        // files of the extents it stored on their own, or a take recovering it could not open
        // (TASK-537.3.4's review).
        let mut order: Vec<(u64, u64)> =
            self.logs.iter().map(|(&log, &have)| (log, have)).collect();
        order.sort_by_key(|&(log, _)| (log >> 1, log & 1 == 0));
        order.into_iter().find_map(|(log, have)| {
            let end = member.ends.get(&log).copied().unwrap_or(0);
            (end < have).then(|| {
                let len = (have - end).min(PIECE as u64);
                let hot = self.hot_slice(log, end, len);
                Sending::Append { log, offset: end, len, hot }
            })
        })
    }

    /// Slices up to `len` bytes starting at `offset` from the hot in-memory window if present.
    fn hot_slice(&self, log: u64, offset: u64, len: u64) -> Option<Vec<u8>> {
        let queue = self.hot.get(&log)?;
        let mut out = Vec::new();
        let mut at = offset;
        let target = offset + len;
        for chunk in queue {
            if chunk.end() <= at {
                continue;
            }
            if chunk.offset > at {
                break;
            }
            let start = (at - chunk.offset) as usize;
            let take = (chunk.end().min(target) - at) as usize;
            out.extend_from_slice(&chunk.bytes[start..start + take]);
            at += take as u64;
            if at >= target {
                break;
            }
        }
        (!out.is_empty()).then_some(out)
    }

    /// Drops hot chunks that all live members have acked (when backed by a [`LogSource`]), and
    /// evicts oldest persisted non-pinned chunks while total hot bytes exceed [`HOT_BYTES`].
    fn trim_hot(&mut self) {
        if !self.has_source {
            return;
        }
        let mut freed = 0u64;
        {
            let live: Vec<&Member> = self
                .members
                .iter()
                .filter(|m| !m.retired && !m.refused && m.down.is_none())
                .collect();
            if !live.is_empty() {
                let logs: Vec<u64> = self.hot.keys().copied().collect();
                for log in logs {
                    let min_end = live
                        .iter()
                        .map(|m| m.ends.get(&log).copied().unwrap_or(0))
                        .min()
                        .unwrap_or(0);
                    if let Some(queue) = self.hot.get_mut(&log) {
                        while let Some(front) = queue.front() {
                            if !front.persisted || front.pinned || front.end() > min_end {
                                break;
                            }
                            freed += front.bytes.len() as u64;
                            queue.pop_front();
                        }
                        if queue.is_empty() {
                            self.hot.remove(&log);
                        }
                    }
                }
            }
        }
        self.sub_hot(freed);
        let budget = Arc::clone(&self.budget);
        while self.hot_bytes > HOT_BYTES
            || budget.total.load(Ordering::Relaxed) > budget.limit.load(Ordering::Relaxed)
        {
            if self.evict_one_persisted() == 0 {
                break;
            }
        }
    }

    /// The members of the active configuration.
    fn active(&self) -> &[Member] {
        self.active.as_ref().map_or(&[], |group| &self.members[group.members.clone()])
    }
}

enum Sending {
    Drop(u64),
    Append { log: u64, offset: u64, len: u64, hot: Option<Vec<u8>> },
}

impl Replicator {
    /// A replicator on `host` of a configuration of `f`, its members reached by `links` (2f of
    /// them, the writer aside), started after the commit of epoch `base`, which none holds, backed
    /// by `source` for catch-up when lagging or replacing: `notify` called once more is replicated,
    /// or the writer is fenced.
    pub fn start_on(
        host: Option<Arc<crate::Host>>,
        f: usize,
        base: u64,
        source: Option<Arc<dyn LogSource>>,
        links: Vec<Arc<dyn MemberLink>>,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Arc<Self> {
        let replicator = Self::idle_on(host, f, base, source, links, notify);
        replicator.run_tasks();
        replicator
    }

    /// [`Replicator::start_on`] on the process's default host outside `madsim`.
    #[cfg(not(madsim))]
    pub fn start(
        f: usize,
        base: u64,
        source: Option<Arc<dyn LogSource>>,
        links: Vec<Arc<dyn MemberLink>>,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Arc<Self> {
        Self::start_on(Some(Arc::clone(crate::Host::process())), f, base, source, links, notify)
    }

    /// A replicator on `host` as [`Replicator::start_on`] makes it, whose members nothing serves
    /// yet: by a task each once [`Replicator::run_tasks`] starts them, or a turn at a time as
    /// [`Replicator::drive`] is called, as the simulation's scheduler does, so that a seed decides
    /// what each member is sent and when (TASK-680).
    pub fn idle_on(
        host: Option<Arc<crate::Host>>,
        f: usize,
        base: u64,
        source: Option<Arc<dyn LogSource>>,
        links: Vec<Arc<dyn MemberLink>>,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Arc<Self> {
        let budget = host.as_ref().map_or_else(HotBudget::new, |h| Arc::clone(&h.hot_budget));
        let mut state = State::new(Arc::clone(&budget));
        state.has_source = source.is_some();
        state.members = vec![Member::default(); links.len()];
        state.active = Some(Group { f, members: 0..links.len() });
        state.last = base;
        let shared = Arc::new(Shared {
            base: base.into(),
            source,
            state: Mutex::new(state),
            changed: Event::new(),
            notify: Box::new(notify),
        });
        {
            let mut list = budget.replicators.lock().unwrap();
            list.retain(|weak| weak.strong_count() > 0);
            list.push(Arc::downgrade(&shared));
        }
        Arc::new(Self {
            shared,
            host,
            links: Mutex::new(links),
            threaded: false.into(),
            tasks: Mutex::default(),
        })
    }

    /// [`Replicator::idle_on`] on the process's default host outside `madsim`.
    #[cfg(not(madsim))]
    pub fn idle(
        f: usize,
        base: u64,
        source: Option<Arc<dyn LogSource>>,
        links: Vec<Arc<dyn MemberLink>>,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Arc<Self> {
        Self::idle_on(Some(Arc::clone(crate::Host::process())), f, base, source, links, notify)
    }

    /// Serves each member by a task of its own, on the host's bucket runtime, from now on, and
    /// those of replacements.
    pub fn run_tasks(&self) {
        self.threaded.store(true, std::sync::atomic::Ordering::Release);
        let links = self.links.lock().unwrap().clone();
        self.serve(0, links);
    }

    /// How many members it has had, those of configurations gone among them: what
    /// [`Replicator::drive`] takes.
    pub fn members(&self) -> usize {
        self.links.lock().unwrap().len()
    }

    /// A turn of member `m`'s, of a replicator no tasks serve ([`Replicator::idle`]): sends it
    /// what it lacks next, if anything, and takes in its answer. Whether it was sent anything and
    /// answered. The simulation's links answer at once: a turn is done in its first poll.
    pub fn drive(&self, m: usize) -> bool {
        debug_assert!(!self.threaded.load(std::sync::atomic::Ordering::Acquire), "driven");
        let Some(link) = self.links.lock().unwrap().get(m).cloned() else { return false };
        let turned = turn(&self.shared, m, &*link).now_or_never();
        turned.expect("a driven member answers at once") == Turn::Sent
    }

    /// Starts a task serving each of `links`, the members from `first` on, on the host's bucket
    /// runtime, which their links' calls run on too (doc-24 step 10). A runtime that can't be had
    /// breaks the replicator, whose commits then wait for the bucket.
    fn serve(&self, first: usize, links: Vec<Arc<dyn MemberLink>>) {
        let runtime = match &self.host {
            Some(host) => host.bucket_runtime().map(|rt| rt.handle().clone()),
            None => {
                #[cfg(not(madsim))]
                {
                    crate::upload::bucket_runtime().map(|rt| rt.handle().clone())
                }
                #[cfg(madsim)]
                {
                    Ok(tokio::runtime::Handle::current())
                }
            }
        };
        let runtime = match runtime {
            Ok(runtime) => runtime,
            Err(err) => {
                let why = format!("the bucket runtime, to serve the members on: {err}");
                log::error!("cladefs: replicating: {why}");
                self.shared.state.lock().unwrap().broken = Some(why);
                self.shared.changed.notify(usize::MAX);
                (self.shared.notify)();
                return;
            }
        };
        let mut tasks = self.tasks.lock().unwrap();
        // Those of members retired that finished, let go of (Codex's review of TASK-537.3.5).
        tasks.retain(|task| !task.is_finished());
        for (m, link) in (first..).zip(links) {
            let shared = self.shared.clone();
            tasks.push(runtime.spawn(async move {
                let served = std::panic::AssertUnwindSafe(serve(&shared, m, &*link));
                if let Err(panic) = served.catch_unwind().await {
                    shared.panicked(m, &*panic);
                }
            }));
        }
    }

    /// The commits through `flushed`'s epoch are being flushed: hands their shared batches directly
    /// to the members in memory without writing a separate spool on disk (board 6902).
    pub fn flushed(&self, flushed: Flushed) {
        self.push_batches(flushed, false);
    }

    /// Seeds the replicator with logs recovered from a previous configuration at take time: pinned
    /// in memory until acknowledged by the bucket (their local `logs/` were closed at recovery).
    pub fn seed(&self, epoch: u64, seed: Vec<(u64, Vec<u8>)>) {
        let appends = seed.into_iter().map(|(log, bytes)| (log, 0, Arc::from(bytes))).collect();
        self.push_batches(Flushed { epoch, appends }, true);
    }

    fn push_batches(&self, flushed: Flushed, pinned: bool) {
        let mut state = self.shared.state.lock().unwrap();
        let mut ends = Vec::with_capacity(flushed.appends.len());
        for (log, offset, bytes) in flushed.appends {
            let have = state.logs.get(&log).copied().unwrap_or(0);
            debug_assert_eq!(have, offset, "a flush appends at its log's end");
            let len = bytes.len() as u64;
            if len > 0 {
                state.add_hot(len);
                state.hot.entry(log).or_default().push_back(HotChunk {
                    offset,
                    bytes,
                    persisted: pinned,
                    pinned,
                });
            }
            ends.push((log, have + len));
        }
        for &(log, end) in &ends {
            state.logs.insert(log, end);
        }
        state.last = state.last.max(flushed.epoch);
        for member in state.members.iter_mut().filter(|member| !member.retired) {
            member.marks.push_back((flushed.epoch, ends.clone()));
        }
        let moved = state.advance();
        state.trim_hot();
        drop(state);
        self.shared.changed.notify(usize::MAX);
        if moved {
            (self.shared.notify)();
        }
    }

    /// Marks all in-memory hot batches so far as persisted in the cell's local `logs/` on disk so
    /// they may be evicted once live peers pass them or [`HOT_BYTES`] is exceeded.
    pub fn persisted(&self) {
        let mut state = self.shared.state.lock().unwrap();
        for queue in state.hot.values_mut() {
            for chunk in queue.iter_mut() {
                chunk.persisted = true;
            }
        }
        state.trim_hot();
    }

    /// The head names segment `segment` and those before it: their logs go, here and on the
    /// members.
    pub fn acknowledged(&self, segment: u64) {
        let through = own_of(segment);
        let mut state = self.shared.state.lock().unwrap();
        if state.dropped.is_some_and(|dropped| dropped >= through) {
            return;
        }
        state.dropped = Some(through);
        let gone: Vec<u64> = state.logs.range(..=through).map(|(&log, _)| log).collect();
        for log in gone {
            state.logs.remove(&log);
            if let Some(queue) = state.hot.remove(&log) {
                let freed: u64 = queue.into_iter().map(|c| c.bytes.len() as u64).sum();
                state.sub_hot(freed);
            }
        }
        let moved = state.advance();
        drop(state);
        self.shared.changed.notify(usize::MAX);
        if moved {
            (self.shared.notify)();
        }
    }

    /// Starts replacing its configuration with one of `f`, its members reached by `links` (2f of
    /// them, the writer aside), each sent every log spooled, from its start, and every flush from
    /// now (doc-21, Replacing a member; TASK-537.3.5). Until [`Replicator::swapped`], a commit is
    /// replicated only once quorums of both hold it; the replacement may be swapped in once
    /// [`Replicator::installed`].
    pub fn replacing(&self, f: usize, links: Vec<Arc<dyn MemberLink>>) {
        let mut state = self.shared.state.lock().unwrap();
        assert!(state.pending.is_none(), "one replacement at a time");
        let first = state.members.len();
        let tail: Vec<(u64, u64)> = state.logs.iter().map(|(&log, &have)| (log, have)).collect();
        // Everything flushed it holds once it holds the tail: or now, if nothing is spooled. What
        // the bucket holds it is never sent: nothing of it was spooled for it.
        let member = Member {
            dropped: state.dropped,
            holds: if tail.is_empty() { state.last } else { 0 },
            marks: match tail.is_empty() {
                true => VecDeque::new(),
                false => VecDeque::from([(state.last, tail)]),
            },
            ..Member::default()
        };
        state.members.extend(std::iter::repeat_n(member, links.len()));
        state.pending = Some(Group { f, members: first..first + links.len() });
        let moved = state.advance();
        self.links.lock().unwrap().extend(links.iter().cloned());
        drop(state);
        if self.threaded.load(std::sync::atomic::Ordering::Acquire) {
            self.serve(first, links);
        }
        if moved {
            (self.shared.notify)();
        }
    }

    /// Whether the replacement may be swapped into the head, the bucket's head acknowledging the
    /// commits through `uploaded`: once f+1 of its members, the writer's own copy one, hold
    /// every commit told replicated (installFirst), the bucket's counting as held; or, of f = 0,
    /// once the bucket holds them (zeroAfterUpload). Of f >= 1, never on the bucket alone, as the
    /// spec's Issue: its members are to take what the writer sends before they are its
    /// configuration (TASK-537.3.5's review). No replacement, none.
    pub fn installed(&self, uploaded: u64) -> bool {
        let state = self.shared.state.lock().unwrap();
        let Some(pending) = &state.pending else { return false };
        match pending.f {
            0 => uploaded >= state.replicated,
            // Of those up: one that refuses, or failed what it was sent, is no member to swap in,
            // whatever the bucket holds of what it was to take (the review's: members picked as
            // they answered, then refusing every append).
            f => {
                let members = &state.members[pending.members.clone()];
                let up = members.iter().filter(|member| member.down.is_none() && !member.refused);
                up.filter(|member| member.holds >= state.replicated).count() >= f
            }
        }
    }

    /// The replacement's swap landed: it alone counts from now, the members of the configuration
    /// it replaced sent nothing more.
    pub fn swapped(&self) {
        let mut state = self.shared.state.lock().unwrap();
        let pending = state.pending.take().expect("a replacement swapped");
        let gone = state.active.replace(pending).expect("an active configuration");
        retire(&mut state, gone);
        let moved = state.advance();
        drop(state);
        self.shared.changed.notify(usize::MAX);
        if moved {
            (self.shared.notify)();
        }
    }

    /// Gives the replacement up, its swap never sent: the active configuration alone counts
    /// again. The indices, among its members after the writer, of those that refused the writer.
    pub fn abandon(&self) -> Vec<usize> {
        let mut state = self.shared.state.lock().unwrap();
        let mut refusing = Vec::new();
        if let Some(pending) = state.pending.take() {
            let first = pending.members.start;
            refusing = (pending.members.clone()).filter(|&m| state.members[m].refused).collect();
            refusing.iter_mut().for_each(|m| *m -= first);
            retire(&mut state, pending);
        }
        let moved = state.advance();
        drop(state);
        self.shared.changed.notify(usize::MAX);
        if moved {
            (self.shared.notify)();
        }
        refusing
    }

    /// Whether a replacement is under way.
    pub fn replacing_now(&self) -> bool {
        self.shared.state.lock().unwrap().pending.is_some()
    }

    /// The epoch of the last commit on f members besides the writer. It counts only once the
    /// bucket holds every commit through [`Replicator::base`]: what a caller tells replicated is
    /// `max(uploaded, replicated)` if `uploaded >= base`, else `uploaded` ([`Replicator::told`]).
    pub fn replicated(&self) -> u64 {
        self.shared.state.lock().unwrap().replicated
    }

    /// The epoch through which a guest may be told its commits are replicated, the bucket holding
    /// those through `uploaded`.
    pub fn told(&self, uploaded: u64) -> u64 {
        match uploaded >= self.base() {
            true => uploaded.max(self.replicated()),
            false => uploaded,
        }
    }

    /// The epoch of the last commit no member holds: before it started, or recorded since in a
    /// segment whose log it started in the middle of.
    pub fn base(&self) -> u64 {
        self.shared.base.load(std::sync::atomic::Ordering::Acquire)
    }

    /// The commits through `epoch` are on no member: none after them is told replicated before
    /// the bucket holds them.
    pub fn raise_base(&self, epoch: u64) {
        self.shared.base.fetch_max(epoch, std::sync::atomic::Ordering::AcqRel);
    }

    /// Why the writer was fenced, if a member said a later epoch fenced the cell.
    pub fn fenced(&self) -> Option<String> {
        self.shared.state.lock().unwrap().fenced.clone()
    }

    /// Why each member of the active configuration down or refusing the writer is.
    pub fn down(&self) -> Vec<String> {
        let state = self.shared.state.lock().unwrap();
        state.active().iter().filter_map(|member| Some(member.down.as_ref()?.0.clone())).collect()
    }

    /// The active configuration's f.
    pub fn f(&self) -> usize {
        self.shared.state.lock().unwrap().active.as_ref().map_or(0, |group| group.f)
    }

    /// How many members of the active configuration, the writer aside, take what it sends.
    pub fn members_up(&self) -> usize {
        let state = self.shared.state.lock().unwrap();
        state.active().iter().filter(|member| member.down.is_none()).count()
    }

    /// Whether a member of the active configuration refuses the writer, or has been down for
    /// `long` at least: one to replace (doc-21, Replacing a member).
    pub fn lost(&self, long: Duration) -> bool {
        let state = self.shared.state.lock().unwrap();
        state.active().iter().any(|member| {
            member.refused || member.down.as_ref().is_some_and(|(_, since)| since.elapsed() >= long)
        })
    }

    /// The indices, in the active configuration's members after the writer, of those refusing
    /// it: not to be picked again at once.
    pub fn refusing(&self) -> Vec<usize> {
        let state = self.shared.state.lock().unwrap();
        let active = state.active();
        (0..active.len()).filter(|&m| active[m].refused).collect()
    }

    /// Whether fewer than f members can take what it sends, its configuration being of f = 0
    /// (no quorum), or the writer is fenced: its commits then wait for the bucket.
    pub fn degraded(&self) -> bool {
        let state = self.shared.state.lock().unwrap();
        let f = state.active.as_ref().map_or(0, |group| group.f);
        let up = state.active().iter().filter(|member| member.down.is_none()).count();
        state.fenced.is_some() || state.broken.is_some() || f == 0 || up < f
    }

    /// Breaks it, for `why`: nothing more is sent, its commits waiting for the bucket.
    pub fn break_with(&self, why: String) {
        log::error!("cladefs: replicating: {why}");
        self.shared.state.lock().unwrap().broken.get_or_insert(why);
        self.shared.changed.notify(usize::MAX);
        (self.shared.notify)();
    }

    /// Why its spool failed, if it did.
    pub fn broken(&self) -> Option<String> {
        self.shared.state.lock().unwrap().broken.clone()
    }

    /// The bytes of logs not yet acknowledged in the bucket.
    pub fn spooled(&self) -> u64 {
        self.shared.state.lock().unwrap().logs.values().sum()
    }

    /// The bytes currently held in the hot in-memory window.
    pub fn hot_bytes(&self) -> u64 {
        self.shared.state.lock().unwrap().hot_bytes
    }

    /// Stops it, and clears its in-memory state: its cell replicated no more.
    pub fn stop(&self) {
        let mut state = self.shared.state.lock().unwrap();
        state.stopping = true;
        state.logs.clear();
        state.clear_hot();
        drop(state);
        self.shared.changed.notify(usize::MAX);
    }
}

/// Retires the members of `group`, a configuration replaced or given up: their tasks stop.
fn retire(state: &mut State, group: Group) {
    for member in &mut state.members[group.members] {
        member.retired = true;
        member.marks.clear();
        member.ends.clear();
    }
}

impl Drop for Replicator {
    /// Its members' tasks stop: each at its next turn, or as it waits for one, holding no thread
    /// meanwhile. What a task sends as it is dropped is a member's to take or not: nothing waits
    /// for its answer.
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().unwrap();
        state.stopping = true;
        state.clear_hot();
        drop(state);
        self.shared.changed.notify(usize::MAX);
    }
}

impl Shared {
    /// cladefs: member `m`'s task panicked, with `panic` (board 8505): the member is down for good,
    /// as one out of reach is, which its writer's keeper replaces once it has been for long
    /// (`lost`), and counts in what is degraded; before, it counted as up for ever, its ends
    /// never moving, and was never replaced. Panicking holding the state, which may be half
    /// changed, it breaks the replicator, its commits waiting for the bucket, and clears the
    /// poison, which every lock of it after would panic on.
    fn panicked(&self, m: usize, panic: &(dyn std::any::Any + Send)) {
        let message = (panic.downcast_ref::<&str>().copied())
            .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("of no message");
        let why = format!("its task panicked: {message}");
        log::error!("cladefs: replicating: member {m} down for good: {why}");
        let poisoned = self.state.is_poisoned();
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if poisoned {
            log::error!("cladefs: replicating: broken, its state left as a panic left it");
            state.broken.get_or_insert_with(|| format!("member {m}'s task panicked holding it"));
            self.state.clear_poison();
        }
        state.members[m].down = Some((why, std::time::Instant::now()));
        drop(state);
        self.changed.notify(usize::MAX);
        (self.notify)();
    }
}

/// Sends member `m`, through `link`, what it lacks, until the replicator stops or retires it: a
/// [`turn`] after another, waiting while it has nothing to send, and backing off, from 50 ms,
/// doubling, to 5 s, while the member is out of reach. A task, which holds no thread as it waits.
async fn serve(shared: &Shared, m: usize, link: &dyn MemberLink) {
    let mut wait = Duration::from_millis(50);
    loop {
        match turn(shared, m, link).await {
            Turn::Stop => return,
            Turn::Sent => wait = Duration::from_millis(50),
            Turn::Again => {}
            Turn::Retry => {
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_secs(5));
            }
            Turn::Idle => loop {
                let changed = shared.changed.listen();
                {
                    let state = shared.state.lock().unwrap();
                    if state.next(m).is_some() {
                        break;
                    }
                    if state.stopping || state.members[m].retired {
                        return;
                    }
                }
                changed.await;
            },
        }
    }
}

/// Sends member `m`, through `link`, what it lacks next, if anything, and takes in its answer.
async fn turn(shared: &Shared, m: usize, link: &dyn MemberLink) -> Turn {
    let mut sending = {
        let state = shared.state.lock().unwrap();
        if state.stopping || state.members[m].retired {
            return Turn::Stop;
        }
        match state.next(m) {
            Some(sending) => sending,
            None if state.members[m].refused
                || state.fenced.is_some()
                || state.broken.is_some() =>
            {
                return Turn::Stop;
            }
            None => return Turn::Idle,
        }
    };
    let done = match sending {
        Sending::Drop(through) => link.drop_through(through).await.map(|()| None),
        Sending::Append { log, offset, len, ref mut hot } => {
            let bytes = match hot.take() {
                Some(bytes) => Ok(bytes),
                None => match &shared.source {
                    Some(source) => crate::upload::blocking(|| source.read(log, offset, len)),
                    None => Err(std::io::Error::other("evicted without log source")),
                },
            };
            match bytes {
                Ok(bytes) if !bytes.is_empty() => link.append(log, offset, bytes).await.map(Some),
                // Dropped since it was chosen, the head naming its segment (Codex's review of
                // TASK-537.3.3): nothing to send, nothing broken.
                _ if shared.state.lock().unwrap().dropped.is_some_and(|dropped| log <= dropped) => {
                    return Turn::Again;
                }
                // The replicator stopped meanwhile: nothing broken.
                _ if shared.state.lock().unwrap().stopping => return Turn::Stop,
                read => {
                    let why = match read {
                        Ok(bytes) => {
                            format!("log {log}: {} of {len} bytes read at {offset}", bytes.len())
                        }
                        Err(err) => format!("reading log {log}: {err}"),
                    };
                    log::error!("cladefs: replicating: {why}");
                    shared.state.lock().unwrap().broken = Some(why);
                    shared.changed.notify(usize::MAX);
                    (shared.notify)();
                    return Turn::Stop;
                }
            }
        }
    };
    let mut state = shared.state.lock().unwrap();
    let moved = match (sending, done) {
        (Sending::Drop(through), Ok(_)) => {
            state.members[m].dropped = Some(through);
            state.members[m].ends.retain(|&log, _| log > through);
            // Up again, if it was down: its first answer may be a drop, nothing else left to
            // send it (TASK-537.3.5's review).
            state.members[m].down = None;
            false
        }
        (Sending::Append { log, .. }, Ok(Some(end))) => {
            // Its end as it says: less than it was sent if it lacked what came before.
            state.members[m].ends.insert(log, end);
            state.members[m].down = None;
            let moved = state.advance();
            state.trim_hot();
            moved
        }
        (_, Ok(_)) => false,
        (_, Err(LinkError::Fenced(why))) => {
            log::warn!("cladefs: replicating: fenced: {why}");
            state.fenced = Some(why);
            drop(state);
            shared.changed.notify(usize::MAX);
            (shared.notify)();
            return Turn::Stop;
        }
        (_, Err(LinkError::Refused(why))) => {
            log::warn!("cladefs: replicating: member {m} refuses the writer: {why}");
            state.members[m].refused = true;
            state.members[m].down = Some((why, std::time::Instant::now()));
            return Turn::Stop;
        }
        (_, Err(LinkError::Down(why))) => {
            let member = &mut state.members[m];
            match &mut member.down {
                Some((was, _)) => *was = why,
                None => {
                    log::warn!("cladefs: replicating: member {m} down: {why}");
                    member.down = Some((why, std::time::Instant::now()));
                }
            }
            return match state.stopping || state.members[m].retired {
                true => Turn::Stop,
                false => Turn::Retry,
            };
        }
    };
    drop(state);
    if moved {
        (shared.notify)();
    }
    Turn::Sent
}

#[cfg(test)]
mod tests;
