// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The memory backend against a VM process of the test's own, which does what Firecracker's
//! `Mappings` backend does (vendor/firecracker/src/vmm/src/memory_backend.rs): guest memory
//! reserved, a userfaultfd made, each entry mapped inaccessible, registered, then opened, every
//! map answered.

use super::protocol::{self, Entry, FromVmm, GuestRegion, ToVmm};
use crate::server::{default_tenant, serve};
use cladefs_core::pages::page_size;
use cladefs_wire::memory_cell::MemoryCell;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::ptr::NonNull;

fn pattern(byte: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| byte ^ (i % 251) as u8).collect()
}

/// What /proc/self/maps says maps the page at `addr`: its file's device and inode, and the
/// offset in it; anonymous memory's being "00:00 0".
fn mapped_from(addr: usize) -> (String, u64) {
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    for line in maps.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (start, end) = fields[0].split_once('-').unwrap();
        let (start, end) =
            (usize::from_str_radix(start, 16).unwrap(), usize::from_str_radix(end, 16).unwrap());
        if (start..end).contains(&addr) {
            let offset = u64::from_str_radix(fields[2], 16).unwrap();
            return (format!("{} {}", fields[3], fields[4]), offset + (addr - start) as u64);
        }
    }
    panic!("{addr:#x} isn't mapped");
}

/// Whether the page at `addr` maps its file's page, the page cache's, as /proc/self/pagemap says
/// (`PM_FILE`): not a private copy, nor anonymous memory.
fn maps_file_page(addr: usize) -> bool {
    use std::os::unix::fs::FileExt;
    let pagemap = File::open("/proc/self/pagemap").unwrap();
    let mut entry = [0u8; 8];
    let at = (addr as u64 / page_size()) * 8;
    pagemap.read_exact_at(&mut entry, at).unwrap();
    let entry = u64::from_le_bytes(entry);
    assert_ne!(entry & 1 << 63, 0, "{addr:#x} isn't in memory");
    entry & 1 << 61 != 0
}

/// Reads the page of arena memory the page at `addr` maps, through a mapping of its own, as any
/// VM process of the default tenant's may map it (shared, read-only): a hole there, faulted in
/// as zeros. Returns its first byte.
fn read_arena_page_of(addr: usize) -> u8 {
    let (file, offset) = mapped_from(addr);
    let inode: u64 = file.split_whitespace().nth(1).unwrap().parse().unwrap();
    let tenant = default_tenant().unwrap();
    let arenas = [tenant.arena(), cladefs_core::pages::ArenaId::DEFAULT];
    let memory = (arenas.into_iter())
        .filter_map(|arena| tenant.arena_memory(arena))
        .find(|memory| {
            // SAFETY: A stat of a descriptor the tenant holds, into a structure of the test's.
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: As above.
            let done = unsafe { libc::fstat(memory.as_raw_fd(), &mut stat) };
            done == 0 && stat.st_ino == inode
        })
        .expect("the page maps an arena's memory");
    let ps = page_size() as usize;
    // SAFETY: Maps one page of the arena's memory, read-only, unmapped below.
    let page = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            ps,
            libc::PROT_READ,
            libc::MAP_SHARED,
            memory.as_raw_fd(),
            offset as libc::off_t,
        )
    };
    assert_ne!(page, libc::MAP_FAILED);
    // SAFETY: The page just mapped, read, then unmapped.
    unsafe {
        let first = page.cast::<u8>().read_volatile();
        libc::munmap(page, ps);
        first
    }
}

/// A userfaultfd made as Firecracker makes one: through `/dev/userfaultfd`, kernel faults
/// included, no events but faults.
fn userfaultfd() -> File {
    const fn ioc(dir: u64, nr: u64, size: usize) -> u64 {
        (dir << 30) | ((size as u64) << 16) | (0xaa << 8) | nr
    }
    #[repr(C)]
    struct Api {
        api: u64,
        features: u64,
        ioctls: u64,
    }
    let device = std::fs::OpenOptions::new().read(true).write(true).open("/dev/userfaultfd");
    let device = device.expect("/dev/userfaultfd");
    // SAFETY: The device's ioctl making a userfaultfd, which this process owns at once.
    let fd = unsafe {
        libc::ioctl(device.as_raw_fd(), ioc(0, 0, 0) as _, libc::O_CLOEXEC | libc::O_NONBLOCK)
    };
    assert!(fd >= 0, "a userfaultfd: {}", std::io::Error::last_os_error());
    // SAFETY: The descriptor the kernel just made.
    let uffd = unsafe { File::from_raw_fd(fd) };
    let mut api = Api { api: 0xaa, features: 0, ioctls: 0 };
    // SAFETY: UFFDIO_API, with the structure it takes.
    let done = unsafe { libc::ioctl(fd, ioc(3, 0x3f, size_of::<Api>()) as _, &mut api) };
    assert_eq!(done, 0, "UFFDIO_API: {}", std::io::Error::last_os_error());
    uffd
}

