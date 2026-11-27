// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The daemon's memory, which its cached extents' pages are of (TASK-50.3): the memory a tenant
//! holds, its arena's, and every tenant's together, each limited at runtime through the control
//! API. Past a limit the daemon evicts the clean pages asked for longest ago, the tenant's own
//! past the tenant's, any tenant's past the total; with none left to evict, what would take more
//! fails with `ENOMEM` (the slabs' relief, with `crate::cache`'s evictor), and the daemon
//! goes on. cladefs: but for a page-in past the total, as the host's memory is full, which waits
//! for a page rather than fail its guest's fault, which killed its VM (`slabs::room_may_come`,
//! board 5662); the governor keeps a floor and refuses cells meanwhile (`super::governor`).
//! cladefs: the private pages of a window that copies on write are its VM process's
//! memory, not the daemon's, and count against none of this (backlog doc-13, TASK-501). Its
//! metrics name tenants by their IDs.

use crate::Daemon;
use crate::quota::QuotaRegistry;
use crate::server::{Parking, Tenant};
use cladefs_core::Errno;
use cladefs_core::slabs::SLAB;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Per-daemon memory governance, tenant registry, held-parts cache, and in-flight byte budgets,
/// owned by [`Daemon`].
pub(super) struct State {
    /// The tenants alive, and their limits, under one lock, so that a tenant made as its limit is
    /// set is limited as it was set.
    registry: Mutex<Registry>,
    /// cladefs: what the governor allows every tenant's memory together, as the host's free memory
    /// goes ([`super::governor`], TASK-358): unlimited for a daemon not governed.
    governed: AtomicU64,
    /// cladefs: the parts of segments the daemon's cells pack as they fill, held until their
    /// framings (`local_cell::HeldParts`, TASK-400).
    held_parts: Arc<local_cell::HeldParts>,
    /// cladefs: the bytes in flight of every tenant whose own aren't set ([`set_in_flight`]).
    tenant_in_flight: Arc<AtomicU64>,
    /// cladefs: what every tenant's connections have the daemon hold together (board 8465).
    total_in_flight: Arc<InFlight>,
    /// Tracks a zero-yield `IdleOnly` eviction pass (`(recheck_after, idle_change_seq, bytes)`) so
    /// sustained `IdleOnly` pressure does not rescan every cached extent every 100 ms when no
    /// idle extents remain.
    idle_empty: Mutex<Option<(std::time::Instant, u64, u64)>>,
}

impl State {
    pub(super) fn new() -> Self {
        Self {
            registry: Mutex::new(Registry {
                quotas: QuotaRegistry::new(DEFAULT_WEIGHT),
                in_flight: None,
                total: None,
            }),
            governed: AtomicU64::new(u64::MAX),
            held_parts: local_cell::HeldParts::new(),
            tenant_in_flight: Arc::new(AtomicU64::new(DEFAULT_TENANT_IN_FLIGHT)),
            total_in_flight: Arc::new(InFlight::made(
                DefaultBudget::Fixed(DEFAULT_TOTAL_IN_FLIGHT),
                None,
            )),
            idle_empty: Mutex::new(None),
        }
    }
}

/// cladefs: a tenant's weight unless it is set: each tenant alike.
pub const DEFAULT_WEIGHT: u64 = 1;

struct Registry {
    /// Live tenants, their byte limits, and their weights (`super::quota`, board 11703).
    quotas: QuotaRegistry<Tenant>,
    /// cladefs: the tenants' bytes in flight ([`InFlight`]), by ID, as their limits are.
    in_flight: Option<HashMap<u64, u64>>,
    /// cladefs: the limit on every tenant's memory together the control API set, if it did, which
    /// the governor's allowance lowers further ([`govern`]).
    total: Option<u64>,
}

impl Registry {
    fn alive(&self) -> impl Iterator<Item = Arc<Tenant>> + '_ {
        self.quotas.alive()
    }

    fn limit(&self, tenant: u64) -> Option<u64> {
        self.quotas.limit(tenant)
    }

    fn weight(&self, tenant: u64) -> u64 {
        self.quotas.weight(tenant)
    }

    /// Limits every tenant's memory together to the least of the control API's limit and the
    /// governor's allowance.
    fn apply_total(&self, host: &cladefs_core::Host, governed: u64) {
        let cap = self.total.unwrap_or(u64::MAX).min(governed);
        host.slabs().set_total_cap((cap != u64::MAX).then_some(cap));
        // cladefs: whose the cap is: the governor's, the host's memory, which page-ins finding no
        // page wait out, or the control API's, which they fail past (board 5662).
        host.cache().set_total_is_the_hosts(
            governed != u64::MAX && governed <= self.total.unwrap_or(u64::MAX),
        );
    }
}

/// The tenant of the ID `id` on `daemon`, whose cells are in `dirs` (canonical, sorted), alive,
/// if one is: one revoked whose VM processes live on, which a grant of it again goes on with
/// (TASK-79.36).
pub(super) fn alive_on(
    daemon: &Daemon,
    id: u64,
    dirs: &[std::path::PathBuf],
) -> Option<Arc<Tenant>> {
    let registry = daemon.memory.registry.lock().unwrap();
    registry.alive().find(|tenant| tenant.id() == id && tenant.dirs() == Some(dirs))
}

/// cladefs: the tenant of the ID `id` alive on `daemon`, granted or still serving VM processes, if
/// there is one.
#[cfg(not(madsim))]
pub(super) fn tenant_on(daemon: &Daemon, id: u64) -> Option<Arc<Tenant>> {
    let registry = daemon.memory.registry.lock().unwrap();
    registry.alive().find(|tenant| tenant.id() == id)
}

/// Counts `tenant`, new, among those alive on `daemon`, limited as its ID is.
pub(super) fn register_on(daemon: &Daemon, tenant: &Arc<Tenant>) {
    let mut registry = daemon.memory.registry.lock().unwrap();
    if let Some(bytes) = registry.limit(tenant.id()) {
        daemon.host.slabs().set_arena_cap(tenant.arena(), Some(bytes));
    }
    let budget = registry.in_flight.as_ref().and_then(|set| set.get(&tenant.id()).copied());
    tenant.in_flight().set_budget(budget);
    registry.quotas.register(tenant);
}

/// Limits the memory of the tenant `tenant` on `daemon` to `bytes`, rounded down to pages, or
/// every tenant's together, without one; or lifts the limit, without `bytes`.
pub(super) fn set_limit_on(
    daemon: &Daemon,
    tenant: Option<u64>,
    bytes: Option<u64>,
) -> Result<(), Errno> {
    if bytes.is_some_and(|bytes| bytes < SLAB) {
        return Err(Errno::EINVAL);
    }
    let Some(id) = tenant else {
        let mut registry = daemon.memory.registry.lock().unwrap();
        registry.total = bytes;
        registry.apply_total(&daemon.host, daemon.memory.governed.load(Ordering::Relaxed));
        return Ok(());
    };
    crate::grants::check_tenant(id)?;
    let mut registry = daemon.memory.registry.lock().unwrap();
    registry.quotas.set_limit(id, bytes);
    for tenant in registry.alive().filter(|tenant| tenant.id() == id) {
        daemon.host.slabs().set_arena_cap(tenant.arena(), bytes);
    }
    Ok(())
}

/// Limits the memory of `tenant` on the process's default daemon.
#[cfg(not(madsim))]
pub fn set_limit(tenant: Option<u64>, bytes: Option<u64>) -> Result<(), Errno> {
    set_limit_on(Daemon::process(), tenant, bytes)
}

