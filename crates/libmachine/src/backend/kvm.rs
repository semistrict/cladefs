// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Linux KVM backend (the production backend).

pub(crate) mod kick;
pub(crate) mod memory;
mod snapshot;
mod suspend;

pub(crate) use snapshot::{VcpuState, VmState};

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use kvm_bindings::{
    CpuId, KVM_CPUID_FLAG_SIGNIFCANT_INDEX, KVM_IRQ_ROUTING_IRQCHIP, KVM_IRQ_ROUTING_MSI,
    KVM_IRQCHIP_IOAPIC, KVM_IRQCHIP_PIC_MASTER, KVM_IRQCHIP_PIC_SLAVE, KVM_MAX_CPUID_ENTRIES,
    KVM_PIT_SPEAKER_DUMMY, KvmIrqRouting, Msrs, kvm_cpuid_entry2, kvm_irq_routing_entry,
    kvm_irq_routing_irqchip, kvm_irq_routing_msi, kvm_lapic_state, kvm_msr_entry, kvm_pit_config,
    kvm_segment, kvm_userspace_memory_region,
};
use kvm_ioctls::{IoEventAddress, Kvm, NoDatamatch, VcpuExit, VcpuFd, VmFd};
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};
use vmm_sys_util::eventfd::EventFd;

use super::{Exit, Platform};
use crate::arch::x86_64::boot::{BootCpu, CpuMode};
use crate::arch::x86_64::cpuid::{self, Topology};
use crate::arch::x86_64::gdt::Segment;
use crate::arch::x86_64::layout::{self, KVM_IDENTITY_MAP_ADDR, KVM_TSS_ADDR};
use crate::cpu::{CpuModel, CpuidEntry};
use crate::devices::pci::{MsiMessage, PciHost};
use crate::error::{Error, Result, kvm_err};
use crate::machine::{ExitReason, Registers};
use crate::memory::{MemoryLayout, SlotTable};
use memory::MemorySlots;

const MSR_IA32_MISC_ENABLE: u32 = 0x1a0;
const MISC_ENABLE_FAST_STRING: u64 = 1;
const MSR_MTRR_DEF_TYPE: u32 = 0x2ff;
/// MTRRs enabled, default memory type write-back, as firmware leaves them.
const MTRR_DEF_TYPE_ENABLED_WB: u64 = (1 << 11) | 6;

const CR0_PE: u64 = 1;
const CR0_ET: u64 = 1 << 4;
const CR0_PG: u64 = 1 << 31;
const CR4_PAE: u64 = 1 << 5;
const EFER_LME: u64 = 1 << 8;
const EFER_LMA: u64 = 1 << 10;

const APIC_LVT0: usize = 0x350;
const APIC_LVT1: usize = 0x360;
const APIC_MODE_NMI: u32 = 0x4;
const APIC_MODE_EXTINT: u32 = 0x7;

/// A KVM VM: RAM in one memory slot per region of the layout, and device
/// memory (BARs included) in slots of their own, all numbered and checked by
/// one table ([`MemorySlots`]). Its vCPUs are [`Vcpu`]s, created with it.
pub(crate) struct Vm {
    // Field order matters for drop: the VM fd must close before the guest
    // RAM mapping it references is unmapped. Device memory slots share the
    // VM fd and may outlive this struct, and so may vCPUs; each vCPU holds
    // the RAM mapping too (`Vcpu::_mem`), so RAM is unmapped only once no
    // vCPU can run.
    vm: Arc<VmFd>,
    slots: MemorySlots,
    mem: GuestMemoryMmap,
    cpu_model: CpuModel,
    /// vCPU 0's CPUID table.
    cpuid: Vec<CpuidEntry>,
    cpu_fingerprint: String,
    /// The MSI routes, by GSI: KVM_SET_GSI_ROUTING replaces the whole
    /// table, so every change sends them all.
    msi_routes: Mutex<BTreeMap<u32, MsiMessage>>,
    /// The MSRs KVM saves (KVM_GET_MSR_INDEX_LIST), for snapshots.
    msr_indices: Vec<u32>,
    /// Whether a device maps a DAX window into the guest ([`Vm::map_dax_windows`]), set before
    /// the vCPUs' threads start, which then take part in the windows' accesses.
    dax_windows: std::sync::atomic::AtomicBool,
}