/// A VM process of the test's: its guest memory, one region, and its connection to the backend.
struct FakeVmm {
    base: NonNull<u8>,
    size: u64,
    uffd: File,
    stream: UnixStream,
}

// SAFETY: Its memory is the test's, which its threads read as a guest would.
unsafe impl Send for FakeVmm {}
// SAFETY: As above.
unsafe impl Sync for FakeVmm {}

impl FakeVmm {
    /// Guest memory of `size` bytes, handed to the backend at `socket`, mapped as it lists it.
    fn start(socket: &Path, size: u64) -> Self {
        // SAFETY: Reserves anonymous memory, which the test owns.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        assert_ne!(base, libc::MAP_FAILED);
        let base = NonNull::new(base.cast()).unwrap();
        let stream = UnixStream::connect(socket).unwrap();
        let uffd = userfaultfd();
        let region = GuestRegion {
            base_host_virt_addr: base.as_ptr() as u64,
            size: size as usize,
            offset: 0,
            page_size: page_size() as usize,
            pmem_id: None,
        };
        let handshake = FromVmm::Handshake { regions: vec![region] };
        protocol::send(&stream, &handshake, &[uffd.as_raw_fd()]).unwrap();
        let vmm = Self { base, size, uffd, stream };
        // Its maps until it is ready.
        loop {
            let (message, fds) = protocol::recv::<ToVmm>(&vmm.stream).unwrap();
            match message {
                ToVmm::Map { id, entries, .. } => vmm.apply(id, &entries, &fds),
                ToVmm::Ready => break,
                ToVmm::Persisted { .. } => panic!("persisted before ready"),
            }
        }
        vmm
    }

    /// Maps `entries` as Firecracker does: inaccessible, registered, opened; and answers map
    /// `id`.
    fn apply(&self, id: u64, entries: &[Entry], fds: &[File]) {
        // UFFDIO_REGISTER_MODE_MISSING, and MINOR.
        const MISSING: u64 = 1;
        const MINOR: u64 = 4;
        for entry in entries {
            let (offset, len, fd, file_offset, mode, open) = match *entry {
                Entry::File { fd, file_offset, offset, len } => {
                    (offset, len, fds[fd].as_raw_fd(), file_offset, Some(MISSING | MINOR), true)
                }
                Entry::Zeros { offset, len } => (offset, len, -1, 0, None, true),
                Entry::None { offset, len } => (offset, len, -1, 0, None, false),
                Entry::Lazy { offset, len } => (offset, len, -1, 0, Some(MISSING), true),
            };
            assert!(offset + len <= self.size);
            let addr = self.base.as_ptr() as u64 + offset;
            let anonymous = if fd < 0 { libc::MAP_ANONYMOUS } else { 0 };
            // SAFETY: Replaces the test's own reservation.
            let mapped = unsafe {
                libc::mmap(
                    addr as *mut libc::c_void,
                    len as usize,
                    libc::PROT_NONE,
                    libc::MAP_PRIVATE | libc::MAP_FIXED | libc::MAP_NORESERVE | anonymous,
                    fd,
                    file_offset as libc::off_t,
                )
            };
            assert_ne!(mapped, libc::MAP_FAILED, "{entry:?}");
            if let Some(mode) = mode {
                #[repr(C)]
                struct Register {
                    start: u64,
                    len: u64,
                    mode: u64,
                    ioctls: u64,
                }
                let mut register = Register { start: addr, len, mode, ioctls: 0 };
                let request = (3u64 << 30) | ((size_of::<Register>() as u64) << 16) | (0xaa << 8);
                // SAFETY: UFFDIO_REGISTER of the range just mapped.
                let done =
                    unsafe { libc::ioctl(self.uffd.as_raw_fd(), request as _, &mut register) };
                assert_eq!(done, 0, "registering {entry:?}: {}", std::io::Error::last_os_error());
            }
            if open {
                let prot = libc::PROT_READ | libc::PROT_WRITE;
                // SAFETY: Opens the range just mapped.
                let opened =
                    unsafe { libc::mprotect(addr as *mut libc::c_void, len as usize, prot) };
                assert_eq!(opened, 0);
            }
        }
        protocol::send(&self.stream, &FromVmm::Mapped { id }, &[]).unwrap();
    }

