// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What a snapshot keeps of a KVM VM and its vCPUs: the in-kernel interrupt
//! controllers and timer, the clock, and every vCPU's registers, FPU and
//! vector state, local APIC, MSRs and pending events, each as KVM's own
//! structure (its UAPI layout).
//!
//! The clock moves on across a snapshot: a restore sets kvmclock to its
//! value at the snapshot plus the real time elapsed since, and moves every
//! vCPU's TSC on by as many cycles, so the guest's wall clock and monotonic
//! clocks are right again whichever clocksource it uses (the TSC or
//! kvmclock), and its vCPUs' TSCs stay in step with each other and with
//! kvmclock (Documentation/virt/kvm/api.rst, KVM_SET_CLOCK; the TSC by
//! MSR, written before the TSC deadline that depends on it).

use kvm_bindings::{
    KVM_IRQCHIP_IOAPIC, KVM_IRQCHIP_PIC_MASTER, KVM_IRQCHIP_PIC_SLAVE, KVM_MAX_MSR_ENTRIES, Msrs,
    Xsave, kvm_clock_data, kvm_debugregs, kvm_irqchip, kvm_lapic_state, kvm_mp_state,
    kvm_msr_entry, kvm_pit_state2, kvm_regs, kvm_sregs, kvm_vcpu_events, kvm_xcrs,
};
use kvm_ioctls::{Cap, VcpuFd};

use super::{Vcpu, Vm};
use crate::error::{Error, Result, kvm_err};
use crate::snapshot::codec::{Decoder, Encoder};

const MSR_IA32_TSC: u32 = 0x10;
/// MSRs KVM saves but does not list in KVM_GET_MSR_INDEX_LIST: the MTRRs
/// (variable ranges, fixed ranges, default type) and the PAT, as firmware
/// and the guest set them.
const UNLISTED_MSRS: &[u32] = &[
    0x200, 0x201, 0x202, 0x203, 0x204, 0x205, 0x206, 0x207, 0x208, 0x209, 0x20a, 0x20b, 0x20c,
    0x20d, 0x20e, 0x20f, 0x250, 0x258, 0x259, 0x268, 0x269, 0x26a, 0x26b, 0x26c, 0x26d, 0x26e,
    0x26f, 0x277, 0x2ff,
];

/// The VM's own state (not its vCPUs').
pub(crate) struct VmState {
    pic_master: kvm_irqchip,
    pic_slave: kvm_irqchip,
    ioapic: kvm_irqchip,
    pit: kvm_pit_state2,
    /// kvmclock at the snapshot, in nanoseconds.
    clock: u64,
    /// The host's CLOCK_REALTIME then, in nanoseconds.
    realtime: i64,
    /// The guest's TSC frequency.
    tsc_khz: u32,
}

/// One vCPU's state.
pub(crate) struct VcpuState {
    mp_state: kvm_mp_state,
    regs: kvm_regs,
    sregs: kvm_sregs,
    /// KVM_GET_XSAVE2's buffer: the legacy 4 KiB region, then the rest.
    xsave_region: Vec<u8>,
    xsave_extra: Vec<u32>,
    xcrs: kvm_xcrs,
    debugregs: kvm_debugregs,
    lapic: kvm_lapic_state,
    /// Every MSR KVM would save for this vCPU: (index, value), by index.
    msrs: Vec<(u32, u64)>,
    events: kvm_vcpu_events,
}

fn realtime_ns() -> i64 {
    let now = std::time::SystemTime::now();
    match now.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i64,
        Err(e) => -(e.duration().as_nanos() as i64),
    }
}

fn irqchip(vm: &Vm, chip_id: u32) -> Result<kvm_irqchip> {
    let mut chip = kvm_irqchip { chip_id, ..Default::default() };
    vm.vm.get_irqchip(&mut chip).map_err(kvm_err("get the interrupt controllers' state"))?;
    Ok(chip)
}

