// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Builds the CPUID table a vCPU sees from the table the hypervisor supports.
//!
//! The guest sees only the leaves and subleaves libmachine lists ([`LEAVES`]):
//! a leaf a newer hypervisor starts reporting stays invisible until someone
//! decides what it means for a guest. Within the listed leaves, the CPU model
//! decides which feature bits pass.
//!
//! This is pure table manipulation, independent of KVM, so it is unit-tested
//! on synthetic tables and can serve other x86 backends (WHP).

use sha2::{Digest, Sha256};

use crate::cpu::{CpuModel, CpuidEntry};
use crate::error::{Error, Result};

/// Where a vCPU sits in the machine's topology. Every vCPU is its own core in
/// one socket; there is no SMT.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Topology {
    pub(crate) vcpu_id: u32,
    pub(crate) vcpu_count: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reg {
    Eax,
    Ebx,
    Ecx,
    Edx,
}

/// Which subleaves of a listed leaf the guest sees.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Subleaves {
    /// The leaf has no subleaves: only its subleaf-0 entry passes.
    Single,
    /// Every subleaf the hypervisor reports (leaf 4 lists one cache each,
    /// up to a null entry).
    All,
    /// Subleaves 0 to the given one; subleaf 0's EAX, the highest subleaf,
    /// is clamped to it.
    UpTo(u32),
    /// Leaf 0xD: subleaves 0 and 1, and one per XSAVE state component in
    /// [`XSAVE_COMPONENTS`] that the model enables.
    Xsave,
    /// Written whole by libmachine (the topology); the hypervisor's
    /// entries are dropped.
    Generated,
}

use Subleaves::*;

/// Every leaf a guest can see, and why libmachine shows it. Any other leaf
/// the hypervisor reports is dropped, and the highest-leaf field of each
/// range (leaves 0, 0x4000_0000, 0x8000_0000) is clamped to the highest leaf
/// listed here, so the guest finds unlisted leaves empty (zeroes), as on a
/// CPU without them.
///
/// Left out on purpose: MONITOR/MWAIT (5), the PMU (0xA, 0x23), resource
/// monitoring and allocation (0xF, 0x10), SGX (0x12), processor trace
/// (0x14), the TSC and core frequencies (0x15, 0x16: the guest gets them
/// from kvmclock), SoC and TLB enumeration (0x17, 0x18), Key Locker (0x19),
/// hybrid cores (0x1A), PCONFIG (0x1B), LBR (0x1C), HRESET (0x20), TDX
/// (0x21), AMD SVM (0x8000_000A), IBS (0x8000_001B), AMD cache and core
/// topology (0x8000_001D/1E: libmachine's topology is in 0xB/0x1F), memory
/// encryption (0x8000_001F), PerfMonV2 (0x8000_0022), AMD extended topology
/// (0x8000_0026, which Linux prefers to 0xB on AMD), and everything above.
const LEAVES: &[(u32, Subleaves, &str)] = &[
    (0x0, Single, "highest basic leaf, vendor"),
    (0x1, Single, "family, model, features, initial APIC ID"),
    (0x2, Single, "cache and TLB descriptors (Intel)"),
    (0x4, All, "deterministic cache parameters (Intel)"),
    (0x6, Single, "power management: the always-running APIC timer only"),
    (0x7, UpTo(2), "structured extended features, speculation controls"),
    (0xb, Generated, "extended topology"),
    (0xd, Xsave, "XSAVE state components and sizes"),
    (0x1d, UpTo(1), "AMX tile palettes"),
    (0x1e, UpTo(1), "AMX TMUL information"),
    (0x1f, Generated, "V2 extended topology"),
    (0x24, UpTo(0), "AVX10 version and vector lengths"),
    (0x4000_0000, Single, "KVM signature, highest KVM leaf"),
    (0x4000_0001, Single, "KVM paravirtual features (kvmclock, PV EOI, ...)"),
    (0x8000_0000, Single, "highest extended leaf, vendor"),
    (0x8000_0001, Single, "extended features"),
    (0x8000_0002, Single, "brand string"),
    (0x8000_0003, Single, "brand string"),
    (0x8000_0004, Single, "brand string"),
    (0x8000_0005, Single, "L1 cache and TLB (AMD)"),
    (0x8000_0006, Single, "L2 and L3 cache"),
    (0x8000_0007, Single, "invariant TSC"),
    (0x8000_0008, Single, "address sizes, speculation controls, cores (AMD)"),
    (0x8000_0019, Single, "1 GiB page TLB (AMD)"),
    (0x8000_001a, Single, "performance optimizations (AMD)"),
    (0x8000_0021, Single, "extended features 2, speculation controls (AMD)"),
];

/// The XSAVE state components a guest may use: x87 and SSE (0, 1), AVX (2),
/// AVX-512 (5-7), PKRU (9), CET user and supervisor state (11, 12), AMX
/// (17, 18), APX (19). Not MPX (3, 4: removed from Linux and KVM), processor
/// trace (8), PASID (10), HDC (13), UINTR (14), LBR (15) or HWP (16). Each
/// is enabled in XCR0 or XSS only if the model keeps it and the hypervisor
/// describes it (its subleaf of leaf 0xD).
const XSAVE_COMPONENTS: u64 = XSAVE_USER | XSAVE_SUPERVISOR;
/// The user components (XCR0) of [`XSAVE_COMPONENTS`].
const XSAVE_USER: u64 = 0b11 | 1 << 2 | 0b111 << 5 | 1 << 9 | 0b11 << 17 | 1 << 19;
/// The supervisor components (XSS) of [`XSAVE_COMPONENTS`].
const XSAVE_SUPERVISOR: u64 = 0b11 << 11;

/// The legacy XSAVE area and header: the size of an area with no extended
/// component.
const XSAVE_LEGACY_SIZE: u32 = 576;

/// How many subleaves of `function` the guest may see, or `None` for an
/// unlisted leaf.
fn subleaves(function: u32) -> Option<Subleaves> {
    LEAVES.iter().find(|&&(f, ..)| f == function).map(|&(_, s, _)| s)
}

