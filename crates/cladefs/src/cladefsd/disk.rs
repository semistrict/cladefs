// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the host's disk a tenant takes (TASK-94), counted, and limited at runtime through the
//! control API, a tenant's and every tenant's together, as its memory is ([`super::memory`]).
//!
//! **What is counted.** A tenant takes what its cells' files do, the cells in its directory, the
//! one its VM processes name cells in: each cell's footprint (`local_cell::footprint`, backlog
//! doc-22): its files' lengths, each rounded up to 4 KiB and counted once however many names it
//! has, and its directories, counted as its cell writes them. One rule for what is shared: a
//! cell is the tenant's whose directory holds it, and one in no tenant's directory is no
//! tenant's. A fork's directory holds what the fork wrote; what it shares of its base is in the
//! bucket, and read through the host's part cache (`CLADEFSD_PART_CACHE`), which is no tenant's:
//! copies of the bucket's objects that any tenant's cells read, which the disk governor bounds. A
//! tenant's reads can't fill the disk through it, and no tenant's limit depends on what another
//! read. The default tenant, of the daemon's own socket, has no directory, and is not counted.
//!
//! **How.** Nothing walks a tenant's directory but once, as it is granted, counting what the
//! cells in it hold that this process hasn't counted yet (TASK-539.5): from then on, each cell
//! counts what it writes as it writes it, and what goes as it goes, a write as the cells' page
//! cache takes it, uploaded or not. What the control API asks (`GetTenantDisk`) is read, not
//! counted anew. Until TASK-539.5 each tenant's directory was walked every 10 s, each walk writing
//! back every tenant's dirty pages first (TASK-538, TASK-525).
//!
//! **Limits.** Past its limit a tenant's writes fail with `ENOSPC`, and so do its VM processes'
//! makings of cells (an `OPEN` making one, a `FORK`), which its guest sees as a full disk; the
//! daemon goes on, and so do other tenants' writes. Past the limit of every tenant's together, any
//! tenant's do. Commits go on: what was written commits, uploads, and leaves the disk, as do
//! frees, which commits make. There are no limits until set. Its metrics name tenants by their
//! IDs.
//!
//! **Not directory quotas.** A filesystem's quota of a tenant's directory (XFS project quotas,
//! ext4's) refuses whichever write of the directory's comes past it: a cell closing and framing a
//! segment, which fails the cell (`local_cell`'s `Phase::Failed`), or a flush appending a
//! commit's record, rather than the guest's next write, which the daemon refuses before the cell
//! takes any of it. And it is the host's setup, a filesystem's that has them, which the daemon
//! can't set as tenants are granted.

use super::governor::DiskHeadroom;
use crate::Errno;
use bucket_cell::host_disk::{self, HostDisk};
use local_cell::footprint::{self, Allotment};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};

/// What a cell a tenant's VM process makes is admitted as ([`footprint::STORE_MADE`]).
pub const STORE_MADE: u64 = footprint::STORE_MADE;

node_local::node_local! {
    /// The tenants' disks alive, and their limits, under one lock, so that a tenant made as its
    /// limit is set is limited as it was set.
    static REGISTRY: Mutex<Registry> =
        Mutex::new(Registry { disks: Vec::new(), limits: None, weights: None });
}

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
    /// The disk of the tenant of the ID `tenant`, whose directory is `dir`: its cells counted,
    /// those this process hasn't counted yet by one listing each, limited as its ID is.
    pub(super) fn new(tenant: u64, dir: &Path) -> std::io::Result<Arc<Self>> {
        let allotment = footprint::allotment(dir)?;
        let mut registry = REGISTRY.get().lock().unwrap();
        allotment.set_limit(registry.limit(tenant));
        allotment.set_weight(registry.weight(tenant));
        let disk = Arc::new(Self { tenant, allotment });
        registry.disks.retain(|alive| alive.strong_count() > 0);
        registry.disks.push(Arc::downgrade(&disk));
        Ok(disk)
    }

    /// What it takes, in bytes: what its cells' files take, counted as they write.
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
    let mut registry = REGISTRY.get().lock().unwrap();
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
    let mut registry = REGISTRY.get().lock().unwrap();
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
    /// What its cells' files take.
    pub bytes: u64,
    /// Its limit, if it has one.
    pub limit: Option<u64>,
    /// What it holds past `bytes` against its limit: its cells' reserves, and its writes
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
    let registry = REGISTRY.get().lock().unwrap();
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
    /// What it takes, in bytes, counted as its cells write.
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
    let registry = REGISTRY.get().lock().unwrap();
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
    /// `CLADEFSD_DISK_BYTES`, if set: cells and part cache together, on every disk.
    pub fixed: Option<u64>,
}

