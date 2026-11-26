// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Guest physical memory and I/O port layout of the PC platform.
//!
//! The map is fixed for every machine, whatever its RAM, so that a snapshot's
//! device windows keep their addresses (decided 2026-11-14, doc-31):
//!
//! ```text
//! 0x0000_0000_0000  RAM, up to 3 GiB
//! 0x0000_c000_0000  32-bit MMIO hole: 32-bit BARs, PCI config space (ECAM),
//!                   the I/O APIC, the local APIC, KVM's pages
//! 0x0001_0000_0000  RAM beyond 3 GiB continues at 4 GiB
//! 0x0100_0000_0000  64-bit MMIO window (1 TiB): 64-bit BARs, e.g. a DAX window
//! 0x0200_0000_0000  end of the map (41 physical address bits)
//! ```
//!
//! RAM can therefore be at most 1 TiB - 1 GiB ([`MAX_RAM`]).
//!
//! The first MiB and the 32-bit hole:
//!
//! ```text
//! 0x0000_0500  GDT
//! 0x0000_6000  PVH start_info, module list, memory map
//! 0x0000_7000  Linux boot_params ("zero page")
//! 0x0000_8ff0  boot stack (grows down)
//! 0x0000_9000  page tables: PML4, PDPT, 4 PDs identity-mapping 4 GiB
//! 0x0002_0000  kernel command line
//! 0x0009_fc00  reserved: EBDA, ACPI tables at 0xe0000, the VM generation
//!              ID at 0xff000, BIOS area
//! 0x0010_0000  kernel, then RAM; the initrd at the top of RAM below 3 GiB
//! 0xc000_0000  32-bit MMIO window: 32-bit BARs (device memory may go here)
//! 0xe000_0000  platform devices: PCI ECAM, I/O APIC at 0xfec0_0000, local
//!              APIC at 0xfee0_0000, KVM's TSS and identity-map pages
//! ```

use crate::memory::{MemoryLayout, MemoryRegion, RegionKind};

pub(crate) const GDT_ADDR: u64 = 0x500;
pub(crate) const PVH_START_INFO_ADDR: u64 = 0x6000;
pub(crate) const PVH_MODLIST_ADDR: u64 = 0x6040;
pub(crate) const PVH_MEMMAP_ADDR: u64 = 0x6100;
pub(crate) const ZERO_PAGE_ADDR: u64 = 0x7000;
pub(crate) const BOOT_STACK_ADDR: u64 = 0x8ff0;
pub(crate) const PML4_ADDR: u64 = 0x9000;
pub(crate) const PDPT_ADDR: u64 = 0xa000;
/// Four page directories of 2 MiB pages: the first 4 GiB.
pub(crate) const PD_ADDR: u64 = 0xb000;
pub(crate) const IDENTITY_MAPPED_GIB: u64 = 4;
pub(crate) const CMDLINE_ADDR: u64 = 0x20000;
pub(crate) const CMDLINE_MAX: usize = 0x10000;
/// Start of the reserved area below 1 MiB (the EBDA, then the BIOS area).
pub(crate) const EBDA_ADDR: u64 = 0x9fc00;
/// ACPI tables, in the BIOS area where the RSDP may also be searched for.
pub(crate) const ACPI_ADDR: u64 = 0xe0000;
pub(crate) const ACPI_END: u64 = 0x100000;
pub(crate) const HIMEM_ADDR: u64 = 0x100000;
/// RAM below 4 GiB ends here, where the 32-bit MMIO hole starts.
pub(crate) const MMIO_HOLE_START: u64 = 0xc000_0000;
/// The 32-bit MMIO window, where 32-bit BARs go: the hole's first 512 MiB.
pub(crate) const MMIO32_START: u64 = MMIO_HOLE_START;
pub(crate) const MMIO32_SIZE: u64 = 0x2000_0000;
/// Platform devices fill the rest of the hole up to 4 GiB: PCI config space,
/// the interrupt controllers and KVM's pages. No device memory goes there.
pub(crate) const PLATFORM_START: u64 = MMIO32_START + MMIO32_SIZE;
pub(crate) const FOUR_GIB: u64 = 1 << 32;
/// RAM beyond what fits below the hole continues here.
pub(crate) const HIGH_RAM_START: u64 = FOUR_GIB;
/// The 64-bit MMIO window, where 64-bit BARs go (cladefs's DAX window, up to
/// 64 GiB, among them): 1 TiB at 1 TiB.
pub(crate) const MMIO64_START: u64 = 1 << 40;
pub(crate) const MMIO64_SIZE: u64 = 1 << 40;
/// The most RAM a machine can have: 3 GiB below the hole, and from 4 GiB up
/// to the 64-bit MMIO window.
pub(crate) const MAX_RAM: u64 = MMIO_HOLE_START + (MMIO64_START - HIGH_RAM_START);
pub(crate) const IOAPIC_ADDR: u32 = 0xfec0_0000;
pub(crate) const LAPIC_ADDR: u32 = 0xfee0_0000;
/// Pages KVM needs for its own use on Intel (real-mode emulation), placed
/// in the MMIO hole, outside RAM.
pub(crate) const KVM_TSS_ADDR: u64 = 0xfffb_d000;
pub(crate) const KVM_IDENTITY_MAP_ADDR: u64 = 0xfffb_c000;

