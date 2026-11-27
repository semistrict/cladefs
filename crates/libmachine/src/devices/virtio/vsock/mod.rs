// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! virtio-vsock (virtio 1.2, section 5.10): stream sockets between the guest
//! and the host, whose end is in this process ([`Vsock`]).
//!
//! Every board machine has one, guest CID 3. The host side lives in-process,
//! so CIDs are per machine: every machine's guest is 3 and its host is 2,
//! with no host kernel module (vhost-vsock), no system-wide CID to
//! allocate and no Unix socket files to name, secure and clean up (the
//! hybrid vsock of Firecracker). Callers that want a socket can bridge one
//! to a [`VsockStream`].
//!
//! Three queues: rx (packets for the guest, in buffers the driver posts),
//! tx (the guest's packets) and event (the transport reset event). Only
//! stream sockets, so the device offers no feature bits (virtio 1.2,
//! section 5.10.3: without VIRTIO_VSOCK_F_STREAM or SEQPACKET negotiated,
//! stream is implied).

mod host;
pub(crate) mod packet;
mod state;

use std::collections::VecDeque;
use std::sync::Arc;

use crate::sys::{EFD_NONBLOCK, EventFd};
use virtio_queue::DescriptorChain;
use vm_memory::{Bytes, GuestMemoryMmap};

use host::Shared;
pub use host::{Vsock, VsockListener, VsockStream};
use packet::{HEADER_LEN, Header, MAX_PAYLOAD};
#[cfg(test)]
pub(crate) use state::guest_stopped;
pub use state::{GuestStopped, is_guest_stopped};
use state::{State, payload_room};

use super::queue::{DeviceQueue, WritableBuffer};
use super::{DeviceState, VirtioDevice, other_version};
use crate::error::{Error, Result};
use crate::snapshot::codec::{Decoder, Encoder};

const VIRTIO_ID_VSOCK: u16 = 19;
/// The guest's CID: the first a guest may have (0-2 are reserved).
pub(crate) const GUEST_CID: u64 = 3;
const RX: usize = 0;
const TX: usize = 1;
const EVENT: usize = 2;
const QUEUE_SIZES: [u16; 3] = [256, 256, 64];
/// VIRTIO_VSOCK_EVENT_TRANSPORT_RESET, the only event.
const EVENT_TRANSPORT_RESET: u32 = 0;
/// The version of the state it saves: the rx buffers it holds and the host
/// port its next connect takes.
const STATE_VERSION: u32 = 4;

/// The device, owned by its transport on the device thread.
pub(crate) struct VsockDevice {
    shared: Arc<Shared>,
    /// The eventfd the device thread waits on.
    wake: EventFd,
    /// Receive buffers taken off the queue that no packet owed fits: room
    /// for a header and no data, while only data is owed. Each waits for a
    /// packet without data; data goes in the buffers after it.
    held: VecDeque<WritableBuffer>,
    /// The driver has started the device (and not reset it since).
    active: bool,
}

/// A random number other than 0.
fn nonzero_random() -> u64 {
    loop {
        let mut b = [0; 8];
        crate::sys::getrandom(&mut b);
        let n = u64::from_le_bytes(b);
        if n != 0 {
            break n;
        }
    }
}

impl VsockDevice {
    /// The device, and the host's handle to it: a new host end, which the
    /// device serves.
    pub(crate) fn new(guest_cid: u64) -> Result<(Self, Vsock)> {
        let wake = EventFd::new(EFD_NONBLOCK).map_err(|source| Error::Hypervisor {
            op: "create the vsock device's wake eventfd",
            source,
        })?;
        let ours = wake.try_clone().map_err(|source| Error::Hypervisor {
            op: "duplicate the vsock device's wake eventfd",
            source,
        })?;
        let shared = Arc::new(Shared::new(guest_cid, wake));
        let vsock = Vsock::new(shared.clone());
        let device = Self { shared, wake: ours, held: VecDeque::new(), active: false };
        Ok((device, vsock))
    }

    /// Uses the ready queues for whatever is owed to the guest, then wakes
    /// the host's waiters.
    fn serve(&mut self, queues: &mut [DeviceQueue], mem: &GuestMemoryMmap, tx: bool) {
        let mut state = self.shared.lock();
        // First whether the guest has taken a reset, so that its new
        // connections are not refused on a stale view.
        if queues[EVENT].ready() {
            look_at_events(&mut state, &mut queues[EVENT], mem);
        }
        if tx && queues[TX].ready() {
            take_tx(&mut state, &mut queues[TX], mem);
        }
        if queues[RX].ready() {
            fill_rx(&mut state, &mut self.held, &mut queues[RX], mem);
        }
        if queues[EVENT].ready() {
            fill_event(&mut state, &mut queues[EVENT], mem);
            look_at_events(&mut state, &mut queues[EVENT], mem);
        }
        // Whatever could go out has; the rest waits for the driver's
        // buffers, which it was asked to notify (DeviceQueue::pop).
        state.wake = false;
        drop(state);
        self.shared.notify();
    }
}

