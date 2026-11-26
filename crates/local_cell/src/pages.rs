// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the cell's page cache (TASK-433): what holds the bytes its data files are written with
//! until they are on the host's disk, and for as long after as memory allows, in memory of the
//! process's own, which the extent daemon governs with the rest (`cladefs::cladefsd::memory`). Its
//! files bypass the host's page cache (`O_DIRECT`), so that no byte a cell holds is held twice,
//! in two memories each budgeted as if the other weren't there. It does what the host's did for
//! them, as Linux's does (mm/filemap.c, mm/page-writeback.c, fs/fs-writeback.c):
//!
//! - **A write lands in memory and returns** ([`Cached::write_at`]): its pages are dirty, and
//!   the disk takes them later, coalesced. Reads see them at once ([`Cached::read_at`]); a read of
//!   what the cache holds not goes to the file, and leaves nothing cached: what is read again, the
//!   readers' own caches hold (the extent cache), so that nothing is held twice here either.
//! - **Write-back** goes in the file's order, contiguous dirty pages one write of up to [`CHUNK`],
//!   from memory aligned to blocks, as iomap's ioends coalesce. One write-back of a file at a time
//!   (`I_SYNC`): a sync, the flusher and a writer writing back take turns, each waiting for the
//!   last to end, which carries what it covers ([`Shared::writeback`]). A page written while its
//!   write-back is in flight is dirty again, and written again after: writers never wait on
//!   write-back (no stable pages). The flusher ([`PageCache`]'s thread) writes back first what a
//!   writer asked it to begin ([`Cached::write_behind`], as `sync_file_range`'s
//!   `SYNC_FILE_RANGE_WRITE` begins it), then what has been dirty longer than [`Options::expire`],
//!   and the oldest first while more than [`Options::background`] is dirty. A cache with no flusher
//!   ([`PageCache::without_flusher`]), as a directory whose I/O is all its callers' has
//!   ([`crate::Dir::concurrent`]), has the writer that takes it past [`Options::background`] write
//!   back itself, as Linux's writers did before per-device flushers: what it writes, and when, is
//!   then a function of the writes alone.
//! - **Writers are throttled**, not refused, past [`Options::hard`] dirty: they wait for
//!   write-back, as `balance_dirty_pages` has them. cladefs: a file whose write-back is failing,
//!   as a full or broken disk fails it, holds its dirty pages, which no wait writes back: they
//!   are left out of what other files' writers wait on ([`PageCache::throttle_for`]), and the writers
//!   held at the bound of it, or of any file of its disk, get its failure, never another disk's
//!   (board 8553): so that what is stuck grows past the bound by a bound's worth a disk at most.
//! - **A sync** ([`Cached::sync`]) writes back what was dirty as it began, waits for every write of
//!   the file in flight, the flusher's too, then syncs the file: what it covers is a snapshot, so
//!   that a writer going on never keeps it from ending (`PAGECACHE_TAG_TOWRITE`). cladefs: a
//!   write-back that failed leaves its pages dirty, which the sync after it writes back again: the
//!   sync fails only if that does (TASK-539.12). Linux's reports it once, errseq's, as it drops the
//!   pages it failed to write; before TASK-539.12 this did too ([`Pages::failed`]), which failed a
//!   cell's flush, and the cell, for a moment of a full disk long gone.
//! - **Eviction** takes clean pages alone, of the files used least recently first
//!   ([`PageCache::evict_to`]), never writing any: what is dirty is the flusher's to write.
//! - **Files are ordered by their keys as they were once** ([`PageCache::files_by`]): when they
//!   were used, when they were dirtied, which other threads change as the cache goes through them,
//!   so that no comparison sees another's file changed since (TASK-493).
//! - **The flusher's work is the process's** ([`FLUSHER`]): every cell's files wait on it, so a
//!   panic on it, which no cell's halt contains, aborts the extent daemon as one holding a lock
//!   shared beyond one cell does ([`crate::shared`], TASK-149.24). Elsewhere, as in tests, its
//!   death is said ([`PageCache::flusher_died`]), and a writer it would have let go panics rather
//!   than wait for good.
//! - **A file removed** keeps its pages while it is open, as an unlinked inode does, and drops
//!   them with its last opening, the dirty ones unwritten (`truncate_inode_pages` at the last
//!   `iput`): a segment's raw file, framed and gone, never reaches the disk but what its syncs
//!   and the flusher took.
//!
//! Every directory a cell opens holds its data files in one ([`crate::CachedDir`]): the host's
//! ([`crate::HostDir`]) in the process's ([`shared`]), and one in memory ([`crate::MemDir`]) in a
//! small one of its own with no flusher, so that the cell's tests, its crash rounds and its
//! Shuttle schedules among them, run through it as the cell does on a host.

use crate::dir::File;
use crate::frames::{Frame, Frames};
#[cfg(not(madsim))]
use crate::shared::Holding;
use crate::sync::{Condvar, Mutex, MutexGuard};
#[cfg(feature = "shuttle")]
use shuttle::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
#[cfg(not(feature = "shuttle"))]
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

/// The cache's page: what it holds, writes back and evicts. A cell's data files are written in
/// whole sectors of this size (TASK-411.8), so a write fills its pages whole but at a file's end.
pub const PAGE: u64 = 4096;

/// The most a write-back writes at once: contiguous dirty pages, one write.
pub const CHUNK: u64 = 1 << 20;

/// What a [`PageCache`]'s flusher holds for its life, as a lock shared beyond one cell
/// ([`Holding::work`]): what the extent daemon's log names as it aborts for a panic on it.
pub const FLUSHER: &str = "page cache flusher";

/// When a [`PageCache`] writes back, and when it holds writers back.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Dirty bytes past which the flusher writes back, the oldest files' first, writers going on.
    pub background: u64,
    /// Dirty bytes past which a writer waits for write-back before it dirties more.
    pub hard: u64,
    /// How long a page stays dirty before the flusher writes it back, however few are dirty.
    pub expire: Duration,
    /// The most bytes the cache holds, clean ones evicted past it, unless a governor holds it to
    /// less ([`PageCache::evict_to`]).
    pub most: u64,
}

/// Bounds that hold the cache to what a process holds whatever its bucket's backlog (TASK-317):
/// the backlog is on the disk, what the cache holds a bounded window of it, the latest. What a
/// process with no governor goes by, and the least a governed one does ([`PageCache::size_to`]).
impl Default for Options {
    fn default() -> Self {
        Self {
            background: 32 << 20,
            hard: 128 << 20,
            expire: Duration::from_secs(1),
            most: 256 << 20,
        }
    }
}

/// What a [`PageCache`] holds, by state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// Bytes of every page held.
    pub held: u64,
    /// Bytes of pages dirty: written, not yet on the disk.
    pub dirty: u64,
    /// Bytes of pages being written back.
    pub writing: u64,
    /// The most bytes held at once so far.
    pub peak: u64,
    /// The most bytes dirty at once so far.
    pub peak_dirty: u64,
    /// cladefs: the writers held back so far, past [`Options::hard`] dirty, waiting for write-back
    /// ([`PageCache::throttle_for`]): flush stalls (board 8565).
    pub stalls: u64,
    /// cladefs: how long those writers were held, their nanoseconds together.
    pub stalled_ns: u64,
    /// cladefs: bytes dirty of files whose write-back is failing, which no wait writes back
    /// (board 8553).
    pub stuck: u64,
}

/// Key identifying a per-disk flusher in [`PageCache`]: physical filesystems share one stable
/// flusher by `st_dev` (so recreating a [`HostDisk`] for the same mount never spawns extra
/// threads), while virtual/in-memory disks get their own flusher that exits once the disk drops.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum DiskKey {
    Default,
    Device(u64),
    Virtual(usize),
}

impl DiskKey {
    fn of(disk: Option<&Arc<bucket_cell::host_disk::HostDisk>>) -> Self {
        match disk {
            None => Self::Default,
            Some(d) => match d.dev() {
                Some(dev) => Self::Device(dev),
                None => Self::Virtual(Arc::as_ptr(d) as usize),
            },
        }
    }
}

/// Per-`HostDisk` flusher wakeup state, write-behind queue, and per-disk dirty/stuck page counts,
/// so each physical JBOD disk flushes and throttles independently without one slow or stalled disk
/// blocking write-back or exhausting the dirty-page budget for healthy disks (board 11939).
#[derive(Default)]
struct DiskFlusher {
    /// The ranges of files on this disk writers asked to be written back, waiting for none of it,
    /// in the order asked: this disk's flusher writes these back first ([`Cached::write_behind`]).
    begun: Mutex<VecDeque<(Weak<dyn Paged>, Range<u64>)>>,
    /// Wakes this disk's flusher.
    flush: Condvar,
    flushing: Mutex<bool>,
    /// Bytes currently dirty on this disk.
    dirty: AtomicU64,
    /// Of `dirty`, bytes held by files whose write-back is failing.
    stuck: AtomicU64,
    /// Live [`Shared`] files currently bound to this disk; `0 -> 1` and `1 -> 0` transitions
    /// maintain [`PageCache::active_disks`] so only disks with open files share the dirty budget.
    open_files: AtomicU64,
    /// Writers currently waiting in [`PageCache::throttle_for`] on this disk.
    #[cfg(test)]
    waiters: AtomicU64,
    /// Weak handle to the [`HostDisk`] for this flusher (used both to reap virtual flushers once
    /// dropped and to read any explicit per-disk `dirty_budget` override).
    disk: Option<Weak<bucket_cell::host_disk::HostDisk>>,
}

impl DiskFlusher {
    fn wake(&self) {
        *self.flushing.lock().unwrap() = true;
        self.flush.notify_one();
    }
}

/// A process's page cache: its files' pages, accounted together, written back by one flusher
/// thread per [`HostDisk`] (or by its writers if it has none), evicted together. One for the
/// process ([`shared`]), which the extent daemon governs.
pub struct PageCache {
    /// What it was made with: its expiry, and the least of its bounds ([`PageCache::size_to`]).
    options: Options,
    /// Its bounds as they are: [`Options::background`], [`Options::hard`] and [`Options::most`],
    /// raised from `options`' by a governor as memory allows ([`PageCache::size_to`]).
    background: AtomicU64,
    hard: AtomicU64,
    most: AtomicU64,
    /// Whether flusher threads write back for it; if not, its writers do.
    flusher: bool,
    /// Whether any of its flushers died, a panic unwinding it: nothing writes back for its disk
    /// since.
    flusher_died: AtomicBool,
    held: AtomicU64,
    peak: AtomicU64,
    dirty: AtomicU64,
    peak_dirty: AtomicU64,
    writing: AtomicU64,
    /// cladefs: [`Usage::stalls`] and [`Usage::stalled_ns`].
    stalls: AtomicU64,
    stalled_ns: AtomicU64,
    /// Each write's number, in order: what a sync covers, the writes before it.
    writes: AtomicU64,
    /// Each use's number, which says which files were used least recently.
    uses: AtomicU64,
    files: Mutex<Vec<Weak<dyn Paged>>>,
    /// Per-disk flushers, keyed by [`DiskKey`].
    flushers: Mutex<HashMap<DiskKey, Arc<DiskFlusher>>>,
    /// Number of disks (including [`DiskKey::Default`]) with at least one open file, used lock-free
    /// to divide `background` and `hard` into per-disk dirty budgets (board 11939).
    active_disks: AtomicU64,
    /// Wakes writers held back.
    drained: Condvar,
    /// What writers held back wait on `drained` with.
    holding: Mutex<()>,
    /// The bytes dirty of files whose write-back is failing ([`Pages::failing`]): what no wait
    /// of a writer's writes back until their disk takes them again (board 8553).
    stuck: AtomicU64,
    /// The disks with files whose write-back is failing, by their address: how many, and the
    /// last failure, which every writer of the disk held at the bound fails with. Taken after a
    /// file's pages' lock.
    failing_disks: Mutex<HashMap<usize, (usize, io::Error)>>,
    /// cladefs: what its pages are held in: frames of slabs of its own, those of pages gone
    /// holding the next (TASK-433.9).
    frames: Arc<Frames>,
}

