// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! PCI: segment 0's bus 0 behind a host bridge the ACPI tables describe
//! (`\_SB.PCI0`), configuration space through ECAM only, memory BARs from
//! the platform's two windows, and interrupts by MSI-X only.
//!
//! There is no host bridge function at 00.0: the OS finds the root bridge
//! in ACPI, and a function there would only borrow some vendor's ID.
//! Devices take device numbers from 1, function 0.

pub(crate) mod config;
pub(crate) mod msix;

use std::ops::Range;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use crate::sys::EventFd;

use crate::arch::layout::{
    PCI_ECAM_ADDR, PCI_ECAM_LAST_BUS, PCI_ECAM_SIZE, PCI_MMIO32_ADDR, PCI_MMIO32_SIZE,
    PCI_MMIO64_ADDR, PCI_MMIO64_SIZE,
};
use crate::error::{Error, Result};

/// An MSI message: what a vector writes, and where.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MsiMessage {
    pub(crate) address: u64,
    pub(crate) data: u32,
}

/// What PCI functions need from the hypervisor: interrupt routes, irqfds,
/// ioeventfds and memory slots. The HVF backend implements it; unit tests
/// record the calls.
pub(crate) trait PciHost {
    /// Routes `gsi` to the MSI `message`.
    fn set_msi_route(&self, gsi: u32, message: MsiMessage) -> Result<()>;
    /// Makes a signal on `evt` raise `gsi`.
    fn assign_irqfd(&self, evt: &EventFd, gsi: u32) -> Result<()>;
    fn deassign_irqfd(&self, evt: &EventFd, gsi: u32) -> Result<()>;
    /// Makes a guest write of any size to `addr` signal `evt` instead of
    /// exiting to the VMM.
    fn register_ioeventfd(&self, evt: &EventFd, addr: u64) -> Result<()>;
    fn unregister_ioeventfd(&self, evt: &EventFd, addr: u64) -> Result<()>;
    /// Maps `len` bytes of host memory at `host_addr` into the guest at
    /// `guest_addr`, and returns the mapping's slot.
    ///
    /// # Safety
    /// The host memory must stay mapped until the slot is unmapped or the
    /// VM is destroyed.
    unsafe fn map_memory(&self, guest_addr: u64, host_addr: *mut u8, len: u64) -> Result<u32>;
    fn unmap_memory(&self, slot: u32) -> Result<()>;
}

/// A PCI function on the bus.
pub(crate) trait PciFunction: Send {
    fn config_read(&mut self, offset: usize, data: &mut [u8]);
    fn config_write(&mut self, offset: usize, data: &[u8], host: &dyn PciHost) -> Result<()>;
    /// The BARs whose accesses exit to the VMM, as (BAR, address, size),
    /// where they are decoded now.
    fn trapped_bars(&self) -> Vec<(usize, u64, u64)>;
    fn bar_read(&mut self, bar: usize, offset: u64, data: &mut [u8]);
    fn bar_write(&mut self, bar: usize, offset: u64, data: &[u8], host: &dyn PciHost)
    -> Result<()>;
}

/// Hands out BAR addresses from the platform's windows, each naturally
/// aligned, as firmware does before the OS boots.
pub(crate) struct BarAllocator {
    next32: u64,
    next64: u64,
}

impl BarAllocator {
    pub(crate) fn new() -> Self {
        Self { next32: PCI_MMIO32_ADDR, next64: PCI_MMIO64_ADDR }
    }

    pub(crate) fn window32() -> Range<u64> {
        PCI_MMIO32_ADDR..PCI_MMIO32_ADDR + PCI_MMIO32_SIZE
    }

    pub(crate) fn window64() -> Range<u64> {
        PCI_MMIO64_ADDR..PCI_MMIO64_ADDR + PCI_MMIO64_SIZE
    }

    /// An address for a BAR of `size` (a power of two) in the 32-bit or
    /// the 64-bit window.
    pub(crate) fn allocate(&mut self, size: u64, is_64bit: bool) -> Result<u64> {
        assert!(size.is_power_of_two());
        let (next, window) = if is_64bit {
            (&mut self.next64, Self::window64())
        } else {
            (&mut self.next32, Self::window32())
        };
        let addr = next.next_multiple_of(size);
        match addr.checked_add(size) {
            Some(end) if end <= window.end => {
                *next = end;
                Ok(addr)
            }
            _ => Err(Error::InvalidConfig(format!(
                "a {size:#x}-byte BAR does not fit in the PCI window {window:#x?}"
            ))),
        }
    }
}

