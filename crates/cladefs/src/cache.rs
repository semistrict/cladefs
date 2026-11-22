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

/// The process's cache of sealed extents (see the module docs): its pager's threads, which read
/// their pages from the cells, and the extents, by arena and ID.
struct ExtentCache {
    /// The threads the pager reads extents' parts on: under Shuttle, none, as the test's run's
    /// tasks are (`spawn_page_in`).
    #[cfg_attr(feature = "shuttle", allow(dead_code))]
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

/// Gives, once, the relief of arenas under pressure (TASK-50.3) its evictor, which frees parts of
/// cached extents (`slabs::relieve_with`): before a cell of any arena opens, whose allocations
/// may wait for it.
pub(crate) fn relieve_pressure() {
    node_local::node_local! {
        static STARTED: std::sync::Once = std::sync::Once::new();
    }
    STARTED.get().call_once(|| {
        let relieved =
            slabs::relieve_with(|arena, bytes| evict_least_used(arena, |evicted| evicted >= bytes));
        // Without its thread, an allocation that finds its arena out of memory fails. cladefs:
        // under madsim, relief is the process's, which another simulated host started already,
        // and starts no thread (doc-24, TASK-685.14).
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

/// A page-in found no memory of its arena to read into.
#[derive(Debug, thiserror::Error)]
#[error("no memory to read into")]
struct NoPages;

/// The errno a page-in that failed with `error` fails its readers with: `ENOENT` for an extent
/// the cell doesn't hold, `ENOMEM` for want of memory, `EIO` for the rest.
fn read_errno(error: &anyhow::Error) -> Errno {
    if error.downcast_ref::<NoExtent>().is_some() {
        Errno::ENOENT
    } else if error.downcast_ref::<NoPages>().is_some() {
        Errno::ENOMEM
    } else {
        Errno::EIO
    }
}

/// Runs `page_in` on the pager's threads: under Shuttle, as a task of the test's run, which its
/// scheduler interleaves with the test's threads.
fn spawn_page_in(page_in: impl std::future::Future<Output = ()> + Send + 'static) {
    #[cfg(feature = "shuttle")]
    drop(shuttle::future::spawn(page_in));
    #[cfg(not(feature = "shuttle"))]
    drop(extent_cache().pager.spawn(page_in));
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
            match read_local(&local, id, offset, buf) {
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
/// since it was mapped, which a follower's view keeps, reads as kept (doc-8). One `local` holds
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
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// A part of a cached extent.
enum PartSlot {
    /// Not in memory.
    Absent,
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
    /// The windows that map it, each once, by its identity ([`window_identity`]).
    windows: HashMap<usize, Weak<Window>>,
    /// The part after the last a page-in asked for: where a reader reading on asks next.
    next: usize,
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
    arena: ArenaId,
    extent: ExtentId,
    contents: Box<dyn Contents>,
    len: u64,
    parts: SharedLock<Mutex<Parts>>,
    /// How many of its parts are in memory, which relief asks of every extent cached (TASK-92.33).
    present: AtomicUsize,
    /// When it was last asked for or faulted on, by [`CACHE_CLOCK`]: those longest ago are evicted
    /// first (TASK-50.3, TASK-92.32).
    used: AtomicU64,
    /// Notified as parts come in, fail to, or go.
    changed: event_listener::Event,
}

impl CachedExtent {
    /// `extent`, of `len` bytes, read from `contents` into slabs of `arena`.
    pub(crate) fn new(
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
        let parts = Parts { slots, windows: HashMap::new(), next: 0 };
        Self {
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
        let parts = self.parts.with(Mutex::lock);
        let mapped = parts.windows.values().any(|window| window.strong_count() > 0);
        Some((mapped, self.used.load(Ordering::Relaxed)))
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
                let wanted = matches!(slot, PartSlot::Absent | PartSlot::Failed(_));
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
            spawn_page_in(self.clone().read_part(part, span, None));
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
        self.evict_pages(0..u64::MAX)
    }

    /// Evicts its parts in memory holding its pages `pages`, as [`CachedExtent::evict`] evicts
    /// them all, each its own (spec rule ownTokens): their slabs moved out of their slots into
    /// evictions of this call's, tokens left in their place; unmapped from each window on its
    /// window set; their slots then absent, and their slabs freed, by this call alone. A part
    /// another eviction is evicting is that one's: this one leaves it.
    fn evict_pages(&self, pages: Range<u64>) -> u64 {
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
            let mut parts = self.parts.with(Mutex::lock);
            for eviction in &evictions {
                let slot = &mut parts.slots[eviction.part];
                assert!(
                    matches!(slot, PartSlot::Evicting(token) if *token == eviction.token),
                    "part {} of {:?} changed under its eviction",
                    eviction.part,
                    self.extent
                );
                *slot = PartSlot::Absent;
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
        let slot = &mut table.slots[part];
        assert!(matches!(slot, PartSlot::Filling), "part {part} of {:?} not filling", self.extent);
        match read {
            Ok(slab) => {
                *slot = PartSlot::Present(slab);
                self.present.fetch_add(1, Ordering::Relaxed);
            }
            Err(error) => {
                // What the read failed with, not its errno alone, which said nothing of why
                // (TASK-217).
                let errno = read_errno(&error);
                let at = self.part_pages(part).start * page_size();
                log::error!("cladefs: reading {:?} at {at:#x}: {errno:?}: {error:#}", self.extent);
                *slot = PartSlot::Failed(errno);
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
        let slab = slabs::alloc(self.arena, pages.end - pages.start, slabs::Wait::Yes)
            .map_err(|_| anyhow::Error::new(NoPages))?;
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
    fn each_present(&self, pages: Range<u64>, found: &mut dyn FnMut(Present<'_>)) {
        let (ps, table) = (page_size(), self.parts.with(Mutex::lock));
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
                    let slot = (page < self.pages()).then(|| &table.slots[self.part_of(page)]);
                    let done = match slot {
                        None => Some(Err(Errno::ENOENT)),
                        Some(PartSlot::Present(_)) => Some(Ok(waited)),
                        Some(PartSlot::Failed(errno)) if waited => Some(Err(*errno)),
                        Some(PartSlot::Filling | PartSlot::Evicting(_)) => None,
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
                    spawn_page_in(crate::request::within(crate::request::current(), read));
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

/// Zeros, for windows to map ([`Zeros`]): made once, and kept for the process's life. `ENOMEM` if
/// the first can't be made, which each window has made as it is created (`Dax::new`): a free maps
/// them holding the cell's locks, memory out, as memory run out finds them.
pub(crate) fn zeros() -> Result<Arc<dyn Source>> {
    static ZEROS: OnceLock<Arc<Zeros>> = OnceLock::new();
    if let Some(zeros) = ZEROS.get() {
        return Ok(zeros.clone());
    }
    let page = slabs::alloc(ArenaId::DEFAULT, 1, slabs::Wait::Yes).map_err(|_| Errno::ENOMEM)?;
    page.write(0, &vec![0; page_size() as usize]).map_err(|err| {
        log::error!("cladefs: writing the zeros windows map: {err}");
        Errno::ENOMEM
    })?;
    Ok(ZEROS.get_or_init(|| Arc::new(Zeros(page))).clone())
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

        fn unmap(&self, offset: u64, len: u64) {
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
