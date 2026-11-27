// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A memory cell's window as a guest's RAM on Hypervisor.framework (board 13994, board 14343): the
//! RAM's host memory, which the VMM's devices read and write as the guest does, holds the window's
//! pages itself, mapped copy-on-write from the arenas' memory objects, so that the guest and the
//! VMM share the daemon's cached pages and each write to one makes it the VM's own, the kernel's
//! copy.
//!
//! The DAX window's stage ([`super::HvfSecondStage`]) maps pages into the guest alone, from
//! mappings of its own, and makes a page private in memory of its own as the guest's write
//! exits: the VMM never touches a DAX window. RAM it touches, through its devices' queues and
//! buffers, at the same host addresses the guest's RAM is mapped from, so the RAM's host memory
//! must be what the guest sees: and macOS has no userfaultfd to make the VMM's own accesses fault
//! in what isn't there yet (`dax::uffd`, Linux's).
//!
//! So the window's pages are all there before the guest runs: the cell's whole image is mapped
//! (`memory_cell`), each data page mapped privately from the daemon's page of it
//! (`MAP_PRIVATE`), each hole left as the RAM's anonymous zeros. A write, the guest's or a
//! device's, copies the page in the kernel, which Hypervisor.framework follows (measured on macOS
//! 27: libmachine's `coherence_tests`). The private pages are the RAM's anonymous ones:
//! [`HvfRamStage::private_runs`] asks the kernel (`mincore`), which says so of a copy and of a
//! hole written, resident or compressed.
//!
//! Hypervisor.framework binds a guest page to the host's page as it maps it, and follows no later
//! change of the host's mapping there: each change made here is followed by [`GuestRam::rebind`],
//! which maps the range into the guest anew.
//!
//! The daemon's changes: a page mapped anew (a reclaim's, as the guest gave it back) replaces what
//! is there, its private copy too; a page the daemon takes back (an eviction) is copied first, so
//! that the RAM keeps it, the VM's own from then on, as the host's changes leave private pages
//! ([`Private::Keep`]); one the guest's `MAP` or `UNMAP` ends is the RAM's anonymous zeros again.
//!
//! Its unit is the host's page (16 KiB on Apple Silicon), which the daemon stages windows in: a
//! page written is copied whole.

use super::super::{Anew, Private, SecondStage};
use crate::error::{Errno, Error, Result};
use crate::pages::{ArenaId, PhysLocation, page_size};
use std::collections::BTreeMap;
use std::ops::Range;
use std::os::fd::{AsRawFd, OwnedFd};
use std::ptr::NonNull;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};

/// What only the VMM can do for a memory window in its guest's RAM: a reservation of host memory
/// that the guest's RAM is mapped from, and its mapping into the guest anew.
pub trait GuestRam: Send + Sync {
    /// The RAM's host memory: [`size`](Self::size) bytes, page-aligned, readable and writable,
    /// which nothing but the window maps anything over.
    fn host(&self) -> NonNull<u8>;

    /// The RAM's bytes.
    fn size(&self) -> u64;

    /// The host's mapping of the `len` bytes at `offset` changed: maps them into the guest anew,
    /// from what is there now, the guest's accesses there waiting meanwhile. Nothing before the
    /// guest's RAM is mapped. A failure leaves the guest's memory unlike the RAM's: the VMM
    /// panics.
    fn rebind(&self, offset: u64, len: u64);
}

/// `mincore`'s bits (`<sys/mman.h>`): resident, compressed or paged out, and of an anonymous
/// object, which a copy on write of a file's page is.
const MINCORE_INCORE: u8 = 0x1;
const MINCORE_PAGED_OUT: u8 = 0x20;
const MINCORE_ANONYMOUS: u8 = 0x80;

/// Whether `mincore`'s `bits` are those of a page of the RAM's own: one written, or a hole the
/// guest read, which the kernel gave a page of zeros (that a snapshot writes as a hole).
fn private(bits: u8) -> bool {
    bits & MINCORE_ANONYMOUS != 0 && bits & (MINCORE_INCORE | MINCORE_PAGED_OUT) != 0
}

/// A memory cell's window as a guest's RAM (see the module docs).
pub struct HvfRamStage {
    ram: std::sync::Arc<dyn GuestRam>,
    size: u64,
    page: u64,
    /// The arenas' memory objects, which the window's pages are of.
    memories: Vec<(ArenaId, OwnedFd)>,
    state: Mutex<State>,
}

impl std::fmt::Debug for HvfRamStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HvfRamStage").field("size", &self.size).finish_non_exhaustive()
    }
}

