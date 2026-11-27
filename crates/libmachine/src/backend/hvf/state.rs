// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What a snapshot holds of a Hypervisor.framework VM ([`VmState`]: its GIC, the host's clock) and
//! of each vCPU ([`VcpuState`]: its registers, the system registers the framework keeps, its GIC
//! CPU interface, its virtual counter's offset, SME's state, its PSCI power), read and set on the
//! vCPU's own thread, as the framework has every call about a vCPU made.
//!
//! The guest's virtual counter goes on from where it was at the snapshot, as Firecracker's arm64
//! restores and QEMU's do: no time passes for the guest while it is in a file, so that no timer
//! of its sees a jump (Linux's watchdogs would take one for a lockup).

use std::ffi::c_void;

use super::ffi::*;
use super::{check, hv_err};
use crate::error::{Error, Result};
use crate::snapshot::codec::{Decoder, Encoder};

/// The registers `hv_vcpu_get_reg` reads: X0 to X30, PC, FPCR, FPSR, CPSR.
const REGS: u32 = 35;
/// The SIMD and floating point registers, Q0 to Q31.
const SIMD_REGS: u32 = 32;

/// The EL1 and EL0 system registers the framework keeps for every vCPU (macOS 15), whose values
/// are the guest's to keep across a snapshot. The ID registers (the host's, as the guest sees
/// them; the fingerprint names the chip), MPIDR_EL1 (set from the vCPU's index) and EL2's (a VM
/// with EL2 is not snapshotted) are not among them.
const SYS_REGS: &[hv_sys_reg_t] = &[
    0x8010, // MDCCINT_EL1
    0x8012, // MDSCR_EL1
    0xc080, // SCTLR_EL1
    0xc081, // ACTLR_EL1
    0xc082, // CPACR_EL1
    0xc100, // TTBR0_EL1
    0xc101, // TTBR1_EL1
    0xc102, // TCR_EL1
    0xc108, // APIAKEYLO_EL1
    0xc109, // APIAKEYHI_EL1
    0xc10a, // APIBKEYLO_EL1
    0xc10b, // APIBKEYHI_EL1
    0xc110, // APDAKEYLO_EL1
    0xc111, // APDAKEYHI_EL1
    0xc112, // APDBKEYLO_EL1
    0xc113, // APDBKEYHI_EL1
    0xc118, // APGAKEYLO_EL1
    0xc119, // APGAKEYHI_EL1
    0xc200, // SPSR_EL1
    0xc201, // ELR_EL1
    0xc208, // SP_EL0
    0xc288, // AFSR0_EL1
    0xc289, // AFSR1_EL1
    0xc290, // ESR_EL1
    0xc300, // FAR_EL1
    0xc3a0, // PAR_EL1
    0xc510, // MAIR_EL1
    0xc518, // AMAIR_EL1
    0xc600, // VBAR_EL1
    0xc681, // CONTEXTIDR_EL1
    0xc684, // TPIDR_EL1
    0xc708, // CNTKCTL_EL1
    0xd000, // CSSELR_EL1
    0xde82, // TPIDR_EL0
    0xde83, // TPIDRRO_EL0
    0xdf11, // CNTP_CTL_EL0
    0xdf12, // CNTP_CVAL_EL0
    HV_SYS_REG_CNTV_CTL_EL0,
    HV_SYS_REG_CNTV_CVAL_EL0,
    0xe208, // SP_EL1
];

/// Those of a CPU with SME (ID_AA64PFR1_EL1.SME), which the framework keeps from macOS 15.2:
/// SMCR_EL1, SMPRI_EL1, TPIDR2_EL0.
const SME_SYS_REGS: &[hv_sys_reg_t] = &[0xc096, 0xc094, 0xde85];

/// The debug registers of breakpoint or watchpoint `n`: DBGBVRn_EL1, DBGBCRn_EL1 (`n` of
/// ID_AA64DFR0_EL1.BRPs), DBGWVRn_EL1, DBGWCRn_EL1 (of WRPs).
fn breakpoint(n: u16) -> [hv_sys_reg_t; 2] {
    [0x8004 + 8 * n, 0x8005 + 8 * n]
}

