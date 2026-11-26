// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A virtqueue as devices see it: [`virtio_queue::Queue`] without the ways
//! to lose a buffer or a notification.
//!
//! A device takes buffers with [`DeviceQueue::pop`] and gives them back with
//! [`DeviceQueue::add_used`]; there is no way to hand a buffer back to the
//! ring. Handing one back (`go_to_previous_position`) was the bug of board 13097
//! and board 13149: the buffer comes up first again the next time, so a device
//! waiting for room never gets past it, and with EVENT_IDX the avail event
//! index stays behind the driver's index, so the driver stops notifying. A
//! device that cannot use a buffer yet holds it instead.
//!
//! And whenever `pop` finds the ring empty it asks the driver to notify on
//! its next buffer, then looks once more (a buffer made available in
//! between would otherwise be missed). A device that ran out of buffers
//! while it has work owed therefore always hears of the next one, without
//! each device getting the disable/drain/enable dance right on its own.

use std::num::Wrapping;
use std::sync::atomic::Ordering;

use virtio_queue::{DescriptorChain, Queue, QueueState, QueueT};
use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryMmap};

use crate::error::{Error, Result};
use crate::snapshot::codec::{Decoder, Encoder};

/// A virtqueue.
#[derive(Debug)]
pub(crate) struct DeviceQueue {
    queue: Queue,
}

impl DeviceQueue {
    /// A queue of at most `max_size` entries (a power of two).
    pub(crate) fn new(max_size: u16) -> std::result::Result<Self, virtio_queue::Error> {
        Ok(Self { queue: Queue::new(max_size)? })
    }

    // What devices use.

    /// Whether the driver has enabled the queue.
    pub(crate) fn ready(&self) -> bool {
        self.queue.ready()
    }

    /// The queue's size, as the driver set it.
    pub(crate) fn size(&self) -> u16 {
        self.queue.size()
    }

