// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the checks of copying on write (doc-13): private pages, their writeback, and rebinds
//! under loads and writes.

use super::*;

/// cladefs: a shared, writable mapping of `len` bytes of `file`, unmapped as it drops.
struct CowMapping {
    addr: *mut libc::c_void,
    len: usize,
}

impl CowMapping {
    fn new(file: &fs::File, len: usize) -> Self {
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(addr, libc::MAP_FAILED, "mmap: {}", std::io::Error::last_os_error());
        Self { addr, len }
    }

    fn bytes(&self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.addr.cast::<u8>(), self.len) }
    }

    fn msync(&self) {
        assert_eq!(unsafe { libc::msync(self.addr, self.len, libc::MS_SYNC) }, 0);
    }

    /// cladefs: mprotect of all of it to `prot`.
    fn protect(&self, prot: libc::c_int) {
        let protected = unsafe { libc::mprotect(self.addr, self.len, prot) };
        assert_eq!(protected, 0, "mprotect: {}", std::io::Error::last_os_error());
    }

    /// cladefs: moves it (mremap), its page tables with it, to where a reservation of its size was.
    fn moved(&mut self) {
        let prot = libc::PROT_NONE;
        let flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
        let to = unsafe { libc::mmap(std::ptr::null_mut(), self.len, prot, flags, -1, 0) };
        assert_ne!(to, libc::MAP_FAILED, "mmap: {}", std::io::Error::last_os_error());
        let flags = libc::MREMAP_MAYMOVE | libc::MREMAP_FIXED;
        let moved = unsafe { libc::mremap(self.addr, self.len, self.len, flags, to) };
        assert_eq!(moved, to, "mremap: {}", std::io::Error::last_os_error());
        self.addr = moved;
    }
}

impl Drop for CowMapping {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.addr, self.len) };
    }
}

