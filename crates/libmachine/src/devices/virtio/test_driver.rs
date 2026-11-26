// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A virtio driver for device tests: split virtqueues in guest memory of
//! its own, buffers posted as the guest's driver would, and what the device
//! did read back, including whether it asked to be notified.

use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryMmap};

use super::queue::DeviceQueue;

const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;
/// Every queue's size.
pub(crate) const SIZE: u16 = 16;
const MEMORY: u64 = 8 << 20;
const BUFFERS: u64 = 0x10_0000;

pub(crate) struct Driver {
    pub(crate) mem: GuestMemoryMmap,
    pub(crate) queues: Vec<DeviceQueue>,
    next_desc: Vec<u16>,
    avail_idx: Vec<u16>,
    used_seen: Vec<u16>,
    /// Each posted head's buffers, per queue: (address, length).
    pub(crate) buffers: Vec<Vec<Vec<(u64, u32)>>>,
    next_buf: u64,
}

impl Driver {
    pub(crate) const SIZE: u16 = SIZE;

    /// `queues` ready queues of [`SIZE`] entries, with EVENT_IDX if asked.
    pub(crate) fn new(queues: usize, event_idx: bool) -> Self {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), MEMORY as usize)]).unwrap();
        let queues: Vec<_> = (0..queues)
            .map(|q| {
                let mut queue = DeviceQueue::new(SIZE).unwrap();
                let base = Self::ring_base(q);
                queue.set_desc_table_address(Some(base as u32), Some(0));
                queue.set_avail_ring_address(Some((base + 0x1000) as u32), Some(0));
                queue.set_used_ring_address(Some((base + 0x2000) as u32), Some(0));
                queue.set_ready(true);
                queue.set_event_idx(event_idx);
                assert!(queue.is_valid(&mem));
                queue
            })
            .collect();
        let n = queues.len();
        Self {
            mem,
            queues,
            next_desc: vec![0; n],
            avail_idx: vec![0; n],
            used_seen: vec![0; n],
            buffers: vec![vec![Vec::new(); usize::from(SIZE)]; n],
            next_buf: BUFFERS,
        }
    }

    /// Where queue `q`'s descriptor table is; its avail ring is 4 KiB
    /// after, its used ring 8 KiB after.
    pub(crate) fn ring_base(q: usize) -> u64 {
        0x1_0000 * (q as u64 + 1)
    }

    fn alloc(&mut self, len: u32) -> u64 {
        let size = u64::from(len).next_multiple_of(64).max(64);
        if self.next_buf + size > MEMORY {
            self.next_buf = BUFFERS;
        }
        let addr = self.next_buf;
        self.next_buf += size;
        addr
    }

    /// Posts a chain of descriptors on queue `q`: (data, device-writable).
    pub(crate) fn post(&mut self, q: usize, parts: &[(Vec<u8>, bool)]) {
        let head = self.next_desc[q];
        let mut bufs = Vec::new();
        for (i, (data, write)) in parts.iter().enumerate() {
            let idx = self.next_desc[q];
            self.next_desc[q] = (idx + 1) % SIZE;
            let addr = self.alloc(data.len() as u32);
            self.mem.write_slice(data, GuestAddress(addr)).unwrap();
            bufs.push((addr, data.len() as u32));
            let mut flags = if *write { DESC_F_WRITE } else { 0 };
            if i + 1 < parts.len() {
                flags |= DESC_F_NEXT;
            }
            let mut d = Vec::new();
            d.extend_from_slice(&addr.to_le_bytes());
            d.extend_from_slice(&(data.len() as u32).to_le_bytes());
            d.extend_from_slice(&flags.to_le_bytes());
            d.extend_from_slice(&self.next_desc[q].to_le_bytes());
            let at = GuestAddress(Self::ring_base(q) + 16 * u64::from(idx));
            self.mem.write_slice(&d, at).unwrap();
        }
        self.buffers[q][usize::from(head)] = bufs;
        let avail = GuestAddress(Self::ring_base(q) + 0x1000);
        let slot = avail.unchecked_add(4 + 2 * u64::from(self.avail_idx[q] % SIZE));
        self.mem.write_obj(head, slot).unwrap();
        self.avail_idx[q] = self.avail_idx[q].wrapping_add(1);
        self.mem.write_obj(self.avail_idx[q], avail.unchecked_add(2)).unwrap();
    }

    /// The used entries the device added to queue `q` since last asked:
    /// (head, length).
    pub(crate) fn used(&mut self, q: usize) -> Vec<(u16, u32)> {
        let used = GuestAddress(Self::ring_base(q) + 0x2000);
        let idx: u16 = self.mem.read_obj(used.unchecked_add(2)).unwrap();
        let mut out = Vec::new();
        while self.used_seen[q] != idx {
            let at = used.unchecked_add(4 + 8 * u64::from(self.used_seen[q] % SIZE));
            let id: u32 = self.mem.read_obj(at).unwrap();
            let len: u32 = self.mem.read_obj(at.unchecked_add(4)).unwrap();
            out.push((id as u16, len));
            self.used_seen[q] = self.used_seen[q].wrapping_add(1);
        }
        out
    }

    /// The first `len` bytes of the buffer posted at `head` on queue `q`.
    pub(crate) fn bytes(&self, q: usize, head: u16, len: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        for &(addr, size) in &self.buffers[q][usize::from(head)] {
            let mut b = vec![0; size as usize];
            self.mem.read_slice(&mut b, GuestAddress(addr)).unwrap();
            bytes.extend(b);
        }
        bytes.truncate(len as usize);
        bytes
    }

    /// EVENT_IDX's avail event index for queue `q`: the driver notifies
    /// when its avail index passes it.
    pub(crate) fn avail_event(&self, q: usize) -> u16 {
        let at = Self::ring_base(q) + 0x2000 + 4 + 8 * u64::from(SIZE);
        self.mem.read_obj(GuestAddress(at)).unwrap()
    }

    /// The used ring's flags for queue `q` (1: VIRTQ_USED_F_NO_NOTIFY).
    pub(crate) fn used_flags(&self, q: usize) -> u16 {
        let at = Self::ring_base(q) + 0x2000;
        self.mem.read_obj(GuestAddress(at)).unwrap()
    }

    /// Whether the driver would notify queue `q` on posting its next
    /// buffer, as Linux's virtqueue_kick_prepare decides.
    pub(crate) fn would_notify(&self, q: usize, event_idx: bool) -> bool {
        if event_idx {
            // vring_need_event(event, new, old) with new = old + 1.
            self.avail_event(q) == self.avail_idx[q]
        } else {
            self.used_flags(q) & 1 == 0
        }
    }
}