/// Whether the hypervisor's entry `e` passes the list (before the model's
/// XSAVE components are applied).
fn listed(e: &CpuidEntry) -> bool {
    let sub0 = !e.index_significant || e.index == 0;
    match subleaves(e.function) {
        None | Some(Generated) => false,
        Some(Single) => sub0,
        Some(All) => true,
        Some(UpTo(max)) => !e.index_significant || e.index <= max,
        Some(Xsave) => e.index < 64 && XSAVE_COMPONENTS & (1 << e.index) != 0,
    }
}

/// Feature bits every model clears: nested virtualization, the PMU, and the
/// features whose leaf or XSAVE component the guest is not shown (so it
/// never looks for them).
const HIDDEN: &[Feature] = &[
    (1, 0, Ecx, 3, "monitor"),
    (1, 0, Ecx, 5, "vmx"),
    (1, 0, Ecx, 15, "pdcm"),
    (7, 0, Ebx, 2, "sgx"),
    (7, 0, Ebx, 12, "cqm"),
    (7, 0, Ebx, 14, "mpx"),
    (7, 0, Ebx, 15, "rdt_a"),
    (7, 0, Ebx, 25, "intel_pt"),
    (7, 0, Ecx, 23, "keylocker"),
    (7, 0, Ecx, 29, "enqcmd"),
    (7, 0, Ecx, 30, "sgx_lc"),
    (7, 0, Edx, 5, "uintr"),
    (7, 0, Edx, 15, "hybrid_cpu"),
    (7, 0, Edx, 18, "pconfig"),
    (7, 0, Edx, 19, "arch_lbr"),
    (7, 1, Eax, 22, "hreset"),
    (0x8000_0001, 0, Ecx, 2, "svm"),
    (0x8000_0001, 0, Ecx, 10, "ibs"),
    (0x8000_0001, 0, Ecx, 22, "topoext"),
    (0x8000_0001, 0, Ecx, 23, "perfctr_core"),
    (0x8000_0001, 0, Ecx, 24, "perfctr_nb"),
    (0x8000_0001, 0, Ecx, 28, "perfctr_llc"),
];

