// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a host's replica directory (doc-21, TASK-537.3.2): the logs of other hosts' cells
//! whose configurations it is a member of, as their writers send them, until the bucket holds
//! them, and each such cell's promise.
//!
//! A member holds, for each configuration of a cell it is in, the writer's segment logs byte for
//! byte, every record held whole (`wal::Runs::Held`): ciphertext under the cell's DEK, and commit
//! metadata MACed, as the bucket's footers are. It opens nothing and checks nothing of them, as it
//! has no key; a taker checks their MACs as it recovers them.
//!
//! Its promise for a cell is the greatest epoch that fenced it: a take fences f+1 members before it
//! reads their logs, and a member refuses every append of an older epoch after, so that no writer
//! the take fenced gathers a quorum behind its read. A promise is on the member's disk, synced
//! before the fence answers; and a disk lost loses its promises with it, so the directory has an
//! incarnation, a random number made with it, which a configuration names its members by: a
//! directory made anew is another member, which appends to configurations naming the one before
//! are refused by (doc-21, Incarnations; the spec's `NoIncarnation`).
//!
//! The directory is flat, as a [`Dir`] removes no directories: `incarnation`, then for each cell
//! `<tenant>.<cell>.promise.<epoch>`, and its logs `<tenant>.<cell>.<number>.<epoch>.<log>`, keyed by the
//! configuration's number and the epoch of the writer that made it: two takers racing may both
//! make the configuration after the active one, each its own. cladefs: every number in base 32,
//! padded, as every other name of cladefs's (`base32_id`, board 10046).
//!
//! Formats: frozen ([`FORMAT`], doc-28, board 10046): every later build reads every version from 4
//! on. Its `incarnation` file says the directory's version ([`HEADER`]); a promise is a name
//! alone. The logs it holds are of frozen families of their own, each saying its version, read
//! by range: a segment's log the cell directory's (`wal::LOG_MAGIC`), an own log's frames extent
//! frames, framed as [`crate::replicate::own_frame`] says. A writer's segment log whose first
//! bytes say a format this build doesn't read is refused as it is appended, before any ack, and as
//! it is read back, never read as holding nothing, which forgot appends it had acked (board 8457).
//!
//! What a peer asks is untrusted (AGENTS.md, Trust boundaries; board 8445): a call is admitted only
//! if the cell's head in the bucket could have asked it ([`admit`]), which the peer service reads
//! for each call before it reaches the directory, so that no peer fences a cell past its head's
//! epoch, at `u64::MAX` for good, nor drops the logs of a configuration its writer appends to not. What a peer's calls hold is
//! bounded (board 8451): every byte appended admitted by the disk governor (the directory's
//! footprint), refused past it; at most [`CONFIGURATIONS_MOST`] configurations of a cell, those its
//! writer gave up trimmed as its next begins ([`Replicas::trim`]); and nothing in memory of a cell
//! holding nothing, the directory listed once, as it opens.

use crate::dir::{Dir, File};
use anyhow::{Context as _, anyhow};
use base32_id::Padded;
use extent_frame::segment::CellId;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::sync::{Arc, Mutex};

/// The directory's incarnation file.
const INCARNATION: &str = "incarnation";

/// The replica directory's family: 2, logs held as their writers send them, no outer framing
/// (board 6902); 3, the formats of the logs it holds named, `cladefs replicas 3, logs ..., own frames
/// ...` (board 8457), refused but by a build of those alone; 4, frozen (doc-28, board 10046): its
/// version alone, the logs it holds saying theirs, and its files' numbers in base 32.
pub const FORMAT: formats::Format = formats::Format::new("replica directory", 4, 4);

/// The incarnation file's first line, before the version.
pub(crate) const HEADER: &str = "cladefs replica directory ";

/// The version the incarnation file's first line, `line`, says, if this build reads it.
fn version_of(line: &str) -> Option<u32> {
    let version = line.strip_prefix(HEADER)?;
    // Digits alone: `u32::from_str` would take a sign too.
    if version.is_empty() || !version.bytes().all(|digit| digit.is_ascii_digit()) {
        return None;
    }
    // Not a version this build reads: None, which the caller refuses with the line and range.
    FORMAT.check(version.parse().ok()?).ok()
}

/// The incarnation file of incarnation `incarnation`, as this build writes it.
fn incarnation_text(incarnation: u64) -> String {
    format!("{HEADER}{}\nincarnation {incarnation}\n", FORMAT.current)
}

/// The incarnation `text`, an incarnation file, names: refused if its version is one this build
/// doesn't read, or it says more or less than an incarnation file does.
fn parse_incarnation(text: &str) -> Result<u64, anyhow::Error> {
    let mut lines = text.lines();
    let first = lines.next().unwrap_or_default();
    if version_of(first).is_none() {
        return Err(anyhow!(
            "{INCARNATION}: {first:?}: this build reads {HEADER}{} to {}",
            FORMAT.oldest,
            FORMAT.current
        ));
    }
    let line = lines.next().unwrap_or_default();
    let number = line.strip_prefix("incarnation ").unwrap_or_default();
    let incarnation =
        number.parse().with_context(|| format!("{INCARNATION}: bad line {line:?}"))?;
    if let Some(more) = lines.next() {
        return Err(anyhow!("{INCARNATION}: a line past its incarnation: {more:?}"));
    }
    Ok(incarnation)
}

/// The most logs a member holds of one configuration (board 8453): a writer sends a segment's two
/// logs ([`crate::replicate::log_of`], [`crate::replicate::own_of`]), each segment holding a commit
/// at least, and its lag lets at most `upload::Options::max_lag` commits (4096) wait for the
/// bucket, whose segments' logs are dropped as it holds them: twice that, doubled again. A take
/// refuses a fence answer naming more, as a member answering so is no member of this build.
pub const LOGS_MOST: usize = 16 << 10;

