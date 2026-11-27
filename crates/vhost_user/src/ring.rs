// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The split ring (virtio 1.2, section 2.7), as Linux's `virtio_ring` and the guest's driver have
//! it, with `VIRTIO_RING_F_EVENT_IDX`'s suppression: a table of descriptors, the ring of chains the
//! driver makes available and the ring of those the device returns used, each with an index the
//! other side reads and an event index saying when to wake it. A descriptor names bytes of guest
//! memory by their guest address.
//!
//! The [`Device`] is the back-end's half, which trusts nothing of the guest's driver: whatever the
//! rings hold, [`Device::pop`] returns a chain of buffers its memory table holds, or an error that
//! ends the queue, never a panic, a loop or a read out of the rings (AGENTS.md, "Trust
//! boundaries"). The [`Driver`] is the test front-end's guest driver (`front_end`). Adapted from
//! the parked `extent_ring` (7453f7cbb), whose rings lived in a region of their own.

use crate::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, Ordering, fence};

/// A descriptor's flag: the chain goes on at its `next`.
pub const DESC_F_NEXT: u16 = 1;
/// A descriptor's flag: the device writes its bytes; else it reads them.
pub const DESC_F_WRITE: u16 = 2;
/// A descriptor's flag: it names a table of descriptors, which the ring takes none of.
pub const DESC_F_INDIRECT: u16 = 4;

/// A descriptor, as the region holds it: `len` bytes at `addr` in the buffer area, its flags, and
/// the next of its chain. Atomics, each field read and written whole, as the other process may
/// write it at any time; their layout is the integers' (virtio's `struct virtq_desc`).
#[repr(C)]
pub(crate) struct Descriptor {
    pub(crate) addr: AtomicU64,
    pub(crate) len: AtomicU32,
    pub(crate) flags: AtomicU16,
    pub(crate) next: AtomicU16,
}

/// A chain the device returned, as the used ring holds it: its head, and the bytes it wrote
/// (virtio's `struct virtq_used_elem`).
#[repr(C)]
pub(crate) struct UsedElem {
    pub(crate) id: AtomicU32,
    pub(crate) len: AtomicU32,
}

/// A ring's parts, wherever they are: guest memory the back-end maps (`crate::queue`), or, in tests
/// and loom's models, memory of their own. Each ring has as many entries as the table has
/// descriptors, a power of two.
pub struct Rings<'a> {
    pub(crate) desc: &'a [Descriptor],
    /// The available ring: its index, the driver's, and its entries, chains' heads.
    pub(crate) avail_idx: &'a AtomicU16,
    pub(crate) avail_ring: &'a [AtomicU16],
    /// The used index past which the driver asks to be interrupted: the available ring's last.
    pub(crate) used_event: &'a AtomicU16,
    /// The used ring: its index, the device's, and its entries.
    pub(crate) used_idx: &'a AtomicU16,
    pub(crate) used_ring: &'a [UsedElem],
    /// The available index past which the device asks to be kicked: the used ring's last.
    pub(crate) avail_event: &'a AtomicU16,
}

/// The bytes of a ring of `size` entries' descriptor table, available ring and used ring.
pub fn ring_lens(size: u16) -> (u64, u64, u64) {
    let size = u64::from(size);
    (16 * size, 6 + 2 * size, 6 + 8 * size)
}

#[cfg(not(feature = "loom"))]
impl<'a> Rings<'a> {
    /// The rings of `size` entries, a power of two, whose descriptor table is at `desc`, available
    /// ring at `avail` and used ring at `used`, aligned as virtio has them (16, 2 and 4 bytes).
    ///
    /// # Safety
    ///
    /// Each points at as many bytes as [`ring_lens`] says, mapped while the rings live, which this
    /// process reads and writes only through atomics; another process may write them at any time.
    pub unsafe fn from_raw(size: u16, desc: *const u8, avail: *const u8, used: *const u8) -> Self {
        let size = usize::from(size);
        // SAFETY: As the caller says: each part mapped and aligned, accessed through atomics alone,
        // whose layouts are the integers' virtio's structures hold.
        unsafe {
            Self {
                desc: std::slice::from_raw_parts(desc.cast(), size),
                avail_idx: &*avail.add(2).cast(),
                avail_ring: std::slice::from_raw_parts(avail.add(4).cast(), size),
                used_event: &*avail.add(4 + 2 * size).cast(),
                used_idx: &*used.add(2).cast(),
                used_ring: std::slice::from_raw_parts(used.add(4).cast(), size),
                avail_event: &*used.add(4 + 8 * size).cast(),
            }
        }
    }
}

