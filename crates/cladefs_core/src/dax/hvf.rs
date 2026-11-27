// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A DAX window's second stage in a guest's physical memory that the VMM maps itself, as
//! Hypervisor.framework's VMMs do (`hv_vm_map`), and whose faults it takes as the guest's exits
//! (board 13069): macOS's, where no userfaultfd follows host memory for the hypervisor
//! ([`super::uffd`] is Linux's).
//!
//! The window's pages are mapped into the guest as the window maps them, read-only and
//! executable, from the arenas' memory objects, mapped into this process run by run: the guest
//! shares the daemon's cached pages, as the host's buffer cache holds them. A guest's access to a
//! page the window hasn't mapped exits to the VMM, which hands it here ([`HvfSecondStage::fault`]):
//! the daemon names the page ([`Faults::fault_page`]), its mapping comes back over the wire before
//! the fault's reply, and the guest's access, retried, finds it.
//!
//! cladefs: a writer's window copies on write (backlog doc-13): a guest's write to a page the
//! window maps exits as a permission fault, and the page is made private here, a copy of what the
//! window names there, in the window's private memory (an anonymous mapping as large as the
//! window, laid out as it is), mapped writable in its place, of `private_max` pages at most. The
//! guest's `MAP` or `UNMAP` over a private page ends it ([`Private::End`]); the host's own changes
//! leave it ([`Private::Keep`]), as [`super::uffd`]'s do.
//!
//! The guest's granule is the window's map unit, 4 KiB (`map_unit`), whatever the host's page:
//! a VMM gives its VM 4 KiB stage 2 pages (`HV_IPA_GRANULE_4KB`), so that each unit maps on its
//! own, from wherever its bytes are in its arena.

use super::{Access, Faults, Private, SecondStage, map_unit};
use crate::error::{Errno, Error, Result};
use crate::pages::{ArenaId, PhysLocation, page_size};
use std::collections::BTreeMap;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// What only the VMM can do for the window: map host memory into its guest at the window's
/// offsets, and unmap it, as Hypervisor.framework's `hv_vm_map` and `hv_vm_unmap` do.
pub trait GuestMemory: Send + Sync {
    /// Maps the `len` bytes of host memory at `host` into the window at `offset`, readable and
    /// executable, and writable if `write`. A failure leaves the guest's memory unlike the
    /// window's: the VMM panics.
    ///
    /// # Safety
    ///
    /// The memory stays mapped in this process until [`unmap`](Self::unmap) takes it out of the
    /// guest.
    unsafe fn map(&self, offset: u64, host: NonNull<u8>, len: u64, write: bool);

    /// Unmaps the `len` bytes at `offset` from the guest, which [`map`](Self::map) mapped.
    fn unmap(&self, offset: u64, len: u64);
}

/// A run of an arena's memory object mapped into this process, read-only, which the guest maps
/// units of. Unmapped once no unit maps it.
struct HostRun {
    addr: NonNull<u8>,
    len: usize,
}

// SAFETY: An address range of this process's, which only the guest accesses through its mapping.
unsafe impl Send for HostRun {}
// SAFETY: As above.
unsafe impl Sync for HostRun {}

impl Drop for HostRun {
    fn drop(&mut self) {
        // SAFETY: The run's own mapping, which nothing maps into the guest any more.
        unsafe { libc::munmap(self.addr.as_ptr().cast(), self.len) };
    }
}

/// A unit of the window, as the guest has it.
enum Unit {
    /// The window's page at `phys`, mapped read-only from `_run`, which it keeps mapped.
    Named { _run: Arc<HostRun>, phys: PhysLocation },
    /// Its private copy, in the window's private memory, mapped writable.
    Private,
}

/// What the window's units are, the units not here being unmapped.
struct State {
    units: BTreeMap<u64, Unit>,
    /// How many units are private.
    private: u64,
    /// Let go of ([`HvfSecondStage::release`]): nothing is mapped any more.
    released: bool,
}

/// The window's private memory: an anonymous mapping as large as the window, of which only the
/// private units are written.
struct PrivateMemory {
    base: NonNull<u8>,
    len: usize,
}