// The ACPI tables (MCFG, the root bridge's _CRS) describe the buses the
// layout names, and this bus is bus 0 alone: the guest probes every bus it is
// told of, at a cost (dead-ends/ecam-all-256-buses.md), so the two must agree.
const _: () = assert!(PCI_ECAM_LAST_BUS == 0, "describe exactly the buses PciBus serves");

/// Segment 0's bus 0, reached from every vCPU thread: each function has its
/// own lock, and the trapped-BAR table a read-write lock that configuration
/// writes take to update it. Lock order: the table, then a function; a
/// function's lock is never held while the table's is taken.
#[derive(Default)]
pub(crate) struct PciBus {
    /// Each device number's function 0.
    functions: Vec<(u8, Arc<Mutex<dyn PciFunction>>)>,
    /// Trapped BARs: (address range, index in `functions`, BAR).
    bars: RwLock<Vec<(Range<u64>, usize, usize)>>,
}

fn lock<'a>(f: &'a Mutex<dyn PciFunction + 'static>) -> MutexGuard<'a, dyn PciFunction + 'static> {
    f.lock().unwrap_or_else(|e| e.into_inner())
}

impl PciBus {
    /// Puts `function` at device number `device`, function 0.
    pub(crate) fn add(&mut self, device: u8, function: Arc<Mutex<dyn PciFunction>>) {
        assert!(device < 32 && self.functions.iter().all(|(d, _)| *d != device));
        self.functions.push((device, function));
        self.refresh_bars();
    }

    /// Takes the functions' trapped BARs again (after a restore moved them).
    pub(crate) fn refresh_bars(&self) {
        let mut bars = self.bars.write().unwrap_or_else(|e| e.into_inner());
        *bars = self
            .functions
            .iter()
            .enumerate()
            .flat_map(|(i, (_, f))| {
                lock(f)
                    .trapped_bars()
                    .into_iter()
                    .map(move |(bar, addr, size)| (addr..addr + size, i, bar))
            })
            .collect();
    }

    /// The function and register an ECAM address names, if a function is
    /// there.
    fn ecam_target(&self, addr: u64, len: usize) -> Option<(usize, usize)> {
        let off = addr.checked_sub(PCI_ECAM_ADDR).filter(|&o| o < PCI_ECAM_SIZE)?;
        let (bus, device, function) = (off >> 20, (off >> 15) & 0x1f, (off >> 12) & 0x7);
        let reg = (off & 0xfff) as usize;
        // An access must not cross its dword (PCIe 6.0, 7.2.2).
        if bus != 0 || function != 0 || reg % 4 + len > 4 {
            return None;
        }
        let index = self.functions.iter().position(|(d, _)| u64::from(*d) == device)?;
        Some((index, reg))
    }

