// Copyright 2026 The cladefs Authors.
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Tests of physical memory in arenas (`physmap`, TASK-43), lifted from cladefs's port of the Zircon
//! kernel (`vm::unittests::arena_unittest`): those of the arenas themselves, which allocate pages
//! now rather than VMOs, and starved arenas relieved by relief's evictor rather than the kernel's.

#[cfg(target_os = "linux")]
use super::{chunk_arena, fill, first_byte};
use super::{chunk_of, chunk_pages};
use crate::{
    ArenaId, CHUNK_SIZE, NoMemory, Page, Wait, alloc, arena_bytes, arena_fd, count_free_chunks,
    create_arena, has_alloc_failed_no_mem, keep_spare_chunks, page_size, relieve_with,
    set_arena_cap,
};
#[cfg(target_os = "linux")]
use crate::{arenas_bytes, default_fd, page_bytes, set_total_cap};
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const CHUNK: u64 = CHUNK_SIZE as u64;

/// `count` pages of `arena`'s.
fn alloc_pages(arena: ArenaId, count: usize) -> Vec<Page> {
    (0..count).map(|_| alloc(arena, Wait::No).unwrap()).collect()
}

/// Runs `f` with the host mapping no chunk of `arena`'s, as when the process makes as many mappings
/// as it may (`vm.max_map_count`): the arena's object made a read-only file, which the host does
/// not map writable. The object must have a slot free for each chunk `f` maps: on Linux, the object
/// grown for one, which it can't be meanwhile, would fail otherwise.
fn with_chunks_unmapped<R>(arena: ArenaId, f: impl FnOnce() -> R) -> R {
    let object = arena_fd(arena).unwrap();
    let writable = object.try_clone().unwrap();
    let read_only = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
    // SAFETY: Makes the descriptor of the arena's object a read-only file's, until it is made the
    // object's again below.
    let made = unsafe { libc::dup2(read_only.as_raw_fd(), object.as_raw_fd()) };
    assert_eq!(made, object.as_raw_fd(), "dup2: {}", std::io::Error::last_os_error());
    let result = f();
    // SAFETY: Makes the descriptor the arena's object's again.
    let made = unsafe { libc::dup2(writable.as_raw_fd(), object.as_raw_fd()) };
    assert_eq!(made, object.as_raw_fd(), "dup2: {}", std::io::Error::last_os_error());
    result
}

/// TASK-79.37: a chunk the host does not map stays free, and so does the slot of the arena's
/// object it would have taken: every way of allocating the page it was for fails at once, whether
/// or not it may wait, as memory out; and the next allocation, once the host maps it, takes both.
#[test]
fn chunks_the_host_does_not_map_stay_free() {
    keep_spare_chunks(false);
    let arena = create_arena();
    // The arena's object's slot free, which its next chunk takes.
    drop(alloc(arena, Wait::No).unwrap());
    let free_chunks = count_free_chunks();

    with_chunks_unmapped(arena, || {
        for wait in [Wait::No, Wait::Yes] {
            assert_eq!(alloc(arena, wait).err(), Some(NoMemory), "{wait:?}");
        }
    });
    assert_eq!(count_free_chunks(), free_chunks, "a chunk lost");
    assert_eq!(arena_bytes(arena), 0);
    assert!(has_alloc_failed_no_mem(), "the host's refusal is not memory out");

    let page = alloc(arena, Wait::No).expect("a chunk the host maps");
    assert_eq!(page.location().offset, 0, "the slot, free still");
    assert_eq!(count_free_chunks(), free_chunks - 1);
}

