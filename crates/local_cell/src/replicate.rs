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
//! A replicator starts as its store opens after a taking, whose opening closed every segment
//! before (`wal::recover`), so that each segment it streams it streams from its log's header. The
//! commits before it started are on no member: none is told replicated before the bucket holds
//! them ([`Replicator::base`]), so that what a guest is told replicated is a prefix of the cell's
//! commits, each in the bucket or on f+1 copies.
//!
//! Each member is sent what it lacks, by a thread of its own, through a [`MemberLink`], in pieces
//! of [`PIECE`] at most, and answers the end of the log it holds, synced; one answering less than
//! it was sent (an append past its end, as a member that lost what it had) is sent again from its
//! end. A member out of reach is tried again, from 50 ms apart, doubling, to 5 s; one that refuses
//! the writer (another incarnation, no replica directory) is sent nothing more, until a
//! replacement (TASK-537.3.5); one that says a later epoch fenced the cell fences the writer: no
//! commit of it is told replicated after.
//!
//! What it sends is spooled ([`Spool`]), not held in memory: a store's spool is a directory of its
//! own, its files bypassing the host's page cache as every store file does, appended to as each
//! flush is sent and read back as each member is sent what it lacks, each log's files removed as
//! the head names its segment. The replicator holds each log's length alone. The spool is no
//! durable state, the store's own log being: it is synced never, and emptied as a replicator
//! starts. Its failing breaks the replicator, whose commits then wait for the bucket.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// The most of a log a member is sent at once.
pub const PIECE: usize = 4 << 20;

/// The log a member keeps segment `segment`'s replica log in.
pub fn log_of(segment: u64) -> u64 {
    segment << 1
}

/// The log a member keeps the extents segment `segment`'s commits stored on their own in.
pub fn own_of(segment: u64) -> u64 {
    (segment << 1) | 1
}

/// An extent stored on its own as its segment's own log holds it: its sequence (u64), its file's
/// length (u64), little-endian, then the file's bytes, the bucket's object byte for byte.
pub fn own_frame(sequence: u64, file: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(16 + file.len());
    frame.extend_from_slice(&sequence.to_le_bytes());
    frame.extend_from_slice(&(file.len() as u64).to_le_bytes());
    frame.extend_from_slice(file);
    frame
}

/// The extents an own log's bytes hold, whole: each sequence and file.
pub fn own_frames(mut bytes: &[u8]) -> Vec<(u64, Vec<u8>)> {
    let mut frames = Vec::new();
    while bytes.len() >= 16 {
        let sequence = u64::from_le_bytes(bytes[..8].try_into().unwrap());
        let len = u64::from_le_bytes(bytes[8..16].try_into().unwrap()) as usize;
        let Some(file) = bytes.get(16..16 + len) else { break };
        frames.push((sequence, file.to_vec()));
        bytes = &bytes[16 + len..];
    }
    frames
}

/// What of `logs`, what members hold of a replicated cell `cell`'s writer's logs by their numbers,
/// goes on from `last`, the last commit the cell's head names (TASK-537.3.4): those segments,
/// whole, after the one whose commits end at `last`, a member dropping what the head names after
/// the head names it; all of them if they begin after `last`. Refused if they neither reach `last`
/// nor begin right after it: what is between, on no member, may be commits a guest was told are
/// replicated.
pub fn trimmed(
    cell: extent_frame::segment::CellId,
    logs: &[(u64, Vec<u8>)],
    last: Option<u64>,
) -> Result<Vec<(u64, Vec<u8>)>, anyhow::Error> {
    use crate::layout::SegmentId;
    use extent_frame::segment::epoch_of;
    let segment = |log: u64| SegmentId { cell: cell.id, number: log >> 1 };
    // Each segment's commits, in order, and the last commit as of the end of each.
    let numbers: std::collections::BTreeSet<u64> = logs.iter().map(|(log, _)| log >> 1).collect();
    let mut heads = Vec::with_capacity(numbers.len());
    let mut first_commit = None;
    let mut head = None;
    for &number in &numbers {
        let replica = logs.iter().find(|(log, _)| *log == log_of(number));
        let commits = replica
            .map(|(_, bytes)| crate::wal::replica_commits(bytes, segment(log_of(number))))
            .unwrap_or_default();
        first_commit = first_commit.or(commits.first().copied());
        head = commits.last().copied().or(head);
        heads.push((number, head));
    }
    let landed = heads.iter().position(|&(_, head)| head.is_some() && head == last);
    let kept: std::collections::BTreeSet<u64> = match landed {
        Some(at) => heads[at + 1..].iter().map(|&(number, _)| number).collect(),
        None => {
            let follows = match (last, first_commit) {
                (_, None) => true,
                (None, Some(first)) => epoch_of(first) <= 1,
                (Some(last), Some(first)) => epoch_of(first) == epoch_of(last) + 1,
            };
            let behind = match (last, head) {
                (Some(last), Some(head)) => epoch_of(head) <= epoch_of(last),
                _ => false,
            };
            match (follows, behind) {
                (true, _) => numbers.clone(),
                (false, true) => Default::default(),
                (false, false) => anyhow::bail!(
                    "the members' logs of {cell:?} begin at commit {first_commit:?}, neither reaching \
                     the head's last, {last:?}, nor going on from it"
                ),
            }
        }
    };
    Ok(logs.iter().filter(|(log, _)| kept.contains(&(log >> 1))).cloned().collect())
}