impl DiskConfig {
    /// As the environment says it: refused, naming the variable, if it says what isn't one.
    pub fn from_env() -> Result<Self, String> {
        let var = |name| node_local::env::var(name).ok();
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
/// every [`super::governor::INTERVAL`], and at once when a write is refused or the part cache
/// passes what its cells leave it (`bucket_cell::host_disk::wake_governor`). Later starts change
/// nothing.
pub fn start(config: DiskConfig) -> Result<(), String> {
    node_local::node_local! {
        static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    }
    if STARTED.get().set(()).is_err() {
        return Ok(());
    }
    let mut errors = std::collections::HashSet::new();
    let governing = move || {
        govern_once(config, &mut errors);
        true
    };
    let (every, woken) = (super::governor::INTERVAL, host_disk::wait_for_governor);
    // cladefs: a thread of its own, or under madsim a task (`super::run`, doc-24 step 12).
    super::run::every("disk governor", every, woken, governing)
        .map_err(|err| format!("starting the disk governor: {err}"))
}

/// cladefs: the disk governor's look at the host's disks, as `config` says (doc-22, TASK-539.9):
/// each one's allowance, what its cells and part cache may keep on it, the headroom it leaves, and
/// whether it is under pressure, published on it; the part cache on it sized to what its cells
/// leave, evicting at once; its cells' uploaders urged as pressure begins. A disk whose space can't
/// be read keeps what it was allowed, logged once until it is read again (`errors`).
pub fn govern_once(config: DiskConfig, errors: &mut std::collections::HashSet<u64>) {
    let disks = HostDisk::all();
    govern_disks(&disks, config, errors);
}

fn govern_disks(
    disks: &[Arc<HostDisk>],
    config: DiskConfig,
    errors: &mut std::collections::HashSet<u64>,
) {
    let cache = super::cells::opened_cache();
    for disk in disks {
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
        // What it counts that its filesystem doesn't hold yet: this disk's cells' pages dirty in
        // the cell page cache (board 5995), and the cache's fills in flight.
        let in_flight = disk.dirty.get() + disk.cache_writing.get();
        let mut allowance =
            super::governor::disk_allowance(disk.held(), sample.free, in_flight, headroom);
        if let Some(fixed) = config.fixed {
            let others: u64 = (disks.iter())
                .filter(|other| !Arc::ptr_eq(other, disk))
                .map(|other| other.held())
                .sum();
            allowance = allowance.min(fixed.saturating_sub(others));
        }
        let spare = allowance.saturating_sub(disk.cells.committed());
        // Derived first: the part cache keeps what the cells leave, down a band below it once it
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
        // Urged as it begins: each urging closes the cells' open segments, however little they
        // hold.
        if pressure && !was {
            log::warn!(
                "cladefsd: disk {dev:#x} under pressure: its cells take {} bytes and reserve \
                 {} more of {allowance}, its filesystem {} bytes free; urging their uploads",
                disk.cells.bytes(),
                disk.cells.reserved() + disk.cells.pledged(),
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
        ("cladefsd_host_disk_stores_bytes", "What the cells on a disk take.", |disk, _| {
            disk.cells.bytes()
        }),
        (
            "cladefsd_host_disk_unframed_bytes",
            "Of that, their files whose framing is to come.",
            |disk, _| disk.cells.unframed(),
        ),
        (
            "cladefsd_host_disk_reserved_bytes",
            "What the cells' own work on a disk may still add.",
            |disk, _| disk.cells.reserved(),
        ),
        ("cladefsd_host_disk_pledged_bytes", "What writes admitted on a disk hold.", |disk, _| {
            disk.cells.pledged()
        }),
        ("cladefsd_host_disk_cache_bytes", "What the part cache keeps on a disk.", |disk, _| {
            disk.cache.get()
        }),
        (
            "cladefsd_host_disk_spare_bytes",
            "What the cells on a disk leave of its allowance.",
            |disk, _| disk.allowance().saturating_sub(disk.cells.committed()),
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

/// cladefs: the host's disks as the stats file says them (doc-22, TASK-539.11): after `disks:`,
/// each disk's device, `MAJOR:MINOR`, and its gauges ([`host_disks`]), `NAME=VALUE`, by the part of
/// their names after `cladefsd_host_disk_`; disks separated by `;`. `disks: none` before any is
/// opened.
pub fn line() -> String {
    let mut by_device: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for (name, _, values) in host_disks() {
        let short = name.trim_start_matches("cladefsd_host_disk_").trim_end_matches("_bytes");
        for (device, value) in values {
            by_device.entry(device).or_default().push(format!("{short}={value}"));
        }
    }
    let disks: Vec<_> = (by_device.into_iter())
        .map(|(device, values)| format!("{device} {}", values.join(" ")))
        .collect();
    match disks.is_empty() {
        true => "disks: none".to_owned(),
        false => format!("disks: {}", disks.join("; ")),
    }
}

/// cladefs: shares what `disk`'s allowance leaves the tenants by their allotments' weights while it
/// is under pressure, each allotment on it refused past its share; none otherwise (TASK-539.10).
/// What its cells in no allotment take, the default tenant's, comes off first.
fn share(disk: &Arc<HostDisk>, allowance: u64, pressure: bool) {
    let on_disk: Vec<_> = (footprint::allotments().into_iter())
        .filter(|allotment| Arc::ptr_eq(allotment.disk(), disk))
        .collect();
    if !pressure {
        on_disk.iter().for_each(|allotment| allotment.set_share(None));
        return;
    }
    let allotted: u64 = on_disk.iter().map(|allotment| allotment.committed()).sum();
    let others = disk.cells.committed().saturating_sub(allotted);
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
    use std::sync::Arc;

    const MIB: u64 = 1 << 20;

    /// A write admitted holds its bytes, and what its cell's own work may add for them, until it
    /// lands, and is counted as its cell's file grows, whether or not the cells' page cache wrote
    /// it back: nothing counts the tenant anew between (doc-22).
    #[test]
    fn writes_are_held_until_they_land_and_counted_as_they_do() {
        use local_cell::footprint::DiskFull;
        let dir = tempfile::tempdir().unwrap();
        let cell_dir = dir.path().join("store");
        std::fs::create_dir(&cell_dir).unwrap();
        let disk = Disk::new(0xace, dir.path()).unwrap();
        let opened = HostDir::new(&cell_dir).unwrap();
        let footprint = opened.footprint().clone();
        let empty = disk.used();
        // A MiB written to no file the cell frames is admitted as four: it, and its reserve.
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
    /// cell's pages: another tenant's dirty ones stay dirty, counted as written.
    #[test]
    fn counting_tenants_writes_back_nothing() {
        use local_cell::pages::{Options, PageCache};
        let (one, other) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let _disk = Disk::new(0x538, one.path()).unwrap();
        let theirs = Disk::new(0x539, other.path()).unwrap();
        let cell_dir = other.path().join("store");
        std::fs::create_dir(&cell_dir).unwrap();
        let cache = PageCache::without_flusher(Options::default());
        let dir = HostDir::on(local_cell::DiskDir::new(&cell_dir).unwrap(), cache.clone()).unwrap();
        // Open, its pages held: its last opening's going writes them back.
        let file = dir.create("dirty", true).unwrap();
        file.write_at(&vec![1; 4 << 12], 0).unwrap();
        let dirty = cache.usage().dirty;
        assert_eq!(dirty, 4 << 12);
        super::count(Some(0x538)).unwrap();
        super::count(None).unwrap();
        super::tenants();
        super::super::metrics::render();
        assert_eq!(cache.usage().dirty, dirty);
        assert_eq!(theirs.used(), 4 << 12);
        drop(file);
    }

    /// What urges it counts its urgings ([`super::host_disk::Urge`]).
    #[derive(Default)]
    struct Urged(std::sync::atomic::AtomicUsize);

    impl super::host_disk::Urge for Urged {
        fn urge(&self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// A governor allowed `fixed` bytes in all, leaving a MiB of each disk.
    fn governing(fixed: u64) {
        let headroom = Some(super::DiskHeadroom::Bytes(MIB));
        super::govern_once(
            super::DiskConfig { headroom, fixed: Some(fixed) },
            &mut Default::default(),
        );
    }

    /// TASK-539.9: the governor sizes the part cache to what its disk's cells leave of what it
    /// allows; urges the cells' uploaders as the disk comes under pressure, its cells leaving it
    /// less than its headroom, and not again while it stays so; and lets it go a quarter of the
    /// headroom past it (doc-22).
    #[test]
    fn the_governor_sizes_the_cache_to_what_cells_leave_and_urges_under_pressure() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in a process of its own, whose other threads don't read
        // the environment yet.
        unsafe { std::env::set_var("CLADEFSD_PART_CACHE", dir.path().join("cache")) };
        std::fs::create_dir(dir.path().join("cache")).unwrap();
        let cache = super::super::cells::part_cache().unwrap().unwrap();
        let disk = cache.disk().clone();
        let cell_dir = dir.path().join("store");
        std::fs::create_dir(&cell_dir).unwrap();
        let opened = HostDir::new(&cell_dir).unwrap();
        opened.create("data", true).unwrap().write_at(&vec![1; MIB as usize], 0).unwrap();
        let urged = Arc::new(Urged::default());
        let weak: std::sync::Weak<dyn super::host_disk::Urge> = Arc::downgrade(&urged) as _;
        disk.on_pressure(weak);
        let urgings = || urged.0.load(std::sync::atomic::Ordering::Relaxed);
        let committed = disk.cells.committed();

        governing(committed + 10 * MIB);
        assert_eq!((cache.capacity(), disk.under_pressure(), urgings()), (10 * MIB, false, 0));
        assert_eq!(disk.allowance(), committed + 10 * MIB);
        governing(committed + MIB / 2);
        assert_eq!((cache.capacity(), disk.under_pressure(), urgings()), (MIB / 2, true, 1));
        governing(committed + MIB / 2);
        assert_eq!(urgings(), 1, "urged as pressure began alone");
        governing(committed + MIB + MIB / 8);
        assert!(disk.under_pressure(), "let go within a quarter past the headroom");
        governing(committed + 2 * MIB);
        assert!(!disk.under_pressure());
    }

    /// TASK-539.10: under pressure, the governor shares what the disk's allowance leaves by the
    /// tenants' weights, a tenant past its share refused while one within its own writes; out of
    /// it, no share holds any. The metrics show the disk, and the tenants' weights and shares.
    #[test]
    fn weights_share_a_disk_under_pressure() {
        use local_cell::footprint::DiskFull;
        let dir = tempfile::tempdir().unwrap();
        let (heavy, light) = (dir.path().join("heavy"), dir.path().join("light"));
        std::fs::create_dir_all(heavy.join("store")).unwrap();
        std::fs::create_dir_all(light.join("store")).unwrap();
        let (heavy_disk, light_disk) =
            (Disk::new(0x3, &heavy).unwrap(), Disk::new(0x1, &light).unwrap());
        super::set_weight(0x3, 3).unwrap();
        let (heavy_cell, light_cell) = (
            HostDir::new(&heavy.join("store")).unwrap(),
            HostDir::new(&light.join("store")).unwrap(),
        );
        let disk = heavy_cell.footprint().disk().unwrap().clone();
        let committed = disk.cells.committed();
        let others =
            committed - heavy_disk.allotment.committed() - light_disk.allotment.committed();

        governing(committed + MIB / 2);
        assert!(disk.under_pressure());
        let base = committed + MIB / 2 - others;
        assert_eq!(heavy_disk.allotment.share(), Some(base * 3 / 4));
        assert_eq!(light_disk.allotment.share(), Some(base / 4));
        let light_room = base / 4 - light_disk.allotment.committed();
        let past = light_room / (1 + bucket_cell::host_disk::RESERVE_FACTOR) + MIB / 64;
        let refused = light_cell.footprint().admit(past, 0).map(drop);
        assert_eq!(refused, Err(DiskFull::Share));
        assert!(heavy_cell.footprint().admit(4096, 0).is_ok());
        assert!(super::line().contains(" pressure=1"), "{}", super::line());
        // Each of its disk's gauges, as its counters say them.
        let dev = disk.dev().unwrap() as libc::dev_t;
        let device = format!("{}:{}", libc::major(dev), libc::minor(dev));
        let gauge = |name: &str| {
            let (_, _, values) = (super::host_disks().into_iter())
                .find(|(named, _, _)| *named == name)
                .unwrap_or_else(|| panic!("no {name}"));
            values.into_iter().find(|(of, _)| *of == device).unwrap().1
        };
        let sample = disk.sample().unwrap();
        assert_eq!(gauge("cladefsd_host_disk_size_bytes"), sample.size);
        assert_eq!(gauge("cladefsd_host_disk_allowance_bytes"), committed + MIB / 2);
        assert_eq!(gauge("cladefsd_host_disk_headroom_bytes"), MIB);
        assert_eq!(gauge("cladefsd_host_disk_stores_bytes"), disk.cells.bytes());
        assert_eq!(gauge("cladefsd_host_disk_unframed_bytes"), disk.cells.unframed());
        assert_eq!(gauge("cladefsd_host_disk_reserved_bytes"), disk.cells.reserved());
        assert_eq!(gauge("cladefsd_host_disk_pledged_bytes"), disk.cells.pledged());
        assert_eq!(gauge("cladefsd_host_disk_cache_bytes"), disk.cache.get());
        let spare = (committed + MIB / 2).saturating_sub(disk.cells.committed());
        assert_eq!(gauge("cladefsd_host_disk_spare_bytes"), spare);
        assert_eq!(gauge("cladefsd_host_disk_pressure"), 1);
        // What its filesystem holds of others': within what others wrote between the samples.
        let other = gauge("cladefsd_host_disk_other_bytes");
        let used = disk.sample().unwrap().used;
        assert!(other.abs_diff(used.saturating_sub(disk.held())) < 64 * MIB, "{other}");
        let rendered = super::super::metrics::render();
        for line in
            ["cladefsd_tenant_disk_weight{tenant=\"3\"} 3", "cladefsd_host_disk_pressure{device="]
        {
            assert!(rendered.contains(line), "no {line} in\n{rendered}");
        }
        assert!(
            rendered.contains(&format!(
                "cladefsd_tenant_disk_share_bytes{{tenant=\"1\"}} {}",
                base / 4
            ))
        );

        governing(committed + 64 * MIB);
        assert_eq!((heavy_disk.allotment.share(), light_disk.allotment.share()), (None, None));
        assert!(light_cell.footprint().admit(past, 0).is_ok());
    }

    /// board 5995: dirty pages in the cell page cache on Disk A reduce Disk A's allowance alone,
    /// leaving an unrelated Disk B's allowance, spare, part cache limit, and pressure untouched.
    #[test]
    fn dirty_pages_on_one_disk_do_not_affect_another_host_disk() {
        use bucket_cell::host_disk::{HostDisk, Sample};
        use local_cell::pages::{Cached, Options, PageCache};

        let size = 100 * MIB;
        let free = 20 * MIB;
        let headroom = 10 * MIB;
        let disk_a = HostDisk::in_memory(move || Sample { size, free, used: size - free });
        let disk_b = HostDisk::in_memory(move || Sample { size, free, used: size - free });

        let dir = local_cell::MemDisk::new();
        let cache = PageCache::without_flusher(Options::default());
        let file_a =
            Cached::on_disk(dir.create("a", true).unwrap(), &cache, Some(disk_a.clone())).unwrap();
        // Dirty 12 MiB on Disk A — more than Disk B's spare headroom above `headroom`.
        let dirty_bytes = 12 * MIB;
        file_a.write_at(&vec![0x5a; dirty_bytes as usize], 0).unwrap();
        disk_a.cells.charge(dirty_bytes, true);
        assert_eq!(disk_a.dirty.get(), dirty_bytes);
        assert_eq!(disk_b.dirty.get(), 0);

        let config =
            super::DiskConfig { headroom: Some(super::DiskHeadroom::Bytes(headroom)), fixed: None };
        let mut errors = std::collections::HashSet::new();
        super::govern_disks(&[disk_a.clone(), disk_b.clone()], config, &mut errors);

        // Disk B has 0 held, 20 MiB free, 0 dirty, 10 MiB headroom => allowance = 10 MiB,
        // spare = 10 MiB, not under pressure (spare == headroom).
        assert_eq!(disk_b.allowance(), free - headroom);
        assert!(!disk_b.under_pressure(), "Disk A's dirty pages must not pressure Disk B");

        // Disk A has 12 MiB held and 12 MiB dirty not yet on disk: free(20) - in_flight(12) +
        // held(12) - headroom(10) = 10 MiB < committed(12 MiB + reserve) => under pressure.
        assert_eq!(disk_a.allowance(), (free - dirty_bytes) + dirty_bytes - headroom);
        assert!(disk_a.under_pressure());

        // Syncing file_a clears Disk A's dirty count cleanly.
        file_a.sync().unwrap();
        assert_eq!(disk_a.dirty.get(), 0);
    }
}
