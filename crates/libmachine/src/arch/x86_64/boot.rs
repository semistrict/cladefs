// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Loads a Linux kernel and initrd into guest memory and writes what its
//! entry point expects.
//!
//! Two formats, each through its documented entry point:
//! - an ELF `vmlinux` through PVH (`CONFIG_PVH`): uncompressed, entered in
//!   32-bit protected mode with paging off and a `hvm_start_info`;
//! - a `bzImage` through the 64-bit Linux boot protocol: entered in long
//!   mode at `startup_64` with a `boot_params` ("zero page").
//!
//! An ELF without a PVH note is refused rather than entered some other way.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use linux_loader::configurator::linux::LinuxBootConfigurator;
use linux_loader::configurator::pvh::PvhBootConfigurator;
use linux_loader::configurator::{BootConfigurator, BootParams};
use linux_loader::loader::bootparam::boot_params;
use linux_loader::loader::bzimage::BzImage;
use linux_loader::loader::elf::{Elf, PvhBootCapability, start_info};
use linux_loader::loader::{KernelLoader, KernelLoaderResult};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use super::gdt::{self, Segment};
use super::layout::*;
use crate::error::{Error, Result};

/// The kernel command line libmachine's platform needs, before the caller's:
/// the console on COM1, and a panic resets the machine instead of hanging.
pub(crate) const BASE_CMDLINE: &str = "console=ttyS0 panic=-1";

const E820_RAM: u32 = 1;
const E820_RESERVED: u32 = 2;
/// boot_params.hdr.type_of_loader: "undefined" boot loader.
const LOADER_TYPE_UNDEFINED: u8 = 0xff;
const XLF_KERNEL_64: u16 = 1 << 0;
const KERNEL_BOOT_FLAG_MAGIC: u16 = 0xaa55;
const KERNEL_HDR_MAGIC: u32 = 0x5372_6448; // "HdrS"
const XEN_HVM_START_MAGIC: u32 = 0x336e_c578;
const PAGE: u64 = 4096;

/// How the vCPU enters the kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CpuMode {
    /// 32-bit protected mode, paging off (PVH).
    Protected,
    /// 64-bit long mode with the identity-mapped boot page tables.
    Long,
}

/// The vCPU state at the kernel's entry point.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BootCpu {
    pub(crate) mode: CpuMode,
    pub(crate) rip: u64,
    pub(crate) rsi: u64,
    pub(crate) rbx: u64,
    pub(crate) rsp: u64,
    pub(crate) gdt_base: u64,
    pub(crate) gdt_limit: u16,
    pub(crate) code: Segment,
    pub(crate) data: Segment,
    pub(crate) tss: Segment,
}

/// What to boot.
#[derive(Debug, Clone)]
pub(crate) struct Kernel<'a> {
    pub(crate) kernel: &'a Path,
    pub(crate) initrd: Option<&'a Path>,
    pub(crate) cmdline: &'a str,
}

enum Format {
    Elf,
    BzImage,
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |source| Error::Io { path: path.to_owned(), source }
}

fn boot_err(what: impl std::fmt::Display) -> Error {
    Error::Boot(what.to_string())
}

fn detect(path: &Path, file: &mut File) -> Result<Format> {
    let mut head = [0u8; 0x206];
    let n = file.read(&mut head).map_err(io_err(path))?;
    file.seek(SeekFrom::Start(0)).map_err(io_err(path))?;
    if n >= 4 && head[..4] == *b"\x7fELF" {
        Ok(Format::Elf)
    } else if n >= 0x206 && head[0x202..0x206] == *b"HdrS" {
        Ok(Format::BzImage)
    } else {
        Err(boot_err(format!("{}: not an ELF vmlinux or a bzImage", path.display())))
    }
}

/// The guest's physical memory map as (address, size, e820 type): low RAM,
/// the reserved EBDA/BIOS area (which holds the ACPI tables), then each RAM
/// region from 1 MiB on: up to 3 GiB, and from 4 GiB past the MMIO hole.
fn memory_map(mem_size: u64) -> Vec<(u64, u64, u32)> {
    let mut map =
        vec![(0, EBDA_ADDR, E820_RAM), (EBDA_ADDR, HIMEM_ADDR - EBDA_ADDR, E820_RESERVED)];
    for (start, size) in ram_regions(mem_size) {
        let start_above_1m = start.max(HIMEM_ADDR);
        let end = start + size;
        if end > start_above_1m {
            map.push((start_above_1m, end - start_above_1m, E820_RAM));
        }
    }
    map
}

