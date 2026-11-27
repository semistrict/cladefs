// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The device's DAX window: guest physical addresses the VMM reserves for it (a BAR of the
//! device's own, which a virtio-PCI transport describes with a shared memory capability), backed
//! by anonymous host memory that the hypervisor maps there, a memory slot of its own. cladefs's
//! extent daemon owns the window: the device hands the host memory to the cell's connection,
//! which maps the window's pages there from the daemon's memory, and serves the faults the
//! hypervisor takes on the rest.
//!
//! The hypervisor maps a page of the window into its second-stage tables on the guest's first
//! access to it, an exit per page, which nested virtualization makes slow: given a
//! [`PrefaultGuestMemory`] (KVM's `KVM_PRE_FAULT_MEMORY`), the window has the hypervisor map the
//! pages as the daemon maps them.
//!
//! On macOS (Hypervisor.framework, board 13069) the window has no host memory of its own: the VMM
//! maps the daemon's pages into the guest at the window's addresses itself, as cladefs's stage there
//! says (`cladefs::HvfSecondStage`), and hands the guest's faults on the window to it
//! ([`window_fault`]).

use std::io;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use crate::BoxError;
use crate::protocol::DAX_WINDOW_ALIGNMENT;

/// What only the VMM can do for a DAX window: place it in its guest's physical address space, and
/// have the hypervisor map host memory there, while the VM runs.
pub trait WindowVm: Send + Sync {
    /// Reserves guest physical addresses for a window of `len` bytes, a multiple of
    /// [`DAX_WINDOW_ALIGNMENT`]: where they start, aligned to it at least. At `at`, if given:
    /// where a snapshot the VM is restored from had the window, which the VMM's restored state
    /// may hold reserved already.
    fn reserve(&self, len: u64, at: Option<u64>) -> Result<u64, BoxError>;

    /// Releases the addresses [`WindowVm::reserve`] reserved for the window of `len` bytes at
    /// `start`, once nothing maps them.
    fn release(&self, start: u64, len: u64);

    /// Maps the `len` bytes of host memory at `host` into the guest at `guest_addr`, as a memory
    /// slot added while the VM runs (KVM's `KVM_SET_USER_MEMORY_REGION`): the slot.
    #[cfg(target_os = "linux")]
    fn add_slot(&self, guest_addr: u64, len: u64, host: NonNull<u8>) -> Result<u32, BoxError>;

    /// Removes the memory slot [`WindowVm::add_slot`] added: the hypervisor lets go of its host
    /// memory.
    #[cfg(target_os = "linux")]
    fn remove_slot(&self, slot: u32);

    /// Maps the `len` bytes of host memory at `host` into the guest at `guest_addr`, readable and
    /// executable, and writable if `write` (Hypervisor.framework's `hv_vm_map`). A failure leaves
    /// the guest's memory unlike the window's: the VMM panics.
    ///
    /// # Safety
    ///
    /// The memory stays mapped in this process until [`WindowVm::unmap`] takes it out of the guest.
    #[cfg(not(target_os = "linux"))]
    unsafe fn map(&self, guest_addr: u64, host: NonNull<u8>, len: u64, write: bool);

    /// Unmaps the `len` bytes at `guest_addr` from the guest, which [`WindowVm::map`] mapped.
    #[cfg(not(target_os = "linux"))]
    fn unmap(&self, guest_addr: u64, len: u64);
}

/// Maps guest memory for the guest ahead of its accesses, as KVM's `KVM_PRE_FAULT_MEMORY` does,
/// rather than on a fault on each page.
pub trait PrefaultGuestMemory: std::fmt::Debug + Send + Sync {
    /// Maps the `len` bytes of guest memory at `guest_addr`, which the host has mapped, for the
    /// guest. It must not wait for the guest.
    fn prefault(&self, guest_addr: u64, len: u64);
}

