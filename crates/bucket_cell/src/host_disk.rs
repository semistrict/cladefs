// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a filesystem the host keeps cells or the part cache on, as the disk governor sees it
//! (backlog doc-22, TASK-539): what the daemon's cells and its part cache hold on it, counted as
//! they write, and what the governor allows them, sampled from the filesystem itself. One per
//! filesystem, however many directories of it are opened ([`HostDisk::of`]): every cell and the
//! part cache on it charge the same counters, which the governor weighs against its free space.
//!
//! What is counted is lengths, each file's rounded up to [`GRANULE`], as written: deterministic,
//! alike on every filesystem and in memory, and known before the filesystem's own block counts
//! catch up (ZFS reports a write's blocks only once its transaction group syncs, TASK-355). A
//! cell directory in memory has a disk of its own, of a capacity a test gives it, which refuses
//! what would pass it with `ENOSPC`, as a full filesystem does ([`HostDisk::in_memory`]).

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;

/// What the disk governor leaves free of a filesystem (bytes or percentage of the disk).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskHeadroom {
    /// Fixed number of bytes left free.
    Bytes(u64),
    /// Percentage (`0..100`) of the filesystem (or cap) left free.
    Percent(u64),
}

impl DiskHeadroom {
    /// Computes the headroom in bytes for a capacity of `size` bytes, falling back to the
    /// default when `set` is `None`: on Linux, a host's own disk, 5% of `size`, at least 4 GiB
    /// (the user, 2026-11-18); on macOS, a developer's working disk, whose APFS volumes share one
    /// container's space and often most of it, 8 GiB (the user, 2026-11-27: 5% of a 1.8 TiB disk
    /// was 93 GiB, more than it had free, and the daemon refused every cell); half of `size` at
    /// most.
    pub fn resolve(size: u64, set: Option<Self>) -> u64 {
        match set {
            Some(Self::Bytes(bytes)) => bytes,
            Some(Self::Percent(percent)) => ((u128::from(size) * u128::from(percent)) / 100) as u64,
            None if cfg!(target_os = "macos") => (8u64 << 30).min(size / 2),
            None => (size / 20).max(4 << 30).min(size / 2),
        }
    }
}

/// Administrative placement/write policy for a managed host disk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum DiskPlacement {
    /// Normal operation: accepts new cells, writes, and part-cache fills.
    #[default]
    ReadWrite = 0,
    /// Draining: no new cells are placed on this disk, while existing cells keep running.
    Drain = 1,
    /// Read-only: refuses new cells, cell writes, and part-cache fills; reads still serve.
    ReadOnly = 2,
}

impl DiskPlacement {
    /// Encodes as numeric gauge value (`0=read_write, 1=drain, 2=read_only`).
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decodes from its numeric representation, defaulting to `ReadWrite`.
    pub const fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Drain,
            2 => Self::ReadOnly,
            _ => Self::ReadWrite,
        }
    }

    /// Human-readable name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadWrite => "read_write",
            Self::Drain => "drain",
            Self::ReadOnly => "read_only",
        }
    }
}

/// Runtime health state of a host disk, driven by `fstatvfs` sampling and part-cache I/O probes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum DiskHealthState {
    /// Healthy and eligible for placement, writes, and part-cache fills.
    #[default]
    Online = 0,
    /// Experienced transient I/O or `fstatvfs` errors; excluded from new placements.
    Suspect = 1,
    /// Repeated failures; isolated from new placements, writes, and cache fills.
    Offline = 2,
    /// Recovering after probes succeeded; promoted back to `Online` once stable.
    Returning = 3,
}

impl DiskHealthState {
    /// Encodes as numeric gauge value (`0=online, 1=suspect, 2=offline, 3=returning`).
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decodes from its numeric representation, defaulting to `Online`.
    pub const fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Suspect,
            2 => Self::Offline,
            3 => Self::Returning,
            _ => Self::Online,
        }
    }

    /// Human-readable name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Suspect => "suspect",
            Self::Offline => "offline",
            Self::Returning => "returning",
        }
    }
}