// SAFETY: An address range of this process's, written under the stage's lock, read by the guest.
unsafe impl Send for PrivateMemory {}
// SAFETY: As above.
unsafe impl Sync for PrivateMemory {}

impl Drop for PrivateMemory {
    fn drop(&mut self) {
        // SAFETY: The stage's own mapping, which the guest maps no unit of once it is released.
        unsafe { libc::munmap(self.base.as_ptr().cast(), self.len) };
    }
}

/// A DAX window's second stage in a guest's memory that the VMM maps (see the module docs).
pub struct HvfSecondStage {
    size: u64,
    unit: u64,
    guest: Arc<dyn GuestMemory>,
    /// The arenas' memory objects, which the window's pages are of.
    memories: Vec<(ArenaId, OwnedFd)>,
    /// Its private memory, of `most` units at most, once it copies on write.
    private: OnceLock<(PrivateMemory, u64)>,
    state: Mutex<State>,
    /// What resolves its faults, once it serves them ([`Self::serve`]).
    faults: OnceLock<Arc<dyn Faults>>,
    /// Units made private: runs, units and nanoseconds, as [`super::uffd`] counts them.
    copies: [AtomicU64; 3],
}

impl std::fmt::Debug for HvfSecondStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HvfSecondStage").field("size", &self.size).finish_non_exhaustive()
    }
}

/// What a stage's faults are served by ([`HvfSecondStage::serve`]): the guest's vCPUs, which the
/// VMM has hand their faults to the stage; nothing of its own to stop.
#[derive(Debug)]
pub struct FaultThread;

impl HvfSecondStage {
    /// A window of `size` bytes, a multiple of the map unit, in `guest`'s memory, its pages of
    /// `memories`' arenas.
    pub fn new(
        size: u64,
        memories: Vec<(ArenaId, OwnedFd)>,
        guest: Arc<dyn GuestMemory>,
    ) -> Result<Arc<Self>, Error> {
        let unit = map_unit();
        if size == 0 || !size.is_multiple_of(page_size()) {
            return Err(Error::new(Errno::EINVAL));
        }
        Ok(Arc::new(Self {
            size,
            unit,
            guest,
            memories,
            private: OnceLock::new(),
            state: Mutex::new(State { units: BTreeMap::new(), private: 0, released: false }),
            faults: OnceLock::new(),
            copies: Default::default(),
        }))
    }