/// Where a new VM's RAM comes from.
pub(crate) enum Ram<'a> {
    /// Anonymous memory, reading as zeroes.
    Anonymous,
    /// A private (copy-on-write) mapping of a snapshot file: each RAM
    /// region, in address order, from its offset in `file`. The guest's
    /// writes never reach the file.
    Snapshot { file: &'a Arc<std::fs::File>, offsets: &'a [u64] },
}

/// One vCPU of a [`Vm`]. Every vCPU gets the same CPU model; only its
/// place in the topology (its APIC ID) differs.
pub(crate) struct Vcpu {
    // Field order matters for drop: the fd closes before the RAM mapping
    // this vCPU keeps alive is released.
    fd: VcpuFd,
    _mem: GuestMemoryMmap,
}

impl Vm {
    /// Creates the VM and its `vcpu_count` vCPUs. vCPU 0 is the boot CPU;
    /// on the PC platform the others wait, inside KVM_RUN, for the INIT and
    /// startup IPIs the guest sends through its local APIC.
    pub(crate) fn new(
        mem_size: usize,
        cpu_model: CpuModel,
        platform: Platform,
        vcpu_count: u32,
        ram: Ram<'_>,
    ) -> Result<(Self, Vec<Vcpu>)> {
        let kvm = Kvm::new().map_err(kvm_err("open /dev/kvm"))?;
        let max = kvm.get_max_vcpus();
        if vcpu_count as usize > max {
            return Err(Error::InvalidConfig(format!(
                "{vcpu_count} vCPUs; this host's KVM allows at most {max}"
            )));
        }
        let vm = kvm.create_vm().map_err(kvm_err("create VM"))?;

        if platform == Platform::Board {
            // KVM's own pages for Intel real-mode emulation, outside RAM.
            vm.set_tss_address(KVM_TSS_ADDR as usize).map_err(kvm_err("set TSS address"))?;
            vm.set_identity_map_address(KVM_IDENTITY_MAP_ADDR)
                .map_err(kvm_err("set identity map address"))?;
            // The in-kernel PIC, I/O APIC and local APICs, and the PIT.
            vm.create_irq_chip().map_err(kvm_err("create irqchip"))?;
            let pit = kvm_pit_config { flags: KVM_PIT_SPEAKER_DUMMY, ..Default::default() };
            vm.create_pit2(pit).map_err(kvm_err("create PIT"))?;
        }

        // RAM around the 32-bit MMIO hole: a region (and slot) below it,
        // and one from 4 GiB for the rest.
        let table =
            SlotTable::new(&layout::memory_layout(mem_size as u64), kvm.get_nr_memslots() as u32)?;
        let ranges: Vec<(GuestAddress, usize)> =
            table.ram_slots().map(|(_, r)| (GuestAddress(r.start), r.size as usize)).collect();
        let mem = guest_ram(&ranges, ram)?;
        for (slot, region) in table.ram_slots() {
            let host_addr = mem.get_host_address(GuestAddress(region.start))?;
            let mem_region = kvm_userspace_memory_region {
                slot,
                flags: 0,
                guest_phys_addr: region.start,
                memory_size: region.size,
                userspace_addr: host_addr as u64,
            };
            // SAFETY: the region is a live mapping owned by `mem`, which the
            // VM and every vCPU keep until their fds are closed.
            unsafe { vm.set_user_memory_region(mem_region) }
                .map_err(kvm_err("register guest memory"))?;
        }
        let vm = Arc::new(vm);
        let slots = MemorySlots::new(vm.clone(), table);

        let supported: Vec<CpuidEntry> = kvm
            .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
            .map_err(kvm_err("get supported CPUID"))?
            .as_slice()
            .iter()
            .map(from_kvm_entry)
            .collect();
        // The fingerprint names the CPU model, not the machine's shape: it
        // is taken over the table of a lone vCPU, so machines with any number
        // of vCPUs of one model share it.
        let alone = Topology { vcpu_id: 0, vcpu_count: 1 };
        let cpu_fingerprint =
            cpuid::fingerprint(&cpuid::build(&supported, cpu_model, alone)?, &feature_msrs(&kvm)?);

        let msr_indices = kvm
            .get_msr_index_list()
            .map_err(kvm_err("list the MSRs KVM saves"))?
            .as_slice()
            .to_vec();
        disable_pmu(&vm)?;
        let mut vcpus = Vec::with_capacity(vcpu_count as usize);
        let mut cpuid0 = Vec::new();
        for vcpu_id in 0..vcpu_count {
            let topology = Topology { vcpu_id, vcpu_count };
            let table = cpuid::build(&supported, cpu_model, topology)?;
            let fd = vm.create_vcpu(vcpu_id.into()).map_err(kvm_err("create vCPU"))?;
            let kvm_cpuid =
                CpuId::from_entries(&table.iter().map(to_kvm_entry).collect::<Vec<_>>())
                    .map_err(|_| Error::CpuModel("CPUID table too large".into()))?;
            fd.set_cpuid2(&kvm_cpuid).map_err(kvm_err("set CPUID"))?;
            if cpuid::amd_invariant_tsc(&table) {
                set_amd_tsc_freq_sel(&fd)?;
            }
            // Firmware wires the boot CPU's LINT pins; an AP's local APIC is
            // reset by the INIT that starts it, and the guest programs it.
            if platform == Platform::Board && vcpu_id == 0 {
                wire_lapic_lints(&fd)?;
            }
            if vcpu_id == 0 {
                cpuid0 = table;
            }
            vcpus.push(Vcpu { fd, _mem: mem.clone() });
        }

        let this = Self {
            vm,
            slots,
            mem,
            cpu_model,
            cpuid: cpuid0,
            cpu_fingerprint,
            msi_routes: Mutex::default(),
            msr_indices,
            dax_windows: std::sync::atomic::AtomicBool::new(false),
        };
        if let Some(boot) = vcpus.first_mut() {
            boot.set_real_mode_entry(0)?;
        }
        Ok((this, vcpus))
    }

