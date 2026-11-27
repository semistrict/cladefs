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
//! window, laid out as it is), mapped writable in its place. The guest's `MAP` or `UNMAP` over a
//! private page ends it ([`Private::End`]); the host's own changes leave it ([`Private::Keep`]),
//! as [`super::uffd`]'s do.
//!
//! The private memory is the host's in host pages, whatever the unit: `private_max` counts the
//! host pages holding a private unit, as [`super::uffd`] counts its pages, so that it bounds what
//! the private memory holds resident, and a host page whose last private unit ends is given back
//! to the host at once (board 14348).
//!
//! The guest's granule is the window's map unit, 4 KiB (`map_unit`), whatever the host's page:
//! a VMM gives its VM 4 KiB stage 2 pages (`HV_IPA_GRANULE_4KB`), so that each unit maps on its
//! own, from wherever its bytes are in its arena.

mod ram;

pub use ram::{GuestRam, HvfRamStage};

use super::{Access, Anew, Faults, Private, SecondStage, map_unit};
use crate::error::{Errno, Error, Result};
use crate::pages::{ArenaId, PhysLocation, page_size};
use std::collections::BTreeMap;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// A window's second stage on Hypervisor.framework: a DAX window's, which maps pages into the
/// guest alone ([`HvfSecondStage`]), or a memory cell's as the guest's RAM, which the VMM's
/// devices access too ([`HvfRamStage`]).
#[derive(Debug)]
pub enum HvfStage {
    Window(Arc<HvfSecondStage>),
    Ram(Arc<HvfRamStage>),
}

impl HvfStage {
    /// The window's size in bytes.
    pub fn size(&self) -> u64 {
        match self {
            Self::Window(stage) => stage.size(),
            Self::Ram(stage) => stage.size(),
        }
    }

    /// [`HvfSecondStage::make_private`]; `EINVAL` of RAM, whose writes copy in the kernel.
    pub async fn make_private(
        &self,
        window: &dyn Faults,
        offset: u64,
        len: u64,
    ) -> Result<(), Errno> {
        match self {
            Self::Window(stage) => stage.make_private(window, offset, len).await,
            Self::Ram(_) => Err(Errno::EINVAL),
        }
    }

    /// The private pages' runs, by window offset.
    pub fn private_runs(&self) -> Result<Vec<std::ops::Range<u64>>> {
        match self {
            Self::Window(stage) => stage.private_runs(),
            Self::Ram(stage) => stage.private_runs(),
        }
    }

    /// How many units are private.
    pub fn private_pages(&self) -> u64 {
        match self {
            Self::Window(stage) => stage.private_pages(),
            Self::Ram(stage) => stage.private_pages(),
        }
    }

    /// What making units private cost: runs, units and nanoseconds; of RAM, the kernel's copies,
    /// nothing counted.
    pub fn private_costs(&self) -> [u64; 3] {
        match self {
            Self::Window(stage) => stage.private_costs(),
            Self::Ram(_) => [0; 3],
        }
    }

    /// The guest's ask to map pages ahead of its reads: nothing to do, either's pages mapped as
    /// the window maps them.
    pub fn prefault(&self, _offset: u64, _len: u64) {}

    /// The runs of pages it may keep mapped: every unit.
    pub fn budget(&self) -> usize {
        match self {
            Self::Window(stage) => stage.budget(),
            Self::Ram(stage) => stage.budget(),
        }
    }

    /// Tells `told` as the stage's share of its process's budget changes: never.
    pub fn on_shared(&self, _told: impl Fn() + Send + Sync + 'static) {}

    /// How many units are mapped.
    pub fn installed(&self) -> u64 {
        match self {
            Self::Window(stage) => stage.installed(),
            Self::Ram(stage) => stage.installed(),
        }
    }

    /// Runs overwritten and left out to keep within a budget: none.
    pub fn taken(&self) -> [u64; 2] {
        [0; 2]
    }

    /// Lets go of every page, as the cell's connection goes.
    pub fn release(&self) {
        match self {
            Self::Window(stage) => stage.release(),
            Self::Ram(stage) => stage.release(),
        }
    }
}

impl SecondStage for HvfStage {
    fn map(&self, offset: u64, phys: PhysLocation, len: u64) {
        match self {
            Self::Window(stage) => stage.map(offset, phys, len),
            Self::Ram(stage) => stage.map(offset, phys, len),
        }
    }