/// cladefs: weighs the tenant `tenant`'s memory on `daemon`, alive now or granted later, against
/// the others' as the host's memory runs low (TASK-358).
pub(super) fn set_weight_on(daemon: &Daemon, tenant: u64, weight: u64) -> Result<(), Errno> {
    crate::grants::check_tenant(tenant)?;
    let mut registry = daemon.memory.registry.lock().unwrap();
    registry.quotas.set_weight(tenant, weight);
    Ok(())
}

/// Weighs the tenant `tenant`'s memory on the process's default daemon.
#[cfg(not(madsim))]
pub fn set_weight(tenant: u64, weight: u64) -> Result<(), Errno> {
    set_weight_on(Daemon::process(), tenant, weight)
}

/// cladefs: the parts of segments `daemon`'s cells pack as they fill, held until their framings,
/// which they share (`local_cell::HeldParts`, TASK-400).
pub(super) fn held_parts_on(daemon: &Daemon) -> &Arc<local_cell::HeldParts> {
    &daemon.memory.held_parts
}

/// cladefs: the parts held on the process's default daemon.
#[cfg(not(madsim))]
pub fn held_parts() -> &'static Arc<local_cell::HeldParts> {
    &Daemon::process().memory.held_parts
}

/// cladefs: the memory `daemon`'s governor governs ([`govern_on`]), in bytes.
pub(super) fn governed_bytes_on(daemon: &Daemon) -> u64 {
    let fetch_mem = cladefs_core::bucket::memory_on(&daemon.host);
    daemon.host.slabs().arenas_bytes()
        + held_parts_on(daemon).bytes()
        + local_cell::replicate::hot_bytes_on(daemon.host.local())
        + fetch_mem.recent_bytes()
        + fetch_mem.tails_bytes()
        + daemon.host.local().pages().bytes()
        + local_cell::framing_buffers_bytes()
        + cladefs_core::host_cell::decoded_kept_bytes_on(&daemon.host)
        + crate::mount::queued_bytes_on(daemon)
}

/// cladefs: the memory the process's default daemon's governor governs.
#[cfg(not(madsim))]
pub fn governed_bytes() -> u64 {
    governed_bytes_on(Daemon::process())
}

/// cladefs: `daemon`'s governor's look at the host's memory (TASK-358), reclaiming all eligible
/// memory down to `allowance`.
#[cfg(not(madsim))]
#[cfg_attr(not(test), allow(dead_code))]
pub(super) async fn govern_on(daemon: &Arc<Daemon>, allowance: u64) {
    govern_with_on(
        daemon,
        allowance,
        crate::governor::ReclaimMode::Mapped { max_shrink_bytes: u64::MAX },
    )
    .await
}

/// Trims all derived (non-slab) caches on `daemon` to fit within `allowance`.
fn trim_derived_caches(daemon: &Daemon, allowance: u64) {
    daemon.memory.governed.store(allowance, Ordering::Relaxed);
    // cladefs: every tenant's bytes in flight within a share of what it allows (board 8465).
    total_in_flight_on(daemon).govern(allowance / GOVERNED_IN_FLIGHT_SHARE);
    let parts = held_parts_on(daemon);
    let room = allowance.saturating_sub(daemon.host.slabs().arenas_bytes());
    if parts.bytes() > room {
        parts.evict_to(room);
    }
    // cladefs: hot replication buffers (board 6902): persisted chunks are derived from the cell's
    // local files (`LogSource`), so evict and size them to the remaining allowance before caches.
    let room = room.saturating_sub(parts.bytes());
    local_cell::replicate::size_hot_to_on(daemon.host.local(), room);
    if local_cell::replicate::hot_bytes_on(daemon.host.local()) > room {
        local_cell::replicate::evict_hot_to_on(daemon.host.local(), room);
    }
    // cladefs: unified recent/unpacked parts (local framed + bucket fetch) and object tails
    // (TASK-603, board 6026). While FUSE cells are attached (`OpenCellDevice::read` bypasses
    // `ExtentCache`, board 8260), `PartMemory` is the only unpacked-part cache: scale it to half
    // the remaining room (at least `RECENT.min(room)`) so multi-file reads across hundreds of
    // files don't thrash a 64 MiB LRU while leaving room for `tails`, `pages`, and the kernel's
    // FUSE page cache (board 12313). Without FUSE cells, `ExtentCache` holds unpacked extents for
    // VMs, so keep `PartMemory` bounded to `RECENT.min(room)`.
    let fetch_mem = cladefs_core::bucket::memory_on(&daemon.host);
    let room = room.saturating_sub(local_cell::replicate::hot_bytes_on(daemon.host.local()));
    let recent_limit = recent_limit_for(room, crate::mount::has_attached_on(daemon));
    fetch_mem.size_recent_to(recent_limit);
    let room = room.saturating_sub(fetch_mem.recent_bytes());
    if fetch_mem.tails_bytes() > room {
        fetch_mem.evict_tails_to(room);
    }
    let room = room.saturating_sub(fetch_mem.tails_bytes());
    let pages = daemon.host.local().pages();
    // Sized to what it may hold, as Linux's page cache to the memory free (TASK-433.8, board 6001).
    pages.size_to(room);
    if pages.bytes() > room {
        pages.evict_to(room);
    }
    let room = room.saturating_sub(pages.bytes());
    if local_cell::framing_buffers_bytes() > room {
        local_cell::evict_framing_buffers_to(room);
    }
    let room = room.saturating_sub(local_cell::framing_buffers_bytes());
    if cladefs_core::host_cell::decoded_kept_bytes_on(&daemon.host) > room {
        cladefs_core::host_cell::evict_decoded_on(&daemon.host, room);
    }
}

fn weighted_tenants(daemon: &Daemon) -> Vec<(u64, u64)> {
    let registry = daemon.memory.registry.lock().unwrap();
    let mut ids: Vec<u64> = registry.alive().map(|tenant| tenant.id()).collect();
    ids.sort();
    ids.dedup();
    ids.into_iter().map(|id| (id, registry.weight(id))).collect()
}

fn effective_cap_for(daemon: &Daemon, allowance: u64, mode: crate::governor::ReclaimMode) -> u64 {
    use crate::governor::ReclaimMode;
    match mode {
        ReclaimMode::None => allowance,
        ReclaimMode::IdleOnly => {
            // Stop growing past the mapped working set that remains, without setting `total_cap`
            // below resident mapped bytes (which would make eviction synchronously evict hot
            // mapped extents on the next page-in).
            let remaining = daemon.host.slabs().arenas_bytes();
            remaining.div_ceil(SLAB).saturating_mul(SLAB).max(allowance)
        }
        ReclaimMode::Mapped { max_shrink_bytes } => {
            let current = daemon.host.slabs().arenas_bytes();
            if current <= allowance {
                allowance
            } else {
                current.saturating_sub(max_shrink_bytes).max(allowance)
            }
        }
    }
}

fn skip_idle_scan(
    daemon: &Daemon,
    mode: crate::governor::ReclaimMode,
    now: std::time::Instant,
    seq_before: u64,
    before_idle: u64,
) -> bool {
    matches!(mode, crate::governor::ReclaimMode::IdleOnly)
        && daemon.memory.idle_empty.lock().unwrap().is_some_and(|(until, seq, bytes)| {
            now < until && seq == seq_before && bytes == before_idle
        })
}

