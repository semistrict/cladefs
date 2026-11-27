// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! virtio-net (virtio 1.2, section 5.1): the guest's network card, whose
//! other end is the VMM's network stack ([`crate::net`]).
//!
//! Two queues, receive and transmit; no control queue, no multiqueue, no
//! offloads. Features offered:
//! - VIRTIO_NET_F_MAC: the guest's MAC address is in the configuration.
//! - VIRTIO_NET_F_MTU: a 65520-byte MTU. Large frames give the throughput
//!   checksum and segmentation offloads would, without the guest handing
//!   the VMM partial checksums and oversized segments to complete.
//! - VIRTIO_NET_F_MRG_RXBUF: a received frame may span several of the
//!   driver's buffers, so a large MTU does not make the driver post
//!   64 KiB buffers (Linux would otherwise post them, 17 pages each).
//!
//! The device only moves frames: between the virtqueues and the [`Link`]
//! to the network thread, which runs the stack. Both directions are
//! bounded: a full link leaves the guest's frames on its transmit queue,
//! and a guest that posts no receive buffers makes the stack wait.
//!
//!
//! A machine with a network is never snapshotted: its connections are host sockets that no
//! restored machine could share, so the device saves no state.

use std::sync::Arc;

use crate::sys::EventFd;
use virtio_queue::DescriptorChain;
use vm_memory::{Bytes, GuestMemoryMmap};

use super::VirtioDevice;
use super::queue::{DeviceQueue, WritableBuffer};
use crate::error::Result;
use crate::net::host::{LINK_FRAMES, Link, NetThread};
use crate::net::stack::{GUEST_MAC, MAX_FRAME};
use crate::net::{MTU, Shared};

const VIRTIO_ID_NET: u16 = 1;
const VIRTIO_NET_F_MTU: u32 = 3;
const VIRTIO_NET_F_MAC: u32 = 5;
const VIRTIO_NET_F_MRG_RXBUF: u32 = 15;
const RX: usize = 0;
const TX: usize = 1;
const QUEUE_SIZES: [u16; 2] = [256, 256];
/// `struct virtio_net_hdr` with `num_buffers`: 12 bytes for a modern
/// device whatever the features (virtio 1.2, section 5.1.6).
const HEADER_LEN: usize = 12;
/// The configuration: mac (6), status (2), max_virtqueue_pairs (2), mtu
/// (2). Status and the queue pairs are only valid with features this
/// device does not offer, and read as 0.
const CONFIG_LEN: usize = 12;
/// The device, owned by its transport on the device thread. It owns the
/// network thread, which stops when the device goes.
pub(crate) struct NetDevice {
    // Field order: the thread stops before the link it uses is released.
    _thread: NetThread,
    link: Arc<Link>,
    mrg_rxbuf: bool,
    /// Receive buffers taken off the queue but not yet used: too few for
    /// the next frame. Holding them (rather than handing them back to the
    /// queue) leaves the queue's next position at the driver's index, so
    /// that the driver notifies when it posts the next buffer.
    held: std::collections::VecDeque<WritableBuffer>,
}

impl NetDevice {
    /// The device, with its network thread applying `shared`'s policy.
    pub(crate) fn new(shared: Arc<Shared>) -> Result<Self> {
        let link = Link::new()?;
        let thread = NetThread::spawn(link.clone(), shared)?;
        Ok(Self { _thread: thread, link, mrg_rxbuf: false, held: Default::default() })
    }

    /// The device side alone, for tests that play the network thread.
    #[cfg(test)]
    fn detached() -> (Self, Arc<Link>) {
        let link = Link::new().unwrap();
        let shared = Shared::new(crate::net::NetworkConfig::default()).unwrap();
        // A thread that only waits: the test takes frames off the link.
        let idle = Link::new().unwrap();
        let thread = NetThread::spawn(idle, shared).unwrap();
        (
            Self {
                _thread: thread,
                link: link.clone(),
                mrg_rxbuf: false,
                held: Default::default(),
            },
            link,
        )
    }

    fn serve(&mut self, queues: &mut [DeviceQueue], mem: &GuestMemoryMmap) {
        if queues[TX].ready() {
            self.take_tx(&mut queues[TX], mem);
        }
        if queues[RX].ready() {
            self.fill_rx(&mut queues[RX], mem);
        }
    }

