// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The device's connections: virtio-vsock's stream protocol (virtio 1.2,
//! section 5.10.6) between the guest and the host's in-process endpoints,
//! with no queues or threads in it, so that every rule can be tested alone.
//!
//! The device thread feeds it the guest's packets ([`State::guest_packet`])
//! and takes the packets for the guest ([`State::next_packet`]); the host's
//! handles ([`super::host`]) connect, listen, read and write. Whoever
//! changes something the other side waits for sets `wake` (the device
//! thread has packets to send) and the caller notifies the host's waiters.
//!
//! Flow control is virtio-vsock's credit: each side tells the other, in
//! every packet, how big its receive buffer is (`buf_alloc`) and how much it
//! has taken out of it (`fwd_cnt`); a sender never has more in flight than
//! that leaves room for. A guest that sends past the host's credit is
//! reset, as Linux does to a host that sends past its own.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io;

use super::packet::{
    HEADER_LEN, Header, MAX_PAYLOAD, Op, SHUTDOWN_BOTH, SHUTDOWN_RCV, SHUTDOWN_SEND, TYPE_STREAM,
};

/// The host's CID (VMADDR_CID_HOST).
pub(crate) const HOST_CID: u64 = 2;
/// Each connection's receive buffer on the host (guest-to-host bytes the
/// host has not read yet): the credit the guest gets.
pub(crate) const RX_CAPACITY: u32 = 256 << 10;
/// How many host-to-guest bytes a connection queues before host writes
/// wait for the guest's credit.
pub(crate) const TX_CAPACITY: usize = 256 << 10;
/// The most connections, in any state, a machine has at once.
pub(crate) const MAX_CONNECTIONS: usize = 1024;
/// The most connections waiting in a listener's backlog.
pub(crate) const MAX_BACKLOG: usize = 128;
/// The most resets for unknown connections waiting to go to the guest; past
/// it they are dropped (the guest's connect then times out instead).
const MAX_PENDING_RESETS: usize = 1024;
/// Host ports of host-initiated connections come from here up.
const EPHEMERAL_START: u32 = 0x8000_0000;
/// VMADDR_PORT_ANY: never a port; a host listener there takes the guest's connections to every
/// port no other listener holds ([`Vsock::listen_any`](super::Vsock::listen_any)).
pub(crate) const PORT_ANY: u32 = u32::MAX;

/// A connection's ports: the host's end, the guest's end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    host_port: u32,
    guest_port: u32,
}

/// Where a connection is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// The host sent a REQUEST and waits for the guest's RESPONSE.
    Connecting,
    Connected,
    /// Over: cleanly (reads end after the data received), or with an error.
    Closed(Option<io::ErrorKind>),
}

#[derive(Debug)]
struct Conn {
    key: Key,
    phase: Phase,
    /// The host's handle is gone: what the guest sends is dropped, and the
    /// connection ends once the guest has seen the host's shutdown.
    released: bool,

    // Guest to host.
    rx: VecDeque<u8>,
    /// Bytes the host has taken out of `rx`, ever (mod 2^32).
    fwd_cnt: u32,
    /// The `fwd_cnt` the guest last heard of.
    sent_fwd_cnt: u32,
    /// The guest will send no more (SHUTDOWN with SEND).
    peer_done_sending: bool,
    /// The host will read no more: what arrives is dropped.
    read_shut: bool,

    // Host to guest.
    tx: VecDeque<u8>,
    /// Bytes sent to the guest, ever (mod 2^32).
    tx_cnt: u32,
    peer_buf_alloc: u32,
    peer_fwd_cnt: u32,
    /// The guest will receive no more (SHUTDOWN with RCV).
    peer_done_receiving: bool,

    // Packets owed to the guest, sent as soon as it posts buffers.
    send_request: bool,
    send_response: bool,
    send_credit_update: bool,
    /// Set when the host is out of the guest's credit, cleared by any
    /// packet that brings more.
    credit_requested: bool,
    /// SHUTDOWN flags the host wants sent (once `tx` has gone out), and
    /// those it sent.
    shutdown_wanted: u32,
    shutdown_sent: u32,
}

impl Conn {
    fn new(key: Key, phase: Phase) -> Self {
        Self {
            key,
            phase,
            released: false,
            rx: VecDeque::new(),
            fwd_cnt: 0,
            sent_fwd_cnt: 0,
            peer_done_sending: false,
            read_shut: false,
            tx: VecDeque::new(),
            tx_cnt: 0,
            peer_buf_alloc: 0,
            peer_fwd_cnt: 0,
            peer_done_receiving: false,
            send_request: false,
            send_response: false,
            send_credit_update: false,
            credit_requested: false,
            shutdown_wanted: 0,
            shutdown_sent: 0,
        }
    }

    /// How many more bytes the guest has room for.
    fn credit(&self) -> u32 {
        let in_flight = self.tx_cnt.wrapping_sub(self.peer_fwd_cnt);
        self.peer_buf_alloc.saturating_sub(in_flight)
    }

    fn connected(&self) -> bool {
        self.phase == Phase::Connected
    }

    /// The next packet this connection owes the guest, if any, given
    /// `max_payload` bytes of room for data.
    fn next_op(&self, max_payload: usize) -> Option<Op> {
        if self.send_request {
            return Some(Op::Request);
        }
        if !self.connected() {
            return None;
        }
        if self.send_response {
            return Some(Op::Response);
        }
        let can_send = !self.peer_done_receiving && !self.tx.is_empty();
        if can_send && self.credit() > 0 && max_payload > 0 {
            return Some(Op::Rw);
        }
        if self.send_credit_update {
            return Some(Op::CreditUpdate);
        }
        if can_send && self.credit() == 0 && !self.credit_requested {
            return Some(Op::CreditRequest);
        }
        let flushed = self.tx.is_empty() || self.peer_done_receiving;
        if flushed && self.shutdown_wanted & !self.shutdown_sent != 0 {
            return Some(Op::Shutdown);
        }
        None
    }

    /// Takes in the guest's credit information from a packet.
    fn update_credit(&mut self, hdr: &Header) {
        let before = self.credit();
        self.peer_buf_alloc = hdr.buf_alloc;
        self.peer_fwd_cnt = hdr.fwd_cnt;
        if self.credit() > before {
            self.credit_requested = false;
        }
    }

    /// After the host took bytes out of `rx`: tell the guest when the room
    /// it knows of is getting short (as Linux does, below one packet's
    /// worth), so that a guest waiting for credit goes on.
    fn maybe_update_credit(&mut self) {
        let received = self.fwd_cnt.wrapping_add(self.rx.len() as u32);
        let unacknowledged = received.wrapping_sub(self.sent_fwd_cnt);
        let room_known = RX_CAPACITY.saturating_sub(unacknowledged);
        if self.fwd_cnt != self.sent_fwd_cnt && (room_known as usize) < MAX_PAYLOAD {
            self.send_credit_update = true;
        }
    }

    /// Drops what the guest sent that the host will never read, counting it
    /// as read so that the guest's credit is not lost.
    fn discard_rx(&mut self) {
        self.fwd_cnt = self.fwd_cnt.wrapping_add(self.rx.len() as u32);
        self.rx.clear();
        self.maybe_update_credit();
    }
}