fn record_idle_scan(
    daemon: &Daemon,
    now: std::time::Instant,
    seq_before: u64,
    before_idle: u64,
    allowance: u64,
) {
    let after_idle = daemon.host.slabs().arenas_bytes();
    let mut slot = daemon.memory.idle_empty.lock().unwrap();
    if after_idle >= before_idle && after_idle > allowance {
        *slot = Some((now + std::time::Duration::from_secs(2), seq_before, after_idle));
    } else {
        *slot = None;
    }
}

fn apply_governed_cap(daemon: &Daemon, effective_cap: u64) {
    let registry = daemon.memory.registry.lock().unwrap();
    daemon.memory.governed.store(effective_cap, Ordering::Relaxed);
    registry.apply_total(&daemon.host, effective_cap);
}

/// cladefs: `daemon`'s governor's look at the host's memory under `mode`: always trims derived
/// caches, spare slabs, and idle (unmapped) DAX extents first; shrinks actively-mapped DAX extents
/// only under [`crate::governor::ReclaimMode::Mapped`] (rate-limited by `max_shrink_bytes`), while
/// [`crate::governor::ReclaimMode::IdleOnly`] caps growth at the remaining mapped working set
/// without triggering synchronous relief eviction of hot mapped extents.
///
/// Outside `madsim` and Shuttle, delegates directly to [`govern_with_sync_on_wait`] on a blocking
/// thread so tests and production execute the exact same synchronous eviction and `Parking::with_wake`
/// quarantine-wait path (hotbed review 139 finding 2).
#[cfg(not(madsim))]
pub(super) async fn govern_with_on(
    daemon: &Arc<Daemon>,
    allowance: u64,
    mode: crate::governor::ReclaimMode,
) {
    if tokio::runtime::Handle::try_current().is_ok() {
        let daemon = Arc::clone(daemon);
        // Run the production sync eviction path outside the simulation on a blocking thread so
        // async callers do not block a Tokio worker.
        tokio::task::spawn_blocking(move || {
            govern_with_sync_on_wait(&daemon, allowance, mode, &mut SyncWait::Standalone);
        })
        .await
        .expect("govern_with_sync_on blocking task panicked");
    } else {
        // Called from a non-Tokio executor (e.g. `futures::executor::block_on` in `cell-bench`).
        govern_with_sync_on_wait(daemon, allowance, mode, &mut SyncWait::Standalone);
    }
}

/// Async version of [`govern_with_on`] used under `madsim` where no OS eviction thread runs and
/// quarantine waits must yield cooperatively to the runtime.
#[cfg(madsim)]
pub(super) async fn govern_with_on(
    daemon: &Arc<Daemon>,
    allowance: u64,
    mode: crate::governor::ReclaimMode,
) {
    use crate::governor::ReclaimMode;
    use cladefs_core::cache::EvictScope;

    // cladefs: its shrinks wait for what they evict this long at most, together (board 8535).
    let deadline = std::time::Instant::now() + PARKED_WAIT;
    trim_derived_caches(daemon, allowance);

    if matches!(mode, ReclaimMode::None) || daemon.host.slabs().arenas_bytes() <= allowance {
        let registry = daemon.memory.registry.lock().unwrap();
        registry.apply_total(&daemon.host, allowance);
        return;
    }

    let weighted = weighted_tenants(daemon);

    daemon.host.slabs().release_spares(None);
    let before_idle = daemon.host.slabs().arenas_bytes();
    if before_idle > allowance {
        let now = std::time::Instant::now();
        let seq_before = daemon.host.cache().idle_change_seq();
        if !skip_idle_scan(daemon, mode, now, seq_before, before_idle) {
            shrink_weighted(daemon, &weighted, allowance, EvictScope::IdleOnly, deadline).await;
            record_idle_scan(daemon, now, seq_before, before_idle, allowance);
        }
    }

    let effective_cap = effective_cap_for(daemon, allowance, mode);
    apply_governed_cap(daemon, effective_cap);

    if let ReclaimMode::Mapped { .. } = mode
        && daemon.host.slabs().arenas_bytes() > effective_cap
    {
        shrink_weighted(daemon, &weighted, effective_cap, EvictScope::GovernorMapped, deadline)
            .await;
    }
}

/// How [`evict_until_sync`] wakes and waits on quarantined VM unmaps.
#[cfg(not(madsim))]
enum SyncWait<'a> {
    /// Running on the unified `"cladefs-eviction"` OS thread: shares `slabs`'s wakeup signal with
    /// starved-arena relief and services relief between every step and during quarantine waits.
    EvictionThread(&'a mut dyn FnMut()),
    /// Running off the eviction thread (e.g. `memory::govern` tests): uses a local condvar via
    /// [`Parking::with_wake`] so it exercises the exact production sync path without consuming the
    /// eviction thread's `wanted` signal.
    Standalone,
}

#[cfg(not(madsim))]
impl SyncWait<'_> {
    fn service_relief(&mut self) {
        match self {
            Self::EvictionThread(service) => service(),
            Self::Standalone => {}
        }
    }
}

/// Synchronous equivalent of [`govern_with_on`] executed on the single unified OS eviction thread
/// (board 11703), servicing starved arena relief via `service_relief` between steps and while
/// waiting on quarantined VM unmaps.
#[cfg(not(madsim))]
pub(super) fn govern_with_sync_on(
    daemon: &Daemon,
    allowance: u64,
    mode: crate::governor::ReclaimMode,
    service_relief: &mut dyn FnMut(),
) {
    govern_with_sync_on_wait(
        daemon,
        allowance,
        mode,
        &mut SyncWait::EvictionThread(service_relief),
    );
}

#[cfg(not(madsim))]
fn govern_with_sync_on_wait(
    daemon: &Daemon,
    allowance: u64,
    mode: crate::governor::ReclaimMode,
    wait: &mut SyncWait<'_>,
) {
    use crate::governor::ReclaimMode;
    use cladefs_core::cache::EvictScope;

    let deadline = std::time::Instant::now() + PARKED_WAIT;
    trim_derived_caches(daemon, allowance);
    wait.service_relief();

    if matches!(mode, ReclaimMode::None) || daemon.host.slabs().arenas_bytes() <= allowance {
        let registry = daemon.memory.registry.lock().unwrap();
        registry.apply_total(&daemon.host, allowance);
        return;
    }

    let weighted = weighted_tenants(daemon);

    daemon.host.slabs().release_spares(None);
    let before_idle = daemon.host.slabs().arenas_bytes();
    if before_idle > allowance {
        let now = std::time::Instant::now();
        let seq_before = daemon.host.cache().idle_change_seq();
        if !skip_idle_scan(daemon, mode, now, seq_before, before_idle) {
            shrink_weighted_sync(
                daemon,
                &weighted,
                allowance,
                EvictScope::IdleOnly,
                deadline,
                wait,
            );
            record_idle_scan(daemon, now, seq_before, before_idle, allowance);
        }
    }

    let effective_cap = effective_cap_for(daemon, allowance, mode);
    apply_governed_cap(daemon, effective_cap);

    if let ReclaimMode::Mapped { .. } = mode
        && daemon.host.slabs().arenas_bytes() > effective_cap
    {
        shrink_weighted_sync(
            daemon,
            &weighted,
            effective_cap,
            EvictScope::GovernorMapped,
            deadline,
            wait,
        );
    }
}

