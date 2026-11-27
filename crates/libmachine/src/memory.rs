// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The guest physical memory map: RAM, the address windows reserved for
//! devices, and the device memory mapped into them at runtime.
//!
//! [`MemoryLayout`] is a plain value describing it, which is what a snapshot
//! records: a restore rebuilds RAM at the same addresses and maps each
//! device's memory again where it was. [`SlotTable`] keeps the hypervisor's
//! memory slots and checks what may be mapped where; it does not depend on
//! a backend.

use std::collections::BTreeMap;

use crate::error::{Error, Result};

const PAGE: u64 = 4096;

/// What a range of guest physical addresses is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RegionKind {
    /// Guest RAM.
    Ram,
    /// The 32-bit MMIO window below 4 GiB, where 32-bit BARs go. Device
    /// memory may be mapped here.
    Mmio32,
    /// Fixed platform devices (PCI config space, interrupt controllers, the
    /// hypervisor's own pages). Nothing is mapped here.
    Platform,
    /// The 64-bit MMIO window, where 64-bit BARs go (e.g. a DAX window).
    /// Device memory may be mapped here.
    Mmio64,
    /// Host memory a device has mapped into the guest at runtime, inside an
    /// MMIO window.
    DeviceMemory,
    /// Persistent memory devices' (virtio-pmem) host files, mapped for the guest (the arm64
    /// board's).
    Pmem,
}

impl RegionKind {
    /// Whether device memory may be mapped in a region of this kind.
    pub fn holds_device_memory(self) -> bool {
        matches!(self, RegionKind::Mmio32 | RegionKind::Mmio64)
    }
}

/// A range of guest physical addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MemoryRegion {
    /// The first guest physical address.
    pub start: u64,
    /// The length in bytes.
    pub size: u64,
    /// What the range is.
    pub kind: RegionKind,
}

impl MemoryRegion {
    /// A region of `size` bytes at `start`.
    pub fn new(start: u64, size: u64, kind: RegionKind) -> Self {
        Self { start, size, kind }
    }

    /// The first address past the region.
    pub fn end(&self) -> u64 {
        self.start + self.size
    }

    /// Whether `addr` is in the region.
    pub fn contains(&self, addr: u64) -> bool {
        (self.start..self.end()).contains(&addr)
    }

    fn overlaps(&self, start: u64, end: u64) -> bool {
        self.start < end && start < self.end()
    }
}

/// A machine's guest physical memory map, sorted by address.
///
/// The RAM regions and MMIO windows are fixed for a given RAM size (on
/// x86_64: RAM up to 3 GiB, the 32-bit MMIO hole to 4 GiB, RAM beyond 3 GiB
/// from 4 GiB, and a 1 TiB 64-bit window at 1 TiB). Device memory regions
/// lie inside a window and are listed right after it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MemoryLayout {
    /// Every region, by start address.
    pub regions: Vec<MemoryRegion>,
}

impl MemoryLayout {
    /// The regions of `kind`, by address.
    pub fn of_kind(&self, kind: RegionKind) -> impl Iterator<Item = &MemoryRegion> {
        self.regions.iter().filter(move |r| r.kind == kind)
    }

    /// Total RAM in bytes.
    pub fn ram_size(&self) -> u64 {
        self.of_kind(RegionKind::Ram).map(|r| r.size).sum()
    }
}

/// The hypervisor's memory slots, and the checks on what may be mapped
/// where: RAM takes the first slots; device memory must be page-aligned, lie
/// inside an MMIO window, and overlap no other device memory. Freed slot
/// numbers are reused (lowest first), so a machine can map and unmap
/// memory forever within the hypervisor's slot limit.
#[derive(Debug)]
pub(crate) struct SlotTable {
    /// RAM and the windows, by address.
    fixed: Vec<MemoryRegion>,
    /// Every slot in use, by number: RAM's and device memory.
    slots: BTreeMap<u32, MemoryRegion>,
    max_slots: u32,
}

impl SlotTable {
    /// A table for `layout` (without device memory) whose hypervisor has
    /// `max_slots` slots. RAM regions take slots 0, 1, ... in address order.
    pub(crate) fn new(layout: &MemoryLayout, max_slots: u32) -> Result<Self> {
        let fixed: Vec<MemoryRegion> =
            layout.regions.iter().filter(|r| r.kind != RegionKind::DeviceMemory).copied().collect();
        let slots: BTreeMap<u32, MemoryRegion> = fixed
            .iter()
            .filter(|r| r.kind == RegionKind::Ram)
            .copied()
            .zip(0..)
            .map(|(r, slot)| (slot, r))
            .collect();
        if slots.len() as u64 > u64::from(max_slots) {
            return Err(Error::MemoryMap(format!(
                "RAM needs {} memory slots, the hypervisor has {max_slots}",
                slots.len()
            )));
        }
        Ok(Self { fixed, slots, max_slots })
    }