    pub(crate) fn cpu_model(&self) -> CpuModel {
        self.cpu_model
    }

    pub(crate) fn cpu_fingerprint(&self) -> &str {
        &self.cpu_fingerprint
    }

    pub(crate) fn cpuid(&self) -> &[CpuidEntry] {
        &self.cpuid
    }

    pub(crate) fn guest_memory(&self) -> &GuestMemoryMmap {
        &self.mem
    }

    pub(crate) fn memory_slots(&self) -> &MemorySlots {
        &self.slots
    }

    /// A vCPU of its own past the guest's `vcpu_count`, never run, the guest's CPUID set on it,
    /// for KVM to map guest memory ahead of the guest's accesses with (`KVM_PRE_FAULT_MEMORY`,
    /// `virtio_extent::kvm`): its descriptor; none if KVM can't.
    pub(crate) fn prefault_vcpu(&self, vcpu_count: u32) -> Result<Option<std::os::fd::OwnedFd>> {
        use std::os::fd::{AsRawFd, FromRawFd};

        let cap = virtio_extent::kvm::KVM_CAP_PRE_FAULT_MEMORY;
        if self.vm.check_extension_raw(cap.into()) <= 0 {
            return Ok(None);
        }
        let fd = self
            .vm
            .create_vcpu(vcpu_count.into())
            .map_err(kvm_err("create a vCPU to prefault with"))?;
        let cpuid = CpuId::from_entries(&self.cpuid.iter().map(to_kvm_entry).collect::<Vec<_>>())
            .map_err(|_| Error::CpuModel("CPUID table too large".into()))?;
        fd.set_cpuid2(&cpuid).map_err(kvm_err("set the prefault vCPU's CPUID"))?;
        // Its own descriptor of the vCPU, which outlives `fd` (whose run structure goes with it),
        // close-on-exec as `fd` is: a child process holding it would keep the VM.
        // SAFETY: duplicates a descriptor `fd` owns; the result is checked.
        let dup = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if dup < 0 {
            return Err(Error::Hypervisor {
                op: "duplicate the prefault vCPU's descriptor",
                source: std::io::Error::last_os_error(),
            });
        }
        // SAFETY: A new descriptor, owned here alone.
        Ok(Some(unsafe { std::os::fd::OwnedFd::from_raw_fd(dup) }))
    }