/// TASK-79.37: an arena starting on its spare chunk while the host maps no chunk takes its page,
/// and does without a spare. Nor do arenas ask the host for one again, which it would refuse too,
/// each allocation in vain, until it maps, or takes back, a chunk: then the arena takes its spare.
#[test]
fn spares_the_host_does_not_map_are_done_without() {
    let (arena, other) = (create_arena(), create_arena());
    // A chunk and a spare; the chunk gone back, its slot of the arena's object free, which the
    // arena's next spare takes.
    drop(alloc(arena, Wait::No).unwrap());
    assert_eq!(arena_bytes(arena), CHUNK, "its spare");
    let free_chunks = count_free_chunks();

    let page = with_chunks_unmapped(arena, || alloc(arena, Wait::No)).expect("a page of its spare");
    assert_eq!(arena_bytes(arena), CHUNK, "a spare taken");
    assert_eq!(count_free_chunks(), free_chunks, "a chunk lost");

    // The host would map one now, and is not asked to.
    let next = alloc(arena, Wait::No).unwrap();
    assert_eq!(chunk_of(&next), chunk_of(&page));
    assert_eq!(arena_bytes(arena), CHUNK, "a spare asked of a refusing host");
    // It maps a chunk, another arena's.
    let others = alloc(other, Wait::No).unwrap();
    let last = alloc(arena, Wait::No).unwrap();
    assert_eq!(chunk_of(&last), chunk_of(&page));
    assert_eq!(arena_bytes(arena), 2 * CHUNK, "a spare once the host maps one");
    drop([page, next, others, last]);
}

/// The mappings the process may make beside those it has (`vm.max_map_count`), taken while this
/// lives, until the host maps nothing more: pages of a shared mapping of its own, which merges with
/// none, every other one made readable, each so a mapping of its own, until the host splits it no
/// more; then a page each, shared memory of its own, until it maps no more, as Linux may map one
/// mapping more than it splits one for (`do_mmap` refusing past its limit, `split_vma` at it). The
/// host then maps no chunk, whether its reservation is split around it or shortened, nor reserves
/// one again in the middle of its arena's mapping.
#[cfg(target_os = "linux")]
struct MappingsTaken {
    split: *mut libc::c_void,
    len: usize,
    pages: Vec<*mut libc::c_void>,
}

#[cfg(target_os = "linux")]
impl MappingsTaken {
    fn new() -> Self {
        let max = std::fs::read_to_string("/proc/sys/vm/max_map_count").unwrap();
        // Two pages a mapping: more than the process may make.
        let len = 2 * max.trim().parse::<usize>().unwrap() * page_bytes();
        let split = map_shared(len).expect("a mapping to split");
        // The thread's heap grown, and its memory freed, before the mappings run out: what the
        // thread allocates meanwhile, as the PMM logs, it takes from the heap as it is, rather than
        // grow it, which the host would refuse, and the process abort.
        drop(std::hint::black_box((0..16).map(|_| vec![1u8; 64 << 10]).collect::<Vec<_>>()));
        // Each page made readable splits what is left of the mapping: two mappings more, until the
        // host refuses, at its limit.
        let mut offset = page_bytes();
        // SAFETY: Changes the protection of a page of the mapping, which this owns.
        while unsafe { libc::mprotect(split.byte_add(offset), page_bytes(), libc::PROT_READ) } == 0
        {
            offset += 2 * page_bytes();
        }
        let error = std::io::Error::last_os_error();
        assert_eq!(error.raw_os_error(), Some(libc::ENOMEM), "mprotect: {error}");
        assert!(offset < len, "the mapping is too small to take every mapping");
        // Room for the few the host may map still, so that none grows the heap.
        let mut pages = Vec::with_capacity(16);
        let refused = (0..pages.capacity()).find_map(|_| match map_shared(page_bytes()) {
            Ok(page) => {
                pages.push(page);
                None
            }
            Err(error) => Some(error),
        });
        let refused = refused.unwrap_or_else(|| panic!("the host maps on: {} pages", pages.len()));
        assert_eq!(refused.raw_os_error(), Some(libc::ENOMEM), "mmap: {refused}");
        Self { split, len, pages }
    }
}

/// A new shared mapping of `len` bytes, of memory of its own, which merges with no other, never
/// touched.
#[cfg(target_os = "linux")]
fn map_shared(len: usize) -> std::io::Result<*mut libc::c_void> {
    // SAFETY: Maps memory of its own, with no address hint.
    let addr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_NONE,
            libc::MAP_SHARED | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if addr == libc::MAP_FAILED { Err(std::io::Error::last_os_error()) } else { Ok(addr) }
}