impl Rings<'_> {
    /// The ring's entries: its table's descriptors.
    fn size(&self) -> u16 {
        // At most `MAX_QUEUE_SIZE`, a u16's (`Layout::new`).
        self.desc.len() as u16
    }

    /// The entry of a ring of the ring's size that the free-running index `idx` is at.
    fn slot(&self, idx: u16) -> usize {
        usize::from(idx) & (self.desc.len() - 1)
    }
}

/// What holds a ring's parts: guest memory mapped (`crate::queue`), or memory of their own in
/// tests.
pub trait RingMemory: Send + Sync {
    /// Its rings.
    fn rings(&self) -> Rings<'_>;
}

impl<T: RingMemory> RingMemory for std::sync::Arc<T> {
    fn rings(&self) -> Rings<'_> {
        (**self).rings()
    }
}

/// Whether a side that moved its index from `old` to `new` wakes the other, which asked to be woken
/// once it moved past `event` (virtio's `vring_need_event`), in u16's wrapping arithmetic.
fn need_event(event: u16, new: u16, old: u16) -> bool {
    new.wrapping_sub(event).wrapping_sub(1) < new.wrapping_sub(old)
}

/// A buffer of a chain: `len` bytes at guest address `offset`, which the device writes, or reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub offset: u64,
    pub len: u32,
    pub writable: bool,
}

/// Why the driver can't add a chain.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AddError {
    #[error("a chain of {wanted} descriptors, with {free} free")]
    Full { wanted: usize, free: usize },
    #[error("a chain of no descriptors")]
    Empty,
}

/// A chain the device returned used: its head, as [`Driver::add`] returned it, and the bytes it
/// wrote of its writable buffers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Used {
    pub head: u16,
    pub len: u32,
}

/// The device returned what the driver never gave it: a bug of the device's, which ends the ring.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("the device returned chain {0}, which it was not given")]
pub struct UnknownChain(pub u32);

/// A guest driver's half of a ring: it adds chains of guest memory, publishes them to the device
/// and kicks it if it asked, and takes back the chains the device returns.
pub struct Driver<M: RingMemory> {
    memory: M,
    /// The descriptors no chain has.
    free: Vec<u16>,
    /// Each chain added and not yet returned, by its head: its descriptors.
    chains: Vec<Vec<u16>>,
    /// The available index the chains added so far take it to.
    next_avail: u16,
    /// The available index last published.
    published: u16,
    /// The used index of the last chain returned.
    last_used: u16,
}

impl<M: RingMemory> Driver<M> {
    /// The driver of the ring `memory` holds, which no one used yet.
    pub fn new(memory: M) -> Self {
        let size = memory.rings().size();
        Self {
            memory,
            free: (0..size).rev().collect(),
            chains: (0..size).map(|_| Vec::new()).collect(),
            next_avail: 0,
            published: 0,
            last_used: 0,
        }
    }

    /// The memory its ring is in.
    pub fn memory(&self) -> &M {
        &self.memory
    }

    /// How many descriptors no chain has.
    pub fn free(&self) -> usize {
        self.free.len()
    }