/// The connections of one machine's vsock device.
#[derive(Debug)]
pub(crate) struct State {
    guest_cid: u64,
    /// The guest's driver has set DRIVER_OK (and not reset since).
    driver_ready: bool,
    /// The machine is gone: every operation fails.
    gone: bool,
    /// The guest stopped (powered off, reset, triple-faulted): every
    /// operation fails with a reset until its driver starts the device
    /// again (a guest that boots again).
    stopped: bool,
    /// By ID, so that a host handle never reaches a later connection that
    /// reuses its ports; ordered, for a fair round-robin.
    conns: BTreeMap<u64, Conn>,
    by_key: HashMap<Key, u64>,
    /// Listening host ports, each with its backlog of accepted connections.
    listeners: HashMap<u32, VecDeque<u64>>,
    /// Resets for connections the device does not have.
    resets: VecDeque<Header>,
    /// A transport reset event is owed to the guest.
    pub(crate) event_pending: bool,
    /// A transport reset the guest has not finished taking: from the reset
    /// until it has reposted every event buffer the device used (Linux
    /// resets its connections, then reposts the buffer), new connections
    /// either way are refused, because the guest's reset would still reset
    /// them (the host's wait for the end instead: `Vsock::connect`).
    pub(crate) resetting: bool,
    /// The event queue's available index at which the guest will have
    /// reposted every buffer the device used, up to the last event's: the
    /// reset is over once the index gets there.
    pub(crate) event_target: Option<u16>,
    /// The most event buffers the guest has had posted at once, unused:
    /// what it has posted when it has reposted every one the device used.
    event_room: u16,
    next_id: u64,
    /// The host port the next connect takes: saved in a snapshot (see
    /// [`next_port`](State::next_port)).
    next_port: u32,
    /// The connection the round-robin served last.
    cursor: u64,
    /// The device thread has packets to send (or an event): signal it.
    pub(crate) wake: bool,
    /// Which restore of a snapshot this machine is: 0 for a machine that
    /// booted, a fresh random number for each restore. The agent client
    /// compares it with what the guest's agent last reseeded for.
    pub(crate) generation: u64,
}

/// What a host operation that may have to wait got.
pub(crate) enum Poll<T> {
    Ready(io::Result<T>),
    Wait,
}

/// What a guest that stopped fails with: a reset, which
/// [`is_guest_stopped`] tells from others.
#[derive(Debug)]
pub struct GuestStopped;

impl std::fmt::Display for GuestStopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the guest stopped (it powered off, reset or triple-faulted)")
    }
}

impl std::error::Error for GuestStopped {}

pub(crate) fn guest_stopped() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionReset, GuestStopped)
}

/// Whether a vsock operation failed because the guest stopped (powered
/// off, reset or triple-faulted): a `ConnectionReset` that no connecting
/// again will cure until the guest runs again and its driver starts the
/// device.
pub fn is_guest_stopped(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<GuestStopped>())
}

pub(crate) fn error(kind: io::ErrorKind) -> io::Error {
    let message = match kind {
        io::ErrorKind::ConnectionRefused => "the guest refused the connection",
        io::ErrorKind::ConnectionReset => "the connection was reset",
        io::ErrorKind::BrokenPipe => "the connection is shut down for writing",
        io::ErrorKind::NotConnected => "the machine is gone",
        _ => return io::Error::from(kind),
    };
    io::Error::new(kind, message)
}

impl State {
    pub(crate) fn new(guest_cid: u64) -> Self {
        Self {
            guest_cid,
            driver_ready: false,
            gone: false,
            stopped: false,
            conns: BTreeMap::new(),
            by_key: HashMap::new(),
            listeners: HashMap::new(),
            resets: VecDeque::new(),
            event_pending: false,
            resetting: false,
            event_target: None,
            event_room: 0,
            next_id: 1,
            next_port: EPHEMERAL_START,
            cursor: 0,
            wake: false,
            generation: 0,
        }
    }

    /// The host port the next connect takes. A snapshot keeps it, so that
    /// a restored machine's connects take only ports its guest has never
    /// seen: a guest socket of a connection from before the snapshot stays
    /// in Linux's connected table after the transport reset (reset, not
    /// released) until its owner closes it, and a request from its port
    /// meanwhile is answered with a reset (found in board 13177: a clone's
    /// first connect, from the first port again, was refused at times).
    pub(crate) fn next_port(&self) -> u32 {
        self.next_port
    }

    /// A restored device's [`next_port`](State::next_port); false if `port`
    /// is not one a connect takes.
    pub(crate) fn set_next_port(&mut self, port: u32) -> bool {
        let ephemeral = (EPHEMERAL_START..PORT_ANY).contains(&port);
        if ephemeral {
            self.next_port = port;
        }
        ephemeral
    }

    pub(crate) fn guest_cid(&self) -> u64 {
        self.guest_cid
    }

    #[cfg(test)]
    pub(crate) fn connection_count(&self) -> usize {
        self.conns.len()
    }

    /// The header of a packet on connection `key`.
    fn header(&self, key: Key, op: Op) -> Header {
        Header {
            src_cid: HOST_CID,
            dst_cid: self.guest_cid,
            src_port: key.host_port,
            dst_port: key.guest_port,
            len: 0,
            socket_type: TYPE_STREAM,
            op,
            flags: 0,
            buf_alloc: RX_CAPACITY,
            fwd_cnt: 0,
        }
    }

    /// Queues a reset for a connection the device no longer has.
    fn queue_reset(&mut self, hdr: Header) {
        if self.resets.len() < MAX_PENDING_RESETS {
            self.resets.push_back(hdr);
            self.wake = true;
        }
    }

    /// Ends connection `id`; with `reset`, tells the guest so.
    fn close(&mut self, id: u64, err: Option<io::ErrorKind>, reset: bool) {
        let Some(conn) = self.conns.get_mut(&id) else {
            return;
        };
        conn.phase = Phase::Closed(err);
        conn.tx.clear();
        let key = conn.key;
        let released = conn.released;
        if self.by_key.get(&key) == Some(&id) {
            self.by_key.remove(&key);
        }
        if released {
            self.conns.remove(&id);
        }
        if reset {
            let hdr = self.header(key, Op::Rst);
            self.queue_reset(hdr);
        }
    }

    /// Resets every connection, as when the guest's driver resets the
    /// device, a transport reset event goes out, or the machine goes.
    fn reset_all(&mut self, err: io::ErrorKind) {
        let ids: Vec<u64> = self.conns.keys().copied().collect();
        for id in ids {
            self.close(id, Some(err), false);
        }
        for backlog in self.listeners.values_mut() {
            backlog.clear();
        }
        self.resets.clear();
    }

    // The device side.

    /// The driver set DRIVER_OK: a guest that stopped is running again.
    pub(crate) fn activate(&mut self) {
        self.driver_ready = true;
        self.stopped = false;
    }

    /// The driver reset the device: every connection is gone with it. A
    /// guest that stopped is running again (booting).
    pub(crate) fn driver_reset(&mut self) {
        self.driver_ready = false;
        self.stopped = false;
        self.event_pending = false;
        self.resetting = false;
        self.event_target = None;
        self.event_room = 0;
        self.reset_all(io::ErrorKind::ConnectionReset);
    }

