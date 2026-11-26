// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The process's cache of sealed extents (`crate::extents`), in part slabs of physical memory
//! (`crate::slabs`, doc-26).
//!
//! A sealed extent is cached once, keyed by its ID and the arena its parts are of, which every cell
//! and window in the process shares: a table of its parts ([`CachedExtent`]), each read in from a
//! cell that holds the extent as it is first read or mapped, a part (`extent_frame::PART`) at a
//! time, into a slab of its own (or a run of a small slab's pages, for a part shorter), on the
//! pager's threads, and evicted, its extents asked for longest ago first, as memory runs out
//! (`slabs::relieve_with`) or the daemon is asked to give memory back. Windows map its pages
//! read-only: one copy of each page, whichever cell or fork maps it, as an extent never changes
//! once sealed, its ID its cell's to write once, which the cell keeps to (TASK-79.6).
//!
//! A part's slot owns its slab while it is present. An eviction moves the slab out of the slot
//! into an [`Eviction`] of its own, leaving a token behind that holds no memory, unmaps it from
//! each window on the extent's window set, then alone frees it: so no eviction frees a slab
//! another holds, whose unmaps that other has yet to send (TASK-560, board 2516; spec rule
//! ownTokens). Freed, a slab staged to VM processes waits in quarantine for them
//! (`slabs::quarantine_with`, TASK-79.9). An extent's windows are a set, each window on it once
//! however often it maps the extent (TASK-602, board 2600; spec rule registerOnce).
//!
//! An extent the default arena holds is that one copy for a tenant's cell too, as every VM
//! process may read the default arena; and one a tenant's arena holds that a cell reads into the
//! default arena, as a public base's extent its publisher's cell had read, moves there: its parts
//! copied from slab to slab in the kernel, rather than paged in again, and evicted from the
//! tenant's (TASK-234).

use crate::bucket::Followed;
use crate::dax::{Origin, Present, Source, Window};
use crate::error::{Errno, Result};
use crate::open_cell::PublicReach;
use crate::pages::{ArenaId, page_size};
use crate::slabs::{self, PartSlab, PhysLocation};
use crate::stats::{self, Counter};
use bucket_cell::trace;
use futures::future::BoxFuture;
use local_cell::shared::SharedLock;
use local_cell::{HostDir, LocalCell};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use storage_device::ExtentId;
use storage_device::NoExtent;

/// Explicit per-host cache of sealed extents (see the module docs, board 10962): its pager's
/// threads, which read their pages from the cells, and the extents, by arena and ID.
pub(crate) struct ExtentCache {
    /// Physical memory slabs this cache allocates parts in.
    slabs: Arc<slabs::HostSlabs>,
    /// Dedicated pager runtime (`"cladefs pager"`, [`PAGER_THREADS`] workers), kept separate from
    /// `cladefsd::Daemon`'s consolidated runtime for DAX page-in latency isolation (board 10774):
    /// page-ins execute synchronous disk reads, zstd decompression, and `slab.write` directly on
    /// pager workers and size `buffers` to `PAGER_THREADS`. Lazily initialized on first page-in;
    /// under Shuttle, never initialized (`spawn_page_in` uses Shuttle's tasks).
    #[cfg_attr(feature = "shuttle", allow(dead_code))]
    pager: OnceLock<tokio::runtime::Runtime>,
    /// By the arena their pages are of (TASK-43), and their IDs. Every tenant's, which a panic
    /// holding aborts the daemon (`local_cell::shared`, TASK-149.24); as do the others here.
    extents: SharedLock<Mutex<HashMap<(ArenaId, ExtentId), Cached>>>,
    /// The arenas but the default that extents were cached of, to look for an extent's copy in,
    /// taken after `extents` (TASK-234).
    arenas: SharedLock<Mutex<HashSet<ArenaId>>>,
    /// The cache's clock, which counts the extents asked for.
    clock: AtomicU64,
    /// Monotonic milliseconds advanced by the memory governor using real wall-clock `Instant`
    /// deltas (`advance_ms`). Used to classify recently governor-evicted parts that fault back in
    /// within [`REFAULT_WINDOW_MS`] as refaults.
    clock_ms: AtomicU64,
    /// Governor-evicted parts that faulted back into memory within [`REFAULT_WINDOW_MS`]
    /// (thrashing signal for the memory governor; excludes tenant-cap relief and harvest evictions
    /// so a single tenant cannot poison host-wide thrashing).
    refaults: AtomicU64,
    /// Round-robin cursor for budgeted background `cachestat(2)` cold-part scans.
    #[cfg_attr(any(not(target_os = "linux"), madsim), allow(dead_code))]
    cold_cursor: AtomicUsize,
    /// Monotonic counter bumped whenever a part becomes present or a window unmaps an extent, so
    /// sustained `IdleOnly` governor ticks can skip redundant full-cache scans when nothing
    /// became idle since the last zero-yield pass.
    idle_change_seq: AtomicU64,
    /// The number of extents past which the cache next lets go of those over its bound: the bound,
    /// or, if its last pass couldn't get below it, as the windows' mappings hold the extents they
    /// map, an eighth of the bound more than it left (TASK-109.2). Each lookup past the bound
    /// sorted every extent cached, under the cache's lock, and let go of none: an 8 GiB window,
    /// mapping more than the bound, never full, so never reclaiming, made each MAP 3-6 ms.
    trim_at: AtomicUsize,
    /// The most extents the cache keeps (TASK-104): each its table of pages, and the cells it reads
    /// through, which an extent nothing maps and whose pages were evicted keeps still. Under
    /// pgbench, small extents by the hundred thousand, the tables of extents never asked for again
    /// would be most of what the cache holds. Past this, those asked for longest ago that nothing
    /// else holds (no window maps them) go, down to seven eighths of it; one asked for again is
    /// read anew from its cell. Tests lower it.
    most_cached: AtomicUsize,
    /// cladefs: whether the cap on every arena's memory together is the memory governor's, the
    /// host's memory, not a limit the control API set: what a page-in finding no page waits out
    /// rather than fails (board 5662). Set as the cap is (`cladefsd::memory`); a daemon not
    /// governed has none.
    total_is_the_hosts: std::sync::atomic::AtomicBool,
    /// Ensures arena relief is registered at most once per host.
    relief_started: std::sync::Once,
    /// Shared zero page in this host's default arena for DAX windows to map.
    zeros: OnceLock<Arc<Zeros>>,
    /// Live DAX windows on this host, held weakly so relief can evict their composed host pages
    /// when a tenant arena or the host is under memory pressure.
    windows: SharedLock<Mutex<Vec<Weak<crate::dax::Window>>>>,
}

impl ExtentCache {
    pub(crate) fn new(slabs: Arc<slabs::HostSlabs>) -> Arc<Self> {
        Arc::new(Self {
            slabs,
            pager: OnceLock::new(),
            extents: SharedLock::new("extent cache", Mutex::default()),
            arenas: SharedLock::new("extent cache arenas", Mutex::default()),
            clock: AtomicU64::new(0),
            clock_ms: AtomicU64::new(1),
            refaults: AtomicU64::new(0),
            cold_cursor: AtomicUsize::new(0),
            idle_change_seq: AtomicU64::new(0),
            trim_at: AtomicUsize::new(0),
            most_cached: AtomicUsize::new(1 << 16),
            total_is_the_hosts: std::sync::atomic::AtomicBool::new(false),
            relief_started: std::sync::Once::new(),
            zeros: OnceLock::new(),
            windows: SharedLock::new("extent cache windows", Mutex::default()),
        })
    }

    /// Registers a DAX window weakly so arena relief can evict its composed host pages.
    pub(crate) fn register_window(&self, window: &Arc<crate::dax::Window>) {
        let mut windows = self.windows.with(Mutex::lock);
        windows.retain(|w| w.strong_count() > 0);
        windows.push(Arc::downgrade(window));
    }

    /// Allocates one host page in `arena` for a composed DAX page, retrying with backoff while
    /// the host's total cap is governed and memory may still return (board 5662) rather than
    /// failing a DAX fault with `ENOMEM` and aborting the VM process.
    pub(crate) async fn alloc_composed(&self, arena: ArenaId) -> crate::Result<PartSlab> {
        let mut pause = RETRY_FIRST;
        loop {
            let alloc = match self.slabs.alloc(arena, 1, slabs::Wait::No) {
                Ok(slab) => Ok(slab),
                Err(slabs::NoMemory) => {
                    #[cfg(all(not(madsim), not(feature = "shuttle")))]
                    {
                        if tokio::runtime::Handle::try_current().is_ok() {
                            let slabs = self.slabs.clone();
                            tokio::task::spawn_blocking(move || {
                                slabs.alloc(arena, 1, slabs::Wait::Yes)
                            })
                            .await
                            .expect("a composed DAX page allocation panicked")
                        } else {
                            self.slabs.alloc(arena, 1, slabs::Wait::Yes)
                        }
                    }
                    #[cfg(any(madsim, feature = "shuttle"))]
                    {
                        self.slabs.alloc(arena, 1, slabs::Wait::Yes)
                    }
                }
            };
            match alloc {
                Ok(slab) => return Ok(slab),
                Err(slabs::NoMemory)
                    if self.slabs.room_may_come(arena)
                        && self.total_is_the_hosts.load(Ordering::Relaxed) =>
                {
                    waiting_for_memory();
                    #[cfg(not(feature = "shuttle"))]
                    {
                        if tokio::runtime::Handle::try_current().is_ok() {
                            tokio::time::sleep(pause).await;
                        } else {
                            std::thread::sleep(pause);
                        }
                    }
                    #[cfg(feature = "shuttle")]
                    {
                        let _ = pause;
                        shuttle::thread::yield_now();
                    }
                    pause = (pause * 2).min(RETRY_MOST);
                }
                Err(slabs::NoMemory) => return Err(crate::Errno::ENOMEM),
            }
        }
    }

    fn live_windows(&self) -> Vec<Arc<crate::dax::Window>> {
        let mut windows = self.windows.with(Mutex::lock);
        windows.retain(|w| w.strong_count() > 0);
        windows.iter().filter_map(Weak::upgrade).collect()
    }

    #[cfg(not(feature = "shuttle"))]
    fn pager(&self) -> &tokio::runtime::Runtime {
        self.pager.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(PAGER_THREADS)
                .thread_name("cladefs pager")
                // cladefs: for page-ins waiting for memory (`CachedExtent::read_pages`, board 5662).
                .enable_all()
                .build()
                .expect("the extent cache's pager")
        })
    }

    #[cfg(test)]
    pub(crate) fn most_cached(&self) -> &AtomicUsize {
        &self.most_cached
    }
}

/// A sealed extent cached.
pub(crate) struct Cached {
    extent: Arc<CachedExtent>,
    /// The cells it is read through (TASK-79.7).
    holders: Arc<Holders>,
}

/// The cells a cached extent's pages are read from (TASK-79.7): each that asked for it, as forks
/// of a base share it, held weakly, so that the cache keeps none open, nor its files and disk
/// space, once it closes. A page-in reads through the first open still that holds the extent, or
/// keeps it, freed, for its uploader: a cell that frees an extent its siblings map, or closes,
/// leaves them reading it through theirs. Each holds the same data by its ID, which its cell
/// writes once (TASK-79.6).
struct Holders(SharedLock<Mutex<Vec<Holder>>>);

impl Default for Holders {
    fn default() -> Self {
        Self(SharedLock::new("cached extent holders", Mutex::default()))
    }
}

/// A cell a cached extent's pages are read from, and its object storage, which holds them if the
/// cell holds them without their data.
struct Holder {
    /// Its cell ID, kept so `Holders::forget` still identifies and evicts extents cached by a
    /// store that has since closed (TASK-601, board 2598, board 2704).
    cell: crate::CellId,
    local: Weak<LocalCell<HostDir>>,
    fetcher: Option<Weak<crate::bucket::HostFetcher>>,
}

/// A [`Holder`] open still.
type OpenHolder = (Arc<LocalCell<HostDir>>, Option<Arc<crate::bucket::HostFetcher>>);