    /// Answers the backend's maps on a thread of its own, as Firecracker's reader does, until the
    /// test ends: what it answers persists, passed on.
    fn serve_maps(self: &std::sync::Arc<Self>) -> std::sync::mpsc::Receiver<serde_json::Value> {
        let vmm = self.clone();
        let (persisted, results) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while let Ok((message, fds)) = protocol::recv::<ToVmm>(&vmm.stream) {
                match message {
                    ToVmm::Map { id, entries, .. } => vmm.apply(id, &entries, &fds),
                    ToVmm::Persisted { result } => persisted.send(result).unwrap(),
                    ToVmm::Ready => panic!("ready twice"),
                }
            }
        });
        results
    }

    /// Asks the backend to persist its memory, as Firecracker does making a snapshot, its vCPUs
    /// paused: what it answers, which `results` (its [`FakeVmm::serve_maps`]) passes on.
    fn persist(&self, results: &std::sync::mpsc::Receiver<serde_json::Value>) -> serde_json::Value {
        protocol::send(&self.stream, &FromVmm::Persist, &[]).unwrap();
        results.recv().unwrap()
    }

    /// Stores `bytes` at `at` in its guest memory, as the guest would.
    fn store(&self, at: u64, bytes: &[u8]) {
        for (i, byte) in bytes.iter().enumerate() {
            // SAFETY: Its guest memory, whose faults the backend serves.
            unsafe { self.base.as_ptr().add(at as usize + i).write_volatile(*byte) };
        }
    }

    /// Its guest memory's bytes from `at` to `end`, as the guest reads them.
    fn read(&self, at: u64, end: u64) -> Vec<u8> {
        // SAFETY: Its guest memory, whose faults the backend serves.
        (at..end).map(|at| unsafe { self.base.as_ptr().add(at as usize).read_volatile() }).collect()
    }

    fn addr(&self, at: u64) -> usize {
        self.base.as_ptr() as usize + at as usize
    }
}

/// How long the tests' persists may take: as long as a test runs.
const PERSIST_DEADLINE: std::time::Duration = std::time::Duration::from_secs(600);

/// A daemon serving a memory cell's version, made of `image`, to its VMs' memory backends.
struct Versions {
    dir: tempfile::TempDir,
    socket: std::path::PathBuf,
    version: cladefs_core::CellVersion,
    size: u64,
}

