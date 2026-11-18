// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the host's disk a tenant takes (TASK-94), counted, and limited at runtime through the
//! control API, a tenant's and every tenant's together, as its memory is ([`super::memory`]).
//!
//! **What is counted.** A tenant takes what its stores' files do, the stores in its directory, the
//! one its VM processes name stores in: each store's footprint (`local_cell::footprint`, backlog
//! doc-22): its files' lengths, each rounded up to 4 KiB and counted once however many names it
//! has, and its directories, counted as its store writes them. One rule for what is shared: a
//! store is the tenant's whose directory holds it, and one in no tenant's directory is no
//! tenant's. A fork's directory holds what the fork wrote; what it shares of its base is in the
//! bucket, and read through the host's part cache (`CLADEFSD_PART_CACHE`), which is no tenant's:
//! copies of the bucket's objects that any tenant's stores read, which the disk governor bounds. A
//! tenant's reads can't fill the disk through it, and no tenant's limit depends on what another
//! read. The default tenant, of the daemon's own socket, has no directory, and is not counted.
//!
//! **How.** Nothing walks a tenant's directory but once, as it is granted, counting what the
//! stores in it hold that this process hasn't counted yet (TASK-539.5): from then on, each store
//! counts what it writes as it writes it, and what goes as it goes, a write as the stores' page
//! cache takes it, uploaded or not. What the control API asks (`GetTenantDisk`) is read, not
//! counted anew. Until TASK-539.5 each tenant's directory was walked every 10 s, each walk writing
//! back every tenant's dirty pages first (TASK-538, TASK-525).
//!
//! **Limits.** Past its limit a tenant's writes fail with `ENOSPC`, and so do its VM processes'
//! makings of stores (an `OPEN` making one, a `FORK`), which its guest sees as a full disk; the
//! daemon goes on, and so do other tenants' writes. Past the limit of every tenant's together, any
//! tenant's do. Commits go on: what was written commits, uploads, and leaves the disk, as do
//! frees, which commits make. There are no limits until set. Its metrics name tenants by their
//! IDs.
//!
//! **Not directory quotas.** A filesystem's quota of a tenant's directory (XFS project quotas,
//! ext4's) refuses whichever write of the directory's comes past it: a store closing and framing a
//! segment, which fails the store (`local_cell`'s `Phase::Failed`), or a flush appending a
//! commit's record, rather than the guest's next write, which the daemon refuses before the store
//! takes any of it. And it is the host's setup, a filesystem's that has them, which the daemon
//! can't set as tenants are granted.

use crate::Errno;
use local_cell::footprint::{self, Allotment, Pledge};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};

/// What a store a tenant's VM process makes is admitted as, before it is counted: a store's
/// files, a fork's made and an uploaded base's, take 12 to 16 KiB of a filesystem's 4 KiB blocks
/// (`tests/disk.rs`), four times which leaves room for larger blocks.
pub const STORE_MADE: u64 = 64 << 10;

/// The tenants' disks alive, and their limits, under one lock, so that a tenant made as its limit
/// is set is limited as it was set.
static REGISTRY: Mutex<Registry> = Mutex::new(Registry { disks: Vec::new(), limits: None });

struct Registry {
    /// The disks of the tenants alive: a tenant revoked and granted again with another directory,
    /// while its earlier VM processes live on, has two for a while, each limited.
    disks: Vec<Weak<Disk>>,
    /// The tenants' limits, in bytes, by ID: those granted later, or again, are limited as they
    /// are made.
    limits: Option<HashMap<u64, u64>>,
}

impl Registry {
    fn alive(&self) -> impl Iterator<Item = Arc<Disk>> + '_ {
        self.disks.iter().filter_map(Weak::upgrade)
    }

    fn limit(&self, tenant: u64) -> Option<u64> {
        self.limits.as_ref().and_then(|limits| limits.get(&tenant).copied())
    }
}

/// A tenant's disk: the allotment of its directory.
pub struct Disk {
    /// Its tenant's ID.
    tenant: u64,
    allotment: Arc<Allotment>,
}

impl Disk {
    /// The disk of the tenant of the ID `tenant`, whose directory is `dir`: its stores counted,
    /// those this process hasn't counted yet by one listing each, limited as its ID is.
    pub(super) fn new(tenant: u64, dir: &Path) -> std::io::Result<Arc<Self>> {
        let allotment = footprint::allotment(dir)?;
        let mut registry = REGISTRY.lock().unwrap();
        allotment.set_limit(registry.limit(tenant));
        let disk = Arc::new(Self { tenant, allotment });
        registry.disks.retain(|alive| alive.strong_count() > 0);
        registry.disks.push(Arc::downgrade(&disk));
        Ok(disk)
    }

    /// What it takes, in bytes: what its stores' files take, counted as they write.
    pub fn used(&self) -> u64 {
        self.allotment.bytes()
    }

    /// Its limit, if it has one.
    pub fn limit(&self) -> Option<u64> {
        self.allotment.limit()
    }

    /// Whether its last write was refused, its limit or every tenant's reached.
    pub(super) fn is_full(&self) -> bool {
        self.allotment.is_full()
    }

    /// Admits a write of `bytes` more, if they are within its limit, and every tenant's: held
    /// until the write lands, which the write says by dropping what this returns; counted as it
    /// lands. `ENOSPC` otherwise.
    pub(super) fn admit(&self, bytes: u64) -> Result<Pledge, Errno> {
        self.allotment.admit(bytes).map_err(|_| Errno::ENOSPC)
    }
}