/// Per-disk governance policy configured from TOML or updated live at runtime.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiskPolicy {
    /// Optional human-readable disk identifier (e.g. `"nvme0"`).
    pub id: Option<String>,
    /// Hard ceiling in bytes on `cells + reserve + pledged + cache` for this disk (`None` = uncapped).
    pub cap: Option<u64>,
    /// Per-disk headroom override (`None` = inherit global default).
    pub headroom: Option<DiskHeadroom>,
    /// Hard ceiling in bytes on the part-cache shard on this disk (`None` = uncapped).
    pub cache_cap: Option<u64>,
    /// Explicit per-disk dirty-page hard ceiling in `local_cell::pages` (`None` = auto-divided).
    pub dirty_budget: Option<u64>,
    /// Placement / write mode for this disk.
    pub placement: DiskPlacement,
}

/// Consecutive `fstatvfs` sample failures required before transitioning a disk from `Suspect` to
/// `Offline`.
const OFFLINE_SAMPLE_ERRORS: u64 = 3;

#[derive(Debug, Default)]
struct DiskMeta {
    id: Option<String>,
    headroom: Option<DiskHeadroom>,
}

/// What every file's length is counted in: a filesystem's block, the largest a host's disks have,
/// as direct I/O moves them ([`crate::direct::DIRECT_BLOCK`]).
pub const GRANULE: u64 = crate::direct::DIRECT_BLOCK;

/// `len` bytes of a file, counted: rounded up to [`GRANULE`].
pub fn counted(len: u64) -> u64 {
    len.next_multiple_of(GRANULE)
}

/// A filesystem's space as it says it is now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    /// All of it, in bytes.
    pub size: u64,
    /// What an unprivileged writer may still take of it (`f_bavail`).
    pub free: u64,
    /// What is taken of it, by anyone (`f_blocks - f_bfree`).
    pub used: u64,
}

/// A count of bytes, which only its owner's accounting changes.
#[derive(Debug, Default)]
pub struct Count(AtomicU64);

impl Count {
    /// Nothing counted.
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// The bytes counted.
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    /// Counts `bytes` more.
    pub fn add(&self, bytes: u64) {
        self.0.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Counts `bytes` fewer, which it counts: a count gone below zero is an accounting's bug,
    /// which a test's build fails on.
    pub fn sub(&self, bytes: u64) {
        let before = self.0.fetch_sub(bytes, Ordering::Relaxed);
        debug_assert!(before >= bytes, "a count of {before} bytes less {bytes}");
    }
}

/// How many times what an open cell's own work may still add, never refused, its unframed bytes and
/// its open extents' are: each byte of an open extent copied once by the commit that seals it, held
/// once in a log's record, and framed once in a copy, and each raw byte framed once in a copy
/// (backlog doc-22). What an open cell's own work took, its guest's writes stopped, was at most 2.5
/// times those, or 1.08 times past [`RESERVE_PER_STORE`] (`local_cell/tests/reserve.rs`,
/// TASK-539.7).
pub const RESERVE_FACTOR: u64 = 3;

/// What each open cell may still add, never refused, whatever it holds unframed or open: a
/// segment's log's header, its framed file being made durable, its write-ahead log's records; at
/// most 28 KiB as measured (TASK-539.7).
pub const RESERVE_PER_STORE: u64 = 64 << 10;

/// What cells take of a disk, counted, and may still take: their files' bytes, and of them their
/// unframed files', raw data whose framing, or sealing in place, is still to come; the bytes of
/// their extents open, each as far as it goes, which the commits sealing them copy; the cells
/// open; and the writes admitted and not yet landed. One per open cell, summed by each allotment,
/// every allotment and each disk.
#[derive(Debug, Default)]
pub struct Tally {
    bytes: Count,
    unframed: Count,
    open: Count,
    cells: Count,
    pledged: Count,
}

impl Tally {
    /// Nothing counted.
    pub const fn new() -> Self {
        Self {
            bytes: Count::new(),
            unframed: Count::new(),
            open: Count::new(),
            cells: Count::new(),
            pledged: Count::new(),
        }
    }

    /// The bytes counted.
    pub fn bytes(&self) -> u64 {
        self.bytes.get()
    }

    /// Of those, unframed files'.
    pub fn unframed(&self) -> u64 {
        self.unframed.get()
    }

    /// The bytes of the extents open, each as far as it goes.
    pub fn open(&self) -> u64 {
        self.open.get()
    }