/// The process's default host's page cache outside `madsim` ([`crate::Host::pages`]).
#[cfg(not(madsim))]
pub fn shared() -> &'static Arc<PageCache> {
    crate::Host::process().pages()
}

impl PageCache {
    /// A cache going by `options`, its default flusher started; additional per-[`HostDisk`]
    /// flusher threads are spawned lazily as files on distinct disks are opened. Not under
    /// Shuttle, whose schedules start no thread of their own: [`PageCache::without_flusher`].
    pub fn new(options: Options) -> Arc<Self> {
        #[cfg(not(madsim))]
        {
            let cache = Self::made(options, true);
            cache.ensure_flusher(DiskKey::Default, None, false);
            cache
        }
        // cladefs: under madsim, which runs one thread, no flusher to wait for: writers past the
        // background write back themselves, as without one; and a task, on the simulation's clock,
        // writes back what expires, a pass at a time (`flush_pass`, doc-24).
        #[cfg(madsim)]
        {
            let cache = Self::made(options, false);
            let ticking = Arc::downgrade(&cache);
            let interval = options.expire / 4;
            drop(tokio::spawn(async move {
                loop {
                    tokio::time::sleep(interval).await;
                    let Some(cache) = ticking.upgrade() else { return };
                    host_fs::may_block(|| cache.flush_pass(Instant::now()));
                }
            }));
            cache
        }
    }

    /// A cache going by `options` with no flusher: the writer that takes it past
    /// [`Options::background`] dirty writes back, on its own thread, until it is under, and
    /// nothing expires.
    pub fn without_flusher(options: Options) -> Arc<Self> {
        Self::made(options, false)
    }

    fn made(options: Options, flusher: bool) -> Arc<Self> {
        Arc::new(Self {
            options,
            background: AtomicU64::new(options.background),
            hard: AtomicU64::new(options.hard),
            most: AtomicU64::new(options.most),
            flusher,
            flusher_died: AtomicBool::new(false),
            held: AtomicU64::new(0),
            peak: AtomicU64::new(0),
            dirty: AtomicU64::new(0),
            peak_dirty: AtomicU64::new(0),
            writing: AtomicU64::new(0),
            stalls: AtomicU64::new(0),
            stalled_ns: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            uses: AtomicU64::new(0),
            files: Mutex::default(),
            flushers: Mutex::default(),
            active_disks: AtomicU64::new(0),
            drained: Condvar::new(),
            holding: Mutex::new(()),
            stuck: AtomicU64::new(0),
            failing_disks: Mutex::default(),
            frames: Frames::new(),
        })
    }

    /// Ensures a [`DiskFlusher`] exists for `disk_key` (spawning its flusher thread outside
    /// `madsim` when `self.flusher` is true) and returns it. When `bind_file` is true, increments
    /// `open_files` under the `flushers` lock and updates [`PageCache::active_disks`] on `0 -> 1`.
    fn ensure_flusher(
        self: &Arc<Self>,
        disk_key: DiskKey,
        disk: Option<&Arc<bucket_cell::host_disk::HostDisk>>,
        bind_file: bool,
    ) -> Arc<DiskFlusher> {
        let mut flushers = self.flushers.lock().unwrap();
        if let Some(existing) = flushers.get(&disk_key) {
            #[cfg(not(madsim))]
            let stale_virtual = matches!(disk_key, DiskKey::Virtual(_))
                && existing.disk.as_ref().is_some_and(|w| w.strong_count() == 0);
            #[cfg(madsim)]
            let stale_virtual = false;
            if !stale_virtual {
                let flusher = existing.clone();
                if bind_file && flusher.open_files.fetch_add(1, Ordering::AcqRel) == 0 {
                    self.active_disks.fetch_add(1, Ordering::AcqRel);
                }
                return flusher;
            }
            existing.wake();
        }
        let flusher = Arc::new(DiskFlusher {
            open_files: AtomicU64::new(u64::from(bind_file)),
            disk: disk.map(Arc::downgrade),
            ..DiskFlusher::default()
        });
        flushers.insert(disk_key, flusher.clone());
        if bind_file {
            self.active_disks.fetch_add(1, Ordering::AcqRel);
        }
        drop(flushers);
        #[cfg(not(madsim))]
        if self.flusher {
            let weak = Arc::downgrade(self);
            let name = match disk_key {
                DiskKey::Default => "store flusher".to_owned(),
                DiskKey::Device(dev) => format!("store flusher dev-{dev:x}"),
                DiskKey::Virtual(ptr) => format!("store flusher v-{ptr:x}"),
            };
            let thread_flusher = flusher.clone();
            std::thread::Builder::new()
                .name(name)
                .spawn(move || flush_disk_while(weak, disk_key, thread_flusher))
                .expect("starting the page cache's flusher");
        }
        flusher
    }

    /// What it holds.
    pub fn usage(&self) -> Usage {
        Usage {
            held: self.held.load(Ordering::Relaxed),
            dirty: self.dirty.load(Ordering::Relaxed),
            writing: self.writing.load(Ordering::Relaxed),
            peak: self.peak.load(Ordering::Relaxed),
            peak_dirty: self.peak_dirty.load(Ordering::Relaxed),
            stalls: self.stalls.load(Ordering::Relaxed),
            stalled_ns: self.stalled_ns.load(Ordering::Relaxed),
            stuck: self.stuck.load(Ordering::Relaxed),
        }
    }

    /// Whether its flusher died, a panic unwinding it, as one can only where a panic on it does not
    /// abort the process ([`FLUSHER`]): nothing writes back what its writers leave dirty since.
    pub fn flusher_died(&self) -> bool {
        self.flusher_died.load(Ordering::Relaxed)
    }

    /// What it goes by now: its bounds as a governor sized them, if one did.
    pub fn options(&self) -> Options {
        Options {
            background: self.background(),
            hard: self.hard(),
            most: self.most(),
            expire: self.options.expire,
        }
    }

    /// cladefs: sizes the cache to `room`, the bytes the process's governor leaves it, as Linux
    /// sizes its page cache to the memory free (TASK-433.8): clean page retention stays bounded by
    /// `room` even below `Options::default().most` (board 6001), while dirty thresholds
    /// (`background` and `hard`) keep their floors so write-back and throttling continue to work.
    pub fn size_to(&self, room: u64) {
        let floors = self.options;
        self.background.store((room / 10).max(floors.background), Ordering::Relaxed);
        self.hard.store((room / 5).max(floors.hard), Ordering::Relaxed);
        self.most.store(room, Ordering::Relaxed);
        self.drained();
    }

    fn background(&self) -> u64 {
        self.background.load(Ordering::Relaxed)
    }

    fn hard(&self) -> u64 {
        self.hard.load(Ordering::Relaxed)
    }

    /// Per-disk background flush threshold: honors an explicit per-disk `dirty_budget` on
    /// `flusher`'s [`HostDisk`] (half the dirty hard budget) or divides [`PageCache::background`]
    /// across active disks (board 11939).
    fn disk_background_for(&self, flusher: &DiskFlusher) -> u64 {
        if let Some(budget) =
            flusher.disk.as_ref().and_then(Weak::upgrade).and_then(|d| d.dirty_budget())
        {
            return (budget / 2).max(1);
        }
        self.disk_background()
    }

    /// Per-disk dirty hard ceiling: honors an explicit per-disk `dirty_budget` on `flusher`'s
    /// [`HostDisk`] or divides [`PageCache::hard`] across active disks (board 11939).
    fn disk_hard_for(&self, flusher: &DiskFlusher) -> u64 {
        if let Some(budget) =
            flusher.disk.as_ref().and_then(Weak::upgrade).and_then(|d| d.dirty_budget())
        {
            return budget;
        }
        self.disk_hard()
    }

    /// Per-disk background flush threshold: divides [`PageCache::background`] across active disks
    /// (board 11939).
    fn disk_background(&self) -> u64 {
        let disks = self.active_disks.load(Ordering::Acquire).max(1);
        self.background() / disks
    }

    /// Per-disk dirty hard ceiling: divides [`PageCache::hard`] across active disks so one slow or
    /// stalled disk cannot exhaust the dirty-page budget for writers on healthy disks (board 11939).
    fn disk_hard(&self) -> u64 {
        let disks = self.active_disks.load(Ordering::Acquire).max(1);
        self.hard() / disks
    }

    fn most(&self) -> u64 {
        self.most.load(Ordering::Relaxed)
    }

    /// The bytes it holds: what the extent daemon's governor counts as the process's.
    pub fn bytes(&self) -> u64 {
        self.held.load(Ordering::Relaxed)
    }

    /// Evicts clean pages, of the files used least recently first, until it holds `target` bytes
    /// or nothing clean is left; and wakes the flusher if what it holds is still more, so that
    /// what is dirty becomes clean, and evictable. Returns the bytes it holds then.
    pub fn evict_to(&self, target: u64) -> u64 {
        let bytes = self.bytes();
        if bytes <= target {
            return bytes;
        }
        // Only scan open files when clean pages exist to evict; under low `most` (< `background`)
        // with only dirty pages resident, avoid scanning and waking the flusher on every write.
        if bytes > self.dirty.load(Ordering::Relaxed) {
            for file in self.files_by(|file| file.used()) {
                if self.bytes() <= target {
                    break;
                }
                file.evict_clean();
            }
        }
        if self.bytes() > target && self.dirty.load(Ordering::Relaxed) > self.background() {
            self.wake();
        }
        self.bytes()
    }

    /// Writes back every dirty page of every file, waiting for every write-back in flight: what
    /// a reader of the files behind the cells' backs needs first, as a test damaging one. A file
    /// failing leaves the others written; the first failure is returned, and each file's next sync
    /// writes it back again.
    pub fn write_back_all(&self) -> io::Result<()> {
        let mut failed = Ok(());
        for file in self.files() {
            if let Err(err) = file.write_back_all() {
                failed = failed.and(Err(err));
            }
        }
        failed
    }

    /// The files it holds pages of, those gone dropped.
    fn files(&self) -> Vec<Arc<dyn Paged>> {
        let mut files = self.files.lock().unwrap();
        files.retain(|file| file.strong_count() > 0);
        files.iter().filter_map(Weak::upgrade).collect()
    }

    /// The files it holds pages of, ordered by `key`, each file's taken once, before any is
    /// compared: keys other threads change as it goes, its files' uses and writes, ordered as each
    /// comparison finds them, are no order, which a sort panics on (TASK-493).
    fn files_by<K: Ord>(&self, key: impl Fn(&dyn Paged) -> K) -> Vec<Arc<dyn Paged>> {
        let mut keyed: Vec<(K, Arc<dyn Paged>)> =
            self.files().into_iter().map(|file| (key(&*file), file)).collect();
        keyed.sort_by(|(one, _), (other, _)| one.cmp(other));
        keyed.into_iter().map(|(_, file)| file).collect()
    }

