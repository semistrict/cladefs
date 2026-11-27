// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The modern virtio-PCI transport (virtio 1.2, section 4.1).
//!
//! BAR 0 (32-bit, 64 KiB) holds the transport's structures, each in a page
//! of its own:
//!
//! ```text
//! 0x0000  common configuration (64 bytes)
//! 0x1000  ISR status (1 byte)
//! 0x2000  device-specific configuration (up to 4 KiB)
//! 0x3000  queue notifications, a dword per queue, each an ioeventfd
//! 0x4000  MSI-X table (up to 2048 vectors)
//! 0xc000  MSI-X pending bit array
//! ```
//!
//! BAR 2 (64-bit, prefetchable), present only if the device has shared
//! memory regions, maps them: one VIRTIO_PCI_CAP_SHARED_MEMORY_CFG
//! capability each, the memory mapped straight into the guest.
//!
//! Interrupts are MSI-X only: the driver picks a vector for configuration
//! changes and one per queue (each queue can have its own). With MSI-X
//! disabled the ISR still records interrupts, but no line is raised: there
//! is no INTx on this platform.

use std::ops::Range;

use crate::sys::{EFD_NONBLOCK, EventFd};
use vm_memory::GuestMemoryMmap;

use super::queue::DeviceQueue;
use super::{DeviceState, OwnQueues, VIRTIO_F_VERSION_1, VIRTIO_RING_F_EVENT_IDX, VirtioDevice};
use crate::devices::pci::config::{Bar, ConfigSpace, Header, overlaps};
use crate::devices::pci::msix::{self, Msix};
use crate::devices::pci::{BarAllocator, PciFunction, PciHost};
use crate::error::{Error, Result};
use crate::snapshot::codec::{Decoder, Encoder};

const VIRTIO_PCI_VENDOR_ID: u16 = 0x1af4;
/// A modern device's PCI device ID is this plus its type, for types 0-63.
const MODERN_DEVICE_ID_BASE: u16 = 0x1040;
const LAST_MODERN_DEVICE_ID: u16 = 0x107f;
/// The last transitional device ID, which types past 63 use (pci_ids).
const LAST_TRANSITIONAL_DEVICE_ID: u16 = 0x103f;

const CAPABILITY_BAR: u8 = 0;
const COMMON_CFG: Range<u64> = 0x0000..0x0040;
const ISR: Range<u64> = 0x1000..0x1001;
const DEVICE_CFG_OFFSET: u64 = 0x2000;
const DEVICE_CFG_MAX: usize = 0x1000;
const NOTIFY_OFFSET: u64 = 0x3000;
const NOTIFY_OFF_MULTIPLIER: u32 = 4;
const MAX_QUEUES: usize = 0x1000 / NOTIFY_OFF_MULTIPLIER as usize;
const MSIX_TABLE_OFFSET: u64 = 0x4000;
const MSIX_PBA_OFFSET: u64 = 0xc000;
const CAPABILITY_BAR_SIZE: u64 = 0x1_0000;
const SHM_BAR: u8 = 2;
/// As many shared memory capabilities as fit after the transport's own in
/// the 256-byte configuration space.
pub(crate) const MAX_SHM_REGIONS: usize = 3;

// Virtio PCI capability structure types (cfg_type).
const VIRTIO_PCI_CAP_VENDOR: u8 = 0x09;
pub(crate) const CAP_COMMON_CFG: u8 = 1;
pub(crate) const CAP_NOTIFY_CFG: u8 = 2;
pub(crate) const CAP_ISR_CFG: u8 = 3;
pub(crate) const CAP_DEVICE_CFG: u8 = 4;
pub(crate) const CAP_PCI_CFG: u8 = 5;
pub(crate) const CAP_SHARED_MEMORY_CFG: u8 = 8;
/// In VIRTIO_PCI_CAP_PCI_CFG: where the access window's data is.
const PCI_CFG_DATA: usize = 16;

/// No MSI-X vector.
pub(crate) const NO_VECTOR: u16 = 0xffff;

// Device status bits (virtio 1.2, section 2.1).
pub(crate) const STATUS_DRIVER_OK: u8 = 4;
pub(crate) const STATUS_FEATURES_OK: u8 = 8;
pub(crate) const STATUS_DEVICE_NEEDS_RESET: u8 = 0x40;

/// The guest-writable fields of the common configuration, as (offset,
/// size); the 64-bit queue addresses are two dwords each.
const COMMON_WRITABLE: [(usize, usize); 15] = [
    (0, 4),  // device_feature_select
    (8, 4),  // driver_feature_select
    (12, 4), // driver_feature
    (16, 2), // config_msix_vector
    (20, 1), // device_status
    (22, 2), // queue_select
    (24, 2), // queue_size
    (26, 2), // queue_msix_vector
    (28, 2), // queue_enable
    (32, 4), // queue_desc, low
    (36, 4), // queue_desc, high
    (40, 4), // queue_driver, low
    (44, 4), // queue_driver, high
    (48, 4), // queue_device, low
    (52, 4), // queue_device, high
];

/// A virtio device type's PCI device ID, subsystem ID and revision ID.
///
/// Types 0-63 take their modern ID, 0x1040 plus the type. A type past 63,
/// such as cladefs's extent device (0x4658), takes the last transitional ID,
/// 0x103f, with its type as the subsystem ID, as virtio 1.2 section 4.1.2.1
/// has transitional devices say theirs: the modern formula would give an ID
/// past 0x107f, which Linux's virtio_pci never binds, and 0x107f itself
/// means type 63 to it (vp_modern_probe). The device is still modern-only:
/// Linux finds its capabilities and binds it modern. Revision ID 0 for the
/// transitional ID, as section 4.1.2.1 requires; 1 for modern ones.
/// The subsystem ID is the type in both cases.
pub(crate) fn pci_ids(device_type: u16) -> (u16, u16, u8) {
    match MODERN_DEVICE_ID_BASE.checked_add(device_type) {
        Some(id) if id <= LAST_MODERN_DEVICE_ID => (id, device_type, 1),
        _ => (LAST_TRANSITIONAL_DEVICE_ID, device_type, 0),
    }
}

/// The PCI class of a virtio device type: what it does, for tools that
/// list devices (drivers match on the IDs).
fn class_code(device_type: u16) -> u32 {
    match device_type {
        1 => 0x02_00_00, // network controller, Ethernet
        2 => 0x01_80_00, // mass storage, other
        3 => 0x07_80_00, // communication controller, other
        _ => 0xff_00_00, // unassigned class
    }
}

/// Where each shared memory region goes in BAR 2, and the BAR's size: in
/// order, each at an offset aligned to its size rounded up to a power of two
/// (at most 1 GiB, so the guest can map it with huge pages), the BAR the
/// power of two that covers them all.
pub(crate) fn shm_layout(sizes: &[u64]) -> (Vec<u64>, u64) {
    let mut end = 0u64;
    let offsets = sizes
        .iter()
        .map(|&size| {
            let offset = end.next_multiple_of(size.next_power_of_two().min(1 << 30));
            end = offset + size;
            offset
        })
        .collect();
    (offsets, end.next_power_of_two().max(0x1000))
}

/// A `struct virtio_pci_cap` (or `virtio_pci_cap64`, with `extra`).
fn virtio_cap(cfg_type: u8, bar: u8, id: u8, offset: u32, length: u32, extra: &[u8]) -> Vec<u8> {
    let mut cap = vec![VIRTIO_PCI_CAP_VENDOR, 0, 0, cfg_type, bar, id, 0, 0];
    cap.extend_from_slice(&offset.to_le_bytes());
    cap.extend_from_slice(&length.to_le_bytes());
    cap.extend_from_slice(extra);
    cap[2] = cap.len() as u8;
    cap
}

/// Counts of what the transport did, for tests and diagnostics.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stats {
    /// Queue notifications that exited to the VMM (no ioeventfd caught them).
    pub(crate) notify_exits: u64,
    /// Queue notifications the device thread took from an ioeventfd.
    pub(crate) notifies: u64,
    /// Used-buffer interrupts signalled.
    pub(crate) interrupts: u64,
}

/// A virtio device on the PCI bus.
pub(crate) struct VirtioPci {
    config: ConfigSpace,
    msix: Msix,
    msix_cap: usize,
    pci_cfg_cap: usize,
    device: Box<dyn VirtioDevice>,
    mem: GuestMemoryMmap,
    device_features: u64,