/// Loads `kernel` into `mem` (`mem_size` bytes of RAM, laid out by
/// [`ram_regions`]) with the ACPI RSDP at `rsdp_addr`, and returns the vCPU
/// state that enters it.
pub(crate) fn load(
    mem: &GuestMemoryMmap,
    mem_size: u64,
    kernel: &Kernel<'_>,
    rsdp_addr: u64,
) -> Result<BootCpu> {
    let mut file = File::open(kernel.kernel).map_err(io_err(kernel.kernel))?;
    let format = detect(kernel.kernel, &mut file)?;
    let highmem = Some(GuestAddress(HIMEM_ADDR));
    let loaded: KernelLoaderResult = match format {
        Format::Elf => Elf::load(mem, None, &mut file, highmem),
        Format::BzImage => BzImage::load(mem, None, &mut file, highmem),
    }
    .map_err(|e| boot_err(format!("{}: {e}", kernel.kernel.display())))?;

    let cmdline = if kernel.cmdline.is_empty() {
        BASE_CMDLINE.to_owned()
    } else {
        format!("{BASE_CMDLINE} {}", kernel.cmdline)
    };

    match format {
        Format::Elf => {
            let PvhBootCapability::PvhEntryPresent(entry) = loaded.pvh_boot_cap else {
                return Err(boot_err(format!(
                    "{}: ELF kernel has no PVH entry point; build it with CONFIG_PVH=y, \
                     or boot its bzImage",
                    kernel.kernel.display()
                )));
            };
            let kernel_end = loaded.kernel_load.0 + loaded.kernel_end;
            write_cmdline(mem, &cmdline, CMDLINE_MAX)?;
            let initrd = load_initrd(mem, mem_size, kernel.initrd, kernel_end, u64::MAX)?;
            write_pvh_start_info(mem, mem_size, initrd, rsdp_addr)?;
            write_gdt(mem, gdt::CODE32)?;
            Ok(boot_cpu(CpuMode::Protected, entry.0, gdt::CODE32, 0, PVH_START_INFO_ADDR))
        }
        Format::BzImage => {
            let hdr = loaded.setup_header.ok_or_else(|| boot_err("bzImage has no setup header"))?;
            if hdr.xloadflags & XLF_KERNEL_64 == 0 {
                return Err(boot_err(format!(
                    "{}: bzImage has no 64-bit entry point",
                    kernel.kernel.display()
                )));
            }
            // The kernel decompresses itself in place: it needs init_size
            // bytes from where it was loaded.
            let kernel_end = loaded.kernel_load.0 + u64::from(hdr.init_size);
            write_cmdline(mem, &cmdline, hdr.cmdline_size as usize)?;
            let initrd = load_initrd(
                mem,
                mem_size,
                kernel.initrd,
                kernel_end,
                u64::from(hdr.initrd_addr_max),
            )?;

            let mut params = boot_params { hdr, ..Default::default() };
            params.hdr.type_of_loader = LOADER_TYPE_UNDEFINED;
            params.hdr.boot_flag = KERNEL_BOOT_FLAG_MAGIC;
            params.hdr.header = KERNEL_HDR_MAGIC;
            params.hdr.cmd_line_ptr = CMDLINE_ADDR as u32;
            params.hdr.cmdline_size = cmdline.len() as u32 + 1;
            if let Some((addr, size)) = initrd {
                params.hdr.ramdisk_image = addr as u32;
                params.hdr.ramdisk_size = size as u32;
            }
            params.acpi_rsdp_addr = rsdp_addr;
            for (i, (addr, size, type_)) in memory_map(mem_size).into_iter().enumerate() {
                params.e820_table[i].addr = addr;
                params.e820_table[i].size = size;
                params.e820_table[i].r#type = type_;
                params.e820_entries = i as u8 + 1;
            }
            LinuxBootConfigurator::write_bootparams(
                &BootParams::new(&params, GuestAddress(ZERO_PAGE_ADDR)),
                mem,
            )
            .map_err(boot_err)?;
            write_page_tables(mem)?;
            write_gdt(mem, gdt::CODE64)?;
            // startup_64 is 0x200 bytes into the protected-mode kernel.
            let entry = loaded.kernel_load.0 + 0x200;
            Ok(boot_cpu(CpuMode::Long, entry, gdt::CODE64, ZERO_PAGE_ADDR, 0))
        }
    }
}