/// Guest RAM of `mem_size` bytes (at most [`MAX_RAM`]) as (start, size)
/// regions: up to 3 GiB from 0, and the rest from 4 GiB.
pub(crate) fn ram_regions(mem_size: u64) -> Vec<(u64, u64)> {
    debug_assert!(mem_size <= MAX_RAM, "mem_size {mem_size:#x} above MAX_RAM");
    let low = mem_size.min(MMIO_HOLE_START);
    let mut regions = vec![(0, low)];
    if mem_size > low {
        regions.push((HIGH_RAM_START, mem_size - low));
    }
    regions
}

/// Where RAM below 4 GiB ends: what 32-bit boot code can reach.
pub(crate) fn low_ram_end(mem_size: u64) -> u64 {
    mem_size.min(MMIO_HOLE_START)
}

/// The machine's fixed memory map for `mem_size` bytes of RAM, by address:
/// RAM and the windows reserved for devices.
pub(crate) fn memory_layout(mem_size: u64) -> MemoryLayout {
    let mut regions: Vec<MemoryRegion> = ram_regions(mem_size)
        .into_iter()
        .map(|(start, size)| MemoryRegion::new(start, size, RegionKind::Ram))
        .collect();
    regions.extend([
        MemoryRegion::new(MMIO32_START, MMIO32_SIZE, RegionKind::Mmio32),
        MemoryRegion::new(PLATFORM_START, FOUR_GIB - PLATFORM_START, RegionKind::Platform),
        MemoryRegion::new(MMIO64_START, MMIO64_SIZE, RegionKind::Mmio64),
    ]);
    regions.sort_by_key(|r| r.start);
    MemoryLayout { regions }
}

/// COM1: a 16550 UART on IRQ 4.
pub(crate) const COM1_IRQ: u32 = 4;
/// The VM generation ID's 16 bytes (VMGENID), in the reserved BIOS area
/// after the ACPI tables: a fresh random ID at boot and at each restore of a
/// snapshot, which the guest mixes into its RNG.
pub(crate) const VMGENID_ADDR: u64 = 0xf_f000;
/// The ACPI Generic Event Device's interrupt, raised after a restore to say
/// that the generation ID changed: an I/O APIC pin past the ISA IRQs, so
/// that it never reaches the legacy PIC, which routes GSIs 0-15 too and,
/// in a guest that has not masked it yet (one snapshotted before it
/// booted), would deliver it with a vector nobody set.
pub(crate) const GED_IRQ: u32 = 16;
/// ACPI hardware-reduced sleep control and status registers, and the reset
/// register, which the FADT points at.
pub(crate) const ACPI_SLEEP_CONTROL_PORT: u16 = 0x600;
pub(crate) const ACPI_SLEEP_STATUS_PORT: u16 = 0x601;
pub(crate) const ACPI_RESET_PORT: u16 = 0x602;
pub(crate) const ACPI_RESET_VALUE: u8 = 1;
/// The SLP_TYP value the DSDT's `_S5_` (soft off) names.
pub(crate) const ACPI_S5_SLP_TYP: u8 = 5;

// ---------------------------------------------------------------------------
// PCI (board 13111), in the windows of the address map above. Fixed whatever the
// RAM size, so a snapshot's BARs keep their addresses.
//
// 0x0000_0000_c000_0000 (3 GiB)    32-bit BAR window, 512 MiB
// 0x0000_0000_e000_0000 (3.5 GiB)  ECAM: room for buses 0-255, 256 MiB
// 0x0000_0100_0000_0000 (1 TiB)    64-bit BAR window, 1 TiB
// ---------------------------------------------------------------------------

