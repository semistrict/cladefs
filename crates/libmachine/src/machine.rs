// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::backend::{self, Platform};
use crate::cpu::CpuModel;
#[cfg(target_arch = "x86_64")]
use crate::cpu::CpuidEntry;
use crate::devices::bus::Bus;
use crate::devices::serial::{self, IrqLine};
use crate::error::{Error, Result};
use crate::exits::{ExitCounters, ExitCounts, ExitKind};
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use crate::net::host::{HeldStreams, NetSlot};
use crate::stop::{StopHandle, StopShared};
use crate::suspend::{Doorbell, Woken};
use crate::vcpus::{RunControl, VcpuLoop, Vcpus};
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::{Duration, Instant};

const PAGE_SIZE: usize = 4096;
/// The kernel command line libmachine starts every board's with: the console on the UART, and a
/// panic resets the machine (a sandbox never hangs on one).
pub(crate) const BASE_CMDLINE: &str = "console=ttyS0 panic=-1";
/// The UART's IIR bit that says no interrupt is pending (16550).
const UART_NO_INTERRUPT: u8 = 1;

/// How [`Machine::snapshot_with`] takes a snapshot.
#[derive(Debug, Clone, Copy, Default)]
pub struct SnapshotOptions<'a> {
    /// Where the guest's RAM goes: a file of its own at this path, as a Firecracker snapshot's
    /// memory file holds it ([`Snapshot::open_split`](crate::Snapshot::open_split) opens the
    /// two); None, the snapshot's file, after its state.
    pub ram: Option<&'a std::path::Path>,
    /// A snapshot for a live migration: its extent store devices' cells are handed over whole,
    /// with no version made of them and their guest's filesystem not frozen, refused while a
    /// cell has uncommitted open extents; else each device's cell gets a version, refused unless
    /// its guest's filesystem is frozen. Either is refused while a device's DAX window holds
    /// private pages.
    pub for_migration: bool,
    /// What the caller keeps with the snapshot, which [`Snapshot::caller_data`] gives back:
    /// nothing of libmachine's reads it.
    ///
    /// [`Snapshot::caller_data`]: crate::Snapshot::caller_data
    pub caller_data: &'a [u8],
}

/// Configuration for a new [`Machine`].
#[derive(Debug, Clone)]
pub struct MachineConfig {
    /// Guest RAM in bytes. Must be a non-zero multiple of 4 KiB, and at most
    /// 1 TiB - 1 GiB. Up to 3 GiB is mapped at guest physical address 0; the
    /// rest from 4 GiB, past the 32-bit MMIO hole ([`Machine::memory_layout`]).
    pub mem_size: usize,
    /// Which CPU features the guest sees.
    pub cpu_model: CpuModel,
    /// How many vCPUs, from 1 to [`MAX_VCPUS`](MachineConfig::MAX_VCPUS).
    /// Each runs on a host thread of its own. A bare machine
    /// ([`Machine::new`]) has exactly one.
    pub vcpus: u32,
    /// The guest's network (board machines, on KVM and Hypervisor.framework): a virtio-net card whose
    /// other end is a userspace network stack in this process, its
    /// connections made from the host where the policy allows
    /// ([`crate::net`]). None, the default, gives no network card at all.
    pub network: Option<crate::net::NetworkConfig>,
    /// How many of the most recent bytes of serial output the machine keeps
    /// for [`Machine::serial_output`]; older output is dropped (a console
    /// set with [`Machine::set_console`] still sees every byte). Bounds the
    /// memory a chatty guest costs the host.
    pub serial_capture_limit: usize,
    /// cladefs's extent daemon, which this process's machines share ([`crate::extent`]): the
    /// one every device of [`extent_stores`](MachineConfig::extent_stores) opens its cell on.
    pub extent_daemon: Option<Arc<crate::ExtentDaemon>>,
    /// The machine's extent store devices ([`Machine::boot_linux`]'s machines): a virtio-PCI
    /// device each, with a request queue per vCPU, its cell opened on
    /// [`extent_daemon`](MachineConfig::extent_daemon) as the machine is made. A machine with any
    /// is neither snapshotted nor suspended, nor restored (board 13139).
    pub extent_stores: Vec<crate::ExtentStore>,
    /// Gives the guest the CPU's virtualization extensions, EL2, so that it runs VMs of its own
    /// with KVM (nested virtualization): its kernel boots at EL2. Hypervisor.framework's on hosts
    /// that have it (Apple M3 and later, macOS 15 and later); refused elsewhere, KVM's included.
    pub nested_virtualization: bool,
    /// Gives the guest a virtio-balloon ([`Machine::balloon`], [`crate::Balloon`]): its free pages
    /// reported and given back to the host, and a target the host sets. A machine with one is
    /// neither snapshotted nor suspended yet (board 13994).
    pub balloon: bool,
    /// The machine's disks (virtio-blk, [`crate::Disk`]): `/dev/vda` on, in their order, after
    /// its extent store devices. A machine with any is neither snapshotted nor suspended.
    pub disks: Vec<crate::Disk>,
    /// The machine's persistent memory devices (virtio-pmem, [`crate::Pmem`]): `/dev/pmem0` on,
    /// in their order, each its file mapped into the guest's memory. The arm64 board's (on Linux
    /// Firecracker serves them); a machine with any is neither snapshotted nor suspended.
    pub pmems: Vec<crate::Pmem>,
}

impl MachineConfig {
    /// The most vCPUs a machine can have: the MADT describes them with
    /// xAPIC entries, whose APIC IDs are a byte, 0xFF is the broadcast ID,
    /// and the I/O APIC takes the ID after the last vCPU's. The host's KVM
    /// may allow fewer.
    pub const MAX_VCPUS: u32 = 254;
    /// The default [`serial_capture_limit`](MachineConfig::serial_capture_limit): 1 MiB.
    pub const DEFAULT_SERIAL_CAPTURE_LIMIT: usize = 1 << 20;
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self {
            mem_size: 16 << 20,
            cpu_model: CpuModel::default(),
            vcpus: 1,
            network: None,
            serial_capture_limit: Self::DEFAULT_SERIAL_CAPTURE_LIMIT,
            extent_daemon: None,
            extent_stores: Vec::new(),
            nested_virtualization: false,
            balloon: false,
            disks: Vec::new(),
            pmems: Vec::new(),
        }
    }
}

/// A Linux kernel to boot with [`Machine::boot_linux`].
#[derive(Debug, Clone)]
pub struct LinuxBoot {
    /// An ELF `vmlinux` with a PVH entry point (`CONFIG_PVH`), or a `bzImage`.
    pub kernel: std::path::PathBuf,
    /// An initramfs (cpio archive, optionally compressed).
    pub initrd: Option<std::path::PathBuf>,
    /// Appended to libmachine's base command line, `console=ttyS0 panic=-1`.
    pub cmdline: String,
}

/// Why [`Machine::run`] returned.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExitReason {
    /// The guest executed `hlt` (bare machines only: on a PC machine `hlt`
    /// waits for an interrupt).
    Halt,
    /// The guest powered the machine off (ACPI soft-off).
    Poweroff,
    /// The guest reset the machine (ACPI reset register), e.g. `reboot`, or a
    /// kernel panic with `panic=-1`.
    Reset,
    /// A [`StopHandle`] stopped the run. The machine keeps its state; `run`
    /// continues it.
    Stopped,
    /// [`Machine::run_for`]'s timeout elapsed. The machine keeps its state.
    TimedOut,
    /// The guest triple-faulted.
    Shutdown,
    /// The guest accessed an I/O port that has no device (bare machines only).
    ///
    /// For reads, calling [`Machine::run`] again resumes the guest with zeroes.
    UnhandledIo { port: u16, size: u8, write: bool },
    /// The guest accessed a physical address that is neither RAM nor a device
    /// (bare machines only).
    UnhandledMmio { addr: u64, size: u8, write: bool },
    /// The hypervisor could not run the vCPU (bad CPU state).
    FailEntry { reason: u64 },
    /// The hypervisor reported an internal error.
    InternalError,
    /// Any other exit the VMM does not handle yet.
    Other(String),
}

/// x86_64 general-purpose register state of a vCPU.
#[cfg(target_arch = "x86_64")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct Registers {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rsp: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags: u64,
}

/// A virtual machine.
///
/// Two kinds: [`Machine::new`] makes a bare machine (one vCPU, no interrupt
/// controller, only a UART, every other access reported to the caller) for
/// instruction-level tests; [`Machine::boot_linux`] makes a PC with
/// [`MachineConfig::vcpus`] vCPUs that boots a Linux kernel and behaves like
/// hardware (unclaimed ports and addresses read as all ones).
///
/// Each vCPU runs on a host thread of its own, which lives as long as the
/// machine (or until it is [suspended](Machine::suspend)) and waits while
/// the machine is not running.
///
/// Each `Machine` owns its own hypervisor VM, so many can exist in one process.
pub struct Machine {
    /// The hypervisor VM and what runs it, or what stands for them while
    /// the machine is suspended.
    body: Body,
    platform: Platform,
    mem_size: usize,
    cpu_model: CpuModel,
    cpu_fingerprint: String,
    vcpu_count: u32,
    #[cfg(target_arch = "x86_64")]
    cpuid: Vec<CpuidEntry>,
    // What follows outlives the VM: a suspension frees the VM and a wake
    // makes another, onto these, so that every handle a caller holds (stop
    // handle, vsock and agent, network, console, exit counts) carries on.
    stop: Arc<StopShared>,
    /// Each vCPU's exit counters, which its thread writes.
    exits: Vec<Arc<ExitCounters>>,
    /// Armed while the machine is suspended; its devices, or its watcher,
    /// ring it.
    doorbell: Arc<Doorbell>,
    /// The UART's output: what [`serial_output`](Machine::serial_output)
    /// returns, and the console.
    serial: serial::SharedOutput,
    /// The host's end of the vsock device (board machines).
    vsock: Option<crate::Vsock>,
    /// The host's side of the balloon, if the machine has one.
    balloon: Option<crate::Balloon>,
    /// The network's handle, if the machine has one.
    network: Option<crate::net::Network>,
    /// Its disks, which a snapshot names for a restore to open again.
    disks: Vec<crate::Disk>,
    /// How many extent store devices it has.
    extent_stores: usize,
    /// Whether its vCPUs have EL2 (nested virtualization), which no snapshot holds yet.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    nested: bool,
    /// Where the virtio-net device puts its host streams when suspended.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    net_slot: NetSlot,
    /// The file the guest's RAM is a private mapping of, if any: the
    /// snapshot's it was restored from, or the machine's own once it has
    /// been suspended (which also holds its state while it is).
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    ram_file: Option<crate::suspend::RamFile>,
    /// The memory slots of the VM the last suspension freed, retired.
    #[cfg(target_os = "linux")]
    retired_slots: Option<crate::MemorySlots>,
    /// The machine was woken from a suspension and has not run since: its
    /// first run resends the interrupts that may have been on their way
    /// ([`Live::resend_interrupts`]).
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    resend: bool,
    /// The caller put devices of its own on the PCI bus
    /// ([`boot_linux_with`](Machine::boot_linux_with)): no suspension could
    /// make them again.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    caller_devices: bool,
}

/// What a machine is made of now.
enum Body {
    /// Its hypervisor VM, vCPU threads and devices.
    Live(Live),
    /// Suspended: its state is in its file, and only what must outlive the
    /// VM is held ([`crate::suspend`]).
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    Frozen(Frozen),
    /// Its wake from a suspension failed, so it is gone: why.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    Lost(String),
}

/// A machine's hypervisor VM and what runs it.
struct Live {
    // Field order matters for drop: the vCPU threads stop and their vCPUs
    // close first; then the VM goes, so no device memory it maps (a shared
    // memory BAR) is unmapped while the VM exists.
    vcpus: Vcpus<MachineVcpu>,
    vm: Arc<backend::Vm>,
    /// The PCI bus and its virtio devices (board machines), also on `bus`.
    pci: Option<Arc<crate::devices::Pci>>,
    /// The devices vCPU exits reach.
    bus: Arc<Bus>,
}

