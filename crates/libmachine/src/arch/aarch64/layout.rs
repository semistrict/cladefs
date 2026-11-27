// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The arm64 machine's guest physical address map, fixed for every machine whatever its RAM size,
//! so that a snapshot's device addresses stay where they were (board 13063).
//!
//! ```text
//! 0x00_0800_0000  GIC distributor (64 KiB)
//! 0x00_0801_0000  GIC MSI frame (GICv2m's, 64 KiB): where devices' MSIs go
//! 0x00_0802_0000  UART: a 16550, its registers a byte apart
//! 0x00_0803_0000  RTC: an ARM PL031
//! 0x00_0900_0000  GIC redistributors, 128 KiB a vCPU: 32 MiB, for the 256 vCPUs the framework
//!                 places them for whatever a VM has
//! 0x00_1000_0000  PCI ECAM: room for buses 0-255 (256 MiB), bus 0 alone described
//! 0x00_4000_0000  32-bit BAR window, 512 MiB
//! 0x00_8000_0000  RAM (2 GiB up)
//! 0x80_0000_0000  64-bit BAR window (512 GiB up), 256 GiB: the extent device's DAX window
//! 0xC0_0000_0000  persistent memory (virtio-pmem), 256 GiB: host files mapped for the guest
//! ```
//!
//! The map fits a 40-bit IPA space, which Apple's hosts give with 4 KiB stage 2 pages
//! (`kern.hv.ipa_size_4k`) as well as 16 KiB ones, so that a DAX window can map 4 KiB pages.

use crate::memory::{MemoryLayout, MemoryRegion, RegionKind};

/// The bits of guest physical address the map needs.
pub(crate) const IPA_BITS: u32 = 40;

/// Every block of the platform area is 64 KiB aligned, as the framework asks of the GIC's.
pub(crate) const GIC_DIST_ADDR: u64 = 0x0800_0000;
/// The GIC's MSI frame, as GICv2m's: an MSI is a write of its SPI to its SETSPI_NS register.
pub(crate) const GIC_MSI_ADDR: u64 = 0x0801_0000;
/// The 16550's registers.
pub(crate) const UART_ADDR: u64 = 0x0802_0000;
pub(crate) const UART_SIZE: u64 = 0x1000;
/// The real-time clock, a PL031: one page of 32-bit registers.
pub(crate) const RTC_ADDR: u64 = 0x0803_0000;
pub(crate) const RTC_SIZE: u64 = 0x1000;
pub(crate) const GIC_REDIST_ADDR: u64 = 0x0900_0000;
/// Each vCPU's redistributor: its two 64 KiB frames (RD_base and SGI_base).
pub(crate) const GIC_REDIST_SIZE: u64 = 0x2_0000;
/// Room for the redistributors of every vCPU the framework supports.
pub(crate) const GIC_REDIST_REGION_SIZE: u64 = 0x200_0000;

/// The most vCPUs whose redistributors fit.
pub(crate) const MAX_VCPUS: u32 = (GIC_REDIST_REGION_SIZE / GIC_REDIST_SIZE) as u32;

/// The interrupts devices raise, as GIC SPIs (INTID 32 up; the device tree names an SPI by its
/// number from 0). The UART's is the first; MSIs take the range after the platform's.
pub(crate) const SPI_BASE: u32 = 32;
pub(crate) const UART_SPI: u32 = SPI_BASE;
/// The RTC's alarm interrupt, described but never raised.
pub(crate) const RTC_SPI: u32 = SPI_BASE + 1;
/// The SPIs the MSI frame raises: devices' MSI-X vectors.
pub(crate) const MSI_SPI_BASE: u32 = SPI_BASE + 32;
pub(crate) const MSI_SPI_COUNT: u32 = 256;

/// The architected timers' PPIs, by INTID, which the GIC Hypervisor.framework makes wires to
/// the vCPUs' timers (`HV_GIC_INT_EL1_VIRTUAL_TIMER` is 27).
pub(crate) const TIMER_PPI_SECURE_PHYS: u32 = 29;
pub(crate) const TIMER_PPI_PHYS: u32 = 30;
pub(crate) const TIMER_PPI_VIRT: u32 = 27;
pub(crate) const TIMER_PPI_HYP: u32 = 26;