/// Tenants currently over their weighted share of `target`, ordered most over-share first with a
/// deterministic tenant-ID tie-break.
fn weighted_over_share(daemon: &Daemon, weighted: &[(u64, u64)], target: u64) -> Vec<(u64, u64)> {
    let mut over: Vec<(u64, u64, u64)> = crate::governor::shares(target, weighted)
        .into_iter()
        .filter_map(|(id, share)| {
            let held = held_by(daemon, id);
            held.checked_sub(share).filter(|&over| over > 0).map(|over| (over, id, share))
        })
        .collect();
    // Most over-share tenant yields first; tie-break deterministically by tenant ID.
    over.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    over.into_iter().map(|(_, id, share)| (id, share)).collect()
}

/// Computes the capped shrink target for `id` (whose weighted share is `share`) given the remaining
/// global excess above `target`, returning `None` if `id` needs no shrink or global `target` is met.
fn tenant_shrink_target(daemon: &Daemon, id: u64, share: u64, target: u64) -> Option<u64> {
    let excess = daemon.host.slabs().arenas_bytes().saturating_sub(target);
    if excess == 0 {
        return None;
    }
    let held = held_by(daemon, id);
    (held > share).then(|| share.max(held.saturating_sub(excess)))
}

#[cfg(not(madsim))]
fn shrink_weighted_sync(
    daemon: &Daemon,
    weighted: &[(u64, u64)],
    target: u64,
    scope: cladefs_core::cache::EvictScope,
    deadline: std::time::Instant,
    wait: &mut SyncWait<'_>,
) {
    for (id, share) in weighted_over_share(daemon, weighted, target) {
        if daemon.host.slabs().arenas_bytes() <= target {
            break;
        }
        if let Some(tenant_target) = tenant_shrink_target(daemon, id, share, target)
            && let Err(err) =
                shrink_scoped_by_sync(daemon, Some(id), tenant_target, scope, deadline, wait)
            && err != Errno::ENOENT
        {
            log::error!("cladefsd: shrinking a tenant's memory to its share: {err}");
        }
        wait.service_relief();
    }
    if daemon.host.slabs().arenas_bytes() > target
        && let Err(err) = shrink_scoped_by_sync(daemon, None, target, scope, deadline, wait)
    {
        log::error!("cladefsd: shrinking the daemon's memory to the host's: {err}");
    }
}

#[cfg(not(madsim))]
fn shrink_scoped_by_sync(
    daemon: &Daemon,
    tenant: Option<u64>,
    target: u64,
    scope: cladefs_core::cache::EvictScope,
    deadline: std::time::Instant,
    wait: &mut SyncWait<'_>,
) -> Result<u64, Errno> {
    let slabs = daemon.host.slabs();
    let Some(id) = tenant else {
        evict_until_sync(
            daemon,
            None,
            scope,
            || slabs.arenas_bytes_in_use().saturating_sub(target),
            deadline,
            wait,
        );
        slabs.release_spares(None);
        return Ok(slabs.arenas_bytes());
    };
    let arenas: Vec<_> = (daemon.memory.registry.lock().unwrap().alive())
        .filter(|tenant| tenant.id() == id)
        .map(|tenant| tenant.arena())
        .collect();
    if arenas.is_empty() {
        return Err(Errno::ENOENT);
    }
    let held = || arenas.iter().map(|arena| slabs.arena_bytes(*arena)).sum::<u64>();
    let in_use = || arenas.iter().map(|arena| slabs.arena_bytes_in_use(*arena)).sum::<u64>();
    for arena in &arenas {
        evict_until_sync(
            daemon,
            Some(*arena),
            scope,
            || in_use().saturating_sub(target),
            deadline,
            wait,
        );
        slabs.release_spares(Some(*arena));
    }
    Ok(held())
}

#[cfg(not(madsim))]
fn evict_until_sync(
    daemon: &Daemon,
    arena: Option<cladefs_core::slabs::ArenaId>,
    scope: cladefs_core::cache::EvictScope,
    deficit: impl Fn() -> u64,
    deadline: std::time::Instant,
    wait: &mut SyncWait<'_>,
) {
    let slabs = daemon.host.slabs();
    loop {
        let want = deficit().min(MAX_SHRINK_BATCH);
        if want == 0 {
            return;
        }
        let standalone = matches!(wait, SyncWait::Standalone)
            .then(|| Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new())));
        let parking = match wait {
            SyncWait::EvictionThread(_) => {
                let wake_slabs = Arc::clone(slabs);
                Parking::with_wake(Arc::new(move || wake_slabs.signal_eviction()))
            }
            SyncWait::Standalone => {
                let sig = Arc::clone(standalone.as_ref().unwrap());
                Parking::with_wake(Arc::new(move || {
                    *sig.0.lock().unwrap() = true;
                    sig.1.notify_all();
                }))
            }
        };
        let stop = |evicted: u64| evicted >= want || deficit() == 0;
        let evicted =
            parking.counting(daemon, || daemon.host.cache().evict_with_scope(arena, scope, stop));
        // Service starved-arena relief after every eviction batch (not only when quarantined) so
        // non-parked governor shrinks cannot stall DAX page-ins (hotbed review 139 finding 1).
        wait.service_relief();
        if !parking.parked() {
            if evicted == 0 || deficit() == 0 {
                return;
            }
            continue;
        }
        while parking.parked() {
            wait.service_relief();
            if !parking.parked() {
                break;
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            let slice = left.min(std::time::Duration::from_millis(50));
            match wait {
                SyncWait::EvictionThread(_) => slabs.wait_eviction(slice),
                SyncWait::Standalone => {
                    let sig = standalone.as_ref().unwrap();
                    let mut signaled = sig.0.lock().unwrap();
                    if !*signaled && parking.parked() {
                        let (guard, _) = sig.1.wait_timeout(signaled, slice).unwrap();
                        signaled = guard;
                    }
                    *signaled = false;
                }
            }
        }
    }
}

/// Evicts each tenant down to its weighted share of `target`, then any arena down to `target`,
/// restricted to `scope`. Caps each tenant's eviction by the remaining global excess above
/// `target` so multi-tenant weighted passes still honor `Mapped { max_shrink_bytes }` rate limits.
#[cfg(madsim)]
async fn shrink_weighted(
    daemon: &Daemon,
    weighted: &[(u64, u64)],
    target: u64,
    scope: cladefs_core::cache::EvictScope,
    deadline: std::time::Instant,
) {
    for (id, share) in weighted_over_share(daemon, weighted, target) {
        if daemon.host.slabs().arenas_bytes() <= target {
            break;
        }
        if let Some(tenant_target) = tenant_shrink_target(daemon, id, share, target)
            && let Err(err) =
                shrink_scoped_by(daemon, Some(id), tenant_target, scope, deadline).await
            && err != Errno::ENOENT
        {
            log::error!("cladefsd: shrinking a tenant's memory to its share: {err}");
        }
    }
    if daemon.host.slabs().arenas_bytes() > target
        && let Err(err) = shrink_scoped_by(daemon, None, target, scope, deadline).await
    {
        log::error!("cladefsd: shrinking the daemon's memory to the host's: {err}");
    }
}

/// cladefs: target byte limit for `PartMemory` (`recent`) given the remaining governor `room` and
/// whether any FUSE cells are attached (board 12313). Never exceeds `room`.
pub(super) fn recent_limit_for(room: u64, fuse_attached: bool) -> u64 {
    let base = bucket_cell::fetch::RECENT.min(room);
    if fuse_attached { (room / 2).max(base) } else { base }
}

/// The process's default daemon's governor's look at the host's memory.
#[cfg(not(madsim))]
pub async fn govern(allowance: u64) {
    govern_on(Daemon::process(), allowance).await
}

