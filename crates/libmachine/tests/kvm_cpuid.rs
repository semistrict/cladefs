// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A real-mode guest runs CPUID, and what it sees is checked against the
//! configured CPU model.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

mod common;

use common::{asm16, kvm_available, load};
use iced_x86::code_asm::*;
use libmachine::{CpuModel, Error, ExitReason, Machine, MachineConfig};

/// Where the guest stores CPUID results: 16 bytes per leaf.
const OUT: u32 = 0x500;

const LEAF1_ECX_VMX: u32 = 1 << 5;
const LEAF1_ECX_OSXSAVE: u32 = 1 << 27;
const LEAF1_ECX_HYPERVISOR: u32 = 1 << 31;
const LEAF1_EDX_APIC: u32 = 1 << 9;
const LEAF7_EBX_AVX2: u32 = 1 << 5;
const LEAF7_EBX_AVX512F: u32 = 1 << 16;
const EXT1_ECX_SVM: u32 = 1 << 2;

fn machine(cpu_model: CpuModel) -> Machine {
    Machine::new(MachineConfig { cpu_model, ..Default::default() }).expect("create machine")
}

/// A program that runs CPUID for each (leaf, subleaf), stores EAX, EBX, ECX,
/// EDX at OUT + 16 * i, and halts.
fn cpuid_program(leaves: &[(u32, u32)]) -> Vec<u8> {
    asm16(|a| {
        for (i, &(leaf, subleaf)) in leaves.iter().enumerate() {
            let out = OUT + 16 * i as u32;
            a.mov(eax, leaf)?;
            a.mov(ecx, subleaf)?;
            a.cpuid()?;
            a.mov(dword_ptr(out), eax)?;
            a.mov(dword_ptr(out + 4), ebx)?;
            a.mov(dword_ptr(out + 8), ecx)?;
            a.mov(dword_ptr(out + 12), edx)?;
        }
        a.hlt()
    })
}

/// Runs CPUID in the guest; returns [eax, ebx, ecx, edx] per leaf.
fn guest_cpuid(m: &mut Machine, leaves: &[(u32, u32)]) -> Vec<[u32; 4]> {
    load(m, &cpuid_program(leaves));
    assert_eq!(m.run().unwrap(), ExitReason::Halt);
    let raw = m.read_memory(OUT as u64, 16 * leaves.len()).unwrap();
    raw.chunks(16)
        .map(|c| {
            std::array::from_fn(|r| u32::from_le_bytes(c[r * 4..r * 4 + 4].try_into().unwrap()))
        })
        .collect()
}

#[test]
fn guest_sees_the_configured_table() {
    if !kvm_available() {
        return;
    }
    for model in [CpuModel::Host, CpuModel::Baseline] {
        let mut m = machine(model);
        let leaves =
            [(0, 0), (1, 0), (7, 0), (0x8000_0000, 0), (0x8000_0001, 0), (0xd, 0), (0xd, 1)];
        let seen = guest_cpuid(&mut m, &leaves);
        for (&(leaf, sub), got) in leaves.iter().zip(&seen) {
            let want = m.cpuid(leaf, sub).expect("leaf configured");
            let mut want = [want.eax, want.ebx, want.ecx, want.edx];
            let mut got = *got;
            if leaf == 1 {
                // Bits the hypervisor derives from live CPU state.
                for r in [&mut want, &mut got] {
                    r[2] &= !LEAF1_ECX_OSXSAVE;
                    r[3] &= !LEAF1_EDX_APIC;
                }
            }
            if leaf == 0xd {
                // EBX is the XSAVE size for the guest's current XCR0/XSS,
                // which the hypervisor computes at run time.
                want[1] = 0;
                got[1] = 0;
            }
            assert_eq!(got, want, "{model} leaf {leaf:#x}.{sub}");
        }
    }
}