    /// A use's number.
    fn used(&self) -> u64 {
        self.uses.fetch_add(1, Ordering::Relaxed)
    }

    /// Wakes all per-disk flushers, if it has any.
    fn wake(&self) {
        if !self.flusher {
            return;
        }
        let flushers: Vec<Arc<DiskFlusher>> =
            self.flushers.lock().unwrap().values().cloned().collect();
        for flusher in flushers {
            flusher.wake();
        }
    }

    /// Writes back, on the caller's thread, the files dirty the longest, a few chunks of each in
    /// turn, until no more than `dirty` bytes are dirty: a file whose write-back fails, which its
    /// next sync reports, is passed over, as the flusher passes it over (board 8553).
    fn write_back_to(&self, dirty: u64) {
        for file in self.files_by(|file| file.dirtied()) {
            if self.dirty.load(Ordering::Relaxed) <= dirty {
                return;
            }
            if file.dirtied().is_none() {
                continue;
            }
            if let Err(err) = file.write_back(16 * CHUNK) {
                log::warn!("extent store: writing back a file's pages: {err}");
            }
        }
    }

    /// Holds a writer about to dirty pages back while more than its disk's share of
    /// [`Options::hard`] is dirty (or global [`Options::hard`] when called without a disk flusher),
    /// until write-back brings it under, its flusher woken. With no flusher, its writers keep it
    /// under [`Options::background`] as they go ([`PageCache::dirtied_on`]). Its flusher dead, it
    /// panics, as none would let it go.
    ///
    /// cladefs: what files whose write-back is failing hold dirty ([`PageCache::stuck`]) no wait
    /// writes back: a writer waits for the rest alone, and one of a failing file, or of a disk
    /// with one, which `failing` gives the failure of, held at the bound, fails with it. Per-disk
    /// dirty budgets (board 11939) further isolate a slow or blocked disk whose write-back has not
    /// yet returned an error so writers on healthy disks continue unthrottled.
    fn throttle_for(
        &self,
        flusher: Option<&DiskFlusher>,
        failing: impl Fn() -> Option<io::Error>,
    ) -> io::Result<()> {
        let current = || match flusher {
            Some(f) => (
                f.dirty.load(Ordering::Relaxed),
                f.stuck.load(Ordering::Relaxed),
                self.disk_hard_for(f),
            ),
            None => (
                self.dirty.load(Ordering::Relaxed),
                self.stuck.load(Ordering::Relaxed),
                self.hard(),
            ),
        };
        if !self.flusher {
            return Ok(());
        }
        let (dirty, _, limit) = current();
        if dirty <= limit {
            return Ok(());
        }
        // cladefs: when it began to wait, if it waited: a stall, counted with its time as it ends
        // (board 8565).
        let mut waited = None;
        let held = loop {
            assert!(
                !self.flusher_died(),
                "the page cache's flusher died: nothing writes back for a writer held at its bound"
            );
            // Not under `holding`: a file's pages' lock goes before it.
            if let Some(error) = failing() {
                break Err(error);
            }
            let holding = self.holding.lock().unwrap();
            let (dirty, stuck, limit) = current();
            let unstuck = dirty.saturating_sub(stuck);
            if dirty <= limit || unstuck <= limit {
                break Ok(());
            }
            match flusher {
                Some(f) => f.wake(),
                None => self.wake(),
            }
            waited.get_or_insert_with(std::time::Instant::now);
            #[cfg(test)]
            if let Some(f) = flusher {
                f.waiters.fetch_add(1, Ordering::Release);
            }
            // Woken as pages go clean, or go, and after each of the flusher's passes, whose
            // failures change what is stuck.
            let waited_guard = self.drained.wait(holding);
            #[cfg(test)]
            if let Some(f) = flusher {
                f.waiters.fetch_sub(1, Ordering::Release);
            }
            drop(waited_guard.unwrap());
        };
        if let Some(began) = waited {
            self.stalls.fetch_add(1, Ordering::Relaxed);
            self.stalled_ns.fetch_add(began.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
        held
    }

    #[cfg(test)]
    fn throttle(&self, failing: impl Fn() -> Option<io::Error>) -> io::Result<()> {
        self.throttle_for(None, failing)
    }

    /// Pages became clean, or went: writers held back may go on.
    fn drained(&self) {
        let _holding = self.holding.lock().unwrap();
        self.drained.notify_all();
    }

    /// Pages were dirtied on `flusher`: wakes `flusher` once its disk exceeds its background
    /// threshold (or writes back inline when there is no flusher), and evicts clean pages past
    /// [`Options::most`].
    fn dirtied_on(&self, flusher: Option<&DiskFlusher>) {
        if let Some(flusher) = flusher {
            if flusher.dirty.load(Ordering::Relaxed) > self.disk_background_for(flusher) {
                flusher.wake();
            }
        } else if self.dirty.load(Ordering::Relaxed) > self.background() {
            self.write_back_to(self.background());
        }
        if self.bytes() > self.most() {
            self.evict_to(self.most());
        }
    }
}

impl Drop for PageCache {
    fn drop(&mut self) {
        let flushers = self.flushers.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner);
        for flusher in flushers.values() {
            flusher.wake();
        }
    }
}

impl PageCache {
    /// A pass across all disks, as of `now` (used by `madsim` and unit tests).
    #[cfg(any(madsim, test))]
    fn flush_pass(&self, now: Instant) {
        let flushers: Vec<Arc<DiskFlusher>> =
            self.flushers.lock().unwrap().values().cloned().collect();
        for flusher in flushers {
            loop {
                let Some((file, range)) = flusher.begun.lock().unwrap().pop_front() else { break };
                let Some(file) = file.upgrade() else { continue };
                if let Err(err) = file.write_range(range) {
                    log::warn!("extent store: writing back a range of a file's pages: {err}");
                }
            }
        }
        for file in self.files_by(|file| file.dirtied()) {
            let Some(dirtied) = file.dirtied() else { continue };
            let over = self.dirty.load(Ordering::Relaxed) > self.background();
            if !over && now.duration_since(dirtied) < self.options.expire {
                break;
            }
            if let Err(err) = file.write_back(16 * CHUNK) {
                log::warn!("extent store: writing back a file's pages: {err}");
            }
        }
        self.drained();
    }

    /// A pass of `disk_key`'s flusher, as of `now`: it writes back the ranges writers asked it to
    /// begin ([`Cached::write_behind`]), in the order asked; then what has been dirty longer than
    /// [`Options::expire`], and, while more than this disk's share of [`Options::background`] is
    /// dirty, the files on `disk_key` dirty the longest, 16 [`CHUNK`]s' worth of writes each in
    /// turn.
    #[cfg(not(madsim))]
    fn flush_pass_for(&self, disk_key: DiskKey, flusher: &DiskFlusher, now: Instant) {
        loop {
            let Some((file, range)) = flusher.begun.lock().unwrap().pop_front() else { break };
            let Some(file) = file.upgrade() else { continue };
            if let Err(err) = file.write_range(range) {
                log::warn!("extent store: writing back a range of a file's pages: {err}");
            }
        }
        for file in self.files_by(|file| file.dirtied()) {
            if file.disk_key() != disk_key {
                continue;
            }
            let Some(dirtied) = file.dirtied() else { continue };
            let over = flusher.dirty.load(Ordering::Relaxed) > self.disk_background_for(flusher);
            if !over && now.duration_since(dirtied) < self.options.expire {
                break;
            }
            if let Err(err) = file.write_back(16 * CHUNK) {
                log::warn!("extent store: writing back a file's pages: {err}");
            }
        }
        self.drained();
    }
}

/// One per-`HostDisk` flusher: while the cache is there, a pass ([`PageCache::flush_pass_for`])
/// each time it is woken, or a quarter of [`Options::expire`] after the last. The process's work
/// for its life ([`FLUSHER`]); unwound by a panic, it says so ([`Dying`]).
#[cfg(not(madsim))]
fn flush_disk_while(cache: Weak<PageCache>, disk_key: DiskKey, flusher: Arc<DiskFlusher>) {
    let _work = Holding::work(FLUSHER);
    let _dying = Dying { cache: cache.clone() };
    loop {
        let Some(strong) = cache.upgrade() else { return };
        let interval = strong.options.expire / 4;
        drop(strong);
        {
            let woken = flusher.flushing.lock().unwrap();
            let (mut woken, _) =
                flusher.flush.wait_timeout_while(woken, interval, |woken| !*woken).unwrap();
            *woken = false;
        }
        let Some(strong) = cache.upgrade() else { return };
        strong.flush_pass_for(disk_key, &flusher, Instant::now());
        if matches!(disk_key, DiskKey::Virtual(_))
            && flusher.disk.as_ref().is_some_and(|w| w.strong_count() == 0)
            && flusher.open_files.load(Ordering::Acquire) == 0
            && flusher.dirty.load(Ordering::Relaxed) == 0
        {
            let mut flushers = strong.flushers.lock().unwrap();
            if flusher.open_files.load(Ordering::Acquire) == 0
                && flusher.begun.lock().unwrap().is_empty()
            {
                if flushers.get(&disk_key).is_some_and(|cur| Arc::ptr_eq(cur, &flusher)) {
                    flushers.remove(&disk_key);
                }
                return;
            }
        }
    }
}

/// A flusher's death, said as a panic unwinds it, where the panic did not abort the process: the
/// cache knows it ([`PageCache::flusher_died`]), the log says it, and writers held back, woken,
/// find it out ([`PageCache::throttle_for`]).
#[cfg(not(madsim))]
struct Dying {
    cache: Weak<PageCache>,
}

#[cfg(not(madsim))]
impl Drop for Dying {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        let Some(cache) = self.cache.upgrade() else { return };
        cache.flusher_died.store(true, Ordering::Relaxed);
        log::error!("extent store: the page cache's flusher died, a panic unwinding it (TASK-493)");
        cache.drained();
    }
}

/// A file a [`PageCache`] holds pages of, as its flusher and its eviction see it.
trait Paged: Send + Sync {
    /// Which per-disk flusher owns this file ([`DiskKey::Default`] when not bound to a
    /// [`HostDisk`]).
    #[cfg(not(madsim))]
    fn disk_key(&self) -> DiskKey {
        DiskKey::Default
    }
    /// When it was used last ([`PageCache::used`]).
    fn used(&self) -> u64;
    /// When its oldest dirty page was dirtied, if it has one.
    fn dirtied(&self) -> Option<Instant>;
    /// Writes back up to `most` bytes of its dirty pages, in its order.
    fn write_back(&self, most: u64) -> io::Result<()>;
    /// Writes back its dirty pages in `range`.
    fn write_range(&self, range: Range<u64>) -> io::Result<()>;
    /// Drops its clean pages.
    fn evict_clean(&self);
    /// Writes back all its dirty pages, waiting for every write-back of it in flight.
    fn write_back_all(&self) -> io::Result<()>;
}

/// A page held.
struct Page {
    /// cladefs: a frame of its cache's (TASK-433.9).
    bytes: Frame,
    /// The number of the oldest write it holds that no write-back took yet, if it is dirty: what
    /// a sync goes by, so that a page written before a sync and again after it began is written
    /// back for it, as it is now.
    dirty: Option<u64>,
    /// The number of the last write to it.
    last: u64,
    /// Whether a write-back of it is in flight.
    writing: bool,
}