    /// The slot numbers and regions of RAM.
    pub(crate) fn ram_slots(&self) -> impl Iterator<Item = (u32, &MemoryRegion)> {
        self.slots.iter().filter(|(_, r)| r.kind == RegionKind::Ram).map(|(&slot, r)| (slot, r))
    }

    /// Claims a slot for `size` bytes of device memory at `start`, after
    /// checking it may go there, and returns its number.
    pub(crate) fn claim(&mut self, start: u64, size: u64) -> Result<u32> {
        let err =
            |why: String| Err(Error::MemoryMap(format!("{size:#x} bytes at {start:#x}: {why}")));
        if size == 0 || !start.is_multiple_of(PAGE) || !size.is_multiple_of(PAGE) {
            return err(format!(
                "address and size must be multiples of {PAGE} bytes, and the size non-zero"
            ));
        }
        let Some(end) = start.checked_add(size) else {
            return err("past the end of the address space".into());
        };
        let window = self
            .fixed
            .iter()
            .find(|r| r.kind.holds_device_memory() && r.start <= start && end <= r.end());
        if window.is_none() {
            let around: Vec<String> = self
                .fixed
                .iter()
                .filter(|r| r.overlaps(start, end))
                .map(|r| format!("{:?} {:#x}-{:#x}", r.kind, r.start, r.end()))
                .collect();
            return err(format!(
                "device memory must lie inside one MMIO window; this range overlaps [{}]",
                around.join(", ")
            ));
        }
        if let Some(other) = self.slots.values().find(|r| r.overlaps(start, end)) {
            return err(format!("overlaps device memory at {:#x}-{:#x}", other.start, other.end()));
        }
        // The lowest number not in use: the first gap in the sorted keys.
        let slot = self
            .slots
            .keys()
            .zip(0..)
            .find(|&(&used, n)| used != n)
            .map_or(self.slots.len() as u32, |(_, n)| n);
        if slot >= self.max_slots {
            return err(format!(
                "all {} of the hypervisor's memory slots are in use",
                self.max_slots
            ));
        }
        self.slots.insert(slot, MemoryRegion::new(start, size, RegionKind::DeviceMemory));
        Ok(slot)
    }

    /// Frees device memory slot `slot`.
    pub(crate) fn release(&mut self, slot: u32) {
        let freed = self.slots.remove(&slot);
        debug_assert!(
            freed.is_some_and(|r| r.kind == RegionKind::DeviceMemory),
            "released slot {slot}, which is {freed:?}"
        );
    }

