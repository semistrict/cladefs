// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Guest memory as a front-end's memory table gives it: regions of memfds, each where it is in the
//! guest and in the front-end's address space. The back-end moves a request's buffers with
//! `pread` and `pwrite` on the regions' descriptors, and maps only the vrings' pages (plan v3,
//! board 14378): it holds no page-table entry on the rest of guest memory.
//!
//! A region is refused unless its memory is a memfd of shared memory (tmpfs: `pwrite`s work, as
//! hugetlbfs's don't), open to read and write, sealed against shrinking and not against writes, and
//! holds the region whole: a mapping of it then never faults past its end (`SIGBUS`), however the
//! front-end changes it. Every field is a multiple of the host's page size, and no two regions
//! overlap, in the guest or in the front-end.

#![deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::ptr::NonNull;

use crate::protocol::Region as Given;

/// Why a memory table, or an access through it, is refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MemoryError {
    #[error("region {0}: {1}")]
    Region(usize, &'static str),
    #[error("regions {0} and {1} overlap")]
    Overlap(usize, usize),
    #[error("{len} bytes at guest address {addr:#x}, which no regions hold whole")]
    Guest { addr: u64, len: u64 },
    #[error("{len} bytes at front-end address {addr:#x}, which no one region holds")]
    User { addr: u64, len: u64 },
    #[error("{0}")]
    Io(String),
}

/// A region of guest memory, checked.
#[derive(Debug)]
struct Region {
    guest: u64,
    size: u64,
    user: u64,
    offset: u64,
    fd: OwnedFd,
}

impl Region {
    /// The end of its range in the front-end's address space, checked as the table was made.
    fn user_end(&self) -> u64 {
        self.user.checked_add(self.size).expect("checked as the table was made")
    }
}

/// The host's page size.
pub fn page_size() -> u64 {
    // SAFETY: Reads a system value.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(size).expect("the page size is positive")
}

/// `TMPFS_MAGIC`: the file system of the memfds `memfd_create` makes without `MFD_HUGETLB`.
const TMPFS_MAGIC: i64 = 0x0102_1994;

/// `Ok` if `fd` is a memfd of shared memory, open to read and write, sealed against shrinking and
/// not against writes, of `need` bytes at least: why not otherwise.
fn check_memory(fd: BorrowedFd<'_>, need: u64) -> Result<(), &'static str> {
    let raw = fd.as_raw_fd();
    // SAFETY: Reads the status flags of a descriptor the caller holds.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags < 0 || flags & libc::O_ACCMODE != libc::O_RDWR {
        return Err("its memory is not open to read and write");
    }
    // SAFETY: Reads the seals of a descriptor the caller holds.
    let seals = unsafe { libc::fcntl(raw, libc::F_GET_SEALS) };
    if seals < 0 {
        return Err("its memory is no memfd");
    }
    if seals & libc::F_SEAL_SHRINK == 0 {
        return Err("its memory is not sealed against shrinking");
    }
    if seals & (libc::F_SEAL_WRITE | libc::F_SEAL_FUTURE_WRITE) != 0 {
        return Err("its memory is sealed against writes");
    }
    // SAFETY: A buffer the call fills.
    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: Fills `fs` in, of a descriptor the caller holds.
    #[allow(clippy::unnecessary_cast, reason = "f_type's type differs between C libraries")]
    if unsafe { libc::fstatfs(raw, &mut fs) } != 0 || fs.f_type as i64 != TMPFS_MAGIC {
        return Err("its memory is not shared memory (tmpfs)");
    }
    // SAFETY: A buffer the call fills.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: Fills `stat` in, of a descriptor the caller holds.
    if unsafe { libc::fstat(raw, &mut stat) } != 0 {
        return Err("its memory's size is unknown");
    }
    if u64::try_from(stat.st_size).unwrap_or(0) < need {
        return Err("its memory is smaller than the region");
    }
    Ok(())
}

/// A front-end's memory table, checked.
#[derive(Debug)]
pub struct MemoryTable {
    /// By guest address.
    regions: Vec<Region>,
}