/// cladefs: the most configurations of a cell a member holds logs of (board 8451): its active one,
/// the next its writer installs (a replacement's, or a take's), a racing taker's of a later epoch,
/// and those given up, which the next one's stream trims ([`Replicas::trim`]); twice as many. A
/// new one past it is refused, so that no peer makes a cell's logs grow without bound, each
/// configuration to the lag bound.
pub const CONFIGURATIONS_MOST: usize = 8;

/// Maximum open replica log file handles cached across all cells (board 6902): keeps sequential
/// append handles (`DiskFile` direct-I/O tail) warm without allowing tiny logs to exhaust
/// `RLIMIT_NOFILE`.
const OPEN_FILES: usize = 64;

/// A configuration of a cell, as a member keys its logs: its number, and the epoch of the writer
/// that made it, which alone appends to it. Ordered as configurations supersede each other: by
/// number, then epoch (two takers racing make the same number, the later's epoch greater).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key {
    pub number: u64,
    pub epoch: u64,
}

/// What a member refuses.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Refused {
    /// The call named another incarnation of the directory than this one: it is not that member.
    #[error("incarnation {asked} asked, this directory's is {ours}")]
    Incarnation { asked: u64, ours: u64 },
    /// A take of a later epoch fenced the cell: appends of this one, or fences of an earlier, are
    /// refused.
    #[error("fenced at epoch {promise}")]
    Fenced { promise: u64 },
    /// The configuration's logs would hold more than the member holds for a configuration.
    #[error("the configuration's logs would hold {would} bytes, past {most}")]
    Full { would: u64, most: u64 },
    /// The configuration would have more logs than a member holds of one ([`LOGS_MOST`]).
    #[error("the configuration would have more than {most} logs")]
    Logs { most: usize },
    /// A segment's log, as its writer sent it or as the directory holds it, is of another format
    /// than this build's (board 8457): its first bytes are not `wal::LOG_MAGIC`.
    #[error(
        "{log}: {found}, where {expected} begins a log of this build's format: refused",
        expected = crate::wal::LOG_MAGIC.escape_ascii()
    )]
    Format { log: String, found: String },
    /// cladefs: the bucket holds no live head of the cell, replicated (board 8445): no writer or
    /// take of it asks a member anything.
    #[error("{cell}: the bucket holds no live replicated head of it: refused")]
    Unheaded { cell: CellId },
    /// cladefs: the cell's head, as the bucket holds it, could not have asked it (board 8445):
    /// its writer appends to its active configuration or one after it of its epoch, a take fences
    /// the active one at no later epoch than the head's, and drops are of its writer's
    /// configurations.
    #[error(
        "{asked} of {cell}, its head being of epoch {epoch}, segments to {end}, {active}: refused"
    )]
    Unasked { cell: CellId, asked: String, epoch: u64, end: u64, active: String },
    /// cladefs: the disk governor allows the append no room (board 8451).
    #[error("no room on the disk for {bytes} bytes: {full}")]
    Disk { bytes: u64, full: String },
    /// cladefs: the cell would have more configurations' logs here than [`CONFIGURATIONS_MOST`]
    /// (board 8451).
    #[error("{cell} would have logs of more than {most} configurations here")]
    Configurations { cell: CellId, most: usize },
}

/// cladefs: what a cell's head in the bucket says, as a member checks a peer's call against it
/// (board 8445): the epoch of its newest taking, the segments it names (0 to `end`), and its active
/// configuration, if it names one. Each only grows as the cell goes on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seen {
    pub epoch: u64,
    pub end: u64,
    pub active: Option<Key>,
}

/// cladefs: a call a peer asks of a member, as [`admit`] checks it against the cell's head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Asked {
    /// Appends to configuration `0`'s logs ([`Replicas::append`]).
    Append(Key),
    /// A fence of configuration `0`'s cell at epoch `1` ([`Replicas::fence`]).
    Fence(Key, u64),
    /// Drops ([`Replicas::drop_logs`]) of configuration `0`'s logs through `1`, or of those of
    /// every configuration before it.
    Drop(Key, Option<u64>),
}

/// cladefs: whether the head of `cell`, as `head` says it (none if the bucket holds no live head of
/// it, or none replicated), could have asked `asked` (board 8445): Unheaded or Unasked if not.
///
/// - Appends go to the active configuration, or one after it made at the head's epoch, which its
///   writer's replacement, or a take, installs before the head names it (doc-21); with none
///   active, one of the head's epoch.
/// - A take fences the active configuration, once it took the head, at the head's epoch: none at a
///   later one, so that no promise is past what a writer of the cell holds, and its writer's
///   appends go on.
/// - Drops through a log are of a configuration its writer appends to, as appends are; those of
///   every configuration before one, of no later one than the active. A log is named by its
///   segment's number in its writer's cell ([`crate::replicate::log_of`]), not the bucket's, which
///   the head names: no head says which logs the bucket holds, and a drop through any log of its
///   writer's configuration is its writer's to ask (hotbed question 499, board 10095).
///
/// What it admits under a head it admits under any later one: each of its fields only grows.
pub fn admit(cell: CellId, head: Option<&Seen>, asked: Asked) -> Result<(), Refused> {
    let Some(head) = head else { return Err(Refused::Unheaded { cell }) };
    admitting(cell, head, asked, false)
}