    /// Resets every connection and tells the guest, with a transport reset
    /// event, to do the same (virtio 1.2, section 5.10.6.7), as after a
    /// restore. Listeners stay.
    pub(crate) fn transport_reset(&mut self) {
        self.reset_all(io::ErrorKind::ConnectionReset);
        if self.driver_ready {
            self.event_pending = true;
            self.resetting = true;
            self.event_target = None;
            self.wake = true;
        }
    }

    /// The event went out, in the event buffer before `next_avail` (the
    /// device's next available index once it took it). The guest has taken
    /// it once it has reposted that buffer and every one before: once the
    /// available index has come `event_room` past it.
    ///
    /// Counted from what the device used, not from the available index
    /// when the event went out: the guest may take the event and repost
    /// its buffer before the device reads that index, which would then
    /// never move past it (found reading this code while board 13131's
    /// reset storms starved connections).
    pub(crate) fn event_sent(&mut self, next_avail: u16) {
        self.event_pending = false;
        self.event_target = Some(next_avail.wrapping_add(self.event_room));
    }

    /// The event queue's available index is `avail` and the device's next
    /// one `next_avail`: notes how many buffers the guest keeps posted, and
    /// ends the reset once it has reposted every buffer the events used.
    pub(crate) fn event_queue_at(&mut self, avail: u16, next_avail: u16) {
        self.event_room = self.event_room.max(avail.wrapping_sub(next_avail));
        let reached = |target: u16| avail.wrapping_sub(target) as i16 >= 0;
        if self.resetting && self.event_target.is_some_and(reached) {
            self.resetting = false;
            self.event_target = None;
        }
    }

    /// The guest stopped: its run ended with it powering off, resetting or
    /// triple-faulting. Every connection is reset, and every operation
    /// fails with [`GuestStopped`] until the guest's driver resets or
    /// starts the device again. What it left in the queues is dropped.
    pub(crate) fn guest_stopped(&mut self) {
        if self.gone {
            return;
        }
        self.stopped = true;
        self.event_pending = false;
        self.resetting = false;
        self.event_target = None;
        self.reset_all(io::ErrorKind::ConnectionReset);
    }

    /// Fails host operations while the guest is stopped or gone.
    fn check_running(&self) -> io::Result<()> {
        if self.gone {
            Err(error(io::ErrorKind::NotConnected))
        } else if self.stopped {
            Err(guest_stopped())
        } else {
            Ok(())
        }
    }

    /// The machine is gone.
    pub(crate) fn machine_gone(&mut self) {
        self.gone = true;
        self.driver_ready = false;
        self.reset_all(io::ErrorKind::NotConnected);
    }

    /// A packet the guest sent on the tx queue. Packets not from the guest's
    /// CID to the host's are dropped.
    pub(crate) fn guest_packet(&mut self, hdr: &Header, payload: &[u8]) {
        if hdr.src_cid != self.guest_cid || hdr.dst_cid != HOST_CID || self.gone || self.stopped {
            return;
        }
        let reply_reset = |state: &mut Self| {
            if hdr.op != Op::Rst {
                state.queue_reset(Header {
                    src_cid: HOST_CID,
                    dst_cid: hdr.src_cid,
                    src_port: hdr.dst_port,
                    dst_port: hdr.src_port,
                    len: 0,
                    socket_type: hdr.socket_type,
                    op: Op::Rst,
                    flags: 0,
                    buf_alloc: 0,
                    fwd_cnt: 0,
                });
            }
        };
        if hdr.socket_type != TYPE_STREAM {
            return reply_reset(self);
        }
        let key = Key { host_port: hdr.dst_port, guest_port: hdr.src_port };
        let Some(&id) = self.by_key.get(&key) else {
            if hdr.op == Op::Request && self.accept_request(key, hdr) {
                return;
            }
            return reply_reset(self);
        };
        let conn = self.conns.get_mut(&id).expect("by_key names live connections");
        conn.update_credit(hdr);
        match (conn.phase, hdr.op) {
            (Phase::Connecting, Op::Response) => conn.phase = Phase::Connected,
            (Phase::Connecting, Op::Rst) => {
                self.close(id, Some(io::ErrorKind::ConnectionRefused), false)
            }
            (Phase::Connected, Op::Rw) => {
                if conn.rx.len() + payload.len() > RX_CAPACITY as usize {
                    // Past the credit the host gave it.
                    self.close(id, Some(io::ErrorKind::ConnectionReset), true);
                } else if conn.released || conn.read_shut {
                    conn.fwd_cnt = conn.fwd_cnt.wrapping_add(payload.len() as u32);
                    conn.maybe_update_credit();
                } else {
                    conn.rx.extend(payload);
                }
            }
            (Phase::Connected, Op::CreditUpdate) => {}
            (Phase::Connected, Op::CreditRequest) => conn.send_credit_update = true,
            (Phase::Connected, Op::Shutdown) => {
                conn.peer_done_receiving |= hdr.flags & SHUTDOWN_RCV != 0;
                conn.peer_done_sending |= hdr.flags & SHUTDOWN_SEND != 0;
                if conn.peer_done_receiving {
                    conn.tx.clear();
                }
                // Both ways shut, and the host's handle gone too: the
                // connection is over, which a reset tells the guest.
                if conn.peer_done_receiving && conn.peer_done_sending && conn.released {
                    self.close(id, None, true);
                }
            }
            (Phase::Connected, Op::Rst) => {
                // After the guest's SHUTDOWN of its sending, a reset only
                // completes the close: what it sent stays readable.
                let err = (!conn.peer_done_sending).then_some(io::ErrorKind::ConnectionReset);
                self.close(id, err, false);
            }
            // Anything else (a REQUEST for a live connection, a RESPONSE
            // when connected, an unknown op) breaks the protocol.
            _ => self.close(id, Some(io::ErrorKind::ConnectionReset), true),
        }
        if self.conns.get(&id).is_some_and(|c| c.next_op(1).is_some()) {
            self.wake = true;
        }
    }

    /// A guest's REQUEST to host port `key.host_port`: accepted if a
    /// listener there has room in its backlog.
    fn accept_request(&mut self, key: Key, hdr: &Header) -> bool {
        if self.conns.len() >= MAX_CONNECTIONS || self.resetting {
            return false;
        }
        // The port's own listener, or the listener of every port none holds.
        let port = match self.listeners.contains_key(&key.host_port) {
            true => key.host_port,
            false => PORT_ANY,
        };
        let Some(backlog) = self.listeners.get_mut(&port) else {
            return false;
        };
        if backlog.len() >= MAX_BACKLOG {
            return false;
        }
        let id = self.next_id;
        self.next_id += 1;
        backlog.push_back(id);
        let mut conn = Conn::new(key, Phase::Connected);
        conn.update_credit(hdr);
        conn.send_response = true;
        self.conns.insert(id, conn);
        self.by_key.insert(key, id);
        self.wake = true;
        true
    }

    /// Whether [`next_packet`](Self::next_packet) has something to send.
    pub(crate) fn has_packet(&self) -> bool {
        !self.resets.is_empty() || self.conns.values().any(|c| c.next_op(1).is_some())
    }

