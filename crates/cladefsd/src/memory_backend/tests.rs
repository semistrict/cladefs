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
    /// test ends.
    fn serve_maps(self: &std::sync::Arc<Self>) {
        let vmm = self.clone();
        std::thread::spawn(move || {
            while let Ok((ToVmm::Map { id, entries, .. }, fds)) =
                protocol::recv::<ToVmm>(&vmm.stream)
            {
                vmm.apply(id, &entries, &fds);
            }
        });
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
        let path = self.dir.path().join(name);
        MemoryCell::open_forked(&self.socket, &path, self.version, self.size)
            .unwrap()
            .close()
            .unwrap();
        let backend = self.dir.path().join(format!("{name}.sock"));
        let listener = UnixListener::bind(&backend).unwrap();
        let serving = super::serve(listener, default_tenant().unwrap(), path);
        (std::sync::Arc::new(FakeVmm::start(&backend, self.size)), serving)
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
