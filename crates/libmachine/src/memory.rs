// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The guest physical memory map: RAM and the address windows reserved for
//! devices.
//!
//! [`MemoryLayout`] is a plain value describing it, which is what a snapshot
//! records: a restore rebuilds RAM at the same addresses.

/// What a range of guest physical addresses is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RegionKind {
    /// Guest RAM.
    Ram,
    /// The 32-bit MMIO window below 4 GiB, where 32-bit BARs go.
    Mmio32,
    /// Fixed platform devices (PCI config space, interrupt controllers, the
    /// hypervisor's own pages). Nothing is mapped here.
    Platform,
    /// The 64-bit MMIO window, where 64-bit BARs go (e.g. a DAX window).
    Mmio64,
    /// Persistent memory devices' (virtio-pmem) host files, mapped for the guest.
    Pmem,
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
}

/// A machine's guest physical memory map, sorted by address.
///
/// The RAM region and MMIO windows are fixed for a given RAM size
/// (`arch::aarch64::layout`).
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