    // The common configuration.
    device_feature_select: u32,
    driver_feature_select: u32,
    driver_features: u64,
    status: u8,
    config_vector: u16,
    queue_select: u16,
    queues: Vec<DeviceQueue>,
    queue_vectors: Vec<u16>,
    isr: u8,
    activated: bool,
    /// The device's own queue threads are stopped for a snapshot ([`VirtioPci::prepare_snapshot`]),
    /// to start again once it is taken.
    paused: bool,
    /// The device took something of its own for the snapshot being taken, which a failed one
    /// drops.
    took_snapshot: bool,
    /// Whether its device let go of what it serves the guest from, the machine paused: an extent
    /// store device's cell, for a live migration's hand-over ([`Self::release_cell`]). Its queues
    /// are paused until it takes it back ([`Self::reopen_cell`]).
    released: bool,
    /// Bumped each time the device's configuration changes on its own (the balloon's target), so
    /// that a driver reading it reads it whole (virtio 1.2, section 4.1.4.3).
    config_generation: u8,

    /// Each queue's notification eventfd.
    notify: Vec<EventFd>,
    /// Where the notify ioeventfds are registered (BAR 0's address).
    notify_registered: Option<u64>,
    /// Each shared memory region's offset in BAR 2.
    shm_offsets: Vec<u64>,
    /// Where the regions are mapped (BAR 2's address), and their slots.
    shm_mapped: Option<u64>,
    shm_slots: Vec<u32>,
    stats: Stats,
}

impl VirtioPci {
    /// The transport for `device`, its BARs allocated from `bars` and live,
    /// its MSI-X vectors on GSIs from `first_gsi`.
    pub(crate) fn new(
        device: Box<dyn VirtioDevice>,
        mem: GuestMemoryMmap,
        bars: &mut BarAllocator,
        first_gsi: u32,
        host: &dyn PciHost,
    ) -> Result<Self> {
        let device_type = device.device_type();
        let invalid = |what: String| {
            Error::InvalidConfig(format!("virtio device type {device_type:#x}: {what}"))
        };
        let queue_count = device.queue_max_sizes().len();
        if queue_count > MAX_QUEUES || queue_count >= usize::from(msix::MAX_VECTORS) {
            return Err(invalid(format!("{queue_count} queues")));
        }
        let queues = device
            .queue_max_sizes()
            .iter()
            .map(|&max| {
                DeviceQueue::new(max).map_err(|e| invalid(format!("queue size {max}: {e}")))
            })
            .collect::<Result<Vec<_>>>()?;
        if device.config_size() > DEVICE_CFG_MAX {
            return Err(invalid(format!("{} bytes of configuration", device.config_size())));
        }
        let regions = device.shm_regions();
        if regions.len() > MAX_SHM_REGIONS {
            return Err(invalid(format!("{} shared memory regions", regions.len())));
        }
        let shm_sizes: Vec<u64> = regions.iter().map(|r| r.memory.size() as u64).collect();
        if shm_sizes.iter().any(|&s| s == 0 || s % 4096 != 0) {
            return Err(invalid("a shared memory region is not a whole number of pages".into()));
        }
        let placed = device.placed_shm();
        if let Some(placed) = placed {
            let window = BarAllocator::window64();
            let fits = placed.bar_size.is_power_of_two()
                && placed.len.is_multiple_of(4096)
                && (4096..=placed.bar_size).contains(&placed.len)
                && placed.guest_addr.is_multiple_of(placed.bar_size)
                && window.start <= placed.guest_addr
                && placed
                    .guest_addr
                    .checked_add(placed.bar_size)
                    .is_some_and(|end| end <= window.end);
            if !regions.is_empty() || !fits {
                return Err(invalid(format!("a placed shared memory region {placed:x?}")));
            }
        }

        // A vector for configuration changes, and one per queue.
        let msix = Msix::new(queue_count as u16 + 1, first_gsi)?;
        let (device_id, subsystem_id, revision_id) = pci_ids(device_type);
        let mut config = ConfigSpace::new(Header {
            vendor_id: VIRTIO_PCI_VENDOR_ID,
            device_id,
            revision_id,
            class_code: class_code(device_type),
            subsystem_vendor_id: VIRTIO_PCI_VENDOR_ID,
            subsystem_id,
        });

        let bar0 = Bar {
            size: CAPABILITY_BAR_SIZE,
            is_64bit: false,
            prefetchable: false,
            window: BarAllocator::window32(),
        };
        let bar0_addr = bars.allocate(CAPABILITY_BAR_SIZE, false)?;
        config.add_bar(usize::from(CAPABILITY_BAR), bar0, bar0_addr);
        let (shm_offsets, shm_bar_size) = shm_layout(&shm_sizes);
        let shm_bar = match placed {
            Some(placed) => Some((placed.bar_size, placed.guest_addr)),
            None if !regions.is_empty() => Some((shm_bar_size, bars.allocate(shm_bar_size, true)?)),
            None => None,
        };
        if let Some((size, addr)) = shm_bar {
            let bar2 =
                Bar { size, is_64bit: true, prefetchable: true, window: BarAllocator::window64() };
            config.add_bar(usize::from(SHM_BAR), bar2, addr);
        }

        let cap = |cfg_type, range: Range<u64>| {
            let len = (range.end - range.start) as u32;
            virtio_cap(cfg_type, CAPABILITY_BAR, 0, range.start as u32, len, &[])
        };
        config.add_capability(&cap(CAP_COMMON_CFG, COMMON_CFG), &[]);
        config.add_capability(&cap(CAP_ISR_CFG, ISR), &[]);
        if device.config_size() > 0 {
            let range = DEVICE_CFG_OFFSET..DEVICE_CFG_OFFSET + device.config_size() as u64;
            config.add_capability(&cap(CAP_DEVICE_CFG, range), &[]);
        }
        let notify = virtio_cap(
            CAP_NOTIFY_CFG,
            CAPABILITY_BAR,
            0,
            NOTIFY_OFFSET as u32,
            queue_count as u32 * NOTIFY_OFF_MULTIPLIER,
            &NOTIFY_OFF_MULTIPLIER.to_le_bytes(),
        );
        config.add_capability(&notify, &[]);
        // The access window through configuration space, which the spec
        // requires: the driver writes bar, offset and length (bytes 4,
        // 8-15), then accesses the data (16-19).
        let pci_cfg = virtio_cap(CAP_PCI_CFG, 0, 0, 0, 0, &[0; 4]);
        let mut writable = vec![0; pci_cfg.len()];
        writable[4] = 0xff;
        writable[8..20].fill(0xff);
        let pci_cfg_cap = config.add_capability(&pci_cfg, &writable);
        let (msix_body, msix_writable) =
            msix.capability(CAPABILITY_BAR, MSIX_TABLE_OFFSET as u32, MSIX_PBA_OFFSET as u32);
        let msix_cap = config.add_capability(&msix_body, &msix_writable);
        let described = regions
            .iter()
            .map(|region| (region.id, region.memory.size() as u64))
            .zip(shm_offsets.iter().copied())
            .chain(placed.map(|placed| ((placed.id, placed.len), 0)));
        for ((id, len), offset) in described {
            let mut high = ((offset >> 32) as u32).to_le_bytes().to_vec();
            high.extend_from_slice(&((len >> 32) as u32).to_le_bytes());
            let cap =
                virtio_cap(CAP_SHARED_MEMORY_CFG, SHM_BAR, id, offset as u32, len as u32, &high);
            config.add_capability(&cap, &[]);
        }
        // Firmware leaves the device decoding its BARs.
        config.set_memory_decode(true);

        let notify = (0..queue_count)
            .map(|_| {
                EventFd::new(EFD_NONBLOCK).map_err(|source| Error::Hypervisor {
                    op: "create a queue's notification eventfd",
                    source,
                })
            })
            .collect::<Result<_>>()?;
        let device_features =
            device.features() | 1 << VIRTIO_F_VERSION_1 | 1 << VIRTIO_RING_F_EVENT_IDX;
        let mut this = Self {
            config,
            msix,
            msix_cap,
            pci_cfg_cap,
            device,
            mem,
            device_features,
            device_feature_select: 0,
            driver_feature_select: 0,
            driver_features: 0,
            status: 0,
            config_vector: NO_VECTOR,
            queue_select: 0,
            queues,
            queue_vectors: vec![NO_VECTOR; queue_count],
            isr: 0,
            config_generation: 0,
            activated: false,
            paused: false,
            took_snapshot: false,
            released: false,
            notify,
            notify_registered: None,
            shm_offsets,
            shm_mapped: None,
            shm_slots: Vec::new(),
            stats: Stats::default(),
        };
        this.sync_bars(host)?;
        Ok(this)
    }