    /// Adds a chain of `segments`, the readable ones first, which the device sees once
    /// [`Driver::publish`] publishes it: its head, which [`Driver::used`] returns it by.
    pub fn add(&mut self, segments: &[Segment]) -> Result<u16, AddError> {
        if segments.is_empty() {
            return Err(AddError::Empty);
        }
        if segments.len() > self.free.len() {
            return Err(AddError::Full { wanted: segments.len(), free: self.free.len() });
        }
        let at = self.free.len() - segments.len();
        let ids = self.free.split_off(at);
        let rings = self.memory.rings();
        for (i, (segment, id)) in segments.iter().zip(&ids).enumerate() {
            let descriptor = &rings.desc[usize::from(*id)];
            descriptor.addr.store(segment.offset, Ordering::Relaxed);
            descriptor.len.store(segment.len, Ordering::Relaxed);
            let next = ids.get(i + 1);
            descriptor.next.store(next.copied().unwrap_or(0), Ordering::Relaxed);
            let flags = if segment.writable { DESC_F_WRITE } else { 0 }
                | if next.is_some() { DESC_F_NEXT } else { 0 };
            descriptor.flags.store(flags, Ordering::Relaxed);
        }
        let head = ids[0];
        rings.avail_ring[rings.slot(self.next_avail)].store(head, Ordering::Relaxed);
        self.next_avail = self.next_avail.wrapping_add(1);
        self.chains[usize::from(head)] = ids;
        Ok(head)
    }

    /// Publishes the chains added since it last did, and says whether to kick the device: whether
    /// it asked to be, once the available index passed where it last looked.
    pub fn publish(&mut self) -> bool {
        let rings = self.memory.rings();
        let (old, new) = (self.published, self.next_avail);
        if old == new {
            return false;
        }
        // The chains' descriptors and entries, written before, published with the index, which
        // the device reads with Acquire (`Device::pop`).
        rings.avail_idx.store(new, Ordering::Release);
        self.published = new;
        // The index stored before the device's event index is read, as the device stores that
        // before it reads the index (`Device::arm`): with a full fence each, one of the two sees
        // the other's store, so the device either finds the chains or is kicked (virtio's
        // `virtio_mb`; `loom::kicks_are_never_lost`).
        fence(Ordering::SeqCst);
        need_event(rings.avail_event.load(Ordering::Relaxed), new, old)
    }

    /// The next chain the device returned, if any, its descriptors free again.
    pub fn used(&mut self) -> Result<Option<Used>, UnknownChain> {
        let rings = self.memory.rings();
        if rings.used_idx.load(Ordering::Acquire) == self.last_used {
            return Ok(None);
        }
        let entry = &rings.used_ring[rings.slot(self.last_used)];
        let (id, len) = (entry.id.load(Ordering::Relaxed), entry.len.load(Ordering::Relaxed));
        let chain = usize::try_from(id).ok().and_then(|head| self.chains.get_mut(head));
        let Some(chain) = chain.filter(|chain| !chain.is_empty()) else {
            return Err(UnknownChain(id));
        };
        self.free.append(chain);
        self.last_used = self.last_used.wrapping_add(1);
        // Within the table (`self.chains`), whose size is a u16's.
        Ok(Some(Used { head: id as u16, len }))
    }

    /// Asks the device to interrupt once it returns a chain past those taken back, and says
    /// whether it returned one meanwhile, which the driver then takes back before it waits.
    pub fn arm(&mut self) -> bool {
        let rings = self.memory.rings();
        rings.used_event.store(self.last_used, Ordering::Relaxed);
        // As in `publish`, with the device's `publish` (`loom::interrupts_are_never_lost`).
        fence(Ordering::SeqCst);
        rings.used_idx.load(Ordering::Acquire) != self.last_used
    }
}

/// Why the device ends a ring: what its driver made of it means nothing, whatever it meant.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RingError {
    #[error("{0} chains made available at once, more than the ring's {1} entries")]
    TooMany(u16, u16),
    #[error("descriptor {0}, past the ring's {1}")]
    Descriptor(u16, u16),
    #[error("chain {0} made available again while it is served")]
    Busy(u16),
    #[error("a chain of more than the ring's {0} descriptors")]
    Loop(u16),
    #[error("an indirect descriptor, which the device offers none of")]
    Indirect,
    #[error("a descriptor of {len} bytes at guest address {offset:#x}, which no memory holds")]
    OutOfMemory { offset: u64, len: u32 },
    #[error("a readable descriptor after a writable one")]
    Order,
}