    /// The cells open.
    pub fn cells(&self) -> u64 {
        self.cells.get()
    }

    /// The writes admitted and not yet landed.
    pub fn pledged(&self) -> u64 {
        self.pledged.get()
    }

    /// What the cells' own work may still add, never refused ([`RESERVE_FACTOR`]).
    pub fn reserved(&self) -> u64 {
        RESERVE_FACTOR * (self.unframed() + self.open()) + RESERVE_PER_STORE * self.cells.get()
    }

    /// What is taken, reserved and pledged: what an admission weighs a write against.
    pub fn committed(&self) -> u64 {
        self.bytes() + self.reserved() + self.pledged()
    }

    /// Counts `bytes` more, `unframed` if so.
    pub fn charge(&self, bytes: u64, unframed: bool) {
        self.bytes.add(bytes);
        if unframed {
            self.unframed.add(bytes);
        }
    }

    /// Counts `bytes` fewer, `unframed` if so.
    pub fn discharge(&self, bytes: u64, unframed: bool) {
        self.bytes.sub(bytes);
        if unframed {
            self.unframed.sub(bytes);
        }
    }

    /// Counts `bytes` of those counted unframed, if `unframed`, or framed: counted either way.
    pub fn reframe(&self, bytes: u64, unframed: bool) {
        match unframed {
            true => self.unframed.add(bytes),
            false => self.unframed.sub(bytes),
        }
    }

    /// Counts `bytes` more of extents open, or fewer, if not `more`.
    pub fn opened(&self, bytes: u64, more: bool) {
        match more {
            true => self.open.add(bytes),
            false => self.open.sub(bytes),
        }
    }

    /// Counts a cell opened, or closed, if not `open`.
    pub fn cell(&self, open: bool) {
        match open {
            true => self.cells.add(1),
            false => self.cells.sub(1),
        }
    }

    /// Counts `bytes` more pledged, or fewer, if not `more`.
    pub fn pledge(&self, bytes: u64, more: bool) {
        match more {
            true => self.pledged.add(bytes),
            false => self.pledged.sub(bytes),
        }
    }
}

/// What a disk urges as it comes under pressure ([`HostDisk::urge`]): an open cell's uploader,
/// which closes its open segment then and uploads it, rather than wait for its guest to fill it,
/// so that what it held leaves the disk.
pub trait Urge: Send + Sync {
    fn urge(&self);
}

/// How a disk is sampled: a host's filesystem through a descriptor of a directory of it, or a
/// disk in memory, by what its owner says of it.
enum Sampler {
    Fs { dev: u64, dir: host_fs::File },
    Memory(Box<dyn Fn() -> Sample + Send + Sync>),
}

/// A filesystem the daemon keeps cells or the part cache on. See the module docs.
pub struct HostDisk {
    sampler: Sampler,
    /// What the cells on it hold: their files' lengths, counted ([`counted`]), each file once
    /// however many names it has, and their directories, a [`GRANULE`] each; and of that, their
    /// unframed files'.
    pub cells: Tally,
    /// What the part cache on it holds, its fills in flight included.
    pub cache: Count,
    /// Of `cache`, its fills in flight, counted but not yet written.
    pub cache_writing: Count,
    /// cladefs: of `cells`, the bytes of their data files' pages held dirty in the cell page cache,
    /// counted as written but not yet written back to the filesystem (board 5995).
    pub dirty: Count,
    /// What the governor allows the daemon to keep on it, cells and part cache together:
    /// `u64::MAX` while no governor looks at it.
    allowance: AtomicU64,
    /// What the governor leaves free of it.
    headroom: AtomicU64,
    /// Whether it is under pressure: its spare under its headroom.
    pressure: AtomicBool,
    /// Whether the last write it was weighed for was refused: logged as it begins to be.
    full: AtomicBool,
    /// What it urges under pressure ([`HostDisk::urge`]).
    urged: Mutex<Vec<Weak<dyn Urge>>>,
    /// Its host's governor wakeup signal, so writes refused on this disk wake its host's governor
    /// directly without global lookup.
    woken: Option<Arc<(Mutex<bool>, Condvar)>>,
    /// Per-disk hard byte cap (`u64::MAX` = uncapped).
    cap: AtomicU64,
    /// Per-disk part-cache shard hard byte cap (`u64::MAX` = uncapped).
    cache_cap: AtomicU64,
    /// Per-disk `PageCache` dirty hard budget (`0` = inherit auto-divided budget).
    dirty_budget: AtomicU64,
    /// Per-disk placement mode ([`DiskPlacement`]).
    placement: AtomicU8,
    /// Per-disk runtime health ([`DiskHealthState`]).
    health: AtomicU8,
    /// Consecutive `fstatvfs` sample errors.
    sample_errors: AtomicU64,
    /// Low-frequency per-disk metadata (`id` and optional `DiskHeadroom` override).
    meta: Mutex<DiskMeta>,
}

/// Per-host registry of alive filesystems' disks and governor wakeup signal, owned by
/// [`crate::Host`].
pub(crate) struct Disks {
    disks: Mutex<Vec<Weak<HostDisk>>>,
    woken: Arc<(Mutex<bool>, Condvar)>,
}

impl Disks {
    pub(crate) fn new() -> Self {
        Self { disks: Mutex::new(Vec::new()), woken: Arc::new((Mutex::new(false), Condvar::new())) }
    }
}

impl HostDisk {
    /// The disk of the filesystem `path` is on for `host`: the one alive already, if any is.
    pub fn of_on(host: &crate::Host, path: &Path) -> io::Result<Arc<Self>> {
        use host_fs::MetadataExt as _;
        let dev = host_fs::metadata(path)?.dev();
        let mut disks = host.disks.disks.lock().unwrap();
        disks.retain(|disk| disk.strong_count() > 0);
        let found = disks.iter().filter_map(Weak::upgrade).find(|disk| disk.dev() == Some(dev));
        if let Some(disk) = found {
            return Ok(disk);
        }
        let dir = host_fs::File::open(path)?;
        let disk =
            Arc::new(Self::with(Sampler::Fs { dev, dir }, Some(Arc::clone(&host.disks.woken))));
        disks.push(Arc::downgrade(&disk));
        Ok(disk)
    }

