// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The daemon's memory, which its cached extents' pages are of (TASK-50.3): the memory a tenant
//! holds, its arena's, and every tenant's together, each limited at runtime through the control
//! API. Past a limit the daemon evicts the clean pages asked for longest ago, the tenant's own
//! past the tenant's, any tenant's past the total; with none left to evict, what would take more
//! fails with `ENOMEM` (physical memory's relief, with `crate::cache`'s evictor), and the daemon
//! goes on. cladefs: but for a page-in past the total, as the host's memory is full, which waits
//! for a page rather than fail its guest's fault, which killed its VM (`physmem::room_may_come`,
//! board 5662); the governor keeps a floor and refuses cells meanwhile (`super::governor`). cladefs: the private pages of a window that copies on write are its VM process's
//! memory, not the daemon's, and count against none of this (backlog doc-13, TASK-501). Its
//! metrics name tenants by their IDs.

use super::server::{Parking, Tenant};
use crate::Errno;
use physmem::CHUNK_SIZE;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

node_local::node_local! {
    /// The tenants alive, and their limits, under one lock, so that a tenant made as its limit is set
    /// is limited as it was set.
    static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
        tenants: Vec::new(),
        limits: None,
        in_flight: None,
        weights: None,
        total: None,
        });
}

node_local::node_local! {
    /// cladefs: what the governor allows every tenant's memory together, as the host's free memory goes
    /// ([`super::governor`], TASK-358): unlimited for a daemon not governed.
    static GOVERNED: AtomicU64 = AtomicU64::new(u64::MAX);
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
    fn apply_total(&self) {
        let cap = self.total.unwrap_or(u64::MAX).min(GOVERNED.get().load(Ordering::Relaxed));
        physmem::set_total_cap((cap != u64::MAX).then_some(cap));
    }
}

/// The tenant of the ID `id`, whose stores are in `dir`, alive, if one is: one revoked whose VM
/// processes live on, which a grant of it again goes on with (TASK-79.36).
pub(super) fn alive(id: u64, dir: &std::path::Path) -> Option<Arc<Tenant>> {
    let registry = REGISTRY.get().lock().unwrap();
    registry.alive().find(|tenant| tenant.id() == id && tenant.dir() == Some(dir))
}

/// Counts `tenant`, new, among those alive, limited as its ID is.
pub(super) fn register(tenant: &Arc<Tenant>) {
    let mut registry = REGISTRY.get().lock().unwrap();
    if let Some(bytes) = registry.limit(tenant.id()) {
        physmem::set_arena_cap(tenant.arena(), Some(bytes));
    }
    let budget = registry.in_flight.as_ref().and_then(|set| set.get(&tenant.id()).copied());
    tenant.in_flight().set_budget(budget);
    registry.tenants.retain(|alive| alive.strong_count() > 0);
    registry.tenants.push(Arc::downgrade(tenant));
}

/// Limits the memory of the tenant `tenant` to `bytes`, rounded down to 2 MiB, or every tenant's
/// together, without one; or lifts the limit, without `bytes`. A tenant's limit holds for it alive
/// now, and granted later, until it is set again. `EINVAL` for a limit under 2 MiB, which would
/// refuse every page.
pub fn set_limit(tenant: Option<u64>, bytes: Option<u64>) -> Result<(), Errno> {
    if bytes.is_some_and(|bytes| bytes < CHUNK_SIZE as u64) {
        return Err(Errno::EINVAL);
    }
    let Some(id) = tenant else {
        let mut registry = REGISTRY.get().lock().unwrap();
        registry.total = bytes;
        registry.apply_total();
        return Ok(());
    };
    super::grants::check_tenant(id)?;
    let mut registry = REGISTRY.get().lock().unwrap();
    let limits = registry.limits.get_or_insert_with(HashMap::new);
    match bytes {
        Some(bytes) => limits.insert(id, bytes),
        None => limits.remove(&id),
    };
    for tenant in registry.alive().filter(|tenant| tenant.id() == id) {
        physmem::set_arena_cap(tenant.arena(), bytes);
    }
    Ok(())
}

/// cladefs: weighs the tenant `tenant`'s memory, alive now or granted later, against the others' as
/// the host's memory runs low (TASK-358): the governor evicts each tenant first down to its share of
/// what it allows by weight, a tenant within its share kept, then any tenant's, least recently used
/// first. Every tenant's weight is [`DEFAULT_WEIGHT`] until set, such as to the memory its VMs are
/// given.
pub fn set_weight(tenant: u64, weight: u64) -> Result<(), Errno> {
    super::grants::check_tenant(tenant)?;
    let mut registry = REGISTRY.get().lock().unwrap();
    registry.weights.get_or_insert_with(HashMap::new).insert(tenant, weight);
    Ok(())
}