/// Limits the disk of the tenant `tenant` to `bytes`, or every tenant's together, without one; or
/// lifts the limit, without `bytes`. A tenant's limit holds for it alive now, and granted later,
/// until it is set again: one alive twice, revoked and granted again with another directory, has
/// it for each.
pub fn set_limit(tenant: Option<u64>, bytes: Option<u64>) -> Result<(), Errno> {
    let Some(id) = tenant else {
        footprint::set_total_limit(bytes);
        return Ok(());
    };
    super::grants::check_tenant(id)?;
    let mut registry = REGISTRY.lock().unwrap();
    let limits = registry.limits.get_or_insert_with(HashMap::new);
    match bytes {
        Some(bytes) => limits.insert(id, bytes),
        None => limits.remove(&id),
    };
    for disk in registry.alive().filter(|disk| disk.tenant == id) {
        disk.allotment.set_limit(bytes);
    }
    Ok(())
}

/// The limit of every tenant's disk together, if it has one.
pub fn total_limit() -> Option<u64> {
    footprint::total_limit()
}

/// What every tenant's disk takes together.
pub fn total() -> u64 {
    footprint::total().bytes()
}

/// What the disk of the tenant `tenant` takes, or every tenant's together, without one, and its
/// limit, if it has one: `ENOENT` if no tenant `tenant` is alive. Read, not counted anew.
pub fn count(tenant: Option<u64>) -> Result<(u64, Option<u64>), Errno> {
    let Some(id) = tenant else { return Ok((total(), total_limit())) };
    let registry = REGISTRY.lock().unwrap();
    let disks: Vec<_> = registry.alive().filter(|disk| disk.tenant == id).collect();
    if disks.is_empty() {
        return Err(Errno::ENOENT);
    }
    Ok((used(&disks), registry.limit(id)))
}

/// What `disks`, a tenant's, take: each allotment once, as one tenant granted again with its
/// directory has two disks of it.
fn used(disks: &[Arc<Disk>]) -> u64 {
    let mut allotments: Vec<&Arc<Allotment>> = disks.iter().map(|disk| &disk.allotment).collect();
    allotments.sort_by_key(|allotment| Arc::as_ptr(allotment));
    allotments.dedup_by(|one, other| Arc::ptr_eq(one, other));
    allotments.iter().map(|allotment| allotment.bytes()).sum()
}

/// A tenant's disk, as the metrics show it.
pub struct TenantDisk {
    /// Its tenant's ID.
    pub tenant: u64,
    /// What it takes, in bytes, counted as its stores write.
    pub bytes: u64,
    /// Its limit, if it has one.
    pub limit: Option<u64>,
}

/// The disk of each tenant alive, by ID: a tenant alive twice, revoked and granted again with
/// another directory, is counted once, with both its directories'.
pub fn tenants() -> Vec<TenantDisk> {
    let registry = REGISTRY.lock().unwrap();
    let mut by_tenant: HashMap<u64, Vec<Arc<Disk>>> = HashMap::new();
    for disk in registry.alive() {
        by_tenant.entry(disk.tenant).or_default().push(disk);
    }
    let mut disks: Vec<_> = by_tenant
        .into_iter()
        .map(|(tenant, disks)| TenantDisk {
            tenant,
            bytes: used(&disks),
            limit: registry.limit(tenant),
        })
        .collect();
    disks.sort_by_key(|disk| disk.tenant);
    disks
}

#[cfg(test)]
mod tests {
    use super::{Disk, set_limit};
    use crate::Errno;
    use local_cell::{Dir as _, File as _, HostDir};

    const MIB: u64 = 1 << 20;

    /// A write admitted holds its bytes until it lands, and is counted as its store's file grows,
    /// whether or not the stores' page cache wrote it back: nothing counts the tenant anew between.
    #[test]
    fn writes_are_held_until_they_land_and_counted_as_they_do() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        std::fs::create_dir(&store).unwrap();
        let disk = Disk::new(0xace, dir.path()).unwrap();
        let empty = disk.used();
        set_limit(Some(0xace), Some(empty + 2 * MIB)).unwrap();
        let writing = disk.admit(MIB).unwrap();
        assert_eq!(disk.admit(2 * MIB).map(drop), Err(Errno::ENOSPC));
        let file = HostDir::new(&store).unwrap().create("written", true).unwrap();
        file.write_at(&vec![1; MIB as usize], 0).unwrap();
        drop(writing);
        assert_eq!(disk.used(), empty + MIB);
        assert_eq!(disk.admit(2 * MIB).map(drop), Err(Errno::ENOSPC));
        assert_eq!(disk.admit(MIB).map(drop), Ok(()));
    }

    /// TASK-538: counting one tenant, every tenant, and rendering their metrics write back no
    /// store's pages: another tenant's dirty ones stay dirty, counted as written.
    #[test]
    fn counting_tenants_writes_back_nothing() {
        use local_cell::pages::{Options, PageCache};
        let (one, other) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let _disk = Disk::new(0x538, one.path()).unwrap();
        let theirs = Disk::new(0x539, other.path()).unwrap();
        let store = other.path().join("store");
        std::fs::create_dir(&store).unwrap();
        let cache = PageCache::without_flusher(Options::default());
        let dir = HostDir::on(local_cell::DiskDir::new(&store).unwrap(), cache.clone()).unwrap();
        dir.create("dirty", true).unwrap().write_at(&vec![1; 4 << 12], 0).unwrap();
        let dirty = cache.usage().dirty;
        assert_eq!(dirty, 4 << 12);
        super::count(Some(0x538)).unwrap();
        super::count(None).unwrap();
        super::tenants();
        super::super::metrics::render();
        assert_eq!(cache.usage().dirty, dirty);
        assert_eq!(theirs.used(), 4 << 12);
    }
}