    /// The disk of the filesystem `path` is on for the process's default host outside `madsim`.
    #[cfg(not(madsim))]
    pub fn of(path: &Path) -> io::Result<Arc<Self>> {
        Self::of_on(crate::Host::process(), path)
    }

    /// A disk in memory, which `sample` says the space of: a cell directory in memory's
    /// (`local_cell::MemDisk`). It is no host's filesystem: [`HostDisk::all_on`] leaves it out.
    pub fn in_memory(sample: impl Fn() -> Sample + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self::with(Sampler::Memory(Box::new(sample)), None))
    }

    fn with(sampler: Sampler, woken: Option<Arc<(Mutex<bool>, Condvar)>>) -> Self {
        Self {
            sampler,
            cells: Tally::default(),
            cache: Count::default(),
            cache_writing: Count::default(),
            dirty: Count::default(),
            allowance: AtomicU64::new(u64::MAX),
            headroom: AtomicU64::new(0),
            pressure: AtomicBool::new(false),
            full: AtomicBool::new(false),
            urged: Mutex::new(Vec::new()),
            woken,
            cap: AtomicU64::new(u64::MAX),
            cache_cap: AtomicU64::new(u64::MAX),
            dirty_budget: AtomicU64::new(0),
            placement: AtomicU8::new(DiskPlacement::ReadWrite.as_u8()),
            health: AtomicU8::new(DiskHealthState::Online.as_u8()),
            sample_errors: AtomicU64::new(0),
            meta: Mutex::new(DiskMeta::default()),
        }
    }

    /// Wakes this disk's host's governor to look at disks again at once.
    pub fn wake_governor(&self) {
        if let Some(woken) = &self.woken {
            *woken.0.lock().unwrap() = true;
            woken.1.notify_all();
        }
    }

    /// The filesystems' disks alive on `host`.
    pub fn all_on(host: &crate::Host) -> Vec<Arc<Self>> {
        host.disks.disks.lock().unwrap().iter().filter_map(Weak::upgrade).collect()
    }

    /// The host's filesystems' disks alive on the process's default host outside `madsim`.
    #[cfg(not(madsim))]
    pub fn all() -> Vec<Arc<Self>> {
        Self::all_on(crate::Host::process())
    }