    fn bar_target(&self, addr: u64) -> Option<(usize, usize, u64)> {
        self.bars
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|(range, _, _)| range.contains(&addr))
            .map(|(range, i, bar)| (*i, *bar, addr - range.start))
    }

    /// Whether `addr` is the bus's: ECAM, or a trapped BAR.
    #[cfg(test)]
    pub(crate) fn claims(&self, addr: u64) -> bool {
        (PCI_ECAM_ADDR..PCI_ECAM_ADDR + PCI_ECAM_SIZE).contains(&addr)
            || self.bar_target(addr).is_some()
    }

    /// A guest read. Returns false if the address is not the bus's.
    /// Configuration space with no function behind it reads as all ones.
    pub(crate) fn mmio_read(&self, addr: u64, data: &mut [u8]) -> bool {
        if (PCI_ECAM_ADDR..PCI_ECAM_ADDR + PCI_ECAM_SIZE).contains(&addr) {
            match self.ecam_target(addr, data.len()) {
                Some((i, reg)) => lock(&self.functions[i].1).config_read(reg, data),
                None => data.fill(0xff),
            }
            return true;
        }
        match self.bar_target(addr) {
            Some((i, bar, offset)) => {
                lock(&self.functions[i].1).bar_read(bar, offset, data);
                true
            }
            None => false,
        }
    }

    /// A guest write. Returns false if the address is not the bus's.
    pub(crate) fn mmio_write(&self, addr: u64, data: &[u8], host: &dyn PciHost) -> Result<bool> {
        if (PCI_ECAM_ADDR..PCI_ECAM_ADDR + PCI_ECAM_SIZE).contains(&addr) {
            if let Some((i, reg)) = self.ecam_target(addr, data.len()) {
                lock(&self.functions[i].1).config_write(reg, data, host)?;
                // The write may have moved a BAR or turned decode on or off.
                self.refresh_bars();
            }
            return Ok(true);
        }
        match self.bar_target(addr) {
            Some((i, bar, offset)) => {
                lock(&self.functions[i].1).bar_write(bar, offset, data, host)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A call a function made to its host.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum HostCall {
        Route(u32, MsiMessage),
        Assign(u32),
        Deassign(u32),
        Ioeventfd(u64),
        NoIoeventfd(u64),
        Map(u32, u64, u64),
        Unmap(u32),
    }

    /// A host that records calls, for unit tests.
    #[derive(Default)]
    pub(crate) struct FakeHost {
        calls: Mutex<Vec<HostCall>>,
        next_slot: Mutex<u32>,
    }

    impl FakeHost {
        pub(crate) fn calls(&self) -> Vec<HostCall> {
            self.calls.lock().unwrap().clone()
        }

        pub(crate) fn take(&self) -> Vec<HostCall> {
            std::mem::take(&mut self.calls.lock().unwrap())
        }

        fn record(&self, call: HostCall) {
            self.calls.lock().unwrap().push(call);
        }
    }

    impl PciHost for FakeHost {
        fn set_msi_route(&self, gsi: u32, message: MsiMessage) -> Result<()> {
            self.record(HostCall::Route(gsi, message));
            Ok(())
        }
        fn assign_irqfd(&self, _: &EventFd, gsi: u32) -> Result<()> {
            self.record(HostCall::Assign(gsi));
            Ok(())
        }
        fn deassign_irqfd(&self, _: &EventFd, gsi: u32) -> Result<()> {
            self.record(HostCall::Deassign(gsi));
            Ok(())
        }
        fn register_ioeventfd(&self, _: &EventFd, addr: u64) -> Result<()> {
            self.record(HostCall::Ioeventfd(addr));
            Ok(())
        }
        fn unregister_ioeventfd(&self, _: &EventFd, addr: u64) -> Result<()> {
            self.record(HostCall::NoIoeventfd(addr));
            Ok(())
        }
        unsafe fn map_memory(&self, guest_addr: u64, _: *mut u8, len: u64) -> Result<u32> {
            let mut slot = self.next_slot.lock().unwrap();
            *slot += 1;
            self.record(HostCall::Map(*slot, guest_addr, len));
            Ok(*slot)
        }
        fn unmap_memory(&self, slot: u32) -> Result<()> {
            self.record(HostCall::Unmap(slot));
            Ok(())
        }
    }

    #[test]
    fn bars_are_allocated_aligned_inside_their_windows() {
        let mut a = BarAllocator::new();
        assert_eq!(a.allocate(0x1000, false).unwrap(), PCI_MMIO32_ADDR);
        assert_eq!(a.allocate(0x1_0000, false).unwrap(), PCI_MMIO32_ADDR + 0x1_0000, "aligned up");
        assert_eq!(a.allocate(0x1000, false).unwrap(), PCI_MMIO32_ADDR + 0x2_0000);
        assert_eq!(a.allocate(1 << 30, true).unwrap(), PCI_MMIO64_ADDR);
        assert_eq!(a.allocate(64 << 30, true).unwrap(), PCI_MMIO64_ADDR + (64 << 30));
        // The 32-bit window holds 512 MiB.
        assert!(a.allocate(512 << 20, false).is_err());
        assert_eq!(a.allocate(256 << 20, false).unwrap(), PCI_MMIO32_ADDR + (256 << 20));
        assert!(a.allocate(256 << 20, false).is_err());
        // The 64-bit window holds 1 TiB.
        assert!(a.allocate(1 << 40, true).is_err());
    }

    /// A function with a 4 KiB trapped BAR 0 that records accesses.
    struct Recorder {
        config: config::ConfigSpace,
        log: Vec<String>,
    }

    impl PciFunction for Recorder {
        fn config_read(&mut self, offset: usize, data: &mut [u8]) {
            self.config.read(offset, data);
        }
        fn config_write(&mut self, offset: usize, data: &[u8], _: &dyn PciHost) -> Result<()> {
            self.config.write(offset, data);
            Ok(())
        }
        fn trapped_bars(&self) -> Vec<(usize, u64, u64)> {
            self.config.decoded_bars()[0].map(|a| (0, a, 0x1000)).into_iter().collect()
        }
        fn bar_read(&mut self, bar: usize, offset: u64, data: &mut [u8]) {
            self.log.push(format!("read {bar} {offset:#x} {}", data.len()));
            data.fill(0x5a);
        }
        fn bar_write(
            &mut self,
            bar: usize,
            offset: u64,
            data: &[u8],
            _: &dyn PciHost,
        ) -> Result<()> {
            self.log.push(format!("write {bar} {offset:#x} {data:?}"));
            Ok(())
        }
    }

    fn recorder() -> Arc<Mutex<Recorder>> {
        let mut config = config::ConfigSpace::new(config::Header {
            vendor_id: 0x1af4,
            device_id: 0x1044,
            revision_id: 1,
            class_code: 0xff0000,
            subsystem_vendor_id: 0x1af4,
            subsystem_id: 4,
        });
        let bar = config::Bar {
            size: 0x1000,
            is_64bit: false,
            prefetchable: false,
            window: BarAllocator::window32(),
        };
        config.add_bar(0, bar, PCI_MMIO32_ADDR);
        config.set_memory_decode(true);
        Arc::new(Mutex::new(Recorder { config, log: Vec::new() }))
    }

    fn ecam(device: u64, reg: u64) -> u64 {
        PCI_ECAM_ADDR + (device << 15) + reg
    }

    #[test]
    fn ecam_reaches_function_zero_of_each_device_on_bus_zero() {
        let mut bus = PciBus::default();
        let f = recorder();
        bus.add(3, f.clone());
        let mut id = [0; 4];
        assert!(bus.mmio_read(ecam(3, 0), &mut id));
        assert_eq!(u32::from_le_bytes(id), 0x1044_1af4);
        let mut word = [0; 2];
        bus.mmio_read(ecam(3, 2), &mut word);
        assert_eq!(u16::from_le_bytes(word), 0x1044, "a word inside the dword");

        // Nobody else: other devices, functions and buses read all ones.
        for addr in [ecam(4, 0), ecam(3, 0) + (1 << 12), ecam(3, 0) + (1 << 20), ecam(0, 0)] {
            assert!(bus.mmio_read(addr, &mut id));
            assert_eq!(id, [0xff; 4], "{addr:#x}");
        }
        // An access crossing its dword reaches nothing.
        bus.mmio_read(ecam(3, 2), &mut id);
        assert_eq!(id, [0xff; 4]);
        // The last byte of ECAM is still ECAM; the next is not.
        assert!(bus.claims(PCI_ECAM_ADDR + PCI_ECAM_SIZE - 1));
        assert!(!bus.claims(PCI_ECAM_ADDR + PCI_ECAM_SIZE));
    }

    #[test]
    fn bar_accesses_follow_the_bar_where_the_guest_moves_it() {
        let host = FakeHost::default();
        let mut bus = PciBus::default();
        let f = recorder();
        bus.add(1, f.clone());
        let mut b = [0; 4];
        assert!(bus.mmio_read(PCI_MMIO32_ADDR + 0x10, &mut b));
        assert_eq!(b, [0x5a; 4]);
        assert!(bus.mmio_write(PCI_MMIO32_ADDR + 0x20, &[1, 2], &host).unwrap());

        // The guest moves the BAR.
        let new = PCI_MMIO32_ADDR + 0x10_0000;
        bus.mmio_write(ecam(1, 0x10), &(new as u32).to_le_bytes(), &host).unwrap();
        assert!(!bus.mmio_read(PCI_MMIO32_ADDR + 0x10, &mut b), "gone from the old address");
        assert!(bus.mmio_read(new + 0xfff, &mut b[..1]));
        assert!(!bus.claims(new + 0x1000));

        // Decode off: no BAR at all.
        bus.mmio_write(ecam(1, 4), &0u16.to_le_bytes(), &host).unwrap();
        assert!(!bus.claims(new));

        assert_eq!(
            f.lock().unwrap().log,
            vec!["read 0 0x10 4", "write 0 0x20 [1, 2]", "read 0 0xfff 1"]
        );
    }
}
