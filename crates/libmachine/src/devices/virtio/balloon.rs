// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! virtio-balloon (virtio 1.2, section 5.5): guest memory given back to the host, so that a
//! long-lived machine keeps only the pages its guest uses (board 13490), as hotbed's Firecracker
//! VMs do.
//!
//! - **Free page reporting** (`VIRTIO_BALLOON_F_REPORTING`): the guest's kernel tells, now and
//!   then, of the large runs of its memory it has free, which the host discards ([`discard`]): the
//!   host's memory again, the guest reading zeros there (or, of memory restored from a snapshot,
//!   the snapshot's pages) when it uses them next, with no word to the host. What frees most:
//!   what a build freed goes back without anyone asking.
//! - **Inflation**: the host sets a target ([`Balloon::set_target`], `num_pages`), and the guest's
//!   driver takes pages out of its use up to it, telling which (the inflate queue), which the
//!   host discards, and hands them back (the deflate queue) as the target falls, or as it runs
//!   short of memory (`VIRTIO_BALLOON_F_DEFLATE_ON_OOM`). The driver says how many it holds
//!   (`actual`, [`Balloon::actual`]).
//!
//! No page poisoning is offered: a page given back promises no contents, so discarding it is
//! enough. Discards are of whole host pages: on a host of larger pages than the guest's 4 KiB (16
//! KiB arm64), a run's edges that are not whole host pages stay.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vm_memory::{
    Bytes as _, GuestAddress, GuestMemoryBackend as _, GuestMemoryMmap, GuestMemoryRegion as _,
};

use super::queue::DeviceQueue;
use super::{DeviceState, VirtioDevice, other_version};
use crate::sys::{EFD_NONBLOCK, EventFd};

const VIRTIO_ID_BALLOON: u16 = 5;
/// The guest deflates the balloon itself as it runs short of memory.
const F_DEFLATE_ON_OOM: u32 = 2;
/// The guest reports its free pages (the reporting queue).
const F_REPORTING: u32 = 5;
/// The inflate, deflate and reporting queues: dense, as the driver numbers the queues of the
/// features offered (no stats, no free page hints).
const QUEUE_SIZES: [u16; 3] = [256, 256, 32];
const INFLATE: usize = 0;
const DEFLATE: usize = 1;
const REPORTING: usize = 2;
/// The balloon's pages, whatever the guest's: 4 KiB (VIRTIO_BALLOON_PFN_SHIFT).
const PFN_SHIFT: u32 = 12;
/// Its configuration: num_pages, actual, free_page_hint_cmd_id, poison_val.
const CONFIG_SIZE: usize = 16;
/// The version of the state it saves: the target and what the driver holds.
const STATE_VERSION: u32 = 1;

/// What the balloon device and its handles share.
#[derive(Debug)]
struct Shared {
    /// The pages the host wants in the balloon (`num_pages`).
    target: AtomicU32,
    /// The pages the driver says it holds (`actual`).
    actual: AtomicU32,
    /// The target changed: the driver is to be told.
    changed: AtomicBool,
    /// Bytes of guest memory discarded, all told.
    discarded: AtomicU64,
    /// Woken as the target changes, for the device thread to tell the driver.
    wake: EventFd,
}

/// A machine's balloon, from the host's side ([`Machine::balloon`](crate::Machine::balloon)):
/// cheap to clone, usable from any thread.
#[derive(Debug, Clone)]
pub struct Balloon(Arc<Shared>);

impl Balloon {
    /// Asks the guest to hold `bytes` of its memory in the balloon, given back to the host: in
    /// 4 KiB pages, rounded down (at most 2^32 - 1 of them). 0 asks for all of it back.
    pub fn set_target(&self, bytes: u64) {
        let pages = u32::try_from(bytes >> PFN_SHIFT).unwrap_or(u32::MAX);
        self.0.target.store(pages, Ordering::Release);
        self.0.changed.store(true, Ordering::Release);
        // A full eventfd is woken already; the device thread then reads the new target.
        let _ = self.0.wake.write(1);
    }

    /// The target, in bytes.
    pub fn target(&self) -> u64 {
        u64::from(self.0.target.load(Ordering::Acquire)) << PFN_SHIFT
    }