/// cladefs: has the budget's worker write back, seal and rebind every private page it can
/// (cladefs.dax_rebind_now), and waits for it: until the rounds it served (dax_rebinds_done)
/// reach the asks, as read after this one (dax_rebinds_asked). The write doesn't wait: a
/// setter that did held the module parameters' lock through a rebind a pause version held.
fn cow_rebind_now() {
    fs::write("/sys/module/cladefs/parameters/dax_rebind_now", b"1").unwrap();
    let asked = stats()["dax_rebinds_asked"];
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while stats()["dax_rebinds_done"] < asked {
        assert!(std::time::Instant::now() < deadline, "rebind {asked} never done");
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// cladefs: copying on write (doc-13, TASK-164.2), a file read in from sealed extents, DAX, stored
/// to through a shared mapping and written with write(2): the bytes reach its new extents by
/// writeback (msync, fsync), committed after them, and are there after a remount, which maps
/// the file again from Fxfs. Fails if writeback wrote the extents' old pages (no copy from the
/// window, or the copy before the writes), if write(2)'s pages never joined the dirty set (no
/// writepages comes for them: fsync writes them back), or if a rebind mapped a page back to its
/// old extent before its new one was sealed.
pub(super) fn test_cow_write_persists() {
    const PAGE: usize = 4096;
    let name = path("cow-write");
    let mut expected = cow_base_file(&name, 16 * PAGE, 0x11);
    let file = fs::OpenOptions::new().read(true).write(true).open(&name).unwrap();
    let map = CowMapping::new(&file, 16 * PAGE);
    map.bytes()[3 * PAGE + 10..3 * PAGE + 21].copy_from_slice(b"stored once");
    expected[3 * PAGE + 10..3 * PAGE + 21].copy_from_slice(b"stored once");
    map.msync();
    // Stored again after the writeback: the page faults writable again, dirty again.
    map.bytes()[3 * PAGE + 100] = 0xee;
    expected[3 * PAGE + 100] = 0xee;
    std::os::unix::fs::FileExt::write_all_at(&file, &[0x5a; 2 * PAGE], 7 * PAGE as u64).unwrap();
    expected[7 * PAGE..9 * PAGE].fill(0x5a);
    // What the window shows, the writes' and the write's, before any writeback.
    assert!(map.bytes()[..] == expected[..], "the mapping before fsync");
    file.sync_all().unwrap();
    // Rebound to the new extents, the window shows the same bytes.
    cow_rebind_now();
    assert!(map.bytes()[..] == expected[..], "the mapping after rebinding");
    assert_eq!(stats()["dax_open_maps"], 0, "the window was asked to map an open extent");
    drop(map);
    drop(file);
    unmount();
    mount();
    assert!(is_dax(&name), "{name} isn't DAX after a remount");
    assert!(fs::read(&name).unwrap() == expected, "{name} after a remount");
    fs::remove_file(&name).unwrap();
}

/// cladefs: a file of two chunks read in sealed, DAX, mapped shared, each chunk read through the
/// mapping 2 MiB at once, so that its window's pages are one large folio, then a write to the
/// second page, which splits the first chunk's PMD, and the file read again, page by page: each
/// page's PTE read-only but the one stored to, all of one folio (TASK-149.16, TASK-149.42).
struct SplitChunk {
    name: String,
    file: fs::File,
    map: CowMapping,
    data: Vec<u8>,
    /// The private pages after the write.
    private: u64,
}

impl SplitChunk {
    const PAGE: usize = 4096;
    const SIZE: usize = 4 << 20;
    /// A page the write left read-only, as r861's child stored to.
    const READ_ONLY: usize = 435 * Self::PAGE + 0x9bf;

    fn new(name: &str) -> Self {
        let name = path(name);
        let mut data: Vec<u8> = (0..Self::SIZE).map(|i| (i % 251) as u8 + 1).collect();
        fs::write(&name, &data).unwrap();
        read_in_sealed(&name);
        let file = fs::OpenOptions::new().read(true).write(true).open(&name).unwrap();
        let map = CowMapping::new(&file, Self::SIZE);
        let huge = stats()["dax_huge_maps"];
        assert!(map.bytes()[..] == data[..], "the file as mapped differs from what was written");
        assert!(stats()["dax_huge_maps"] > huge, "no fault mapped a PMD's 2 MiB: {:?}", stats());
        let before = Self::private();
        map.bytes()[Self::PAGE + 100] = 0xee;
        data[Self::PAGE + 100] = 0xee;
        assert_eq!(Self::private(), before + 1, "private pages after a write to a PMD's page");
        // Page by page now, the PMD split: each page's PTE read-only, but the one stored to.
        assert!(map.bytes()[..] == data[..], "the file read again, page by page");
        Self { name, file, map, data, private: before + 1 }
    }

    fn private() -> u64 {
        stats()["dax_private_pages"]
    }

    /// Writes to the page at `offset` left read-only through the mapping, as it is now, from this
    /// process: the write must fault, which makes the page private.
    fn write_to_read_only_at(&mut self, offset: usize, how: &str) {
        self.map.bytes()[offset] = 0xcd;
        self.data[offset] = 0xcd;
        self.private += 1;
        assert_eq!(Self::private(), self.private, "private pages after the write {how}");
    }

    /// Writes to the page left read-only through the mapping, as it is now, from this process:
    /// the write must fault, which makes the page private.
    fn write_to_read_only(&mut self, how: &str) {
        self.write_to_read_only_at(Self::READ_ONLY, how);
    }

    /// The mapping holds what was stored, and so does the file, evicted.
    fn finish(self) {
        let Self { name, file, map, data, .. } = self;
        assert!(map.bytes()[..] == data[..], "the mapping after the writes");
        drop(map);
        drop(file);
        evict(&name);
        assert!(fs::read(&name).unwrap() == data, "{name} evicted");
        fs::remove_file(&name).unwrap();
    }
}

/// cladefs: copying on write (doc-13), a fork after a chunk mapped whole and split (SplitChunk):
/// the child's write to a page it only read faults, which makes the page private, as the parent's
/// would (TASK-149.16). Fork copies a large folio's PTEs as one, all writable if any is (6.18's
/// copy_present_ptes, FPB_MERGE_WRITE), of a mapping it copies at all: a VM_MIXEDMAP one, as Fxfs's
/// were. The child stored, unfaulted, to a page no PRIVATE had made the VM's, which the host
/// refused, again and again, till the VM went down (GCE r861, fxfs-guest-fsx-faults-dax; r866).
pub(super) fn test_cow_fork_after_a_split_pmd() {
    let mut chunk = SplitChunk::new("cow-fork-after-split");
    let child = unsafe { libc::fork() };
    assert!(child >= 0, "fork: {}", std::io::Error::last_os_error());
    if child == 0 {
        unsafe {
            (chunk.map.addr.cast::<u8>().add(SplitChunk::READ_ONLY)).write_volatile(0xcd);
            libc::_exit(0);
        }
    }
    let status = wait_for_child(child, std::time::Duration::from_secs(10));
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0, "the child: {status:#x}");
    chunk.data[SplitChunk::READ_ONLY] = 0xcd;
    assert_eq!(SplitChunk::private(), chunk.private + 1, "private pages after the child's write");
    chunk.finish();
}

/// cladefs: copying on write (doc-13), the mapping of a chunk mapped whole and split (SplitChunk)
/// made read-only (mprotect) and writable again: a write to a page only read faults, which makes
/// the page private (TASK-149.42, board 14407). Checks:
/// - 2-step `PROT_READ` -> `PROT_READ | PROT_WRITE`;
/// - 3-step `PROT_READ` -> `PROT_NONE` -> `PROT_READ | PROT_WRITE` (where an intermediate
///   non-writable mprotect smeared dirty bits across clean siblings of a compound folio);
/// - dirtying page 15 (the tail of a 16-PTE / 64 KiB arm64 contpte block) then toggling
///   `PROT_READ` -> `PROT_READ | PROT_WRITE` and storing to clean sibling page 12.
pub(super) fn test_cow_mprotect_after_a_split_pmd() {
    let mut chunk = SplitChunk::new("cow-mprotect-after-split");
    chunk.map.protect(libc::PROT_READ);
    chunk.map.protect(libc::PROT_READ | libc::PROT_WRITE);
    chunk.write_to_read_only("after 2-step mprotect");

    // 3-step mprotect: dirty page 15 (arm64 contpte block tail), toggle PROT_READ -> PROT_NONE ->
    // PROT_READ|PROT_WRITE, then store to clean sibling page 12 of the same 16-PTE block and to a
    // page in another block.
    chunk.write_to_read_only_at(15 * SplitChunk::PAGE + 7, "dirtying contpte block tail");
    chunk.map.protect(libc::PROT_READ);
    chunk.map.protect(libc::PROT_NONE);
    chunk.map.protect(libc::PROT_READ | libc::PROT_WRITE);
    chunk.write_to_read_only_at(
        12 * SplitChunk::PAGE + 7,
        "after 3-step mprotect (contpte sibling)",
    );
    chunk.write_to_read_only_at(200 * SplitChunk::PAGE + 7, "after 3-step mprotect");
    chunk.finish();
}

/// cladefs: a file read in sealed, DAX, mapped shared, page 3 stored to, then written back, sealed
/// and rebound by the budget (cladefs.dax_rebind_now), every page private no more; `toggle` makes
/// the mapping read-only, then writable again; then page 3 is stored to again, which must fault and
/// make the page private (TASK-558). The kernel makes a shared mapping's PTE writable with no fault
/// where it is dirty (can_change_shared_pte_writable), from mprotect and from a userfaultfd's
/// unprotect alike: with writeback's write-protect alone (wp_shared_mapping_range), which leaves it
/// dirty, the store went to the window page the rebind mapped the sealed extent's at, which the
/// host copied unknown to the guest or refused, the VM stopping, and the file, evicted, lacked it.
fn cow_store_after_toggle(name: &str, toggle: impl FnOnce(&CowMapping)) {
    const PAGE: usize = 4096;
    const LEN: usize = 16 * PAGE;
    let name = path(name);
    let mut expected = cow_base_file(&name, LEN, 0x3c);
    let file = fs::OpenOptions::new().read(true).write(true).open(&name).unwrap();
    let map = CowMapping::new(&file, LEN);
    map.bytes()[3 * PAGE + 10] = 0x01;
    expected[3 * PAGE + 10] = 0x01;
    cow_rebind_now();
    let private = stats()["dax_private_pages"];
    toggle(&map);
    map.bytes()[3 * PAGE + 20] = 0x02;
    expected[3 * PAGE + 20] = 0x02;
    assert_eq!(stats()["dax_private_pages"], private + 1, "private pages after the store");
    assert!(map.bytes()[..] == expected[..], "the mapping after the store");
    drop(map);
    drop(file);
    evict(&name);
    assert!(fs::read(&name).unwrap() == expected, "{name} evicted");
    fs::remove_file(&name).unwrap();
}

/// cladefs: [`cow_store_after_toggle`] with mprotect, read-only then writable again (TASK-558).
pub(super) fn test_cow_mprotect_after_budget_rebind() {
    cow_store_after_toggle("cow-mprotect-after-rebind", |map| {
        map.protect(libc::PROT_READ);
        map.protect(libc::PROT_READ | libc::PROT_WRITE);
    });
}

/// cladefs: [`cow_store_after_toggle`] with a userfaultfd's write-protect and unprotect
/// (UFFDIO_WRITEPROTECT), asynchronous (UFFD_FEATURE_WP_ASYNC), the one way a file's mapping takes
/// one (vma_can_userfault): the unprotect makes a dirty PTE writable as mprotect does
/// (uffd_wp_range's MM_CP_TRY_CHANGE_WRITABLE, TASK-558).
pub(super) fn test_cow_uffd_after_budget_rebind() {
    cow_store_after_toggle("cow-uffd-after-rebind", |map| {
        let uffd = Uffd::write_protecting(map);
        uffd.protect(map, true);
        uffd.protect(map, false);
    });
}

/// cladefs: a userfaultfd write-protecting a mapping, asynchronously: a write to a page it
/// protects resolves itself, no reader of the descriptor needed. Its ioctls by hand, as `libc`
/// has none (linux/userfaultfd.h).
struct Uffd(std::os::fd::OwnedFd);

#[repr(C)]
struct UffdioApi {
    api: u64,
    features: u64,
    ioctls: u64,
}

#[repr(C)]
struct UffdioRange {
    start: u64,
    len: u64,
}

#[repr(C)]
struct UffdioRegister {
    range: UffdioRange,
    mode: u64,
    ioctls: u64,
}

#[repr(C)]
struct UffdioWriteprotect {
    range: UffdioRange,
    mode: u64,
}

impl Uffd {
    const API: u64 = 0xaa;
    const FEATURE_WP_ASYNC: u64 = 1 << 15;
    const REGISTER_MODE_WP: u64 = 1 << 1;
    const WRITEPROTECT_MODE_WP: u64 = 1 << 0;
    /// _IOWR(0xaa, NR, SIZE).
    const fn iowr(nr: u64, size: usize) -> u64 {
        (3 << 30) | ((size as u64) << 16) | (0xaa << 8) | nr
    }
    const IOCTL_API: u64 = Self::iowr(0x3f, std::mem::size_of::<UffdioApi>());
    const IOCTL_REGISTER: u64 = Self::iowr(0x00, std::mem::size_of::<UffdioRegister>());
    const IOCTL_WRITEPROTECT: u64 = Self::iowr(0x06, std::mem::size_of::<UffdioWriteprotect>());

    fn ioctl<T>(&self, request: u64, arg: &mut T, what: &str) {
        use std::os::fd::AsRawFd;
        let arg = std::ptr::from_mut(arg);
        let done = unsafe { libc::ioctl(self.0.as_raw_fd(), request as _, arg) };
        assert_eq!(done, 0, "{what}: {}", std::io::Error::last_os_error());
    }

    fn range(map: &CowMapping) -> UffdioRange {
        UffdioRange { start: map.addr as u64, len: map.len as u64 }
    }

    fn write_protecting(map: &CowMapping) -> Self {
        use std::os::fd::FromRawFd;
        let flags = libc::O_CLOEXEC | libc::O_NONBLOCK;
        let fd = unsafe { libc::syscall(libc::SYS_userfaultfd, flags) };
        assert!(fd >= 0, "userfaultfd: {}", std::io::Error::last_os_error());
        let uffd = Self(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) });
        let mut api = UffdioApi { api: Self::API, features: Self::FEATURE_WP_ASYNC, ioctls: 0 };
        uffd.ioctl(Self::IOCTL_API, &mut api, "UFFDIO_API");
        let mut register =
            UffdioRegister { range: Self::range(map), mode: Self::REGISTER_MODE_WP, ioctls: 0 };
        uffd.ioctl(Self::IOCTL_REGISTER, &mut register, "UFFDIO_REGISTER");
        uffd
    }

    /// Write-protects all of `map`, or takes the write-protect away.
    fn protect(&self, map: &CowMapping, protect: bool) {
        let mode = if protect { Self::WRITEPROTECT_MODE_WP } else { 0 };
        let mut writeprotect = UffdioWriteprotect { range: Self::range(map), mode };
        self.ioctl(Self::IOCTL_WRITEPROTECT, &mut writeprotect, "UFFDIO_WRITEPROTECT");
    }
}