/// Errors making the DAX window.
#[derive(Debug, thiserror::Error)]
pub enum DaxWindowError {
    #[error("The DAX window's size, {0:#x} bytes, is not a nonzero multiple of 2 MiB")]
    Size(u64),
    #[error("Cannot allocate guest addresses for the DAX window: {0}")]
    GuestAddresses(BoxError),
    #[error("Cannot map host memory for the DAX window: {0}")]
    HostMemory(#[from] io::Error),
    #[cfg(target_os = "linux")]
    #[error("Cannot map the DAX window into the guest: {0}")]
    MemorySlot(BoxError),
}

/// The window's guest physical addresses, which the VMM reserved. Released on drop.
struct Reservation {
    vm: Arc<dyn WindowVm>,
    start: u64,
    /// The window's length.
    len: u64,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.vm.release(self.start, self.len);
    }
}

/// The memory slot mapping the window's host memory into the guest. Removed on drop.
#[cfg(target_os = "linux")]
struct Slot {
    vm: Arc<dyn WindowVm>,
    slot: u32,
}

#[cfg(target_os = "linux")]
impl Drop for Slot {
    fn drop(&mut self) {
        self.vm.remove_slot(self.slot);
    }
}

/// Anonymous host memory of the window's size, aligned as the window is, so that the hypervisor
/// maps it for the guest with huge pages where the daemon's are. Unmapped on drop. cladefs:
/// inaccessible until the cell's connection takes it over, as once it lets it go (TASK-79.8): a
/// guest's accesses meanwhile, as before it sets DRIVER_OK, fault, rather than growing its VMM's
/// memory by the window's size.
#[derive(Debug)]
#[cfg(target_os = "linux")]
struct HostMemory {
    base: NonNull<u8>,
    len: usize,
}

// SAFETY: The mapping is only an address range, which the cell's connection maps pages into.
#[cfg(target_os = "linux")]
unsafe impl Send for HostMemory {}

#[cfg(target_os = "linux")]
impl HostMemory {
    fn map(len: usize) -> io::Result<Self> {
        let align = DAX_WINDOW_ALIGNMENT as usize;
        let reserved = len + align;
        // SAFETY: A new anonymous mapping, whose result is checked.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                reserved,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        // The aligned part, unmapping what is around it.
        let start = ptr as usize;
        let base = start.next_multiple_of(align);
        let head = base - start;
        let tail = reserved - head - len;
        // SAFETY: Both are parts of the mapping just made, outside the part kept.
        unsafe {
            if head > 0 {
                libc::munmap(ptr, head);
            }
            if tail > 0 {
                libc::munmap((base + len) as *mut libc::c_void, tail);
            }
        }
        Ok(Self { base: NonNull::new(base as *mut u8).expect("mmap returned a null mapping"), len })
    }
}

#[cfg(target_os = "linux")]
impl Drop for HostMemory {
    fn drop(&mut self) {
        // SAFETY: The mapping is this one's, which nothing uses once it is dropped.
        unsafe {
            libc::munmap(self.base.as_ptr().cast(), self.len);
        }
    }
}

/// The DAX window's memory: its guest physical addresses, and the host memory the hypervisor
/// maps there.
pub(crate) struct WindowMemory {
    // In the order they are dropped in: the hypervisor lets go of the host memory before it is
    // unmapped, and the guest addresses are released last.
    #[cfg(target_os = "linux")]
    _slot: Slot,
    #[cfg(target_os = "linux")]
    host: HostMemory,
    /// The VM, which maps the window's pages into the guest as cladefs's stage says.
    #[cfg(not(target_os = "linux"))]
    vm: Arc<dyn WindowVm>,
    guest: Reservation,
}

impl std::fmt::Debug for WindowMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowMemory")
            .field("guest_addr", &self.guest.start)
            .field("len", &self.guest.len)
            .finish_non_exhaustive()
    }
}

