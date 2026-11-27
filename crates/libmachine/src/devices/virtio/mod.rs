// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Virtio devices (virtio 1.2), all attached over the modern virtio-PCI
//! transport (doc-30), and the thread that runs their queues.

pub(crate) mod balloon;
pub(crate) mod blk;
pub(crate) mod extent;
pub(crate) mod net;
pub(crate) mod pci;
pub(crate) mod queue;
pub(crate) mod rng;
#[cfg(test)]
pub(crate) mod test_device;
#[cfg(test)]
pub(crate) mod test_driver;
pub(crate) mod vsock;
pub(crate) mod worker;

use std::sync::Arc;

use crate::sys::EventFd;
use vm_memory::{GuestMemoryMmap, MmapRegion};

use queue::DeviceQueue;

/// VIRTIO_F_VERSION_1: a modern device. The transport always offers it, and
/// a driver that does not accept it is refused.
pub(crate) const VIRTIO_F_VERSION_1: u32 = 32;
/// VIRTIO_RING_F_EVENT_IDX: the used and available event indexes suppress
/// notifications and interrupts. The transport offers it for every device.
pub(crate) const VIRTIO_RING_F_EVENT_IDX: u32 = 29;

/// A shared memory region (virtio 1.2, section 2.10): host memory the guest
/// maps directly, through the transport's shared memory BAR.
pub(crate) struct ShmRegion {
    /// The region's ID, as the device type defines them.
    pub(crate) id: u8,
    /// The host memory; its size is a multiple of 4 KiB.
    pub(crate) memory: Arc<MmapRegion>,
}

/// A shared memory region a device placed in the guest's physical address space and maps there
/// itself, rather than the transport ([`ShmRegion`]): the extent store device's DAX window,
/// whose guest addresses it reserved and whose memory slot it adds, as cladefs maps the window's
/// pages (`virtio_extent::WindowVm`). The transport puts BAR 2 there and describes the region,
/// and never maps or moves it: a guest that moves the BAR moves no window, as on Firecracker,
/// which relocates no BAR either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlacedShm {
    /// The region's ID, as the device type defines them.
    pub(crate) id: u8,
    /// Where the region starts: BAR 2's address, aligned to `bar_size`.
    pub(crate) guest_addr: u64,
    /// The region's length, a multiple of 4 KiB.
    pub(crate) len: u64,
    /// BAR 2's size: a power of two, `len` at least.
    pub(crate) bar_size: u64,
}

/// What a device whose own threads serve its queues ([`VirtioDevice::serves_its_queues`]) is
/// given as the driver activates it: the queues the driver set up, guest memory, each queue's
/// notification event, and the eventfds of the MSI-X vectors the driver chose for each queue and
/// for configuration changes, which interrupt the guest when signalled, from any thread, without
/// the transport (whose lock a reset holds while the device's threads finish).
pub(crate) struct OwnQueues<'a> {
    pub(crate) queues: &'a [DeviceQueue],
    pub(crate) mem: &'a GuestMemoryMmap,
    pub(crate) notify: &'a [EventFd],
    /// Each queue's vector's eventfd: none for a queue with no vector.
    pub(crate) queue_vectors: Vec<Option<EventFd>>,
    pub(crate) config_vector: Option<EventFd>,
}

/// A device's own state in a snapshot: what it keeps outside guest memory
/// and the transport, with a version of the device's choosing. A device
/// refuses a version it did not write (no conversions: AGENTS.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeviceState {
    pub(crate) version: u32,
    pub(crate) data: Vec<u8>,
}

/// The error for a device that cannot be snapshotted.
pub(crate) fn not_snapshottable(device_type: u16, why: &str) -> crate::error::Error {
    crate::error::Error::Snapshot(format!(
        "virtio device type {device_type} cannot be snapshotted: {why}"
    ))
}

/// The error for a device state of another version.
pub(crate) fn other_version(device_type: u16, theirs: u32, ours: u32) -> crate::error::Error {
    crate::error::Error::Snapshot(format!(
        "the snapshot holds version {theirs} of virtio device type {device_type}'s state, \
         this libmachine version {ours}"
    ))
}