/// cladefs: copying on write (doc-13), the mapping of a chunk mapped whole and split (SplitChunk)
/// moved (mremap), its PTEs with it: a write to a page only read faults, which makes the page
/// private (TASK-149.42, board 14407). Also checks `mprotect(PROT_READ)` -> `mremap` ->
/// `mprotect(PROT_READ | PROT_WRITE)`: on a compound folio, `mremap_folio_pte_batch` ORs dirty
/// bits across the batch so the final `mprotect` would make clean sibling PTEs writable.
pub(super) fn test_cow_mremap_after_a_split_pmd() {
    let mut chunk = SplitChunk::new("cow-mremap-after-split");
    chunk.map.moved();
    chunk.write_to_read_only("after mremap");
    chunk.map.protect(libc::PROT_READ);
    chunk.map.moved();
    chunk.map.protect(libc::PROT_READ | libc::PROT_WRITE);
    chunk.write_to_read_only_at(200 * SplitChunk::PAGE + 13, "after mprotect + mremap + mprotect");
    chunk.finish();
}

/// cladefs: copying on write, per page (doc-13, the 400 GB case at small scale): of a large file
/// read in from sealed extents, mapped shared, three pages stored to cost three private pages,
/// and the rest stay mapped from the extents, shared, costing none, read as they were; written
/// back, sealed and rebound, the three are private no more, and read as stored. Fails if the
/// file left DAX as it was written, if a write or a read made other pages private (a range's
/// pages mapped writable, or a write iomap wider than its page), or if rebinding lost a write.
pub(super) fn test_cow_unwritten_stay_shared() {
    const PAGE: usize = 4096;
    const LEN: usize = 8 << 20;
    let name = path("cow-shared");
    let mut expected = cow_base_file(&name, LEN, 0x22);
    let file = fs::OpenOptions::new().read(true).write(true).open(&name).unwrap();
    // Every private page so far written back and rebound: none left.
    cow_rebind_now();
    assert_eq!(stats()["dax_private_pages"], 0, "private pages before the writes");
    let map = CowMapping::new(&file, LEN);
    // Read whole, through the mapping: nothing private.
    assert!(map.bytes()[..] == expected[..], "the mapping before the writes");
    assert_eq!(stats()["dax_private_pages"], 0, "private pages after reading");
    for page in [5, 700, 1900] {
        map.bytes()[page * PAGE + 1] = 0xab;
        expected[page * PAGE + 1] = 0xab;
    }
    assert_eq!(stats()["dax_private_pages"], 3, "private pages after three writes");
    assert!(is_dax(&name), "{name} left DAX as it was stored to");
    assert!(map.bytes()[..] == expected[..], "the mapping after the writes");
    assert_eq!(stats()["dax_private_pages"], 3, "private pages after reading again");
    map.msync();
    cow_rebind_now();
    assert_eq!(stats()["dax_private_pages"], 0, "private pages once rebound");
    assert!(map.bytes()[..] == expected[..], "the mapping once rebound");
    drop(map);
    drop(file);
    evict(&name);
    assert!(fs::read(&name).unwrap() == expected, "{name} evicted");
    fs::remove_file(&name).unwrap();
}