/// cladefs: the parts of segments the daemon's stores pack as they fill, held until their framings,
/// which they share (`local_cell::HeldParts`, TASK-400): memory the daemon holds, which the
/// governor lets go of before any tenant's ([`govern`]).
pub fn held_parts() -> &'static Arc<local_cell::HeldParts> {
    node_local::node_local! {
        static HELD: Arc<local_cell::HeldParts> = local_cell::HeldParts::new();
    }
    HELD.get()
}

/// cladefs: the memory the governor governs ([`govern`]), in bytes: what every tenant's arena holds,
/// and the parts held ([`held_parts`]).
pub fn governed_bytes() -> u64 {
    physmem::arenas_bytes()
        + held_parts().bytes()
        + local_cell::unpacked().bytes()
        + local_cell::pages::shared().bytes()
}

/// cladefs: the governor's look at the host's memory (TASK-358): every tenant's memory together, and
/// the parts held ([`held_parts`]), limited to `allowance` bytes, and held past it, evicted at
/// once: parts held first, newest first, down to what the tenants leave of it, as they are derived
/// from what the stores hold on disk, whose framings read and pack them again (TASK-400); then each
/// tenant past its share by weight down to it ([`super::governor::shares`]), then any tenant's,
/// least recently used first. The tenants are limited to `allowance`: the parts held take no room
/// from them, but what the headroom leaves between looks. cladefs: the stores' page cache
/// (`local_cell::pages`, TASK-433) as the parts held are, its clean pages evicted, its dirty
/// ones written back to be: what it holds counted as the process's, where the host's page cache
/// was counted free; and sized to what is left it ([`local_cell::pages::PageCache::size_to`]),
/// as Linux sizes its page cache to the memory free. cladefs: its shrinks wait for the pages they
/// evict holding no thread ([`shrink`], board 2946).
pub async fn govern(allowance: u64) {
    GOVERNED.get().store(allowance, Ordering::Relaxed);
    let parts = held_parts();
    let room = allowance.saturating_sub(physmem::arenas_bytes());
    if parts.bytes() > room {
        parts.evict_to(room);
    }
    // cladefs: the parts of the stores' files unpacked lately, a cache as the pages are, which
    // reads unpack again (TASK-603).
    let unpacked = local_cell::unpacked();
    let room = room.saturating_sub(parts.bytes());
    if unpacked.bytes() > room {
        unpacked.evict_to(room);
    }
    let room = room.saturating_sub(unpacked.bytes());
    let pages = local_cell::pages::shared();
    // Sized to what it may hold, as Linux's page cache to the memory free (TASK-433.8).
    pages.size_to(room);
    if pages.bytes() > room {
        pages.evict_to(room);
    }
    let weighted: Vec<(u64, u64)> = {
        let registry = REGISTRY.get().lock().unwrap();
        registry.apply_total();
        if physmem::arenas_bytes() <= allowance {
            return;
        }
        let mut ids: Vec<u64> = registry.alive().map(|tenant| tenant.id()).collect();
        ids.sort();
        ids.dedup();
        ids.into_iter().map(|id| (id, registry.weight(id))).collect()
    };
    for (id, share) in super::governor::shares(allowance, &weighted) {
        if held_by(id) > share
            && let Err(err) = shrink(Some(id), share).await
            && err != Errno::ENOENT
        {
            log::error!("cladefsd: shrinking a tenant's memory to its share: {err}");
        }
    }
    if physmem::arenas_bytes() > allowance
        && let Err(err) = shrink(None, allowance).await
    {
        log::error!("cladefsd: shrinking the daemon's memory to the host's: {err}");
    }
}

/// What the tenants alive of the ID `id` hold.
fn held_by(id: u64) -> u64 {
    let registry = REGISTRY.get().lock().unwrap();
    registry
        .alive()
        .filter(|tenant| tenant.id() == id)
        .map(|tenant| physmem::arena_bytes(tenant.arena()))
        .sum()
}

