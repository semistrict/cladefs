// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! MSR state the CPU model controls beyond CPUID (board 13121): no PMU, and
//! AMD's HWCR.TscFreqSel set where the TSC is invariant.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

mod common;

use common::{ENTRY, asm16, kvm_available, machine_with};
use iced_x86::code_asm::*;
use libmachine::{ExitReason, MachineConfig};

/// Where the guest stores what it read.
const OUT: u32 = 0x500;
/// Where the #GP handler records that it ran.
const FAULTED: u32 = 0x508;
const HANDLER_ADDR: u64 = 0x2000;
const GP_VECTOR: u64 = 13;

const MSR_K7_HWCR: u32 = 0xc001_0015;
const MSR_K7_EVNTSEL0: u32 = 0xc001_0000;
const MSR_K7_PERFCTR0: u32 = 0xc001_0004;
const MSR_IA32_PERFEVTSEL0: u32 = 0x186;
const MSR_IA32_PMC0: u32 = 0xc1;
/// Event select: count in user and kernel mode, enabled.
const EVTSEL_USR_OS_EN: u32 = (1 << 16) | (1 << 17) | (1 << 22);
/// "Cycles not in halt": AMD event 0x76, Intel event 0x3c.
const AMD_CYCLES: u32 = 0x76;
const INTEL_CYCLES: u32 = 0x3c;

fn host_is_amd() -> bool {
    // SAFETY: CPUID leaf 0 exists on every x86_64 CPU.
    #[allow(unused_unsafe)]
    let r = unsafe { std::arch::x86_64::__cpuid(0) };
    [r.ebx, r.edx, r.ecx] == [0x6874_7541, 0x6974_6e65, 0x444d_4163] // "AuthenticAMD"
}

fn host_ignores_msrs() -> bool {
    std::fs::read_to_string("/sys/module/kvm/parameters/ignore_msrs").is_ok_and(|s| s.trim() == "Y")
}

/// Runs `rdmsr` for `msr` in a real-mode guest. Returns the value read, or
/// None if the read raised #GP (caught through the real-mode IVT).
fn guest_rdmsr(msr: u32) -> Option<u64> {
    run_catching_gp(&asm16(|a| {
        a.mov(ecx, msr)?;
        a.rdmsr()?;
        a.mov(dword_ptr(OUT), eax)?;
        a.mov(dword_ptr(OUT + 4), edx)?;
        a.hlt()
    }))
}

/// Programs performance counter 0 to count cycles, runs a busy loop, and
/// reads the counter. None if any of it raised #GP.
fn guest_counts_cycles(evtsel: u32, counter: u32, event: u32) -> Option<u64> {
    run_catching_gp(&asm16(|a| {
        let mut spin = a.create_label();
        a.mov(ecx, evtsel)?;
        a.mov(eax, EVTSEL_USR_OS_EN | event)?;
        a.xor(edx, edx)?;
        a.wrmsr()?;
        a.mov(ecx, 100_000u32)?;
        a.set_label(&mut spin)?;
        a.dec(ecx)?;
        a.jnz(spin)?;
        a.mov(ecx, counter)?;
        a.rdmsr()?;
        a.mov(dword_ptr(OUT), eax)?;
        a.mov(dword_ptr(OUT + 4), edx)?;
        a.hlt()
    }))
}

/// Runs `code` with a real-mode #GP handler installed. Returns what the
/// guest stored at OUT, or None if it faulted.
fn run_catching_gp(code: &[u8]) -> Option<u64> {
    let handler = asm16(|a| {
        a.mov(dword_ptr(FAULTED), 0xdead)?;
        a.hlt()
    });
    let mut m = machine_with(MachineConfig::default(), code);
    m.load(HANDLER_ADDR, &handler).unwrap();
    // IVT entry for #GP: offset, then segment 0.
    let mut ivt = (HANDLER_ADDR as u16).to_le_bytes().to_vec();
    ivt.extend(0u16.to_le_bytes());
    m.load(GP_VECTOR * 4, &ivt).unwrap();
    assert_eq!(m.run().unwrap(), ExitReason::Halt);
    let faulted = m.read_memory(FAULTED.into(), 4).unwrap();
    if faulted == 0xdeadu32.to_le_bytes() {
        return None;
    }
    let raw = m.read_memory(OUT.into(), 8).unwrap();
    Some(u64::from_le_bytes(raw.try_into().unwrap()))
}

/// With the vPMU off a counter never counts. KVM answers AMD's legacy
/// counter MSRs with 0 and drops writes (guests probe them unconditionally)
/// rather than faulting; on Intel they fault. Either way: no cycles counted.
#[test]
fn guest_performance_counters_do_not_count() {
    if !kvm_available() {
        return;
    }
    let (evtsel, counter, event) = if host_is_amd() {
        (MSR_K7_EVNTSEL0, MSR_K7_PERFCTR0, AMD_CYCLES)
    } else {
        (MSR_IA32_PERFEVTSEL0, MSR_IA32_PMC0, INTEL_CYCLES)
    };
    let counted = guest_counts_cycles(evtsel, counter, event);
    assert!(
        matches!(counted, None | Some(0)),
        "performance counter {counter:#x} counted {counted:?} cycles"
    );
}

#[test]
fn amd_guest_sees_tsc_freq_sel() {
    if !kvm_available() {
        return;
    }
    let m = machine_with(MachineConfig::default(), &[]);
    let invariant_tsc = m.cpuid(0x8000_0007, 0).is_some_and(|e| e.edx & (1 << 8) != 0);
    if !host_is_amd() || !invariant_tsc {
        eprintln!("skipping: not an AMD host with an invariant TSC");
        return;
    }
    let hwcr = guest_rdmsr(MSR_K7_HWCR).expect("HWCR is readable");
    assert_ne!(hwcr & (1 << 24), 0, "HWCR {hwcr:#x} lacks TscFreqSel");
}

#[test]
fn the_ivt_handler_catches_a_fault() {
    // Checks the test harness itself: an MSR that exists nowhere faults.
    if !kvm_available() || host_ignores_msrs() {
        return;
    }
    assert_eq!(guest_rdmsr(0xdead_0000), None);
    let _ = ENTRY;
}