/// A chain the device took off the available ring: its head, which [`Device::push`] returns it by,
/// and its buffers, each in guest memory, the readable ones first.
#[derive(Debug, PartialEq, Eq)]
pub struct Chain {
    pub head: u16,
    pub segments: Vec<Segment>,
}

/// The back-end's half of a ring: it takes the chains the guest's driver made available, checked,
/// and returns each used, interrupting the driver if it asked.
pub struct Device<M: RingMemory> {
    memory: M,
    /// The available index of the next chain to take.
    last_avail: u16,
    /// The used index the chains returned so far take it to.
    next_used: u16,
    /// The used index last published.
    published: u16,
    /// Whether each head's chain is taken and not yet returned.
    serving: Vec<bool>,
}

impl<M: RingMemory> Device<M> {
    /// The device of the ring `memory` holds, taking chains from available index `last_avail`
    /// (`SET_VRING_BASE`), returning them from the used index the ring holds.
    pub fn new(memory: M, last_avail: u16) -> Self {
        let rings = memory.rings();
        let size = rings.desc.len();
        let used = rings.used_idx.load(Ordering::Acquire);
        Self { memory, last_avail, next_used: used, published: used, serving: vec![false; size] }
    }

    /// The memory its ring is in.
    pub fn memory(&self) -> &M {
        &self.memory
    }

    /// Has its ring in `memory` from now on, the same ring mapped anew (`SET_MEM_TABLE`).
    pub fn set_memory(&mut self, memory: M) {
        assert_eq!(memory.rings().desc.len(), self.serving.len(), "a ring of another size");
        self.memory = memory;
    }

    /// The available index of the next chain to take: what `GET_VRING_BASE` returns.
    pub fn last_avail(&self) -> u16 {
        self.last_avail
    }

    /// How many chains it took and has not returned.
    pub fn serving(&self) -> usize {
        self.serving.iter().filter(|serving| **serving).count()
    }

    /// The next chain made available, if any, its descriptors read once and checked, each buffer
    /// held by memory as `within` says: an error if they mean nothing, after which the ring is of
    /// no more use.
    #[deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
    pub fn pop(&mut self, within: impl Fn(u64, u32) -> bool) -> Result<Option<Chain>, RingError> {
        let rings = self.memory.rings();
        let size = rings.size();
        let available = rings.avail_idx.load(Ordering::Acquire);
        let pending = available.wrapping_sub(self.last_avail);
        if pending == 0 {
            return Ok(None);
        }
        if pending > size {
            return Err(RingError::TooMany(pending, size));
        }
        let entry = rings.avail_ring.get(rings.slot(self.last_avail)).expect("a slot is within");
        let head = entry.load(Ordering::Relaxed);
        self.last_avail = self.last_avail.wrapping_add(1);
        match self.serving.get(usize::from(head)) {
            None => return Err(RingError::Descriptor(head, size)),
            Some(true) => return Err(RingError::Busy(head)),
            Some(false) => {}
        }
        let mut segments = Vec::new();
        let mut id = head;
        loop {
            if segments.len() == usize::from(size) {
                return Err(RingError::Loop(size));
            }
            let descriptor =
                rings.desc.get(usize::from(id)).ok_or(RingError::Descriptor(id, size))?;
            let flags = descriptor.flags.load(Ordering::Relaxed);
            if flags & DESC_F_INDIRECT != 0 {
                return Err(RingError::Indirect);
            }
            let offset = descriptor.addr.load(Ordering::Relaxed);
            let len = descriptor.len.load(Ordering::Relaxed);
            if !within(offset, len) {
                return Err(RingError::OutOfMemory { offset, len });
            }
            let writable = flags & DESC_F_WRITE != 0;
            if !writable && segments.last().is_some_and(|last: &Segment| last.writable) {
                return Err(RingError::Order);
            }
            segments.push(Segment { offset, len, writable });
            if flags & DESC_F_NEXT == 0 {
                break;
            }
            id = descriptor.next.load(Ordering::Relaxed);
        }
        *self.serving.get_mut(usize::from(head)).expect("checked above") = true;
        Ok(Some(Chain { head, segments }))
    }