    /// The layout: RAM, the windows, and the device memory mapped now.
    pub(crate) fn layout(&self) -> MemoryLayout {
        let mut regions = self.fixed.clone();
        regions.extend(self.slots.values().filter(|r| r.kind == RegionKind::DeviceMemory));
        // Device memory sorts after the window that holds it.
        regions.sort_by_key(|r| (r.start, r.kind == RegionKind::DeviceMemory));
        MemoryLayout { regions }
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;
    use crate::arch::x86_64::layout::{MMIO32_START, MMIO64_START, memory_layout};

    const GIB: u64 = 1 << 30;

    fn table(mem_size: u64) -> SlotTable {
        SlotTable::new(&memory_layout(mem_size), 32).unwrap()
    }

    #[test]
    fn ram_takes_the_first_slots() {
        let t = table(6 * GIB);
        let ram: Vec<(u32, (u64, u64))> =
            t.ram_slots().map(|(slot, r)| (slot, (r.start, r.size))).collect();
        assert_eq!(ram, [(0, (0, 3 * GIB)), (1, (4 * GIB, 3 * GIB))]);
        assert_eq!(table(GIB).ram_slots().count(), 1);
    }

    #[test]
    fn device_memory_goes_in_the_mmio_windows() {
        let mut t = table(GIB);
        assert_eq!(t.claim(MMIO64_START, 64 * GIB).unwrap(), 1);
        assert_eq!(t.claim(MMIO32_START, 2 << 20).unwrap(), 2);
        // The last page of the 64-bit window.
        assert_eq!(t.claim(2 * MMIO64_START - 4096, 4096).unwrap(), 3);
    }

    #[test]
    fn device_memory_elsewhere_is_refused() {
        let mut t = table(6 * GIB);
        for (start, size, why) in [
            (0, 4096, "RAM"),
            (5 * GIB, 4096, "RAM above 4 GiB"),
            (8 * GIB, 4096, "a gap that is no window"),
            (0xfee0_0000, 4096, "the local APIC"),
            (0xe000_0000, 4096, "PCI config space"),
            (0xdfff_f000, 0x2000, "straddles the 32-bit window's end"),
            (MMIO64_START - 4096, 0x2000, "straddles the 64-bit window's start"),
            (2 * MMIO64_START, 4096, "past the 64-bit window"),
            (!4095, 4096, "wraps the address space"),
            (MMIO64_START + 1, 4096, "unaligned address"),
            (MMIO64_START, 4097, "unaligned size"),
            (MMIO64_START, 0, "empty"),
        ] {
            match t.claim(start, size) {
                Err(Error::MemoryMap(msg)) => {
                    assert!(msg.contains(&format!("{start:#x}")), "{msg}")
                }
                other => panic!("{why}: expected a refusal, got {other:?}"),
            }
        }
        assert_eq!(t.layout(), memory_layout(6 * GIB), "nothing was mapped");
    }

    #[test]
    fn device_memory_must_not_overlap() {
        let mut t = table(GIB);
        t.claim(MMIO64_START + GIB, GIB).unwrap();
        for start in [MMIO64_START + GIB, MMIO64_START + GIB / 2, MMIO64_START + GIB + GIB / 2] {
            assert!(matches!(t.claim(start, GIB), Err(Error::MemoryMap(_))), "{start:#x}");
        }
        // Touching is fine.
        t.claim(MMIO64_START, GIB).unwrap();
        t.claim(MMIO64_START + 2 * GIB, GIB).unwrap();
    }

    #[test]
    fn freed_slots_are_reused_lowest_first() {
        let mut t = table(6 * GIB);
        let a = t.claim(MMIO64_START, 4096).unwrap();
        let b = t.claim(MMIO64_START + 4096, 4096).unwrap();
        let c = t.claim(MMIO64_START + 8192, 4096).unwrap();
        assert_eq!((a, b, c), (2, 3, 4));
        t.release(b);
        t.release(a);
        assert_eq!(t.claim(MMIO64_START + GIB, 4096).unwrap(), 2);
        assert_eq!(t.claim(MMIO64_START + 2 * GIB, 4096).unwrap(), 3);
        assert_eq!(t.claim(MMIO64_START + 3 * GIB, 4096).unwrap(), 5);
        // The freed range can be mapped again.
        t.release(c);
        assert_eq!(t.claim(MMIO64_START + 8192, 4096).unwrap(), 4);
    }

    #[test]
    fn running_out_of_slots_is_an_error_and_freeing_one_recovers() {
        let mut t = SlotTable::new(&memory_layout(GIB), 4).unwrap();
        for i in 0..3 {
            t.claim(MMIO64_START + i * GIB, 4096).unwrap();
        }
        match t.claim(MMIO64_START + 3 * GIB, 4096) {
            Err(Error::MemoryMap(msg)) => assert!(msg.contains("slots"), "{msg}"),
            other => panic!("expected running out of slots, got {other:?}"),
        }
        t.release(2);
        assert_eq!(t.claim(MMIO64_START + 3 * GIB, 4096).unwrap(), 2);
        assert!(SlotTable::new(&memory_layout(6 * GIB), 1).is_err());
    }

    #[test]
    fn the_layout_lists_device_memory_after_its_window() {
        let mut t = table(GIB);
        let slot = t.claim(MMIO64_START, 64 * GIB).unwrap();
        let layout = t.layout();
        let kinds: Vec<RegionKind> = layout.regions.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            [
                RegionKind::Ram,
                RegionKind::Mmio32,
                RegionKind::Platform,
                RegionKind::Mmio64,
                RegionKind::DeviceMemory,
            ]
        );
        let dm: Vec<_> = layout.of_kind(RegionKind::DeviceMemory).collect();
        assert_eq!((dm[0].start, dm[0].size), (MMIO64_START, 64 * GIB));
        assert_eq!(layout.ram_size(), GIB);
        t.release(slot);
        assert_eq!(t.layout(), memory_layout(GIB));
    }
}