/// Evicts clean pages, those asked for longest ago first, of the tenant `tenant`'s, or of any, until
/// it, or the daemon, holds at most `target` bytes, or nothing is left to evict; and returns what it
/// holds then. `ENOENT` if no tenant `tenant` is alive. Memory is held in chunks of 2 MiB, which go
/// back once the last of their pages does. cladefs: a page a VM process's window maps, evicted, is
/// free once the VM process has unmapped it, or the daemon let it go (TASK-79.9): each extent's
/// pages free before what is held is looked at again ([`Parking`]), so that it evicts no more than
/// it must, and returns what is held once they are, not before; waiting for them holding no thread
/// (board 2946).
pub async fn shrink(tenant: Option<u64>, target: u64) -> Result<u64, Errno> {
    let parking = Parking::new();
    let Some(id) = tenant else {
        // Free memory first: the arenas' spare chunks.
        parking.counting(|| physmem::release_spares(None));
        evict_until(&parking, None, || physmem::arenas_bytes() <= target).await;
        return Ok(physmem::arenas_bytes());
    };
    let arenas: Vec<_> = (REGISTRY.get().lock().unwrap().alive())
        .filter(|tenant| tenant.id() == id)
        .map(|tenant| tenant.arena())
        .collect();
    if arenas.is_empty() {
        return Err(Errno::ENOENT);
    }
    let held = || arenas.iter().map(|arena| physmem::arena_bytes(*arena)).sum::<u64>();
    for arena in &arenas {
        parking.counting(|| physmem::release_spares(Some(*arena)));
        evict_until(&parking, Some(*arena), || held() <= target).await;
    }
    Ok(held())
}