    /// Returns the chain of `head`, which it took, used, having written `len` bytes of it: seen by
    /// the driver once [`Device::publish`] publishes it.
    pub fn push(&mut self, head: u16, len: u32) {
        let serving = &mut self.serving[usize::from(head)];
        assert!(*serving, "chain {head} returned, which the device is not serving");
        *serving = false;
        let rings = self.memory.rings();
        let entry = &rings.used_ring[rings.slot(self.next_used)];
        entry.id.store(u32::from(head), Ordering::Relaxed);
        entry.len.store(len, Ordering::Relaxed);
        self.next_used = self.next_used.wrapping_add(1);
    }

    /// Publishes the chains returned since it last did, and says whether to interrupt the driver:
    /// whether it asked to be, once the used index passed where it last looked.
    pub fn publish(&mut self) -> bool {
        let rings = self.memory.rings();
        let (old, new) = (self.published, self.next_used);
        if old == new {
            return false;
        }
        // The entries, written before, published with the index, which the driver reads with
        // Acquire (`Driver::used`).
        rings.used_idx.store(new, Ordering::Release);
        self.published = new;
        // As the driver's `publish`, with its `arm` (`loom::interrupts_are_never_lost`).
        fence(Ordering::SeqCst);
        need_event(rings.used_event.load(Ordering::Relaxed), new, old)
    }

    /// Asks the driver to kick once it makes a chain available past those taken, and says
    /// whether it made one available meanwhile, which the device then takes before it waits.
    pub fn arm(&mut self) -> bool {
        let rings = self.memory.rings();
        rings.avail_event.store(self.last_avail, Ordering::Relaxed);
        // As in the driver's `publish` (`loom::kicks_are_never_lost`).
        fence(Ordering::SeqCst);
        rings.avail_idx.load(Ordering::Acquire) != self.last_avail
    }
}

/// A ring's parts in memory of their own, for the tests and loom's models: as a region holds them,
/// but where the test, as a hostile driver, writes what it likes.
#[cfg(test)]
pub(crate) mod owned {
    use super::*;

    pub(crate) struct Owned {
        pub(crate) desc: Vec<Descriptor>,
        pub(crate) avail_idx: AtomicU16,
        pub(crate) avail_ring: Vec<AtomicU16>,
        pub(crate) used_event: AtomicU16,
        pub(crate) used_idx: AtomicU16,
        pub(crate) used_ring: Vec<UsedElem>,
        pub(crate) avail_event: AtomicU16,
    }

    impl Owned {
        /// A ring of `size` entries, a power of two.
        pub(crate) fn new(size: u16) -> Self {
            assert!(size.is_power_of_two());
            let size = usize::from(size);
            Self {
                desc: (0..size)
                    .map(|_| Descriptor {
                        addr: AtomicU64::new(0),
                        len: AtomicU32::new(0),
                        flags: AtomicU16::new(0),
                        next: AtomicU16::new(0),
                    })
                    .collect(),
                avail_idx: AtomicU16::new(0),
                avail_ring: (0..size).map(|_| AtomicU16::new(0)).collect(),
                used_event: AtomicU16::new(0),
                used_idx: AtomicU16::new(0),
                used_ring: (0..size)
                    .map(|_| UsedElem { id: AtomicU32::new(0), len: AtomicU32::new(0) })
                    .collect(),
                avail_event: AtomicU16::new(0),
            }
        }
    }