    /// What the guest's driver says it holds in the balloon, in bytes.
    pub fn actual(&self) -> u64 {
        u64::from(self.0.actual.load(Ordering::Acquire)) << PFN_SHIFT
    }

    /// Bytes of guest memory the host discarded since the machine was made: inflated and
    /// reported free.
    pub fn discarded(&self) -> u64 {
        self.0.discarded.load(Ordering::Relaxed)
    }
}

/// How guest RAM given back goes back to the host: its guest address, its host address in this
/// process, its length, whole host pages; the hypervisor's to do ([`discard`]).
pub(crate) type FreeRam =
    Box<dyn Fn(u64, std::ptr::NonNull<u8>, u64) -> std::io::Result<()> + Send>;

/// The balloon pages the guest holds in the balloon, by host page: a host page goes back to the
/// host once every balloon page of it is held (a 16 KiB host page, four 4 KiB balloon pages,
/// which the guest's driver takes one at a time, from anywhere). The guest's own to take back
/// (deflated), which it then has, fault by fault.
struct Held {
    host_page: u64,
    /// Each host page's balloon pages held, a bit each.
    pages: std::collections::HashMap<u64, u64>,
}

impl Held {
    fn new(host_page: u64) -> Self {
        Self { host_page, pages: std::collections::HashMap::new() }
    }

    /// Holds balloon page `page` (a guest address): its host page, if now all of it is held.
    fn hold(&mut self, page: u64) -> Option<u64> {
        let host = page - page % self.host_page;
        let per_host = self.host_page >> PFN_SHIFT;
        let held = self.pages.entry(host).or_insert(0);
        *held |= 1 << ((page - host) >> PFN_SHIFT);
        (u64::from(held.count_ones()) == per_host).then_some(host)
    }

    /// Releases balloon page `page`, deflated.
    fn release(&mut self, page: u64) {
        let host = page - page % self.host_page;
        if let Some(held) = self.pages.get_mut(&host) {
            *held &= !(1 << ((page - host) >> PFN_SHIFT));
            if *held == 0 {
                self.pages.remove(&host);
            }
        }
    }
}

/// The page numbers in the readable buffers of `chain`.
fn page_numbers(
    chain: &virtio_queue::DescriptorChain<&GuestMemoryMmap>,
    mem: &GuestMemoryMmap,
) -> Vec<u32> {
    let mut numbers = Vec::new();
    for desc in chain.clone().readable() {
        let mut bytes = vec![0u8; (desc.len() as usize) & !3];
        if mem.read_slice(&mut bytes, desc.addr()).is_ok() {
            let words = bytes.as_chunks::<4>().0.iter().map(|w| u32::from_le_bytes(*w));
            numbers.extend(words);
        }
    }
    numbers
}

/// The balloon device.
pub(crate) struct Device {
    shared: Arc<Shared>,
    free: FreeRam,
    held: Held,
}

impl Device {
    /// A balloon, empty, giving back RAM through `free`, and its host handle.
    pub(crate) fn new(free: FreeRam) -> crate::error::Result<(Self, Balloon)> {
        let wake = EventFd::new(EFD_NONBLOCK).map_err(|source| {
            crate::error::Error::Hypervisor { op: "create the balloon's eventfd", source }
        })?;
        let shared = Arc::new(Shared {
            target: AtomicU32::new(0),
            actual: AtomicU32::new(0),
            changed: AtomicBool::new(false),
            discarded: AtomicU64::new(0),
            wake,
        });
        let held = Held::new(host_page().max(1 << PFN_SHIFT));
        Ok((Self { shared: shared.clone(), free, held }, Balloon(shared)))
    }

    /// Discards `len` bytes of guest memory at `addr`, counted.
    fn give_back(&self, mem: &GuestMemoryMmap, addr: u64, len: u64) {
        let freed = discard(mem, addr, len, &self.free);
        self.shared.discarded.fetch_add(freed, Ordering::Relaxed);
    }
}

impl VirtioDevice for Device {
    fn device_type(&self) -> u16 {
        VIRTIO_ID_BALLOON
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &QUEUE_SIZES
    }

