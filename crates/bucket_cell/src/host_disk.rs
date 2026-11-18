// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a filesystem the host keeps stores or the part cache on, as the disk governor sees it
//! (backlog doc-22, TASK-539): what the daemon's stores and its part cache hold on it, counted as
//! they write, and what the governor allows them, sampled from the filesystem itself. One per
//! filesystem, however many directories of it are opened ([`HostDisk::of`]): every store and the
//! part cache on it charge the same counters, which the governor weighs against its free space.
//!
//! What is counted is lengths, each file's rounded up to [`GRANULE`], as written: deterministic,
//! alike on every filesystem and in memory, and known before the filesystem's own block counts
//! catch up (ZFS reports a write's blocks only once its transaction group syncs, TASK-355). A
//! store's directory in memory has a disk of its own, of a capacity a test gives it, which refuses
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

/// How a disk is sampled: a host's filesystem through a descriptor of a directory of it, or a
/// disk in memory, by what its owner says of it.
enum Sampler {
    Fs { dev: u64, dir: std::fs::File },
    Memory(Box<dyn Fn() -> Sample + Send + Sync>),
}

/// A filesystem the daemon keeps stores or the part cache on. See the module docs.
pub struct HostDisk {
    sampler: Sampler,
    /// What the stores on it hold: their files' lengths, counted ([`counted`]), each file once
    /// however many names it has, and their directories, a [`GRANULE`] each.
    pub stores: Count,
    /// Of `stores`, the stores' raw files whose framing is still to come: open segments' and open
    /// extents', which the stores' own work, never refused, may add to as it frames them.
    pub unframed: Count,
    /// What writes admitted and not yet landed hold.
    pub pledged: Count,
    /// What the part cache on it holds, its fills in flight included.
    pub cache: Count,
    /// Of `cache`, its fills in flight, counted but not yet written.
    pub cache_writing: Count,
    /// What the governor allows the daemon to keep on it, stores and part cache together:
    /// `u64::MAX` while no governor looks at it.
    allowance: AtomicU64,
    /// What the governor leaves free of it.
    headroom: AtomicU64,
    /// Whether it is under pressure: its spare under its headroom.
    pressure: AtomicBool,
}

/// The host's filesystems' disks alive, so that two directories of one filesystem share one.
static DISKS: Mutex<Vec<Weak<HostDisk>>> = Mutex::new(Vec::new());

impl HostDisk {
    /// The disk of the filesystem `path` is on: the one alive already, if any is.
    pub fn of(path: &Path) -> io::Result<Arc<Self>> {
        use std::os::unix::fs::MetadataExt as _;
        let dev = std::fs::metadata(path)?.dev();
        let mut disks = DISKS.lock().unwrap();
        disks.retain(|disk| disk.strong_count() > 0);
        let found = disks.iter().filter_map(Weak::upgrade).find(|disk| disk.dev() == Some(dev));
        if let Some(disk) = found {
            return Ok(disk);
        }
        let dir = std::fs::File::open(path)?;
        let disk = Arc::new(Self::with(Sampler::Fs { dev, dir }));
        disks.push(Arc::downgrade(&disk));
        Ok(disk)
    }

    /// A disk in memory, which `sample` says the space of: a store's directory in memory's
    /// (`local_cell::MemDisk`). It is no host's filesystem: [`HostDisk::all`] leaves it out.
    pub fn in_memory(sample: impl Fn() -> Sample + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self::with(Sampler::Memory(Box::new(sample))))
    }

    fn with(sampler: Sampler) -> Self {
        Self {
            sampler,
            stores: Count::default(),
            unframed: Count::default(),
            pledged: Count::default(),
            cache: Count::default(),
            cache_writing: Count::default(),
            allowance: AtomicU64::new(u64::MAX),
            headroom: AtomicU64::new(0),
            pressure: AtomicBool::new(false),
        }
    }

    /// The host's filesystems' disks alive.
    pub fn all() -> Vec<Arc<Self>> {
        DISKS.lock().unwrap().iter().filter_map(Weak::upgrade).collect()
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

    /// What the daemon keeps on it: its stores' and its part cache's.
    pub fn held(&self) -> u64 {
        self.stores.get() + self.cache.get()
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
            .field("stores", &self.stores)
            .field("unframed", &self.unframed)
            .field("pledged", &self.pledged)
            .field("cache", &self.cache)
            .field("allowance", &self.allowance())
            .finish_non_exhaustive()
    }
}

/// The space of the filesystem `dir`, a descriptor of a directory of it, is on.
fn fs_sample(dir: &std::fs::File) -> io::Result<Sample> {
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

/// Whether the governor is to look again at once, and what tells it so.
static WOKEN: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());

/// Has the governor look at the disks again at once: a write refused, or the part cache past what
/// the stores leave it.
pub fn wake_governor() {
    let (woken, wake) = &WOKEN;
    *woken.lock().unwrap() = true;
    wake.notify_all();
}

/// Waits, the governor, until it is woken ([`wake_governor`]) or `most` has passed.
pub fn wait_for_governor(most: Duration) {
    let (woken, wake) = &WOKEN;
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
        std::fs::create_dir(dir.path().join("a")).unwrap();
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

    /// A disk in memory is what its owner says, ungoverned until a governor looks at it, and no
    /// host's.
    #[test]
    fn a_disk_in_memory_is_what_its_owner_says() {
        let disk = HostDisk::in_memory(|| Sample { size: 100, free: 60, used: 40 });
        assert_eq!(disk.sample().unwrap(), Sample { size: 100, free: 60, used: 40 });
        assert_eq!((disk.dev(), disk.allowance(), disk.under_pressure()), (None, u64::MAX, false));
        assert!(!HostDisk::all().iter().any(|alive| Arc::ptr_eq(alive, &disk)));
        disk.stores.add(3 * GRANULE);
        disk.cache.add(GRANULE);
        disk.stores.sub(GRANULE);
        assert_eq!(disk.held(), 3 * GRANULE);
        disk.govern(50, 10, true);
        assert_eq!((disk.allowance(), disk.headroom(), disk.under_pressure()), (50, 10, true));
    }

    #[test]
    fn lengths_count_in_granules() {
        assert_eq!([0, 1, GRANULE, GRANULE + 1].map(counted), [0, GRANULE, GRANULE, 2 * GRANULE]);
    }
}
