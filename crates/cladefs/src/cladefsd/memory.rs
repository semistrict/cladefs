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

use super::Daemon;
use super::server::{Parking, Tenant};
use crate::Errno;
use crate::slabs::SLAB;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

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
}

impl State {
    pub(super) fn new() -> Self {
        Self {
            registry: Mutex::new(Registry {
                tenants: Vec::new(),
                limits: None,
                in_flight: None,
                weights: None,
                total: None,
            }),
            governed: AtomicU64::new(u64::MAX),
            held_parts: local_cell::HeldParts::new(),
            tenant_in_flight: Arc::new(AtomicU64::new(DEFAULT_TENANT_IN_FLIGHT)),
            total_in_flight: Arc::new(InFlight::total()),
        }
    }
}

/// cladefs: a tenant's weight unless it is set: each tenant alike.
pub const DEFAULT_WEIGHT: u64 = 1;

struct Registry {
    /// The tenants alive, which may outlive their grants, while their VM processes do: a tenant
    /// revoked and granted again goes on as it was ([`alive`]), but with another directory, when
    /// it is two at once for a while, each limited.
    tenants: Vec<Weak<Tenant>>,
    /// The tenants' limits, in bytes, by ID: those granted later, or again, are limited as they
    /// are made.
    limits: Option<HashMap<u64, u64>>,
    /// cladefs: the tenants' bytes in flight ([`InFlight`]), by ID, as their limits are.
    in_flight: Option<HashMap<u64, u64>>,
    /// cladefs: the tenants' weights, by ID, as their limits are ([`set_weight`], TASK-358).
    weights: Option<HashMap<u64, u64>>,
    /// cladefs: the limit on every tenant's memory together the control API set, if it did, which
    /// the governor's allowance lowers further ([`govern`]).
    total: Option<u64>,
}