    /// How many MSI-X vectors (and so GSIs) the transport uses.
    pub(crate) fn vector_count(&self) -> u16 {
        self.msix.count()
    }

    #[cfg_attr(not(test), allow(dead_code, reason = "diagnostics, read by tests"))]
    pub(crate) fn stats(&self) -> Stats {
        self.stats
    }

    /// Each queue's notification eventfd, for the device thread to wait on: none of a device
    /// that serves its own queues ([`VirtioDevice::serves_its_queues`]), whose threads wait on
    /// them.
    pub(crate) fn notify_fds(&self) -> Result<Vec<EventFd>> {
        if self.device.serves_its_queues() {
            return Ok(Vec::new());
        }
        self.notify
            .iter()
            .map(|evt| {
                evt.try_clone().map_err(|source| Error::Hypervisor {
                    op: "duplicate a queue's notification eventfd",
                    source,
                })
            })
            .collect()
    }

    /// The device's wake eventfd, if it has one, for the device thread to
    /// wait on.
    pub(crate) fn wake_fd(&self) -> Result<Option<EventFd>> {
        self.device
            .wake_fd()
            .map(|evt| {
                evt.try_clone().map_err(|source| Error::Hypervisor {
                    op: "duplicate a device's wake eventfd",
                    source,
                })
            })
            .transpose()
    }

    /// The driver notified queue `index` (through its ioeventfd).
    pub(crate) fn notified(&mut self, index: usize) {
        self.stats.notifies += 1;
        self.process_queue(index);
    }

    /// Lets the device take what the driver made available on queue
    /// `index`, and interrupts the driver for each queue the device used,
    /// if the driver wants to hear of it.
    pub(crate) fn process_queue(&mut self, index: usize) {
        if !self.activated || !self.queues.get(index).is_some_and(|q| q.ready()) {
            return;
        }
        let before = self.used_positions();
        self.device.process_queue(index, &mut self.queues, &self.mem);
        self.signal_moved(&before);
    }

    /// The device's host side has work for the guest, or changed its configuration.
    pub(crate) fn woken(&mut self) {
        if !self.activated {
            return;
        }
        let before = self.used_positions();
        self.device.woken(&mut self.queues, &self.mem);
        self.signal_moved(&before);
        if self.device.take_config_change() {
            self.config_generation = self.config_generation.wrapping_add(1);
            self.signal_config();
        }
    }

    /// Interrupts the driver for a configuration change: its MSI-X vector, or the ISR's second
    /// bit (virtio 1.2, section 4.1.4.5).
    fn signal_config(&mut self) {
        self.stats.interrupts += 1;
        if self.msix.enabled() {
            if self.config_vector != NO_VECTOR {
                self.msix.trigger(self.config_vector);
            }
        } else {
            self.isr |= 2;
        }
    }

    /// Where each queue's used ring is now.
    fn used_positions(&self) -> Vec<u16> {
        self.queues.iter().map(|q| q.next_used()).collect()
    }

    /// Interrupts for each queue whose used ring moved since `before`.
    fn signal_moved(&mut self, before: &[u16]) {
        for (index, &was) in before.iter().enumerate() {
            if self.queues[index].next_used() == was {
                continue;
            }
            if self.queues[index].needs_notification(&self.mem) {
                self.signal_used(index);
            }
        }
    }

    fn signal_used(&mut self, index: usize) {
        self.stats.interrupts += 1;
        if self.msix.enabled() {
            let vector = self.queue_vectors[index];
            if vector != NO_VECTOR {
                self.msix.trigger(vector);
            }
        } else {
            self.isr |= 1;
        }
    }

    /// Brings what the hypervisor maps in line with where the BARs are
    /// decoded: queue notification ioeventfds in BAR 0, the shared memory
    /// regions in BAR 2.
    fn sync_bars(&mut self, host: &dyn PciHost) -> Result<()> {
        let decoded = self.config.decoded_bars();
        let stride = u64::from(NOTIFY_OFF_MULTIPLIER);
        let notify_at = decoded[usize::from(CAPABILITY_BAR)].map(|bar| bar + NOTIFY_OFFSET);
        if notify_at != self.notify_registered {
            if let Some(old) = self.notify_registered.take() {
                for (q, evt) in self.notify.iter().enumerate() {
                    host.unregister_ioeventfd(evt, old + q as u64 * stride)?;
                }
            }
            if let Some(new) = notify_at {
                for (q, evt) in self.notify.iter().enumerate() {
                    host.register_ioeventfd(evt, new + q as u64 * stride)?;
                }
                self.notify_registered = Some(new);
            }
        }

        let shm_at = decoded[usize::from(SHM_BAR)].filter(|_| !self.shm_offsets.is_empty());
        if shm_at != self.shm_mapped {
            for slot in self.shm_slots.drain(..) {
                host.unmap_memory(slot)?;
            }
            self.shm_mapped = None;
            if let Some(base) = shm_at {
                for (region, &offset) in self.device.shm_regions().iter().zip(&self.shm_offsets) {
                    let (ptr, len) = (region.memory.as_ptr(), region.memory.size() as u64);
                    // SAFETY: the region's memory belongs to the device, which
                    // this transport owns; the transport lives as long as the
                    // VM, whose machine destroys the VM before its devices.
                    let slot = unsafe { host.map_memory(base + offset, ptr, len) }?;
                    self.shm_slots.push(slot);
                }
                self.shm_mapped = Some(base);
            }
        }
        Ok(())
    }

    fn valid_vector(&self, vector: u16) -> u16 {
        // A vector the table lacks reads back as NO_VECTOR, which tells the
        // driver it was refused (virtio 1.2, section 4.1.4.3.1).
        if vector < self.msix.count() { vector } else { NO_VECTOR }
    }

    fn selected_queue(&self) -> Option<&DeviceQueue> {
        self.queues.get(usize::from(self.queue_select))
    }

    /// The common configuration as the guest reads it now.
    fn common_cfg(&self) -> [u8; 64] {
        let mut b = [0; 64];
        let mut put = |off: usize, bytes: &[u8]| b[off..off + bytes.len()].copy_from_slice(bytes);
        let word = |features: u64, select: u32| match select {
            0 => features as u32,
            1 => (features >> 32) as u32,
            _ => 0,
        };
        put(0, &self.device_feature_select.to_le_bytes());
        put(4, &word(self.device_features, self.device_feature_select).to_le_bytes());
        put(8, &self.driver_feature_select.to_le_bytes());
        put(12, &word(self.driver_features, self.driver_feature_select).to_le_bytes());
        put(16, &self.config_vector.to_le_bytes());
        put(18, &(self.queues.len() as u16).to_le_bytes());
        put(20, &[self.status, self.config_generation]);
        put(22, &self.queue_select.to_le_bytes());
        if let Some(q) = self.selected_queue() {
            let index = usize::from(self.queue_select);
            put(24, &q.size().to_le_bytes());
            put(26, &self.queue_vectors[index].to_le_bytes());
            put(28, &u16::from(q.ready()).to_le_bytes());
            put(30, &self.queue_select.to_le_bytes()); // queue_notify_off
            put(32, &q.desc_table().to_le_bytes());
            put(40, &q.avail_ring().to_le_bytes());
            put(48, &q.used_ring().to_le_bytes());
        }
        b
    }

    fn common_read(&self, offset: usize, data: &mut [u8]) {
        let cfg = self.common_cfg();
        for (i, b) in data.iter_mut().enumerate() {
            *b = cfg.get(offset + i).copied().unwrap_or(0);
        }
    }

