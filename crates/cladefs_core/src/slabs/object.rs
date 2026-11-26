// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! An arena's memory object: a memfd on Linux; on macOS, development only, an unlinked file of the
//! process's temporary directory (doc-26, macOS). The daemon maps none of it: it reads and writes
//! it by offset, and punches out what it frees. Only VM processes, and windows in this process,
//! map it.

use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::FromRawFd;
use std::os::fd::{AsRawFd, OwnedFd};

/// Kernel page-cache residency counts for a slab's range of an arena's `memfd`, from Linux's
/// `cachestat(2)` syscall (Linux 6.5+). For `shmem` (`memfd`), pages aged out by MGLRU into swap
/// entries appear in `evicted_pages`, letting `cladefsd` harvest cold slabs without touching hot
/// working sets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SlabResidency {
    /// Pages currently resident in host RAM.
    pub cached_pages: u64,
    /// Pages evicted from RAM (for `memfd`, currently stored in shmem swap entries).
    pub evicted_pages: u64,
    /// Subset of `evicted_pages` still inside MGLRU's workingset refault window.
    pub recently_evicted_pages: u64,
}

/// An arena's memory object (see the module docs). It only grows.
pub(super) struct Object {
    fd: std::sync::Arc<OwnedFd>,
}

impl Object {
    /// A new object, of no bytes. cladefs: the failpoint `slabs::memory_object` fails it as a
    /// process with as many files open as it may would (`EMFILE`), for tests (board 8491).
    pub(super) fn new() -> io::Result<Self> {
        fail::fail_point!("slabs::memory_object", |_| Err(io::Error::from_raw_os_error(
            libc::EMFILE
        )));
        Ok(Self { fd: std::sync::Arc::new(create()?) })
    }

    /// Its descriptor, for mapping its pages elsewhere.
    pub(super) fn fd(&self) -> &std::sync::Arc<OwnedFd> {
        &self.fd
    }

