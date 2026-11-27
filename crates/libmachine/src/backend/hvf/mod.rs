// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! macOS's Hypervisor.framework on Apple Silicon (board 13063): arm64 guests, with the GICv3 the
//! framework emulates (macOS 15 and later), its MSI frame, and its virtual timers.
//!
//! What a kernel hypervisor does in the kernel and the framework doesn't, this backend does in
//! the VMM:
//!
//! - **ioeventfds**: a guest's write to a queue's notify address exits to the vCPU, which
//!   signals the queue's event itself ([`PciHost::register_ioeventfd`]) and goes back in;
//! - **irqfds**: a thread waits on every interrupt event a device signals and sends the MSI its
//!   route names to the GIC ([`irqfd`]);
//! - **PSCI**: a guest's HVC calls to power vCPUs on and off, and the machine off or reset
//!   ([`psci`]);
//! - **WFI**, should it exit: the vCPU's thread sleeps until an interrupt is sent to the GIC or
//!   its virtual timer would fire. With the framework's GIC it does not (no `wait` exit in a
//!   boot, measured 2026-11-26 on macOS 27): the framework waits inside `hv_vcpu_run`, where an
//!   SGI from another vCPU reaches it.
//!
//! The framework gives a process one VM: one machine runs at a time in a process
//! ([`Vm::new`] refuses a second). A vCPU belongs to the thread that created it, and every call
//! about it must come from that thread: a [`Vcpu`] is created on its vCPU thread as the machine
//! is made, one after the other in their order, and destroyed when that thread exits; its state
//! is read and set there for a snapshot ([`state`]).

#[cfg(test)]
mod coherence_tests;
mod ffi;
pub(crate) mod irqfd;
pub(crate) mod kick;
mod psci;
pub(crate) mod ram;
mod state;

pub(crate) use state::{VcpuState, VmState};

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

use self::ffi::*;
use self::kick::CpuSlot;
use super::{Exit, Platform};
use crate::arch::aarch64::boot::BootCpu;
use crate::arch::aarch64::layout::{self, GIC_DIST_ADDR, GIC_MSI_ADDR, GIC_REDIST_ADDR};
use crate::devices::pci::{MsiMessage, PciHost};
use crate::error::{Error, Result};
use crate::exits::ExitKind;
use crate::machine::ExitReason;
use crate::memory::MemoryLayout;
use crate::sys::EventFd;

/// An error of the framework's, for `op`.
fn hv_err(op: &'static str, ret: hv_return_t) -> Error {
    Error::Hypervisor {
        op,
        source: std::io::Error::other(format!("Hypervisor.framework error {ret:#x}")),
    }
}

/// `ret` as a result.
fn check(op: &'static str, ret: hv_return_t) -> Result<()> {
    if ret == HV_SUCCESS { Ok(()) } else { Err(hv_err(op, ret)) }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The process's one VM: held while a [`Vm`] exists.
static VM_HELD: Mutex<bool> = Mutex::new(false);

/// The GIC the framework emulates, where the map puts it, and the sizes the framework gives it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Gic {
    pub(crate) dist_size: u64,
    /// Each vCPU's redistributor.
    pub(crate) redist_size: u64,
    pub(crate) msi_size: u64,
}

/// A VM: its RAM, mapped once, its GIC, and the irqfds, ioeventfds and memory slots the VMM
/// keeps itself.
pub(crate) struct Vm {
    // Field order matters for drop: the irqfd thread stops before the VM goes.
    irqfds: irqfd::Forwarder,
    shared: Arc<Shared>,
    mem: GuestMemoryMmap,
    layout: MemoryLayout,
    gic: Gic,
}

/// What the VM's vCPUs and its irqfd thread share.
pub(crate) struct Shared {
    /// Each vCPU's slot: how to kick it, wake it, power it on.
    pub(crate) cpus: Vec<Arc<CpuSlot>>,
    /// The MSI routes, by GSI.
    msi_routes: Mutex<BTreeMap<u32, MsiMessage>>,
    /// Queue notify addresses and the events a write there signals.
    ioeventfds: RwLock<Vec<(u64, EventFd)>>,
    /// Device memory mapped into the guest, by slot: (guest address, size).
    slots: Mutex<BTreeMap<u32, (u64, u64)>>,
    next_slot: AtomicU32,
    /// Whether the vCPUs have EL2 (nested virtualization): they start there.
    el2: bool,
    /// Each vCPU's redistributor's size: vCPU `i`'s is at `GIC_REDIST_ADDR + i * redist_size`.
    redist_size: u64,
    /// Whether the VM exists, for the DAX windows' maps, which their cells' connections make, and
    /// may make as the machine goes, after its VM: none once it is gone, which mapped nothing
    /// more ([`Shared::map_window`]).
    live: RwLock<bool>,
    /// Its RAM, if its caller gave it ([`Ram::Given`]): a memory cell's window, which maps its
    /// pages anew as the machine runs.
    ram: Option<Arc<ram::GuestRam>>,
}

impl Shared {
    /// Maps the `len` bytes of host memory at `host` into the VM at `guest_addr`, readable and
    /// executable, and writable if `write`: a page of an extent store device's DAX window, as
    /// cladefs's stage maps it (board 13069); nothing once the VM is gone. The VM's guest memory
    /// no longer matches the window if the framework refuses: a panic.
    ///
    /// # Safety
    ///
    /// The memory stays mapped in this process until [`Shared::unmap_window`] takes it out of the
    /// VM.
    pub(crate) unsafe fn map_window(
        &self,
        guest_addr: u64,
        host: std::ptr::NonNull<u8>,
        len: u64,
        write: bool,
    ) {
        let live = self.live.read().unwrap_or_else(|e| e.into_inner());
        if !*live {
            return;
        }
        let flags = HV_MEMORY_READ | HV_MEMORY_EXEC | if write { HV_MEMORY_WRITE } else { 0 };
        // SAFETY: As the caller promises; the VM exists while `live` is held.
        let ret = unsafe { hv_vm_map(host.as_ptr().cast(), guest_addr, len as usize, flags) };
        if let Err(err) = check("map a DAX window's page", ret) {
            panic!("{len:#x} bytes at {guest_addr:#x}: {err}");
        }
    }