fn boot_cpu(mode: CpuMode, rip: u64, code_flags: u16, rsi: u64, rbx: u64) -> BootCpu {
    let entries = gdt_entries(code_flags);
    BootCpu {
        mode,
        rip,
        rsi,
        rbx,
        rsp: BOOT_STACK_ADDR,
        gdt_base: GDT_ADDR,
        gdt_limit: (entries.len() * 8 - 1) as u16,
        code: gdt::segment(1, entries[1]),
        data: gdt::segment(2, entries[2]),
        tss: gdt::segment(3, entries[3]),
    }
}

/// Null, code, flat data, and a TSS (which the CPU requires to be valid
/// even though the kernel loads its own before using it).
fn gdt_entries(code_flags: u16) -> [u64; 4] {
    [
        0,
        gdt::entry(code_flags, 0, 0xfffff),
        gdt::entry(gdt::DATA, 0, 0xfffff),
        gdt::entry(gdt::TSS, 0, 0x67),
    ]
}

fn write_gdt(mem: &GuestMemoryMmap, code_flags: u16) -> Result<()> {
    for (i, entry) in gdt_entries(code_flags).into_iter().enumerate() {
        mem.write_obj(entry, GuestAddress(GDT_ADDR + i as u64 * 8))?;
    }
    Ok(())
}

/// Identity-maps the first 4 GiB with 2 MiB pages, which covers the kernel,
/// its boot data, the initrd and the MMIO hole. (The kernel maps RAM above
/// 4 GiB itself, from the e820 table.)
fn write_page_tables(mem: &GuestMemoryMmap) -> Result<()> {
    const PRESENT_WRITABLE: u64 = 0x3;
    const HUGE: u64 = 0x80;
    mem.write_obj(PDPT_ADDR | PRESENT_WRITABLE, GuestAddress(PML4_ADDR))?;
    for gib in 0..IDENTITY_MAPPED_GIB {
        let pd = PD_ADDR + gib * PAGE;
        mem.write_obj(pd | PRESENT_WRITABLE, GuestAddress(PDPT_ADDR + gib * 8))?;
        for i in 0..512u64 {
            let addr = (gib << 30) | (i << 21);
            mem.write_obj(addr | PRESENT_WRITABLE | HUGE, GuestAddress(pd + i * 8))?;
        }
    }
    Ok(())
}

fn write_cmdline(mem: &GuestMemoryMmap, cmdline: &str, max: usize) -> Result<()> {
    let max = max.min(CMDLINE_MAX);
    if cmdline.len() >= max || cmdline.contains('\0') {
        return Err(boot_err(format!(
            "kernel command line is {} bytes, the kernel takes at most {}",
            cmdline.len(),
            max - 1
        )));
    }
    mem.write_slice(cmdline.as_bytes(), GuestAddress(CMDLINE_ADDR))?;
    mem.write_obj(0u8, GuestAddress(CMDLINE_ADDR + cmdline.len() as u64))?;
    Ok(())
}

/// Loads the initrd page-aligned at the top of RAM below 4 GiB (and at or
/// below `addr_max`), and returns its address and size. Below 4 GiB, the
/// bzImage entry's identity map covers it and PVH's 32-bit entry reaches it.
fn load_initrd(
    mem: &GuestMemoryMmap,
    mem_size: u64,
    initrd: Option<&Path>,
    kernel_end: u64,
    addr_max: u64,
) -> Result<Option<(u64, u64)>> {
    let Some(path) = initrd else {
        return Ok(None);
    };
    let mut file = File::open(path).map_err(io_err(path))?;
    let size = file.metadata().map_err(io_err(path))?.len();
    let top = low_ram_end(mem_size).min(addr_max.saturating_add(1));
    let addr = top.checked_sub(size).map(|a| a & !(PAGE - 1));
    let Some(addr) = addr.filter(|&a| a >= kernel_end) else {
        return Err(boot_err(format!(
            "{}: the initrd ({size} bytes) does not fit in guest RAM above the kernel",
            path.display()
        )));
    };
    mem.read_exact_volatile_from(GuestAddress(addr), &mut file, size as usize)?;
    Ok(Some((addr, size)))
}