impl Holders {
    /// Adds `local`, reading through `fetcher`, unless it is there, letting go of those closed.
    fn add(
        &self,
        local: &Arc<LocalCell<HostDir>>,
        fetcher: Option<&Arc<crate::bucket::HostFetcher>>,
    ) {
        let mut holders = self.0.with(Mutex::lock);
        holders.retain(|holder| holder.local.strong_count() > 0);
        if !holders.iter().any(|holder| std::ptr::eq(holder.local.as_ptr(), Arc::as_ptr(local))) {
            let cell = local.description().cell;
            let (local, fetcher) = (Arc::downgrade(local), fetcher.map(Arc::downgrade));
            holders.push(Holder { cell, local, fetcher });
        }
    }

    /// Adds those of `other` open still, as [`Holders::add`] does.
    fn adopt(&self, other: &Holders) {
        for (local, fetcher) in other.open() {
            self.add(&local, fetcher.as_ref());
        }
    }

    /// Those open still, in the order they asked, with their object storage.
    fn open(&self) -> Vec<OpenHolder> {
        let holders = self.0.with(Mutex::lock);
        (holders.iter())
            .filter_map(|holder| {
                Some((holder.local.upgrade()?, holder.fetcher.as_ref().and_then(Weak::upgrade)))
            })
            .collect()
    }

    /// cladefs: lets go of those whose cells `scope` says are of it, open or closed, and of any
    /// other closed holder: no page-in reads through them again, and closed stores' cached
    /// extents are still evicted on forget (TASK-411.7, TASK-601, board 2598, board 2704).
    /// Returns whether it let go of one of `scope`.
    fn forget(&self, scope: &dyn Fn(crate::CellId) -> bool) -> bool {
        let mut forgot = false;
        self.0.with(Mutex::lock).retain(|holder| {
            if scope(holder.cell) {
                forgot = true;
                return false;
            }
            holder.local.strong_count() > 0
        });
        forgot
    }

    /// Whether one is open still.
    fn any_open(&self) -> bool {
        self.0.with(Mutex::lock).iter().any(|holder| holder.local.strong_count() > 0)
    }

    /// Whether one open still, but `local`, holds `extent`.
    fn another_holds(&self, local: &Arc<LocalCell<HostDir>>, extent: ExtentId) -> bool {
        (self.open().into_iter())
            .filter(|(holder, _)| !Arc::ptr_eq(holder, local))
            .any(|(holder, _)| holder.extent(extent).is_ok_and(|held| held.is_some()))
    }
}

/// How many threads the cache's pager reads extents' pages on: a read of the cell, and the
/// decompression of a part, keep one busy, while page-ins of other extents wait for theirs.
pub(crate) const PAGER_THREADS: usize = 6;

/// cladefs: how long a page-in that found no page, the host's memory full, waits before it tries
/// again, at first, then twice as long each time, [`RETRY_MOST`] at most: the memory governor's
/// look, after which its cap may have changed (board 5662).
const RETRY_FIRST: std::time::Duration = std::time::Duration::from_millis(1);
const RETRY_MOST: std::time::Duration = std::time::Duration::from_millis(100);

/// How many parts after those a reader reading on asks for are read ahead of its requests.
pub(crate) const PARTS_AHEAD: u64 = 2;

/// How many milliseconds after a governor-driven mapped eviction a page-in of that same part
/// counts as a refault: 30 seconds.
pub(crate) const REFAULT_WINDOW_MS: u64 = 30_000;

/// Which cached extents an eviction pass may reclaim.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EvictScope {
    /// Only unmapped (`!mapped`) extents: reclaims memory with zero VM TLB shootdowns or DAX fault
    /// storms on live windows.
    IdleOnly,
    /// Unmapped extents first, then mapped extents (LRU) and composed window pages, for relief
    /// or per-tenant limits (does NOT feed host-wide governor thrashing).
    All,
    /// Unmapped extents first, then mapped extents (LRU) and composed window pages, driven by the
    /// host memory governor's `Mapped` shrink (evicted parts feed `refaults()` for thrash backoff).
    GovernorMapped,
}

impl ExtentCache {
    /// Advances the cache's wall-clock millisecond counter used for refault recency.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn advance_ms(&self, delta_ms: u64) -> u64 {
        self.clock_ms.fetch_add(delta_ms, Ordering::Relaxed).saturating_add(delta_ms)
    }

    /// Total governor-evicted part refaults observed within [`REFAULT_WINDOW_MS`] of eviction.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn refaults(&self) -> u64 {
        self.refaults.load(Ordering::Relaxed)
    }

    /// Monotonic counter bumped whenever a cached part becomes present or a window unmaps an
    /// extent (used by `IdleOnly` governance to avoid rescanning when no idle extents can exist).
    #[cfg_attr(not(any(target_os = "linux", madsim)), allow(dead_code))]
    pub(crate) fn idle_change_seq(&self) -> u64 {
        self.idle_change_seq.load(Ordering::Relaxed)
    }

    /// Evicts the pages of the cached extents of `arena`, or of every arena, those no window maps
    /// first, then those used longest ago (`CachedExtent::eviction_order`), until `enough` says, of
    /// the bytes evicted so far, that they are, or none is left to evict: memory an arena past its
    /// cap, or the arenas past the total's, give back (TASK-50.3). Returns the bytes it evicted.
    /// Evicted pages are read in again as they are touched.
    ///
    /// cladefs: in a span of its own, `cache.relief` (`bucket_cell::trace`, TASK-258).
    pub(crate) fn evict_least_used(
        &self,
        arena: Option<ArenaId>,
        enough: impl FnMut(u64) -> bool,
    ) -> u64 {
        self.evict_with_scope(arena, EvictScope::All, enough)
    }

    /// Evicts cached extents of `arena` (or every arena) restricted to `scope` until `enough`
    /// returns true or no eligible extent remains. Returns the bytes evicted.
    pub(crate) fn evict_with_scope(
        &self,
        arena: Option<ArenaId>,
        scope: EvictScope,
        enough: impl FnMut(u64) -> bool,
    ) -> u64 {
        let span = trace::Span::start("cache.relief");
        if let Some(arena) = arena {
            span.set_count(trace::ARENA, u64::from(arena.0));
        }
        let (evicted, extents) = self.evict_in_order(arena, scope, enough);
        span.set_count(trace::BYTES, evicted);
        span.set_count(trace::EXTENTS, extents);
        evicted
    }

    /// Evicts as [`ExtentCache::evict_with_scope`] says: returns the bytes it evicted, and of how
    /// many extents.
    fn evict_in_order(
        &self,
        arena: Option<ArenaId>,
        scope: EvictScope,
        mut enough: impl FnMut(u64) -> bool,
    ) -> (u64, u64) {
        let candidates: Vec<Arc<CachedExtent>> = {
            let extents = self.extents.with(Mutex::lock);
            (extents.iter())
                .filter(|((of, _), _)| arena.is_none_or(|arena| *of == arena))
                .map(|(_, cached)| cached.extent.clone())
                .collect()
        };
        // Past the cache's lock, as each takes its own; those evicted already, which stay cached
        // until freed, left out, each pass having gone through them all (TASK-92.33). Zero
        // syscalls on this relief hot path (pure in-memory LRU ordering).
        let mut candidates: Vec<_> = (candidates.into_iter())
            .filter_map(|extent| {
                let order = extent.eviction_order()?;
                if scope == EvictScope::IdleOnly && order.0 {
                    return None;
                }
                Some((order, extent))
            })
            .collect();
        candidates.sort_unstable_by_key(|(order, _)| *order);
        let by_governor = scope == EvictScope::GovernorMapped;
        let (mut evicted, mut extents) = (0, 0);
        for (_, extent) in candidates {
            if enough(evicted) {
                break;
            }
            let bytes = extent.evict_with_cause(by_governor);
            evicted += bytes;
            extents += u64::from(bytes > 0);
        }
        if scope != EvictScope::IdleOnly && !enough(evicted) {
            for window in self.live_windows() {
                if enough(evicted) {
                    break;
                }
                let base = evicted;
                evicted += window.evict_composed(arena, |added| enough(base + added));
            }
        }
        (evicted, extents)
    }

    /// Budgeted round-robin background scan that harvests individual cached parts Linux MGLRU has
    /// completely paged out to swap and marked cold (`cached == 0 && evicted > 0 &&
    /// recently_evicted == 0`) on two consecutive scans. Calls `cachestat(2)` outside any extent
    /// lock so DAX faults are never blocked, inspects at most `scan_budget` extents per call, and
    /// does not count harvested parts toward governor refault thrashing.
    #[cfg_attr(any(not(target_os = "linux"), madsim), allow(dead_code))]
    pub(crate) fn evict_kernel_cold(&self, scan_budget: usize, max_bytes: u64) -> u64 {
        if scan_budget == 0 || max_bytes == 0 {
            return 0;
        }
        let all: Vec<Arc<CachedExtent>> = {
            let extents = self.extents.with(Mutex::lock);
            extents
                .values()
                .filter(|cached| cached.extent.present.load(Ordering::Relaxed) > 0)
                .map(|cached| cached.extent.clone())
                .collect()
        };
        if all.is_empty() {
            return 0;
        }
        let n = all.len();
        let take = scan_budget.min(n);
        let start = self.cold_cursor.fetch_add(take, Ordering::Relaxed) % n;
        let mut harvested = 0u64;
        for i in 0..take {
            if harvested >= max_bytes {
                break;
            }
            let extent = &all[(start + i) % n];
            harvested += extent.evict_kernel_cold(max_bytes.saturating_sub(harvested));
        }
        harvested
    }

    /// Gives, once, the relief of arenas under pressure (TASK-50.3) its evictor, which frees parts
    /// of cached extents (`HostSlabs::relieve_with`): before a cell of any arena opens, whose
    /// allocations may wait for it.
    pub(crate) fn relieve_pressure(self: &Arc<Self>) {
        self.relief_started.call_once(|| {
            let weak = Arc::downgrade(self);
            let relieved = self.slabs.relieve_with(move |arena, bytes| {
                weak.upgrade()
                    .map_or(0, |cache| cache.evict_least_used(arena, |evicted| evicted >= bytes))
            });
            match relieved {
                Err(err) if cfg!(madsim) && err.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(err) => {
                    log::error!("cladefs: starting the thread that frees capped memory: {err}")
                }
                Ok(()) => {}
            }
        });
    }

    /// Lets go of the extents cached in `arena`: a tenant's, which nothing holds now
    /// (`crate::pages::Arena`, TASK-43.4). Their pages are freed as the last holders of the
    /// extents, page-ins in flight, let them go.
    pub(crate) fn evict_arena(&self, arena: ArenaId) {
        let evicted: Vec<Cached> = {
            let mut extents = self.extents.with(Mutex::lock);
            self.arenas.with(Mutex::lock).remove(&arena);
            extents.extract_if(|(of, _), _| *of == arena).map(|(_, cached)| cached).collect()
        };
        drop(evicted);
        for window in self.live_windows() {
            window.evict_composed(Some(arena), |_| false);
        }
    }

    /// cladefs: forgets the cells whose cells `scope` says are of it, as their host does as it is
    /// told their tenant's KEK is revoked or destroyed, or their cell shredded (doc-16, Shredding;
    /// TASK-411.7). Returns the bytes of pages it evicted, and of how many extents.
    pub(crate) fn forget(&self, scope: impl Fn(crate::CellId) -> bool) -> (u64, u64) {
        let gone: Vec<Cached> = {
            let mut extents = self.extents.with(Mutex::lock);
            let doomed: Vec<(ArenaId, ExtentId)> = (extents.iter())
                .filter(|(_, cached)| cached.holders.forget(&scope) && !cached.holders.any_open())
                .map(|(key, _)| *key)
                .collect();
            doomed.iter().filter_map(|key| extents.remove(key)).collect()
        };
        let evicted = gone.iter().map(|cached| cached.extent.revoke()).sum();
        (evicted, gone.len() as u64)
    }

    /// The sealed `extent`, of `len` bytes, in this cache, of `cached_in`'s arena, cached from
    /// `local` if it isn't.
    pub(crate) async fn cached(
        self: &Arc<Self>,
        local: &Arc<LocalCell<HostDir>>,
        cached_in: CachedIn,
        fetcher: Option<Arc<crate::bucket::HostFetcher>>,
        extent: ExtentId,
        len: u64,
    ) -> Result<Arc<CachedExtent>> {
        let (found, moved) = self.cache_now(local, cached_in, fetcher, extent, len);
        if let Some(from) = moved {
            // What of it the tenant's arena holds, moved before the caller maps it, so that its MAP
            // maps those pages: moved as asked for, each was a fault of the guest's, and the first
            // sandbox's cold read of the Linux tree took 13.7 s, paging nothing in, 4,422 faults,
            // against the next sandbox's 5.7 s and none (GCE r991).
            for page in from.present_parts() {
                if let Err(errno) = found.clone().bring_in(page).await {
                    log::warn!("cladefs: moving {:?}'s page {page}: {errno:?}", found.extent);
                }
            }
        }
        Ok(found)
    }

    /// cladefs: the cache's part of [`ExtentCache::cached`], under its lock, which holds no await.
    fn cache_now(
        self: &Arc<Self>,
        local: &Arc<LocalCell<HostDir>>,
        cached_in: CachedIn,
        fetcher: Option<Arc<crate::bucket::HostFetcher>>,
        extent: ExtentId,
        len: u64,
    ) -> (Arc<CachedExtent>, Option<Arc<CachedExtent>>) {
        let mut extents = self.extents.with(Mutex::lock);
        let key = held_key(&extents, cached_in.arena(), extent);
        let mut moved = None;
        if !extents.contains_key(&key) {
            let holders = Arc::new(Holders::default());
            let stored = StoredExtent::new(holders.clone(), local, extent);
            let mut contents: Box<dyn Contents> = Box::new(stored);
            if let CachedIn::Public(reach) = &cached_in {
                // A tenant's copy, which moves here (TASK-234): of this extent alone.
                assert_eq!(reach.extent(), extent, "a tenant's copy moved for another extent");
                let arenas = self.arenas.with(Mutex::lock);
                let other = arenas.iter().find_map(|of| extents.remove(&(*of, extent)));
                if let Some(other) = other {
                    holders.adopt(&other.holders);
                    moved = Some(other.extent.clone());
                    let from = SharedLock::new("promoted extent", Mutex::new(Some(other.extent)));
                    contents = Box::new(Promoted { from, stored: contents });
                }
            } else if key.0 != ArenaId::DEFAULT {
                self.arenas.with(Mutex::lock).insert(key.0);
            }
            let new = Arc::new(CachedExtent::new_on(self, key.0, extent, contents, len));
            extents.insert(key, Cached { extent: new, holders });
        }
        let cached = extents.get_mut(&key).expect("cached");
        cached.extent.used();
        cached.holders.add(local, fetcher.as_ref());
        let found = cached.extent.clone();
        let most = self.most_cached.load(Ordering::Relaxed);
        let gone = (extents.len() > most.max(self.trim_at.load(Ordering::Relaxed))).then(|| {
            let started = std::time::Instant::now();
            let gone = let_go_least_used(&mut extents, most);
            self.trim_at.store(extents.len() + most / 8, Ordering::Relaxed);
            stats::count_since(Counter::CacheTrims, Counter::CacheTrimNs, started);
            gone
        });
        drop(extents);
        // Freed past the cache's lock.
        drop(gone);
        if let Some(from) = &moved {
            // The tenant's windows map it from here from now on, and nothing of the copy: a fault on
            // it would read it in again. Past the cache's lock, which no window's is taken under.
            let (from_source, to_source): (Arc<dyn Source>, Arc<dyn Source>) =
                (from.clone(), found.clone());
            for window in from.windows() {
                window.rebind(&from_source, to_source.clone());
            }
        }
        (found, moved)
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn insert_for_test(
        &self,
        arena: ArenaId,
        extent: ExtentId,
        cached: Arc<CachedExtent>,
    ) {
        self.extents
            .with(Mutex::lock)
            .insert((arena, extent), Cached { extent: cached, holders: Arc::default() });
    }

    #[cfg(test)]
    pub(crate) fn find(&self, arena: ArenaId, extent: ExtentId) -> Option<Arc<CachedExtent>> {
        let extents = self.extents.with(Mutex::lock);
        extents.get(&(arena, extent)).map(|cached| cached.extent.clone())
    }

    /// Lets go of the cached `extent` of `arena`, which `local` frees, unless another cell that
    /// asked for it holds it still.
    pub(crate) fn freed(
        &self,
        local: &Arc<LocalCell<HostDir>>,
        arena: ArenaId,
        extent: ExtentId,
    ) -> Option<Arc<CachedExtent>> {
        let (key, cached, holders) = {
            let extents = self.extents.with(Mutex::lock);
            let key = held_key(&extents, arena, extent);
            let (cached, holders) =
                extents.get(&key).map(|cached| (cached.extent.clone(), cached.holders.clone()))?;
            (key, cached, holders)
        };
        if !holders.another_holds(local, extent) {
            let mut extents = self.extents.with(Mutex::lock);
            let same = extents.get(&key).is_some_and(|now| Arc::ptr_eq(&now.extent, &cached));
            let removed = same.then(|| extents.remove(&key));
            drop(extents);
            drop(removed);
        }
        Some(cached)
    }

    /// The extents cached, as of now: a pass over them, under the cache's lock.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn entries(&self) -> Entries {
        let extents = self.extents.with(Mutex::lock);
        let idle = extents.values().filter(|cached| Arc::strong_count(&cached.extent) == 1).count();
        Entries { pinned: (extents.len() - idle) as u64, idle: idle as u64 }
    }

    #[cfg(test)]
    pub(crate) fn keys(&self) -> Vec<(ArenaId, ExtentId)> {
        self.extents.with(Mutex::lock).keys().copied().collect()
    }

    /// cladefs: says whether the cap on every arena's memory together is the host's
    /// (`ExtentCache::total_is_the_hosts`).
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "the daemon, Linux's alone, sets it")
    )]
    pub(crate) fn set_total_is_the_hosts(&self, is: bool) {
        self.total_is_the_hosts.store(is, Ordering::Relaxed);
    }

    /// Zeros, for windows on this host to map ([`Zeros`]): made once per host.
    pub(crate) fn zeros(&self) -> Result<Arc<dyn Source>> {
        if let Some(zeros) = self.zeros.get() {
            return Ok(zeros.clone());
        }
        let page =
            self.slabs.alloc(ArenaId::DEFAULT, 1, slabs::Wait::Yes).map_err(|_| Errno::ENOMEM)?;
        page.write(0, &vec![0; page_size() as usize]).map_err(|err| {
            log::error!("cladefs: writing the zeros windows map: {err}");
            Errno::ENOMEM
        })?;
        Ok(self.zeros.get_or_init(|| Arc::new(Zeros(page))).clone())
    }
}