#[cfg(test)]
thread_local! { static WRITE_BACK_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

/// A file's pages, and what the cache knows of the file.
struct Pages {
    dirty_pages: u64,
    pages: BTreeMap<u64, Page>,
    /// The file's size: its own, or past it, what was written.
    size: u64,
    /// When the oldest of its dirty pages was dirtied.
    dirtied: Option<Instant>,
    /// A write-back that failed, which the next sync reports, once.
    failed: Option<io::Error>,
    /// Whether its write-back is failing: the last failure, until a write-back succeeds. Its
    /// dirty pages are then counted stuck too ([`PageCache::stuck`]).
    failing: Option<io::Error>,
}

/// A file, `F`, through the page cache: what [`File`] it is to its cell.
pub struct Cached<F: File> {
    shared: Arc<Shared<F>>,
}

/// What a file's openings share: one cache of its pages, however many times it is opened, and
/// whatever its name ([`crate::HostDir`]'s files, by path).
pub(crate) struct Shared<F: File> {
    file: F,
    cache: Arc<PageCache>,
    /// cladefs: the host disk this file lives on, whose dirty count tracks its dirty pages
    /// (board 5995).
    disk: Option<Arc<bucket_cell::host_disk::HostDisk>>,
    /// Per-disk flusher and dirty/stuck counters for this file's [`DiskKey`] when the cache runs
    /// background flusher threads (`None` on inline caches without flushers; board 11939).
    flusher: Option<Arc<DiskFlusher>>,
    pages: Mutex<Pages>,
    /// Keeps the file's bytes stable while readers miss the cache. Write-back, punching and
    /// dropping dirty pages take it exclusively. Taken before the pages' lock: a cache hit, or a
    /// write needing no disk read, need not take it.
    backing: RwLock<()>,
    used: AtomicU64,
    /// Whether the file is at no name any more ([`Cached::unlink`]): its last opening drops its
    /// pages unwritten.
    unlinked: AtomicBool,
}

/// A file through the page cache that keeps it open no more ([`Cached::downgrade`]): what a
/// directory names its files' pages by, which go with the file's last opening.
pub(crate) struct WeakCached<F: File> {
    shared: Weak<Shared<F>>,
}

impl<F: File> Clone for WeakCached<F> {
    fn clone(&self) -> Self {
        Self { shared: self.shared.clone() }
    }
}

impl<F: File> WeakCached<F> {
    /// The file, if an opening of it holds it still.
    pub fn upgrade(&self) -> Option<Cached<F>> {
        self.shared.upgrade().map(|shared| Cached { shared })
    }
}

impl<F: File> Clone for Cached<F> {
    fn clone(&self) -> Self {
        Self { shared: self.shared.clone() }
    }
}

impl<F: File> Cached<F> {
    /// `file`, through `cache`.
    pub fn new(file: F, cache: &Arc<PageCache>) -> io::Result<Self> {
        Self::on_disk(file, cache, None)
    }

    /// `file`, through `cache`, charging dirty pages to `disk` if given (board 5995).
    pub fn on_disk(
        file: F,
        cache: &Arc<PageCache>,
        disk: Option<Arc<bucket_cell::host_disk::HostDisk>>,
    ) -> io::Result<Self> {
        let size = file.size()?;
        let flusher = if cache.flusher {
            let disk_key = DiskKey::of(disk.as_ref());
            Some(cache.ensure_flusher(disk_key, disk.as_ref(), true))
        } else {
            None
        };
        let shared = Arc::new(Shared {
            file,
            cache: cache.clone(),
            disk,
            flusher,
            pages: Mutex::new(Pages {
                dirty_pages: 0,
                pages: BTreeMap::new(),
                size,
                dirtied: None,
                failed: None,
                failing: None,
            }),
            backing: RwLock::new(()),
            used: AtomicU64::new(cache.used()),
            unlinked: AtomicBool::new(false),
        });
        let paged: Arc<dyn Paged> = shared.clone();
        cache.files.lock().unwrap().push(Arc::downgrade(&paged));
        Ok(Self { shared })
    }

    /// The file under the cache.
    pub fn file(&self) -> &F {
        &self.shared.file
    }

    /// Writes back every dirty page: before the file is given to the host
    /// ([`crate::CachedDir`]'s `give`), which reads it around the cache.
    pub(crate) fn write_back_whole(&self) -> io::Result<()> {
        self.shared.flush(0..u64::MAX, None)
    }

    /// Drops every page, the dirty ones unwritten: the file is the host's now
    /// (given by [`crate::CachedDir`]'s `give`), or a test's to damage.
    pub(crate) fn cancel(&self) {
        self.shared.drop_pages(0..u64::MAX);
    }

    /// The file is at no name any more: its openings read and write it on, and the last of them
    /// drops its pages, the dirty ones unwritten ([`crate::CachedDir`]'s `remove`).
    pub(crate) fn unlink(&self) {
        self.shared.unlinked.store(true, Ordering::Relaxed);
    }

    /// This, held no more: what a directory names it by.
    pub(crate) fn downgrade(&self) -> WeakCached<F> {
        WeakCached { shared: Arc::downgrade(&self.shared) }
    }

    /// Whether `other` is an opening of the same file.
    pub(crate) fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
}

impl<F: File> Shared<F> {
    /// Counts `bytes` more of its pages dirty: stuck too while its write-back is `failing`, as
    /// its pages say under their lock.
    fn dirty_add(&self, bytes: u64, failing: bool) {
        if bytes == 0 {
            return;
        }
        let dirty = self.cache.dirty.fetch_add(bytes, Ordering::Relaxed) + bytes;
        self.cache.peak_dirty.fetch_max(dirty, Ordering::Relaxed);
        if let Some(flusher) = &self.flusher {
            flusher.dirty.fetch_add(bytes, Ordering::Relaxed);
        }
        if failing {
            self.cache.stuck.fetch_add(bytes, Ordering::Relaxed);
            if let Some(flusher) = &self.flusher {
                flusher.stuck.fetch_add(bytes, Ordering::Relaxed);
            }
        }
        if let Some(disk) = &self.disk {
            disk.dirty.add(bytes);
        }
    }

    /// Counts `bytes` of its pages dirty no more, as [`Shared::dirty_add`] counted them.
    fn dirty_sub(&self, bytes: u64, failing: bool) {
        if bytes == 0 {
            return;
        }
        if failing {
            self.cache.stuck.fetch_sub(bytes, Ordering::Relaxed);
            if let Some(flusher) = &self.flusher {
                flusher.stuck.fetch_sub(bytes, Ordering::Relaxed);
            }
        }
        self.cache.dirty.fetch_sub(bytes, Ordering::Relaxed);
        if let Some(flusher) = &self.flusher {
            flusher.dirty.fetch_sub(bytes, Ordering::Relaxed);
        }
        if let Some(disk) = &self.disk {
            disk.dirty.sub(bytes);
        }
    }

    /// Notes whether its write-back is failing, as one just did or did not, under its pages'
    /// lock: its dirty pages counted stuck from the first failure, and no more from a success;
    /// its disk failing while it is.
    fn note_failing(&self, pages: &mut Pages, failed: Option<&io::Error>) {
        let bytes = pages.dirty_pages * PAGE;
        match (failed, pages.failing.is_some()) {
            (Some(err), false) => {
                self.cache.stuck.fetch_add(bytes, Ordering::Relaxed);
                if let Some(flusher) = &self.flusher {
                    flusher.stuck.fetch_add(bytes, Ordering::Relaxed);
                }
                self.disk_failing(Some(err));
            }
            (Some(err), true) => self.disk_failing_again(err),
            (None, true) => {
                self.cache.stuck.fetch_sub(bytes, Ordering::Relaxed);
                if let Some(flusher) = &self.flusher {
                    flusher.stuck.fetch_sub(bytes, Ordering::Relaxed);
                }
                self.disk_failing(None);
            }
            (None, false) => {}
        }
        pages.failing = failed.map(copied);
    }

    /// Its disk has one more file failing, `failed`'s, or, given none, one fewer.
    fn disk_failing(&self, failed: Option<&io::Error>) {
        let Some(disk) = &self.disk else { return };
        let key = Arc::as_ptr(disk) as usize;
        let mut disks = self.cache.failing_disks.lock().unwrap();
        match failed {
            Some(err) => {
                let entry = disks.entry(key).or_insert_with(|| (0, copied(err)));
                *entry = (entry.0 + 1, copied(err));
            }
            None => {
                let entry = disks.get_mut(&key).expect("a disk with a file failing");
                entry.0 -= 1;
                if entry.0 == 0 {
                    disks.remove(&key);
                }
            }
        }
    }

    /// Its disk's failure is `failed` now, as its file failed again.
    fn disk_failing_again(&self, failed: &io::Error) {
        let Some(disk) = &self.disk else { return };
        let mut disks = self.cache.failing_disks.lock().unwrap();
        if let Some(entry) = disks.get_mut(&(Arc::as_ptr(disk) as usize)) {
            entry.1 = copied(failed);
        }
    }

    /// Its write-back's failure, if it is failing, or its disk's, if a file of it is: what its
    /// writers held at the cache's bound fail with ([`PageCache::throttle_for`]).
    fn failing(&self) -> Option<io::Error> {
        if let Some(err) = self.pages().failing.as_ref() {
            return Some(copied(err));
        }
        let disk = self.disk.as_ref()?;
        let disks = self.cache.failing_disks.lock().unwrap();
        disks.get(&(Arc::as_ptr(disk) as usize)).map(|(_, err)| copied(err))
    }

    /// Its write-back, taken: none other of it in flight.
    fn writeback(&self) -> RwLockWriteGuard<'_, ()> {
        self.backing.write().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn read_backing(&self) -> RwLockReadGuard<'_, ()> {
        self.backing.read().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Its pages, locked, a use of it; as a panic left them, if one did ([`InFlight`]).
    fn lock(&self) -> MutexGuard<'_, Pages> {
        self.used.store(self.cache.used(), Ordering::Relaxed);
        self.pages()
    }

    /// Its pages, locked, as the cache's eviction and flusher look at them, no use of it; as a
    /// panic left them, if one did: a lock a writer's panic poisoned panics no flusher after it.
    fn pages(&self) -> MutexGuard<'_, Pages> {
        self.pages.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Drops the pages in `range`, dirty or not, waiting for a write-back in flight to end.
    fn drop_pages(&self, range: Range<u64>) {
        let _writeback = self.writeback();
        let mut pages = self.lock();
        let first = range.start / PAGE;
        let last = range.end.div_ceil(PAGE);
        let doomed: Vec<u64> = pages.pages.range(first..last).map(|(&index, _)| index).collect();
        let mut dropped_dirty = 0u64;
        for index in doomed {
            let page = pages.pages.remove(&index).unwrap();
            self.cache.held.fetch_sub(PAGE, Ordering::Relaxed);
            if page.dirty.is_some() {
                pages.dirty_pages -= 1;
                dropped_dirty += 1;
            }
        }
        self.dirty_sub(dropped_dirty * PAGE, pages.failing.is_some());
        if pages.dirty_pages == 0 {
            pages.dirtied = None;
        }
        drop(pages);
        self.cache.drained();
    }

    /// Writes back the dirty pages in `range`, those dirtied by writes numbered `through` or
    /// before if given; or, if `most` is given, up to `most` bytes of them. It waits for the
    /// write-back in flight first, if any. Pages are written in the file's order, contiguous dirty
    /// ones at once, up to [`CHUNK`] a write.
    fn flush(&self, range: Range<u64>, through: Option<u64>) -> io::Result<()> {
        let flushed = self.write_runs(range, through, None);
        // Clean pages just written back stay capped at `most` (board 6001).
        let most = self.cache.most();
        if self.cache.bytes() > most {
            self.cache.evict_to(most);
        }
        flushed
    }

    fn write_runs(
        &self,
        range: Range<u64>,
        through: Option<u64>,
        most: Option<u64>,
    ) -> io::Result<()> {
        let exclusive = self.writeback();
        self.write_locked_runs(range, through, most, &exclusive)
    }

    fn write_locked_runs(
        &self,
        range: Range<u64>,
        through: Option<u64>,
        mut most: Option<u64>,
        _exclusive: &RwLockWriteGuard<'_, ()>,
    ) -> io::Result<()> {
        let (mut first, last) = (range.start / PAGE, range.end.div_ceil(PAGE));
        let covers =
            |page: &Page| page.dirty.is_some_and(|dirty| through.is_none_or(|t| dirty <= t));
        let mut pages = self.lock();
        loop {
            if most.is_some_and(|most| most == 0) {
                return Ok(());
            }
            // The first run of pages it covers not in flight, contiguous, up to a chunk.
            let mut run: Vec<u64> = Vec::new();
            for (&index, page) in pages.pages.range(first..last) {
                #[cfg(test)]
                WRITE_BACK_VISITS.set(WRITE_BACK_VISITS.get() + 1);
                let fits = run.last().is_none_or(|&before| index == before + 1)
                    && (run.len() as u64) * PAGE < CHUNK;
                match (covers(page), run.is_empty()) {
                    (true, true) => run.push(index),
                    (true, false) if fits => run.push(index),
                    (_, true) => {}
                    _ => break,
                }
            }
            if run.is_empty() {
                break;
            }
            first = run[run.len() - 1] + 1;
            let start = run[0] * PAGE;
            let end = ((run[run.len() - 1] + 1) * PAGE).min(pages.size.max(start));
            // Assembled in memory aligned to blocks, which direct I/O writes as it is: whole
            // pages, but at the file's end (TASK-433.8).
            let mut buffer = bucket_cell::direct::Buffer::new(run.len() * PAGE as usize);
            let mut written = Vec::with_capacity(run.len());
            for (&index, into) in run.iter().zip(buffer.bytes_mut().chunks_exact_mut(PAGE as usize))
            {
                let page = pages.pages.get_mut(&index).unwrap();
                page.writing = true;
                written.push((index, page.last));
                into.copy_from_slice(&page.bytes);
            }
            let bytes = &buffer.bytes()[..(end - start) as usize];
            let len = run.len() as u64 * PAGE;
            self.cache.writing.fetch_add(len, Ordering::Relaxed);
            drop(pages);
            let in_flight = InFlight { shared: self, run: &written, len, ended: false };
            let wrote = self.file.write_at(bytes, start);
            pages = in_flight.end();
            let mut cleaned = 0u64;
            for &(index, last) in &written {
                let Some(page) = pages.pages.get_mut(&index) else { continue };
                if wrote.is_err() {
                    continue;
                }
                if page.last == last {
                    page.dirty = None;
                    pages.dirty_pages -= 1;
                    cleaned += 1;
                } else {
                    // Written again while it was written back: what it holds since is the next
                    // write-back's.
                    page.dirty = Some(last + 1);
                }
            }
            self.dirty_sub(cleaned * PAGE, pages.failing.is_some());
            if pages.dirty_pages == 0 {
                pages.dirtied = None;
            }
            self.note_failing(&mut pages, wrote.as_ref().err());
            self.cache.drained();
            if let Err(err) = wrote {
                let kept = io::Error::new(err.kind(), err.to_string());
                pages.failed.get_or_insert(kept);
                return Err(err);
            }
            if let Some(most) = &mut most {
                *most = most.saturating_sub(len);
            }
        }
        Ok(())
    }
}