    fn unmap(&self, offset: u64, len: u64, private: Private) {
        match self {
            Self::Window(stage) => stage.unmap(offset, len, private),
            Self::Ram(stage) => stage.unmap(offset, len, private),
        }
    }

    fn remap(&self, offset: u64, len: u64, private: Private, anew: &[Anew]) {
        match self {
            Self::Window(stage) => stage.remap(offset, len, private, anew),
            Self::Ram(stage) => stage.remap(offset, len, private, anew),
        }
    }
}

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
    /// The host pages of the private memory holding private units, by offset, and how many each
    /// holds: what the private memory holds resident.
    private: BTreeMap<u64, u32>,
    /// Let go of ([`HvfSecondStage::release`]): nothing is mapped any more.
    released: bool,
    /// How many unmaps it made: what tells a fault whose unit went after the daemon named it
    /// from one the daemon named nothing at ([`HvfSecondStage::fault`]).
    unmaps: u64,
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

impl PrivateMemory {
    /// Gives the `len` bytes at `offset`, whole host pages the guest maps no unit of, back to the
    /// host: fresh anonymous memory in their place, as the mapping was made, holding nothing.
    fn give_back(&self, offset: u64, len: u64) {
        let host_page = page_size();
        assert!(
            offset.is_multiple_of(host_page)
                && len.is_multiple_of(host_page)
                && offset + len <= self.len as u64,
            "giving back {len:#x} bytes at {offset:#x} of private memory of {:#x}",
            self.len
        );
        // SAFETY: Whole host pages of the stage's own mapping, which nothing maps into the guest
        // any more: replaced where they are, the mapping's extent unchanged.
        let addr = unsafe {
            libc::mmap(
                self.base.as_ptr().add(offset as usize).cast(),
                len as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_NORESERVE | libc::MAP_FIXED,
                -1,
                0,
            )
        };
        assert!(
            addr != libc::MAP_FAILED,
            "giving back {len:#x} bytes of private memory at {offset:#x}: {}",
            std::io::Error::last_os_error()
        );
    }
}

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
    /// Its private memory, of `most` host pages at most, once it copies on write.
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
            state: Mutex::new(State {
                units: BTreeMap::new(),
                private: BTreeMap::new(),
                released: false,
                unmaps: 0,
            }),
            faults: OnceLock::new(),
            copies: Default::default(),
        }))
    }

    /// cladefs: copies on write, of `most` host pages of private units at most (backlog doc-13).
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
    /// there, `EROFS` for a write to a window that doesn't copy, `ENOMEM` past its private units:
    /// each the guest's access refused. `ENOTCONN` before it serves faults, once it is released,
    /// or once what resolves its faults went ([`Faults::lost`]): nothing refused, the VM can't go
    /// on without its daemon.
    pub fn fault(&self, offset: u64, access: Access) -> Result<(), Errno> {
        if offset >= self.size {
            return Err(Errno::EINVAL);
        }
        let at = offset - offset % self.unit;
        let faults = self.faults.get().ok_or(Errno::ENOTCONN)?;
        // The unmaps made as the daemon was last asked to name the unit. The guest's next change
        // may take a unit away after the daemon named it, before its fault's reply comes back,
        // as the window then lets go of it too (a MAP of a unit beside it, board 14436): asked
        // again, the daemon names it anew, as KVM's fault again does on Linux. Gone with no
        // unmap since, the daemon named nothing there.
        let mut asked = None;
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
                    (None, _) if asked == Some(state.unmaps) => return Err(Errno::ENOENT),
                    (None, _) => asked = Some(state.unmaps),
                }
            }
            if let Err(err) = futures::executor::block_on(faults.fault_page(at)) {
                return Err(if faults.lost() { Errno::ENOTCONN } else { err });
            }
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
        if !self.copying() {
            return Err(Errno::EINVAL);
        }
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
            // Past the window's private pages, it refuses before making any unit private.
            self.privatize(&mut state, offset..end)?;
            self.count_copies(named, start);
            return Ok(());
        }
        Err(Errno::ENOMEM)
    }

    /// Makes the named units of `range` private, the stage's lock held (`state`): each copied into
    /// the private memory from its arena, its mapping in the guest replaced with the copy's,
    /// writable. `EROFS` for a window that doesn't copy, `ENOMEM`, nothing made private, past its
    /// private pages: the host pages holding private units.
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
        let mut new_pages: Vec<u64> = (named.iter())
            .map(|&(at, _)| host_page_of(at))
            .filter(|page| !state.private.contains_key(page))
            .collect();
        new_pages.dedup();
        if (state.private.len() + new_pages.len()) as u64 > *most {
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
            super::read_at(fd, bytes, phys.offset);
            self.guest.unmap(at, self.unit);
            // SAFETY: The private memory, mapped as long as the stage, which unmaps every unit
            // from the guest before it goes.
            unsafe {
                self.guest.map(at, NonNull::from(&mut bytes[0]), self.unit, true);
            }
            state.units.insert(at, Unit::Private);
            *state.private.entry(host_page_of(at)).or_insert(0) += 1;
        }
        Ok(())
    }

    /// Ends the private units at `ended`, which the guest maps no more, the stage's lock held
    /// (`state`): each host page left with none given back to the host.
    fn end_private(&self, state: &mut State, ended: &[u64]) {
        // A window that doesn't copy has none to end, as its unmaps say.
        let Some((memory, _)) = self.private.get() else {
            assert!(ended.is_empty(), "private units of a window that doesn't copy");
            return;
        };
        for &at in ended {
            let page = host_page_of(at);
            let held = state.private.get_mut(&page).expect("a private unit's host page counted");
            *held -= 1;
            if *held == 0 {
                state.private.remove(&page);
                memory.give_back(page, page_size());
            }
        }
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

    /// How many host pages hold private units: what `private_max` bounds.
    pub fn private_pages(&self) -> u64 {
        self.state.lock().unwrap().private.len() as u64
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
        let pages = std::mem::take(&mut state.private);
        if let Some((memory, _)) = self.private.get() {
            for page in pages.into_keys() {
                memory.give_back(page, page_size());
            }
        }
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
        // The window maps and unmaps whole host pages: an unmap of some units of one takes its
        // other units away too, which the window has let go of (a composed page's slab freed),
        // keeping their private copies, as `dax::uffd`'s replaced memory does. Only the units
        // asked end their private copies (board 14436).
        let host_page = page_size();
        let end = offset + len;
        let whole = offset - offset % host_page..end.next_multiple_of(host_page).min(self.size);
        let mut gone = Vec::new();
        let mut ended = Vec::new();
        for (&at, unit) in state.units.range(whole) {
            match (unit, private) {
                (Unit::Named { .. }, _) => gone.push(at),
                (Unit::Private, Private::End) if (offset..end).contains(&at) => {
                    gone.push(at);
                    ended.push(at);
                }
                (Unit::Private, _) => {}
            }
        }
        self.unmap_units(&gone);
        for at in &gone {
            state.units.remove(at);
        }
        self.end_private(&mut state, &ended);
        state.unmaps += 1;
    }
}