    /// Unmaps the `len` bytes at `guest_addr` that [`Shared::map_window`] mapped; nothing once the
    /// VM is gone, which took them with it.
    pub(crate) fn unmap_window(&self, guest_addr: u64, len: u64) {
        let live = self.live.read().unwrap_or_else(|e| e.into_inner());
        if !*live {
            return;
        }
        // SAFETY: Unmaps what `map_window` mapped; the VM exists while `live` is held.
        let ret = unsafe { hv_vm_unmap(guest_addr, len as usize) };
        if let Err(err) = check("unmap a DAX window's page", ret) {
            panic!("{len:#x} bytes at {guest_addr:#x}: {err}");
        }
    }
    /// Gives the host back the `len` bytes of guest RAM at `guest_addr`, at `host` in this
    /// process, whose contents the guest gave up (the balloon's): out of the VM, their host pages
    /// replaced by fresh anonymous memory, and into the VM again, zeros. `madvise` frees no page
    /// Hypervisor.framework maps, as measured (board 13490). Nothing once the VM is gone. The
    /// guest must not touch the range meanwhile, as a balloon's guest doesn't the pages it gives;
    /// a VM that can't have its RAM back is broken: a panic.
    ///
    /// # Safety
    ///
    /// `host..host + len` is guest RAM this VM maps at `guest_addr`, whole host pages of it.
    pub(crate) unsafe fn discard_ram(
        &self,
        guest_addr: u64,
        host: std::ptr::NonNull<u8>,
        len: u64,
    ) {
        let live = self.live.read().unwrap_or_else(|e| e.into_inner());
        if !*live {
            return;
        }
        // RAM its caller gave, which takes what the guest gives back, if it says so: a memory
        // cell's window maps the cell's pages there again.
        if let Some(taken) = self.ram.as_ref().and_then(|ram| ram.given_back()) {
            let offset = guest_addr - layout::RAM_ADDR;
            taken(offset, len).unwrap_or_else(|why| {
                panic!("the RAM's {len:#x} bytes at {offset:#x}, given back: {why}")
            });
            return;
        }
        let len = len as usize;
        // SAFETY: As the caller promises; the VM exists while `live` is held.
        check("unmap guest RAM given back", unsafe { hv_vm_unmap(guest_addr, len) })
            .unwrap_or_else(|err| panic!("{len:#x} bytes at {guest_addr:#x}: {err}"));
        // SAFETY: The range is this process's guest RAM, mapped by it alone, out of the VM now.
        let fresh = unsafe {
            libc::mmap(
                host.as_ptr().cast(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_FIXED | libc::MAP_ANON | libc::MAP_PRIVATE,
                -1,
                0,
            )
        };
        assert_eq!(
            fresh,
            host.as_ptr().cast(),
            "replacing {len:#x} bytes of guest RAM at {guest_addr:#x}: {}",
            std::io::Error::last_os_error()
        );
        let flags = HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC;
        // SAFETY: As the caller promises; mapped back as `Vm::new` mapped it.
        check("map guest RAM given back", unsafe {
            hv_vm_map(host.as_ptr().cast(), guest_addr, len, flags)
        })
        .unwrap_or_else(|err| panic!("{len:#x} bytes at {guest_addr:#x}: {err}"));
    }

    /// Wakes every vCPU waiting for an interrupt: one was just sent to the GIC.
    pub(crate) fn interrupt_sent(&self) {
        for cpu in &self.cpus {
            cpu.wake();
        }
    }

    /// Sends the MSI `gsi`'s route names, if it has one.
    pub(crate) fn send_msi(&self, gsi: u32) {
        let Some(message) = lock(&self.msi_routes).get(&gsi).copied() else {
            return;
        };
        // SAFETY: no pointers; the GIC exists while the VM does.
        let ret = unsafe { hv_gic_send_msi(message.address, message.data) };
        debug_assert_eq!(ret, HV_SUCCESS, "send MSI {message:?}");
        self.interrupt_sent();
    }
}

/// Raises SPI `intid` as an edge: the UART's interrupt.
pub(crate) fn raise_spi(shared: &Shared, intid: u32) -> std::io::Result<()> {
    // SAFETY: no pointers; the GIC exists while the VM does.
    let ret = unsafe {
        let up = hv_gic_set_spi(intid, true);
        let down = hv_gic_set_spi(intid, false);
        if up != HV_SUCCESS { up } else { down }
    };
    if ret != HV_SUCCESS {
        return Err(std::io::Error::other(format!("hv_gic_set_spi({intid}): {ret:#x}")));
    }
    shared.interrupt_sent();
    Ok(())
}

/// Where a new VM's RAM comes from.
pub(crate) enum Ram<'a> {
    /// Anonymous memory, reading as zeroes.
    Anonymous,
    /// A private (copy-on-write) mapping of a snapshot's RAM: each RAM region, in address order,
    /// from its offset in `file`. The guest's writes never reach the file.
    Snapshot { file: &'a Arc<std::fs::File>, offsets: &'a [u64] },
    /// Host memory its caller fills ([`ram::GuestRam`]): one region, the RAM's.
    Given(&'a Arc<ram::GuestRam>),
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
                        libc::MAP_PRIVATE,
                    )
                    .map_err(|e| alloc(&e))?;
                    vm_memory::GuestRegionMmap::new(region, addr)
                        .ok_or_else(|| alloc(&"a RAM region past the address space"))
                })
                .collect::<Result<Vec<_>>>()?;
            GuestMemoryMmap::from_regions(regions).map_err(|e| alloc(&e))
        }
        Ram::Given(ram) => {
            let &[(addr, size)] = ranges else {
                return Err(alloc(&format!("given RAM is one region, not {}", ranges.len())));
            };
            if size != ram.size() {
                return Err(alloc(&format!("RAM of {size} bytes, given {}", ram.size())));
            }
            // SAFETY: The RAM's reservation, of its size, which the VM holds as long as it maps
            // it (`Shared::ram`); not this region's to unmap.
            let region = unsafe {
                vm_memory::MmapRegion::build_raw(
                    ram.host().as_ptr(),
                    size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANON,
                )
            }
            .map_err(|e| alloc(&e))?;
            let region = vm_memory::GuestRegionMmap::new(region, addr)
                .ok_or_else(|| alloc(&"a RAM region past the address space"))?;
            GuestMemoryMmap::from_regions(vec![region]).map_err(|e| alloc(&e))
        }
    }
}

impl Vm {
    /// Creates the process's VM, its GIC and its `vcpu_count` vCPUs' slots; the vCPUs themselves
    /// are created on their threads ([`Vcpu`]). vCPU 0 boots; the others wait for the guest to
    /// power them on (PSCI CPU_ON).
    pub(crate) fn new(
        mem_size: usize,
        platform: Platform,
        vcpu_count: u32,
        ram: Ram<'_>,
    ) -> Result<(Self, Vec<Vcpu>)> {
        Self::with_el2(mem_size, platform, vcpu_count, ram, false)
    }