    /// Moves the guest's frames from the transmit queue to the link.
    fn take_tx(&self, queue: &mut DeviceQueue, mem: &GuestMemoryMmap) {
        let mut frames = self.link.lock();
        let mut pushed = false;
        queue.quiet(mem);
        loop {
            if frames.to_stack.len() >= LINK_FRAMES {
                // The rest waits on the queue until the stack has room and
                // wakes the device.
                frames.device_waiting = true;
                break;
            }
            let Some(chain) = queue.pop(mem) else {
                break;
            };
            let head = chain.head_index();
            if let Some(frame) = read_frame(chain, mem) {
                frames.to_stack.push_back(frame);
                pushed = true;
            }
            if !queue.add_used(mem, head, 0) {
                break;
            }
        }
        drop(frames);
        if pushed {
            self.link.wake_stack();
        }
    }

    /// Moves frames from the link into the receive buffers the driver
    /// posted. Out of buffers, it has asked the driver to notify when it
    /// posts more (DeviceQueue::pop).
    fn fill_rx(&mut self, queue: &mut DeviceQueue, mem: &GuestMemoryMmap) {
        let link = self.link.clone();
        let mut frames = link.lock();
        while let Some(frame) = frames.to_guest.front() {
            match self.place(frame, queue, mem) {
                Placed::Done | Placed::Dropped => {
                    frames.to_guest.pop_front();
                }
                Placed::NoRoom => break,
            }
        }
        let wake = frames.stack_waiting && frames.to_guest.len() < LINK_FRAMES;
        if wake {
            frames.stack_waiting = false;
        }
        drop(frames);
        if wake {
            self.link.wake_stack();
        }
    }

    /// Writes `frame` into receive buffers: as many as it takes with
    /// MRG_RXBUF, else one, which must hold it all.
    fn place(&mut self, frame: &[u8], queue: &mut DeviceQueue, mem: &GuestMemoryMmap) -> Placed {
        let need = HEADER_LEN + frame.len();
        loop {
            let room: usize = self.held.iter().map(|b| b.room).sum();
            if room >= need || (!self.mrg_rxbuf && !self.held.is_empty()) {
                break;
            }
            if self.held.len() >= usize::from(queue.size()) {
                // A whole ring of buffers cannot hold it: it never will.
                return Placed::Dropped;
            }
            let Some(chain) = queue.pop(mem) else {
                return Placed::NoRoom;
            };
            self.held.push_back(WritableBuffer::new(chain));
        }
        if !self.mrg_rxbuf && self.held[0].room < need {
            // One buffer, too small: the driver posts buffers for its MTU,
            // so this frame is not one it can take. The buffer waits for
            // the next frame.
            return Placed::Dropped;
        }
        // The fewest held buffers, in order, that hold the frame.
        let mut count = 0;
        let mut room = 0;
        while room < need {
            room += self.held[count].room;
            count += 1;
        }
        let mut header = [0u8; HEADER_LEN];
        header[10..12].copy_from_slice(&(count as u16).to_le_bytes());
        let mut parts = [&header[..], frame];
        for buffer in self.held.drain(..count) {
            let written = buffer.write(mem, &mut parts);
            if !queue.add_used(mem, buffer.head, written) {
                return Placed::Dropped;
            }
        }
        Placed::Done
    }
}

enum Placed {
    Done,
    /// The frame cannot be delivered and is dropped.
    Dropped,
    /// Not enough buffers yet.
    NoRoom,
}

/// A frame from the device-readable descriptors of `chain`, without its
/// virtio-net header: None if shorter than a header and an Ethernet
/// header, or longer than the MTU allows.
fn read_frame(chain: DescriptorChain<&GuestMemoryMmap>, mem: &GuestMemoryMmap) -> Option<Vec<u8>> {
    let limit = HEADER_LEN + MAX_FRAME;
    let mut bytes = Vec::new();
    for desc in chain.readable() {
        let len = desc.len() as usize;
        if bytes.len() + len > limit {
            return None;
        }
        let start = bytes.len();
        bytes.resize(start + len, 0);
        mem.read_slice(&mut bytes[start..], desc.addr()).ok()?;
    }
    if bytes.len() < HEADER_LEN + 14 {
        return None;
    }
    bytes.drain(..HEADER_LEN);
    Some(bytes)
}

impl VirtioDevice for NetDevice {
    fn device_type(&self) -> u16 {
        VIRTIO_ID_NET
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &QUEUE_SIZES
    }

    fn features(&self) -> u64 {
        1 << VIRTIO_NET_F_MTU | 1 << VIRTIO_NET_F_MAC | 1 << VIRTIO_NET_F_MRG_RXBUF
    }