impl MemoryTable {
    /// The table of `regions`, as a `SET_MEM_TABLE` gives them: refused unless each is a checked
    /// memfd's (the module's), page-aligned, and none overlaps another.
    pub fn new(regions: Vec<Given>) -> Result<Self, MemoryError> {
        let page = page_size();
        let aligned = |value: u64| value.is_multiple_of(page);
        let mut checked: Vec<Region> = Vec::with_capacity(regions.len());
        for (i, region) in regions.into_iter().enumerate() {
            let Given { guest, size, user, offset, fd } = region;
            let refuse = |why| MemoryError::Region(i, why);
            if size == 0 {
                return Err(refuse("of no bytes"));
            }
            if ![guest, size, user, offset].into_iter().all(aligned) {
                return Err(refuse("not page-aligned"));
            }
            let (Some(guest_end), Some(user_end), Some(end)) =
                (guest.checked_add(size), user.checked_add(size), offset.checked_add(size))
            else {
                return Err(refuse("ending past a u64"));
            };
            // Within a file's offsets, which `pread`'s and `mmap`'s `off_t` holds.
            if i64::try_from(end).is_err() {
                return Err(refuse("ending past an off_t in its memory"));
            }
            check_memory(fd.as_fd(), end).map_err(refuse)?;
            let overlaps = |a: u64, a_end: u64, b: u64, b_end: u64| a < b_end && b < a_end;
            for (j, other) in checked.iter().enumerate() {
                let other_guest_end = other.guest.checked_add(other.size).expect("checked");
                if overlaps(guest, guest_end, other.guest, other_guest_end)
                    || overlaps(user, user_end, other.user, other.user_end())
                {
                    return Err(MemoryError::Overlap(j, i));
                }
            }
            checked.push(Region { guest, size, user, offset, fd });
        }
        checked.sort_by_key(|region| region.guest);
        Ok(Self { regions: checked })
    }

    /// The region holding guest address `addr`, and `addr`'s offset in it.
    fn at(&self, addr: u64) -> Option<(&Region, u64)> {
        let i = self.regions.partition_point(|region| region.guest <= addr).checked_sub(1)?;
        let region = self.regions.get(i)?;
        let within = addr.checked_sub(region.guest)?;
        (within < region.size).then_some((region, within))
    }

    /// The pieces of the `len` bytes at guest address `addr`, each within a region: its memory, the
    /// offset there, and its length; refused unless regions hold every byte.
    fn pieces(&self, addr: u64, len: u64) -> Result<Vec<(BorrowedFd<'_>, u64, u64)>, MemoryError> {
        let refused = || MemoryError::Guest { addr, len };
        addr.checked_add(len).ok_or_else(refused)?;
        let mut pieces = Vec::new();
        let (mut at, mut left) = (addr, len);
        while left > 0 {
            let (region, within) = self.at(at).ok_or_else(refused)?;
            let piece = region.size.checked_sub(within).ok_or_else(refused)?.min(left);
            let offset = region.offset.checked_add(within).ok_or_else(refused)?;
            pieces.push((region.fd.as_fd(), offset, piece));
            at = at.checked_add(piece).ok_or_else(refused)?;
            left = left.checked_sub(piece).ok_or_else(refused)?;
        }
        Ok(pieces)
    }

    /// `Ok` if regions hold every one of the `len` bytes at guest address `addr`.
    pub fn check(&self, addr: u64, len: u64) -> Result<(), MemoryError> {
        self.pieces(addr, len).map(drop)
    }

    /// Reads the bytes at guest address `addr` into `buf`, with `pread`.
    pub fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), MemoryError> {
        let mut rest = buf;
        for (fd, offset, len) in self.pieces(addr, rest.len() as u64)? {
            let len = usize::try_from(len).expect("within the buffer");
            let (part, after) = rest.split_at_mut_checked(len).expect("within the buffer");
            transfer(fd, offset, part.as_mut_ptr(), len, false)?;
            rest = after;
        }
        Ok(())
    }

    /// Writes `buf` at guest address `addr`, with `pwrite`.
    pub fn write(&self, addr: u64, buf: &[u8]) -> Result<(), MemoryError> {
        let mut rest = buf;
        for (fd, offset, len) in self.pieces(addr, rest.len() as u64)? {
            let len = usize::try_from(len).expect("within the buffer");
            let (part, after) = rest.split_at_checked(len).expect("within the buffer");
            // Only read from: `pwrite` reads the buffer.
            transfer(fd, offset, part.as_ptr().cast_mut(), len, true)?;
            rest = after;
        }
        Ok(())
    }

    /// Maps the `len` bytes at front-end address `addr`, which one region holds, writable if
    /// `writable`: a vring's part.
    pub fn map_user(&self, addr: u64, len: u64, writable: bool) -> Result<Mapping, MemoryError> {
        let refused = || MemoryError::User { addr, len };
        let end = addr.checked_add(len).ok_or_else(refused)?;
        let region = self
            .regions
            .iter()
            .find(|region| region.user <= addr && end <= region.user_end())
            .ok_or_else(refused)?;
        let page = page_size();
        // Within the region, whose fields are page-aligned and whose ends are checked.
        let within = addr.checked_sub(region.user).ok_or_else(refused)?;
        let first = within.checked_rem(page).and_then(|rem| within.checked_sub(rem));
        let first = first.ok_or_else(refused)?;
        let last = within
            .checked_add(len)
            .map(|end| end.div_ceil(page))
            .and_then(|pages| pages.checked_mul(page))
            .ok_or_else(refused)?;
        let file = region.offset.checked_add(first).ok_or_else(refused)?;
        let skip = within.checked_sub(first).ok_or_else(refused)?;
        let map_len = last.checked_sub(first).ok_or_else(refused)?;
        Mapping::new(region.fd.as_fd(), file, map_len, skip, writable)
    }
}