impl Vm {
    /// The VM's state, with every vCPU stopped.
    pub(crate) fn save_state(&self, vcpu0: &Vcpu) -> Result<VmState> {
        let clock: kvm_clock_data = self.vm.get_clock().map_err(kvm_err("get the clock"))?;
        Ok(VmState {
            pic_master: irqchip(self, KVM_IRQCHIP_PIC_MASTER)?,
            pic_slave: irqchip(self, KVM_IRQCHIP_PIC_SLAVE)?,
            ioapic: irqchip(self, KVM_IRQCHIP_IOAPIC)?,
            pit: self.vm.get_pit2().map_err(kvm_err("get the PIT's state"))?,
            clock: clock.clock,
            realtime: realtime_ns(),
            tsc_khz: vcpu0.fd.get_tsc_khz().map_err(kvm_err("get the TSC frequency"))?,
        })
    }

    /// Puts back the VM's state before its vCPUs' ([`Vcpu::restore_state`]
    /// with the returned cycles): the interrupt controllers, the PIT, and
    /// the clock moved on by the real time elapsed since the snapshot.
    /// Returns by how many cycles every vCPU's TSC must move on with it.
    pub(crate) fn restore_state(&self, state: &VmState, vcpus: &[Vcpu]) -> Result<u64> {
        // The guest calibrated its TSC at this frequency: keep it (KVM
        // scales the TSC if the host's differs beyond its tolerance, or
        // refuses).
        for vcpu in vcpus {
            let khz = vcpu.fd.get_tsc_khz().map_err(kvm_err("get the TSC frequency"))?;
            if khz != state.tsc_khz {
                vcpu.fd.set_tsc_khz(state.tsc_khz).map_err(|e| {
                    Error::Snapshot(format!(
                        "the snapshot's guest TSC runs at {} kHz, this host's at {khz} kHz, \
                         and KVM cannot scale it: {e}",
                        state.tsc_khz
                    ))
                })?;
            }
        }
        self.vm.set_pit2(&state.pit).map_err(kvm_err("set the PIT's state"))?;
        for chip in [&state.pic_master, &state.pic_slave, &state.ioapic] {
            self.vm.set_irqchip(chip).map_err(kvm_err("set the interrupt controllers' state"))?;
        }
        let elapsed = u64::try_from(realtime_ns().saturating_sub(state.realtime)).unwrap_or(0);
        let clock =
            kvm_clock_data { clock: state.clock.saturating_add(elapsed), ..Default::default() };
        self.vm.set_clock(&clock).map_err(kvm_err("set the clock"))?;
        let cycles = u128::from(elapsed) * u128::from(state.tsc_khz) / 1_000_000;
        Ok(cycles as u64)
    }
}

impl VmState {
    pub(crate) fn encode(&self, e: &mut Encoder) {
        e.raw(&self.pic_master);
        e.raw(&self.pic_slave);
        e.raw(&self.ioapic);
        e.raw(&self.pit);
        e.u64(self.clock);
        e.i64(self.realtime);
        e.u32(self.tsc_khz);
    }

    pub(crate) fn decode(d: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self {
            pic_master: d.raw()?,
            pic_slave: d.raw()?,
            ioapic: d.raw()?,
            pit: d.raw()?,
            clock: d.u64()?,
            realtime: d.i64()?,
            tsc_khz: d.u32()?,
        })
    }
}

/// The MSRs among `indices` that `fd` can read, with their values: KVM
/// stops a read at the first MSR this vCPU does not have, which is skipped.
fn read_msrs(fd: &VcpuFd, indices: &[u32]) -> Result<Vec<(u32, u64)>> {
    let mut out = Vec::with_capacity(indices.len());
    let mut rest = indices;
    while !rest.is_empty() {
        let chunk = &rest[..rest.len().min(KVM_MAX_MSR_ENTRIES)];
        let entries: Vec<kvm_msr_entry> =
            chunk.iter().map(|&index| kvm_msr_entry { index, ..Default::default() }).collect();
        let mut msrs = Msrs::from_entries(&entries).expect("a chunk fits");
        let read = fd.get_msrs(&mut msrs).map_err(kvm_err("get MSRs"))?;
        out.extend(msrs.as_slice()[..read].iter().map(|e| (e.index, e.data)));
        rest = &rest[(read + 1).min(chunk.len())..];
    }
    Ok(out)
}