    /// cladefs: copies on write, of `most` private units at most (backlog doc-13).
    pub fn copy_on_write(&self, most: u64) -> Result<(), Error> {
        if most == 0 {
            return Err(Error::new(Errno::EINVAL));
        }
        let len = usize::try_from(self.size).map_err(|_| Error::new(Errno::EFBIG))?;
        // SAFETY: A new anonymous mapping, whose result is checked.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(Error::new(std::io::Error::last_os_error()));
        }
        let memory =
            PrivateMemory { base: NonNull::new(base.cast()).expect("mmap's address"), len };
        self.private.set((memory, most)).map_err(|_| Error::new(Errno::EEXIST))
    }

    /// Whether it copies on write.
    pub fn copying(&self) -> bool {
        self.private.get().is_some()
    }

    /// Has `window` resolve the guest's faults, which its VMM hands to [`Self::fault`].
    pub fn serve(self: &Arc<Self>, window: Arc<dyn Faults>) -> Result<FaultThread, Error> {
        self.faults.set(window).map_err(|_| Error::new(Errno::EEXIST))?;
        Ok(FaultThread)
    }

    /// The window's size in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Resolves the guest's fault at `offset` in the window, for `access`, blocking its vCPU until
    /// the access may be retried: a page not mapped is named by the daemon, which maps it; a write
    /// to a page mapped makes it private, a writer's window's. `ENOENT` if the daemon names nothing
    /// there, `EROFS` for a write to a window that doesn't copy, `ENOMEM` past its private units,
    /// `ENOTCONN` before it serves faults: each a fault the guest can't go on from.
    pub fn fault(&self, offset: u64, access: Access) -> Result<(), Errno> {
        if offset >= self.size {
            return Err(Errno::EINVAL);
        }
        let at = offset - offset % self.unit;
        let faults = self.faults.get().ok_or(Errno::ENOTCONN)?;
        let mut named = false;
        loop {
            {
                let mut state = self.state.lock().unwrap();
                if state.released {
                    return Err(Errno::ENOTCONN);
                }
                match (state.units.get(&at), access) {
                    (Some(Unit::Private), _) | (Some(Unit::Named { .. }), Access::Read) => {
                        return Ok(());
                    }
                    (Some(Unit::Named { .. }), Access::Write) => {
                        let start = std::time::Instant::now();
                        self.privatize(&mut state, at..at + self.unit)?;
                        self.count_copies(1, start);
                        return Ok(());
                    }
                    (None, _) if named => return Err(Errno::ENOENT),
                    (None, _) => {}
                }
            }
            futures::executor::block_on(faults.fault_page(at))?;
            named = true;
        }
    }

    /// cladefs: makes the `len` bytes at `offset` private now, mapped writable for the guest ahead
    /// of its writes: doc-13's `PRIVATE`. `ENOMEM`, nothing made private, past the window's private
    /// units; `EINVAL` for a window that doesn't copy, a range not within it or not of whole units,
    /// or one with a unit `window` maps nothing at.
    pub async fn make_private(
        &self,
        window: &dyn Faults,
        offset: u64,
        len: u64,
    ) -> Result<(), Errno> {
        let most = self.private.get().ok_or(Errno::EINVAL)?.1;
        let end = offset.checked_add(len).ok_or(Errno::EINVAL)?;
        if len == 0
            || !offset.is_multiple_of(self.unit)
            || !len.is_multiple_of(self.unit)
            || end > self.size
        {
            return Err(Errno::EINVAL);
        }
        // A range can span more parts than the cache holds at once: a few tries at naming it all.
        const NAME_ATTEMPTS: usize = 4;
        for _ in 0..NAME_ATTEMPTS {
            let unnamed: Vec<u64> = {
                let state = self.state.lock().unwrap();
                (offset..end)
                    .step_by(self.unit as usize)
                    .filter(|at| !state.units.contains_key(at))
                    .collect()
            };
            for at in unnamed {
                match window.fault_page(at).await {
                    Ok(()) => {}
                    Err(Errno::ENOENT) => return Err(Errno::EINVAL),
                    Err(err) => return Err(err),
                }
            }
            let start = std::time::Instant::now();
            let mut state = self.state.lock().unwrap();
            if state.released {
                return Ok(());
            }
            let mut named = 0u64;
            let mut all = true;
            for at in (offset..end).step_by(self.unit as usize) {
                match state.units.get(&at) {
                    Some(Unit::Named { .. }) => named += 1,
                    Some(Unit::Private) => {}
                    None => all = false,
                }
            }
            if !all {
                continue;
            }
            if state.private + named > most {
                return Err(Errno::ENOMEM);
            }
            self.privatize(&mut state, offset..end)?;
            self.count_copies(named, start);
            return Ok(());
        }
        Err(Errno::ENOMEM)
    }

    /// Makes the named units of `range` private, the stage's lock held (`state`): each copied into
    /// the private memory from its arena, its mapping in the guest replaced with the copy's,
    /// writable. `EROFS` for a window that doesn't copy, `ENOMEM` past its private units.
    fn privatize(&self, state: &mut State, range: std::ops::Range<u64>) -> Result<(), Errno> {
        let Some((memory, most)) = self.private.get() else {
            return Err(Errno::EROFS);
        };
        let named: Vec<(u64, PhysLocation)> = state
            .units
            .range(range)
            .filter_map(|(&at, unit)| match unit {
                Unit::Named { phys, .. } => Some((at, *phys)),
                Unit::Private => None,
            })
            .collect();
        if state.private + named.len() as u64 > *most {
            return Err(Errno::ENOMEM);
        }
        for (at, phys) in named {
            let (fd, _) = self.memory(phys.arena).expect("a window's page is of its arenas");
            // SAFETY: The unit's bytes of the private memory, within the window's size, which only
            // the guest reads, through the mapping made below once they are written.
            let bytes = unsafe {
                std::slice::from_raw_parts_mut(
                    memory.base.as_ptr().add(at as usize),
                    self.unit as usize,
                )
            };
            read_at(fd, bytes, phys.offset);
            self.guest.unmap(at, self.unit);
            // SAFETY: The private memory, mapped as long as the stage, which unmaps every unit
            // from the guest before it goes.
            unsafe {
                self.guest.map(at, NonNull::from(&mut bytes[0]), self.unit, true);
            }
            state.units.insert(at, Unit::Private);
            state.private += 1;
        }
        Ok(())
    }

    fn count_copies(&self, units: u64, start: std::time::Instant) {
        if units == 0 {
            return;
        }
        self.copies[0].fetch_add(1, Ordering::Relaxed);
        self.copies[1].fetch_add(units, Ordering::Relaxed);
        self.copies[2].fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    /// The descriptor of `arena`'s memory object, and whether it is read-only.
    fn memory(&self, arena: ArenaId) -> Option<(RawFd, bool)> {
        let fd = self.memories.iter().find(|(given, _)| *given == arena)?.1.as_raw_fd();
        // SAFETY: Plain system call on a descriptor the stage holds.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        Some((fd, flags >= 0 && flags & libc::O_ACCMODE == libc::O_RDONLY))
    }

    /// Maps the `len` bytes of `phys`'s arena at its offset into this process, read-only: the run
    /// the guest maps them from, and where they start in it.
    fn host_run(&self, phys: PhysLocation, len: u64) -> (Arc<HostRun>, NonNull<u8>) {
        let (fd, _) = self.memory(phys.arena).expect("a window's page is of its arenas");
        let host_page = page_size();
        let from = phys.offset - phys.offset % host_page;
        let delta = (phys.offset - from) as usize;
        let mapped = (delta + len as usize).next_multiple_of(host_page as usize);
        // SAFETY: A new shared mapping of an open descriptor's object, whose result is checked.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                mapped,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                from as libc::off_t,
            )
        };
        assert!(
            addr != libc::MAP_FAILED,
            "mapping {len:#x} bytes of arena {:?} at {:#x}: {}",
            phys.arena,
            phys.offset,
            std::io::Error::last_os_error()
        );
        let run = Arc::new(HostRun {
            addr: NonNull::new(addr.cast()).expect("mmap's address"),
            len: mapped,
        });
        // SAFETY: Within the run just mapped.
        let start = unsafe { NonNull::new_unchecked(run.addr.as_ptr().add(delta)) };
        (run, start)
    }

    /// Unmaps from the guest, in runs, the units of `units`, consecutive or not, in order.
    fn unmap_units(&self, units: &[u64]) {
        let mut runs: Vec<(u64, u64)> = Vec::new();
        for &at in units {
            match runs.last_mut() {
                Some((start, len)) if *start + *len == at => *len += self.unit,
                _ => runs.push((at, self.unit)),
            }
        }
        for (start, len) in runs {
            self.guest.unmap(start, len);
        }
    }

    /// The private units' runs, by window offset.
    pub fn private_runs(&self) -> Result<Vec<std::ops::Range<u64>>> {
        let state = self.state.lock().unwrap();
        let mut runs: Vec<std::ops::Range<u64>> = Vec::new();
        for (&at, unit) in &state.units {
            if !matches!(unit, Unit::Private) {
                continue;
            }
            match runs.last_mut() {
                Some(run) if run.end == at => run.end += self.unit,
                _ => runs.push(at..at + self.unit),
            }
        }
        Ok(runs)
    }

    /// How many units are private.
    pub fn private_pages(&self) -> u64 {
        self.state.lock().unwrap().private
    }

    /// What making units private cost: runs, units and nanoseconds.
    pub fn private_costs(&self) -> [u64; 3] {
        self.copies.each_ref().map(|count| count.load(Ordering::Relaxed))
    }

    /// The guest's ask to map pages ahead of its reads (`MAP_PREFAULT`): the window's pages are
    /// mapped into the guest as the window maps them, so nothing is left to do.
    pub fn prefault(&self, _offset: u64, _len: u64) {}

    /// The runs of pages it may keep mapped, which the daemon's metrics show: every unit, as the
    /// hypervisor bounds none.
    pub fn budget(&self) -> usize {
        (self.size / self.unit) as usize
    }

    /// Tells `told` as the stage's share of its process's budget changes: never, as it has all.
    pub fn on_shared(&self, _told: impl Fn() + Send + Sync + 'static) {}

    /// How many units are mapped into the guest.
    pub fn installed(&self) -> u64 {
        self.state.lock().unwrap().units.len() as u64
    }

    /// Runs overwritten and left out to keep within a budget: none, as it has no budget to keep.
    pub fn taken(&self) -> [u64; 2] {
        [0; 2]
    }

    /// Lets go of every page: each unmapped from the guest, which reaches nothing in the window
    /// after, as the cell's connection goes.
    pub fn release(&self) {
        let mut state = self.state.lock().unwrap();
        if state.released {
            return;
        }
        state.released = true;
        let units: Vec<u64> = state.units.keys().copied().collect();
        self.unmap_units(&units);
        state.units.clear();
        state.private = 0;
    }
}

