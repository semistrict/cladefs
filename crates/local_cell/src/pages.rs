// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the store's page cache (TASK-433): what holds the bytes its data files are written with
//! until they are on the host's disk, and for as long after as memory allows, in memory of the
//! process's own, which the extent daemon governs with the rest (`cladefs::cladefsd::memory`). Its
//! files bypass the host's page cache (`O_DIRECT`), so that no byte a store holds is held twice,
//! in two memories each budgeted as if the other weren't there. It does what the host's did for
//! them, as Linux's does (mm/filemap.c, mm/page-writeback.c, fs/fs-writeback.c):
//!
//! - **A write lands in memory and returns** ([`Cached::write_at`]): its pages are dirty, and
//!   the disk takes them later, coalesced. Reads see them at once ([`Cached::read_at`]); a read of
//!   what the cache holds not goes to the file, and leaves nothing cached: what is read again, the
//!   readers' own caches hold (the extent cache), so that nothing is held twice here either.
//! - **Write-back** goes in the file's order, contiguous dirty pages one write of up to
//!   [`CHUNK`], from memory aligned to blocks, as iomap's ioends coalesce. One write-back of a
//!   file at a time (`I_SYNC`): a sync, the flusher and a writer writing back take turns, each
//!   waiting for the last to end, which carries what it covers ([`Shared::writeback`]). A page
//!   written while its write-back is in flight is dirty again, and written again after: writers
//!   never wait on write-back (no stable pages). The flusher ([`PageCache`]'s thread) writes back
//!   first what a writer asked it to begin ([`Cached::write_behind`], as `sync_file_range`'s
//!   `SYNC_FILE_RANGE_WRITE` begins it), then what has been dirty longer than
//!   [`Options::expire`], and the oldest first while more than [`Options::background`] is dirty. A cache with no flusher ([`PageCache::without_flusher`]),
//!   as a directory whose I/O is all its callers' has ([`crate::Dir::concurrent`]), has the writer
//!   that takes it past [`Options::background`] write back itself, as Linux's writers did before
//!   per-device flushers: what it writes, and when, is then a function of the writes alone.
//! - **Writers are throttled**, not refused, past [`Options::hard`] dirty: they wait for
//!   write-back, as `balance_dirty_pages` has them.
//! - **A sync** ([`Cached::sync`]) writes back what was dirty as it began, waits for every write of
//!   the file in flight, the flusher's too, then syncs the file: what it covers is a snapshot, so
//!   that a writer going on never keeps it from ending (`PAGECACHE_TAG_TOWRITE`). cladefs: a
//!   write-back that failed leaves its pages dirty, which the sync after it writes back again: the
//!   sync fails only if that does (TASK-539.12). Linux's reports it once, errseq's, as it drops
//!   the pages it failed to write; before TASK-539.12 this did too ([`Pages::failed`]), which failed
//!   a store's flush, and the store, for a moment of a full disk long gone.
//! - **Eviction** takes clean pages alone, of the files used least recently first
//!   ([`PageCache::evict_to`]), never writing any: what is dirty is the flusher's to write.
//! - **Files are ordered by their keys as they were once** ([`PageCache::files_by`]): when they
//!   were used, when they were dirtied, which other threads change as the cache goes through them,
//!   so that no comparison sees another's file changed since (TASK-493).
//! - **The flusher's work is the process's** ([`FLUSHER`]): every store's files wait on it, so a
//!   panic on it, which no store's halt contains, aborts the extent daemon as one holding a lock
//!   shared beyond one store does ([`crate::shared`], TASK-149.24). Elsewhere, as in tests, its
//!   death is said ([`PageCache::flusher_died`]), and a writer it would have let go panics rather
//!   than wait for good.
//! - **A file removed** keeps its pages while it is open, as an unlinked inode does, and drops
//!   them with its last opening, the dirty ones unwritten (`truncate_inode_pages` at the last
//!   `iput`): a segment's raw file, framed and gone, never reaches the disk but what its syncs
//!   and the flusher took.
//!
//! Every directory a store opens holds its data files in one ([`crate::CachedDir`]): the host's
//! ([`crate::HostDir`]) in the process's ([`shared`]), and one in memory ([`crate::MemDir`]) in a
//! small one of its own with no flusher, so that the store's tests, its crash rounds and its
//! Shuttle schedules among them, run through it as the store does on a host.

