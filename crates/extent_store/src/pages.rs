// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the store's page cache (TASK-433): what holds the bytes its data files are written with
//! until they are on the host's disk, and for as long after as memory allows, in memory of the
//! process's own, which the extent daemon governs with the rest (`cladefs::extentd::memory`). Its
//! files bypass the host's page cache (`O_DIRECT`), so that no byte a store holds is held twice,
//! in two memories each budgeted as if the other weren't there. It does what the host's did for
//! them, as Linux's does (mm/filemap.c, mm/page-writeback.c, fs/fs-writeback.c):
//!
//! - **A write lands in memory and returns** ([`Cached::write_at`]): its pages are dirty, and
//!   the disk takes them later, coalesced. Reads see them at once ([`Cached::read_at`]); a read of
//!   what the cache holds not goes to the file, and leaves nothing cached: what is read again, the
//!   readers' own caches hold (the extent cache), so that nothing is held twice here either.
//! - **Write-back** goes in the file's order, contiguous dirty pages one write of up to
//!   [`CHUNK`], from memory aligned to blocks, as iomap's ioends coalesce. A page written while
//!   its write-back is in flight is dirty again, and written again after: writers never wait on
//!   write-back (no stable pages). The flusher ([`PageCache`]'s thread) writes back what has been
//!   dirty longer than [`Options::expire`], and the oldest first while more than
//!   [`Options::background`] is dirty.
//! - **Writers are throttled**, not refused, past [`Options::hard`] dirty: they wait for
//!   write-back, as `balance_dirty_pages` has them.
//! - **A sync** ([`Cached::sync`]) writes back what was dirty as it began, waits for every write of
//!   the file in flight, the flusher's too, then syncs the file: what it covers is a snapshot, so
//!   that a writer going on never keeps it from ending (`PAGECACHE_TAG_TOWRITE`). A write-back that
//!   failed fails the sync after it, once ([`Pages::failed`], errseq's semantics for one opener).
//! - **Eviction** takes clean pages alone, of the files used least recently first
//!   ([`PageCache::evict_to`]), never writing any: what is dirty is the flusher's to write.
//! - **A file removed** drops its pages, the dirty ones unwritten (`folio_cancel_dirty`): a
//!   segment's raw file, framed and gone, never reaches the disk but what its syncs needed.

use crate::dir::File;
use std::collections::BTreeMap;
use std::io;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

/// The cache's page: what it holds, writes back and evicts. A store's data files are written in
/// whole sectors of this size (TASK-411.8), so a write fills its pages whole but at a file's end.
pub const PAGE: u64 = 4096;

/// The most a write-back writes at once: contiguous dirty pages, one write.
pub const CHUNK: u64 = 1 << 20;

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
/// the backlog is on the disk, what the cache holds a bounded window of it, the latest.
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
}

/// A process's page cache: its files' pages, accounted together, written back by one flusher
/// thread, evicted together. One for the process ([`shared`]), which the extent daemon governs.
pub struct PageCache {
    options: Options,
    held: AtomicU64,
    peak: AtomicU64,
    dirty: AtomicU64,
    writing: AtomicU64,
    /// Each write's number, in order: what a sync covers, the writes before it.
    writes: AtomicU64,
    /// Each use's number, which says which files were used least recently.
    uses: AtomicU64,
    files: Mutex<Vec<Weak<dyn Paged>>>,
    /// Wakes the flusher.
    flush: Condvar,
    flushing: Mutex<bool>,
    /// Wakes writers held back.
    drained: Condvar,
    holding: Mutex<()>,
}

/// The process's page cache, which every store's files share ([`crate::HostDir`]).
pub fn shared() -> &'static Arc<PageCache> {
    static SHARED: std::sync::LazyLock<Arc<PageCache>> =
        std::sync::LazyLock::new(|| PageCache::new(Options::default()));
    &SHARED
}