    /// [`Vm::new`], its vCPUs given EL2 if `el2` (nested virtualization): they start at EL2, as
    /// a kernel that runs VMs of its own boots, which the host must allow (Apple M3 and later).
    pub(crate) fn with_el2(
        mem_size: usize,
        platform: Platform,
        vcpu_count: u32,
        ram: Ram<'_>,
        el2: bool,
    ) -> Result<(Self, Vec<Vcpu>)> {
        // A bare machine is the board without its devices: the GIC is there either way.
        let _ = platform;
        let mut held = lock(&VM_HELD);
        if *held {
            return Err(Error::InvalidConfig(
                "Hypervisor.framework gives a process one VM, and another machine has it".into(),
            ));
        }
        let max = max_vcpus()?;
        if vcpu_count > max {
            return Err(Error::InvalidConfig(format!(
                "{vcpu_count} vCPUs; this host's Hypervisor.framework and libmachine's map allow \
                 at most {max}"
            )));
        }
        let given = match ram {
            Ram::Given(ram) => Some(ram.clone()),
            _ => None,
        };
        create_vm(el2)?;
        // From here the VM exists: a failure destroys it.
        let made = (|| {
            let gic = create_gic(vcpu_count)?;
            let regions: Vec<(GuestAddress, usize)> = layout::ram_regions(mem_size as u64)
                .into_iter()
                .map(|(start, size)| (GuestAddress(start), size as usize))
                .collect();
            let mem = guest_ram(&regions, ram)?;
            if let Some(given) = &given {
                // Mapped as the RAM follows its window's changes: anew as they are made.
                given.bind(regions[0].0.0)?;
                return Ok((gic, mem));
            }
            for &(start, size) in &regions {
                let host = mem.get_host_address(start)?;
                // SAFETY: `mem` maps the region and outlives the VM (field order, and vCPUs
                // keep the mapping: `Vcpu::_mem`).
                let ret = unsafe {
                    hv_vm_map(
                        host.cast(),
                        start.0,
                        size,
                        HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC,
                    )
                };
                check("map guest RAM", ret)?;
            }
            Ok((gic, mem))
        })();
        let (gic, mem) = match made {
            Ok(made) => made,
            Err(e) => {
                // SAFETY: no vCPU exists yet.
                unsafe { hv_vm_destroy() };
                return Err(e);
            }
        };
        *held = true;
        drop(held);

        let cpus: Vec<Arc<CpuSlot>> =
            (0..vcpu_count).map(|i| Arc::new(CpuSlot::new(i == 0))).collect();
        let shared = Arc::new(Shared {
            cpus,
            msi_routes: Mutex::default(),
            ioeventfds: RwLock::default(),
            slots: Mutex::default(),
            next_slot: AtomicU32::new(0),
            live: RwLock::new(true),
            ram: given,
            el2,
            redist_size: gic.redist_size,
        });
        let irqfds = irqfd::Forwarder::spawn(shared.clone())?;
        let vcpus = (0..vcpu_count)
            .map(|index| Vcpu {
                index,
                shared: shared.clone(),
                slot: shared.cpus[index as usize].clone(),
                boot: None,
                pending_read: None,
                data: [0; 8],
                window_failures: cladefs::AccessFailures::default(),
                _mem: mem.clone(),
            })
            .collect();
        let layout = layout::memory_layout(mem_size as u64);
        Ok((Self { irqfds, shared, mem, layout, gic }, vcpus))
    }

    /// The host's CPU, as the guest sees it: the framework shows the host's, so the
    /// fingerprint names the host's chip.
    pub(crate) fn cpu_fingerprint(&self) -> &str {
        host_fingerprint()
    }

    pub(crate) fn guest_memory(&self) -> &GuestMemoryMmap {
        &self.mem
    }

    pub(crate) fn memory_layout(&self) -> MemoryLayout {
        self.layout.clone()
    }

    pub(crate) fn gic(&self) -> Gic {
        self.gic
    }

    pub(crate) fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    pub(crate) fn write_memory(&self, addr: u64, bytes: &[u8]) -> Result<()> {
        Ok(self.mem.write_slice(bytes, GuestAddress(addr))?)
    }

    pub(crate) fn read_memory(&self, addr: u64, buf: &mut [u8]) -> Result<()> {
        Ok(self.mem.read_slice(buf, GuestAddress(addr))?)
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        self.irqfds.stop();
        // No DAX window maps anything after: the VM goes with what they mapped. Nor is given RAM
        // mapped anew in it.
        *self.shared.live.write().unwrap_or_else(|e| e.into_inner()) = false;
        if let Some(ram) = &self.shared.ram {
            ram.unbind();
        }
        for (_, (addr, size)) in std::mem::take(&mut *lock(&self.shared.slots)) {
            // SAFETY: unmaps what map_memory mapped.
            unsafe { hv_vm_unmap(addr, size as usize) };
        }
        // SAFETY: every vCPU was destroyed on its thread, which the machine joined first.
        let ret = unsafe { hv_vm_destroy() };
        debug_assert_eq!(ret, HV_SUCCESS, "destroy the VM");
        *lock(&VM_HELD) = false;
    }
}

impl PciHost for Vm {
    fn set_msi_route(&self, gsi: u32, message: MsiMessage) -> Result<()> {
        lock(&self.shared.msi_routes).insert(gsi, message);
        Ok(())
    }

    fn assign_irqfd(&self, evt: &EventFd, gsi: u32) -> Result<()> {
        self.irqfds.assign(evt, gsi)
    }

    fn deassign_irqfd(&self, evt: &EventFd, gsi: u32) -> Result<()> {
        self.irqfds.deassign(evt, gsi)
    }

    fn register_ioeventfd(&self, evt: &EventFd, addr: u64) -> Result<()> {
        let evt = evt
            .try_clone()
            .map_err(|source| Error::Hypervisor { op: "clone an ioeventfd", source })?;
        self.shared.ioeventfds.write().unwrap_or_else(|e| e.into_inner()).push((addr, evt));
        Ok(())
    }

    fn unregister_ioeventfd(&self, evt: &EventFd, addr: u64) -> Result<()> {
        use std::os::fd::AsRawFd;
        let mut all = self.shared.ioeventfds.write().unwrap_or_else(|e| e.into_inner());
        all.retain(|(a, e)| !(*a == addr && e.as_raw_fd() == evt.as_raw_fd()));
        Ok(())
    }