/// What the RAM's pages are, those of neither map being its anonymous memory.
struct State {
    /// The pages mapped from the daemon's, by offset, each its page in an arena: shared until
    /// written.
    named: BTreeMap<u64, PhysLocation>,
    /// Let go of ([`HvfRamStage::release`]): nothing is mapped from the daemon any more.
    released: bool,
}

impl HvfRamStage {
    /// A window of `ram`'s whole size, a multiple of the host's page, its pages of `memories`'
    /// arenas. The RAM must be its anonymous zeros.
    pub fn new(
        ram: std::sync::Arc<dyn GuestRam>,
        memories: Vec<(ArenaId, OwnedFd)>,
    ) -> Result<std::sync::Arc<Self>, Error> {
        let (size, page) = (ram.size(), page_size());
        if size == 0
            || !size.is_multiple_of(page)
            || !(ram.host().as_ptr() as u64).is_multiple_of(page)
        {
            return Err(Error::new(Errno::EINVAL).context(format!(
                "RAM of {size:#x} bytes at {:p}: not whole host pages",
                ram.host()
            )));
        }
        Ok(std::sync::Arc::new(Self {
            ram,
            size,
            page,
            memories,
            state: Mutex::new(State { named: BTreeMap::new(), released: false }),
        }))
    }

    /// The window's size in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The host address of the RAM's byte at `offset`, within it.
    fn at(&self, offset: u64) -> *mut u8 {
        debug_assert!(offset <= self.size);
        // SAFETY: Within the RAM's reservation, as `offset` is.
        unsafe { self.ram.host().as_ptr().add(offset as usize) }
    }

    /// The `mincore` bits of each host page of `range`, whole pages within the RAM.
    fn residency(&self, range: Range<u64>) -> Vec<u8> {
        let mut bits = vec![0u8; ((range.end - range.start) / self.page) as usize];
        if bits.is_empty() {
            return bits;
        }
        // SAFETY: Whole pages of the RAM's reservation, mapped as long as the stage; a byte for
        // each.
        let ret = unsafe {
            libc::mincore(
                self.at(range.start).cast(),
                (range.end - range.start) as usize,
                bits.as_mut_ptr().cast(),
            )
        };
        assert_eq!(
            ret,
            0,
            "mincore of the RAM's {:#x}..{:#x}: {}",
            range.start,
            range.end,
            std::io::Error::last_os_error()
        );
        bits
    }

    /// Asserts that `offset..offset + len` is whole host pages within the RAM: the daemon stages
    /// windows in host pages.
    fn check(&self, offset: u64, len: u64) {
        assert!(
            offset.is_multiple_of(self.page)
                && len.is_multiple_of(self.page)
                && offset.checked_add(len).is_some_and(|end| end <= self.size),
            "a change of {len:#x} bytes at {offset:#x} of RAM of {:#x}: not whole {:#x}-byte pages",
            self.size,
            self.page
        );
    }

    /// Maps the arena's pages at `phys` privately over the RAM's `len` bytes at `offset`.
    fn map_from(&self, offset: u64, phys: PhysLocation, len: u64) {
        let fd = (self.memories.iter().find(|(arena, _)| *arena == phys.arena))
            .unwrap_or_else(|| panic!("a page of arena {:?}, not the window's", phys.arena))
            .1
            .as_raw_fd();
        let at = self.at(offset);
        // SAFETY: Replaces whole pages of the RAM's reservation, which the window alone maps,
        // with a private mapping of an open descriptor's object; checked below.
        let mapped = unsafe {
            libc::mmap(
                at.cast(),
                len as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_FIXED,
                fd,
                phys.offset as libc::off_t,
            )
        };
        assert_eq!(
            mapped,
            at.cast(),
            "mapping {len:#x} bytes of arena {:?} at {:#x} over the RAM's {offset:#x}: {}",
            phys.arena,
            phys.offset,
            std::io::Error::last_os_error()
        );
    }

    /// Makes the RAM's `len` bytes at `offset` its anonymous zeros again, what was there gone.
    fn zero(&self, offset: u64, len: u64) {
        let at = self.at(offset);
        // SAFETY: Replaces whole pages of the RAM's reservation, which the window alone maps,
        // with new anonymous memory; checked below.
        let mapped = unsafe {
            libc::mmap(
                at.cast(),
                len as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED,
                -1,
                0,
            )
        };
        assert_eq!(
            mapped,
            at.cast(),
            "zeroing the RAM's {len:#x} bytes at {offset:#x}: {}",
            std::io::Error::last_os_error()
        );
    }