/// cladefs: whether a head of `cell` the bucket held once, `noted`, read for an earlier call, the
/// bucket out of reach now, admits `asked` (board 8445): as [`admit`] does, but that its writer
/// may have been taken since by a writer of a later epoch, its active configuration one after
/// `noted`'s, which a take installs and the head names before the bucket goes out of reach. So
/// appends, and drops through a log, go to a configuration no earlier than `noted`'s active one,
/// or of a later epoch than `noted`'s; what else it admits, a later head admits too. What
/// none of a head since would have asked harms none of its writers: a promise is no later than
/// `noted`'s epoch, and no take reads a configuration not its head's active one.
pub fn admit_noted(cell: CellId, noted: &Seen, asked: Asked) -> Result<(), Refused> {
    admitting(cell, noted, asked, true)
}

/// cladefs: [`admit`]'s, or, `noted`, [`admit_noted`]'s.
fn admitting(cell: CellId, head: &Seen, asked: Asked, noted: bool) -> Result<(), Refused> {
    let Seen { epoch, end, active } = *head;
    let appended = |key: Key| match (active, noted) {
        (Some(active), false) => {
            key == active || (key.number > active.number && key.epoch == epoch)
        }
        (None, false) => key.epoch == epoch,
        (Some(active), true) => key >= active || key.epoch > epoch,
        (None, true) => key.epoch >= epoch,
    };
    let admitted = match asked {
        Asked::Append(key) | Asked::Drop(key, Some(_)) => appended(key),
        Asked::Fence(key, at) => active == Some(key) && at <= epoch,
        Asked::Drop(key, None) => active.is_some_and(|active| key <= active),
    };
    if admitted {
        return Ok(());
    }
    let asked = match asked {
        Asked::Append(key) => format!("appends to configuration {key:?}"),
        Asked::Fence(key, at) => format!("a fence at epoch {at} of configuration {key:?}"),
        Asked::Drop(key, Some(through)) => {
            format!("a drop of configuration {key:?}'s logs through {through}")
        }
        Asked::Drop(key, None) => format!("a drop of the configurations before {key:?}"),
    };
    let active = active.map_or("no configuration".to_owned(), |key| format!("{key:?} active"));
    Err(Refused::Unasked { cell, asked, epoch, end, active })
}

impl Refused {
    /// The refusal of `log`, holding `found` where this build's format's magic is due.
    fn format(log: String, found: &[u8]) -> Self {
        Self::Format { log, found: found.escape_ascii().to_string() }
    }
}

/// What a member did not do: refused, or failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Refused(#[from] Refused),
    #[error(transparent)]
    Io(#[from] anyhow::Error),
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Self::Io(err.into())
    }
}

/// Bounded LRU of open replica log file handles across all cells (board 6902).
type FileCache<F> = bucket_cell::lru::LruMap<(CellId, Key, u64), Arc<F>>;

/// A host's replica directory.
pub struct Replicas<D: Dir> {
    dir: D,
    incarnation: u64,
    /// The most bytes a configuration's logs hold here: the writer's lag bound in bytes, which
    /// bounds what is not in the bucket yet (doc-21, Commit). Per configuration, as a take installs
    /// its own while the one before, holding the same commits, is still here until it activates.
    most: u64,
    /// The most logs a configuration has here: [`LOGS_MOST`], or fewer in a test.
    logs_most: usize,
    /// cladefs: the cells it holds files of, each as the directory's one listing, as it opened,
    /// found them, read as it is first asked of (board 8451); and, while a call of it is served, a
    /// cell holding nothing, which goes from here as its last call ends ([`Holding`]).
    cells: Mutex<HashMap<CellId, Arc<Mutex<Held>>>>,
    /// Bounded LRU of open log file handles across all cells (board 6902).
    files: Mutex<FileCache<D::File>>,
}

/// What the directory holds of a cell, under its lock: an append and a fence of it never at once,
/// so that a fence answered leaves no append of an older epoch in flight.
#[derive(Default)]
struct Held {
    /// cladefs: the names of its files the directory's listing found as it opened, until they are
    /// read into the rest: by the first call of the cell, under its lock alone (board 8451; each
    /// new cell's call listed the whole directory, under the lock of every cell).
    unread: Option<Vec<String>>,
    /// Its promise, as on the disk: the greatest of `promises`.
    promise: u64,
    /// The epochs of its promises' files on the disk: the promise's, and those before it that a
    /// crash, or a removal failing, left, which the next fence removes (board 5420).
    promises: BTreeSet<u64>,
    /// Its logs, by configuration and log.
    logs: BTreeMap<(Key, u64), Log>,
    /// cladefs: the newest of its head the peer service read for its calls, which admits them
    /// while the bucket is out of reach ([`Replicas::saw`], board 8445). Kept while it holds files.
    seen: Option<Seen>,
}

impl Held {
    /// The bytes the logs of configuration `key` hold.
    fn bytes(&self, key: Key) -> u64 {
        self.logs.range((key, 0)..=(key, u64::MAX)).map(|(_, log)| log.end).sum()
    }

    /// How many logs of configuration `key` it holds.
    fn logs_of(&self, key: Key) -> usize {
        self.logs.range((key, 0)..=(key, u64::MAX)).count()
    }

    /// The configurations it holds logs of, in order: each found past the one before's last log.
    fn configurations(&self) -> Vec<Key> {
        use std::ops::Bound::{Excluded, Unbounded};
        let mut keys = Vec::new();
        let mut next = self.logs.keys().next().map(|&(key, _)| key);
        while let Some(key) = next {
            keys.push(key);
            let after = self.logs.range((Excluded((key, u64::MAX)), Unbounded)).next();
            next = after.map(|(&(key, _), _)| key);
        }
        keys
    }