    /// A guest write to the common configuration: each field it touches
    /// takes the bytes written over its current value.
    fn common_write(&mut self, offset: usize, data: &[u8]) {
        let mut cfg = self.common_cfg();
        for (i, &b) in data.iter().enumerate() {
            if let Some(byte) = cfg.get_mut(offset + i) {
                *byte = b;
            }
        }
        for (field, size) in COMMON_WRITABLE {
            if overlaps(offset, data.len(), field, size) {
                let mut value = [0; 4];
                value[..size].copy_from_slice(&cfg[field..field + size]);
                self.write_common_field(field, u32::from_le_bytes(value));
            }
        }
    }

    fn write_common_field(&mut self, field: usize, value: u32) {
        let index = usize::from(self.queue_select);
        // Queue fields change only before the queue is enabled.
        let unready = self.queues.get(index).is_some_and(|q| !q.ready());
        match field {
            0 => self.device_feature_select = value,
            8 => self.driver_feature_select = value,
            12 if self.status & STATUS_FEATURES_OK == 0 => match self.driver_feature_select {
                0 => {
                    self.driver_features = (self.driver_features & !0xffff_ffff) | u64::from(value)
                }
                1 => {
                    self.driver_features =
                        (self.driver_features & 0xffff_ffff) | u64::from(value) << 32
                }
                _ => {}
            },
            16 => self.config_vector = self.valid_vector(value as u16),
            20 => self.set_status(value as u8),
            22 => self.queue_select = value as u16,
            24 if unready => {
                // An invalid size (not a power of two, too big) is ignored.
                self.queues[index].try_set_size(value as u16);
            }
            26 if index < self.queues.len() => {
                self.queue_vectors[index] = self.valid_vector(value as u16)
            }
            28 if unready && value == 1 => self.queues[index].set_ready(true),
            32 if unready => self.queues[index].set_desc_table_address(Some(value), None),
            36 if unready => self.queues[index].set_desc_table_address(None, Some(value)),
            40 if unready => self.queues[index].set_avail_ring_address(Some(value), None),
            44 if unready => self.queues[index].set_avail_ring_address(None, Some(value)),
            48 if unready => self.queues[index].set_used_ring_address(Some(value), None),
            52 if unready => self.queues[index].set_used_ring_address(None, Some(value)),
            _ => {}
        }
    }

    fn set_status(&mut self, status: u8) {
        if status == 0 {
            self.reset();
            return;
        }
        let mut status = status;
        if status & STATUS_FEATURES_OK != 0 && self.status & STATUS_FEATURES_OK == 0 {
            // Refuse features not offered, and a driver that is not modern:
            // FEATURES_OK reads back clear (virtio 1.2, section 3.1.1).
            let offered = self.driver_features & !self.device_features == 0;
            let modern = self.driver_features & 1 << VIRTIO_F_VERSION_1 != 0;
            if !(offered && modern) {
                status &= !STATUS_FEATURES_OK;
            }
        }
        // Once until a reset: a driver that clears DRIVER_OK without one and sets it again does not
        // activate the device again over the running one (board 13067's review: the extent store
        // device's workers would have served its queues twice, from where it started).
        let newly_ok = status & STATUS_DRIVER_OK != 0 && !self.activated;
        if newly_ok && !self.activate() {
            status |= STATUS_DEVICE_NEEDS_RESET;
        }
        self.status = status;
    }

    /// DRIVER_OK: checks the enabled queues and starts the device.
    fn activate(&mut self) -> bool {
        if self.status & STATUS_FEATURES_OK == 0 {
            return false;
        }
        let event_idx = self.driver_features & 1 << VIRTIO_RING_F_EVENT_IDX != 0;
        for q in &mut self.queues {
            if q.ready() {
                if !q.is_valid(&self.mem) {
                    return false;
                }
                q.set_event_idx(event_idx);
            }
        }
        if !self.start_device() {
            return false;
        }
        self.activated = true;
        // Buffers the driver made available before DRIVER_OK.
        for i in 0..self.queues.len() {
            self.process_queue(i);
        }
        true
    }

    /// Activates the device with the features the driver accepted: one that serves its own
    /// queues is handed them ([`OwnQueues`]), and may refuse.
    fn start_device(&mut self) -> bool {
        if !self.device.serves_its_queues() {
            self.device.activate(self.driver_features);
            return true;
        }
        let vector = |v: u16| match v {
            NO_VECTOR => Ok(None),
            v => self.msix.vector_event(v).transpose(),
        };
        let vectors = self.queue_vectors.iter().map(|&v| vector(v)).collect();
        let (queue_vectors, config_vector) = match (vectors, vector(self.config_vector)) {
            (Ok(queues), Ok(config)) => (queues, config),
            // No eventfd to interrupt with: the activation fails as the device's would.
            _ => return false,
        };
        let own = OwnQueues {
            queues: &self.queues,
            mem: &self.mem,
            notify: &self.notify,
            queue_vectors,
            config_vector,
        };
        self.device.activate_queues(self.driver_features, own).is_ok()
    }

    fn reset(&mut self) {
        self.status = 0;
        self.device_feature_select = 0;
        self.driver_feature_select = 0;
        self.driver_features = 0;
        self.config_vector = NO_VECTOR;
        self.queue_select = 0;
        for q in &mut self.queues {
            q.reset();
        }
        self.queue_vectors.fill(NO_VECTOR);
        self.isr = 0;
        self.activated = false;
        self.device.reset();
    }

    fn bar0_read(&mut self, offset: u64, data: &mut [u8]) {
        data.fill(0);
        let len = data.len() as u64;
        let within = |r: Range<u64>| offset >= r.start && offset + len <= r.end;
        let device_cfg = DEVICE_CFG_OFFSET..DEVICE_CFG_OFFSET + self.device.config_size() as u64;
        let table = MSIX_TABLE_OFFSET..MSIX_TABLE_OFFSET + self.msix.table_len() as u64;
        let pba = MSIX_PBA_OFFSET..MSIX_PBA_OFFSET + self.msix.pba_len() as u64;
        if within(COMMON_CFG) {
            self.common_read((offset - COMMON_CFG.start) as usize, data);
        } else if offset == ISR.start {
            // Reading the ISR clears it.
            data[0] = std::mem::take(&mut self.isr);
        } else if within(device_cfg) {
            self.device.read_config((offset - DEVICE_CFG_OFFSET) as usize, data);
        } else if within(table) {
            self.msix.table_read((offset - MSIX_TABLE_OFFSET) as usize, data);
        } else if within(pba) {
            self.msix.pba_read((offset - MSIX_PBA_OFFSET) as usize, data);
        }
    }

    fn bar0_write(&mut self, offset: u64, data: &[u8], host: &dyn PciHost) -> Result<()> {
        let len = data.len() as u64;
        let within = |r: Range<u64>| offset >= r.start && offset + len <= r.end;
        let device_cfg = DEVICE_CFG_OFFSET..DEVICE_CFG_OFFSET + self.device.config_size() as u64;
        let stride = u64::from(NOTIFY_OFF_MULTIPLIER);
        let notify = NOTIFY_OFFSET..NOTIFY_OFFSET + self.queues.len() as u64 * stride;
        let table = MSIX_TABLE_OFFSET..MSIX_TABLE_OFFSET + self.msix.table_len() as u64;
        if within(COMMON_CFG) {
            self.common_write((offset - COMMON_CFG.start) as usize, data);
        } else if within(device_cfg) {
            self.device.write_config((offset - DEVICE_CFG_OFFSET) as usize, data);
        } else if within(notify) {
            // A notification no ioeventfd caught: hand it to the device
            // thread the same way.
            self.stats.notify_exits += 1;
            let q = ((offset - NOTIFY_OFFSET) / stride) as usize;
            let _ = self.notify[q].write(1);
        } else if within(table) {
            self.msix.table_write((offset - MSIX_TABLE_OFFSET) as usize, data, host)?;
        }
        Ok(())
    }

    /// Readies the device for a snapshot of the machine, as `mode` says: the threads of one that
    /// serves its own queues stopped, its queues where they left them, then what it takes of its
    /// own ([`VirtioDevice::take_snapshot`]). [`snapshot_done`](Self::snapshot_done) follows,
    /// whether or not this succeeded.
    pub(crate) fn prepare_snapshot(
        &mut self,
        mode: virtio_extent::SnapshotMode,
    ) -> Result<Option<virtio_extent::persist::State>> {
        if !self.released && self.pause_queues() {
            self.paused = true;
        }
        let taken = self.device.take_snapshot(mode);
        self.took_snapshot = matches!(taken, Ok(Some(_)));
        taken
    }