    /// Copies the RAM's host page at `offset` in the kernel, as a write to it would: the RAM's
    /// own from then on, whatever the daemon makes of the page it was mapped from. A write of
    /// what is there, atomically, so that a write of the guest's or a device's at once is kept.
    fn copy(&self, offset: u64) {
        // SAFETY: A byte of the RAM, mapped readable and writable as long as the stage; the
        // guest's and the devices' accesses to it are the machine's, outside this process's
        // memory model, as volatile ones are.
        unsafe { AtomicU8::from_ptr(self.at(offset)) }.fetch_or(0, Ordering::SeqCst);
    }

    /// Maps `phys`'s pages over the RAM's `len` bytes at `offset`, but its private pages there,
    /// which stay its own, unless `anew`, the guest's map, which ends them. The state's lock held;
    /// the guest's RAM not rebound.
    fn map_locked(&self, state: &mut State, offset: u64, phys: PhysLocation, len: u64) {
        let residency = self.residency(offset..offset + len);
        let mut run: Option<(u64, u64)> = None;
        let flush = |run: &mut Option<(u64, u64)>| {
            if let Some((start, len)) = run.take() {
                let delta = start - offset;
                self.map_from(
                    start,
                    PhysLocation { arena: phys.arena, offset: phys.offset + delta },
                    len,
                );
            }
        };
        for (i, bits) in residency.into_iter().enumerate() {
            let at = offset + i as u64 * self.page;
            if private(bits) {
                flush(&mut run);
                state.named.remove(&at);
                continue;
            }
            state.named.insert(
                at,
                PhysLocation { arena: phys.arena, offset: phys.offset + (at - offset) },
            );
            match &mut run {
                Some((start, len)) if *start + *len == at => *len += self.page,
                _ => {
                    flush(&mut run);
                    run = Some((at, self.page));
                }
            }
        }
        flush(&mut run);
    }

    /// Unmaps the window's `len` bytes at `offset`, as `private` says, the state's lock held:
    /// the daemon's pages there let go of, each copied first if the host takes it back
    /// ([`Private::Keep`]), the RAM's anonymous zeros there if the guest's map or unmap ends
    /// them ([`Private::End`]). Whether the guest's RAM is to be rebound there.
    fn unmap_locked(&self, state: &mut State, offset: u64, len: u64, private: Private) -> bool {
        let named: Vec<u64> = state.named.range(offset..offset + len).map(|(&at, _)| at).collect();
        for at in &named {
            state.named.remove(at);
        }
        match private {
            Private::Keep => {
                for at in named {
                    self.copy(at);
                }
                false
            }
            Private::End => {
                self.zero(offset, len);
                true
            }
        }
    }

    /// The host pages of `ranges` the daemon has mapped nothing of here, nor are the RAM's own:
    /// what of the window's image is not there yet, by offset.
    pub fn unnamed(&self, ranges: &[Range<u64>]) -> Vec<u64> {
        let state = self.state.lock().unwrap();
        let mut unnamed = Vec::new();
        for range in ranges {
            let start = range.start - range.start % self.page;
            let end = range.end.next_multiple_of(self.page).min(self.size);
            for (i, bits) in self.residency(start..end).into_iter().enumerate() {
                let at = start + i as u64 * self.page;
                if !private(bits) && !state.named.contains_key(&at) {
                    unnamed.push(at);
                }
            }
        }
        unnamed
    }

    /// Makes the RAM's `len` bytes at `offset`, whole host pages, its anonymous zeros again, what
    /// it held there gone, private pages too: a hole the guest gave back, which the daemon maps
    /// nothing at.
    pub fn discard(&self, offset: u64, len: u64) {
        self.check(offset, len);
        let mut state = self.state.lock().unwrap();
        if state.released || len == 0 {
            return;
        }
        self.unmap_locked(&mut state, offset, len, Private::End);
        self.ram.rebind(offset, len);
    }

    /// The runs of the RAM's private pages, its own, by offset, in order: those written since
    /// the window was made, or since they were last mapped anew.
    pub fn private_runs(&self) -> Result<Vec<Range<u64>>> {
        // A chunk's bits at a time: a byte a page.
        const CHUNK: u64 = 1 << 30;
        let mut runs: Vec<Range<u64>> = Vec::new();
        let mut start = 0;
        while start < self.size {
            let end = (start + CHUNK).min(self.size);
            for (i, bits) in self.residency(start..end).into_iter().enumerate() {
                if !private(bits) {
                    continue;
                }
                let at = start + i as u64 * self.page;
                match runs.last_mut() {
                    Some(run) if run.end == at => run.end += self.page,
                    _ => runs.push(at..at + self.page),
                }
            }
            start = end;
        }
        Ok(runs)
    }