    /// The filesystem's device, if it is a host's.
    pub fn dev(&self) -> Option<u64> {
        match &self.sampler {
            Sampler::Fs { dev, .. } => Some(*dev),
            Sampler::Memory(_) => None,
        }
    }

    /// The filesystem's space now.
    pub fn sample(&self) -> io::Result<Sample> {
        match &self.sampler {
            Sampler::Fs { dir, .. } => fs_sample(dir),
            Sampler::Memory(sample) => Ok(sample()),
        }
    }

    /// What the daemon keeps on it: its cells' and its part cache's.
    pub fn held(&self) -> u64 {
        self.cells.bytes() + self.cache.get()
    }

    /// What the governor allows the daemon to keep on it: `u64::MAX` while none looks at it.
    pub fn allowance(&self) -> u64 {
        self.allowance.load(Ordering::Relaxed)
    }

    /// What the governor leaves free of it.
    pub fn headroom(&self) -> u64 {
        self.headroom.load(Ordering::Relaxed)
    }

    /// Whether it is under pressure, as the governor last said.
    pub fn under_pressure(&self) -> bool {
        self.pressure.load(Ordering::Relaxed)
    }

    /// Has it urge `urged` as it comes under pressure, while `urged` lives.
    pub fn on_pressure(&self, urged: Weak<dyn Urge>) {
        let mut all = self.urged.lock().unwrap();
        all.retain(|urged| urged.strong_count() > 0);
        all.push(urged);
    }

    /// Urges what it urges under pressure ([`HostDisk::on_pressure`]): the uploaders of the cells
    /// on it, which close and upload their open segments.
    pub fn urge(&self) {
        let urged: Vec<_> = self.urged.lock().unwrap().iter().filter_map(Weak::upgrade).collect();
        urged.iter().for_each(|urged| urged.urge());
    }

    /// A write weighed against it is refused: returns whether its refusals begin with it, to be
    /// logged once.
    pub fn refused(&self) -> bool {
        !self.full.swap(true, Ordering::Relaxed)
    }

    /// A write weighed against it is admitted: its refusals, if any, ended.
    pub fn admitted(&self) {
        self.full.store(false, Ordering::Relaxed);
    }

    /// The governor's look at it: what it allows, the headroom it leaves, and whether it is under
    /// pressure.
    pub fn govern(&self, allowance: u64, headroom: u64, pressure: bool) {
        self.allowance.store(allowance, Ordering::Relaxed);
        self.headroom.store(headroom, Ordering::Relaxed);
        self.pressure.store(pressure, Ordering::Relaxed);
    }

    /// Applies `policy` to this disk, waking the governor so new caps take effect immediately.
    pub fn set_policy(&self, policy: DiskPolicy) {
        {
            let mut meta = self.meta.lock().unwrap();
            if policy.id.is_some() {
                meta.id = policy.id;
            }
            meta.headroom = policy.headroom;
        }
        self.cap.store(policy.cap.unwrap_or(u64::MAX), Ordering::Relaxed);
        self.cache_cap.store(policy.cache_cap.unwrap_or(u64::MAX), Ordering::Relaxed);
        self.dirty_budget.store(policy.dirty_budget.unwrap_or(0), Ordering::Relaxed);
        self.placement.store(policy.placement.as_u8(), Ordering::Relaxed);
        self.wake_governor();
    }

    /// Configured human-readable disk ID, if any.
    pub fn id(&self) -> Option<String> {
        self.meta.lock().unwrap().id.clone()
    }

    /// Configured per-disk hard byte cap, or `None` when uncapped.
    pub fn cap(&self) -> Option<u64> {
        match self.cap.load(Ordering::Relaxed) {
            u64::MAX => None,
            cap => Some(cap),
        }
    }

    /// Updates the per-disk hard byte cap (`None` lifts the cap) and wakes the governor.
    pub fn set_cap(&self, cap: Option<u64>) {
        self.cap.store(cap.unwrap_or(u64::MAX), Ordering::Relaxed);
        self.wake_governor();
    }

    /// Configured per-disk part-cache byte cap, or `None` when uncapped.
    pub fn cache_cap(&self) -> Option<u64> {
        match self.cache_cap.load(Ordering::Relaxed) {
            u64::MAX => None,
            cap => Some(cap),
        }
    }