/// Takes the guest's packets off the tx queue.
fn take_tx(state: &mut State, queue: &mut DeviceQueue, mem: &GuestMemoryMmap) {
    queue.quiet(mem);
    while let Some(chain) = queue.pop(mem) {
        let head = chain.head_index();
        if let Some((hdr, payload)) = read_packet(chain, mem) {
            state.guest_packet(&hdr, &payload);
        }
        if !queue.add_used(mem, head, 0) {
            return;
        }
    }
}

/// A packet from the device-readable descriptors of `chain`: None if it is
/// shorter than its header says, or says it is longer than a packet may be.
fn read_packet(
    chain: DescriptorChain<&GuestMemoryMmap>,
    mem: &GuestMemoryMmap,
) -> Option<(Header, Vec<u8>)> {
    let limit = HEADER_LEN + MAX_PAYLOAD;
    let mut bytes = Vec::with_capacity(HEADER_LEN);
    for desc in chain.readable() {
        let take = (desc.len() as usize).min(limit - bytes.len());
        let start = bytes.len();
        bytes.resize(start + take, 0);
        mem.read_slice(&mut bytes[start..], desc.addr()).ok()?;
        if bytes.len() == limit {
            break;
        }
    }
    let hdr = Header::decode(bytes.get(..HEADER_LEN)?.try_into().unwrap());
    let len = hdr.len as usize;
    if len > MAX_PAYLOAD || bytes.len() < HEADER_LEN + len {
        return None;
    }
    bytes.truncate(HEADER_LEN + len);
    Some((hdr, bytes.split_off(HEADER_LEN)))
}

/// Puts the packets owed to the guest into the buffers its driver posted
/// on the rx queue: first the held buffers, for packets that fit them, then
/// buffers off the queue. A buffer that no packet owed fits is held, never
/// handed back to the ring, where it would come up first again (board 13149).
fn fill_rx(
    state: &mut State,
    held: &mut VecDeque<WritableBuffer>,
    queue: &mut DeviceQueue,
    mem: &GuestMemoryMmap,
) {
    while state.has_packet() {
        if let Some(buffer) = held.front() {
            // Held buffers always have room for a header.
            let room = payload_room(buffer.room).unwrap_or(0);
            if let Some((hdr, payload)) = state.next_packet(room) {
                let buffer = held.pop_front().unwrap();
                if !put_packet(queue, mem, &buffer, &hdr, &payload) {
                    return;
                }
                continue;
            }
        }
        // Out of buffers, the driver was asked to notify on its next one.
        let Some(chain) = queue.pop(mem) else {
            return;
        };
        let buffer = WritableBuffer::new(chain);
        let Some(room) = payload_room(buffer.room) else {
            // A buffer too small for a header: give it back empty.
            if !queue.add_used(mem, buffer.head, 0) {
                return;
            }
            continue;
        };
        match state.next_packet(room) {
            Some((hdr, payload)) => {
                if !put_packet(queue, mem, &buffer, &hdr, &payload) {
                    return;
                }
            }
            None => held.push_back(buffer),
        }
    }
}

/// Writes a packet into `buffer` and returns it to the driver; false if
/// guest memory refused the used ring.
fn put_packet(
    queue: &mut DeviceQueue,
    mem: &GuestMemoryMmap,
    buffer: &WritableBuffer,
    hdr: &Header,
    payload: &[u8],
) -> bool {
    let header = hdr.encode();
    let written = buffer.write(mem, &mut [&header[..], payload]);
    queue.add_used(mem, buffer.head, written)
}

/// Tells the state where the event queue is: how many buffers the guest
/// has posted, and whether it has reposted those the events used. While a
/// reset is under way, asks the guest to notify on its next buffer: its
/// repost ends the reset, and with EVENT_IDX it would otherwise notify
/// nobody (Linux keeps its 8 event buffers posted, so the avail event
/// index at the device's next buffer is never crossed), leaving the host's
/// connects to wait until something else woke the device (board 13177).
fn look_at_events(state: &mut State, queue: &mut DeviceQueue, mem: &GuestMemoryMmap) {
    // Looking once more if the guest posted meanwhile; not forever, which
    // a guest moving its index all the time could make it.
    for _ in 0..2 {
        let Some(avail) = queue.avail_idx(mem) else {
            return;
        };
        state.event_queue_at(avail, queue.next_avail());
        // While the event waits for a buffer, the empty ring already asks
        // for the next one (DeviceQueue::pop).
        if !state.resetting || state.event_pending || !queue.notify_past(mem, avail) {
            return;
        }
    }
}

/// Sends the transport reset event, if one is owed, in a buffer the driver
/// posted on the event queue.
fn fill_event(state: &mut State, queue: &mut DeviceQueue, mem: &GuestMemoryMmap) {
    while state.event_pending {
        // How many buffers the guest keeps posted, before taking one.
        look_at_events(state, queue, mem);
        // None: the driver was asked to notify when it posts a buffer.
        let Some(chain) = queue.pop(mem) else {
            return;
        };
        let head = chain.head_index();
        let event = EVENT_TRANSPORT_RESET.to_le_bytes();
        let desc = chain.writable().find(|d| d.len() as usize >= event.len());
        let len = match desc {
            Some(d) if mem.write_slice(&event, d.addr()).is_ok() => event.len() as u32,
            _ => 0,
        };
        if !queue.add_used(mem, head, len) {
            return;
        }
        if len > 0 {
            state.event_sent(queue.next_avail());
        }
    }
}

