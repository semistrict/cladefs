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
//! of [`PIECE`] at most (or, in the simulation, one turn at a time as its scheduler says,
//! [`Replicator::idle`] and [`Replicator::drive`]), and answers the end of the log it holds, synced; one answering less than
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

/// Whether what members hold, ending at the commit `members`, goes past a store's own write-ahead
/// log, ending at `own` (TASK-537.3.4's review): a later writer's commits, or the same writer's
/// beyond what its own log holds (sent before its own sync, which a crash cut). Then the members'
/// logs, not its own, are what the store recovers: they hold every commit a guest was told
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
    let kept = trimmed(cell, logs, last, None)?;
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
    /// Each member's link, by its place among [`State::members`].
    links: Mutex<Vec<Arc<dyn MemberLink>>>,
    /// Whether each member is served by a thread of its own ([`Replicator::run_threads`]), or
    /// driven, a turn at a time ([`Replicator::drive`]).
    threaded: std::sync::atomic::AtomicBool,
    threads: Mutex<Vec<JoinHandle<()>>>,
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
    spool: Arc<dyn Spool>,
    state: Mutex<State>,
    changed: Condvar,
    /// Called once more is replicated, or the writer is fenced, holding no lock.
    notify: Box<dyn Fn() + Send + Sync>,
}

/// A configuration's members, among a replicator's: its f, and where they are in
/// [`State::members`].
#[derive(Clone, Debug)]
struct Group {
    f: usize,
    members: std::ops::Range<usize>,
}

#[derive(Default)]
struct State {
    /// Each log's length in the spool, until its segment is in the bucket.
    logs: BTreeMap<u64, u64>,
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
    /// Why the spool failed, if it did: nothing more is sent.
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

impl State {
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
                Sending::Append { log, offset: end, len }
            })
        })
    }

    /// The members of the active configuration.
    fn active(&self) -> &[Member] {
        self.active.as_ref().map_or(&[], |group| &self.members[group.members.clone()])
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
        let replicator = Self::idle(f, base, spool, links, notify);
        replicator.run_threads();
        replicator
    }

    /// A replicator as [`Replicator::start`] makes it, whose members nothing serves yet: by a
    /// thread each once [`Replicator::run_threads`] starts them, or a turn at a time as
    /// [`Replicator::drive`] is called, as the simulation's scheduler does, so that a seed decides
    /// what each member is sent and when (TASK-680).
    pub fn idle(
        f: usize,
        base: u64,
        spool: Arc<dyn Spool>,
        links: Vec<Arc<dyn MemberLink>>,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Arc<Self> {
        let state = State {
            members: vec![Member::default(); links.len()],
            active: Some(Group { f, members: 0..links.len() }),
            last: base,
            ..State::default()
        };
        let shared = Arc::new(Shared {
            base: base.into(),
            spool,
            state: Mutex::new(state),
            changed: Condvar::new(),
            notify: Box::new(notify),
        });
        Arc::new(Self {
            shared,
            links: Mutex::new(links),
            threaded: false.into(),
            threads: Mutex::default(),
        })
    }

    /// Serves each member by a thread of its own, from now on, and those of replacements.
    pub fn run_threads(&self) {
        self.threaded.store(true, std::sync::atomic::Ordering::Release);
        let links = self.links.lock().unwrap().clone();
        self.serve(0, links);
    }

    /// How many members it has had, those of configurations gone among them: what
    /// [`Replicator::drive`] takes.
    pub fn members(&self) -> usize {
        self.links.lock().unwrap().len()
    }

    /// A turn of member `m`'s, of a replicator no threads serve ([`Replicator::idle`]): sends it
    /// what it lacks next, if anything, and takes in its answer. Whether it was sent anything and
    /// answered.
    pub fn drive(&self, m: usize) -> bool {
        debug_assert!(!self.threaded.load(std::sync::atomic::Ordering::Acquire), "driven");
        let Some(link) = self.links.lock().unwrap().get(m).cloned() else { return false };
        turn(&self.shared, m, &*link) == Turn::Sent
    }

    /// Starts a thread serving each of `links`, the members from `first` on.
    fn serve(&self, first: usize, links: Vec<Arc<dyn MemberLink>>) {
        let mut threads = self.threads.lock().unwrap();
        // Those of members retired that finished, joined (Codex's review of TASK-537.3.5).
        let (finished, running) = std::mem::take(&mut *threads)
            .into_iter()
            .partition::<Vec<_>, _>(|thread| thread.is_finished());
        *threads = running;
        for thread in finished {
            let _ = thread.join();
        }
        for (m, link) in (first..).zip(links) {
            let shared = self.shared.clone();
            let thread = std::thread::Builder::new()
                .name("cladefs replicate".into())
                .spawn(move || serve(&shared, m, &*link))
                .expect("a replicating thread");
            threads.push(thread);
        }
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
        state.last = state.last.max(flushed.epoch);
        for member in state.members.iter_mut().filter(|member| !member.retired) {
            member.marks.push_back((flushed.epoch, ends.clone()));
        }
        let moved = state.advance();
        drop(state);
        self.shared.changed.notify_all();
        if moved {
            (self.shared.notify)();
        }
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
        let moved = state.advance();
        drop(state);
        self.shared.changed.notify_all();
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
        self.shared.changed.notify_all();
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
        self.shared.changed.notify_all();
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
        self.shared.changed.notify_all();
        (self.shared.notify)();
    }

    /// Why its spool failed, if it did.
    pub fn broken(&self) -> Option<String> {
        self.shared.state.lock().unwrap().broken.clone()
    }

    /// The bytes it spools of logs not in the bucket.
    pub fn spooled(&self) -> u64 {
        self.shared.state.lock().unwrap().logs.values().sum()
    }

    /// Stops it, and empties its spool: its cell replicated no more.
    pub fn stop(&self) {
        let mut state = self.shared.state.lock().unwrap();
        state.stopping = true;
        for log in std::mem::take(&mut state.logs).into_keys() {
            if let Err(err) = self.shared.spool.remove(log) {
                log::warn!("cladefs: replicating: removing spooled log {log}: {err}");
            }
        }
        drop(state);
        self.shared.changed.notify_all();
    }
}