    /// After a snapshot of the machine, `kept` or failed: what the device took for it dropped if
    /// it failed, and its own queue threads started again where they stopped.
    pub(crate) fn snapshot_done(&mut self, kept: bool) {
        // Only what this snapshot took: an earlier one's, kept, is its file's.
        if std::mem::take(&mut self.took_snapshot) && !kept {
            self.device.drop_snapshot();
        }
        if std::mem::take(&mut self.paused) {
            self.restart_device();
        }
    }

    /// Pauses the device's own queue threads, if it runs any, its queues back in the transport:
    /// whether it did.
    fn pause_queues(&mut self) -> bool {
        if !(self.activated && self.device.serves_its_queues()) || self.paused {
            return false;
        }
        for (index, queue) in self.device.pause_queues() {
            self.queues[index] = queue;
        }
        true
    }

    /// Starts the device on its queues again where they stopped, as an activation that fails if
    /// it can't: the device says why, and stops the machine if it can't go on (the extent store
    /// device does).
    fn restart_device(&mut self) {
        if !self.start_device() {
            self.status |= STATUS_DEVICE_NEEDS_RESET;
        }
    }

    /// Has the device let go of what it serves the guest from, its queues paused meanwhile, the
    /// machine paused: an extent store device's cell, for a live migration's hand-over
    /// (`virtio_extent::Device::release_cell`). Refused, the device going on, if it can't.
    pub(crate) fn release_cell(&mut self) -> Result<()> {
        if self.released {
            return Ok(());
        }
        let paused = self.pause_queues();
        match self.device.release_cell() {
            Ok(()) => {
                self.released = true;
                Ok(())
            }
            Err(e) => {
                if paused {
                    self.restart_device();
                }
                Err(e)
            }
        }
    }

    /// Takes back what [`release_cell`](Self::release_cell) let go of, its queues started again:
    /// a live migration aborted. Refused, the device still released, if it can't.
    pub(crate) fn reopen_cell(&mut self) -> Result<()> {
        if !self.released {
            return Ok(());
        }
        self.device.reopen_cell()?;
        self.released = false;
        if self.activated && self.device.serves_its_queues() {
            self.restart_device();
        }
        Ok(())
    }

    /// The function's state for a snapshot, its device's included: the
    /// configuration space, MSI-X, the common configuration and the
    /// queues. Taken with the device thread held off (the caller holds
    /// this transport's lock).
    pub(crate) fn save(&self, e: &mut Encoder) -> Result<()> {
        let device = self.device.save()?;
        self.encode(&device, e);
        Ok(())
    }

    fn encode(&self, device: &DeviceState, e: &mut Encoder) {
        e.u16(self.device.device_type());
        e.bytes(self.config.regs());
        self.msix.save(e);
        e.u32(self.device_feature_select);
        e.u32(self.driver_feature_select);
        e.u64(self.driver_features);
        e.u8(self.status);
        e.u16(self.config_vector);
        e.u16(self.queue_select);
        e.u8(self.isr);
        e.bool(self.activated);
        e.u8(self.config_generation);
        e.count(self.queues.len());
        for (queue, &vector) in self.queues.iter().zip(&self.queue_vectors) {
            queue.save(e);
            e.u16(vector);
        }
        e.u32(device.version);
        e.bytes(&device.data);
    }

    /// Puts back what [`save`](Self::save) wrote into this transport, built
    /// as the one saved was, and redoes what the hypervisor holds for it:
    /// the queue ioeventfds and shared memory at the guest's BARs, the
    /// MSI-X routes and irqfds. An active device is activated again with
    /// the features the driver had, then given its own state.
    pub(crate) fn restore(&mut self, d: &mut Decoder<'_>, host: &dyn PciHost) -> Result<()> {
        let device_type = d.u16()?;
        if device_type != self.device.device_type() {
            return Err(Error::Snapshot(format!(
                "the snapshot has virtio device type {device_type} where this machine has {}",
                self.device.device_type()
            )));
        }
        self.config.restore_regs(d.bytes()?)?;
        self.msix.restore(d, host)?;
        self.device_feature_select = d.u32()?;
        self.driver_feature_select = d.u32()?;
        self.driver_features = d.u64()?;
        self.status = d.u8()?;
        self.config_vector = d.u16()?;
        self.queue_select = d.u16()?;
        self.isr = d.u8()?;
        self.activated = d.bool()?;
        self.config_generation = d.u8()?;
        let n = d.count(1)?;
        if n != self.queues.len() {
            return Err(Error::Snapshot(format!(
                "the snapshot's virtio device type {device_type} has {n} queues, this one {}",
                self.queues.len()
            )));
        }
        for i in 0..n {
            self.queues[i].restore(d)?;
            self.queue_vectors[i] = d.u16()?;
        }
        let state = DeviceState { version: d.u32()?, data: d.bytes()?.to_vec() };
        if self.driver_features & !self.device_features != 0 {
            return Err(Error::Snapshot(format!(
                "the snapshot's driver of virtio device type {device_type} accepted features \
                 {:#x} this device does not offer",
                self.driver_features & !self.device_features
            )));
        }
        if self.activated && !self.start_device() {
            return Err(Error::Snapshot(format!(
                "virtio device type {device_type} refused its activation as it was restored"
            )));
        }
        self.device.restore(&state)?;
        self.sync_bars(host)
    }

    /// Interrupts the driver on every queue of an active device: after a
    /// restore, so that an interrupt that was on its way when the snapshot
    /// was taken is not lost (a spare one only makes the driver look).
    pub(crate) fn signal_all_queues(&mut self) {
        if !self.activated {
            return;
        }
        for index in 0..self.queues.len() {
            if self.queues[index].ready() {
                self.signal_used(index);
            }
        }
    }

    /// The VIRTIO_PCI_CAP_PCI_CFG window: its BAR, offset and length.
    fn pci_cfg_window(&self) -> (u8, u64, usize) {
        let cap = self.pci_cfg_cap;
        let mut bar = [0];
        self.config.read(cap + 4, &mut bar);
        let offset = u64::from(self.config.u32_at(cap + 8));
        (bar[0], offset, self.config.u32_at(cap + 12) as usize)
    }
}

impl PciFunction for VirtioPci {
    fn config_read(&mut self, offset: usize, data: &mut [u8]) {
        if offset == self.pci_cfg_cap + PCI_CFG_DATA {
            let (bar, bar_offset, len) = self.pci_cfg_window();
            data.fill(0);
            if bar == CAPABILITY_BAR && matches!(len, 1 | 2 | 4) && len <= data.len() {
                self.bar0_read(bar_offset, &mut data[..len]);
            }
            return;
        }
        self.config.read(offset, data);
    }

    fn config_write(&mut self, offset: usize, data: &[u8], host: &dyn PciHost) -> Result<()> {
        if offset == self.pci_cfg_cap + PCI_CFG_DATA {
            let (bar, bar_offset, len) = self.pci_cfg_window();
            if bar == CAPABILITY_BAR && matches!(len, 1 | 2 | 4) && len <= data.len() {
                self.bar0_write(bar_offset, &data[..len], host)?;
            }
            return Ok(());
        }
        self.config.write(offset, data);
        let control = self.msix_cap + msix::CONTROL_OFFSET;
        if overlaps(offset, data.len(), control, 2) {
            self.msix.set_control(self.config.u16_at(control), host)?;
        }
        self.sync_bars(host)
    }

    fn trapped_bars(&self) -> Vec<(usize, u64, u64)> {
        let bar = usize::from(CAPABILITY_BAR);
        self.config.decoded_bars()[bar]
            .map(|addr| (bar, addr, CAPABILITY_BAR_SIZE))
            .into_iter()
            .collect()
    }

    fn bar_read(&mut self, bar: usize, offset: u64, data: &mut [u8]) {
        if bar == usize::from(CAPABILITY_BAR) {
            self.bar0_read(offset, data);
        } else {
            data.fill(0);
        }
    }