    /// The next buffer the driver made available. When there is none, asks
    /// the driver to notify when it makes the next one available, and looks
    /// once more in case it just did. While a device drains the queue,
    /// notifications are suppressed (see [`quiet`](Self::quiet)).
    pub(crate) fn pop<'m>(
        &mut self,
        mem: &'m GuestMemoryMmap,
    ) -> Option<DescriptorChain<&'m GuestMemoryMmap>> {
        if let Some(chain) = self.queue.pop_descriptor_chain(mem) {
            return Some(chain);
        }
        match self.queue.enable_notification(mem) {
            // More arrived meanwhile: drain it quietly again. (Looking only
            // once more: an index the ring refuses, a driver lying about
            // it, must not spin the device.)
            Ok(true) => {
                self.quiet(mem);
                self.queue.pop_descriptor_chain(mem)
            }
            // Empty and asked to notify; or the rings are unreadable, and
            // the driver must reset the device.
            Ok(false) | Err(_) => None,
        }
    }

    /// Tells the driver it need not notify while the device drains the
    /// queue (without EVENT_IDX; with it, the event index already does).
    /// [`pop`](Self::pop) asks for notifications again when it finds the
    /// queue empty.
    pub(crate) fn quiet(&mut self, mem: &GuestMemoryMmap) {
        let _ = self.queue.disable_notification(mem);
    }

    /// Returns the buffer whose chain starts at `head` to the driver, with
    /// `len` bytes written into it. False if guest memory refused the write
    /// (the driver broke its rings and must reset the device).
    pub(crate) fn add_used(&mut self, mem: &GuestMemoryMmap, head: u16, len: u32) -> bool {
        self.queue.add_used(mem, head, len).is_ok()
    }

    /// The driver's available index, as it is in guest memory now.
    pub(crate) fn avail_idx(&self, mem: &GuestMemoryMmap) -> Option<u16> {
        self.queue.avail_idx(mem, Ordering::Acquire).ok().map(|Wrapping(i)| i)
    }

    /// How many buffers the device has taken off the ring.
    pub(crate) fn next_avail(&self) -> u16 {
        self.queue.next_avail()
    }

    /// Asks the driver to notify once its available index moves past
    /// `seen` (an index the device has read), even while buffers the
    /// device has not taken are still posted: what a device needs that
    /// waits for the driver's next buffer rather than for any buffer.
    /// [`pop`](Self::pop) asks only when the ring is empty (the avail event
    /// index at the device's next one), which a driver that keeps buffers
    /// posted never crosses. Returns whether the index has already moved
    /// (look again: the driver may have posted before it could see this).
    pub(crate) fn notify_past(&mut self, mem: &GuestMemoryMmap, seen: u16) -> bool {
        if self.queue.event_idx_enabled() {
            // virtio 1.2, 2.7.10: the driver notifies when its index passes
            // avail_event, the u16 after the used ring's elements.
            let at = self.queue.used_ring().checked_add(4 + 8 * u64::from(self.queue.size()));
            if let Some(at) = at {
                let _ = mem.store(seen.to_le(), GuestAddress(at), Ordering::Relaxed);
            }
        } else {
            // The flags ask for every notification.
            let _ = self.queue.enable_notification(mem);
        }
        std::sync::atomic::fence(Ordering::SeqCst);
        self.avail_idx(mem) != Some(seen)
    }

    // What the transport uses.

    /// How many buffers the device has put in the used ring.
    pub(crate) fn next_used(&self) -> u16 {
        self.queue.next_used()
    }

    /// Whether the driver wants an interrupt for what was used since the
    /// last one (EVENT_IDX's used event index, or the flags).
    pub(crate) fn needs_notification(&mut self, mem: &GuestMemoryMmap) -> bool {
        // On a guest memory error, interrupt: the driver will look.
        self.queue.needs_notification(mem).unwrap_or(true)
    }

    pub(crate) fn try_set_size(&mut self, size: u16) -> bool {
        self.queue.try_set_size(size).is_ok()
    }

    pub(crate) fn set_ready(&mut self, ready: bool) {
        self.queue.set_ready(ready);
    }

    pub(crate) fn set_desc_table_address(&mut self, low: Option<u32>, high: Option<u32>) {
        self.queue.set_desc_table_address(low, high);
    }

    pub(crate) fn set_avail_ring_address(&mut self, low: Option<u32>, high: Option<u32>) {
        self.queue.set_avail_ring_address(low, high);
    }

    pub(crate) fn set_used_ring_address(&mut self, low: Option<u32>, high: Option<u32>) {
        self.queue.set_used_ring_address(low, high);
    }

    pub(crate) fn desc_table(&self) -> u64 {
        self.queue.desc_table()
    }

    pub(crate) fn avail_ring(&self) -> u64 {
        self.queue.avail_ring()
    }

    pub(crate) fn used_ring(&self) -> u64 {
        self.queue.used_ring()
    }

    /// Whether the rings lie in guest memory, aligned, for the size set.
    pub(crate) fn is_valid(&self, mem: &GuestMemoryMmap) -> bool {
        self.queue.is_valid(mem)
    }

    pub(crate) fn set_event_idx(&mut self, enabled: bool) {
        self.queue.set_event_idx(enabled);
    }

    /// Back to the state before the driver set the queue up.
    pub(crate) fn reset(&mut self) {
        self.queue.reset();
    }

    /// What the queue keeps outside guest memory, for a snapshot: the
    /// driver's settings and the device's positions in the rings.
    pub(crate) fn save(&self, e: &mut Encoder) {
        let q = &self.queue;
        e.u16(q.size());
        e.bool(q.ready());
        e.u64(q.desc_table());
        e.u64(q.avail_ring());
        e.u64(q.used_ring());
        e.u16(q.next_avail());
        e.u16(q.next_used());
        e.bool(q.event_idx_enabled());
    }

    /// Puts back what [`save`](Self::save) wrote, into a queue of the same
    /// maximum size.
    pub(crate) fn restore(&mut self, d: &mut Decoder<'_>) -> Result<()> {
        let state = QueueState {
            max_size: self.queue.max_size(),
            size: d.u16()?,
            ready: d.bool()?,
            desc_table: d.u64()?,
            avail_ring: d.u64()?,
            used_ring: d.u64()?,
            next_avail: d.u16()?,
            next_used: d.u16()?,
            event_idx_enabled: d.bool()?,
        };
        self.queue = Queue::try_from(state)
            .map_err(|e| Error::Snapshot(format!("the snapshot's virtqueue: {e}")))?;
        Ok(())
    }
}

/// A buffer taken off a queue for the device to write into, now or later:
/// a device that cannot fill it yet holds it (rather than handing it back to
/// the ring, which `DeviceQueue` cannot do).
pub(crate) struct WritableBuffer {
    /// The head of its descriptor chain, for `add_used`.
    pub(crate) head: u16,
    /// Its device-writable segments, in order.
    segments: Vec<(GuestAddress, usize)>,
    /// Their total length.
    pub(crate) room: usize,
}

impl WritableBuffer {
    pub(crate) fn new(chain: DescriptorChain<&GuestMemoryMmap>) -> Self {
        let head = chain.head_index();
        let segments: Vec<_> = chain.writable().map(|d| (d.addr(), d.len() as usize)).collect();
        let room = segments.iter().map(|s| s.1).sum();
        Self { head, segments, room }
    }

    /// The buffer, for a snapshot of a device that holds it.
    pub(crate) fn save(&self, e: &mut Encoder) {
        e.u16(self.head);
        e.count(self.segments.len());
        for &(addr, len) in &self.segments {
            e.u64(addr.0);
            e.u64(len as u64);
        }
    }