    impl RingMemory for Owned {
        fn rings(&self) -> Rings<'_> {
            Rings {
                desc: &self.desc,
                avail_idx: &self.avail_idx,
                avail_ring: &self.avail_ring,
                used_event: &self.used_event,
                used_idx: &self.used_idx,
                used_ring: &self.used_ring,
                avail_event: &self.avail_event,
            }
        }
    }
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use std::sync::Arc;

    use rand::{RngExt as _, SeedableRng as _};

    use super::owned::Owned;
    use super::*;

    const SIZE: u16 = 8;
    /// The guest memory the tests' chains name: the first MiB.
    const BUFFERS: u64 = 1 << 20;

    fn within(offset: u64, len: u32) -> bool {
        offset.checked_add(u64::from(len)).is_some_and(|end| end <= BUFFERS)
    }

    fn readable(offset: u64, len: u32) -> Segment {
        Segment { offset, len, writable: false }
    }

    fn writable(offset: u64, len: u32) -> Segment {
        Segment { offset, len, writable: true }
    }

    /// A hostile driver's descriptor `id`: what it likes.
    fn set(ring: &Owned, id: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let descriptor = &ring.desc[usize::from(id)];
        descriptor.addr.store(addr, Ordering::Relaxed);
        descriptor.len.store(len, Ordering::Relaxed);
        descriptor.flags.store(flags, Ordering::Relaxed);
        descriptor.next.store(next, Ordering::Relaxed);
    }

    /// A hostile driver makes the chain of head `head` available, the `n`th.
    fn offer(ring: &Owned, n: u16, head: u16) {
        ring.avail_ring[usize::from(n) % usize::from(SIZE)].store(head, Ordering::Relaxed);
        ring.avail_idx.store(n.wrapping_add(1), Ordering::Release);
    }

    /// What a device makes of the one chain of head 0 a hostile driver offers, its descriptors
    /// set by `describe`.
    fn popped(describe: impl FnOnce(&Owned)) -> Result<Option<Chain>, RingError> {
        let ring = Owned::new(SIZE);
        describe(&ring);
        offer(&ring, 0, 0);
        Device::new(ring, 0).pop(within)
    }

    /// Whatever a hostile driver's descriptors say, the device refuses what means nothing, at the
    /// edges of what it checks: past the table, a loop, an indirect table, past the buffer area
    /// or a u64, a readable buffer after a writable one; and takes what is within them.
    #[test]
    fn chains_out_of_bounds_are_refused() {
        // At the buffer area's end, and one byte past it.
        let edge = popped(|ring| set(ring, 0, BUFFERS - 1, 1, 0, 0)).unwrap().unwrap();
        assert_eq!(edge.segments, [readable(BUFFERS - 1, 1)]);
        assert_eq!(
            popped(|ring| set(ring, 0, BUFFERS, 1, 0, 0)),
            Err(RingError::OutOfMemory { offset: BUFFERS, len: 1 })
        );
        assert_eq!(
            popped(|ring| set(ring, 0, u64::MAX, u32::MAX, 0, 0)),
            Err(RingError::OutOfMemory { offset: u64::MAX, len: u32::MAX })
        );
        assert_eq!(
            popped(|ring| set(ring, 0, 1, u32::MAX, DESC_F_WRITE, 0)),
            Err(RingError::OutOfMemory { offset: 1, len: u32::MAX })
        );
        // Empty buffers, at 0 and at the end, are within.
        let empty = popped(|ring| {
            set(ring, 0, 0, 0, DESC_F_NEXT, 1);
            set(ring, 1, BUFFERS, 0, DESC_F_WRITE, 0);
        });
        assert_eq!(empty.unwrap().unwrap().segments, [readable(0, 0), writable(BUFFERS, 0)]);
        // The last descriptor's next past the table; a head past it.
        assert_eq!(
            popped(|ring| set(ring, 0, 0, 8, DESC_F_NEXT, SIZE)),
            Err(RingError::Descriptor(SIZE, SIZE))
        );
        assert_eq!(
            popped(|ring| set(ring, 0, 0, 8, DESC_F_NEXT, u16::MAX)),
            Err(RingError::Descriptor(u16::MAX, SIZE))
        );
        let ring = Owned::new(SIZE);
        offer(&ring, 0, u16::MAX);
        assert_eq!(Device::new(ring, 0).pop(within), Err(RingError::Descriptor(u16::MAX, SIZE)));
        // A loop on itself, and through every descriptor; a chain of all of them is within.
        assert_eq!(popped(|ring| set(ring, 0, 0, 8, DESC_F_NEXT, 0)), Err(RingError::Loop(SIZE)));
        let through_all = popped(|ring| {
            (0..SIZE).for_each(|id| set(ring, id, 0, 8, DESC_F_NEXT, (id + 1) % SIZE));
        });
        assert_eq!(through_all, Err(RingError::Loop(SIZE)));
        let whole = popped(|ring| {
            (0..SIZE).for_each(|id| set(ring, id, 0, 8, DESC_F_NEXT, id + 1));
            set(ring, SIZE - 1, 0, 8, 0, 0);
        });
        assert_eq!(whole.unwrap().unwrap().segments.len(), usize::from(SIZE));
        // An indirect table, and a readable buffer after a writable one.
        assert_eq!(
            popped(|ring| set(ring, 0, 0, 16, DESC_F_INDIRECT, 0)),
            Err(RingError::Indirect)
        );
        assert_eq!(
            popped(|ring| {
                set(ring, 0, 0, 8, DESC_F_WRITE | DESC_F_NEXT, 1);
                set(ring, 1, 8, 8, 0, 0);
            }),
            Err(RingError::Order)
        );
    }

    /// A hostile driver's available index: more chains than the ring holds at once, by one and by
    /// the most; and a chain made available again while the device serves it.
    #[test]
    fn indices_out_of_bounds_are_refused() {
        for jump in [SIZE + 1, u16::MAX] {
            let ring = Owned::new(SIZE);
            ring.avail_idx.store(jump, Ordering::Release);
            assert_eq!(Device::new(ring, 0).pop(within), Err(RingError::TooMany(jump, SIZE)));
        }
        let ring = Owned::new(SIZE);
        set(&ring, 0, 0, 8, 0, 0);
        offer(&ring, 0, 0);
        offer(&ring, 1, 0);
        let mut device = Device::new(ring, 0);
        assert_eq!(device.pop(within).unwrap().unwrap().head, 0);
        assert_eq!(device.pop(within), Err(RingError::Busy(0)));
    }

    /// The device returning what it wasn't given ends the driver's ring, its bug, never the
    /// driver's process.
    #[test]
    fn chains_never_given_are_refused() {
        let ring = Arc::new(Owned::new(SIZE));
        let mut driver = Driver::new(ring.clone());
        driver.add(&[readable(0, 8)]).unwrap();
        driver.publish();
        for id in [1, u32::from(SIZE), u32::MAX] {
            ring.used_ring[0].id.store(id, Ordering::Relaxed);
            ring.used_idx.store(1, Ordering::Release);
            assert_eq!(driver.used(), Err(UnknownChain(id)));
        }
    }

    /// Chains added in any order and number, returned in any order, wrapping the indices many
    /// times: each reaches the device once, as added, and comes back once, as returned, with no
    /// descriptor lost.
    #[test]
    fn chains_go_round_and_come_back() {
        let mut random = rand::rngs::StdRng::seed_from_u64(14537);
        let ring = Arc::new(Owned::new(SIZE));
        let (mut driver, mut device) = (Driver::new(ring.clone()), Device::new(ring, 0));
        let mut added = std::collections::BTreeMap::new();
        let mut serving = Vec::new();
        let mut returned = 0u64;
        for round in 0..100_000u64 {
            match random.random_range(0..4) {
                0 => {
                    let len = random.random_range(1..=3);
                    let segments: Vec<_> = (0..len)
                        .map(|i| Segment {
                            offset: round % BUFFERS,
                            len: 1,
                            writable: i + 1 == len && random.random(),
                        })
                        .collect();
                    if let Ok(head) = driver.add(&segments) {
                        assert_eq!(added.insert(head, segments), None);
                    }
                }
                1 => {
                    device.arm();
                    driver.publish();
                    while let Some(chain) = device.pop(within).unwrap() {
                        assert_eq!(Some(&chain.segments), added.get(&chain.head));
                        serving.push(chain.head);
                    }
                }
                2 if !serving.is_empty() => {
                    let head = serving.swap_remove(random.random_range(0..serving.len()));
                    device.push(head, u32::from(head));
                    device.publish();
                }
                _ => {
                    driver.arm();
                    while let Some(used) = driver.used().unwrap() {
                        assert_eq!(used.len, u32::from(used.head));
                        assert!(added.remove(&used.head).is_some());
                        returned += 1;
                    }
                }
            }
            let held: usize = added.values().map(Vec::len).sum();
            assert_eq!(driver.free() + held, usize::from(SIZE));
        }
        assert!(returned > 10_000, "{returned} chains came back");
    }
}