    fn features(&self) -> u64 {
        1 << F_DEFLATE_ON_OOM | 1 << F_REPORTING
    }

    fn config_size(&self) -> usize {
        CONFIG_SIZE
    }

    fn read_config(&self, offset: usize, data: &mut [u8]) {
        let mut config = [0u8; CONFIG_SIZE];
        config[..4].copy_from_slice(&self.shared.target.load(Ordering::Acquire).to_le_bytes());
        config[4..8].copy_from_slice(&self.shared.actual.load(Ordering::Acquire).to_le_bytes());
        data.fill(0);
        if let Some(from) = config.get(offset..) {
            let n = data.len().min(from.len());
            data[..n].copy_from_slice(&from[..n]);
        }
    }

    /// The driver writes `actual` alone (offset 4); the rest is the device's.
    fn write_config(&mut self, offset: usize, data: &[u8]) {
        if offset == 4 && data.len() == 4 {
            let actual = u32::from_le_bytes(data.try_into().expect("4 bytes"));
            self.shared.actual.store(actual, Ordering::Release);
        }
    }

    fn wake_fd(&self) -> Option<&EventFd> {
        Some(&self.shared.wake)
    }

    fn woken(&mut self, _queues: &mut [DeviceQueue], _mem: &GuestMemoryMmap) {
        // Read until empty: the target is in `shared`, the wake only says it changed.
        while self.shared.wake.read().is_ok() {}
    }

    fn take_config_change(&mut self) -> bool {
        self.shared.changed.swap(false, Ordering::AcqRel)
    }

    fn reset(&mut self) {
        self.shared.actual.store(0, Ordering::Release);
        self.held.pages.clear();
    }

    fn process_queue(&mut self, index: usize, queues: &mut [DeviceQueue], mem: &GuestMemoryMmap) {
        let Some(queue) = queues.get_mut(index) else { return };
        queue.quiet(mem);
        while let Some(chain) = queue.pop(mem) {
            let head = chain.head_index();
            match index {
                // Each buffer an array of little-endian u32 page numbers: discarded, in runs.
                // Each buffer an array of little-endian u32 page numbers: held, and each host page
                // all of whose balloon pages are held given back.
                INFLATE | DEFLATE => {
                    for pfn in page_numbers(&chain, mem) {
                        let page = u64::from(pfn) << PFN_SHIFT;
                        if index == DEFLATE {
                            self.held.release(page);
                        } else if let Some(host_page) = self.held.hold(page) {
                            self.give_back(mem, host_page, self.held.host_page);
                        }
                    }
                }
                // Each buffer is a free run of guest memory itself, its address and length.
                REPORTING => {
                    for desc in chain.writable() {
                        self.give_back(mem, desc.addr().0, u64::from(desc.len()));
                    }
                }
                _ => {}
            }
            if !queue.add_used(mem, head, 0) {
                return;
            }
        }
    }

    fn save(&self) -> crate::error::Result<DeviceState> {
        let mut data = Vec::with_capacity(8);
        data.extend_from_slice(&self.shared.target.load(Ordering::Acquire).to_le_bytes());
        data.extend_from_slice(&self.shared.actual.load(Ordering::Acquire).to_le_bytes());
        Ok(DeviceState { version: STATE_VERSION, data })
    }

    fn restore(&mut self, state: &DeviceState) -> crate::error::Result<()> {
        let (Some(target), Some(actual)) = (state.data.get(..4), state.data.get(4..8)) else {
            return Err(other_version(VIRTIO_ID_BALLOON, state.version, STATE_VERSION));
        };
        if state.version != STATE_VERSION || state.data.len() != 8 {
            return Err(other_version(VIRTIO_ID_BALLOON, state.version, STATE_VERSION));
        }
        let word = |b: &[u8]| u32::from_le_bytes(b.try_into().expect("4 bytes"));
        self.shared.target.store(word(target), Ordering::Release);
        self.shared.actual.store(word(actual), Ordering::Release);
        Ok(())
    }
}

/// The host's page size.
fn host_page() -> u64 {
    // SAFETY: a plain system call.
    u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap_or(4096)
}