    pub(crate) fn restore(d: &mut Decoder<'_>) -> Result<Self> {
        let head = d.u16()?;
        let n = d.count(16)?;
        let segments: Vec<(GuestAddress, usize)> = (0..n)
            .map(|_| Ok((GuestAddress(d.u64()?), d.u64()? as usize)))
            .collect::<Result<_>>()?;
        let room = segments.iter().map(|s| s.1).sum();
        Ok(Self { head, segments, room })
    }

    /// Writes as much of `parts`, in order, as fits, consuming what it
    /// wrote; returns the bytes written (fewer if a segment leaves guest
    /// memory).
    pub(crate) fn write(&self, mem: &GuestMemoryMmap, parts: &mut [&[u8]]) -> u32 {
        let mut written = 0u32;
        for &(addr, len) in &self.segments {
            let mut at = addr;
            let mut room = len;
            for part in parts.iter_mut() {
                let n = part.len().min(room);
                if n == 0 {
                    continue;
                }
                if mem.write_slice(&part[..n], at).is_err() {
                    return written;
                }
                *part = &part[n..];
                at = at.unchecked_add(n as u64);
                room -= n;
                written += n as u32;
            }
            if parts.iter().all(|p| p.is_empty()) {
                break;
            }
        }
        written
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::virtio::test_driver::Driver;

    /// A device that finds the ring empty has asked the driver to notify on
    /// its next buffer: EVENT_IDX's avail event at the driver's index, or
    /// without it the used ring's NO_NOTIFY flag clear.
    #[test]
    fn finding_the_ring_empty_asks_for_the_next_buffer() {
        for event_idx in [true, false] {
            let mut d = Driver::new(1, event_idx);
            let mem = d.mem.clone();
            d.post(0, &[(vec![0; 16], true)]);
            d.post(0, &[(vec![0; 16], true)]);
            let q = &mut d.queues[0];
            q.quiet(&mem);
            assert!(q.pop(&mem).is_some());
            if !event_idx {
                assert_eq!(d.used_flags(0), 1, "quiet while draining");
            }
            let q = &mut d.queues[0];
            assert!(q.pop(&mem).is_some());
            assert!(q.pop(&mem).is_none());
            if event_idx {
                assert_eq!(d.avail_event(0), 2, "notify on the third buffer");
            } else {
                assert_eq!(d.used_flags(0), 0, "notify");
            }
        }
    }

    /// Buffers posted after the device found the ring empty are taken, and
    /// the device asks again when they run out.
    #[test]
    fn buffers_after_an_empty_ring_are_taken() {
        let mut d = Driver::new(1, true);
        let mem = d.mem.clone();
        d.post(0, &[(vec![0; 16], true)]);
        assert!(d.queues[0].pop(&mem).is_some());
        // The driver's index moves on (as if between pop's two looks).
        d.post(0, &[(vec![0; 16], true)]);
        assert!(d.queues[0].pop(&mem).is_some());
        assert!(d.queues[0].pop(&mem).is_none());
        assert_eq!(d.avail_event(0), 2);
        assert_eq!(d.queues[0].avail_idx(&mem), Some(2));
        assert_eq!(d.queues[0].next_avail(), 2);
    }

    /// A queue and a held buffer come back from a snapshot as they were:
    /// the restored queue goes on from the same ring positions.
    #[test]
    fn queue_and_buffer_state_come_back() {
        let mut d = Driver::new(1, true);
        let mem = d.mem.clone();
        d.post(0, &[(vec![0; 16], true), (vec![0; 32], true)]);
        d.post(0, &[(vec![0; 16], true)]);
        let chain = d.queues[0].pop(&mem).unwrap();
        let held = WritableBuffer::new(chain);
        let mut e = Encoder::default();
        d.queues[0].save(&mut e);
        held.save(&mut e);
        let bytes = e.finish();
        let mut restored = DeviceQueue::new(Driver::SIZE).unwrap();
        let mut dec = Decoder::new(&bytes);
        restored.restore(&mut dec).unwrap();
        let back = WritableBuffer::restore(&mut dec).unwrap();
        dec.end().unwrap();
        assert_eq!((back.head, back.room), (held.head, 48));
        assert_eq!(back.segments, held.segments);
        assert!(restored.ready());
        assert_eq!(restored.next_avail(), 1);
        // The next buffer is the second one posted.
        let next = restored.pop(&mem).unwrap();
        assert_eq!(next.head_index(), 2);
        assert!(restored.add_used(&mem, back.head, 0));
        assert_eq!(d.used(0), vec![(0, 0)]);
    }

    /// Unreadable rings: pop gives nothing and does not spin.
    #[test]
    fn broken_rings_give_nothing() {
        let mut d = Driver::new(1, true);
        let mem = d.mem.clone();
        // An avail index far past the ring's size is the driver lying.
        let avail = GuestAddress(Driver::ring_base(0) + 0x1000);
        mem.write_obj(1000u16, avail.unchecked_add(2)).unwrap();
        assert!(d.queues[0].pop(&mem).is_none());
    }
}