impl SecondStage for HvfSecondStage {
    fn map(&self, offset: u64, phys: PhysLocation, len: u64) {
        assert!(
            offset.is_multiple_of(self.unit)
                && len.is_multiple_of(self.unit)
                && phys.offset.is_multiple_of(self.unit)
                && offset + len <= self.size,
            "a map of {len:#x} bytes at {offset:#x} from {phys:?}, units of {:#x}",
            self.unit
        );
        let mut state = self.state.lock().unwrap();
        if state.released {
            return;
        }
        // What the window mapped there before is taken away first, as `dax::uffd` takes it away:
        // a map names the range anew, whatever it named. A private unit stays the guest's copy.
        let replaced: Vec<u64> = (state.units.range(offset..offset + len))
            .filter(|(_, unit)| matches!(unit, Unit::Named { .. }))
            .map(|(&at, _)| at)
            .collect();
        self.unmap_units(&replaced);
        for at in &replaced {
            state.units.remove(at);
        }
        let (run, host) = self.host_run(phys, len);
        // The units mapped anew, in runs: a private unit there stays the guest's copy.
        let mut mapping: Option<(u64, u64)> = None;
        let flush = |mapping: &mut Option<(u64, u64)>| {
            if let Some((start, len)) = mapping.take() {
                // SAFETY: Of the run, which each unit mapped holds until it is unmapped.
                unsafe {
                    let at = NonNull::new_unchecked(host.as_ptr().add((start - offset) as usize));
                    self.guest.map(start, at, len, false);
                }
            }
        };
        for at in (offset..offset + len).step_by(self.unit as usize) {
            if matches!(state.units.get(&at), Some(Unit::Private)) {
                flush(&mut mapping);
                continue;
            }
            let unit_phys = PhysLocation { arena: phys.arena, offset: phys.offset + (at - offset) };
            state.units.insert(at, Unit::Named { _run: run.clone(), phys: unit_phys });
            match &mut mapping {
                Some((start, len)) if *start + *len == at => *len += self.unit,
                _ => {
                    flush(&mut mapping);
                    mapping = Some((at, self.unit));
                }
            }
        }
        flush(&mut mapping);
    }