impl Live {
    /// The machine's live parts from a VM, its vCPUs and its bus, with its
    /// vCPU threads started, stopped through `stop` and counting their
    /// exits in `exits` (one per vCPU).
    fn spawn(
        vm: backend::Vm,
        vcpus: Vec<backend::Vcpu>,
        bus: Bus,
        stop: &Arc<StopShared>,
        exits: &[Arc<ExitCounters>],
    ) -> Result<Self> {
        let vm = Arc::new(vm);
        let bus = Arc::new(bus);
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        let windows = vm.maps_dax_windows().then(|| vm.memory_fault_info());
        let mut runners = Vec::with_capacity(vcpus.len());
        for (vcpu, exits) in vcpus.into_iter().zip(exits) {
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            let index = runners.len() as u32;
            runners.push(MachineVcpu {
                vcpu,
                bus: bus.clone(),
                vm: vm.clone(),
                exits: exits.clone(),
                #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
                accesses: windows.map(|memory_fault_info| {
                    virtio_extent::vcpu::FailingAccesses::new(index, memory_fault_info)
                }),
            });
        }
        let vcpus = Vcpus::spawn(runners, stop.clone())?;
        // Each vCPU created on its thread in its order, as the framework hands out the GIC's
        // redistributors (`hvf::Vcpu::create`).
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        for i in 0..vcpus.len() {
            vcpus.on(i, Box::new(|v: &mut MachineVcpu| v.vcpu.create_now()))?;
        }
        Ok(Self { vcpus, vm, pci: None, bus })
    }
}

impl Live {
    /// Interrupts the guest again from every device that may have had an
    /// interrupt on its way when its state was saved, which went with the
    /// VM that was to take it (KVM injects an irqfd's interrupt from a
    /// workqueue when it cannot at once: always for the UART's I/O APIC
    /// route, sometimes for an MSI): every active queue, and the UART if
    /// it says an interrupt is pending. A spare one only makes the driver
    /// look.
    fn resend_interrupts(&self) {
        if let Some(pci) = &self.pci {
            pci.signal_all_queues();
        }
        let serial = self.bus.serial();
        if serial.state().interrupt_identification & UART_NO_INTERRUPT == 0 {
            use vm_superio::Trigger;
            let _ = serial.interrupt_evt().trigger();
        }
    }
}

/// What a suspended machine holds besides its file.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
struct Frozen {
    /// Stops first, before the streams it watches close.
    watcher: Option<crate::suspend::Watcher>,
    /// The host streams of the guest's connections.
    net: Option<HeldStreams>,
    /// The vsock's host end, detached: marked gone if the machine is
    /// dropped suspended. Taken when it wakes.
    vsock: Option<crate::Vsock>,
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
impl Drop for Frozen {
    fn drop(&mut self) {
        drop(self.watcher.take());
        if let Some(vsock) = self.vsock.take() {
            vsock.machine_gone();
        }
    }
}

/// What each vCPU thread runs: its vCPU, with exits dispatched to the bus
/// and counted.
struct MachineVcpu {
    vcpu: backend::Vcpu,
    bus: Arc<Bus>,
    vm: Arc<backend::Vm>,
    exits: Arc<ExitCounters>,
    /// Its accesses to an extent store device's DAX window failing as cladefs changes the
    /// window's memory, which it retries, and gives up on as they keep failing: of a machine
    /// with a window alone (`Vm::maps_dax_windows`). Without one, an access KVM fails is the
    /// hypervisor's error.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    accesses: Option<virtio_extent::vcpu::FailingAccesses>,
}

impl VcpuLoop for MachineVcpu {
    fn kick_target(&mut self) -> crate::backend::kick::KickTarget {
        self.vcpu.kick_target()
    }

    // cladefs: this thread's faults on a DAX window that copies on write are the guest's own
    // accesses, a store a store, unlike KVM's asynchronous page fault worker's (doc-13); until it
    // exits, whose ID the kernel then reuses.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn thread_started(&mut self) {
        if self.accesses.is_some() {
            cladefs::guest_thread_started();
        }
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn thread_ended(&mut self) {
        if self.accesses.is_some() {
            cladefs::guest_thread_ended();
        }
    }

    fn run(&mut self, ctl: &RunControl<'_>) -> Option<Result<ExitReason>> {
        let outcome = self.run_until_stopped(ctl);
        // The last exit's access is finished now, not at the next run, so
        // that the state between runs is the state the guest resumes from.
        self.vcpu.complete_pending();
        outcome
    }
}

impl MachineVcpu {
    fn run_until_stopped(&mut self, ctl: &RunControl<'_>) -> Option<Result<ExitReason>> {
        loop {
            // Kicks set immediate_exit after the condition they signal (a
            // stop request, the run ending), so clearing it before checking
            // those conditions never loses one; a kick landing after the
            // check makes the next KVM_RUN return at once.
            self.vcpu.clear_immediate_exit();
            if let ControlFlow::Break(outcome) = ctl.check() {
                return outcome;
            }
            let exit = match self.vcpu.run_once() {
                Ok(exit) => exit,
                Err(e) => {
                    self.exits.count(ExitKind::Other);
                    return Some(Err(e));
                }
            };
            // cladefs: an access KVM failed as cladefs changes a DAX window's memory is retried;
            // one that keeps failing (refused for good: past the window's private_max) stops the
            // machine, telling what it can of it (virtio_extent::vcpu).
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            let exit = match exit {
                backend::Exit::AccessFailed(fault) => {
                    let Some(accesses) = &mut self.accesses else {
                        self.exits.count(ExitKind::Other);
                        let source = std::io::Error::from_raw_os_error(libc::EFAULT);
                        return Some(Err(Error::Hypervisor { op: "run vCPU", source }));
                    };
                    self.exits.count(ExitKind::AccessRetried);
                    match accesses.failed(fault) {
                        Ok(()) => continue,
                        Err(why) => return Some(Err(Error::ExtentStore(why))),
                    }
                }
                exit => {
                    if let Some(accesses) = &mut self.accesses {
                        accesses.ran();
                    }
                    exit
                }
            };
            match self.bus.handle(exit, &self.vm, &self.exits) {
                Ok(None) => {}
                Ok(Some(reason)) => return Some(Ok(reason)),
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

/// The guest's network card for [`PcDevices::new`]: a new one, or that of
/// a suspended machine being woken, with the host streams it held.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
enum NetCard {
    New(Arc<crate::net::Shared>, NetSlot),
    Adopt(Arc<crate::net::Shared>, NetSlot, HeldStreams),
}

/// The devices of a PC besides the UART's model: the UART's interrupt, the
/// PCI bus with virtio-rng, virtio-vsock, virtio-net if there is a network,
/// and `extra`, in that order (so that every PC machine with the same
/// devices has the same BARs and GSIs, which a restore relies on). The
/// vsock's host end is new, or `vsock`, which the device's restore then
/// attaches it to.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
struct PcDevices {
    uart_irq: vmm_sys_util::eventfd::EventFd,
    vsock: crate::Vsock,
    pci: Arc<crate::devices::Pci>,
    /// What the devices ring when they have something for the guest.
    doorbell: Arc<Doorbell>,
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
impl PcDevices {
    fn new(
        vm: &backend::Vm,
        network: Option<NetCard>,
        extra: Vec<Box<dyn crate::devices::virtio::VirtioDevice>>,
        vsock: Option<crate::Vsock>,
        doorbell: Arc<Doorbell>,
        bars: crate::devices::pci::BarAllocator,
    ) -> Result<Self> {
        use crate::arch::x86_64::layout;
        use crate::devices::virtio::net::NetDevice;
        use crate::devices::virtio::vsock::{GUEST_CID, VsockDevice};
        use crate::devices::virtio::{VirtioDevice, rng::Rng};
        use vmm_sys_util::eventfd::{EFD_NONBLOCK, EventFd};

        let uart_irq = EventFd::new(EFD_NONBLOCK)
            .map_err(|source| Error::Hypervisor { op: "create the UART's eventfd", source })?;
        vm.register_irqfd(&uart_irq, layout::COM1_IRQ)?;
        let (vsock_device, vsock) = match vsock {
            Some(vsock) => (VsockDevice::adopt(&vsock)?, vsock),
            None => VsockDevice::new(GUEST_CID)?,
        };
        let mut all: Vec<Box<dyn VirtioDevice>> = vec![Box::new(Rng), Box::new(vsock_device)];
        match network {
            Some(NetCard::New(shared, slot)) => all.push(Box::new(NetDevice::new(shared, slot)?)),
            Some(NetCard::Adopt(shared, slot, streams)) => {
                all.push(Box::new(NetDevice::adopt(shared, slot, streams)?));
            }
            None => {}
        }
        all.extend(extra);
        let pci = Arc::new(crate::devices::Pci::new(all, vm.guest_memory(), vm, &doorbell, bars)?);
        Ok(Self { uart_irq, vsock, pci, doorbell })
    }
}

/// What a machine restored from saved state ([`Machine::rebuild`]) keeps
/// of the one that saved it: a suspended machine's, or new ones for a
/// clone.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
struct Kept<'a> {
    doorbell: Arc<Doorbell>,
    stop: &'a Arc<StopShared>,
    exits: &'a [Arc<ExitCounters>],
    serial: serial::SharedOutput,
    vsock: Option<crate::Vsock>,
    network: Option<NetCard>,
}

/// Writes a fresh random VM generation ID (VMGENID) into guest memory.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn write_generation_id(vm: &backend::Vm) -> Result<()> {
    let mut id = [0u8; 16];
    crate::sys::getrandom(&mut id);
    vm.write_memory(crate::arch::x86_64::layout::VMGENID_ADDR, &id)
}

/// Refuses nested virtualization where libmachine gives none: on KVM, and to a bare machine.
fn no_nested(config: &MachineConfig, what: &str) -> Result<()> {
    if config.nested_virtualization {
        return Err(Error::InvalidConfig(format!(
            "{what} has no nested virtualization, which machines on Hypervisor.framework alone \
             get; nested_virtualization must be false"
        )));
    }
    Ok(())
}

fn check_mem_size(mem_size: usize) -> Result<()> {
    if mem_size == 0 || !mem_size.is_multiple_of(PAGE_SIZE) {
        return Err(Error::InvalidConfig(format!(
            "mem_size must be a non-zero multiple of {PAGE_SIZE} bytes, got {mem_size}"
        )));
    }
    #[cfg(target_arch = "x86_64")]
    {
        use crate::arch::x86_64::layout::{MAX_RAM, MMIO64_START};
        if mem_size as u64 > MAX_RAM {
            return Err(Error::InvalidConfig(format!(
                "mem_size is {mem_size} bytes; RAM can be at most {MAX_RAM} bytes \
                 (1 TiB - 1 GiB), or it would reach the 64-bit MMIO window at {MMIO64_START:#x}"
            )));
        }
    }
    Ok(())
}

fn check_vcpus(vcpus: u32) -> Result<()> {
    if !(1..=MachineConfig::MAX_VCPUS).contains(&vcpus) {
        return Err(Error::InvalidConfig(format!(
            "vcpus must be from 1 to {}, got {vcpus}",
            MachineConfig::MAX_VCPUS
        )));
    }
    Ok(())
}

/// The daemon `config`'s extent store devices open their cells on, if it has any devices: refused
/// if it names none, or two devices of one name.
fn extent_daemon(config: &MachineConfig) -> Result<Option<&Arc<crate::ExtentDaemon>>> {
    if config.extent_stores.is_empty() {
        return Ok(None);
    }
    let Some(daemon) = &config.extent_daemon else {
        return Err(Error::InvalidConfig(
            "extent store devices need an extent daemon: extent_daemon is None".into(),
        ));
    };
    for (i, store) in config.extent_stores.iter().enumerate() {
        if config.extent_stores[..i].iter().any(|other| other.id == store.id) {
            return Err(Error::InvalidConfig(format!(
                "two extent store devices are named {}",
                store.id
            )));
        }
    }
    Ok(Some(daemon))
}

/// The disks of `config`'s ([`MachineConfig::disks`]), a virtio-blk device each.
fn disk_devices(
    config: &MachineConfig,
) -> Result<Vec<Box<dyn crate::devices::virtio::VirtioDevice>>> {
    (config.disks.iter())
        .map(|disk| {
            let device = crate::devices::virtio::blk::Block::new(disk)?;
            Ok(Box::new(device) as Box<dyn crate::devices::virtio::VirtioDevice>)
        })
        .collect()
}

/// The persistent memory devices of `config`'s ([`MachineConfig::pmems`]), a virtio-pmem device
/// each, their memory mapped into `vm`'s guest one after another from the board's pmem region.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn pmem_devices(
    config: &MachineConfig,
    vm: &backend::Vm,
) -> Result<Vec<Box<dyn crate::devices::virtio::VirtioDevice>>> {
    use crate::arch::aarch64::layout::{PMEM_ADDR, PMEM_SIZE};
    use crate::devices::virtio::pmem::PmemDevice;

    let mut next = PMEM_ADDR;
    let mut devices: Vec<Box<dyn crate::devices::virtio::VirtioDevice>> = Vec::new();
    for pmem in &config.pmems {
        let device = PmemDevice::new(pmem, vm, next)?;
        next += device.size();
        if next > PMEM_ADDR + PMEM_SIZE {
            return Err(Error::InvalidConfig(format!(
                "pmem devices past the board's {PMEM_SIZE:#x} bytes of persistent memory"
            )));
        }
        devices.push(Box::new(device));
    }
    Ok(devices)
}

/// The balloon of `config`'s, if it asks for one ([`MachineConfig::balloon`]), added to `devices`,
/// and its host handle.
fn balloon_device(
    config: &MachineConfig,
    vm: &backend::Vm,
    devices: &mut Vec<Box<dyn crate::devices::virtio::VirtioDevice>>,
) -> Result<Option<crate::Balloon>> {
    if !config.balloon {
        return Ok(None);
    }
    #[cfg(target_os = "linux")]
    let free = {
        let _ = vm;
        crate::devices::virtio::balloon::madvise_ram()
    };
    #[cfg(target_os = "macos")]
    let free: crate::devices::virtio::balloon::FreeRam = {
        let shared = vm.shared().clone();
        Box::new(move |guest_addr, host, len| {
            // SAFETY: the balloon gives whole host pages of this VM's guest RAM, given up.
            unsafe { shared.discard_ram(guest_addr, host, len) };
            Ok(())
        })
    };
    let (device, balloon) = crate::devices::virtio::balloon::Device::new(free)?;
    devices.push(Box::new(device));
    Ok(Some(balloon))
}

/// The extent store devices of `config`'s ([`MachineConfig::extent_stores`]), with a request
/// queue per vCPU, their DAX windows' BARs from `bars`, stopping the machine through `stop` as one
/// can't go on: on Hypervisor.framework, the windows' pages mapped by libmachine (board 13069).
/// Restored from a snapshot, each made from what it `saved` of itself, in order.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn extent_devices(
    config: &MachineConfig,
    vm: &backend::Vm,
    bars: &mut crate::devices::pci::BarAllocator,
    stop: &Arc<StopShared>,
    saved: &[virtio_extent::persist::State],
) -> Result<Vec<Box<dyn crate::devices::virtio::VirtioDevice>>> {
    use crate::devices::virtio::extent::{ExtentDevice, ExtentVm, Failing};

    let Some(daemon) = extent_daemon(config)? else {
        return Ok(Vec::new());
    };
    let queues = u16::try_from(config.vcpus).expect("check_vcpus bounds them");
    config
        .extent_stores
        .iter()
        .enumerate()
        .map(|(i, store)| {
            let device = ExtentDevice::new(
                store,
                daemon,
                queues,
                bars,
                |at| Arc::new(ExtentVm { vm: vm.shared().clone(), at }),
                None,
                Failing::new(stop),
                saved.get(i),
            )?;
            Ok(Box::new(device) as Box<dyn crate::devices::virtio::VirtioDevice>)
        })
        .collect()
}

/// The extent store devices of `config`'s ([`MachineConfig::extent_stores`]), on `vm`, with a
/// request queue per vCPU, their DAX windows' BARs from `bars`, stopping the machine through
/// `stop` as one can't go on.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn extent_devices(
    config: &MachineConfig,
    vm: &backend::Vm,
    bars: &mut crate::devices::pci::BarAllocator,
    stop: &Arc<StopShared>,
) -> Result<Vec<Box<dyn crate::devices::virtio::VirtioDevice>>> {
    use crate::devices::virtio::extent::{ExtentDevice, ExtentVm, Failing};

    let Some(daemon) = extent_daemon(config)? else {
        return Ok(Vec::new());
    };
    // KVM's prefault of the windows' pages, on a vCPU of its own after the guest's, if any
    // device has a window and KVM can.
    let windows = config.extent_stores.iter().any(|store| store.dax_window_size.is_some());
    if windows {
        vm.map_dax_windows();
    }
    let prefault: Option<Arc<dyn virtio_extent::PrefaultGuestMemory>> = match windows {
        true => vm
            .prefault_vcpu(config.vcpus)?
            .map(|vcpu| Arc::new(virtio_extent::kvm::KvmPrefault::new(vcpu)) as _),
        false => None,
    };
    let queues = u16::try_from(config.vcpus).expect("check_vcpus bounds them");
    config
        .extent_stores
        .iter()
        .map(|store| {
            let slots = vm.memory_slots();
            let device = ExtentDevice::new(
                store,
                daemon,
                queues,
                bars,
                |at| Arc::new(ExtentVm { slots: slots.clone(), at }),
                prefault.clone(),
                Failing::new(stop),
                // KVM restores no extent store device yet.
                None,
            )?;
            Ok(Box::new(device) as Box<dyn crate::devices::virtio::VirtioDevice>)
        })
        .collect()
}

/// How `options` have the extent store devices' snapshots taken.
fn snapshot_mode(options: &SnapshotOptions<'_>) -> virtio_extent::SnapshotMode {
    match options.for_migration {
        true => virtio_extent::SnapshotMode::Migration,
        false => virtio_extent::SnapshotMode::Versioned,
    }
}

/// Where `options` put a snapshot's RAM.
fn ram_at<'a>(options: &SnapshotOptions<'a>) -> crate::snapshot::RamAt<'a> {
    match options.ram {
        Some(path) => crate::snapshot::RamAt::File(path),
        None => crate::snapshot::RamAt::Inline,
    }
}

