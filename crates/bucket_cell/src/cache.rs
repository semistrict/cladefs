// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A host's copies of objects of its bucket (doc-7, Reading: the NVMe cache; doc-14): of the
//! parts it fetched, and of whole objects it wrote, adopted once the bucket has them
//! ([`Cache::adopt`]). Each object's is a sparse file of its size, at its key, holding its tail,
//! which says where its parts are stored, and those of its parts kept, as stored, compressed or
//! not, at their offsets, the rest holes. Those read least recently are evicted, by punching holes,
//! an object's tail with its last part, so that they take no more than its limit: its budget, if it
//! has one, and what the disk governor leaves it of its filesystem ([`Cache::set_limit`], backlog
//! doc-22), which it gives back as the cells, or the host, need it. They are copies of what is in
//! the bucket, so none is data the bucket lacks. What it keeps, it counts on its filesystem's disk
//! ([`HostDisk::cache`]), its fills in flight too.
//!
//! The files are all the cache keeps: one opened finds what an earlier one kept in them
//! ([`Cache::open`]), so that a host's restart refetches nothing it had. Its index is rebuilt from
//! their tails alone, each checked against its CRCs, a file whose tail fails going whole; an
//! object's parts kept are those its file holds data for, not holes, none of them read: a restart
//! with hundreds of GiB cached reads their tails, not the parts behind them (board 14702). Some it
//! counts so are no parts: one a crash tore, one shorter than a block, whose punch freed nothing,
//! and, on APFS, which fills with zeros the holes near what is written, those never written. And
//! what it gives, it checks: a tail against its CRCs, a part against the one its tail gives,
//! dropping what fails. So whatever a crash left of a write, torn or lost, and whatever an
//! eviction left of a part, reads as not kept, never as other bytes, and is fetched again by its
//! reader. One cache at a time has a directory.

use crate::host_disk::HostDisk;
use crate::object;
use extent_frame::{Frame, Parts, TRAILER};
use host_fs::File;
use host_fs::FileExt as _;
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// The number an object's tail is kept by, among its parts, which count from 0: its footer, table
/// and trailer, after its parts, which a fetcher of another of the host's cells reads here rather
/// than from the bucket (TASK-92.22), and without which none of its parts can be read.
const TAIL: u64 = u64::MAX;

/// Runtime health state of a JBOD part-cache shard, modelled on RustFS's
/// `RuntimeDriveHealthState` and JuiceFS's `dcState`: a failing disk stops receiving new bucket
/// fetches and is isolated while surviving disks keep serving, and background probes restore it
/// once I/O succeeds again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ShardHealth {
    /// Healthy and accepting reads, fills, and adoptions.
    Online = 0,
    /// Saw repeated I/O errors recently; excluded from new bucket fills while probed.
    Suspect = 1,
    /// Persistently failing I/O; skipped on reads and fills until probes succeed.
    Offline = 2,
    /// Probes succeeded after failure; verifying stability before returning to `Online`.
    Returning = 3,
}

impl ShardHealth {
    /// Metric value for `cladefsd_part_cache_shard_state`.
    pub const fn as_u64(self) -> u64 {
        self as u64
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Suspect,
            2 => Self::Offline,
            3 => Self::Returning,
            _ => Self::Online,
        }
    }
}

/// Errors within [`HEALTH_WINDOW`] that move an `Online` shard to `Suspect`.
const SUSPECT_ERRORS: u32 = 3;
/// Consecutive errors while `Suspect` (or time spent `Suspect`) that move a shard to `Offline`.
const OFFLINE_ERRORS: u32 = 10;
const OFFLINE_AFTER: Duration = Duration::from_secs(30 * 60);
/// Consecutive probe successes that promote `Offline` -> `Returning` -> `Online`.
const RETURNING_SUCCESSES: u32 = 3;
const HEALTH_WINDOW: Duration = Duration::from_secs(60);
/// Minimum interval between health probes on the same unhealthy shard so probes neither false-recover
/// in milliseconds nor burn through `OFFLINE_ERRORS` before `OFFLINE_AFTER`.
const PROBE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug)]
struct HealthInner {
    window_start: Instant,
    window_errors: u32,
    suspect_since: Option<Instant>,
    consecutive_errors: u32,
    probe_successes: u32,
    last_probe: Option<Instant>,
}

#[derive(Debug)]
struct ShardHealthState {
    state: AtomicU8,
    probe_in_flight: AtomicBool,
    inner: Mutex<HealthInner>,
}

impl ShardHealthState {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(ShardHealth::Online as u8),
            probe_in_flight: AtomicBool::new(false),
            inner: Mutex::new(HealthInner {
                window_start: Instant::now(),
                window_errors: 0,
                suspect_since: None,
                consecutive_errors: 0,
                probe_successes: 0,
                last_probe: None,
            }),
        }
    }

    fn get(&self) -> ShardHealth {
        ShardHealth::from_u8(self.state.load(Ordering::Acquire))
    }

    fn is_online(&self) -> bool {
        self.get() == ShardHealth::Online
    }

    fn is_offline(&self) -> bool {
        self.get() == ShardHealth::Offline
    }

    /// Reserves a rate-limited health probe slot for this shard if it is unhealthy and not already
    /// being probed.
    fn try_begin_probe(&self, now: Instant) -> bool {
        if self.is_online() {
            return false;
        }
        if self.probe_in_flight.swap(true, Ordering::AcqRel) {
            return false;
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.last_probe.is_some_and(|last| now.duration_since(last) < PROBE_INTERVAL) {
            self.probe_in_flight.store(false, Ordering::Release);
            return false;
        }
        inner.last_probe = Some(now);
        true
    }

    fn finish_probe(&self) {
        self.probe_in_flight.store(false, Ordering::Release);
    }

    fn on_io_success(&self) {
        if self.is_online() {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        inner.consecutive_errors = 0;
        let current = self.get();
        match current {
            ShardHealth::Online => {}
            ShardHealth::Suspect => {
                inner.probe_successes += 1;
                if inner.probe_successes >= RETURNING_SUCCESSES {
                    inner.probe_successes = 0;
                    inner.suspect_since = None;
                    inner.window_errors = 0;
                    self.state.store(ShardHealth::Online as u8, Ordering::Release);
                }
            }
            ShardHealth::Offline => {
                inner.probe_successes = 1;
                self.state.store(ShardHealth::Returning as u8, Ordering::Release);
            }
            ShardHealth::Returning => {
                inner.probe_successes += 1;
                if inner.probe_successes >= RETURNING_SUCCESSES {
                    inner.probe_successes = 0;
                    inner.suspect_since = None;
                    inner.window_errors = 0;
                    self.state.store(ShardHealth::Online as u8, Ordering::Release);
                }
            }
        }
    }

    fn on_io_error(&self, dir: &Path, err: &io::Error) {
        if !is_disk_io_error(err) {
            return;
        }
        self.record_error(dir, err);
    }

    /// Records any failure of an explicit health probe (including `InvalidData` read-back mismatch).
    fn on_probe_error(&self, dir: &Path, err: &io::Error) {
        self.record_error(dir, err);
    }

    fn record_error(&self, dir: &Path, err: &io::Error) {
        let mut inner = self.inner.lock().unwrap();
        let now = Instant::now();
        if now.duration_since(inner.window_start) >= HEALTH_WINDOW {
            inner.window_start = now;
            inner.window_errors = 0;
        }
        inner.window_errors = inner.window_errors.saturating_add(1);
        inner.consecutive_errors = inner.consecutive_errors.saturating_add(1);
        inner.probe_successes = 0;
        let current = self.get();
        let next = match current {
            ShardHealth::Online if inner.window_errors >= SUSPECT_ERRORS => {
                inner.suspect_since = Some(now);
                ShardHealth::Suspect
            }
            ShardHealth::Suspect
                if inner.consecutive_errors >= OFFLINE_ERRORS
                    || inner
                        .suspect_since
                        .is_some_and(|s| now.duration_since(s) >= OFFLINE_AFTER) =>
            {
                ShardHealth::Offline
            }
            ShardHealth::Returning => ShardHealth::Offline,
            other => other,
        };
        if next != current {
            self.state.store(next as u8, Ordering::Release);
            log::error!(
                "cladefs: part cache shard {} health {current:?} -> {next:?} after I/O error: {err}",
                dir.display()
            );
        }
    }
}

/// Whether `err` indicates a failing storage device (rather than missing/corrupt entry).
fn is_disk_io_error(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(libc::EIO | libc::ENODEV | libc::ENXIO | libc::EROFS | libc::ESTALE)
    ) || matches!(err.kind(), io::ErrorKind::TimedOut | io::ErrorKind::BrokenPipe)
}

/// The cache's lock is held only to change what it keeps, not across its files' I/O: a part's
/// write, or its eviction, is done outside it, the part busy meanwhile, and an object's file
/// removed, or replaced by one adopted, so too, the object's key removing; neither is kept or read
/// until done (TASK-92.21).
///
/// Supports JBOD across one or more independent [`Shard`]s (each on its own directory /
/// [`HostDisk`]): bucket fetches distribute across healthy shards by capacity-weighted rendezvous
/// hashing, while adoptions (`adopt`) route directly to the shard on the source file's filesystem
/// so `rename(2)` never crosses filesystems (`EXDEV`), and lookups check shard indices in memory.
#[derive(Debug)]
pub struct Cache {
    shards: Vec<Shard>,
    /// Total budget across all shards (`u64::MAX` when governed by disk headroom alone).
    total_budget: u64,
    /// Round-robin cursor for selecting a healthy shard's `versions` directory.
    versions_rr: AtomicUsize,
}

/// One disk shard of a [`Cache`]: its own directory, [`HostDisk`], sparse files, in-memory LRU
/// index, persistent disk ID, and runtime health state.
#[derive(Debug)]
struct Shard {
    files: Files,
    /// Persistent random non-zero ID stored in `<root>/.disk-id`, keeping rendezvous hashing
    /// stable even if configured paths are reordered.
    disk_id: u64,
    /// Last sampled filesystem size in bytes, used as placement weight for heterogeneous JBOD.
    disk_size: AtomicU64,
    /// The most it keeps, its budget, if it has one.
    budget: AtomicU64,
    /// The most the disk governor leaves it ([`Shard::set_limit`]): `u64::MAX` until it says.
    governed: AtomicU64,
    kept: Mutex<Index>,
    /// cladefs: beside its files, on their filesystem, the cells at versions its host opens of its
    /// own ([`Cache::versions`], board 6589).
    versions: PathBuf,
    /// Runtime disk health state for JBOD failover.
    health: ShardHealthState,
    /// Keys dropped from the in-memory index while this shard was `Offline` (so healthy-disk
    /// adoptions never block on an offline disk's unlink); drained once the shard recovers.
    pending_forgets: Mutex<Vec<String>>,
    /// The directory, locked, so that no other cache opens it while this one has it.
    _lock: host_fs::Flock,
}

/// The cache's index: the objects it keeps parts of, and those parts by when they were last read.
#[derive(Debug)]
struct Index {
    objects: HashMap<String, Object>,
    /// The parts kept, by when they were last read.
    by_use: BTreeMap<u64, (String, u64)>,
    /// The objects whose files are being removed, or replaced by one adopted.
    removing: HashSet<String>,
    clock: u64,
    /// The bytes of the parts kept, and being written: as its disk counts them too
    /// ([`Index::take`], [`Index::give`]).
    bytes: u64,
    /// cladefs: its filesystem's disk, which counts what it keeps (doc-22).
    disk: Arc<HostDisk>,
}

/// An object the cache keeps some of, or is writing: in the cache's index while it keeps or
/// writes any of it. Its parts are kept only with its tail, and it is read only by it. It is
/// readable, its tail being written, or forgotten.
#[derive(Debug)]
struct Object {
    /// Where its parts are stored, as its tail says.
    parts: Parts,
    /// The parts kept, its tail among them once written ([`TAIL`]), and when each was last read,
    /// its tail after its parts: their places in `by_use`.
    kept: HashMap<u64, u64>,
    /// The parts being written, or punched.
    busy: HashMap<u64, Busy>,
    /// Dropped while parts of it were busy: nothing more of it is kept, and its file goes, with
    /// what they wrote, once they are done.
    forgotten: bool,
}

/// What is being done with a part busy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Busy {
    Writing,
    Punching,
}

impl Object {
    /// An object of parts stored as `parts` says, whose part `part` is being written.
    fn writing(parts: Parts, part: u64) -> Self {
        let busy = HashMap::from([(part, Busy::Writing)]);
        Self { parts, kept: HashMap::new(), busy, forgotten: false }
    }

    /// Where part `part`, or its tail, is in its file.
    fn place(&self, part: u64) -> Range<u64> {
        match part {
            TAIL => self.parts.frame.tail(),
            part => self.parts.stored[part as usize].range(),
        }
    }

    fn len(&self, part: u64) -> u64 {
        let place = self.place(part);
        place.end - place.start
    }

    /// Whether its tail is kept: whether it can be read.
    fn readable(&self) -> bool {
        self.kept.contains_key(&TAIL)
    }
}

/// What evicting a part leaves to do with its object's file, outside the cache's lock.
#[derive(Debug)]
enum Evicted {
    /// Punch the part's hole: where it is in the file.
    Punch { key: String, part: u64, place: Range<u64> },
    /// Remove the file of the object, whose last part it was.
    Remove(String),
}

impl Index {
    fn new(disk: Arc<HostDisk>) -> Self {
        Self {
            objects: HashMap::new(),
            by_use: BTreeMap::new(),
            removing: HashSet::new(),
            clock: 0,
            bytes: 0,
            disk,
        }
    }

    /// Counts `len` bytes more kept, or being written, here and on its disk.
    fn take(&mut self, len: u64) {
        self.bytes += len;
        self.disk.cache.add(len);
    }

    /// Counts `len` bytes fewer.
    fn give(&mut self, len: u64) {
        self.bytes -= len;
        self.disk.cache.sub(len);
    }

    /// Counts part `part` of the object at `key` read now, and its tail after it.
    fn touch(&mut self, key: &str, part: u64) {
        let parts: &[u64] = if part == TAIL { &[TAIL] } else { &[part, TAIL] };
        for &part in parts {
            let Some(used) =
                self.objects.get_mut(key).and_then(|object| object.kept.get_mut(&part))
            else {
                continue;
            };
            self.clock += 1;
            self.by_use.remove(used);
            *used = self.clock;
            self.by_use.insert(self.clock, (key.to_owned(), part));
        }
    }

    /// Keeps part `part` of the object at `key`, read now.
    fn keep(&mut self, key: &str, part: u64) {
        self.clock += 1;
        let object = self.objects.get_mut(key).expect("an object kept");
        object.kept.insert(part, self.clock);
        self.by_use.insert(self.clock, (key.to_owned(), part));
    }

