// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Guest RAM around the 32-bit MMIO hole, and device memory mapped into the
//! MMIO windows at runtime, as long-mode guest programs on a bare machine
//! see them.
//!
//! A bare machine starts in real mode; each program here first switches to
//! long mode with page tables that identity-map the 2 MiB pages it uses.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

mod common;

use std::time::Duration;

use common::{asm16, kvm_available, load};
use iced_x86::code_asm::*;
use libmachine::{
    Error, ExitReason, HostMemory, Machine, MachineConfig, MemoryLayout, MemoryRegion, RegionKind,
};

const GIB: u64 = 1 << 30;
const PAGE_2M: u64 = 2 << 20;
/// The 64-bit MMIO window, where device memory such as a DAX window goes.
const MMIO64: u64 = 0x100_0000_0000;
const MMIO32: u64 = 0xc000_0000;

const GDT: u64 = 0x500;
const GDTR: u64 = 0x600;
/// Where the 64-bit part of a program goes.
const LONG_ENTRY: u64 = 0x2000;
/// Page tables, from here up.
const TABLES: u64 = 0x10000;

const MAGIC: u64 = 0x0123_4567_89ab_cdef;
const WRITTEN: u64 = 0xfeed_face_cafe_beef;

/// The table `tables[t][index]` points to, created if missing.
fn child(tables: &mut Vec<[u64; 512]>, t: usize, index: usize) -> usize {
    if tables[t][index] == 0 {
        tables.push([0; 512]);
        let n = tables.len() - 1;
        tables[t][index] = (TABLES + n as u64 * 4096) | 0x3; // present, writable
        n
    } else {
        ((tables[t][index] & !0xfff) - TABLES) as usize / 4096
    }
}

/// Loads `program` (64-bit code) and the real-mode code that enters it in
/// long mode, with page tables identity-mapping the first 2 MiB and the
/// 2 MiB pages holding `addrs`.
fn load_long_mode(
    m: &mut Machine,
    addrs: &[u64],
    program: impl FnOnce(&mut CodeAssembler) -> Result<(), IcedError>,
) {
    // Page tables (data, not code): PML4, then the PDPTs and PDs it needs.
    let mut tables = vec![[0u64; 512]];
    for &addr in [0].iter().chain(addrs) {
        let page = addr & !(PAGE_2M - 1);
        let pdpt = child(&mut tables, 0, (page >> 39) as usize & 511);
        let pd = child(&mut tables, pdpt, (page >> 30) as usize & 511);
        tables[pd][(page >> 21) as usize & 511] = page | 0x83; // present, writable, 2 MiB
    }
    for (i, table) in tables.iter().enumerate() {
        let bytes: Vec<u8> = table.iter().flat_map(|e| e.to_le_bytes()).collect();
        m.load(TABLES + i as u64 * 4096, &bytes).unwrap();
    }
    // GDT: null, 64-bit code, data. GDTR: limit, base.
    let gdt: Vec<u8> = [0u64, 0x00af_9a00_0000_ffff, 0x00cf_9200_0000_ffff]
        .iter()
        .flat_map(|e| e.to_le_bytes())
        .collect();
    m.load(GDT, &gdt).unwrap();
    let mut gdtr = 23u16.to_le_bytes().to_vec();
    gdtr.extend_from_slice(&(GDT as u32).to_le_bytes());
    m.load(GDTR, &gdtr).unwrap();

    let mut a = CodeAssembler::new(64).unwrap();
    program(&mut a).expect("assemble the 64-bit program");
    let code64 = a.assemble(LONG_ENTRY).expect("encode the 64-bit program");
    m.load(LONG_ENTRY, &code64).unwrap();

    let enter = asm16(|a| {
        a.cli()?;
        a.lgdt(ptr(GDTR))?;
        a.mov(eax, cr4)?;
        a.or(eax, 1 << 5)?; // PAE
        a.mov(cr4, eax)?;
        a.mov(eax, TABLES as u32)?;
        a.mov(cr3, eax)?;
        a.mov(ecx, 0xc000_0080u32)?; // EFER
        a.rdmsr()?;
        a.or(eax, 1 << 8)?; // LME
        a.wrmsr()?;
        a.mov(eax, cr0)?;
        a.or(eax, 0x8000_0001u32)?; // PG, PE
        a.mov(cr0, eax)?;
        // Far return into the 64-bit code segment.
        a.push(0x08)?;
        a.push(LONG_ENTRY as i32)?;
        a.retf()
    });
    load(m, &enter);
}

fn ram_regions(layout: &MemoryLayout) -> Vec<(u64, u64)> {
    layout.of_kind(RegionKind::Ram).map(|r| (r.start, r.size)).collect()
}

fn device_memory(layout: &MemoryLayout) -> Vec<(u64, u64)> {
    layout.of_kind(RegionKind::DeviceMemory).map(|r| (r.start, r.size)).collect()
}