impl WindowMemory {
    /// A window of `len` bytes, a multiple of [`DAX_WINDOW_ALIGNMENT`], mapped into `vm`'s guest:
    /// at `at`, where a snapshot the VM is restored from had it, if given.
    pub(crate) fn new(
        vm: Arc<dyn WindowVm>,
        len: u64,
        at: Option<u64>,
    ) -> Result<Self, DaxWindowError> {
        if len == 0 || !len.is_multiple_of(DAX_WINDOW_ALIGNMENT) {
            return Err(DaxWindowError::Size(len));
        }
        let start = vm.reserve(len, at).map_err(DaxWindowError::GuestAddresses)?;
        let guest = Reservation { vm: vm.clone(), start, len };
        #[cfg(target_os = "linux")]
        {
            let host = HostMemory::map(usize::try_from(len).expect("a window fits in memory"))?;
            let slot = vm.add_slot(start, len, host.base).map_err(DaxWindowError::MemorySlot)?;
            WINDOWS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push((
                start,
                len,
                host.base.as_ptr() as usize,
            ));
            Ok(Self { _slot: Slot { vm, slot }, host, guest })
        }
        #[cfg(not(target_os = "linux"))]
        Ok(Self { vm, guest })
    }

    /// The guest physical address the window starts at.
    pub(crate) fn guest_addr(&self) -> u64 {
        self.guest.start
    }

    /// The window's length in bytes.
    pub(crate) fn len(&self) -> u64 {
        self.guest.len
    }

    #[cfg(target_os = "linux")]
    /// Creates `cell`'s DAX window in the window's host memory, which its connection takes over:
    /// it copies on write, of `private_max` pages at most (backlog doc-13). `prefault` maps the window's pages for the guest as the cell maps
    /// them, if given.
    ///
    /// # Safety
    ///
    /// The window outlives `cell`'s connection, whose fault thread serves the window's host
    /// memory: the cell is dropped first.
    pub(crate) unsafe fn create(
        &self,
        cell: &cladefs::CellClient,
        prefault: Option<Arc<dyn PrefaultGuestMemory>>,
        private_max: u64,
    ) -> Result<(), cladefs::Error> {
        let prefault = prefault.map(|prefault| -> Arc<dyn cladefs::Prefault> {
            Arc::new(WindowPrefault { guest_addr: self.guest_addr(), prefault })
        });
        // SAFETY: The window's host memory is the device's, which only the guest accesses, and
        // which outlives the cell's connection, as the caller says.
        unsafe {
            cell.create_dax_window_in_host_memory(self.host.base, self.len(), prefault, private_max)
        }
    }
}