    /// The next packet for the guest, with at most `max_payload` bytes of
    /// data: resets first, then a packet from each connection in turn.
    pub(crate) fn next_packet(&mut self, max_payload: usize) -> Option<(Header, Vec<u8>)> {
        if let Some(hdr) = self.resets.pop_front() {
            return Some((hdr, Vec::new()));
        }
        let max_payload = max_payload.min(MAX_PAYLOAD);
        // Round-robin: the connections after the cursor, then the rest.
        let id = self
            .conns
            .range(self.cursor + 1..)
            .chain(self.conns.range(..=self.cursor))
            .find(|(_, c)| c.next_op(max_payload).is_some())
            .map(|(&id, _)| id)?;
        self.cursor = id;
        let mut hdr = self.header(self.conns[&id].key, Op::Rw);
        let conn = self.conns.get_mut(&id).unwrap();
        let op = conn.next_op(max_payload).unwrap();
        hdr.op = op;
        let mut payload = Vec::new();
        match op {
            Op::Request => conn.send_request = false,
            Op::Response => conn.send_response = false,
            Op::CreditUpdate => {}
            Op::CreditRequest => conn.credit_requested = true,
            Op::Rw => {
                let n = conn.tx.len().min(conn.credit() as usize).min(max_payload);
                payload.extend(conn.tx.drain(..n));
                conn.tx_cnt = conn.tx_cnt.wrapping_add(n as u32);
                hdr.len = n as u32;
            }
            Op::Shutdown => {
                conn.shutdown_sent = conn.shutdown_wanted;
                hdr.flags = conn.shutdown_wanted;
            }
            Op::Rst | Op::Unknown(_) => unreachable!("next_op never gives {op:?}"),
        }
        // Every packet carries the host's credit.
        hdr.fwd_cnt = conn.fwd_cnt;
        conn.sent_fwd_cnt = conn.fwd_cnt;
        conn.send_credit_update = false;
        Some((hdr, payload))
    }

    // The host side.

    fn conn(&mut self, id: u64) -> io::Result<&mut Conn> {
        self.conns.get_mut(&id).ok_or_else(|| error(io::ErrorKind::NotConnected))
    }

    /// Starts a connection to guest port `port`; the host then waits for
    /// [`phase`](Self::phase) to leave `Connecting`.
    pub(crate) fn connect(&mut self, port: u32) -> io::Result<u64> {
        self.check_running()?;
        if !self.driver_ready {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "the guest's vsock driver is not ready",
            ));
        }
        if self.resetting {
            // Have the device thread look whether the guest is done.
            self.wake = true;
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "the guest is still taking a transport reset",
            ));
        }
        if self.conns.len() >= MAX_CONNECTIONS {
            return Err(io::Error::other("too many vsock connections"));
        }
        let key = loop {
            let host_port = self.next_port;
            self.next_port = match self.next_port.wrapping_add(1) {
                PORT_ANY | 0 => EPHEMERAL_START,
                p => p,
            };
            let key = Key { host_port, guest_port: port };
            if !self.listeners.contains_key(&host_port) && !self.by_key.contains_key(&key) {
                break key;
            }
        };
        let id = self.next_id;
        self.next_id += 1;
        let mut conn = Conn::new(key, Phase::Connecting);
        conn.send_request = true;
        self.conns.insert(id, conn);
        self.by_key.insert(key, id);
        self.wake = true;
        Ok(id)
    }

    /// Why a connect, read or write of a connection in `phase` failed with
    /// `kind`: a guest that stopped says so.
    pub(crate) fn failed(&self, kind: io::ErrorKind) -> io::Error {
        match self.check_running() {
            Err(e) => e,
            Ok(()) => error(kind),
        }
    }

    pub(crate) fn phase(&self, id: u64) -> Phase {
        self.conns.get(&id).map_or(Phase::Closed(Some(io::ErrorKind::NotConnected)), |c| c.phase)
    }

    /// The host and guest ports of connection `id`.
    pub(crate) fn ports(&self, id: u64) -> Option<(u32, u32)> {
        self.conns.get(&id).map(|c| (c.key.host_port, c.key.guest_port))
    }

    /// The host gave up waiting for the guest to answer a connect.
    pub(crate) fn abandon(&mut self, id: u64) {
        let reset = self.phase(id) == Phase::Connecting;
        self.close(id, Some(io::ErrorKind::TimedOut), reset);
    }

    /// Reads what the guest sent; once the connection is over, what it
    /// sent before stays readable, then reads end or fail.
    pub(crate) fn read(&mut self, id: u64, buf: &mut [u8]) -> Poll<usize> {
        let stopped = self.stopped;
        let conn = match self.conn(id) {
            Ok(c) => c,
            Err(_) if stopped => return Poll::Ready(Err(guest_stopped())),
            Err(e) => return Poll::Ready(Err(e)),
        };
        if conn.read_shut {
            return Poll::Ready(Ok(0));
        }
        if !conn.rx.is_empty() {
            let n = buf.len().min(conn.rx.len());
            let (front, back) = conn.rx.as_slices();
            let first = front.len().min(n);
            buf[..first].copy_from_slice(&front[..first]);
            buf[first..n].copy_from_slice(&back[..n - first]);
            conn.rx.drain(..n);
            conn.fwd_cnt = conn.fwd_cnt.wrapping_add(n as u32);
            conn.maybe_update_credit();
            if conn.send_credit_update {
                self.wake = true;
            }
            return Poll::Ready(Ok(n));
        }
        match conn.phase {
            Phase::Connected if conn.peer_done_sending => Poll::Ready(Ok(0)),
            Phase::Connecting | Phase::Connected => Poll::Wait,
            Phase::Closed(None) => Poll::Ready(Ok(0)),
            Phase::Closed(Some(_)) if stopped => Poll::Ready(Err(guest_stopped())),
            Phase::Closed(Some(kind)) => Poll::Ready(Err(error(kind))),
        }
    }

    pub(crate) fn write(&mut self, id: u64, buf: &[u8]) -> Poll<usize> {
        if self.stopped {
            return Poll::Ready(Err(guest_stopped()));
        }
        let conn = match self.conn(id) {
            Ok(c) => c,
            Err(e) => return Poll::Ready(Err(e)),
        };
        match conn.phase {
            Phase::Closed(Some(kind)) => return Poll::Ready(Err(error(kind))),
            Phase::Closed(None) => return Poll::Ready(Err(error(io::ErrorKind::BrokenPipe))),
            Phase::Connecting => return Poll::Wait,
            Phase::Connected => {}
        }
        if conn.peer_done_receiving || conn.shutdown_wanted & SHUTDOWN_SEND != 0 {
            return Poll::Ready(Err(error(io::ErrorKind::BrokenPipe)));
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let n = buf.len().min(TX_CAPACITY - conn.tx.len());
        if n == 0 {
            return Poll::Wait;
        }
        conn.tx.extend(&buf[..n]);
        self.wake = true;
        Poll::Ready(Ok(n))
    }

    /// Ready once everything written has gone to the guest.
    pub(crate) fn flush(&mut self, id: u64) -> Poll<()> {
        if self.stopped {
            return Poll::Ready(Err(guest_stopped()));
        }
        match self.conn(id) {
            Ok(c) if c.connected() && !c.tx.is_empty() => Poll::Wait,
            Ok(_) => Poll::Ready(Ok(())),
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    /// The host shuts down its reading (`SHUTDOWN_RCV`), writing
    /// (`SHUTDOWN_SEND`) or both.
    pub(crate) fn shutdown(&mut self, id: u64, how: u32) {
        let Ok(conn) = self.conn(id) else { return };
        if !conn.connected() {
            return;
        }
        if how & SHUTDOWN_RCV != 0 {
            conn.read_shut = true;
            conn.discard_rx();
        }
        conn.shutdown_wanted |= how & SHUTDOWN_BOTH;
        if conn.next_op(1).is_some() {
            self.wake = true;
        }
    }

    /// The host's handle of connection `id` is gone.
    pub(crate) fn release(&mut self, id: u64) {
        let Some(conn) = self.conns.get_mut(&id) else {
            return;
        };
        conn.released = true;
        match conn.phase {
            Phase::Closed(_) => {
                self.conns.remove(&id);
            }
            Phase::Connecting => self.close(id, Some(io::ErrorKind::ConnectionAborted), true),
            Phase::Connected if conn.peer_done_receiving && conn.peer_done_sending => {
                self.close(id, None, true)
            }
            Phase::Connected => {
                conn.discard_rx();
                conn.shutdown_wanted = SHUTDOWN_BOTH;
                if conn.next_op(1).is_some() {
                    self.wake = true;
                }
            }
        }
    }

    pub(crate) fn listen(&mut self, port: u32) -> io::Result<()> {
        if self.gone {
            return Err(error(io::ErrorKind::NotConnected));
        }
        // A listener may be set up while the guest is stopped, for when it
        // runs again. PORT_ANY's takes the connections to every port no other holds.
        if self.listeners.contains_key(&port) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("host vsock port {port} is taken"),
            ));
        }
        self.listeners.insert(port, VecDeque::new());
        Ok(())
    }

    /// The next connection a guest made to listening port `port`.
    pub(crate) fn accept(&mut self, port: u32) -> Poll<u64> {
        if let Err(e) = self.check_running() {
            return Poll::Ready(Err(e));
        }
        match self.listeners.get_mut(&port).and_then(|b| b.pop_front()) {
            Some(id) => Poll::Ready(Ok(id)),
            None => Poll::Wait,
        }
    }

    /// The listener on `port` is gone: connections nobody accepted are
    /// reset.
    pub(crate) fn unlisten(&mut self, port: u32) {
        for id in self.listeners.remove(&port).unwrap_or_default() {
            if let Some(conn) = self.conns.get_mut(&id) {
                conn.released = true;
            }
            self.close(id, Some(io::ErrorKind::ConnectionReset), true);
        }
    }
}