/// `err`, again: an `io::Error` is no `Clone`.
fn copied(err: &io::Error) -> io::Error {
    io::Error::new(err.kind(), err.to_string())
}

/// A write-back in flight, of a run of a file's pages, by their indexes with the numbers of their
/// last writes: ended ([`InFlight::end`]) as its write returns, or, if a panic unwinds it, by its
/// drop, its pages left dirty, and in flight no more, and the failure kept for the next sync.
struct InFlight<'a, 'r, F: File> {
    shared: &'a Shared<F>,
    run: &'r [(u64, u64)],
    len: u64,
    ended: bool,
}

impl<'a, F: File> InFlight<'a, '_, F> {
    /// Its pages in flight no more, the file's lock held, for the caller to say what its write
    /// did.
    fn end(mut self) -> MutexGuard<'a, Pages> {
        self.ended = true;
        self.unmark()
    }

    fn unmark(&self) -> MutexGuard<'a, Pages> {
        let mut pages = self.shared.lock();
        self.shared.cache.writing.fetch_sub(self.len, Ordering::Relaxed);
        for (index, _) in self.run {
            if let Some(page) = pages.pages.get_mut(index) {
                page.writing = false;
            }
        }
        pages
    }
}

impl<F: File> Drop for InFlight<'_, '_, F> {
    fn drop(&mut self) {
        if self.ended {
            return;
        }
        let mut pages = self.unmark();
        let interrupted = io::Error::new(io::ErrorKind::Interrupted, "a write-back panicked");
        pages.failed.get_or_insert(interrupted);
    }
}

/// A file whose every opening went: what of it is dirty is written back, as a file closed keeps
/// what was written to it, unless it is at no name; then its pages go, counted out.
impl<F: File> Drop for Shared<F> {
    fn drop(&mut self) {
        // Not as a panic unwinds: its pages' lock may be poisoned, and a second panic aborts.
        // Dirty pages flushed on drop move off an async worker (board 10824).
        if !self.unlinked.load(Ordering::Relaxed)
            && !std::thread::panicking()
            && self.dirtied().is_some()
            && let Err(err) = crate::upload::blocking(|| self.flush(0..u64::MAX, None))
        {
            log::warn!("extent store: writing back a file closed: {err}");
        }
        let pages = self.pages.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner);
        let dirty = pages.pages.values().filter(|page| page.dirty.is_some()).count() as u64;
        let failing = pages.failing.is_some();
        self.cache.held.fetch_sub(pages.pages.len() as u64 * PAGE, Ordering::Relaxed);
        self.dirty_sub(dirty * PAGE, failing);
        // A file gone fails its disk no more.
        if failing {
            self.disk_failing(None);
        }
        if let Some(flusher) = &self.flusher
            && flusher.open_files.fetch_sub(1, Ordering::AcqRel) == 1
        {
            self.cache.active_disks.fetch_sub(1, Ordering::AcqRel);
        }
        self.cache.drained();
    }
}

impl<F: File> Paged for Shared<F> {
    #[cfg(not(madsim))]
    fn disk_key(&self) -> DiskKey {
        DiskKey::of(self.disk.as_ref())
    }

    fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    fn dirtied(&self) -> Option<Instant> {
        self.pages().dirtied
    }

    fn write_back(&self, most: u64) -> io::Result<()> {
        let wrote = self.write_runs(0..u64::MAX, None, Some(most));
        let limit = self.cache.most();
        if self.cache.bytes() > limit {
            self.cache.evict_to(limit);
        }
        wrote
    }

    fn write_range(&self, range: Range<u64>) -> io::Result<()> {
        self.flush(range, None)
    }

    fn write_back_all(&self) -> io::Result<()> {
        self.flush(0..u64::MAX, None)
    }

    fn evict_clean(&self) {
        let mut pages = self.pages();
        let before = pages.pages.len();
        pages.pages.retain(|_, page| page.dirty.is_some() || page.writing);
        let evicted = (before - pages.pages.len()) as u64 * PAGE;
        self.cache.held.fetch_sub(evicted, Ordering::Relaxed);
    }
}