    unsafe fn map_memory(&self, guest_addr: u64, host_addr: *mut u8, len: u64) -> Result<u32> {
        // SAFETY: the caller keeps the host memory mapped until the slot is unmapped.
        let ret = unsafe {
            hv_vm_map(host_addr.cast(), guest_addr, len as usize, HV_MEMORY_READ | HV_MEMORY_WRITE)
        };
        check("map device memory", ret)?;
        let slot = self.shared.next_slot.fetch_add(1, Ordering::Relaxed);
        lock(&self.shared.slots).insert(slot, (guest_addr, len));
        Ok(slot)
    }

    fn unmap_memory(&self, slot: u32) -> Result<()> {
        if let Some((addr, len)) = lock(&self.shared.slots).remove(&slot) {
            // SAFETY: unmaps what map_memory mapped.
            check("unmap device memory", unsafe { hv_vm_unmap(addr, len as usize) })?;
        }
        Ok(())
    }
}

fn max_vcpus() -> Result<u32> {
    let mut max = 0;
    // SAFETY: writes the count into a live local.
    check("get the most vCPUs", unsafe { hv_vm_get_max_vcpu_count(&mut max) })?;
    Ok(max.min(layout::MAX_VCPUS))
}

/// Creates the process's VM with the map's IPA size and 4 KiB stage 2 pages, where the host
/// can (macOS 26), so that a DAX window maps the guest's pages one by one; and EL2 if `el2`,
/// refused if the host has none.
fn create_vm(el2: bool) -> Result<()> {
    if el2 {
        let mut supported = false;
        // SAFETY: writes into a live local.
        check("ask whether VMs may have EL2", unsafe {
            hv_vm_config_get_el2_supported(&mut supported)
        })?;
        if !supported {
            return Err(Error::InvalidConfig(
                "this host's Hypervisor.framework gives VMs no EL2 (nested virtualization: \
                 Apple M3 and later, macOS 15 and later); nested_virtualization must be false"
                    .into(),
            ));
        }
    }
    let mut max_ipa = 0;
    // SAFETY: writes the size into a live local.
    check("get the largest IPA size", unsafe { hv_vm_config_get_max_ipa_size(&mut max_ipa) })?;
    if max_ipa < layout::IPA_BITS {
        return Err(Error::InvalidConfig(format!(
            "this host's VMs have {max_ipa}-bit guest physical addresses; libmachine's map needs {}",
            layout::IPA_BITS
        )));
    }
    // SAFETY: a new config object, released below.
    let config = unsafe { hv_vm_config_create() };
    if config.is_null() {
        return Err(hv_err("create a VM configuration", -1));
    }
    // SAFETY: `config` is live; no other pointers.
    let ret = unsafe {
        let ipa = hv_vm_config_set_ipa_size(config, layout::IPA_BITS);
        if ipa != HV_SUCCESS {
            ipa
        } else {
            // Before macOS 26, the framework's own granule, 16 KiB.
            if let Some(set_ipa_granule) = hv_vm_config_set_ipa_granule() {
                let _ = set_ipa_granule(config, HV_IPA_GRANULE_4KB);
            }
            match el2 {
                true => match hv_vm_config_set_el2_enabled(config, true) {
                    HV_SUCCESS => hv_vm_create(config),
                    refused => refused,
                },
                false => hv_vm_create(config),
            }
        }
    };
    // SAFETY: the VM keeps what it needs of the config.
    unsafe { os_release(config) };
    check("create the VM (is the binary signed with com.apple.security.hypervisor?)", ret)
}

/// Creates the VM's GIC where the map puts it, with its MSI frame.
fn create_gic(vcpu_count: u32) -> Result<Gic> {
    let (mut dist, mut redist, mut redist_region, mut msi) = (0usize, 0usize, 0usize, 0usize);
    let (mut dist_align, mut redist_align, mut msi_align) = (0usize, 0usize, 0usize);
    let (mut spi_base, mut spi_count) = (0u32, 0u32);
    // SAFETY: each writes into a live local.
    unsafe {
        check("get the GIC distributor's size", hv_gic_get_distributor_size(&mut dist))?;
        check("get a GIC redistributor's size", hv_gic_get_redistributor_size(&mut redist))?;
        check(
            "get the GIC redistributors' size",
            hv_gic_get_redistributor_region_size(&mut redist_region),
        )?;
        check("get the GIC MSI frame's size", hv_gic_get_msi_region_size(&mut msi))?;
        check(
            "get the GIC distributor's alignment",
            hv_gic_get_distributor_base_alignment(&mut dist_align),
        )?;
        check(
            "get the GIC redistributors' alignment",
            hv_gic_get_redistributor_base_alignment(&mut redist_align),
        )?;
        check(
            "get the GIC MSI frame's alignment",
            hv_gic_get_msi_region_base_alignment(&mut msi_align),
        )?;
        check("get the GIC's SPIs", hv_gic_get_spi_interrupt_range(&mut spi_base, &mut spi_count))?;
    }
    let gic = Gic { dist_size: dist as u64, redist_size: redist as u64, msi_size: msi as u64 };
    let aligned = |addr: u64, align: usize| align == 0 || addr.is_multiple_of(align as u64);
    // The framework places a redistributor for every vCPU it supports, whatever the VM has.
    let fits = gic.dist_size <= GIC_MSI_ADDR - GIC_DIST_ADDR
        && gic.msi_size <= layout::UART_ADDR - GIC_MSI_ADDR
        && gic.redist_size <= layout::GIC_REDIST_SIZE
        && redist_region as u64 <= layout::GIC_REDIST_REGION_SIZE
        && aligned(GIC_DIST_ADDR, dist_align)
        && aligned(GIC_REDIST_ADDR, redist_align)
        && aligned(GIC_MSI_ADDR, msi_align)
        && spi_base <= layout::UART_SPI
        && layout::MSI_SPI_BASE + layout::MSI_SPI_COUNT <= spi_base + spi_count;
    if !fits {
        return Err(Error::InvalidConfig(format!(
            "this host's GIC ({gic:?}, redistributors {redist_region:#x}, alignments {dist_align:#x} \
             {redist_align:#x} {msi_align:#x}, SPIs {spi_base}+{spi_count}) does not fit libmachine's \
             map ({vcpu_count} vCPUs)"
        )));
    }
    // SAFETY: a new config object, released below.
    let config = unsafe { hv_gic_config_create() };
    if config.is_null() {
        return Err(hv_err("create a GIC configuration", -1));
    }
    // SAFETY: `config` is live; no other pointers.
    let ret = unsafe {
        [
            hv_gic_config_set_distributor_base(config, GIC_DIST_ADDR),
            hv_gic_config_set_redistributor_base(config, GIC_REDIST_ADDR),
            hv_gic_config_set_msi_region_base(config, GIC_MSI_ADDR),
            hv_gic_config_set_msi_interrupt_range(
                config,
                layout::MSI_SPI_BASE,
                layout::MSI_SPI_COUNT,
            ),
        ]
        .into_iter()
        .find(|&r| r != HV_SUCCESS)
        .unwrap_or_else(|| hv_gic_create(config))
    };
    // SAFETY: the GIC keeps what it needs of the config.
    unsafe { os_release(config) };
    check("create the GIC", ret)?;
    Ok(gic)
}