/// The window 32-bit memory BARs are allocated from.
pub(crate) const PCI_MMIO32_ADDR: u64 = MMIO32_START;
pub(crate) const PCI_MMIO32_SIZE: u64 = MMIO32_SIZE;
/// PCI Express enhanced configuration access (ECAM): 1 MiB per bus, 4 KiB
/// per function, for segment 0. The window has room for buses 0-255 and is
/// reserved whole, but the platform has bus 0 only, and says so (MCFG,
/// `_CRS`): x86 Linux probes every bus up to the MCFG's last one for peer
/// root bridges, a config read per device, which cost 8160 exits and ~60 ms
/// of boot with buses 0-255 described (dead-ends/ecam-all-256-buses.md).
pub(crate) const PCI_ECAM_ADDR: u64 = PLATFORM_START;
pub(crate) const PCI_ECAM_SIZE: u64 = 0x1000_0000;
pub(crate) const PCI_ECAM_LAST_BUS: u8 = 0;
/// The window 64-bit memory BARs (shared memory regions, such as cladefs's DAX
/// window) are allocated from.
pub(crate) const PCI_MMIO64_ADDR: u64 = MMIO64_START;
pub(crate) const PCI_MMIO64_SIZE: u64 = MMIO64_SIZE;
/// The first GSI given to MSI-X vectors, past the I/O APIC's 24 pins.
pub(crate) const PCI_MSI_GSI_BASE: u32 = 24;

// The ECAM is the platform area's first part, clear of the APICs and KVM's
// pages; the windows are the address map's.
const _: () = {
    assert!(PCI_ECAM_ADDR + PCI_ECAM_SIZE <= IOAPIC_ADDR as u64);
    assert!(PCI_MMIO32_ADDR == 0xc000_0000 && PCI_ECAM_ADDR == 0xe000_0000);
    assert!(PCI_MMIO64_ADDR == 0x100_0000_0000);
};

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;
    const PAGE: u64 = 4096;

    #[test]
    fn ram_up_to_3_gib_is_one_region_at_0() {
        assert_eq!(ram_regions(256 << 20), [(0, 256 << 20)]);
        assert_eq!(ram_regions(3 * GIB - PAGE), [(0, 3 * GIB - PAGE)]);
        assert_eq!(ram_regions(3 * GIB), [(0, 3 * GIB)]);
    }

    #[test]
    fn ram_past_3_gib_continues_at_4_gib() {
        assert_eq!(ram_regions(3 * GIB + PAGE), [(0, 3 * GIB), (4 * GIB, PAGE)]);
        assert_eq!(ram_regions(4 * GIB), [(0, 3 * GIB), (4 * GIB, GIB)]);
        assert_eq!(ram_regions(6 * GIB), [(0, 3 * GIB), (4 * GIB, 3 * GIB)]);
        assert_eq!(low_ram_end(6 * GIB), 3 * GIB);
        assert_eq!(low_ram_end(GIB), GIB);
    }

    #[test]
    fn the_most_ram_ends_where_the_64_bit_window_starts() {
        assert_eq!(MAX_RAM, (1 << 40) - GIB);
        let regions = ram_regions(MAX_RAM);
        let (start, size) = regions[1];
        assert_eq!(start + size, MMIO64_START);
    }

    #[test]
    fn the_layout_is_sorted_and_its_regions_do_not_overlap() {
        for mem_size in [16 << 20, 3 * GIB, 3 * GIB + PAGE, 6 * GIB, MAX_RAM] {
            let layout = memory_layout(mem_size);
            for pair in layout.regions.windows(2) {
                assert!(pair[0].end() <= pair[1].start, "{mem_size:#x}: {pair:?}");
            }
            assert_eq!(layout.ram_size(), mem_size);
        }
    }

    #[test]
    fn the_windows_are_where_the_address_map_says() {
        let layout = memory_layout(16 << 20);
        let window = |kind| {
            let r = layout.regions.iter().find(|r| r.kind == kind).unwrap();
            (r.start, r.end())
        };
        assert_eq!(window(RegionKind::Mmio32), (0xc000_0000, 0xe000_0000));
        assert_eq!(window(RegionKind::Platform), (0xe000_0000, 0x1_0000_0000));
        assert_eq!(window(RegionKind::Mmio64), (0x100_0000_0000, 0x200_0000_0000));
        // The interrupt controllers and KVM's pages are in the platform part.
        for addr in
            [u64::from(IOAPIC_ADDR), u64::from(LAPIC_ADDR), KVM_TSS_ADDR, KVM_IDENTITY_MAP_ADDR]
        {
            assert!((PLATFORM_START..FOUR_GIB).contains(&addr), "{addr:#x}");
        }
        // A 64 GiB DAX window fits the 64-bit window many times over.
        const { assert!(MMIO64_SIZE >= 64 * GIB) };
    }
}