impl Versions {
    /// A daemon, and a version of a memory cell holding `image`, `holes` (pages) discarded.
    fn of(image: &[u8], holes: std::ops::Range<u64>) -> Self {
        let _ = crate::logging::to_stderr();
        // SAFETY: nextest runs each test in a process of its own, whose other threads don't read
        // the environment yet.
        unsafe {
            std::env::set_var("CLADEFSD_BUCKET", "memory:///cells");
            std::env::set_var("CLADEFSD_TOKEN", "1");
        }
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("cladefsd.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || serve(listener));
        let (ps, size) = (page_size(), image.len() as u64);
        let memory = MemoryCell::open(&socket, &dir.path().join("memory"), size).unwrap();
        memory.write(0, image).unwrap();
        memory.discard(holes.start * ps, (holes.end - holes.start) * ps).unwrap();
        memory.commit().unwrap();
        let version = memory.make_version().unwrap();
        memory.close().unwrap();
        Self { dir, socket, version, size }
    }

    /// A VM restored from the version, from a fork of its own named `name`, its maps answered,
    /// and the thread serving its memory.
    fn restore(
        &self,
        name: &str,
    ) -> (std::sync::Arc<FakeVmm>, std::thread::JoinHandle<Result<(), String>>) {
        self.restore_from(self.version, name)
    }

    /// A VM restored from version `version` of the memory cell, as [`Versions::restore`] does.
    fn restore_from(
        &self,
        version: cladefs_core::CellVersion,
        name: &str,
    ) -> (std::sync::Arc<FakeVmm>, std::thread::JoinHandle<Result<(), String>>) {
        let (backend, serving) = self.serve_fork(version, name, PERSIST_DEADLINE);
        (std::sync::Arc::new(FakeVmm::start(&backend, self.size)), serving)
    }

    /// A fork of version `version` of the memory cell, named `name`, served by a memory backend,
    /// its persists past `persist_deadline` killing its VM process: the backend's socket, and the
    /// thread serving it.
    fn serve_fork(
        &self,
        version: cladefs_core::CellVersion,
        name: &str,
        persist_deadline: std::time::Duration,
    ) -> (std::path::PathBuf, std::thread::JoinHandle<Result<(), String>>) {
        let path = self.dir.path().join(name);
        MemoryCell::open_forked(&self.socket, &path, version, self.size).unwrap().close().unwrap();
        let backend = self.dir.path().join(format!("{name}.sock"));
        let listener = UnixListener::bind(&backend).unwrap();
        let tenant = default_tenant().unwrap();
        (backend, super::serve(listener, tenant, path, persist_deadline))
    }
}

/// Whether the page at `addr` is in memory: in the page table, or its file's page in the page
/// cache.
fn resident(addr: usize) -> bool {
    let mut vec = [0u8];
    // SAFETY: Asks of one page of the test's own mappings.
    let done =
        unsafe { libc::mincore(addr as *mut libc::c_void, page_size() as usize, vec.as_mut_ptr()) };
    assert_eq!(done, 0, "mincore: {}", std::io::Error::last_os_error());
    vec[0] & 1 != 0
}

/// A memory cell's version served as two VMs' guest memory through the memory backend, each
/// from a fork of its own: each reads the image, its holes zeros, every node's range listed as
/// its first touch reaches it; the data pages each maps are the same arena pages, shared, and
/// stay so through the cache's eviction of them, paged in again where they were as the guests
/// touch them, zeros another mapping faulted in there meanwhile never served; a store of one VM's
/// is its own, the other's page unchanged.
#[test]
fn versions_serve_guest_memory_shared_between_vms() {
    let ps = page_size();
    // Three nodes of the image's tree, the last part of one.
    const PAGES: u64 = 400;
    let size = PAGES * ps;
    let mut image = Vec::with_capacity(size as usize);
    for page in 0..PAGES {
        image.extend(pattern(page as u8, ps as usize));
    }
    let versions = Versions::of(&image, 200..230);
    image[(200 * ps) as usize..(230 * ps) as usize].fill(0);
    let ((a, _a_serving), (b, _b_serving)) = (versions.restore("one"), versions.restore("two"));
    a.serve_maps();
    b.serve_maps();
    let shared = [0, 150, 340, 399];
    assert!(a.read(0, size) == image, "the first VM's memory holds the image");
    assert!(b.read(0, size) == image, "the second VM's memory holds the image");
    let is_shared = |page: u64| {
        let (at_a, at_b) = (a.addr(page * ps), b.addr(page * ps));
        maps_file_page(at_a) && maps_file_page(at_b) && mapped_from(at_a) == mapped_from(at_b)
    };
    for page in shared {
        assert!(is_shared(page), "page {page} isn't shared");
    }
    assert_eq!(mapped_from(a.addr(210 * ps)).0, "00:00 0", "a hole maps zeros");

    cladefs_core::cache::evict_least_used(None, |_| false);
    for page in shared {
        assert!(!resident(a.addr(page * ps)), "page {page} wasn't evicted");
        // Faulted in as zeros by another mapping before the part is paged in again there: what
        // the VMs are served is still the image, never that.
        assert_eq!(read_arena_page_of(a.addr(page * ps)), 0, "page {page}'s place isn't a hole");
    }
    assert!(a.read(0, size) == image, "the first VM's memory holds the image after an eviction");
    assert!(b.read(0, size) == image, "the second VM's memory holds the image after an eviction");
    for page in shared {
        assert!(is_shared(page), "page {page} isn't shared after an eviction");
    }

    // SAFETY: A store of the guest's to its memory.
    unsafe { (a.addr(5 * ps) as *mut u8).write_volatile(0xee) };
    assert_eq!(a.read(5 * ps, 5 * ps + 1), [0xee]);
    assert!(!maps_file_page(a.addr(5 * ps)), "a store wrote the shared page");
    assert_eq!(b.read(5 * ps, 6 * ps), pattern(5, ps as usize), "the other VM's page changed");
}

/// Threads faulting on a node of the image before its map is applied are woken by it, each
/// finding its page: the first's, whose fault the map is for, and a second's, on a hole of the
/// node, whose fault the handler reads after the map, and whose page nothing installs.
#[test]
fn faults_on_a_node_mapping_are_all_woken() {
    let ps = page_size();
    let mut image = pattern(3, (16 * ps) as usize);
    let versions = Versions::of(&image, 1..4);
    image[ps as usize..(4 * ps) as usize].fill(0);
    let (vmm, _serving) = versions.restore("one");
    let touch = |page: u64| {
        let vmm = vmm.clone();
        std::thread::spawn(move || vmm.read(page * ps, page * ps + 1))
    };
    let first = touch(0);
    let (map, fds) = protocol::recv::<ToVmm>(&vmm.stream).unwrap();
    let ToVmm::Map { id, entries, .. } = map else { panic!("{map:?} in place of a map") };
    let second = touch(2);
    // The second's fault waits in the userfaultfd, unread, the handler waiting for the map.
    let mut poll = libc::pollfd { fd: vmm.uffd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: Polls the test's own copy of the userfaultfd.
    assert_eq!(unsafe { libc::poll(&mut poll, 1, -1) }, 1);
    vmm.apply(id, &entries, &fds);
    vmm.serve_maps();
    assert_eq!(first.join().unwrap(), [image[0]]);
    assert_eq!(second.join().unwrap(), [0], "a hole's page");
}

/// An answer to a map the handler never sent ends the VM's memory backend: answers wait to be
/// taken, so a VM process sending them unasked holds up nothing of the daemon's but its own.
#[test]
fn answers_unasked_end_the_memory_backend() {
    let versions = Versions::of(&pattern(4, (16 * page_size()) as usize), 0..0);
    let (vmm, serving) = versions.restore("one");
    protocol::send(&vmm.stream, &FromVmm::Mapped { id: 99 }, &[]).unwrap();
    assert_eq!(serving.join().unwrap(), Err("an answer to map 99, which none waited for".into()));
}

/// A VM process that makes its userfaultfd blocking ends its memory backend, rather than leave
/// its fault thread reading for ever once the VM process is gone.
#[test]
fn blocking_userfaultfds_end_the_memory_backend() {
    let versions = Versions::of(&pattern(4, (16 * page_size()) as usize), 0..0);
    let (vmm, serving) = versions.restore("one");
    // SAFETY: Clears the flags of the test's own userfaultfd, O_NONBLOCK among them.
    assert_eq!(unsafe { libc::fcntl(vmm.uffd.as_raw_fd(), libc::F_SETFL, 0) }, 0);
    let ended = serving.join().unwrap();
    assert_eq!(ended, Err("its faults: the userfaultfd polls as an error: made blocking".into()));
}

/// A map the VM process refuses ends its memory backend, saying why, and shuts its socket: the
/// VM process sees its backend gone, as Firecracker goes then (`MemoryBackendLost`), rather than
/// leave the vCPU whose fault the map was for waiting for ever.
#[test]
fn maps_refused_end_the_memory_backend() {
    let ps = page_size();
    let versions = Versions::of(&pattern(7, (16 * ps) as usize), 0..0);
    let (vmm, serving) = versions.restore("one");
    let toucher = vmm.clone();
    // Its fault waits for ever: the test's process ends it.
    std::thread::spawn(move || toucher.read(0, 1));
    let (map, _) = protocol::recv::<ToVmm>(&vmm.stream).unwrap();
    let ToVmm::Map { id, .. } = map else { panic!("{map:?} in place of a map") };
    let refused = FromVmm::MapRefused { id, error: "no room".into() };
    protocol::send(&vmm.stream, &refused, &[]).unwrap();
    assert_eq!(serving.join().unwrap(), Err("no room".to_owned()));
    let after = protocol::recv::<ToVmm>(&vmm.stream);
    assert!(matches!(after, Err(protocol::FrameError::Gone)), "{after:?}");
}

/// A persist keeps what the guest stored: the version it answers holds the VM's memory, stores
/// over data and over a hole, a page zeroed a hole, and a VM restored from it reads that; the VM
/// persisted reads what it did, its pages written shared again, with the restored VM's, the page
/// it zeroed zeros.
#[test]
fn persists_keep_what_guests_stored_shared_again() {
    let ps = page_size();
    const PAGES: u64 = 400;
    let size = PAGES * ps;
    let mut image = Vec::with_capacity(size as usize);
    for page in 0..PAGES {
        image.extend(pattern(page as u8, ps as usize));
    }
    let versions = Versions::of(&image, 200..230);
    image[(200 * ps) as usize..(230 * ps) as usize].fill(0);
    let (a, _a_serving) = versions.restore("one");
    let persisted = a.serve_maps();
    assert!(a.read(0, size) == image, "the VM's memory holds the image");
    let mut expected = image.clone();
    let mut store = |page: u64, bytes: Vec<u8>| {
        a.store(page * ps, &bytes);
        expected[(page * ps) as usize..((page + 1) * ps) as usize].copy_from_slice(&bytes);
    };
    store(5, pattern(0xa5, ps as usize));
    store(210, pattern(0x77, ps as usize));
    store(7, vec![0; ps as usize]);
    store(399, pattern(0x3c, ps as usize));

    let result = a.persist(&persisted);
    let version: cladefs_core::CellVersion =
        serde_json::from_value(result["version"].clone()).unwrap_or_else(|_| panic!("{result}"));
    assert!(a.read(0, size) == expected, "the VM persisted reads what it stored");
    for page in [5, 210, 399] {
        assert!(maps_file_page(a.addr(page * ps)), "page {page} is private still");
    }
    assert_eq!(mapped_from(a.addr(7 * ps)).0, "00:00 0", "a page zeroed maps zeros");

    let (c, _c_serving) = versions.restore_from(version, "two");
    c.serve_maps();
    assert!(c.read(0, size) == expected, "the VM restored reads what was stored");
    for page in [5, 210, 399, 0] {
        let (at_a, at_c) = (a.addr(page * ps), c.addr(page * ps));
        assert!(maps_file_page(at_c), "page {page} is private in the VM restored");
        assert_eq!(mapped_from(at_a), mapped_from(at_c), "page {page} isn't shared");
    }
}

/// What a VM process of the test's in a process of its own does ([`child_vmm`]): its role, then
/// what it needs, `:` apart.
const CHILD_VMM: &str = "CLADEFSD_TEST_CHILD_VMM";

/// A VM process of the test's in a process of its own, this test run again as a child, which its
/// environment says what to do ([`CHILD_VMM`]):
/// - `connect:SOCKET`: connects, and waits until killed.
/// - `persist-stalls:SIZE:SOCKET`: a VM process of `SIZE` bytes of guest memory, which stores to
///   a page, asks for a persist, and answers no map after: until killed.
#[test]
#[ignore = "a VM process of the tests', which they run as a child"]
fn child_vmm() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let role = std::env::var(CHILD_VMM).expect("a child VM process's role");
    let (role, args) = role.split_once(':').unwrap();
    match role {
        "connect" => {
            let _stream = UnixStream::connect(args).unwrap();
            loop {
                std::thread::park();
            }
        }
        "persist-stalls" => {
            let (size, socket) = args.split_once(':').unwrap();
            let vmm = std::sync::Arc::new(FakeVmm::start(Path::new(socket), size.parse().unwrap()));
            let stalled = std::sync::Arc::new(AtomicBool::new(false));
            let (server, stalls) = (vmm.clone(), stalled.clone());
            std::thread::spawn(move || {
                while let Ok((message, fds)) = protocol::recv::<ToVmm>(&server.stream) {
                    match message {
                        ToVmm::Map { .. } if stalls.load(Ordering::Acquire) => {}
                        ToVmm::Map { id, entries, .. } => server.apply(id, &entries, &fds),
                        other => panic!("{other:?} before the persist's maps"),
                    }
                }
            });
            // A page of its own to persist, its fault served.
            vmm.store(0, &[1]);
            stalled.store(true, Ordering::Release);
            protocol::send(&vmm.stream, &FromVmm::Persist, &[]).unwrap();
            loop {
                std::thread::park();
            }
        }
        _ => panic!("a child VM process's role: {role}"),
    }
}

/// A [`child_vmm`] running: killed and reaped as it is dropped, a test failing included.
struct ChildVmm(std::process::Child);

impl Drop for ChildVmm {
    fn drop(&mut self) {
        // Gone already, if the test waited for it.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Runs [`child_vmm`] in a process of its own, as `role` says.
fn child_vmm_as(role: &str) -> ChildVmm {
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "memory_backend::tests::child_vmm", "--ignored", "--nocapture"])
        .env(CHILD_VMM, role)
        .spawn()
        .unwrap();
    ChildVmm(child)
}

/// The exit of `vmm`, which must come within `within`: waited for on its pidfd.
fn exited_within(vmm: &mut ChildVmm, within: std::time::Duration) -> std::process::ExitStatus {
    use std::os::fd::{FromRawFd, OwnedFd};
    // SAFETY: pidfd_open of the child's PID, which it holds until reaped below.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, vmm.0.id(), 0) };
    assert!(pidfd >= 0, "pidfd_open: {}", std::io::Error::last_os_error());
    // SAFETY: The descriptor just made.
    let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd as i32) };
    let mut poll = libc::pollfd { fd: pidfd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: Polls the pidfd, readable once the process exits.
    let polled = unsafe { libc::poll(&mut poll, 1, within.as_millis() as i32) };
    assert_eq!(polled, 1, "the VM process didn't exit within {within:?}");
    vmm.0.wait().unwrap()
}