/// The error for anything asked of a machine whose wake failed.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn lost(why: &str) -> Error {
    Error::GuestGone(format!("the machine is gone: {why}"))
}

impl Machine {
    /// Creates a bare VM with zeroed RAM and one vCPU reset to 16-bit real
    /// mode at address 0. `config.vcpus` must be 1: a bare machine has no
    /// local APICs, so no way to start a second vCPU.
    pub fn new(config: MachineConfig) -> Result<Self> {
        check_mem_size(config.mem_size)?;
        no_nested(&config, "a bare machine")?;
        if config.network.is_some() {
            return Err(Error::InvalidConfig(
                "a bare machine has no network card; network must be None".into(),
            ));
        }
        if !config.extent_stores.is_empty() {
            return Err(Error::InvalidConfig(
                "a bare machine has no PCI bus; extent_stores must be empty".into(),
            ));
        }
        if config.vcpus != 1 {
            return Err(Error::InvalidConfig(format!(
                "a bare machine has exactly one vCPU, got vcpus = {}",
                config.vcpus
            )));
        }
        let (vm, vcpus) = backend::Vm::new(
            config.mem_size,
            config.cpu_model,
            Platform::Bare,
            1,
            backend::Ram::Anonymous,
        )?;
        let output = serial::output(config.serial_capture_limit);
        let bus = Bus::bare(serial::new(IrqLine::None, output.clone()));
        let doorbell = Arc::<Doorbell>::default();
        let stop = Arc::new(StopShared::with_doorbell(doorbell.clone()));
        Self::assemble(vm, vcpus, bus, Platform::Bare, config.mem_size, stop, doorbell, output)
    }

    /// The machine from its parts, with its vCPU threads started, stopped through `stop`, whose
    /// suspensions wait on `doorbell`.
    #[allow(clippy::too_many_arguments)]
    fn assemble(
        vm: backend::Vm,
        vcpus: Vec<backend::Vcpu>,
        bus: Bus,
        platform: Platform,
        mem_size: usize,
        stop: Arc<StopShared>,
        doorbell: Arc<Doorbell>,
        serial: serial::SharedOutput,
    ) -> Result<Self> {
        let exits: Vec<Arc<ExitCounters>> = vcpus.iter().map(|_| Arc::default()).collect();
        let live = Live::spawn(vm, vcpus, bus, &stop, &exits)?;
        Ok(Self::from_live(live, platform, mem_size, stop, exits, doorbell, serial))
    }

    /// The machine around `live` and what outlives it.
    fn from_live(
        live: Live,
        platform: Platform,
        mem_size: usize,
        stop: Arc<StopShared>,
        exits: Vec<Arc<ExitCounters>>,
        doorbell: Arc<Doorbell>,
        serial: serial::SharedOutput,
    ) -> Self {
        Self {
            platform,
            mem_size,
            cpu_model: live.vm.cpu_model(),
            cpu_fingerprint: live.vm.cpu_fingerprint().to_owned(),
            vcpu_count: live.vcpus.len() as u32,
            #[cfg(target_arch = "x86_64")]
            cpuid: live.vm.cpuid().to_vec(),
            body: Body::Live(live),
            stop,
            exits,
            doorbell,
            serial,
            vsock: None,
            balloon: None,
            network: None,
            disks: Vec::new(),
            extent_stores: 0,
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            nested: false,
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            net_slot: NetSlot::default(),
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            ram_file: None,
            #[cfg(target_os = "linux")]
            retired_slots: None,
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            resend: false,
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            caller_devices: false,
        }
    }

    /// The live parts, or why there are none.
    fn live(&self) -> Result<&Live> {
        match &self.body {
            Body::Live(live) => Ok(live),
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            Body::Frozen(_) => Err(Error::InvalidConfig(
                "the machine is suspended: its vCPUs and devices are in its file until it runs \
                 again"
                    .into(),
            )),
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            Body::Lost(why) => Err(lost(why)),
        }
    }

    /// The live parts, restoring them first if the machine is suspended
    /// (it stays suspended until it runs: its devices ring the doorbell).
    fn live_mut(&mut self) -> Result<&mut Live> {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        self.thaw()?;
        match &mut self.body {
            Body::Live(live) => Ok(live),
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            _ => unreachable!("thawed"),
        }
    }

    /// Creates a PC and loads `boot`'s kernel into it, ready to [`run`]:
    /// `config.vcpus` vCPUs, in-kernel interrupt controllers and timer, ACPI
    /// tables (CPUs, power off and reset), a 16550 UART on COM1 as the
    /// console, and a PCI bus with a virtio entropy device (virtio-rng) and a
    /// virtio-vsock device (guest CID 3, the host's end in this process:
    /// [`vsock`](Machine::vsock)), and a virtio-net card if
    /// [`MachineConfig::network`] asks for one, the guest's address then
    /// configured on the kernel command line (`ip=`, before `boot`'s).
    /// vCPU 0 starts at the kernel's entry point;
    /// the others wait for the kernel to start them (INIT and startup IPIs).
    ///
    /// RAM past 3 GiB continues at 4 GiB, and the kernel finds it in the
    /// e820 table (bzImage) or the PVH memory map.
    ///
    /// [`run`]: Machine::run
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    pub fn boot_linux(config: MachineConfig, boot: &LinuxBoot) -> Result<Self> {
        Self::boot_linux_with(config, boot, Vec::new())
    }