    /// Says that a device maps a DAX window into the guest (an extent store device's), before the
    /// vCPUs' threads start: they then tell cladefs their accesses are the guest's, and retry
    /// those KVM fails as cladefs changes a window's memory (virtio_extent::vcpu).
    pub(crate) fn map_dax_windows(&self) {
        self.dax_windows.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether a device maps a DAX window into the guest ([`Vm::map_dax_windows`]).
    pub(crate) fn maps_dax_windows(&self) -> bool {
        self.dax_windows.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether KVM says where some accesses it fails with `EFAULT` failed
    /// (`KVM_CAP_MEMORY_FAULT_INFO`, `KVM_EXIT_MEMORY_FAULT`).
    pub(crate) fn memory_fault_info(&self) -> bool {
        self.vm.check_extension_raw(kvm_bindings::KVM_CAP_MEMORY_FAULT_INFO.into()) > 0
    }

    pub(crate) fn memory_layout(&self) -> MemoryLayout {
        self.slots.layout()
    }

    /// Has a write to `evt` raise the interrupt on `gsi` (in-kernel irqchip).
    pub(crate) fn register_irqfd(&self, evt: &EventFd, gsi: u32) -> Result<()> {
        self.vm.register_irqfd(evt, gsi).map_err(kvm_err("register irqfd"))
    }

    /// Sets interrupt line `gsi` (of the in-kernel irqchip) high or low.
    pub(crate) fn set_irq_line(&self, gsi: u32, high: bool) -> Result<()> {
        self.vm.set_irq_line(gsi, high).map_err(kvm_err("set an interrupt line"))
    }

    pub(crate) fn write_memory(&self, addr: u64, bytes: &[u8]) -> Result<()> {
        Ok(self.mem.write_slice(bytes, GuestAddress(addr))?)
    }

    pub(crate) fn read_memory(&self, addr: u64, buf: &mut [u8]) -> Result<()> {
        Ok(self.mem.read_slice(buf, GuestAddress(addr))?)
    }
}

impl Vcpu {
    pub(crate) fn clear_immediate_exit(&mut self) {
        self.fd.set_kvm_immediate_exit(0);
    }

    /// Finishes the I/O or MMIO access of the last exit without running the
    /// guest. KVM completes an access (an `in`'s value into its register,
    /// RIP past the instruction) only on the next KVM_RUN, and the state
    /// userspace reads before that is the state before it (api.rst,
    /// KVM_RUN: "userspace should ensure that the operation is completed
    /// before performing a live migration"); KVM_RUN with immediate_exit
    /// set completes it and returns at once. Done whenever a run ends, so
    /// that what is read between runs (registers, a snapshot) is what the
    /// guest resumes from.
    pub(crate) fn complete_pending(&mut self) {
        self.fd.set_kvm_immediate_exit(1);
        // Returns EINTR once the access is complete; nothing else happens.
        let _ = self.fd.run();
        self.fd.set_kvm_immediate_exit(0);
    }

    /// What the kick needs to reach this vCPU: its `kvm_run.immediate_exit`, for the kick
    /// signal handler. Valid as long as the vCPU exists (the `kvm_run` mapping does not move).
    pub(crate) fn kick_target(&mut self) -> kick::KickTarget {
        std::ptr::addr_of_mut!(self.fd.get_kvm_run().immediate_exit)
    }

    /// Puts the vCPU in the state a kernel's entry point expects.
    pub(crate) fn set_boot_cpu(&mut self, cpu: &BootCpu) -> Result<()> {
        let mut sregs = self.fd.get_sregs().map_err(kvm_err("get sregs"))?;
        sregs.gdt.base = cpu.gdt_base;
        sregs.gdt.limit = cpu.gdt_limit;
        sregs.idt.base = 0;
        sregs.idt.limit = 0;
        sregs.cs = kvm_seg(&cpu.code);
        let data = kvm_seg(&cpu.data);
        (sregs.ds, sregs.es, sregs.fs, sregs.gs, sregs.ss) = (data, data, data, data, data);
        sregs.tr = kvm_seg(&cpu.tss);
        match cpu.mode {
            CpuMode::Protected => {
                sregs.cr0 = CR0_PE | CR0_ET;
                sregs.cr3 = 0;
                sregs.cr4 = 0;
                sregs.efer = 0;
            }
            CpuMode::Long => {
                sregs.cr0 = CR0_PE | CR0_ET | CR0_PG;
                sregs.cr3 = crate::arch::x86_64::layout::PML4_ADDR;
                sregs.cr4 = CR4_PAE;
                sregs.efer = EFER_LME | EFER_LMA;
            }
        }
        self.fd.set_sregs(&sregs).map_err(kvm_err("set sregs"))?;

        let regs = kvm_bindings::kvm_regs {
            rip: cpu.rip,
            rsi: cpu.rsi,
            rbx: cpu.rbx,
            rsp: cpu.rsp,
            rbp: cpu.rsp,
            rflags: 0x2,
            ..Default::default()
        };
        self.fd.set_regs(&regs).map_err(kvm_err("set regs"))?;

        // What firmware leaves set: fast string operations, and MTRRs
        // enabled with write-back as the default memory type.
        let msrs = Msrs::from_entries(&[
            kvm_msr_entry {
                index: MSR_IA32_MISC_ENABLE,
                data: MISC_ENABLE_FAST_STRING,
                ..Default::default()
            },
            kvm_msr_entry {
                index: MSR_MTRR_DEF_TYPE,
                data: MTRR_DEF_TYPE_ENABLED_WB,
                ..Default::default()
            },
        ])
        .expect("two MSRs fit");
        let set = self.fd.set_msrs(&msrs).map_err(kvm_err("set MSRs"))?;
        if set != 2 {
            return Err(Error::Hypervisor {
                op: "set boot MSRs",
                source: std::io::Error::other(format!("KVM set {set} of 2")),
            });
        }
        Ok(())
    }

    pub(crate) fn set_real_mode_entry(&mut self, addr: u64) -> Result<()> {
        let mut sregs = self.fd.get_sregs().map_err(kvm_err("get sregs"))?;
        sregs.cs.base = 0;
        sregs.cs.selector = 0;
        self.fd.set_sregs(&sregs).map_err(kvm_err("set sregs"))?;

        let mut regs = self.fd.get_regs().map_err(kvm_err("get regs"))?;
        regs.rip = addr;
        regs.rflags = 0x2; // bit 1 is reserved and must be set
        self.fd.set_regs(&regs).map_err(kvm_err("set regs"))?;
        Ok(())
    }

    pub(crate) fn registers(&self) -> Result<Registers> {
        let r = self.fd.get_regs().map_err(kvm_err("get regs"))?;
        Ok(Registers {
            rax: r.rax,
            rbx: r.rbx,
            rcx: r.rcx,
            rdx: r.rdx,
            rsi: r.rsi,
            rdi: r.rdi,
            rsp: r.rsp,
            rbp: r.rbp,
            r8: r.r8,
            r9: r.r9,
            r10: r.r10,
            r11: r.r11,
            r12: r.r12,
            r13: r.r13,
            r14: r.r14,
            r15: r.r15,
            rip: r.rip,
            rflags: r.rflags,
        })
    }

    pub(crate) fn run_once(&mut self) -> Result<Exit<'_>> {
        // kvm-ioctls hands a port I/O exit over as its bytes alone; the
        // access size, which tells `outw` (one 2-byte access) from
        // `rep outsb` (two 1-byte ones), is read from kvm_run after the exit.
        let kvm_run: *const kvm_bindings::kvm_run = self.fd.get_kvm_run();
        let exit = match self.fd.run() {
            Ok(exit) => exit,
            // A signal, or immediate_exit set by a kick, stopped KVM_RUN
            // before or while it ran the guest. The caller clears the flag
            // and decides whether to re-enter.
            Err(e) if e.errno() == EINTR => return Ok(Exit::Interrupted),
            // An AP waiting for its startup IPI got it (or an INIT): KVM_RUN
            // returns without running the guest, to be entered again.
            Err(e) if e.errno() == EAGAIN => return Ok(Exit::Startup),
            // cladefs: an access failed, which the vCPU loop retries as cladefs changes a DAX
            // window's memory (virtio_extent::vcpu).
            Err(e) if e.errno() == libc::EFAULT => return Ok(Exit::AccessFailed(None)),
            Err(e) => return Err(kvm_err("run vCPU")(e)),
        };
        Ok(match exit {
            VcpuExit::IoOut(port, data) => Exit::IoOut {
                port,
                // SAFETY: kvm_run is the vCPU's mapping, valid while `self`
                // is; the exit's io fields are not the data bytes borrowed.
                size: unsafe { io_size(kvm_run) },
                data,
            },
            VcpuExit::IoIn(port, data) => Exit::IoIn {
                port,
                // SAFETY: as above.
                size: unsafe { io_size(kvm_run) },
                data,
            },
            VcpuExit::Hlt => Exit::Done(ExitReason::Halt),
            VcpuExit::Shutdown => Exit::Done(ExitReason::Shutdown),
            VcpuExit::MmioRead(addr, data) => Exit::MmioRead { addr, data },
            VcpuExit::MmioWrite(addr, data) => Exit::MmioWrite { addr, data },
            VcpuExit::FailEntry(reason, _cpu) => Exit::Done(ExitReason::FailEntry { reason }),
            VcpuExit::InternalError => Exit::Done(ExitReason::InternalError),
            // cladefs: where an access failed, as KVM says of some (KVM_CAP_MEMORY_FAULT_INFO).
            VcpuExit::MemoryFault { flags, gpa, size } => {
                Exit::AccessFailed(Some(virtio_extent::vcpu::MemoryFault { flags, gpa, size }))
            }
            other => Exit::Done(ExitReason::Other(format!("{other:?}"))),
        })
    }
}

/// Guest RAM for `ranges` (guest address, size), from `ram`.
fn guest_ram(ranges: &[(GuestAddress, usize)], ram: Ram<'_>) -> Result<GuestMemoryMmap> {
    let alloc = |e: &dyn std::fmt::Display| Error::MemoryAllocation(e.to_string());
    match ram {
        Ram::Anonymous => GuestMemoryMmap::<()>::from_ranges(ranges).map_err(|e| alloc(&e)),
        Ram::Snapshot { file, offsets } => {
            assert_eq!(ranges.len(), offsets.len(), "a file offset per RAM region");
            let regions = ranges
                .iter()
                .zip(offsets)
                .map(|(&(addr, size), &offset)| {
                    let region = vm_memory::MmapRegion::build(
                        Some(vm_memory::FileOffset::from_arc(file.clone(), offset)),
                        size,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_PRIVATE | libc::MAP_NORESERVE,
                    )
                    .map_err(|e| alloc(&e))?;
                    vm_memory::GuestRegionMmap::new(region, addr)
                        .ok_or_else(|| alloc(&"a RAM region past the address space"))
                })
                .collect::<Result<Vec<_>>>()?;
            GuestMemoryMmap::from_regions(regions).map_err(|e| alloc(&e))
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl PciHost for Vm {
    fn set_msi_route(&self, gsi: u32, message: MsiMessage) -> Result<()> {
        let mut routes = lock(&self.msi_routes);
        routes.insert(gsi, message);
        let mut entries = default_irqchip_routes();
        entries.extend(routes.iter().map(|(&gsi, &m)| msi_route(gsi, m)));
        let mut table = KvmIrqRouting::new(entries.len()).map_err(|e| Error::Hypervisor {
            op: "build the GSI routing table",
            source: std::io::Error::other(format!("{e:?}")),
        })?;
        table.as_mut_slice().copy_from_slice(&entries);
        self.vm.set_gsi_routing(&table).map_err(kvm_err("set GSI routing"))
    }

    fn assign_irqfd(&self, evt: &EventFd, gsi: u32) -> Result<()> {
        self.register_irqfd(evt, gsi)
    }

    fn deassign_irqfd(&self, evt: &EventFd, gsi: u32) -> Result<()> {
        self.vm.unregister_irqfd(evt, gsi).map_err(kvm_err("unregister irqfd"))
    }

    fn register_ioeventfd(&self, evt: &EventFd, addr: u64) -> Result<()> {
        self.vm
            .register_ioevent(evt, &IoEventAddress::Mmio(addr), NoDatamatch)
            .map_err(kvm_err("register ioeventfd"))
    }

    fn unregister_ioeventfd(&self, evt: &EventFd, addr: u64) -> Result<()> {
        self.vm
            .unregister_ioevent(evt, &IoEventAddress::Mmio(addr), NoDatamatch)
            .map_err(kvm_err("unregister ioeventfd"))
    }

    // BARs' memory goes through the same slot table as every other device
    // mapping: one owner numbers the slots, refuses overlaps, and records the
    // mappings in the layout snapshots use.
    unsafe fn map_memory(&self, guest_addr: u64, host_addr: *mut u8, len: u64) -> Result<u32> {
        // SAFETY: as the caller promises (PciHost::map_memory).
        unsafe { self.slots.map_raw(guest_addr, host_addr, len) }
    }

    fn unmap_memory(&self, slot: u32) -> Result<()> {
        self.slots.unmap_raw(slot)
    }
}

fn irqchip_route(gsi: u32, irqchip: u32, pin: u32) -> kvm_irq_routing_entry {
    let mut entry =
        kvm_irq_routing_entry { gsi, type_: KVM_IRQ_ROUTING_IRQCHIP, ..Default::default() };
    entry.u.irqchip = kvm_irq_routing_irqchip { irqchip, pin };
    entry
}

fn msi_route(gsi: u32, message: MsiMessage) -> kvm_irq_routing_entry {
    let mut entry = kvm_irq_routing_entry { gsi, type_: KVM_IRQ_ROUTING_MSI, ..Default::default() };
    entry.u.msi = kvm_irq_routing_msi {
        address_lo: message.address as u32,
        address_hi: (message.address >> 32) as u32,
        data: message.data,
        ..Default::default()
    };
    entry
}

/// The routes KVM sets up with its in-kernel irqchip, which a
/// KVM_SET_GSI_ROUTING table must repeat: GSIs 0-15 to both PICs' pins and
/// the I/O APIC's, 16-23 to the I/O APIC's.
fn default_irqchip_routes() -> Vec<kvm_irq_routing_entry> {
    let mut routes = Vec::new();
    for gsi in 0..24 {
        routes.push(irqchip_route(gsi, KVM_IRQCHIP_IOAPIC, gsi));
        if gsi < 16 {
            let pic = if gsi < 8 { KVM_IRQCHIP_PIC_MASTER } else { KVM_IRQCHIP_PIC_SLAVE };
            routes.push(irqchip_route(gsi, pic, gsi % 8));
        }
    }
    routes
}

/// Linux's EINTR and EAGAIN.
const EINTR: i32 = 4;
const EAGAIN: i32 = 11;

fn kvm_seg(s: &Segment) -> kvm_segment {
    kvm_segment {
        base: s.base,
        limit: s.limit,
        selector: s.selector,
        type_: s.type_,
        present: s.present.into(),
        dpl: s.dpl,
        db: s.db.into(),
        s: s.s.into(),
        l: s.l.into(),
        g: s.g.into(),
        avl: s.avl.into(),
        unusable: 0,
        padding: 0,
    }
}

const MSR_K7_HWCR: u32 = 0xc001_0015;
const HWCR_TSC_FREQ_SEL: u64 = 1 << 24;

/// Turns off KVM's virtual PMU for the whole VM (before any vCPU exists).
/// Hiding the PMU's CPUID bits is not enough: AMD's legacy performance
/// counters have no CPUID bit, and KVM emulates them unless told not to.
/// A sandbox gets no performance counters (they are a side channel and
/// snapshot state).
fn disable_pmu(vm: &VmFd) -> Result<()> {
    let supported = vm.check_extension_raw(kvm_bindings::KVM_CAP_PMU_CAPABILITY.into());
    if supported <= 0 || supported as u32 & kvm_bindings::KVM_PMU_CAP_DISABLE == 0 {
        return Err(Error::Hypervisor {
            op: "disable the guest PMU",
            source: std::io::Error::other(
                "KVM lacks KVM_CAP_PMU_CAPABILITY (host Linux 5.18 or later needed)",
            ),
        });
    }
    let mut cap = kvm_bindings::kvm_enable_cap {
        cap: kvm_bindings::KVM_CAP_PMU_CAPABILITY,
        ..Default::default()
    };
    cap.args[0] = kvm_bindings::KVM_PMU_CAP_DISABLE.into();
    vm.enable_cap(&cap).map_err(kvm_err("disable the guest PMU"))
}

/// Sets AMD's HWCR.TscFreqSel, as AMD firmware does: the TSC counts at the
/// P0 frequency. KVM accepts it from userspace since Linux 6.6; on older
/// hosts the bit stays clear and the guest only warns, so failure is not an
/// error.
fn set_amd_tsc_freq_sel(vcpu: &VcpuFd) -> Result<()> {
    let mut msrs =
        Msrs::from_entries(&[kvm_msr_entry { index: MSR_K7_HWCR, ..Default::default() }])
            .expect("one MSR fits");
    if vcpu.get_msrs(&mut msrs).map_err(kvm_err("read HWCR"))? != 1 {
        return Ok(());
    }
    msrs.as_mut_slice()[0].data |= HWCR_TSC_FREQ_SEL;
    // Ignored if KVM refuses the bit (older hosts): see above.
    let _ = vcpu.set_msrs(&msrs);
    Ok(())
}

/// Wires the local APIC's LINT0 to the PIC (ExtINT) and LINT1 to NMI, as
/// PC firmware does and the MADT's NMI entry says.
fn wire_lapic_lints(vcpu: &VcpuFd) -> Result<()> {
    let mut lapic: kvm_lapic_state = vcpu.get_lapic().map_err(kvm_err("get LAPIC"))?;
    for (reg, mode) in [(APIC_LVT0, APIC_MODE_EXTINT), (APIC_LVT1, APIC_MODE_NMI)] {
        let bytes: [u8; 4] = std::array::from_fn(|i| lapic.regs[reg + i] as u8);
        let value = (u32::from_le_bytes(bytes) & !0x700) | (mode << 8);
        for (i, b) in value.to_le_bytes().into_iter().enumerate() {
            lapic.regs[reg + i] = b as _;
        }
    }
    vcpu.set_lapic(&lapic).map_err(kvm_err("set LAPIC"))
}

fn from_kvm_entry(e: &kvm_cpuid_entry2) -> CpuidEntry {
    CpuidEntry {
        function: e.function,
        index: e.index,
        index_significant: e.flags & KVM_CPUID_FLAG_SIGNIFCANT_INDEX != 0,
        eax: e.eax,
        ebx: e.ebx,
        ecx: e.ecx,
        edx: e.edx,
    }
}

fn to_kvm_entry(e: &CpuidEntry) -> kvm_cpuid_entry2 {
    kvm_cpuid_entry2 {
        function: e.function,
        index: e.index,
        flags: if e.index_significant { KVM_CPUID_FLAG_SIGNIFCANT_INDEX } else { 0 },
        eax: e.eax,
        ebx: e.ebx,
        ecx: e.ecx,
        edx: e.edx,
        ..Default::default()
    }
}

/// The values of KVM's feature MSRs (e.g. IA32_ARCH_CAPABILITIES): the
/// CPU-model state that lives in MSRs rather than CPUID.
fn feature_msrs(kvm: &Kvm) -> Result<Vec<(u32, u64)>> {
    let indices = kvm.get_msr_feature_index_list().map_err(kvm_err("list feature MSRs"))?;
    let entries: Vec<kvm_msr_entry> = indices
        .as_slice()
        .iter()
        .map(|&index| kvm_msr_entry { index, ..Default::default() })
        .collect();
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let mut msrs = Msrs::from_entries(&entries)
        .map_err(|_| Error::CpuModel("too many feature MSRs".into()))?;
    let read = kvm.get_msrs(&mut msrs).map_err(kvm_err("read feature MSRs"))?;
    Ok(msrs.as_slice()[..read].iter().map(|e| (e.index, e.data)).collect())
}

/// The access size of the port I/O exit in `kvm_run` (1, 2 or 4).
///
/// # Safety
/// `kvm_run` must point to a vCPU's live `kvm_run` after a KVM_EXIT_IO.
unsafe fn io_size(kvm_run: *const kvm_bindings::kvm_run) -> u8 {
    // SAFETY: as the caller promises; the union's io member is the active
    // one after a KVM_EXIT_IO.
    let size = unsafe { std::ptr::addr_of!((*kvm_run).__bindgen_anon_1.io.size).read() };
    size.max(1)
}