/// What `serving` came to, which must come within `within`.
fn joined_within<T: Send + 'static>(
    serving: std::thread::JoinHandle<T>,
    within: std::time::Duration,
) -> T {
    let (joined, done) = std::sync::mpsc::channel();
    std::thread::spawn(move || joined.send(serving.join().unwrap()));
    done.recv_timeout(within).expect("the memory backend didn't end")
}

/// A persist past its deadline kills the VM process, through its pidfd, and ends its memory
/// backend: here, one that answers none of the persist's maps.
#[test]
fn persists_past_their_deadline_kill_the_vm_process() {
    use std::os::unix::process::ExitStatusExt;
    let size = 16 * page_size();
    let versions = Versions::of(&pattern(9, size as usize), 0..0);
    let deadline = std::time::Duration::from_secs(3);
    let (backend, serving) = versions.serve_fork(versions.version, "one", deadline);
    let mut vmm = child_vmm_as(&format!("persist-stalls:{size}:{}", backend.display()));
    let status = exited_within(&mut vmm, 10 * deadline);
    assert_eq!(status.signal(), Some(libc::SIGKILL), "the VM process wasn't killed");
    let ended = joined_within(serving, 10 * deadline);
    assert_eq!(ended, Err("the VM process went".to_owned()));
}

/// A persist asked for as a map waits for its answer is served once the map is answered.
#[test]
fn persists_asked_as_a_map_waits_are_served_after_it() {
    let ps = page_size();
    let image = pattern(6, (16 * ps) as usize);
    let versions = Versions::of(&image, 0..0);
    let (vmm, _serving) = versions.restore("one");
    let toucher = vmm.clone();
    let touched = std::thread::spawn(move || toucher.read(0, 1));
    let (map, fds) = protocol::recv::<ToVmm>(&vmm.stream).unwrap();
    let ToVmm::Map { id, entries, .. } = map else { panic!("{map:?} in place of a map") };
    protocol::send(&vmm.stream, &FromVmm::Persist, &[]).unwrap();
    vmm.apply(id, &entries, &fds);
    let persisted = vmm.serve_maps();
    assert_eq!(touched.join().unwrap(), [image[0]]);
    let result = persisted.recv().unwrap();
    let version: Result<cladefs_core::CellVersion, _> =
        serde_json::from_value(result["version"].clone());
    assert!(version.is_ok(), "{result}");
}

/// A VM process's pidfd names it alone: gone, and reaped, so that another may take its PID, what
/// would read its memory is refused (`ESRCH`), as is killing it.
#[test]
fn peers_gone_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("peer.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let mut vmm = child_vmm_as(&format!("connect:{}", socket.display()));
    let (stream, _) = listener.accept().unwrap();
    let peer = super::peer::Peer::of(&stream).unwrap();
    peer.pagemap().expect("its pagemap, while it runs");
    vmm.0.kill().unwrap();
    vmm.0.wait().unwrap();
    let errno = |err: std::io::Error| err.raw_os_error();
    let mut byte = [0u8];
    assert_eq!(peer.read(page_size(), &mut byte).map_err(errno), Err(Some(libc::ESRCH)));
    assert_eq!(peer.pagemap().map(drop).map_err(errno), Err(Some(libc::ENOENT)));
    assert_eq!(peer.check().map_err(errno), Err(Some(libc::ESRCH)));
    assert_eq!(peer.kill().map_err(errno), Err(Some(libc::ESRCH)));
}