    /// [`boot_linux`](Machine::boot_linux), with `devices` on the PCI bus
    /// after the virtio-rng, the virtio-vsock and the virtio-net if any
    /// (from device number 3, or 4 with a network). Such a machine cannot
    /// be suspended: only the caller could make its devices again.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    pub(crate) fn boot_linux_with(
        config: MachineConfig,
        boot: &LinuxBoot,
        devices: Vec<Box<dyn crate::devices::virtio::VirtioDevice>>,
    ) -> Result<Self> {
        use crate::arch::x86_64::{acpi, boot as linux, layout};

        check_mem_size(config.mem_size)?;
        check_vcpus(config.vcpus)?;
        no_nested(&config, "a machine on KVM")?;
        let (vm, mut vcpus) = backend::Vm::new(
            config.mem_size,
            config.cpu_model,
            Platform::Board,
            config.vcpus,
            backend::Ram::Anonymous,
        )?;
        for (addr, table) in acpi::tables(config.vcpus) {
            vm.write_memory(addr, &table)?;
        }
        write_generation_id(&vm)?;
        let network =
            config.network.as_ref().map(|n| crate::net::Network::new(n.clone())).transpose()?;
        // Ours first, so that the caller's can override it.
        let cmdline = match network {
            Some(_) => format!("{} {}", crate::net::kernel_ip_param(), boot.cmdline),
            None => boot.cmdline.clone(),
        };
        let kernel = linux::Kernel {
            kernel: &boot.kernel,
            initrd: boot.initrd.as_deref(),
            cmdline: &cmdline,
        };
        let cpu =
            linux::load(vm.guest_memory(), config.mem_size as u64, &kernel, layout::ACPI_ADDR)?;
        vcpus[0].set_boot_cpu(&cpu)?;

        let caller_devices = !devices.is_empty();
        let net_slot = NetSlot::default();
        let card = network.as_ref().map(|n| NetCard::New(n.shared(), net_slot.clone()));
        let doorbell = Arc::<Doorbell>::default();
        let stop = Arc::new(StopShared::with_doorbell(doorbell.clone()));
        // The extent store devices first, before the caller's: their DAX windows' BARs before
        // any transport's.
        let mut bars = crate::devices::pci::BarAllocator::new();
        if !config.pmems.is_empty() {
            return Err(Error::InvalidConfig(
                "pmem devices are the arm64 board's (on Linux, Firecracker serves them); pmems \
                 must be empty"
                    .into(),
            ));
        }
        let mut all = extent_devices(&config, &vm, &mut bars, &stop)?;
        all.extend(disk_devices(&config)?);
        let balloon = balloon_device(&config, &vm, &mut all)?;
        all.extend(devices);
        let pc = PcDevices::new(&vm, card, all, None, doorbell, bars)?;
        let output = serial::output(config.serial_capture_limit);
        let serial = serial::new(IrqLine::EventFd(pc.uart_irq), output.clone());
        let bus = Bus::board(serial, pc.pci.clone());
        let (mem_size, doorbell) = (config.mem_size, pc.doorbell);
        let mut machine =
            Self::assemble(vm, vcpus, bus, Platform::Board, mem_size, stop, doorbell, output)?;
        if let Body::Live(live) = &mut machine.body {
            live.pci = Some(pc.pci);
        }
        machine.vsock = Some(pc.vsock);
        machine.balloon = balloon;
        machine.network = network;
        machine.net_slot = net_slot;
        machine.caller_devices = caller_devices;
        machine.disks = config.disks;
        machine.extent_stores = config.extent_stores.len();
        Ok(machine)
    }

    /// Creates the arm64 board and loads `boot`'s kernel (an arm64 `Image`) into it, ready to
    /// [`run`]: `config.vcpus` vCPUs on Hypervisor.framework, its GICv3 and timers, PSCI, a 16550
    /// as the console, and a PCI bus with a virtio entropy device (virtio-rng) and a virtio-vsock
    /// device (guest CID 3, the host's end in this process: [`vsock`](Machine::vsock)); the
    /// device tree describes them all. vCPU 0 starts at the kernel's entry point; the others wait
    /// for the kernel to power them on (PSCI CPU_ON).
    ///
    /// A process has one Hypervisor.framework VM: while this machine exists, making another
    /// fails. No network yet (board 13063).
    ///
    /// [`run`]: Machine::run
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub fn boot_linux(config: MachineConfig, boot: &LinuxBoot) -> Result<Self> {
        use crate::arch::aarch64::{boot as linux, fdt, layout};
        use crate::devices::virtio::vsock::{GUEST_CID, VsockDevice};
        use crate::devices::virtio::{VirtioDevice, rng::Rng};

        check_mem_size(config.mem_size)?;
        check_vcpus(config.vcpus)?;
        let network =
            config.network.as_ref().map(|n| crate::net::Network::new(n.clone())).transpose()?;
        let (vm, mut vcpus) = backend::Vm::with_el2(
            config.mem_size,
            config.cpu_model,
            Platform::Board,
            config.vcpus,
            backend::Ram::Anonymous,
            config.nested_virtualization,
        )?;
        let loaded = linux::load(
            vm.guest_memory(),
            config.mem_size as u64,
            &boot.kernel,
            boot.initrd.as_deref(),
        )?;
        // Ours first, so that the caller's can override it.
        let ours = match network {
            Some(_) => format!("{BASE_CMDLINE} {}", crate::net::kernel_ip_param()),
            None => BASE_CMDLINE.to_owned(),
        };
        let cmdline = match boot.cmdline.as_str() {
            "" => ours,
            theirs => format!("{ours} {theirs}"),
        };
        let mut rng_seed = [0u8; 64];
        crate::sys::getrandom(&mut rng_seed);
        let gic = vm.gic();
        let tree = fdt::device_tree(&fdt::Board {
            vcpus: config.vcpus,
            mem_size: config.mem_size as u64,
            cmdline: &cmdline,
            initrd: loaded.initrd,
            gic_dist_size: gic.dist_size,
            gic_msi_size: gic.msi_size,
            rng_seed,
            el2: config.nested_virtualization,
        })?;
        if tree.len() as u64 > linux::FDT_MAX {
            return Err(Error::InvalidConfig(format!(
                "the device tree takes {} bytes, past the {} its place holds",
                tree.len(),
                linux::FDT_MAX
            )));
        }
        vm.write_memory(loaded.fdt, &tree)?;
        vcpus[0].set_boot_cpu(&linux::boot_cpu(&loaded))?;

        let doorbell: Arc<Doorbell> = Arc::default();
        let stop = Arc::new(StopShared::with_doorbell(doorbell.clone()));
        // The extent store devices first, as on the PC.
        let mut bars = crate::devices::pci::BarAllocator::new();
        let mut devices = extent_devices(&config, &vm, &mut bars, &stop, &[])?;
        devices.extend(disk_devices(&config)?);
        devices.extend(pmem_devices(&config, &vm)?);
        let balloon = balloon_device(&config, &vm, &mut devices)?;
        let (vsock_device, vsock) = VsockDevice::new(GUEST_CID)?;
        devices.extend([Box::new(Rng) as Box<dyn VirtioDevice>, Box::new(vsock_device)]);
        if let Some(network) = &network {
            use crate::devices::virtio::net::NetDevice;
            let slot = crate::net::host::NetSlot::default();
            devices.push(Box::new(NetDevice::new(network.shared(), slot)?));
        }
        let pci =
            Arc::new(crate::devices::Pci::new(devices, vm.guest_memory(), &vm, &doorbell, bars)?);
        let output = serial::output(config.serial_capture_limit);
        let uart = IrqLine::Spi(vm.shared().clone(), layout::UART_SPI);
        let bus = Bus::board(serial::new(uart, output.clone()), pci.clone());
        let mut machine = Self::assemble(
            vm,
            vcpus,
            bus,
            Platform::Board,
            config.mem_size,
            stop,
            doorbell,
            output,
        )?;
        machine.live_mut()?.pci = Some(pci);
        machine.balloon = balloon;
        machine.network = network;
        machine.vsock = Some(vsock);
        machine.disks = config.disks;
        machine.extent_stores = config.extent_stores.len();
        machine.nested = config.nested_virtualization;
        Ok(machine)
    }

    /// [`snapshot`](Machine::snapshot), as `options` say: its RAM in a file of its own, for a
    /// live migration, with the caller's data. On Hypervisor.framework: the arm64 board, its
    /// balloon, disks (their files are the host's: a restore opens them again) and extent store
    /// devices with it, not one with a network or nested virtualization.
    ///
    /// Each extent store device's snapshot is taken first (doc-4): unless `for_migration`, its
    /// guest's filesystem must be frozen (`fsfreeze`, which `VIRTIO_EXTENT_T_FREEZE` tells the
    /// device of), and its cell gets a version, which a snapshot failing after drops. The guest's
    /// virtual counter goes on from the snapshot's count in a restore.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub fn snapshot_with(
        &mut self,
        path: impl AsRef<std::path::Path>,
        options: &SnapshotOptions<'_>,
    ) -> Result<()> {
        if self.platform != Platform::Board {
            return Err(Error::Snapshot(
                "only board machines (Machine::boot_linux) can be snapshotted".into(),
            ));
        }
        if self.network.is_some() {
            return Err(Error::Snapshot(
                "a machine with a network cannot be snapshotted: its connections are host \
                 sockets that no restored machine could share"
                    .into(),
            ));
        }
        if self.nested {
            return Err(Error::Snapshot(
                "a machine with nested virtualization is not snapshotted yet: its EL2 state is \
                 not saved"
                    .into(),
            ));
        }
        let live = self.live()?;
        let pci = live.pci.as_ref().expect("a board machine has a PCI bus");
        // The device thread waits until the state and RAM are written.
        let mut quiesced = pci.quiesce();
        let taken = (|| {
            let extent_stores = quiesced.take_snapshots(snapshot_mode(options))?;
            let mut e = crate::snapshot::codec::Encoder::default();
            self.encode_cpus(live, &mut e)?;
            quiesced.save(&mut e)?;
            let mut header = self.header(live.vm.memory_layout());
            header.extent_stores = extent_stores;
            header.caller_data = options.caller_data.to_vec();
            let ram = ram_at(options);
            crate::snapshot::write(path.as_ref(), &header, &e.finish(), live.vm.guest_memory(), ram)
        })();
        quiesced.snapshots_done(taken.is_ok());
        taken
    }

    /// The GIC's state and the host's clock, every vCPU's (read on its own thread), the UART's
    /// and the RTC's, between runs.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn encode_cpus(&self, live: &Live, e: &mut crate::snapshot::codec::Encoder) -> Result<()> {
        type Job = Box<dyn FnOnce(&mut MachineVcpu) -> Result<backend::VcpuState> + Send>;
        let jobs = (0..live.vcpus.len())
            .map(|_| Box::new(|v: &mut MachineVcpu| v.vcpu.save_state()) as Job)
            .collect();
        let vcpus = live.vcpus.on_each(jobs)?;
        backend::VmState::save()?.encode(e);
        e.count(vcpus.len());
        vcpus.iter().for_each(|state| state.encode(e));
        serial::save(&live.bus.serial(), e);
        live.bus.rtc().save(e);
        Ok(())
    }

    /// A new machine from `snapshot`, ready to [`run`](Machine::run) from where the snapshot's
    /// machine stopped, on Hypervisor.framework. `config` says what machine to build, and must be
    /// the snapshot's ([`Snapshot::config`](crate::Snapshot::config)) but for where its devices
    /// are on the host: its disks' files, its extent store devices' cells (each forked from the
    /// snapshot's version of its cell, or the cell itself reopened, as virtio_extent opens it),
    /// with an extent daemon for them. Its RAM is a private copy-on-write mapping of the
    /// snapshot's; its vsock connections from before the snapshot are reset; the guest's virtual
    /// counter goes on from where it was, so no time passes for it while it was in the file.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub fn restore(snapshot: &crate::Snapshot, config: MachineConfig) -> Result<Self> {
        Self::restore_with(snapshot, config, None)
    }

    /// [`restore`](Machine::restore), onto `vsock` if given: as KVM's.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub(crate) fn restore_with(
        snapshot: &crate::Snapshot,
        config: MachineConfig,
        vsock: Option<crate::Vsock>,
    ) -> Result<Self> {
        use crate::arch::aarch64::layout;
        use crate::devices::virtio::vsock::{GUEST_CID, VsockDevice};
        use crate::devices::virtio::{VirtioDevice, rng::Rng};

        check_mem_size(config.mem_size)?;
        check_vcpus(config.vcpus)?;
        if config.nested_virtualization {
            return Err(Error::Snapshot(
                "no machine with nested virtualization is snapshotted, nor restored".into(),
            ));
        }
        snapshot.check(&config, &layout::memory_layout(config.mem_size as u64))?;
        let (vm, vcpus) = backend::Vm::new(
            config.mem_size,
            config.cpu_model,
            Platform::Board,
            config.vcpus,
            backend::Ram::Snapshot { file: snapshot.file(), offsets: snapshot.ram_offsets() },
        )?;
        snapshot.check_cpu(config.cpu_model, vm.cpu_fingerprint())?;
        let mut d = snapshot.state();
        let vm_state = backend::VmState::decode(&mut d)?;
        let n = d.count(1)?;
        if n != vcpus.len() {
            return Err(Error::Snapshot(format!(
                "the snapshot's state holds {n} vCPUs, its header {}",
                vcpus.len()
            )));
        }
        let vcpu_states =
            (0..n).map(|_| backend::VcpuState::decode(&mut d)).collect::<Result<Vec<_>>>()?;

        // The devices as `boot_linux` makes them, in its order, so that each is where it was.
        let doorbell: Arc<Doorbell> = Arc::default();
        let stop = Arc::new(StopShared::with_doorbell(doorbell.clone()));
        let mut bars = crate::devices::pci::BarAllocator::new();
        if !config.pmems.is_empty() {
            return Err(Error::Snapshot("a machine with pmem devices is never snapshotted".into()));
        }
        let mut devices = extent_devices(&config, &vm, &mut bars, &stop, snapshot.extent_states())?;
        devices.extend(disk_devices(&config)?);
        let balloon = balloon_device(&config, &vm, &mut devices)?;
        let (vsock_device, vsock) = match vsock {
            Some(vsock) => (VsockDevice::adopt(&vsock)?, vsock),
            None => VsockDevice::new(GUEST_CID)?,
        };
        devices.extend([Box::new(Rng) as Box<dyn VirtioDevice>, Box::new(vsock_device)]);
        let pci =
            Arc::new(crate::devices::Pci::new(devices, vm.guest_memory(), &vm, &doorbell, bars)?);
        let output = serial::output(config.serial_capture_limit);
        let uart = IrqLine::Spi(vm.shared().clone(), layout::UART_SPI);
        let serial = serial::restore(&mut d, uart, output.clone())?;
        let bus = Bus::board(serial, pci.clone());
        bus.rtc().restore(&mut d)?;
        let exits: Vec<Arc<ExitCounters>> = (0..config.vcpus).map(|_| Arc::default()).collect();
        let mut live = Live::spawn(vm, vcpus, bus, &stop, &exits)?;
        live.pci = Some(pci.clone());

        // Each vCPU made and set on its own thread; the GIC's state once they all exist, before
        // any runs, as the framework has it; then their CPU interfaces, against it.
        type Job = Box<dyn FnOnce(&mut MachineVcpu) -> Result<()> + Send>;
        let states = Arc::new(vcpu_states);
        let shift = vm_state.shift();
        let jobs = (0..states.len())
            .map(|i| {
                let states = states.clone();
                Box::new(move |v: &mut MachineVcpu| v.vcpu.restore_state(&states[i], shift)) as Job
            })
            .collect();
        live.vcpus.on_each(jobs)?;
        vm_state.restore_gic()?;
        let jobs = (0..states.len())
            .map(|i| {
                let states = states.clone();
                Box::new(move |v: &mut MachineVcpu| v.vcpu.restore_icc(&states[i])) as Job
            })
            .collect();
        live.vcpus.on_each(jobs)?;
        // The devices last, so that their interrupts land on the restored GIC.
        pci.restore(&mut d, &*live.vm)?;
        d.end()?;
        live.resend_interrupts();

        let mut machine =
            Self::from_live(live, Platform::Board, config.mem_size, stop, exits, doorbell, output);
        machine.vsock = Some(vsock);
        machine.balloon = balloon;
        machine.disks = config.disks;
        machine.extent_stores = config.extent_stores.len();
        Ok(machine)
    }

    /// Writes the machine's state to a snapshot file at `path`, between
    /// runs (the `&mut` guarantees no [`Run`] is in progress: every vCPU is
    /// stopped). [`Snapshot::open`](crate::Snapshot::open) then reads it,
    /// and [`Machine::restore`] makes machines from it. The machine itself
    /// is not changed and can run on.
    ///
    /// PC machines only, without a network: a networked machine's
    /// connections are host sockets no clone could share, so it is refused,
    /// as is a machine with device memory mapped through
    /// [`memory_slots`](Machine::memory_slots) (its devices must save it).
    /// A suspended machine is restored for it, then suspended again (if
    /// that fails, it stays suspended with its VM back, as before a
    /// suspension frees it).
    ///
    /// Commands the guest agent runs at the time keep running in every
    /// clone, orphaned (no clone's client knows them); take snapshots of an
    /// idle guest.
    pub fn snapshot(&mut self, path: impl AsRef<std::path::Path>) -> Result<()> {
        self.snapshot_with(path, &SnapshotOptions::default())
    }

    /// [`snapshot`](Machine::snapshot), as `options` say: its RAM in a file of its own, for a
    /// live migration, with the caller's data.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    pub fn snapshot_with(
        &mut self,
        path: impl AsRef<std::path::Path>,
        options: &SnapshotOptions<'_>,
    ) -> Result<()> {
        if self.platform != Platform::Board {
            return Err(Error::Snapshot(
                "only PC machines (Machine::boot_linux) can be snapshotted".into(),
            ));
        }
        if self.network.is_some() {
            return Err(Error::Snapshot(
                "a machine with a network cannot be snapshotted: its connections are host \
                 sockets that no restored machine could share"
                    .into(),
            ));
        }
        if self.balloon.is_some() {
            return Err(Error::Snapshot(
                "a machine with a balloon is not snapshotted on KVM yet (board 13994)".into(),
            ));
        }
        if !self.disks.is_empty() || self.extent_stores > 0 {
            return Err(Error::Snapshot(
                "a machine with disks or extent store devices is not snapshotted on KVM yet \
                 (Hypervisor.framework's are)"
                    .into(),
            ));
        }
        let refreeze = matches!(self.body, Body::Frozen(_));
        self.thaw()?;
        let taken = self.write_snapshot(path.as_ref(), options);
        if refreeze {
            // Its own file exists: the directory is not used.
            let _ = self.suspend(std::path::Path::new("."));
        }
        taken
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn write_snapshot(
        &mut self,
        path: &std::path::Path,
        options: &SnapshotOptions<'_>,
    ) -> Result<()> {
        use crate::memory::RegionKind;
        use crate::snapshot::codec::Encoder;

        let live = self.live()?;
        let Some(pci) = &live.pci else {
            return Err(Error::Snapshot(
                "only PC machines (Machine::boot_linux) can be snapshotted".into(),
            ));
        };
        let layout = live.vm.memory_layout();
        if let Some(r) = layout.of_kind(RegionKind::DeviceMemory).next() {
            return Err(Error::Snapshot(format!(
                "device memory is mapped at {:#x} ({:#x} bytes), which no device saves",
                r.start, r.size
            )));
        }
        // The device thread waits until the state and RAM are written.
        let mut quiesced = pci.quiesce();
        let taken = (|| {
            let extent_stores = quiesced.take_snapshots(snapshot_mode(options))?;
            let mut e = Encoder::default();
            self.encode_cpus(live, &mut e)?;
            quiesced.save(&mut e)?;
            let mut header = self.header(layout);
            header.extent_stores = extent_stores;
            header.caller_data = options.caller_data.to_vec();
            let ram = ram_at(options);
            crate::snapshot::write(path, &header, &e.finish(), live.vm.guest_memory(), ram)
        })();
        quiesced.snapshots_done(taken.is_ok());
        taken
    }

    /// What a snapshot's or a suspension's state says the machine is, but for what only a
    /// snapshot says (its extent store devices' snapshots, the caller's data).
    fn header(&self, layout: crate::memory::MemoryLayout) -> crate::snapshot::MachineHeader {
        crate::snapshot::MachineHeader {
            cpu_model: self.cpu_model,
            cpu_fingerprint: self.cpu_fingerprint.clone(),
            vcpus: self.vcpu_count,
            mem_size: self.mem_size as u64,
            layout,
            balloon: self.balloon.is_some(),
            disks: self.disks.clone(),
            extent_stores: Vec::new(),
            caller_data: Vec::new(),
        }
    }

    /// KVM's VM state, every vCPU's, and the UART's, between runs.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn encode_cpus(&self, live: &Live, e: &mut crate::snapshot::codec::Encoder) -> Result<()> {
        let vm_state = live.vm.save_state(&live.vcpus.get(0).vcpu)?;
        vm_state.encode(e);
        e.count(live.vcpus.len());
        for i in 0..live.vcpus.len() {
            live.vcpus.get(i).vcpu.save_state(&live.vm)?.encode(e);
        }
        serial::save(&live.bus.serial(), e);
        Ok(())
    }

    /// Suspends the guest between runs, while it waits for something from
    /// outside (a slow server's answer, an agent call): the whole machine
    /// goes to a file in `dir` (its vCPUs' and devices' state, the network
    /// stack's with every guest connection's TCP state, and the guest's
    /// RAM), and its hypervisor VM, its vCPU, device and network threads
    /// and the guest's RAM are freed. What cannot go in a file stays: the
    /// host sockets of the guest's connections, the vsock's host end (agent
    /// clients and streams carry on), the network's policy and log, the
    /// console, and a thread that watches the sockets and the vsock. When
    /// something comes for the guest (data or a close on one of its
    /// connections, a packet for it on the vsock: an agent call), the
    /// machine wakes: [`wait_woken`](Machine::wait_woken) returns, and
    /// running it ([`run`](Machine::run)) restores it from the file onto
    /// what was held, the guest carrying on where it was on the same
    /// connections, its pages coming back from the file as it touches them.
    ///
    /// Only an idle guest is suspended, every vCPU halted with no interrupt
    /// waiting, every device settled and the network with nothing to do by
    /// itself (no DNS lookup or decider question in flight); else
    /// [`Error::Busy`]: run it a little and try again. Its clocks run on
    /// meanwhile, so it sees the time it waited, as if it had waited awake;
    /// its own timers fire when it runs again. See [`crate::suspend`].
    ///
    /// The first suspension makes the file, unnamed, in `dir`, which must
    /// be on a disk, not tmpfs or ramfs (refused), with room for the RAM
    /// the guest uses; later ones write into the same file only what the
    /// guest changed since, and the state (and ignore `dir`). It goes with
    /// the machine. It is no template: it lacks the host sockets and the
    /// vsock's host end, says so, and [`Snapshot::open`](crate::Snapshot::open)
    /// refuses it.
    ///
    /// PC machines only, without device memory mapped through
    /// [`memory_slots`](Machine::memory_slots) (whose handles map nothing
    /// once the VM is freed: take a new one after a wake). Suspending a
    /// suspended machine does nothing.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    pub fn suspend(&mut self, dir: impl AsRef<std::path::Path>) -> Result<()> {
        use crate::memory::RegionKind;

        let live = match &self.body {
            Body::Live(live) => live,
            Body::Frozen(_) => return Ok(()),
            Body::Lost(why) => return Err(lost(why)),
        };
        let Some(pci) = live.pci.clone() else {
            return Err(Error::InvalidConfig(
                "only PC machines (Machine::boot_linux) can be suspended".into(),
            ));
        };
        if self.caller_devices {
            return Err(Error::InvalidConfig(
                "a machine with devices of the caller's cannot be suspended: a suspension frees \
                 the devices, and only the caller could make them again"
                    .into(),
            ));
        }
        if self.balloon.is_some() {
            return Err(Error::InvalidConfig(
                "a machine with a balloon is not suspended yet (board 13994)".into(),
            ));
        }
        if !self.disks.is_empty() || self.extent_stores > 0 {
            return Err(Error::InvalidConfig(
                "a machine with disks or extent store devices is not suspended yet".into(),
            ));
        }
        let layout = live.vm.memory_layout();
        if let Some(r) = layout.of_kind(RegionKind::DeviceMemory).next() {
            return Err(Error::InvalidConfig(format!(
                "device memory is mapped at {:#x} ({:#x} bytes), which a suspension cannot save",
                r.start, r.size
            )));
        }
        // Armed first: what a device puts in a queue from now on wakes the
        // guest; what it put there before, an idle guest has taken (a
        // driver polling its queue is not halted), or is an interrupt
        // still waiting, which the checks see. (Already armed: the machine
        // is suspended with its VM back, and a ring since counts.)
        let was_armed = self.doorbell.armed();
        if !was_armed {
            self.doorbell.arm();
        }
        if let Err(e) = self.save_suspended(&pci, &layout, dir.as_ref()) {
            if !was_armed {
                self.doorbell.disarm();
            }
            return Err(e);
        }
        self.freeze();
        Ok(())
    }

    /// Checks that the guest is idle, then writes the whole machine to its
    /// file, its host ends handed over (the vsock detached, the network's
    /// host streams in the slot); on any failure, they are taken back and
    /// the machine is as it was.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn save_suspended(
        &mut self,
        pci: &crate::devices::Pci,
        layout: &crate::memory::MemoryLayout,
        dir: &std::path::Path,
    ) -> Result<()> {
        use crate::snapshot::codec::Encoder;

        let Body::Live(live) = &self.body else {
            unreachable!("only a live machine is saved");
        };
        for i in 0..live.vcpus.len() {
            if !live.vcpus.get(i).vcpu.idle()? {
                return Err(Error::Busy(format!(
                    "vCPU {i} is running or has an interrupt to take"
                )));
            }
        }
        // A device with something for the guest makes it busy too: what
        // the guest gave it and it has not yet taken (the last packet of a
        // vsock connection the guest closed, whose answer is on its way),
        // or what its host side holds and has not yet queued.
        let mut quiesced = pci.quiesce();
        if let Some(device) = quiesced.settle() {
            return Err(Error::Busy(format!(
                "virtio device type {device} has something for the guest to take"
            )));
        }
        // With the device thread held off, no device can ring now: one
        // that rang since the bell was armed gave the guest something
        // after the checks above began, which it may not have seen.
        if self.doorbell.rung() {
            return Err(Error::Busy(
                "a device gave the guest something as it was being suspended".into(),
            ));
        }
        let mut e = Encoder::default();
        self.header(layout.clone()).encode(&mut e);
        self.encode_cpus(live, &mut e)?;
        let mut lacks = crate::suspend::LACKS_VSOCK;
        if self.network.is_some() {
            lacks |= crate::suspend::LACKS_NETWORK;
        }
        // From here on no device serves the host ends: the vsock's is
        // detached before its device saves (board 13211), and the network
        // stack hands its host streams over as it saves.
        let vsock = self.vsock.as_ref().expect("a PC machine has a vsock");
        vsock.detach();
        let saved = (|| -> Result<()> {
            // Takes back what it handed over if it fails (the network
            // Busy, say).
            quiesced.hand_over(&mut e)?;
            let state = e.finish();
            let written = crate::suspend::write_to_file(
                live.vm.guest_memory(),
                layout,
                dir,
                &mut self.ram_file,
            )
            .and_then(|()| {
                let ram = self.ram_file.as_ref().expect("written");
                crate::suspend::write_state(ram, layout, &state, lacks)
            });
            if let Err(e) = written {
                return Err(match quiesced.take_back() {
                    Ok(()) => e,
                    Err(back) => Error::Snapshot(format!(
                        "{e}; then the network could not be taken back: {back}"
                    )),
                });
            }
            Ok(())
        })();
        if saved.is_err() {
            vsock.reattach();
        }
        saved
    }

    /// Frees the machine's VM, threads and RAM, its state being in its
    /// file, and starts watching what it holds.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn freeze(&mut self) {
        let net = self.net_slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        let body = std::mem::replace(&mut self.body, Body::Lost("being suspended".into()));
        let Body::Live(live) = body else {
            unreachable!("only a live machine is frozen");
        };
        let slots = live.vm.memory_slots().clone();
        // The vCPU threads stop and the device thread (the devices' saves
        // stopped the network thread), then the VM closes and the RAM is
        // unmapped.
        drop(live);
        // The handle held the VM open until now.
        slots.retire();
        self.retired_slots = Some(slots);
        let watched = net.as_ref().map(HeldStreams::watched).unwrap_or_default();
        let watcher = match crate::suspend::Watcher::spawn(
            self.doorbell.clone(),
            self.vsock.clone(),
            &watched,
        ) {
            Ok(watcher) => Some(watcher),
            // Nothing would wake it: it wakes now instead, and runs.
            Err(_) => {
                self.doorbell.ring();
                None
            }
        };
        self.body = Body::Frozen(Frozen { watcher, net, vsock: self.vsock.clone() });
    }

    /// Restores a suspended machine from its file onto what it held: the
    /// guest is where it was, still suspended (the doorbell armed) until
    /// it runs. If that fails, the machine is lost, and so is everything on
    /// its vsock.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn thaw(&mut self) -> Result<()> {
        match &self.body {
            Body::Live(_) => return Ok(()),
            Body::Lost(why) => return Err(lost(why)),
            Body::Frozen(_) => {}
        }
        let body = std::mem::replace(&mut self.body, Body::Lost("being woken".into()));
        let Body::Frozen(mut frozen) = body else {
            unreachable!("frozen");
        };
        // The watcher stops before the host ends go to the new devices.
        drop(frozen.watcher.take());
        frozen.vsock = None;
        let net = frozen.net.take();
        drop(frozen);
        match self.wake(net) {
            Ok(live) => {
                self.body = Body::Live(live);
                self.resend = true;
                Ok(())
            }
            Err(e) => {
                let why = format!("waking it from its suspension failed: {e}");
                self.body = Body::Lost(why);
                if let Some(vsock) = &self.vsock {
                    vsock.machine_gone();
                }
                Err(e)
            }
        }
    }

    /// The live machine from the state in its file, onto its stop state,
    /// doorbell, exit counters, console, vsock host end and network.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn wake(&mut self, net: Option<HeldStreams>) -> Result<Live> {
        use crate::snapshot::{MachineHeader, codec::Decoder};

        let ram = self.ram_file.clone().expect("a suspended machine has its file");
        let state = crate::suspend::read_state(&ram.file)?;
        let mut d = Decoder::new(&state);
        let header = MachineHeader::decode(&mut d)?;
        let layout = crate::arch::x86_64::layout::memory_layout(self.mem_size as u64);
        if header != self.header(layout) {
            return Err(Error::Snapshot(
                "the suspended machine's file holds another machine".into(),
            ));
        }
        let config = MachineConfig {
            mem_size: self.mem_size,
            cpu_model: self.cpu_model,
            vcpus: self.vcpu_count,
            network: None,
            serial_capture_limit: 0,
            // No machine with extent store devices or a balloon is suspended.
            extent_daemon: None,
            extent_stores: Vec::new(),
            nested_virtualization: false,
            balloon: false,
            disks: Vec::new(),
            pmems: Vec::new(),
        };
        let network = self.network.as_ref().map(|n| {
            NetCard::Adopt(
                n.shared(),
                self.net_slot.clone(),
                net.unwrap_or_else(|| HeldStreams::new(Default::default(), &[])),
            )
        });
        let fingerprint = self.cpu_fingerprint.clone();
        let check_cpu = |model: CpuModel, theirs: &str| {
            if theirs == fingerprint {
                Ok(())
            } else {
                Err(Error::Snapshot(format!(
                    "the suspended guest saw CPU fingerprint {fingerprint}; CPU model {model} now \
                     gives {theirs}"
                )))
            }
        };
        let kept = Kept {
            doorbell: self.doorbell.clone(),
            stop: &self.stop,
            exits: &self.exits,
            serial: self.serial.clone(),
            vsock: self.vsock.clone(),
            network,
        };
        let (live, _) =
            Self::rebuild(&config, &ram.file, &ram.offsets, &mut d, check_cpu, kept, false)?;
        Ok(live)
    }

    /// Whether the machine is suspended ([`suspend`](Machine::suspend)) and
    /// has not run since.
    pub fn suspended(&self) -> bool {
        self.doorbell.armed()
    }

    /// Waits, at most `timeout` (None: as long as it takes), until
    /// something comes for the suspended guest ([`Woken::Device`]: run it),
    /// or a [`StopHandle`] asks the machine to stop ([`Woken::Stop`]: the
    /// request is used up, and the guest stays suspended). Returns
    /// [`Woken::Device`] at once if something already came, or if the
    /// machine is not suspended.
    pub fn wait_woken(&self, timeout: Option<Duration>) -> Woken {
        if !self.doorbell.armed() {
            return Woken::Device;
        }
        self.doorbell.wait(|| self.stop.take_request(), timeout)
    }

    /// A new machine from `snapshot`, ready to [`run`](Machine::run) from
    /// where the snapshot's machine stopped. `config` says what machine to
    /// build, and must be the snapshot's ([`Snapshot::config`]): the same
    /// vCPU count and RAM size (so the same address map), no network, and a
    /// CPU model that gives the same CPU fingerprint on this host; anything
    /// else is refused, never converted.
    ///
    /// The guest's RAM is a private copy-on-write mapping of the snapshot
    /// file: what the guest writes stays in this machine, never reaching
    /// the file or other machines restored from it. Its clock is moved on to
    /// the present; its VM generation ID changes, so that its kernel
    /// reseeds its RNG, and the guest agent reseeds it with host entropy
    /// before running anything ([`agent`](crate::agent)); its vsock
    /// connections from before the snapshot are reset (a transport reset
    /// event).
    ///
    /// [`Snapshot::config`]: crate::Snapshot::config
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    pub fn restore(snapshot: &crate::Snapshot, config: MachineConfig) -> Result<Self> {
        Self::restore_with(snapshot, config, None)
    }

    /// [`restore`](Machine::restore), onto `vsock` if given: the host end
    /// of the machine the snapshot was taken of, detached from it
    /// ([`Vsock::detach`](crate::Vsock::detach)) before the snapshot. Its
    /// connections and listeners then carry on, with no transport reset,
    /// and the agent clients and streams on it go on as if the machine had
    /// only paused. Refused, with the host end unchanged, unless it is
    /// detached and nothing has passed between it and a guest since the
    /// snapshot. Without `vsock`, a new host end, connections reset.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    pub(crate) fn restore_with(
        snapshot: &crate::Snapshot,
        config: MachineConfig,
        vsock: Option<crate::Vsock>,
    ) -> Result<Self> {
        use crate::arch::x86_64::layout;

        check_mem_size(config.mem_size)?;
        check_vcpus(config.vcpus)?;
        no_nested(&config, "a machine on KVM")?;
        if !config.extent_stores.is_empty() {
            return Err(Error::Snapshot(
                "machines with extent store devices are not restored yet (board 13139)".into(),
            ));
        }
        snapshot.check(&config, &layout::memory_layout(config.mem_size as u64))?;
        let doorbell = Arc::<Doorbell>::default();
        let stop = Arc::new(StopShared::with_doorbell(doorbell.clone()));
        let exits: Vec<Arc<ExitCounters>> = (0..config.vcpus).map(|_| Arc::default()).collect();
        let output = serial::output(config.serial_capture_limit);
        let kept = Kept {
            doorbell: doorbell.clone(),
            stop: &stop,
            exits: &exits,
            serial: output.clone(),
            vsock,
            network: None,
        };
        let (live, vsock) = Self::rebuild(
            &config,
            snapshot.file(),
            snapshot.ram_offsets(),
            &mut snapshot.state(),
            |model, fingerprint| snapshot.check_cpu(model, fingerprint),
            kept,
            true,
        )?;
        let mut machine =
            Self::from_live(live, Platform::Board, config.mem_size, stop, exits, doorbell, output);
        machine.vsock = Some(vsock);
        machine.ram_file = Some(crate::suspend::RamFile {
            file: snapshot.file().clone(),
            offsets: snapshot.ram_offsets().to_vec(),
            ours: false,
        });
        Ok(machine)
    }

    /// A PC machine's live parts from saved state `d` (past its header)
    /// and its RAM in `file` from `offsets` (mapped privately), onto what
    /// `kept` gives; its host end of the vsock. A `clone` (of a snapshot)
    /// gets a new VM generation ID; a suspended machine woken is the same
    /// guest, which [`Live::resend_interrupts`] when it runs (not before:
    /// a machine woken to be snapshotted is suspended again, and spare
    /// interrupts would make its guest busy); a clone's do now.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn rebuild(
        config: &MachineConfig,
        file: &Arc<std::fs::File>,
        offsets: &[u64],
        d: &mut crate::snapshot::codec::Decoder<'_>,
        check_cpu: impl FnOnce(CpuModel, &str) -> Result<()>,
        kept: Kept<'_>,
        clone: bool,
    ) -> Result<(Live, crate::Vsock)> {
        use crate::arch::x86_64::layout::GED_IRQ;

        let (vm, vcpus) = backend::Vm::new(
            config.mem_size,
            config.cpu_model,
            Platform::Board,
            config.vcpus,
            backend::Ram::Snapshot { file, offsets },
        )?;
        check_cpu(config.cpu_model, vm.cpu_fingerprint())?;
        let vm_state = backend::VmState::decode(d)?;
        let n = d.count(1)?;
        if n != vcpus.len() {
            return Err(Error::Snapshot(format!(
                "the snapshot's state holds {n} vCPUs, its header {}",
                vcpus.len()
            )));
        }
        let vcpu_states =
            (0..n).map(|_| backend::VcpuState::decode(d)).collect::<Result<Vec<_>>>()?;

        let bars = crate::devices::pci::BarAllocator::new();
        let pc = PcDevices::new(&vm, kept.network, Vec::new(), kept.vsock, kept.doorbell, bars)?;
        let serial = serial::restore(d, IrqLine::EventFd(pc.uart_irq), kept.serial)?;
        // KVM's state first, so that what the devices do next (MSI routes,
        // irqfds, interrupts) lands on the restored interrupt controllers.
        let tsc_cycles = vm.restore_state(&vm_state, &vcpus)?;
        for (vcpu, state) in vcpus.iter().zip(&vcpu_states) {
            vcpu.restore_state(&vm, state, tsc_cycles)?;
        }
        pc.pci.restore(d, &vm)?;
        d.end()?;
        if clone {
            // A new generation ID, and the event that tells the guest.
            write_generation_id(&vm)?;
            vm.set_irq_line(GED_IRQ, true)?;
            vm.set_irq_line(GED_IRQ, false)?;
        }
        let bus = Bus::board(serial, pc.pci.clone());
        let mut live = Live::spawn(vm, vcpus, bus, kept.stop, kept.exits)?;
        live.pci = Some(pc.pci);
        if clone {
            live.resend_interrupts();
        }
        Ok((live, pc.vsock))
    }

    /// The CPU model the guest sees.
    pub fn cpu_model(&self) -> CpuModel {
        self.cpu_model
    }

    /// A stable identifier of the CPU the guest sees (lowercase hex SHA-256):
    /// equal fingerprints mean the same CPUID and feature MSRs, so a
    /// snapshot taken on one machine is CPU-compatible with the other.
    pub fn cpu_fingerprint(&self) -> &str {
        &self.cpu_fingerprint
    }

    /// How many vCPUs the machine has.
    pub fn vcpus(&self) -> u32 {
        self.vcpu_count
    }

    /// The CPUID result the guest gets for leaf `function`, subleaf `index`,
    /// as configured for vCPU 0 (other vCPUs differ only in their APIC ID
    /// and topology fields). `None` for a leaf the table does not hold.
    #[cfg(target_arch = "x86_64")]
    pub fn cpuid(&self, function: u32, index: u32) -> Option<CpuidEntry> {
        self.cpuid.iter().find(|e| e.matches(function, index)).copied()
    }

    /// Size of guest RAM in bytes.
    pub fn mem_size(&self) -> usize {
        self.mem_size
    }

    /// The guest physical memory map: RAM, the MMIO windows, and the device
    /// memory mapped into them now. A snapshot records it, so that a restore
    /// puts everything back at the same addresses.
    pub fn memory_layout(&self) -> crate::memory::MemoryLayout {
        match &self.body {
            Body::Live(live) => live.vm.memory_layout(),
            // A suspended machine has no device memory mapped.
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            _ => crate::arch::x86_64::layout::memory_layout(self.mem_size as u64),
        }
    }

    /// A handle that maps host memory into the guest's MMIO windows at
    /// runtime, from any thread, each mapping in a memory slot of its own
    /// that is removed when dropped. [`load`](Machine::load) and
    /// [`read_memory`](Machine::read_memory) reach RAM only; device memory
    /// is accessed through its [`HostMemory`](crate::HostMemory).
    ///
    /// The handle is the hypervisor VM's: a [suspension](Machine::suspend)
    /// frees the VM, after which the handle maps nothing (an error says
    /// why), and this gives one that maps nothing until the machine runs
    /// again.
    #[cfg(target_os = "linux")]
    pub fn memory_slots(&self) -> crate::MemorySlots {
        match &self.body {
            Body::Live(live) => live.vm.memory_slots().clone(),
            #[cfg(target_arch = "x86_64")]
            _ => self.retired_slots.clone().expect("a machine that is not live was suspended"),
        }
    }

    /// Copies `bytes` into guest RAM at guest physical address `addr` (a
    /// suspended machine is restored for it, and stays suspended).
    pub fn load(&mut self, addr: u64, bytes: &[u8]) -> Result<()> {
        self.live_mut()?.vm.write_memory(addr, bytes)
    }

    /// Reads `len` bytes of guest RAM starting at guest physical address
    /// `addr` (from its file, if the machine is suspended).
    pub fn read_memory(&self, addr: u64, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0; len];
        match &self.body {
            Body::Live(live) => live.vm.read_memory(addr, &mut buf)?,
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            Body::Frozen(_) => self.read_suspended(addr, &mut buf)?,
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            Body::Lost(why) => return Err(lost(why)),
        }
        Ok(buf)
    }

    /// Guest RAM at `addr`, read from a suspended machine's file.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn read_suspended(&self, addr: u64, buf: &mut [u8]) -> Result<()> {
        use crate::memory::RegionKind;
        use std::os::unix::fs::FileExt;

        let ram = self.ram_file.as_ref().expect("a suspended machine has its file");
        let layout = crate::arch::x86_64::layout::memory_layout(self.mem_size as u64);
        let mut at = addr;
        let mut done = 0;
        while done < buf.len() {
            let (region, offset) = layout
                .of_kind(RegionKind::Ram)
                .zip(&ram.offsets)
                .find(|(r, _)| r.start <= at && at < r.start + r.size)
                .ok_or_else(|| {
                    Error::InvalidConfig(format!(
                        "{at:#x} is not guest RAM (reading {} bytes from {addr:#x})",
                        buf.len()
                    ))
                })?;
            let n = ((region.start + region.size - at) as usize).min(buf.len() - done);
            ram.file
                .read_exact_at(&mut buf[done..done + n], offset + (at - region.start))
                .map_err(|source| Error::Io {
                    path: "the suspended machine's file".into(),
                    source,
                })?;
            done += n;
            at += n as u64;
        }
        Ok(())
    }

    /// Points vCPU 0 at `addr` in 16-bit real mode (CS base 0, IP = `addr`).
    /// `addr` must be below 64 KiB.
    #[cfg(target_arch = "x86_64")]
    pub fn set_entry(&mut self, addr: u64) -> Result<()> {
        if addr > 0xffff {
            return Err(Error::InvalidConfig(format!(
                "real-mode entry point must be below 0x10000, got {addr:#x}"
            )));
        }
        self.live_mut()?.vcpus.get(0).vcpu.set_real_mode_entry(addr)
    }

    /// Points vCPU 0 at `addr`, at EL1 with its MMU off and interrupts masked, as a kernel's
    /// entry is.
    #[cfg(target_arch = "aarch64")]
    pub fn set_entry(&mut self, addr: u64) -> Result<()> {
        let cpu = crate::arch::aarch64::boot::BootCpu { pc: addr, x: [0; 4] };
        // On its own thread, which created it.
        let job = Box::new(move |v: &mut MachineVcpu| v.vcpu.set_boot_cpu(&cpu));
        self.live_mut()?.vcpus.on(0, job)
    }

    /// vCPU 0's register state (not while the machine is suspended: they
    /// are in its file until it runs).
    #[cfg(target_arch = "x86_64")]
    pub fn registers(&self) -> Result<Registers> {
        self.live()?.vcpus.get(0).vcpu.registers()
    }

    fn output(&self) -> std::sync::MutexGuard<'_, serial::Output> {
        self.serial.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A copy of the most recent bytes the guest has written to the serial
    /// port, at most [`MachineConfig::serial_capture_limit`] of them.
    pub fn serial_output(&self) -> Vec<u8> {
        self.output().recent().to_vec()
    }

    /// Takes and clears the captured serial output.
    pub fn take_serial_output(&mut self) -> Vec<u8> {
        self.output().take()
    }

    /// Also copies the guest's serial output to `console` as it is written
    /// (it is still collected for [`serial_output`]). A console that fails
    /// to write is dropped, so it can never stall the guest.
    ///
    /// [`serial_output`]: Machine::serial_output
    pub fn set_console(&mut self, console: impl std::io::Write + Send + 'static) {
        self.output().console = Some(std::sync::Mutex::new(Box::new(console)));
    }

    /// The PCI bus, on a PC machine that is not suspended.
    #[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
    pub(crate) fn pci(&self) -> Option<&crate::devices::Pci> {
        self.live().ok()?.pci.as_deref()
    }

    /// The vCPU that ended the last run (none for a stop request seen
    /// before the run, or a timeout).
    #[cfg(test)]
    pub(crate) fn last_exit_vcpu(&self) -> Option<u32> {
        self.live().ok()?.vcpus.last_vcpu()
    }

    /// The CPU time each vCPU thread has used.
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn vcpu_thread_cpu_times(&self) -> Vec<Duration> {
        self.live().map(|live| live.vcpus.thread_cpu_times()).unwrap_or_default()
    }

    /// The host's side of the machine's balloon, if it has one ([`MachineConfig::balloon`]).
    pub fn balloon(&self) -> Option<&crate::Balloon> {
        self.balloon.as_ref()
    }

    /// The host's end of the machine's vsock (board machines only): connect to
    /// the guest's ports and accept its connections, from any thread,
    /// including while another is in [`run`](Machine::run), and across
    /// suspensions.
    pub fn vsock(&self) -> Option<crate::Vsock> {
        self.vsock.clone()
    }

    /// The guest agent (`libmachine-agent`) on the machine's vsock, at its
    /// default port (board machines only); usable from any thread like
    /// [`vsock`](Machine::vsock).
    pub fn agent(&self) -> Option<crate::agent::Agent> {
        self.vsock.clone().map(crate::agent::Agent::new)
    }

    /// How many vCPU exits of each kind the machine has taken since it was
    /// created (across suspensions), over all its vCPUs: the accesses to
    /// each device, the kicks that stopped a vCPU, and so on
    /// ([`ExitCounts`]). Counting costs the exit path no measurable time.
    /// While the machine runs, a reading may lag the vCPUs by a few exits.
    pub fn exit_counts(&self) -> ExitCounts {
        self.exits.iter().map(|e| e.read()).sum()
    }

    /// The machine's network, if [`MachineConfig::network`] gave it one:
    /// its policy can change from any thread, including while another is in
    /// [`run`](Machine::run) or the machine is suspended.
    pub fn network(&self) -> Option<crate::net::Network> {
        self.network.clone()
    }

    /// A handle that stops [`run`](Machine::run) from any thread (and ends
    /// a suspended machine's [`wait_woken`](Machine::wait_woken)).
    pub fn stop_handle(&self) -> StopHandle {
        StopHandle::new(self.stop.clone())
    }

    /// Runs every vCPU until the guest powers off, resets, triple-faults,
    /// or (on a bare machine) halts or makes an access no device handles, or
    /// a [`StopHandle`] stops it. Whichever vCPU ends the run, every vCPU
    /// stops before `run` returns, and the machine keeps its state: `run`
    /// again resumes them all. An error on any vCPU ends the run the same
    /// way and is returned. Blocks the calling thread while the guest runs.
    /// A suspended machine is restored from its file first.
    ///
    /// When the guest powers off, resets or triple-faults, everything
    /// waiting on it fails at once: vsock streams and connects with a reset,
    /// agent calls with [`Error::GuestGone`] (see [`Vsock`](crate::Vsock)).
    pub fn run(&mut self) -> Result<ExitReason> {
        self.start_run(None).wait()
    }

    /// Like [`run`](Machine::run), but returns [`ExitReason::TimedOut`] once
    /// `timeout` has elapsed, with every vCPU stopped. The machine keeps its
    /// state and can be run again.
    pub fn run_for(&mut self, timeout: Duration) -> Result<ExitReason> {
        self.start_run(Some(timeout)).wait()
    }

    /// After a run, whichever way it ended (every [`Run`] outcome passes
    /// through [`Run::ended`]): a guest that powered off, reset or
    /// triple-faulted is
    /// not there to answer, so what waits on it fails now rather than
    /// waiting for a timeout. libmachine does not reboot it: running the
    /// machine again continues its vCPUs where they stopped; a guest that
    /// starts its vsock driver again is answered again.
    fn ran(&self, outcome: &Result<ExitReason>) {
        let stopped =
            matches!(outcome, Ok(ExitReason::Poweroff | ExitReason::Reset | ExitReason::Shutdown));
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        if stopped && let Some(vsock) = &self.vsock {
            vsock.guest_stopped();
        }
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        let _ = stopped;
    }

    /// Starts a run like [`run`](Machine::run) (or, with a `timeout`,
    /// [`run_for`](Machine::run_for)) and returns without waiting for it:
    /// the vCPUs run while the caller does something else, and
    /// [`Run::wait_for`] waits for the outcome in steps without disturbing
    /// them, e.g. to handle signals or other events between waits.
    ///
    /// The timeout is enforced while someone waits on the run; a run left
    /// unwaited past its timeout ends at the next wait.
    pub fn start_run(&mut self, timeout: Option<Duration>) -> Run<'_> {
        // A suspended machine resumes: restored from its file, its RAM
        // coming back as it is touched.
        let thawed = self.live_mut().map(|_| ());
        self.doorbell.disarm();
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        if thawed.is_ok()
            && std::mem::take(&mut self.resend)
            && let Body::Live(live) = &self.body
        {
            live.resend_interrupts();
        }
        let ready = match thawed {
            Ok(()) => self.running_vcpus().start(timeout.map(|t| Instant::now() + t)),
            Err(e) => Some(Err(e)),
        };
        Run { machine: self, ready }
    }

    /// The vCPUs of a machine with a run in progress (which is live).
    fn running_vcpus(&mut self) -> &mut Vcpus<MachineVcpu> {
        match &mut self.body {
            Body::Live(live) => &mut live.vcpus,
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            _ => unreachable!("a run in progress has its vCPUs"),
        }
    }
}

