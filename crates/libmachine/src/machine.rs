// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::backend::{self, Platform};
use crate::devices::bus::Bus;
use crate::devices::serial::{self, IrqLine};
use crate::error::{Error, Result};
use crate::exits::{ExitCounters, ExitCounts, ExitKind};
use crate::stop::{StopHandle, StopShared};
use crate::vcpus::{RunControl, VcpuLoop, Vcpus};
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::{Duration, Instant};

const PAGE_SIZE: usize = 4096;
/// The kernel command line libmachine starts every board's with: the console on the UART, and a
/// panic resets the machine.
pub(crate) const BASE_CMDLINE: &str = "console=ttyS0 panic=-1";
/// The UART's IIR bit that says no interrupt is pending (16550).
const UART_NO_INTERRUPT: u8 = 1;

/// How [`Machine::snapshot_with`] takes a snapshot.
#[derive(Debug, Clone, Copy, Default)]
pub struct SnapshotOptions<'a> {
    /// Where the guest's RAM goes: the snapshot's file, after its state, by default; a file of
    /// its own, as a Firecracker snapshot's memory file holds it; or nowhere, its caller keeping
    /// it.
    pub ram: crate::SnapshotRam<'a>,
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
    /// Guest RAM in bytes. Must be a non-zero multiple of 4 KiB, and at most the arm64 map's
    /// RAM ([`Machine::memory_layout`]).
    pub mem_size: usize,
    /// How many vCPUs, from 1 to [`MAX_VCPUS`](MachineConfig::MAX_VCPUS).
    /// Each runs on a host thread of its own. A bare machine
    /// ([`Machine::new`]) has exactly one.
    pub vcpus: u32,
    /// The guest's network (board machines): a virtio-net card whose other end is a userspace
    /// network stack in this process, its connections made from the host where the policy allows
    /// ([`crate::net`]). None, the default, gives no network card at all. A machine with one is
    /// not snapshotted.
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
    /// [`extent_daemon`](MachineConfig::extent_daemon) as the machine is made.
    pub extent_stores: Vec<crate::ExtentStore>,
    /// Gives the guest the CPU's virtualization extensions, EL2, so that it runs VMs of its own
    /// with KVM (nested virtualization): its kernel boots at EL2. Needs a host that has it (Apple
    /// M3 and later, macOS 15 and later); no snapshot holds a machine with it yet.
    pub nested_virtualization: bool,
    /// Gives the guest a virtio-balloon ([`Machine::balloon`], [`crate::Balloon`]): its free pages
    /// reported and given back to the host, and a target the host sets.
    pub balloon: bool,
    /// The machine's disks (virtio-blk, [`crate::Disk`]): `/dev/vda` on, in their order, after
    /// its extent store devices.
    pub disks: Vec<crate::Disk>,
    /// The machine's persistent memory devices (virtio-pmem, [`crate::Pmem`]): `/dev/pmem0` on,
    /// in their order, each its file mapped into the guest's memory. A machine with any is not
    /// snapshotted.
    pub pmems: Vec<crate::Pmem>,
}