/// Evicts the pages of the cached extents of `arena`, or of every arena, on the process's default
/// host.
#[cfg(all(test, not(madsim)))]
pub(crate) fn evict_least_used(arena: Option<ArenaId>, enough: impl FnMut(u64) -> bool) -> u64 {
    crate::Host::process().cache().evict_least_used(arena, enough)
}

/// Which arena an extent's pages are of, and whether a tenant's copy of it may move to the default
/// arena: only an extent reachable from a published generation may (TASK-234), as every VM process
/// maps the default arena, and a tenant's pages there would be every tenant's to read.
pub(crate) enum CachedIn {
    /// Of `arena`: a tenant's, or the default for a cell of none, which takes no other arena's
    /// copy.
    Arena(ArenaId),
    /// Of the default arena, reachable from a published generation, as its cell found: a
    /// tenant's copy of it moves there.
    Public(PublicReach),
}

impl CachedIn {
    fn arena(&self) -> ArenaId {
        match self {
            CachedIn::Arena(arena) => *arena,
            CachedIn::Public(_) => ArenaId::DEFAULT,
        }
    }
}

/// The sealed `extent`, of `len` bytes, in the process's default cache.
#[cfg(test)]
pub(crate) async fn cached(
    local: &Arc<LocalCell<HostDir>>,
    cached_in: CachedIn,
    fetcher: Option<Arc<crate::bucket::HostFetcher>>,
    extent: ExtentId,
    len: u64,
) -> Result<Arc<CachedExtent>> {
    crate::Host::process().cache().cached(local, cached_in, fetcher, extent, len).await
}

/// The key `extent` of `arena` is cached by: the default arena's, if it holds it, as every VM
/// process may read it, a tenant's too: one copy (TASK-234).
fn held_key(
    extents: &HashMap<(ArenaId, ExtentId), Cached>,
    arena: ArenaId,
    extent: ExtentId,
) -> (ArenaId, ExtentId) {
    let public = (ArenaId::DEFAULT, extent);
    if arena != ArenaId::DEFAULT && extents.contains_key(&public) {
        public
    } else {
        (arena, extent)
    }
}

/// The cached `extent` of `arena`, if it is cached.
#[cfg(test)]
pub(crate) fn find(arena: ArenaId, extent: ExtentId) -> Option<Arc<CachedExtent>> {
    crate::Host::process().cache().find(arena, extent)
}

/// The extents cached: how many something but the cache holds (`pinned`), as a window's mapping, a
/// read or a page-in does, which it keeps however many it holds, and how many nothing else does
/// (`idle`), which it lets go of past its bound (TASK-104), the extent daemon's gauges
/// (`crate::cladefsd::metrics`, TASK-131.6).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Entries {
    pub pinned: u64,
    pub idle: u64,
}

/// The extents cached, by arena and ID.
#[cfg(test)]
pub(crate) fn keys() -> Vec<(ArenaId, ExtentId)> {
    crate::Host::process().cache().keys()
}

/// Takes out of the cache `extents` those asked for longest ago that nothing else holds, down to
/// seven eighths of `most`, and returns them, for the caller to let go of past the cache's lock
/// (TASK-104).
fn let_go_least_used(
    extents: &mut HashMap<(ArenaId, ExtentId), Cached>,
    most: usize,
) -> Vec<Cached> {
    // Those no cell open reads first, which none asks for but to cache them anew.
    let mut idle: Vec<((bool, u64), (ArenaId, ExtentId))> = (extents.iter())
        .filter(|(_, cached)| Arc::strong_count(&cached.extent) == 1)
        .map(|(key, cached)| {
            let open = cached.holders.any_open();
            ((open, cached.extent.used.load(Ordering::Relaxed)), *key)
        })
        .collect();
    idle.sort_unstable_by_key(|(order, _)| *order);
    let excess = extents.len().saturating_sub(most - most / 8);
    idle.into_iter().take(excess).filter_map(|(_, key)| extents.remove(&key)).collect()
}

/// cladefs: a run of `pages` pages of `arena` on `slabs` for a page-in, waiting for relief if it
/// must, off the pager's workers (board 5662).
async fn allocate(
    slabs: &Arc<slabs::HostSlabs>,
    arena: ArenaId,
    pages: u64,
) -> Result<PartSlab, slabs::NoMemory> {
    if let Ok(slab) = slabs.alloc(arena, pages, slabs::Wait::No) {
        return Ok(slab);
    }
    #[cfg(not(madsim))]
    {
        let slabs = slabs.clone();
        return tokio::task::spawn_blocking(move || slabs.alloc(arena, pages, slabs::Wait::Yes))
            .await
            .expect("a page-in's allocation panicked");
    }
    #[cfg(madsim)]
    return slabs.alloc(arena, pages, slabs::Wait::Yes);
}

#[cfg(test)]
pub(crate) fn set_total_is_the_hosts(is: bool) {
    crate::Host::process().cache().set_total_is_the_hosts(is);
}

/// cladefs: how many times page-ins waited for memory ([`waiting_for_memory`]).
// The process's count, every simulated host's together, as a process's metrics are.
static WAITS_FOR_MEMORY: AtomicU64 = AtomicU64::new(0);

/// cladefs: says a page-in waits for memory, the host's full and relief having found nothing to
/// free: at most once every 10 s, with how many waits since, as a full host makes many (board 5662).
fn waiting_for_memory() {
    // When the process last said its page-ins wait: one warning for every host, at most every 10 s.
    static SAID_AT: AtomicU64 = AtomicU64::new(0);
    let waited = WAITS_FOR_MEMORY.fetch_add(1, Ordering::Relaxed) + 1;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let said = SAID_AT.load(Ordering::Relaxed);
    if now >= said + 10
        && SAID_AT.compare_exchange(said, now, Ordering::Relaxed, Ordering::Relaxed).is_ok()
    {
        log::warn!("cladefs: page-ins wait for memory, the host's full: {waited} waits so far");
    }
}

