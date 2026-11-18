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

use super::governor::DiskHeadroom;
use crate::Errno;
use bucket_cell::host_disk::{self, HostDisk};
use local_cell::footprint::{self, Allotment};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};

/// What a store a tenant's VM process makes is admitted as ([`footprint::STORE_MADE`]).
pub const STORE_MADE: u64 = footprint::STORE_MADE;

/// The tenants' disks alive, and their limits, under one lock, so that a tenant made as its limit
/// is set is limited as it was set.
static REGISTRY: Mutex<Registry> =
    Mutex::new(Registry { disks: Vec::new(), limits: None, weights: None });

struct Registry {
    /// The disks of the tenants alive: a tenant revoked and granted again with another directory,
    /// while its earlier VM processes live on, has two for a while, each limited.
    disks: Vec<Weak<Disk>>,
    /// The tenants' limits, in bytes, by ID: those granted later, or again, are limited as they
    /// are made.
    limits: Option<HashMap<u64, u64>>,
    /// The tenants' weights, by ID, as their limits are ([`set_weight`]).
    weights: Option<HashMap<u64, u64>>,
}

impl Registry {
    fn alive(&self) -> impl Iterator<Item = Arc<Disk>> + '_ {
        self.disks.iter().filter_map(Weak::upgrade)
    }

    fn limit(&self, tenant: u64) -> Option<u64> {
        self.limits.as_ref().and_then(|limits| limits.get(&tenant).copied())
    }

    fn weight(&self, tenant: u64) -> u64 {
        let weight = self.weights.as_ref().and_then(|weights| weights.get(&tenant).copied());
        weight.unwrap_or(footprint::DEFAULT_WEIGHT)
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
        allotment.set_weight(registry.weight(tenant));
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

/// cladefs: weighs the disk of the tenant `tenant`, alive now or granted later, against the
/// others' as their disks come under pressure (doc-22, TASK-539.10): each tenant's writes are
/// refused past its share by weight of what the disk governor leaves the tenants, a tenant within
/// its share writing on. Every tenant's weight is [`footprint::DEFAULT_WEIGHT`] until set.
pub fn set_weight(tenant: u64, weight: u64) -> Result<(), Errno> {
    super::grants::check_tenant(tenant)?;
    let mut registry = REGISTRY.lock().unwrap();
    registry.weights.get_or_insert_with(HashMap::new).insert(tenant, weight);
    for disk in registry.alive().filter(|disk| disk.tenant == tenant) {
        disk.allotment.set_weight(weight);
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

/// A tenant's disk, or every tenant's, as the control API reports it ([`count`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Counted {
    /// What its stores' files take.
    pub bytes: u64,
    /// Its limit, if it has one.
    pub limit: Option<u64>,
    /// What it holds past `bytes` against its limit: its stores' reserves, and its writes
    /// admitted ([`Allotment::reserved`]).
    pub reserved: u64,
}

/// What the disk of the tenant `tenant` takes, or every tenant's together, without one, and its
/// limit, if it has one: `ENOENT` if no tenant `tenant` is alive. Read, not counted anew.
pub fn count(tenant: Option<u64>) -> Result<Counted, Errno> {
    let Some(id) = tenant else {
        let all = footprint::total();
        let reserved = all.reserved() + all.pledged();
        return Ok(Counted { bytes: all.bytes(), limit: total_limit(), reserved });
    };
    let registry = REGISTRY.lock().unwrap();
    let disks: Vec<_> = registry.alive().filter(|disk| disk.tenant == id).collect();
    if disks.is_empty() {
        return Err(Errno::ENOENT);
    }
    let allotments = allotments(&disks);
    Ok(Counted {
        bytes: allotments.iter().map(|allotment| allotment.bytes()).sum(),
        limit: registry.limit(id),
        reserved: allotments.iter().map(|allotment| allotment.reserved()).sum(),
    })
}

/// The allotments of `disks`, a tenant's, each once, as one tenant granted again with its
/// directory has two disks of it.
fn allotments(disks: &[Arc<Disk>]) -> Vec<&Arc<Allotment>> {
    let mut allotments: Vec<&Arc<Allotment>> = disks.iter().map(|disk| &disk.allotment).collect();
    allotments.sort_by_key(|allotment| Arc::as_ptr(allotment));
    allotments.dedup_by(|one, other| Arc::ptr_eq(one, other));
    allotments
}

/// A tenant's disk, as the metrics show it.
pub struct TenantDisk {
    /// Its tenant's ID.
    pub tenant: u64,
    /// What it takes, in bytes, counted as its stores write.
    pub bytes: u64,
    /// Its limit, if it has one.
    pub limit: Option<u64>,
    /// What it holds past `bytes` against its limit ([`Allotment::reserved`]).
    pub reserved: u64,
    /// Its weight ([`set_weight`]).
    pub weight: u64,
    /// What it keeps while its disk is under pressure, if it is.
    pub share: Option<u64>,
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
        .map(|(tenant, disks)| {
            let allotments = allotments(&disks);
            let shares = allotments.iter().filter_map(|allotment| allotment.share());
            let share = shares.reduce(|one, other| one + other);
            TenantDisk {
                tenant,
                bytes: allotments.iter().map(|allotment| allotment.bytes()).sum(),
                limit: registry.limit(tenant),
                reserved: allotments.iter().map(|allotment| allotment.reserved()).sum(),
                weight: registry.weight(tenant),
                share,
            }
        })
        .collect();
    disks.sort_by_key(|disk| disk.tenant);
    disks
}

/// cladefs: how the disk governor governs (doc-22, TASK-539.9): the headroom it leaves of each
/// filesystem, and the most the daemon keeps on all of them together, if fixed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiskConfig {
    /// `CLADEFSD_DISK_HEADROOM`'s, if set ([`super::governor::disk_headroom`]).
    pub headroom: Option<DiskHeadroom>,
    /// `CLADEFSD_DISK_BYTES`, if set: stores and part cache together, on every disk.
    pub fixed: Option<u64>,
}

impl DiskConfig {
    /// As the environment says it: refused, naming the variable, if it says what isn't one.
    pub fn from_env() -> Result<Self, String> {
        let var = |name| std::env::var(name).ok();
        let headroom = super::governor::disk_headroom_of(var("CLADEFSD_DISK_HEADROOM").as_deref())?;
        let fixed = match var("CLADEFSD_DISK_BYTES").filter(|value| !value.is_empty()) {
            None => None,
            Some(bytes) => match bytes.parse() {
                Ok(0) | Err(_) => {
                    return Err(format!(
                        "CLADEFSD_DISK_BYTES must be a positive number of bytes: {bytes:?}"
                    ));
                }
                Ok(bytes) => Some(bytes),
            },
        };
        Ok(Self { headroom, fixed })
    }
}

/// cladefs: starts, once, the daemon's disk governor, on a thread of its own, looking at its disks
/// every [`super::governor::INTERVAL`], and at once when a write is refused or the part cache passes
/// what its stores leave it (`bucket_cell::host_disk::wake_governor`). Later starts change nothing.
pub fn start(config: DiskConfig) -> Result<(), String> {
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if STARTED.set(()).is_err() {
        return Ok(());
    }
    std::thread::Builder::new()
        .name("disk governor".into())
        .spawn(move || {
            let mut errors = std::collections::HashSet::new();
            loop {
                govern_once(config, &mut errors);
                host_disk::wait_for_governor(super::governor::INTERVAL);
            }
        })
        .map_err(|err| format!("starting the disk governor: {err}"))?;
    Ok(())
}

/// cladefs: the disk governor's look at the host's disks, as `config` says (doc-22, TASK-539.9): each
/// one's allowance, what its stores and part cache may keep on it, the headroom it leaves, and
/// whether it is under pressure, published on it; the part cache on it sized to what its stores
/// leave, evicting at once; its stores' uploaders urged as pressure begins. A disk whose space
/// can't be read keeps what it was allowed, logged once until it is read again (`errors`).
pub fn govern_once(config: DiskConfig, errors: &mut std::collections::HashSet<u64>) {
    let disks = HostDisk::all();
    let cache = super::cells::opened_cache();
    for disk in &disks {
        let dev = disk.dev().unwrap_or_default();
        let sample = match disk.sample() {
            Ok(sample) => {
                errors.remove(&dev);
                sample
            }
            Err(err) => {
                if errors.insert(dev) {
                    log::error!("cladefsd: reading the space of disk {dev:#x}: {err}");
                }
                continue;
            }
        };
        let headroom = super::governor::disk_headroom(sample.size, config.headroom);
        // What it counts that its filesystem doesn't hold yet: the stores' pages dirty, and the
        // cache's fills in flight.
        let in_flight = local_cell::pages::shared().usage().dirty + disk.cache_writing.get();
        let mut allowance =
            super::governor::disk_allowance(disk.held(), sample.free, in_flight, headroom);
        if let Some(fixed) = config.fixed {
            let others: u64 = (disks.iter())
                .filter(|other| !Arc::ptr_eq(other, disk))
                .map(|other| other.held())
                .sum();
            allowance = allowance.min(fixed.saturating_sub(others));
        }
        let spare = allowance.saturating_sub(disk.stores.committed());
        // Derived first: the part cache keeps what the stores leave, down a band below it once it
        // must give back, so that it doesn't evict at every look.
        if let Some(cache) = cache.as_ref().filter(|cache| Arc::ptr_eq(cache.disk(), disk)) {
            let limit = match cache.bytes() > spare {
                true => spare.saturating_sub(headroom / 4),
                false => spare,
            };
            if let Err(err) = cache.set_limit(limit) {
                log::warn!("cladefsd: evicting the part cache to {limit} bytes: {err}");
            }
        }
        let was = disk.under_pressure();
        let pressure = super::governor::pressured(was, spare, headroom);
        disk.govern(allowance, headroom, pressure);
        share(disk, allowance, pressure);
        // Urged as it begins: each urging closes the stores' open segments, however little they
        // hold.
        if pressure && !was {
            log::warn!(
                "cladefsd: disk {dev:#x} under pressure: its stores take {} bytes and reserve \
                 {} more of {allowance}, its filesystem {} bytes free; urging their uploads",
                disk.stores.bytes(),
                disk.stores.reserved() + disk.stores.pledged(),
                sample.free
            );
            disk.urge();
        }
    }
}

/// cladefs: each host's disk's gauges, by name: their help, and each disk's value, by its device as
/// `MAJOR:MINOR` (`super::metrics`, doc-22). What can't be sampled is left out.
pub fn host_disks() -> Vec<(&'static str, &'static str, Vec<(String, u64)>)> {
    type Gauge = (&'static str, &'static str, fn(&HostDisk, &host_disk::Sample) -> u64);
    let gauges: [Gauge; 12] = [
        ("cladefsd_host_disk_size_bytes", "A disk's filesystem's size.", |_, sample| sample.size),
        ("cladefsd_host_disk_free_bytes", "What a disk's filesystem has free.", |_, sample| {
            sample.free
        }),
        (
            "cladefsd_host_disk_headroom_bytes",
            "What the disk governor leaves free of a disk.",
            |disk, _| disk.headroom(),
        ),
        (
            "cladefsd_host_disk_allowance_bytes",
            "What the disk governor allows the daemon on a disk.",
            |disk, _| disk.allowance(),
        ),
        ("cladefsd_host_disk_stores_bytes", "What the stores on a disk take.", |disk, _| {
            disk.stores.bytes()
        }),
        (
            "cladefsd_host_disk_unframed_bytes",
            "Of that, their files whose framing is to come.",
            |disk, _| disk.stores.unframed(),
        ),
        (
            "cladefsd_host_disk_reserved_bytes",
            "What the stores' own work on a disk may still add.",
            |disk, _| disk.stores.reserved(),
        ),
        ("cladefsd_host_disk_pledged_bytes", "What writes admitted on a disk hold.", |disk, _| {
            disk.stores.pledged()
        }),
        ("cladefsd_host_disk_cache_bytes", "What the part cache keeps on a disk.", |disk, _| {
            disk.cache.get()
        }),
        (
            "cladefsd_host_disk_spare_bytes",
            "What the stores on a disk leave of its allowance.",
            |disk, _| disk.allowance().saturating_sub(disk.stores.committed()),
        ),
        (
            "cladefsd_host_disk_other_bytes",
            "What a disk's filesystem holds that the daemon counts as none of its own.",
            |disk, sample| sample.used.saturating_sub(disk.held()),
        ),
        ("cladefsd_host_disk_pressure", "Whether a disk is under pressure: 1 if so.", |disk, _| {
            u64::from(disk.under_pressure())
        }),
    ];
    let sampled: Vec<_> = (HostDisk::all().into_iter())
        .filter_map(|disk| {
            let sample = disk.sample().ok()?;
            let dev = disk.dev()?;
            let device =
                format!("{}:{}", libc::major(dev as libc::dev_t), libc::minor(dev as libc::dev_t));
            Some((device, disk, sample))
        })
        .collect();
    (gauges.into_iter())
        .map(|(name, help, of)| {
            let values =
                sampled.iter().map(|(device, disk, sample)| (device.clone(), of(disk, sample)));
            (name, help, values.collect())
        })
        .collect()
}

/// cladefs: shares what `disk`'s allowance leaves the tenants by their allotments' weights while it is
/// under pressure, each allotment on it refused past its share; none otherwise (TASK-539.10).
/// What its stores in no allotment take, the default tenant's, comes off first.
fn share(disk: &Arc<HostDisk>, allowance: u64, pressure: bool) {
    let on_disk: Vec<_> = (footprint::allotments().into_iter())
        .filter(|allotment| Arc::ptr_eq(allotment.disk(), disk))
        .collect();
    if !pressure {
        on_disk.iter().for_each(|allotment| allotment.set_share(None));
        return;
    }
    let allotted: u64 = on_disk.iter().map(|allotment| allotment.committed()).sum();
    let others = disk.stores.committed().saturating_sub(allotted);
    let weighted: Vec<_> =
        on_disk.iter().enumerate().map(|(at, allotment)| (at as u64, allotment.weight())).collect();
    for (at, share) in super::governor::shares(allowance.saturating_sub(others), &weighted) {
        on_disk[at as usize].set_share(Some(share));
    }
}

#[cfg(test)]
mod tests {
    use super::{Disk, set_limit};
    use local_cell::{Dir as _, File as _, HostDir};

    const MIB: u64 = 1 << 20;

    /// A write admitted holds its bytes, and what its store's own work may add for them, until it
    /// lands, and is counted as its store's file grows, whether or not the stores' page cache wrote
    /// it back: nothing counts the tenant anew between (doc-22).
    #[test]
    fn writes_are_held_until_they_land_and_counted_as_they_do() {
        use local_cell::footprint::DiskFull;
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        std::fs::create_dir(&store).unwrap();
        let disk = Disk::new(0xace, dir.path()).unwrap();
        let opened = HostDir::new(&store).unwrap();
        let footprint = opened.footprint().clone();
        let empty = disk.used();
        // A MiB written to no file the store frames is admitted as four: it, and its reserve.
        let admitted = (1 + bucket_cell::host_disk::RESERVE_FACTOR) * MIB;
        set_limit(Some(0xace), Some(empty + footprint.reserved() + 2 * admitted)).unwrap();
        let writing = footprint.admit(MIB, 0).unwrap();
        assert_eq!(footprint.admit(2 * MIB, 0).map(drop), Err(DiskFull::Allotment));
        opened.create("written", true).unwrap().write_at(&vec![1; MIB as usize], 0).unwrap();
        drop(writing);
        assert_eq!(disk.used(), empty + MIB);
        assert_eq!(footprint.admit(2 * MIB, 0).map(drop), Err(DiskFull::Allotment));
        assert_eq!(footprint.admit(MIB, 0).map(drop), Ok(()));
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