    /// Sizes it to `size` bytes at least. It only grows, sealed against shrinking on Linux
    /// ([`create`]): a process it was handed to that may write it may have grown it past that
    /// already, and may as it is grown here (TASK-149.17).
    pub(super) fn grow(&self, size: u64) -> io::Result<()> {
        let size =
            libc::off_t::try_from(size).map_err(|_| io::Error::from_raw_os_error(libc::EFBIG))?;
        loop {
            // SAFETY: a stat buffer the call fills, of an open descriptor of this object's.
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: as above.
            if unsafe { libc::fstat(self.fd.as_raw_fd(), &mut stat) } != 0 {
                return Err(io::Error::last_os_error());
            }
            if stat.st_size >= size {
                return Ok(());
            }
            // SAFETY: sizes an object of this one's.
            if unsafe { libc::ftruncate(self.fd.as_raw_fd(), size) } == 0 {
                return Ok(());
            }
            // Grown past `size` since it was looked at: a shrink, which the seal refuses.
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EPERM) {
                return Err(err);
            }
        }
    }

    /// Writes `data` at `offset`, whole.
    pub(super) fn write_at(&self, mut offset: u64, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            // SAFETY: writes `data`'s bytes, which it holds, to an open descriptor.
            let wrote = unsafe {
                libc::pwrite(
                    self.fd.as_raw_fd(),
                    data.as_ptr().cast(),
                    data.len(),
                    offset as libc::off_t,
                )
            };
            match wrote {
                -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => continue,
                -1 => return Err(io::Error::last_os_error()),
                0 => return Err(io::Error::from(io::ErrorKind::WriteZero)),
                wrote => {
                    data = &data[wrote as usize..];
                    offset += wrote as u64;
                }
            }
        }
        Ok(())
    }

    /// Reads `buf.len()` bytes at `offset` into `buf`: within its size, as its runs are.
    pub(super) fn read_at(&self, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
        while !buf.is_empty() {
            // SAFETY: reads into `buf`'s bytes, which it holds, from an open descriptor.
            let read = unsafe {
                libc::pread(
                    self.fd.as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    offset as libc::off_t,
                )
            };
            match read {
                -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => continue,
                -1 => return Err(io::Error::last_os_error()),
                0 => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
                read => {
                    buf = &mut buf[read as usize..];
                    offset += read as u64;
                }
            }
        }
        Ok(())
    }

    /// Copies `len` bytes at `from` to `to`'s at `at`, in the kernel where it can
    /// (`copy_file_range` on Linux: no bounce through this process's memory, TASK-234).
    pub(super) fn copy_to(&self, from: u64, to: &Object, at: u64, len: u64) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            let (mut from, mut at, mut left) =
                (from as libc::loff_t, at as libc::loff_t, len as usize);
            while left > 0 {
                // SAFETY: copies between two open descriptors, the offsets the call's to advance.
                let copied = unsafe {
                    libc::copy_file_range(
                        self.fd.as_raw_fd(),
                        &mut from,
                        to.fd.as_raw_fd(),
                        &mut at,
                        left,
                        0,
                    )
                };
                match copied {
                    -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
                    -1 => return Err(io::Error::last_os_error()),
                    0 => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
                    copied => left -= copied as usize,
                }
            }
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let mut bytes = vec![0; len as usize];
            self.read_at(from, &mut bytes)?;
            to.write_at(at, &bytes)
        }
    }

    /// Gives the memory of the `len` bytes at `offset` back: they read as zeros after.
    pub(super) fn punch(&self, offset: u64, len: u64) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        // SAFETY: punches a range of an open descriptor's object, keeping its size.
        let punched = unsafe {
            libc::fallocate(
                self.fd.as_raw_fd(),
                libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                offset as libc::off_t,
                len as libc::off_t,
            )
        };
        #[cfg(target_os = "macos")]
        let punched = {
            let hole = libc::fpunchhole_t {
                fp_flags: 0,
                reserved: 0,
                fp_offset: offset as libc::off_t,
                fp_length: len as libc::off_t,
            };
            // SAFETY: punches a range of an open descriptor's file, described by `hole`.
            unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_PUNCHHOLE, &hole) }
        };
        if punched != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Queries kernel page-cache residency (`cachestat(2)`, Linux 6.5+) for `[offset, offset + len)`
    /// on an arena `memfd` descriptor. Returns `None` on non-Linux hosts, under `madsim`, or on
    /// kernels without `SYS_cachestat`.
    #[cfg(all(target_os = "linux", not(madsim)))]
    pub(super) fn cachestat_fd(fd: &OwnedFd, offset: u64, len: u64) -> Option<SlabResidency> {
        use std::sync::atomic::{AtomicBool, Ordering};
        // Process-wide fact of the hardware/kernel (outside the simulation): whether SYS_cachestat
        // (Linux 6.5, syscall 451 on x86_64 and aarch64) is implemented by this host's kernel.
        static UNSUPPORTED: AtomicBool = AtomicBool::new(false);
        if UNSUPPORTED.load(Ordering::Relaxed) || len == 0 {
            return None;
        }
        #[repr(C)]
        struct CachestatRange {
            off: u64,
            len: u64,
        }
        #[repr(C)]
        struct Cachestat {
            nr_cache: u64,
            nr_dirty: u64,
            nr_writeback: u64,
            nr_evicted: u64,
            nr_recently_evicted: u64,
        }
        // __NR_cachestat is 451 in both x86_64 syscall_64.tbl and asm-generic/unistd.h (aarch64).
        const SYS_CACHESTAT: libc::c_long = 451;
        let range = CachestatRange { off: offset, len };
        let mut out = Cachestat {
            nr_cache: 0,
            nr_dirty: 0,
            nr_writeback: 0,
            nr_evicted: 0,
            nr_recently_evicted: 0,
        };
        // SAFETY: `range` and `out` are valid C-layout structs for `cachestat(2)` on this open fd.
        let ret = unsafe {
            libc::syscall(SYS_CACHESTAT, fd.as_raw_fd(), &raw const range, &raw mut out, 0usize)
        };
        if ret != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENOSYS) {
                UNSUPPORTED.store(true, Ordering::Relaxed);
            }
            return None;
        }
        Some(SlabResidency {
            cached_pages: out.nr_cache,
            evicted_pages: out.nr_evicted,
            recently_evicted_pages: out.nr_recently_evicted,
        })
    }

    #[cfg(all(target_os = "linux", not(madsim)))]
    pub(super) fn cachestat(&self, offset: u64, len: u64) -> Option<SlabResidency> {
        Self::cachestat_fd(&self.fd, offset, len)
    }

    /// Queries kernel page-cache residency (`None` on non-Linux hosts or under `madsim`).
    #[cfg(any(not(target_os = "linux"), madsim))]
    #[allow(dead_code)]
    pub(super) fn cachestat_fd(_fd: &OwnedFd, _offset: u64, _len: u64) -> Option<SlabResidency> {
        None
    }

    /// Queries kernel page-cache residency (`None` on non-Linux hosts or under `madsim`).
    #[cfg(any(not(target_os = "linux"), madsim))]
    #[allow(dead_code)]
    pub(super) fn cachestat(&self, _offset: u64, _len: u64) -> Option<SlabResidency> {
        None
    }

    /// Probes `SYS_cachestat` on a temporary 1-page `memfd` at startup so governed daemons fail
    /// hard on kernels lacking Linux 6.5+ `cachestat(2)` instead of silently disabling MGLRU
    /// cold-slab harvesting.
    #[cfg(all(target_os = "linux", not(madsim)))]
    pub(super) fn require_cachestat() -> io::Result<()> {
        let obj = Self::new()?;
        let ps = super::page_size();
        obj.grow(ps)?;
        obj.write_at(0, &[1u8])?;
        if obj.cachestat(0, ps).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "host kernel lacks cachestat(2) (Linux 6.5+ required for unified MGLRU slab harvesting)",
            ));
        }
        Ok(())
    }

    /// The bytes of memory it holds, as the host counts them: its blocks.
    #[cfg(all(test, target_os = "linux", not(feature = "shuttle")))]
    pub(super) fn held(&self) -> io::Result<u64> {
        // SAFETY: a stat buffer the call fills, of an open descriptor of this object's.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        if unsafe { libc::fstat(self.fd.as_raw_fd(), &mut stat) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(stat.st_blocks as u64 * 512)
    }
}