/// cladefs: copying on write, a punch, a clone and a truncate of a file with dirty private pages,
/// stored through a mapping and written with write(2), none written back: the punched page
/// reads as zeros and the others as stored; the clone, into a file created here (a page-cache
/// inode beside the DAX one, TASK-164.3), has the writes; the truncate drops the dirty pages
/// past the end, which read as zeros once it grows again; all of it as written after a
/// remount. Fails if an UNMAP of a range dropped its dirty pages unwritten (the punch's or
/// clone's drop of the chunk not writing back first), if the clone shared the extents' old
/// blocks (write(2)'s pages, which tag nothing, not written back for it), or if the dirty page
/// past the end was written back after the truncate, or kept in the window.
pub(super) fn test_cow_dirty_truncate_punch_clone() {
    use std::os::unix::fs::FileExt;
    const PAGE: usize = 4096;
    const LEN: usize = 16 * PAGE;
    let name = path("cow-dirty");
    let clone = path("cow-dirty-clone");
    let mut expected = cow_base_file(&name, LEN, 0x33);
    let file = fs::OpenOptions::new().read(true).write(true).open(&name).unwrap();
    let map = CowMapping::new(&file, LEN);
    for page in [2, 5, 9] {
        map.bytes()[page * PAGE..(page + 1) * PAGE].fill(0xaa);
        expected[page * PAGE..(page + 1) * PAGE].fill(0xaa);
    }
    file.write_all_at(&[0xbb; PAGE], 12 * PAGE as u64).unwrap();
    expected[12 * PAGE..13 * PAGE].fill(0xbb);
    drop(map);

    // A hole over page 5, beside the dirty pages 2 and 9 of its chunk.
    let punch = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;
    let fd = file.as_raw_fd();
    assert_eq!(unsafe { libc::fallocate(fd, punch, 5 * PAGE as i64, PAGE as i64) }, 0);
    expected[5 * PAGE..6 * PAGE].fill(0);
    assert!(fs::read(&name).unwrap() == expected, "{name} punched");

    // Page 9 dirty again, then the whole file cloned.
    file.write_all_at(&[0xcc; 10], 9 * PAGE as u64).unwrap();
    expected[9 * PAGE..9 * PAGE + 10].fill(0xcc);
    let dst = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&clone)
        .unwrap();
    assert!(!is_dax(&clone), "{clone}, created here, is DAX");
    clone_range(&dst, &file, 0, LEN as u64, 0);
    assert!(fs::read(&clone).unwrap() == expected, "{clone} as cloned");

    // Page 12 dirty again, then cut, with page 10's end, and the file grown again.
    file.write_all_at(&[0xdd; 10], 12 * PAGE as u64).unwrap();
    file.set_len(10 * PAGE as u64 + 100).unwrap();
    file.set_len(LEN as u64).unwrap();
    expected[10 * PAGE + 100..].fill(0);
    assert!(fs::read(&name).unwrap() == expected, "{name} truncated and grown");
    file.sync_all().unwrap();
    dst.sync_all().unwrap();
    let cloned = fs::read(&clone).unwrap();
    drop(file);
    drop(dst);
    unmount();
    mount();
    assert!(fs::read(&name).unwrap() == expected, "{name} after a remount");
    assert!(fs::read(&clone).unwrap() == cloned, "{clone} after a remount");
    fs::remove_file(&name).unwrap();
    fs::remove_file(&clone).unwrap();
}