    /// Whether it holds nothing: no file, read or not.
    fn is_empty(&self) -> bool {
        self.unread.is_none() && self.promises.is_empty() && self.logs.is_empty()
    }
}

/// A log a member holds (board 6902): raw `ReplicaLog` bytes (even log) or `own_frame` bytes (odd
/// log) appended directly at their log offset without any outer envelope. On reopen after a crash,
/// `end` is recovered by scanning the self-delimited, hash-checked inner records (so zero-padding
/// from `O_DIRECT` block writes or torn tails are ignored). Open file handles live in
/// [`Replicas::files`] so descriptor use stays bounded by [`OPEN_FILES`].
struct Log {
    /// Its valid end in bytes.
    end: u64,
}

impl Log {
    /// Recovers the log in `file`, at `path`, for `(cell, log)` up to the last intact record or
    /// own-frame. A segment's log of another format than this build's is refused (board 8457),
    /// never read as holding nothing: its member acked what it holds. One whose header is torn, a
    /// crash's, holds nothing, as nothing of it was acked.
    fn read(cell: CellId, log: u64, file: &impl File, path: &str) -> Result<Self, Error> {
        let bytes = crate::dir::read_file(file).with_context(|| format!("reading {path}"))?;
        let end = match log & 1 {
            0 => {
                if let Some(found) = crate::wal::unsupported_log_magic(&bytes) {
                    return Err(Refused::format(path.to_owned(), &found).into());
                }
                let segment = crate::layout::SegmentId { cell: cell.id, number: log >> 1 };
                crate::wal::replica_valid_end(&bytes, segment)
            }
            _ => crate::replicate::own_frames_end(&bytes)
                .with_context(|| format!("reading {path}"))? as u64,
        };
        Ok(Self { end })
    }
}

impl<D: Dir> Replicas<D> {
    /// The replica directory `dir`: of the incarnation its file names, or a new one, made, if it
    /// holds nothing of a member's yet (a file system's own `lost+found`, or an incarnation's file
    /// a crash left half made, aside). A directory holding a member's files and no incarnation is
    /// refused: what its logs were promised can't be known. Each configuration's logs hold at most
    /// `most` bytes.
    pub fn open(dir: D, most: u64) -> Result<Self, Error> {
        Self::open_as(dir, most, || {
            loop {
                let number: u64 = rand::random();
                if number != 0 {
                    break number;
                }
            }
        })
    }

    /// Opens the replica directory `dir` as [`Replicas::open`] does, a new incarnation, if one is
    /// made, `new`'s: as the simulation draws it from its seed, so that a run replays (TASK-680).
    /// It must not be 0.
    pub fn open_as(dir: D, most: u64, new: impl FnOnce() -> u64) -> Result<Self, Error> {
        let names = dir.list("").context("listing the replica directory")?;
        let incarnation = if names.iter().any(|name| name == INCARNATION) {
            parse_incarnation(&read_text(&dir, INCARNATION)?)?
        } else {
            let held: Vec<&String> =
                names.iter().filter(|name| parse_name(name).is_some()).collect();
            if !held.is_empty() {
                return Err(anyhow!(
                    "the replica directory holds {} files of a member's ({}...) and no \
                     {INCARNATION}: refused, its promises unknown",
                    held.len(),
                    held[0]
                )
                .into());
            }
            let incarnation = new();
            assert_ne!(incarnation, 0, "a replica directory's incarnation is never 0");
            write_text(&dir, INCARNATION, &incarnation_text(incarnation))?;
            incarnation
        };
        // cladefs: each cell's files, as this one listing found them, read as the cell is first
        // asked of (board 8451).
        let mut found: HashMap<CellId, Vec<String>> = HashMap::new();
        for name in names {
            if let Some((cell, _)) = parse_name(&name) {
                found.entry(cell).or_default().push(name);
            }
        }
        let cells = (found.into_iter())
            .map(|(cell, names)| {
                let held = Held { unread: Some(names), ..Held::default() };
                (cell, Arc::new(Mutex::new(held)))
            })
            .collect();
        Ok(Self {
            dir,
            incarnation,
            most,
            logs_most: LOGS_MOST,
            cells: Mutex::new(cells),
            files: Mutex::new(bucket_cell::lru::LruMap::new()),
        })
    }

    fn cache_file(&self, id: (CellId, Key, u64), file: Arc<D::File>) {
        let mut files = self.files.lock().unwrap();
        files.insert(id, file);
        while files.len() > OPEN_FILES {
            files.pop_oldest();
        }
    }

    fn open_cached(&self, id: (CellId, Key, u64), path: &str) -> Result<Arc<D::File>, Error> {
        if let Some(file) = self.files.lock().unwrap().get(&id).cloned() {
            return Ok(file);
        }
        let opened =
            Arc::new(self.dir.open(path, false).with_context(|| format!("opening {path}"))?);
        self.cache_file(id, opened.clone());
        Ok(opened)
    }

    fn forget_file(&self, id: &(CellId, Key, u64)) {
        self.files.lock().unwrap().remove(id);
    }

    /// This, holding at most `logs` logs of a configuration, fewer than [`LOGS_MOST`], as a test
    /// reaches the bound without making as many.
    #[cfg(test)]
    fn holding_at_most(self, logs: usize) -> Self {
        Self { logs_most: logs, ..self }
    }

    #[cfg(test)]
    fn open_files(&self) -> usize {
        self.files.lock().unwrap().len()
    }

    /// The directory's incarnation: what a configuration naming this host as a member names.
    pub fn incarnation(&self) -> u64 {
        self.incarnation
    }

