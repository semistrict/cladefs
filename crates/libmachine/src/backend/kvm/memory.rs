// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Device memory: host memory mapped into the guest in KVM memory slots of
//! its own, added and removed while the machine runs.

use std::mem::ManuallyDrop;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use kvm_bindings::kvm_userspace_memory_region;
use kvm_ioctls::VmFd;
use vm_memory::{Bytes, MmapRegion, VolatileMemory};

use crate::error::{Error, Result, kvm_err};
use crate::memory::{MemoryLayout, SlotTable};

const PAGE: usize = 4096;

/// Host memory for a device to map into the guest: an anonymous private
/// mapping that is lazily backed (`MAP_NORESERVE`), so reserving even tens
/// of GiB costs no host memory until pages are touched.
pub struct HostMemory {
    region: MmapRegion<()>,
}

impl HostMemory {
    /// Reserves `size` bytes, a non-zero multiple of 4 KiB, reading as zero.
    pub fn new(size: usize) -> Result<Self> {
        if size == 0 || !size.is_multiple_of(PAGE) {
            return Err(Error::MemoryMap(format!(
                "host memory must be a non-zero multiple of {PAGE} bytes, got {size}"
            )));
        }
        let region = MmapRegion::<()>::new(size)
            .map_err(|e| Error::MemoryAllocation(format!("{size} bytes of host memory: {e}")))?;
        Ok(Self { region })
    }

    /// The size in bytes.
    pub fn size(&self) -> usize {
        self.region.size()
    }

    /// The start of the mapping, for code that manages the memory itself
    /// (e.g. with userfaultfd). Valid while `self` lives.
    pub fn as_ptr(&self) -> *mut u8 {
        self.region.as_ptr()
    }

    /// Copies `buf.len()` bytes at `offset` into `buf`.
    pub fn read(&self, offset: usize, buf: &mut [u8]) -> Result<()> {
        let slice = self.region.get_slice(offset, buf.len()).map_err(range_err)?;
        slice.read_slice(buf, 0).map_err(range_err)
    }

    /// Copies `data` to `offset`.
    pub fn write(&self, offset: usize, data: &[u8]) -> Result<()> {
        let slice = self.region.get_slice(offset, data.len()).map_err(range_err)?;
        slice.write_slice(data, 0).map_err(range_err)
    }
}

fn range_err(e: vm_memory::volatile_memory::Error) -> Error {
    Error::MemoryMap(format!("host memory access out of range: {e}"))
}

impl std::fmt::Debug for HostMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostMemory")
            .field("ptr", &self.as_ptr())
            .field("size", &self.size())
            .finish()
    }
}

struct Shared {
    table: Mutex<Slots>,
}

/// The VM's slots, and the VM: None once [retired](MemorySlots::retire).
struct Slots {
    vm: Option<Arc<VmFd>>,
    table: SlotTable,
}

impl Slots {
    /// The VM to map into, unless its machine has let it go.
    fn vm(&self) -> Result<&VmFd> {
        self.vm.as_deref().ok_or_else(|| {
            Error::MemoryMap(
                "this handle's hypervisor VM is gone: its machine was suspended since \
                 (Machine::suspend frees the VM); take a new handle from \
                 Machine::memory_slots once it runs again"
                    .into(),
            )
        })
    }
}

impl Shared {
    fn table(&self) -> MutexGuard<'_, Slots> {
        // The table is consistent between statements, so a panic elsewhere
        // while holding the lock leaves nothing half-done.
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Maps device memory into a machine's guest, from any thread, while it
/// runs or not. Get one from [`Machine::memory_slots`]; clones refer to the
/// same machine.
///
/// The handle belongs to the machine's hypervisor VM, which a suspension
/// frees ([`Machine::suspend`]): from then on it maps nothing (an error
/// says why), and the machine gives a new one once it runs again.
///
/// [`Machine::suspend`]: crate::Machine::suspend
///
/// [`Machine::memory_slots`]: crate::Machine::memory_slots
#[derive(Clone)]
pub struct MemorySlots {
    shared: Arc<Shared>,
}

impl MemorySlots {
    pub(crate) fn new(vm: Arc<VmFd>, table: SlotTable) -> Self {
        Self { shared: Arc::new(Shared { table: Mutex::new(Slots { vm: Some(vm), table }) }) }
    }

    /// Maps `memory` into the guest at `guest_addr`, a multiple of 4 KiB,
    /// in a memory slot of its own. The range must lie inside one of the
    /// layout's MMIO windows ([`RegionKind::holds_device_memory`]) and
    /// overlap no other device memory. The guest sees the memory until the
    /// returned slot is dropped.
    ///
    /// [`RegionKind::holds_device_memory`]: crate::RegionKind::holds_device_memory
    pub fn map(&self, guest_addr: u64, memory: HostMemory) -> Result<MemorySlot> {
        let size = memory.size() as u64;
        let mut slots = self.shared.table();
        slots.vm()?;
        let slot = slots.table.claim(guest_addr, size)?;
        let region = kvm_userspace_memory_region {
            slot,
            flags: 0,
            guest_phys_addr: guest_addr,
            memory_size: size,
            userspace_addr: memory.as_ptr() as u64,
        };
        // SAFETY: `memory` is a live mapping that moves into the returned
        // MemorySlot, whose drop deletes this KVM slot before unmapping it
        // (and leaks the mapping if it cannot delete the slot).
        if let Err(e) = unsafe { slots.vm()?.set_user_memory_region(region) } {
            slots.table.release(slot);
            return Err(kvm_err("add a memory slot")(e));
        }
        drop(slots);
        Ok(MemorySlot { slots: self.clone(), slot, guest_addr, memory: ManuallyDrop::new(memory) })
    }