/// cladefs: copying on write, a writeback whose transaction fails once its bytes are written,
/// before it commits, as a crash between the two (cladefs.dax_fail_commits, Fxfs's FAIL_COMMITS):
/// fsync says so (EIO), and the file holds its old data, all of it, never a mix of old and new
/// pages, after a remount, which maps it again from what Fxfs committed; written again, it
/// holds the new. Fails if the writeback committed before its bytes, or in pieces with
/// commits between, or if the failure was lost (fsync returning 0).
pub(super) fn test_cow_commit_after_bytes() {
    use std::os::unix::fs::FileExt;
    const PAGE: usize = 4096;
    let name = path("cow-commit");
    let old = cow_base_file(&name, 4 * PAGE, 0x44);
    let file = fs::OpenOptions::new().read(true).write(true).open(&name).unwrap();
    file.write_all_at(&[0x55; 4 * PAGE], 0).unwrap();
    fs::write("/sys/module/cladefs/parameters/dax_fail_commits", b"1").unwrap();
    let synced = file.sync_all();
    fs::write("/sys/module/cladefs/parameters/dax_fail_commits", b"0").unwrap();
    let err = synced.expect_err("fsync of a writeback whose commit failed");
    assert_eq!(err.raw_os_error(), Some(libc::EIO), "{err}");
    drop(file);
    unmount();
    mount();
    assert!(fs::read(&name).unwrap() == old, "{name} after a failed commit");
    let file = fs::OpenOptions::new().read(true).write(true).open(&name).unwrap();
    file.write_all_at(&[0x66; 4 * PAGE], 0).unwrap();
    file.sync_all().unwrap();
    drop(file);
    unmount();
    mount();
    assert!(fs::read(&name).unwrap() == vec![0x66; 4 * PAGE], "{name} written again");
    fs::remove_file(&name).unwrap();
}