/// cladefs: how long a shrink waits at most for the pages it evicted that are quarantined, a
/// governor's look's shrinks together (board 8535): twice [`super::server::UNRESPONSIVE`], by when
/// a VM process that stopped answering was let go, found within a look of the watchdog's, and
/// killed has exited, its pages back. Past it, what is quarantined still is held still, which the
/// shrink evicts other extents' pages for, rather than wait: one VM process that never exits, in
/// uninterruptible sleep or a frozen cgroup, ended every later look of the governor, every tenant's
/// memory ungoverned until the daemon restarted (hotbed question 484).
pub(super) const PARKED_WAIT: std::time::Duration = crate::server::UNRESPONSIVE.saturating_mul(2);

/// What the tenants alive of the ID `id` on `daemon` hold.
fn held_by(daemon: &Daemon, id: u64) -> u64 {
    let registry = daemon.memory.registry.lock().unwrap();
    registry
        .alive()
        .filter(|tenant| tenant.id() == id)
        .map(|tenant| daemon.host.slabs().arena_bytes(tenant.arena()))
        .sum()
}

/// Evicts clean pages on `daemon` until `tenant` (or the daemon) holds at most `target` bytes.
pub(super) async fn shrink_on(
    daemon: &Daemon,
    tenant: Option<u64>,
    target: u64,
) -> Result<u64, Errno> {
    shrink_by(daemon, tenant, target, std::time::Instant::now() + PARKED_WAIT).await
}

/// Evicts clean pages on the process's default daemon.
#[cfg(not(madsim))]
pub async fn shrink(tenant: Option<u64>, target: u64) -> Result<u64, Errno> {
    shrink_on(Daemon::process(), tenant, target).await
}

/// cladefs: [`shrink_on`], waiting for the pages it evicts until `deadline` at most (board 8535).
async fn shrink_by(
    daemon: &Daemon,
    tenant: Option<u64>,
    target: u64,
    deadline: std::time::Instant,
) -> Result<u64, Errno> {
    shrink_scoped_by(daemon, tenant, target, cladefs_core::cache::EvictScope::All, deadline).await
}

/// cladefs: [`shrink_by`] restricted to `scope` (`IdleOnly` or `All`).
async fn shrink_scoped_by(
    daemon: &Daemon,
    tenant: Option<u64>,
    target: u64,
    scope: cladefs_core::cache::EvictScope,
    deadline: std::time::Instant,
) -> Result<u64, Errno> {
    let slabs = daemon.host.slabs();
    let Some(id) = tenant else {
        evict_until(
            daemon,
            None,
            scope,
            || slabs.arenas_bytes_in_use().saturating_sub(target),
            deadline,
        )
        .await;
        // Then the spare slabs, which its evictions' frees may have kept.
        slabs.release_spares(None);
        return Ok(slabs.arenas_bytes());
    };
    let arenas: Vec<_> = (daemon.memory.registry.lock().unwrap().alive())
        .filter(|tenant| tenant.id() == id)
        .map(|tenant| tenant.arena())
        .collect();
    if arenas.is_empty() {
        return Err(Errno::ENOENT);
    }
    let held = || arenas.iter().map(|arena| slabs.arena_bytes(*arena)).sum::<u64>();
    let in_use = || arenas.iter().map(|arena| slabs.arena_bytes_in_use(*arena)).sum::<u64>();
    for arena in &arenas {
        evict_until(daemon, Some(*arena), scope, || in_use().saturating_sub(target), deadline)
            .await;
        slabs.release_spares(Some(*arena));
    }
    Ok(held())
}

/// Maximum bytes [`evict_until`] evicts in one batch before waiting for quarantined unmaps to
/// acknowledge (board 12315): avoids 1-extent-per-RTT stalls under large governor shrinks while
/// keeping unmap bursts bounded.
const MAX_SHRINK_BATCH: u64 = 64 * SLAB;

/// cladefs: evicts the pages of `arena`'s cached extents on `daemon` within `scope`, until
/// `deficit` reaches 0 (or nothing remains to evict).
async fn evict_until(
    daemon: &Daemon,
    arena: Option<cladefs_core::slabs::ArenaId>,
    scope: cladefs_core::cache::EvictScope,
    deficit: impl Fn() -> u64,
    deadline: std::time::Instant,
) {
    loop {
        let want = deficit().min(MAX_SHRINK_BATCH);
        if want == 0 {
            return;
        }
        let parking = Parking::new();
        let stop = |evicted: u64| evicted >= want || deficit() == 0;
        let evicted =
            parking.counting(daemon, || daemon.host.cache().evict_with_scope(arena, scope, stop));
        if !parking.parked() {
            if evicted == 0 || deficit() == 0 {
                return;
            }
            continue;
        }
        // Past it, at once: what is quarantined still, held, the next pass evicts others' for.
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        drop(tokio::time::timeout(left, parking.wait()).await);
    }
}

/// A tenant's memory, as the metrics show it.
pub struct TenantMemory {
    /// Its ID.
    pub tenant: u64,
    /// What its arena holds, in bytes.
    pub bytes: u64,
    /// Its limit, if it has one.
    pub limit: Option<u64>,
}

/// The memory of each tenant alive on `daemon`, by ID.
pub(super) fn tenants_on(daemon: &Daemon) -> Vec<TenantMemory> {
    let registry = daemon.memory.registry.lock().unwrap();
    let mut memory: HashMap<u64, TenantMemory> = HashMap::new();
    for tenant in registry.alive() {
        let entry = memory.entry(tenant.id()).or_insert_with(|| TenantMemory {
            tenant: tenant.id(),
            bytes: 0,
            limit: registry.limit(tenant.id()),
        });
        entry.bytes += daemon.host.slabs().arena_bytes(tenant.arena());
    }
    let mut memory: Vec<_> = memory.into_values().collect();
    memory.sort_by_key(|memory| memory.tenant);
    memory
}

/// The memory of each tenant alive on the process's default daemon.
#[cfg(not(madsim))]
pub fn tenants() -> Vec<TenantMemory> {
    tenants_on(Daemon::process())
}

/// cladefs: what every tenant's connections on `daemon` have the daemon hold of their VM
/// processes' requests now, together ([`InFlight`]).
pub(super) fn in_flight_held_on(daemon: &Daemon) -> u64 {
    total_in_flight_on(daemon).held()
}

/// cladefs: of `reserved` connections each tenant alive on `daemon` keeps, those its VM processes
/// don't have, every tenant's together (`server::Counted`, board 8465).
pub(super) fn reserved_connections_left_on(daemon: &Daemon, reserved: usize) -> usize {
    let registry = daemon.memory.registry.lock().unwrap();
    let default = crate::server::default_tenant_made(daemon);
    (registry.alive().chain(default))
        .map(|tenant| reserved.saturating_sub(tenant.connections()))
        .sum()
}

/// cladefs: the size from which the C library's allocator maps each allocation on its own, and
/// unmaps it as it is freed ([`return_large_allocations`], TASK-317).
pub const MMAP_THRESHOLD: usize = 1 << 20;

/// cladefs: mimalloc v3's `mi_option_t` indices used by [`return_large_allocations`] on musl
/// (from `libmimalloc-sys`'s bundled `v3/include/mimalloc.h`, which `libmimalloc-sys` does not
/// re-export as named constants). Verified by `tests::large_buffers_freed_go_back` on musl.
#[cfg(target_env = "musl")]
const MI_OPTION_PURGE_DELAY: libmimalloc_sys::mi_option_t = 15;
#[cfg(target_env = "musl")]
const MI_OPTION_ARENA_MAX_OBJECT_SIZE: libmimalloc_sys::mi_option_t = 45;