fn watchpoint(n: u16) -> [hv_sys_reg_t; 2] {
    [0x8006 + 8 * n, 0x8007 + 8 * n]
}

const ID_AA64DFR0_EL1: hv_sys_reg_t = 0xc028;

/// The GIC CPU interface's registers a guest at EL1 sets (ICC_RPR_EL1, read only, is the
/// interface's to work out).
const ICC_REGS: &[hv_gic_icc_reg_t] = &[
    0xc230, // ICC_PMR_EL1
    0xc643, // ICC_BPR0_EL1
    0xc644, // ICC_AP0R0_EL1
    0xc648, // ICC_AP1R0_EL1
    0xc663, // ICC_BPR1_EL1
    0xc664, // ICC_CTLR_EL1
    0xc665, // ICC_SRE_EL1
    0xc666, // ICC_IGRPEN0_EL1
    0xc667, // ICC_IGRPEN1_EL1
];

/// A vCPU's PSCI power state, as a snapshot keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Power {
    Off,
    /// CPU_ON came; its thread had not started it yet.
    Starting {
        entry: u64,
        context: u64,
    },
    On,
}

/// SME's state of a vCPU in streaming mode, or with ZA on: what the framework zeroes as either
/// changes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sme {
    streaming: bool,
    za: bool,
    /// Z0 to Z31 and P0 to P15, in streaming mode: the streaming vector length's bytes each, an
    /// eighth of them each.
    z: Vec<Vec<u8>>,
    p: Vec<Vec<u8>>,
    /// ZA, with ZA on: the vector length's square; and ZT0 on an SME2 CPU.
    za_array: Vec<u8>,
    zt0: Option<Vec<u8>>,
}

/// A vCPU's state in a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VcpuState {
    pub(crate) power: Power,
    regs: Vec<u64>,
    simd: Vec<[u8; 16]>,
    /// The system registers and their values.
    sys: Vec<(hv_sys_reg_t, u64)>,
    icc: Vec<(hv_gic_icc_reg_t, u64)>,
    /// The virtual counter's offset from the host's ticks, which [`VmState::ticks`] ties to the
    /// guest's count.
    vtimer_offset: u64,
    /// Whether the framework had the virtual timer masked: it masks it as its interrupt goes to
    /// the GIC, until the guest deactivates it, which the GIC's state may have in flight.
    vtimer_masked: bool,
    sme: Option<Sme>,
}

/// A VM's state in a snapshot, besides its vCPUs'.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VmState {
    /// The host's ticks (`mach_absolute_time`) when the state was taken: with each vCPU's offset,
    /// the guest's count then.
    ticks: u64,
    /// The GIC's distributor's and redistributors' state, as the framework serializes it.
    gic: Vec<u8>,
}

fn get_reg(id: hv_vcpu_t, reg: hv_reg_t) -> Result<u64> {
    let mut value = 0;
    // SAFETY: this thread owns the vCPU; writes into a live local.
    check("get a vCPU register", unsafe { hv_vcpu_get_reg(id, reg, &mut value) })?;
    Ok(value)
}

fn get_sys(id: hv_vcpu_t, reg: hv_sys_reg_t) -> Result<u64> {
    let mut value = 0;
    // SAFETY: this thread owns the vCPU; writes into a live local.
    check("get a system register", unsafe { hv_vcpu_get_sys_reg(id, reg, &mut value) })?;
    Ok(value)
}

/// The system registers of this host's vCPUs that a snapshot keeps: the list, the debug
/// registers the CPU has, and SME's if it has SME.
fn sys_regs(id: hv_vcpu_t) -> Result<Vec<hv_sys_reg_t>> {
    let dfr0 = get_sys(id, ID_AA64DFR0_EL1)?;
    let brps = ((dfr0 >> 12) & 0xf) as u16 + 1;
    let wrps = ((dfr0 >> 20) & 0xf) as u16 + 1;
    let mut regs = SYS_REGS.to_vec();
    regs.extend((0..brps).flat_map(breakpoint));
    regs.extend((0..wrps).flat_map(watchpoint));
    if sme_level(id)? > 0 {
        regs.extend(SME_SYS_REGS);
    }
    Ok(regs)
}