/// Retires the members of `group`, a configuration replaced or given up: their threads stop.
fn retire(state: &mut State, group: Group) {
    for member in &mut state.members[group.members] {
        member.retired = true;
        member.marks.clear();
        member.ends.clear();
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

/// Sends member `m`, through `link`, what it lacks, until the replicator stops or retires it: a
/// [`turn`] after another, waiting while it has nothing to send, and backing off, from 50 ms,
/// doubling, to 5 s, while the member is out of reach.
fn serve(shared: &Shared, m: usize, link: &dyn MemberLink) {
    let mut wait = Duration::from_millis(50);
    loop {
        match turn(shared, m, link) {
            Turn::Stop => return,
            Turn::Sent => wait = Duration::from_millis(50),
            Turn::Again => {}
            Turn::Retry => {
                std::thread::sleep(wait);
                wait = (wait * 2).min(Duration::from_secs(5));
            }
            Turn::Idle => {
                let mut state = shared.state.lock().unwrap();
                while state.next(m).is_none() {
                    if state.stopping || state.members[m].retired {
                        return;
                    }
                    state = shared.changed.wait(state).unwrap();
                }
            }
        }
    }
}

/// Sends member `m`, through `link`, what it lacks next, if anything, and takes in its answer.
fn turn(shared: &Shared, m: usize, link: &dyn MemberLink) -> Turn {
    let sending = {
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
    let done = match &sending {
        Sending::Drop(through) => link.drop_through(*through).map(|()| None),
        Sending::Append { log, offset, len } => match shared.spool.read(*log, *offset, *len) {
            Ok(bytes) if bytes.len() as u64 == *len => link.append(*log, *offset, &bytes).map(Some),
            // Dropped since it was chosen, the head naming its segment (Codex's review of
            // TASK-537.3.3): nothing to send, nothing broken.
            _ if shared.state.lock().unwrap().dropped.is_some_and(|dropped| *log <= dropped) => {
                return Turn::Again;
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
                return Turn::Stop;
            }
        },
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
            state.advance()
        }
        (_, Ok(_)) => false,
        (_, Err(LinkError::Fenced(why))) => {
            log::warn!("cladefs: replicating: fenced: {why}");
            state.fenced = Some(why);
            drop(state);
            shared.changed.notify_all();
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