    fn config_size(&self) -> usize {
        CONFIG_LEN
    }

    fn read_config(&self, offset: usize, data: &mut [u8]) {
        let mut config = [0u8; CONFIG_LEN];
        config[..6].copy_from_slice(&GUEST_MAC);
        config[10..12].copy_from_slice(&MTU.to_le_bytes());
        for (i, b) in data.iter_mut().enumerate() {
            *b = config.get(offset + i).copied().unwrap_or(0);
        }
    }

    fn activate(&mut self, features: u64) {
        self.mrg_rxbuf = features & (1 << VIRTIO_NET_F_MRG_RXBUF) != 0;
        self.held.clear();
    }

    fn reset(&mut self) {
        self.held.clear();
        // Frames for a driver that is gone; the stack may send again.
        let mut frames = self.link.lock();
        frames.to_guest.clear();
        frames.to_stack.clear();
        frames.device_waiting = false;
        let wake = std::mem::take(&mut frames.stack_waiting);
        drop(frames);
        if wake {
            self.link.wake_stack();
        }
    }

    fn process_queue(&mut self, _index: usize, queues: &mut [DeviceQueue], mem: &GuestMemoryMmap) {
        self.serve(queues, mem);
    }

    fn wake_fd(&self) -> Option<&EventFd> {
        Some(self.link.device_wake())
    }