    /// Appends `bytes` at `offset` of the log `log` of `cell`'s configuration `key`, whose writer's
    /// epoch is `key.epoch`, to this member named in `incarnation`, and syncs it: the log's end,
    /// synced. Bytes before the end again are not written again, as a writer catching a member up
    /// sends what it may have had: the same, as a configuration's log is written once. An append
    /// past the end writes nothing, and answers the end, from which the writer sends again. A
    /// segment's log not begun with this build's `wal::LOG_MAGIC` is refused, written not, so that
    /// a writer of another format gets no ack of this member's (board 8457).
    pub fn append(
        &self,
        (cell, key, incarnation): (CellId, Key, u64),
        log: u64,
        offset: u64,
        bytes: &[u8],
    ) -> Result<u64, Error> {
        self.check(incarnation)?;
        let holding = self.holding(cell);
        let mut held = holding.lock()?;
        if key.epoch < held.promise {
            return Err(Refused::Fenced { promise: held.promise }.into());
        }
        // Nothing to append makes nothing: no file a writer's empty appends would make by the
        // thousand, past the bound, which counts bytes (Codex's review of TASK-537.3.2).
        if bytes.is_empty() {
            return Ok(held.logs.get(&(key, log)).map_or(0, |state| state.end));
        }
        let path = log_path(cell, key, log);
        // What it would append, checked before any file is made: an append past the end, all of
        // it held already, or past the bound, leaves nothing behind.
        let end = held.logs.get(&(key, log)).map_or(0, |state| state.end);
        if offset > end || offset + bytes.len() as u64 <= end {
            return Ok(end);
        }
        // A segment's log's first bytes, this append's of them: of a format this build reads, or
        // refused (board 8457, doc-28). Its own extents' frames say this directory's version, and
        // the writer's formats (which name `replica::FORMAT`) are checked where its stream of
        // appends enters (`cladefsd::peers`).
        if log & 1 == 0
            && let Some(given) = log_start_refused(offset, bytes)
        {
            let at = format!("log {log} of {cell} at {offset}");
            return Err(Refused::format(at, given).into());
        }
        let would = held.bytes(key) + (offset + bytes.len() as u64 - end);
        if would > self.most {
            return Err(Refused::Full { would, most: self.most }.into());
        }
        if !held.logs.contains_key(&(key, log)) && held.logs_of(key) >= self.logs_most {
            return Err(Refused::Logs { most: self.logs_most }.into());
        }
        // cladefs: a configuration new here, past those a cell's logs are of (board 8451).
        if held.logs_of(key) == 0 && held.configurations().len() >= CONFIGURATIONS_MOST {
            return Err(Refused::Configurations { cell, most: CONFIGURATIONS_MOST }.into());
        }
        // cladefs: what it adds, admitted by the disk governor before any file is made, held
        // until written (board 8451).
        let _pledge = self.admit(offset + bytes.len() as u64 - end)?;
        let mut created_file = None;
        if !held.logs.contains_key(&(key, log)) {
            // Its file made, it is held at once, empty, however its first append ends: a failure
            // after leaves a file the next append opens, and drops and sweeps remove.
            match self.dir.create(&path, false) {
                Ok(file) => {
                    let file = Arc::new(file);
                    self.cache_file((cell, key, log), file.clone());
                    created_file = Some(file);
                    held.logs.insert((key, log), Log { end: 0 });
                    self.dir.sync_dir("").context("syncing the replica directory")?;
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    // Made by an append that failed before it was held: what it holds, if any of
                    // it was written, read as a reopening would.
                    let file =
                        self.dir.open(&path, false).with_context(|| format!("opening {path}"))?;
                    let found = Log::read(cell, log, &file, &path)?;
                    let found_end = found.end;
                    held.logs.insert((key, log), found);
                    if found_end != 0 {
                        drop(file);
                        drop(held);
                        drop(holding);
                        return self.append((cell, key, incarnation), log, offset, bytes);
                    }
                    let file = Arc::new(file);
                    self.cache_file((cell, key, log), file.clone());
                    created_file = Some(file);
                }
                Err(err) => {
                    return Err(anyhow::Error::from(err).context(format!("making {path}")).into());
                }
            }
        }
        let state = held.logs.get_mut(&(key, log)).unwrap();
        let new = &bytes[(end - offset) as usize..];
        let file = match created_file {
            Some(file) => file,
            None => self.open_cached((cell, key, log), &path)?,
        };
        file.write_at(new, state.end).with_context(|| format!("writing {path}"))?;
        file.sync_cached().with_context(|| format!("syncing {path}"))?;
        state.end += new.len() as u64;
        Ok(state.end)
    }