/// Creates an arena's memory object, of no bytes.
///
/// As the processes it is handed to may be hostile, a VM process escaped from its guest
/// (TASK-149.17): sealed against shrinking, which one handed it to write could otherwise do; and
/// its owner's alone to open anew (0600, where a memfd is anyone's), so that one handed it
/// read-only can't reopen it to write through `/proc/self/fd`. It only grows ([`Object::grow`]).
/// And sealed against more seals, which one handed it could have the extent daemon add, given back
/// as what it seals (a transfer area, against writes, TASK-547), the daemon's punching its freed
/// runs out then refused.
#[cfg(target_os = "linux")]
fn create() -> io::Result<OwnedFd> {
    let flags = libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING;
    // SAFETY: The name is a valid C string.
    let fd = unsafe { libc::memfd_create(c"cladefs-physical-memory".as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just created and is owned here.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    // SAFETY: seals the object just made.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, seals) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: modes the object just made.
    if unsafe { libc::fchmod(fd.as_raw_fd(), 0o600) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// Creates an arena's memory object, of no bytes: a file of the process's temporary directory,
/// 0600, unlinked as soon as it is open, which `pread`, `pwrite` and `F_PUNCHHOLE` serve, as
/// `shm_open`'s objects serve neither. Its pages are the host's page cache: development only, where
/// no governed budget is measured (doc-26).
#[cfg(target_os = "macos")]
fn create() -> io::Result<OwnedFd> {
    // An arena's memory, unlinked as made, no file of the host's disk the daemon keeps, as a memfd
    // is: outside the simulation, here and below.
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU32, Ordering};
    // Numbers the process gives out: unique among simulated hosts too, all it asks.
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let name =
        format!("cladefs-arena-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
    let path = std::env::temp_dir().join(name);
    // Made and unlinked outside the simulation, as above.
    let file = (std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600))
        .open(&path)?;
    // Unlinked outside the simulation, as above.
    std::fs::remove_file(&path)?;
    Ok(OwnedFd::from(file))
}