/// cladefs: has the process's allocator give back what the daemon frees of its large buffers at
/// once, each allocation of [`MMAP_THRESHOLD`] or more mapped on its own (glibc's
/// `M_MMAP_THRESHOLD`; on musl, mimalloc's `mi_option_arena_max_object_size` and
/// `mi_option_purge_delay`), before any thread but the caller's is made: the segments it frames
/// and uploads, 32 MiB each, the parts it fetches, of a MiB. By default glibc raises that
/// threshold to the largest allocation freed, up to 32 MiB, and keeps buffers below it in its
/// arenas, one a thread at most eight times the CPUs: under an upload backlog on the test host,
/// 134 of them, 64 MiB each, held 2.6 GB of anonymous memory the daemon had freed, twice its heap
/// at its peak (TASK-317), what the host's OOM killer took it for. On musl, mimalloc v3's arenas
/// likewise hold objects up to 2 GiB for 1000 ms before purging; setting the arena cap just below
/// [`MMAP_THRESHOLD`] routes large buffers straight to OS `mmap`/`munmap` on free, and lowering
/// `purge_delay` to 10 ms returns freed sub-threshold arena slices promptly.
pub fn return_large_allocations() {
    #[cfg(target_env = "gnu")]
    {
        // SAFETY: mallopt changes the allocator's settings, which every allocation after reads.
        let set = unsafe { libc::mallopt(libc::M_MMAP_THRESHOLD, MMAP_THRESHOLD as libc::c_int) };
        if set != 1 {
            log::warn!("cladefsd: glibc refused M_MMAP_THRESHOLD {MMAP_THRESHOLD}");
        }
    }
    #[cfg(target_env = "musl")]
    {
        // mimalloc's arena slice granularity is 64 KiB and `arena.c` checks
        // `slice_count <= arena_max_object_size / 64 KiB`, so 960 KiB sends >= 1 MiB allocations
        // directly to OS mmap/munmap.
        const ARENA_SLICE_KIB: usize = 64;
        const MAX_ARENA_OBJECT_KIB: std::ffi::c_long =
            ((MMAP_THRESHOLD >> 10) - ARENA_SLICE_KIB) as std::ffi::c_long;
        // SAFETY: Called from `main` before worker threads start (and idempotently in tests);
        // `mi_option_set` writes mimalloc's process-wide option table for subsequent allocations.
        unsafe {
            libmimalloc_sys::mi_option_set(MI_OPTION_ARENA_MAX_OBJECT_SIZE, MAX_ARENA_OBJECT_KIB);
            libmimalloc_sys::mi_option_set(MI_OPTION_PURGE_DELAY, 10);
        }
    }
}

/// cladefs: the bytes in flight a tenant's connections share, unless they are set for it
/// ([`set_in_flight`]): what of its VM processes' requests the daemon holds at once, their stream
/// data and what it has queued for them, together (TASK-149.35). Each connection has a credit of
/// its own too (`server::Peer::credit`, 272 MiB without a window), and a tenant as many as
/// `server::MAX_CONNECTIONS` connections: some 1.1 TiB of one tenant's, held within the protocol;
/// every tenant's together within [`total_in_flight`]'s (board 8465). A VM process moves the data of its requests through its transfer area, up to 256 KiB each, 8
/// at once, which no room counts; past that, through the stream. 8 GiB is 30 VM processes at once
/// keeping their rooms full, which none does but writing 64 MiB extents back to back, where a
/// host's density case, 300 forks reading their base, held 16 MiB of the tenant's at most.
pub const DEFAULT_TENANT_IN_FLIGHT: u64 = 8 << 30;

/// cladefs: the bytes in flight every tenant's connections share together, unless set otherwise
/// ([`set_total_in_flight`]), and the governor allows fewer (board 8465): two tenants' at their
/// default's most (hotbed question 482).
pub const DEFAULT_TOTAL_IN_FLIGHT: u64 = 2 * DEFAULT_TENANT_IN_FLIGHT;

/// cladefs: what of the memory the governor allows the daemon every tenant's bytes in flight may
/// hold together, in parts of it (board 8465): a quarter, the rest the cache's.
const GOVERNED_IN_FLIGHT_SHARE: u64 = 4;

/// cladefs: what every tenant's connections on `daemon` have the daemon hold together (board 8465),
/// which each tenant's holds within ([`InFlight::of_a_tenant`]).
pub(super) fn total_in_flight_on(daemon: &Daemon) -> &Arc<InFlight> {
    &daemon.memory.total_in_flight
}

/// cladefs: sets the bytes in flight every tenant's connections on `daemon` share together.
#[cfg(not(madsim))]
pub(super) fn set_total_in_flight_on(daemon: &Daemon, bytes: Option<u64>) -> Result<(), Errno> {
    if bytes.is_some_and(|bytes| bytes < crate::wire::MAX_PAYLOAD) {
        return Err(Errno::EINVAL);
    }
    total_in_flight_on(daemon).set_budget(bytes);
    Ok(())
}

/// cladefs: sets the bytes in flight on the process's default daemon.
#[cfg(not(madsim))]
pub fn set_total_in_flight(bytes: Option<u64>) -> Result<(), Errno> {
    set_total_in_flight_on(Daemon::process(), bytes)
}

/// cladefs: sets the bytes in flight the connections of the tenant `tenant` on `daemon` share.
pub(super) fn set_in_flight_on(
    daemon: &Arc<Daemon>,
    tenant: Option<u64>,
    bytes: Option<u64>,
) -> Result<(), Errno> {
    if bytes.is_some_and(|bytes| bytes < crate::wire::MAX_PAYLOAD) {
        return Err(Errno::EINVAL);
    }
    let Some(id) = tenant else {
        daemon
            .memory
            .tenant_in_flight
            .store(bytes.unwrap_or(DEFAULT_TENANT_IN_FLIGHT), Ordering::Relaxed);
        // Larger, it lets those waiting go on.
        let registry = daemon.memory.registry.lock().unwrap();
        registry.alive().for_each(|tenant| tenant.in_flight().changed());
        if let Ok(tenant) = crate::server::default_tenant_on(daemon) {
            tenant.in_flight().changed();
        }
        return Ok(());
    };
    crate::grants::check_tenant(id)?;
    let mut registry = daemon.memory.registry.lock().unwrap();
    let in_flight = registry.in_flight.get_or_insert_with(HashMap::new);
    match bytes {
        Some(bytes) => in_flight.insert(id, bytes),
        None => in_flight.remove(&id),
    };
    for tenant in registry.alive().filter(|tenant| tenant.id() == id) {
        tenant.in_flight().set_budget(bytes);
    }
    Ok(())
}

/// cladefs: sets the bytes in flight on the process's default daemon.
#[cfg(not(madsim))]
pub fn set_in_flight(tenant: Option<u64>, bytes: Option<u64>) -> Result<(), Errno> {
    set_in_flight_on(Daemon::process(), tenant, bytes)
}

pub use cladefs_wire::in_flight::{DefaultBudget, InFlight};

/// cladefs: a tenant's [`InFlight`] on `daemon`, within every tenant's together (board 8465).
pub(super) fn in_flight_of_a_tenant(daemon: &Daemon) -> InFlight {
    InFlight::made(
        DefaultBudget::Shared(Arc::clone(&daemon.memory.tenant_in_flight)),
        Some(Arc::clone(&daemon.memory.total_in_flight)),
    )
}