    fn bar_write(
        &mut self,
        bar: usize,
        offset: u64,
        data: &[u8],
        host: &dyn PciHost,
    ) -> Result<()> {
        if bar == usize::from(CAPABILITY_BAR) {
            self.bar0_write(offset, data, host)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use vm_memory::{Bytes, GuestAddress, MmapRegion};

    use super::*;
    use crate::arch::layout::{PCI_MMIO32_ADDR, PCI_MMIO64_ADDR};
    use crate::devices::pci::MsiMessage;
    use crate::devices::pci::config;
    use crate::devices::pci::tests::{FakeHost, HostCall};
    use crate::devices::virtio::rng::Rng;
    use crate::devices::virtio::test_device::{TEST_CONFIG, TEST_DEVICE_TYPE, TestDevice};

    const STATUS_ACKNOWLEDGE: u8 = 1;
    const STATUS_DRIVER: u8 = 2;
    const ALL_OK: u8 = STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK;
    const FEATURES_OK: u8 = STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK;

    fn memory() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 1 << 20)]).unwrap()
    }

    fn test_device(host: &FakeHost) -> VirtioPci {
        let shm = Arc::new(MmapRegion::new(2 << 20).unwrap());
        let device = Box::new(TestDevice::new(shm));
        VirtioPci::new(device, memory(), &mut BarAllocator::new(), 40, host).unwrap()
    }

    fn rng(host: &FakeHost) -> VirtioPci {
        VirtioPci::new(Box::new(Rng), memory(), &mut BarAllocator::new(), 24, host).unwrap()
    }

    /// The capabilities as a driver walks them: (offset, bytes).
    fn capabilities(t: &mut VirtioPci) -> Vec<(usize, Vec<u8>)> {
        let mut caps = Vec::new();
        let mut ptr = [0];
        t.config_read(config::CAPABILITIES_POINTER, &mut ptr);
        let mut off = usize::from(ptr[0]);
        while off != 0 {
            // Capabilities sit at 0x40 or later, 4 bytes at least: a list
            // longer than 48 has a cycle, which would otherwise loop forever.
            assert!(caps.len() < 48, "the capability list has a cycle: {caps:x?}");
            let mut head = [0; 3];
            t.config_read(off, &mut head);
            let len = if head[0] == msix::CAPABILITY_ID {
                msix::CAPABILITY_LEN
            } else {
                usize::from(head[2])
            };
            let mut body = vec![0; len];
            t.config_read(off, &mut body);
            caps.push((off, body));
            off = usize::from(head[1]);
        }
        caps
    }

    fn u32_at(b: &[u8], off: usize) -> u32 {
        u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
    }

    fn virtio_caps(t: &mut VirtioPci, cfg_type: u8) -> Vec<Vec<u8>> {
        capabilities(t)
            .into_iter()
            .map(|(_, b)| b)
            .filter(|b| b[0] == VIRTIO_PCI_CAP_VENDOR && b[3] == cfg_type)
            .collect()
    }

    fn one_cap(t: &mut VirtioPci, cfg_type: u8) -> Vec<u8> {
        let found = virtio_caps(t, cfg_type);
        assert_eq!(found.len(), 1, "cfg_type {cfg_type}");
        found.into_iter().next().unwrap()
    }

    fn write(t: &mut VirtioPci, host: &FakeHost, offset: u64, data: &[u8]) {
        t.bar_write(0, offset, data, host).unwrap();
    }

    fn read32(t: &mut VirtioPci, offset: u64) -> u32 {
        let mut b = [0; 4];
        t.bar_read(0, offset, &mut b);
        u32::from_le_bytes(b)
    }

    fn read16(t: &mut VirtioPci, offset: u64) -> u16 {
        let mut b = [0; 2];
        t.bar_read(0, offset, &mut b);
        u16::from_le_bytes(b)
    }

    fn status(t: &mut VirtioPci) -> u8 {
        let mut b = [0];
        t.bar_read(0, 20, &mut b);
        b[0]
    }

    #[test]
    fn device_ids_modern_and_past_63() {
        assert_eq!(pci_ids(1), (0x1041, 1, 1));
        assert_eq!(pci_ids(4), (0x1044, 4, 1));
        assert_eq!(pci_ids(63), (0x107f, 63, 1));
        assert_eq!(pci_ids(64), (0x103f, 64, 0));
        assert_eq!(pci_ids(0x4658), (0x103f, 0x4658, 0), "cladefs's extent device");
        assert_eq!(pci_ids(0xffff), (0x103f, 0xffff, 0));
    }

    #[test]
    fn header_says_what_the_device_is() {
        let host = FakeHost::default();
        let t = test_device(&host);
        let c = &t.config;
        assert_eq!(c.u16_at(config::VENDOR_ID), 0x1af4);
        assert_eq!(c.u16_at(config::DEVICE_ID), 0x103f);
        assert_eq!(c.u16_at(config::SUBSYSTEM_VENDOR_ID), 0x1af4);
        assert_eq!(c.u16_at(config::SUBSYSTEM_ID), TEST_DEVICE_TYPE);
        let r = rng(&host);
        assert_eq!(r.config.u16_at(config::DEVICE_ID), 0x1044);
        assert_eq!(r.config.u32_at(config::REVISION_ID), 0xff00_0001, "class ff, revision 1");
    }

    #[test]
    fn capabilities_describe_bar0_and_the_shared_memory() {
        let host = FakeHost::default();
        let mut t = test_device(&host);
        let caps = capabilities(&mut t);
        let (last_off, last) = caps.last().unwrap();
        assert!(last_off + last.len() <= 256);

        let common = one_cap(&mut t, CAP_COMMON_CFG);
        assert_eq!((common[4], u32_at(&common, 8), u32_at(&common, 12)), (0, 0, 64));
        let isr = one_cap(&mut t, CAP_ISR_CFG);
        assert_eq!((isr[4], u32_at(&isr, 8), u32_at(&isr, 12)), (0, 0x1000, 1));
        let device = one_cap(&mut t, CAP_DEVICE_CFG);
        let device_len = TEST_CONFIG.len() as u32;
        assert_eq!((device[4], u32_at(&device, 8), u32_at(&device, 12)), (0, 0x2000, device_len));
        let notify = one_cap(&mut t, CAP_NOTIFY_CFG);
        assert_eq!(notify[2], 20, "cap_len with notify_off_multiplier");
        assert_eq!((notify[4], u32_at(&notify, 8), u32_at(&notify, 12)), (0, 0x3000, 8));
        assert_eq!(u32_at(&notify, 16), NOTIFY_OFF_MULTIPLIER);
        let pci_cfg = one_cap(&mut t, CAP_PCI_CFG);
        assert_eq!(pci_cfg[2], 20);

        let shm = one_cap(&mut t, CAP_SHARED_MEMORY_CFG);
        assert_eq!(shm[2], 24, "virtio_pci_cap64");
        assert_eq!(shm[4], SHM_BAR);
        assert_eq!(shm[5], 0, "region ID");
        let offset = u64::from(u32_at(&shm, 8)) | u64::from(u32_at(&shm, 16)) << 32;
        let length = u64::from(u32_at(&shm, 12)) | u64::from(u32_at(&shm, 20)) << 32;
        assert_eq!((offset, length), (0, 2 << 20));

        let msix = caps.iter().find(|(_, b)| b[0] == msix::CAPABILITY_ID).unwrap().1.clone();
        assert_eq!(u16::from_le_bytes([msix[2], msix[3]]), 2, "config and two queue vectors");
        assert_eq!(u32_at(&msix, 4), MSIX_TABLE_OFFSET as u32);
        assert_eq!(u32_at(&msix, 8), MSIX_PBA_OFFSET as u32);

        // BAR 0 in the 32-bit window; BAR 2 (64-bit, prefetchable) in the
        // 64-bit one, sized to cover the region.
        let c = &t.config;
        assert_eq!(c.bar_addr(0), PCI_MMIO32_ADDR);
        assert_eq!(c.bar_addr(2), PCI_MMIO64_ADDR);
        assert_eq!(c.u32_at(config::BAR0 + 8) & 0xf, 0b1100);
        assert_eq!(c.bar(2).unwrap().size, 2 << 20);
    }

    #[test]
    fn a_device_without_shared_memory_or_config_has_neither_capability() {
        let host = FakeHost::default();
        let mut r = rng(&host);
        assert!(r.config.bar(2).is_none());
        assert!(virtio_caps(&mut r, CAP_SHARED_MEMORY_CFG).is_empty());
        assert!(virtio_caps(&mut r, CAP_DEVICE_CFG).is_empty(), "virtio-rng has no configuration");
        assert_eq!(host.calls(), vec![HostCall::Ioeventfd(PCI_MMIO32_ADDR + NOTIFY_OFFSET)]);
    }

    #[test]
    fn shm_layout_aligns_regions_and_covers_them() {
        assert_eq!(shm_layout(&[]), (vec![], 0x1000));
        assert_eq!(shm_layout(&[2 << 20]), (vec![0], 2 << 20));
        assert_eq!(shm_layout(&[3 << 20]), (vec![0], 4 << 20));
        assert_eq!(shm_layout(&[0x1000, 2 << 20]), (vec![0, 2 << 20], 4 << 20));
        assert_eq!(shm_layout(&[64 << 30, 0x1000]), (vec![0, 64 << 30], 128 << 30));
        // Huge regions align to 1 GiB, not to their own size.
        assert_eq!(shm_layout(&[0x1000, 3 << 30]).0, vec![0, 1 << 30]);
    }

    #[test]
    fn mappings_follow_the_bars() {
        let host = FakeHost::default();
        let mut t = test_device(&host);
        let notify = PCI_MMIO32_ADDR + NOTIFY_OFFSET;
        assert_eq!(
            host.take(),
            vec![
                HostCall::Ioeventfd(notify),
                HostCall::Ioeventfd(notify + 4),
                HostCall::Map(1, PCI_MMIO64_ADDR, 2 << 20),
            ],
            "live from the start, as firmware leaves them"
        );

        // The guest turns decode off to size the BARs: everything goes.
        t.config_write(config::COMMAND, &0u16.to_le_bytes(), &host).unwrap();
        assert_eq!(
            host.take(),
            vec![
                HostCall::NoIoeventfd(notify),
                HostCall::NoIoeventfd(notify + 4),
                HostCall::Unmap(1)
            ]
        );
        assert!(t.trapped_bars().is_empty());
        for reg in [config::BAR0, config::BAR0 + 8, config::BAR0 + 12] {
            t.config_write(reg, &(!0u32).to_le_bytes(), &host).unwrap();
        }
        assert!(host.take().is_empty(), "nothing maps while decode is off");

        // It moves both and turns decode back on.
        let bar0 = PCI_MMIO32_ADDR + 0x10_0000;
        let bar2 = PCI_MMIO64_ADDR + (1 << 30);
        t.config_write(config::BAR0, &(bar0 as u32).to_le_bytes(), &host).unwrap();
        t.config_write(config::BAR0 + 8, &(bar2 as u32).to_le_bytes(), &host).unwrap();
        t.config_write(config::BAR0 + 12, &((bar2 >> 32) as u32).to_le_bytes(), &host).unwrap();
        let memory = config::COMMAND_MEMORY.to_le_bytes();
        t.config_write(config::COMMAND, &memory, &host).unwrap();
        assert_eq!(
            host.take(),
            vec![
                HostCall::Ioeventfd(bar0 + NOTIFY_OFFSET),
                HostCall::Ioeventfd(bar0 + NOTIFY_OFFSET + 4),
                HostCall::Map(2, bar2, 2 << 20),
            ]
        );
        assert_eq!(t.trapped_bars(), vec![(0, bar0, CAPABILITY_BAR_SIZE)]);

        // A BAR moved outside its window is off.
        t.config_write(config::BAR0 + 12, &0u32.to_le_bytes(), &host).unwrap();
        assert_eq!(host.take(), vec![HostCall::Unmap(2)]);
    }

    #[test]
    fn feature_negotiation_and_queue_setup() {
        let host = FakeHost::default();
        let mut t = test_device(&host);
        // Device features: EVENT_IDX (bit 29) and VERSION_1 (bit 32).
        write(&mut t, &host, 0, &0u32.to_le_bytes());
        assert_eq!(read32(&mut t, 4), 1 << VIRTIO_RING_F_EVENT_IDX);
        write(&mut t, &host, 0, &1u32.to_le_bytes());
        assert_eq!(read32(&mut t, 4), 1);
        assert_eq!(read16(&mut t, 18), 2, "num_queues");

        // A driver that does not accept VERSION_1 is refused.
        write(&mut t, &host, 20, &[FEATURES_OK]);
        assert_eq!(status(&mut t) & STATUS_FEATURES_OK, 0);
        // So is one accepting a feature never offered.
        write(&mut t, &host, 8, &0u32.to_le_bytes());
        write(&mut t, &host, 12, &(1u32 << 5).to_le_bytes());
        write(&mut t, &host, 8, &1u32.to_le_bytes());
        write(&mut t, &host, 12, &1u32.to_le_bytes());
        write(&mut t, &host, 20, &[FEATURES_OK]);
        assert_eq!(status(&mut t) & STATUS_FEATURES_OK, 0);
        write(&mut t, &host, 8, &0u32.to_le_bytes());
        write(&mut t, &host, 12, &0u32.to_le_bytes());
        write(&mut t, &host, 20, &[FEATURES_OK]);
        assert_eq!(status(&mut t), FEATURES_OK);

        // Queue 1: size, vector, addresses (a 64-bit write, then halves).
        write(&mut t, &host, 22, &1u16.to_le_bytes());
        assert_eq!(read16(&mut t, 24), 16, "max size");
        write(&mut t, &host, 24, &3u16.to_le_bytes());
        assert_eq!(read16(&mut t, 24), 16, "not a power of two: ignored");
        write(&mut t, &host, 24, &8u16.to_le_bytes());
        assert_eq!(read16(&mut t, 24), 8);
        assert_eq!(read16(&mut t, 30), 1, "queue_notify_off");
        write(&mut t, &host, 26, &7u16.to_le_bytes());
        assert_eq!(read16(&mut t, 26), NO_VECTOR, "no vector 7: refused");
        write(&mut t, &host, 26, &2u16.to_le_bytes());
        assert_eq!(read16(&mut t, 26), 2);
        write(&mut t, &host, 32, &0x1_0000_2000u64.to_le_bytes());
        assert_eq!(read32(&mut t, 32), 0x2000);
        assert_eq!(read32(&mut t, 36), 1);
        write(&mut t, &host, 32, &0x3000u32.to_le_bytes());
        write(&mut t, &host, 36, &0u32.to_le_bytes());
        write(&mut t, &host, 40, &0x4000u32.to_le_bytes());
        write(&mut t, &host, 48, &0x5000u32.to_le_bytes());
        write(&mut t, &host, 28, &1u16.to_le_bytes());
        assert_eq!(read16(&mut t, 28), 1);
        // Enabled: the queue's fields are fixed now.
        write(&mut t, &host, 24, &4u16.to_le_bytes());
        assert_eq!(read16(&mut t, 24), 8);
        // A queue past the last reads as absent.
        write(&mut t, &host, 22, &5u16.to_le_bytes());
        assert_eq!(read16(&mut t, 24), 0);

        write(&mut t, &host, 20, &[ALL_OK]);
        assert_eq!(status(&mut t), ALL_OK);
        assert!(t.activated);

        // Reset: back to the start.
        write(&mut t, &host, 20, &[0]);
        assert_eq!(status(&mut t), 0);
        assert!(!t.activated);
        write(&mut t, &host, 22, &1u16.to_le_bytes());
        assert_eq!(read16(&mut t, 24), 16);
        assert_eq!(read16(&mut t, 26), NO_VECTOR);
        assert_eq!(read16(&mut t, 28), 0);
    }

    /// A driver that clears DRIVER_OK without a reset and sets it again activates the device
    /// once: activated over itself, the extent store device's workers would serve its queues twice,
    /// the second from where they began (board 13067's review). After a reset, again.
    #[test]
    fn driver_ok_set_again_without_a_reset_activates_once() {
        let host = FakeHost::default();
        let shm = Arc::new(MmapRegion::new(2 << 20).unwrap());
        let device = TestDevice::new(shm);
        let activations = device.activations.clone();
        let mut t = VirtioPci::new(Box::new(device), memory(), &mut BarAllocator::new(), 40, &host)
            .unwrap();
        let negotiate = |t: &mut VirtioPci| {
            write(t, &host, 8, &1u32.to_le_bytes());
            write(t, &host, 12, &1u32.to_le_bytes());
            write(t, &host, 20, &[FEATURES_OK]);
        };
        negotiate(&mut t);
        write(&mut t, &host, 20, &[ALL_OK]);
        assert_eq!(activations.load(std::sync::atomic::Ordering::Relaxed), 1);
        write(&mut t, &host, 20, &[FEATURES_OK]);
        write(&mut t, &host, 20, &[ALL_OK]);
        assert_eq!(status(&mut t), ALL_OK);
        assert_eq!(activations.load(std::sync::atomic::Ordering::Relaxed), 1);
        write(&mut t, &host, 20, &[0]);
        negotiate(&mut t);
        write(&mut t, &host, 20, &[ALL_OK]);
        assert_eq!(activations.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    #[test]
    fn driver_ok_with_a_broken_queue_needs_reset() {
        let host = FakeHost::default();
        let mut t = test_device(&host);
        write(&mut t, &host, 8, &1u32.to_le_bytes());
        write(&mut t, &host, 12, &1u32.to_le_bytes());
        write(&mut t, &host, 20, &[FEATURES_OK]);
        // A queue whose rings lie outside guest memory.
        write(&mut t, &host, 32, &(1u64 << 40).to_le_bytes());
        write(&mut t, &host, 28, &1u16.to_le_bytes());
        write(&mut t, &host, 20, &[ALL_OK]);
        assert_ne!(status(&mut t) & STATUS_DEVICE_NEEDS_RESET, 0);
        assert!(!t.activated);
    }

    #[test]
    fn device_config_reads_and_writes_through_bar0_and_the_pci_cfg_window() {
        let host = FakeHost::default();
        let mut t = test_device(&host);
        let mut b = [0; 16];
        t.bar_read(0, DEVICE_CFG_OFFSET, &mut b);
        assert_eq!(&b, TEST_CONFIG);
        t.bar_write(0, DEVICE_CFG_OFFSET + 12, b"WXYZ", &host).unwrap();
        t.bar_read(0, DEVICE_CFG_OFFSET + 12, &mut b[..4]);
        assert_eq!(&b[..4], b"WXYZ");

        // The same through VIRTIO_PCI_CAP_PCI_CFG.
        let cap = t.pci_cfg_cap;
        t.config_write(cap + 4, &[0], &host).unwrap();
        t.config_write(cap + 8, &(DEVICE_CFG_OFFSET as u32).to_le_bytes(), &host).unwrap();
        t.config_write(cap + 12, &4u32.to_le_bytes(), &host).unwrap();
        let mut d = [0; 4];
        t.config_read(cap + PCI_CFG_DATA, &mut d);
        assert_eq!(&d, &TEST_CONFIG[..4]);
        t.config_write(cap + PCI_CFG_DATA, b"abcd", &host).unwrap();
        t.bar_read(0, DEVICE_CFG_OFFSET, &mut b[..4]);
        assert_eq!(&b[..4], b"abcd");
        // And the common configuration's num_queues.
        t.config_write(cap + 8, &18u32.to_le_bytes(), &host).unwrap();
        t.config_write(cap + 12, &2u32.to_le_bytes(), &host).unwrap();
        t.config_read(cap + PCI_CFG_DATA, &mut d);
        assert_eq!(u16::from_le_bytes([d[0], d[1]]), 2);
    }

    #[test]
    fn msix_table_through_bar0_routes_vectors() {
        let host = FakeHost::default();
        let mut t = test_device(&host);
        host.take();
        // Enable MSI-X, then program and unmask vector 1.
        let control = t.msix_cap + msix::CONTROL_OFFSET;
        t.config_write(control, &(1u16 << 15).to_le_bytes(), &host).unwrap();
        let entry = MSIX_TABLE_OFFSET + 16;
        write(&mut t, &host, entry, &0xfee0_0000u32.to_le_bytes());
        write(&mut t, &host, entry + 8, &0x4021u32.to_le_bytes());
        write(&mut t, &host, entry + 12, &0u32.to_le_bytes());
        let message = MsiMessage { address: 0xfee0_0000, data: 0x4021 };
        assert_eq!(host.take(), vec![HostCall::Route(41, message), HostCall::Assign(41)]);
        assert_eq!(read32(&mut t, entry + 8), 0x4021);
        // The PBA reads, and is empty.
        let mut pba = [0xff; 8];
        t.bar_read(0, MSIX_PBA_OFFSET, &mut pba);
        assert_eq!(pba, [0; 8]);
    }

    /// Lays out a split queue in guest memory with one writable buffer of
    /// `len` bytes available: (desc, avail, used, buffer) addresses.
    fn one_buffer(mem: &GuestMemoryMmap, len: u32) -> (u64, u64, u64, u64) {
        let (desc, avail, used, buf) = (0x1000u64, 0x2000u64, 0x3000u64, 0x8000u64);
        let mut d = Vec::new();
        d.extend_from_slice(&buf.to_le_bytes());
        d.extend_from_slice(&len.to_le_bytes());
        d.extend_from_slice(&2u16.to_le_bytes()); // VIRTQ_DESC_F_WRITE
        d.extend_from_slice(&0u16.to_le_bytes());
        mem.write_slice(&d, GuestAddress(desc)).unwrap();
        // avail: flags 0, idx 1, ring[0] = descriptor 0.
        mem.write_slice(&[0, 0, 1, 0, 0, 0], GuestAddress(avail)).unwrap();
        (desc, avail, used, buf)
    }

    #[test]
    fn rng_fills_a_buffer_and_interrupts_its_queue_vector() {
        let host = FakeHost::default();
        let mut t = rng(&host);
        let mem = t.mem.clone();
        let (desc, avail, used, buf) = one_buffer(&mem, 64);

        // MSI-X on, vectors 0 (config) and 1 (the queue) unmasked.
        let control = t.msix_cap + msix::CONTROL_OFFSET;
        t.config_write(control, &(1u16 << 15).to_le_bytes(), &host).unwrap();
        for v in 0..2u64 {
            let entry = MSIX_TABLE_OFFSET + 16 * v;
            write(&mut t, &host, entry, &0xfee0_0000u32.to_le_bytes());
            write(&mut t, &host, entry + 8, &(0x30 + v as u32).to_le_bytes());
            write(&mut t, &host, entry + 12, &0u32.to_le_bytes());
        }

        write(&mut t, &host, 8, &1u32.to_le_bytes());
        write(&mut t, &host, 12, &1u32.to_le_bytes()); // VERSION_1
        write(&mut t, &host, 20, &[FEATURES_OK]);
        write(&mut t, &host, 16, &0u16.to_le_bytes());
        write(&mut t, &host, 22, &0u16.to_le_bytes());
        write(&mut t, &host, 24, &4u16.to_le_bytes());
        write(&mut t, &host, 26, &1u16.to_le_bytes());
        write(&mut t, &host, 32, &desc.to_le_bytes());
        write(&mut t, &host, 40, &avail.to_le_bytes());
        write(&mut t, &host, 48, &used.to_le_bytes());
        write(&mut t, &host, 28, &1u16.to_le_bytes());
        // DRIVER_OK takes the buffer made available before it.
        write(&mut t, &host, 20, &[ALL_OK]);
        assert_eq!(status(&mut t), ALL_OK);

        let mut used_ring = [0u8; 12];
        mem.read_slice(&mut used_ring, GuestAddress(used)).unwrap();
        assert_eq!(u16::from_le_bytes([used_ring[2], used_ring[3]]), 1, "used idx");
        assert_eq!(u32_at(&used_ring, 4), 0, "descriptor 0");
        assert_eq!(u32_at(&used_ring, 8), 64, "all 64 bytes written");
        let mut random = [0u8; 64];
        mem.read_slice(&mut random, GuestAddress(buf)).unwrap();
        assert_ne!(random, [0; 64]);

        assert_eq!(t.stats().interrupts, 1);
        assert_eq!(t.msix.irqfd(1).read().unwrap(), 1, "the queue's vector");
        assert!(t.msix.irqfd(0).read().is_err(), "not the config vector");

        // A notification that exits (no ioeventfd caught it) reaches the
        // device thread's eventfd.
        write(&mut t, &host, NOTIFY_OFFSET, &0u16.to_le_bytes());
        assert_eq!(t.stats().notify_exits, 1);
        assert_eq!(t.notify[0].read().unwrap(), 1);
    }
}