#[cfg(target_os = "linux")]
impl Drop for MappingsTaken {
    fn drop(&mut self) {
        // SAFETY: Unmaps the mappings, which nothing else uses: whole mappings, none split.
        let status = unsafe { libc::munmap(self.split, self.len) };
        assert_eq!(status, 0, "munmap: {}", std::io::Error::last_os_error());
        for &page in &self.pages {
            // SAFETY: As above.
            let status = unsafe { libc::munmap(page, page_bytes()) };
            assert_eq!(status, 0, "munmap: {}", std::io::Error::last_os_error());
        }
    }
}

/// TASK-79.37: a chunk the host does not reserve again as it goes back, as reserving it splits
/// the mapping of its arena's chunks around it when the process makes as many mappings as it may
/// (`vm.max_map_count`), stays its arena's, its pages free, as its spare, and goes back with the
/// spares once the host takes it. Meanwhile another arena takes no chunk, failing at once, though
/// it may wait, and relief is not asked for: neither one free, which the host does not map, nor,
/// at the total cap, that spare, which it does not take back.
#[cfg(target_os = "linux")]
#[test]
fn chunks_the_host_does_not_take_back_stay_their_arenas() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    keep_spare_chunks(false);
    let (arena, other) = (create_arena(), create_arena());
    // Relief started before the mappings are taken, its thread's stack one of them.
    let relieved = Arc::new(AtomicUsize::new(0));
    relieve_with({
        let relieved = relieved.clone();
        move |_, _| {
            relieved.fetch_add(1, Ordering::Relaxed);
            0
        }
    })
    .unwrap();
    // Three chunks, next to one another, in the first three slots of the arena's object: one
    // mapping, which the middle one going back splits.
    let mut pages = alloc_pages(arena, 3 * chunk_pages());
    let chunks: Vec<usize> = pages.iter().step_by(chunk_pages()).map(chunk_of).collect();
    assert_eq!(chunks, [chunks[0], chunks[0] + 1, chunks[0] + 2]);
    let offsets: Vec<u64> =
        pages.iter().step_by(chunk_pages()).map(|page| page.location().offset).collect();
    assert_eq!(offsets, [0, CHUNK, 2 * CHUNK]);
    let free_chunks = count_free_chunks();

    let taken = MappingsTaken::new();
    drop(pages.drain(chunk_pages()..2 * chunk_pages()));
    assert_eq!(chunk_arena(chunks[1]), Some(arena), "the middle chunk went back");
    assert_eq!(arena_bytes(arena), 3 * CHUNK);
    assert_eq!(alloc(other, Wait::Yes).err(), Some(NoMemory));
    set_total_cap(Some(arenas_bytes()));
    assert_eq!(alloc(other, Wait::Yes).err(), Some(NoMemory));
    assert_eq!(chunk_arena(chunks[1]), Some(arena), "the spare went back");
    set_total_cap(None);
    assert_eq!(count_free_chunks(), free_chunks, "a chunk lost");
    drop(taken);
    assert_eq!(relieved.load(Ordering::Relaxed), 0, "relief asked for");

    crate::release_spares(Some(arena));
    assert_eq!(chunk_arena(chunks[1]), None, "the middle chunk, its spare, stayed");
    assert_eq!(arena_bytes(arena), 2 * CHUNK);
    pages.push(alloc(other, Wait::No).expect("a chunk the host maps"));
}