    /// Keeps the object at `key` that `found` is, its parts read now, its tail after them.
    fn insert(&mut self, key: &str, found: Found) {
        let object = Object {
            parts: found.parts,
            kept: HashMap::new(),
            busy: HashMap::new(),
            forgotten: false,
        };
        self.objects.insert(key.to_owned(), object);
        for part in found.kept.into_iter().chain([TAIL]) {
            self.keep(key, part);
        }
    }

    /// Evicts what was read least recently, but tails whose objects keep or write other parts,
    /// until `len` bytes more fit in `limit`; or nothing, if they can't be made to. Returns whether
    /// they fit, and what the evictions leave to do. Chosen first, evicted only if enough: a fill
    /// that can't fit evicted all it could, emptying the cache for nothing (TASK-443.8).
    fn make_room(&mut self, limit: u64, len: u64) -> (bool, Vec<Evicted>) {
        let (mut chosen, mut freed) = (Vec::new(), 0);
        // What each object keeps, as the evictions chosen leave it.
        let mut left: HashMap<&str, usize> = HashMap::new();
        for (key, part) in self.by_use.values() {
            if self.bytes + len <= limit.saturating_add(freed) {
                break;
            }
            let object = &self.objects[key];
            let kept = left.entry(key).or_insert(object.kept.len());
            // A tail goes with its object's last part, and not while one is being written.
            let writing = object.busy.values().any(|busy| *busy == Busy::Writing);
            if *part == TAIL && (*kept > 1 || writing) {
                continue;
            }
            *kept -= 1;
            freed += object.len(*part);
            chosen.push((key.clone(), *part));
        }
        if self.bytes + len > limit.saturating_add(freed) {
            return (false, Vec::new());
        }
        let evicted = chosen.into_iter().flat_map(|(key, part)| self.evict(&key, part)).collect();
        (true, evicted)
    }

    /// Drops part `part` of the object at `key`, kept: its tail drops the object. Returns what that
    /// leaves to do with its file, the part, or its object, busy until done.
    fn evict(&mut self, key: &str, part: u64) -> Option<Evicted> {
        if part == TAIL {
            return self.forget(key);
        }
        let object = self.objects.get_mut(key).expect("an object kept");
        let used = object.kept.remove(&part).expect("a part kept");
        self.by_use.remove(&used);
        let place = object.place(part);
        object.busy.insert(part, Busy::Punching);
        self.give(place.end - place.start);
        Some(Evicted::Punch { key: key.to_owned(), part, place })
    }

    /// Drops what is kept of the object at `key`. Returns its file to remove, if none of it is
    /// busy: else the last part of it that is removes it once done.
    fn forget(&mut self, key: &str) -> Option<Evicted> {
        let object = self.objects.get_mut(key)?;
        object.forgotten = true;
        let mut gone = 0;
        for (part, used) in std::mem::take(&mut object.kept) {
            self.by_use.remove(&used);
            gone += object.len(part);
        }
        self.give(gone);
        self.release(key)
    }

    /// Part `part` of the object at `key`, which was being written, is written, if `written`: kept
    /// then, unless its object was forgotten meanwhile. Returns the object's file to remove, if
    /// that leaves it nothing.
    fn written(&mut self, key: &str, part: u64, written: bool) -> Option<Evicted> {
        let object = self.objects.get_mut(key).expect("an object its busy parts keep");
        let len = object.len(part);
        self.disk.cache_writing.sub(len);
        if written && !object.forgotten {
            object.busy.remove(&part);
            self.keep(key, part);
            self.touch(key, TAIL);
            return None;
        }
        self.give(len);
        self.freed(key, part)
    }

    /// Part `part` of the object at `key`, not kept, is busy no more. Returns the object's file to
    /// remove, if that leaves it nothing.
    fn freed(&mut self, key: &str, part: u64) -> Option<Evicted> {
        (self.objects.get_mut(key).expect("an object its busy parts keep").busy).remove(&part);
        self.release(key)
    }

    /// The file of the object at `key` to remove, if it keeps nothing and nothing of it is busy.
    fn release(&mut self, key: &str) -> Option<Evicted> {
        let object = &self.objects[key];
        if !object.kept.is_empty() || !object.busy.is_empty() {
            return None;
        }
        self.objects.remove(key);
        self.removing.insert(key.to_owned());
        Some(Evicted::Remove(key.to_owned()))
    }
}

/// Where the cache keeps its files under the directory it is given.
const FILES: &str = "cladefs-parts";

/// cladefs: where, under the directory it is given, the cells at versions its host opens of its
/// own are ([`Cache::versions`]).
const VERSIONS: &str = "cladefs-versions";

/// The blocks of the filesystems the cache's files are on, as the parts in them are aligned to:
/// what punching a hole frees whole, and all macOS punches ([`punch`]).
#[cfg(target_os = "macos")]
const BLOCK: u64 = extent_frame::ALIGN;

/// Persistent disk-ID file stored at the root of each part-cache shard (beside `cladefs-parts/`
/// and `cladefs-versions/`), keeping rendezvous hashing stable across path reorderings.
const DISK_ID: &str = ".disk-id";

/// Probe file written/read/removed when probing a `Suspect`/`Offline`/`Returning` shard.
const PROBE_FILE: &str = ".health-probe";

/// Reads or creates a persistent non-zero random disk ID in `<root>/.disk-id`.
///
/// The part cache is rebuildable (`doc-28`), so a missing, torn/non-UTF-8, or invalid `.disk-id`
/// regenerates a fresh ID instead of failing daemon boot (board 12102), while real I/O errors
/// (such as `EIO` or `EACCES`) are still propagated.
fn load_or_create_disk_id(root: &Path) -> io::Result<u64> {
    let path = root.join(DISK_ID);
    match host_fs::read(&path) {
        Ok(bytes) => {
            if let Ok(text) = std::str::from_utf8(&bytes)
                && let Ok(id) = text.trim().parse::<u64>()
                && id != 0
            {
                return Ok(id);
            }
            log::warn!(
                "cladefs: corrupt part-cache disk id at {} ({} bytes); regenerating",
                path.display(),
                bytes.len()
            );
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    // Outside the simulation: host-local disk cache identity generated once on a real disk.
    let id = rand::random::<u64>() | 1;
    let tmp = root.join(".disk-id.new");
    let content = format!("{id}\n");
    {
        let file =
            host_fs::OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?;
        file.write_all_at(content.as_bytes(), 0)?;
        file.sync_all()?;
    }
    host_fs::rename(&tmp, &path)?;
    host_fs::sync_dir(root)?;
    Ok(id)
}

/// Computes a weighted rendezvous score in `(0, +inf)` for `(disk_id, key)` with `weight >= 1`.
/// Using `-weight / ln(u)` for uniform `u in (0, 1)` picks shard `k` with exact probability
/// `weight_k / sum(weight)` and moves the minimal fraction of keys when shards are added/removed.
fn weighted_score(disk_id: u64, weight: u64, key: &str) -> f64 {
    let mut hasher = Sha256::new();
    hasher.update(disk_id.to_le_bytes());
    hasher.update((key.len() as u64).to_le_bytes());
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();
    let h = u64::from_le_bytes(digest[..8].try_into().expect("sha256 has 32 bytes"));
    let u = ((h >> 11) as f64 + 0.5) / ((1u64 << 53) as f64);
    -(weight.max(1) as f64) / u.ln()
}

/// Apportions `total` proportionally to `weights`, assigning any remainder to the largest shards
/// so the sum equals `total` (unless `total == u64::MAX`, in which case every shard gets
/// `u64::MAX`).
fn apportion(total: u64, weights: &[u64]) -> Vec<u64> {
    let n = weights.len();
    if n == 0 {
        return Vec::new();
    }
    if total == u64::MAX {
        return vec![u64::MAX; n];
    }
    let sum: u128 = weights.iter().map(|&w| u128::from(w.max(1))).sum();
    let mut shares: Vec<u64> = weights
        .iter()
        .map(|&w| ((u128::from(total) * u128::from(w.max(1))) / sum) as u64)
        .collect();
    let mut rem = total.saturating_sub(shares.iter().copied().sum::<u64>());
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(weights[i]));
    for i in order {
        if rem == 0 {
            break;
        }
        shares[i] = shares[i].saturating_add(1);
        rem -= 1;
    }
    shares
}

impl Shard {
    fn open_on(host: &crate::Host, dir: &Path, budget: u64) -> io::Result<Self> {
        let versions = dir.join(VERSIONS);
        let parts_dir = dir.join(FILES);
        host_fs::create_dir_all(&parts_dir)?;
        let lock = lock(&parts_dir)?;
        let disk_id = load_or_create_disk_id(dir)?;
        // What an earlier cache's host left of its cells at versions, which it alone had: they
        // hold nothing of their own.
        match host_fs::remove_dir_all(&versions) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
            _ => host_fs::create_dir(&versions)?,
        }
        let files = Files::new_on(host, parts_dir)?;
        let sample_size = files.disk.sample().map_or(1, |s| s.size.max(1));
        let mut found = Vec::new();
        files.walk(Path::new(""), &mut found)?;
        // By when each was written last: it was read then, if not since.
        found.sort_by_key(|(_, found)| found.modified);
        let mut kept = Index::new(files.disk.clone());
        for (key, found) in found {
            kept.take(found.bytes());
            kept.insert(&key, found);
        }
        let shard = Self {
            files,
            disk_id,
            disk_size: AtomicU64::new(sample_size),
            budget: AtomicU64::new(budget),
            governed: AtomicU64::new(u64::MAX),
            kept: Mutex::new(kept),
            versions,
            health: ShardHealthState::new(),
            pending_forgets: Mutex::new(Vec::new()),
            _lock: lock,
        };
        shard.set_limit(u64::MAX)?;
        Ok(shard)
    }

    fn set_budget(&self, budget: u64) -> io::Result<u64> {
        self.budget.store(budget, Ordering::Relaxed);
        let (_, evicted) = self.kept.lock().unwrap().make_room(self.capacity(), 0);
        self.evict(evicted)?;
        Ok(self.bytes())
    }

    fn set_limit(&self, bytes: u64) -> io::Result<u64> {
        self.governed.store(bytes, Ordering::Relaxed);
        let (_, evicted) = self.kept.lock().unwrap().make_room(self.capacity(), 0);
        self.evict(evicted)?;
        Ok(self.bytes())
    }

    fn weight(&self) -> u64 {
        let allowance = self.files.disk.allowance();
        if allowance < u64::MAX {
            allowance.max(1)
        } else {
            self.disk_size.load(Ordering::Relaxed).max(1)
        }
    }

    fn record_io<T>(&self, res: io::Result<T>) -> io::Result<T> {
        match &res {
            Ok(_) => self.health.on_io_success(),
            Err(err) => self.health.on_io_error(&self.files.dir, err),
        }
        res
    }

    fn probe(&self) {
        if self.health.is_online() {
            return;
        }
        let Some(root) = self.files.dir.parent() else { return };
        let path = root.join(PROBE_FILE);
        let res = (|| -> io::Result<()> {
            let block = crate::direct::DIRECT_BLOCK as usize;
            let mut wbuf = crate::direct::Buffer::new(block);
            wbuf.bytes_mut().fill(0x5a);
            {
                let file = host_fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&path)?;
                crate::direct::bypass(&file, &path)?;
                file.write_all_at(wbuf.bytes(), 0)?;
                file.sync_all()?;
            }
            let mut rbuf = crate::direct::Buffer::new(block);
            {
                let file = host_fs::File::open(&path)?;
                crate::direct::bypass(&file, &path)?;
                file.read_exact_at(rbuf.bytes_mut(), 0)?;
            }
            // Best-effort cleanup of the temporary probe file; probe outcome is decided by the
            // direct-I/O read-back check and logged on error below.
            if let Err(err) = host_fs::remove_file(&path) {
                log::warn!("cladefs: removing probe file {}: {err}", path.display());
            }
            if rbuf.bytes() != wbuf.bytes() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "probe mismatch"));
            }
            Ok(())
        })();
        match res {
            Ok(()) => {
                self.health.on_io_success();
                if !self.health.is_offline() {
                    self.drain_pending_forgets();
                }
            }
            Err(err) => {
                self.health.on_probe_error(&self.files.dir, &err);
                log::warn!(
                    "cladefs: part cache shard {} health probe failed: {err}",
                    root.display()
                );
            }
        }
    }

    fn forget_in_memory(&self, key: &str) {
        let gone = {
            let mut kept = self.kept.lock().unwrap();
            let gone = kept.forget(key);
            if let Some(Evicted::Remove(k)) = &gone {
                kept.removing.remove(k);
            }
            gone
        };
        if let Some(Evicted::Remove(k)) = gone {
            self.pending_forgets.lock().unwrap().push(k);
        }
    }

    fn drain_pending_forgets(&self) {
        let pending = std::mem::take(&mut *self.pending_forgets.lock().unwrap());
        for key in pending {
            if let Err(err) = self.files.remove(&key)
                && err.kind() != io::ErrorKind::NotFound
            {
                log::warn!("cladefs: removing deferred stale copy of {key}: {err}");
            }
        }
    }

    fn has_key(&self, key: &str) -> bool {
        let kept = self.kept.lock().unwrap();
        kept.objects.contains_key(key)
    }

    fn holds(&self, key: &str, part: u64) -> bool {
        let kept = self.kept.lock().unwrap();
        kept.objects.get(key).is_some_and(|object| object.kept.contains_key(&part))
    }

    fn tail(&self, key: &str) -> io::Result<Option<object::Tail>> {
        let (place, frame) = {
            let mut kept = self.kept.lock().unwrap();
            let Some(object) = kept.objects.get(key).filter(|object| object.readable()) else {
                return Ok(None);
            };
            let found = (object.place(TAIL), object.parts.frame);
            kept.touch(key, TAIL);
            found
        };
        let Some(bytes) = self.read(key, place)? else { return Ok(None) };
        match object::Tail::of(key, bytes) {
            Ok(tail) if tail.parts.frame == frame => Ok(Some(tail)),
            checked => {
                let failed =
                    checked.err().map_or("its frame changed".into(), |err| err.to_string());
                log::warn!("cladefs: the tail of {key} in the cache: {failed}");
                self.forget(key)?;
                Ok(None)
            }
        }
    }

    fn part(&self, key: &str, part: u64) -> io::Result<Option<Vec<u8>>> {
        let stored = {
            let mut kept = self.kept.lock().unwrap();
            let Some(object) = kept.objects.get(key) else { return Ok(None) };
            let Some(&stored) = object.parts.stored.get(part as usize) else { return Ok(None) };
            if !object.kept.contains_key(&part) {
                return Ok(None);
            }
            kept.touch(key, part);
            stored
        };
        let Some(bytes) = self.read(key, stored.range())? else { return Ok(None) };
        if stored.check(&bytes).is_ok() {
            return Ok(Some(bytes));
        }
        // Zeros are where no part is, a hole its filesystem reads as data (APFS fills those near
        // a write): what an opening counts kept unread, not a part gone bad.
        match bytes.iter().all(|&byte| byte == 0) {
            true => log::debug!("cladefs: part {part} of {key} in the cache is zeros"),
            false => log::warn!("cladefs: part {part} of {key} in the cache failed its CRC"),
        }
        self.forget_part(key, part)?;
        Ok(None)
    }

    fn read(&self, key: &str, place: Range<u64>) -> io::Result<Option<Vec<u8>>> {
        match self.files.read(key, place) {
            Ok(bytes) => {
                self.health.on_io_success();
                Ok(Some(bytes))
            }
            Err(err)
                if matches!(err.kind(), io::ErrorKind::NotFound | io::ErrorKind::UnexpectedEof) =>
            {
                self.forget(key)?;
                Ok(None)
            }
            Err(err) => {
                self.health.on_io_error(&self.files.dir, &err);
                Err(err)
            }
        }
    }

    /// Keeps `tail` on this shard. Returns `Ok(true)` if kept (or already kept/busy), `Ok(false)`
    /// if `tail` did not fit this shard's capacity (allowing JBOD failover to another shard).
    fn keep_tail(&self, key: &str, tail: &object::Tail) -> io::Result<bool> {
        let len = tail.bytes.len() as u64;
        if len > self.capacity() {
            return Ok(false);
        }
        let mut kept = self.kept.lock().unwrap();
        if let Some(object) = kept.objects.get(key)
            && object.parts != tail.parts
            && object.busy.is_empty()
        {
            let gone = Vec::from_iter(kept.forget(key));
            drop(kept);
            self.evict(gone)?;
            kept = self.kept.lock().unwrap();
        }
        match kept.objects.get(key) {
            Some(object) if object.readable() && object.parts == tail.parts => {
                kept.touch(key, TAIL);
                return Ok(true);
            }
            // Being written, or forgotten; or another object, parts of it busy.
            Some(_) => return Ok(true),
            None if kept.removing.contains(key) => return Ok(true),
            None => {}
        }
        kept.objects.insert(key.to_owned(), Object::writing(tail.parts.clone(), TAIL));
        let (fits, mut evicted) = kept.make_room(self.capacity(), len);
        if fits {
            kept.take(len);
            kept.disk.cache_writing.add(len);
        } else {
            evicted.extend(kept.freed(key, TAIL));
        }
        drop(kept);
        let evicting = self.evict(evicted);
        if !fits {
            evicting?;
            return Ok(false);
        }
        let frame = tail.parts.frame;
        let written = evicting
            .and_then(|()| self.files.create(key, frame.size()))
            .and_then(|()| self.files.write(key, frame.tail().start, &tail.bytes));
        self.record_io(self.settle(key, TAIL, written))?;
        Ok(true)
    }

    fn keep_part(&self, key: &str, part: u64, bytes: &[u8]) -> io::Result<()> {
        let stored = {
            let kept = self.kept.lock().unwrap();
            let stored =
                kept.objects.get(key).and_then(|object| object.parts.stored.get(part as usize));
            let Some(&stored) = stored else { return Ok(()) };
            stored
        };
        debug_assert!(stored.check(bytes).is_ok(), "part {part} of {key}, kept unchecked");
        let len = stored.len;
        if len > self.capacity() {
            return Ok(());
        }
        let (fits, evicted) = {
            let mut kept = self.kept.lock().unwrap();
            let Some(object) = kept.objects.get_mut(key) else { return Ok(()) };
            if !object.readable()
                || object.parts.stored.get(part as usize) != Some(&stored)
                || object.kept.contains_key(&part)
                || object.busy.contains_key(&part)
            {
                return Ok(());
            }
            object.busy.insert(part, Busy::Writing);
            let (fits, mut evicted) = kept.make_room(self.capacity(), len);
            if fits {
                kept.take(len);
                kept.disk.cache_writing.add(len);
            } else {
                evicted.extend(kept.freed(key, part));
            }
            (fits, evicted)
        };
        let evicting = self.evict(evicted);
        if !fits {
            return evicting;
        }
        let written =
            evicting.and_then(|()| self.files.write(key, stored.offset, &bytes[..len as usize]));
        self.record_io(self.settle(key, part, written))
    }

    fn settle(&self, key: &str, part: u64, written: io::Result<()>) -> io::Result<()> {
        let gone = self.kept.lock().unwrap().written(key, part, written.is_ok());
        if let Err(err) = self.evict(Vec::from_iter(gone)) {
            log::warn!("cladefs: removing {key} from the cache: {err}");
        }
        written
    }

    fn adopt(&self, key: &str, from: &Path) -> io::Result<bool> {
        let found = self.files.found(from, key)?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, format!("{}: no object", from.display()))
        })?;
        let len = found.bytes();
        if len > self.capacity() {
            return Ok(false);
        }
        let (fits, evicted) = {
            let mut kept = self.kept.lock().unwrap();
            match kept.objects.get(key) {
                _ if kept.removing.contains(key) => return Ok(false),
                Some(object) if !object.busy.is_empty() => return Ok(false),
                // Its file goes with the rename, which replaces it.
                Some(_) => {
                    kept.forget(key);
                }
                None => {
                    kept.removing.insert(key.to_owned());
                }
            }
            let (fits, mut evicted) = kept.make_room(self.capacity(), len);
            if fits {
                kept.take(len);
            } else {
                evicted.push(Evicted::Remove(key.to_owned()));
            }
            (fits, evicted)
        };
        let evicting = self.evict(evicted);
        if !fits {
            return evicting.map(|()| false);
        }
        let renamed = evicting.and_then(|()| self.files.rename(from, key));
        let mut kept = self.kept.lock().unwrap();
        kept.removing.remove(key);
        match renamed {
            Ok(()) => {
                kept.insert(key, found);
                self.health.on_io_success();
                Ok(true)
            }
            Err(err) => {
                kept.give(len);
                self.health.on_io_error(&self.files.dir, &err);
                Err(err)
            }
        }
    }

    fn forget(&self, key: &str) -> io::Result<()> {
        let gone = self.kept.lock().unwrap().forget(key);
        self.evict(Vec::from_iter(gone))
    }

    fn forget_part(&self, key: &str, part: u64) -> io::Result<()> {
        let evicted = {
            let mut kept = self.kept.lock().unwrap();
            match kept.objects.get(key) {
                Some(object) if object.kept.contains_key(&part) => kept.evict(key, part),
                _ => return Ok(()),
            }
        };
        self.evict(Vec::from_iter(evicted))
    }

    fn capacity(&self) -> u64 {
        let base = self.budget.load(Ordering::Relaxed).min(self.governed.load(Ordering::Relaxed));
        match self.files.disk.cache_cap() {
            Some(cap) => base.min(cap),
            None => base,
        }
    }

    fn allows_fills(&self) -> bool {
        !self.health.is_offline() && self.files.disk.accepts_writes()
    }

    fn bytes(&self) -> u64 {
        self.kept.lock().unwrap().bytes
    }

    fn evict(&self, mut evicted: Vec<Evicted>) -> io::Result<()> {
        let present = |done: io::Result<()>| match done {
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            done => done,
        };
        let mut done = Ok(());
        let mut at = 0;
        while let Some(next) = evicted.get(at) {
            at += 1;
            match next {
                Evicted::Punch { key, part, place } => {
                    let res = present(self.files.punch(key, place.clone()));
                    if let Err(err) = &res {
                        self.health.on_io_error(&self.files.dir, err);
                    }
                    done = done.and(res);
                    let gone = self.kept.lock().unwrap().freed(key, *part);
                    evicted.extend(gone);
                }
                Evicted::Remove(key) => {
                    let res = present(self.files.remove(key));
                    if let Err(err) = &res {
                        self.health.on_io_error(&self.files.dir, err);
                    }
                    done = done.and(res);
                    self.kept.lock().unwrap().removing.remove(key);
                }
            }
        }
        done
    }
}