impl Registry {
    fn alive(&self) -> impl Iterator<Item = Arc<Tenant>> + '_ {
        self.tenants.iter().filter_map(Weak::upgrade)
    }

    fn limit(&self, tenant: u64) -> Option<u64> {
        self.limits.as_ref().and_then(|limits| limits.get(&tenant).copied())
    }

    fn weight(&self, tenant: u64) -> u64 {
        self.weights
            .as_ref()
            .and_then(|weights| weights.get(&tenant).copied())
            .unwrap_or(DEFAULT_WEIGHT)
    }

    /// Limits every tenant's memory together to the least of the control API's limit and the
    /// governor's allowance.
    fn apply_total(&self, host: &crate::Host, governed: u64) {
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

/// Counts `tenant`, new, among those alive on `daemon`, limited as its ID is.
pub(super) fn register_on(daemon: &Daemon, tenant: &Arc<Tenant>) {
    let mut registry = daemon.memory.registry.lock().unwrap();
    if let Some(bytes) = registry.limit(tenant.id()) {
        daemon.host.slabs().set_arena_cap(tenant.arena(), Some(bytes));
    }
    let budget = registry.in_flight.as_ref().and_then(|set| set.get(&tenant.id()).copied());
    tenant.in_flight().set_budget(budget);
    registry.tenants.retain(|alive| alive.strong_count() > 0);
    registry.tenants.push(Arc::downgrade(tenant));
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
    super::grants::check_tenant(id)?;
    let mut registry = daemon.memory.registry.lock().unwrap();
    let limits = registry.limits.get_or_insert_with(HashMap::new);
    match bytes {
        Some(bytes) => limits.insert(id, bytes),
        None => limits.remove(&id),
    };
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
    super::grants::check_tenant(tenant)?;
    let mut registry = daemon.memory.registry.lock().unwrap();
    registry.weights.get_or_insert_with(HashMap::new).insert(tenant, weight);
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
    let fetch_mem = crate::bucket::memory_on(&daemon.host);
    daemon.host.slabs().arenas_bytes()
        + held_parts_on(daemon).bytes()
        + local_cell::replicate::hot_bytes_on(daemon.host.local())
        + fetch_mem.recent_bytes()
        + fetch_mem.tails_bytes()
        + daemon.host.local().pages().bytes()
        + local_cell::framing_buffers_bytes()
        + crate::host_cell::decoded_kept_bytes_on(&daemon.host)
}

/// cladefs: the memory the process's default daemon's governor governs.
#[cfg(not(madsim))]
pub fn governed_bytes() -> u64 {
    governed_bytes_on(Daemon::process())
}

/// cladefs: `daemon`'s governor's look at the host's memory (TASK-358).
pub(super) async fn govern_on(daemon: &Daemon, allowance: u64) {
    daemon.memory.governed.store(allowance, Ordering::Relaxed);
    // cladefs: every tenant's bytes in flight within a share of what it allows (board 8465).
    total_in_flight_on(daemon).govern(allowance / GOVERNED_IN_FLIGHT_SHARE);
    // cladefs: its shrinks wait for what they evict this long at most, together (board 8535).
    let deadline = std::time::Instant::now() + PARKED_WAIT;
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
    // (TASK-603, board 6026).
    let fetch_mem = crate::bucket::memory_on(&daemon.host);
    let room = room.saturating_sub(local_cell::replicate::hot_bytes_on(daemon.host.local()));
    if fetch_mem.recent_bytes() > room {
        fetch_mem.evict_recent_to(room);
    }
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
    if crate::host_cell::decoded_kept_bytes_on(&daemon.host) > room {
        crate::host_cell::evict_decoded_on(&daemon.host, room);
    }
    let weighted: Vec<(u64, u64)> = {
        let registry = daemon.memory.registry.lock().unwrap();
        registry.apply_total(&daemon.host, allowance);
        if daemon.host.slabs().arenas_bytes() <= allowance {
            return;
        }
        let mut ids: Vec<u64> = registry.alive().map(|tenant| tenant.id()).collect();
        ids.sort();
        ids.dedup();
        ids.into_iter().map(|id| (id, registry.weight(id))).collect()
    };
    for (id, share) in super::governor::shares(allowance, &weighted) {
        if held_by(daemon, id) > share
            && let Err(err) = shrink_by(daemon, Some(id), share, deadline).await
            && err != Errno::ENOENT
        {
            log::error!("cladefsd: shrinking a tenant's memory to its share: {err}");
        }
    }
    if daemon.host.slabs().arenas_bytes() > allowance
        && let Err(err) = shrink_by(daemon, None, allowance, deadline).await
    {
        log::error!("cladefsd: shrinking the daemon's memory to the host's: {err}");
    }
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
pub(super) const PARKED_WAIT: std::time::Duration = super::server::UNRESPONSIVE.saturating_mul(2);

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
    let slabs = daemon.host.slabs();
    let Some(id) = tenant else {
        evict_until(&daemon.host, None, || slabs.arenas_bytes_in_use() <= target, deadline).await;
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
        evict_until(&daemon.host, Some(*arena), || in_use() <= target, deadline).await;
        slabs.release_spares(Some(*arena));
    }
    Ok(held())
}

/// cladefs: evicts the pages of `arena`'s cached extents on `host`, until `enough` says so.
async fn evict_until(
    host: &crate::Host,
    arena: Option<crate::slabs::ArenaId>,
    enough: impl Fn() -> bool,
    deadline: std::time::Instant,
) {
    loop {
        let parking = Parking::new();
        let stop = |_: u64| parking.parked() || enough();
        parking.counting(|| host.cache().evict_least_used(arena, stop));
        if !parking.parked() {
            return;
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
    let default = super::server::default_tenant_made(daemon);
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
    if bytes.is_some_and(|bytes| bytes < super::wire::MAX_PAYLOAD) {
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
    if bytes.is_some_and(|bytes| bytes < super::wire::MAX_PAYLOAD) {
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
        if let Ok(tenant) = super::server::default_tenant_on(daemon) {
            tenant.in_flight().changed();
        }
        return Ok(());
    };
    super::grants::check_tenant(id)?;
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

/// Where an [`InFlight`]'s default budget comes from when no per-tenant budget is set.
enum DefaultBudget {
    Fixed(u64),
    Shared(Arc<AtomicU64>),
}

impl DefaultBudget {
    fn get(&self) -> u64 {
        match self {
            Self::Fixed(bytes) => *bytes,
            Self::Shared(shared) => shared.load(Ordering::Relaxed),
        }
    }
}

/// cladefs: what a tenant's connections have the daemon hold of its VM processes' requests together
/// (TASK-149.35).
pub struct InFlight {
    held: AtomicU64,
    /// Its budget, or 0, its default's ([`InFlight::default`]).
    budget: AtomicU64,
    /// cladefs: the most the governor allows it, every tenant's ([`total_in_flight_on`]), or
    /// `u64::MAX`.
    governed: AtomicU64,
    /// The most it held, which the tests measure.
    peak: AtomicU64,
    /// What says it held less, or its budget grew.
    less: event_listener::Event,
    /// cladefs: its budget unless set.
    default: DefaultBudget,
    /// cladefs: a tenant's, what every tenant's together hold within too ([`total_in_flight_on`]).
    within: Option<Arc<InFlight>>,
}

impl InFlight {
    fn made(default: DefaultBudget, within: Option<Arc<InFlight>>) -> Self {
        Self {
            held: AtomicU64::new(0),
            budget: AtomicU64::new(0),
            governed: AtomicU64::new(u64::MAX),
            peak: AtomicU64::new(0),
            less: event_listener::Event::new(),
            default,
            within,
        }
    }

    /// cladefs: a tenant's on `daemon`, within every tenant's together (board 8465).
    pub(super) fn of_a_tenant(daemon: &Daemon) -> Self {
        Self::made(
            DefaultBudget::Shared(Arc::clone(&daemon.memory.tenant_in_flight)),
            Some(Arc::clone(&daemon.memory.total_in_flight)),
        )
    }

    /// cladefs: every tenant's together ([`total_in_flight_on`]).
    fn total() -> Self {
        Self::made(DefaultBudget::Fixed(DEFAULT_TOTAL_IN_FLIGHT), None)
    }

    /// cladefs: the governor allows it `bytes` at most, a payload's at least (board 8465).
    fn govern(&self, bytes: u64) {
        let bytes = bytes.max(super::wire::MAX_PAYLOAD);
        if self.governed.swap(bytes, Ordering::Relaxed) < bytes {
            // Larger, it lets those waiting go on.
            self.changed();
        }
    }

    /// What it holds.
    pub fn held(&self) -> u64 {
        self.held.load(Ordering::Relaxed)
    }

    /// The most it held.
    pub fn peak(&self) -> u64 {
        self.peak.load(Ordering::Relaxed)
    }

    /// Its budget: its own, or its default; no more than the governor allows.
    pub fn budget(&self) -> u64 {
        let budget = match self.budget.load(Ordering::Relaxed) {
            0 => self.default.get(),
            budget => budget,
        };
        budget.min(self.governed.load(Ordering::Relaxed))
    }

    fn set_budget(&self, bytes: Option<u64>) {
        self.budget.store(bytes.unwrap_or(0), Ordering::Relaxed);
        self.changed();
    }

    /// Tells those waiting within its budget that there may be enough.
    fn changed(&self) {
        self.less.notify(usize::MAX);
    }

    /// Holds `bytes` more, within its budget or not: what the daemon queued for a VM process, which
    /// it holds whatever. cladefs: and every tenant's, a tenant's.
    pub fn add(&self, bytes: u64) {
        let held = self.held.fetch_add(bytes, Ordering::Relaxed) + bytes;
        self.peak.fetch_max(held, Ordering::Relaxed);
        if let Some(within) = &self.within {
            within.add(bytes);
        }
    }

    /// Holds `bytes` fewer. cladefs: and every tenant's, a tenant's.
    pub fn sub(&self, bytes: u64) {
        if bytes > 0 {
            self.held.fetch_sub(bytes, Ordering::Relaxed);
            self.changed();
            if let Some(within) = &self.within {
                within.sub(bytes);
            }
        }
    }

    /// Holds `bytes` more, if they are within its budget, or it holds nothing.
    fn try_take(&self, bytes: u64) -> bool {
        let budget = self.budget();
        let taken = self.held.try_update(Ordering::Relaxed, Ordering::Relaxed, |held| {
            (held == 0 || held + bytes <= budget).then_some(held + bytes)
        });
        if let Ok(held) = taken {
            self.peak.fetch_max(held + bytes, Ordering::Relaxed);
        }
        taken.is_ok()
    }

    /// Holds `bytes` more, once they are within its budget, or it holds nothing: waiting, holding
    /// no thread, until then. cladefs: a tenant's, then within every tenant's, as it is (board
    /// 8465): what it took of its own given back if it is dropped waiting for that.
    pub async fn take(&self, bytes: u64) {
        self.take_own(bytes).await;
        let Some(within) = &self.within else { return };
        /// What a tenant took of its own, given back unless every tenant's is taken too.
        struct Taken<'a>(&'a InFlight, u64);
        impl Drop for Taken<'_> {
            fn drop(&mut self) {
                self.0.held.fetch_sub(self.1, Ordering::Relaxed);
                self.0.changed();
            }
        }
        let taken = Taken(self, bytes);
        within.take_own(bytes).await;
        std::mem::forget(taken);
    }

    /// Holds `bytes` more of its own, as [`InFlight::take`] does, its budget alone looked at.
    async fn take_own(&self, bytes: u64) {
        loop {
            if self.try_take(bytes) {
                return;
            }
            let less = self.less.listen();
            if self.try_take(bytes) {
                return;
            }
            less.await;
        }
    }
}

#[cfg(test)]
mod tests {
    /// The process's anonymous memory resident, in bytes, as /proc says.
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
}