/// The payload room a buffer of `capacity` bytes leaves after the header.
pub(crate) fn payload_room(capacity: usize) -> Option<usize> {
    capacity.checked_sub(HEADER_LEN)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUEST: u64 = 3;
    const AGENT_PORT: u32 = 1024;

    fn ready() -> State {
        let mut s = State::new(GUEST);
        s.activate();
        s
    }

    /// A packet from the guest.
    fn from_guest(op: Op, guest_port: u32, host_port: u32) -> Header {
        Header {
            src_cid: GUEST,
            dst_cid: HOST_CID,
            src_port: guest_port,
            dst_port: host_port,
            len: 0,
            socket_type: TYPE_STREAM,
            op,
            flags: 0,
            buf_alloc: 64 << 10,
            fwd_cnt: 0,
        }
    }

    /// Every packet the state has for the guest now, with room for
    /// `room` bytes of data each.
    fn drain(s: &mut State, room: usize) -> Vec<(Header, Vec<u8>)> {
        let mut out = Vec::new();
        while s.has_packet() {
            match s.next_packet(room) {
                Some(p) => out.push(p),
                None => break,
            }
        }
        assert!(s.next_packet(room).is_none());
        out
    }

    fn ops(packets: &[(Header, Vec<u8>)]) -> Vec<Op> {
        packets.iter().map(|(h, _)| h.op).collect()
    }

    /// Connects host to the guest's AGENT_PORT: (id, host port).
    fn connected(s: &mut State) -> (u64, u32) {
        let id = s.connect(AGENT_PORT).unwrap();
        let out = drain(s, 4096);
        assert_eq!(ops(&out), vec![Op::Request]);
        let req = out[0].0;
        assert_eq!((req.src_cid, req.dst_cid), (HOST_CID, GUEST));
        assert_eq!(req.dst_port, AGENT_PORT);
        assert_eq!(req.buf_alloc, RX_CAPACITY);
        assert!(req.src_port >= EPHEMERAL_START);
        assert_eq!(s.phase(id), Phase::Connecting);
        s.guest_packet(&from_guest(Op::Response, AGENT_PORT, req.src_port), &[]);
        assert_eq!(s.phase(id), Phase::Connected);
        (id, req.src_port)
    }

    /// The guest, keeping one event buffer posted, takes the reset event.
    fn take_reset(s: &mut State) {
        s.event_queue_at(1, 0);
        s.event_sent(1);
        s.event_queue_at(2, 1);
        assert!(!s.resetting);
    }

    fn read_all(s: &mut State, id: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0; 1000];
        while let Poll::Ready(Ok(n @ 1..)) = s.read(id, &mut buf) {
            out.extend_from_slice(&buf[..n]);
        }
        out
    }

    #[test]
    fn connect_before_the_driver_is_ready_is_refused() {
        let mut s = State::new(GUEST);
        let err = s.connect(AGENT_PORT).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    }

    #[test]
    fn a_reset_answering_a_request_is_a_refusal() {
        let mut s = ready();
        let id = s.connect(AGENT_PORT).unwrap();
        let req = drain(&mut s, 0)[0].0;
        s.guest_packet(&from_guest(Op::Rst, AGENT_PORT, req.src_port), &[]);
        assert_eq!(s.phase(id), Phase::Closed(Some(io::ErrorKind::ConnectionRefused)));
        s.release(id);
        assert_eq!(s.connection_count(), 0);
        assert!(drain(&mut s, 0).is_empty(), "no reset answers a reset");
    }

    #[test]
    fn data_both_ways_within_credit() {
        let mut s = ready();
        let (id, port) = connected(&mut s);
        // The guest has 64 KiB of room: the host's 100 KiB goes out in
        // packets of at most the buffer's room, then waits for credit.
        let data: Vec<u8> = (0..100 << 10).map(|i| i as u8).collect();
        let Poll::Ready(Ok(n)) = s.write(id, &data) else { panic!() };
        assert_eq!(n, data.len());
        let out = drain(&mut s, 4000);
        let rw: Vec<_> = out.iter().filter(|(h, _)| h.op == Op::Rw).collect();
        let sent: usize = rw.iter().map(|(_, p)| p.len()).sum();
        assert_eq!(sent, 64 << 10, "exactly the guest's credit");
        assert!(rw.iter().all(|(h, p)| p.len() <= 4000 && h.len as usize == p.len()));
        assert_eq!(out.last().unwrap().0.op, Op::CreditRequest, "out of credit");
        assert_eq!(ops(&drain(&mut s, 4000)), vec![], "asked once");
        // The guest read it all: the rest goes.
        let mut update = from_guest(Op::CreditUpdate, AGENT_PORT, port);
        update.fwd_cnt = 64 << 10;
        s.guest_packet(&update, &[]);
        let rest: Vec<u8> = drain(&mut s, 64 << 10).into_iter().flat_map(|(_, p)| p).collect();
        assert_eq!(rest, &data[64 << 10..]);

        // Guest to host.
        let mut rw = from_guest(Op::Rw, AGENT_PORT, port);
        rw.len = 5;
        s.guest_packet(&rw, b"hello");
        assert_eq!(read_all(&mut s, id), b"hello");
        assert!(matches!(s.read(id, &mut [0; 8]), Poll::Wait));
    }

    #[test]
    fn the_guest_gets_credit_back_as_the_host_reads() {
        let mut s = ready();
        let (id, port) = connected(&mut s);
        // The guest fills the host's buffer.
        let chunk = vec![7u8; MAX_PAYLOAD];
        for _ in 0..RX_CAPACITY as usize / MAX_PAYLOAD {
            s.guest_packet(&from_guest(Op::Rw, AGENT_PORT, port), &chunk);
        }
        assert!(drain(&mut s, 4096).is_empty(), "nothing owed yet");
        // One byte more is past its credit: reset.
        let mut over = State::new(GUEST);
        over.activate();
        let (oid, oport) = connected(&mut over);
        for _ in 0..RX_CAPACITY as usize / MAX_PAYLOAD {
            over.guest_packet(&from_guest(Op::Rw, AGENT_PORT, oport), &chunk);
        }
        over.guest_packet(&from_guest(Op::Rw, AGENT_PORT, oport), b"x");
        assert_eq!(ops(&drain(&mut over, 0)), vec![Op::Rst]);
        assert_eq!(over.phase(oid), Phase::Closed(Some(io::ErrorKind::ConnectionReset)));

        // A small read leaves plenty the guest knows of: no update yet...
        let mut buf = vec![0; 1000];
        assert!(matches!(s.read(id, &mut buf), Poll::Ready(Ok(1000))));
        let out = drain(&mut s, 4096);
        assert_eq!(ops(&out), vec![Op::CreditUpdate], "guest had none left");
        assert_eq!(out[0].0.fwd_cnt, 1000);
        // ...and reading on with the guest still short of room updates it.
        let mut big = vec![0; 100 << 10];
        assert!(matches!(s.read(id, &mut big), Poll::Ready(Ok(_))));
        let out = drain(&mut s, 4096);
        assert_eq!(ops(&out), vec![Op::CreditUpdate]);
        assert_eq!(out[0].0.fwd_cnt, 1000 + (100 << 10));
        // With lots of room known, reads send nothing.
        assert!(matches!(s.read(id, &mut buf), Poll::Ready(Ok(1000))));
        assert!(drain(&mut s, 4096).is_empty());
        // The guest asks: it is told.
        s.guest_packet(&from_guest(Op::CreditRequest, AGENT_PORT, port), &[]);
        let out = drain(&mut s, 4096);
        assert_eq!(ops(&out), vec![Op::CreditUpdate]);
        assert_eq!(out[0].0.fwd_cnt, 2000 + (100 << 10));
    }

    #[test]
    fn guest_shutdown_then_reset_is_a_clean_end() {
        let mut s = ready();
        let (id, port) = connected(&mut s);
        s.guest_packet(&from_guest(Op::Rw, AGENT_PORT, port), b"bye");
        let mut shut = from_guest(Op::Shutdown, AGENT_PORT, port);
        shut.flags = SHUTDOWN_BOTH;
        s.guest_packet(&shut, &[]);
        assert!(matches!(
            s.write(id, b"x"),
            Poll::Ready(Err(e)) if e.kind() == io::ErrorKind::BrokenPipe
        ));
        // Linux resets a fully shut connection after a while: the data
        // stays readable, then reads end.
        s.guest_packet(&from_guest(Op::Rst, AGENT_PORT, port), &[]);
        assert_eq!(read_all(&mut s, id), b"bye");
        assert!(matches!(s.read(id, &mut [0; 4]), Poll::Ready(Ok(0))));
        s.release(id);
        assert_eq!(s.connection_count(), 0);
    }

    #[test]
    fn a_reset_without_shutdown_is_an_error_after_the_data() {
        let mut s = ready();
        let (id, port) = connected(&mut s);
        s.guest_packet(&from_guest(Op::Rw, AGENT_PORT, port), b"partial");
        s.guest_packet(&from_guest(Op::Rst, AGENT_PORT, port), &[]);
        assert_eq!(read_all(&mut s, id), b"partial");
        assert!(matches!(
            s.read(id, &mut [0; 4]),
            Poll::Ready(Err(e)) if e.kind() == io::ErrorKind::ConnectionReset
        ));
    }

    #[test]
    fn host_close_flushes_then_shuts_down_and_ends_on_the_guests_reset() {
        let mut s = ready();
        let (id, port) = connected(&mut s);
        let mut credit = from_guest(Op::CreditUpdate, AGENT_PORT, port);
        credit.buf_alloc = 4;
        s.guest_packet(&credit, &[]);
        assert!(matches!(s.write(id, b"abcdef"), Poll::Ready(Ok(6))));
        s.release(id);
        let out = drain(&mut s, 4096);
        assert_eq!(ops(&out), vec![Op::Rw, Op::CreditRequest]);
        assert_eq!(out[0].1, b"abcd");
        // Credit for the rest: it goes, then the shutdown.
        credit.fwd_cnt = 4;
        s.guest_packet(&credit, &[]);
        let out = drain(&mut s, 4096);
        assert_eq!(ops(&out), vec![Op::Rw, Op::Shutdown]);
        assert_eq!(out[0].1, b"ef");
        assert_eq!(out[1].0.flags, SHUTDOWN_BOTH);
        // Data the guest sends now is dropped, but counted as read.
        s.guest_packet(&from_guest(Op::Rw, AGENT_PORT, port), &[1; 100]);
        assert_eq!(s.connection_count(), 1);
        s.guest_packet(&from_guest(Op::Rst, AGENT_PORT, port), &[]);
        assert_eq!(s.connection_count(), 0);
        assert!(drain(&mut s, 4096).is_empty());
    }

    #[test]
    fn host_shutdown_of_writing_waits_for_the_data() {
        let mut s = ready();
        let (id, _) = connected(&mut s);
        assert!(matches!(s.write(id, b"data"), Poll::Ready(Ok(4))));
        s.shutdown(id, SHUTDOWN_SEND);
        let out = drain(&mut s, 4096);
        assert_eq!(ops(&out), vec![Op::Rw, Op::Shutdown]);
        assert_eq!(out[1].0.flags, SHUTDOWN_SEND);
        assert!(matches!(
            s.write(id, b"more"),
            Poll::Ready(Err(e)) if e.kind() == io::ErrorKind::BrokenPipe
        ));
    }

    #[test]
    fn packets_for_unknown_connections_are_reset_but_resets_are_not() {
        let mut s = ready();
        s.guest_packet(&from_guest(Op::Rw, 5000, 6000), b"stray");
        s.guest_packet(&from_guest(Op::Rst, 5001, 6000), &[]);
        s.guest_packet(&from_guest(Op::Request, 5002, 6000), &[]);
        let out = drain(&mut s, 4096);
        assert_eq!(ops(&out), vec![Op::Rst, Op::Rst]);
        assert_eq!((out[0].0.src_port, out[0].0.dst_port), (6000, 5000));
        assert_eq!(out[1].0.dst_port, 5002, "nobody listens on 6000");
        // Wrong CIDs and other socket types.
        let mut wrong = from_guest(Op::Request, 5003, 6000);
        wrong.src_cid = 4;
        s.guest_packet(&wrong, &[]);
        assert!(drain(&mut s, 4096).is_empty(), "spoofed CID: dropped");
        let mut seqpacket = from_guest(Op::Request, 5004, 6000);
        seqpacket.socket_type = 2;
        s.guest_packet(&seqpacket, &[]);
        let out = drain(&mut s, 4096);
        assert_eq!(ops(&out), vec![Op::Rst]);
        assert_eq!(out[0].0.socket_type, 2);
    }

    #[test]
    fn guest_connections_to_a_listener() {
        let mut s = ready();
        s.listen(7000).unwrap();
        assert_eq!(s.listen(7000).unwrap_err().kind(), io::ErrorKind::AddrInUse);
        assert!(matches!(s.accept(7000), Poll::Wait));
        s.guest_packet(&from_guest(Op::Request, 40000, 7000), &[]);
        let out = drain(&mut s, 4096);
        assert_eq!(ops(&out), vec![Op::Response]);
        assert_eq!((out[0].0.src_port, out[0].0.dst_port), (7000, 40000));
        let Poll::Ready(Ok(id)) = s.accept(7000) else { panic!() };
        assert_eq!(s.ports(id), Some((7000, 40000)));
        s.guest_packet(&from_guest(Op::Rw, 40000, 7000), b"hi host");
        assert_eq!(read_all(&mut s, id), b"hi host");

        // The backlog is bounded.
        for port in 0..MAX_BACKLOG as u32 + 1 {
            s.guest_packet(&from_guest(Op::Request, 50000 + port, 7000), &[]);
        }
        let out = drain(&mut s, 4096);
        assert_eq!(out.iter().filter(|(h, _)| h.op == Op::Response).count(), MAX_BACKLOG);
        assert_eq!(out[0].0.op, Op::Rst, "resets go first");
        assert_eq!(out[0].0.dst_port, 50000 + MAX_BACKLOG as u32);
        // Closing the listener resets what it never accepted.
        s.unlisten(7000);
        let out = drain(&mut s, 4096);
        assert_eq!(out.len(), MAX_BACKLOG);
        assert!(out.iter().all(|(h, _)| h.op == Op::Rst));
        assert_eq!(s.connection_count(), 1, "the accepted one stays");
    }

    #[test]
    fn transport_reset_resets_everything_but_listeners() {
        let mut s = ready();
        let (id, port) = connected(&mut s);
        s.listen(7000).unwrap();
        s.guest_packet(&from_guest(Op::Rw, AGENT_PORT, port), b"x");
        s.transport_reset();
        assert!(s.event_pending);
        assert_eq!(read_all(&mut s, id), b"x");
        assert!(matches!(
            s.read(id, &mut [0; 1]),
            Poll::Ready(Err(e)) if e.kind() == io::ErrorKind::ConnectionReset
        ));
        assert!(drain(&mut s, 4096).is_empty(), "the event says it all");
        // Once the guest has taken the event, the ports are free again and
        // the listener still listens.
        take_reset(&mut s);
        s.guest_packet(&from_guest(Op::Request, AGENT_PORT, 7000), &[]);
        assert_eq!(ops(&drain(&mut s, 4096)), vec![Op::Response]);
    }

    #[test]
    fn no_connections_until_the_guest_has_taken_a_reset() {
        let mut s = ready();
        s.listen(7000).unwrap();
        s.transport_reset();
        assert!(s.resetting);
        let refused = s.connect(AGENT_PORT).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::ConnectionRefused);
        s.guest_packet(&from_guest(Op::Request, 4000, 7000), &[]);
        assert_eq!(ops(&drain(&mut s, 4096)), vec![Op::Rst]);
        // The guest keeps 8 event buffers posted (as Linux does); the event
        // went out in the first.
        s.event_queue_at(8, 0);
        s.event_sent(1);
        s.event_queue_at(8, 1);
        assert!(s.connect(AGENT_PORT).is_err());
        // The guest reposted the buffer: it is done.
        s.event_queue_at(9, 1);
        assert!(!s.resetting);
        s.connect(AGENT_PORT).unwrap();
        s.guest_packet(&from_guest(Op::Request, 4001, 7000), &[]);
        assert!(ops(&drain(&mut s, 4096)).contains(&Op::Response));
    }

    /// The guest may take an event and repost its buffer before the device
    /// looks at the available index again (its event work was running): the
    /// reset still ends. Before, the device took the index after sending as
    /// the mark, and the guest's repost could already be in it: the reset
    /// never ended, and every connection was refused until the next one.
    #[test]
    fn a_reset_ends_however_soon_the_guest_reposts() {
        let mut s = ready();
        s.event_queue_at(8, 0);
        s.transport_reset();
        // Sent in buffer 0; the guest reposted it at once: index 9.
        s.event_sent(1);
        s.event_queue_at(9, 1);
        assert!(!s.resetting);
        // With the indexes wrapping around too.
        s.transport_reset();
        s.event_sent(u16::MAX);
        s.event_queue_at(u16::MAX.wrapping_add(7), u16::MAX);
        assert!(s.resetting, "one buffer short");
        s.event_queue_at(u16::MAX.wrapping_add(8), u16::MAX);
        assert!(!s.resetting);
    }

    /// Two resets before the guest took the first: the second ends only
    /// once the guest has reposted both buffers, so a repost for the first
    /// does not end it early (a connection made then would be reset by the
    /// second).
    #[test]
    fn a_reset_ends_only_once_every_event_was_taken() {
        let mut s = ready();
        s.event_queue_at(8, 0);
        s.transport_reset();
        s.event_sent(1);
        s.transport_reset();
        s.event_sent(2);
        s.event_queue_at(9, 2);
        assert!(s.resetting, "the second event is not taken yet");
        assert!(s.connect(AGENT_PORT).is_err());
        s.event_queue_at(10, 2);
        assert!(!s.resetting);
        // The guest's driver resets the device: what it kept posted is
        // learnt again.
        s.driver_reset();
        s.activate();
        s.event_queue_at(2, 0);
        s.transport_reset();
        s.event_sent(1);
        s.event_queue_at(3, 1);
        assert!(!s.resetting);
    }

    #[test]
    fn a_gone_machine_fails_everything() {
        let mut s = ready();
        let (id, _) = connected(&mut s);
        s.machine_gone();
        assert!(matches!(
            s.read(id, &mut [0; 1]),
            Poll::Ready(Err(e)) if e.kind() == io::ErrorKind::NotConnected
        ));
        assert_eq!(s.connect(1).unwrap_err().kind(), io::ErrorKind::NotConnected);
        assert!(s.listen(1).is_err());
    }

    #[test]
    fn a_stopped_guest_fails_every_operation_until_its_driver_starts_again() {
        let mut s = ready();
        let (id, port) = connected(&mut s);
        let (quiet, _) = connected(&mut s);
        s.listen(7000).unwrap();
        s.guest_packet(&from_guest(Op::Rw, AGENT_PORT, port), b"bye");
        s.transport_reset();
        let (id2, port2) = {
            take_reset(&mut s);
            connected(&mut s)
        };
        s.guest_packet(&from_guest(Op::Rw, AGENT_PORT, port2), b"still");
        s.guest_stopped();
        let stopped = |r: Poll<usize>| matches!(r, Poll::Ready(Err(e)) if is_guest_stopped(&e));
        // What the guest sent before stays readable; then a reset.
        assert_eq!(read_all(&mut s, id2), b"still");
        assert!(stopped(s.read(id2, &mut [0; 4])));
        assert_eq!(read_all(&mut s, id), b"bye");
        assert!(stopped(s.read(id, &mut [0; 4])), "reset earlier, stopped now");
        assert!(stopped(s.write(quiet, b"x")));
        assert!(matches!(s.flush(quiet), Poll::Ready(Err(e)) if is_guest_stopped(&e)));
        assert!(is_guest_stopped(&s.connect(AGENT_PORT).unwrap_err()));
        assert!(matches!(s.accept(7000), Poll::Ready(Err(e)) if is_guest_stopped(&e)));
        assert!(!s.event_pending && !s.resetting, "no reset left to deliver");
        // The guest's packets are dropped, unanswered.
        s.guest_packet(&from_guest(Op::Request, 4000, 7000), &[]);
        s.guest_packet(&from_guest(Op::Rw, 4001, 7000), b"stray");
        assert!(drain(&mut s, 4096).is_empty());
        // Released handles go, as ever.
        for i in [id, id2, quiet] {
            s.release(i);
        }
        assert_eq!(s.connection_count(), 0);
        // The guest boots again: its driver resets the device, then starts
        // it.
        s.driver_reset();
        assert_eq!(s.connect(AGENT_PORT).unwrap_err().kind(), io::ErrorKind::ConnectionRefused);
        s.activate();
        connected(&mut s);
        s.guest_packet(&from_guest(Op::Request, 4002, 7000), &[]);
        assert!(matches!(s.accept(7000), Poll::Ready(Ok(_))));
        // A guest that is gone stays gone.
        s.machine_gone();
        s.guest_stopped();
        s.activate();
        assert_eq!(s.connect(AGENT_PORT).unwrap_err().kind(), io::ErrorKind::NotConnected);
    }

    #[test]
    fn connections_take_turns() {
        let mut s = ready();
        let (a, _) = connected(&mut s);
        let (b, _) = connected(&mut s);
        for id in [a, b] {
            let mut credit = from_guest(Op::CreditUpdate, AGENT_PORT, s.ports(id).unwrap().0);
            credit.buf_alloc = 1 << 20;
            s.guest_packet(&credit, &[]);
            assert!(matches!(s.write(id, &[0; 10_000]), Poll::Ready(Ok(_))));
        }
        let out = drain(&mut s, 1000);
        let order: Vec<u32> = out.iter().map(|(h, _)| h.src_port).take(4).collect();
        assert_ne!(order[0], order[1]);
        assert_eq!(order[0], order[2]);
        assert_eq!(order[1], order[3]);
    }

    /// A small deterministic generator, for the randomized test below.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// Whatever a guest sends, in any order: the device never panics, never
    /// sends a connection more than its credit or a packet more than its
    /// room, and holds no more than its buffers allow.
    #[test]
    fn any_guest_packets_keep_the_invariants() {
        for seed in 1..200u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let mut s = ready();
            s.listen(7000).unwrap();
            let mut ids = Vec::new();
            // What each guest port has been granted: (buf_alloc, fwd_cnt).
            let mut sent: HashMap<(u32, u32), u32> = HashMap::new();
            let mut grants: HashMap<(u32, u32), (u32, u32)> = HashMap::new();
            for _ in 0..400 {
                match rng.below(10) {
                    0 => ids.extend(s.connect(AGENT_PORT + rng.below(3) as u32).ok()),
                    1 => {
                        if let Poll::Ready(Ok(id)) = s.accept(7000) {
                            ids.push(id);
                        }
                    }
                    2 if !ids.is_empty() => {
                        let id = ids[rng.below(ids.len() as u64) as usize];
                        let _ = s.write(id, &vec![1; rng.below(100_000) as usize]);
                    }
                    3 if !ids.is_empty() => {
                        let id = ids[rng.below(ids.len() as u64) as usize];
                        let _ = s.read(id, &mut vec![0; rng.below(100_000) as usize]);
                    }
                    4 if !ids.is_empty() => {
                        let i = rng.below(ids.len() as u64) as usize;
                        s.release(ids.swap_remove(i));
                    }
                    5 => {
                        let room = rng.below(70_000) as usize;
                        if let Some((h, p)) = s.next_packet(room) {
                            assert!(p.len() <= room && p.len() == h.len as usize);
                            assert_eq!((h.src_cid, h.dst_cid), (HOST_CID, GUEST));
                            if h.op == Op::Rw {
                                let k = (h.src_port, h.dst_port);
                                let (alloc, fwd) = grants.get(&k).copied().unwrap_or((0, 0));
                                let total = sent.entry(k).or_default();
                                *total = total.wrapping_add(p.len() as u32);
                                assert!(
                                    total.wrapping_sub(fwd) <= alloc,
                                    "seed {seed}: past the guest's credit"
                                );
                            }
                        }
                    }
                    6 => match rng.below(8) {
                        0 => s.guest_stopped(),
                        1 => {
                            s.driver_reset();
                            s.activate();
                        }
                        _ => s.transport_reset(),
                    },
                    _ => {
                        // A packet from the guest, ports from a small set so
                        // that they hit live connections.
                        let known: Vec<(u32, u32)> =
                            ids.iter().filter_map(|&id| s.ports(id)).collect();
                        let (host_port, guest_port) = if !known.is_empty() && rng.below(4) > 0 {
                            known[rng.below(known.len() as u64) as usize]
                        } else {
                            (7000 + rng.below(2) as u32, AGENT_PORT + rng.below(3) as u32)
                        };
                        let op = match rng.below(9) {
                            0 => Op::Request,
                            1 => Op::Response,
                            2 => Op::Rst,
                            3 => Op::Shutdown,
                            4 | 5 => Op::Rw,
                            6 => Op::CreditUpdate,
                            7 => Op::CreditRequest,
                            _ => Op::Unknown(rng.below(100) as u16),
                        };
                        let mut h = from_guest(op, guest_port, host_port);
                        h.flags = rng.below(4) as u32;
                        h.buf_alloc = rng.below(1 << 20) as u32;
                        h.fwd_cnt = rng.next() as u32;
                        let k = (host_port, guest_port);
                        // A fresh connection starts its count at zero.
                        if op == Op::Request || op == Op::Response {
                            sent.insert(k, 0);
                            h.fwd_cnt = 0;
                        }
                        grants.insert(k, (h.buf_alloc, h.fwd_cnt));
                        let payload = vec![2; rng.below(MAX_PAYLOAD as u64) as usize];
                        s.guest_packet(&h, if op == Op::Rw { &payload } else { &[] });
                    }
                }
                assert!(s.conns.len() <= MAX_CONNECTIONS);
                assert!(s.resets.len() <= MAX_PENDING_RESETS);
                for c in s.conns.values() {
                    assert!(c.rx.len() <= RX_CAPACITY as usize);
                    assert!(c.tx.len() <= TX_CAPACITY);
                }
                for (key, id) in &s.by_key {
                    assert_eq!(s.conns[id].key, *key);
                }
            }
        }
    }
}