/// cladefs: copying on write, a file created in the mount is a page-cache inode, its writes the
/// page cache's (TASK-164.3): not DAX, costing no private page, until it is read in again,
/// once a barrier has sealed its writes; written back and read in again before that barrier,
/// it is a page-cache inode again, as the window would find its extents open. Fails if a
/// created inode got S_DAX, if a sealed one read in didn't, or if one read in before its seal
/// mapped its open extents (dax_open_maps, or a seal the mapping had to make). The last,
/// with no seal of the journal's between the writeback and the read-in, a few milliseconds.
pub(super) fn test_cow_created_in_page_cache() {
    let name = path("cow-created");
    let before = stats()["dax_private_pages"];
    fs::write(&name, vec![0x77; 3 << 20]).unwrap();
    assert!(!is_dax(&name), "{name}, created here, is DAX");
    assert_eq!(stats()["dax_private_pages"], before, "a created file's writes went private");
    evict(&name);
    assert!(is_dax(&name), "{name}, read in once sealed, isn't DAX");
    fs::remove_file(&name).unwrap();

    // Written back, its writes unsealed, evicted, read in again: the page cache's still.
    let name = path("cow-unsealed");
    let file = fs::File::create(&name).unwrap();
    (&file).write_all(&[0x78; 1 << 20]).unwrap();
    let fd = file.as_raw_fd();
    let wrote = unsafe {
        libc::sync_file_range(
            fd,
            0,
            0,
            libc::SYNC_FILE_RANGE_WAIT_BEFORE
                | libc::SYNC_FILE_RANGE_WRITE
                | libc::SYNC_FILE_RANGE_WAIT_AFTER,
        )
    };
    assert_eq!(wrote, 0, "sync_file_range: {}", std::io::Error::last_os_error());
    assert_eq!(unsafe { libc::posix_fadvise(fd, 0, 0, libc::POSIX_FADV_DONTNEED) }, 0);
    drop(file);
    let seals = stats()["dax_seals"];
    fs::write("/proc/sys/vm/drop_caches", b"2").unwrap();
    assert!(!is_dax(&name), "{name}, read in before its writes were sealed, is DAX");
    assert!(fs::read(&name).unwrap() == vec![0x78; 1 << 20], "{name} read in again");
    // Sealed, evicted, read in: DAX, mapping what is sealed.
    evict(&name);
    assert!(is_dax(&name), "{name}, read in once sealed, isn't DAX");
    assert!(fs::read(&name).unwrap() == vec![0x78; 1 << 20], "{name} read through the window");
    assert_eq!(stats()["dax_seals"], seals, "mapping {name} had to seal");
    assert_eq!(stats()["dax_open_maps"], 0, "the window was asked to map an open extent");
    fs::remove_file(&name).unwrap();
}