/// The CPU's SME, as its guest sees it: 0 none, 1 SME, 2 SME2 (ID_AA64PFR1_EL1.SME).
fn sme_level(id: hv_vcpu_t) -> Result<u64> {
    Ok((get_sys(id, HV_SYS_REG_ID_AA64PFR1_EL1)? >> 24) & 0xf)
}

/// The streaming vector length's bytes, the most a guest may set (its SMCR_EL1.LEN may set
/// fewer; the framework's registers are of the most).
fn svl_bytes() -> Result<usize> {
    let mut svl = 0;
    // SAFETY: writes into a live local.
    check("get SME's vector length", unsafe { hv_sme_config_get_max_svl_bytes(&mut svl) })?;
    Ok(svl)
}

impl VcpuState {
    /// The state of vCPU `id`, which this thread owns, powered `power`.
    pub(crate) fn save(id: hv_vcpu_t, power: Power) -> Result<Self> {
        let regs = (0..REGS).map(|r| get_reg(id, HV_REG_X0 + r)).collect::<Result<_>>()?;
        let simd = (0..SIMD_REGS)
            .map(|r| {
                let mut q = [0u8; 16];
                // SAFETY: this thread owns the vCPU; writes 16 bytes into a live local.
                check("get a SIMD register", unsafe { hv_vcpu_get_simd_fp_reg(id, r, &mut q) })?;
                Ok(q)
            })
            .collect::<Result<_>>()?;
        let sys = sys_regs(id)?
            .into_iter()
            .map(|reg| Ok((reg, get_sys(id, reg)?)))
            .collect::<Result<_>>()?;
        let icc = ICC_REGS
            .iter()
            .map(|&reg| {
                let mut value = 0;
                // SAFETY: this thread owns the vCPU; writes into a live local.
                check("get a GIC CPU interface register", unsafe {
                    hv_gic_get_icc_reg(id, reg, &mut value)
                })?;
                Ok((reg, value))
            })
            .collect::<Result<_>>()?;
        let mut vtimer_offset = 0;
        // SAFETY: this thread owns the vCPU; writes into a live local.
        check("get the vtimer offset", unsafe {
            hv_vcpu_get_vtimer_offset(id, &mut vtimer_offset)
        })?;
        let mut vtimer_masked = false;
        // SAFETY: this thread owns the vCPU; writes into a live local.
        check("get the vtimer mask", unsafe { hv_vcpu_get_vtimer_mask(id, &mut vtimer_masked) })?;
        let sme = Sme::save(id)?;
        Ok(Self { power, regs, simd, sys, icc, vtimer_offset, vtimer_masked, sme })
    }

    /// Sets vCPU `id`, which this thread owns, as it was but for its GIC CPU interface
    /// ([`restore_icc`](Self::restore_icc), once the GIC's state is back): its counter shifted by
    /// `shift` host ticks, the time that passed between the snapshot and the restore, so that the
    /// guest's count goes on from where it was.
    pub(crate) fn restore(&self, id: hv_vcpu_t, shift: u64) -> Result<()> {
        // The counter first: the timer's registers then never meet a count of the host's, which
        // their compare value would be long past.
        // SAFETY: this thread owns the vCPU.
        check("set the vtimer offset", unsafe {
            hv_vcpu_set_vtimer_offset(id, self.vtimer_offset.wrapping_add(shift))
        })?;
        for (r, &value) in self.regs.iter().enumerate() {
            // SAFETY: this thread owns the vCPU.
            check("set a vCPU register", unsafe {
                hv_vcpu_set_reg(id, HV_REG_X0 + r as u32, value)
            })?;
        }
        for (r, &q) in self.simd.iter().enumerate() {
            // SAFETY: this thread owns the vCPU.
            check("set a SIMD register", unsafe { hv_vcpu_set_simd_fp_reg(id, r as u32, q) })?;
        }
        let ours = sys_regs(id)?;
        for &(reg, value) in &self.sys {
            if !ours.contains(&reg) {
                return Err(Error::Snapshot(format!(
                    "the snapshot's vCPU has system register {reg:#x}, which this host's do not"
                )));
            }
            // SAFETY: this thread owns the vCPU.
            check("set a system register", unsafe { hv_vcpu_set_sys_reg(id, reg, value) })?;
        }
        if let Some(sme) = &self.sme {
            sme.restore(id)?;
        }
        Ok(())
    }