    /// Maps `len` bytes of host memory a device owns itself (a BAR's shared
    /// memory region) at `guest_addr`, with the same checks as [`map`], and
    /// returns the KVM slot number for [`unmap_raw`].
    ///
    /// # Safety
    /// The host memory must stay mapped until `unmap_raw` removes the slot
    /// or the VM is destroyed.
    ///
    /// [`map`]: MemorySlots::map
    /// [`unmap_raw`]: MemorySlots::unmap_raw
    pub(crate) unsafe fn map_raw(
        &self,
        guest_addr: u64,
        host_addr: *mut u8,
        len: u64,
    ) -> Result<u32> {
        let mut slots = self.shared.table();
        slots.vm()?;
        let slot = slots.table.claim(guest_addr, len)?;
        let region = kvm_userspace_memory_region {
            slot,
            flags: 0,
            guest_phys_addr: guest_addr,
            memory_size: len,
            userspace_addr: host_addr as u64,
        };
        // SAFETY: the caller keeps the memory mapped while the slot exists.
        if let Err(e) = unsafe { slots.vm()?.set_user_memory_region(region) } {
            slots.table.release(slot);
            return Err(kvm_err("add a memory slot")(e));
        }
        Ok(slot)
    }

    /// Removes a slot [`map_raw`](MemorySlots::map_raw) added.
    pub(crate) fn unmap_raw(&self, slot: u32) -> Result<()> {
        let mut slots = self.shared.table();
        let region = kvm_userspace_memory_region { slot, ..Default::default() };
        // SAFETY: a zero-sized region deletes the slot; nothing is mapped.
        unsafe { slots.vm()?.set_user_memory_region(region) }
            .map_err(kvm_err("remove a memory slot"))?;
        slots.table.release(slot);
        Ok(())
    }

    /// The guest's memory map now: RAM, the MMIO windows, and the device
    /// memory mapped into them.
    pub fn layout(&self) -> MemoryLayout {
        self.shared.table().table.layout()
    }

    /// Lets go of the VM, its machine having freed it (a suspension, which
    /// maps no device memory): every clone of the handle maps nothing from
    /// now on, and none keeps the VM open. If this held the VM's last
    /// reference, its file closes on a thread of its own: KVM destroys the
    /// VM as it closes, which takes it 20-30 ms (board 13213: tearing down its
    /// memory map, waiting for grace periods), and no caller needs to wait
    /// for that.
    pub(crate) fn retire(&self) {
        let Some(vm) = self.shared.table().vm.take() else {
            return;
        };
        // If the thread cannot start, the closure, VM and all, drops here.
        let _ = std::thread::Builder::new()
            .name("libmachine-vmfree".into())
            .stack_size(64 << 10)
            .spawn(move || drop(vm));
    }
}

impl std::fmt::Debug for MemorySlots {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemorySlots").field("table", &self.shared.table().table).finish()
    }
}

/// Host memory mapped into the guest in a KVM memory slot of its own
/// ([`MemorySlots::map`]). Dropping it removes the slot, after which guest
/// accesses there are MMIO again, then unmaps the memory.
pub struct MemorySlot {
    slots: MemorySlots,
    slot: u32,
    guest_addr: u64,
    /// Unmapped only once KVM no longer maps it into the guest.
    memory: ManuallyDrop<HostMemory>,
}

impl MemorySlot {
    /// The guest physical address the memory is mapped at.
    pub fn guest_addr(&self) -> u64 {
        self.guest_addr
    }

    /// The size in bytes.
    pub fn size(&self) -> u64 {
        self.memory.size() as u64
    }

    /// The host memory the guest sees.
    pub fn memory(&self) -> &HostMemory {
        &self.memory
    }
}

impl Drop for MemorySlot {
    fn drop(&mut self) {
        let shared = &self.slots.shared;
        let mut slots = shared.table();
        // A zero size deletes the slot. KVM has stopped every vCPU's access
        // through it when the ioctl returns.
        let region = kvm_userspace_memory_region {
            slot: self.slot,
            flags: 0,
            guest_phys_addr: self.guest_addr,
            memory_size: 0,
            userspace_addr: 0,
        };
        // A retired handle's VM is going (only a race with a suspension
        // gets here: it refuses machines with device memory mapped); KVM
        // may still map the memory until then, so it is leaked below.
        let deleted = slots.vm().and_then(|vm| {
            // SAFETY: deleting a slot hands KVM no memory.
            unsafe { vm.set_user_memory_region(region) }.map_err(kvm_err("remove a memory slot"))
        });
        match deleted {
            Ok(()) => {
                slots.table.release(self.slot);
                // SAFETY: `memory` is not used again, and KVM no longer maps it.
                unsafe { ManuallyDrop::drop(&mut self.memory) };
            }
            // Unmapping memory the guest can still reach would let it reach
            // whatever the host maps there next: leak the mapping, and the
            // slot with it.
            Err(e) => eprintln!(
                "libmachine: cannot remove memory slot {} at {:#x} ({e}); leaking its memory",
                self.slot, self.guest_addr
            ),
        }
    }
}

impl std::fmt::Debug for MemorySlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemorySlot")
            .field("slot", &self.slot)
            .field("guest_addr", &format_args!("{:#x}", self.guest_addr))
            .field("size", &self.size())
            .finish()
    }
}

// Devices map and unmap memory from their own threads.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<HostMemory>();
    assert_send_sync::<MemorySlots>();
    assert_send_sync::<MemorySlot>();
};