/// A run in progress, from [`Machine::start_run`].
///
/// Each way of finishing it takes the `Run`, so a run's outcome is returned
/// once. Dropping a run that has not ended stops it and waits for every vCPU
/// to stop, so once the `Run` is gone the machine is between runs, whatever
/// happened.
#[must_use = "dropping a Run stops it"]
pub struct Run<'m> {
    machine: &'m mut Machine,
    /// The outcome of a run that ended before any vCPU entered the guest.
    ready: Option<Result<ExitReason>>,
}

/// What [`Run::wait_for`] found.
#[must_use]
pub enum Waited<'m> {
    /// The run ended, with every vCPU stopped: what
    /// [`Machine::run`] would have returned.
    Ended(Result<ExitReason>),
    /// The run goes on, its vCPUs undisturbed.
    Running(Run<'m>),
}

impl std::fmt::Debug for Waited<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ended(outcome) => f.debug_tuple("Ended").field(outcome).finish(),
            Self::Running(_) => f.write_str("Running"),
        }
    }
}

impl<'m> Run<'m> {
    /// Waits up to `timeout` for the run to end.
    pub fn wait_for(mut self, timeout: Duration) -> Waited<'m> {
        if let Some(outcome) = self.ready.take() {
            return Waited::Ended(self.ended(outcome));
        }
        match self.machine.running_vcpus().wait(Some(Instant::now() + timeout)) {
            Some(outcome) => Waited::Ended(self.ended(outcome)),
            None => Waited::Running(self),
        }
    }

    /// Waits for the run to end.
    pub fn wait(mut self) -> Result<ExitReason> {
        match self.ready.take() {
            Some(outcome) => self.ended(outcome),
            None => self.finish(),
        }
    }

    /// Ends the run now and returns its outcome: [`ExitReason::Stopped`],
    /// unless it had already ended another way. Unlike a [`StopHandle`]'s
    /// request, it leaves nothing behind for the next run.
    pub fn stop(mut self) -> Result<ExitReason> {
        if let Some(outcome) = self.ready.take() {
            return self.ended(outcome);
        }
        self.machine.running_vcpus().stop();
        self.finish()
    }

    /// The machine's exit counts so far ([`Machine::exit_counts`]).
    pub fn exit_counts(&self) -> ExitCounts {
        self.machine.exit_counts()
    }

    fn finish(&mut self) -> Result<ExitReason> {
        let outcome =
            self.machine.running_vcpus().wait(None).expect("a wait without a limit lasts the run");
        self.ended(outcome)
    }

    /// Every outcome of a run passes through here, however the run ended
    /// (waited, stopped, dropped; `run` and `run_for` are a `Run` too), so
    /// what follows a run's end (the vsock's "guest stopped") cannot be
    /// skipped by one way of ending it.
    fn ended(&self, outcome: Result<ExitReason>) -> Result<ExitReason> {
        self.machine.ran(&outcome);
        outcome
    }
}