    fn unmap(&self, offset: u64, len: u64, private: Private) {
        let mut state = self.state.lock().unwrap();
        if state.released {
            return;
        }
        let mut gone = Vec::new();
        let mut ended = 0;
        for (&at, unit) in state.units.range(offset..offset + len) {
            match (unit, private) {
                (Unit::Named { .. }, _) => gone.push(at),
                (Unit::Private, Private::End) => {
                    gone.push(at);
                    ended += 1;
                }
                (Unit::Private, Private::Keep) => {}
            }
        }
        self.unmap_units(&gone);
        for at in &gone {
            state.units.remove(at);
        }
        state.private -= ended;
    }
}

impl Drop for HvfSecondStage {
    fn drop(&mut self) {
        self.release();
    }
}

/// Reads `bytes.len()` bytes of `fd`'s object at `offset` into `bytes`.
fn read_at(fd: RawFd, bytes: &mut [u8], offset: u64) {
    let mut done = 0;
    while done < bytes.len() {
        let rest = &mut bytes[done..];
        let at = (offset + done as u64) as libc::off_t;
        // SAFETY: Reads into `rest`, which is as long as asked.
        let read = unsafe { libc::pread(fd, rest.as_mut_ptr().cast(), rest.len(), at) };
        if read < 0 {
            let err = std::io::Error::last_os_error();
            assert_eq!(err.kind(), std::io::ErrorKind::Interrupted, "read {offset:#x}: {err}");
            continue;
        }
        assert!(read > 0, "read {offset:#x}: past the memory's end");
        done += read as usize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// A guest's memory as a test has it: the maps and unmaps made, in order.
    #[derive(Default)]
    struct Recorded(Mutex<Vec<String>>);

    impl GuestMemory for Recorded {
        unsafe fn map(&self, offset: u64, _host: NonNull<u8>, len: u64, write: bool) {
            let kind = if write { "map rw" } else { "map ro" };
            self.0.lock().unwrap().push(format!("{kind} {offset:#x} {len:#x}"));
        }

        fn unmap(&self, offset: u64, len: u64) {
            self.0.lock().unwrap().push(format!("unmap {offset:#x} {len:#x}"));
        }
    }

    /// A stage of a 1 MiB window over an arena of 256 KiB of 7s, its guest's memory recorded.
    fn stage() -> (Arc<HvfSecondStage>, Arc<Recorded>) {
        let mut arena = tempfile::tempfile().unwrap();
        arena.write_all(&[7; 256 << 10]).unwrap();
        let guest = Arc::new(Recorded::default());
        let memories = vec![(ArenaId::DEFAULT, OwnedFd::from(arena))];
        let stage = HvfSecondStage::new(1 << 20, memories, guest.clone()).unwrap();
        (stage, guest)
    }

    fn at(offset: u64) -> PhysLocation {
        PhysLocation { arena: ArenaId::DEFAULT, offset }
    }

    /// A map over units mapped already names them anew, as `dax::uffd`'s does: what was there is
    /// unmapped from the guest first (board 14362, which panicked).
    #[test]
    fn a_map_over_mapped_units_replaces_them() {
        let (stage, guest) = stage();
        let unit = map_unit();
        stage.map(0, at(0), 2 * unit);
        stage.map(unit, at(4 * unit), 2 * unit);
        assert_eq!(stage.installed(), 3);
        assert_eq!(
            *guest.0.lock().unwrap(),
            [
                format!("map ro 0x0 {:#x}", 2 * unit),
                format!("unmap {unit:#x} {unit:#x}"),
                format!("map ro {unit:#x} {:#x}", 2 * unit),
            ]
        );
    }

    /// A unit made private stays the guest's copy through a map over it and a host's unmap
    /// (`Private::Keep`); the guest's unmap (`Private::End`) ends it.
    #[test]
    fn private_units_stay_through_maps_until_the_guest_unmaps_them() {
        let (stage, guest) = stage();
        stage.copy_on_write(4).unwrap();
        let unit = map_unit();
        stage.map(0, at(0), unit);
        let mut state = stage.state.lock().unwrap();
        stage.privatize(&mut state, 0..unit).unwrap();
        drop(state);
        stage.map(0, at(unit), 2 * unit);
        stage.unmap(0, 2 * unit, Private::Keep);
        assert_eq!(stage.private_pages(), 1);
        stage.unmap(0, 2 * unit, Private::End);
        assert_eq!((stage.private_pages(), stage.installed()), (0, 0));
        assert_eq!(
            *guest.0.lock().unwrap(),
            [
                format!("map ro 0x0 {unit:#x}"),
                format!("unmap 0x0 {unit:#x}"),
                format!("map rw 0x0 {unit:#x}"),
                format!("map ro {unit:#x} {unit:#x}"),
                format!("unmap {unit:#x} {unit:#x}"),
                format!("unmap 0x0 {unit:#x}"),
            ]
        );
    }
}