/// cladefs: what a shard kept, its files, stays on its disk, which another opening of it counts
/// anew.
impl Drop for Shard {
    fn drop(&mut self) {
        let kept = self.kept.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner);
        let bytes = kept.bytes;
        kept.give(bytes);
    }
}

impl Cache {
    /// The cache in `dir` on the process's default host outside `madsim` ([`Cache::open_on`]).
    #[cfg(not(madsim))]
    pub fn open(dir: &Path, budget: u64) -> io::Result<Self> {
        Self::open_on(crate::Host::process(), dir, budget)
    }

    /// The JBOD cache across `dirs` on the process's default host outside `madsim`.
    #[cfg(not(madsim))]
    pub fn open_all(dirs: &[PathBuf], budget: u64) -> io::Result<Self> {
        Self::open_all_on(crate::Host::process(), dirs, budget)
    }

    /// The cache in `dir` on `host`, keeping at most `budget` bytes of objects (`u64::MAX`, none),
    /// and what the disk governor leaves it, once it says ([`Cache::set_limit`]): what an earlier
    /// one kept, read from its files, those that fail their checks removed, then evicted, those
    /// read least recently first, as far as `budget` needs; and its directory of cells at versions
    /// emptied ([`Cache::versions`]). Fails while another cache has the directory.
    pub fn open_on(host: &crate::Host, dir: &Path, budget: u64) -> io::Result<Self> {
        Self::open_all_on(host, &[dir.to_owned()], budget)
    }