impl<F: File> File for Cached<F> {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let shared = &*self.shared;
        let mut backing = None;
        let mut pages = shared.lock();
        let end = offset + buf.len() as u64;
        let mut at = offset;
        while at < end {
            let index = at / PAGE;
            let within = (at % PAGE) as usize;
            let take = ((PAGE - at % PAGE).min(end - at)) as usize;
            let to = (at - offset) as usize..(at - offset) as usize + take;
            match pages.pages.get(&index) {
                Some(page) => buf[to].copy_from_slice(&page.bytes[within..within + take]),
                None => {
                    if backing.is_none() {
                        drop(pages);
                        backing = Some(shared.read_backing());
                        pages = shared.lock();
                        // The cached prefix could have changed or been written back while waiting.
                        at = offset;
                        continue;
                    }
                    // The run of pages it holds not, read at once from the file.
                    let mut miss_end = at + take as u64;
                    while miss_end < end && !pages.pages.contains_key(&(miss_end / PAGE)) {
                        miss_end = ((miss_end / PAGE + 1) * PAGE).min(end);
                    }
                    let to = (at - offset) as usize..(miss_end - offset) as usize;
                    drop(pages);
                    shared.file.read_at(&mut buf[to], at)?;
                    pages = shared.lock();
                    at = miss_end;
                    continue;
                }
            }
            at += take as u64;
        }
        if backing.is_some() {
            // Write-back and dropping dirty pages waited for this read. A newly written page
            // is still cached, and any evicted clean page still agrees with the file.
            for (&index, page) in pages.pages.range(offset / PAGE..end.div_ceil(PAGE)) {
                let start = index * PAGE;
                let from = start.max(offset);
                let to = (start + PAGE).min(end);
                buf[(from - offset) as usize..(to - offset) as usize]
                    .copy_from_slice(&page.bytes[(from - start) as usize..(to - start) as usize]);
            }
        }
        // Past the end of what was written, as the file reads: zeros.
        let size = pages.size;
        if end > size {
            let from = size.saturating_sub(offset).min(buf.len() as u64) as usize;
            buf[from..].fill(0);
        }
        Ok(())
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        let shared = &*self.shared;
        shared.cache.throttle_for(shared.flusher.as_deref(), || shared.failing())?;
        let mut backing = None;
        let mut pages = shared.lock();
        let mut end = offset + buf.len() as u64;
        let partial = [
            (offset % PAGE != 0).then_some(offset / PAGE),
            (end % PAGE != 0).then_some(end / PAGE),
        ];
        let mut loaded = Vec::new();
        let mut failed = None;
        // Only partial pages need the old bytes. Read them before applying the write, then
        // prefer bytes another writer has put in the cache while a read was in flight.
        while let Some(index) = partial.into_iter().flatten().find(|&index| {
            offset < end
                && index * PAGE < end.min(pages.size)
                && !pages.pages.contains_key(&index)
                && !loaded.iter().any(|(loaded, _)| *loaded == index)
        }) {
            drop(pages);
            if backing.is_none() {
                backing = Some(shared.read_backing());
                pages = shared.lock();
                continue;
            }
            let mut bytes = shared.cache.frames.zeroed();
            let read = shared.file.read_at(&mut bytes, index * PAGE);
            pages = shared.lock();
            match read {
                Ok(()) => loaded.push((index, bytes)),
                // The rest of the page can come from a concurrent writer.
                Err(_) if pages.pages.contains_key(&index) => {}
                Err(err) => {
                    end = offset.max(index * PAGE);
                    failed = Some(err);
                }
            }
        }
        let number = shared.cache.writes.fetch_add(1, Ordering::Relaxed) + 1;
        let mut at = offset;
        let (mut added, mut dirtied) = (0, 0);
        while at < end {
            let index = at / PAGE;
            let within = (at % PAGE) as usize;
            let take = ((PAGE - at % PAGE).min(end - at)) as usize;
            let written = &buf[(at - offset) as usize..(at - offset) as usize + take];
            let page = match pages.pages.entry(index) {
                std::collections::btree_map::Entry::Occupied(page) => {
                    let page = page.into_mut();
                    page.bytes[within..within + take].copy_from_slice(written);
                    page
                }
                std::collections::btree_map::Entry::Vacant(vacant) => {
                    // cladefs: a frame filled whole, by the write, or by zeros, and what of the
                    // page the write leaves as the file holds it: never what a frame held before,
                    // another file's (TASK-433.9).
                    let bytes = if take == PAGE as usize {
                        shared.cache.frames.copied(written)
                    } else {
                        let mut bytes = loaded
                            .iter()
                            .position(|(loaded, _)| *loaded == index)
                            .map(|at| loaded.swap_remove(at).1)
                            .unwrap_or_else(|| shared.cache.frames.zeroed());
                        bytes[within..within + take].copy_from_slice(written);
                        bytes
                    };
                    added += 1;
                    vacant.insert(Page { bytes, dirty: None, last: 0, writing: false })
                }
            };
            if page.dirty.is_none() {
                dirtied += 1;
                page.dirty = Some(number);
            }
            page.last = number;
            at += take as u64;
        }
        // cladefs: what it wrote before a read failed counted as what it wrote whole is: the pages
        // it holds go uncounted no more, to be taken off the counts as they go (TASK-433.10).
        pages.size = pages.size.max(at);
        pages.dirty_pages += dirtied;
        if at > offset {
            pages.dirtied.get_or_insert_with(Instant::now);
        }
        let held = shared.cache.held.fetch_add(added * PAGE, Ordering::Relaxed) + added * PAGE;
        shared.cache.peak.fetch_max(held, Ordering::Relaxed);
        shared.dirty_add(dirtied * PAGE, pages.failing.is_some());
        drop(pages);
        drop(backing);
        shared.cache.dirtied_on(shared.flusher.as_deref());
        failed.map_or(Ok(()), Err)
    }

    /// The pages in the range written back, and the file told to write them back too, as one
    /// with a cache of its own does.
    fn write_back(&self, offset: u64, len: u64) -> io::Result<()> {
        self.shared.flush(offset..offset + len, None)?;
        self.shared.file.write_back(offset, len)
    }

    /// Has the flusher begin writing back the pages in the range, waiting for none of it, before
    /// anything else it writes back, as `sync_file_range`'s `SYNC_FILE_RANGE_WRITE` has the host's
    /// begin it: so that the writer's [`File::write_back`] of the range after, which waits, finds
    /// it written, or in flight, rather than write it all itself then (TASK-433.8). Until then
    /// this woke the flusher alone, which wrote nothing under [`Options::background`]: the writer
    /// then wrote back every range it waited for, a MiB at a time, cell-bench `burst`'s writes
    /// 2.3x slower and its commit 5x on the test host against the host's page cache. A failure is
    /// the file's next sync's. With no flusher, nothing: its writers write back as they go.
    fn write_behind(&self, offset: u64, len: u64) -> io::Result<()> {
        if let Some(flusher) = &self.shared.flusher {
            let paged: Arc<dyn Paged> = self.shared.clone();
            flusher.begun.lock().unwrap().push_back((Arc::downgrade(&paged), offset..offset + len));
            flusher.wake();
        }
        Ok(())
    }

    fn size(&self) -> io::Result<u64> {
        let size = self.shared.lock().size;
        Ok(size.max(self.shared.file.size()?))
    }

    fn sync(&self) -> io::Result<()> {
        let shared = &*self.shared;
        // What was written before it, a snapshot: writes after it are the next sync's.
        let through = shared.cache.writes.load(Ordering::Relaxed);
        shared.flush(0..u64::MAX, Some(through))?;
        // Written back whole since: what failed before is on the file now.
        shared.lock().failed = None;
        shared.file.sync()
    }

    /// cladefs: its pages in the range written back first, then let go, clean, so that none hides
    /// the hole (TASK-698.2).
    fn punch(&self, offset: u64, len: u64) -> io::Result<()> {
        let shared = &*self.shared;
        let range = offset..offset + len;
        let blocks = offset / PAGE..range.end.div_ceil(PAGE);
        let exclusive = shared.writeback();
        loop {
            shared.write_locked_runs(range.clone(), None, None, &exclusive)?;
            let mut pages = shared.lock();
            // If a write landed during write-back, write it back before giving the blocks back.
            // Keep the pages locked across the punch so a partial write sees the hole's zeros.
            if pages.pages.range(blocks.clone()).any(|(_, page)| page.dirty.is_some()) {
                continue;
            }
            shared.file.punch(offset, len)?;
            let gone: Vec<u64> = pages.pages.range(blocks).map(|(&index, _)| index).collect();
            for index in &gone {
                pages.pages.remove(index);
            }
            shared.cache.held.fetch_sub(gone.len() as u64 * PAGE, Ordering::Relaxed);
            return Ok(());
        }
    }

    /// cladefs: its pages written back first, which the file takes once they are.
    fn allocated(&self) -> io::Result<u64> {
        self.shared.flush(0..u64::MAX, None)?;
        self.shared.file.allocated()
    }
}

#[cfg(test)]
mod tests {
    mod audit_tests;

    use super::*;
    use crate::dir::Dir as _;
    use crate::mem::MemDisk;

    fn cache(options: Options) -> Arc<PageCache> {
        PageCache::new(options)
    }

    fn pattern(seed: u8, len: usize) -> Vec<u8> {
        (0..len).map(|at| seed.wrapping_add((at % 251) as u8)).collect()
    }

    /// cladefs: the frames of pages gone hold the next pages written, never what they held
    /// before, another file's (TASK-433.9).
    #[test]
    fn frames_of_pages_gone_hold_the_next_and_none_of_their_bytes() {
        let dir = MemDisk::new();
        let cache = cache(Options { expire: Duration::from_secs(3600), ..Options::default() });
        let gone = Cached::new(dir.create("gone", true).unwrap(), &cache).unwrap();
        gone.write_at(&pattern(5, 2 * PAGE as usize), 0).unwrap();
        gone.sync().unwrap();
        drop(gone);
        assert_eq!(cache.bytes(), 0);

        let next = Cached::new(dir.create("next", true).unwrap(), &cache).unwrap();
        next.write_at(&pattern(6, 10), 100).unwrap();
        let mut read = vec![0; PAGE as usize];
        next.read_at(&mut read, 0).unwrap();
        let mut model = vec![0; 100];
        model.extend(pattern(6, 10));
        model.resize(PAGE as usize, 0);
        assert_eq!(read, model, "zeros but what was written, in a frame that held another file's");
        assert_eq!(cache.bytes(), PAGE);
    }

    /// cladefs: a file whose reads fail, its writes and the rest as `F`'s.
    struct FailingReads<F>(F);

    impl<F: File> File for FailingReads<F> {
        fn read_at(&self, _buf: &mut [u8], _offset: u64) -> io::Result<()> {
            Err(io::Error::other("a read refused"))
        }

        fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
            self.0.write_at(buf, offset)
        }

        fn size(&self) -> io::Result<u64> {
            self.0.size()
        }