/// The rings under loom, its every interleaving of the driver's and the device's threads, and the
/// weak orderings of their accesses (docs/memory-ordering.md).
#[cfg(all(test, feature = "loom"))]
mod loom {
    use ::loom::sync::Arc;
    use ::loom::sync::atomic::AtomicBool;

    /// The ring's parts, shared by std's `Arc`, which the models' orderings don't run through:
    /// its atomics are loom's (`Owned`).
    fn ring() -> std::sync::Arc<Owned> {
        std::sync::Arc::new(Owned::new(SIZE))
    }

    use super::owned::Owned;
    use super::*;

    const SIZE: u16 = 4;

    fn within(_: u64, _: u32) -> bool {
        true
    }

    fn chain(offset: u64) -> [Segment; 1] {
        [Segment { offset, len: 512, writable: false }]
    }

    /// A chain made available while the device arms to wait: the device takes it, or the driver
    /// kicks it, never neither, which would leave it asleep with a request waiting (the
    /// suppression's lost wakeup). The device took one chain before, and has not armed since: its
    /// event index is stale, and only `arm` says it wants another kick.
    #[test]
    fn kicks_are_never_lost() {
        ::loom::model(|| {
            let ring = ring();
            let (mut driver, mut device) = (Driver::new(ring.clone()), Device::new(ring, 0));
            driver.add(&chain(0)).unwrap();
            assert!(driver.publish(), "the device's first chain kicks it");
            let first = device.pop(within).unwrap().unwrap();
            device.push(first.head, 0);
            let kicked = Arc::new(AtomicBool::new(false));
            let adding = ::loom::thread::spawn({
                let kicked = kicked.clone();
                move || {
                    driver.add(&chain(512)).unwrap();
                    if driver.publish() {
                        kicked.store(true, Ordering::Relaxed);
                    }
                }
            });
            let took = device.arm() && device.pop(within).unwrap().is_some();
            adding.join().unwrap();
            assert!(took || kicked.load(Ordering::Relaxed), "a chain waits, the device asleep");
        });
    }