impl VirtioDevice for VsockDevice {
    fn device_type(&self) -> u16 {
        VIRTIO_ID_VSOCK
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &QUEUE_SIZES
    }

    /// `struct virtio_vsock_config { le64 guest_cid; }`
    fn config_size(&self) -> usize {
        8
    }

    fn read_config(&self, offset: usize, data: &mut [u8]) {
        let config = self.shared.lock().guest_cid().to_le_bytes();
        for (i, b) in data.iter_mut().enumerate() {
            *b = config.get(offset + i).copied().unwrap_or(0);
        }
    }

    fn activate(&mut self, _features: u64) {
        self.held.clear();
        self.active = true;
        self.shared.lock().activate();
        self.shared.notify();
    }

    fn reset(&mut self) {
        self.held.clear();
        self.active = false;
        self.shared.lock().driver_reset();
        self.shared.notify();
    }

    /// The rx buffers it holds (the queues are the transport's), and the host
    /// port its next connect takes. The connections are the host end's, which
    /// no restored machine has.
    fn save(&self) -> Result<DeviceState> {
        let mut e = Encoder::default();
        e.count(self.held.len());
        for buffer in &self.held {
            buffer.save(&mut e);
        }
        e.u32(self.shared.lock().next_port());
        Ok(DeviceState { version: STATE_VERSION, data: e.finish() })
    }

    /// A restored device takes back the buffers it held, and starts with a
    /// transport reset (virtio 1.2, section 5.10.6.7: what a device sends
    /// after a migration), so that the guest drops the connections it had at
    /// the snapshot, whose other ends are not in this machine. It gets a new
    /// random generation, so that the agent client has the guest's agent
    /// reseed its RNG before anything else.
    fn restore(&mut self, state: &DeviceState) -> Result<()> {
        if state.version != STATE_VERSION {
            return Err(other_version(VIRTIO_ID_VSOCK, state.version, STATE_VERSION));
        }
        let mut d = Decoder::new(&state.data);
        let n = d.count(2)?;
        let held =
            (0..n).map(|_| WritableBuffer::restore(&mut d)).collect::<Result<VecDeque<_>>>()?;
        let next_port = d.u32()?;
        d.end()?;
        let mut s = self.shared.lock();
        if !s.set_next_port(next_port) {
            return Err(Error::Snapshot(format!(
                "the vsock's next host port {next_port:#x} is not an ephemeral port"
            )));
        }
        self.held = held;
        s.generation = nonzero_random();
        s.transport_reset();
        drop(s);
        self.shared.notify();
        let _ = self.wake.write(1);
        Ok(())
    }

    fn process_queue(&mut self, index: usize, queues: &mut [DeviceQueue], mem: &GuestMemoryMmap) {
        self.serve(queues, mem, index == TX);
    }

    fn wake_fd(&self) -> Option<&EventFd> {
        Some(&self.wake)
    }

    fn woken(&mut self, queues: &mut [DeviceQueue], mem: &GuestMemoryMmap) {
        self.serve(queues, mem, false);
    }
}

/// The machine is gone, and with it the host end.
impl Drop for VsockDevice {
    fn drop(&mut self) {
        self.shared.lock().machine_gone();
        self.shared.notify();
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Read, Write};
    use std::time::Duration;

    use vm_memory::GuestAddress;

    use super::packet::{Op, TYPE_STREAM};
    use super::state::HOST_CID;
    use super::*;
    use crate::devices::virtio::test_driver::Driver;

    const AGENT_PORT: u32 = 1024;

    /// What a vsock driver does with the test driver's queues.
    trait VsockDriver {
        /// Posts `n` rx buffers of `len` bytes, each in two descriptors
        /// (the first `HEADER_LEN` long) if `split`.
        fn post_rx(&mut self, n: usize, len: usize, split: bool);
        /// The packets the device put in rx buffers since last asked.
        fn received(&mut self) -> Vec<(Header, Vec<u8>)>;
        /// Sends a packet on tx, header and payload in descriptors of
        /// their own if `split`.
        fn send(&mut self, hdr: Header, payload: &[u8], split: bool);
    }

    impl VsockDriver for Driver {
        fn post_rx(&mut self, n: usize, len: usize, split: bool) {
            for _ in 0..n {
                if split {
                    self.post(
                        RX,
                        &[(vec![0; HEADER_LEN], true), (vec![0; len - HEADER_LEN], true)],
                    );
                } else {
                    self.post(RX, &[(vec![0; len], true)]);
                }
            }
        }

        fn received(&mut self) -> Vec<(Header, Vec<u8>)> {
            self.used(RX)
                .into_iter()
                .map(|(head, len)| {
                    let mut bytes = self.bytes(RX, head, len);
                    let hdr = Header::decode(bytes[..HEADER_LEN].try_into().unwrap());
                    assert_eq!(hdr.len as usize, bytes.len() - HEADER_LEN);
                    (hdr, bytes.split_off(HEADER_LEN))
                })
                .collect()
        }