/// The host's chip, as `machdep.cpu.brand_string` names it.
fn host_fingerprint() -> &'static str {
    static FINGERPRINT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    FINGERPRINT.get_or_init(|| {
        let name = c"machdep.cpu.brand_string";
        let mut buf = [0u8; 128];
        let mut len = buf.len();
        // SAFETY: sysctlbyname writes at most `len` bytes into `buf`, and the new length.
        let ret = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                buf.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        let brand = if ret == 0 {
            String::from_utf8_lossy(&buf[..len]).trim_end_matches('\0').to_owned()
        } else {
            "unknown".to_owned()
        };
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(format!("hvf-arm64:{brand}"));
        digest.iter().map(|b| format!("{b:02x}")).collect()
    })
}

/// An access the guest made that the last exit left unfinished: a load from a device, whose
/// value the bus put in [`Vcpu::data`], for the register it loads.
#[derive(Debug, Clone, Copy)]
struct PendingRead {
    reg: u32,
    size: usize,
    sign_extend: bool,
    /// A 64-bit register (else a 32-bit one, its top half zeroed).
    wide: bool,
}

/// The vCPU this thread owns: created on it, destroyed when it exits.
struct Owned(hv_vcpu_t, *const hv_vcpu_exit_t);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: this thread created the vCPU, and destroys it as it exits.
        unsafe { hv_vcpu_destroy(self.0) };
    }
}

thread_local! {
    static OWNED: RefCell<Option<Owned>> = const { RefCell::new(None) };
}

/// One vCPU of a [`Vm`], created on the thread that runs it ([`Vcpu::create_now`]).
pub(crate) struct Vcpu {
    index: u32,
    shared: Arc<Shared>,
    slot: Arc<CpuSlot>,
    /// The state it starts in once created (vCPU 0's: the kernel's entry).
    boot: Option<BootCpu>,
    pending_read: Option<PendingRead>,
    /// The bytes of the last MMIO exit's access.
    data: [u8; 8],
    /// cladefs: its accesses to a DAX window refused, retried a while before the machine stops
    /// for them, as KVM's VMMs retry a `KVM_RUN` that fails with `EFAULT` (`virtio_extent::vcpu`).
    window_failures: cladefs::AccessFailures,
    _mem: GuestMemoryMmap,
}

impl Vcpu {
    /// Clears a kick that was handled: the run loop checks why it might stop after this.
    pub(crate) fn clear_immediate_exit(&mut self) {
        self.slot.clear_kick();
    }

    /// Finishes the last exit's access: a load from a device gets its value.
    pub(crate) fn complete_pending(&mut self) {
        if let Some(id) = owned() {
            let _ = self.finish_read(id);
        }
    }

    /// What the kick needs to reach this vCPU.
    pub(crate) fn kick_target(&self) -> Arc<CpuSlot> {
        self.slot.clone()
    }

    /// Puts the vCPU in the state a kernel's entry point expects.
    /// On the vCPU's own thread once it exists ([`Vcpus::on`]), set at once; before, from its
    /// creation.
    ///
    /// [`Vcpus::on`]: crate::vcpus::Vcpus::on
    pub(crate) fn set_boot_cpu(&mut self, cpu: &BootCpu) -> Result<()> {
        self.boot = Some(*cpu);
        if owned().is_some() {
            let (id, _) = self.create()?;
            self.apply_boot(id)?;
        }
        Ok(())
    }

    /// Puts the vCPU in its boot state, if it has one to take.
    fn apply_boot(&mut self, id: hv_vcpu_t) -> Result<()> {
        if let Some(boot) = self.boot.take() {
            let mut regs = vec![(HV_REG_PC, boot.pc), (HV_REG_CPSR, self.entry_pstate())];
            regs.extend(boot.x.iter().enumerate().map(|(i, &x)| (HV_REG_X0 + i as u32, x)));
            set_regs(id, &regs)?;
        }
        Ok(())
    }

    /// The vCPU's state for a snapshot, between runs, on its own thread ([`Vcpus::on_each`]):
    /// created now, in its boot state, if it has not run yet.
    ///
    /// [`Vcpus::on_each`]: crate::vcpus::Vcpus::on_each
    pub(crate) fn save_state(&mut self) -> Result<VcpuState> {
        debug_assert!(self.pending_read.is_none(), "a run finishes its last access");
        let (id, _) = self.create()?;
        VcpuState::save(id, self.slot.power())
    }

    /// Sets the vCPU as `state` says, but for its GIC CPU interface
    /// ([`restore_icc`](Self::restore_icc)), on its own thread, before it first runs: its
    /// counter shifted by `shift` host ticks ([`VmState::shift`]).
    pub(crate) fn restore_state(&mut self, state: &VcpuState, shift: u64) -> Result<()> {
        debug_assert!(self.boot.is_none(), "a restored vCPU boots nothing");
        let (id, _) = self.create()?;
        state.restore(id, shift)?;
        self.slot.set_power(state.power);
        Ok(())
    }

    /// Sets the vCPU's GIC CPU interface as `state` says, on its own thread, once the GIC's
    /// state is back ([`VmState::restore_gic`]).
    pub(crate) fn restore_icc(&mut self, state: &VcpuState) -> Result<()> {
        let (id, _) = self.create()?;
        state.restore_icc(id)
    }