    /// Opens a JBOD part cache across one or more directories `dirs` on `host`, keeping at most
    /// `budget` bytes of objects across all shards combined (`u64::MAX` for unlimited, governed
    /// per-filesystem by the disk governor). When `budget` is finite, it is apportioned across
    /// shards proportionally to each shard's filesystem size. Refuses an empty list or duplicate
    /// directories.
    pub fn open_all_on(host: &crate::Host, dirs: &[PathBuf], budget: u64) -> io::Result<Self> {
        if dirs.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "part cache requires at least one directory",
            ));
        }
        let mut seen_dirs = HashSet::new();
        let mut seen_devs = HashSet::new();
        let mut shards = Vec::with_capacity(dirs.len());
        for dir in dirs {
            host_fs::create_dir_all(dir)?;
            let canonical = host_fs::canonicalize(dir)?;
            if !seen_dirs.insert(canonical.clone()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{}: duplicate part cache directory", dir.display()),
                ));
            }
            let shard = Shard::open_on(host, dir, u64::MAX)?;
            if let Some(dev) = shard.files.disk.dev()
                && !seen_devs.insert(dev)
            {
                log::warn!(
                    "cladefs: part cache directory {} shares filesystem {dev:#x} with another shard",
                    dir.display()
                );
            }
            shards.push(shard);
        }
        let weights: Vec<u64> =
            shards.iter().map(|s| s.disk_size.load(Ordering::Relaxed)).collect();
        let budgets = apportion(budget, &weights);
        for (shard, shard_budget) in shards.iter().zip(budgets) {
            shard.set_budget(shard_budget)?;
        }
        Ok(Self { shards, total_budget: budget, versions_rr: AtomicUsize::new(0) })
    }

    /// Ranks shard indices for `key` by weighted rendezvous score (highest first), skipping
    /// `Offline` shards (or non-`Online` shards when `online_only` is set).
    fn ranked_shards(&self, key: &str, online_only: bool) -> Vec<usize> {
        if self.shards.len() == 1 {
            let s = &self.shards[0];
            let ok = if online_only { !s.health.is_offline() } else { true };
            return if ok { vec![0] } else { Vec::new() };
        }
        let mut scored: Vec<(usize, f64)> = self
            .shards
            .iter()
            .enumerate()
            .filter(
                |(_, s)| {
                    if online_only { s.health.is_online() } else { !s.health.is_offline() }
                },
            )
            .map(|(i, s)| (i, weighted_score(s.disk_id, s.weight(), key)))
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        scored.into_iter().map(|(i, _)| i).collect()
    }

    /// cladefs: keeps no more than `bytes` of objects across all shards combined, nor its budget,
    /// from now on, evicting what was read least recently down to that at once. Returns what it
    /// keeps then.
    pub fn set_limit(&self, bytes: u64) -> io::Result<u64> {
        if self.shards.len() == 1 {
            return self.shards[0].set_limit(bytes);
        }
        let weights: Vec<u64> =
            self.shards.iter().map(|s| s.disk_size.load(Ordering::Relaxed)).collect();
        let limits = apportion(bytes, &weights);
        let mut total = 0u64;
        for (shard, limit) in self.shards.iter().zip(limits) {
            total += shard.set_limit(limit)?;
        }
        Ok(total)
    }

    /// cladefs: governs the part-cache shard(s) living on `disk`, setting their governed limit to
    /// `bytes` (what `disk`'s cells leave of its allowance, doc-22), capped by any per-disk
    /// `cache_cap`, and updating `disk_size` to the disk's effective capped capacity for weighted
    /// JBOD placement. Returns `Ok(None)` if no shard lives on `disk`.
    pub fn set_limit_on(
        &self,
        disk: &Arc<HostDisk>,
        bytes: u64,
        disk_size: u64,
    ) -> io::Result<Option<u64>> {
        let matching: Vec<&Shard> =
            self.shards.iter().filter(|s| Arc::ptr_eq(&s.files.disk, disk)).collect();
        if matching.is_empty() {
            return Ok(None);
        }
        let capped_bytes = match disk.cache_cap() {
            Some(cap) => bytes.min(cap),
            None => bytes,
        };
        let effective_weight = match disk.cache_cap() {
            Some(cap) => disk.effective_capacity(disk_size).min(cap),
            None => disk.effective_capacity(disk_size),
        };
        let n = matching.len() as u64;
        let per = if capped_bytes == u64::MAX { u64::MAX } else { capped_bytes / n };
        let mut kept = 0u64;
        for shard in matching {
            if effective_weight > 0 {
                shard.disk_size.store(effective_weight, Ordering::Relaxed);
            }
            kept += shard.set_limit(per)?;
        }
        Ok(Some(kept))
    }

    /// Re-probes any `Suspect`, `Offline`, or `Returning` shards so recovered disks rejoin
    /// automatically. Probes are rate-limited per shard and dispatched off the governor thread so
    /// a wedged drive never stalls governance of healthy disks.
    pub fn probe_unhealthy(self: &Arc<Self>) {
        struct ProbeGuard<'a>(&'a ShardHealthState);
        impl Drop for ProbeGuard<'_> {
            fn drop(&mut self) {
                self.0.finish_probe();
            }
        }

        let now = Instant::now();
        for idx in 0..self.shards.len() {
            if !self.shards[idx].health.try_begin_probe(now) {
                continue;
            }
            #[cfg(not(madsim))]
            {
                let cache = Arc::clone(self);
                let spawned = std::thread::Builder::new()
                    .name(format!("cache shard probe {idx}"))
                    .spawn(move || {
                        let _guard = ProbeGuard(&cache.shards[idx].health);
                        cache.shards[idx].probe();
                    });
                if let Err(err) = spawned {
                    self.shards[idx].health.finish_probe();
                    log::warn!("cladefs: spawning part cache shard {idx} probe thread: {err}");
                }
            }
            #[cfg(madsim)]
            {
                let _guard = ProbeGuard(&self.shards[idx].health);
                host_fs::may_block(|| self.shards[idx].probe());
            }
        }
    }

    /// All shard root directories (and object directories) currently held open by this cache.
    pub fn shard_dirs(&self) -> Vec<PathBuf> {
        let mut out = Vec::with_capacity(self.shards.len() * 2);
        for shard in &self.shards {
            if let Some(root) = shard.files.dir.parent() {
                out.push(root.to_path_buf());
            }
            out.push(shard.files.dir.clone());
        }
        out
    }

    /// Runtime health state of each shard paired with its [`HostDisk`], for metrics.
    pub fn shard_states(&self) -> Vec<(Arc<HostDisk>, ShardHealth)> {
        self.shards.iter().map(|s| (s.files.disk.clone(), s.health.get())).collect()
    }

    #[cfg(test)]
    pub(crate) fn path(&self, key: &str) -> PathBuf {
        for shard in &self.shards {
            if shard.has_key(key) {
                return shard.files.path(key);
            }
        }
        let idx = self.ranked_shards(key, false).into_iter().next().unwrap_or(0);
        self.shards[idx].files.path(key)
    }

    /// The first shard's directory where the cache keeps its objects.
    pub fn dir(&self) -> &Path {
        &self.shards[0].files.dir
    }

    /// All shard object directories.
    pub fn dirs(&self) -> Vec<&Path> {
        self.shards.iter().map(|s| s.files.dir.as_path()).collect()
    }

    /// Human-readable display of the cache's shard directories (`:`-separated when multiple).
    pub fn dirs_display(&self) -> String {
        self.shards.iter().map(|s| s.files.dir.display().to_string()).collect::<Vec<_>>().join(":")
    }

    /// cladefs: a directory, on the filesystem of one of the cache's healthy shards, for cells at
    /// versions the host opens of its own, to fork them (board 6589): as every cell the cache
    /// serves, they must sit on a shard's filesystem (TASK-359). Spread round-robin across healthy
    /// shards so temporary version dirs share JBOD IOPS.
    pub fn versions(&self) -> &Path {
        if self.shards.len() == 1 {
            return &self.shards[0].versions;
        }
        let start = self.versions_rr.fetch_add(1, Ordering::Relaxed) % self.shards.len();
        for offset in 0..self.shards.len() {
            let shard = &self.shards[(start + offset) % self.shards.len()];
            if shard.health.is_online() {
                return &shard.versions;
            }
        }
        &self.shards[start].versions
    }

    /// The tail of the object at `key`, if it is kept on any shard, checked against its CRCs: one
    /// that fails is dropped, its object with it.
    pub fn tail(&self, key: &str) -> io::Result<Option<object::Tail>> {
        if self.shards.len() == 1 {
            return self.shards[0].tail(key);
        }
        let ranked = self.ranked_shards(key, false);
        let mut last_err = None;
        for idx in ranked {
            let shard = &self.shards[idx];
            if !shard.has_key(key) {
                continue;
            }
            match shard.tail(key) {
                Ok(Some(tail)) => return Ok(Some(tail)),
                Ok(None) => {}
                Err(err) => last_err = Some(err),
            }
        }
        match last_err {
            Some(err) => Err(err),
            None => Ok(None),
        }
    }

    /// Whether part `part` of the object at `key` is kept on any non-offline shard, as the cache's
    /// index has it: reading nothing of its file, nor counting it read, as [`Cache::part`] does.
    pub fn holds(&self, key: &str, part: u64) -> bool {
        if self.shards.len() == 1 {
            return self.shards[0].holds(key, part);
        }
        self.shards.iter().any(|s| !s.health.is_offline() && s.holds(key, part))
    }

    /// The stored bytes of part `part` of the object at `key`, if it is kept, and they pass the CRC
    /// its tail, kept with it, gives: one that fails is dropped.
    pub fn part(&self, key: &str, part: u64) -> io::Result<Option<Vec<u8>>> {
        if self.shards.len() == 1 {
            return self.shards[0].part(key, part);
        }
        let ranked = self.ranked_shards(key, false);
        let mut last_err = None;
        for idx in ranked {
            let shard = &self.shards[idx];
            if !shard.has_key(key) {
                continue;
            }
            match shard.part(key, part) {
                Ok(Some(bytes)) => return Ok(Some(bytes)),
                Ok(None) => {}
                Err(err) => last_err = Some(err),
            }
        }
        match last_err {
            Some(err) => Err(err),
            None => Ok(None),
        }
    }

    /// Keeps `tail`, the tail of the object at `key`, as read, evicting what was read least
    /// recently to make room. Across JBOD shards, tries healthy shards in weighted rendezvous
    /// order so a full or failing shard transparently fails over to the next healthy shard.
    pub fn keep_tail(&self, key: &str, tail: &object::Tail) -> io::Result<()> {
        if self.shards.len() == 1 {
            if !self.shards[0].allows_fills() {
                return Ok(());
            }
            self.shards[0].keep_tail(key, tail)?;
            return Ok(());
        }
        // If any writable non-offline shard already holds `key`, update/touch it in place rather
        // than duplicating across shards.
        for shard in &self.shards {
            if shard.allows_fills() && shard.has_key(key) {
                shard.keep_tail(key, tail)?;
                return Ok(());
            }
        }
        let mut ranked = self.ranked_shards(key, true);
        if ranked.is_empty() {
            ranked = self.ranked_shards(key, false);
        }
        let mut last_err = None;
        for idx in ranked {
            if !self.shards[idx].allows_fills() {
                continue;
            }
            match self.shards[idx].keep_tail(key, tail) {
                Ok(true) => return Ok(()),
                Ok(false) => continue,
                Err(err) => last_err = Some(err),
            }
        }
        match last_err {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// Keeps `bytes`, the stored bytes of part `part` of the object at `key`, on the shard that
    /// holds its tail.
    pub fn keep_part(&self, key: &str, part: u64, bytes: &[u8]) -> io::Result<()> {
        if self.shards.len() == 1 {
            if !self.shards[0].allows_fills() {
                return Ok(());
            }
            return self.shards[0].keep_part(key, part, bytes);
        }
        for shard in &self.shards {
            if shard.allows_fills() && shard.has_key(key) {
                return shard.keep_part(key, part, bytes);
            }
        }
        Ok(())
    }

    /// Takes the object framed whole in the file at `from`, on one of the cache's filesystems, as
    /// the one at `key`, by renaming it onto the shard living on `from`'s filesystem (never
    /// crossing filesystems with `EXDEV`), evicting what was read least recently there to make
    /// room. If another shard held an earlier copy of `key`, it is forgotten there.
    pub fn adopt(&self, key: &str, from: &Path) -> io::Result<bool> {
        if self.shards.len() == 1 {
            return self.shards[0].adopt(key, from);
        }
        use host_fs::MetadataExt as _;
        let from_dev = host_fs::metadata(from)?.dev();
        let mut candidates: Vec<usize> = self
            .shards
            .iter()
            .enumerate()
            .filter(|(_, s)| s.files.disk.dev() == Some(from_dev) && !s.health.is_offline())
            .map(|(i, _)| i)
            .collect();
        if candidates.is_empty() {
            return Err(io::Error::from_raw_os_error(libc::EXDEV));
        }
        // When multiple shards share one filesystem (e.g. unit tests on one tempdir), pick by
        // weighted rendezvous score; on JBOD production hosts there is one shard per filesystem.
        candidates.sort_by(|&a, &b| {
            let sa = weighted_score(self.shards[a].disk_id, self.shards[a].weight(), key);
            let sb = weighted_score(self.shards[b].disk_id, self.shards[b].weight(), key);
            sb.total_cmp(&sa).then_with(|| a.cmp(&b))
        });
        let target = candidates[0];
        let adopted = self.shards[target].adopt(key, from)?;
        if adopted {
            for (i, shard) in self.shards.iter().enumerate() {
                if i != target && shard.has_key(key) {
                    if shard.health.is_offline() {
                        // Never block a healthy disk's WAL adoption on an offline disk's unlink:
                        // drop the superseded entry from memory immediately and defer unlinking
                        // until the shard passes health probes.
                        shard.forget_in_memory(key);
                    } else if let Err(err) = shard.forget(key) {
                        log::warn!("cladefs: forgetting superseded copy of {key}: {err}");
                    }
                }
            }
        }
        Ok(adopted)
    }

    /// Drops the object at `key`: one whose copy failed its CRCs.
    pub fn forget(&self, key: &str) -> io::Result<()> {
        let mut done = Ok(());
        for shard in &self.shards {
            if shard.has_key(key) {
                done = done.and(shard.forget(key));
            }
        }
        done
    }

    /// Drops part `part` of the object at `key`, if it is kept: its tail, the object.
    #[cfg(test)]
    pub(crate) fn forget_part(&self, key: &str, part: u64) -> io::Result<()> {
        let mut done = Ok(());
        for shard in &self.shards {
            if shard.has_key(key) {
                done = done.and(shard.forget_part(key, part));
            }
        }
        done
    }

    /// cladefs: the disk of the first shard's filesystem (doc-22).
    pub fn disk(&self) -> &Arc<HostDisk> {
        &self.shards[0].files.disk
    }

    /// cladefs: all host disks backing this cache's JBOD shards.
    pub fn disks(&self) -> Vec<Arc<HostDisk>> {
        self.shards.iter().map(|s| s.files.disk.clone()).collect()
    }

    /// Whether any shard of this cache lives on the filesystem `dev` (`st_dev`), allowing
    /// zero-copy `rename(2)` adoption from cell directories on `dev`.
    pub fn has_disk(&self, dev: u64) -> bool {
        self.shards.iter().any(|s| s.files.disk.dev() == Some(dev))
    }

    /// How many bytes the shard(s) on `disk` currently hold, or `None` if no shard is on `disk`.
    pub fn bytes_on(&self, disk: &Arc<HostDisk>) -> Option<u64> {
        let mut found = false;
        let mut sum = 0u64;
        for shard in &self.shards {
            if Arc::ptr_eq(&shard.files.disk, disk) {
                found = true;
                sum += shard.bytes();
            }
        }
        found.then_some(sum)
    }

    /// The most bytes of objects it keeps across all shards combined: its budget, or what the disk
    /// governor leaves them, the less.
    pub fn capacity(&self) -> u64 {
        let sum = self.shards.iter().fold(0u64, |acc, s| acc.saturating_add(s.capacity()));
        self.total_budget.min(sum)
    }

    /// How many bytes of objects are kept across all shards combined.
    pub fn bytes(&self) -> u64 {
        self.shards.iter().map(Shard::bytes).sum()
    }
}

/// Locks the directory `dir`, as a cache that has it does: fails if another has. cladefs: a
/// process's lock, which its death lets go, a simulated host's too (`host_fs::Flock`).
fn lock(dir: &Path) -> io::Result<host_fs::Flock> {
    match host_fs::Flock::try_lock(File::open(dir)?) {
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
            let held = format!("{}: another cache has it", dir.display());
            Err(io::Error::new(io::ErrorKind::WouldBlock, held))
        }
        locked => locked,
    }
}

/// An object a file of the cache holds: where its parts are stored, as its tail, checked against
/// its CRCs, says; those the file holds data for, not holes, unread, their CRCs checked as each is
/// read ([`Cache::part`]); and when it was written last.
#[derive(Debug)]
struct Found {
    parts: Parts,
    kept: Vec<u64>,
    modified: SystemTime,
}

impl Found {
    /// The bytes of its tail and of the parts it holds.
    fn bytes(&self) -> u64 {
        let tail = self.parts.frame.tail();
        let parts = self.kept.iter().map(|&part| self.parts.stored[part as usize].len);
        tail.end - tail.start + parts.sum::<u64>()
    }
}

/// cladefs: the file at `path`, opened to read, and to write if `write`, bypassing the host's page
/// cache ([`crate::direct::bypass`], TASK-433.4), or failing if its file system can't (TASK-433.6).
fn opened(path: &Path, write: bool) -> io::Result<File> {
    let file = File::options().read(true).write(write).open(path)?;
    // A directory where an object's file was is none, as a read of it would say, rather than
    // what bypassing the cache on it says (EPERM on macOS).
    if file.metadata()?.is_dir() {
        return Err(io::Error::from_raw_os_error(libc::EISDIR));
    }
    crate::direct::bypass(&file, path)?;
    Ok(file)
}

/// The ranges of `file`, of `size` bytes, that hold data, not holes, as `SEEK_DATA` and
/// `SEEK_HOLE` find them: all of it, on a filesystem that finds no holes.
fn data(file: &File, size: u64) -> io::Result<Vec<Range<u64>>> {
    host_fs::data_ranges(file, size)
}

/// cladefs: the filesystem `dir` is on, checked once, as the cache opens: that its files bypass
/// the host's page cache, or the cache fails to open, naming `dir` (TASK-433.6).
fn bypassed(dir: &Path) -> io::Result<()> {
    let path = dir.join(".bypass");
    let mut options = host_fs::OpenOptions::new();
    let file = options.create(true).truncate(true).read(true).write(true).open(&path)?;
    let bypassed = crate::direct::bypass(&file, dir);
    host_fs::remove_file(&path)?;
    bypassed
}

/// Whether `range` is within one of `data`, ranges in order.
fn holds(data: &[Range<u64>], range: Range<u64>) -> bool {
    let after = data.partition_point(|data| data.start <= range.start);
    after > 0 && data[after - 1].end >= range.end
}

/// What changes the cache makes to its files: each a step of [`Files`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    /// An object's file made, as long as it ever is not yet.
    Create,
    /// Made as long as its object.
    Resize,
    /// Bytes written to it.
    Write,
    Punch,
    Remove,
    /// A file adopted.
    Rename,
}

/// The cache's files, under its directory, each its object's at its key: every change the cache
/// makes to them is one of these, which the crash tests cut, as a crash would, at each.
#[derive(Debug)]
struct Files {
    dir: PathBuf,
    /// cladefs: its filesystem's disk (doc-22).
    disk: Arc<HostDisk>,
    #[cfg(test)]
    cut: Option<std::sync::Arc<tests::Cut>>,
    /// The bytes read of its files, an opening's included.
    #[cfg(test)]
    bytes_read: AtomicU64,
}

impl Files {
    fn new_on(host: &crate::Host, dir: PathBuf) -> io::Result<Self> {
        bypassed(&dir)?;
        Ok(Self {
            disk: HostDisk::of_on(host, &dir)?,
            dir,
            #[cfg(test)]
            cut: None,
            #[cfg(test)]
            bytes_read: AtomicU64::new(0),
        })
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(key)
    }