/// A page-in found no memory of its arena to read into.
#[derive(Debug, thiserror::Error)]
#[error("no memory to read into")]
struct NoPages;

/// The errno a page-in that failed with `error` fails its readers with: `ENOENT` for an extent
/// the cell doesn't hold, `ENOMEM` for want of memory, `EACCES` for revoked keys, `EIO` for the
/// rest.
fn read_errno(error: &anyhow::Error) -> Errno {
    if error.downcast_ref::<NoExtent>().is_some() {
        Errno::ENOENT
    } else if error.downcast_ref::<NoPages>().is_some() {
        Errno::ENOMEM
    } else if let Some(&errno) = error.downcast_ref::<Errno>() {
        errno
    } else {
        Errno::EIO
    }
}

/// Runs `page_in` on `cache`'s pager threads: under Shuttle, as a task of the test's run, which
/// its scheduler interleaves with the test's threads.
fn spawn_page_in(
    cache: &Weak<ExtentCache>,
    page_in: impl std::future::Future<Output = ()> + Send + 'static,
) {
    #[cfg(feature = "shuttle")]
    {
        let _ = cache;
        drop(shuttle::future::spawn(page_in));
    }
    #[cfg(not(feature = "shuttle"))]
    if let Some(cache) = cache.upgrade() {
        drop(cache.pager().spawn(page_in));
    }
}

/// What a cached extent's parts are read from: its contents.
pub(crate) trait Contents: Send + Sync {
    /// Reads its bytes at `offset` into `buf`: a future, which a read that waits, as a fetch from
    /// object storage does, holds no thread while it waits on.
    fn read<'a>(
        &'a self,
        offset: u64,
        buf: &'a mut [u8],
    ) -> BoxFuture<'a, std::result::Result<(), anyhow::Error>>;

    /// Fills `slab` with its `len` bytes at `offset`, and zeros past them: read into a buffer
    /// ([`Contents::read`]), and written into the slab, as the daemon maps no slab (doc-26).
    fn fill<'a>(
        &'a self,
        offset: u64,
        len: u64,
        slab: &'a PartSlab,
    ) -> BoxFuture<'a, std::result::Result<(), anyhow::Error>> {
        Box::pin(async move {
            let mut buffer = buffers::Buffer::take(slab.len() as usize);
            let (data, past) = buffer.split_at_mut(len as usize);
            past.fill(0);
            self.read(offset, data).await?;
            slab.write(0, &buffer)?;
            Ok(())
        })
    }

    /// Where, in the extent, its parts of `extent_frame::PART` bytes begin: an offset within the
    /// first, in whole pages. A part is decompressed whole, so reading less of it costs nearly as
    /// much.
    fn phase(&self) -> u64 {
        0
    }
}

/// The buffers page-ins read parts into before writing them into their slabs: a few kept, the
/// pager's threads' worth, as a part's (1 MiB) is the size the C library's allocator maps and
/// unmaps each of on its own (`cladefsd::memory::MMAP_THRESHOLD`). Those past them are freed as
/// they are given back. A page-in holds one only as its thread reads a part from a cell, and copies
/// one in memory (`cladefsd::process::PAGE_INS`).
mod buffers {
    use std::sync::Mutex;

    /// How many it keeps.
    const KEPT: usize = super::PAGER_THREADS;

    // The process's pool of spare buffers: what it holds is no host's.
    static KEPT_BUFFERS: Mutex<Vec<Box<[u8]>>> = Mutex::new(Vec::new());

    /// A buffer of a part at most, given back as it is dropped.
    pub(super) struct Buffer {
        bytes: Option<Box<[u8]>>,
        len: usize,
    }

    impl Buffer {
        /// A buffer of `len` bytes, a slab's at most, its bytes whatever they were.
        pub(super) fn take(len: usize) -> Self {
            let kept = KEPT_BUFFERS.lock().unwrap().pop();
            let bytes = kept.unwrap_or_else(|| vec![0; crate::slabs::SLAB as usize].into());
            assert!(len <= bytes.len(), "a buffer of {len} bytes, past a slab's");
            Self { bytes: Some(bytes), len }
        }
    }

    impl std::ops::Deref for Buffer {
        type Target = [u8];

        fn deref(&self) -> &[u8] {
            &self.bytes.as_ref().expect("a buffer until dropped")[..self.len]
        }
    }

    impl std::ops::DerefMut for Buffer {
        fn deref_mut(&mut self) -> &mut [u8] {
            &mut self.bytes.as_mut().expect("a buffer until dropped")[..self.len]
        }
    }

    impl Drop for Buffer {
        fn drop(&mut self) {
            let mut kept = KEPT_BUFFERS.lock().unwrap();
            if kept.len() < KEPT {
                kept.extend(self.bytes.take());
            }
        }
    }
}

/// A sealed extent's contents, in the cells that hold it, or in object storage, if one holds it
/// without its data.
struct StoredExtent {
    /// The cells it is read through (TASK-79.7).
    holders: Arc<Holders>,
    extent: ExtentId,
    /// Where the parts of the object its data is in begin in it: its offset in the object, in
    /// whole pages, which page-ins are.
    phase: u64,
}

impl StoredExtent {
    /// `extent`, read through `holders`, laid out as `local` holds it.
    fn new(holders: Arc<Holders>, local: &LocalCell<HostDir>, extent: ExtentId) -> Self {
        // Its offset in the data of the object it is in: a segment's, its own file's, or the one
        // object storage holds it in.
        let offset = match local.place(extent) {
            Ok(Some(local_cell::Place::Run { offset, .. })) => offset,
            Ok(Some(local_cell::Place::Bucket(location))) => location.offset,
            _ => 0,
        };
        let phase = offset % extent_frame::PART / page_size() * page_size();
        Self { holders, extent, phase }
    }
}

/// Where a stored extent's bytes were found ([`StoredExtent::look`]).
enum Found {
    /// Read from a cell of the host's.
    Read,
    /// In object storage, which the fetch fetches: their bytes, as many as there are.
    Fetch(BoxFuture<'static, std::result::Result<Vec<u8>, anyhow::Error>>),
}

impl StoredExtent {
    /// Reads its bytes at `offset` into `buf` from the first of the cells that asked for it that
    /// holds it still (TASK-79.7), which doesn't wait for long, on the pager's thread; or finds them
    /// in object storage, to fetch, which the page-in waits for holding no thread.
    fn look(&self, offset: u64, buf: &mut [u8]) -> std::result::Result<Found, anyhow::Error> {
        stats::add(Counter::PageIns, 1);
        let id = self.extent;
        let mut failed = anyhow::Error::new(NoExtent(id));
        for (local, fetcher) in self.holders.open() {
            match local_cell::upload::blocking(|| read_local(&local, id, offset, buf)) {
                Ok(()) => {
                    stats::add(Counter::PageInBytes, buf.len() as u64);
                    return Ok(Found::Read);
                }
                Err(ref error)
                    if let Some(data) = error.downcast_ref::<local_cell::BucketData>()
                        && let Some(fetcher) = fetcher =>
                {
                    let (local, location) = (Arc::downgrade(&local), data.location.clone());
                    let len = buf.len() as u64;
                    let fetched = fetched(local, fetcher, id, location, offset, len);
                    return Ok(Found::Fetch(Box::pin(fetched)));
                }
                Err(error) => failed = error,
            }
        }
        Err(failed)
    }
}

impl Contents for StoredExtent {
    fn read<'a>(
        &'a self,
        offset: u64,
        buf: &'a mut [u8],
    ) -> BoxFuture<'a, std::result::Result<(), anyhow::Error>> {
        Box::pin(async move {
            if let Found::Fetch(fetched) = self.look(offset, buf)? {
                let data = fetched.await?;
                buf[..data.len()].copy_from_slice(&data);
                buf[data.len()..].fill(0);
            }
            Ok(())
        })
    }

    /// Read from a cell into a buffer, and written; or fetched, and what is fetched written, the
    /// buffer given back while the fetch waits, so that page-ins hold buffers only as their
    /// threads read (`process::PAGE_INS`).
    fn fill<'a>(
        &'a self,
        offset: u64,
        len: u64,
        slab: &'a PartSlab,
    ) -> BoxFuture<'a, std::result::Result<(), anyhow::Error>> {
        Box::pin(async move {
            let mut buffer = buffers::Buffer::take(slab.len() as usize);
            let (data, past) = buffer.split_at_mut(len as usize);
            past.fill(0);
            match self.look(offset, data)? {
                Found::Read => slab.write(0, &buffer)?,
                Found::Fetch(fetched) => {
                    drop(buffer);
                    let data = fetched.await?;
                    slab.write(0, &data)?;
                    slab.zero(data.len() as u64)?;
                }
            }
            Ok(())
        })
    }

    fn phase(&self) -> u64 {
        self.phase
    }
}

/// cladefs: reads `buf.len()` bytes at `offset` in `id`, as `local` holds it: an extent freed
/// since it was mapped, which its uploader keeps, reads as kept. One `local` holds
/// in the bucket fails with [`local_cell::BucketData`], saying where.
fn read_local(
    local: &LocalCell<HostDir>,
    id: ExtentId,
    offset: u64,
    buf: &mut [u8],
) -> anyhow::Result<()> {
    match local.read(id, offset, buf) {
        Err(error) if error.downcast_ref::<NoExtent>().is_some() => {
            local.read_kept(id, offset, buf)
        }
        read => read,
    }
}

/// cladefs: fetches `len` bytes at `offset` in `id`, which `local` held at `location` in the
/// bucket, through `fetcher`: as many as it holds; or reads them where `local` holds it now,
/// relocated meanwhile (`HostFetcher::read_following`, doc-27). The cell held weakly, as the cache
/// holds its cells, and looked at only if the fetch finds its object gone: the cache keeps none
/// open, nor its files, once it closes.
pub(crate) async fn fetched(
    local: Weak<LocalCell<HostDir>>,
    fetcher: Arc<crate::bucket::HostFetcher>,
    id: ExtentId,
    location: extent_frame::segment::Location,
    offset: u64,
    len: u64,
) -> anyhow::Result<Vec<u8>> {
    let again = || {
        let local = local.upgrade().ok_or_else(|| {
            anyhow::Error::new(NoExtent(id)).context("its cell closed as it was fetched")
        })?;
        let mut here = vec![0; len as usize];
        read_local(&local, id, offset, &mut here).map(|()| here)
    };
    match fetcher.read_following(id, location, (offset, len), again).await? {
        Followed::Fetched(data) => {
            stats::add(Counter::FetchedBytes, data.len() as u64);
            Ok(data)
        }
        Followed::Here(here) => {
            stats::add(Counter::PageInBytes, here.len() as u64);
            Ok(here)
        }
    }
}

/// An extent's contents read into the default arena from a tenant's copy of it (TASK-234): its
/// parts in memory copied, slab to slab, and the rest read from the cells; either way, the copy's
/// parts go, and the copy once none is left.
struct Promoted {
    from: SharedLock<Mutex<Option<Arc<CachedExtent>>>>,
    stored: Box<dyn Contents>,
}

impl Contents for Promoted {
    fn read<'a>(
        &'a self,
        offset: u64,
        buf: &'a mut [u8],
    ) -> BoxFuture<'a, std::result::Result<(), anyhow::Error>> {
        self.stored.read(offset, buf)
    }

    /// The copy's part, if it holds it: copied from its slab, in the kernel, no bounce through the
    /// daemon's memory where the copy's parts are laid out as these are (doc-26); else read in.
    fn fill<'a>(
        &'a self,
        offset: u64,
        len: u64,
        slab: &'a PartSlab,
    ) -> BoxFuture<'a, std::result::Result<(), anyhow::Error>> {
        let ps = page_size();
        let pages = offset / ps..(offset + slab.len()).div_ceil(ps);
        let from = self.from.with(Mutex::lock).clone();
        Box::pin(async move {
            let copied = match &from {
                Some(from) => from.copy_into(offset, len, slab)?,
                None => false,
            };
            if !copied {
                self.stored.fill(offset, len, slab).await?;
            }
            if let Some(from) = from {
                from.evict_pages(pages);
                if from.present.load(Ordering::Relaxed) == 0 {
                    self.from.with(Mutex::lock).take();
                }
            }
            Ok(())
        })
    }

    fn phase(&self) -> u64 {
        self.stored.phase()
    }
}