impl MachineConfig {
    /// The most vCPUs a machine can have. The host's Hypervisor.framework may allow fewer.
    pub const MAX_VCPUS: u32 = 254;
    /// The default [`serial_capture_limit`](MachineConfig::serial_capture_limit): 1 MiB.
    pub const DEFAULT_SERIAL_CAPTURE_LIMIT: usize = 1 << 20;
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self {
            mem_size: 16 << 20,
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
    /// An arm64 `Image`.
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
    /// The guest executed `hlt` (bare machines only: on a board machine `hlt`
    /// waits for an interrupt).
    Halt,
    /// The guest powered the machine off (PSCI SYSTEM_OFF).
    Poweroff,
    /// The guest reset the machine (PSCI SYSTEM_RESET), e.g. `reboot`, or a
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

/// A virtual machine.
///
/// Two kinds: [`Machine::new`] makes a bare machine (one vCPU, no interrupt
/// controller, only a UART, every other access reported to the caller) for
/// instruction-level tests; [`Machine::boot_linux`] makes the arm64 board with
/// [`MachineConfig::vcpus`] vCPUs that boots a Linux kernel and behaves like
/// hardware (unclaimed addresses read as all ones).
///
/// Each vCPU runs on a host thread of its own, which lives as long as the
/// machine and waits while the machine is not running.
///
/// A process has one Hypervisor.framework VM: while a machine exists, making
/// another fails.
pub struct Machine {
    /// The hypervisor VM and what runs it.
    live: Live,
    platform: Platform,
    mem_size: usize,
    cpu_fingerprint: String,
    vcpu_count: u32,
    stop: Arc<StopShared>,
    /// Each vCPU's exit counters, which its thread writes.
    exits: Vec<Arc<ExitCounters>>,
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
    nested: bool,
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
        let runners = vcpus
            .into_iter()
            .zip(exits)
            .map(|(vcpu, exits)| MachineVcpu {
                vcpu,
                bus: bus.clone(),
                vm: vm.clone(),
                exits: exits.clone(),
            })
            .collect();
        let vcpus = Vcpus::spawn(runners, stop.clone())?;
        // Each vCPU created on its thread in its order, as the framework hands out the GIC's
        // redistributors (`hvf::Vcpu::create`).
        for i in 0..vcpus.len() {
            vcpus.on(i, Box::new(|v: &mut MachineVcpu| v.vcpu.create_now()))?;
        }
        Ok(Self { vcpus, vm, pci: None, bus })
    }

    /// Interrupts the guest again from every device that may have had an
    /// interrupt on its way when its state was saved, which went with the
    /// VM that was to take it: every active queue, and the UART if it says
    /// an interrupt is pending. A spare one only makes the driver look.
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

/// What each vCPU thread runs: its vCPU, with exits dispatched to the bus
/// and counted.
struct MachineVcpu {
    vcpu: backend::Vcpu,
    bus: Arc<Bus>,
    vm: Arc<backend::Vm>,
    exits: Arc<ExitCounters>,
}

impl VcpuLoop for MachineVcpu {
    fn kick_target(&mut self) -> crate::backend::kick::KickTarget {
        self.vcpu.kick_target()
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
            // check makes the next run return at once.
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
            match self.bus.handle(exit, &self.vm, &self.exits) {
                Ok(None) => {}
                Ok(Some(reason)) => return Some(Ok(reason)),
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

/// Refuses nested virtualization to a bare machine.
fn no_nested(config: &MachineConfig, what: &str) -> Result<()> {
    if config.nested_virtualization {
        return Err(Error::InvalidConfig(format!(
            "{what} has no nested virtualization, which board machines alone get; \
             nested_virtualization must be false"
        )));
    }
    Ok(())
}

fn check_mem_size(mem_size: usize) -> Result<()> {
    use crate::arch::aarch64::layout::{MAX_RAM, PCI_MMIO64_ADDR};
    if mem_size == 0 || !mem_size.is_multiple_of(PAGE_SIZE) {
        return Err(Error::InvalidConfig(format!(
            "mem_size must be a non-zero multiple of {PAGE_SIZE} bytes, got {mem_size}"
        )));
    }
    if mem_size as u64 > MAX_RAM {
        return Err(Error::InvalidConfig(format!(
            "mem_size is {mem_size} bytes; RAM can be at most {MAX_RAM} bytes, or it would reach \
             the 64-bit MMIO window at {PCI_MMIO64_ADDR:#x}"
        )));
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

/// How `options` have the extent store devices' snapshots taken.
fn snapshot_mode(options: &SnapshotOptions<'_>) -> virtio_extent::SnapshotMode {
    match options.for_migration {
        true => virtio_extent::SnapshotMode::Migration,
        false => virtio_extent::SnapshotMode::Versioned,
    }
}

/// The extent store devices of `config`'s ([`MachineConfig::extent_stores`]), with a request
/// queue per vCPU, their DAX windows' BARs from `bars`, stopping the machine through `stop` as one
/// can't go on: the windows' pages are mapped by libmachine (board 13069). Restored from a
/// snapshot, each made from what it `saved` of itself, in order.
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

/// The host memory of the one region of RAM `mem` is, and its size.
fn ram_host_of(mem: &vm_memory::GuestMemoryMmap) -> Result<(std::ptr::NonNull<u8>, usize)> {
    use vm_memory::{GuestMemoryBackend, GuestMemoryRegion};
    let region = mem.iter().next().ok_or_else(|| Error::MemoryMap("no RAM".into()))?;
    let host = mem.get_host_address(region.start_addr())?;
    let host = std::ptr::NonNull::new(host).expect("RAM's host address");
    Ok((host, region.len() as usize))
}

impl Machine {
    /// Creates a bare VM with zeroed RAM and one vCPU. `config.vcpus` must be 1: a bare machine
    /// has no interrupt controller to start a second vCPU with.
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
        let (vm, vcpus) =
            backend::Vm::new(config.mem_size, Platform::Bare, 1, backend::Ram::Anonymous)?;
        let output = serial::output(config.serial_capture_limit);
        let bus = Bus::bare(serial::new(IrqLine::None, output.clone()));
        let stop = Arc::<StopShared>::default();
        Self::assemble(vm, vcpus, bus, Platform::Bare, config.mem_size, stop, output)
    }

    /// The machine from its parts, with its vCPU threads started, stopped through `stop`.
    fn assemble(
        vm: backend::Vm,
        vcpus: Vec<backend::Vcpu>,
        bus: Bus,
        platform: Platform,
        mem_size: usize,
        stop: Arc<StopShared>,
        serial: serial::SharedOutput,
    ) -> Result<Self> {
        let exits: Vec<Arc<ExitCounters>> = vcpus.iter().map(|_| Arc::default()).collect();
        let live = Live::spawn(vm, vcpus, bus, &stop, &exits)?;
        Ok(Self::from_live(live, platform, mem_size, stop, exits, serial))
    }

    /// The machine around `live`.
    fn from_live(
        live: Live,
        platform: Platform,
        mem_size: usize,
        stop: Arc<StopShared>,
        exits: Vec<Arc<ExitCounters>>,
        serial: serial::SharedOutput,
    ) -> Self {
        Self {
            platform,
            mem_size,
            cpu_fingerprint: live.vm.cpu_fingerprint().to_owned(),
            vcpu_count: live.vcpus.len() as u32,
            live,
            stop,
            exits,
            serial,
            vsock: None,
            balloon: None,
            network: None,
            disks: Vec::new(),
            extent_stores: 0,
            nested: false,
        }
    }

    /// Creates the arm64 board and loads `boot`'s kernel (an arm64 `Image`) into it, ready to
    /// [`run`]: `config.vcpus` vCPUs on Hypervisor.framework, its GICv3 and timers, PSCI, a 16550
    /// as the console, and a PCI bus with a virtio entropy device (virtio-rng) and a virtio-vsock
    /// device (guest CID 3, the host's end in this process: [`vsock`](Machine::vsock)), and a
    /// virtio-net card if [`MachineConfig::network`] asks for one, the guest's address then
    /// configured on the kernel command line (`ip=`, before `boot`'s); the device tree describes
    /// them all. vCPU 0 starts at the kernel's entry point; the others wait for the kernel to
    /// power them on (PSCI CPU_ON).
    ///
    /// A process has one Hypervisor.framework VM: while this machine exists, making another
    /// fails.
    ///
    /// [`run`]: Machine::run
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

        let stop = Arc::<StopShared>::default();
        // The extent store devices first, so that their DAX windows' BARs come before any
        // transport's.
        let mut bars = crate::devices::pci::BarAllocator::new();
        let mut devices = extent_devices(&config, &vm, &mut bars, &stop, &[])?;
        devices.extend(disk_devices(&config)?);
        devices.extend(pmem_devices(&config, &vm)?);
        let balloon = balloon_device(&config, &vm, &mut devices)?;
        let (vsock_device, vsock) = VsockDevice::new(GUEST_CID)?;
        devices.extend([Box::new(Rng) as Box<dyn VirtioDevice>, Box::new(vsock_device)]);
        if let Some(network) = &network {
            use crate::devices::virtio::net::NetDevice;
            devices.push(Box::new(NetDevice::new(network.shared())?));
        }
        let pci = Arc::new(crate::devices::Pci::new(devices, vm.guest_memory(), &vm, bars)?);
        let output = serial::output(config.serial_capture_limit);
        let uart = IrqLine::Spi(vm.shared().clone(), layout::UART_SPI);
        let bus = Bus::board(serial::new(uart, output.clone()), pci.clone());
        let mut machine =
            Self::assemble(vm, vcpus, bus, Platform::Board, config.mem_size, stop, output)?;
        machine.live.pci = Some(pci);
        machine.balloon = balloon;
        machine.network = network;
        machine.vsock = Some(vsock);
        machine.disks = config.disks;
        machine.extent_stores = config.extent_stores.len();
        machine.nested = config.nested_virtualization;
        Ok(machine)
    }

    /// [`snapshot`](Machine::snapshot), as `options` say: its RAM in a file of its own, for a
    /// live migration, with the caller's data. The arm64 board, its balloon, disks (their files
    /// are the host's: a restore opens them again) and extent store devices with it, not one with
    /// a network or nested virtualization.
    ///
    /// Each extent store device's snapshot is taken first (doc-4): unless `for_migration`, its
    /// guest's filesystem must be frozen (`fsfreeze`, which `VIRTIO_EXTENT_T_FREEZE` tells the
    /// device of), and its cell gets a version, which a snapshot failing after drops. The guest's
    /// virtual counter goes on from the snapshot's count in a restore.
    pub fn snapshot_with(
        &mut self,
        path: impl AsRef<std::path::Path>,
        options: &SnapshotOptions<'_>,
    ) -> Result<()> {
        self.snapshot_with_ram(path, options, |_, _| Ok(()))
    }

    /// [`snapshot_with`](Machine::snapshot_with), and `ram`, given the guest's RAM's host memory
    /// and size, once the state is written, before any device goes on: what its caller keeps of
    /// RAM the snapshot holds none of ([`SnapshotRam::Kept`](crate::SnapshotRam::Kept)), a memory
    /// cell's version or a diff, as of the state's moment. Its failure fails the snapshot, as one
    /// of the state's would: what the extent store devices took for it dropped.
    pub fn snapshot_with_ram<T>(
        &mut self,
        path: impl AsRef<std::path::Path>,
        options: &SnapshotOptions<'_>,
        ram: impl FnOnce(std::ptr::NonNull<u8>, usize) -> std::result::Result<T, String>,
    ) -> Result<T> {
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
        let live = &self.live;
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
            let mem = live.vm.guest_memory();
            crate::snapshot::write(path.as_ref(), &header, &e.finish(), mem, options.ram)?;
            let (host, size) = ram_host_of(mem)?;
            ram(host, size).map_err(Error::Snapshot)
        })();
        quiesced.snapshots_done(taken.is_ok());
        taken
    }

    /// Writes the machine's state to a snapshot file at `path`, between
    /// runs (the `&mut` guarantees no [`Run`] is in progress: every vCPU is
    /// stopped). [`Snapshot::open`](crate::Snapshot::open) then reads it,
    /// and [`Machine::restore`] makes machines from it. The machine itself
    /// is not changed and can run on. See [`snapshot_with`](Machine::snapshot_with)
    /// for what it takes.
    ///
    /// Commands the guest agent runs at the time keep running in every
    /// clone, orphaned (no clone's client knows them); take snapshots of an
    /// idle guest.
    pub fn snapshot(&mut self, path: impl AsRef<std::path::Path>) -> Result<()> {
        self.snapshot_with(path, &SnapshotOptions::default())
    }

    /// Has the machine's extent store devices let go of their cells, between runs: a live
    /// migration's hand-over of them to another host (cladefs's `HandOverCell`), the VM process
    /// and the guest's memory kept. Its devices serve the guest nothing until
    /// [`reopen_extent_cells`](Machine::reopen_extent_cells) takes the cells back; a snapshot is
    /// refused meanwhile. Refused while a device's DAX window holds private pages or its cell
    /// uncommitted open extents. Their daemon must listen at a socket
    /// ([`ExtentDaemon::at_socket`](crate::ExtentDaemon::at_socket)), where they are opened again.
    pub fn release_extent_cells(&mut self) -> Result<()> {
        let pci = self.live.pci.as_ref().expect("a board machine has a PCI bus");
        pci.quiesce().release_cells()
    }

    /// Takes back the cells [`release_extent_cells`](Machine::release_extent_cells) let go of,
    /// between runs: the migration aborted, the machine goes on.
    pub fn reopen_extent_cells(&mut self) -> Result<()> {
        let pci = self.live.pci.as_ref().expect("a board machine has a PCI bus");
        pci.quiesce().reopen_cells()
    }

    /// The GIC's state and the host's clock, every vCPU's (read on its own thread), the UART's
    /// and the RTC's, between runs.
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

    /// What a snapshot's state says the machine is, but for what only a snapshot says (its
    /// extent store devices' snapshots, the caller's data).
    fn header(&self, layout: crate::memory::MemoryLayout) -> crate::snapshot::MachineHeader {
        crate::snapshot::MachineHeader {
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

    /// A new machine from `snapshot`, ready to [`run`](Machine::run) from where the snapshot's
    /// machine stopped. `config` says what machine to build, and must be the snapshot's
    /// ([`Snapshot::config`](crate::Snapshot::config)) but for where its devices are on the
    /// host: its disks' files, its extent store devices' cells (each forked from the snapshot's
    /// version of its cell, or the cell itself reopened, as virtio_extent opens it), with an
    /// extent daemon for them; anything else is refused, never converted. Its RAM is a private
    /// copy-on-write mapping of the snapshot's, and what the guest writes stays in this machine,
    /// never reaching the file or other machines restored from it; its vsock connections from
    /// before the snapshot are reset, and the guest agent reseeds the guest's RNG with host
    /// entropy before running anything ([`agent`](crate::agent)); the guest's virtual counter
    /// goes on from where it was, so no time passes for it while it was in the file.
    pub fn restore(snapshot: &crate::Snapshot, config: MachineConfig) -> Result<Self> {
        Self::restore_with(snapshot, config, None)
    }

    /// [`restore`](Machine::restore), its RAM `ram`, which holds the snapshot's RAM already and
    /// which its caller keeps as the machine runs (a memory cell's window, cladefs's
    /// `MemoryCell::window_in_ram`), rather than a mapping of the snapshot's: of the snapshot's
    /// RAM's size. A snapshot whose RAM its caller kept ([`SnapshotRam::Kept`]) is opened
    /// without it ([`Snapshot::open_without_ram`]).
    ///
    /// [`SnapshotRam::Kept`]: crate::SnapshotRam::Kept
    /// [`Snapshot::open_without_ram`]: crate::Snapshot::open_without_ram
    pub fn restore_onto(
        snapshot: &crate::Snapshot,
        config: MachineConfig,
        ram: Arc<crate::GuestRam>,
    ) -> Result<Self> {
        Self::restore_with(snapshot, config, Some(ram))
    }

    /// [`restore`](Machine::restore), onto `ram` if given ([`restore_onto`](Machine::restore_onto)).
    fn restore_with(
        snapshot: &crate::Snapshot,
        config: MachineConfig,
        ram: Option<Arc<crate::GuestRam>>,
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
        let ram = match &ram {
            Some(ram) => backend::Ram::Given(ram),
            None => {
                backend::Ram::Snapshot { file: snapshot.file()?, offsets: snapshot.ram_offsets() }
            }
        };
        let (vm, vcpus) = backend::Vm::new(config.mem_size, Platform::Board, config.vcpus, ram)?;
        snapshot.check_cpu(vm.cpu_fingerprint())?;
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
        let stop = Arc::<StopShared>::default();
        let mut bars = crate::devices::pci::BarAllocator::new();
        if !config.pmems.is_empty() {
            return Err(Error::Snapshot("a machine with pmem devices is never snapshotted".into()));
        }
        let mut devices = extent_devices(&config, &vm, &mut bars, &stop, snapshot.extent_states())?;
        devices.extend(disk_devices(&config)?);
        let balloon = balloon_device(&config, &vm, &mut devices)?;
        let (vsock_device, vsock) = VsockDevice::new(GUEST_CID)?;
        devices.extend([Box::new(Rng) as Box<dyn VirtioDevice>, Box::new(vsock_device)]);
        let pci = Arc::new(crate::devices::Pci::new(devices, vm.guest_memory(), &vm, bars)?);
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
            Self::from_live(live, Platform::Board, config.mem_size, stop, exits, output);
        machine.vsock = Some(vsock);
        machine.balloon = balloon;
        machine.disks = config.disks;
        machine.extent_stores = config.extent_stores.len();
        Ok(machine)
    }

    /// A stable identifier of the CPU the guest sees (lowercase hex SHA-256):
    /// equal fingerprints mean the same CPU, so a snapshot taken on one host
    /// is CPU-compatible with another.
    pub fn cpu_fingerprint(&self) -> &str {
        &self.cpu_fingerprint
    }

    /// How many vCPUs the machine has.
    pub fn vcpus(&self) -> u32 {
        self.vcpu_count
    }

    /// Size of guest RAM in bytes.
    pub fn mem_size(&self) -> usize {
        self.mem_size
    }

    /// The guest physical memory map: RAM, the MMIO windows, and the device
    /// memory mapped into them now. A snapshot records it, so that a restore
    /// puts everything back at the same addresses.
    pub fn memory_layout(&self) -> crate::memory::MemoryLayout {
        self.live.vm.memory_layout()
    }

    /// The guest's RAM as this process maps it: its one region's host memory, which its guest
    /// and devices change as the machine runs, a memory cell's snapshot reads (cladefs's
    /// `MemoryCell::write_resident`) while it doesn't.
    pub fn ram_host(&self) -> Result<(std::ptr::NonNull<u8>, usize)> {
        ram_host_of(self.live.vm.guest_memory())
    }

    /// Copies `bytes` into guest RAM at guest physical address `addr`.
    pub fn load(&mut self, addr: u64, bytes: &[u8]) -> Result<()> {
        self.live.vm.write_memory(addr, bytes)
    }

    /// Reads `len` bytes of guest RAM starting at guest physical address
    /// `addr`.
    pub fn read_memory(&self, addr: u64, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0; len];
        self.live.vm.read_memory(addr, &mut buf)?;
        Ok(buf)
    }

    /// Points vCPU 0 at `addr`, at EL1 with its MMU off and interrupts masked, as a kernel's
    /// entry is.
    pub fn set_entry(&mut self, addr: u64) -> Result<()> {
        let cpu = crate::arch::aarch64::boot::BootCpu { pc: addr, x: [0; 4] };
        // On its own thread, which created it.
        let job = Box::new(move |v: &mut MachineVcpu| v.vcpu.set_boot_cpu(&cpu));
        self.live.vcpus.on(0, job)
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

    /// The host's side of the machine's balloon, if it has one ([`MachineConfig::balloon`]).
    pub fn balloon(&self) -> Option<&crate::Balloon> {
        self.balloon.as_ref()
    }

    /// The host's end of the machine's vsock (board machines only): connect to
    /// the guest's ports and accept its connections, from any thread,
    /// including while another is in [`run`](Machine::run).
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
    /// created, over all its vCPUs: the accesses to each device, the kicks
    /// that stopped a vCPU, and so on ([`ExitCounts`]). Counting costs the
    /// exit path no measurable time. While the machine runs, a reading may
    /// lag the vCPUs by a few exits.
    pub fn exit_counts(&self) -> ExitCounts {
        self.exits.iter().map(|e| e.read()).sum()
    }

    /// The machine's network, if [`MachineConfig::network`] gave it one:
    /// its policy can change from any thread, including while another is in
    /// [`run`](Machine::run).
    pub fn network(&self) -> Option<crate::net::Network> {
        self.network.clone()
    }

    /// A handle that stops [`run`](Machine::run) from any thread.
    pub fn stop_handle(&self) -> StopHandle {
        StopHandle::new(self.stop.clone())
    }

    /// Runs every vCPU until the guest powers off, resets, triple-faults,
    /// or (on a bare machine) halts or makes an access no device handles, or
    /// a [`StopHandle`] stops it. Whichever vCPU ends the run, every vCPU
    /// stops before `run` returns, and the machine keeps its state: `run`
    /// again resumes them all. An error on any vCPU ends the run the same
    /// way and is returned. Blocks the calling thread while the guest runs.
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
        if stopped && let Some(vsock) = &self.vsock {
            vsock.guest_stopped();
        }
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
        let ready = self.live.vcpus.start(timeout.map(|t| Instant::now() + t));
        Run { machine: self, ready }
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
        match self.machine.live.vcpus.wait(Some(Instant::now() + timeout)) {
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
        self.machine.live.vcpus.stop();
        self.finish()
    }

    /// The machine's exit counts so far ([`Machine::exit_counts`]).
    pub fn exit_counts(&self) -> ExitCounts {
        self.machine.exit_counts()
    }

    fn finish(&mut self) -> Result<ExitReason> {
        let outcome =
            self.machine.live.vcpus.wait(None).expect("a wait without a limit lasts the run");
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
        if self.machine.live.vcpus.in_run() {
            self.machine.live.vcpus.stop();
            let _ = self.finish();
        }
    }
}

impl std::fmt::Debug for Run<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Run").field("machine", &self.machine).finish_non_exhaustive()
    }
}

// Machines are shared across threads by callers, and a run in progress can be waited on from
// another thread.
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
            .field("serial_bytes", &self.output().len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
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

    #[test]
    fn ram_size_limits() {
        use crate::arch::aarch64::layout::MAX_RAM;
        for ok in [4096, 3 << 30, (3 << 30) + 4096, 4 << 30, MAX_RAM as usize] {
            assert!(check_mem_size(ok).is_ok(), "{ok:#x}");
        }
        for bad in [0, 4097, MAX_RAM as usize + 4096, 1 << 40] {
            assert!(matches!(check_mem_size(bad), Err(Error::InvalidConfig(_))), "{bad:#x}");
        }
    }
}