    /// Finds the objects in the files under `rel`, removing files of none, and directories left
    /// empty. Returns whether there is any.
    fn walk(&self, rel: &Path, found: &mut Vec<(String, Found)>) -> io::Result<bool> {
        let mut any = false;
        for entry in host_fs::read_dir(self.dir.join(rel))? {
            let entry = entry?;
            let (rel, kind) = (rel.join(entry.file_name()), entry.file_type()?);
            let path = self.dir.join(&rel);
            if kind.is_dir() {
                if self.walk(&rel, found)? {
                    any = true;
                } else if let Err(err) = host_fs::remove_dir(&path) {
                    log::warn!("cladefs: removing {} from the cache: {err}", path.display());
                }
                continue;
            }
            let object = match rel.to_str() {
                Some(key) if kind.is_file() => match self.found(&path, key) {
                    Ok(object) => object.map(|object| (key.to_owned(), object)),
                    Err(err) => {
                        log::warn!("cladefs: reading {} in the cache: {err}", path.display());
                        None
                    }
                },
                _ => None,
            };
            match object {
                Some(object) => {
                    found.push(object);
                    any = true;
                }
                None => {
                    log::warn!("cladefs: removing {} from the cache: no object", path.display());
                    host_fs::remove_file(&path)?;
                }
            }
        }
        Ok(any)
    }

    /// The object at `key` the file at `path` holds, if its tail, at its end, passes its checks:
    /// its tail read, and where the file holds data, none of its parts.
    fn found(&self, path: &Path, key: &str) -> io::Result<Option<Found>> {
        let file = opened(path, false)?;
        let metadata = file.metadata()?;
        let size = metadata.len();
        let Some(at) = size.checked_sub(TRAILER) else { return Ok(None) };
        let mut trailer = vec![0; TRAILER as usize];
        self.read_exact(&file, &mut trailer, at)?;
        let Ok(frame) = Frame::of_trailer(&trailer) else { return Ok(None) };
        if frame.size() != size {
            return Ok(None);
        }
        let tail = frame.tail();
        let mut bytes = vec![0; (tail.end - tail.start) as usize];
        self.read_exact(&file, &mut bytes, tail.start)?;
        let Ok(tail) = object::Tail::of(key, bytes) else { return Ok(None) };
        let data = data(&file, size)?;
        let stored = (0..).zip(&tail.parts.stored);
        let kept = stored.filter(|(_, stored)| holds(&data, stored.range())).map(|(part, _)| part);
        let kept = kept.collect();
        Ok(Some(Found { parts: tail.parts, kept, modified: metadata.modified()? }))
    }

    /// Makes the file of the object at `key`, of `size` bytes, if there is none.
    fn create(&self, key: &str, size: u64) -> io::Result<()> {
        if !self.done(Change::Create) {
            return Ok(());
        }
        let path = self.path(key);
        if let Some(parent) = path.parent() {
            host_fs::create_dir_all(parent)?;
        }
        let file =
            host_fs::OpenOptions::new().create(true).write(true).truncate(false).open(path)?;
        // Its file is the object's size from the first, so that what is not written to it is a
        // hole: some filesystems (APFS) fill what a write past the end skips.
        if file.metadata()?.len() < size && self.done(Change::Resize) {
            file.set_len(size)?;
        }
        Ok(())
    }

    /// Writes `bytes` at `offset` in the file of the object at `key`, bypassing the host's page
    /// cache ([`crate::direct`], TASK-433.4): the cache's copies are on its disk alone, what is
    /// held in memory the fetchers' to hold, within their budget.
    fn write(&self, key: &str, offset: u64, bytes: &[u8]) -> io::Result<()> {
        let file = opened(&self.path(key), true)?;
        for piece in self.pieces(Change::Write, offset..offset + bytes.len() as u64) {
            let from = (piece.start - offset) as usize..(piece.end - offset) as usize;
            let mut tail = crate::direct::Tail::default();
            crate::direct::write_at(&file, &mut tail, &bytes[from], piece.start)?;
        }
        Ok(())
    }

    /// Frees the blocks wholly within `place` in the file of the object at `key`, which reads
    /// zeros there after.
    fn punch(&self, key: &str, place: Range<u64>) -> io::Result<()> {
        let file = File::options().write(true).open(self.path(key))?;
        for piece in self.pieces(Change::Punch, place) {
            punch(&file, piece.start, piece.end - piece.start)?;
        }
        Ok(())
    }

    fn remove(&self, key: &str) -> io::Result<()> {
        match self.done(Change::Remove) {
            true => host_fs::remove_file(self.path(key)),
            false => Ok(()),
        }
    }

    /// Moves the file at `from` to be that of the object at `key`, replacing any there.
    fn rename(&self, from: &Path, key: &str) -> io::Result<()> {
        let path = self.path(key);
        if let Some(parent) = path.parent() {
            host_fs::create_dir_all(parent)?;
        }
        match self.done(Change::Rename) {
            true => host_fs::rename(from, path),
            false => Ok(()),
        }
    }

    /// The bytes at `place` in the file of the object at `key`, read bypassing the host's page
    /// cache ([`Files::write`]).
    fn read(&self, key: &str, place: Range<u64>) -> io::Result<Vec<u8>> {
        let mut bytes = vec![0; (place.end - place.start) as usize];
        let file = opened(&self.path(key), false)?;
        self.read_exact(&file, &mut bytes, place.start)?;
        Ok(bytes)
    }

    /// cladefs: reads all of `bytes` at `at` in `file`, as [`crate::direct::read_at`] does, failing
    /// as `read_exact_at` does if the file ends before them (`UnexpectedEof`): one that shrank
    /// behind the cache's back.
    fn read_exact(&self, file: &File, bytes: &mut [u8], at: u64) -> io::Result<()> {
        if at + bytes.len() as u64 > file.metadata()?.len() {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        #[cfg(test)]
        self.bytes_read.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        crate::direct::read_at(file, bytes, at)
    }

    /// Whether `change`, which isn't a write, is made.
    #[cfg(not(test))]
    fn done(&self, _: Change) -> bool {
        true
    }

    /// The pieces of `range` that `change`, a write or a punch, changes.
    #[cfg(not(test))]
    fn pieces(&self, _: Change, range: Range<u64>) -> Vec<Range<u64>> {
        vec![range]
    }

    #[cfg(test)]
    fn done(&self, change: Change) -> bool {
        self.cut.as_ref().is_none_or(|cut| cut.done(change))
    }

    #[cfg(test)]
    fn pieces(&self, change: Change, range: Range<u64>) -> Vec<Range<u64>> {
        match &self.cut {
            Some(cut) => cut.pieces(change, range),
            None => vec![range],
        }
    }
}

/// Frees the blocks wholly within `len` bytes at `offset` of `file`, which reads zeros there after.
/// macOS punches whole blocks only, so a part's last partial block stays.
fn punch(file: &File, offset: u64, len: u64) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    let len = len / BLOCK * BLOCK;
    host_fs::punch_hole(file, offset, len)
}

#[cfg(test)]
mod tests {
    mod audit_tests;

    use super::*;
    use crate::layout::PART;
    use std::sync::Arc;

    const KEY: &str = "segments/0000000000001/0000000000002";
    const OTHER: &str = "segments/0000000000001/0000000000003";