/// Writes what a member holds of a replicated cell's writer's logs, `logs` by their numbers in its
/// directory, into `dir`, a store's directory of `cell`, made and holding no write-ahead log, as
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
    let kept = trimmed(cell, logs, last)?;
    let write = |path: &str, bytes: &[u8], data: bool| -> Result<(), anyhow::Error> {
        let file = dir.create(path, data).with_context(|| format!("making {path}"))?;
        if !bytes.is_empty() {
            crate::File::write_at(&file, bytes, 0).with_context(|| format!("writing {path}"))?;
        }
        crate::File::sync(&file).with_context(|| format!("syncing {path}"))
    };
    for (log, bytes) in &kept {
        match log & 1 {
            0 => {
                write(&log_path(segment(*log)), bytes, false)?;
                write(&open_segment_path(segment(*log)), &[], true)?;
            }
            _ => {
                for (sequence, file) in own_frames(bytes) {
                    let extent = storage_device::ExtentId { cell: cell.id, sequence };
                    write(&extent_path(extent), &file, true)?;
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

/// Where a replicator keeps what it sends, until the bucket holds it: logs by their numbers,
/// appended to, read back, and removed. None of it synced.
pub trait Spool: Send + Sync + 'static {
    /// Appends `bytes` at `offset`, the log's end.
    fn append(&self, log: u64, offset: u64, bytes: &[u8]) -> std::io::Result<()>;
    /// `len` bytes at `offset` of the log.
    fn read(&self, log: u64, offset: u64, len: u64) -> std::io::Result<Vec<u8>>;
    /// Removes the log, if it is there.
    fn remove(&self, log: u64) -> std::io::Result<()>;
}

/// A spool in memory, for tests.
#[derive(Default)]
pub struct MemSpool {
    logs: Mutex<BTreeMap<u64, Vec<u8>>>,
}

impl Spool for MemSpool {
    fn append(&self, log: u64, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
        let mut logs = self.logs.lock().unwrap();
        let held = logs.entry(log).or_default();
        assert_eq!(held.len() as u64, offset, "appended at its end");
        held.extend_from_slice(bytes);
        Ok(())
    }

    fn read(&self, log: u64, offset: u64, len: u64) -> std::io::Result<Vec<u8>> {
        let logs = self.logs.lock().unwrap();
        let held = logs.get(&log).map_or(&[][..], Vec::as_slice);
        let from = (offset as usize).min(held.len());
        let to = (offset + len).min(held.len() as u64) as usize;
        Ok(held[from..to.max(from)].to_vec())
    }

    fn remove(&self, log: u64) -> std::io::Result<()> {
        self.logs.lock().unwrap().remove(&log);
        Ok(())
    }
}

/// How the writer reaches a member of its configuration.
pub trait MemberLink: Send + Sync + 'static {
    /// Appends `bytes` at `offset` of the member's log `log`: the log's end there, synced.
    fn append(&self, log: u64, offset: u64, bytes: &[u8]) -> Result<u64, LinkError>;
    /// Drops the member's logs to `through`, whose segments the head names.
    fn drop_through(&self, through: u64) -> Result<(), LinkError>;
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
    /// Each append: the log, the offset, and the bytes.
    pub appends: Vec<(u64, u64, Vec<u8>)>,
}

/// A replicated cell's writer's replicator: what its members hold, and what is replicated.
pub struct Replicator {
    shared: Arc<Shared>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

struct Shared {
    f: usize,
    /// The epoch of the last commit on no member: before it started, or recorded in a segment
    /// whose log it started in the middle of.
    base: std::sync::atomic::AtomicU64,
    spool: Arc<dyn Spool>,
    state: Mutex<State>,
    changed: Condvar,
    /// Called once more is replicated, or the writer is fenced, holding no lock.
    notify: Box<dyn Fn() + Send + Sync>,
}

#[derive(Default)]
struct State {
    /// Each log's length in the spool, until its segment is in the bucket.
    logs: BTreeMap<u64, u64>,
    /// The logs dropped, to this one, their segments in the bucket.
    dropped: Option<u64>,
    /// Each flush's epoch and the ends of the logs it appended to, those not every member holds.
    marks: VecDeque<(u64, Vec<(u64, u64)>)>,
    /// The index of the first of `marks` among every flush's.
    first_mark: usize,
    members: Vec<Member>,
    replicated: u64,
    fenced: Option<String>,
    /// Why the spool failed, if it did: nothing more is sent.
    broken: Option<String>,
    stopping: bool,
}

#[derive(Clone, Debug, Default)]
struct Member {
    /// The ends of its logs it answered, synced.
    ends: BTreeMap<u64, u64>,
    /// The index among every flush's of the first it holds not.
    next_mark: usize,
    /// The epoch of the last flush it holds whole.
    holds: u64,
    /// The logs dropped on it, to this one.
    dropped: Option<u64>,
    /// Why it holds nothing more, if it is down or refused.
    down: Option<String>,
    refused: bool,
}

impl State {
    /// Whether member `m` holds a mark's `ends`: every log it appended to, to its end, or dropped,
    /// in the bucket.
    fn holds(&self, m: usize, ends: &[(u64, u64)]) -> bool {
        ends.iter().all(|&(log, end)| {
            self.dropped.is_some_and(|dropped| log <= dropped)
                || self.members[m].ends.get(&log).is_some_and(|&held| held >= end)
        })
    }

    /// Moves each member past the marks it holds, and drops the marks every member holds: whether
    /// the replicated epoch moved.
    fn advance(&mut self, f: usize) -> bool {
        for m in 0..self.members.len() {
            loop {
                let next = self.members[m].next_mark;
                let Some((epoch, ends)) = self.marks.get(next - self.first_mark) else { break };
                if !self.holds(m, ends) {
                    break;
                }
                let epoch = *epoch;
                let member = &mut self.members[m];
                member.next_mark += 1;
                member.holds = epoch;
            }
        }
        let all = self.members.iter().map(|member| member.next_mark).min().unwrap_or(0);
        while self.first_mark < all {
            self.marks.pop_front();
            self.first_mark += 1;
        }
        let mut holds: Vec<u64> = self.members.iter().map(|member| member.holds).collect();
        holds.sort_unstable_by(|a, b| b.cmp(a));
        let replicated = match f {
            0 => 0,
            f => holds.get(f - 1).copied().unwrap_or(0),
        };
        let moved = replicated > self.replicated;
        self.replicated = self.replicated.max(replicated);
        moved
    }

    /// What member `m` is to be sent next: its logs' drop, or an append from its end.
    fn next(&self, m: usize) -> Option<Sending> {
        let member = &self.members[m];
        if member.refused || self.fenced.is_some() || self.broken.is_some() {
            return None;
        }
        if self.dropped > member.dropped {
            return self.dropped.map(Sending::Drop);
        }
        self.logs.iter().find_map(|(&log, &have)| {
            let end = member.ends.get(&log).copied().unwrap_or(0);
            (end < have).then(|| {
                let len = (have - end).min(PIECE as u64);
                Sending::Append { log, offset: end, len }
            })
        })
    }
}

enum Sending {
    Drop(u64),
    Append { log: u64, offset: u64, len: u64 },
}

impl Replicator {
    /// A replicator of a configuration of `f`, its members reached by `links` (2f of them, the
    /// writer aside), started after the commit of epoch `base`, which none holds, spooling what it
    /// sends in `spool`, empty: `notify` called once more is replicated, or the writer is fenced.
    pub fn start(
        f: usize,
        base: u64,
        spool: Arc<dyn Spool>,
        links: Vec<Arc<dyn MemberLink>>,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Arc<Self> {
        let state = State { members: vec![Member::default(); links.len()], ..State::default() };
        let shared = Arc::new(Shared {
            f,
            base: base.into(),
            spool,
            state: Mutex::new(state),
            changed: Condvar::new(),
            notify: Box::new(notify),
        });
        let threads = links
            .into_iter()
            .enumerate()
            .map(|(m, link)| {
                let shared = shared.clone();
                std::thread::Builder::new()
                    .name("cladefs replicate".into())
                    .spawn(move || serve(&shared, m, &*link))
                    .expect("a replicating thread")
            })
            .collect();
        Arc::new(Self { shared, threads: Mutex::new(threads) })
    }

    /// The commits through `flushed`'s epoch are local: spools them, and sends them to the
    /// members. Fails if the spool does: nothing of them is sent, and the replicator is broken.
    pub fn flushed(&self, flushed: Flushed) -> std::io::Result<()> {
        let mut state = self.shared.state.lock().unwrap();
        let mut ends = Vec::with_capacity(flushed.appends.len());
        for (log, offset, bytes) in &flushed.appends {
            let have = state.logs.get(log).copied().unwrap_or(0);
            debug_assert_eq!(have, *offset, "a flush appends at its log's end");
            if let Err(err) = self.shared.spool.append(*log, have, bytes) {
                state.broken = Some(format!("spooling log {log}: {err}"));
                drop(state);
                self.shared.changed.notify_all();
                (self.shared.notify)();
                return Err(err);
            }
            ends.push((*log, have + bytes.len() as u64));
        }
        for &(log, end) in &ends {
            state.logs.insert(log, end);
        }
        state.marks.push_back((flushed.epoch, ends));
        drop(state);
        self.shared.changed.notify_all();
        Ok(())
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
            if let Err(err) = self.shared.spool.remove(log) {
                log::warn!("cladefs: replicating: removing spooled log {log}: {err}");
            }
        }
        let moved = state.advance(self.shared.f);
        drop(state);
        self.shared.changed.notify_all();
        if moved {
            (self.shared.notify)();
        }
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

    /// Why each member down or refusing the writer is.
    pub fn down(&self) -> Vec<String> {
        let state = self.shared.state.lock().unwrap();
        state.members.iter().filter_map(|member| member.down.clone()).collect()
    }

    /// Whether fewer than f members can take what it sends, or the writer is fenced: its commits
    /// then wait for the bucket.
    pub fn degraded(&self) -> bool {
        let state = self.shared.state.lock().unwrap();
        let up = state.members.iter().filter(|member| member.down.is_none()).count();
        state.fenced.is_some() || state.broken.is_some() || up < self.shared.f
    }

    /// Why its spool failed, if it did.
    pub fn broken(&self) -> Option<String> {
        self.shared.state.lock().unwrap().broken.clone()
    }

    /// The bytes it spools of logs not in the bucket.
    pub fn spooled(&self) -> u64 {
        self.shared.state.lock().unwrap().logs.values().sum()
    }
}

impl Drop for Replicator {
    fn drop(&mut self) {
        self.shared.state.lock().unwrap().stopping = true;
        self.shared.changed.notify_all();
        for thread in self.threads.lock().unwrap().drain(..) {
            let _ = thread.join();
        }
    }
}

/// Sends member `m`, through `link`, what it lacks, until the replicator stops.
fn serve(shared: &Shared, m: usize, link: &dyn MemberLink) {
    let mut wait = Duration::from_millis(50);
    loop {
        let sending = {
            let mut state = shared.state.lock().unwrap();
            loop {
                if state.stopping {
                    return;
                }
                if let Some(sending) = state.next(m) {
                    break sending;
                }
                state = shared.changed.wait(state).unwrap();
            }
        };
        let done = match &sending {
            Sending::Drop(through) => link.drop_through(*through).map(|()| None),
            Sending::Append { log, offset, len } => match shared.spool.read(*log, *offset, *len) {
                Ok(bytes) if bytes.len() as u64 == *len => {
                    link.append(*log, *offset, &bytes).map(Some)
                }
                read => {
                    let why = match read {
                        Ok(bytes) => format!("log {log}: {} of {len} bytes spooled", bytes.len()),
                        Err(err) => format!("reading spooled log {log}: {err}"),
                    };
                    log::error!("cladefs: replicating: {why}");
                    shared.state.lock().unwrap().broken = Some(why);
                    shared.changed.notify_all();
                    (shared.notify)();
                    return;
                }
            },
        };
        let mut state = shared.state.lock().unwrap();
        let moved = match (sending, done) {
            (Sending::Drop(through), Ok(_)) => {
                state.members[m].dropped = Some(through);
                state.members[m].ends.retain(|&log, _| log > through);
                false
            }
            (Sending::Append { log, .. }, Ok(Some(end))) => {
                // Its end as it says: less than it was sent if it lacked what came before.
                state.members[m].ends.insert(log, end);
                state.members[m].down = None;
                wait = Duration::from_millis(50);
                state.advance(shared.f)
            }
            (_, Ok(_)) => false,
            (_, Err(LinkError::Fenced(why))) => {
                log::warn!("cladefs: replicating: fenced: {why}");
                state.fenced = Some(why);
                drop(state);
                shared.changed.notify_all();
                (shared.notify)();
                return;
            }
            (_, Err(LinkError::Refused(why))) => {
                log::warn!("cladefs: replicating: member {m} refuses the writer: {why}");
                state.members[m].refused = true;
                state.members[m].down = Some(why);
                return;
            }
            (_, Err(LinkError::Down(why))) => {
                if state.members[m].down.is_none() {
                    log::warn!("cladefs: replicating: member {m} down: {why}");
                }
                state.members[m].down = Some(why);
                let stopping = state.stopping;
                drop(state);
                if stopping {
                    return;
                }
                std::thread::sleep(wait);
                wait = (wait * 2).min(Duration::from_secs(5));
                continue;
            }
        };
        drop(state);
        if moved {
            (shared.notify)();
        }
    }
}

#[cfg(test)]
mod tests;