/// Leaves libmachine does not list are absent from the table, and a guest
/// reads them as a CPU without them answers: zeroes (for leaves within the
/// highest leaf of their range, which is the list's).
#[test]
fn guest_sees_only_listed_leaves() {
    if !kvm_available() {
        return;
    }
    let unlisted = [
        (0x3, 0),
        (0x5, 0),
        (0x7, 3),
        (0xa, 0),
        (0xd, 3),
        (0xd, 8),
        (0x12, 0),
        (0x14, 0),
        (0x8000_000a, 0),
        (0x8000_001e, 0),
        (0x8000_001f, 0),
        (0x8000_0022, 0),
        (0x8000_0026, 0),
        (0x4000_0002, 0),
        (0x4000_0100, 0),
    ];
    for model in [CpuModel::Host, CpuModel::Baseline] {
        let mut m = machine(model);
        for &(leaf, sub) in &unlisted {
            assert_eq!(m.cpuid(leaf, sub), None, "{model} leaf {leaf:#x}.{sub}");
        }
        let highest = |range: u32| m.cpuid(range, 0).expect("range leaf").eax;
        let (basic, kvm, ext) = (highest(0), highest(0x4000_0000), highest(0x8000_0000));
        assert!(basic <= 0x24, "{model}: highest basic leaf {basic:#x}");
        assert_eq!(kvm, 0x4000_0001, "{model}");
        assert!(ext <= 0x8000_0021, "{model}: highest extended leaf {ext:#x}");
        let in_range: Vec<(u32, u32)> = unlisted
            .iter()
            .copied()
            .filter(|&(leaf, _)| match leaf {
                0x8000_0000.. => leaf <= ext,
                0x4000_0000.. => leaf <= kvm,
                _ => leaf <= basic,
            })
            .collect();
        assert!(in_range.len() >= 8, "{model}: {in_range:x?}");
        let seen = guest_cpuid(&mut m, &in_range);
        for (&(leaf, sub), got) in in_range.iter().zip(&seen) {
            assert_eq!(*got, [0; 4], "{model} leaf {leaf:#x}.{sub}");
        }
    }
}

#[test]
fn guest_never_sees_nested_virtualization_and_knows_it_is_virtual() {
    if !kvm_available() {
        return;
    }
    for model in [CpuModel::Host, CpuModel::Baseline] {
        let mut m = machine(model);
        let seen = guest_cpuid(&mut m, &[(1, 0), (0x8000_0001, 0)]);
        assert_ne!(seen[0][2] & LEAF1_ECX_HYPERVISOR, 0, "{model}");
        assert_eq!(seen[0][2] & LEAF1_ECX_VMX, 0, "{model}");
        assert_eq!(seen[1][2] & EXT1_ECX_SVM, 0, "{model}");
        // One vCPU: APIC ID 0, one logical processor.
        assert_eq!(seen[0][1] >> 16, 1, "{model}");
    }
}

#[test]
fn baseline_hides_avx512_and_host_model_follows_the_host() {
    if !kvm_available() {
        return;
    }
    let host_has_avx512 = std::arch::is_x86_feature_detected!("avx512f");
    let mut host = machine(CpuModel::Host);
    let mut base =
        match Machine::new(MachineConfig { cpu_model: CpuModel::Baseline, ..Default::default() }) {
            Ok(m) => m,
            Err(Error::CpuModel(msg)) => {
                eprintln!("skipping: host cannot provide the baseline: {msg}");
                return;
            }
            Err(e) => panic!("{e}"),
        };
    let h = guest_cpuid(&mut host, &[(7, 0), (0xd, 0)]);
    let b = guest_cpuid(&mut base, &[(7, 0), (0xd, 0)]);
    assert_eq!(h[0][1] & LEAF7_EBX_AVX512F != 0, host_has_avx512);
    assert_eq!(b[0][1] & LEAF7_EBX_AVX512F, 0);
    assert_ne!(b[0][1] & LEAF7_EBX_AVX2, 0);
    assert_eq!(b[1][0], 0b111, "baseline XSAVE: x87, SSE, AVX");
    assert_eq!(base.cpu_model(), CpuModel::Baseline);
    assert_eq!(host.cpu_model(), CpuModel::Host);
}

#[test]
fn fingerprints_identify_the_model() {
    if !kvm_available() {
        return;
    }
    let a = machine(CpuModel::Host);
    let b = machine(CpuModel::Host);
    assert_eq!(a.cpu_fingerprint(), b.cpu_fingerprint());
    assert_eq!(a.cpu_fingerprint().len(), 64);
    assert!(a.cpu_fingerprint().bytes().all(|c| c.is_ascii_hexdigit()));
    if std::arch::is_x86_feature_detected!("avx512f") {
        let base = machine(CpuModel::Baseline);
        assert_ne!(a.cpu_fingerprint(), base.cpu_fingerprint());
    }
}

#[test]
fn cpu_model_parses_from_strings() {
    assert_eq!("host".parse::<CpuModel>(), Ok(CpuModel::Host));
    assert_eq!("baseline".parse::<CpuModel>(), Ok(CpuModel::Baseline));
    assert!("haswell".parse::<CpuModel>().is_err());
    assert_eq!(CpuModel::default(), CpuModel::Host);
}
