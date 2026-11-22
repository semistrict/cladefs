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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;

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
}

node_local::node_local! {
    /// The host's filesystems' disks alive, so that two directories of one filesystem share one: a
    /// daemon's, each simulated node's its own (doc-24).
    static DISKS: Mutex<Vec<Weak<HostDisk>>> = Mutex::new(Vec::new());
}

impl HostDisk {
    /// The disk of the filesystem `path` is on: the one alive already, if any is.
    pub fn of(path: &Path) -> io::Result<Arc<Self>> {
        use host_fs::MetadataExt as _;
        let dev = host_fs::metadata(path)?.dev();
        let mut disks = DISKS.get().lock().unwrap();
        disks.retain(|disk| disk.strong_count() > 0);
        let found = disks.iter().filter_map(Weak::upgrade).find(|disk| disk.dev() == Some(dev));
        if let Some(disk) = found {
            return Ok(disk);
        }
        let dir = host_fs::File::open(path)?;
        let disk = Arc::new(Self::with(Sampler::Fs { dev, dir }));
        disks.push(Arc::downgrade(&disk));
        Ok(disk)
    }

    /// A disk in memory, which `sample` says the space of: a cell directory in memory's
    /// (`local_cell::MemDisk`). It is no host's filesystem: [`HostDisk::all`] leaves it out.
    pub fn in_memory(sample: impl Fn() -> Sample + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self::with(Sampler::Memory(Box::new(sample))))
    }

    fn with(sampler: Sampler) -> Self {
        Self {
            sampler,
            cells: Tally::default(),
            cache: Count::default(),
            cache_writing: Count::default(),
            allowance: AtomicU64::new(u64::MAX),
            headroom: AtomicU64::new(0),
            pressure: AtomicBool::new(false),
            full: AtomicBool::new(false),
            urged: Mutex::new(Vec::new()),
        }
    }

    /// The host's filesystems' disks alive.
    pub fn all() -> Vec<Arc<Self>> {
        DISKS.get().lock().unwrap().iter().filter_map(Weak::upgrade).collect()
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

node_local::node_local! {
    /// Whether the governor is to look again at once, and what tells it so: a daemon's.
    static WOKEN: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());
}

/// Has the governor look at the disks again at once: a write refused, or the part cache past what
/// the cells leave it.
pub fn wake_governor() {
    let (woken, wake) = WOKEN.get();
    *woken.lock().unwrap() = true;
    wake.notify_all();
}

/// Waits, the governor, until it is woken ([`wake_governor`]) or `most` has passed.
pub fn wait_for_governor(most: Duration) {
    let (woken, wake) = WOKEN.get();
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
}