use crate::dir::File;
use crate::frames::{Frame, Frames};
use crate::shared::Holding;
use crate::sync::{Condvar, Mutex, MutexGuard};
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

/// The cache's page: what it holds, writes back and evicts. A store's data files are written in
/// whole sectors of this size (TASK-411.8), so a write fills its pages whole but at a file's end.
pub const PAGE: u64 = 4096;

/// The most a write-back writes at once: contiguous dirty pages, one write.
pub const CHUNK: u64 = 1 << 20;

/// What a [`PageCache`]'s flusher holds for its life, as a lock shared beyond one store
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
}

/// A process's page cache: its files' pages, accounted together, written back by one flusher
/// thread, or by its writers if it has none, evicted together. One for the process ([`shared`]),
/// which the extent daemon governs.
pub struct PageCache {
    /// What it was made with: its expiry, and the least of its bounds ([`PageCache::size_to`]).
    options: Options,
    /// Its bounds as they are: [`Options::background`], [`Options::hard`] and [`Options::most`],
    /// raised from `options`' by a governor as memory allows ([`PageCache::size_to`]).
    background: AtomicU64,
    hard: AtomicU64,
    most: AtomicU64,
    /// Whether a flusher thread writes back for it; if not, its writers do.
    flusher: bool,
    /// Whether its flusher died, a panic unwinding it: nothing writes back for it since.
    flusher_died: AtomicBool,
    held: AtomicU64,
    peak: AtomicU64,
    dirty: AtomicU64,
    peak_dirty: AtomicU64,
    writing: AtomicU64,
    /// Each write's number, in order: what a sync covers, the writes before it.
    writes: AtomicU64,
    /// Each use's number, which says which files were used least recently.
    uses: AtomicU64,
    files: Mutex<Vec<Weak<dyn Paged>>>,
    /// The ranges of files writers asked to be written back, waiting for none of it, in the order
    /// asked: the flusher's to begin before anything else ([`Cached::write_behind`]).
    begun: Mutex<VecDeque<(Weak<dyn Paged>, Range<u64>)>>,
    /// Wakes the flusher.
    flush: Condvar,
    flushing: Mutex<bool>,
    /// Wakes writers held back.
    drained: Condvar,
    holding: Mutex<()>,
    /// cladefs: how many write-backs failed, and the last's error, which a writer held back while
    /// nothing drains is given ([`PageCache::throttle`], TASK-517).
    failures: AtomicU64,
    failure: Mutex<Option<(io::ErrorKind, String)>>,
    /// cladefs: what its pages are held in: frames of slabs of its own, those of pages gone
    /// holding the next (TASK-433.9).
    frames: Arc<Frames>,
}

/// The process's page cache, which every store's files share ([`crate::HostDir`]).
pub fn shared() -> &'static Arc<PageCache> {
    static SHARED: std::sync::LazyLock<Arc<PageCache>> =
        std::sync::LazyLock::new(|| PageCache::new(Options::default()));
    &SHARED
}