    /// Fences `cell` at `epoch`, for this member named in `incarnation`: its promise raised to it,
    /// on the disk, before this returns; then the ends of the logs it holds of the configuration
    /// `key`. Refused if a later epoch fenced it already.
    pub fn fence(
        &self,
        (cell, key, incarnation): (CellId, Key, u64),
        epoch: u64,
    ) -> Result<Vec<(u64, u64)>, Error> {
        self.check(incarnation)?;
        let holding = self.holding(cell);
        let mut held = holding.lock()?;
        if epoch < held.promise {
            return Err(Refused::Fenced { promise: held.promise }.into());
        }
        if epoch > held.promise {
            // A file of each promise, made and synced before the ones before go: a crash
            // between leaves both, the greatest read. Nothing renamed over another (TASK-537.3.4's
            // review: `Dir::rename` renames to no file there). A fence at this epoch that failed
            // partway left its file, made again here (board 5420).
            let path = promise_path(cell, epoch);
            let made = match self.dir.create(&path, false) {
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    self.dir.open(&path, false)
                }
                made => made,
            };
            let file = made.with_context(|| format!("making {path}"))?;
            held.promises.insert(epoch);
            file.sync().with_context(|| format!("syncing {path}"))?;
            self.dir.sync_dir("").context("syncing the replica directory")?;
            // Made: the promise, from here on, whatever of the rest fails. The files before it
            // going fail no fence: they are left as a crash leaves them, the next fence's to remove.
            held.promise = epoch;
            if let Err(err) = self.remove_promises(cell, &mut held, ..epoch) {
                log::warn!("replicas: {cell}'s promises before {epoch}: {err:#}");
            }
        }
        Ok((held.logs.range((key, 0)..=(key, u64::MAX)))
            .map(|(&(_, log), state)| (log, state.end))
            .collect())
    }

    /// `len` bytes at `offset` of the log `log` of `cell`'s configuration `key`, as far as it holds
    /// them: fewer past its end, none of a log it does not hold.
    pub fn read(
        &self,
        (cell, key, incarnation): (CellId, Key, u64),
        log: u64,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, Error> {
        self.check(incarnation)?;
        let holding = self.holding(cell);
        let held = holding.lock()?;
        let Some(state) = held.logs.get(&(key, log)) else { return Ok(Vec::new()) };
        let available = state.end.saturating_sub(offset).min(len) as usize;
        if available == 0 {
            return Ok(Vec::new());
        }
        let path = log_path(cell, key, log);
        let file = self.open_cached((cell, key, log), &path)?;
        let mut bytes = vec![0; available];
        file.read_at(&mut bytes, offset).with_context(|| format!("reading {path}"))?;
        Ok(bytes)
    }

    /// Drops `cell`'s logs that no take will read: with `through`, those of the configuration
    /// `key` to it, which the bucket holds, the head naming the segments they wrote, and nothing of
    /// any other configuration; without, every configuration's before `key`, which the head names
    /// no more. A writer replacing its configuration drops the new one's logs as the bucket takes
    /// them while the head still names the one before, whose logs a take would read (Codex's
    /// review of TASK-537.3.5): only a writer whose head names `key` drops those before.
    pub fn drop_logs(
        &self,
        (cell, key, incarnation): (CellId, Key, u64),
        through: Option<u64>,
    ) -> Result<(), Error> {
        self.check(incarnation)?;
        let holding = self.holding(cell);
        let mut held = holding.lock()?;
        let gone: Vec<(Key, u64)> = (held.logs.keys())
            .filter(|&&(of, log)| match through {
                Some(last) => of == key && log <= last,
                None => of < key,
            })
            .copied()
            .collect();
        self.remove(cell, &mut held, &gone)
    }

    /// The cells the directory holds logs or a promise of, and the configurations it holds logs
    /// of: what [`Replicas::sweep`] looks at, against each cell's head.
    /// cladefs: as it keeps them in memory, its directory listed once, as it opened (board 8451).
    pub fn cells(&self) -> Result<Vec<(CellId, Vec<Key>)>, Error> {
        let kept: Vec<(CellId, Arc<Mutex<Held>>)> =
            self.cells.lock().unwrap().iter().map(|(&cell, held)| (cell, held.clone())).collect();
        let mut cells: BTreeMap<(u64, u64), Vec<Key>> = BTreeMap::new();
        for (cell, held) in kept {
            let held = held.lock().unwrap();
            if held.is_empty() {
                continue;
            }
            let keys = match &held.unread {
                Some(names) => {
                    let keys = names.iter().filter_map(|name| parse_name(name)?.1);
                    keys.collect::<BTreeSet<Key>>().into_iter().collect()
                }
                None => held.configurations(),
            };
            cells.insert((cell.tenant, cell.id), keys);
        }
        Ok(cells.into_iter().map(|((tenant, id), keys)| (CellId { tenant, id }, keys)).collect())
    }

    /// Drops what the directory holds of `cell` that its head names no more ([`Swept`]).
    pub fn sweep(&self, cell: CellId, swept: Swept) -> Result<(), Error> {
        let holding = self.holding(cell);
        let mut held = holding.lock()?;
        let gone: Vec<(Key, u64)> = match swept {
            // A replicated cell whose head names no configuration yet: its first may be being
            // installed, or active since the head was read. Nothing goes (the spec sweeps only
            // what the head names a configuration later than; TASK-537.3.2's review).
            Swept::Unconfigured => return Ok(()),
            Swept::Before(active) => {
                held.logs.keys().filter(|&&(of, _)| of < active).copied().collect()
            }
            Swept::Gone => held.logs.keys().copied().collect(),
        };
        self.remove(cell, &mut held, &gone)?;
        if swept == Swept::Gone {
            // Every promise, of `u64::MAX` too (board 8449: removing those before `u64::MAX` kept
            // its file, while the promise held went to 0, accepting appends it promised to refuse):
            // the promise held 0 only once all are gone, durably.
            self.remove_promises(cell, &mut held, ..)?;
            held.promise = 0;
        }
        Ok(())
    }

    /// Removes the files of `held`'s promises of epochs in `epochs`, durably: those a removal
    /// failed before (board 5420) too, each gone from `promises` once it went, and found gone as
    /// gone.
    fn remove_promises(
        &self,
        cell: CellId,
        held: &mut Held,
        epochs: impl std::ops::RangeBounds<u64>,
    ) -> Result<(), Error> {
        let before: Vec<u64> = held.promises.range(epochs).copied().collect();
        if before.is_empty() {
            return Ok(());
        }
        for promised in before {
            let path = promise_path(cell, promised);
            match self.dir.remove(&path) {
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                removed => removed.with_context(|| format!("removing {path}"))?,
            }
            held.promises.remove(&promised);
        }
        self.dir.sync_dir("").context("syncing the replica directory")?;
        Ok(())
    }

    fn remove(&self, cell: CellId, held: &mut Held, gone: &[(Key, u64)]) -> Result<(), Error> {
        for &(key, log) in gone {
            let path = log_path(cell, key, log);
            self.forget_file(&(cell, key, log));
            self.dir.remove(&path).with_context(|| format!("removing {path}"))?;
            held.logs.remove(&(key, log));
        }
        if !gone.is_empty() {
            self.dir.sync_dir("").context("syncing the replica directory")?;
        }
        Ok(())
    }

    fn check(&self, incarnation: u64) -> Result<(), Refused> {
        match incarnation == self.incarnation {
            true => Ok(()),
            false => Err(Refused::Incarnation { asked: incarnation, ours: self.incarnation }),
        }
    }

    /// What the directory holds of `cell`, held for a call of it ([`Holding`]): its entry, or one
    /// made, empty, so that calls of a cell new here are served one at a time too.
    fn holding(&self, cell: CellId) -> Holding<'_, D> {
        let held = self.cells.lock().unwrap().entry(cell).or_default().clone();
        Holding { replicas: self, cell, held }
    }

    /// Reads the files `held` names unread ([`Held::unread`]), `cell`'s, from the disk: kept unread
    /// if any fails to be, to be read again by the next call.
    fn read_held(&self, cell: CellId, held: &mut Held) -> Result<(), Error> {
        let Some(names) = held.unread.take() else { return Ok(()) };
        let mut read = Held::default();
        let reading = (|| {
            for name in &names {
                match parse_name(name) {
                    Some((_, None)) => {
                        let epoch = last_number(name).expect("a promise's name, read");
                        // A crash between a promise and the removal of the one before leaves both.
                        read.promise = read.promise.max(epoch);
                        read.promises.insert(epoch);
                    }
                    Some((_, Some(key))) => {
                        let log = last_number(name).expect("a log's name, read");
                        let file = (self.dir.open(name, false))
                            .with_context(|| format!("opening {name}"))?;
                        read.logs.insert((key, log), Log::read(cell, log, &file, name)?);
                    }
                    None => {}
                }
            }
            Ok::<_, Error>(())
        })();
        match reading {
            Ok(()) => {
                *held = read;
                Ok(())
            }
            Err(err) => {
                held.unread = Some(names);
                Err(err)
            }
        }
    }

    /// cladefs: drops the logs of `cell`'s configurations that a writer's stream of appends to
    /// `key`, its head's active configuration being `active`, leaves behind (board 8451): those
    /// before `key` of its epoch or an earlier one, but `active`. Its writer moves to a configuration
    /// only past those it gave up; and a configuration of an earlier epoch, not active, no writer
    /// appends to again, nor a take reads (a take reads the active one). One of a later epoch, a
    /// racing taker's, is kept. Called as the stream begins, `active` read from the bucket then,
    /// once its call is admitted ([`admit`]): never with an older head, whose active configuration
    /// may be one the head names no more, for one it names since.
    pub fn trim(
        &self,
        (cell, key, incarnation): (CellId, Key, u64),
        active: Option<Key>,
    ) -> Result<(), Error> {
        self.check(incarnation)?;
        let holding = self.holding(cell);
        let mut held = holding.lock()?;
        let left = |of: &Key| of.epoch <= key.epoch && *of < key && Some(*of) != active;
        let gone: Vec<(Key, u64)> =
            held.logs.range(..(key, 0)).map(|(&at, _)| at).filter(|(of, _)| left(of)).collect();
        if !gone.is_empty() {
            log::info!(
                "replicas: {cell}'s configurations before {key:?} left behind: {:?}",
                gone.iter().map(|&(of, _)| of).collect::<BTreeSet<_>>()
            );
        }
        self.remove(cell, &mut held, &gone)
    }

    /// cladefs: notes that `cell`'s head, read from the bucket, said `seen` (board 8445): kept while
    /// the cell holds files here, the newest of what was read, to admit its calls by while the bucket
    /// is out of reach ([`Replicas::seen`]). Each of a head's fields only grows.
    pub fn saw(&self, cell: CellId, seen: Seen) -> Result<(), Error> {
        let holding = self.holding(cell);
        let mut held = holding.lock()?;
        let newest =
            |was: Seen| (was.epoch, was.end, was.active) < (seen.epoch, seen.end, seen.active);
        if held.seen.is_none_or(newest) {
            held.seen = Some(seen);
        }
        Ok(())
    }

    /// cladefs: the newest of `cell`'s head the directory noted ([`Replicas::saw`]), if it holds
    /// files of the cell: a head the bucket held once, by which what is admitted harms no writer or
    /// take of the head since (board 8445): a promise no later than its epoch, no later than any
    /// head's since; appends and drops of a configuration superseded since, which no take reads.
    pub fn seen(&self, cell: CellId) -> Result<Option<Seen>, Error> {
        let holding = self.holding(cell);
        let held = holding.lock()?;
        Ok(held.seen)
    }

    /// cladefs: admits `bytes` more appended, if the directory's disk has room for them, as the
    /// disk governor says (board 8451): what the returned pledge holds until it goes, the bytes
    /// written, counted in the directory's footprint as they are.
    fn admit(&self, bytes: u64) -> Result<Option<crate::footprint::Pledge>, Refused> {
        let Some(footprint) = self.dir.footprint() else { return Ok(None) };
        let admitted = footprint.admit(bytes, 0);
        admitted.map(Some).map_err(|full| Refused::Disk { bytes, full: full.to_string() })
    }

    /// cladefs: how many cells it keeps in memory, read or not.
    #[cfg(test)]
    fn cells_kept(&self) -> usize {
        self.cells.lock().unwrap().len()
    }
}