    fn woken(&mut self, queues: &mut [DeviceQueue], mem: &GuestMemoryMmap) {
        self.serve(queues, mem);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::virtio::test_driver::Driver;

    fn frame(len: usize, fill: u8) -> Vec<u8> {
        vec![fill; len]
    }

    #[test]
    fn config_and_features() {
        let (dev, _) = NetDevice::detached();
        assert_eq!(dev.device_type(), 1);
        assert_eq!(dev.features(), 1 << 3 | 1 << 5 | 1 << 15);
        let mut config = [0xff; 12];
        dev.read_config(0, &mut config);
        assert_eq!(&config[..6], &GUEST_MAC);
        assert_eq!(&config[6..10], &[0; 4]);
        assert_eq!(u16::from_le_bytes([config[10], config[11]]), 65520);
    }

    #[test]
    fn transmitted_frames_reach_the_link_without_their_header() {
        let (mut dev, link) = NetDevice::detached();
        dev.activate(0);
        let mut d = Driver::new(2, false);
        let mut packet = vec![0u8; HEADER_LEN];
        packet.extend(frame(60, 0xaa));
        // Header and frame in one descriptor, and split across two.
        d.post(TX, &[(packet.clone(), false)]);
        d.post(TX, &[(packet[..5].to_vec(), false), (packet[5..].to_vec(), false)]);
        // Too short; a writable descriptor carries nothing.
        d.post(TX, &[(vec![0; HEADER_LEN + 13], false)]);
        d.post(TX, &[(packet.clone(), true)]);
        dev.process_queue(TX, &mut d.queues, &d.mem);
        assert_eq!(d.used(TX).len(), 4, "every buffer goes back");
        let frames = std::mem::take(&mut link.lock().to_stack);
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|f| *f == frame(60, 0xaa)));
    }

    #[test]
    fn a_full_link_leaves_frames_on_the_queue() {
        let (mut dev, link) = NetDevice::detached();
        let mut d = Driver::new(2, false);
        for _ in 0..LINK_FRAMES - 1 {
            link.lock().to_stack.push_back(frame(60, 0));
        }
        let mut packet = vec![0u8; HEADER_LEN];
        packet.extend(frame(60, 1));
        for _ in 0..3 {
            d.post(TX, &[(packet.clone(), false)]);
        }
        dev.process_queue(TX, &mut d.queues, &d.mem);
        assert_eq!(d.used(TX).len(), 1);
        assert!(link.lock().device_waiting);
        // The stack drains the link and wakes the device.
        link.lock().to_stack.clear();
        dev.woken(&mut d.queues, &d.mem);
        assert_eq!(d.used(TX).len(), 2);
        assert_eq!(link.lock().to_stack.len(), 2);
    }

    #[test]
    fn received_frames_span_mergeable_buffers() {
        let (mut dev, link) = NetDevice::detached();
        dev.activate(1 << VIRTIO_NET_F_MRG_RXBUF);
        let mut d = Driver::new(2, false);
        let big = frame(3000, 0x55);
        link.lock().to_guest.push_back(big.clone());
        // Not enough room yet: nothing is used, the frame waits.
        d.post(RX, &[(vec![0; 1500], true)]);
        dev.woken(&mut d.queues, &d.mem);
        assert!(d.used(RX).is_empty());
        d.post(RX, &[(vec![0; 1000], true), (vec![0; 500], true)]);
        d.post(RX, &[(vec![0; 1500], true)]);
        dev.process_queue(RX, &mut d.queues, &d.mem);
        let used = d.used(RX);
        assert_eq!(used.len(), 3);
        let bytes: Vec<u8> = used.iter().flat_map(|&(h, l)| d.bytes(RX, h, l)).collect();
        assert_eq!(u16::from_le_bytes([bytes[10], bytes[11]]), 3, "num_buffers");
        assert_eq!(&bytes[HEADER_LEN..], &big[..]);
        assert!(link.lock().to_guest.is_empty());
    }

    /// A frame waiting for more receive buffers than the driver posted
    /// must leave the driver asked to notify on its next buffer: with
    /// EVENT_IDX, the avail event index at the driver's avail index. (A
    /// device that hands the buffers it looked at back to the queue leaves
    /// the event index behind it, and the driver never notifies: the frame
    /// waits for unrelated traffic.)
    #[test]
    fn waiting_for_buffers_asks_the_driver_to_notify() {
        let (mut dev, link) = NetDevice::detached();
        dev.activate(1 << VIRTIO_NET_F_MRG_RXBUF);
        let mut d = Driver::new(2, true);
        link.lock().to_guest.push_back(frame(3000, 1));
        for posted in 1..=2u16 {
            d.post(RX, &[(vec![0; 1000], true)]);
            dev.process_queue(RX, &mut d.queues, &d.mem);
            assert!(d.used(RX).is_empty(), "{posted} buffers are too few");
            assert_eq!(d.avail_event(RX), posted, "notify on the next buffer");
        }
        d.post(RX, &[(vec![0; 1500], true)]);
        dev.process_queue(RX, &mut d.queues, &d.mem);
        assert_eq!(d.used(RX).len(), 3);
    }

    /// Out of receive buffers with a frame to deliver, or done with every
    /// transmit buffer, the device has asked the driver to notify on its
    /// next buffer, with EVENT_IDX or without.
    #[test]
    fn out_of_buffers_the_driver_is_asked_to_notify() {
        for event_idx in [true, false] {
            let (mut dev, link) = NetDevice::detached();
            dev.activate(1 << VIRTIO_NET_F_MRG_RXBUF);
            let mut d = Driver::new(2, event_idx);
            let mut packet = vec![0u8; HEADER_LEN];
            packet.extend(frame(60, 1));
            d.post(TX, &[(packet.clone(), false)]);
            d.post(TX, &[(packet, false)]);
            dev.process_queue(TX, &mut d.queues, &d.mem);
            assert_eq!(d.used(TX).len(), 2);
            assert!(d.would_notify(TX, event_idx), "tx, event_idx {event_idx}");
            link.lock().to_guest.push_back(frame(3000, 1));
            d.post(RX, &[(vec![0; 1000], true)]);
            dev.process_queue(RX, &mut d.queues, &d.mem);
            assert!(d.used(RX).is_empty());
            assert!(d.would_notify(RX, event_idx), "rx, event_idx {event_idx}");
        }
    }

    #[test]
    fn without_mergeable_buffers_a_frame_needs_one_buffer() {
        let (mut dev, link) = NetDevice::detached();
        dev.activate(0);
        let mut d = Driver::new(2, false);
        link.lock().to_guest.push_back(frame(2000, 1));
        link.lock().to_guest.push_back(frame(100, 2));
        link.lock().stack_waiting = true;
        d.post(RX, &[(vec![0; 1500], true)]);
        dev.process_queue(RX, &mut d.queues, &d.mem);
        // The big frame is dropped; the small one uses the same buffer.
        let used = d.used(RX);
        assert_eq!(used.len(), 1);
        let bytes = d.bytes(RX, used[0].0, used[0].1);
        assert_eq!(bytes.len(), HEADER_LEN + 100);
        assert_eq!(u16::from_le_bytes([bytes[10], bytes[11]]), 1);
        assert!(!link.lock().stack_waiting, "the stack was woken");
    }

    #[test]
    fn reset_drops_queued_frames() {
        let (mut dev, link) = NetDevice::detached();
        link.lock().to_guest.push_back(frame(100, 2));
        link.lock().to_stack.push_back(frame(100, 2));
        dev.reset();
        assert!(link.lock().to_guest.is_empty());
        assert!(link.lock().to_stack.is_empty());
    }
}
