// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The process's cache of sealed extents (`crate::extents`), in pages of physical memory
//! (`crate::pages`).
//!
//! A sealed extent is cached once, keyed by its ID and the arena its pages are of, which every cell
//! and window in the process shares: a table of its pages ([`CachedExtent`]), each read in from a
//! cell that holds the extent as it is first read or mapped, a part (`extent_frame::PART`) at a
//! time, on the pager's threads, and evicted, its extents asked for longest ago first, as memory
//! runs out (`physmem::relieve_with`) or the daemon is asked to give memory back. Windows map its
//! pages read-only: one copy of each page, whichever cell or fork maps it, as an extent never
//! changes once sealed, its ID its cell's to write once, which the cell keeps to (TASK-79.6).
//! A page evicted is unmapped from each window that maps it first; freed, it waits in quarantine
//! for the VM processes that may reach it still (`physmem::quarantine_with`, TASK-79.9).
//!
//! An extent the default arena holds is that one copy for a tenant's cell too, as every VM
//! process may read the default arena; and one a tenant's arena holds that a cell reads into the
//! default arena, as a public base's extent its publisher's cell had read, moves there: its pages
//! copied in memory, rather than paged in again, and evicted from the tenant's (TASK-234).

use crate::dax::{Origin, Source, Window};
use crate::error::{Errno, Result};
use crate::open_cell::PublicReach;
use crate::pages::{ArenaId, page_size};
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

/// The process's cache of sealed extents (see the module docs): its pager's threads, which read
/// their pages from the cells, and the extents, by arena and ID.
struct ExtentCache {
    /// The threads the pager reads extents' parts on.
    pager: tokio::runtime::Runtime,
    /// By the arena their pages are of (TASK-43), and their IDs. Every tenant's, which a panic
    /// holding aborts the daemon (`local_cell::shared`, TASK-149.24); as do the others here.
    extents: SharedLock<Mutex<HashMap<(ArenaId, ExtentId), Cached>>>,
    /// The arenas but the default that extents were cached of, to look for an extent's copy in,
    /// taken after `extents` (TASK-234).
    arenas: SharedLock<Mutex<HashSet<ArenaId>>>,
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
/// keeps it for a follower's view: a cell that frees an extent its siblings map, or closes,
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
            let (local, fetcher) = (Arc::downgrade(local), fetcher.map(Arc::downgrade));
            holders.push(Holder { local, fetcher });
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