/// cladefs: where a page-in read its part from, which it says once it has, before its pages are
/// in: none if from the host's cells alone, which go to no fetcher ([`trace::sourced`],
/// TASK-672.2).
type Landed = OnceLock<Option<trace::Source>>;

/// What an eviction leaves in its part's slot: its number, which holds no memory, the slab being
/// its eviction's ([`Eviction`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EvictionToken(u64);

impl EvictionToken {
    /// A token no other eviction has.
    fn new() -> Self {
        // Numbers the process gives out: unique among simulated hosts too, all it asks.
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// A part of a cached extent.
enum PartSlot {
    /// Never paged in yet.
    Absent,
    /// Evicted at the given `ExtentCache::clock_ms`; faulting a governor-evicted part back in
    /// within [`REFAULT_WINDOW_MS`] records a refault for the memory governor.
    Evicted { at_ms: u64, by_governor: bool },
    /// Being read in, which [`CachedExtent::changed`] tells of the end of.
    Filling,
    /// In memory: its slab, the slot's.
    Present(PartSlab),
    /// Being evicted, by the eviction holding the token, which holds the slab: unmapped from the
    /// windows that map it, then freed.
    Evicting(EvictionToken),
    /// Its read in failed, as the next waiting for it are told; asked for again, it is read in
    /// anew.
    Failed(Errno),
}

/// A part's slab, taken out of its slot by an eviction, the eviction's alone, which frees it once
/// it has unmapped it from the windows ([`CachedExtent::evict_pages`]).
struct Eviction {
    part: usize,
    slab: PartSlab,
    token: EvictionToken,
}

/// The runs evictions of an extent's parts hold: what a window's pages of them are found by.
struct Evicted {
    arena: ArenaId,
    /// The runs, by their offsets in the arena's object: start to end.
    runs: std::collections::BTreeMap<u64, u64>,
}

impl Evicted {
    fn new(arena: ArenaId, evictions: &[Eviction]) -> Self {
        let runs = (evictions.iter())
            .map(|eviction| {
                let at = eviction.slab.location().offset;
                (at, at + eviction.slab.len())
            })
            .collect();
        Self { arena, runs }
    }

    /// Whether the page at `at` is of a run evicted.
    fn contains(&self, at: PhysLocation) -> bool {
        at.arena == self.arena
            && (self.runs.range(..=at.offset).next_back()).is_some_and(|(_, end)| at.offset < *end)
    }
}

/// A cached extent's parts, and the windows that map them, under its lock.
struct Parts {
    slots: Vec<PartSlot>,
    /// Consecutive cold `cachestat(2)` observations per part; reset on install/eviction/activity.
    cold_strikes: Vec<u8>,
    /// The windows that map it, each once, by its identity ([`window_identity`]).
    windows: HashMap<usize, Weak<Window>>,
    /// The part after the last a page-in asked for: where a reader reading on asks next.
    next: usize,
    /// cladefs: set by [`CachedExtent::revoke`] when `cache::forget` drops this extent: any
    /// in-flight `Filling` page-in discards its slab instead of publishing it, and every waiter
    /// fails with `EACCES` (TASK-630, board 2658, board 5544, board 2704).
    revoked: bool,
}

/// The part page `page` of an extent is in: of an object's parts (`extent_frame::PART`), which a
/// sealed extent's data is stored and fetched in, its first shortened by the extent's `phase`
/// ([`Contents::phase`]).
fn part_index(page: u64, phase: u64) -> usize {
    ((page * page_size() + phase) / extent_frame::PART) as usize
}

/// The identity of `window`, the same for each of its handles: what an extent's window set keys it
/// by.
fn window_identity(window: &Window) -> usize {
    window as *const Window as usize
}

/// A sealed extent in the cache: a table of its parts. See the module docs.
pub(crate) struct CachedExtent {
    cache: Weak<ExtentCache>,
    slabs: Arc<slabs::HostSlabs>,
    arena: ArenaId,
    extent: ExtentId,
    contents: Box<dyn Contents>,
    len: u64,
    parts: SharedLock<Mutex<Parts>>,
    /// How many of its parts are in memory, which relief asks of every extent cached (TASK-92.33).
    present: AtomicUsize,
    /// When it was last asked for or faulted on, by `ExtentCache::clock`: those longest ago are
    /// evicted first (TASK-50.3, TASK-92.32).
    used: AtomicU64,
    /// Notified as parts come in, fail to, or go.
    changed: event_listener::Event,
}

impl CachedExtent {
    /// `extent`, of `len` bytes, read from `contents` into slabs of `arena` on `cache`.
    pub(crate) fn new_on(
        cache: &Arc<ExtentCache>,
        arena: ArenaId,
        extent: ExtentId,
        contents: Box<dyn Contents>,
        len: u64,
    ) -> Self {
        let phase = contents.phase();
        let pages = len.div_ceil(page_size());
        let count = pages.checked_sub(1).map_or(0, |last| part_index(last, phase) + 1);
        let slots = std::iter::repeat_with(|| PartSlot::Absent).take(count).collect();
        // A reader from its start reads on.
        let parts = Parts {
            slots,
            cold_strikes: vec![0u8; count],
            windows: HashMap::new(),
            next: 0,
            revoked: false,
        };
        Self {
            cache: Arc::downgrade(cache),
            slabs: cache.slabs.clone(),
            arena,
            extent,
            contents,
            len,
            parts: SharedLock::new("cached extent parts", Mutex::new(parts)),
            present: AtomicUsize::new(0),
            used: AtomicU64::new(0),
            changed: event_listener::Event::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn new(
        arena: ArenaId,
        extent: ExtentId,
        contents: Box<dyn Contents>,
        len: u64,
    ) -> Self {
        Self::new_on(crate::Host::process().cache(), arena, extent, contents, len)
    }

    /// Notes that it is in use now: asked for, or faulted on, as a guest keeping its mapping asks
    /// for it only once (TASK-92.32).
    fn used(&self) {
        if let Some(cache) = self.cache.upgrade() {
            self.used.store(cache.clock.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
        }
    }

    /// When relief evicts it, those first that sort first: whether a window maps it, as what
    /// guests keep mapped they may use without a sign, and an eviction takes from all of them,
    /// then when it was last used. `None` with nothing in memory to evict.
    pub(crate) fn eviction_order(&self) -> Option<(bool, u64)> {
        if self.present.load(Ordering::Relaxed) == 0 {
            return None;
        }
        let parts = self.parts.with(Mutex::lock);
        let mapped = parts.windows.values().any(|window| window.strong_count() > 0);
        Some((mapped, self.used.load(Ordering::Relaxed)))
    }

    /// Evicts individual present parts whose backing slabs Linux MGLRU has completely paged out to
    /// swap and marked cold (`cached_pages == 0 && evicted_pages > 0 && recently_evicted_pages ==
    /// 0`) on two consecutive scans. All `cachestat(2)` syscalls execute outside `self.parts` so
    /// DAX faults are never blocked.
    #[cfg_attr(any(not(target_os = "linux"), madsim), allow(dead_code))]
    fn evict_kernel_cold(&self, max_bytes: u64) -> u64 {
        if self.present.load(Ordering::Relaxed) == 0 || max_bytes == 0 {
            return 0;
        }
        let handles: Vec<(usize, std::sync::Arc<std::os::fd::OwnedFd>, u64, u64)> = {
            let parts = self.parts.with(Mutex::lock);
            parts
                .slots
                .iter()
                .enumerate()
                .filter_map(|(part, slot)| {
                    let PartSlot::Present(slab) = slot else { return None };
                    let (fd, off, len) = slab.residency_handle();
                    Some((part, fd, off, len))
                })
                .collect()
        };
        if handles.is_empty() {
            return 0;
        }
        let mut results = Vec::with_capacity(handles.len());
        for (part, fd, off, len) in handles {
            let Some(res) = crate::slabs::cachestat_range(&fd, off, len) else {
                return 0;
            };
            let is_cold =
                res.cached_pages == 0 && res.evicted_pages > 0 && res.recently_evicted_pages == 0;
            results.push((part, off, len, is_cold));
        }
        let cold_parts: Vec<usize> = {
            let mut parts = self.parts.with(Mutex::lock);
            let mut ready = Vec::new();
            for (part, off, len, is_cold) in results {
                let Some(PartSlot::Present(slab)) = parts.slots.get(part) else {
                    continue;
                };
                // Ignore stale samples if this slot was evicted and refilled while unlocked.
                if !slab.same_range(off, len) {
                    continue;
                }
                if is_cold {
                    parts.cold_strikes[part] = parts.cold_strikes[part].saturating_add(1);
                    if parts.cold_strikes[part] >= 2 {
                        parts.cold_strikes[part] = 0;
                        ready.push(part);
                    }
                } else {
                    parts.cold_strikes[part] = 0;
                }
            }
            ready
        };
        let mut harvested = 0u64;
        for part in cold_parts {
            if harvested >= max_bytes {
                break;
            }
            harvested += self.evict_pages_with_cause(self.part_pages(part), false);
        }
        harvested
    }

    /// How many pages it is.
    fn pages(&self) -> u64 {
        self.len.div_ceil(page_size())
    }

    /// The part `page` is in ([`part_index`]).
    fn part_of(&self, page: u64) -> usize {
        part_index(page, self.contents.phase())
    }

    /// The pages of its part `part`.
    fn part_pages(&self, part: usize) -> Range<u64> {
        let (ps, phase, part) = (page_size(), self.contents.phase(), part as u64);
        let start = (part * extent_frame::PART).saturating_sub(phase) / ps;
        let end = ((part + 1) * extent_frame::PART - phase).div_ceil(ps).min(self.pages());
        start..end
    }

    /// The parts holding its pages `pages`.
    fn parts_of(&self, pages: Range<u64>) -> Range<usize> {
        let end = pages.end.min(self.pages());
        match pages.start < end {
            true => self.part_of(pages.start)..self.part_of(end - 1) + 1,
            false => 0..0,
        }
    }

    /// How many of its pages are in memory.
    #[cfg(test)]
    pub(crate) fn held(&self) -> u64 {
        let parts = self.parts.with(Mutex::lock);
        let present = parts.slots.iter().map(|slot| match slot {
            PartSlot::Present(slab) => slab.pages(),
            _ => 0,
        });
        present.sum()
    }

    /// How many windows its window set holds, those gone included until it next lets go of them.
    #[cfg(test)]
    pub(crate) fn windows_held(&self) -> usize {
        self.parts.with(Mutex::lock).windows.len()
    }

    /// Reads its bytes at `offset` into `data`, reading in its parts that aren't in memory, as a
    /// read of the extent that maps nothing does, awaiting them, holding no thread (doc-24, step
    /// 5). Past its end, zeros. cladefs: a part in memory is read from its slab as it is found,
    /// allocating nothing; one not, brought in first (`Source::bring_in`, a future boxed),
    /// TASK-453.
    pub(crate) async fn read(self: &Arc<Self>, offset: u64, data: &mut [u8]) -> Result<()> {
        let ps = page_size();
        let held = self.len.saturating_sub(offset).min(data.len() as u64) as usize;
        let (bytes, zeros) = data.split_at_mut(held);
        zeros.fill(0);
        let mut done = 0;
        while done < bytes.len() {
            let at = offset + done as u64;
            let page = at / ps;
            {
                let parts = self.parts.with(Mutex::lock);
                if parts.revoked {
                    return Err(Errno::EACCES);
                }
                let part = self.part_of(page);
                if let Some(PartSlot::Present(slab)) = parts.slots.get(part) {
                    let within = at - self.part_pages(part).start * ps;
                    let len = ((slab.len() - within) as usize).min(bytes.len() - done);
                    if let Err(err) = slab.read(within, &mut bytes[done..done + len]) {
                        log::error!("cladefs: reading {:?} from its slab: {err}", self.extent);
                        return Err(Errno::EIO);
                    }
                    done += len;
                    continue;
                }
            }
            // Read in, then read, unless evicted meanwhile: read in again.
            self.clone().bring_in(page).await?;
        }
        Ok(())
    }

    /// Marks its parts `parts` filling, as far as they aren't in memory or being read in, and
    /// returns those it marked, to read in; under its lock.
    fn mark_filling(table: &mut Parts, parts: impl IntoIterator<Item = usize>) -> Vec<usize> {
        (parts.into_iter())
            .filter(|&part| {
                let slot = &mut table.slots[part];
                let wanted = matches!(
                    slot,
                    PartSlot::Absent | PartSlot::Evicted { .. } | PartSlot::Failed(_)
                );
                if wanted {
                    *slot = PartSlot::Filling;
                }
                wanted
            })
            .collect()
    }

    /// cladefs: reads the parts of it holding its `len` bytes at `offset` into memory, ahead of
    /// any reader, as page-ins read them: those neither in memory nor being read in. Not as a
    /// reader reading on: what a page-in reads ahead of one it reads nothing of. How many parts it
    /// read in, once none of them is being read in any more. What a fork reads ahead of its guest
    /// of what its version's evidence predicts (`crate::evidence`, board 6225).
    pub(crate) async fn read_ahead(self: &Arc<Self>, offset: u64, len: u64) -> u64 {
        self.used();
        let ps = page_size();
        let pages = offset / ps..offset.saturating_add(len).div_ceil(ps);
        let parts = {
            let mut table = self.parts.with(Mutex::lock);
            Self::mark_filling(&mut table, self.parts_of(pages))
        };
        // Read in by the pager, as a page-in is, so that the reader's going leaves none of them
        // filling, which a fault would wait on for good.
        for &part in &parts {
            let span = self.page_in_span(part);
            spawn_page_in(&self.cache, self.clone().read_part(part, span, None));
        }
        loop {
            let listener = {
                let table = self.parts.with(Mutex::lock);
                let filling = |part: &usize| matches!(table.slots[*part], PartSlot::Filling);
                if !parts.iter().any(filling) {
                    break;
                }
                // Listened for under the lock, which what changes a part takes.
                self.changed.listen()
            };
            listener.await;
        }
        parts.len() as u64
    }

    /// Copies its `len` bytes at `offset`, and zeros past them, into `slab`, if its parts they are
    /// of are all in memory: whether they were (TASK-234). A part laid out as `slab` is copied from
    /// its slab in the kernel; others are read from their slabs into a buffer, and written.
    fn copy_into(
        &self,
        offset: u64,
        len: u64,
        slab: &PartSlab,
    ) -> std::result::Result<bool, anyhow::Error> {
        let ps = page_size();
        let parts = self.parts.with(Mutex::lock);
        let pages = offset / ps..(offset + slab.len()).div_ceil(ps);
        let mut present = Vec::new();
        for part in self.parts_of(pages.clone()) {
            let PartSlot::Present(from) = &parts.slots[part] else { return Ok(false) };
            present.push((self.part_pages(part), from));
        }
        if let [(from_pages, from)] = &present[..]
            && *from_pages == pages
            && from.len() == slab.len()
        {
            slab.copy_from(from)?;
            return Ok(true);
        }
        let mut buffer = buffers::Buffer::take(slab.len() as usize);
        buffer.fill(0);
        for (from_pages, from) in present {
            let start = from_pages.start.max(pages.start);
            let end = from_pages.end.min(pages.end);
            let within = (start - from_pages.start) * ps;
            let into = ((start - pages.start) * ps) as usize;
            let bytes = ((end - start) * ps) as usize;
            from.read(within, &mut buffer[into..into + bytes])?;
        }
        buffer[len as usize..].fill(0);
        slab.write(0, &buffer)?;
        Ok(true)
    }

    /// The windows that map it.
    fn windows(&self) -> Vec<Arc<Window>> {
        let parts = self.parts.with(Mutex::lock);
        parts.windows.values().filter_map(Weak::upgrade).collect()
    }

    /// The first pages of its parts in memory (TASK-234).
    fn present_parts(&self) -> Vec<u64> {
        let parts = self.parts.with(Mutex::lock);
        (parts.slots.iter().enumerate())
            .filter(|(_, slot)| matches!(slot, PartSlot::Present(_)))
            .map(|(part, _)| self.part_pages(part).start)
            .collect()
    }

    /// Evicts its parts in memory: unmaps them from the windows that map them, then frees their
    /// slabs, those staged to VM processes into quarantine (`slabs::quarantine_with`). Returns the
    /// bytes it evicted.
    pub(crate) fn evict(&self) -> u64 {
        self.evict_pages_with_cause(0..u64::MAX, false)
    }

    fn evict_with_cause(&self, by_governor: bool) -> u64 {
        self.evict_pages_with_cause(0..u64::MAX, by_governor)
    }

    /// cladefs: permanently revokes this cached extent on `cache::forget`, evicting all present
    /// pages, unmapping any composed DAX pages derived from this extent even if its parts were
    /// already evicted, and ensuring any in-flight `Filling` page-in drops its slab without
    /// installing it (TASK-630, board 2658, board 5544, board 2704; review 102 finding 351).
    pub(crate) fn revoke(&self) -> u64 {
        let windows = {
            let mut parts = self.parts.with(Mutex::lock);
            parts.revoked = true;
            parts.windows.retain(|_, window| window.strong_count() > 0);
            parts.windows.values().filter_map(Weak::upgrade).collect::<Vec<_>>()
        };
        self.changed.notify(usize::MAX);
        let evicted = self.evict();
        let identity = self as *const Self as *const () as usize;
        for window in windows {
            window.unmap_pages_of(identity, &|_| true);
        }
        evicted
    }

    /// Evicts its parts in memory holding its pages `pages`, as [`CachedExtent::evict`] evicts
    /// them all, each its own (spec rule ownTokens): their slabs moved out of their slots into
    /// evictions of this call's, tokens left in their place; unmapped from each window on its
    /// window set; their slots then absent, and their slabs freed, by this call alone. A part
    /// another eviction is evicting is that one's: this one leaves it.
    fn evict_pages(&self, pages: Range<u64>) -> u64 {
        self.evict_pages_with_cause(pages, false)
    }

    fn evict_pages_with_cause(&self, pages: Range<u64>, by_governor: bool) -> u64 {
        let (evictions, windows) = {
            let mut parts = self.parts.with(Mutex::lock);
            let mut evictions = Vec::new();
            for part in self.parts_of(pages) {
                let slot = &mut parts.slots[part];
                if !matches!(slot, PartSlot::Present(_)) {
                    continue;
                }
                let token = EvictionToken::new();
                let PartSlot::Present(slab) = std::mem::replace(slot, PartSlot::Evicting(token))
                else {
                    unreachable!("a part present")
                };
                parts.cold_strikes[part] = 0;
                self.present.fetch_sub(1, Ordering::Relaxed);
                evictions.push(Eviction { part, slab, token });
            }
            parts.windows.retain(|_, window| window.strong_count() > 0);
            (evictions, parts.windows.values().filter_map(Weak::upgrade).collect::<Vec<_>>())
        };
        if evictions.is_empty() {
            return 0;
        }
        let evicted = Evicted::new(self.arena, &evictions);
        let identity = self as *const Self as *const () as usize;
        for window in windows {
            window.unmap_pages_of(identity, &|at| evicted.contains(at));
        }
        {
            let at_ms =
                self.cache.upgrade().map_or(1, |cache| cache.clock_ms.load(Ordering::Relaxed));
            let mut parts = self.parts.with(Mutex::lock);
            for eviction in &evictions {
                let slot = &mut parts.slots[eviction.part];
                assert!(
                    matches!(slot, PartSlot::Evicting(token) if *token == eviction.token),
                    "part {} of {:?} changed under its eviction",
                    eviction.part,
                    self.extent
                );
                *slot = PartSlot::Evicted { at_ms, by_governor };
            }
        }
        self.changed.notify(usize::MAX);
        let bytes = evictions.iter().map(|eviction| eviction.slab.len()).sum();
        // Freed past the extent's lock, each into quarantine if it was staged.
        drop(evictions);
        bytes
    }

    /// cladefs: the span of a page-in of its part `part`, `cache.page_in` (`bucket_cell::trace`,
    /// TASK-258): its reads, from the cell or the bucket, its children.
    fn page_in_span(&self, part: usize) -> trace::Span {
        let pages = self.part_pages(part);
        let span = trace::Span::start("cache.page_in");
        span.set_with(trace::CELL, || base32_id::Short(self.extent.cell).to_string());
        span.set_count(trace::EXTENT, self.extent.sequence);
        span.set_count(trace::ARENA, u64::from(self.arena.0));
        span.set_count(trace::BYTES, (pages.end - pages.start) * page_size());
        span
    }

    /// Reads in its part `part`, which a page-in marked filling, into a slab of its arena, and
    /// puts it in its slot: `span` the page-in's, the parent of its reads (TASK-258). cladefs: says
    /// where it read it from on `span`, `local` if from the host's cells alone, and tells
    /// `landed`, if a bring-in waits to hear it, before the part is in; `span` ended before those
    /// waiting for it are told, so that their spans end after it (TASK-672.2).
    async fn read_part(
        self: Arc<Self>,
        part: usize,
        span: trace::Span,
        landed: Option<Arc<Landed>>,
    ) {
        let (read, source) = span.around(trace::sourced(self.fill(part))).await;
        span.set(trace::SOURCE, source.map_or("local", trace::Source::name));
        if let Some(landed) = landed {
            landed.set(source).expect("a page-in lands once");
        }
        let entered = span.enter();
        let mut table = self.parts.with(Mutex::lock);
        let revoked = table.revoked;
        let slot = &mut table.slots[part];
        assert!(matches!(slot, PartSlot::Filling), "part {part} of {:?} not filling", self.extent);
        if revoked {
            // Forgotten while this page-in was in flight: drop any filled slab rather than
            // publishing plaintext (TASK-630, board 5544, board 2704).
            drop(read);
            *slot = PartSlot::Failed(Errno::EACCES);
        } else {
            match read {
                Ok(slab) => {
                    *slot = PartSlot::Present(slab);
                    self.present.fetch_add(1, Ordering::Relaxed);
                    if let Some(cache) = self.cache.upgrade() {
                        cache.idle_change_seq.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(error) => {
                    // What the read failed with, not its errno alone, which said nothing of why
                    // (TASK-217).
                    let errno = read_errno(&error);
                    let at = self.part_pages(part).start * page_size();
                    log::error!(
                        "cladefs: reading {:?} at {at:#x}: {errno:?}: {error:#}",
                        self.extent
                    );
                    *slot = PartSlot::Failed(errno);
                }
            }
        }
        drop(table);
        drop(entered);
        drop(span);
        self.changed.notify(usize::MAX);
    }

    /// Its part `part`, read in: a slab of its arena, a part's long, filled with its bytes, zeros
    /// past its end.
    async fn fill(&self, part: usize) -> std::result::Result<PartSlab, anyhow::Error> {
        let pages = self.part_pages(part);
        // cladefs: with no memory for it, relief having found none to free, the host's memory
        // full, it waits for some rather than fail the guest's fault, which killed its VM (board
        // 5662), the governor telling the VM manager by refusing cells
        // (`cladefsd::governor::full`). At its arena's own cap, or past a limit the control API
        // set, it fails, as before.
        let mut pause = RETRY_FIRST;
        let slab = loop {
            match allocate(&self.slabs, self.arena, pages.end - pages.start).await {
                Ok(slab) => break slab,
                Err(slabs::NoMemory)
                    if self.slabs.room_may_come(self.arena)
                        && self.cache.upgrade().is_some_and(|cache| {
                            cache.total_is_the_hosts.load(Ordering::Relaxed)
                        }) =>
                {
                    waiting_for_memory();
                    tokio::time::sleep(pause).await;
                    pause = (pause * 2).min(RETRY_MOST);
                }
                Err(slabs::NoMemory) => return Err(anyhow::Error::new(NoPages)),
            }
        };
        let offset = pages.start * page_size();
        // What of it the extent holds; past it, zeros.
        let len = self.len.saturating_sub(offset).min(slab.len());
        self.contents.fill(offset, len, &slab).await?;
        Ok(slab)
    }

    /// Marks the part of `page`, and those after it a reader reading on asks for next, filling,
    /// as far as they aren't in memory or being read in, and returns them, to read in; under its
    /// lock.
    fn parts_to_read(&self, table: &mut Parts, page: u64) -> Vec<usize> {
        let count = table.slots.len();
        let part = self.part_of(page);
        let ahead = match part == table.next {
            true => PARTS_AHEAD as usize,
            false => 0,
        };
        let parts = part..(part + 1 + ahead).min(count);
        table.next = parts.end;
        Self::mark_filling(table, parts)
    }
}

impl Source for CachedExtent {
    fn is_revoked(&self) -> bool {
        self.parts.with(Mutex::lock).revoked
    }

    fn each_present(&self, pages: Range<u64>, found: &mut dyn FnMut(Present<'_>)) {
        let (ps, table) = (page_size(), self.parts.with(Mutex::lock));
        if table.revoked {
            return;
        }
        for part in self.parts_of(pages.clone()) {
            let PartSlot::Present(slab) = &table.slots[part] else { continue };
            let part_pages = self.part_pages(part);
            let (start, end) = (part_pages.start.max(pages.start), part_pages.end.min(pages.end));
            let at = PhysLocation {
                arena: slab.arena(),
                offset: slab.location().offset + (start - part_pages.start) * ps,
            };
            found(Present { page: start, count: end - start, at, slab: Some(slab) });
        }
    }

    fn bring_in(self: Arc<Self>, page: u64) -> BoxFuture<'static, Result<bool>> {
        Box::pin(async move {
            self.used();
            let mut waited = false;
            // cladefs: where the page-in it started of its page's part read from, if it started
            // one (TASK-672.2).
            let mut landing: Option<Arc<Landed>> = None;
            loop {
                let mut reading = Vec::new();
                let listener = {
                    let mut table = self.parts.with(Mutex::lock);
                    if table.revoked {
                        return Err(Errno::EACCES);
                    }
                    let slot = (page < self.pages()).then(|| &table.slots[self.part_of(page)]);
                    let done = match slot {
                        None => Some(Err(Errno::ENOENT)),
                        Some(PartSlot::Present(_)) => Some(Ok(waited)),
                        Some(PartSlot::Failed(errno)) if waited => Some(Err(*errno)),
                        Some(PartSlot::Filling | PartSlot::Evicting(_)) => None,
                        Some(PartSlot::Evicted { at_ms, by_governor }) => {
                            let (at_ms, by_governor) = (*at_ms, *by_governor);
                            if by_governor && let Some(cache) = self.cache.upgrade() {
                                let now = cache.clock_ms.load(Ordering::Relaxed);
                                if now.saturating_sub(at_ms) <= REFAULT_WINDOW_MS {
                                    cache.refaults.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            reading = self.parts_to_read(&mut table, page);
                            None
                        }
                        Some(PartSlot::Absent | PartSlot::Failed(_)) => {
                            reading = self.parts_to_read(&mut table, page);
                            None
                        }
                    };
                    if let Some(done) = done {
                        drop(table);
                        // cladefs: what waited for the bucket, with the stack that did: this
                        // future's, polled by every future awaiting it, up to its task's, as the
                        // page-in, a task of its own, has none of theirs (TASK-672.2).
                        let landed = landing.as_ref().and_then(|landed| landed.get().copied());
                        if landed == Some(Some(trace::Source::Bucket)) {
                            trace::record_backtrace();
                        }
                        return done;
                    }
                    // Listened for under the lock, which what changes the part takes.
                    self.changed.listen()
                };
                // Started past the lock, which a page-in takes as it ends.
                let wanted = self.part_of(page);
                for part in reading {
                    // cladefs: a child of what faulted, or read, on the page; for the guest
                    // request that wants it first, if any, which its reads of object storage log
                    // (TASK-260).
                    let span = self.page_in_span(part);
                    let landed = (part == wanted).then(|| landing.insert(Arc::default()).clone());
                    let read = self.clone().read_part(part, span, landed);
                    spawn_page_in(
                        &self.cache,
                        crate::request::within(crate::request::current(), read),
                    );
                }
                listener.await;
                waited = true;
            }
        })
    }

    /// The rest of its part, which a page-in reads whole.
    fn brought_in_with(&self, page: u64) -> u64 {
        self.part_pages(self.part_of(page)).end.saturating_sub(page).max(1)
    }

    /// `window` is on its window set from now on, once however often it is told (spec rule
    /// registerOnce, TASK-602).
    fn mapped(&self, window: &Arc<Window>) {
        let mut table = self.parts.with(Mutex::lock);
        table.windows.insert(window_identity(window), Arc::downgrade(window));
    }

    fn unmapped(&self, window: &Window) {
        self.parts.with(Mutex::lock).windows.remove(&window_identity(window));
        if let Some(cache) = self.cache.upgrade() {
            cache.idle_change_seq.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn origin(&self) -> Origin {
        Origin::Extent(self.extent)
    }
}

/// Zeros, which windows map read-only where nothing else is to be read: one page of them, of the
/// default arena, which every VM process maps, wherever they are mapped. Never freed, it is staged
/// to none.
struct Zeros(PartSlab);

impl Source for Zeros {
    fn each_present(&self, pages: Range<u64>, found: &mut dyn FnMut(Present<'_>)) {
        for page in pages {
            found(Present { page, count: 1, at: self.0.location(), slab: None });
        }
    }

    fn bring_in(self: Arc<Self>, _page: u64) -> BoxFuture<'static, Result<bool>> {
        Box::pin(std::future::ready(Ok(false)))
    }

    fn origin(&self) -> Origin {
        Origin::Zeros
    }
}

/// cladefs: board 5662: page-ins of a host whose memory is full.
#[cfg(test)]
mod full_host {
    use super::*;

    /// Contents read once a gate opens: what page-ins in flight wait on, holding their pages.
    struct Gated(tokio::sync::watch::Receiver<bool>);

    impl Contents for Gated {
        fn read<'a>(
            &'a self,
            _offset: u64,
            buf: &'a mut [u8],
        ) -> BoxFuture<'a, std::result::Result<(), anyhow::Error>> {
            Box::pin(async move {
                self.0.clone().wait_for(|open| *open).await?;
                buf.fill(0x56);
                Ok(())
            })
        }
    }

    /// The extents of `count` parts each a page-in reads from `gate`, cached in the default arena.
    fn gated(count: u64, gate: &tokio::sync::watch::Receiver<bool>) -> Vec<Arc<CachedExtent>> {
        (0..count)
            .map(|sequence| {
                let extent = ExtentId { cell: 5662, sequence };
                let contents = Box::new(Gated(gate.clone()));
                let len = extent_frame::PART;
                let cached = Arc::new(CachedExtent::new(ArenaId::DEFAULT, extent, contents, len));
                let entry = Cached { extent: cached.clone(), holders: Arc::default() };
                crate::Host::process()
                    .cache()
                    .extents
                    .with(Mutex::lock)
                    .insert((ArenaId::DEFAULT, extent), entry);
                cached
            })
            .collect()
    }

    /// The process's memory, if no other test made it first, its arenas capped at `count` slabs:
    /// the parts that many hold.
    fn capped(count: u64) -> u64 {
        let _ = slabs::configure(16 * slabs::SLAB);
        crate::Host::process().cache().relieve_pressure();
        slabs::set_total_cap(Some(count * slabs::SLAB));
        count * slabs::SLAB / extent_frame::PART
    }

    /// Page-ins finding no page, the host's memory full and what holds it in flight, wait for it
    /// off the pager's threads, more of them than it has: those in flight then end, and relief
    /// gives their pages to those waiting, every page-in ending. Blocked on its threads, they
    /// waited for pages those in flight, never polled, never let go of: every fault on the host
    /// stalled (the review of 024c3be89).
    #[test]
    fn page_ins_waiting_for_memory_leave_the_pager_to_those_in_flight() {
        let held = capped(2);
        set_total_is_the_hosts(true);
        let (open, gate) = tokio::sync::watch::channel(false);
        let extents = gated(held + PAGER_THREADS as u64 + 2, &gate);
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let waits = WAITS_FOR_MEMORY.load(Ordering::Relaxed);
            let page_ins: Vec<_> =
                extents.iter().map(|extent| tokio::spawn(extent.clone().bring_in(0))).collect();
            // Once more page-ins wait for memory than the pager has threads.
            while WAITS_FOR_MEMORY.load(Ordering::Relaxed) < waits + PAGER_THREADS as u64 {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            open.send(true).unwrap();
            for page_in in page_ins {
                let ended = tokio::time::timeout(std::time::Duration::from_secs(30), page_in);
                assert_eq!(ended.await.expect("a page-in stalled").unwrap(), Ok(true));
            }
        });
        slabs::set_total_cap(None);
    }

    /// Past a limit the control API set on every arena's memory, not the host's, page-ins finding
    /// no page fail with ENOMEM, as before board 5662, rather than wait.
    #[test]
    fn page_ins_past_a_limit_set_fail_rather_than_wait() {
        let held = capped(2);
        set_total_is_the_hosts(false);
        let (open, gate) = tokio::sync::watch::channel(false);
        let extents = gated(held + 3, &gate);
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let mut page_ins: Vec<_> =
                extents.iter().map(|extent| tokio::spawn(extent.clone().bring_in(0))).collect();
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
            while page_ins.iter().filter(|page_in| page_in.is_finished()).count() < 3 {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the page-ins past the limit waited"
                );
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            let (failed, in_flight): (Vec<_>, Vec<_>) =
                page_ins.drain(..).partition(|page_in| page_in.is_finished());
            for page_in in failed {
                assert_eq!(page_in.await.unwrap(), Err(Errno::ENOMEM));
            }
            assert_eq!(in_flight.len() as u64, held);
            open.send(true).unwrap();
            for page_in in in_flight {
                assert_eq!(page_in.await.unwrap(), Ok(true));
            }
        });
        slabs::set_total_cap(None);
    }

    /// TASK-630 (board 2658) & board 5544: `cache::forget` while a part page-in is in `Filling`
    /// revokes the extent so the in-flight page-in fails with `EACCES`, drops its slab, and leaves
    /// zero bytes held in the extent cache.
    #[test]
    fn forget_revokes_in_flight_page_in_and_releases_slab() {
        let cell = crate::CellId { tenant: 77, id: 5544 };
        let extent = ExtentId { cell: cell.id, sequence: 1 };
        let (open, gate) = tokio::sync::watch::channel(false);
        let cached = Arc::new(CachedExtent::new(
            ArenaId::DEFAULT,
            extent,
            Box::new(Gated(gate)),
            extent_frame::PART,
        ));
        let entry = Cached { extent: cached.clone(), holders: Arc::default() };
        entry.holders.0.with(Mutex::lock).push(Holder { cell, local: Weak::new(), fetcher: None });
        let cache = crate::Host::process().cache();
        cache.extents.with(Mutex::lock).insert((ArenaId::DEFAULT, extent), entry);

        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let filling = tokio::spawn(cached.clone().bring_in(0));
            // Wait until the part enters `Filling`.
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let is_filling = {
                    let parts = cached.parts.with(Mutex::lock);
                    matches!(parts.slots[0], PartSlot::Filling)
                };
                if is_filling {
                    break;
                }
                assert!(tokio::time::Instant::now() < deadline, "page-in never entered Filling");
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            assert_eq!(cache.forget(|c| c == cell), (0, 1));
            open.send(true).unwrap();
            assert_eq!(filling.await.unwrap(), Err(Errno::EACCES));
            assert_eq!(cached.held(), 0);
            assert!(
                !keys().contains(&(ArenaId::DEFAULT, extent)),
                "forgotten extent remained in cache"
            );
        });
    }

    /// Review 102 finding 351: at a 1-slab tenant arena cap (`slabs::SLAB`), when a single full
    /// phase-0 part (`extent_frame::PART == SLAB`) fills the arena as the only eviction candidate,
    /// faulting a composed sub-page mapping must preserve the copied bytes across relief so
    /// self-eviction of the source part completes instead of livelocking; and a subsequent
    /// `cache::forget` / `CachedExtent::revoke` must still unmap that composed page even though
    /// the source part was already evicted.
    #[test]
    fn composed_fault_under_one_slab_cap_does_not_self_evict_livelock() {
        struct Byte(u8);
        impl Contents for Byte {
            fn read<'a>(
                &'a self,
                _offset: u64,
                buf: &'a mut [u8],
            ) -> BoxFuture<'a, std::result::Result<(), anyhow::Error>> {
                buf.fill(self.0);
                Box::pin(std::future::ready(Ok(())))
            }
        }

        let ps = page_size();
        let unit = ps / 4;
        let host = crate::Host::process();
        let cache = host.cache();
        cache.relieve_pressure();
        let tenant = crate::pages::Arena::new_on(host).unwrap();
        let tenant_id = tenant.id();
        host.slabs().set_arena_cap(tenant_id, Some(slabs::SLAB));

        let cell = crate::CellId { tenant: u64::from(tenant_id.0), id: 102_351 };
        let extent = ExtentId { cell: cell.id, sequence: 1 };
        let cached = Arc::new(CachedExtent::new_on(
            cache,
            tenant_id,
            extent,
            Box::new(Byte(0x6d)),
            extent_frame::PART,
        ));
        let entry = Cached { extent: cached.clone(), holders: Arc::default() };
        entry.holders.0.with(Mutex::lock).push(Holder { cell, local: Weak::new(), fetcher: None });
        cache.extents.with(Mutex::lock).insert((tenant_id, extent), entry);

        let source: Arc<dyn Source> = cached.clone();
        let (mut dax, window) =
            crate::dax::Dax::with_stage_and_unit_on(host, tenant_id, 2 * ps, unit, None, false)
                .unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            // Bring the full 1-slab part into memory so the tenant arena is 100% full.
            cached.clone().bring_in(0).await.unwrap();
            assert_eq!(host.slabs().arena_bytes_in_use(tenant_id), slabs::SLAB);

            // Map 1 logical sub-page so the host page requires composition (1-page PartSlab).
            dax.map(0, &source, 0, unit);
            let resolved = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                window.fault(0, crate::Access::Read),
            )
            .await
            .expect("composed fault livelocked under 1-slab tenant cap");
            assert_eq!(resolved, Ok(()));
            // SAFETY: Host page 0 was just faulted into `window`'s host memory.
            assert_eq!(unsafe { *window.host_base() }, 0x6d);
            // Source part was evicted to make room for the 1-page composed slab.
            assert_eq!(cached.held(), 0);

            // Revoking the extent via `cache::forget` must unmap the composed page even though
            // `cached` currently has no resident parts.
            assert_eq!(cache.forget(|c| c == cell), (0, 1));
            assert_eq!(
                window.fault(0, crate::Access::Read).await,
                Err(Errno::EACCES),
                "revoked composed page remained mapped or re-installed plaintext"
            );
        });
        host.slabs().set_arena_cap(tenant_id, None);
        drop((dax, window, source, cached, tenant));
    }

    /// EvictScope::IdleOnly evicts unmapped extents while preserving mapped extents untouched;
    /// EvictScope::All evicts mapped extents, and faulting an evicted part back in within
    /// `REFAULT_WINDOW_TICKS` increments `refaults()`, while faulting after the window expires
    /// does not.
    #[test]
    fn idle_only_eviction_preserves_mapped_extents_and_tracks_refaults() {
        struct Fill(u8);
        impl Contents for Fill {
            fn read<'a>(
                &'a self,
                _offset: u64,
                buf: &'a mut [u8],
            ) -> BoxFuture<'a, std::result::Result<(), anyhow::Error>> {
                buf.fill(self.0);
                Box::pin(std::future::ready(Ok(())))
            }
        }

        let host = crate::Host::new();
        let cache = host.cache();
        let mapped_id = ExtentId { cell: 9001, sequence: 1 };
        let idle_id = ExtentId { cell: 9001, sequence: 2 };
        let mapped = Arc::new(CachedExtent::new_on(
            cache,
            ArenaId::DEFAULT,
            mapped_id,
            Box::new(Fill(0xaa)),
            extent_frame::PART,
        ));
        let idle = Arc::new(CachedExtent::new_on(
            cache,
            ArenaId::DEFAULT,
            idle_id,
            Box::new(Fill(0xbb)),
            extent_frame::PART,
        ));
        {
            let mut extents = cache.extents.with(Mutex::lock);
            extents.insert(
                (ArenaId::DEFAULT, mapped_id),
                Cached { extent: mapped.clone(), holders: Arc::default() },
            );
            extents.insert(
                (ArenaId::DEFAULT, idle_id),
                Cached { extent: idle.clone(), holders: Arc::default() },
            );
        }

        let (mut dax, window) =
            crate::dax::Dax::new_on(&host, ArenaId::DEFAULT, extent_frame::PART, None).unwrap();
        let source: Arc<dyn Source> = mapped.clone();
        dax.map(0, &source, 0, extent_frame::PART);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            // Cold first-time page-ins must not count as refaults.
            mapped.clone().bring_in(0).await.unwrap();
            idle.clone().bring_in(0).await.unwrap();
            assert_eq!(cache.refaults(), 0);
            assert!(mapped.held() > 0 && idle.held() > 0);

            // IdleOnly evicts only the unmapped extent and leaves the mapped extent intact.
            let evicted_idle = cache.evict_with_scope(None, EvictScope::IdleOnly, |_| false);
            assert_eq!(evicted_idle, extent_frame::PART);
            assert_eq!(idle.held(), 0);
            assert!(mapped.held() > 0, "IdleOnly must not evict mapped extents");

            // EvictScope::All (tenant-cap / relief) must NOT trigger governor refault thrashing.
            let evicted_relief = cache.evict_with_scope(None, EvictScope::All, |_| false);
            assert_eq!(evicted_relief, extent_frame::PART);
            assert_eq!(mapped.held(), 0);
            mapped.clone().bring_in(0).await.unwrap();
            assert_eq!(
                cache.refaults(),
                0,
                "non-governor relief eviction must not trigger host-wide thrashing"
            );

            // EvictScope::GovernorMapped evicts the mapped extent; bringing it back within
            // REFAULT_WINDOW_MS records a governor refault.
            let evicted_mapped =
                cache.evict_with_scope(None, EvictScope::GovernorMapped, |_| false);
            assert_eq!(evicted_mapped, extent_frame::PART);
            assert_eq!(mapped.held(), 0);
            mapped.clone().bring_in(0).await.unwrap();
            assert_eq!(cache.refaults(), 1, "re-faulting a governor-evicted part must count");

            // Evict again, advance past REFAULT_WINDOW_MS, then fault back in: not a refault.
            assert_eq!(
                cache.evict_with_scope(None, EvictScope::GovernorMapped, |_| false),
                extent_frame::PART
            );
            cache.advance_ms(REFAULT_WINDOW_MS + 1);
            mapped.clone().bring_in(0).await.unwrap();
            assert_eq!(cache.refaults(), 1, "re-fault past REFAULT_WINDOW_TICKS must not count");
        });
        drop((dax, window, source, mapped, idle));
    }
}