/// A feature bit: leaf, subleaf, register, bit, name.
type Feature = (u32, u32, Reg, u32, &'static str);

use Reg::*;

/// x86-64-v3 plus the system features a modern Linux guest expects, limited
/// to what Skylake and Zen 1 and newer all have. Every bit here must be
/// present on the host, or the baseline cannot be provided.
const BASELINE: &[Feature] = &[
    // Leaf 1 ECX.
    (1, 0, Ecx, 0, "sse3"),
    (1, 0, Ecx, 1, "pclmulqdq"),
    (1, 0, Ecx, 9, "ssse3"),
    (1, 0, Ecx, 12, "fma"),
    (1, 0, Ecx, 13, "cx16"),
    (1, 0, Ecx, 19, "sse4_1"),
    (1, 0, Ecx, 20, "sse4_2"),
    (1, 0, Ecx, 21, "x2apic"),
    (1, 0, Ecx, 22, "movbe"),
    (1, 0, Ecx, 23, "popcnt"),
    (1, 0, Ecx, 24, "tsc_deadline_timer"),
    (1, 0, Ecx, 25, "aes"),
    (1, 0, Ecx, 26, "xsave"),
    (1, 0, Ecx, 28, "avx"),
    (1, 0, Ecx, 29, "f16c"),
    (1, 0, Ecx, 30, "rdrand"),
    // Leaf 1 EDX.
    (1, 0, Edx, 0, "fpu"),
    (1, 0, Edx, 1, "vme"),
    (1, 0, Edx, 2, "de"),
    (1, 0, Edx, 3, "pse"),
    (1, 0, Edx, 4, "tsc"),
    (1, 0, Edx, 5, "msr"),
    (1, 0, Edx, 6, "pae"),
    (1, 0, Edx, 7, "mce"),
    (1, 0, Edx, 8, "cx8"),
    (1, 0, Edx, 9, "apic"),
    (1, 0, Edx, 11, "sep"),
    (1, 0, Edx, 12, "mtrr"),
    (1, 0, Edx, 13, "pge"),
    (1, 0, Edx, 14, "mca"),
    (1, 0, Edx, 15, "cmov"),
    (1, 0, Edx, 16, "pat"),
    (1, 0, Edx, 17, "pse36"),
    (1, 0, Edx, 19, "clflush"),
    (1, 0, Edx, 23, "mmx"),
    (1, 0, Edx, 24, "fxsr"),
    (1, 0, Edx, 25, "sse"),
    (1, 0, Edx, 26, "sse2"),
    // Leaf 7 subleaf 0 EBX.
    (7, 0, Ebx, 0, "fsgsbase"),
    (7, 0, Ebx, 3, "bmi1"),
    (7, 0, Ebx, 5, "avx2"),
    (7, 0, Ebx, 7, "smep"),
    (7, 0, Ebx, 8, "bmi2"),
    (7, 0, Ebx, 18, "rdseed"),
    (7, 0, Ebx, 19, "adx"),
    (7, 0, Ebx, 20, "smap"),
    (7, 0, Ebx, 23, "clflushopt"),
    // Leaf 0x8000_0001 ECX.
    (0x8000_0001, 0, Ecx, 0, "lahf_lm"),
    (0x8000_0001, 0, Ecx, 5, "abm"),
    (0x8000_0001, 0, Ecx, 8, "3dnowprefetch"),
    // Leaf 0x8000_0001 EDX.
    (0x8000_0001, 0, Edx, 11, "syscall"),
    (0x8000_0001, 0, Edx, 20, "nx"),
    (0x8000_0001, 0, Edx, 26, "pdpe1gb"),
    (0x8000_0001, 0, Edx, 27, "rdtscp"),
    (0x8000_0001, 0, Edx, 29, "lm"),
];

/// Speculation-control bits (leaf 7.0 EDX) that follow the host in every
/// model, so the guest can use the host's mitigations: md_clear, spec_ctrl,
/// stibp, flush_l1d, arch_capabilities, ssbd.
const SPEC_CTRL_7_0_EDX: u32 = 1 << 10 | 1 << 26 | 1 << 27 | 1 << 28 | 1 << 29 | 1 << 31;

/// XSAVE components the baseline enables: x87, SSE, AVX.
const BASELINE_XCR0: u32 = 0b111;
/// XSAVE instructions the baseline offers (leaf 0xD.1 EAX): xsaveopt, xsavec,
/// xgetbv1. Not xsaves, which needs supervisor state.
const BASELINE_XSAVE_1_EAX: u32 = 0b111;

const LEAF1_ECX_OSXSAVE: u32 = 1 << 27;
const LEAF1_ECX_HYPERVISOR: u32 = 1 << 31;
const LEAF1_EDX_HTT: u32 = 1 << 28;
const LEAF6_EAX_ARAT: u32 = 1 << 2;
const LEAF7_EDX_SPEC_CTRL: u32 = 1 << 26;
const LEAF7_EDX_SSBD: u32 = 1 << 31;

/// The highest subleaf of leaf 7 the baseline shows: subleaf 2 carries
/// speculation-control bits.
const BASELINE_LEAF7_MAX_SUBLEAF: u32 = 2;

fn reg(e: &mut CpuidEntry, r: Reg) -> &mut u32 {
    match r {
        Eax => &mut e.eax,
        Ebx => &mut e.ebx,
        Ecx => &mut e.ecx,
        Edx => &mut e.edx,
    }
}

fn find(entries: &mut [CpuidEntry], function: u32, index: u32) -> Option<&mut CpuidEntry> {
    entries.iter_mut().find(|e| e.matches(function, index))
}

fn zero(e: &mut CpuidEntry) {
    e.eax = 0;
    e.ebx = 0;
    e.ecx = 0;
    e.edx = 0;
}

#[derive(PartialEq, Eq)]
enum Vendor {
    Intel,
    Amd,
    Other,
}

fn vendor(entries: &[CpuidEntry]) -> Vendor {
    let Some(leaf0) = entries.iter().find(|e| e.function == 0) else {
        return Vendor::Other;
    };
    let mut id = [0u8; 12];
    id[0..4].copy_from_slice(&leaf0.ebx.to_le_bytes());
    id[4..8].copy_from_slice(&leaf0.edx.to_le_bytes());
    id[8..12].copy_from_slice(&leaf0.ecx.to_le_bytes());
    match &id {
        b"GenuineIntel" => Vendor::Intel,
        b"AuthenticAMD" | b"HygonGenuine" => Vendor::Amd,
        _ => Vendor::Other,
    }
}

/// Builds the CPUID table for one vCPU from the hypervisor's supported table.
pub(crate) fn build(
    supported: &[CpuidEntry],
    model: CpuModel,
    topology: Topology,
) -> Result<Vec<CpuidEntry>> {
    let mut entries = allow_listed(supported);
    hide_unsupported(&mut entries);
    if model == CpuModel::Baseline {
        apply_baseline(&mut entries)?;
    }
    finish_xsave(&mut entries);
    apply_topology(&mut entries, topology);
    Ok(entries)
}

/// The hypervisor's entries that [`LEAVES`] lists, with each range's
/// highest leaf and each listed leaf's highest subleaf clamped to the list.
fn allow_listed(supported: &[CpuidEntry]) -> Vec<CpuidEntry> {
    let mut entries: Vec<CpuidEntry> = supported.iter().copied().filter(listed).collect();
    for e in entries.iter_mut() {
        if e.index != 0 && e.index_significant {
            continue;
        }
        if matches!(e.function, 0 | 0x4000_0000 | 0x8000_0000) {
            let range = e.function;
            let highest = LEAVES
                .iter()
                .map(|&(f, ..)| f)
                .filter(|&f| f & 0xf000_0000 == range)
                .max()
                .expect("the range's own leaf is listed");
            e.eax = e.eax.min(highest);
        }
        if let Some(UpTo(max)) = subleaves(e.function) {
            e.eax = e.eax.min(max);
        }
    }
    entries
}

/// What every model hides ([`HIDDEN`]), and leaf 6 beyond the
/// always-running APIC timer (a vCPU has no thermal or power controls).
/// Sets the hypervisor bit.
fn hide_unsupported(entries: &mut [CpuidEntry]) {
    for &(function, index, r, bit, _) in HIDDEN {
        if let Some(e) = find(entries, function, index) {
            *reg(e, r) &= !(1 << bit);
        }
    }
    for e in entries.iter_mut() {
        match (e.function, e.index) {
            (1, _) => e.ecx |= LEAF1_ECX_HYPERVISOR,
            (6, _) => {
                e.eax &= LEAF6_EAX_ARAT;
                e.ebx = 0;
                e.ecx = 0;
                e.edx = 0;
            }
            // SSBD is controlled through IA32_SPEC_CTRL: without that MSR
            // (as KVM reports on some AMD hosts) the bit is inconsistent,
            // and Linux warns. AMD guests use 0x8000_0008 EBX's SSBD bits.
            (7, 0) if e.edx & LEAF7_EDX_SPEC_CTRL == 0 => e.edx &= !LEAF7_EDX_SSBD,
            _ => {}
        }
    }
}

/// Makes leaf 0xD describe exactly the XSAVE components the guest may
/// enable: XCR0 (0xD.0 EDX:EAX) and XSS (0xD.1 EDX:ECX) keep only the
/// user and supervisor components of [`XSAVE_COMPONENTS`] that have their
/// subleaf, only the
/// enabled components keep their subleaves, and the area's size is that of
/// the user components left (the end of the last one, else the legacy area).
fn finish_xsave(entries: &mut Vec<CpuidEntry>) {
    let described = entries
        .iter()
        .filter(|e| e.function == 0xd && (2..64).contains(&e.index))
        .fold(0b11u64, |m, e| m | 1 << e.index);
    let keep = XSAVE_COMPONENTS & described;
    let (mut xcr0, mut xss) = (0, 0);
    if let Some(e) = find(entries, 0xd, 0) {
        xcr0 = (u64::from(e.edx) << 32 | u64::from(e.eax)) & keep & XSAVE_USER;
        (e.eax, e.edx) = (xcr0 as u32, (xcr0 >> 32) as u32);
    }
    if let Some(e) = find(entries, 0xd, 1) {
        xss = (u64::from(e.edx) << 32 | u64::from(e.ecx)) & keep & XSAVE_SUPERVISOR;
        (e.ecx, e.edx) = (xss as u32, (xss >> 32) as u32);
    }
    let enabled = xcr0 | xss;
    entries.retain(|e| e.function != 0xd || e.index < 2 || enabled & (1 << e.index) != 0);
    let size = entries
        .iter()
        .filter(|e| e.function == 0xd && e.index >= 2 && xcr0 & (1 << e.index) != 0)
        .map(|e| e.ebx.saturating_add(e.eax))
        .fold(XSAVE_LEGACY_SIZE, u32::max);
    if let Some(e) = find(entries, 0xd, 0) {
        e.ebx = size;
        e.ecx = size;
    }
}

fn apply_baseline(entries: &mut [CpuidEntry]) -> Result<()> {
    let missing: Vec<&str> = BASELINE
        .iter()
        .filter(|&&(f, i, r, bit, _)| {
            !find(entries, f, i).is_some_and(|e| *reg(e, r) & (1 << bit) != 0)
        })
        .map(|&(.., name)| name)
        .collect();
    if !missing.is_empty() {
        return Err(Error::CpuModel(format!(
            "this host cannot provide the baseline CPU model; missing: {}",
            missing.join(", ")
        )));
    }

    let mask = |f: u32, i: u32, r: Reg| -> u32 {
        BASELINE
            .iter()
            .filter(|&&(bf, bi, br, ..)| bf == f && bi == i && br == r)
            .fold(0, |m, &(.., bit, _)| m | 1 << bit)
    };

    let has_leaf7_subleaf2 = entries.iter().any(|e| e.matches(7, 2) && e.index == 2);
    for e in entries.iter_mut() {
        match (e.function, e.index) {
            (1, _) => {
                // OSXSAVE mirrors CR4.OSXSAVE and the hypervisor sets it at
                // run time; keep whatever it is.
                let dynamic = e.ecx & LEAF1_ECX_OSXSAVE;
                e.ecx = (e.ecx & mask(1, 0, Ecx)) | dynamic | LEAF1_ECX_HYPERVISOR;
                e.edx &= mask(1, 0, Edx) | LEAF1_EDX_HTT;
            }
            (7, 0) => {
                e.eax = if has_leaf7_subleaf2 { BASELINE_LEAF7_MAX_SUBLEAF } else { 0 };
                e.ebx &= mask(7, 0, Ebx);
                e.ecx = 0;
                e.edx &= SPEC_CTRL_7_0_EDX;
            }
            // Subleaf 2 keeps only its speculation-control bits (EDX).
            (7, 2) => {
                e.eax = 0;
                e.ebx = 0;
                e.ecx = 0;
            }
            (7, _) => zero(e),
            (0xd, 0) => {
                e.eax &= BASELINE_XCR0;
                e.edx = 0;
            }
            // No supervisor state (XSS); `finish_xsave` drops the subleaves
            // of the components left out and sizes the area.
            (0xd, 1) => {
                e.eax &= BASELINE_XSAVE_1_EAX;
                e.ebx = 0;
                e.ecx = 0;
                e.edx = 0;
            }
            (0x8000_0001, _) => {
                e.ecx &= mask(0x8000_0001, 0, Ecx);
                e.edx &= mask(0x8000_0001, 0, Edx);
            }
            _ => {}
        }
    }
    Ok(())
}

/// ceil(log2(n)): how many APIC ID bits `n` vCPUs need.
fn id_bits(n: u32) -> u32 {
    if n <= 1 { 0 } else { 32 - (n - 1).leading_zeros() }
}

/// The extended topology leaves (0xB and 0x1F): level 0 is SMT (one thread
/// per core), level 1 is cores (all vCPUs, in one package), level 2 ends the
/// list. libmachine writes them whole instead of patching the host's: KVM's
/// table may hold only some of their subleaves (on AMD hosts, 0xB subleaf 0
/// alone), and a guest reads every subleaf up to the end marker.
fn extended_topology(function: u32, t: Topology) -> [CpuidEntry; 3] {
    let count = t.vcpu_count.max(1);
    let level = |index: u32, (eax, ebx, ecx): (u32, u32, u32)| CpuidEntry {
        function,
        index,
        index_significant: true,
        eax,
        ebx,
        ecx,
        edx: t.vcpu_id,
    };
    [level(0, (0, 1, 1 << 8)), level(1, (id_bits(count), count, 2 << 8 | 1)), level(2, (0, 0, 2))]
}

fn apply_topology(entries: &mut Vec<CpuidEntry>, t: Topology) {
    let vendor = vendor(entries);
    let count = t.vcpu_count.max(1);
    // The extended topology leaves the guest can reach (up to the highest
    // basic leaf), all generated; the host's are dropped.
    let max_basic = entries.iter().find(|e| e.function == 0).map_or(0, |e| e.eax);
    entries.retain(|e| e.function != 0xb && e.function != 0x1f);
    for function in [0xb, 0x1f] {
        if max_basic >= function {
            entries.extend(extended_topology(function, t));
        }
    }
    for e in entries.iter_mut() {
        match e.function {
            1 => {
                e.ebx = (e.ebx & 0xffff) | (count.min(255) << 16) | ((t.vcpu_id & 0xff) << 24);
                if count > 1 {
                    e.edx |= LEAF1_EDX_HTT;
                } else {
                    e.edx &= !LEAF1_EDX_HTT;
                }
            }
            // Intel deterministic cache parameters.
            4 if vendor == Vendor::Intel && e.eax & 0x1f != 0 => {
                let level = (e.eax >> 5) & 0x7;
                let sharing = if level >= 3 { count - 1 } else { 0 };
                e.eax = (e.eax & 0x3fff) | ((sharing & 0xfff) << 14) | ((count - 1).min(63) << 26);
            }
            // AMD: number of cores and APIC ID size.
            0x8000_0008 if vendor == Vendor::Amd => {
                e.ecx = (e.ecx & !0xf0ff) | (id_bits(count) << 12) | (count - 1);
            }
            _ => {}
        }
    }
}

/// Whether the table describes an AMD CPU with an invariant TSC (leaf
/// 0x8000_0007 EDX bit 8). Such a CPU's TSC counts at the P0 frequency, which
/// AMD firmware reports by setting HWCR.TscFreqSel; Linux warns when it is
/// clear.
pub(crate) fn amd_invariant_tsc(entries: &[CpuidEntry]) -> bool {
    vendor(entries) == Vendor::Amd
        && entries.iter().any(|e| e.function == 0x8000_0007 && e.edx & (1 << 8) != 0)
}

/// A stable identifier of the CPU model: SHA-256 over the vCPU-0 CPUID table
/// and the hypervisor's feature MSRs, as lowercase hex.
pub(crate) fn fingerprint(entries: &[CpuidEntry], feature_msrs: &[(u32, u64)]) -> String {
    let mut entries = entries.to_vec();
    entries.sort_by_key(|e| (e.function, e.index));
    let mut msrs = feature_msrs.to_vec();
    msrs.sort_unstable();

    let mut h = Sha256::new();
    // v2: the allow list (board 13125) replaced zeroed leaves with absent ones.
    h.update(b"libmachine-cpu-model-v2");
    for e in &entries {
        for v in [e.function, e.index, e.index_significant as u32, e.eax, e.ebx, e.ecx, e.edx] {
            h.update(v.to_le_bytes());
        }
    }
    for (index, value) in &msrs {
        h.update(index.to_le_bytes());
        h.update(value.to_le_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(function: u32, index: u32, regs: [u32; 4]) -> CpuidEntry {
        CpuidEntry {
            function,
            index,
            // As KVM reports them: the leaves whose subleaves differ.
            index_significant: matches!(
                function,
                4 | 7 | 0xb | 0xd | 0x1f | 0x8000_001d | 0x8000_0020 | 0x8000_0026
            ),
            eax: regs[0],
            ebx: regs[1],
            ecx: regs[2],
            edx: regs[3],
        }
    }

    fn vendor_leaf(id: &[u8; 12]) -> CpuidEntry {
        let w = |i: usize| u32::from_le_bytes(id[i..i + 4].try_into().unwrap());
        entry(0, 0, [0x1f, w(0), w(8), w(4)])
    }

    /// A host with every baseline feature plus AVX-512F, VMX, PMU and XSAVE
    /// components beyond AVX.
    fn rich_host(id: &[u8; 12]) -> Vec<CpuidEntry> {
        let mut t = vec![
            vendor_leaf(id),
            entry(1, 0, [0x000a_0655, 0x0000_0800, u32::MAX, u32::MAX]),
            entry(4, 0, [0x0000_0121, 0, 0, 0]),
            entry(4, 1, [0x0000_0163, 0, 0, 0]),
            entry(7, 0, [2, u32::MAX, u32::MAX, u32::MAX]),
            entry(7, 1, [u32::MAX, u32::MAX, u32::MAX, u32::MAX]),
            entry(7, 2, [u32::MAX, u32::MAX, u32::MAX, u32::MAX]),
            entry(0xa, 0, [0x0804_3005, 0, 0, 0x0603]),
            entry(0xb, 0, [1, 2, 0x100, 0]),
            entry(0xb, 1, [4, 16, 0x201, 0]),
            entry(0xd, 0, [0xe7, 2696, 2696, 0]),
            entry(0xd, 1, [0xf, 0, 0x100, 0]),
            entry(0xd, 2, [256, 576, 0, 0]),
            entry(0xd, 5, [64, 1088, 0, 0]),
            entry(0x8000_0001, 0, [0, 0, u32::MAX, u32::MAX]),
            entry(0x8000_0008, 0, [0x3030, 0, 0x0000_700f, 0]),
            entry(0x8000_001e, 0, [1, 2, 3, 4]),
        ];
        // AVX-512F is 7.0 EBX bit 16: set (the host has it).
        t[4].ebx |= 1 << 16;
        // The rest of AVX-512's state (appended, so indices above hold).
        t.push(entry(0xd, 6, [512, 1152, 0, 0]));
        t.push(entry(0xd, 7, [1024, 1664, 0, 0]));
        t
    }

    /// Whether `t` has the feature [`HIDDEN`] names `name`.
    fn has(t: &[CpuidEntry], name: &str) -> bool {
        let &(f, i, r, bit, _) = HIDDEN
            .iter()
            .find(|h| h.4 == name)
            .unwrap_or_else(|| panic!("{name} is not in HIDDEN"));
        let mut e = get(t, f, i);
        *reg(&mut e, r) & (1 << bit) != 0
    }

    fn get(t: &[CpuidEntry], f: u32, i: u32) -> CpuidEntry {
        *t.iter().find(|e| e.matches(f, i)).expect("leaf present")
    }

    const ONE: Topology = Topology { vcpu_id: 0, vcpu_count: 1 };

    #[test]
    fn every_model_hides_nested_virt_pmu_and_sets_hypervisor_bit() {
        for model in [CpuModel::Host, CpuModel::Baseline] {
            let t = build(&rich_host(b"GenuineIntel"), model, ONE).unwrap();
            assert_ne!(get(&t, 1, 0).ecx & LEAF1_ECX_HYPERVISOR, 0, "{model}");
            for name in ["vmx", "pdcm", "svm", "sgx", "intel_pt", "sgx_lc", "topoext"] {
                assert!(!has(&t, name), "{model}: {name}");
            }
            // The PMU and AMD's topology leaves are not listed: absent.
            for leaf in [0xa, 0x8000_001e] {
                assert!(t.iter().all(|e| e.function != leaf), "{model}: {leaf:#x}");
            }
        }
    }

    #[test]
    fn ssbd_needs_spec_ctrl() {
        let mut host = rich_host(b"AuthenticAMD");
        host[4].edx = LEAF7_EDX_SSBD;
        for model in [CpuModel::Host, CpuModel::Baseline] {
            let t = build(&host, model, ONE).unwrap();
            assert_eq!(get(&t, 7, 0).edx & LEAF7_EDX_SSBD, 0, "{model}");
        }
        host[4].edx = LEAF7_EDX_SSBD | LEAF7_EDX_SPEC_CTRL;
        let t = build(&host, CpuModel::Host, ONE).unwrap();
        assert_ne!(get(&t, 7, 0).edx & LEAF7_EDX_SSBD, 0);
    }

    #[test]
    fn host_model_keeps_host_features() {
        let t = build(&rich_host(b"GenuineIntel"), CpuModel::Host, ONE).unwrap();
        assert_ne!(get(&t, 7, 0).ebx & (1 << 16), 0, "avx512f kept");
        assert_eq!(get(&t, 0xd, 0).eax, 0xe7, "xcr0 components kept");
    }

    #[test]
    fn baseline_masks_to_template() {
        let t = build(&rich_host(b"GenuineIntel"), CpuModel::Baseline, ONE).unwrap();
        let l7 = get(&t, 7, 0);
        assert_eq!(l7.ebx & (1 << 16), 0, "avx512f hidden");
        assert_ne!(l7.ebx & (1 << 5), 0, "avx2 kept");
        assert_eq!(l7.ecx, 0);
        assert_eq!(l7.edx, SPEC_CTRL_7_0_EDX, "spec-ctrl bits pass through");
        assert_eq!(l7.eax, 2, "max subleaf");
        assert_eq!(get(&t, 7, 1), entry(7, 1, [0; 4]));
        assert_eq!(get(&t, 7, 2), entry(7, 2, [0, 0, 0, u32::MAX]));
        let xsave = get(&t, 0xd, 0);
        assert_eq!(xsave.eax, 0b111, "x87, SSE, AVX only");
        assert_eq!((xsave.ebx, xsave.ecx), (832, 832), "legacy + AVX area");
        assert_eq!(get(&t, 0xd, 1).eax, 0b111, "no xsaves");
        for avx512 in 5..=7 {
            assert!(!t.iter().any(|e| e.matches(0xd, avx512)), "AVX-512 state {avx512} hidden");
        }
    }

    #[test]
    fn baseline_is_the_same_on_hosts_with_more_features() {
        let richer = rich_host(b"GenuineIntel");
        let mut plain = rich_host(b"GenuineIntel");
        // The plain host lacks AVX-512 and the extra XSAVE components.
        plain[4].ebx &= !(1 << 16);
        plain[10].eax = 0x7;
        plain[1].ecx &= !(1 << 2); // a leaf-1 feature outside the template
        let a = build(&richer, CpuModel::Baseline, ONE).unwrap();
        let b = build(&plain, CpuModel::Baseline, ONE).unwrap();
        assert_eq!(fingerprint(&a, &[]), fingerprint(&b, &[]));
        let ha = build(&richer, CpuModel::Host, ONE).unwrap();
        let hb = build(&plain, CpuModel::Host, ONE).unwrap();
        assert_ne!(fingerprint(&ha, &[]), fingerprint(&hb, &[]));
    }

    #[test]
    fn baseline_names_missing_features() {
        let mut t = rich_host(b"GenuineIntel");
        t[4].ebx &= !(1 << 5 | 1 << 8); // no avx2, no bmi2
        match build(&t, CpuModel::Baseline, ONE) {
            Err(Error::CpuModel(msg)) => {
                assert!(msg.contains("avx2") && msg.contains("bmi2"), "{msg}");
                assert!(!msg.contains("fma"), "{msg}");
            }
            other => panic!("expected CpuModel error, got {other:?}"),
        }
        // The host model works on the same table.
        build(&t, CpuModel::Host, ONE).unwrap();
    }

    #[test]
    fn topology_for_several_vcpus() {
        let t4 = Topology { vcpu_id: 3, vcpu_count: 4 };
        let t = build(&rich_host(b"GenuineIntel"), CpuModel::Host, t4).unwrap();
        let l1 = get(&t, 1, 0);
        assert_eq!(l1.ebx >> 24, 3, "initial APIC ID");
        assert_eq!((l1.ebx >> 16) & 0xff, 4, "logical processors");
        assert_ne!(l1.edx & LEAF1_EDX_HTT, 0);
        assert_eq!(get(&t, 0xb, 0), entry(0xb, 0, [0, 1, 0x100, 3]));
        assert_eq!(get(&t, 0xb, 1), entry(0xb, 1, [2, 4, 0x201, 3]));
        // Intel leaf 4: L1 (level 1) is private, cores per package = 4.
        let l1d = get(&t, 4, 0);
        assert_eq!(l1d.eax >> 26, 3);
        assert_eq!((l1d.eax >> 14) & 0xfff, 0);

        let one = build(&rich_host(b"GenuineIntel"), CpuModel::Host, ONE).unwrap();
        assert_eq!(get(&one, 1, 0).edx & LEAF1_EDX_HTT, 0);
        assert_eq!(get(&one, 0xb, 1).eax, 0);
    }

    #[test]
    fn amd_core_count() {
        let t = build(
            &rich_host(b"AuthenticAMD"),
            CpuModel::Host,
            Topology { vcpu_id: 0, vcpu_count: 6 },
        )
        .unwrap();
        let e = get(&t, 0x8000_0008, 0);
        assert_eq!(e.ecx & 0xff, 5, "NC = cores - 1");
        assert_eq!((e.ecx >> 12) & 0xf, 3, "APIC ID size");
        assert!(!has(&t, "topoext"));
        // Intel's leaf 4 is left alone on AMD.
        assert_eq!(get(&t, 4, 0).eax, 0x0000_0121);
    }

    #[test]
    fn fingerprint_ignores_order_and_includes_msrs() {
        let t = build(&rich_host(b"GenuineIntel"), CpuModel::Host, ONE).unwrap();
        let mut rev = t.clone();
        rev.reverse();
        assert_eq!(fingerprint(&t, &[(1, 2)]), fingerprint(&rev, &[(1, 2)]));
        assert_ne!(fingerprint(&t, &[(1, 2)]), fingerprint(&t, &[(1, 3)]));
        assert_eq!(fingerprint(&t, &[]).len(), 64);
    }

    #[test]
    fn amd_invariant_tsc_needs_amd_and_the_bit() {
        let mut amd = rich_host(b"AuthenticAMD");
        assert!(!amd_invariant_tsc(&amd), "no leaf 0x8000_0007 yet");
        amd.push(entry(0x8000_0007, 0, [0, 0, 0, 1 << 8]));
        assert!(amd_invariant_tsc(&amd));
        let mut intel = rich_host(b"GenuineIntel");
        intel.push(entry(0x8000_0007, 0, [0, 0, 0, 1 << 8]));
        assert!(!amd_invariant_tsc(&intel));
    }

    /// The topology a guest finds, read as Linux 6.18 reads it
    /// (`arch/x86/kernel/cpu/topology_ext.c`, `topology_amd.c`): leaf 0x1F
    /// if the basic leaves reach it and it parses, else (AMD) leaf
    /// 0x8000_0026 likewise, else leaf 0xB, else (AMD) leaf 0x8000_0008. Returns (SMT shift, core shift,
    /// processors at the core level, x2APIC IDs the subleaves report).
    fn guest_topology(t: &[CpuidEntry]) -> (u32, u32, u32, Vec<u32>) {
        /// More topology levels than the architecture defines (SMT, core,
        /// module, tile, die, die group, package).
        const MAX_SUBLEAVES: u32 = 16;
        // What the CPU returns for a subleaf the table lacks: zeroes.
        let read = |f: u32, i: u32| {
            t.iter().find(|e| e.matches(f, i)).copied().unwrap_or(entry(f, i, [0; 4]))
        };
        let max_basic = read(0, 0).eax;
        let max_ext = read(0x8000_0000, 0).eax;
        for leaf in [0x1f, 0x8000_0026, 0xb] {
            let max = if leaf & 0x8000_0000 != 0 { max_ext } else { max_basic };
            if max < leaf {
                continue;
            }
            let (mut smt, mut core, mut ids) = (None, None, Vec::new());
            // A real table ends its list within a few subleaves; one that
            // answers every subleaf (an entry whose index is not
            // significant) would make an unbounded walk run until memory
            // runs out, so the walk is bounded and fails instead.
            for sub in 0..=MAX_SUBLEAVES {
                assert!(
                    sub < MAX_SUBLEAVES,
                    "leaf {leaf:#x} lists more than {MAX_SUBLEAVES} topology levels"
                );
                let e = read(leaf, sub);
                let kind = (e.ecx >> 8) & 0xff;
                if e.ebx & 0xffff == 0 || kind == 0 {
                    break;
                }
                ids.push(e.edx);
                match kind {
                    1 => smt = Some((e.eax & 0x1f, e.ebx & 0xffff)),
                    2 => core = Some((e.eax & 0x1f, e.ebx & 0xffff)),
                    // Domains above the core (module, tile, die, ...).
                    _ => {}
                }
            }
            if let Some((smt_shift, _)) = smt {
                // A missing core level leaves the core domain as wide as SMT.
                let (core_shift, ncpus) = core.unwrap_or((smt_shift, 1));
                return (smt_shift, core_shift, ncpus, ids);
            }
        }
        let ecx = read(0x8000_0008, 0).ecx;
        let shift = match (ecx >> 12) & 0xf {
            0 => id_bits((ecx & 0xff) + 1),
            sft => sft,
        };
        (0, shift, (ecx & 0xff) + 1, Vec::new())
    }

    /// Host tables with any subset of the topology subleaves, any highest
    /// basic leaf and either vendor: every vCPU of every count finds itself
    /// a core of one package of `count` cores, one thread each, under its
    /// own APIC ID.
    #[test]
    fn every_host_table_gives_the_same_topology() {
        let optional = [(0xb, 0), (0xb, 1), (0x1f, 0), (0x1f, 1), (0x1f, 2), (0x8000_0026, 0)];
        for vendor_id in [b"GenuineIntel", b"AuthenticAMD"] {
            for max_basic in [0xd, 0x10, 0x1f, 0x24] {
                for present in 0..1u32 << optional.len() {
                    let mut host: Vec<CpuidEntry> =
                        rich_host(vendor_id).into_iter().filter(|e| e.function != 0xb).collect();
                    host[0].eax = max_basic;
                    host.push(entry(0x8000_0000, 0, [0x8000_0026, 0, 0, 0]));
                    for (bit, &(f, i)) in optional.iter().enumerate() {
                        if present & (1 << bit) != 0 {
                            // The host's own topology: 2 threads a core.
                            host.push(entry(f, i, [1 + i, 2 << i, (i + 1) << 8 | i, 7]));
                        }
                    }
                    for count in [1, 2, 3, 4, 9, 254] {
                        for vcpu_id in [0, count / 2, count - 1] {
                            let t = Topology { vcpu_id, vcpu_count: count };
                            let table = build(&host, CpuModel::Host, t).unwrap();
                            let what = format!(
                                "{} max leaf {max_basic:#x}, subleaves {present:#b}, \
                                 vCPU {vcpu_id} of {count}",
                                String::from_utf8_lossy(vendor_id)
                            );
                            let (smt, core, ncpus, ids) = guest_topology(&table);
                            assert_eq!(smt, 0, "{what}: one thread per core");
                            assert_eq!(core, id_bits(count), "{what}: core shift");
                            assert_eq!(vcpu_id >> core, 0, "{what}: package 0");
                            assert_eq!(ncpus, count, "{what}: cores in the package");
                            assert!(ids.iter().all(|&id| id == vcpu_id), "{what}: {ids:?}");
                            let leaf1 = get(&table, 1, 0);
                            assert_eq!(leaf1.ebx >> 24, vcpu_id & 0xff, "{what}");
                        }
                    }
                }
            }
        }
    }

    /// A host that reports every leaf of every range, and leaves past them,
    /// with subleaves and every bit set: what a future hypervisor might
    /// hand libmachine. Built on [`rich_host`], whose leaves it keeps.
    fn junk_host(id: &[u8; 12]) -> Vec<CpuidEntry> {
        let mut t = rich_host(id);
        t[0].eax = 0x30;
        let known: Vec<u32> = t.iter().map(|e| e.function).collect();
        let leaves = (0..=0x30)
            .chain(0x4000_0000..=0x4000_0110)
            .chain(0x8000_0000..=0x8000_0030)
            .chain(0xc000_0000..=0xc000_0004)
            .chain([0x2000_0000, 0xffff_ffff]);
        for f in leaves.filter(|f| !known.contains(f)) {
            // Some leaves as one entry for every subleaf, the others with
            // subleaves that differ.
            if f % 3 == 0 {
                t.push(CpuidEntry { index_significant: false, ..entry(f, 0, [u32::MAX; 4]) });
            } else {
                for i in 0..4 {
                    t.push(CpuidEntry { index_significant: true, ..entry(f, i, [u32::MAX; 4]) });
                }
            }
        }
        for i in 3..64 {
            if !t.iter().any(|e| e.function == 0xd && e.index == i) {
                t.push(entry(0xd, i, [64, 0x1000 + 64 * i, 0, 0]));
            }
        }
        for i in 3..8 {
            t.push(entry(7, i, [u32::MAX; 4]));
        }
        let xsave = t.iter_mut().find(|e| e.matches(0xd, 0)).unwrap();
        (xsave.eax, xsave.edx) = (u32::MAX, u32::MAX);
        let xss = t.iter_mut().find(|e| e.matches(0xd, 1)).unwrap();
        (xss.ecx, xss.edx) = (u32::MAX, u32::MAX);
        t
    }

    /// What the guest may see, written out apart from [`LEAVES`] so that the
    /// two check each other: (leaf, subleaf) of every entry allowed.
    fn allowed(model: CpuModel, function: u32, index: u32) -> bool {
        match function {
            0 | 1 | 2 | 6 | 0x4000_0000 | 0x4000_0001 => index == 0,
            0x8000_0000..=0x8000_0008 | 0x8000_0019 | 0x8000_001a | 0x8000_0021 => index == 0,
            4 => true,
            7 | 0xb | 0x1f => index <= 2,
            0x1d | 0x1e => index <= 1,
            0x24 => index == 0,
            0xd if model == CpuModel::Host => {
                [0, 1, 2, 5, 6, 7, 9, 11, 12, 17, 18, 19].contains(&index)
            }
            0xd => index <= 2,
            _ => false,
        }
    }

    #[test]
    fn unknown_leaves_never_reach_the_guest() {
        for vendor_id in [b"GenuineIntel", b"AuthenticAMD"] {
            let host = junk_host(vendor_id);
            for model in [CpuModel::Host, CpuModel::Baseline] {
                for vcpus in [1, 4] {
                    let topology = Topology { vcpu_id: vcpus - 1, vcpu_count: vcpus };
                    let t = build(&host, model, topology).unwrap();
                    let what = format!("{} {model} x{vcpus}", String::from_utf8_lossy(vendor_id));
                    for e in &t {
                        assert!(
                            allowed(model, e.function, e.index),
                            "{what}: leaf {:#x}.{} reached the guest",
                            e.function,
                            e.index
                        );
                    }
                    // The listed leaves the host has all pass.
                    for (f, i) in [(0x4000_0001, 0), (0x8000_0021, 0), (0x1d, 1), (0x1f, 2)] {
                        assert!(t.iter().any(|e| e.matches(f, i)), "{what}: {f:#x}.{i}");
                    }
                    // The highest leaf of each range, and of leaves with
                    // subleaves, is the list's.
                    assert_eq!(get(&t, 0, 0).eax, 0x24, "{what}");
                    assert_eq!(get(&t, 0x4000_0000, 0).eax, 0x4000_0001, "{what}");
                    assert_eq!(get(&t, 0x8000_0000, 0).eax, 0x8000_0021, "{what}");
                    assert_eq!(get(&t, 7, 0).eax, 2, "{what}");
                    assert_eq!(get(&t, 0x1d, 0).eax, 1, "{what}");
                    assert_eq!(get(&t, 0x24, 0).eax, 0, "{what}");
                    // Leaf 6 keeps only ARAT.
                    assert_eq!(get(&t, 6, 0), entry(6, 0, [LEAF6_EAX_ARAT, 0, 0, 0]));
                    // No feature bit points at a leaf the guest lacks.
                    for &(.., name) in HIDDEN {
                        assert!(!has(&t, name), "{what}: {name}");
                    }
                }
            }
        }
    }

    /// Whatever the host reports, leaf 0xD describes exactly the components
    /// XCR0 and XSS enable, and the area holds every user component.
    #[test]
    fn xsave_describes_exactly_the_enabled_components() {
        let mut hosts = vec![rich_host(b"GenuineIntel"), junk_host(b"AuthenticAMD")];
        // A host that enables AVX-512 state it does not describe.
        let mut missing = rich_host(b"GenuineIntel");
        missing.retain(|e| !(e.function == 0xd && e.index == 6));
        hosts.push(missing);
        for host in &hosts {
            for model in [CpuModel::Host, CpuModel::Baseline] {
                let t = build(host, model, ONE).unwrap();
                let (x0, x1) = (get(&t, 0xd, 0), get(&t, 0xd, 1));
                let xcr0 = u64::from(x0.edx) << 32 | u64::from(x0.eax);
                let xss = u64::from(x1.edx) << 32 | u64::from(x1.ecx);
                assert_eq!(xcr0 & xss, 0, "{model}: a component in both");
                let subleaves: u64 = t
                    .iter()
                    .filter(|e| e.function == 0xd && e.index >= 2)
                    .fold(0, |m, e| m | 1 << e.index);
                assert_eq!(subleaves, (xcr0 | xss) & !0b11, "{model}: {xcr0:#x} {xss:#x}");
                let end = t
                    .iter()
                    .filter(|e| e.function == 0xd && e.index >= 2 && xcr0 & (1 << e.index) != 0)
                    .map(|e| e.ebx + e.eax)
                    .max()
                    .unwrap_or(XSAVE_LEGACY_SIZE);
                assert_eq!((x0.ebx, x0.ecx), (end, end), "{model}: area size");
            }
        }
        // Without its subleaf, the component is not enabled.
        let t = build(&hosts[2], CpuModel::Host, ONE).unwrap();
        assert_eq!(get(&t, 0xd, 0).eax, 0xa7, "ZMM_Hi256 (6) dropped");
    }

    #[test]
    fn id_bits_rounds_up() {
        assert_eq!([1, 2, 3, 4, 5, 8, 9].map(id_bits), [0, 1, 2, 2, 3, 3, 4]);
    }
}