/// PCI Express enhanced configuration access (ECAM) for segment 0: 1 MiB per bus, 4 KiB per
/// function. The window has room for buses 0-255 and is reserved whole, as the PC's is, but the
/// board has bus 0 only, and its device tree says so (`bus-range`, and `reg` its 1 MiB).
pub(crate) const PCI_ECAM_ADDR: u64 = 0x1000_0000;
pub(crate) const PCI_ECAM_SIZE: u64 = 0x1000_0000;
pub(crate) const PCI_ECAM_LAST_BUS: u8 = 0;
/// The window 32-bit memory BARs are allocated from.
pub(crate) const PCI_MMIO32_ADDR: u64 = 0x4000_0000;
pub(crate) const PCI_MMIO32_SIZE: u64 = 0x2000_0000;
/// The window 64-bit memory BARs (shared memory regions, such as cladefs's DAX window) are
/// allocated from.
pub(crate) const PCI_MMIO64_ADDR: u64 = 0x80_0000_0000;
pub(crate) const PCI_MMIO64_SIZE: u64 = 0x40_0000_0000;
/// Where virtio-pmem devices' memory goes, past the 64-bit window, as Firecracker places it: no
/// BAR of the PCI host's, which the guest would reassign.
pub(crate) const PMEM_ADDR: u64 = PCI_MMIO64_ADDR + PCI_MMIO64_SIZE;
pub(crate) const PMEM_SIZE: u64 = 0x40_0000_0000;
/// The first GSI given to MSI-X vectors: a GSI here is only the key of an MSI route, which the
/// HVF backend keeps itself.
pub(crate) const PCI_MSI_GSI_BASE: u32 = 0;

/// Where RAM starts.
pub(crate) const RAM_ADDR: u64 = 0x8000_0000;
/// The most RAM the map holds below the 64-bit window.
pub(crate) const MAX_RAM: u64 = PCI_MMIO64_ADDR - RAM_ADDR;

const _: () = {
    assert!(GIC_MSI_ADDR >= GIC_DIST_ADDR + 0x1_0000);
    assert!(UART_ADDR >= GIC_MSI_ADDR + 0x1_0000);
    assert!(RTC_ADDR >= UART_ADDR + 0x1_0000);
    assert!(GIC_REDIST_ADDR >= RTC_ADDR + 0x1_0000);
    assert!(GIC_REDIST_ADDR + GIC_REDIST_REGION_SIZE <= PCI_ECAM_ADDR);
    // A gap between them: an address past the ECAM is nobody's.
    assert!(PCI_ECAM_ADDR + PCI_ECAM_SIZE < PCI_MMIO32_ADDR);
    assert!(PCI_MMIO32_ADDR + PCI_MMIO32_SIZE <= RAM_ADDR);
    assert!(PCI_MMIO64_ADDR + PCI_MMIO64_SIZE <= PMEM_ADDR);
    assert!(PMEM_ADDR + PMEM_SIZE <= 1 << IPA_BITS);
    assert!(PCI_ECAM_SIZE >= (PCI_ECAM_LAST_BUS as u64 + 1) << 20);
};

/// Guest RAM of `mem_size` bytes (at most [`MAX_RAM`]) as (start, size) regions: one, from
/// [`RAM_ADDR`].
pub(crate) fn ram_regions(mem_size: u64) -> Vec<(u64, u64)> {
    debug_assert!(mem_size <= MAX_RAM, "mem_size {mem_size:#x} above MAX_RAM");
    vec![(RAM_ADDR, mem_size)]
}

/// The machine's fixed memory map for `mem_size` bytes of RAM, by address: the platform's
/// devices, the BAR windows, and RAM.
pub(crate) fn memory_layout(mem_size: u64) -> MemoryLayout {
    let mut regions = vec![
        MemoryRegion::new(
            GIC_DIST_ADDR,
            PCI_ECAM_ADDR + PCI_ECAM_SIZE - GIC_DIST_ADDR,
            RegionKind::Platform,
        ),
        MemoryRegion::new(PCI_MMIO32_ADDR, PCI_MMIO32_SIZE, RegionKind::Mmio32),
        MemoryRegion::new(PCI_MMIO64_ADDR, PCI_MMIO64_SIZE, RegionKind::Mmio64),
        MemoryRegion::new(PMEM_ADDR, PMEM_SIZE, RegionKind::Pmem),
    ];
    regions.extend(
        ram_regions(mem_size)
            .into_iter()
            .map(|(start, size)| MemoryRegion::new(start, size, RegionKind::Ram)),
    );
    regions.sort_by_key(|r| r.start);
    MemoryLayout { regions }
}