    /// `len` bytes that don't compress, from a xorshift generator seeded by `seed`: their objects'
    /// parts are stored raw.
    fn noise(len: u64, seed: u64) -> Vec<u8> {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64 ^ seed;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    /// An object at its key, framed: its bytes and its tail.
    #[derive(Clone)]
    struct Object {
        key: String,
        bytes: Vec<u8>,
        tail: object::Tail,
    }

    impl Object {
        /// The object of `data` at `key`.
        fn new(key: &str, data: &[u8]) -> Self {
            let bytes = extent_frame::framed(
                data,
                b"footer",
                crate::testing::encrypting(crate::layout::test_cell(1)),
            );
            let start = Frame::of(&bytes).unwrap().tail().start as usize;
            let tail = object::Tail::of(key, bytes[start..].to_vec()).unwrap();
            Self { key: key.to_owned(), bytes, tail }
        }

        /// The object at `key` of `len` bytes of noise seeded by `seed`.
        fn noise(key: &str, len: u64, seed: u64) -> Self {
            Self::new(key, &noise(len, seed))
        }

        fn parts(&self) -> u64 {
            self.tail.parts.frame.parts()
        }

        fn stored(&self, part: u64) -> extent_frame::Stored {
            self.tail.parts.stored[part as usize]
        }

        /// Part `part`'s stored bytes.
        fn part(&self, part: u64) -> &[u8] {
            let range = self.stored(part).range();
            &self.bytes[range.start as usize..range.end as usize]
        }

        fn tail_len(&self) -> u64 {
            self.tail.bytes.len() as u64
        }

        /// The bytes of its tail and all its parts.
        fn len(&self) -> u64 {
            self.tail_len() + (0..self.parts()).map(|part| self.stored(part).len).sum::<u64>()
        }

        fn keep_tail(&self, cache: &Cache) {
            cache.keep_tail(&self.key, &self.tail).unwrap();
        }

        fn keep_part(&self, cache: &Cache, part: u64) {
            cache.keep_part(&self.key, part, self.part(part)).unwrap();
        }

        /// Keeps in `cache` what it finds there not: as a reader does, which fetches and keeps
        /// what it reads not.
        fn fetch(&self, cache: &Cache) {
            if cache.tail(&self.key).unwrap().is_none() {
                self.keep_tail(cache);
            }
            for part in 0..self.parts() {
                if self.read(cache, part).is_none() {
                    self.keep_part(cache, part);
                }
            }
        }

        /// Keeps its tail and all its parts in `cache`.
        fn keep(&self, cache: &Cache) {
            self.keep_tail(cache);
            for part in 0..self.parts() {
                self.keep_part(cache, part);
            }
        }

        /// Part `part`'s stored bytes, as `cache` keeps them.
        fn read(&self, cache: &Cache, part: u64) -> Option<Vec<u8>> {
            cache.part(&self.key, part).unwrap()
        }

        /// Which of its parts `cache` keeps, read back as stored: none without its tail.
        fn kept(&self, cache: &Cache) -> Vec<u64> {
            if cache.tail(&self.key).unwrap().is_none() {
                return Vec::new();
            }
            let parts = (0..self.parts()).filter(|&part| self.read(cache, part).is_some());
            parts.collect()
        }

        /// Writes it whole to a file in `dir`, as a cell's WAL would: its path.
        fn write_to(&self, dir: &Path) -> PathBuf {
            let path = dir.join(self.key.replace('/', "-"));
            host_fs::write(&path, &self.bytes).unwrap();
            path
        }
    }

    impl Index {
        /// Checks what the index says of itself: each object it holds keeps or writes something,
        /// keeps parts only with its tail, read after them; what it counts by use it keeps; and,
        /// with none busy, the bytes it counts are those it keeps.
        fn check(&self) {
            let (mut bytes, mut kept) = (0, 0);
            for (key, object) in &self.objects {
                assert!(!object.kept.is_empty() || !object.busy.is_empty(), "{key} holds nothing");
                assert!(object.kept.is_empty() || object.readable(), "{key} kept without its tail");
                assert!(!self.removing.contains(key), "{key} kept while removing");
                let writing = object.busy.get(&TAIL) == Some(&Busy::Writing);
                assert!(object.readable() || writing || object.forgotten, "{key} stuck");
                assert!(!object.forgotten || object.kept.is_empty(), "{key} kept forgotten");
                for (&part, &used) in &object.kept {
                    assert_eq!(self.by_use.get(&used), Some(&(key.clone(), part)));
                    assert!(part == TAIL || used < object.kept[&TAIL], "{key}'s tail read first");
                    bytes += object.len(part);
                }
                kept += object.kept.len();
            }
            assert_eq!(self.by_use.len(), kept);
            if self.objects.values().all(|object| object.busy.is_empty()) {
                assert_eq!(self.bytes, bytes);
            }
        }
    }

    impl Cache {
        /// Its index, checked across all shards.
        fn check(&self) {
            for shard in &self.shards {
                shard.kept.lock().unwrap().check();
            }
        }

        /// The keys of the objects it holds across all shards.
        fn keys(&self) -> Vec<String> {
            let mut keys = Vec::new();
            for shard in &self.shards {
                keys.extend(shard.kept.lock().unwrap().objects.keys().cloned());
            }
            keys.sort();
            keys
        }
    }

    /// Flips a bit of the byte at `at` in the file at `path`, as a torn write might.
    fn flip(path: &Path, at: u64) {
        let file = File::options().read(true).write(true).open(path).unwrap();
        let mut byte = [0];
        file.read_exact_at(&mut byte, at).unwrap();
        file.write_all_at(&[byte[0] ^ 1], at).unwrap();
    }

    /// The keys of the files under `dir`, a cache's.
    fn files(dir: &Path) -> Vec<String> {
        fn walk(dir: &Path, rel: &Path, keys: &mut Vec<String>) {
            for entry in host_fs::read_dir(dir.join(rel)).unwrap() {
                let rel = rel.join(entry.unwrap().file_name());
                match dir.join(&rel).is_dir() {
                    true => walk(dir, &rel, keys),
                    false => keys.push(rel.to_str().unwrap().to_owned()),
                }
            }
        }
        let mut keys = Vec::new();
        walk(&dir.join(FILES), Path::new(""), &mut keys);
        keys.sort();
        keys
    }

    /// Parts read least recently go first, by punching holes where they were, their object's tail
    /// after its last; an object's last part takes its file with it.
    #[test]
    fn keeps_parts_evicting_the_least_recently_read() {
        let dir = tempfile::tempdir().unwrap();
        let object = Object::noise(KEY, 6 * PART, 1);
        let cache = Cache::open(dir.path(), 3 * PART + object.tail_len()).unwrap();
        object.keep_tail(&cache);
        for part in 0..3 {
            object.keep_part(&cache, part);
        }
        assert_eq!(cache.bytes(), 3 * PART + object.tail_len());
        assert_eq!(object.read(&cache, 0).as_deref(), Some(object.part(0)));

        // Part 1 was read least recently.
        object.keep_part(&cache, 5);
        assert_eq!(object.read(&cache, 1), None);
        let mut hole = vec![1; PART as usize];
        File::open(cache.path(KEY)).unwrap().read_exact_at(&mut hole, PART).unwrap();
        assert_eq!(hole, vec![0; PART as usize]);
        assert_eq!(object.kept(&cache), [0, 2, 5]);

        // The tail goes last, with its object's file.
        let other = Object::noise(OTHER, 3 * PART, 2);
        other.keep(&cache);
        assert_eq!(object.kept(&cache), Vec::<u64>::new());
        assert!(!cache.path(KEY).exists());
        assert_eq!(other.kept(&cache), [0, 1, 2]);
        cache.check();
        cache.forget(OTHER).unwrap();
        assert!(!cache.path(OTHER).exists());
        assert_eq!((cache.bytes(), other.kept(&cache)), (0, vec![]));
        cache.check();
    }

    /// Evicted parts' blocks are freed, and parts never kept take none. (APFS frees what it
    /// punches, but fills with zeros the holes near what is written, under 16 MiB or so: parts
    /// never kept take blocks there, board 14705.)
    /// On any filesystem that frees what is punched: the file's data, as SEEK_DATA and SEEK_HOLE
    /// find it, not its blocks, which ZFS, a hotbed workspace's, counts compressed and once their
    /// transaction group syncs; read after a sync, and held to three parts' worth, as ZFS's holes
    /// are of its records, not 4 KiB blocks (board 7751).
    #[cfg(target_os = "linux")]
    #[test]
    fn evicted_parts_free_their_blocks() {
        use std::os::fd::AsRawFd as _;
        // The bytes of the object's file that are data, not holes, its writes and punches synced.
        let allocated = |cache: &Cache, key: &str| {
            let file = host_fs::File::open(cache.path(key)).unwrap();
            let fd = file.as_raw_fd();
            // SAFETY: Syncs the filesystem of a descriptor this closure holds open.
            assert_eq!(unsafe { libc::syncfs(fd) }, 0);
            let (mut at, mut data) = (0, 0);
            loop {
                // SAFETY: Seeks a descriptor this closure holds open.
                let start = unsafe { libc::lseek(fd, at, libc::SEEK_DATA) };
                if start < 0 {
                    // ENXIO: no data past `at`.
                    assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ENXIO));
                    break data;
                }
                // SAFETY: As above.
                let end = unsafe { libc::lseek(fd, start, libc::SEEK_HOLE) };
                assert!(end > start);
                data += (end - start) as u64;
                at = end;
            }
        };
        let dir = tempfile::tempdir().unwrap();
        let object = Object::noise(KEY, 64 * PART, 1);
        let cache = Cache::open(dir.path(), 3 * PART + object.tail_len()).unwrap();
        object.keep_tail(&cache);
        for part in 0..3 {
            object.keep_part(&cache, part);
        }
        // Three parts and the tail, which takes a block of the parts' last: none of the 61 others.
        let kept = allocated(&cache, KEY);
        assert!((3 * PART..4 * PART).contains(&kept), "{kept} bytes allocated for 3 parts");
        // A fourth evicts one of the three: its blocks freed.
        object.keep_part(&cache, 63);
        assert_eq!(allocated(&cache, KEY), kept);
    }

    /// TASK-539.6: a limit the disk governor sets evicts what was read least recently down to it
    /// at once, and keeps what would pass it from being kept; lifted, the cache keeps again. Its
    /// disk counts what it keeps, and stops counting it as it goes.
    #[test]
    fn a_governed_limit_evicts_down_to_it_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let object = Object::noise(KEY, 4 * PART, 1);
        let cache = Cache::open(dir.path(), u64::MAX).unwrap();
        let disk = cache.disk().clone();
        let counted = disk.cache.get() - cache.bytes();
        object.keep(&cache);
        assert_eq!((cache.bytes(), disk.cache.get() - counted), (object.len(), object.len()));
        object.read(&cache, 2);
        object.read(&cache, 3);
        let limit = 2 * PART + object.tail_len();
        assert_eq!(cache.set_limit(limit).unwrap(), limit);
        assert_eq!((object.kept(&cache), cache.capacity()), (vec![2, 3], limit));
        // Kept within it by evicting what was read least recently.
        object.keep_part(&cache, 0);
        assert_eq!((object.kept(&cache), cache.bytes()), (vec![0, 3], limit));
        cache.set_limit(0).unwrap();
        assert_eq!((cache.bytes(), cache.keys(), disk.cache.get() - counted), (0, vec![], 0));
        cache.set_limit(u64::MAX).unwrap();
        object.keep(&cache);
        assert_eq!(object.kept(&cache), [0, 1, 2, 3]);
        drop(cache);
        assert_eq!(disk.cache.get(), counted, "what a cache closed kept counts no more");
    }

    /// TASK-443.8: a fill that can't fit, whatever is evicted, evicts nothing: it emptied the cache
    /// for nothing, as one past a floor the filesystem's free space couldn't meet did. Here the
    /// object filled's own tail can't go while its part is written.
    #[test]
    fn fills_that_cant_fit_evict_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (small, large) = (Object::noise(OTHER, PART / 4, 2), Object::noise(KEY, PART, 1));
        let limit = PART + large.tail_len() - 1;
        let cache = Cache::open(dir.path(), limit).unwrap();
        small.keep(&cache);
        large.keep_tail(&cache);
        large.keep_part(&cache, 0);
        assert_eq!((small.kept(&cache), large.kept(&cache)), (vec![0], vec![]));
        assert_eq!(cache.bytes(), small.len() + large.tail_len());
    }

    /// A part is kept only with its object's tail, where the tail kept says it is stored, and is
    /// held by its object's key and its number (TASK-407).
    #[test]
    fn parts_are_kept_with_their_tails() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path(), 8 * PART).unwrap();
        let object = Object::noise(KEY, 2 * PART, 1);
        object.keep_part(&cache, 0);
        assert_eq!((cache.bytes(), cache.keys()), (0, Vec::<String>::new()));
        object.keep_tail(&cache);
        assert_eq!(cache.tail(KEY).unwrap(), Some(object.tail.clone()));
        assert!(!cache.holds(KEY, 0));
        object.keep_part(&cache, 0);
        assert_eq!(object.read(&cache, 0).as_deref(), Some(object.part(0)));
        assert_eq!(cache.bytes(), PART + object.tail_len());
        // What it holds: part 0 of the object, not part 1, nor any part of another key.
        assert!(cache.holds(KEY, 0));
        assert!(!cache.holds(KEY, 1));
        assert!(!cache.holds("segments/0000000000002/0000000000000", 0));
        cache.check();
    }

    /// A part whose bytes changed behind the cache's back, as a torn write leaves them, fails its
    /// CRC, reads as not kept, and is dropped; a tail so takes its object with it.
    #[test]
    fn parts_failing_their_crcs_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path(), 8 * PART).unwrap();
        let object = Object::noise(KEY, 2 * PART, 1);
        object.keep(&cache);
        flip(&cache.path(KEY), object.stored(1).offset + 7);
        assert_eq!(object.read(&cache, 1), None);
        assert_eq!(cache.bytes(), PART + object.tail_len());
        assert_eq!(object.kept(&cache), [0]);
        flip(&cache.path(KEY), object.tail.parts.frame.tail().start + 1);
        assert_eq!(cache.tail(KEY).unwrap(), None);
        assert_eq!((cache.bytes(), object.read(&cache, 0)), (0, None));
        assert!(!cache.path(KEY).exists());
        cache.check();
    }

    /// An object whose file went, or shrank, behind the cache's back reads as not kept, and is
    /// dropped; one whose file can't be read fails.
    #[test]
    fn parts_gone_read_as_not_kept() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path(), 3 * PART).unwrap();
        let object = Object::noise(KEY, PART + 100, 1);
        object.keep(&cache);
        File::options().write(true).open(cache.path(KEY)).unwrap().set_len(PART).unwrap();
        assert_eq!((object.read(&cache, 1), cache.bytes()), (None, 0));
        object.keep(&cache);
        host_fs::remove_file(cache.path(KEY)).unwrap();
        assert_eq!((object.read(&cache, 1), cache.bytes()), (None, 0));
        object.keep(&cache);
        host_fs::remove_file(cache.path(KEY)).unwrap();
        host_fs::create_dir(cache.path(KEY)).unwrap();
        let err = cache.part(KEY, 1).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EISDIR), "{err}");
    }

    /// TASK-433.4: what the cache keeps is on its disk alone: none of an object's file, its tail
    /// and parts kept, read and kept again, is in the host's page cache, the cache's file system
    /// bypassing it.
    #[cfg(target_os = "linux")]
    #[test]
    fn kept_objects_hold_no_pages_in_the_page_cache() {
        use std::os::fd::AsRawFd as _;
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path(), 8 * PART).unwrap();
        let object = Object::noise(KEY, 2 * PART + 100, 1);
        object.keep(&cache);
        assert_eq!(object.read(&cache, 1).as_deref(), Some(object.part(1)));
        let file = File::open(cache.path(KEY)).unwrap();
        let len = file.metadata().unwrap().len() as usize;
        let page = 4096;
        // SAFETY: Maps the file, unread, to ask the kernel which of its pages it caches, and
        // unmaps it.
        let cached = unsafe {
            let addr = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            );
            assert_ne!(addr, libc::MAP_FAILED, "{}", io::Error::last_os_error());
            let mut pages = vec![0u8; len.div_ceil(page)];
            assert_eq!(libc::mincore(addr, len, pages.as_mut_ptr()), 0);
            libc::munmap(addr, len);
            pages.iter().filter(|page| **page & 1 == 1).count()
        };
        assert_eq!(cached, 0, "of {} pages", len.div_ceil(page));
    }

    /// A part larger than the cache isn't kept, and evicts nothing; one that fits beside its tail
    /// is, evicting what was read before; one kept already is kept once.
    #[test]
    fn keeps_parts_that_fit_once() {
        let dir = tempfile::tempdir().unwrap();
        let (object, other) = (Object::noise(KEY, 1000, 1), Object::noise(OTHER, PART + 100, 2));
        let capacity = 1000 + object.tail_len() + other.tail_len();
        let cache = Cache::open(dir.path(), capacity).unwrap();
        other.keep_tail(&cache);
        other.keep_part(&cache, 1);
        other.keep_part(&cache, 1);
        assert_eq!(cache.bytes(), 100 + other.tail_len());
        other.keep_part(&cache, 0);
        assert_eq!(other.kept(&cache), [1]);
        object.keep(&cache);
        assert_eq!(object.kept(&cache), [0]);
        assert_eq!(cache.tail(OTHER).unwrap(), Some(other.tail.clone()));
        assert_eq!(other.read(&cache, 1), None);
        assert_eq!(cache.bytes(), capacity);
        cache.check();
    }

    /// TASK-92.21: a part dropped whose object's file went meanwhile, as one does that another
    /// put's eviction of the object's last part removes outside the cache's lock, is dropped, its
    /// hole gone with the file; and so is the object's tail.
    #[test]
    fn parts_dropped_whose_files_went_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path(), 3 * PART).unwrap();
        let object = Object::noise(KEY, PART + 100, 1);
        object.keep(&cache);
        host_fs::remove_file(cache.path(KEY)).unwrap();
        cache.forget_part(KEY, 0).unwrap();
        assert_eq!(cache.bytes(), 100 + object.tail_len());
        cache.forget_part(KEY, TAIL).unwrap();
        assert_eq!((cache.bytes(), cache.keys()), (0, Vec::<String>::new()));
        cache.check();
    }

    /// TASK-92.21: keeps, reads and drops of a few objects' tails and parts, in threads at once,
    /// evicting one another's, keep the cache whole: none fails, and, once they are done, what the
    /// cache counts it keeps, each part kept read whole; and a cache opened after it finds the
    /// same.
    #[test]
    fn parts_kept_read_and_dropped_at_once_keep_the_cache_whole() {
        const OBJECTS: u64 = 3;
        const PARTS: u64 = 4;
        let dir = tempfile::tempdir().unwrap();
        let key = |object: u64| {
            format!("segments/0000000000001/{object}", object = base32_id::Padded(object))
        };
        let objects: Arc<Vec<_>> =
            Arc::new((0..OBJECTS).map(|at| Object::noise(&key(at), PARTS * PART, at)).collect());
        let cache = Arc::new(Cache::open(dir.path(), 6 * PART).unwrap());
        let threads: Vec<_> = (0..4u64)
            .map(|thread| {
                let (cache, objects) = (cache.clone(), objects.clone());
                std::thread::spawn(move || {
                    let mut state = 0x9e37_79b9_7f4a_7c15_u64 ^ thread;
                    for _ in 0..500 {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        let object = &objects[(state % OBJECTS) as usize];
                        let part = state / OBJECTS % PARTS;
                        match state / (OBJECTS * PARTS) % 16 {
                            0..=5 => object.keep_part(&cache, part),
                            6..=7 => object.keep_tail(&cache),
                            8..=11 => drop(object.read(&cache, part)),
                            12 => drop(cache.tail(&object.key).unwrap()),
                            13..=14 => cache.forget_part(&object.key, part).unwrap(),
                            _ => cache.forget(&object.key).unwrap(),
                        }
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let read = |cache: &Cache| {
            cache.check();
            let mut kept = 0;
            for object in objects.iter() {
                let Some(tail) = cache.tail(&object.key).unwrap() else { continue };
                assert_eq!(tail, object.tail);
                kept += object.tail_len();
                for part in 0..PARTS {
                    if let Some(bytes) = object.read(cache, part) {
                        assert_eq!(bytes, object.part(part), "part {part} of {}", object.key);
                        kept += PART;
                    }
                }
            }
            assert_eq!(cache.bytes(), kept);
            assert_eq!(cache.keys(), files(dir.path()));
            kept
        };
        let kept = read(&cache);
        assert!(cache.shards[0].kept.lock().unwrap().removing.is_empty());
        drop(cache);
        assert_eq!(read(&Cache::open(dir.path(), 6 * PART).unwrap()), kept);
    }

    /// TASK-92.21: a benchmark of reads of a part kept while others are kept, each evicting one,
    /// in another thread: how long the reads took, the most and in all, which wait for the
    /// cache's lock. `cargo nextest run -p bucket_cell --run-ignored only
    /// bench_reads_while_parts_are_kept --no-capture`.
    #[test]
    #[ignore = "a benchmark (TASK-92.21): --run-ignored only, alone"]
    fn bench_reads_while_parts_are_kept() {
        const PUTS: u64 = 2048;
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(dir.path(), 16 * PART).unwrap());
        let read = Object::noise(KEY, PART, 1);
        read.keep(&cache);
        let objects: Vec<_> = (0..PUTS / 8)
            .map(|at| {
                Object::noise(
                    &format!("segments/0000000000002/{at}", at = base32_id::Padded(at)),
                    8 * PART,
                    at,
                )
            })
            .collect();
        let keeping = {
            let cache = cache.clone();
            std::thread::spawn(move || {
                for number in 0..PUTS {
                    let object = &objects[(number / 8) as usize];
                    if number % 8 == 0 {
                        object.keep_tail(&cache);
                    }
                    object.keep_part(&cache, number % 8);
                }
            })
        };
        let (mut most, mut all, mut reads) =
            (std::time::Duration::ZERO, std::time::Duration::ZERO, 0);
        while !keeping.is_finished() {
            let started = std::time::Instant::now();
            assert_eq!(read.read(&cache, 0).as_deref(), Some(read.part(0)));
            let took = started.elapsed();
            (most, all, reads) = (most.max(took), all + took, reads + 1);
        }
        keeping.join().unwrap();
        println!("{reads} reads while {PUTS} parts were kept: at most {most:?}, {all:?} in all");
    }

    /// TASK-165.4: a cache keeps what an earlier one in its directory kept: its tails, and the
    /// parts it held, counted as they were, but not those it evicted, whose holes it finds.
    #[test]
    fn keeps_what_an_earlier_one_kept() {
        let dir = tempfile::tempdir().unwrap();
        let object = Object::noise(KEY, 4 * PART + 100, 1);
        let other = Object::noise(OTHER, 3000, 2);
        let capacity = 3 * PART + 100 + object.tail_len() + other.len();
        let cache = Cache::open(dir.path(), capacity).unwrap();
        object.keep_tail(&cache);
        for part in [0, 1, 2, 4] {
            object.keep_part(&cache, part);
        }
        object.read(&cache, 0);
        other.keep(&cache);
        // Part 1 goes for it.
        object.keep_part(&cache, 3);
        assert_eq!(object.kept(&cache), [0, 2, 3, 4]);
        let bytes = cache.bytes();
        drop(cache);

        let cache = Cache::open(dir.path(), capacity).unwrap();
        cache.check();
        assert_eq!(cache.bytes(), bytes);
        assert_eq!(object.kept(&cache), [0, 2, 3, 4]);
        assert_eq!(other.kept(&cache), [0]);
        assert_eq!(cache.tail(KEY).unwrap(), Some(object.tail.clone()));
        for part in [0, 2, 3, 4] {
            assert_eq!(object.read(&cache, part).as_deref(), Some(object.part(part)));
        }
        assert_eq!(cache.bytes(), bytes);
    }

    /// TASK-165.4: a cache opened drops the files an earlier one left that hold no object whose
    /// tail passes its checks, as a crash leaves one whose tail was being written, and anything
    /// else in its directory but objects' files; and directories left empty.
    #[test]
    fn earlier_files_failing_their_checks_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let objects: Vec<_> = (0..4)
            .map(|at| {
                Object::noise(
                    &format!("segments/0000000000001/{at}", at = base32_id::Padded(at)),
                    3000,
                    at,
                )
            })
            .collect();
        let cache = Cache::open(dir.path(), 8 * PART).unwrap();
        for object in &objects {
            object.keep(&cache);
        }
        let path = |at: usize| cache.path(&objects[at].key);
        // A byte of its table flipped.
        flip(&path(0), objects[0].tail.parts.frame.table().start);
        // Cut short, longer, a stray file, a link, an empty directory.
        File::options().write(true).open(path(1)).unwrap().set_len(1000).unwrap();
        let longer = objects[2].bytes.len() as u64 + 8;
        File::options().write(true).open(path(2)).unwrap().set_len(longer).unwrap();
        let stray = cache.path("segments/stray");
        host_fs::write(&stray, b"stray").unwrap();
        std::os::unix::fs::symlink(path(3), cache.path("segments/link")).unwrap();
        host_fs::create_dir_all(cache.path("checkpoints/0000000000001")).unwrap();
        drop(cache);

        let cache = Cache::open(dir.path(), 8 * PART).unwrap();
        cache.check();
        assert_eq!(cache.keys(), [objects[3].key.clone()]);
        assert_eq!(files(dir.path()), [objects[3].key.clone()]);
        assert_eq!(cache.bytes(), objects[3].len());
        assert!(!cache.path("checkpoints").exists());
    }

    /// TASK-165.4: a part an earlier cache kept whose bytes changed since, as a write torn by a
    /// crash leaves them, is counted until read: then it fails its CRC, and is dropped.
    #[test]
    fn earlier_parts_failing_their_crcs_are_dropped_as_read() {
        let dir = tempfile::tempdir().unwrap();
        let object = Object::noise(KEY, 2 * PART, 1);
        let cache = Cache::open(dir.path(), 8 * PART).unwrap();
        object.keep(&cache);
        drop(cache);
        flip(&dir.path().join(FILES).join(KEY), object.stored(1).offset + 100);

        let cache = Cache::open(dir.path(), 8 * PART).unwrap();
        assert_eq!(cache.bytes(), object.len());
        assert_eq!(object.read(&cache, 1), None);
        assert_eq!(cache.bytes(), object.len() - PART);
        assert_eq!(object.kept(&cache), [0]);
    }

    /// TASK-165.4: a cache opened with a smaller budget than an earlier one evicts what that one
    /// kept, by when each object was written last, until what is left fits; limited to nothing,
    /// it evicts all.
    #[test]
    fn budgets_and_limits_hold_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let (object, other) = (Object::noise(KEY, 2 * PART, 1), Object::noise(OTHER, PART, 2));
        let cache = Cache::open(dir.path(), 8 * PART).unwrap();
        object.keep(&cache);
        other.keep(&cache);
        let (path, written) = (cache.path(OTHER), SystemTime::now());
        drop(cache);
        // The other written after it, as its file says.
        let file = File::options().write(true).open(&path).unwrap();
        file.set_modified(written + std::time::Duration::from_secs(10)).unwrap();

        let capacity = PART + object.tail_len() + other.len();
        let cache = Cache::open(dir.path(), capacity).unwrap();
        cache.check();
        assert!(cache.bytes() <= capacity);
        assert_eq!((object.kept(&cache), other.kept(&cache)), (vec![1], vec![0]));
        drop(cache);

        let cache = Cache::open(dir.path(), capacity).unwrap();
        cache.set_limit(0).unwrap();
        assert_eq!((cache.bytes(), cache.keys(), files(dir.path())), (0, vec![], vec![]));
    }

    /// board 14702: a cache opened reads its files' tails, however many parts they keep, and none
    /// of the parts: a host's restart with hundreds of GiB cached reads none of them, each told by
    /// its CRC as it is first read instead. A part an earlier cache evicted, its hole found, is
    /// not counted; those whose bytes changed since, all the others here, are, until read.
    #[test]
    fn opening_reads_tails_not_parts() {
        let dir = tempfile::tempdir().unwrap();
        let objects: Vec<_> = (0..8)
            .map(|at| {
                let key = format!("segments/0000000000001/{at}", at = base32_id::Padded(at));
                Object::noise(&key, 4 * PART, at)
            })
            .collect();
        let cache = Cache::open(dir.path(), u64::MAX).unwrap();
        for object in &objects {
            object.keep(&cache);
            cache.forget_part(&object.key, 0).unwrap();
        }
        drop(cache);
        for object in &objects {
            for part in 1..object.parts() {
                flip(&dir.path().join(FILES).join(&object.key), object.stored(part).offset + 100);
            }
        }

        let cache = Cache::open(dir.path(), u64::MAX).unwrap();
        let tails = objects.iter().map(|object| TRAILER + object.tail_len()).sum::<u64>();
        assert_eq!(cache.shards[0].files.bytes_read.load(Ordering::Relaxed), tails);
        let counted = |object: &Object| {
            let parts = (1..object.parts()).map(|part| object.stored(part).len);
            object.tail_len() + parts.sum::<u64>()
        };
        assert_eq!(cache.bytes(), objects.iter().map(counted).sum::<u64>());
        for object in &objects {
            assert_eq!(object.kept(&cache), Vec::<u64>::new(), "{}", object.key);
        }
        let tails = objects.iter().map(Object::tail_len).sum::<u64>();
        assert_eq!(cache.bytes(), tails);
        cache.check();
    }

    /// board 14702: why a cache opened counts parts it doesn't read, which may be none: the
    /// filesystems it runs on find the holes a punch leaves, as of a part evicted, but APFS fills
    /// with zeros those near a write, of ranges never written.
    #[test]
    fn filesystems_find_punched_holes_but_apfs_fills_unwritten_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        let mut options = host_fs::OpenOptions::new();
        let file = options.create(true).truncate(true).read(true).write(true).open(&path).unwrap();
        file.set_len(4 * PART).unwrap();
        file.write_all_at(&[1; PART as usize], PART).unwrap();
        file.write_all_at(&[1; PART as usize], 3 * PART).unwrap();
        punch(&file, 3 * PART, PART).unwrap();
        let data = data(&file, 4 * PART).unwrap();
        match cfg!(target_os = "macos") {
            true => assert_eq!(data, [0..3 * PART]),
            false => assert_eq!(data, [PART..2 * PART]),
        }
    }

    /// TASK-433.6: a cache on a file system that can't bypass the host's page cache, as the
    /// failpoint makes every one, fails to open, naming its directory, and leaves no file of its
    /// probe there, rather than keep objects in the host's page cache, a memory no budget counts.
    #[test]
    fn a_cache_whose_file_system_cant_bypass_the_page_cache_fails_to_open() {
        let dir = tempfile::tempdir().unwrap();
        let scenario = fail::FailScenario::setup();
        fail::cfg("direct::bypass", "return").unwrap();
        let err = Cache::open(dir.path(), 8 * PART).err().unwrap();
        scenario.teardown();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
        let refused = "its file system can't bypass the host's page cache (O_DIRECT, F_NOCACHE)";
        let prefix = format!("{}: {refused}", dir.path().join(FILES).display());
        assert!(err.to_string().starts_with(&prefix), "{err}");
        assert_eq!(files(dir.path()), Vec::<String>::new());
    }

    /// TASK-165.4: one cache at a time has a directory; another may once it is dropped.
    #[test]
    fn one_cache_has_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path(), PART).unwrap();
        let err = Cache::open(dir.path(), PART).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock, "{err}");
        drop(cache);
        Cache::open(dir.path(), PART).unwrap();
    }

    /// TASK-165.4: an object adopted from a file on the cache's filesystem, as a WAL segment
    /// acknowledged is, is moved in whole, and kept as fetched parts are: read back, evicted as
    /// they are, and kept by the cache opened next.
    #[test]
    fn adopts_whole_objects() {
        let dir = tempfile::tempdir().unwrap();
        let object = Object::noise(KEY, 2 * PART + 100, 1);
        let other = Object::noise(OTHER, PART, 2);
        let cache = Cache::open(dir.path(), object.len() + other.len() - PART).unwrap();
        let from = object.write_to(dir.path());
        assert!(cache.adopt(KEY, &from).unwrap());
        assert!(!from.exists());
        assert_eq!(cache.bytes(), object.len());
        assert_eq!(object.kept(&cache), [0, 1, 2]);
        object.read(&cache, 1);
        object.read(&cache, 2);
        // Part 0 was read least recently.
        other.keep(&cache);
        assert_eq!((object.kept(&cache), other.kept(&cache)), (vec![1, 2], vec![0]));
        cache.check();
        drop(cache);
        let cache = Cache::open(dir.path(), 8 * PART).unwrap();
        assert_eq!((object.kept(&cache), other.kept(&cache)), (vec![1, 2], vec![0]));
    }

    /// TASK-165.4: an object adopted replaces what is kept at its key, and is counted once.
    #[test]
    fn adoptions_replace_what_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path(), 8 * PART).unwrap();
        let object = Object::noise(KEY, 2 * PART, 1);
        object.keep_tail(&cache);
        object.keep_part(&cache, 1);
        assert!(cache.adopt(KEY, &object.write_to(dir.path())).unwrap());
        assert_eq!((cache.bytes(), object.kept(&cache)), (object.len(), vec![0, 1]));
        cache.check();
    }

    /// TASK-165.4: an object that can't fit, past the budget or the governed limit, isn't adopted:
    /// its file is left where it was. One holding no object fails.
    #[test]
    fn adoptions_past_the_limit_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let object = Object::noise(KEY, 2 * PART, 1);
        let other = Object::noise(OTHER, 100, 2);
        let cache = Cache::open(dir.path(), object.len() - 1).unwrap();
        other.keep(&cache);
        let from = object.write_to(dir.path());
        assert!(!cache.adopt(KEY, &from).unwrap());
        assert_eq!((cache.bytes(), other.kept(&cache)), (other.len(), vec![0]));
        drop(cache);
        let cache = Cache::open(dir.path(), 8 * PART).unwrap();
        cache.set_limit(0).unwrap();
        assert!(!cache.adopt(KEY, &from).unwrap());
        assert_eq!((host_fs::read(&from).unwrap(), cache.bytes()), (object.bytes.clone(), 0));
        assert_eq!(cache.keys(), Vec::<String>::new());

        drop(cache);
        let cache = Cache::open(dir.path(), 8 * PART).unwrap();
        host_fs::write(&from, b"no object").unwrap();
        let err = cache.adopt(KEY, &from).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        assert!(from.exists());
    }

    /// How a crash test cuts the change it crashes at, if a write or a punch: what of it is made.
    #[derive(Clone, Copy, Debug)]
    enum Tear {
        Nothing,
        FirstByte,
        Half,
        AllButLastByte,
        /// Every other block, by the file's, as writes back reordered leave them.
        OddBlocks,
    }

    const TEARS: [Tear; 5] =
        [Tear::Nothing, Tear::FirstByte, Tear::Half, Tear::AllButLastByte, Tear::OddBlocks];

    impl Tear {
        fn pieces(self, range: Range<u64>) -> Vec<Range<u64>> {
            const BLOCK: u64 = 4096;
            let Range { start, end } = range;
            let prefix = |len: u64| {
                let piece = start..start + len;
                vec![piece]
            };
            match self {
                Tear::Nothing => vec![],
                Tear::FirstByte => prefix(1.min(end - start)),
                Tear::Half => prefix((end - start) / 2),
                Tear::AllButLastByte => prefix((end - start).saturating_sub(1)),
                Tear::OddBlocks => (start / BLOCK..end.div_ceil(BLOCK))
                    .filter(|block| block % 2 == 1)
                    .map(|block| (block * BLOCK).max(start)..((block + 1) * BLOCK).min(end))
                    .collect(),
            }
        }
    }

    /// Where a crash test cuts the changes a cache makes to its files ([`Files`]), numbered as
    /// made: those before `at` made, but `lost`, a write or a punch a power cut lost while later
    /// ones landed, as nothing that syncs orders them; change `at` torn as `tear` says, if a write
    /// or a punch, else not made; none after it.
    #[derive(Debug)]
    pub(super) struct Cut {
        at: usize,
        tear: Tear,
        lost: Option<usize>,
        made: Mutex<Vec<Change>>,
    }

    impl Cut {
        fn new(at: usize, tear: Tear, lost: Option<usize>) -> Arc<Self> {
            Arc::new(Self { at, tear, lost, made: Mutex::default() })
        }

        /// Counts `change` made, or not: its number.
        fn next(&self, change: Change) -> usize {
            let mut made = self.made.lock().unwrap();
            made.push(change);
            made.len() - 1
        }

        pub(super) fn done(&self, change: Change) -> bool {
            let number = self.next(change);
            number < self.at && Some(number) != self.lost
        }

        pub(super) fn pieces(&self, change: Change, range: Range<u64>) -> Vec<Range<u64>> {
            let number = self.next(change);
            match number.cmp(&self.at) {
                std::cmp::Ordering::Less if Some(number) != self.lost => vec![range],
                std::cmp::Ordering::Equal => self.tear.pieces(range),
                _ => vec![],
            }
        }
    }

    /// The crash test's objects: one of two parts, its last short; one of a part shorter than a
    /// block; one of a third of a part; one whose part compresses; and one adopted, of two parts.
    fn crash_objects() -> [Object; 5] {
        // Framed once: each framing encrypts under a salt of its own (TASK-411.28), and the crash
        // test's workload and its checks must see the same bytes.
        static OBJECTS: std::sync::LazyLock<[Object; 5]> = std::sync::LazyLock::new(|| {
            let pattern: Vec<u8> = (0..300_000u32).map(|at| (at % 251) as u8).collect();
            [
                Object::noise(KEY, PART + 3000, 1),
                Object::noise(OTHER, 3000, 2),
                Object::noise("segments/0000000000001/0000000000004", 300_000, 3),
                Object::new("checkpoints/0000000000001/0000000000004", &pattern),
                Object::noise("segments/0000000000002/0000000000001", PART + 100, 4),
            ]
        });
        OBJECTS.clone()
    }

    /// The crash test's capacity: the first object, and a part's third, so that what it keeps
    /// evicts parts, tails and objects.
    const CRASH_CAPACITY: u64 = PART + 300_000;

    /// What the crash test does with a cache in `dir`, its changes cut by `cut`: keeps objects and
    /// reads them, evicting parts and objects to stay within its capacity, forgets one, and adopts
    /// another from a file in `wal`.
    fn crash_workload(dir: &Path, wal: &Path, cut: Arc<Cut>) {
        let [a, b, c, d, e] = crash_objects();
        let mut cache = Cache::open(dir, CRASH_CAPACITY).unwrap();
        cache.shards[0].files.cut = Some(cut);
        // A crash past a change makes none after it: what fails then is not the cache's.
        let keep = |object: &Object, parts: &[u64]| {
            let _ = cache.keep_tail(&object.key, &object.tail);
            for &part in parts {
                let _ = cache.keep_part(&object.key, part, object.part(part));
            }
        };
        keep(&a, &[0, 1]);
        keep(&b, &[0]);
        let _ = cache.part(&a.key, 0);
        // Evicts a's part 1, b, and a's part 0.
        keep(&c, &[0]);
        let _ = cache.forget(&a.key);
        keep(&d, &[0]);
        // Evicts c's part.
        let _ = cache.adopt(&e.key, &e.write_to(wal));
        keep(&b, &[0]);
        let _ = cache.tail(&e.key);
        let _ = cache.part(&e.key, 1);
    }

    /// Checks the cache in `dir` opened after a crash: its index whole, within its capacity; no
    /// file but its objects'; and everything it keeps read back as kept, or dropped. Then a reader
    /// of all keeps all anew, as in a cache never crashed, which one opened after it finds.
    fn check_after_crash(dir: &Path, crashed: &str) {
        let objects = crash_objects();
        let cache = Cache::open(dir, CRASH_CAPACITY).unwrap();
        cache.check();
        assert!(cache.bytes() <= CRASH_CAPACITY, "{crashed}: {} bytes", cache.bytes());
        assert_eq!(cache.keys(), files(dir), "{crashed}");
        let mut read = 0;
        for object in &objects {
            let Some(tail) = cache.tail(&object.key).unwrap() else { continue };
            assert_eq!(tail, object.tail, "{crashed}: {}", object.key);
            read += object.tail_len();
            for part in 0..object.parts() {
                let Some(bytes) = object.read(&cache, part) else { continue };
                assert_eq!(bytes, object.part(part), "{crashed}: part {part} of {}", object.key);
                read += object.stored(part).len;
            }
        }
        assert_eq!(cache.bytes(), read, "{crashed}");
        cache.check();
        drop(cache);

        let cache = Cache::open(dir, 8 * PART).unwrap();
        for object in &objects {
            object.fetch(&cache);
        }
        drop(cache);
        let cache = Cache::open(dir, 8 * PART).unwrap();
        for object in &objects {
            assert_eq!(object.kept(&cache), Vec::from_iter(0..object.parts()), "{crashed}");
        }
        assert_eq!(cache.bytes(), objects.iter().map(Object::len).sum::<u64>(), "{crashed}");
    }

    /// TASK-165.4: a crash at any change the cache makes to its files leaves nothing a cache
    /// opened after it reads back wrong: each change in turn the one cut, not made, or a write or
    /// a punch torn (its first byte, half, all but its last byte, every other block), the changes
    /// after it never made; and, as a power cut leaves them, each write or punch lost with those
    /// after it made, the next one or all. What a cache opened then keeps reads
    /// back as kept, or fails its check and is dropped; it holds no file but its objects', counts
    /// what it keeps, and keeps all anew.
    #[test]
    fn crashes_at_each_change_leave_nothing_read_back_wrong() {
        let changes = {
            let dir = tempfile::tempdir().unwrap();
            let cut = Cut::new(usize::MAX, Tear::Nothing, None);
            crash_workload(dir.path(), dir.path(), cut.clone());
            check_after_crash(dir.path(), "uncut");
            cut.made.lock().unwrap().clone()
        };
        // It makes each kind of change.
        for change in [Change::Create, Change::Resize, Change::Write, Change::Punch] {
            assert!(changes.contains(&change), "{change:?} in {changes:?}");
        }
        assert!(changes.contains(&Change::Remove) && changes.contains(&Change::Rename));
        let mut cuts = Vec::new();
        for (at, change) in changes.iter().enumerate() {
            match change {
                Change::Write | Change::Punch => cuts.extend(TEARS.map(|tear| (at, tear, None))),
                _ => cuts.push((at, Tear::Nothing, None)),
            }
        }
        for (lost, change) in changes.iter().enumerate() {
            if matches!(change, Change::Write | Change::Punch) {
                if lost + 2 < changes.len() {
                    cuts.push((lost + 2, Tear::Nothing, Some(lost)));
                }
                cuts.push((changes.len(), Tear::Nothing, Some(lost)));
            }
        }
        // Each in a directory of its own, a few at once.
        let threads = std::thread::available_parallelism().map_or(4, usize::from);
        let cuts = Mutex::new(cuts);
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    loop {
                        let Some((at, tear, lost)) = cuts.lock().unwrap().pop() else { break };
                        let crashed =
                            format!("cut at {at} of {changes:?}, {tear:?}, losing {lost:?}");
                        let dir = tempfile::tempdir().unwrap();
                        crash_workload(dir.path(), dir.path(), Cut::new(at, tear, lost));
                        check_after_crash(dir.path(), &crashed);
                    }
                });
            }
        });
    }

    #[test]
    fn jbod_opens_multiple_shards_spreads_and_reopens() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let dirs = [dir1.path().to_path_buf(), dir2.path().to_path_buf()];
        let objects: Vec<Object> = (0..12)
            .map(|i| Object::noise(&format!("segments/0000000000001/{i:013}"), PART, i + 10))
            .collect();
        let cap = 32 * PART;
        let cache = Cache::open_all(&dirs, cap).unwrap();
        assert_eq!(cache.dirs().len(), 2);
        assert!(dir1.path().join(".disk-id").exists());
        assert!(dir2.path().join(".disk-id").exists());
        for obj in &objects {
            obj.keep(&cache);
        }
        cache.check();
        let s0 = cache.shards[0].bytes();
        let s1 = cache.shards[1].bytes();
        assert!(s0 > 0 && s1 > 0, "objects should spread across both shards: s0={s0}, s1={s1}");
        assert_eq!(cache.bytes(), s0 + s1);
        drop(cache);

        let reopened = Cache::open_all(&dirs, cap).unwrap();
        reopened.check();
        assert_eq!(reopened.bytes(), s0 + s1);
        for obj in &objects {
            assert_eq!(obj.read(&reopened, 0).as_deref(), Some(obj.part(0)));
        }
    }

    #[test]
    fn jbod_refuses_duplicate_directories() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = [dir.path().to_path_buf(), dir.path().to_path_buf()];
        let err = Cache::open_all(&dirs, 4 * PART).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn jbod_adopt_routes_and_forgets_superseded_copy() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let wal = tempfile::tempdir().unwrap();
        let dirs = [dir1.path().to_path_buf(), dir2.path().to_path_buf()];
        let cache = Cache::open_all(&dirs, 8 * PART).unwrap();
        let eio = io::Error::from_raw_os_error(libc::EIO);

        // Determine which shard weighted rendezvous prefers for KEY, mark it offline so `first`
        // lands on the OTHER shard, then bring it back online so `adopt` picks the primary shard
        // and genuinely exercises cross-shard forgetting of the superseded copy.
        let primary = cache.ranked_shards(KEY, false)[0];
        let secondary = 1 - primary;
        for _ in 0..OFFLINE_ERRORS {
            cache.shards[primary].health.on_io_error(dirs[primary].as_path(), &eio);
        }
        assert!(cache.shards[primary].health.is_offline());

        let first = Object::noise(KEY, PART, 1);
        first.keep(&cache);
        assert!(cache.shards[secondary].has_key(KEY));
        assert!(!cache.shards[primary].has_key(KEY));

        for _ in 0..RETURNING_SUCCESSES {
            cache.shards[primary].health.on_io_success();
        }
        assert!(cache.shards[primary].health.is_online());

        let second = Object::noise(KEY, PART, 2);
        let from = second.write_to(wal.path());
        assert!(cache.adopt(KEY, &from).unwrap());
        cache.check();
        assert!(cache.shards[primary].has_key(KEY));
        assert!(!cache.shards[secondary].has_key(KEY));
        assert_eq!(cache.shards[secondary].bytes(), 0);
        assert_eq!(cache.keys(), vec![KEY.to_owned()]);
        assert_eq!(second.read(&cache, 0).as_deref(), Some(second.part(0)));

        // Adopting onto primary while secondary is Offline must forget in memory without touching
        // disk, queuing the unlink until secondary recovers.
        for _ in 0..OFFLINE_ERRORS {
            cache.shards[primary].health.on_io_error(dirs[primary].as_path(), &eio);
        }
        let third = Object::noise(OTHER, PART, 3);
        third.keep(&cache);
        assert!(cache.shards[secondary].has_key(OTHER));
        for _ in 0..RETURNING_SUCCESSES {
            cache.shards[primary].health.on_io_success();
        }
        for _ in 0..OFFLINE_ERRORS {
            cache.shards[secondary].health.on_io_error(dirs[secondary].as_path(), &eio);
        }
        let fourth = Object::noise(OTHER, PART, 4);
        let from2 = fourth.write_to(wal.path());
        assert!(cache.adopt(OTHER, &from2).unwrap());
        assert!(cache.shards[primary].has_key(OTHER));
        assert!(!cache.shards[secondary].has_key(OTHER));
        assert_eq!(cache.shards[secondary].pending_forgets.lock().unwrap().len(), 1);
        assert_eq!(fourth.read(&cache, 0).as_deref(), Some(fourth.part(0)));
    }

    #[test]
    fn jbod_health_transitions_failover_and_recovery() {
        let state = ShardHealthState::new();
        let eio = io::Error::from_raw_os_error(libc::EIO);
        let dummy = Path::new("/tmp/shard");
        assert_eq!(state.get(), ShardHealth::Online);
        state.on_io_error(dummy, &eio);
        state.on_io_error(dummy, &eio);
        assert_eq!(state.get(), ShardHealth::Online);
        state.on_io_error(dummy, &eio);
        assert_eq!(state.get(), ShardHealth::Suspect);
        for _ in 0..7 {
            state.on_io_error(dummy, &eio);
        }
        assert_eq!(state.get(), ShardHealth::Offline);
        state.on_io_success();
        assert_eq!(state.get(), ShardHealth::Returning);
        // InvalidData probe mismatch during Returning must demote back to Offline and reset
        // probe_successes.
        let mismatch = io::Error::new(io::ErrorKind::InvalidData, "probe mismatch");
        state.on_probe_error(dummy, &mismatch);
        assert_eq!(state.get(), ShardHealth::Offline);
        state.on_io_success();
        assert_eq!(state.get(), ShardHealth::Returning);
        state.on_io_success();
        assert_eq!(state.get(), ShardHealth::Returning);
        state.on_io_success();
        assert_eq!(state.get(), ShardHealth::Online);

        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let dirs = [dir1.path().to_path_buf(), dir2.path().to_path_buf()];
        let cache = Cache::open_all(&dirs, 8 * PART).unwrap();
        for _ in 0..OFFLINE_ERRORS {
            cache.shards[0].health.on_io_error(dir1.path(), &eio);
        }
        assert!(cache.shards[0].health.is_offline());
        for i in 0..6u64 {
            let obj = Object::noise(&format!("segments/0000000000002/{i:013}"), PART, i + 100);
            obj.keep(&cache);
            assert_eq!(obj.read(&cache, 0).as_deref(), Some(obj.part(0)));
        }
        assert_eq!(cache.shards[0].bytes(), 0, "offline shard must not receive new fills");
        assert!(cache.shards[1].bytes() > 0, "healthy shard must absorb failover fills");
        cache.check();
    }

    #[test]
    fn jbod_corrupt_disk_id_regenerates_without_failing_open() {
        let dir = tempfile::tempdir().unwrap();
        let disk_id_path = dir.path().join(DISK_ID);

        // Torn / non-UTF-8 bytes after a crash must regenerate `.disk-id` instead of failing open
        // (board 12102).
        for corrupt in [b"\xff\xfe\x00\x80".as_slice(), b"0\n".as_slice(), b"garbage\n".as_slice()]
        {
            host_fs::write(&disk_id_path, corrupt).unwrap();
            let cache = Cache::open(dir.path(), 4 * PART).unwrap();
            let id = cache.shards[0].disk_id;
            assert_ne!(id, 0);
            assert_eq!(
                host_fs::read_to_string(&disk_id_path).unwrap().trim().parse::<u64>().unwrap(),
                id
            );
            drop(cache);
            let reopened = Cache::open(dir.path(), 4 * PART).unwrap();
            assert_eq!(reopened.shards[0].disk_id, id, "regenerated disk id must persist");
        }

        // Real I/O errors (such as EISDIR) must still propagate rather than being masked.
        host_fs::remove_file(&disk_id_path).unwrap();
        host_fs::create_dir(&disk_id_path).unwrap();
        let err = match Cache::open(dir.path(), 4 * PART) {
            Ok(_) => panic!("opening cache with directory at .disk-id must fail"),
            Err(err) => err,
        };
        assert_eq!(err.raw_os_error(), Some(libc::EISDIR), "{err}");
    }
}