impl PageCache {
    /// A cache going by `options`, its flusher started.
    pub fn new(options: Options) -> Arc<Self> {
        let cache = Arc::new(Self {
            options,
            held: AtomicU64::new(0),
            peak: AtomicU64::new(0),
            dirty: AtomicU64::new(0),
            writing: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            uses: AtomicU64::new(0),
            files: Mutex::default(),
            flush: Condvar::new(),
            flushing: Mutex::new(false),
            drained: Condvar::new(),
            holding: Mutex::new(()),
        });
        let flusher = Arc::downgrade(&cache);
        std::thread::Builder::new()
            .name("store flusher".into())
            .spawn(move || flush_while(flusher))
            .expect("starting the page cache's flusher");
        cache
    }

    /// What it holds.
    pub fn usage(&self) -> Usage {
        Usage {
            held: self.held.load(Ordering::Relaxed),
            dirty: self.dirty.load(Ordering::Relaxed),
            writing: self.writing.load(Ordering::Relaxed),
            peak: self.peak.load(Ordering::Relaxed),
        }
    }

    /// What it goes by.
    pub fn options(&self) -> Options {
        self.options
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
        let mut files = self.files();
        files.sort_by_key(|file| file.used());
        for file in files {
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

    /// A use's number.
    fn used(&self) -> u64 {
        self.uses.fetch_add(1, Ordering::Relaxed)
    }

    /// Wakes the flusher.
    fn wake(&self) {
        *self.flushing.lock().unwrap() = true;
        self.flush.notify_one();
    }

    /// Holds a writer about to dirty pages back while more than [`Options::hard`] is dirty, until
    /// write-back brings it under, the flusher woken.
    fn throttle(&self) {
        if self.dirty.load(Ordering::Relaxed) <= self.options.hard {
            return;
        }
        self.wake();
        let mut holding = self.holding.lock().unwrap();
        while self.dirty.load(Ordering::Relaxed) > self.options.hard {
            holding = self.drained.wait_timeout(holding, Duration::from_millis(10)).unwrap().0;
            self.wake();
        }
    }

    /// Pages became clean, or went: writers held back may go on.
    fn drained(&self) {
        let _holding = self.holding.lock().unwrap();
        self.drained.notify_all();
    }

    /// Pages were dirtied: the flusher writes back once more than [`Options::background`] is
    /// dirty; and clean ones are evicted once more than [`Options::most`] is held.
    fn dirtied(&self) {
        if self.dirty.load(Ordering::Relaxed) > self.options.background {
            self.wake();
        }
        if self.bytes() > self.options.most {
            self.evict_to(self.options.most);
        }
    }
}

/// The flusher: while the cache is there, it writes back what has been dirty longer than
/// [`Options::expire`], and, while more than [`Options::background`] is dirty, the files dirty
/// the longest, a [`CHUNK`]'s worth of writes each in turn.
fn flush_while(cache: Weak<PageCache>) {
    loop {
        let Some(cache) = cache.upgrade() else { return };
        {
            let woken = cache.flushing.lock().unwrap();
            let interval = cache.options.expire / 4;
            let (mut woken, _) =
                cache.flush.wait_timeout_while(woken, interval, |woken| !*woken).unwrap();
            *woken = false;
        }
        let now = Instant::now();
        let mut files = cache.files();
        files.sort_by_key(|file| file.dirtied());
        for file in files {
            let Some(dirtied) = file.dirtied() else { continue };
            let over = cache.dirty.load(Ordering::Relaxed) > cache.options.background;
            if !over && now.duration_since(dirtied) < cache.options.expire {
                break;
            }
            if let Err(err) = file.write_back(16 * CHUNK) {
                log::warn!("extent store: writing back a file's pages: {err}");
            }
        }
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
    /// Drops its clean pages.
    fn evict_clean(&self);
    /// Writes back all its dirty pages, waiting for every write-back of it in flight.
    fn write_back_all(&self) -> io::Result<()>;
}

/// A page held.
struct Page {
    bytes: Box<[u8]>,
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
    /// Write-backs in flight.
    in_flight: usize,
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
    /// Signalled as its write-backs end.
    written: Condvar,
    used: AtomicU64,
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
            pages: Mutex::new(Pages {
                pages: BTreeMap::new(),
                size,
                dirtied: None,
                in_flight: 0,
                failed: None,
            }),
            written: Condvar::new(),
            used: AtomicU64::new(cache.used()),
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

    /// Drops every page, the dirty ones unwritten: the file is gone ([`crate::HostDir`]'s
    /// `remove`).
    pub(crate) fn cancel(&self) {
        self.shared.drop_pages(0..u64::MAX);
    }

    /// This, held no more: what a directory names it by.
    pub(crate) fn downgrade(&self) -> WeakCached<F> {
        WeakCached { shared: Arc::downgrade(&self.shared) }
    }

    /// Whether `other` is an opening of the same file.
    pub(crate) fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }

    /// Whether this is the file's only opening.
    pub(crate) fn alone(&self) -> bool {
        Arc::strong_count(&self.shared) == 1
    }
}

impl<F: File> Shared<F> {
    fn lock(&self) -> MutexGuard<'_, Pages> {
        self.used.store(self.cache.used(), Ordering::Relaxed);
        self.pages.lock().unwrap()
    }

    /// Drops the pages in `range`, dirty or not, waiting for write-backs in flight there.
    fn drop_pages(&self, range: Range<u64>) {
        let mut pages = self.lock();
        let first = range.start / PAGE;
        let last = range.end.div_ceil(PAGE);
        loop {
            let writing = pages.pages.range(first..last).any(|(_, page)| page.writing);
            if !writing {
                break;
            }
            pages = self.written.wait(pages).unwrap();
        }
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
    /// before if given, and waits for every write-back of them in flight; or, if `most` is given,
    /// writes back up to `most` bytes and waits for none but its own. Pages are written in the
    /// file's order, contiguous dirty ones at once, up to [`CHUNK`] a write.
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
        let covers = |page: &Page| {
            !page.writing && page.dirty.is_some_and(|dirty| through.is_none_or(|t| dirty <= t))
        };
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
                // None left to write: a sync waits for every write-back in flight in the range,
                // the flusher's too, which may carry what it covers.
                let in_flight = pages.pages.range(first..last).any(|(_, page)| page.writing);
                if most.is_some() || !in_flight {
                    break;
                }
                pages = self.written.wait(pages).unwrap();
                continue;
            }
            let start = run[0] * PAGE;
            let end = ((run[run.len() - 1] + 1) * PAGE).min(pages.size.max(start));
            let mut bytes = Vec::with_capacity((end - start) as usize);
            let mut written = Vec::with_capacity(run.len());
            for &index in &run {
                let page = pages.pages.get_mut(&index).unwrap();
                page.writing = true;
                written.push((index, page.last));
                bytes.extend_from_slice(&page.bytes);
            }
            bytes.truncate((end - start) as usize);
            pages.in_flight += 1;
            let len = run.len() as u64 * PAGE;
            self.cache.writing.fetch_add(len, Ordering::Relaxed);
            drop(pages);
            let wrote = self.file.write_at(&bytes, start);
            pages = self.lock();
            self.cache.writing.fetch_sub(len, Ordering::Relaxed);
            pages.in_flight -= 1;
            for (index, last) in written {
                let Some(page) = pages.pages.get_mut(&index) else { continue };
                page.writing = false;
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
            self.written.notify_all();
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

/// A file whose every opening went: what of it is dirty is written back, as a file closed keeps
/// what was written to it; then its pages go, counted out.
impl<F: File> Drop for Shared<F> {
    fn drop(&mut self) {
        if let Err(err) = self.flush(0..u64::MAX, None) {
            log::warn!("extent store: writing back a file closed: {err}");
        }
        let pages = self.pages.get_mut().unwrap();
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
        self.pages.lock().unwrap().dirtied
    }

    fn write_back(&self, most: u64) -> io::Result<()> {
        self.write_runs(0..u64::MAX, None, Some(most))
    }

    fn write_back_all(&self) -> io::Result<()> {
        self.flush(0..u64::MAX, None)
    }

    fn evict_clean(&self) {
        let mut pages = self.pages.lock().unwrap();
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
        shared.cache.throttle();
        let number = shared.cache.writes.fetch_add(1, Ordering::Relaxed) + 1;
        let mut pages = shared.lock();
        let end = offset + buf.len() as u64;
        let mut at = offset;
        let (mut added, mut dirtied) = (0, 0);
        while at < end {
            let index = at / PAGE;
            let within = (at % PAGE) as usize;
            let take = ((PAGE - at % PAGE).min(end - at)) as usize;
            let size = pages.size;
            let page = match pages.pages.entry(index) {
                std::collections::btree_map::Entry::Occupied(page) => page.into_mut(),
                std::collections::btree_map::Entry::Vacant(vacant) => {
                    let mut bytes = vec![0; PAGE as usize].into_boxed_slice();
                    // What of the page the write leaves, as the file holds it.
                    let partial = within != 0 || take != PAGE as usize;
                    if partial && index * PAGE < size {
                        shared.file.read_at(&mut bytes, index * PAGE)?;
                    }
                    added += 1;
                    vacant.insert(Page { bytes, dirty: None, last: 0, writing: false })
                }
            };
            page.bytes[within..within + take]
                .copy_from_slice(&buf[(at - offset) as usize..(at - offset) as usize + take]);
            if page.dirty.is_none() {
                dirtied += 1;
                page.dirty = Some(number);
            }
            page.last = number;
            at += take as u64;
        }
        pages.size = pages.size.max(end);
        pages.dirtied.get_or_insert_with(Instant::now);
        let held = shared.cache.held.fetch_add(added * PAGE, Ordering::Relaxed) + added * PAGE;
        shared.cache.peak.fetch_max(held, Ordering::Relaxed);
        shared.cache.dirty.fetch_add(dirtied * PAGE, Ordering::Relaxed);
        drop(pages);
        shared.cache.dirtied();
        Ok(())
    }

    fn write_back(&self, offset: u64, len: u64) -> io::Result<()> {
        self.shared.flush(offset..offset + len, None)
    }

    fn write_behind(&self, _offset: u64, _len: u64) -> io::Result<()> {
        self.shared.cache.wake();
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
        if let Some(failed) = shared.lock().failed.take() {
            return Err(failed);
        }
        shared.file.sync()
    }

    fn sync_cached(&self) -> io::Result<()> {
        self.sync()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dir::Dir as _;
    use crate::mem::MemDir;

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
        let dir = MemDir::new();
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
            Usage { held: 4 * PAGE, dirty: 4 * PAGE, writing: 0, peak: 4 * PAGE }
        );
        file.sync().unwrap();
        file.file().read_at(&mut under, 0).unwrap();
        assert_eq!(under, data);
        assert_eq!(file.size().unwrap(), data.len() as u64);
        assert_eq!(cache.usage(), Usage { held: 4 * PAGE, dirty: 0, writing: 0, peak: 4 * PAGE });
        assert_eq!(cache.evict_to(0), 0);
        file.read_at(&mut read, 0).unwrap();
        assert_eq!(read, data, "read from the file, evicted");
    }

    /// A write over part of a page keeps the rest as the file holds it; one past the end, zeros
    /// between.
    #[test]
    fn partial_pages_keep_what_the_file_holds() {
        let dir = MemDir::new();
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
        let dir = MemDir::new();
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
        let dir = MemDir::new();
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

    /// A file's pages dropped, the dirty ones unwritten, as a removed file's are.
    #[test]
    fn cancelled_pages_are_never_written() {
        let dir = MemDir::new();
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
        let dir = MemDir::new();
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
}