        fn send(&mut self, hdr: Header, payload: &[u8], split: bool) {
            let mut hdr = hdr;
            hdr.len = payload.len() as u32;
            let header = hdr.encode().to_vec();
            if split {
                self.post(TX, &[(header, false), (payload.to_vec(), false)]);
            } else {
                let mut all = header;
                all.extend_from_slice(payload);
                self.post(TX, &[(all, false)]);
            }
        }
    }

    fn from_guest(op: Op, guest_port: u32, host_port: u32) -> Header {
        Header {
            src_cid: GUEST_CID,
            dst_cid: HOST_CID,
            src_port: guest_port,
            dst_port: host_port,
            len: 0,
            socket_type: TYPE_STREAM,
            op,
            flags: 0,
            buf_alloc: 256 << 10,
            fwd_cnt: 0,
        }
    }

    fn device() -> (VsockDevice, Vsock, Driver) {
        let (mut dev, vsock) = VsockDevice::new(GUEST_CID).unwrap();
        dev.activate(0);
        (dev, vsock, Driver::new(3, false))
    }

    fn notify(dev: &mut VsockDevice, d: &mut Driver, q: usize) {
        dev.process_queue(q, &mut d.queues, &d.mem);
    }

    fn woken(dev: &mut VsockDevice, d: &mut Driver) {
        if dev.wake.read().is_ok() {
            dev.woken(&mut d.queues, &d.mem);
        }
    }

    #[test]
    fn config_space_holds_the_guest_cid() {
        let (dev, vsock, _) = device();
        assert_eq!(dev.device_type(), 19);
        assert_eq!(dev.queue_max_sizes().len(), 3);
        assert_eq!(dev.features(), 0, "stream only, implied");
        let mut cid = [0xff; 8];
        dev.read_config(0, &mut cid);
        assert_eq!(u64::from_le_bytes(cid), 3);
        let mut high = [0xff; 4];
        dev.read_config(4, &mut high);
        assert_eq!(high, [0; 4]);
        assert_eq!(vsock.guest_cid(), 3);
    }

    /// The host connects and talks to a guest that answers through the
    /// queues, driven here, while the host blocks in another thread.
    #[test]
    fn host_connection_through_the_queues() {
        let (mut dev, vsock, mut d) = device();
        let host = std::thread::spawn(move || {
            let mut s = vsock.connect(AGENT_PORT, Duration::from_secs(10)).unwrap();
            s.write_all(b"ping").unwrap();
            let mut answer = String::new();
            s.read_to_string(&mut answer).unwrap();
            answer
        });
        d.post_rx(4, 4096, false);
        // The REQUEST.
        let mut got = Vec::new();
        while got.is_empty() {
            std::thread::sleep(Duration::from_millis(1));
            woken(&mut dev, &mut d);
            got = d.received();
        }
        assert_eq!(got.len(), 1);
        let req = got[0].0;
        assert_eq!(req.op, Op::Request);
        assert_eq!((req.src_cid, req.dst_cid, req.dst_port), (2, 3, AGENT_PORT));
        let host_port = req.src_port;
        d.send(from_guest(Op::Response, AGENT_PORT, host_port), &[], false);
        notify(&mut dev, &mut d, TX);
        assert_eq!(d.used(TX).len(), 1);

        // The host's data.
        let mut data = Vec::new();
        while data.len() < 4 {
            std::thread::sleep(Duration::from_millis(1));
            woken(&mut dev, &mut d);
            for (h, p) in d.received() {
                assert_eq!(h.op, Op::Rw);
                data.extend(p);
            }
        }
        assert_eq!(data, b"ping");
        // The guest answers in a split packet and shuts down.
        d.send(from_guest(Op::Rw, AGENT_PORT, host_port), b"pong", true);
        let mut shut = from_guest(Op::Shutdown, AGENT_PORT, host_port);
        shut.flags = packet::SHUTDOWN_BOTH;
        d.send(shut, &[], false);
        notify(&mut dev, &mut d, TX);
        assert_eq!(host.join().unwrap(), "pong");
        // The host's handle went: the connection ends with a reset.
        woken(&mut dev, &mut d);
        let got = d.received();
        assert_eq!(got.last().unwrap().0.op, Op::Rst, "{got:?}");
    }