#[cfg(not(target_os = "linux"))]
impl WindowMemory {
    /// Creates `cell`'s DAX window in the guest's memory at the window's addresses, which the VM
    /// maps the daemon's pages into as cladefs's stage says, and whose faults it hands to the stage
    /// ([`window_fault`]): it copies on write, of `private_max` pages at most (backlog doc-13).
    pub(crate) fn create(
        &self,
        cell: &cladefs::CellClient,
        private_max: u64,
    ) -> Result<(), cladefs::Error> {
        let guest = Arc::new(GuestAt { vm: self.vm.clone(), start: self.guest.start });
        let stage = cell.create_dax_window_in_guest(self.len(), guest, private_max)?;
        STAGES.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push((
            self.guest.start,
            self.guest.len,
            Arc::downgrade(&stage),
        ));
        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl Drop for WindowMemory {
    fn drop(&mut self) {
        let mut windows = WINDOWS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let this = (self.guest.start, self.guest.len, self.host.base.as_ptr() as usize);
        windows.retain(|window| *window != this);
    }
}

#[cfg(not(target_os = "linux"))]
impl Drop for WindowMemory {
    fn drop(&mut self) {
        let mut stages = STAGES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        stages.retain(|(start, _, _)| *start != self.guest.start);
    }
}

/// The guest's memory at a window's addresses, from `start`, as cladefs's stage maps it there.
#[cfg(not(target_os = "linux"))]
struct GuestAt {
    vm: Arc<dyn WindowVm>,
    start: u64,
}

#[cfg(not(target_os = "linux"))]
impl cladefs::GuestMemory for GuestAt {
    unsafe fn map(&self, offset: u64, host: NonNull<u8>, len: u64, write: bool) {
        // SAFETY: As the stage promises: the memory stays mapped until it unmaps it.
        unsafe { self.vm.map(self.start + offset, host, len, write) }
    }

    fn unmap(&self, offset: u64, len: u64) {
        self.vm.unmap(self.start + offset, len);
    }
}

/// cladefs: the DAX windows of this process's guest, by their guest physical addresses and
/// lengths, and their stages: where a vCPU's fault on a window goes ([`window_fault`]). A process
/// has one VM on Hypervisor.framework.
#[cfg(not(target_os = "linux"))]
#[allow(clippy::type_complexity, reason = "a list of windows")]
static STAGES: Mutex<Vec<(u64, u64, std::sync::Weak<cladefs::HvfSecondStage>)>> =
    Mutex::new(Vec::new());

/// cladefs: whether guest physical address `gpa` is in a DAX window of this process's, whose
/// faults [`window_fault`] resolves.
#[cfg(not(target_os = "linux"))]
pub fn is_window(gpa: u64) -> bool {
    let stages = STAGES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    stages.iter().any(|(start, len, _)| (*start..*start + *len).contains(&gpa))
}

/// cladefs: resolves the guest's fault at guest physical address `gpa`, for writing if `write`,
/// if a DAX window of this process's is there, blocking the vCPU until it may retry its access
/// (`HvfSecondStage::fault`): `None` if no window is, the access the VMM's own to emulate; the
/// errno of a fault the guest can't go on from.
#[cfg(not(target_os = "linux"))]
pub fn window_fault(gpa: u64, write: bool) -> Option<Result<(), cladefs::Errno>> {
    let stage = {
        let stages = STAGES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let within = |(start, len, _): &&(u64, u64, _)| (*start..*start + *len).contains(&gpa);
        let (start, _, stage) = stages.iter().find(within)?;
        (*start, stage.upgrade())
    };
    let access = if write { cladefs::Access::Write } else { cladefs::Access::Read };
    Some(match stage {
        (start, Some(stage)) => stage.fault(gpa - start, access),
        // The window's cell let go of it: nothing is there for the guest any more.
        (_, None) => Err(cladefs::Errno::ENOTCONN),
    })
}

/// cladefs: the DAX windows of this process's guests, as their guest physical addresses, lengths
/// and host memory: where a vCPU whose access keeps failing finds what it failed at
/// ([`dax_window_at`]).
#[cfg(target_os = "linux")]
static WINDOWS: Mutex<Vec<(u64, u64, usize)>> = Mutex::new(Vec::new());

/// cladefs: the DAX window guest physical address `gpa` is in, if any: its offset there, and the
/// host address the hypervisor maps there, which cladefs's second stage of the window is in
/// (`cladefs::describe_windows`). For a vCPU to say where an access that kept failing failed: of
/// a process's one VM, as Firecracker's are; with several, a guest address may be in each one's
/// window, and this names the first made.
#[cfg(target_os = "linux")]
pub fn dax_window_at(gpa: u64) -> Option<(u64, usize)> {
    let windows = WINDOWS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let within = |&&(start, len, _): &&(u64, u64, usize)| (start..start + len).contains(&gpa);
    let &(start, _, host) = windows.iter().find(within)?;
    let offset = gpa - start;
    Some((offset, host + usize::try_from(offset).expect("a window fits in memory")))
}

/// Has `prefault` map the pages the cell maps into the window, at the window's guest address.
#[cfg(target_os = "linux")]
struct WindowPrefault {
    guest_addr: u64,
    prefault: Arc<dyn PrefaultGuestMemory>,
}

#[cfg(target_os = "linux")]
impl cladefs::Prefault for WindowPrefault {
    fn prefault(&self, offset: u64, len: u64) {
        self.prefault.prefault(self.guest_addr + offset, len);
    }
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) mod tests {
    use std::sync::Mutex;

    use super::*;

    /// A VM's address space as a test has it: what it reserved, released, and its slots, by the
    /// order they came in.
    #[derive(Debug, Default)]
    pub(crate) struct FakeVm {
        pub(crate) calls: Mutex<Vec<String>>,
        pub(crate) refuse_slots: bool,
    }

    impl WindowVm for FakeVm {
        fn reserve(&self, len: u64, at: Option<u64>) -> Result<u64, BoxError> {
            let start = at.unwrap_or(1 << 40);
            self.calls.lock().unwrap().push(format!("reserve {start:#x} {len:#x}"));
            Ok(start)
        }

        fn release(&self, start: u64, len: u64) {
            self.calls.lock().unwrap().push(format!("release {start:#x} {len:#x}"));
        }

        fn add_slot(&self, guest_addr: u64, len: u64, host: NonNull<u8>) -> Result<u32, BoxError> {
            assert!((host.as_ptr() as u64).is_multiple_of(DAX_WINDOW_ALIGNMENT));
            if self.refuse_slots {
                return Err("no slot".into());
            }
            self.calls.lock().unwrap().push(format!("add_slot {guest_addr:#x} {len:#x}"));
            Ok(3)
        }

        fn remove_slot(&self, slot: u32) {
            self.calls.lock().unwrap().push(format!("remove_slot {slot}"));
        }
    }

    /// A window reserves its addresses, maps its host memory there, aligned, and is found there
    /// by guest address; dropped, its slot goes before its addresses, and it is found no more.
    #[test]
    fn windows_are_reserved_mapped_and_released_in_order() {
        let vm = Arc::new(FakeVm::default());
        let len = 4 << 20;
        let window = WindowMemory::new(vm.clone(), len, Some(0x7_0000_0000)).unwrap();
        assert_eq!((window.guest_addr(), window.len()), (0x7_0000_0000, len));
        let (offset, host) = dax_window_at(0x7_0000_1000).unwrap();
        assert_eq!((offset, host), (0x1000, window.host.base.as_ptr() as usize + 0x1000));
        assert_eq!(dax_window_at(0x7_0000_0000 + len), None);
        drop(window);
        assert_eq!(dax_window_at(0x7_0000_1000), None);
        assert_eq!(
            *vm.calls.lock().unwrap(),
            [
                "reserve 0x700000000 0x400000",
                "add_slot 0x700000000 0x400000",
                "remove_slot 3",
                "release 0x700000000 0x400000"
            ]
        );
    }

    /// A window of a size not a multiple of 2 MiB is refused before anything is reserved; one
    /// whose slot can't be added releases its addresses.
    #[test]
    fn windows_that_cant_be_made_leave_nothing() {
        let vm = Arc::new(FakeVm::default());
        let refused = WindowMemory::new(vm.clone(), 3 << 20, None).unwrap_err();
        assert_eq!(
            refused.to_string(),
            "The DAX window's size, 0x300000 bytes, is not a nonzero multiple of 2 MiB"
        );
        assert!(vm.calls.lock().unwrap().is_empty());
        let vm = Arc::new(FakeVm { refuse_slots: true, ..FakeVm::default() });
        let refused = WindowMemory::new(vm.clone(), 2 << 20, None).unwrap_err();
        assert_eq!(refused.to_string(), "Cannot map the DAX window into the guest: no slot");
        assert_eq!(
            *vm.calls.lock().unwrap(),
            ["reserve 0x10000000000 0x200000", "release 0x10000000000 0x200000"]
        );
    }
}