/// cladefs: files created in the mount, sealed and evicted, read in again as DAX with an unused
/// DAX inode ahead of them on the kernel's inode list, one whose mapping holds one DAX entry, at
/// index 0 (board 13582). The inode shrinker took that inode for one with a shadow entry, as
/// cleared as it is invalidated, and retried it for good: no inode behind it was evicted under
/// memory pressure, nor by drop_caches but once (dropping the page cache took a reference of the
/// inode ahead, and its last one let it pass once), and a build's outputs stayed in the page
/// cache. So the created files' pages go file by file and their inodes by `drop_caches 2`, as
/// reclaim's, which drops no page cache. Fails if a created file reads in again in the page cache.
pub(super) fn test_cow_created_behind_a_dax_inode() {
    const PAGE: usize = 4096;
    // A page read through a mapping of a file read in from sealed extents: its one DAX entry.
    let first = path("cow-behind-first");
    fs::write(&first, vec![0x5a; PAGE]).unwrap();
    evict(&first);
    assert!(is_dax(&first), "{first}, read in once sealed, isn't DAX");
    let file = fs::OpenOptions::new().read(true).write(true).open(&first).unwrap();
    assert_eq!(CowMapping::new(&file, PAGE).bytes()[0], 0x5a, "{first} read through the window");
    // The time the read set written back: an inode dirty leaves the list, and returns clean
    // behind the created files', as a build's sources, read before, never are.
    file.sync_all().unwrap();
    drop(file);
    // Its inode unused, on the inode list before the created files'.
    fs::write("/proc/sys/vm/drop_caches", b"2").unwrap();
    let names: Vec<String> = (0..8).map(|i| path(&format!("cow-behind-{i}"))).collect();
    for (i, name) in names.iter().enumerate() {
        fs::write(name, vec![i as u8; 16 * PAGE]).unwrap();
        assert!(!is_dax(name), "{name}, created here, is DAX");
    }
    for name in &names {
        drop_pages(name);
    }
    fs::write("/proc/sys/vm/drop_caches", b"2").unwrap();
    for name in &names {
        assert!(
            is_dax(name),
            "{name}, created, sealed and evicted, read in again in the page cache"
        );
    }
    for name in names.iter().chain([&first]) {
        fs::remove_file(name).unwrap();
    }
}

/// cladefs: copying on write (doc-13), a rebind paused before its MAP over a private page,
/// holding the file's semaphore for writing (Pauses::REBIND): a load through another process's
/// PTE of the page reads what was stored there, and a write to it by a process with no PTE of
/// it waits for the rebind, then lands, the page private again. The rebind rebinds that page
/// alone, and the file, through the mapping and evicted, holds both writes. Fails if a write
/// went in during the rebind (one taking the semaphore shared), if the rebind's MAP lost it,
/// or came before the writeback's bytes were in the extent (the writes gone), or if a load had
/// to wait for it. Copying on write, nothing moves: this replaced writable DAX's races of moves
/// against loads and writes (moved_across_seal, load_during_move, TASK-121, 149.14).
pub(super) fn test_rebind_against_loads_and_writes() {
    const PAGE: usize = 4096;
    const LEN: usize = 16 * PAGE;
    const STORED: usize = 3 * PAGE + 10;
    const AGAIN: usize = 3 * PAGE + 100;
    let name = path("rebind-against-loads-and-writes");
    let mut expected = cow_base_file(&name, LEN, 0x88);
    // Every other file's private pages rebound first: the rebind pauses at this file's.
    cow_rebind_now();
    let file = fs::OpenOptions::new().read(true).write(true).open(&name).unwrap();
    let map = CowMapping::new(&file, LEN);
    map.bytes()[STORED..STORED + 6].copy_from_slice(b"stored");
    expected[STORED..STORED + 6].copy_from_slice(b"stored");
    // Written back and sealed, its page clean and private: the next rebind's.
    map.msync();
    let mapped = map.addr as usize;
    let pipe = || {
        let mut fds = [0; 2];
        let made = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(made, 0, "pipe: {}", std::io::Error::last_os_error());
        fds
    };
    // A byte a child writes to `fd`, within `ms` milliseconds, if it does.
    let heard = |fd: libc::c_int, ms: libc::c_int| {
        let mut ready = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut ready, 1, ms) } != 1 {
            return None;
        }
        let mut byte = 0u8;
        assert_eq!(unsafe { libc::read(fd, (&raw mut byte).cast(), 1) }, 1, "a child's byte");
        Some(byte)
    };

    // The loader, in a process of its own: a load now makes its PTE of the page, through
    // which it loads again once told to.
    let [from_loader, loader_says] = pipe();
    let [loader_hears, to_loader] = pipe();
    let loader = unsafe { libc::fork() };
    assert!(loader >= 0, "fork: {}", std::io::Error::last_os_error());
    if loader == 0 {
        unsafe {
            let at = (mapped + STORED) as *const u8;
            let mut byte = at.read_volatile();
            libc::write(loader_says, (&raw const byte).cast(), 1);
            libc::read(loader_hears, (&raw mut byte).cast(), 1);
            byte = at.read_volatile();
            let wrote = libc::write(loader_says, (&raw const byte).cast(), 1);
            libc::_exit(if wrote == 1 { 0 } else { 1 });
        }
    }
    assert_eq!(heard(from_loader, 10_000), Some(b's'), "the loader's first load");

    let rebound = stats()["dax_rebound_pages"];
    let (pauses, only) = Pauses::begin_of(Pauses::REBIND, &name);
    let rebinder = std::thread::spawn(cow_rebind_now);
    Pauses::wait(Pauses::REBIND);
    let go = 0u8;
    assert_eq!(unsafe { libc::write(to_loader, (&raw const go).cast(), 1) }, 1, "go");
    assert_eq!(heard(from_loader, 1000), Some(b's'), "a load during the rebind");
    // The mapped_writer, in a process of its own: its write's fault, waiting, holds its process's
    // mmap_lock, which this one's threads would otherwise wait for.
    let [from_mapped_writer, mapped_writer_says] = pipe();
    let mapped_writer = unsafe { libc::fork() };
    assert!(mapped_writer >= 0, "fork: {}", std::io::Error::last_os_error());
    if mapped_writer == 0 {
        unsafe {
            ((mapped + AGAIN) as *mut u8).write_volatile(0xee);
            let done = 1u8;
            let wrote = libc::write(mapped_writer_says, (&raw const done).cast(), 1);
            libc::_exit(if wrote == 1 { 0 } else { 1 });
        }
    }
    assert_eq!(heard(from_mapped_writer, 200), None, "a write went in during the rebind");
    drop(pauses);
    drop(only);
    rebinder.join().unwrap();
    assert_eq!(heard(from_mapped_writer, 10_000), Some(1), "the write after the rebind");
    for (child, who) in [(loader, "the loader"), (mapped_writer, "the mapped writer")] {
        let status = wait_for_child(child, std::time::Duration::from_secs(10));
        let exited = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;
        assert!(exited, "{who}: {status:#x}");
    }
    let fds =
        [from_loader, loader_says, loader_hears, to_loader, from_mapped_writer, mapped_writer_says];
    for fd in fds {
        unsafe { libc::close(fd) };
    }
    expected[AGAIN] = 0xee;
    assert_eq!(stats()["dax_rebound_pages"], rebound + 1, "the pages rebound");
    assert!(map.bytes()[..] == expected[..], "the mapping after the rebind");
    drop(map);
    drop(file);
    evict(&name);
    assert!(fs::read(&name).unwrap() == expected, "{name} evicted");
    fs::remove_file(&name).unwrap();
}