/// cladefs: what the directory holds of a cell, held for a call of it: its entry dropped as the
/// last call ends, if it holds nothing then (board 8451), so that calls of cells holding nothing,
/// a peer's by the hundred thousand, leave nothing in memory.
struct Holding<'a, D: Dir> {
    replicas: &'a Replicas<D>,
    cell: CellId,
    held: Arc<Mutex<Held>>,
}

impl<D: Dir> Holding<'_, D> {
    /// What the directory holds of the cell, under its lock, its files read if they were not.
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Held>, Error> {
        let mut held = self.held.lock().unwrap();
        self.replicas.read_held(self.cell, &mut held)?;
        Ok(held)
    }
}

impl<D: Dir> Drop for Holding<'_, D> {
    fn drop(&mut self) {
        let mut cells = self.replicas.cells.lock().unwrap();
        // Held by the map and this alone: no other call holds it, or waits for it; none takes it
        // from the map while the map is locked.
        let alone = Arc::strong_count(&self.held) == 2
            && cells.get(&self.cell).is_some_and(|held| Arc::ptr_eq(held, &self.held));
        if alone && self.held.lock().unwrap().is_empty() {
            cells.remove(&self.cell);
        }
    }
}

/// What a sweep found of a cell in its head ([`Replicas::sweep`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Swept {
    /// Its head names this configuration: those before it go.
    Before(Key),
    /// Its head names no configuration: nothing goes, as one may be installed and active by now.
    Unconfigured,
    /// It is gone, or deleted, for good: all of it goes, its promise too.
    Gone,
}