/// Resident host memory of this process, in bytes.
fn rss() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
    let pages: u64 = statm.split_whitespace().nth(1).unwrap().parse().unwrap();
    pages * 4096
}

#[test]
fn ram_past_3_gib_is_at_4_gib_and_the_hole_is_not_ram() {
    if !kvm_available() {
        return;
    }
    let config = MachineConfig { mem_size: (4 * GIB) as usize, ..Default::default() };
    let mut m = Machine::new(config).unwrap();
    assert_eq!(ram_regions(&m.memory_layout()), [(0, 3 * GIB), (4 * GIB, GIB)]);
    // The host reaches RAM at 4 GiB, and none in the hole.
    let high = 4 * GIB + 0x1000;
    let last = 5 * GIB - 8;
    m.load(high, &MAGIC.to_le_bytes()).unwrap();
    m.load(last, &(!MAGIC).to_le_bytes()).unwrap();
    assert!(matches!(m.load(MMIO32 + 0x1000, &[1]), Err(Error::GuestMemory(_))));
    assert!(matches!(m.load(5 * GIB, &[1]), Err(Error::GuestMemory(_))));

    load_long_mode(&mut m, &[high, last, MMIO32], |a| {
        a.mov(rbx, high)?;
        a.mov(rax, qword_ptr(rbx))?;
        a.mov(rcx, WRITTEN)?;
        a.mov(qword_ptr(rbx + 8), rcx)?;
        a.mov(rsi, last)?;
        a.mov(rdx, qword_ptr(rsi))?;
        a.hlt()?;
        // The hole: an MMIO access, not RAM.
        a.mov(rdi, MMIO32)?;
        a.mov(rax, qword_ptr(rdi))?;
        a.hlt()
    });
    assert_eq!(m.run().unwrap(), ExitReason::Halt);
    let regs = m.registers().unwrap();
    assert_eq!(regs.rax, MAGIC, "read RAM at 4 GiB");
    assert_eq!(regs.rdx, !MAGIC, "read the last RAM byte, at 5 GiB");
    assert_eq!(m.read_memory(high + 8, 8).unwrap(), WRITTEN.to_le_bytes());
    assert_eq!(m.run().unwrap(), ExitReason::UnhandledMmio { addr: MMIO32, size: 8, write: false });
}

/// A 64 GiB DAX-window-sized mapping costs no host memory until touched;
/// the guest sees it while it is mapped, and MMIO once it is dropped; the
/// same addresses can be mapped again.
#[test]
fn device_memory_comes_and_goes_at_runtime() {
    if !kvm_available() {
        return;
    }
    let mut m = Machine::new(MachineConfig::default()).unwrap();
    let slots = m.memory_slots();
    let before = rss();

    let window = 64 * GIB;
    let last = MMIO64 + window - 8;
    let memory = HostMemory::new(window as usize).unwrap();
    memory.write(0, &MAGIC.to_le_bytes()).unwrap();
    let slot = slots.map(MMIO64, memory).unwrap();
    assert_eq!((slot.guest_addr(), slot.size()), (MMIO64, window));
    assert_eq!(device_memory(&m.memory_layout()), [(MMIO64, window)]);

    load_long_mode(&mut m, &[MMIO64, last], |a| {
        a.mov(rbx, MMIO64)?;
        a.mov(rax, qword_ptr(rbx))?;
        a.mov(rcx, WRITTEN)?;
        a.mov(qword_ptr(rbx + 8), rcx)?;
        a.mov(rsi, last)?;
        a.mov(qword_ptr(rsi), rcx)?;
        a.hlt()?;
        // Once with the slot gone, once with a new one in its place.
        a.mov(rdx, qword_ptr(rbx))?;
        a.hlt()?;
        a.mov(rdx, qword_ptr(rbx))?;
        a.hlt()
    });
    assert_eq!(m.run().unwrap(), ExitReason::Halt);
    assert_eq!(m.registers().unwrap().rax, MAGIC);
    let mut buf = [0u8; 8];
    slot.memory().read(8, &mut buf).unwrap();
    assert_eq!(u64::from_le_bytes(buf), WRITTEN);
    slot.memory().read(window as usize - 8, &mut buf).unwrap();
    assert_eq!(u64::from_le_bytes(buf), WRITTEN, "the window's last qword");
    let grown = rss().saturating_sub(before);
    assert!(
        grown < 64 << 20,
        "a 64 GiB window with three qwords touched grew RSS by {grown} bytes"
    );

    drop(slot);
    assert_eq!(device_memory(&m.memory_layout()), []);
    assert_eq!(m.run().unwrap(), ExitReason::UnhandledMmio { addr: MMIO64, size: 8, write: false });

    let memory = HostMemory::new(PAGE_2M as usize).unwrap();
    memory.write(0, &(!MAGIC).to_le_bytes()).unwrap();
    let _slot = slots.map(MMIO64, memory).unwrap();
    // The MMIO read completes (with all ones on a bare machine), then halts.
    assert_eq!(m.run().unwrap(), ExitReason::Halt);
    assert_eq!(m.registers().unwrap().rdx, u64::MAX);
    assert_eq!(m.run().unwrap(), ExitReason::Halt);
    assert_eq!(m.registers().unwrap().rdx, !MAGIC, "the new slot's memory");
}