fn write_pvh_start_info(
    mem: &GuestMemoryMmap,
    mem_size: u64,
    initrd: Option<(u64, u64)>,
    rsdp_addr: u64,
) -> Result<()> {
    let memmap: Vec<start_info::hvm_memmap_table_entry> = memory_map(mem_size)
        .into_iter()
        .map(|(addr, size, type_)| start_info::hvm_memmap_table_entry {
            addr,
            size,
            type_,
            reserved: 0,
        })
        .collect();
    let info = start_info::hvm_start_info {
        magic: XEN_HVM_START_MAGIC,
        version: 1,
        cmdline_paddr: CMDLINE_ADDR,
        rsdp_paddr: rsdp_addr,
        memmap_paddr: PVH_MEMMAP_ADDR,
        memmap_entries: memmap.len() as u32,
        nr_modules: initrd.is_some() as u32,
        modlist_paddr: if initrd.is_some() { PVH_MODLIST_ADDR } else { 0 },
        ..Default::default()
    };
    let mut params = BootParams::new(&info, GuestAddress(PVH_START_INFO_ADDR));
    params.set_sections(&memmap, GuestAddress(PVH_MEMMAP_ADDR));
    if let Some((paddr, size)) = initrd {
        let module = start_info::hvm_modlist_entry { paddr, size, ..Default::default() };
        params.set_modules(&[module], GuestAddress(PVH_MODLIST_ADDR));
    }
    PvhBootConfigurator::write_bootparams(&params, mem).map_err(boot_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;

    fn ram(map: &[(u64, u64, u32)]) -> Vec<(u64, u64)> {
        map.iter().filter(|e| e.2 == E820_RAM).map(|&(addr, size, _)| (addr, addr + size)).collect()
    }

    #[test]
    fn below_the_hole_ram_is_the_first_640k_and_from_1_mib() {
        let map = memory_map(256 * MIB);
        assert_eq!(ram(&map), [(0, EBDA_ADDR), (HIMEM_ADDR, 256 * MIB)]);
        assert_eq!(map[1], (EBDA_ADDR, HIMEM_ADDR - EBDA_ADDR, E820_RESERVED));
        assert_eq!(ram(&memory_map(3 * GIB)), [(0, EBDA_ADDR), (HIMEM_ADDR, 3 * GIB)]);
    }

    #[test]
    fn ram_past_3_gib_is_listed_from_4_gib() {
        assert_eq!(
            ram(&memory_map(3 * GIB + 4096)),
            [(0, EBDA_ADDR), (HIMEM_ADDR, 3 * GIB), (4 * GIB, 4 * GIB + 4096)]
        );
        assert_eq!(
            ram(&memory_map(6 * GIB)),
            [(0, EBDA_ADDR), (HIMEM_ADDR, 3 * GIB), (4 * GIB, 7 * GIB)]
        );
        assert_eq!(
            ram(&memory_map(MAX_RAM)),
            [(0, EBDA_ADDR), (HIMEM_ADDR, 3 * GIB), (4 * GIB, MMIO64_START)]
        );
    }

    /// The map covers exactly the RAM: nothing in the hole, every byte once.
    #[test]
    fn the_map_lists_all_ram_and_nothing_else() {
        for mem_size in [2 * MIB, 256 * MIB, 3 * GIB, 3 * GIB + 4096, 6 * GIB, MAX_RAM] {
            let map = memory_map(mem_size);
            let listed: u64 = map.iter().map(|e| e.1).sum();
            assert_eq!(listed, mem_size, "{mem_size:#x}");
            for pair in map.windows(2) {
                assert!(pair[0].0 + pair[0].1 <= pair[1].0, "{mem_size:#x}: {pair:?}");
            }
            for &(addr, size, _) in &map {
                assert!(
                    addr + size <= MMIO_HOLE_START || addr >= FOUR_GIB,
                    "{mem_size:#x}: {addr:#x}+{size:#x} reaches into the hole"
                );
            }
            // It fits the zero page's e820 table and the PVH memmap area.
            assert!(map.len() <= 128);
            assert!(PVH_MEMMAP_ADDR + map.len() as u64 * 24 <= ZERO_PAGE_ADDR);
        }
    }
}