/// A virtio device: what is specific to its type. The transport owns the
/// queues and interrupts, and asks the device to process a queue when the
/// driver notifies it.
pub(crate) trait VirtioDevice: Send {
    /// The virtio device type (virtio 1.2, section 5).
    fn device_type(&self) -> u16;
    /// Each queue's maximum size (a power of two).
    fn queue_max_sizes(&self) -> &[u16];
    /// The device-type feature bits it offers (the transport adds its own).
    fn features(&self) -> u64 {
        0
    }
    /// The size of its device-specific configuration, 0 if it has none.
    fn config_size(&self) -> usize {
        0
    }
    fn read_config(&self, _offset: usize, data: &mut [u8]) {
        data.fill(0);
    }
    fn write_config(&mut self, _offset: usize, _data: &[u8]) {}
    /// Its shared memory regions.
    fn shm_regions(&self) -> &[ShmRegion] {
        &[]
    }
    /// The shared memory region it placed and maps itself, if any ([`PlacedShm`]); a device has
    /// it or [`shm_regions`](Self::shm_regions), not both.
    fn placed_shm(&self) -> Option<PlacedShm> {
        None
    }
    /// Whether threads of its own serve its queues, rather than the device thread through
    /// [`process_queue`](Self::process_queue): the device thread then leaves its queues'
    /// notification events alone, and the driver's DRIVER_OK reaches it as
    /// [`activate_queues`](Self::activate_queues).
    fn serves_its_queues(&self) -> bool {
        false
    }
    /// The driver set DRIVER_OK having accepted `features`.
    fn activate(&mut self, _features: u64) {}
    /// The driver set DRIVER_OK having accepted `features`, for a device that
    /// [serves its queues](Self::serves_its_queues): it starts serving `own`'s, copies of the
    /// transport's, until the driver resets it. An error refuses the activation: the device
    /// needs a reset (DEVICE_NEEDS_RESET); telling of it is the device's.
    fn activate_queues(&mut self, _features: u64, _own: OwnQueues<'_>) -> crate::error::Result<()> {
        Err(crate::error::Error::InvalidConfig(format!(
            "virtio device type {:#x} does not serve its own queues",
            self.device_type()
        )))
    }
    /// The driver reset the device.
    fn reset(&mut self) {}
    /// Takes what the driver made available on queue `index` (which is
    /// ready), and may use any other ready queue while at it: a reply goes
    /// out on another queue. The transport interrupts the driver for each
    /// queue whose used ring moved, if the driver wants to hear of it.
    fn process_queue(&mut self, index: usize, queues: &mut [DeviceQueue], mem: &GuestMemoryMmap);
    /// An eventfd the host side of the device signals when it has work for
    /// the guest; the device thread then calls [`woken`](Self::woken).
    fn wake_fd(&self) -> Option<&EventFd> {
        None
    }
    /// Whether the host side holds something for the guest that it has not
    /// yet put in a queue: a guest with that waiting is not idle, whatever
    /// its vCPUs do ([`crate::suspend`]).
    fn pending_for_guest(&self) -> bool {
        false
    }
    /// The host side signalled [`wake_fd`](Self::wake_fd): use the ready
    /// queues as for [`process_queue`](Self::process_queue).
    fn woken(&mut self, _queues: &mut [DeviceQueue], _mem: &GuestMemoryMmap) {}

    /// Whether its configuration changed on its own since asked last (the balloon's target): the
    /// transport then tells the driver (a configuration change interrupt). Asked as it is woken.
    fn take_config_change(&mut self) -> bool {
        false
    }

    /// Stops the threads of a device that serves its own queues
    /// ([`serves_its_queues`](Self::serves_its_queues)), once what they took is served, and gives
    /// back each queue they served, by index, as they left it: for a snapshot, which saves the
    /// queues where they are. The transport starts the device on them again after it
    /// ([`activate_queues`](Self::activate_queues)). Nothing by default.
    fn pause_queues(&mut self) -> Vec<(usize, DeviceQueue)> {
        Vec::new()
    }

    /// Takes what a snapshot of the machine needs of the device besides its
    /// [`save`](Self::save), as `mode` says, its queues paused: an extent store device's
    /// version of its cell (or a migration's mappings), the state a restore makes the device
    /// from again. None by default.
    fn take_snapshot(
        &mut self,
        _mode: virtio_extent::SnapshotMode,
    ) -> crate::error::Result<Option<virtio_extent::persist::State>> {
        Ok(None)
    }

    /// Drops what [`take_snapshot`](Self::take_snapshot) took: the machine's snapshot failed
    /// after it, and no file names it.
    fn drop_snapshot(&mut self) {}

    /// The device's state for a snapshot, taken with every vCPU stopped
    /// and the device thread held off (guest memory and the queues are the
    /// transport's to save). A device that keeps state the guest relies on
    /// outside them must save it; by default a device refuses.
    fn save(&self) -> crate::error::Result<DeviceState> {
        Err(not_snapshottable(self.device_type(), "it does not save its state"))
    }

    /// The device's state for a suspension ([`crate::suspend`]), which
    /// frees the machine and restores it later in this process: like
    /// [`save`](Self::save), but the device stops acting for the guest and
    /// hands what must outlive its machine (host connections) to whoever
    /// will restore it, rather than carrying on. [`Error::Busy`] if it
    /// would act for the guest by itself, as it then still does. By
    /// default, what `save` gives.
    ///
    /// [`Error::Busy`]: crate::Error::Busy
    fn hand_over(&mut self) -> crate::error::Result<DeviceState> {
        self.save()
    }

    /// Undoes [`hand_over`](Self::hand_over), the suspension having failed:
    /// the device carries on as before it. Nothing if it handed nothing.
    fn take_back(&mut self) -> crate::error::Result<()> {
        Ok(())
    }

    /// Puts back what [`save`](Self::save) gave, in a new machine restored
    /// from the snapshot: the transport has put its own state back, and
    /// activated the device with the features the driver had accepted if
    /// it was active.
    fn restore(&mut self, _state: &DeviceState) -> crate::error::Result<()> {
        Err(not_snapshottable(self.device_type(), "it does not restore its state"))
    }
}