/// TASK-79.76: relief asked for an arena again as its pass runs, by an allocation the pass then
/// makes room for, runs no pass of its own, as the arena is starved no longer: one would evict as
/// much again of the arena's pages for no allocation. Another arena, starved at its cap, asked for
/// after, and so relieved after, shows when the pass would have run.
#[test]
fn relief_asked_for_as_a_pass_relieves_the_arena_runs_no_pass_more() {
    keep_spare_chunks(false);
    let (arena, after) = (create_arena(), create_arena());
    assert!(arena < after, "the arena's ask taken first");
    for of in [arena, after] {
        set_arena_cap(of, Some(CHUNK));
    }
    let mut pages = alloc_pages(arena, chunk_pages());
    let held = alloc_pages(after, chunk_pages());
    // What the evictor frees of the arena's, once: four pages.
    let to_free = Arc::new(Mutex::new(pages.split_off(chunk_pages() - 4)));
    // The arenas relief's evictor is called for; and what the arena's allocation asking again, as
    // its first pass runs, the arena's pages not yet freed, was told.
    let (called, calls) = std::sync::mpsc::channel();
    let asked_again = Arc::new(Mutex::new(None));
    relieve_with({
        let (to_free, asked_again) = (to_free.clone(), asked_again.clone());
        move |from, _| {
            let mut asked = asked_again.lock().unwrap();
            if from == Some(arena) && asked.is_none() {
                *asked = Some(alloc(arena, Wait::No).err());
            }
            drop(asked);
            let freed = std::mem::take(&mut *to_free.lock().unwrap());
            let bytes = freed.len() as u64 * page_size();
            drop(freed);
            // Calls after the test's are let be.
            let _ = called.send(from);
            bytes
        }
    })
    .unwrap();

    pages.push(alloc(arena, Wait::Yes).expect("a page of the arena's, freed"));
    assert_eq!(alloc(after, Wait::Yes).err(), Some(NoMemory), "nothing of its own freed");
    let mut called_for = Vec::new();
    while called_for.last() != Some(&Some(after)) {
        called_for.push(calls.recv_timeout(Duration::from_secs(10)).expect("relief's pass"));
    }
    assert_eq!(*asked_again.lock().unwrap(), Some(Some(NoMemory)));
    assert_eq!(called_for, [Some(arena), Some(after)]);
    drop((pages, held));
}

/// TASK-50.3: relief gone, as its evictor panicked, the allocations it would make room for fail,
/// those waiting for it too.
#[test]
fn allocations_fail_once_relief_is_gone() {
    keep_spare_chunks(false);
    let arena = create_arena();
    set_arena_cap(arena, Some(CHUNK));
    let pages = alloc_pages(arena, chunk_pages());
    relieve_with(|_, _| panic!("an evictor that fails")).unwrap();
    assert_eq!(alloc(arena, Wait::Yes).err(), Some(NoMemory));
    assert_eq!(alloc(arena, Wait::Yes).err(), Some(NoMemory), "relief gone");
    drop(pages);
}

/// cladefs TASK-149.17: an arena's object, which the extent daemon hands its tenant's VM processes
/// to write, and the default's, which it hands them all to read, is sealed against shrinking,
/// which would have the pages past its new end fault this process's own accesses (`SIGBUS`); and
/// is its owner's alone to open anew, so that a process of another user, handed it read-only,
/// can't reopen it to write through `/proc/self/fd`. One a process grew, as it may, still takes
/// chunks, its pages as they were.
#[cfg(target_os = "linux")]
#[test]
fn arenas_objects_are_sealed_and_their_owners() {
    let arena = create_arena();
    let page = alloc(arena, Wait::No).unwrap();
    fill(&page, 0x3c);
    // So that each object holds a chunk, which a shrink to nothing would take away.
    let _default = alloc(ArenaId::DEFAULT, Wait::No).unwrap();
    let fd = arena_fd(arena).unwrap();
    for fd in [fd.as_raw_fd(), default_fd().as_raw_fd()] {
        // SAFETY: Plain system calls, of descriptors the test holds.
        unsafe {
            let seals = libc::fcntl(fd, libc::F_GET_SEALS);
            assert_eq!(seals & libc::F_SEAL_SHRINK, libc::F_SEAL_SHRINK, "seals {seals:#x}");
            let mut stat: libc::stat = std::mem::zeroed();
            assert_eq!(libc::fstat(fd, &mut stat), 0);
            assert_eq!(stat.st_mode & 0o777, 0o600);
            assert_eq!(libc::ftruncate(fd, 0), -1);
            assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EPERM));
        }
    }
    // Grown by a tebibyte, as a VM process handed it may.
    // SAFETY: As above.
    unsafe {
        let mut stat: libc::stat = std::mem::zeroed();
        assert_eq!(libc::fstat(fd.as_raw_fd(), &mut stat), 0);
        assert_eq!(libc::ftruncate(fd.as_raw_fd(), stat.st_size + (1 << 40)), 0);
    }
    let more = alloc_pages(arena, 2 * chunk_pages());
    fill(more.last().unwrap(), 0xc3);
    assert_eq!(first_byte(&page), 0x3c);
    assert_eq!(first_byte(more.last().unwrap()), 0xc3);
}