/// cladefs: evicts the pages of `arena`'s cached extents, or of every arena's, as
/// `crate::cache::evict_least_used` orders them, until `enough` says so, looked at once the pages
/// evicted so far are free, or none is left to evict; and returns once those it evicted are free
/// ([`Parking`]). Each pass of the cache's is synchronous, and ends at the first extent whose
/// pages it left quarantined, a window's, which it waits for between passes, holding no thread
/// (board 2946); the next pass orders what is left anew.
async fn evict_until(
    parking: &Parking,
    arena: Option<physmem::ArenaId>,
    enough: impl Fn() -> bool,
) {
    loop {
        let stop = |_: u64| parking.parked() || enough();
        parking.counting(|| crate::cache::evict_least_used(arena, stop));
        if !parking.parked() {
            return;
        }
        parking.wait().await;
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

/// The memory of each tenant alive, by ID: a tenant alive twice, revoked and granted again with
/// another directory, is counted once, with both its arenas'.
pub fn tenants() -> Vec<TenantMemory> {
    let registry = REGISTRY.get().lock().unwrap();
    let mut memory: HashMap<u64, TenantMemory> = HashMap::new();
    for tenant in registry.alive() {
        let entry = memory.entry(tenant.id()).or_insert_with(|| TenantMemory {
            tenant: tenant.id(),
            bytes: 0,
            limit: registry.limit(tenant.id()),
        });
        entry.bytes += physmem::arena_bytes(tenant.arena());
    }
    let mut memory: Vec<_> = memory.into_values().collect();
    memory.sort_by_key(|memory| memory.tenant);
    memory
}

/// cladefs: what every tenant's connections have the daemon hold of their VM processes' requests
/// now, together ([`InFlight`]): the default tenant's, if made, and each granted one's alive.
pub(super) fn in_flight_held() -> u64 {
    let registry = REGISTRY.get().lock().unwrap();
    let granted: u64 = registry.alive().map(|tenant| tenant.in_flight().held()).sum();
    let default =
        super::server::default_tenant_made().map_or(0, |tenant| tenant.in_flight().held());
    granted + default
}

/// cladefs: the size from which the C library's allocator maps each allocation on its own, and
/// unmaps it as it is freed ([`return_large_allocations`], TASK-317).
pub const MMAP_THRESHOLD: usize = 1 << 20;

/// cladefs: has the C library's allocator give back what the daemon frees of its large buffers at
/// once, each allocation of [`MMAP_THRESHOLD`] or more mapped on its own (glibc's
/// `M_MMAP_THRESHOLD`), before any thread but the caller's is made: the segments it frames and
/// uploads, 32 MiB each, the parts it fetches, of a MiB. By default glibc raises that threshold to
/// the largest allocation freed, up to 32 MiB, and keeps buffers below it in its arenas, one a
/// thread at most eight times the CPUs: under an upload backlog on the test host, 134 of them,
/// 64 MiB each, held 2.6 GB of anonymous memory the daemon had freed, twice its heap at its peak
/// (TASK-317), what the host's OOM killer took it for. Nothing elsewhere: another C library's
/// allocator is left as it is.
pub fn return_large_allocations() {
    #[cfg(target_env = "gnu")]
    {
        // SAFETY: mallopt changes the allocator's settings, which every allocation after reads.
        let set = unsafe { libc::mallopt(libc::M_MMAP_THRESHOLD, MMAP_THRESHOLD as libc::c_int) };
        if set != 1 {
            log::warn!("cladefsd: glibc refused M_MMAP_THRESHOLD {MMAP_THRESHOLD}");
        }
    }
}

/// cladefs: the bytes in flight a tenant's connections share, unless they are set for it
/// ([`set_in_flight`]): what of its VM processes' requests the daemon holds at once, their stream
/// data and what it has queued for them, together (TASK-149.35). Each connection has a credit of
/// its own too (`server::Peer::credit`, 272 MiB without a window), and a tenant as many as 1,024 connections:
/// some 272 GiB of one tenant's, held within the protocol, every tenant's daemon with it. A VM
/// process moves the data of its requests through its transfer area, up to 256 KiB each, 8 at once,
/// which no room counts; past that, through the stream. 8 GiB is 30 VM processes at once keeping
/// their rooms full, which none does but writing 64 MiB extents back to back, where a host's
/// density case, 300 forks reading their base, held 16 MiB of the tenant's at most.
pub const DEFAULT_TENANT_IN_FLIGHT: u64 = 8 << 30;

node_local::node_local! {
    /// cladefs: the bytes in flight of every tenant whose own aren't set ([`set_in_flight`]).
    static TENANT_IN_FLIGHT: AtomicU64 = AtomicU64::new(DEFAULT_TENANT_IN_FLIGHT);
}

/// cladefs: sets the bytes in flight the connections of the tenant `tenant` share to `bytes`, or,
/// without `bytes`, to every tenant's; or, without `tenant`, every tenant's whose own aren't set,
/// the default without `bytes` (TASK-149.35). A tenant's hold for it alive now, and granted later,
/// until they are set again. `EINVAL` for fewer than a payload's most, which a request of it
/// couldn't be held in, and for the default tenant, which is every tenant's with them.
pub fn set_in_flight(tenant: Option<u64>, bytes: Option<u64>) -> Result<(), Errno> {
    if bytes.is_some_and(|bytes| bytes < super::wire::MAX_PAYLOAD) {
        return Err(Errno::EINVAL);
    }
    let Some(id) = tenant else {
        TENANT_IN_FLIGHT.get().store(bytes.unwrap_or(DEFAULT_TENANT_IN_FLIGHT), Ordering::Relaxed);
        // Larger, it lets those waiting go on.
        let registry = REGISTRY.get().lock().unwrap();
        registry.alive().for_each(|tenant| tenant.in_flight().changed());
        if let Ok(tenant) = super::server::default_tenant() {
            tenant.in_flight().changed();
        }
        return Ok(());
    };
    super::grants::check_tenant(id)?;
    let mut registry = REGISTRY.get().lock().unwrap();
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

/// cladefs: what a tenant's connections have the daemon hold of its VM processes' requests together
/// (TASK-149.35): what each counts against its own room, its requests' stream data and what it
/// queued for its VM process, within the bytes in flight they share, its budget, the tenant's own
/// or every tenant's ([`DEFAULT_TENANT_IN_FLIGHT`]). A request past it waits, before its payload
/// is read, until what is held leaves it enough; or, with nothing held, goes on, so that one as
/// large as the budget is served. Not let go, as a connection past its own room is: it keeps
/// within its own, and the tenant's other VM processes, a healthy tenant's busy ones, take theirs.
#[derive(Default)]
pub struct InFlight {
    held: AtomicU64,
    /// Its budget, or 0, every tenant's ([`TENANT_IN_FLIGHT`]).
    budget: AtomicU64,
    /// The most it held, which the tests measure.
    peak: AtomicU64,
    /// What says it held less, or its budget grew.
    less: event_listener::Event,
}

impl InFlight {
    /// What it holds.
    pub fn held(&self) -> u64 {
        self.held.load(Ordering::Relaxed)
    }

    /// The most it held.
    pub fn peak(&self) -> u64 {
        self.peak.load(Ordering::Relaxed)
    }

    /// Its budget: its own, or every tenant's.
    pub fn budget(&self) -> u64 {
        match self.budget.load(Ordering::Relaxed) {
            0 => TENANT_IN_FLIGHT.get().load(Ordering::Relaxed),
            budget => budget,
        }
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
    /// it holds whatever.
    pub fn add(&self, bytes: u64) {
        let held = self.held.fetch_add(bytes, Ordering::Relaxed) + bytes;
        self.peak.fetch_max(held, Ordering::Relaxed);
    }

    /// Holds `bytes` fewer.
    pub fn sub(&self, bytes: u64) {
        if bytes > 0 {
            self.held.fetch_sub(bytes, Ordering::Relaxed);
            self.changed();
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
    /// no thread, until then.
    pub async fn take(&self, bytes: u64) {
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
    /// 2.6 GB of them in the daemon otherwise.
    #[cfg(target_env = "gnu")]
    #[test]
    fn large_buffers_freed_go_back() {
        const THREADS: usize = 32;
        const BUFFERS: usize = 8;
        const SEGMENT: usize = 32 << 20;
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
}