    /// Sets the GIC CPU interface of vCPU `id`, which this thread owns, as it was, and its
    /// virtual timer's mask, as the GIC's state has the timer's interrupt.
    pub(crate) fn restore_icc(&self, id: hv_vcpu_t) -> Result<()> {
        // SAFETY: this thread owns the vCPU.
        check("set the vtimer mask", unsafe { hv_vcpu_set_vtimer_mask(id, self.vtimer_masked) })?;
        for &(reg, value) in &self.icc {
            // SAFETY: this thread owns the vCPU.
            check("set a GIC CPU interface register", unsafe {
                hv_gic_set_icc_reg(id, reg, value)
            })?;
        }
        Ok(())
    }

    pub(crate) fn encode(&self, e: &mut Encoder) {
        match self.power {
            Power::Off => e.u8(0),
            Power::On => e.u8(1),
            Power::Starting { entry, context } => {
                e.u8(2);
                e.u64(entry);
                e.u64(context);
            }
        }
        e.count(self.regs.len());
        self.regs.iter().for_each(|&r| e.u64(r));
        e.count(self.simd.len());
        self.simd.iter().for_each(|q| e.raw(q));
        for pairs in [&self.sys, &self.icc] {
            e.count(pairs.len());
            for &(reg, value) in pairs {
                e.u16(reg);
                e.u64(value);
            }
        }
        e.u64(self.vtimer_offset);
        e.bool(self.vtimer_masked);
        match &self.sme {
            None => e.bool(false),
            Some(sme) => {
                e.bool(true);
                sme.encode(e);
            }
        }
    }

    pub(crate) fn decode(d: &mut Decoder<'_>) -> Result<Self> {
        let power = match d.u8()? {
            0 => Power::Off,
            1 => Power::On,
            2 => Power::Starting { entry: d.u64()?, context: d.u64()? },
            other => return Err(damaged(&format!("vCPU power state {other}"))),
        };
        let n = counted(d, REGS, 8, "general registers")?;
        let regs = (0..n).map(|_| d.u64()).collect::<Result<Vec<_>>>()?;
        let n = counted(d, SIMD_REGS, 16, "SIMD registers")?;
        let simd = (0..n).map(|_| d.raw::<[u8; 16]>()).collect::<Result<Vec<_>>>()?;
        let mut pairs = || -> Result<Vec<(u16, u64)>> {
            let n = d.count(10)?;
            (0..n).map(|_| Ok((d.u16()?, d.u64()?))).collect()
        };
        let sys = pairs()?;
        let icc = pairs()?;
        if icc.iter().map(|&(reg, _)| reg).ne(ICC_REGS.iter().copied()) {
            return Err(damaged("its GIC CPU interface's registers are not this libmachine's"));
        }
        let vtimer_offset = d.u64()?;
        let vtimer_masked = d.bool()?;
        let sme = if d.bool()? { Some(Sme::decode(d)?) } else { None };
        Ok(Self { power, regs, simd, sys, icc, vtimer_offset, vtimer_masked, sme })
    }
}

/// A list's count, of items of at least `min` bytes each: refused unless it is `want`.
fn counted(d: &mut Decoder<'_>, want: u32, min: usize, what: &str) -> Result<usize> {
    let n = d.count(min)?;
    if n != want as usize {
        return Err(damaged(&format!("{n} {what}, not {want}")));
    }
    Ok(n)
}