/// cladefs: copying on write (doc-13), writes through a mapping and write(2)s to a file's pages,
/// one after another for two seconds, while the budget's worker writes back, seals and rebinds
/// every private page it can every 10 ms (cladefs.dax_rebind_every, doc-13's stage 2): each reads
/// back as stored, then and at the end, and the file evicted holds them all. Fails if a rebind
/// lost a write (a page mapped back to its extent before the writeback of a write to it was
/// sealed, or with a write to it under way), or if a writeback lost one made after its
/// write-protect (a write into its swapped-out set, its page left clean).
///
/// The writes go to a set of `HOT` pages that moves on by a page every `STEP`, so that each
/// page, left behind, goes unstored for many of the worker's rounds, which rebind it, while
/// writes race the rebinds of the pages beside it, and, as the set comes back to it, its own.
/// Stored to all in turn, a page went unstored between its writeback and its rebind only if
/// the loop stalled: the faster the loop, the fewer rebound, 31 and 45 at two million rounds,
/// none at last (GCE r582, r583; TASK-171).
pub(super) fn test_writes_under_rebinds() {
    use std::os::unix::fs::FileExt;
    const PAGE: usize = 4096;
    const PAGES: usize = 64;
    const HOT: usize = 8;
    const STEP: Duration = Duration::from_millis(2);
    const LEN: usize = PAGES * PAGE;
    let name = path("writes-under-rebinds");
    let mut expected = cow_base_file(&name, LEN, 0x99);
    let file = fs::OpenOptions::new().read(true).write(true).open(&name).unwrap();
    let map = CowMapping::new(&file, LEN);
    let rebound = stats()["dax_rebound_pages"];
    let every = Param::set("dax_rebind_every", "10");
    let start = Instant::now();
    let deadline = start + Duration::from_secs(2);
    let mut round: usize = 0;
    while Instant::now() < deadline {
        // The hot pages in an order that varies, each at an offset of its own, stored and
        // written in turn.
        let first = (start.elapsed().as_nanos() / STEP.as_nanos()) as usize;
        let page = (first + round * 7 % HOT) % PAGES;
        let at = page * PAGE + round * 61 % (PAGE - 8);
        let value = (round as u64).to_le_bytes();
        if round % 2 == 0 {
            map.bytes()[at..at + 8].copy_from_slice(&value);
        } else {
            file.write_all_at(&value, at as u64).unwrap();
        }
        expected[at..at + 8].copy_from_slice(&value);
        assert!(map.bytes()[at..at + 8] == value, "round {round}: the bytes at {at}");
        if round % 256 == 0 {
            assert!(map.bytes()[..] == expected[..], "round {round}: the mapping");
        }
        round += 1;
    }
    drop(every);
    let rebinds = stats()["dax_rebound_pages"] - rebound;
    eprintln!("writes_under_rebinds: {round} rounds, {rebinds} pages rebound");
    assert!(map.bytes()[..] == expected[..], "the mapping after {round} rounds");
    assert_ne!(rebinds, 0, "no page rebound in {round} rounds");
    drop(map);
    drop(file);
    evict(&name);
    assert!(fs::read(&name).unwrap() == expected, "{name} evicted");
    fs::remove_file(&name).unwrap();
}