    /// Runs the guest until an exit the bus handles, or one the backend handled itself
    /// ([`Exit::Handled`]): a queue notification, a PSCI call, a WFI, a system register.
    pub(crate) fn run_once(&mut self) -> Result<Exit<'_>> {
        if self.slot.take_kick() {
            return Ok(Exit::Interrupted);
        }
        let (id, exit) = self.create()?;
        // A vCPU the guest has not powered on (or has off) waits for CPU_ON.
        match self.slot.wait_until_on() {
            kick::Power::Kicked => return Ok(Exit::Interrupted),
            kick::Power::Start { entry, context } => {
                // As a CPU coming out of reset: MMU and caches off (PSCI CPU_ON's entry state),
                // which a vCPU powered off after running has on, at the EL it boots at.
                let (sctlr, reset) = match self.shared.el2 {
                    true => (HV_SYS_REG_SCTLR_EL2, SCTLR_EL2_RESET),
                    false => (HV_SYS_REG_SCTLR_EL1, SCTLR_EL1_RESET),
                };
                // SAFETY: this thread owns the vCPU.
                check("reset SCTLR", unsafe { hv_vcpu_set_sys_reg(id, sctlr, reset) })?;
                set_regs(
                    id,
                    &[(HV_REG_PC, entry), (HV_REG_X0, context), (HV_REG_CPSR, self.entry_pstate())],
                )?;
            }
            kick::Power::On => {}
        }
        self.finish_read(id)?;
        // SAFETY: this thread owns the vCPU.
        check("run the vCPU", unsafe { hv_vcpu_run(id) })?;
        // SAFETY: the framework keeps the exit information at `exit` for the vCPU's life, and
        // writes it only in hv_vcpu_run, on this thread.
        let exit = unsafe { *exit };
        if !Self::window_abort(&exit) {
            self.window_failures.ran(Instant::now);
        }
        match exit.reason {
            HV_EXIT_REASON_CANCELED => Ok(Exit::Interrupted),
            HV_EXIT_REASON_EXCEPTION => self.exception(id, &exit.exception),
            HV_EXIT_REASON_VTIMER_ACTIVATED => {
                Ok(Exit::Done(ExitReason::Other("the virtual timer fired outside the GIC".into())))
            }
            other => Ok(Exit::Done(ExitReason::Other(format!("hv_vcpu_run exit reason {other}")))),
        }
    }

    /// Creates this thread's vCPU now, if it has none yet: the machine creates them in their
    /// order, one after the other, as they come up ([`Vcpus::on`]).
    ///
    /// [`Vcpus::on`]: crate::vcpus::Vcpus::on
    pub(crate) fn create_now(&mut self) -> Result<()> {
        self.create().map(drop)
    }

    /// This thread's vCPU, created now if it has none yet, in its boot state, its redistributor
    /// where its index puts it (refused otherwise: the device tree says so to the guest).
    ///
    /// Every machine creates its vCPUs in their order ([`create_now`](Self::create_now)), the
    /// framework's handles counting up with them, vCPU `i`'s handle `i` (refused otherwise):
    /// created as their threads raced, a restore's order differed from the snapshot's machine's,
    /// and one restore in twenty-odd, under load, had a vCPU never interrupted again, the GIC's
    /// state going back to the vCPUs by handle.
    fn create(&mut self) -> Result<(hv_vcpu_t, *const hv_vcpu_exit_t)> {
        if let Some(owned) = OWNED.with(|o| o.borrow().as_ref().map(|o| (o.0, o.1))) {
            return Ok(owned);
        }
        let (mut id, mut exit) = (0, std::ptr::null());
        // SAFETY: writes the new vCPU's ID and exit pointer into live locals.
        check("create a vCPU", unsafe {
            hv_vcpu_create(&mut id, &mut exit, std::ptr::null_mut())
        })?;
        OWNED.with(|o| *o.borrow_mut() = Some(Owned(id, exit)));
        if id != u64::from(self.index) {
            return Err(Error::Hypervisor {
                op: "create a vCPU",
                source: std::io::Error::other(format!(
                    "vCPU {} got the framework's handle {id}: the vCPUs were not created in their \
                     order, as a snapshot's GIC state needs them",
                    self.index
                )),
            });
        }
        // SAFETY: this thread owns the vCPU.
        check("set MPIDR_EL1", unsafe {
            hv_vcpu_set_sys_reg(id, HV_SYS_REG_MPIDR_EL1, mpidr(self.index))
        })?;
        let mut redist = 0;
        // SAFETY: writes into a live local; the vCPU's affinity is set.
        check("get a vCPU's GIC redistributor", unsafe {
            hv_gic_get_redistributor_base(id, &mut redist)
        })?;
        let expected = GIC_REDIST_ADDR + u64::from(self.index) * self.shared.redist_size;
        if redist != expected {
            return Err(Error::Hypervisor {
                op: "create a vCPU",
                source: std::io::Error::other(format!(
                    "vCPU {} has the GIC redistributor at {redist:#x}, not {expected:#x}",
                    self.index
                )),
            });
        }
        if self.shared.el2 {
            hide_from_el2(id)?;
        }
        self.apply_boot(id)?;
        self.slot.created(id);
        Ok((id, exit))
    }

    /// PSTATE at a kernel's entry: EL2 with EL2's stack pointer if the vCPU has EL2, else EL1's,
    /// everything masked.
    fn entry_pstate(&self) -> u64 {
        match self.shared.el2 {
            true => PSTATE_EL2H_MASKED,
            false => PSTATE_EL1H_MASKED,
        }
    }

    /// Gives a pending load its value and moves past it.
    fn finish_read(&mut self, id: hv_vcpu_t) -> Result<()> {
        let Some(read) = self.pending_read.take() else {
            return Ok(());
        };
        let mut value = 0u64;
        for (i, b) in self.data[..read.size].iter().enumerate() {
            value |= u64::from(*b) << (8 * i);
        }
        if read.sign_extend {
            let shift = 64 - 8 * read.size as u32;
            value = ((value << shift) as i64 >> shift) as u64;
        }
        if !read.wide {
            value &= 0xffff_ffff;
        }
        if read.reg != 31 {
            set_regs(id, &[(HV_REG_X0 + read.reg, value)])?;
        }
        advance_pc(id)
    }

    fn exception(&mut self, id: hv_vcpu_t, e: &hv_vcpu_exit_exception_t) -> Result<Exit<'_>> {
        let syndrome = e.syndrome;
        let class = (syndrome >> 26) & 0x3f;
        match class {
            // An access to given RAM as it was being mapped anew: retried once it is.
            EC_DATA_ABORT_LOWER | EC_INSTRUCTION_ABORT_LOWER
                if self.shared.ram.as_ref().is_some_and(|ram| ram.holds(e.physical_address)) =>
            {
                self.shared.ram.as_ref().expect("given RAM").wait();
                Ok(Exit::Handled(ExitKind::AccessRetried))
            }
            EC_DATA_ABORT_LOWER | EC_INSTRUCTION_ABORT_LOWER => {
                // A cache maintenance instruction's abort (CM) says it writes (WnR), which it
                // doesn't: a load's, as far as the window goes.
                let write = class == EC_DATA_ABORT_LOWER
                    && syndrome & ESR_WNR != 0
                    && syndrome & ESR_CM == 0;
                match virtio_extent::window_fault(e.physical_address, write) {
                    // The page mapped, the access is retried.
                    Some(Ok(())) => Ok(Exit::Handled(ExitKind::AccessRetried)),
                    Some(Err(errno)) => {
                        let why = format!(
                            "a guest access to its DAX window at {:#x} ({}) failed: {errno}",
                            e.physical_address,
                            if write { "write" } else { "read" }
                        );
                        // ENOTCONN: the window's daemon, or its cell, went. Anything else is the
                        // guest's access refused (`HvfSecondStage::fault`), retried a while first,
                        // as KVM's VMMs retry it (`cladefs::AccessFailures`, virtio_extent::vcpu),
                        // before the machine stops saying where the guest was (board 14531).
                        if errno == cladefs::Errno::ENOTCONN {
                            return Err(Error::ExtentDaemonLost(why));
                        }
                        match self.window_failures.failed(Instant::now()) {
                            cladefs::Failure::Retry { .. } => {
                                Ok(Exit::Handled(ExitKind::AccessRetried))
                            }
                            // Where the guest was, for its kernel's symbols to say what it did.
                            cladefs::Failure::GiveUp(run) => Err(Error::AccessRefused(format!(
                                "{why}, {} times in {:?}, at virtual address {:#x}, the guest's pc \
                                 {:#x}, lr {:#x}",
                                run.failures,
                                run.last - run.first,
                                e.virtual_address,
                                get_reg(id, HV_REG_PC)?,
                                get_reg(id, HV_REG_X0 + 30)?
                            ))),
                        }
                    }
                    None if class == EC_DATA_ABORT_LOWER => {
                        self.data_abort(id, syndrome, e.physical_address)
                    }
                    None => Ok(Exit::Done(ExitReason::Other(format!(
                        "an instruction fetch from {:#x}, which no memory backs (ESR {syndrome:#x})",
                        e.physical_address
                    )))),
                }
            }
            EC_HVC64 => {
                let x = |i: u32| get_reg(id, HV_REG_X0 + i);
                let call = psci::Call { function: x(0)? as u32, args: [x(1)?, x(2)?, x(3)?] };
                match psci::handle(&call, &self.shared.cpus) {
                    psci::Outcome::Return(value) => {
                        set_regs(id, &[(HV_REG_X0, value as u64)])?;
                        Ok(Exit::Handled(ExitKind::Hypercall))
                    }
                    psci::Outcome::Off => {
                        self.slot.power_off();
                        Ok(Exit::Handled(ExitKind::Hypercall))
                    }
                    psci::Outcome::Machine(reason) => Ok(Exit::Done(reason)),
                }
            }
            EC_WFX => {
                advance_pc(id)?;
                // WFE (bit 0): a hint, nothing to wait for here.
                if syndrome & 1 == 0 {
                    self.slot.wait_for_interrupt(timer_deadline(id)?);
                }
                Ok(Exit::Handled(ExitKind::Wait))
            }
            EC_SYSREG => {
                let read = syndrome & 1 == 1;
                let rt = ((syndrome >> 5) & 0x1f) as u32;
                // The register, as the framework numbers its own (`hv_sys_reg_t`): op0, op1, CRn,
                // CRm, op2, as MRS and MSR encode them.
                let reg = (((syndrome >> 20) & 0x3) << 14
                    | ((syndrome >> 14) & 0x7) << 11
                    | ((syndrome >> 10) & 0xf) << 7
                    | ((syndrome >> 1) & 0xf) << 3
                    | ((syndrome >> 17) & 0x7)) as hv_sys_reg_t;
                // One the framework keeps, which it hands over with EL2 (a guest hypervisor's
                // CNTHCTL_EL2 and the like), is the vCPU's own; the rest (debug, OS lock, LORegions)
                // read as zero, their writes ignored, as nothing is behind them here.
                if read {
                    let mut value = 0;
                    // SAFETY: this thread owns the vCPU; writes into a live local.
                    let kept = unsafe { hv_vcpu_get_sys_reg(id, reg, &mut value) } == HV_SUCCESS;
                    if rt != 31 {
                        set_regs(id, &[(HV_REG_X0 + rt, if kept { value } else { 0 })])?;
                    }
                } else {
                    let value = if rt == 31 { 0 } else { get_reg(id, HV_REG_X0 + rt)? };
                    // SAFETY: this thread owns the vCPU. A register the framework doesn't keep is
                    // refused, the write ignored.
                    let _ = unsafe { hv_vcpu_set_sys_reg(id, reg, value) };
                }
                advance_pc(id)?;
                Ok(Exit::Handled(ExitKind::SystemRegister))
            }
            _ => Ok(Exit::Done(ExitReason::Other(format!(
                "exception class {class:#x} (ESR {syndrome:#x}) at guest address {:#x}",
                e.physical_address
            )))),
        }
    }

    /// cladefs: whether `exit` is an abort on a DAX window, which [`Self::exception`] resolves,
    /// retrying it while it is refused: a refused access's run of failures
    /// ([`cladefs::AccessFailures`]) goes on through it; every other exit counts as the vCPU
    /// running on.
    fn window_abort(exit: &hv_vcpu_exit_t) -> bool {
        let class = (exit.exception.syndrome >> 26) & 0x3f;
        exit.reason == HV_EXIT_REASON_EXCEPTION
            && matches!(class, EC_DATA_ABORT_LOWER | EC_INSTRUCTION_ABORT_LOWER)
            && virtio_extent::is_window(exit.exception.physical_address)
    }

    /// A load or store to an address no memory backs: a device's register, or a queue's notify
    /// address.
    fn data_abort(&mut self, id: hv_vcpu_t, syndrome: u64, addr: u64) -> Result<Exit<'_>> {
        // ISV: the syndrome describes the access. Linux's MMIO accessors are single loads and
        // stores, which it always does.
        if syndrome & (1 << 24) == 0 {
            return Ok(Exit::Done(ExitReason::Other(format!(
                "an MMIO access to {addr:#x} the hypervisor could not decode (ESR {syndrome:#x})"
            ))));
        }
        let size = 1usize << ((syndrome >> 22) & 0x3);
        let reg = ((syndrome >> 16) & 0x1f) as u32;
        let write = syndrome & ESR_WNR != 0;
        if write {
            let value = if reg == 31 { 0 } else { get_reg(id, HV_REG_X0 + reg)? };
            self.data[..size].copy_from_slice(&value.to_le_bytes()[..size]);
            advance_pc(id)?;
            if self.notify(addr) {
                return Ok(Exit::Handled(ExitKind::PciBar));
            }
            return Ok(Exit::MmioWrite { addr, data: &self.data[..size] });
        }
        self.pending_read = Some(PendingRead {
            reg,
            size,
            sign_extend: syndrome & (1 << 21) != 0,
            wide: syndrome & (1 << 15) != 0,
        });
        Ok(Exit::MmioRead { addr, data: &mut self.data[..size] })
    }

    /// Signals the queue whose notify address `addr` is, if it is one: an ioeventfd.
    fn notify(&self, addr: u64) -> bool {
        let all = self.shared.ioeventfds.read().unwrap_or_else(|e| e.into_inner());
        match all.iter().find(|(a, _)| *a == addr) {
            Some((_, evt)) => {
                let _ = evt.write(1);
                true
            }
            None => false,
        }
    }
}