    /// A chain returned while the driver arms to wait: the driver takes it back, or the device
    /// interrupts it, never neither. The driver took one back before and has not armed since.
    #[test]
    fn interrupts_are_never_lost() {
        ::loom::model(|| {
            let ring = ring();
            let (mut driver, mut device) = (Driver::new(ring.clone()), Device::new(ring, 0));
            driver.add(&chain(0)).unwrap();
            driver.add(&chain(512)).unwrap();
            driver.publish();
            let (first, second) =
                (device.pop(within).unwrap().unwrap(), device.pop(within).unwrap().unwrap());
            device.push(first.head, 0);
            assert!(device.publish(), "the driver's first chain back interrupts it");
            assert_eq!(driver.used().unwrap().map(|used| used.head), Some(first.head));
            let interrupted = Arc::new(AtomicBool::new(false));
            let returning = ::loom::thread::spawn({
                let interrupted = interrupted.clone();
                move || {
                    device.push(second.head, 0);
                    if device.publish() {
                        interrupted.store(true, Ordering::Relaxed);
                    }
                }
            });
            let took = driver.arm() && driver.used().unwrap().is_some();
            returning.join().unwrap();
            let interrupted = interrupted.load(Ordering::Relaxed);
            assert!(took || interrupted, "a chain back, the driver asleep");
        });
    }

    /// A chain the device takes is the one the driver added, its descriptors whole: published with
    /// the available index.
    #[test]
    fn chains_are_published_whole() {
        ::loom::model(|| {
            let ring = ring();
            let (mut driver, mut device) = (Driver::new(ring.clone()), Device::new(ring, 0));
            let adding = ::loom::thread::spawn(move || {
                driver.add(&chain(1024)).unwrap();
                driver.publish();
            });
            if let Some(taken) = device.pop(within).unwrap() {
                assert_eq!(taken.segments, chain(1024));
            }
            adding.join().unwrap();
        });
    }
}