#[cfg(all(test, feature = "shuttle"))]
mod shuttle_tests {
    //! The extent cache and its slabs under Shuttle (awslabs/shuttle): evictions of one extent's
    //! parts on threads of their own, racing a page-in and a map, against a window whose second
    //! stage records what it maps, and a quarantine that checks, as each slab is freed, that the
    //! stage maps none of its pages (TASK-560, board 2516; spec rule ownTokens).

    use super::*;
    use crate::Access;
    use crate::dax::{Dax, SecondStage};
    use crate::shuttle_testing::{config, runs};
    use crate::slabs::VmProcessId;
    use shuttle::scheduler::{PctScheduler, RandomScheduler};
    use shuttle::{Runner, future, thread};
    use std::collections::BTreeMap;

    /// An extent's contents, each part's bytes its number and one.
    struct Numbered;

    impl Contents for Numbered {
        fn read<'a>(
            &'a self,
            offset: u64,
            buf: &'a mut [u8],
        ) -> BoxFuture<'a, std::result::Result<(), anyhow::Error>> {
            buf.fill((offset / extent_frame::PART) as u8 + 1);
            Box::pin(std::future::ready(Ok(())))
        }
    }

    /// The VM process of [`Recording`].
    const VM: VmProcessId = VmProcessId(1);

    /// A second stage that makes each change as it is made, as a VM process would once it made
    /// them in order, and records the pages it maps: a slab the window staged here, freed, waits
    /// in quarantine for nothing, so that what frees it while a page of it is mapped here is
    /// caught as it frees it.
    #[derive(Default)]
    struct Recording(shuttle::sync::Mutex<BTreeMap<u64, PhysLocation>>);

    impl SecondStage for Recording {
        fn map(&self, offset: u64, phys: PhysLocation, len: u64) {
            let mut pages = self.0.lock().unwrap();
            for at in (0..len).step_by(page_size() as usize) {
                let page = PhysLocation { offset: phys.offset + at, ..phys };
                assert!(pages.insert(offset + at, page).is_none(), "mapped over {offset:#x}");
            }
        }

        fn unmap(&self, offset: u64, len: u64, _: crate::dax::Private) {
            let mut pages = self.0.lock().unwrap();
            for at in (offset..offset + len).step_by(page_size() as usize) {
                assert!(pages.remove(&at).is_some(), "unmapped {at:#x}, not mapped");
            }
        }

        fn vm_process(&self) -> Option<VmProcessId> {
            Some(VM)
        }
    }

    /// The stage of the schedule running, which the quarantine checks the slabs freed against.
    static STAGE: std::sync::Mutex<Option<Arc<Recording>>> = std::sync::Mutex::new(None);

    /// The quarantine of [`evictions_race_page_ins_and_maps`]: each slab freed, the stage maps none
    /// of its pages, or a VM process would keep reading memory freed, punched or another's.
    fn checking_quarantine() -> crate::slabs::QuarantineForTest {
        crate::slabs::quarantine_for_test(|freed| {
            assert_eq!(freed.staged(), [VM]);
            let stage = STAGE.lock().unwrap().clone().expect("a schedule's stage");
            let pages = stage.0.lock().unwrap();
            let still: Vec<_> = pages.iter().filter(|(_, at)| freed.contains(**at)).collect();
            assert!(still.is_empty(), "a second stage maps a freed run: {still:x?}");
        })
    }

    /// TASK-560 (board 2516): an extent of three parts, two in memory and mapped, evicted a part
    /// each by two threads at once, as relief and a promotion's copy do, while a third faults the
    /// third part in and a fourth MAPs the extent over its own range: no slab is freed while the
    /// window's stage maps a page of it. Each eviction frees its own part's slab alone, after it
    /// has unmapped it; the reaping of every evicting part by whichever eviction came last freed
    /// the other's before its unmaps were sent.
    fn evictions_race_page_ins_and_maps() {
        let (ps, part) = (page_size(), extent_frame::PART);
        let stage = Arc::new(Recording::default());
        *STAGE.lock().unwrap() = Some(stage.clone());
        let extent = ExtentId { cell: 0, sequence: 560 };
        let cached =
            Arc::new(CachedExtent::new(ArenaId::DEFAULT, extent, Box::new(Numbered), 3 * part));
        let source: Arc<dyn Source> = cached.clone();
        let (mut dax, window) = Dax::new(3 * part, Some(stage.clone())).unwrap();
        dax.map(0, &source, 0, 3 * part);
        for at in [0, part] {
            future::block_on(window.fault(at, Access::Read)).unwrap();
        }
        let evictors: Vec<_> = [0, part / ps]
            .into_iter()
            .map(|first| {
                let cached = cached.clone();
                thread::spawn(move || cached.evict_pages(first..first + part / ps))
            })
            .collect();
        let faulting = window.clone();
        let fault = thread::spawn(move || {
            future::block_on(faulting.fault(2 * part, Access::Read)).unwrap();
            future::block_on(faulting.fault(0, Access::Read)).unwrap();
        });
        let remapped = source.clone();
        let map = thread::spawn(move || {
            dax.map(part, &remapped, part, part);
            dax
        });
        for evictor in evictors {
            evictor.join().unwrap();
        }
        fault.join().unwrap();
        let mut dax = map.join().unwrap();
        assert_eq!(cached.windows_held(), 1);
        dax.unmap(0, 3 * part);
        cached.evict();
        drop((dax, window));
        STAGE.lock().unwrap().take();
    }

    #[test]
    fn evictions_race_page_ins_and_maps_in_any_order() {
        let _quarantine = checking_quarantine();
        Runner::new(RandomScheduler::new(runs(100)), config())
            .run(evictions_race_page_ins_and_maps);
        Runner::new(PctScheduler::new(3, runs(100)), config())
            .run(evictions_race_page_ins_and_maps);
    }
}