impl PageCache {
    /// A cache going by `options`, its flusher started. Not under Shuttle, whose schedules start
    /// no thread of their own: [`PageCache::without_flusher`].
    pub fn new(options: Options) -> Arc<Self> {
        let cache = Self::made(options, true);
        let flusher = Arc::downgrade(&cache);
        std::thread::Builder::new()
            .name("store flusher".into())
            .spawn(move || flush_while(flusher))
            .expect("starting the page cache's flusher");
        cache
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
            writes: AtomicU64::new(0),
            uses: AtomicU64::new(0),
            files: Mutex::default(),
            begun: Mutex::default(),
            flush: Condvar::new(),
            flushing: Mutex::new(false),
            drained: Condvar::new(),
            holding: Mutex::new(()),
            failures: AtomicU64::new(0),
            failure: Mutex::new(None),
            frames: Frames::new(),
        })
    }

    /// What it holds.
    pub fn usage(&self) -> Usage {
        Usage {
            held: self.held.load(Ordering::Relaxed),
            dirty: self.dirty.load(Ordering::Relaxed),
            writing: self.writing.load(Ordering::Relaxed),
            peak: self.peak.load(Ordering::Relaxed),
            peak_dirty: self.peak_dirty.load(Ordering::Relaxed),
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
    /// sizes its page cache to the memory free (TASK-433.8): it holds as much as `room`, the
    /// flusher writes back past a tenth of it dirty, and writers wait past a fifth
    /// (`vm.dirty_background_ratio` and `vm.dirty_ratio`'s defaults), none less than what it was
    /// made with ([`Options`]). The governor evicts it to `room` as well ([`PageCache::evict_to`]),
    /// so that what this allows past `room` under pressure is dirty pages alone, to be written.
    /// With the fixed bounds alone, a burst of 256 MiB, as cell-bench's, was evicted as it was
    /// written, and read back from the disk to be framed: the commit after it 2-4x slower on
    /// ubu1 than through the host's page cache.
    pub fn size_to(&self, room: u64) {
        let floors = self.options;
        self.background.store((room / 10).max(floors.background), Ordering::Relaxed);
        self.hard.store((room / 5).max(floors.hard), Ordering::Relaxed);
        self.most.store(room.max(floors.most), Ordering::Relaxed);
        self.drained();
    }

    fn background(&self) -> u64 {
        self.background.load(Ordering::Relaxed)
    }

    fn hard(&self) -> u64 {
        self.hard.load(Ordering::Relaxed)
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
        if self.bytes() <= target {
            return self.bytes();
        }
        for file in self.files_by(|file| file.used()) {
            if self.bytes() <= target {
                break;
            }
            file.evict_clean();
        }
        if self.bytes() > target {
            self.wake();
        }
        self.bytes()
    }

    /// Writes back every dirty page of every file, waiting for every write-back in flight: what
    /// a reader of the files behind the stores' backs needs first, as a test damaging one, or the
    /// daemon counting what tenants' files take on disk. A file failing leaves the others written;
    /// the first failure is returned, and each file's next sync reports its own.
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

    /// Wakes the flusher, if it has one.
    fn wake(&self) {
        if !self.flusher {
            return;
        }
        *self.flushing.lock().unwrap() = true;
        self.flush.notify_one();
    }

    /// Has the flusher begin writing back `range` of `file`, before anything else it writes back,
    /// if it has one; with none, nothing: its writers write back past [`Options::background`].
    fn begin(&self, file: Weak<dyn Paged>, range: Range<u64>) {
        if !self.flusher {
            return;
        }
        self.begun.lock().unwrap().push_back((file, range));
        self.wake();
    }

    /// Writes back, on the caller's thread, the files dirty the longest, a few chunks of each in
    /// turn, until no more than `dirty` bytes are dirty, or a write-back fails, which the file's
    /// next sync reports.
    fn write_back_to(&self, dirty: u64) {
        for file in self.files_by(|file| file.dirtied()) {
            if self.dirty.load(Ordering::Relaxed) <= dirty {
                return;
            }
            if file.dirtied().is_none() {
                continue;
            }
            if let Err(err) = file.write_back(16 * CHUNK) {
                return log::warn!("extent store: writing back a file's pages: {err}");
            }
        }
    }

    /// Holds a writer about to dirty pages back while more than [`Options::hard`] is dirty, until
    /// write-back brings it under, the flusher woken. With no flusher, its writers keep it under
    /// [`Options::background`] as they go ([`PageCache::dirtied`]); what failed to be written
    /// stays dirty, which none waits for. Its flusher dead, it panics, as none would let it go.
    /// cladefs: a write-back failing while it waits, and nothing drained meanwhile, it gives the
    /// writer that failure, which dirties nothing, rather than hold it for as long as the disk
    /// refuses: a disk full, or failing, held every store's writers sharing the cache (TASK-517).
    fn throttle(&self) -> io::Result<()> {
        if !self.flusher || self.dirty.load(Ordering::Relaxed) <= self.hard() {
            return Ok(());
        }
        self.wake();
        let failures = self.failures.load(Ordering::Relaxed);
        let mut holding = self.holding.lock().unwrap();
        while self.dirty.load(Ordering::Relaxed) > self.hard() {
            assert!(
                !self.flusher_died(),
                "the page cache's flusher died: nothing writes back for a writer held at its bound"
            );
            let dirty = self.dirty.load(Ordering::Relaxed);
            holding = self.drained.wait_timeout(holding, Duration::from_millis(10)).unwrap().0;
            let drained = self.dirty.load(Ordering::Relaxed) < dirty;
            if !drained && self.failures.load(Ordering::Relaxed) > failures {
                let failure = self.failure.lock().unwrap().clone();
                let (kind, said) = failure.expect("a failure counted is kept");
                return Err(io::Error::new(kind, format!("writing back the page cache: {said}")));
            }
            self.wake();
        }
        Ok(())
    }

    /// cladefs: a write-back failed with `err`: what a writer held back is given, if nothing drains.
    fn failed(&self, err: &io::Error) {
        *self.failure.lock().unwrap() = Some((err.kind(), err.to_string()));
        self.failures.fetch_add(1, Ordering::Relaxed);
    }

    /// Pages became clean, or went: writers held back may go on.
    fn drained(&self) {
        let _holding = self.holding.lock().unwrap();
        self.drained.notify_all();
    }

    /// Pages were dirtied: the flusher writes back once more than [`Options::background`] is
    /// dirty, or the writer, with none; and clean ones are evicted once more than
    /// [`Options::most`] is held.
    fn dirtied(&self) {
        if self.dirty.load(Ordering::Relaxed) > self.background() {
            if self.flusher {
                self.wake();
            } else {
                self.write_back_to(self.background());
            }
        }
        if self.bytes() > self.most() {
            self.evict_to(self.most());
        }
    }
}

impl PageCache {
    /// A pass of the flusher's, as of `now`: it writes back the ranges writers asked it to begin
    /// ([`PageCache::begin`]), in the order asked; then what has been dirty longer than
    /// [`Options::expire`], and, while more than [`Options::background`] is dirty, the files
    /// dirty the longest, 16 [`CHUNK`]s' worth of writes each in turn.
    fn flush_pass(&self, now: Instant) {
        loop {
            let Some((file, range)) = self.begun.lock().unwrap().pop_front() else { break };
            let Some(file) = file.upgrade() else { continue };
            if let Err(err) = file.write_range(range) {
                log::warn!("extent store: writing back a range of a file's pages: {err}");
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
    }
}

/// The flusher: while the cache is there, a pass ([`PageCache::flush_pass`]) each time it is
/// woken, or a quarter of [`Options::expire`] after the last. The process's work for its life
/// ([`FLUSHER`]); unwound by a panic, it says so ([`Dying`]).
fn flush_while(cache: Weak<PageCache>) {
    let _work = Holding::work(FLUSHER);
    let _dying = Dying { cache: cache.clone() };
    loop {
        let Some(cache) = cache.upgrade() else { return };
        {
            let woken = cache.flushing.lock().unwrap();
            let interval = cache.options.expire / 4;
            let (mut woken, _) =
                cache.flush.wait_timeout_while(woken, interval, |woken| !*woken).unwrap();
            *woken = false;
        }
        cache.flush_pass(Instant::now());
    }
}

/// A flusher's death, said as a panic unwinds it, where the panic did not abort the process: the
/// cache knows it ([`PageCache::flusher_died`]), the log says it, and writers held back, woken,
/// find it out ([`PageCache::throttle`]).
struct Dying {
    cache: Weak<PageCache>,
}

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

/// A file's pages, and what the cache knows of the file.
struct Pages {
    pages: BTreeMap<u64, Page>,
    /// The file's size: its own, or past it, what was written.
    size: u64,
    /// When the oldest of its dirty pages was dirtied.
    dirtied: Option<Instant>,
    /// A write-back that failed, which the next sync reports, once.
    failed: Option<io::Error>,
}

/// A file, `F`, through the page cache: what [`File`] it is to its store.
pub struct Cached<F: File> {
    shared: Arc<Shared<F>>,
}

/// What a file's openings share: one cache of its pages, however many times it is opened, and
/// whatever its name ([`crate::HostDir`]'s files, by path).
pub(crate) struct Shared<F: File> {
    file: F,
    cache: Arc<PageCache>,
    pages: Mutex<Pages>,
    /// Held by its write-back, one at a time (`I_SYNC`): what a sync, or a dropping of pages,
    /// waits on for the write-back in flight to end. A lock, not a condition variable, so that
    /// whatever wakes a thread waiting on it, it waits on (Shuttle's scoped threads wake the scope's
    /// thread as the last ends, whatever it waits on). Taken before its pages' lock.
    writeback: Mutex<()>,
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
        let size = file.size()?;
        let shared = Arc::new(Shared {
            file,
            cache: cache.clone(),
            pages: Mutex::new(Pages { pages: BTreeMap::new(), size, dirtied: None, failed: None }),
            writeback: Mutex::new(()),
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

    /// Writes back the dirty pages in `range`: what a read of the file's own, around the cache, is
    /// to be preceded by, so that it reads what was written (an image's device,
    /// `cladefs::storage::image`).
    pub fn settle(&self, range: Range<u64>) -> io::Result<()> {
        self.shared.flush(range, None)
    }

    /// Writes back the dirty pages in `range`, drops every page there, and takes the file to be as
    /// long as `range` goes at least: what a write of the file's own, around the cache, is to be
    /// preceded by, so that no page hides what it writes, and reads see it all.
    pub fn settle_for_write(&self, range: Range<u64>) -> io::Result<()> {
        self.shared.flush(range.clone(), None)?;
        self.shared.drop_pages(range.clone());
        let mut pages = self.shared.lock();
        pages.size = pages.size.max(range.end);
        Ok(())
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
    /// Its write-back, taken: none other of it in flight.
    fn writeback(&self) -> MutexGuard<'_, ()> {
        self.writeback.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
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
        for index in doomed {
            let page = pages.pages.remove(&index).unwrap();
            self.cache.held.fetch_sub(PAGE, Ordering::Relaxed);
            if page.dirty.is_some() {
                self.cache.dirty.fetch_sub(PAGE, Ordering::Relaxed);
            }
        }
        if !pages.pages.values().any(|page| page.dirty.is_some()) {
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
        self.write_runs(range, through, None)
    }

    fn write_runs(
        &self,
        range: Range<u64>,
        through: Option<u64>,
        mut most: Option<u64>,
    ) -> io::Result<()> {
        let (first, last) = (range.start / PAGE, range.end.div_ceil(PAGE));
        let covers =
            |page: &Page| page.dirty.is_some_and(|dirty| through.is_none_or(|t| dirty <= t));
        let _writeback = self.writeback();
        let mut pages = self.lock();
        loop {
            if most.is_some_and(|most| most == 0) {
                return Ok(());
            }
            // The first run of pages it covers not in flight, contiguous, up to a chunk.
            let mut run: Vec<u64> = Vec::new();
            for (&index, page) in pages.pages.range(first..last) {
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
            for &(index, last) in &written {
                let Some(page) = pages.pages.get_mut(&index) else { continue };
                if wrote.is_err() {
                    continue;
                }
                if page.last == last {
                    page.dirty = None;
                    self.cache.dirty.fetch_sub(PAGE, Ordering::Relaxed);
                } else {
                    // Written again while it was written back: what it holds since is the next
                    // write-back's.
                    page.dirty = Some(last + 1);
                }
            }
            if !pages.pages.values().any(|page| page.dirty.is_some()) {
                pages.dirtied = None;
            }
            self.cache.drained();
            if let Err(err) = wrote {
                let kept = io::Error::new(err.kind(), err.to_string());
                pages.failed.get_or_insert(kept);
                self.cache.failed(&err);
                return Err(err);
            }
            if let Some(most) = &mut most {
                *most = most.saturating_sub(len);
            }
        }
        Ok(())
    }
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
        if !self.unlinked.load(Ordering::Relaxed)
            && !std::thread::panicking()
            && let Err(err) = self.flush(0..u64::MAX, None)
        {
            log::warn!("extent store: writing back a file closed: {err}");
        }
        let pages = self.pages.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner);
        let dirty = pages.pages.values().filter(|page| page.dirty.is_some()).count() as u64;
        self.cache.held.fetch_sub(pages.pages.len() as u64 * PAGE, Ordering::Relaxed);
        self.cache.dirty.fetch_sub(dirty * PAGE, Ordering::Relaxed);
        self.cache.drained();
    }
}

impl<F: File> Paged for Shared<F> {
    fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    fn dirtied(&self) -> Option<Instant> {
        self.pages().dirtied
    }

    fn write_back(&self, most: u64) -> io::Result<()> {
        self.write_runs(0..u64::MAX, None, Some(most))
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
        let pages = shared.lock();
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
                    // The run of pages it holds not, read at once from the file.
                    let mut miss_end = at + take as u64;
                    while miss_end < end && !pages.pages.contains_key(&(miss_end / PAGE)) {
                        miss_end = ((miss_end / PAGE + 1) * PAGE).min(end);
                    }
                    let to = (at - offset) as usize..(miss_end - offset) as usize;
                    shared.file.read_at(&mut buf[to], at)?;
                    at = miss_end;
                    continue;
                }
            }
            at += take as u64;
        }
        // Past the end of what was written, as the file reads: zeros.
        let size = pages.size;
        if end > size {
            let from = size.saturating_sub(offset).min(buf.len() as u64) as usize;
            buf[from..].fill(0);
        }
        Ok(())
    }

    fn read_cached(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.read_at(buf, offset)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        let shared = &*self.shared;
        shared.cache.throttle()?;
        let number = shared.cache.writes.fetch_add(1, Ordering::Relaxed) + 1;
        let mut pages = shared.lock();
        let end = offset + buf.len() as u64;
        let mut at = offset;
        let (mut added, mut dirtied) = (0, 0);
        let mut failed = None;
        while at < end {
            let index = at / PAGE;
            let within = (at % PAGE) as usize;
            let take = ((PAGE - at % PAGE).min(end - at)) as usize;
            let size = pages.size;
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
                        let mut bytes = shared.cache.frames.zeroed();
                        if index * PAGE < size
                            && let Err(err) = shared.file.read_at(&mut bytes, index * PAGE)
                        {
                            failed = Some(err);
                            break;
                        }
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
        if at > offset {
            pages.dirtied.get_or_insert_with(Instant::now);
        }
        let held = shared.cache.held.fetch_add(added * PAGE, Ordering::Relaxed) + added * PAGE;
        shared.cache.peak.fetch_max(held, Ordering::Relaxed);
        let dirty =
            shared.cache.dirty.fetch_add(dirtied * PAGE, Ordering::Relaxed) + dirtied * PAGE;
        shared.cache.peak_dirty.fetch_max(dirty, Ordering::Relaxed);
        drop(pages);
        shared.cache.dirtied();
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
    /// 2.3x slower and its commit 5x on ubu1 against the host's page cache. A failure is the
    /// file's next sync's. With no flusher, nothing: its writers write back as they go.
    fn write_behind(&self, offset: u64, len: u64) -> io::Result<()> {
        let paged: Arc<dyn Paged> = self.shared.clone();
        self.shared.cache.begin(Arc::downgrade(&paged), offset..offset + len);
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

    fn sync_cached(&self) -> io::Result<()> {
        self.sync()
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

    /// Writes land in memory: reads see them at once, the file has none of them until a sync,
    /// which writes them back, in the file's order, and syncs the file. Clean, they stay held,
    /// read from memory, until evicted; then reads go to the file.
    #[test]
    fn writes_are_held_until_synced_and_read_from_memory() {
        let dir = MemDisk::new();
        let cache = cache(Options { expire: Duration::from_secs(3600), ..Options::default() });
        let file = Cached::new(dir.create("f", true).unwrap(), &cache).unwrap();
        let data = pattern(1, 3 * PAGE as usize + 100);
        file.write_at(&data, 0).unwrap();
        let mut read = vec![0; data.len()];
        file.read_at(&mut read, 0).unwrap();
        assert_eq!(read, data);
        let mut under = vec![0; data.len()];
        file.file().read_at(&mut under, 0).unwrap();
        assert_eq!(under, vec![0; data.len()], "the file holds none of it before a sync");
        assert_eq!(
            cache.usage(),
            Usage {
                held: 4 * PAGE,
                dirty: 4 * PAGE,
                writing: 0,
                peak: 4 * PAGE,
                peak_dirty: 4 * PAGE
            }
        );
        file.sync().unwrap();
        file.file().read_at(&mut under, 0).unwrap();
        assert_eq!(under, data);
        assert_eq!(file.size().unwrap(), data.len() as u64);
        assert_eq!(
            cache.usage(),
            Usage { held: 4 * PAGE, dirty: 0, writing: 0, peak: 4 * PAGE, peak_dirty: 4 * PAGE }
        );
        assert_eq!(cache.evict_to(0), 0);
        file.read_at(&mut read, 0).unwrap();
        assert_eq!(read, data, "read from the file, evicted");
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

        fn sync_cached(&self) -> io::Result<()> {
            self.0.sync_cached()
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

    /// Sized to a governor's room, the cache's bounds are a tenth, a fifth and all of it, none
    /// below what it was made with; sized down again, they go back to those (TASK-433.8).
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
            (made.background, made.hard, made.most)
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
    /// not correctly implement a total order"), as the flusher did on ubu1, dying (TASK-493).
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

    /// A cache whose flusher was woken to a [`Panicking`] file, and the file, which the caller holds.
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

    /// A flusher a panic unwinds says it died, where nothing aborts the process: the cache knows it,
    /// and a writer held back at its bound panics, as nothing would write back for it, rather than
    /// wait for good, as it did, the flusher dead and none knowing (TASK-493).
    #[test]
    fn a_dead_flusher_is_known_and_holds_no_writer_for_good() {
        let (cache, _file) = flusher_panicking();
        assert!(died_within(&cache), "the flusher died unsaid");
        cache.dirty.store(cache.hard() + 1, Ordering::Relaxed);
        let held = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cache.throttle()))
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

    /// What [`child_flusher_panics`] does, if its parent sets it.
    const CHILD: &str = "EXTENT_STORE_PAGES_CHILD";

    /// Run by [`a_panic_on_the_flusher_aborts_the_daemon`], in a process of its own: aborting on
    /// panics holding what is shared beyond one store, as the extent daemon does, its cache's
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
    /// one store does (TASK-149.24), its log naming the flusher: every store's files wait on it, and
    /// no store's halt contains its death, which left the daemon going on with nothing writing back
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
}