fn damaged(what: &str) -> Error {
    Error::Snapshot(format!("the snapshot's vCPU state is damaged: {what}"))
}

impl Sme {
    /// SME's state of vCPU `id`, if it has SME and is in streaming mode or has ZA on.
    fn save(id: hv_vcpu_t) -> Result<Option<Self>> {
        let level = sme_level(id)?;
        if level == 0 {
            return Ok(None);
        }
        let mut state = hv_vcpu_sme_state_t::default();
        // SAFETY: this thread owns the vCPU; writes into a live local.
        check("get SME's state", unsafe { hv_vcpu_get_sme_state(id, &mut state) })?;
        let (streaming, za) = (state.streaming_sve_mode_enabled, state.za_storage_enabled);
        if !streaming && !za {
            return Ok(None);
        }
        let svl = svl_bytes()?;
        let read = |what: &'static str, len: usize, get: &dyn Fn(*mut u8, usize) -> hv_return_t| {
            let mut bytes = vec![0u8; len];
            check(what, get(bytes.as_mut_ptr(), len)).map(|()| bytes)
        };
        let (mut z, mut p) = (Vec::new(), Vec::new());
        if streaming {
            for r in 0..32 {
                // SAFETY: this thread owns the vCPU; writes `len` bytes into the buffer.
                z.push(read("get an SME Z register", svl, &|v, len| unsafe {
                    hv_vcpu_get_sme_z_reg(id, r, v, len)
                })?);
            }
            for r in 0..16 {
                // SAFETY: as above.
                p.push(read("get an SME P register", svl / 8, &|v, len| unsafe {
                    hv_vcpu_get_sme_p_reg(id, r, v, len)
                })?);
            }
        }
        let (mut za_array, mut zt0) = (Vec::new(), None);
        if za {
            // SAFETY: as above.
            za_array = read("get SME's ZA", svl * svl, &|v, len| unsafe {
                hv_vcpu_get_sme_za_reg(id, v, len)
            })?;
            if level >= 2 {
                let mut t = [0u8; 64];
                // SAFETY: this thread owns the vCPU; writes into a live local.
                check("get SME's ZT0", unsafe { hv_vcpu_get_sme_zt0_reg(id, &mut t) })?;
                zt0 = Some(t.to_vec());
            }
        }
        Ok(Some(Self { streaming, za, z, p, za_array, zt0 }))
    }

    /// Sets SME's state of vCPU `id` as it was: the modes first, which zero the registers.
    fn restore(&self, id: hv_vcpu_t) -> Result<()> {
        let level = sme_level(id)?;
        let svl = if level > 0 { svl_bytes()? } else { 0 };
        let sized = |bytes: &[u8], len: usize| bytes.len() == len;
        let fits = self.z.iter().all(|z| sized(z, svl))
            && self.p.iter().all(|p| sized(p, svl / 8))
            && (!self.za || sized(&self.za_array, svl * svl))
            && (self.zt0.is_none() || level >= 2);
        if level == 0 || !fits {
            return Err(Error::Snapshot(format!(
                "the snapshot's vCPU used SME with registers this host's vCPUs (SME level \
                 {level}, {svl}-byte vectors) do not have"
            )));
        }
        let state = hv_vcpu_sme_state_t {
            streaming_sve_mode_enabled: self.streaming,
            za_storage_enabled: self.za,
        };
        // SAFETY: this thread owns the vCPU; reads a live local.
        check("set SME's state", unsafe { hv_vcpu_set_sme_state(id, &state) })?;
        for (r, z) in self.z.iter().enumerate() {
            // SAFETY: this thread owns the vCPU; reads the buffer's bytes.
            check("set an SME Z register", unsafe {
                hv_vcpu_set_sme_z_reg(id, r as u32, z.as_ptr(), z.len())
            })?;
        }
        for (r, p) in self.p.iter().enumerate() {
            // SAFETY: as above.
            check("set an SME P register", unsafe {
                hv_vcpu_set_sme_p_reg(id, r as u32, p.as_ptr(), p.len())
            })?;
        }
        if self.za {
            // SAFETY: as above.
            check("set SME's ZA", unsafe {
                hv_vcpu_set_sme_za_reg(id, self.za_array.as_ptr(), self.za_array.len())
            })?;
        }
        if let Some(zt0) = &self.zt0 {
            let t: [u8; 64] = zt0.as_slice().try_into().map_err(|_| damaged("ZT0's size"))?;
            // SAFETY: this thread owns the vCPU; reads a live local.
            check("set SME's ZT0", unsafe { hv_vcpu_set_sme_zt0_reg(id, &t) })?;
        }
        Ok(())
    }

    fn encode(&self, e: &mut Encoder) {
        e.bool(self.streaming);
        e.bool(self.za);
        for regs in [&self.z, &self.p] {
            e.count(regs.len());
            regs.iter().for_each(|r| e.bytes(r));
        }
        e.bytes(&self.za_array);
        match &self.zt0 {
            None => e.bool(false),
            Some(t) => {
                e.bool(true);
                e.bytes(t);
            }
        }
    }

    fn decode(d: &mut Decoder<'_>) -> Result<Self> {
        let (streaming, za) = (d.bool()?, d.bool()?);
        let mut regs = |want: usize| -> Result<Vec<Vec<u8>>> {
            let n = d.count(4)?;
            if n != want {
                return Err(damaged(&format!("{n} SME registers, not {want}")));
            }
            (0..n).map(|_| Ok(d.bytes()?.to_vec())).collect()
        };
        let z = regs(if streaming { 32 } else { 0 })?;
        let p = regs(if streaming { 16 } else { 0 })?;
        let za_array = d.bytes()?.to_vec();
        let zt0 = if d.bool()? { Some(d.bytes()?.to_vec()) } else { None };
        Ok(Self { streaming, za, z, p, za_array, zt0 })
    }
}