fn write_msrs(fd: &VcpuFd, msrs: &[(u32, u64)]) -> Result<()> {
    for chunk in msrs.chunks(KVM_MAX_MSR_ENTRIES) {
        let entries: Vec<kvm_msr_entry> = chunk
            .iter()
            .map(|&(index, data)| kvm_msr_entry { index, data, ..Default::default() })
            .collect();
        let set = fd
            .set_msrs(&Msrs::from_entries(&entries).expect("a chunk fits"))
            .map_err(kvm_err("set MSRs"))?;
        if set != chunk.len() {
            return Err(Error::Snapshot(format!(
                "KVM refused MSR {:#x} of the snapshot's",
                chunk[set].0
            )));
        }
    }
    Ok(())
}

impl Vcpu {
    /// The vCPU's state, between runs. `msr_indices` is what to save.
    pub(crate) fn save_state(&self, vm: &Vm) -> Result<VcpuState> {
        let fd = &self.fd;
        // First: reading the MP state makes KVM take the APIC events
        // pending (INIT, SIPI), which can change the rest.
        let mp_state = fd.get_mp_state().map_err(kvm_err("get the MP state"))?;
        let regs = fd.get_regs().map_err(kvm_err("get regs"))?;
        let sregs = fd.get_sregs().map_err(kvm_err("get sregs"))?;
        let xsave = xsave_buffer(vm)?;
        let mut xsave = xsave;
        // SAFETY: the buffer is as large as KVM_CAP_XSAVE2 says it writes.
        unsafe { fd.get_xsave2(&mut xsave) }.map_err(kvm_err("get XSAVE state"))?;
        let region = xsave.as_fam_struct_ref().xsave.region;
        let xsave_region = region.iter().flat_map(|w| w.to_le_bytes()).collect();
        let xsave_extra = xsave.as_slice().to_vec();
        let xcrs = fd.get_xcrs().map_err(kvm_err("get XCRs"))?;
        let debugregs = fd.get_debug_regs().map_err(kvm_err("get debug registers"))?;
        let lapic = fd.get_lapic().map_err(kvm_err("get the local APIC"))?;
        let mut indices: Vec<u32> = vm.msr_indices.iter().chain(UNLISTED_MSRS).copied().collect();
        indices.sort_unstable();
        indices.dedup();
        let msrs = read_msrs(fd, &indices)?;
        let events = fd.get_vcpu_events().map_err(kvm_err("get pending events"))?;
        Ok(VcpuState {
            mp_state,
            regs,
            sregs,
            xsave_region,
            xsave_extra,
            xcrs,
            debugregs,
            lapic,
            msrs,
            events,
        })
    }