    /// How many of the RAM's pages are private: host pages, as the kernel copies them, and as
    /// the daemon counts the pages it maps (`window_pages`).
    pub fn private_pages(&self) -> u64 {
        let runs = self.private_runs().unwrap_or_default();
        runs.iter().map(|run| (run.end - run.start) / self.page).sum()
    }

    /// How many of its pages are mapped from the daemon's: host pages.
    pub fn installed(&self) -> u64 {
        self.state.lock().unwrap().named.len() as u64
    }

    /// The pages it may keep mapped, which the daemon's metrics show: every one, as the
    /// hypervisor bounds none.
    pub fn budget(&self) -> usize {
        (self.size / self.page) as usize
    }

    /// Lets go of every page the daemon's are: each the RAM's anonymous zeros, so that nothing of
    /// the RAM reads a page the daemon may make another's, as the cell's connection goes. The
    /// guest's RAM is gone by then, or broken.
    pub fn release(&self) {
        let mut state = self.state.lock().unwrap();
        if state.released {
            return;
        }
        state.released = true;
        let named: Vec<u64> = std::mem::take(&mut state.named).into_keys().collect();
        let mut runs: Vec<(u64, u64)> = Vec::new();
        for at in named {
            match runs.last_mut() {
                Some((start, len)) if *start + *len == at => *len += self.page,
                _ => runs.push((at, self.page)),
            }
        }
        for (start, len) in runs {
            self.zero(start, len);
            self.ram.rebind(start, len);
        }
    }
}

impl SecondStage for HvfRamStage {
    fn map(&self, offset: u64, phys: PhysLocation, len: u64) {
        self.remap(offset, 0, Private::Keep, &[Anew::Map { offset, phys, len }]);
    }

    fn unmap(&self, offset: u64, len: u64, private: Private) {
        self.remap(offset, len, private, &[]);
    }

    fn remap(&self, offset: u64, len: u64, private: Private, anew: &[Anew]) {
        self.check(offset, len);
        let mut state = self.state.lock().unwrap();
        if state.released {
            return;
        }
        let mut changed: Vec<Range<u64>> = Vec::new();
        if len > 0 && self.unmap_locked(&mut state, offset, len, private) {
            changed.push(offset..offset + len);
        }
        for anew in anew {
            if let Anew::Map { offset, phys, len } = *anew {
                self.check(offset, len);
                assert!(phys.offset.is_multiple_of(self.page), "a map from {phys:?}");
                self.map_locked(&mut state, offset, phys, len);
                changed.push(offset..offset + len);
            }
        }
        changed.sort_by_key(|range| range.start);
        let mut joined: Vec<Range<u64>> = Vec::new();
        for range in changed {
            match joined.last_mut() {
                Some(last) if last.end >= range.start => last.end = last.end.max(range.end),
                _ => joined.push(range),
            }
        }
        for range in joined {
            self.ram.rebind(range.start, range.end - range.start);
        }
    }
}

impl Drop for HvfRamStage {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::sync::Arc;

    /// RAM as a test has it: an anonymous reservation, and the rebinds asked of it.
    struct Reserved {
        host: NonNull<u8>,
        size: u64,
        rebinds: Mutex<Vec<(u64, u64)>>,
    }

    // SAFETY: A reservation of this process's, which the test alone uses.
    unsafe impl Send for Reserved {}
    // SAFETY: As above.
    unsafe impl Sync for Reserved {}