#[cfg(test)]
mod tests {
    /// The process's anonymous memory resident, in bytes, as /proc says.
    #[cfg(target_os = "linux")]
    fn anonymous() -> u64 {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let line = status.lines().find_map(|line| line.strip_prefix("RssAnon:")).unwrap();
        line.trim().trim_end_matches("kB").trim().parse::<u64>().unwrap() << 10
    }

    /// TASK-317: buffers of a segment's size, written and freed on many threads at once, as the
    /// daemon frames and uploads segments, leave no memory behind them once
    /// [`super::return_large_allocations`] has the allocator give them back: glibc's arenas kept
    /// 2.6 GB of them in the daemon otherwise, and mimalloc v3's arenas keep them for 1 s.
    #[cfg(any(target_env = "gnu", target_env = "musl"))]
    #[test]
    fn large_buffers_freed_go_back() {
        const THREADS: usize = 32;
        const BUFFERS: usize = 8;
        const SEGMENT: usize = 32 << 20;
        #[cfg(target_env = "musl")]
        {
            // Verify our mimalloc v3 option indices still match `libmimalloc-sys`'s defaults before
            // overriding them so a future mimalloc bump cannot silently shift an enum index.
            // SAFETY: Single-threaded before spawning worker threads; reads mimalloc's version and
            // option defaults.
            let (version, purge_delay, max_arena_kib) = unsafe {
                (
                    libmimalloc_sys::mi_version(),
                    libmimalloc_sys::mi_option_get(super::MI_OPTION_PURGE_DELAY),
                    libmimalloc_sys::mi_option_get(super::MI_OPTION_ARENA_MAX_OBJECT_SIZE),
                )
            };
            assert!(version >= 300, "expected mimalloc v3+, got {version}");
            assert_eq!(purge_delay, 1000, "mi_option_purge_delay index drifted");
            assert_eq!(max_arena_kib, 2 << 20, "mi_option_arena_max_object_size index drifted");
        }
        super::return_large_allocations();
        let before = anonymous();
        let threads: Vec<_> = (0..THREADS)
            .map(|thread| {
                std::thread::spawn(move || {
                    for buffer in 0..BUFFERS {
                        // A segment a little smaller than the most, as a framed one is often.
                        let mut data = vec![0u8; SEGMENT - buffer * 4096];
                        data.iter_mut().step_by(4096).for_each(|byte| *byte = thread as u8);
                        std::hint::black_box(&data);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let left = anonymous().saturating_sub(before);
        assert!(left < 16 << 20, "{} MiB of freed buffers resident", left >> 20);
    }

    /// Board 6902 (Codex review): `governed_bytes()` counts hot replication buffers (including
    /// take-time seeds) and `govern()` evicts persisted hot buffers under pressure.
    #[tokio::test(flavor = "multi_thread")]
    async fn govern_counts_and_reclaims_replication_hot_buffers() {
        use local_cell::replicate::{
            Flushed, LinkError, LogSource, MemberLink, Replicator, log_of,
        };
        use std::sync::Arc;

        struct Stalled;
        impl MemberLink for Stalled {
            fn append(
                &self,
                _: u64,
                _: u64,
                _: Vec<u8>,
            ) -> futures::future::BoxFuture<'_, Result<u64, LinkError>> {
                Box::pin(async { Err(LinkError::Down("gated".into())) })
            }
            fn drop_through(
                &self,
                _: u64,
            ) -> futures::future::BoxFuture<'_, Result<(), LinkError>> {
                Box::pin(async { Err(LinkError::Down("gated".into())) })
            }
        }
        struct EmptySource;
        impl LogSource for EmptySource {
            fn read(&self, _: u64, _: u64, _: u64) -> std::io::Result<Vec<u8>> {
                Ok(Vec::new())
            }
        }

        let links: Vec<Arc<dyn MemberLink>> = vec![Arc::new(Stalled), Arc::new(Stalled)];
        let rep = Replicator::idle(1, 0, Some(Arc::new(EmptySource)), links, || {});
        let bytes = vec![0x33u8; 2 << 20];
        let before = super::governed_bytes();
        rep.flushed(Flushed {
            epoch: 1,
            appends: vec![(log_of(1), 0, Arc::from(bytes.as_slice()))],
        });
        rep.persisted();
        assert!(super::governed_bytes() >= before + bytes.len() as u64);
        super::govern(0).await;
        assert_eq!(rep.hot_bytes(), 0);
        // Restore unrestricted allowance for other tests in the process.
        super::govern(u64::MAX).await;
    }

    /// `govern_with_on` in `IdleOnly` mode evicts idle (unmapped) extents and stops growth by
    /// capping `total_cap` at the remaining mapped working set without evicting mapped extents;
    #[cfg(target_os = "linux")]
    struct TestZeroFill;

    #[cfg(target_os = "linux")]
    impl cladefs_core::cache::Contents for TestZeroFill {
        fn read<'a>(
            &'a self,
            _offset: u64,
            buf: &'a mut [u8],
        ) -> futures::future::BoxFuture<'a, Result<(), anyhow::Error>> {
            buf.fill(0x5a);
            Box::pin(std::future::ready(Ok(())))
        }
    }

    /// `Mapped { max_shrink_bytes }` steps down mapped extents by at most `max_shrink_bytes` per
    /// pass; and `GetStats` (`stats::take_on`) exposes all governor gauges and counters.
    #[cfg(target_os = "linux")]
    #[test]
    fn govern_with_idle_only_holds_mapped_working_set_and_mapped_mode_rate_limits() {
        use crate::governor::ReclaimMode;
        use TestZeroFill as ZeroFill;
        use cladefs_core::cache::CachedExtent;
        use cladefs_core::dax::{Dax, Source};
        use cladefs_core::pages::ArenaId;
        use cladefs_core::slabs::SLAB;
        use std::sync::Arc;
        use storage_device::ExtentId;

        let daemon = crate::Daemon::new();
        daemon.block_on(async {
            let cache = daemon.host.cache();
            let slabs = daemon.host.slabs();

            let baseline = slabs.arenas_bytes();
            // Populate 3 mapped extents (3 MiB) and 2 idle unmapped extents (2 MiB).
            let (mut dax, window) =
                Dax::new_on(&daemon.host, ArenaId::DEFAULT, 3 * SLAB, None).unwrap();
            let zero_page = slabs.arenas_bytes() - baseline;
            let mut mapped = Vec::new();
            for seq in 1..=3u64 {
                let id = ExtentId { cell: 4242, sequence: seq };
                let ext = Arc::new(CachedExtent::new_on(
                    cache,
                    ArenaId::DEFAULT,
                    id,
                    Box::new(ZeroFill),
                    SLAB,
                ));
                cache.insert_for_test(ArenaId::DEFAULT, id, ext.clone());
                let source: Arc<dyn Source> = ext.clone();
                dax.map((seq - 1) * SLAB, &source, 0, SLAB);
                ext.clone().bring_in(0).await.unwrap();
                mapped.push(ext);
            }
            let mut idle = Vec::new();
            for seq in 4..=5u64 {
                let id = ExtentId { cell: 4242, sequence: seq };
                let ext = Arc::new(CachedExtent::new_on(
                    cache,
                    ArenaId::DEFAULT,
                    id,
                    Box::new(ZeroFill),
                    SLAB,
                ));
                cache.insert_for_test(ArenaId::DEFAULT, id, ext.clone());
                ext.clone().bring_in(0).await.unwrap();
                idle.push(ext);
            }
            assert_eq!(slabs.arenas_bytes(), baseline + zero_page + 5 * SLAB);

            // IdleOnly with allowance = 1 MiB: evicts the 2 MiB of idle extents, leaves all 3 MiB of
            // mapped extents resident, and sets total_cap >= remaining mapped bytes (slab-aligned) so
            // growth stops without thrashing.
            super::govern_with_on(&daemon, SLAB, ReclaimMode::IdleOnly).await;
            let mapped_held = baseline + zero_page + 3 * SLAB;
            assert_eq!(slabs.arenas_bytes(), mapped_held);
            assert_eq!(slabs.total_cap(), Some(mapped_held.div_ceil(SLAB) * SLAB));
            assert!(idle.iter().all(|e| e.held() == 0), "idle extents must be evicted");
            assert!(mapped.iter().all(|e| e.held() > 0), "mapped extents must stay resident");

            // Rate-limited Mapped mode (1 MiB/tick) shrinks mapped extents by 1 MiB per pass, not all
            // the way to 1 MiB at once.
            super::govern_with_on(&daemon, SLAB, ReclaimMode::Mapped { max_shrink_bytes: SLAB })
                .await;
            assert_eq!(slabs.arenas_bytes(), mapped_held - SLAB);
            assert_eq!(slabs.total_cap(), Some(mapped_held - SLAB));

            // Second pass steps down another 1 MiB toward the 1 MiB allowance.
            super::govern_with_on(&daemon, SLAB, ReclaimMode::Mapped { max_shrink_bytes: SLAB })
                .await;
            assert_eq!(slabs.arenas_bytes(), mapped_held - 2 * SLAB);
            assert_eq!(slabs.total_cap(), Some(mapped_held - 2 * SLAB));

            // Re-faulting one of the evicted mapped extents increments refaults, surfaced via GetStats.
            let evicted = mapped.iter().find(|e| e.held() == 0).expect("an evicted mapped extent");
            // Lift cap so the refault succeeds without waiting on relief.
            super::govern_with_on(&daemon, 8 * SLAB, ReclaimMode::None).await;
            evicted.clone().bring_in(0).await.unwrap();
            assert_eq!(cache.refaults(), 1);

            let stats = crate::stats::take_on(&daemon, std::time::Duration::ZERO, false).await;
            for gauge in [
                "memory_full",
                "memory_pressure",
                "memory_thrashing",
                "memory_floor_bytes",
                "memory_target_bytes",
                "memory_refaults_per_second",
                "memory_psi_some_avg10_bps",
                "memory_psi_full_avg10_bps",
            ] {
                assert!(
                    stats.gauges.iter().any(|(name, _)| *name == gauge),
                    "GetStats missing governor gauge {gauge}"
                );
            }
            assert!(
                stats.counters.iter().any(|(name, value)| name == "memory_refaults" && *value == 1),
                "GetStats missing memory_refaults counter"
            );
            assert!(
                stats.counters.iter().any(|(name, _)| name == "memory_full_entered"),
                "GetStats missing memory_full_entered counter"
            );
            drop((dax, window, mapped, idle));
        });
    }

    /// Multi-tenant weighted shrinking must honor `Mapped { max_shrink_bytes }`: an over-share
    /// tenant is shrunk by at most the remaining global excess above `effective_cap`, not all the
    /// way down to its weighted share in one tick.
    #[cfg(target_os = "linux")]
    #[test]
    fn govern_with_weighted_tenants_respects_mapped_rate_limit() {
        use crate::governor::ReclaimMode;
        use TestZeroFill as ZeroFill;
        use cladefs_core::cache::CachedExtent;
        use cladefs_core::dax::{Dax, Source};
        use cladefs_core::slabs::SLAB;
        use std::sync::Arc;
        use storage_device::ExtentId;

        let dir = tempfile::tempdir().unwrap();
        let daemon = crate::Daemon::new();
        cladefs_core::pages::configure_on(
            &daemon.host,
            cladefs_core::pages::Config::with_memory(16 * SLAB),
        )
        .unwrap();
        daemon.block_on(async {
            let cache = daemon.host.cache();
            let slabs = daemon.host.slabs();

            let t1 = crate::server::Tenant::new_on(&daemon, 1, dir.path()).unwrap();
            let t2 = crate::server::Tenant::new_on(&daemon, 2, dir.path()).unwrap();
            let (mut dax1, win1) = Dax::new_on(&daemon.host, t1.arena(), 4 * SLAB, None).unwrap();
            let (mut dax2, win2) = Dax::new_on(&daemon.host, t2.arena(), 2 * SLAB, None).unwrap();
            let baseline = slabs.arenas_bytes();
            let t2_baseline = slabs.arena_bytes(t2.arena());

            // Tenant 1 holds 4 MiB mapped; Tenant 2 holds 1 MiB mapped (total +5 MiB).
            let mut exts = Vec::new();
            for seq in 1..=4u64 {
                let id = ExtentId { cell: 9001, sequence: seq };
                let ext =
                    Arc::new(CachedExtent::new_on(cache, t1.arena(), id, Box::new(ZeroFill), SLAB));
                cache.insert_for_test(t1.arena(), id, ext.clone());
                let source: Arc<dyn Source> = ext.clone();
                dax1.map((seq - 1) * SLAB, &source, 0, SLAB);
                ext.clone().bring_in(0).await.unwrap();
                exts.push(ext);
            }
            {
                let id = ExtentId { cell: 9002, sequence: 1 };
                let ext =
                    Arc::new(CachedExtent::new_on(cache, t2.arena(), id, Box::new(ZeroFill), SLAB));
                cache.insert_for_test(t2.arena(), id, ext.clone());
                let source: Arc<dyn Source> = ext.clone();
                dax2.map(0, &source, 0, SLAB);
                ext.clone().bring_in(0).await.unwrap();
                exts.push(ext);
            }
            let total_before = slabs.arenas_bytes();
            assert_eq!(total_before, baseline + 5 * SLAB);

            // Equal weights: effective_cap = total_before - 1 MiB -> Tenant 1's share would be
            // ~2 MiB (2 MiB below its 4 MiB). Rate limit must cap eviction to 1 MiB total!
            super::govern_with_on(&daemon, SLAB, ReclaimMode::Mapped { max_shrink_bytes: SLAB })
                .await;
            assert_eq!(
                slabs.arenas_bytes(),
                total_before - SLAB,
                "weighted shrink must not exceed max_shrink_bytes in one tick"
            );
            // Tenant 1 (over-share) yielded the 1 MiB; Tenant 2 stayed untouched.
            assert_eq!(slabs.arena_bytes(t2.arena()), t2_baseline + SLAB);

            drop((exts, dax1, win1, dax2, win2, t1, t2));
        });
    }

    /// board 13925: `govern_on` / `govern_with_on` must work when polled outside a Tokio runtime
    /// (for example via `futures::executor::block_on` in `cell-bench`).
    #[cfg(not(madsim))]
    #[test]
    fn govern_works_outside_tokio_runtime() {
        let daemon = crate::Daemon::new();
        assert!(tokio::runtime::Handle::try_current().is_err());
        const CAP: u64 = 8 << 20;
        futures::executor::block_on(super::govern_on(&daemon, CAP));
        assert_eq!(daemon.memory.governed.load(std::sync::atomic::Ordering::Relaxed), CAP);
    }
}