/// Another thread maps memory while the vCPU runs: the guest spins on a
/// flag in one slot until the thread has mapped a second, then reads it.
#[test]
fn device_memory_can_be_mapped_while_the_guest_runs() {
    if !kvm_available() {
        return;
    }
    let mut m = Machine::new(MachineConfig::default()).unwrap();
    let slots = m.memory_slots();
    let flag_addr = MMIO32;
    let data_addr = MMIO64 + GIB;
    let flag = slots.map(flag_addr, HostMemory::new(4096).unwrap()).unwrap();

    load_long_mode(&mut m, &[flag_addr, data_addr], |a| {
        let mut spin = a.create_label();
        a.mov(rbx, flag_addr)?;
        a.set_label(&mut spin)?;
        a.pause()?;
        a.cmp(qword_ptr(rbx), 0)?;
        a.je(spin)?;
        a.mov(rsi, data_addr)?;
        a.mov(rax, qword_ptr(rsi))?;
        a.hlt()
    });
    let mapper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        let memory = HostMemory::new(4096).unwrap();
        memory.write(0, &MAGIC.to_le_bytes()).unwrap();
        let data = slots.map(data_addr, memory).unwrap();
        flag.memory().write(0, &1u64.to_le_bytes()).unwrap();
        (flag, data)
    });
    let exit = m.run_for(Duration::from_secs(10)).unwrap();
    let _slots = mapper.join().unwrap();
    assert_eq!(exit, ExitReason::Halt);
    assert_eq!(m.registers().unwrap().rax, MAGIC);
}

/// What a snapshot records is enough to put device memory back at the same
/// guest physical addresses in a new machine.
#[test]
fn the_layout_lets_a_new_machine_map_device_memory_where_it_was() {
    if !kvm_available() {
        return;
    }
    let config = MachineConfig { mem_size: (3 * GIB + 4096) as usize, ..Default::default() };
    let a = Machine::new(config.clone()).unwrap();
    let _window = a
        .memory_slots()
        .map(MMIO64 + 8 * GIB, HostMemory::new((64 * GIB) as usize).unwrap())
        .unwrap();
    let _bar =
        a.memory_slots().map(MMIO32 + PAGE_2M, HostMemory::new(PAGE_2M as usize).unwrap()).unwrap();
    let recorded = a.memory_layout();
    assert_eq!(ram_regions(&recorded), [(0, 3 * GIB), (4 * GIB, 4096)], "{recorded:#x?}");

    let b = Machine::new(config).unwrap();
    let restored: Vec<_> = recorded
        .of_kind(RegionKind::DeviceMemory)
        .map(|r| b.memory_slots().map(r.start, HostMemory::new(r.size as usize).unwrap()).unwrap())
        .collect();
    assert_eq!(restored.len(), 2);
    assert_eq!(b.memory_layout(), recorded);
}

#[test]
fn device_memory_where_it_does_not_belong_is_refused() {
    if !kvm_available() {
        return;
    }
    let m = Machine::new(MachineConfig::default()).unwrap();
    let slots = m.memory_slots();
    let page = || HostMemory::new(4096).unwrap();
    let _taken = slots.map(MMIO64, page()).unwrap();
    for addr in [0, 0x1000, 0xfee0_0000, MMIO64, MMIO64 + 1, 2 * MMIO64] {
        match slots.map(addr, page()) {
            Err(Error::MemoryMap(msg)) => assert!(msg.contains(&format!("{addr:#x}")), "{msg}"),
            other => panic!("{addr:#x}: expected a refusal, got {other:?}"),
        }
    }
    for size in [0, 4097] {
        assert!(matches!(HostMemory::new(size), Err(Error::MemoryMap(_))));
    }
    let expected: Vec<MemoryRegion> =
        m.memory_layout().of_kind(RegionKind::DeviceMemory).copied().collect();
    assert_eq!(expected.len(), 1, "only the first mapping took");
}

#[test]
fn too_much_ram_is_refused() {
    // Refused before any VM exists: no KVM needed.
    let too_much =
        MachineConfig { mem_size: ((1 << 40) - GIB + 4096) as usize, ..Default::default() };
    match Machine::new(too_much) {
        Err(Error::InvalidConfig(msg)) => assert!(msg.contains("at most"), "{msg}"),
        other => panic!("expected a configuration error, got {other:?}"),
    }
}