/// The bytes of `bytes`, appended at `offset` of a segment's log, that fall in its first eight, if
/// they say a format this build doesn't read: `FXLG` and a version of the cell directory's it
/// reads (`wal::LOG_MAGIC`). A version split across appends is held to this build's own, the most
/// one append can tell of it.
fn log_start_refused(offset: u64, bytes: &[u8]) -> Option<&[u8]> {
    let magic = crate::wal::LOG_MAGIC;
    // An offset past what this host addresses is past the magic's bytes: none to check.
    let at = usize::try_from(offset).ok().filter(|at| *at < magic.len())?;
    let given = &bytes[..(magic.len() - at).min(bytes.len())];
    let refused = match (at, given.len()) {
        // The whole of it: its kind and a version read.
        (0, 8) => !crate::wal::log_magic_read(given),
        _ => given != &magic[at..at + given.len()],
    };
    refused.then_some(given)
}

fn promise_path(cell: CellId, epoch: u64) -> String {
    format!("{}.{}.promise.{}", Padded(cell.tenant), Padded(cell.id), Padded(epoch))
}

fn log_path(cell: CellId, key: Key, log: u64) -> String {
    let (tenant, id) = (Padded(cell.tenant), Padded(cell.id));
    format!("{tenant}.{id}.{}.{}.{}", Padded(key.number), Padded(key.epoch), Padded(log))
}

/// The cell a file of the directory is of, and the configuration, if it is a log's.
fn parse_name(name: &str) -> Option<(CellId, Option<Key>)> {
    let parts: Vec<&str> = name.split('.').collect();
    // Not a number in base 32, padded: not a file of a cell's.
    let number = |at: usize| parts.get(at).and_then(|part| base32_id::parse_padded(part));
    let cell = CellId { tenant: number(0)?, id: number(1)? };
    match parts.len() {
        4 if parts[2] == "promise" => {
            number(3)?;
            Some((cell, None))
        }
        5 => {
            number(4)?;
            Some((cell, Some(Key { number: number(2)?, epoch: number(3)? })))
        }
        _ => None,
    }
}

/// The number the name of a file of a cell's ends with, [`parse_name`] read: a promise's epoch, a
/// log's number.
fn last_number(name: &str) -> Option<u64> {
    name.rsplit('.').next().and_then(base32_id::parse_padded)
}

fn read_text<D: Dir>(dir: &D, path: &str) -> Result<String, Error> {
    let file = dir.open(path, false).with_context(|| format!("opening {path}"))?;
    let mut bytes = vec![0; file.size().with_context(|| format!("sizing {path}"))? as usize];
    file.read_at(&mut bytes, 0).with_context(|| format!("reading {path}"))?;
    Ok(String::from_utf8(bytes).with_context(|| format!("{path}: not text"))?)
}

/// Writes `text` as the file at `path`, at once: written beside it, synced, renamed over it, and
/// the directory synced, so that a crash leaves the old text or the new, whole.
fn write_text<D: Dir>(dir: &D, path: &str, text: &str) -> Result<(), Error> {
    let new = format!("{path}.new");
    match dir.remove(&new) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => {
            return Err(anyhow::Error::from(err).context(format!("removing {new}")).into());
        }
        _ => {}
    }
    let file = dir.create(&new, false).with_context(|| format!("making {new}"))?;
    file.write_at(text.as_bytes(), 0).with_context(|| format!("writing {new}"))?;
    file.sync().with_context(|| format!("syncing {new}"))?;
    dir.rename(&new, path).with_context(|| format!("renaming {new} to {path}"))?;
    dir.sync_dir("").context("syncing the replica directory")?;
    Ok(())
}

#[cfg(test)]
mod tests;