    /// Updates the per-disk part-cache byte cap (`None` lifts the cap) and wakes the governor.
    pub fn set_cache_cap(&self, cap: Option<u64>) {
        self.cache_cap.store(cap.unwrap_or(u64::MAX), Ordering::Relaxed);
        self.wake_governor();
    }

    /// Configured per-disk dirty-page hard ceiling in `local_cell::pages`, or `None` when auto.
    pub fn dirty_budget(&self) -> Option<u64> {
        match self.dirty_budget.load(Ordering::Relaxed) {
            0 => None,
            bytes => Some(bytes),
        }
    }

    /// Updates the per-disk dirty-page hard ceiling (`None` restores auto-division).
    pub fn set_dirty_budget(&self, bytes: Option<u64>) {
        self.dirty_budget.store(bytes.unwrap_or(0), Ordering::Relaxed);
    }

    /// Configured per-disk headroom override, if any.
    pub fn policy_headroom(&self) -> Option<DiskHeadroom> {
        self.meta.lock().unwrap().headroom
    }

    /// Updates the per-disk headroom override and wakes the governor.
    pub fn set_policy_headroom(&self, headroom: Option<DiskHeadroom>) {
        self.meta.lock().unwrap().headroom = headroom;
        self.wake_governor();
    }

    /// Current placement mode ([`DiskPlacement`]).
    pub fn placement(&self) -> DiskPlacement {
        DiskPlacement::from_u8(self.placement.load(Ordering::Relaxed))
    }

    /// Updates the placement mode and wakes the governor.
    pub fn set_placement(&self, placement: DiskPlacement) {
        self.placement.store(placement.as_u8(), Ordering::Relaxed);
        self.wake_governor();
    }

    /// Current health state ([`DiskHealthState`]).
    pub fn health(&self) -> DiskHealthState {
        DiskHealthState::from_u8(self.health.load(Ordering::Relaxed))
    }

    /// Records a successful `fstatvfs` sample on this disk, clearing sample error streaks and
    /// promoting `Suspect`/`Offline`/`Returning` back to `Online` when driven by sample health.
    pub fn record_sample_ok(&self) {
        let prev = self.sample_errors.swap(0, Ordering::Relaxed);
        if prev > 0 {
            self.health.store(DiskHealthState::Online.as_u8(), Ordering::Relaxed);
        }
    }

    /// Records a failed `fstatvfs` sample on this disk, degrading its health to `Suspect` and then
    /// `Offline` after [`OFFLINE_SAMPLE_ERRORS`] consecutive failures.
    pub fn record_sample_error(&self) -> DiskHealthState {
        let count = self.sample_errors.fetch_add(1, Ordering::Relaxed) + 1;
        let next = if count >= OFFLINE_SAMPLE_ERRORS {
            DiskHealthState::Offline
        } else {
            DiskHealthState::Suspect
        };
        self.health.store(next.as_u8(), Ordering::Relaxed);
        next
    }

    /// Effective usable capacity of this disk given its filesystem size `fs_size`: capped by
    /// [`HostDisk::cap`] when configured.
    pub fn effective_capacity(&self, fs_size: u64) -> u64 {
        match self.cap() {
            Some(cap) => fs_size.min(cap),
            None => fs_size,
        }
    }

    /// Bytes of spare capacity left by cells under the current governed allowance (and hard cap).
    pub fn spare(&self) -> u64 {
        let allowance = self.allowance();
        let effective = match self.cap() {
            Some(cap) => allowance.min(cap),
            None => allowance,
        };
        effective.saturating_sub(self.cells.committed())
    }

    /// Whether new cells may be placed on this disk (`Online` and `ReadWrite`).
    pub fn accepts_new_cells(&self) -> bool {
        self.health() == DiskHealthState::Online && self.placement() == DiskPlacement::ReadWrite
    }

    /// Whether existing cells on this disk may admit new writes (not `ReadOnly` and not `Offline`).
    pub fn accepts_writes(&self) -> bool {
        self.health() != DiskHealthState::Offline && self.placement() != DiskPlacement::ReadOnly
    }
}