    impl Reserved {
        fn new(pages: u64) -> Arc<Self> {
            let size = pages * page_size();
            // SAFETY: New anonymous memory, checked.
            let host = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    size as usize,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANON,
                    -1,
                    0,
                )
            };
            assert_ne!(host, libc::MAP_FAILED);
            Arc::new(Self {
                host: NonNull::new(host.cast()).unwrap(),
                size,
                rebinds: Mutex::default(),
            })
        }

        /// The byte at `at` of page `i`.
        fn byte(&self, i: u64, at: u64) -> *mut u8 {
            assert!(at < page_size() && i * page_size() < self.size);
            // SAFETY: Within the reservation.
            unsafe { self.host.as_ptr().add((i * page_size() + at) as usize) }
        }

        fn get(&self, i: u64, at: u64) -> u8 {
            // SAFETY: A byte of the reservation, which the test alone uses.
            unsafe { self.byte(i, at).read() }
        }

        fn set(&self, i: u64, at: u64, value: u8) {
            // SAFETY: As above.
            unsafe { self.byte(i, at).write(value) }
        }
    }

    impl GuestRam for Reserved {
        fn host(&self) -> NonNull<u8> {
            self.host
        }
        fn size(&self) -> u64 {
            self.size
        }
        fn rebind(&self, offset: u64, len: u64) {
            self.rebinds.lock().unwrap().push((offset, len));
        }
    }

    impl Drop for Reserved {
        fn drop(&mut self) {
            // SAFETY: The reservation's own mapping.
            unsafe { libc::munmap(self.host.as_ptr().cast(), self.size as usize) };
        }
    }

    /// A stage over 8 pages of RAM, its arena 4 pages, page `i` of `i + 1`s.
    fn stage() -> (Arc<HvfRamStage>, Arc<Reserved>, std::fs::File) {
        let ps = page_size() as usize;
        let mut arena = tempfile::tempfile().unwrap();
        for i in 0..4u8 {
            arena.write_all(&vec![i + 1; ps]).unwrap();
        }
        let ram = Reserved::new(8);
        let memories = vec![(ArenaId::DEFAULT, OwnedFd::from(arena.try_clone().unwrap()))];
        (HvfRamStage::new(ram.clone(), memories).unwrap(), ram, arena)
    }

    fn at(page: u64) -> PhysLocation {
        PhysLocation { arena: ArenaId::DEFAULT, offset: page * page_size() }
    }

    /// The daemon's pages are the RAM's, shared until written; a write makes the page private,
    /// and a map over it keeps it, as the host's changes keep private pages.
    #[test]
    fn written_pages_are_private_and_stay_through_maps() {
        let (stage, ram, _arena) = stage();
        let ps = page_size();
        stage.map(0, at(0), 4 * ps);
        assert_eq!((ram.get(0, 0), ram.get(3, ps - 1)), (1, 4));
        assert_eq!(stage.private_runs().unwrap(), []);
        assert_eq!(stage.installed(), 4);
        ram.set(1, 7, 9);
        // A hole, written.
        ram.set(6, 0, 5);
        assert_eq!(stage.private_runs().unwrap(), [ps..2 * ps, 6 * ps..7 * ps]);
        stage.map(0, at(2), 2 * ps);
        assert_eq!((ram.get(0, 0), ram.get(1, 7), ram.get(1, 0)), (3, 9, 2));
        assert_eq!(*ram.rebinds.lock().unwrap(), [(0, 4 * ps), (0, 2 * ps)]);
    }

    /// The host's taking a page back copies it first: the RAM keeps what it read there, whatever
    /// the arena's page becomes; the guest's unmap makes it zeros, its private pages gone.
    #[test]
    fn pages_taken_back_are_kept_and_the_guests_unmap_ends_them() {
        use std::os::unix::fs::FileExt as _;
        let (stage, ram, arena) = stage();
        let ps = page_size();
        stage.map(0, at(0), 2 * ps);
        stage.unmap(0, 2 * ps, Private::Keep);
        arena.write_all_at(&vec![7; ps as usize], 0).unwrap();
        assert_eq!(ram.get(0, 0), 1);
        let (copied, three) = (0..2 * ps, 0..3 * ps);
        assert_eq!(stage.private_runs().unwrap(), [copied]);
        assert_eq!(stage.unnamed(&[three]), [2 * ps]);
        stage.unmap(0, 2 * ps, Private::End);
        // Asked before the pages are read: a read of anonymous memory makes it resident, a page
        // of zeros the RAM's own as far as the kernel says.
        assert_eq!(stage.private_runs().unwrap(), []);
        let first = 0..ps;
        assert_eq!(stage.unnamed(&[first]), [0]);
        assert_eq!((ram.get(0, 0), ram.get(1, 0)), (0, 0));
    }

    /// A release leaves nothing of the daemon's mapped; a discard makes a hole of private pages.
    #[test]
    fn released_and_discarded_pages_are_zeros() {
        let (stage, ram, _arena) = stage();
        let ps = page_size();
        stage.map(0, at(0), 2 * ps);
        ram.set(5, 0, 3);
        stage.discard(4 * ps, 2 * ps);
        // Asked before the page is read, as above.
        assert_eq!(stage.private_runs().unwrap(), []);
        assert_eq!(ram.get(5, 0), 0);
        stage.release();
        assert_eq!((ram.get(0, 0), stage.installed()), (0, 0));
    }
}