        fn sync(&self) -> io::Result<()> {
            self.0.sync()
        }
    }

    /// cladefs: a write whose read of what the file holds of a partial page fails counts the pages
    /// it held and dirtied before, which their going takes off the counts after, none wrapping
    /// (TASK-433.10).
    #[test]
    fn a_write_failing_its_read_counts_what_it_held() {
        let dir = MemDisk::new();
        let cache = cache(Options { expire: Duration::from_secs(3600), ..Options::default() });
        let raw = dir.create("f", true).unwrap();
        raw.write_at(&pattern(7, 3 * PAGE as usize), 0).unwrap();
        let file = Cached::new(FailingReads(raw), &cache).unwrap();
        // Two whole pages, then part of a third, which the file holds: its read fails.
        let err = file.write_at(&pattern(8, 2 * PAGE as usize + 10), 0).unwrap_err();
        assert_eq!(err.to_string(), "a read refused");
        assert_eq!(cache.usage().held, 2 * PAGE);
        assert_eq!(cache.usage().dirty, 2 * PAGE);
        file.sync().unwrap();
        assert_eq!(cache.usage().dirty, 0);
        assert_eq!(cache.evict_to(0), 0);
        assert_eq!(cache.usage().held, 0);
    }

    /// A write over part of a page keeps the rest as the file holds it; one past the end, zeros
    /// between.
    #[test]
    fn partial_pages_keep_what_the_file_holds() {
        let dir = MemDisk::new();
        let cache = cache(Options::default());
        let raw = dir.create("f", true).unwrap();
        raw.write_at(&pattern(2, 2 * PAGE as usize), 0).unwrap();
        let file = Cached::new(raw, &cache).unwrap();
        file.write_at(&pattern(3, 10), 100).unwrap();
        file.write_at(&pattern(4, 10), 3 * PAGE + 5).unwrap();
        let mut model = pattern(2, 2 * PAGE as usize);
        model.resize(3 * PAGE as usize + 15, 0);
        model[100..110].copy_from_slice(&pattern(3, 10));
        model[3 * PAGE as usize + 5..].copy_from_slice(&pattern(4, 10));
        let mut read = vec![0; model.len()];
        file.read_at(&mut read, 0).unwrap();
        assert_eq!(read, model);
        file.sync().unwrap();
        let mut under = vec![0; model.len()];
        file.file().read_at(&mut under, 0).unwrap();
        assert_eq!(under, model);
    }

    #[cfg(feature = "shuttle")]
    mod shuttle_tests {
        use super::*;

        #[test]
        fn a_cold_read_or_partial_write_rechecks_a_page_written_during_its_read() {
            use shuttle::sync::mpsc::{Receiver, Sender, channel};

            struct PausedRead<F> {
                file: F,
                pause: Mutex<Option<(Sender<()>, Receiver<()>)>>,
            }

            impl<F: File> File for PausedRead<F> {
                fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
                    self.file.read_at(buf, offset)?;
                    let pause = self.pause.lock().unwrap().take();
                    if let Some((started, resume)) = pause {
                        started.send(()).unwrap();
                        resume.recv().unwrap();
                    }
                    Ok(())
                }

                fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
                    self.file.write_at(buf, offset)
                }

                fn size(&self) -> io::Result<u64> {
                    self.file.size()
                }

                fn sync(&self) -> io::Result<()> {
                    self.file.sync()
                }
            }

            for partial in [false, true] {
                shuttle::check_random(
                    move || {
                        let dir = MemDisk::new();
                        let raw = dir.create("f", true).unwrap();
                        raw.write_at(&[1; PAGE as usize], 0).unwrap();
                        let (started, entered) = channel();
                        let (resume, resumed) = channel();
                        let backing =
                            PausedRead { file: raw, pause: Mutex::new(Some((started, resumed))) };
                        let file =
                            Cached::new(backing, &PageCache::without_flusher(Options::default()))
                                .unwrap();

                        let cold = file.clone();
                        let reader = shuttle::thread::spawn(move || {
                            if partial {
                                cold.write_at(&[3; 10], 100).unwrap();
                            } else {
                                let mut read = [0; PAGE as usize];
                                cold.read_at(&mut read, 0).unwrap();
                                assert_eq!(read, [2; PAGE as usize]);
                            }
                        });
                        entered.recv().unwrap();
                        file.write_at(&[2; PAGE as usize], 0).unwrap();
                        resume.send(()).unwrap();
                        reader.join().unwrap();

                        file.sync().unwrap();
                        let mut expected = [2; PAGE as usize];
                        if partial {
                            expected[100..110].fill(3);
                        }
                        let mut backed = [0; PAGE as usize];
                        file.file().read_at(&mut backed, 0).unwrap();
                        assert_eq!(backed, expected);
                    },
                    10,
                );
            }
        }
    }

    /// Pages dirtied go unwritten past a crash before a sync, as the host's page cache's did; a
    /// crash after the sync keeps them.
    #[test]
    fn writes_unsynced_are_lost_in_a_crash() {
        let dir = MemDisk::new();
        let cache = cache(Options { expire: Duration::from_secs(3600), ..Options::default() });
        let file = Cached::new(dir.create("f", true).unwrap(), &cache).unwrap();
        dir.sync_dir("").unwrap();
        file.write_at(&pattern(5, PAGE as usize), 0).unwrap();
        file.sync().unwrap();
        file.write_at(&pattern(6, PAGE as usize), PAGE).unwrap();
        let crashed = dir.crash(1);
        let mut read = vec![0; 2 * PAGE as usize];
        crashed.open("f", true).unwrap().read_at(&mut read, 0).unwrap();
        assert_eq!(&read[..PAGE as usize], &pattern(5, PAGE as usize)[..]);
        assert_eq!(&read[PAGE as usize..], &vec![0; PAGE as usize][..]);
    }

    /// The flusher writes back what has been dirty past the expiry, with no sync.
    #[test]
    fn the_flusher_writes_back_what_expires() {
        let dir = MemDisk::new();
        let cache = cache(Options { expire: Duration::from_millis(20), ..Options::default() });
        let file = Cached::new(dir.create("f", true).unwrap(), &cache).unwrap();
        file.write_at(&pattern(7, 2 * PAGE as usize), 0).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while cache.usage().dirty > 0 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(cache.usage().dirty, 0);
        let mut under = vec![0; 2 * PAGE as usize];
        file.file().read_at(&mut under, 0).unwrap();
        assert_eq!(under, pattern(7, 2 * PAGE as usize));
    }

    /// Sized to a governor's room, the cache's bounds are a tenth, a fifth and all of it, the first
    /// two none below what it was made with (TASK-433.8); sized down again, those two go back to
    /// them, and what it holds at most is the room, though below what it was made with (board
    /// 6001).
    #[test]
    fn caches_size_to_the_room_a_governor_leaves() {
        let cache = PageCache::without_flusher(Options::default());
        let made = Options::default();
        cache.size_to(4 << 30);
        let sized = cache.options();
        assert_eq!(
            (sized.background, sized.hard, sized.most, sized.expire),
            ((4 << 30) / 10, (4 << 30) / 5, 4 << 30, made.expire)
        );
        cache.size_to(64 << 20);
        let sized = cache.options();
        assert_eq!(
            (sized.background, sized.hard, sized.most),
            (made.background, made.hard, 64 << 20)
        );
    }

    /// A range written behind is written back by the flusher, ahead of any threshold or expiry,
    /// waiting for none of it: the rest of the file stays dirty (TASK-433.8).
    #[test]
    fn ranges_written_behind_are_written_back_by_the_flusher() {
        let dir = MemDisk::new();
        let cache = cache(Options { expire: Duration::from_secs(3600), ..Options::default() });
        let file = Cached::new(dir.create("f", true).unwrap(), &cache).unwrap();
        let data = pattern(9, 2 * PAGE as usize);
        file.write_at(&data, 0).unwrap();
        file.write_behind(0, PAGE).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while cache.usage().dirty > PAGE && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(cache.usage().dirty, PAGE);
        let mut under = vec![1; 2 * PAGE as usize];
        file.file().read_at(&mut under, 0).unwrap();
        assert_eq!(&under[..PAGE as usize], &data[..PAGE as usize]);
        assert_eq!(&under[PAGE as usize..], &vec![0; PAGE as usize][..], "the rest unwritten");
    }

    /// A file's pages dropped, the dirty ones unwritten, as a removed file's are.
    #[test]
    fn cancelled_pages_are_never_written() {
        let dir = MemDisk::new();
        let cache = cache(Options { expire: Duration::from_secs(3600), ..Options::default() });
        let file = Cached::new(dir.create("f", true).unwrap(), &cache).unwrap();
        file.write_at(&pattern(8, PAGE as usize), 0).unwrap();
        file.cancel();
        let usage = cache.usage();
        assert_eq!((usage.held, usage.dirty, usage.writing), (0, 0, 0));
        file.sync().unwrap();
        let mut under = vec![1; PAGE as usize];
        file.file().read_at(&mut under, 0).unwrap();
        assert_eq!(under, vec![0; PAGE as usize]);
    }

    /// Writes from many threads, synced as they go, each read back as written, the file holding
    /// all of it once synced: writers going on while write-backs are in flight.
    #[test]
    fn concurrent_writes_and_syncs_keep_every_write() {
        let dir = MemDisk::new();
        let cache = cache(Options { background: 4 * PAGE, hard: 16 * PAGE, ..Options::default() });
        let file = Cached::new(dir.create("f", true).unwrap(), &cache).unwrap();
        std::thread::scope(|scope| {
            for thread in 0..4u64 {
                let file = file.clone();
                scope.spawn(move || {
                    for round in 0..50u64 {
                        let at = (thread * 50 + round) * PAGE;
                        let data = pattern((thread * 7 + round) as u8, PAGE as usize);
                        file.write_at(&data, at).unwrap();
                        if round % 10 == 0 {
                            file.sync().unwrap();
                        }
                    }
                });
            }
        });
        file.sync().unwrap();
        for thread in 0..4u64 {
            for round in 0..50u64 {
                let at = (thread * 50 + round) * PAGE;
                let mut under = vec![0; PAGE as usize];
                file.file().read_at(&mut under, at).unwrap();
                assert_eq!(under, pattern((thread * 7 + round) as u8, PAGE as usize), "at {at}");
            }
        }
        assert_eq!(cache.usage().dirty, 0);
    }

    /// A file whose keys change as they are read, as a file other threads use and write does
    /// while the cache orders its files: each read of either key, its own or another file's,
    /// changes every file's, to one that orders the files anew, as no file's use or write since
    /// can be ruled out (TASK-493). Counts what the cache asks of it.
    struct Shifting {
        reads: Arc<AtomicU64>,
        since: Instant,
        asked: AtomicU64,
    }

    impl Paged for Shifting {
        fn used(&self) -> u64 {
            self.read()
        }

        fn dirtied(&self) -> Option<Instant> {
            Some(self.since + Duration::from_nanos(self.read() >> 4))
        }

        fn write_back(&self, _most: u64) -> io::Result<()> {
            self.asked.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn write_range(&self, _range: Range<u64>) -> io::Result<()> {
            self.asked.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn evict_clean(&self) {
            self.asked.fetch_add(1, Ordering::Relaxed);
        }

        fn write_back_all(&self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Shifting {
        /// A key as of this read: the read's number, scrambled (splitmix64's finish).
        fn read(&self) -> u64 {
            let mut key = self.reads.fetch_add(1, Ordering::Relaxed);
            key = (key ^ (key >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            key = (key ^ (key >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            key ^ (key >> 31)
        }
    }

    /// `count` [`Shifting`] files, held by the caller, in `cache`.
    fn shifting(cache: &PageCache, count: usize) -> Vec<Arc<Shifting>> {
        let (reads, since) = (Arc::new(AtomicU64::new(0)), Instant::now());
        let files: Vec<Arc<Shifting>> = (0..count)
            .map(|_| {
                let file = Shifting { reads: reads.clone(), since, asked: AtomicU64::new(0) };
                Arc::new(file)
            })
            .collect();
        for file in &files {
            let paged: Arc<dyn Paged> = file.clone();
            cache.files.lock().unwrap().push(Arc::downgrade(&paged));
        }
        files
    }

    /// Each file asked once, as many times as the cache went through them.
    fn asked_each(files: &[Arc<Shifting>], times: u64) {
        for (at, file) in files.iter().enumerate() {
            assert_eq!(file.asked.load(Ordering::Relaxed), times, "file {at}");
        }
    }

    /// Eviction, writers' write-back and the flusher order the files by keys other threads change
    /// as they go: each by its key as it was once, not as each comparison finds it, which is no
    /// order at all, and a sort given none panics (Rust's "user-provided comparison function does
    /// not correctly implement a total order"), as the flusher did on the test host, dying
    /// (TASK-493).
    #[test]
    fn files_ordered_by_keys_changing_as_they_are_read() {
        for count in 0..=256 {
            let cache = PageCache::without_flusher(Options::default());
            let files = shifting(&cache, count);
            cache.held.store(u64::MAX / 2, Ordering::Relaxed);
            cache.evict_to(0);
            asked_each(&files, 1);
            cache.dirty.store(u64::MAX / 2, Ordering::Relaxed);
            cache.write_back_to(0);
            asked_each(&files, 2);
            cache.flush_pass(Instant::now());
            asked_each(&files, 3);
        }
    }

    /// A file whose pages panic the cache as it looks at them: a bug the flusher's pass runs into.
    struct Panicking;

    impl Paged for Panicking {
        fn used(&self) -> u64 {
            0
        }

        fn dirtied(&self) -> Option<Instant> {
            panic!("a bug in a file's pages, as a test's");
        }

        fn write_back(&self, _most: u64) -> io::Result<()> {
            Ok(())
        }

        fn write_range(&self, _range: Range<u64>) -> io::Result<()> {
            Ok(())
        }

        fn evict_clean(&self) {}

        fn write_back_all(&self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A cache whose flusher was woken to a [`Panicking`] file, and the file, which the caller
    /// holds.
    fn flusher_panicking() -> (Arc<PageCache>, Arc<dyn Paged>) {
        let cache = cache(Options { expire: Duration::from_secs(3600), ..Options::default() });
        let file: Arc<dyn Paged> = Arc::new(Panicking);
        cache.files.lock().unwrap().push(Arc::downgrade(&file));
        cache.wake();
        (cache, file)
    }

    /// Whether `cache`'s flusher died within 10 s.
    fn died_within(cache: &PageCache) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !cache.flusher_died() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        cache.flusher_died()
    }

    /// A flusher a panic unwinds says it died, where nothing aborts the process: the cache knows
    /// it, and a writer held back at its bound panics, as nothing would write back for it, rather
    /// than wait for good, as it did, the flusher dead and none knowing (TASK-493).
    #[test]
    fn a_dead_flusher_is_known_and_holds_no_writer_for_good() {
        let (cache, _file) = flusher_panicking();
        assert!(died_within(&cache), "the flusher died unsaid");
        cache.dirty.store(cache.hard() + 1, Ordering::Relaxed);
        let held =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cache.throttle(|| None)))
                .unwrap_err();
        let said = (held.downcast_ref::<&str>().copied())
            .or_else(|| held.downcast_ref::<String>().map(String::as_str));
        assert_eq!(
            said,
            Some(
                "the page cache's flusher died: nothing writes back for a writer held at its bound"
            )
        );
        cache.dirty.store(0, Ordering::Relaxed);
    }

    /// cladefs: a writer held back past [`Options::hard`] dirty is a flush stall, counted with how
    /// long it waited, which the daemon's metrics say (board 8565); one let through, as under the
    /// bound, none. The writer here waits until the second look at its disk finds it failing.
    #[test]
    fn writers_held_back_are_stalls_counted_with_their_time() {
        let cache = cache(Options { background: PAGE, hard: 2 * PAGE, ..Options::default() });
        cache.throttle(|| None).unwrap();
        assert_eq!((cache.usage().stalls, cache.usage().stalled_ns), (0, 0), "none held");
        cache.dirty.store(cache.hard() + 1, Ordering::Relaxed);
        let looks = AtomicU64::new(0);
        std::thread::scope(|scope| {
            let held = scope.spawn(|| {
                let failing = || match looks.fetch_add(1, Ordering::Relaxed) {
                    0 => None,
                    _ => Some(io::Error::other("its disk failing")),
                };
                cache.throttle(failing)
            });
            // Woken again and again until it gives up, its second look.
            while !held.is_finished() {
                cache.drained();
                std::thread::yield_now();
            }
            assert_eq!(held.join().unwrap().unwrap_err().to_string(), "its disk failing");
        });
        let usage = cache.usage();
        assert_eq!(usage.stalls, 1);
        assert!(usage.stalled_ns > 0, "{usage:?}");
        cache.dirty.store(0, Ordering::Relaxed);
    }

    /// What [`child_flusher_panics`] does, if its parent sets it.
    const CHILD: &str = "EXTENT_STORE_PAGES_CHILD";

    /// Run by [`a_panic_on_the_flusher_aborts_the_daemon`], in a process of its own: aborting on
    /// panics holding what is shared beyond one cell, as the extent daemon does, its cache's
    /// flusher panicking; it says if the process lived on.
    #[test]
    #[ignore = "run by a_panic_on_the_flusher_aborts_the_daemon, in a process of its own"]
    fn child_flusher_panics() {
        if std::env::var(CHILD).is_err() {
            return;
        }
        crate::shared::abort_on_panics();
        let (cache, _file) = flusher_panicking();
        let died = died_within(&cache);
        println!("LIVED ON, the flusher dead: {died}");
    }

    /// A panic on the flusher aborts the extent daemon, which a panic holding a lock shared beyond
    /// one cell does (TASK-149.24), its log naming the flusher: every cell's files wait on it, and
    /// no cell's halt contains its death, which left the daemon going on with nothing writing back
    /// what was dirty but writers held at the bound (TASK-493).
    #[test]
    fn a_panic_on_the_flusher_aborts_the_daemon() {
        use std::os::unix::process::ExitStatusExt;
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "pages::tests::child_flusher_panics", "--ignored", "--nocapture"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let said =
            String::from_utf8_lossy(&output.stdout) + String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.signal(), Some(libc::SIGABRT), "{said}");
        assert!(crate::shared::aborted_holding(&said, FLUSHER), "{said}");
        assert!(!said.contains("LIVED ON"), "{said}");
    }

    /// Two distinct `HostDisk`s get independent flusher threads: a blocked write on disk A must not
    /// stall write-back on disk B.
    #[cfg(not(madsim))]
    #[test]
    fn two_disks_flush_concurrently() {
        use std::sync::mpsc;

        struct BlockingFile {
            entered: Mutex<Option<mpsc::SyncSender<()>>>,
            release: Mutex<mpsc::Receiver<()>>,
            wrote: AtomicU64,
        }

        impl File for BlockingFile {
            fn read_at(&self, buf: &mut [u8], _offset: u64) -> io::Result<()> {
                buf.fill(0);
                Ok(())
            }
            fn write_at(&self, buf: &[u8], _offset: u64) -> io::Result<()> {
                if let Some(tx) = self.entered.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                let _ = self.release.lock().unwrap().recv();
                self.wrote.fetch_add(buf.len() as u64, Ordering::Relaxed);
                Ok(())
            }
            fn size(&self) -> io::Result<u64> {
                Ok(0)
            }
            fn sync(&self) -> io::Result<()> {
                Ok(())
            }
        }

        struct CountingFile {
            wrote: Arc<AtomicU64>,
        }

        impl File for CountingFile {
            fn read_at(&self, buf: &mut [u8], _offset: u64) -> io::Result<()> {
                buf.fill(0);
                Ok(())
            }
            fn write_at(&self, buf: &[u8], _offset: u64) -> io::Result<()> {
                self.wrote.fetch_add(buf.len() as u64, Ordering::Relaxed);
                Ok(())
            }
            fn size(&self) -> io::Result<u64> {
                Ok(0)
            }
            fn sync(&self) -> io::Result<()> {
                Ok(())
            }
        }

        let disk_a =
            bucket_cell::host_disk::HostDisk::in_memory(bucket_cell::host_disk::Sample::default);
        let disk_b =
            bucket_cell::host_disk::HostDisk::in_memory(bucket_cell::host_disk::Sample::default);
        assert!(!Arc::ptr_eq(&disk_a, &disk_b));

        let cache = cache(Options {
            background: PAGE,
            hard: 2 * PAGE,
            expire: Duration::from_secs(3600),
            ..Options::default()
        });
        struct ReleaseGuard(Option<mpsc::SyncSender<()>>);
        impl ReleaseGuard {
            fn release(&mut self) {
                if let Some(tx) = self.0.take() {
                    let _ = tx.send(());
                }
            }
        }
        impl Drop for ReleaseGuard {
            fn drop(&mut self) {
                self.release();
            }
        }

        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let file_a = Cached::on_disk(
            BlockingFile {
                entered: Mutex::new(Some(entered_tx)),
                release: Mutex::new(release_rx),
                wrote: AtomicU64::new(0),
            },
            &cache,
            Some(disk_a.clone()),
        )
        .unwrap();
        // Declared after `file_a` so any early panic releases disk A's gate before `file_a` drops.
        let mut release_guard = ReleaseGuard(Some(release_tx));
        let file_a2 = Cached::on_disk(
            CountingFile { wrote: Arc::new(AtomicU64::new(0)) },
            &cache,
            Some(disk_a),
        )
        .unwrap();
        let flusher_a = file_a2.shared.flusher.clone().unwrap();
        let wrote_b = Arc::new(AtomicU64::new(0));
        let file_b =
            Cached::on_disk(CountingFile { wrote: wrote_b.clone() }, &cache, Some(disk_b)).unwrap();
        assert_eq!(cache.active_disks.load(Ordering::Acquire), 2);

        // Dirty 3 * PAGE on disk A (> global `hard = 2 * PAGE` and > per-disk `disk_hard = PAGE`)
        // in one write and block disk A's flusher inside `write_at`.
        file_a.write_at(&vec![1u8; (3 * PAGE) as usize], 0).unwrap();
        file_a.write_behind(0, 3 * PAGE).unwrap();
        // Wait until disk A's flusher is blocked inside write_at.
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            cache.usage().dirty > cache.hard(),
            "disk A must exceed global hard while blocked in flight"
        );

        // Foreground writes on healthy disk B must still complete and flush via disk B's flusher
        // without hanging on disk A's stalled write-back (board 11939). Run them on a worker so a
        // regression fails the timeout instead of deadlocking the test thread.
        let (b_done_tx, b_done_rx) = mpsc::sync_channel(1);
        let b_thread = std::thread::spawn(move || {
            for i in 0..4u64 {
                file_b.write_at(&vec![2u8; PAGE as usize], i * PAGE).unwrap();
                file_b.write_behind(i * PAGE, PAGE).unwrap();
            }
            let _ = b_done_tx.send(());
            file_b
        });
        b_done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("disk B foreground writes must not hang while disk A is blocked past hard");
        let file_b = b_thread.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while wrote_b.load(Ordering::Relaxed) < 4 * PAGE && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            wrote_b.load(Ordering::Relaxed),
            4 * PAGE,
            "disk B flusher must finish while disk A is blocked past hard"
        );

        // Meanwhile, a second writer on stalled disk A must enter throttle_for's wait on disk A and
        // record its own stall once disk A drains.
        let stalls_before_a2 = cache.usage().stalls;
        let a2_done = Arc::new(AtomicBool::new(false));
        let a2_done_bg = a2_done.clone();
        let a2_thread = std::thread::spawn(move || {
            file_a2.write_at(&vec![3u8; PAGE as usize], 0).unwrap();
            a2_done_bg.store(true, Ordering::Release);
            file_a2
        });
        let wait_deadline = Instant::now() + Duration::from_secs(5);
        while flusher_a.waiters.load(Ordering::Acquire) == 0
            && !a2_done.load(Ordering::Acquire)
            && Instant::now() < wait_deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            flusher_a.waiters.load(Ordering::Acquire) >= 1 && !a2_done.load(Ordering::Acquire),
            "writer on stalled disk A must be waiting inside throttle_for before disk A drains"
        );
        release_guard.release();
        let file_a2 = a2_thread.join().unwrap();
        assert!(
            cache.usage().stalls > stalls_before_a2,
            "stalled disk A writer must record a stall of its own"
        );

        // Dropping all files on disk B returns its budget share immediately, and opening a Default
        // file alongside disk A counts both toward active_disks.
        drop(file_b);
        assert_eq!(cache.active_disks.load(Ordering::Acquire), 1);
        let file_default =
            Cached::new(CountingFile { wrote: Arc::new(AtomicU64::new(0)) }, &cache).unwrap();
        assert_eq!(cache.active_disks.load(Ordering::Acquire), 2);
        assert_eq!(cache.disk_hard(), PAGE);
        drop(file_default);
        assert_eq!(cache.active_disks.load(Ordering::Acquire), 1);
        assert_eq!(cache.disk_hard(), 2 * PAGE);
        drop(file_a2);
        drop(file_a);
        assert_eq!(cache.active_disks.load(Ordering::Acquire), 0);

        // Inline caches without flushers must not register virtual disks in `flushers`.
        let inline = PageCache::without_flusher(Options::default());
        for _ in 0..4 {
            let d = bucket_cell::host_disk::HostDisk::in_memory(
                bucket_cell::host_disk::Sample::default,
            );
            let f = Cached::on_disk(
                CountingFile { wrote: Arc::new(AtomicU64::new(0)) },
                &inline,
                Some(d),
            )
            .unwrap();
            drop(f);
        }
        assert!(
            inline.flushers.lock().unwrap().is_empty(),
            "inline caches must not retain virtual disk registrations"
        );

        // Repeated HostDisk creation/drop on the same filesystem reuses one Device flusher entry
        // rather than leaking a new flusher thread each time.
        let dir = tempfile::tempdir().unwrap();
        let before = cache.flushers.lock().unwrap().len();
        for _ in 0..8 {
            let host = bucket_cell::Host::new();
            let disk = bucket_cell::host_disk::HostDisk::of_on(&host, dir.path()).unwrap();
            let f = Cached::on_disk(
                CountingFile { wrote: Arc::new(AtomicU64::new(0)) },
                &cache,
                Some(disk),
            )
            .unwrap();
            drop(f);
        }
        assert_eq!(
            cache.flushers.lock().unwrap().len(),
            before + 1,
            "recreating HostDisk on the same st_dev must reuse one flusher"
        );
    }
}