// SAFETY: the exit pointer and the vCPU it belongs to are used only on the thread that created
// them (`OWNED`); a `Vcpu` moved to another thread creates its own there.
unsafe impl Send for Vcpu {}

/// The exception classes (ESR_EL2.EC) the backend handles.
const EC_WFX: u64 = 0x01;
const EC_HVC64: u64 = 0x16;
const EC_SYSREG: u64 = 0x18;
const EC_DATA_ABORT_LOWER: u64 = 0x24;
const EC_INSTRUCTION_ABORT_LOWER: u64 = 0x20;
/// A data abort's syndrome: its access a write (WnR), a cache maintenance instruction's (CM).
const ESR_WNR: u64 = 1 << 6;
const ESR_CM: u64 = 1 << 8;

/// PSTATE at a kernel's entry: EL1 with SP_EL1 (EL1h), debug, SError, IRQ and FIQ masked.
const PSTATE_EL1H_MASKED: u64 = 0x3c5;
/// The same at EL2 (EL2h), a vCPU with EL2's.
const PSTATE_EL2H_MASKED: u64 = 0x3c9;
/// SCTLR_EL1 out of reset: its RES1 bits (29, 28, 23, 22, 20, 11), the MMU and caches off.
const SCTLR_EL1_RESET: u64 = 0x30d0_0800;
/// SCTLR_EL2 out of reset, without VHE: its RES1 bits (29, 28, 23, 22, 18, 16, 11, 5, 4), the MMU
/// and caches off.
const SCTLR_EL2_RESET: u64 = 0x30c5_0830;