/// Gives the host back the whole host pages of `len` bytes of guest memory at `addr`, which the
/// guest promised not to need the contents of, through `free`. The bytes given back; ranges
/// outside guest memory are passed over, as the guest's to get right.
pub(crate) fn discard(mem: &GuestMemoryMmap, addr: u64, len: u64, free: &FreeRam) -> u64 {
    let page = host_page();
    let Some(end) = addr.checked_add(len) else { return 0 };
    let mut freed = 0;
    for region in mem.iter() {
        let (start, region_end) = (region.start_addr().0, region.start_addr().0 + region.len());
        let (from, to) = (addr.max(start), end.min(region_end));
        // Whole host pages of the range, within the region.
        let (from, to) = (from.next_multiple_of(page), to - to % page);
        if from >= to {
            continue;
        }
        let Some(host) =
            mem.get_host_address(GuestAddress(from)).ok().and_then(std::ptr::NonNull::new)
        else {
            continue;
        };
        match free(from, host, to - from) {
            Ok(()) => freed += to - from,
            // libmachine has no logger: as its extent device says what it can't undo.
            Err(e) => eprintln!(
                "libmachine: the balloon giving back {} bytes at guest {from:#x}: {e}",
                to - from
            ),
        }
    }
    freed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::virtio::test_driver::Driver;

    /// On a host of 16 KiB pages, a host page goes back once its four balloon pages are all
    /// held, however they come, and again once one deflated is held again.
    #[test]
    fn a_host_page_goes_back_once_all_of_it_is_held() {
        let mut held = Held::new(16 << 10);
        let base = 7 * (16 << 10);
        for (i, part) in [3u64, 0, 2].into_iter().enumerate() {
            assert_eq!(held.hold(base + (part << 12)), None, "{i}");
        }
        assert_eq!(held.hold(base + (1 << 12)), Some(base));
        held.release(base + (2 << 12));
        assert_eq!(held.hold(base + (2 << 12)), Some(base));
        for part in 0..4 {
            held.release(base + (part << 12));
        }
        assert!(held.pages.is_empty(), "forgotten once none of it is held");
        let mut one = Held::new(4 << 10);
        assert_eq!(one.hold(5 << 12), Some(5 << 12), "a 4 KiB host page, at once");
    }

    /// Inflated page numbers and reported runs are used, the driver's `actual` read back, and a
    /// new target is a configuration change.
    #[test]
    fn queues_and_config() {
        let free: FreeRam = Box::new(|_, _, _| Ok(()));
        let (mut device, balloon) = Device::new(free).unwrap();
        assert_eq!(device.features(), 1 << 2 | 1 << 5);
        balloon.set_target(64 << 20);
        assert!(device.take_config_change());
        assert!(!device.take_config_change(), "told once");
        let mut config = [0u8; 8];
        device.read_config(0, &mut config);
        assert_eq!(u32::from_le_bytes(config[..4].try_into().unwrap()), 16384);
        device.write_config(4, &7u32.to_le_bytes());
        assert_eq!(balloon.actual(), 7 << 12);
        // A write of what the device owns changes nothing.
        device.write_config(0, &9u32.to_le_bytes());
        assert_eq!(balloon.target(), 64 << 20);

        let mut d = Driver::new(3, true);
        let pfns: Vec<u8> = [1u32, 2, 3, 9].iter().flat_map(|p| p.to_le_bytes()).collect();
        d.post(INFLATE, &[(pfns, false)]);
        d.post(REPORTING, &[(vec![0; 8192], true)]);
        device.process_queue(INFLATE, &mut d.queues, &d.mem);
        device.process_queue(REPORTING, &mut d.queues, &d.mem);
        assert_eq!(d.used(INFLATE).len(), 1);
        assert_eq!(d.used(REPORTING).iter().map(|u| u.1).collect::<Vec<_>>(), [0]);
        let saved = device.save().unwrap();
        let (mut other, restored) = Device::new(Box::new(|_, _, _| Ok(()))).unwrap();
        other.restore(&saved).unwrap();
        assert_eq!((restored.target(), restored.actual()), (64 << 20, 7 << 12));
        let bad = DeviceState { version: 2, data: saved.data.clone() };
        assert!(other.restore(&bad).is_err());
    }
}