    #[test]
    fn packets_wait_for_rx_buffers_and_fill_split_ones() {
        let (mut dev, vsock, mut d) = device();
        let listener = vsock.listen(7000).unwrap();
        d.send(from_guest(Op::Request, 4000, 7000), &[], false);
        notify(&mut dev, &mut d, TX);
        assert!(d.received().is_empty(), "no buffer yet");
        let s = listener.accept(Some(Duration::from_secs(1))).unwrap();
        assert_eq!((s.local_port(), s.peer_port()), (7000, 4000));
        // Buffers in two descriptors, the header's and the payload's.
        d.post_rx(2, HEADER_LEN + 8, true);
        notify(&mut dev, &mut d, RX);
        let got = d.received();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0.op, Op::Response);
        (&s).write_all(b"0123456789").unwrap();
        woken(&mut dev, &mut d);
        let got = d.received();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, b"01234567", "as much as fits");
        d.post_rx(1, 4096, false);
        notify(&mut dev, &mut d, RX);
        assert_eq!(d.received()[0].1, b"89");
    }

    /// board 13149: data owed and a buffer with room for only a header. The
    /// device must neither use the buffer for nothing nor hand it back to
    /// the ring (where it would come up first again, forever, and leave
    /// the avail event index behind so that the driver never notifies):
    /// it holds the buffer for a packet that fits, puts the data in the
    /// next buffer, and asks the driver to notify when it posts that one.
    #[test]
    fn a_packet_too_big_for_a_buffer_waits_for_the_next_one() {
        let (mut dev, vsock, mut d) = device();
        for q in &mut d.queues {
            q.set_event_idx(true);
        }
        let listener = vsock.listen(7000).unwrap();
        d.send(from_guest(Op::Request, 4000, 7000), &[], false);
        notify(&mut dev, &mut d, TX);
        let s = listener.accept(Some(Duration::from_secs(1))).unwrap();
        d.post_rx(1, 4096, false);
        notify(&mut dev, &mut d, RX);
        assert_eq!(d.received()[0].0.op, Op::Response);
        (&s).write_all(b"data").unwrap();
        // Room for a header only: the data does not fit.
        d.post(RX, &[(vec![0; HEADER_LEN], true)]);
        woken(&mut dev, &mut d);
        assert!(d.received().is_empty());
        assert_eq!(d.avail_event(RX), 2, "notify on the next buffer");
        d.post_rx(1, 4096, false);
        notify(&mut dev, &mut d, RX);
        let got = d.received();
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].1, b"data");
        // The small buffer takes the next packet that fits: the shutdown
        // when the host's end goes.
        drop(s);
        woken(&mut dev, &mut d);
        let used = d.used(RX);
        assert_eq!(used, vec![(1, HEADER_LEN as u32)], "the held buffer");
        let hdr = Header::decode(d.bytes(RX, 1, HEADER_LEN as u32)[..].try_into().unwrap());
        assert_eq!(hdr.op, Op::Shutdown);
    }

    /// On every queue, a device with work owed and no buffer left (rx: a
    /// packet; event: a reset), or done with the guest's packets (tx), has
    /// asked the driver to notify on its next buffer, with EVENT_IDX or
    /// without.
    #[test]
    fn out_of_buffers_the_driver_is_asked_to_notify() {
        for event_idx in [true, false] {
            let (mut dev, vsock, _) = device();
            let mut d = Driver::new(3, event_idx);
            let listener = vsock.listen(7000).unwrap();
            d.send(from_guest(Op::Request, 4000, 7000), &[], false);
            d.send(from_guest(Op::Request, 4001, 7000), &[], false);
            notify(&mut dev, &mut d, TX);
            assert_eq!(d.used(TX).len(), 2);
            assert!(d.would_notify(TX, event_idx), "tx, event_idx {event_idx}");
            // Two responses owed, one buffer.
            let _a = listener.accept(Some(Duration::from_secs(1))).unwrap();
            let _b = listener.accept(Some(Duration::from_secs(1))).unwrap();
            d.post_rx(1, 4096, false);
            notify(&mut dev, &mut d, RX);
            assert_eq!(d.received().len(), 1);
            assert!(d.would_notify(RX, event_idx), "rx, event_idx {event_idx}");
            d.post_rx(1, 4096, false);
            notify(&mut dev, &mut d, RX);
            assert_eq!(d.received().len(), 1, "the second on the next buffer");
            // A reset owed, no event buffer.
            vsock.reset_connections();
            woken(&mut dev, &mut d);
            assert!(d.used(EVENT).is_empty());
            assert!(d.would_notify(EVENT, event_idx), "event, {event_idx}");
        }
    }

    /// A device restored from a snapshot holds the rx buffers the saved one
    /// held, starts with a transport reset for the guest (so that it drops
    /// the connections it had, whose other ends are not in this machine),
    /// holds host connects back until the guest has taken it, and has a
    /// new generation for the agent's reseed.
    #[test]
    fn a_restored_device_resets_the_transport_and_keeps_its_buffers() {
        let (mut dev, vsock, mut d) = device();
        let listener = vsock.listen(7000).unwrap();
        d.send(from_guest(Op::Request, 4000, 7000), &[], false);
        notify(&mut dev, &mut d, TX);
        let s = listener.accept(Some(Duration::from_secs(1))).unwrap();
        d.post_rx(1, 4096, false);
        notify(&mut dev, &mut d, RX);
        assert_eq!(d.received()[0].0.op, Op::Response);
        (&s).write_all(b"data").unwrap();
        d.post(RX, &[(vec![0; HEADER_LEN], true)]);
        woken(&mut dev, &mut d);
        assert_eq!(dev.held.len(), 1);
        assert_eq!(vsock.generation(), 0, "a machine that booted");
        let state = dev.save().unwrap();

        // The new machine's device, on the same guest memory and queues.
        let (mut restored, vsock2) = VsockDevice::new(GUEST_CID).unwrap();
        restored.activate(0);
        restored.restore(&state).unwrap();
        assert_eq!(restored.held.len(), 1, "the held buffer is back");
        assert_ne!(vsock2.generation(), 0);
        let held = vsock2.connect(AGENT_PORT, Duration::from_millis(1)).unwrap_err();
        assert_eq!(held.kind(), io::ErrorKind::TimedOut);
        assert!(held.to_string().contains("transport reset"), "{held}");
        d.post(EVENT, &[(vec![0xff; 4], true)]);
        woken(&mut restored, &mut d);
        let used = d.used(EVENT);
        assert_eq!(used.len(), 1, "the transport reset event");
        let (head, _) = used[0];
        let event: u32 =
            d.mem.read_obj(GuestAddress(d.buffers[EVENT][usize::from(head)][0].0)).unwrap();
        assert_eq!(event, EVENT_TRANSPORT_RESET);
        // The guest takes it and reposts its buffer (notifying the event
        // queue): connections work.
        d.post(EVENT, &[(vec![0; 4], true)]);
        notify(&mut restored, &mut d, EVENT);
        let timed_out = vsock2.connect(AGENT_PORT, Duration::from_millis(1)).unwrap_err();
        assert_eq!(timed_out.kind(), io::ErrorKind::TimedOut, "past the reset");
        assert!(!timed_out.to_string().contains("reset"), "{timed_out}");

        // Two restores of one snapshot get two generations; another
        // version of the state is refused.
        let (mut again, vsock3) = VsockDevice::new(GUEST_CID).unwrap();
        again.activate(0);
        again.restore(&state).unwrap();
        assert_ne!(vsock3.generation(), vsock2.generation());
        let other = DeviceState { version: STATE_VERSION + 1, data: state.data.clone() };
        assert!(matches!(again.restore(&other), Err(Error::Snapshot(_))));
    }

    /// Connects from the host through `dev`, the guest answering: the host
    /// port the connection took.
    fn connect_through(dev: &mut VsockDevice, vsock: &Vsock, d: &mut Driver) -> u32 {
        let host = {
            let vsock = vsock.clone();
            std::thread::spawn(move || vsock.connect(AGENT_PORT, Duration::from_secs(10)))
        };
        let start = std::time::Instant::now();
        let request = loop {
            assert!(start.elapsed() < Duration::from_secs(5), "no request");
            woken(dev, d);
            if let Some((h, _)) = d.received().into_iter().find(|(h, _)| h.op == Op::Request) {
                break h;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        d.send(from_guest(Op::Response, AGENT_PORT, request.src_port), &[], false);
        notify(dev, d, TX);
        drop(host.join().unwrap().unwrap());
        request.src_port
    }

    /// A restored device's connects take host ports its guest has never
    /// seen, carrying on from the saved device's: a guest socket from before
    /// the snapshot may still be in Linux's connected table after the
    /// transport reset (reset, not yet released), and a request from its
    /// port is answered with a reset. Before (board 13177), every restored
    /// device started from the first port again, and a clone's first
    /// connect to its agent was refused about one restore in four.
    #[test]
    fn a_restored_device_takes_no_port_its_guest_saw_before() {
        let (mut dev, vsock, mut d) = device();
        d.post_rx(16, 4096, false);
        let used: Vec<u32> = (0..3).map(|_| connect_through(&mut dev, &vsock, &mut d)).collect();
        let state = dev.save().unwrap();
        let (mut restored, vsock2) = VsockDevice::new(GUEST_CID).unwrap();
        restored.activate(0);
        restored.restore(&state).unwrap();
        // The guest takes the reset.
        d.post(EVENT, &[(vec![0; 4], true)]);
        woken(&mut restored, &mut d);
        assert_eq!(d.used(EVENT).len(), 1);
        d.post(EVENT, &[(vec![0; 4], true)]);
        notify(&mut restored, &mut d, EVENT);
        let port = connect_through(&mut restored, &vsock2, &mut d);
        assert!(port > *used.iter().max().unwrap(), "port {port:#x} after {used:x?}");
        // A saved port that is not one a connect takes is refused.
        let mut e = Encoder::default();
        e.count(0);
        e.u32(7);
        let bad = DeviceState { version: STATE_VERSION, data: e.finish() };
        let (mut other, _) = VsockDevice::new(GUEST_CID).unwrap();
        assert!(matches!(other.restore(&bad), Err(Error::Snapshot(_))));
    }

    #[test]
    fn malformed_tx_packets_are_dropped() {
        let (mut dev, vsock, mut d) = device();
        let listener = vsock.listen(7000).unwrap();
        // Shorter than a header; longer than it says; claiming more than
        // a packet may hold.
        d.post(TX, &[(vec![0; HEADER_LEN - 1], false)]);
        let mut h = from_guest(Op::Request, 4000, 7000).encode().to_vec();
        h[24..28].copy_from_slice(&10u32.to_le_bytes());
        d.post(TX, &[(h.clone(), false)]);
        h[24..28].copy_from_slice(&((MAX_PAYLOAD + 1) as u32).to_le_bytes());
        h.resize(HEADER_LEN + MAX_PAYLOAD + 1, 0);
        d.post(TX, &[(h, false)]);
        // A writable descriptor is not part of the packet.
        d.post(TX, &[(vec![0; HEADER_LEN], true)]);
        notify(&mut dev, &mut d, TX);
        assert_eq!(d.used(TX).len(), 4, "every buffer goes back");
        d.post_rx(4, 4096, false);
        notify(&mut dev, &mut d, RX);
        assert!(d.received().is_empty());
        assert!(listener.accept(Some(Duration::from_millis(1))).is_err());
    }

    #[test]
    fn transport_reset_event_and_machine_gone() {
        let (mut dev, vsock, mut d) = device();
        let listener = vsock.listen(7000).unwrap();
        d.send(from_guest(Op::Request, 4000, 7000), &[], false);
        notify(&mut dev, &mut d, TX);
        let s = listener.accept(Some(Duration::from_secs(1))).unwrap();
        vsock.reset_connections();
        woken(&mut dev, &mut d);
        assert!(d.used(EVENT).is_empty(), "no event buffer yet");
        d.post(EVENT, &[(vec![0xff; 4], true)]);
        notify(&mut dev, &mut d, EVENT);
        assert_eq!(d.used(EVENT), vec![(0, 4)]);
        let event: u32 = d.mem.read_obj(GuestAddress(d.buffers[EVENT][0][0].0)).unwrap();
        assert_eq!(event, EVENT_TRANSPORT_RESET);
        let err = (&s).read(&mut [0; 4]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
        // No connection until the guest posts an event buffer again: a
        // connect waits for it.
        let held = vsock.connect(AGENT_PORT, Duration::from_millis(1)).unwrap_err();
        assert_eq!(held.kind(), io::ErrorKind::TimedOut);
        assert!(held.to_string().contains("transport reset"), "{held}");
        woken(&mut dev, &mut d);
        assert!(vsock.connect(AGENT_PORT, Duration::from_millis(1)).is_err());
        d.post(EVENT, &[(vec![0; 4], true)]);
        woken(&mut dev, &mut d);
        let timed_out = vsock.connect(AGENT_PORT, Duration::from_millis(1)).unwrap_err();
        assert_eq!(timed_out.kind(), io::ErrorKind::TimedOut, "past the reset now");
        assert!(!timed_out.to_string().contains("reset"), "{timed_out}");

        drop(dev);
        let err = vsock.connect(AGENT_PORT, Duration::from_secs(1)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotConnected);
        assert!(listener.accept(None).is_err());
    }

    #[test]
    fn connect_times_out_when_the_guest_does_not_answer() {
        let (_dev, vsock, _d) = device();
        let err = vsock.connect(AGENT_PORT, Duration::from_millis(20)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        let s = vsock.listen(1).unwrap();
        s.accept(Some(Duration::from_millis(1))).unwrap_err();
    }

    #[test]
    fn driver_reset_resets_connections() {
        let (mut dev, vsock, mut d) = device();
        let listener = vsock.listen(7000).unwrap();
        d.send(from_guest(Op::Request, 4000, 7000), &[], false);
        notify(&mut dev, &mut d, TX);
        let s = listener.accept(Some(Duration::from_secs(1))).unwrap();
        dev.reset();
        assert_eq!((&s).write(b"x").unwrap_err().kind(), io::ErrorKind::ConnectionReset);
        assert_eq!(
            vsock.connect(1, Duration::from_secs(1)).unwrap_err().kind(),
            io::ErrorKind::ConnectionRefused,
            "the driver is not ready"
        );
    }

    /// The guest took a transport reset and reposted its event buffer
    /// without notifying (a driver that does not, or one that reposted
    /// before the device could ask to be told): a host connect still gets
    /// through, the device looking once when it starts to wait. Before
    /// board 13131, it was refused at once, and under resets closer than the
    /// client's retry delay nothing ever connected.
    #[test]
    fn a_connect_sees_a_reset_the_guest_took_without_notifying() {
        let (mut dev, vsock, mut d) = device();
        d.post(EVENT, &[(vec![0; 4], true)]);
        d.post_rx(4, 4096, false);
        notify(&mut dev, &mut d, EVENT);
        vsock.reset_connections();
        woken(&mut dev, &mut d);
        assert_eq!(d.used(EVENT).len(), 1, "the event went out");
        // The guest takes it and reposts the buffer, telling nobody.
        d.post(EVENT, &[(vec![0; 4], true)]);
        let host = {
            let vsock = vsock.clone();
            std::thread::spawn(move || vsock.connect(AGENT_PORT, Duration::from_secs(10)))
        };
        // The device thread, woken when the host asks.
        let start = std::time::Instant::now();
        let request = loop {
            assert!(start.elapsed() < Duration::from_secs(5), "no request");
            if host.is_finished() {
                panic!("the connect ended: {:?}", host.join().unwrap().err());
            }
            std::thread::sleep(Duration::from_millis(1));
            woken(&mut dev, &mut d);
            if let Some((h, _)) = d.received().into_iter().find(|(h, _)| h.op == Op::Request) {
                break h;
            }
        };
        d.send(from_guest(Op::Response, AGENT_PORT, request.src_port), &[], false);
        notify(&mut dev, &mut d, TX);
        host.join().unwrap().unwrap();
    }

    /// After a transport reset (as after every restore) the device waits
    /// for the guest to repost the event buffer it used, which Linux does
    /// while keeping 8 posted: with EVENT_IDX that crosses no avail event
    /// index unless the device asks for it, so the guest notified nobody
    /// and a host connect learned of the end only by trying again. Now the
    /// device asks (with EVENT_IDX or without), the repost notifies it,
    /// and a connect waiting meanwhile goes through at once, never refused.
    /// Found measuring the first agent answer after a restore (board 13177).
    #[test]
    fn the_guests_repost_after_a_reset_notifies_the_device() {
        for event_idx in [false, true] {
            let (mut dev, vsock) = VsockDevice::new(GUEST_CID).unwrap();
            dev.activate(0);
            let mut d = Driver::new(3, event_idx);
            for _ in 0..8 {
                d.post(EVENT, &[(vec![0; 4], true)]);
            }
            d.post_rx(4, 4096, false);
            notify(&mut dev, &mut d, EVENT);
            vsock.reset_connections();
            woken(&mut dev, &mut d);
            assert_eq!(d.used(EVENT).len(), 1, "the event went out");
            assert!(
                d.would_notify(EVENT, event_idx),
                "the guest's repost notifies the device (EVENT_IDX {event_idx})"
            );
            let host = {
                let vsock = vsock.clone();
                std::thread::spawn(move || vsock.connect(AGENT_PORT, Duration::from_secs(10)))
            };
            // The connect waits; the look it asks for finds nothing new.
            std::thread::sleep(Duration::from_millis(30));
            woken(&mut dev, &mut d);
            std::thread::sleep(Duration::from_millis(30));
            woken(&mut dev, &mut d);
            assert!(!host.is_finished(), "a connect during a reset waits");
            assert!(d.received().is_empty(), "nothing goes out during it");
            // The guest takes the reset: it reposts, and notifies.
            d.post(EVENT, &[(vec![0; 4], true)]);
            notify(&mut dev, &mut d, EVENT);
            let start = std::time::Instant::now();
            let request = loop {
                assert!(start.elapsed() < Duration::from_secs(5), "no request");
                woken(&mut dev, &mut d);
                if let Some((h, _)) = d.received().into_iter().find(|(h, _)| h.op == Op::Request) {
                    break h;
                }
                std::thread::sleep(Duration::from_millis(1));
            };
            d.send(from_guest(Op::Response, AGENT_PORT, request.src_port), &[], false);
            notify(&mut dev, &mut d, TX);
            host.join().unwrap().unwrap();
        }
    }

    /// The guest stops (its machine's run ends with it powering off,
    /// resetting or triple-faulting): what waits on it fails at once, with
    /// data it sent before still readable, and so does what comes next;
    /// once its driver starts the device again (a guest booting again),
    /// connections work as before.
    #[test]
    fn a_stopped_guest_fails_everything_at_once_until_it_starts_again() {
        let (mut dev, vsock, mut d) = device();
        let listener = vsock.listen(7000).unwrap();
        d.send(from_guest(Op::Request, 4000, 7000), &[], false);
        d.send(from_guest(Op::Rw, 4000, 7000), b"last words", false);
        notify(&mut dev, &mut d, TX);
        let s = listener.accept(Some(Duration::from_secs(1))).unwrap();
        // A reader, a host connect and an accept, all waiting for the guest
        // with no timeout.
        let reader = std::thread::spawn(move || {
            let mut got = [0; 10];
            (&s).read_exact(&mut got).unwrap();
            assert_eq!(&got, b"last words");
            (&s).read(&mut [0; 1]).unwrap_err()
        });
        let connector = {
            let vsock = vsock.clone();
            std::thread::spawn(move || vsock.connect(AGENT_PORT, Duration::from_secs(600)))
        };
        let acceptor = std::thread::spawn(move || listener.accept(None).map(|_| ()));
        std::thread::sleep(Duration::from_millis(50));
        let start = std::time::Instant::now();
        vsock.guest_stopped();
        let read = reader.join().unwrap();
        let connect = connector.join().unwrap().unwrap_err();
        let accept = acceptor.join().unwrap().unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(5));
        for e in [&read, &connect, &accept] {
            assert_eq!(e.kind(), io::ErrorKind::ConnectionReset, "{e}");
            assert!(is_guest_stopped(e), "{e}");
            assert!(e.to_string().contains("powered off"), "{e}");
        }
        // New ones fail at once too; packets the guest left are dropped.
        let e = vsock.connect(AGENT_PORT, Duration::from_secs(600)).unwrap_err();
        assert!(is_guest_stopped(&e));
        let listener = vsock.listen(7001).unwrap();
        assert!(is_guest_stopped(&listener.accept(None).unwrap_err()));
        d.send(from_guest(Op::Request, 4001, 7001), &[], false);
        d.post_rx(4, 4096, false);
        notify(&mut dev, &mut d, TX);
        assert!(d.received().is_empty(), "nothing answers a stopped guest");
        // Machine::run again: the guest boots again, resetting the device
        // and starting it.
        dev.reset();
        let e = vsock.connect(AGENT_PORT, Duration::from_secs(1)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::ConnectionRefused, "booting");
        assert!(!is_guest_stopped(&e));
        dev.activate(0);
        d.send(from_guest(Op::Request, 4002, 7001), &[], false);
        notify(&mut dev, &mut d, TX);
        let s = listener.accept(Some(Duration::from_secs(1))).unwrap();
        assert_eq!(s.peer_port(), 4002);
        assert_eq!(d.received()[0].0.op, Op::Response);
        // And a stop after the machine is gone changes nothing.
        drop(dev);
        vsock.guest_stopped();
        let e = vsock.connect(AGENT_PORT, Duration::from_secs(1)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotConnected);
    }
}