/// The MPIDR_EL1 of vCPU `index`: 16 to a cluster (Aff0), as GICv3's SGIs address them.
pub(crate) fn mpidr(index: u32) -> u64 {
    u64::from(index % 16) | (u64::from(index / 16) << 8)
}

/// Hides from a vCPU with EL2 what the framework's EL2 lacks though the CPU has it, so that the
/// guest never uses it: SME (ID_AA64PFR1_EL1.SME), which Apple's nested virtualization doesn't
/// implement, its SMCR_EL2 undefined there, the kernel's write of it at boot an exception it never
/// returns from (QEMU hides it so too).
fn hide_from_el2(id: hv_vcpu_t) -> Result<()> {
    const SME: u64 = 0xf << 24;
    let mut pfr1 = 0;
    // SAFETY: this thread owns the vCPU; writes into a live local.
    check("read ID_AA64PFR1_EL1", unsafe {
        hv_vcpu_get_sys_reg(id, HV_SYS_REG_ID_AA64PFR1_EL1, &mut pfr1)
    })?;
    // SAFETY: this thread owns the vCPU.
    check("write ID_AA64PFR1_EL1", unsafe {
        hv_vcpu_set_sys_reg(id, HV_SYS_REG_ID_AA64PFR1_EL1, pfr1 & !SME)
    })
}

/// This thread's vCPU, if it has created one.
fn owned() -> Option<hv_vcpu_t> {
    OWNED.with(|o| o.borrow().as_ref().map(|o| o.0))
}

fn get_reg(id: hv_vcpu_t, reg: hv_reg_t) -> Result<u64> {
    let mut value = 0;
    // SAFETY: this thread owns the vCPU; writes into a live local.
    check("get a vCPU register", unsafe { hv_vcpu_get_reg(id, reg, &mut value) })?;
    Ok(value)
}

fn set_regs(id: hv_vcpu_t, regs: &[(hv_reg_t, u64)]) -> Result<()> {
    for &(reg, value) in regs {
        // SAFETY: this thread owns the vCPU.
        check("set a vCPU register", unsafe { hv_vcpu_set_reg(id, reg, value) })?;
    }
    Ok(())
}

/// Moves past the instruction that exited: every one that exits here is 4 bytes.
fn advance_pc(id: hv_vcpu_t) -> Result<()> {
    let pc = get_reg(id, HV_REG_PC)?;
    set_regs(id, &[(HV_REG_PC, pc + 4)])
}

/// When the vCPU's virtual timer fires, if it is on and unmasked: how long a WFI may sleep.
fn timer_deadline(id: hv_vcpu_t) -> Result<Option<Instant>> {
    let (mut ctl, mut cval, mut offset) = (0u64, 0u64, 0u64);
    // SAFETY: this thread owns the vCPU; writes into live locals.
    unsafe {
        check("get CNTV_CTL_EL0", hv_vcpu_get_sys_reg(id, HV_SYS_REG_CNTV_CTL_EL0, &mut ctl))?;
        check("get CNTV_CVAL_EL0", hv_vcpu_get_sys_reg(id, HV_SYS_REG_CNTV_CVAL_EL0, &mut cval))?;
        check("get the vtimer offset", hv_vcpu_get_vtimer_offset(id, &mut offset))?;
    }
    // ENABLE set, IMASK clear.
    if ctl & 0b11 != 0b01 {
        return Ok(None);
    }
    // SAFETY: no arguments.
    let now = unsafe { mach_absolute_time() }.wrapping_sub(offset);
    let ticks = cval.saturating_sub(now);
    Ok(Some(Instant::now() + ticks_to_duration(ticks)))
}

/// `ticks` of the host's counter as time.
fn ticks_to_duration(ticks: u64) -> Duration {
    static TIMEBASE: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();
    let &(numer, denom) = TIMEBASE.get_or_init(|| {
        let mut info = mach_timebase_info_t::default();
        // SAFETY: writes into a live local.
        unsafe { mach_timebase_info(&mut info) };
        (u64::from(info.numer), u64::from(info.denom.max(1)))
    });
    Duration::from_nanos((u128::from(ticks) * u128::from(numer) / u128::from(denom)) as u64)
}