impl Drop for Run<'_> {
    fn drop(&mut self) {
        if let Body::Live(live) = &mut self.machine.body
            && live.vcpus.in_run()
        {
            live.vcpus.stop();
            let _ = self.finish();
        }
    }
}

impl std::fmt::Debug for Run<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Run").field("machine", &self.machine).finish_non_exhaustive()
    }
}

// Machines are shared across threads by callers (and Python requires it),
// and a run in progress can be waited on from another thread.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Machine>();
    assert_send_sync::<Run<'static>>();
};

impl std::fmt::Debug for Machine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Machine")
            .field("platform", &self.platform)
            .field("mem_size", &self.mem_size)
            .field("vcpus", &self.vcpu_count)
            .field("suspended", &self.suspended())
            .field("serial_bytes", &self.output().len())
            .finish_non_exhaustive()
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;

    #[test]
    fn vcpu_count_limits() {
        for ok in [1, 2, 4, MachineConfig::MAX_VCPUS] {
            assert!(check_vcpus(ok).is_ok(), "{ok}");
        }
        for bad in [0, MachineConfig::MAX_VCPUS + 1, u32::MAX] {
            assert!(matches!(check_vcpus(bad), Err(Error::InvalidConfig(_))), "{bad}");
        }
    }

    /// A suspended machine whose wake fails (its file damaged here) is
    /// lost cleanly: the run fails, so does every call waiting on its
    /// vsock (none waits for good), and so does everything after.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_wake_that_fails_loses_the_machine_and_fails_its_callers() {
        use std::os::unix::fs::FileExt;
        let Some(dir) = crate::devices::guest_tests::guest() else {
            return;
        };
        let boot = LinuxBoot {
            kernel: dir.join("vmlinux"),
            initrd: Some(dir.join("initramfs.cpio")),
            cmdline: "quiet libmachine_action=agent".into(),
        };
        let config = MachineConfig { mem_size: 256 << 20, ..Default::default() };
        let mut m = Machine::boot_linux(config, &boot).unwrap();
        let agent = m.agent().unwrap().timeout(Some(Duration::from_secs(60)));
        let stop = m.stop_handle();
        let runner = std::thread::spawn(move || (m.run(), m));
        agent.wait_ready(Duration::from_secs(60)).unwrap();
        stop.stop();
        let (exit, mut m) = runner.join().unwrap();
        assert_eq!(exit.unwrap(), ExitReason::Stopped);
        let ram_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match m.suspend(&ram_dir) {
                Ok(()) => break,
                Err(Error::Busy(_)) if Instant::now() < deadline => {
                    m.run_for(Duration::from_millis(2)).unwrap();
                }
                Err(e) => panic!("suspend: {e}"),
            }
        }
        // A byte of the state's checksum, in the header.
        let file = m.ram_file.as_ref().unwrap().file.clone();
        let mut b = [0u8; 1];
        file.read_exact_at(&mut b, 40).unwrap();
        file.write_all_at(&[!b[0]], 40).unwrap();

        let caller = std::thread::spawn(move || agent.exec(&crate::agent::Command::new("true")));
        assert_eq!(m.wait_woken(Some(Duration::from_secs(30))), Woken::Device);
        let err = m.run().unwrap_err();
        assert!(err.to_string().contains("checksum"), "{err}");
        let start = Instant::now();
        assert!(caller.join().unwrap().is_err(), "the call fails");
        assert!(start.elapsed() < Duration::from_secs(10));
        assert!(!m.suspended());
        assert!(matches!(m.run(), Err(Error::GuestGone(_))));
        assert!(matches!(m.suspend(&ram_dir), Err(Error::GuestGone(_))));
        assert!(m.read_memory(0x10_0000, 16).is_err());
        assert!(m.vsock().unwrap().connect(1024, Duration::from_secs(1)).is_err());
    }

    #[test]
    fn ram_size_limits() {
        use crate::arch::x86_64::layout::MAX_RAM;
        for ok in [4096, 3 << 30, (3 << 30) + 4096, 4 << 30, MAX_RAM as usize] {
            assert!(check_mem_size(ok).is_ok(), "{ok:#x}");
        }
        for bad in [0, 4097, MAX_RAM as usize + 4096, 1 << 40] {
            assert!(matches!(check_mem_size(bad), Err(Error::InvalidConfig(_))), "{bad:#x}");
        }
    }
}