    /// cladefs: lets go of those whose cells `scope` says are of it, and of those closed: no
    /// page-in reads through them again (TASK-411.7). Returns whether it let go of one of `scope`.
    fn forget(&self, scope: &dyn Fn(crate::CellId) -> bool) -> bool {
        let mut forgot = false;
        self.0.with(Mutex::lock).retain(|holder| match holder.local.upgrade() {
            None => false,
            Some(local) if scope(local.description().cell) => {
                forgot = true;
                false
            }
            Some(_) => true,
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

node_local::node_local! {
    /// The cache's clock, which counts the extents asked for.
    static CACHE_CLOCK: AtomicU64 = AtomicU64::new(0);
}

node_local::node_local! {
    /// The most extents the cache keeps (TASK-104): each its table of pages, and the cells it reads
    /// through, which an extent nothing maps and whose pages were evicted keeps still. Under
    /// pgbench, small extents by the hundred thousand, the tables of extents never asked for again
    /// would be most of what the cache holds. Past this, those asked for longest ago that nothing
    /// else holds (no window maps them) go, down to seven eighths of it; one asked for again is
    /// read anew from its cell. Tests lower it.
    pub(crate) static MOST_CACHED: AtomicUsize = AtomicUsize::new(1 << 16);
}

node_local::node_local! {
    /// The number of extents past which the cache next lets go of those over its bound: the bound,
    /// or, if its last pass couldn't get below it, as the windows' mappings hold the extents they
    /// map, an eighth of the bound more than it left (TASK-109.2). Each lookup past the bound
    /// sorted every extent cached, under the cache's lock, and let go of none: an 8 GiB window,
    /// mapping more than the bound, never full, so never reclaiming, made each MAP 3-6 ms.
    static TRIM_AT: AtomicUsize = AtomicUsize::new(0);
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

node_local::node_local! {
    static EXTENT_CACHE: OnceLock<ExtentCache> = OnceLock::new();
}

fn extent_cache() -> &'static ExtentCache {
    EXTENT_CACHE.get().get_or_init(|| {
        let pager = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(PAGER_THREADS)
            .thread_name("cladefs pager")
            // cladefs: for page-ins waiting for memory (`CachedExtent::read_pages`, board 5662).
            .enable_all()
            .build()
            .expect("the extent cache's pager");
        ExtentCache {
            pager,
            extents: SharedLock::new("extent cache", Mutex::default()),
            arenas: SharedLock::new("extent cache arenas", Mutex::default()),
        }
    })
}

/// Evicts the pages of the cached extents of `arena`, or of every arena, those no window maps
/// first, then those used longest ago (`CachedExtent::eviction_order`), until `enough` says, of
/// the bytes evicted so far, that they are, or none is left to evict: memory an arena past its cap,
/// or the arenas past the total's, give back (TASK-50.3). Returns the bytes it evicted. Evicted
/// pages are read in again as they are touched.
///
/// cladefs: in a span of its own, `cache.relief` (`bucket_cell::trace`, TASK-258).
pub(crate) fn evict_least_used(arena: Option<ArenaId>, enough: impl FnMut(u64) -> bool) -> u64 {
    let span = trace::Span::start("cache.relief");
    if let Some(arena) = arena {
        span.set_count(trace::ARENA, u64::from(arena.0));
    }
    let (evicted, extents) = evict_in_order(arena, enough);
    span.set_count(trace::BYTES, evicted);
    span.set_count(trace::EXTENTS, extents);
    evicted
}

/// Evicts as [`evict_least_used`] says: returns the bytes it evicted, and of how many extents.
fn evict_in_order(arena: Option<ArenaId>, mut enough: impl FnMut(u64) -> bool) -> (u64, u64) {
    let Some(cache) = EXTENT_CACHE.get().get() else { return (0, 0) };
    let candidates: Vec<Arc<CachedExtent>> = {
        let extents = cache.extents.with(Mutex::lock);
        (extents.iter())
            .filter(|((of, _), _)| arena.is_none_or(|arena| *of == arena))
            .map(|(_, cached)| cached.extent.clone())
            .collect()
    };
    // Past the cache's lock, as each takes its own; those evicted already, which stay cached
    // until freed, left out, each pass having gone through them all (TASK-92.33).
    let mut candidates: Vec<_> = (candidates.into_iter())
        .filter_map(|extent| Some((extent.eviction_order()?, extent)))
        .collect();
    candidates.sort_unstable_by_key(|(order, _)| *order);
    let (mut evicted, mut extents) = (0, 0);
    for (_, extent) in candidates {
        if enough(evicted) {
            break;
        }
        let bytes = extent.evict();
        evicted += bytes;
        extents += u64::from(bytes > 0);
    }
    (evicted, extents)
}

/// Gives, once, the relief of arenas under pressure (TASK-50.3) its evictor, which frees pages of
/// cached extents (`physmem::relieve_with`): before a cell of any arena opens, whose allocations
/// may wait for it.
pub(crate) fn relieve_pressure() {
    node_local::node_local! {
        static STARTED: std::sync::Once = std::sync::Once::new();
    }
    STARTED.get().call_once(|| {
        let relieved = physmem::relieve_with(|arena, bytes| {
            evict_least_used(arena, |evicted| evicted >= bytes)
        });
        // Without its thread, an allocation that finds its arena out of memory fails. cladefs:
        // under madsim, physical memory is the process's, which another simulated host started
        // relief of already, and starts no thread (doc-24, TASK-685.14).
        match relieved {
            Err(err) if cfg!(madsim) && err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => log::error!("cladefs: starting the thread that frees capped memory: {err}"),
            Ok(()) => {}
        }
    });
}

/// Lets go of the extents cached in `arena`: a tenant's, which nothing holds now
/// (`crate::pages::Arena`, TASK-43.4). Their pages are freed as the last holders of the extents,
/// page-ins in flight, let them go.
pub(crate) fn evict_arena(arena: ArenaId) {
    let Some(cache) = EXTENT_CACHE.get().get() else { return };
    let evicted: Vec<Cached> = {
        let mut extents = cache.extents.with(Mutex::lock);
        cache.arenas.with(Mutex::lock).remove(&arena);
        extents.extract_if(|(of, _), _| *of == arena).map(|(_, cached)| cached).collect()
    };
    drop(evicted);
}

/// cladefs: forgets the cells whose cells `scope` says are of it, as their host does as it is
/// told their tenant's KEK is revoked or destroyed, or their cell shredded (doc-16, Shredding;
/// TASK-411.7): no cached extent's page is read in through them again, and an extent no other open
/// cell holds goes from the cache, its pages evicted, unmapped from every window that maps them
/// first, so that what the cells' keys decrypted is no longer in memory, nor read in again: a
/// window that maps it, faulting, finds no cell to read it through. An extent another cell open
/// holds stays, its pages theirs to read (another tenant's fork of a shared version, a public
/// base, a fork that inherited it). Returns the bytes of pages it evicted, and of how many
/// extents.
pub(crate) fn forget(scope: impl Fn(crate::CellId) -> bool) -> (u64, u64) {
    let Some(cache) = EXTENT_CACHE.get().get() else { return (0, 0) };
    let gone: Vec<Cached> = {
        let mut extents = cache.extents.with(Mutex::lock);
        let doomed: Vec<(ArenaId, ExtentId)> = (extents.iter())
            .filter(|(_, cached)| cached.holders.forget(&scope) && !cached.holders.any_open())
            .map(|(key, _)| *key)
            .collect();
        doomed.iter().filter_map(|key| extents.remove(key)).collect()
    };
    let evicted = gone.iter().map(|cached| cached.extent.evict()).sum();
    (evicted, gone.len() as u64)
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

/// The sealed `extent`, of `len` bytes, in the cache, of `cached_in`'s arena, cached from `local`
/// if it isn't: read from it, or from `fetcher`; and from any
/// other cell that asked for it, which `local` is one of from now on (TASK-79.7). cladefs: a copy
/// of it a tenant's arena held, moved here, has its pages brought in awaited (doc-24, step 5).
pub(crate) async fn cached(
    local: &Arc<LocalCell<HostDir>>,
    cached_in: CachedIn,
    fetcher: Option<Arc<crate::bucket::HostFetcher>>,
    extent: ExtentId,
    len: u64,
) -> Result<Arc<CachedExtent>> {
    let (found, moved) = cache_now(local, cached_in, fetcher, extent, len);
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

/// cladefs: the cache's part of [`cached`], under its lock, which holds no await: the extent
/// cached, and a tenant's copy of it moved here, if one was, its windows rebound, whose pages its
/// caller brings in.
fn cache_now(
    local: &Arc<LocalCell<HostDir>>,
    cached_in: CachedIn,
    fetcher: Option<Arc<crate::bucket::HostFetcher>>,
    extent: ExtentId,
    len: u64,
) -> (Arc<CachedExtent>, Option<Arc<CachedExtent>>) {
    let cache = extent_cache();
    let mut extents = cache.extents.with(Mutex::lock);
    let key = held_key(&extents, cached_in.arena(), extent);
    let mut moved = None;
    if !extents.contains_key(&key) {
        let holders = Arc::new(Holders::default());
        let stored = StoredExtent::new(holders.clone(), local, extent);
        let mut contents: Box<dyn Contents> = Box::new(stored);
        if let CachedIn::Public(reach) = &cached_in {
            // A tenant's copy, which moves here (TASK-234): of this extent alone.
            assert_eq!(reach.extent(), extent, "a tenant's copy moved for another extent");
            let arenas = cache.arenas.with(Mutex::lock);
            let other = arenas.iter().find_map(|of| extents.remove(&(*of, extent)));
            if let Some(other) = other {
                holders.adopt(&other.holders);
                moved = Some(other.extent.clone());
                let from = SharedLock::new("promoted extent", Mutex::new(Some(other.extent)));
                contents = Box::new(Promoted { from, stored: contents });
            }
        } else if key.0 != ArenaId::DEFAULT {
            cache.arenas.with(Mutex::lock).insert(key.0);
        }
        let new = Arc::new(CachedExtent::new(key.0, extent, contents, len));
        extents.insert(key, Cached { extent: new, holders });
    }
    let cached = extents.get_mut(&key).expect("cached");
    cached.extent.used();
    cached.holders.add(local, fetcher.as_ref());
    let found = cached.extent.clone();
    let most = MOST_CACHED.get().load(Ordering::Relaxed);
    let gone = (extents.len() > most.max(TRIM_AT.get().load(Ordering::Relaxed))).then(|| {
        let started = std::time::Instant::now();
        let gone = let_go_least_used(&mut extents, most);
        TRIM_AT.get().store(extents.len() + most / 8, Ordering::Relaxed);
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
    let cache = EXTENT_CACHE.get().get()?;
    let extents = cache.extents.with(Mutex::lock);
    extents.get(&(arena, extent)).map(|cached| cached.extent.clone())
}

/// Lets go of the cached `extent` of `arena`, which `local` frees, unless another cell that asked
/// for it holds it still, which reads its pages through itself (TASK-79.7): one that holds it reads
/// them anew. Returns what keeps its pages, which the window's mappings of it map zeros in place
/// of, if it was cached.
pub(crate) fn freed(
    local: &Arc<LocalCell<HostDir>>,
    arena: ArenaId,
    extent: ExtentId,
) -> Option<Arc<CachedExtent>> {
    let cache = EXTENT_CACHE.get().get()?;
    let (key, cached, holders) = {
        let extents = cache.extents.with(Mutex::lock);
        let key = held_key(&extents, arena, extent);
        let (cached, holders) =
            extents.get(&key).map(|cached| (cached.extent.clone(), cached.holders.clone()))?;
        (key, cached, holders)
    };
    if !holders.another_holds(local, extent) {
        let mut extents = cache.extents.with(Mutex::lock);
        let same = extents.get(&key).is_some_and(|now| Arc::ptr_eq(&now.extent, &cached));
        let removed = same.then(|| extents.remove(&key));
        drop(extents);
        drop(removed);
    }
    Some(cached)
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

/// The extents cached, as of now: a pass over them, under the cache's lock.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn entries() -> Entries {
    let Some(cache) = EXTENT_CACHE.get().get() else { return Entries::default() };
    let extents = cache.extents.with(Mutex::lock);
    let idle = extents.values().filter(|cached| Arc::strong_count(&cached.extent) == 1).count();
    Entries { pinned: (extents.len() - idle) as u64, idle: idle as u64 }
}

/// The extents cached, by arena and ID.
#[cfg(test)]
pub(crate) fn keys() -> Vec<(ArenaId, ExtentId)> {
    let Some(cache) = EXTENT_CACHE.get().get() else { return Vec::new() };
    cache.extents.with(Mutex::lock).keys().copied().collect()
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

/// cladefs: `count` pages of `arena` for a page-in, as `physmem::alloc_run` allocates them, waiting
/// for relief if it must, off the pager's workers: a wait for relief is one for pages it evicted to
/// come back from quarantine, as each VM process unmaps them, seconds on a full host, which the
/// page-ins in flight on those workers, whose pages relief would free next, must not wait behind
/// (board 5662). At once, with no wait, if it can be; under madsim, inline, as the simulated host's
/// threads are its runtime's.
async fn allocate(arena: ArenaId, count: usize) -> Result<Vec<physmem::Page>, physmem::NoMemory> {
    if let Ok(frames) = physmem::alloc_run(arena, count, physmem::Wait::No) {
        return Ok(frames);
    }
    #[cfg(not(madsim))]
    return tokio::task::spawn_blocking(move || {
        physmem::alloc_run(arena, count, physmem::Wait::Yes)
    })
    .await
    .expect("a page-in's allocation panicked");
    #[cfg(madsim)]
    return physmem::alloc_run(arena, count, physmem::Wait::Yes);
}

/// cladefs: whether the cap on every arena's memory together is the memory governor's, the host's
/// memory, not a limit the control API set: what a page-in finding no page waits out rather than
/// fails (board 5662). Set as the cap is (`cladefsd::memory`); a daemon not governed has none.
static TOTAL_IS_THE_HOSTS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// cladefs: says whether the cap on every arena's memory together is the host's
/// ([`TOTAL_IS_THE_HOSTS`]).
pub(crate) fn set_total_is_the_hosts(is: bool) {
    TOTAL_IS_THE_HOSTS.store(is, Ordering::Relaxed);
}

/// cladefs: how many times page-ins waited for memory ([`waiting_for_memory`]).
static WAITS_FOR_MEMORY: AtomicU64 = AtomicU64::new(0);

/// cladefs: says a page-in waits for memory, the host's full and relief having found nothing to
/// free: at most once every 10 s, with how many waits since, as a full host makes many (board 5662).
fn waiting_for_memory() {
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

/// A page-in found no pages of its arena to read into.
#[derive(Debug, thiserror::Error)]
#[error("no pages to read into")]
struct NoPages;

/// The errno a page-in that failed with `error` fails its readers with: `ENOENT` for an extent
/// the cell doesn't hold, `ENOMEM` for want of pages, `EIO` for the rest.
fn read_errno(error: &anyhow::Error) -> Errno {
    if error.downcast_ref::<NoExtent>().is_some() {
        Errno::ENOENT
    } else if error.downcast_ref::<NoPages>().is_some() {
        Errno::ENOMEM
    } else {
        Errno::EIO
    }
}

/// What a cached extent's pages are read from: its contents, in parts.
pub(crate) trait Contents: Send + Sync {
    /// Reads its bytes at `offset` into `buf`: a future, which a read that waits, as a fetch from
    /// object storage does, holds no thread while it waits on.
    fn read<'a>(
        &'a self,
        offset: u64,
        buf: &'a mut [u8],
    ) -> BoxFuture<'a, std::result::Result<(), anyhow::Error>>;

    /// Where, in the extent, its parts of `extent_frame::PART` bytes begin: an offset within the
    /// first, in whole pages. A part is decompressed whole, so reading less of it costs nearly as
    /// much.
    fn phase(&self) -> u64 {
        0
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

impl Contents for StoredExtent {
    /// A read of the local cell, which doesn't wait for long, on the pager's thread; or a fetch,
    /// which the page-in waits for holding no thread.
    fn read<'a>(
        &'a self,
        offset: u64,
        buf: &'a mut [u8],
    ) -> BoxFuture<'a, std::result::Result<(), anyhow::Error>> {
        stats::add(Counter::PageIns, 1);
        let id = self.extent;
        // Through the first of the cells that asked for it that holds it still (TASK-79.7).
        let mut failed = anyhow::Error::new(NoExtent(id));
        for (local, fetcher) in self.holders.open() {
            // An extent freed since it was mapped, which a follower's view keeps, reads as kept
            // (doc-8).
            let read = match local.read(id, offset, buf) {
                Err(error) if error.downcast_ref::<NoExtent>().is_some() => {
                    local.read_kept(id, offset, buf)
                }
                read => read,
            };
            match read {
                Ok(()) => {
                    stats::add(Counter::PageInBytes, buf.len() as u64);
                    return Box::pin(std::future::ready(Ok(())));
                }
                Err(ref error)
                    if let Some(data) = error.downcast_ref::<local_cell::BucketData>()
                        && let Some(fetcher) = fetcher =>
                {
                    let fetched = fetcher.read(id, data.location.clone(), offset, buf.len() as u64);
                    return Box::pin(async move {
                        let data = fetched.await?;
                        stats::add(Counter::FetchedBytes, data.len() as u64);
                        buf[..data.len()].copy_from_slice(&data);
                        buf[data.len()..].fill(0);
                        Ok(())
                    });
                }
                Err(error) => failed = error,
            }
        }
        Box::pin(std::future::ready(Err(failed)))
    }

    fn phase(&self) -> u64 {
        self.phase
    }
}

/// An extent's contents read into the default arena from a tenant's copy of it (TASK-234): its
/// pages in memory copied, and the rest read from the cells; either way, the copy's pages go, and
/// the copy once none is left.
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
        let ps = page_size();
        let pages = offset / ps..(offset + buf.len() as u64).div_ceil(ps);
        let from = self.from.with(Mutex::lock).clone();
        Box::pin(async move {
            let copied = from.as_ref().is_some_and(|from| from.copy_present(offset, buf));
            if !copied {
                self.stored.read(offset, buf).await?;
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

/// A page of a cached extent.
enum Slot {
    /// Not in memory.
    Absent,
    /// Being read in, which [`CachedExtent::changed`] tells of the end of.
    Filling,
    /// In memory.
    Present(physmem::Page),
    /// Being evicted: unmapped from the windows that map it, then let go of.
    Evicting(physmem::Page),
    /// Its read in failed, as the next waiting for it are told; asked for again, it is read in
    /// anew.
    Failed(Errno),
}

/// A cached extent's pages, and the windows that map them, under its lock.
struct Pages {
    slots: Vec<Slot>,
    windows: Vec<Weak<Window>>,
    /// The page after the last part a page-in asked for: where a reader reading on asks next.
    next: u64,
}

/// A sealed extent in the cache: a table of its pages. See the module docs.
pub(crate) struct CachedExtent {
    arena: ArenaId,
    extent: ExtentId,
    contents: Box<dyn Contents>,
    len: u64,
    pages: SharedLock<Mutex<Pages>>,
    /// How many of its pages are in memory, which relief asks of every extent cached (TASK-92.33).
    present: AtomicUsize,
    /// When it was last asked for or faulted on, by [`CACHE_CLOCK`]: those longest ago are evicted
    /// first (TASK-50.3, TASK-92.32).
    used: AtomicU64,
    /// Notified as pages come in, fail to, or go.
    changed: event_listener::Event,
}

impl CachedExtent {
    /// `extent`, of `len` bytes, read from `contents` into pages of `arena`.
    pub(crate) fn new(
        arena: ArenaId,
        extent: ExtentId,
        contents: Box<dyn Contents>,
        len: u64,
    ) -> Self {
        let count = len.div_ceil(page_size()) as usize;
        let slots = std::iter::repeat_with(|| Slot::Absent).take(count).collect();
        // A reader from its start reads on.
        let pages = SharedLock::new(
            "cached extent pages",
            Mutex::new(Pages { slots, windows: Vec::new(), next: 0 }),
        );
        Self {
            arena,
            extent,
            contents,
            len,
            pages,
            present: AtomicUsize::new(0),
            used: AtomicU64::new(0),
            changed: event_listener::Event::new(),
        }
    }

    /// Notes that it is in use now: asked for, or faulted on, as a guest keeping its mapping asks
    /// for it only once (TASK-92.32).
    fn used(&self) {
        self.used.store(CACHE_CLOCK.get().fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
    }

    /// When relief evicts it, those first that sort first: whether a window maps it, as what
    /// guests keep mapped they may use without a sign, and an eviction takes from all of them,
    /// then when it was last used. `None` with nothing in memory to evict.
    pub(crate) fn eviction_order(&self) -> Option<(bool, u64)> {
        if self.present.load(Ordering::Relaxed) == 0 {
            return None;
        }
        let pages = self.pages.with(Mutex::lock);
        let mapped = pages.windows.iter().any(|window| window.strong_count() > 0);
        Some((mapped, self.used.load(Ordering::Relaxed)))
    }

    /// The part `page` is in, of its `pages`, as page numbers: a part of an object
    /// (`extent_frame::PART`), which a sealed extent's data is stored and fetched in.
    fn part_of(&self, page: u64, pages: u64) -> Range<u64> {
        let (ps, part, phase) = (page_size(), extent_frame::PART, self.contents.phase());
        let index = (page * ps + phase) / part;
        let start = (index * part).saturating_sub(phase) / ps;
        let end = ((index + 1) * part - phase).div_ceil(ps).min(pages);
        start..end
    }

    /// How many of its pages are in memory.
    #[cfg(test)]
    pub(crate) fn held(&self) -> u64 {
        let pages = self.pages.with(Mutex::lock);
        pages.slots.iter().filter(|slot| matches!(slot, Slot::Present(_))).count() as u64
    }

    /// Reads its bytes at `offset` into `data`, reading in its pages that aren't in memory, as a
    /// read of the extent that maps nothing does, awaiting them, holding no thread (doc-24, step
    /// 5). Past its end, zeros. cladefs: a page in memory is copied as it is found, allocating
    /// nothing; one not, brought in first (`Source::bring_in`, a future boxed), TASK-453.
    pub(crate) async fn read(self: &Arc<Self>, offset: u64, data: &mut [u8]) -> Result<()> {
        let ps = page_size();
        let held = self.len.saturating_sub(offset).min(data.len() as u64) as usize;
        let (bytes, zeros) = data.split_at_mut(held);
        zeros.fill(0);
        let mut done = 0;
        while done < bytes.len() {
            let at = offset + done as u64;
            let (page, within) = (at / ps, (at % ps) as usize);
            let len = (ps as usize - within).min(bytes.len() - done);
            {
                let pages = self.pages.with(Mutex::lock);
                if let Some(Slot::Present(frame)) = pages.slots.get(page as usize) {
                    // SAFETY: The page is the extent's, in memory while its lock is held, and
                    // encrypted: only read.
                    let memory = unsafe { std::slice::from_raw_parts(frame.as_ptr(), ps as usize) };
                    bytes[done..done + len].copy_from_slice(&memory[within..within + len]);
                    done += len;
                    continue;
                }
            }
            // Read in, then copied, unless evicted meanwhile: read in again.
            self.clone().bring_in(page).await?;
        }
        Ok(())
    }

    /// Copies its bytes at `offset` into `data`, if its pages they are of are all in memory:
    /// whether they were (TASK-234).
    fn copy_present(&self, offset: u64, data: &mut [u8]) -> bool {
        let ps = page_size();
        let pages = self.pages.with(Mutex::lock);
        let first = offset / ps;
        let count = (data.len() as u64).div_ceil(ps);
        let frames: Option<Vec<_>> = (first..first + count)
            .map(|page| match pages.slots.get(page as usize) {
                Some(Slot::Present(frame)) => Some(frame.as_ptr()),
                _ => None,
            })
            .collect();
        let Some(frames) = frames else { return false };
        for (frame, bytes) in frames.into_iter().zip(data.chunks_mut(ps as usize)) {
            // SAFETY: The page is the extent's, in memory while its lock is held, and sealed: only
            // read.
            unsafe { std::ptr::copy_nonoverlapping(frame, bytes.as_mut_ptr(), bytes.len()) };
        }
        true
    }

    /// The windows that map it.
    fn windows(&self) -> Vec<Arc<Window>> {
        let pages = self.pages.with(Mutex::lock);
        pages.windows.iter().filter_map(Weak::upgrade).collect()
    }

    /// The first pages of its parts of which a page is in memory (TASK-234).
    fn present_parts(&self) -> Vec<u64> {
        let pages = self.pages.with(Mutex::lock);
        let count = pages.slots.len() as u64;
        let mut parts = Vec::new();
        let mut page = 0;
        while page < count {
            let part = self.part_of(page, count);
            if part.clone().any(|page| matches!(pages.slots[page as usize], Slot::Present(_))) {
                parts.push(part.start);
            }
            page = part.end;
        }
        parts
    }

    /// Evicts its pages in memory: unmaps them from the windows that map them, then lets go of
    /// them, those a window's second stage mapped into quarantine (`physmem::quarantine_with`).
    /// Returns the bytes it evicted.
    pub(crate) fn evict(&self) -> u64 {
        self.evict_pages(0..u64::MAX)
    }

    /// Evicts its pages in memory of `range`, as [`CachedExtent::evict`] evicts them all.
    fn evict_pages(&self, range: Range<u64>) -> u64 {
        let (frames, windows) = {
            let mut pages = self.pages.with(Mutex::lock);
            let mut frames = HashSet::new();
            let end = range.end.min(pages.slots.len() as u64) as usize;
            let start = (range.start as usize).min(end);
            for slot in &mut pages.slots[start..end] {
                *slot = match std::mem::replace(slot, Slot::Absent) {
                    Slot::Present(frame) => {
                        frames.insert(frame.frame());
                        self.present.fetch_sub(1, Ordering::Relaxed);
                        Slot::Evicting(frame)
                    }
                    other => other,
                };
            }
            pages.windows.retain(|window| window.strong_count() > 0);
            (frames, pages.windows.iter().filter_map(Weak::upgrade).collect::<Vec<_>>())
        };
        if frames.is_empty() {
            return 0;
        }
        let identity = self as *const Self as *const () as usize;
        for window in windows {
            window.unmap_pages_of(identity, &|frame| frames.contains(&frame));
        }
        let evicted: Vec<physmem::Page> = {
            let mut pages = self.pages.with(Mutex::lock);
            (pages.slots.iter_mut())
                .filter_map(|slot| match std::mem::replace(slot, Slot::Absent) {
                    Slot::Evicting(frame) => Some(frame),
                    other => {
                        *slot = other;
                        None
                    }
                })
                .collect()
        };
        self.changed.notify(usize::MAX);
        let bytes = evicted.len() as u64 * page_size();
        drop(evicted);
        bytes
    }

    /// cladefs: the span of a page-in of its part `pages`, `cache.page_in` (`bucket_cell::trace`,
    /// TASK-258): its reads, from the cell or the bucket, its children.
    fn page_in_span(&self, pages: &Range<u64>) -> trace::Span {
        let span = trace::Span::start("cache.page_in");
        span.set_with(trace::CELL, || base32_id::Short(self.extent.cell).to_string());
        span.set_count(trace::EXTENT, self.extent.sequence);
        span.set_count(trace::ARENA, u64::from(self.arena.0));
        span.set_count(trace::BYTES, (pages.end - pages.start) * page_size());
        span
    }

    /// Reads in the part `pages` of the extent, which a page-in marked filling, into pages of its
    /// arena, and puts those still filling then in its table: `span` the page-in's, the parent of
    /// its reads (TASK-258). cladefs: says where it read them from on `span`, `local` if from the
    /// host's cells alone, and tells `landed`, if a bring-in waits to hear it, before the pages
    /// are in; `span` ended before those waiting for them are told, so that their spans end after
    /// it (TASK-672.2).
    async fn read_part(
        self: Arc<Self>,
        pages: Range<u64>,
        span: trace::Span,
        landed: Option<Arc<Landed>>,
    ) {
        let ps = page_size();
        let (read, source) = span.around(trace::sourced(self.read_pages(pages.clone()))).await;
        span.set(trace::SOURCE, source.map_or("local", trace::Source::name));
        if let Some(landed) = landed {
            landed.set(source).expect("a page-in lands once");
        }
        let entered = span.enter();
        let mut table = self.pages.with(Mutex::lock);
        match read {
            Ok(frames) => {
                for (page, frame) in pages.zip(frames) {
                    let slot = &mut table.slots[page as usize];
                    if let Slot::Filling = slot {
                        *slot = Slot::Present(frame);
                        self.present.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Err(error) => {
                // What the read failed with, not its errno alone, which said nothing of why
                // (TASK-217).
                let errno = read_errno(&error);
                log::error!(
                    "cladefs: reading {:?} at {:#x}: {errno:?}: {error:#}",
                    self.extent,
                    pages.start * ps
                );
                for page in pages {
                    let slot = &mut table.slots[page as usize];
                    if let Slot::Filling = slot {
                        *slot = Slot::Failed(errno);
                    }
                }
            }
        }
        drop(table);
        drop(entered);
        drop(span);
        self.changed.notify(usize::MAX);
    }

    /// The pages `pages` of the extent, read in: pages of its arena, consecutive where they can be
    /// (`physmem::alloc_run`), so that a window maps them in as few runs as can be (TASK-108.1),
    /// and read into in place if they are, else copied into.
    async fn read_pages(
        &self,
        pages: Range<u64>,
    ) -> std::result::Result<Vec<physmem::Page>, anyhow::Error> {
        let ps = page_size();
        let count = (pages.end - pages.start) as usize;
        // cladefs: with no page for it, relief having found none to free, the host's memory full,
        // it waits for one rather than fail the guest's fault, which killed its VM (board 5662),
        // the governor telling the VM manager by refusing cells (`cladefsd::governor::full`). At
        // its arena's own cap, or past a limit the control API set, it fails, as before.
        let mut pause = RETRY_FIRST;
        let frames = loop {
            match allocate(self.arena, count).await {
                Ok(frames) => break frames,
                Err(physmem::NoMemory)
                    if physmem::room_may_come(self.arena)
                        && TOTAL_IS_THE_HOSTS.load(Ordering::Relaxed) =>
                {
                    waiting_for_memory();
                    tokio::time::sleep(pause).await;
                    pause = (pause * 2).min(RETRY_MOST);
                }
                Err(physmem::NoMemory) => return Err(anyhow::Error::new(NoPages)),
            }
        };
        let offset = pages.start * ps;
        let len = count * ps as usize;
        // What of it the extent holds; past it, zeros.
        let read = self.len.saturating_sub(offset).min(len as u64) as usize;
        let consecutive = frames
            .windows(2)
            .all(|pair| pair[1].as_ptr() as usize == pair[0].as_ptr() as usize + ps as usize);
        let result = if consecutive {
            // SAFETY: The pages are consecutive in this process's memory, and allocated, and only
            // this read's until it puts them in the table.
            let memory = unsafe { std::slice::from_raw_parts_mut(frames[0].as_ptr(), len) };
            let (data, past) = memory.split_at_mut(read);
            past.fill(0);
            self.contents.read(offset, data).await
        } else {
            let mut data = vec![0; len];
            let result = self.contents.read(offset, &mut data[..read]).await;
            for (frame, bytes) in frames.iter().zip(data.chunks(ps as usize)) {
                // SAFETY: As above, a page at a time.
                unsafe {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), frame.as_ptr(), bytes.len())
                };
            }
            result
        };
        result?;
        Ok(frames)
    }

    /// Marks the parts of `page`, and those after it a reader reading on asks for next, filling,
    /// as far as they aren't in memory or being read in, and returns them, to read in; under its
    /// lock.
    fn parts_to_read(&self, table: &mut Pages, page: u64) -> Vec<Range<u64>> {
        let count = table.slots.len() as u64;
        let mut parts = Vec::new();
        let part = self.part_of(page, count);
        let sequential = part.start == table.next;
        let mut next = part.clone();
        parts.push(part);
        if sequential {
            for _ in 0..PARTS_AHEAD {
                if next.end >= count {
                    break;
                }
                next = self.part_of(next.end, count);
                parts.push(next.clone());
            }
        }
        table.next = parts.last().map_or(u64::MAX, |part| part.end);
        (parts.into_iter())
            .filter_map(|part| {
                let mut wanted = false;
                for page in part.clone() {
                    let slot = &mut table.slots[page as usize];
                    if matches!(slot, Slot::Absent | Slot::Failed(_)) {
                        *slot = Slot::Filling;
                        wanted = true;
                    }
                }
                wanted.then_some(part)
            })
            .collect()
    }
}

impl Source for CachedExtent {
    fn each_present(&self, pages: Range<u64>, found: &mut dyn FnMut(u64, &physmem::Page)) {
        let table = self.pages.with(Mutex::lock);
        for page in pages {
            if let Some(Slot::Present(frame)) = table.slots.get(page as usize) {
                found(page, frame);
            }
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
                let listener = {
                    let mut table = self.pages.with(Mutex::lock);
                    let done = match table.slots.get(page as usize) {
                        None => Some(Err(Errno::ENOENT)),
                        Some(Slot::Present(_)) => Some(Ok(waited)),
                        Some(Slot::Failed(errno)) if waited => Some(Err(*errno)),
                        Some(Slot::Filling | Slot::Evicting(_)) => None,
                        Some(Slot::Absent | Slot::Failed(_)) => {
                            // cladefs: for the guest request that wants it first, if any, which
                            // its reads of object storage log (TASK-260).
                            for part in self.parts_to_read(&mut table, page) {
                                // cladefs: a child of what faulted, or read, on the page.
                                let span = self.page_in_span(&part);
                                let landed = (part.contains(&page))
                                    .then(|| landing.insert(Arc::default()).clone());
                                let reading = self.clone().read_part(part, span, landed);
                                let request = crate::request::current();
                                extent_cache()
                                    .pager
                                    .spawn(crate::request::within(request, reading));
                            }
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
                    // Listened for under the lock, which what changes the page takes.
                    self.changed.listen()
                };
                listener.await;
                waited = true;
            }
        })
    }

    /// The rest of its part, which a page-in reads whole.
    fn brought_in_with(&self, page: u64) -> u64 {
        let count = self.pages.with(Mutex::lock).slots.len() as u64;
        self.part_of(page, count).end.saturating_sub(page).max(1)
    }

    fn mapped(&self, window: &Arc<Window>) {
        self.pages.with(Mutex::lock).windows.push(Arc::downgrade(window));
    }

    fn unmapped(&self, window: &Window) {
        let mut table = self.pages.with(Mutex::lock);
        table.windows.retain(|mapped| !std::ptr::eq(mapped.as_ptr(), window));
    }

    fn origin(&self) -> Origin {
        Origin::Extent(self.extent)
    }
}

/// Zeros, which windows map read-only where nothing else is to be read: one page of them, of the
/// default arena, which every VM process maps, wherever they are mapped.
struct Zeros(physmem::Page);

impl Source for Zeros {
    fn each_present(&self, pages: Range<u64>, found: &mut dyn FnMut(u64, &physmem::Page)) {
        for page in pages {
            found(page, &self.0);
        }
    }

    fn bring_in(self: Arc<Self>, _page: u64) -> BoxFuture<'static, Result<bool>> {
        Box::pin(std::future::ready(Ok(false)))
    }

    fn origin(&self) -> Origin {
        Origin::Zeros
    }
}

/// Zeros, for windows to map ([`Zeros`]): made once, and kept for the process's life. `ENOMEM` if
/// the first can't be made, which each window has made as it is created (`Dax::new`): a free maps
/// them holding the cell's locks, memory out, as memory run out finds them.
pub(crate) fn zeros() -> Result<Arc<dyn Source>> {
    static ZEROS: OnceLock<Arc<Zeros>> = OnceLock::new();
    if let Some(zeros) = ZEROS.get() {
        return Ok(zeros.clone());
    }
    let page = physmem::alloc(ArenaId::DEFAULT, physmem::Wait::Yes).map_err(|_| Errno::ENOMEM)?;
    // SAFETY: The page is allocated, and only this one's, never written after.
    unsafe { std::ptr::write_bytes(page.as_ptr(), 0, page_size() as usize) };
    Ok(ZEROS.get_or_init(|| Arc::new(Zeros(page))).clone())
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
                extent_cache().extents.with(Mutex::lock).insert((ArenaId::DEFAULT, extent), entry);
                cached
            })
            .collect()
    }

    /// The process's memory, if no other test made it first, its arenas capped at `chunks`
    /// chunks: the parts that many hold.
    fn capped(chunks: u64) -> u64 {
        let chunk = physmem::CHUNK_SIZE as u64;
        let _ = physmem::configure(16 * chunk);
        relieve_pressure();
        physmem::set_total_cap(Some(chunks * chunk));
        chunks * chunk / extent_frame::PART
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
        physmem::set_total_cap(None);
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
    }
}