    /// Puts back a vCPU's state, its TSC moved on by `tsc_cycles`
    /// ([`Vm::restore_state`]), before the vCPU first runs.
    pub(crate) fn restore_state(&self, vm: &Vm, state: &VcpuState, tsc_cycles: u64) -> Result<()> {
        let fd = &self.fd;
        fd.set_mp_state(state.mp_state).map_err(kvm_err("set the MP state"))?;
        fd.set_regs(&state.regs).map_err(kvm_err("set regs"))?;
        fd.set_sregs(&state.sregs).map_err(kvm_err("set sregs"))?;
        let mut xsave = xsave_buffer(vm)?;
        if xsave.as_slice().len() != state.xsave_extra.len() || state.xsave_region.len() != 4096 {
            return Err(Error::Snapshot(format!(
                "the snapshot's XSAVE state is {} bytes, this host's {}",
                state.xsave_region.len() + 4 * state.xsave_extra.len(),
                4096 + 4 * xsave.as_slice().len()
            )));
        }
        // SAFETY: only the region's words change; the length stays.
        let region = unsafe { &mut xsave.as_mut_fam_struct().xsave.region };
        for (w, b) in region.iter_mut().zip(state.xsave_region.as_chunks::<4>().0) {
            *w = u32::from_le_bytes(*b);
        }
        xsave.as_mut_slice().copy_from_slice(&state.xsave_extra);
        // SAFETY: the buffer is the size KVM_CAP_XSAVE2 gives.
        unsafe { fd.set_xsave2(&xsave) }.map_err(kvm_err("set XSAVE state"))?;
        fd.set_xcrs(&state.xcrs).map_err(kvm_err("set XCRs"))?;
        fd.set_debug_regs(&state.debugregs).map_err(kvm_err("set debug registers"))?;
        fd.set_lapic(&state.lapic).map_err(kvm_err("set the local APIC"))?;
        // In index order: the TSC (0x10) before the TSC deadline (0x6e0)
        // that is relative to it.
        let msrs: Vec<(u32, u64)> = state
            .msrs
            .iter()
            .map(|&(index, data)| match index {
                MSR_IA32_TSC => (index, data.wrapping_add(tsc_cycles)),
                _ => (index, data),
            })
            .collect();
        write_msrs(fd, &msrs)?;
        fd.set_vcpu_events(&state.events).map_err(kvm_err("set pending events"))?;
        // Tell the guest it was paused, so that its watchdogs do not take
        // the jump for a lockup. Fails (and does not matter) until the
        // guest has set up kvmclock.
        let _ = fd.kvmclock_ctrl();
        Ok(())
    }
}

/// A KVM_GET_XSAVE2 buffer of the size this VM's KVM uses.
fn xsave_buffer(vm: &Vm) -> Result<Xsave> {
    let size = vm.vm.check_extension_int(Cap::Xsave2);
    let base = size_of::<kvm_bindings::kvm_xsave>();
    let extra = usize::try_from(size)
        .ok()
        .filter(|&s| s >= base)
        .map(|s| (s - base).div_ceil(4))
        .ok_or_else(|| Error::Hypervisor {
        op: "size the XSAVE state",
        source: std::io::Error::other("KVM lacks KVM_CAP_XSAVE2 (host Linux 5.17 or later)"),
    })?;
    Xsave::new(extra).map_err(|e| Error::Hypervisor {
        op: "allocate the XSAVE state",
        source: std::io::Error::other(format!("{e:?}")),
    })
}

impl VcpuState {
    pub(crate) fn encode(&self, e: &mut Encoder) {
        e.raw(&self.mp_state);
        e.raw(&self.regs);
        e.raw(&self.sregs);
        e.bytes(&self.xsave_region);
        e.raw(self.xsave_extra.as_slice());
        e.raw(&self.xcrs);
        e.raw(&self.debugregs);
        e.raw(&self.lapic);
        e.count(self.msrs.len());
        for &(index, data) in &self.msrs {
            e.u32(index);
            e.u64(data);
        }
        e.raw(&self.events);
    }

    pub(crate) fn decode(d: &mut Decoder<'_>) -> Result<Self> {
        let mp_state = d.raw()?;
        let regs = d.raw()?;
        let sregs = d.raw()?;
        let xsave_region = d.bytes()?.to_vec();
        let extra = d.bytes()?;
        if extra.len() % 4 != 0 {
            return Err(Error::Snapshot("XSAVE state of odd length".into()));
        }
        let xsave_extra = extra.as_chunks::<4>().0.iter().map(|b| u32::from_le_bytes(*b)).collect();
        let xcrs = d.raw()?;
        let debugregs = d.raw()?;
        let lapic = d.raw()?;
        let n = d.count(12)?;
        let msrs = (0..n).map(|_| Ok((d.u32()?, d.u64()?))).collect::<Result<_>>()?;
        Ok(Self {
            mp_state,
            regs,
            sregs,
            xsave_region,
            xsave_extra,
            xcrs,
            debugregs,
            lapic,
            msrs,
            events: d.raw()?,
        })
    }
}