impl VmState {
    /// The GIC's state, the VM stopped (between runs), and the host's ticks now.
    pub(crate) fn save() -> Result<Self> {
        // SAFETY: a new OS object, released below; no other pointers.
        let state = unsafe { hv_gic_state_create() };
        if state.is_null() {
            return Err(hv_err("take the GIC's state (the framework could not represent it)", -1));
        }
        let gic = (|| -> Result<Vec<u8>> {
            let mut size = 0;
            // SAFETY: `state` is live; writes into a live local.
            check("get the GIC state's size", unsafe { hv_gic_state_get_size(state, &mut size) })?;
            let mut gic = vec![0u8; size];
            // SAFETY: `gic` holds the `size` bytes the framework writes.
            check("get the GIC's state", unsafe {
                hv_gic_state_get_data(state, gic.as_mut_ptr().cast::<c_void>())
            })?;
            Ok(gic)
        })();
        // SAFETY: the framework's object, used no more.
        unsafe { os_release(state) };
        // SAFETY: no arguments.
        let ticks = unsafe { mach_absolute_time() };
        Ok(Self { ticks, gic: gic? })
    }

    /// How many host ticks passed since the state was taken, as this host counts (on another,
    /// or after a restart, a count from elsewhere: the guest's clock goes on from where it was
    /// either way, by its offsets).
    pub(crate) fn shift(&self) -> u64 {
        // SAFETY: no arguments.
        unsafe { mach_absolute_time() }.wrapping_sub(self.ticks)
    }

    /// Puts the GIC's state back: every vCPU created, none run yet.
    pub(crate) fn restore_gic(&self) -> Result<()> {
        // SAFETY: the framework reads `len` bytes of the buffer.
        check("restore the GIC's state (one of another macOS?)", unsafe {
            hv_gic_set_state(self.gic.as_ptr().cast::<c_void>(), self.gic.len())
        })
    }

    pub(crate) fn encode(&self, e: &mut Encoder) {
        e.u64(self.ticks);
        e.bytes(&self.gic);
    }

    pub(crate) fn decode(d: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self { ticks: d.u64()?, gic: d.bytes()?.to_vec() })
    }
}