/// Moves `len` bytes between `buf` and `fd` at `offset`, writing to `fd` if `write`: as many calls
/// as it takes; a call moving nothing fails it.
fn transfer(
    fd: BorrowedFd<'_>,
    mut offset: u64,
    mut buf: *mut u8,
    mut len: usize,
    write: bool,
) -> Result<(), MemoryError> {
    while len > 0 {
        let at =
            libc::off_t::try_from(offset).map_err(|_| MemoryError::Io("past an off_t".into()))?;
        let moved = if write {
            // SAFETY: `len` bytes of the caller's buffer, which `pwrite` only reads.
            unsafe { libc::pwrite(fd.as_raw_fd(), buf.cast(), len, at) }
        } else {
            // SAFETY: `len` bytes of the caller's buffer.
            unsafe { libc::pread(fd.as_raw_fd(), buf.cast(), len, at) }
        };
        if moved < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(MemoryError::Io(format!("guest memory: {err}")));
        }
        let moved = usize::try_from(moved).expect("not negative");
        if moved == 0 {
            return Err(MemoryError::Io("guest memory ends short of a region".into()));
        }
        len = len.checked_sub(moved).expect("at most what was asked");
        offset = offset.checked_add(moved as u64).expect("within the region");
        // SAFETY: Within the buffer: `moved` of its bytes left.
        buf = unsafe { buf.add(moved) };
    }
    Ok(())
}

/// Pages of guest memory this process maps: a vring's part, from where its bytes begin.
#[derive(Debug)]
pub struct Mapping {
    base: NonNull<u8>,
    len: usize,
    skip: usize,
}

// SAFETY: A mapping of shared memory, which any thread accesses through atomics alone.
unsafe impl Send for Mapping {}
// SAFETY: As above.
unsafe impl Sync for Mapping {}

impl Mapping {
    fn new(
        fd: BorrowedFd<'_>,
        offset: u64,
        len: u64,
        skip: u64,
        writable: bool,
    ) -> Result<Self, MemoryError> {
        let failed = |why: &str| MemoryError::Io(why.into());
        let len = usize::try_from(len).map_err(|_| failed("a mapping past a usize"))?;
        let skip = usize::try_from(skip).map_err(|_| failed("a mapping past a usize"))?;
        let offset =
            libc::off_t::try_from(offset).map_err(|_| failed("a mapping past an off_t"))?;
        let prot = libc::PROT_READ | if writable { libc::PROT_WRITE } else { 0 };
        // SAFETY: Maps pages of a memfd the table holds, sealed against shrinking, within its size
        // (`check_memory`): no access through it faults past the file's end.
        let base = unsafe {
            libc::mmap(std::ptr::null_mut(), len, prot, libc::MAP_SHARED, fd.as_raw_fd(), offset)
        };
        if base == libc::MAP_FAILED {
            let err = std::io::Error::last_os_error();
            return Err(MemoryError::Io(format!("mapping a vring: {err}")));
        }
        let base = NonNull::new(base.cast()).expect("mmap gives no null mapping");
        Ok(Self { base, len, skip })
    }

    /// Where the bytes asked for begin.
    pub fn ptr(&self) -> *mut u8 {
        // SAFETY: Within the mapping (`MemoryTable::map_user`).
        unsafe { self.base.as_ptr().add(self.skip) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: Unmaps the mapping, which nothing uses any more: its rings are gone with it.
        unsafe { libc::munmap(self.base.as_ptr().cast(), self.len) };
    }
}