/// The offset of the host page holding the window's offset `at`.
fn host_page_of(at: u64) -> u64 {
    at - at % page_size()
}

impl Drop for HvfSecondStage {
    fn drop(&mut self) {
        self.release();
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

    /// On a host whose page holds several units, the window unmaps whole host pages: the
    /// guest's `MAP` of one unit takes the page's other units away too, whose memory (a composed
    /// page's slab) the window lets go of, keeping their private copies; only the unit asked ends
    /// its own (board 14436, writes_under_rebinds read a slab reused). On a host of 4 KiB pages,
    /// the unit alone.
    #[test]
    fn a_partial_unmap_takes_its_host_pages_named_units_away() {
        let (stage, guest) = stage();
        stage.copy_on_write(4).unwrap();
        let (unit, host_page) = (map_unit(), page_size());
        let units = host_page / unit;
        stage.map(0, at(0), host_page);
        stage.map(host_page, at(host_page), unit);
        let mut state = stage.state.lock().unwrap();
        stage.privatize(&mut state, 0..unit).unwrap();
        stage.privatize(&mut state, host_page - unit..host_page).unwrap();
        drop(state);
        guest.0.lock().unwrap().clear();
        stage.unmap(0, unit, Private::End);
        // The last unit of the page, private, stays, and the next page's; the rest went.
        let (left, private) = if units > 1 { (2, 1) } else { (1, 0) };
        assert_eq!((stage.installed(), stage.private_pages()), (left, private));
        let unmapped = match units {
            1 => vec![format!("unmap 0x0 {unit:#x}")],
            _ => vec![format!("unmap 0x0 {:#x}", host_page - unit)],
        };
        assert_eq!(*guest.0.lock().unwrap(), unmapped);
        // A host's unmap keeps every private unit, the page's included.
        stage.unmap(host_page, unit, Private::Keep);
        assert_eq!((stage.installed(), stage.private_pages()), (private, private));
    }

    /// Faults whose daemon maps the unit asked for, and, for its first `racing` faults, has the
    /// guest's next change take it away again before the fault's reply, as a MAP beside it does.
    struct Racing {
        stage: OnceLock<std::sync::Weak<HvfSecondStage>>,
        racing: u64,
        asked: AtomicU64,
    }

    impl Faults for Racing {
        fn size(&self) -> u64 {
            1 << 20
        }

        fn fault_page(&self, offset: u64) -> futures::future::BoxFuture<'_, Result<(), Errno>> {
            let stage = self.stage.get().unwrap().upgrade().unwrap();
            stage.map(offset, at(0), map_unit());
            if self.asked.fetch_add(1, Ordering::Relaxed) < self.racing {
                stage.unmap(offset, map_unit(), Private::End);
            }
            Box::pin(std::future::ready(Ok(())))
        }

        fn probe(&self, offset: u64) -> futures::future::BoxFuture<'_, Result<(), Errno>> {
            self.fault_page(offset)
        }
    }

    /// A fault whose unit the guest's next change took away after the daemon named it asks the
    /// daemon again, rather than refusing the access (board 14436: a check's VM stopped on a load
    /// so); a unit the daemon says it named, gone with no unmap since, refuses it.
    #[test]
    fn a_fault_raced_by_an_unmap_asks_again() {
        for racing in [0, 1, 3] {
            let (stage, _guest) = stage();
            let faults = Arc::new(Racing { stage: OnceLock::new(), racing, asked: 0.into() });
            faults.stage.set(Arc::downgrade(&stage)).unwrap();
            let _serving = stage.serve(faults.clone()).unwrap();
            assert_eq!(stage.fault(0, Access::Read), Ok(()), "{racing} raced");
            assert_eq!(faults.asked.load(Ordering::Relaxed), racing + 1);
        }
        let (stage, _guest) = stage();
        let _serving =
            stage.serve(Arc::new(Refusing { refusal: Errno::EIO, lost: false })).unwrap();
        assert_eq!(stage.fault(0, Access::Read), Err(Errno::EIO));
    }

    /// Faults whose daemon answers each, mapping nothing.
    struct Silent;

    impl Faults for Silent {
        fn size(&self) -> u64 {
            1 << 20
        }

        fn fault_page(&self, _: u64) -> futures::future::BoxFuture<'_, Result<(), Errno>> {
            Box::pin(std::future::ready(Ok(())))
        }

        fn probe(&self, _: u64) -> futures::future::BoxFuture<'_, Result<(), Errno>> {
            Box::pin(std::future::ready(Ok(())))
        }
    }

    /// A daemon that answers a fault mapping nothing there, nothing unmapped since it was asked,
    /// has the access refused, once asked again: no fault waits for good.
    #[test]
    fn a_fault_the_daemon_maps_nothing_at_is_refused() {
        let (stage, _guest) = stage();
        let _serving = stage.serve(Arc::new(Silent)).unwrap();
        assert_eq!(stage.fault(0, Access::Read), Err(Errno::ENOENT));
        assert_eq!(stage.fault(0, Access::Write), Err(Errno::ENOENT));
    }

    /// Faults whose daemon names nothing, refusing each with `refusal`, gone if `lost`.
    struct Refusing {
        refusal: Errno,
        lost: bool,
    }

    impl Faults for Refusing {
        fn size(&self) -> u64 {
            1 << 20
        }

        fn fault_page(&self, _: u64) -> futures::future::BoxFuture<'_, Result<(), Errno>> {
            Box::pin(std::future::ready(Err(self.refusal)))
        }

        fn probe(&self, _: u64) -> futures::future::BoxFuture<'_, Result<(), Errno>> {
            Box::pin(std::future::ready(Err(self.refusal)))
        }

        fn lost(&self) -> bool {
            self.lost
        }
    }

    /// A fault the daemon refuses is the guest's access refused, by its errno, which the VMM
    /// stops the machine for (libmachine's `AccessRefused`); one failing as the daemon went is
    /// `ENOTCONN`, as a stage released or not yet serving faults is: the daemon lost, nothing
    /// refused (board 14374).
    #[test]
    fn faults_say_refused_apart_from_the_daemon_gone() {
        for (refusal, lost, said) in [
            (Errno::ENOENT, false, Errno::ENOENT),
            (Errno::EIO, false, Errno::EIO),
            (Errno::EIO, true, Errno::ENOTCONN),
            (Errno::ENOENT, true, Errno::ENOTCONN),
        ] {
            let (stage, _guest) = stage();
            assert_eq!(stage.fault(0, Access::Read), Err(Errno::ENOTCONN), "not serving yet");
            let _serving = stage.serve(Arc::new(Refusing { refusal, lost })).unwrap();
            assert_eq!(stage.fault(0, Access::Read), Err(said), "{refusal:?}, lost {lost}");
            assert_eq!(stage.fault(0, Access::Write), Err(said), "{refusal:?}, lost {lost}");
            stage.release();
            assert_eq!(stage.fault(0, Access::Read), Err(Errno::ENOTCONN), "released");
        }
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

    /// Faults of a window whose every unit written is mapped already: none reaches it.
    struct Mapped;

    impl Faults for Mapped {
        fn size(&self) -> u64 {
            1 << 20
        }

        fn fault_page(&self, _: u64) -> futures::future::BoxFuture<'_, Result<(), Errno>> {
            Box::pin(async { Err(Errno::ENOENT) })
        }

        fn probe(&self, _: u64) -> futures::future::BoxFuture<'_, Result<(), Errno>> {
            Box::pin(async { Err(Errno::ENOENT) })
        }
    }

    /// The bytes of `stage`'s private memory resident in this process, by `mincore`.
    fn resident(stage: &HvfSecondStage) -> u64 {
        let (memory, _) = stage.private.get().unwrap();
        let host_page = page_size() as usize;
        let mut pages = vec![0 as libc::c_char; memory.len / host_page];
        // SAFETY: The stage's own mapping, whole, and a byte for each of its pages.
        let ret =
            unsafe { libc::mincore(memory.base.as_ptr().cast(), memory.len, pages.as_mut_ptr()) };
        assert_eq!(ret, 0, "mincore: {}", std::io::Error::last_os_error());
        let incore = pages.iter().filter(|&&page| page & libc::MINCORE_INCORE as libc::c_char != 0);
        (incore.count() * host_page) as u64
    }

    /// A guest that writes a unit, has it ended (a `MAP` or `UNMAP` over it, `Private::End`) and
    /// writes elsewhere, a unit of every host page of the window in turn, holds no more private
    /// memory resident than `private_max` pages: each host page whose last private unit ends is
    /// given back (board 14348, which kept every one). The host's unmap (`Private::Keep`) keeps it.
    #[test]
    fn private_units_ended_give_their_memory_back() {
        let (stage, _guest) = stage();
        stage.copy_on_write(2).unwrap();
        let _serving = stage.serve(Arc::new(Mapped)).unwrap();
        let (unit, host_page) = (map_unit(), page_size());
        for offset in (0..stage.size()).step_by(host_page as usize) {
            stage.map(offset, at(0), unit);
            stage.fault(offset, Access::Write).unwrap();
            stage.unmap(offset, unit, Private::Keep);
            assert_eq!((stage.private_pages(), resident(&stage)), (1, host_page));
            stage.unmap(offset, unit, Private::End);
            assert_eq!((stage.private_pages(), resident(&stage)), (0, 0), "at {offset:#x}");
        }
        assert_eq!(stage.installed(), 0);
    }

    /// `private_max` counts the host pages holding private units, what the private memory holds
    /// resident: the units of one host page take one, and a unit of another past it is refused,
    /// `ENOMEM`, until the first page's units end.
    #[test]
    fn private_max_counts_host_pages() {
        let (stage, _guest) = stage();
        stage.copy_on_write(1).unwrap();
        let _serving = stage.serve(Arc::new(Mapped)).unwrap();
        let (unit, host_page) = (map_unit(), page_size());
        stage.map(0, at(0), 2 * host_page);
        for offset in (0..host_page).step_by(unit as usize) {
            stage.fault(offset, Access::Write).unwrap();
        }
        assert_eq!(stage.private_pages(), 1);
        assert_eq!(stage.fault(host_page, Access::Write), Err(Errno::ENOMEM));
        stage.unmap(0, host_page, Private::End);
        assert_eq!((stage.private_pages(), resident(&stage)), (0, 0));
        assert_eq!(stage.fault(host_page, Access::Write), Ok(()));
        assert_eq!((stage.private_pages(), resident(&stage)), (1, host_page));
        stage.release();
        assert_eq!((stage.private_pages(), resident(&stage)), (0, 0));
    }
}