impl std::fmt::Debug for HostDisk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostDisk")
            .field("dev", &self.dev())
            .field("cells", &self.cells)
            .field("cache", &self.cache)
            .field("allowance", &self.allowance())
            .finish_non_exhaustive()
    }
}

/// cladefs: the space of a simulated host's disk in memory, a terabyte, holding nothing as the
/// governor counts it (doc-24): what its cells hold is counted as they write it.
#[cfg(madsim)]
fn fs_sample(_: &host_fs::File) -> io::Result<Sample> {
    Ok(Sample { size: 1 << 40, free: 1 << 40, used: 0 })
}

/// The space of the filesystem `dir`, a descriptor of a directory of it, is on.
#[cfg(not(madsim))]
fn fs_sample(dir: &host_fs::File) -> io::Result<Sample> {
    use std::os::fd::AsRawFd as _;
    // SAFETY: statvfs is plain data, valid all zeros.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `dir` is an open descriptor and `stat` a statvfs, which the call fills.
    if unsafe { libc::fstatvfs(dir.as_raw_fd(), &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    #[allow(clippy::unnecessary_cast, reason = "the fields' types differ by platform")]
    let (blocks, free, available, size) =
        (stat.f_blocks as u64, stat.f_bfree as u64, stat.f_bavail as u64, stat.f_frsize as u64);
    Ok(Sample { size: blocks * size, free: available * size, used: (blocks - free) * size })
}

/// Has `host`'s governor look at its disks again at once: a write refused, or the part cache past
/// what the cells leave it.
pub fn wake_governor_on(host: &crate::Host) {
    let (woken, wake) = &*host.disks.woken;
    *woken.lock().unwrap() = true;
    wake.notify_all();
}

/// Has the process's default host's governor look at the disks again at once outside `madsim`.
#[cfg(not(madsim))]
pub fn wake_governor() {
    wake_governor_on(crate::Host::process());
}

/// Waits, `host`'s governor, until it is woken ([`wake_governor_on`]) or `most` has passed.
pub fn wait_for_governor_on(host: &crate::Host, most: Duration) {
    let (woken, wake) = &*host.disks.woken;
    let waited = wake.wait_timeout_while(woken.lock().unwrap(), most, |woken| !*woken);
    *waited.unwrap().0 = false;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two directories of one filesystem share its disk; one of another has its own.
    #[test]
    fn a_filesystem_has_one_disk() {
        let dir = tempfile::tempdir().unwrap();
        host_fs::create_dir(dir.path().join("a")).unwrap();
        let (disk, a) = (HostDisk::of(dir.path()).unwrap(), HostDisk::of(&dir.path().join("a")));
        assert!(Arc::ptr_eq(&disk, &a.unwrap()));
        assert!(HostDisk::all().iter().any(|alive| Arc::ptr_eq(alive, &disk)));
        #[cfg(target_os = "linux")]
        {
            let shm = HostDisk::of(Path::new("/dev/shm")).unwrap();
            assert!(!Arc::ptr_eq(&disk, &shm));
            assert_ne!(disk.dev(), shm.dev());
        }
    }

    /// The free bytes are the free blocks' size, as statvfs counts them.
    #[test]
    fn free_bytes_are_the_free_blocks_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = std::ffi::CString::new(dir.path().as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: statvfs is plain data, valid all zeros.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `path` is a C string and `stat` a statvfs, which the call fills.
        assert_eq!(unsafe { libc::statvfs(path.as_ptr(), &mut stat) }, 0);
        #[allow(clippy::unnecessary_cast, reason = "the fields' types differ by platform")]
        let (blocks, size) = (stat.f_bavail as u64, stat.f_frsize as u64);
        // Within a few blocks of it: other processes write meanwhile.
        let sample = HostDisk::of(dir.path()).unwrap().sample().unwrap();
        let free = sample.free;
        assert!(free.abs_diff(blocks * size) < 1024 * size, "{free} bytes, {blocks} of {size}");
        assert!(sample.used <= sample.size && free <= sample.size, "{sample:?}");
    }

    /// A length counts as the granules that hold it, a partial one whole.
    #[test]
    fn lengths_count_in_granules() {
        assert_eq!([0, 1, GRANULE, GRANULE + 1].map(counted), [0, GRANULE, GRANULE, 2 * GRANULE]);
    }
}
