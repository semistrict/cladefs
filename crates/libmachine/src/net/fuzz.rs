// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Fuzzing the guest-facing network code (doc-30: guest-facing parsers are
//! fuzzed): the whole path a guest frame takes into the VMM, through the
//! stack's own inspection, smoltcp, the DNS server and the relay, with a
//! fake host so that no real socket or thread is involved.
//!
//! The decider's questions are answered (or left to time out) by the fuzz
//! input, the caller's host entries are set, and time jumps far enough for
//! learnt names to expire.
//!
//! Beside raw frames, the input drives guest connections that follow TCP
//! ([`TcpGuest`]), so that data moves both ways; every byte that reaches
//! either end is checked against what the other sent.
//!
//! The cargo-fuzz targets in `crates/libmachine/fuzz` call
//! [`guest_frames`] and [`dns_query`] (with the `fuzzing` feature); the
//! tests below run the same harness over seeds and their mutations, so
//! that every `cargo test` exercises it.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use smoltcp::phy::ChecksumCapabilities;
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, EthernetAddress, EthernetFrame, EthernetProtocol,
    EthernetRepr, Icmpv4Packet, Icmpv4Repr, IpProtocol, Ipv4Packet, Ipv4Repr, TcpControl,
    TcpPacket, TcpRepr, TcpSeqNumber, UdpPacket, UdpRepr,
};

use super::decide::{ConnectionRequest, Decider};
use super::hosts::Hosts;
use super::policy::{EgressPolicy, HostPattern, Verdict};
use super::stack::{
    GATEWAY_MAC, GUEST_MAC, HostNet, HostStream, LookupError, MAX_FRAME, Options, Ready, Stack,
};
use super::{GATEWAY_ADDR, GUEST_ADDR, NetworkConfig, Shared};

/// A host whose connections and lookups the test or fuzzer completes.
#[derive(Debug, Default)]
pub(crate) struct FakeHost {
    /// Every connection asked for, in order.
    pub(crate) connects: Vec<SocketAddrV4>,
    /// Lookups not yet answered: (id, name).
    pub(crate) lookups: Vec<(u64, String)>,
    /// Every question for the decider, in order: (id, request).
    pub(crate) decisions: Vec<(u64, ConnectionRequest)>,
    /// Connections fail.
    pub(crate) refuse: bool,
    /// What each connection's stream was written, by connection ID (the
    /// latest stream with that ID).
    pub(crate) written: HashMap<u64, Arc<Mutex<Vec<u8>>>>,
    /// While set, every stream's writes would block: a host that does not
    /// read.
    pub(crate) stalled: Arc<AtomicBool>,
}

impl FakeHost {
    /// What connection `id`'s stream was written.
    pub(crate) fn written(&self, id: u64) -> Vec<u8> {
        self.written.get(&id).map(|w| w.lock().unwrap().clone()).unwrap_or_default()
    }
}

/// The byte a fake host stream sends at `offset`.
pub(crate) fn host_byte(offset: usize) -> u8 {
    (offset % 251) as u8
}

/// The byte a [`TcpGuest`] sends at `offset`.
pub(crate) fn guest_byte(offset: usize) -> u8 {
    (offset * 7 % 253) as u8
}

/// A fake host connection: it sends `pending` bytes ([`host_byte`] at each
/// offset), then EOF; it takes everything written to it (and logs it), but
/// says "would block" every third write, so that both paths run.
#[derive(Debug)]
pub(crate) struct FakeStream {
    fail: bool,
    pending: usize,
    sent: usize,
    writes: usize,
    written: Arc<Mutex<Vec<u8>>>,
    stalled: Arc<AtomicBool>,
}

impl Read for FakeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = buf.len().min(self.pending).min(3000);
        for (i, b) in buf[..n].iter_mut().enumerate() {
            *b = host_byte(self.sent + i);
        }
        self.pending -= n;
        self.sent += n;
        Ok(n)
    }
}

impl Write for FakeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writes += 1;
        if self.writes.is_multiple_of(3) || self.stalled.load(Ordering::Relaxed) {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        self.written.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl HostStream for FakeStream {
    fn connect_result(&self) -> io::Result<()> {
        if self.fail { Err(io::ErrorKind::ConnectionRefused.into()) } else { Ok(()) }
    }

    fn shutdown_write(&self) {}

    fn reset_on_drop(&self) {}
}

impl HostNet for FakeHost {
    type Stream = FakeStream;

    fn connect(&mut self, id: u64, to: SocketAddrV4) -> io::Result<FakeStream> {
        self.connects.push(to);
        let written = Arc::new(Mutex::new(Vec::new()));
        self.written.insert(id, written.clone());
        Ok(FakeStream {
            fail: self.refuse || to.port().is_multiple_of(7),
            pending: usize::from(to.port() % 5) * 10_000,
            sent: 0,
            writes: 0,
            written,
            stalled: self.stalled.clone(),
        })
    }

    fn resolve(&mut self, id: u64, name: String) {
        self.lookups.push((id, name));
    }

    fn decide(&mut self, id: u64, request: ConnectionRequest, _decider: Decider) {
        self.decisions.push((id, request));
    }
}

/// Builds the frames a guest sends, and reads the ones it gets.
pub(crate) mod frames {
    use super::*;

    fn eth(ethertype: EthernetProtocol, len: usize, fill: impl FnOnce(&mut [u8])) -> Vec<u8> {
        let repr = EthernetRepr {
            src_addr: EthernetAddress(GUEST_MAC),
            dst_addr: EthernetAddress(GATEWAY_MAC),
            ethertype,
        };
        let mut buf = vec![0; repr.buffer_len() + len];
        let mut frame = EthernetFrame::new_unchecked(&mut buf[..]);
        repr.emit(&mut frame);
        fill(frame.payload_mut());
        buf
    }

    pub(crate) fn arp(repr: ArpRepr) -> Vec<u8> {
        eth(EthernetProtocol::Arp, repr.buffer_len(), |b| {
            repr.emit(&mut ArpPacket::new_unchecked(b))
        })
    }

    /// An IPv4 frame from `src` to `dst` whose payload `fill` writes.
    pub(crate) fn ip(
        src: Ipv4Addr,
        dst: Ipv4Addr,
        next_header: IpProtocol,
        len: usize,
        fill: impl FnOnce(&mut [u8]),
    ) -> Vec<u8> {
        let repr =
            Ipv4Repr { src_addr: src, dst_addr: dst, next_header, payload_len: len, hop_limit: 64 };
        eth(EthernetProtocol::Ipv4, repr.buffer_len() + len, |b| {
            let mut packet = Ipv4Packet::new_unchecked(b);
            repr.emit(&mut packet, &ChecksumCapabilities::default());
            fill(packet.payload_mut());
        })
    }

    pub(crate) fn tcp(src: Ipv4Addr, dst: Ipv4Addr, repr: TcpRepr<'_>) -> Vec<u8> {
        ip(src, dst, IpProtocol::Tcp, repr.buffer_len(), |b| {
            repr.emit(
                &mut TcpPacket::new_unchecked(b),
                &src.into(),
                &dst.into(),
                &ChecksumCapabilities::default(),
            )
        })
    }

    pub(crate) fn udp(
        src: Ipv4Addr,
        dst: Ipv4Addr,
        sport: u16,
        dport: u16,
        data: &[u8],
    ) -> Vec<u8> {
        let repr = UdpRepr { src_port: sport, dst_port: dport };
        ip(src, dst, IpProtocol::Udp, repr.header_len() + data.len(), |b| {
            repr.emit(
                &mut UdpPacket::new_unchecked(b),
                &src.into(),
                &dst.into(),
                data.len(),
                |p| p.copy_from_slice(data),
                &ChecksumCapabilities::default(),
            )
        })
    }

    pub(crate) fn icmp(src: Ipv4Addr, dst: Ipv4Addr, repr: Icmpv4Repr<'_>) -> Vec<u8> {
        ip(src, dst, IpProtocol::Icmp, repr.buffer_len(), |b| {
            repr.emit(&mut Icmpv4Packet::new_unchecked(b), &ChecksumCapabilities::default())
        })
    }

    fn ipv4(frame: &[u8]) -> Option<Ipv4Packet<&[u8]>> {
        let eth = EthernetFrame::new_checked(frame).ok()?;
        if eth.ethertype() != EthernetProtocol::Ipv4 {
            return None;
        }
        Ipv4Packet::new_checked(eth.payload()).ok()
    }

    /// A TCP segment for the guest: its header (payload left out) and
    /// payload. Checks the checksums the stack wrote.
    #[cfg(test)]
    pub(crate) fn parse_tcp(frame: &[u8]) -> Option<(TcpRepr<'static>, Vec<u8>)> {
        parse_tcp_from(frame).map(|(_, repr, payload)| (repr, payload))
    }

    /// [`parse_tcp`], with the segment's source address.
    pub(crate) fn parse_tcp_from(frame: &[u8]) -> Option<(Ipv4Addr, TcpRepr<'static>, Vec<u8>)> {
        let ip = ipv4(frame)?;
        assert!(ip.verify_checksum());
        if ip.next_header() != IpProtocol::Tcp {
            return None;
        }
        let tcp = TcpPacket::new_checked(ip.payload()).ok()?;
        let src = ip.src_addr().into();
        let dst = ip.dst_addr().into();
        let repr = TcpRepr::parse(&tcp, &src, &dst, &ChecksumCapabilities::default()).ok()?;
        let payload = repr.payload.to_vec();
        Some((ip.src_addr(), TcpRepr { payload: &[], ..repr }, payload))
    }

    #[cfg(test)]
    /// A UDP datagram for the guest: (source, source port, payload).
    pub(crate) fn parse_udp(frame: &[u8]) -> Option<(Ipv4Addr, u16, Vec<u8>)> {
        let ip = ipv4(frame)?;
        if ip.next_header() != IpProtocol::Udp {
            return None;
        }
        let udp = UdpPacket::new_checked(ip.payload()).ok()?;
        assert!(udp.verify_checksum(&ip.src_addr().into(), &ip.dst_addr().into()));
        Some((ip.src_addr(), udp.src_port(), udp.payload().to_vec()))
    }

    /// The guest's ARP request for the gateway.
    pub(crate) fn arp_request() -> Vec<u8> {
        arp(ArpRepr::EthernetIpv4 {
            operation: ArpOperation::Request,
            source_hardware_addr: EthernetAddress(GUEST_MAC),
            source_protocol_addr: GUEST_ADDR,
            target_hardware_addr: EthernetAddress([0; 6]),
            target_protocol_addr: GATEWAY_ADDR,
        })
    }
}

/// What a [`TcpGuest`] segment does besides carrying data.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Flags {
    /// The guest closes its end after the data.
    pub(crate) fin: bool,
    /// The guest resets the connection.
    pub(crate) rst: bool,
    /// The guest's window is closed.
    pub(crate) zero_window: bool,
}

/// A guest connection a [`TcpGuest`] drives.
#[derive(Debug)]
pub(crate) struct GuestConn {
    pub(crate) port: u16,
    pub(crate) dst: SocketAddrV4,
    isn: TcpSeqNumber,
    stack_isn: Option<TcpSeqNumber>,
    /// The host's data received in order.
    pub(crate) received: usize,
    /// The stack's FIN came, after all its data.
    pub(crate) received_fin: bool,
    /// The guest's data the stack acknowledged.
    pub(crate) acked: usize,
    /// The guest closed its end after this many bytes.
    pub(crate) fin_at: Option<usize>,
    pub(crate) fin_acked: bool,
    /// The stack reset the connection, or the guest did.
    pub(crate) reset: bool,
    /// The guest acknowledged the stack's FIN: once its own FIN is
    /// acknowledged too, the connection is over, and the guest sends
    /// nothing more.
    fin_ack_sent: bool,
    /// The stack's ID for it, when last seen.
    id: Option<u64>,
}

impl GuestConn {
    pub(crate) fn guest(&self) -> SocketAddrV4 {
        SocketAddrV4::new(GUEST_ADDR, self.port)
    }

    /// The handshake completed, and the stack has not reset it.
    pub(crate) fn open(&self) -> bool {
        self.stack_isn.is_some() && !self.reset
    }
}

/// Guest connections that follow TCP: a guest that sends [`guest_byte`] at
/// each offset (going back to what the stack acknowledged, so nothing is
/// ever missing), acknowledges what it received, and checks that every byte
/// of the host's data is [`host_byte`] at its offset: so a byte lost,
/// duplicated at another offset or damaged anywhere between the two ends
/// is caught.
#[derive(Debug, Default)]
pub(crate) struct TcpGuest {
    pub(crate) conns: Vec<GuestConn>,
}

fn seq_offset(seq: TcpSeqNumber, base: TcpSeqNumber) -> i64 {
    i64::from(seq.0.wrapping_sub(base.0))
}

impl TcpGuest {
    /// A SYN for a new connection from `port` to `dst`, and the
    /// connection's index (a connection with the same ends is replaced).
    pub(crate) fn connect(&mut self, port: u16, dst: SocketAddrV4) -> (usize, Vec<u8>) {
        let isn = TcpSeqNumber(0x1000_0000_i32.wrapping_add(i32::from(port).wrapping_mul(4099)));
        let conn = GuestConn {
            port,
            dst,
            isn,
            stack_isn: None,
            received: 0,
            received_fin: false,
            acked: 0,
            fin_at: None,
            fin_acked: false,
            reset: false,
            fin_ack_sent: false,
            id: None,
        };
        let i = match self.conns.iter().position(|c| c.port == port && c.dst == dst) {
            Some(i) => {
                self.conns[i] = conn;
                i
            }
            None => {
                self.conns.push(conn);
                self.conns.len() - 1
            }
        };
        let syn = frames::tcp(
            GUEST_ADDR,
            *dst.ip(),
            TcpRepr {
                src_port: port,
                dst_port: dst.port(),
                control: TcpControl::Syn,
                seq_number: isn,
                ack_number: None,
                window_len: 65535,
                window_scale: None,
                max_seg_size: Some(1460),
                sack_permitted: false,
                sack_ranges: [None; 3],
                timestamp: None,
                payload: &[],
            },
        );
        (i, syn)
    }

    /// A segment on connection `i` with `len` bytes of the guest's data
    /// from what the stack acknowledged, acknowledging everything received:
    /// None before the stack's SYN-ACK, or once it reset the connection.
    pub(crate) fn segment(&mut self, i: usize, len: usize, flags: Flags) -> Option<Vec<u8>> {
        let start = self.conns.get(i)?.acked;
        self.segment_at(i, start, len, flags)
    }

    /// [`segment`](Self::segment), with the data from offset `start`
    /// (past what was acknowledged: out of order).
    pub(crate) fn segment_at(
        &mut self,
        i: usize,
        start: usize,
        len: usize,
        flags: Flags,
    ) -> Option<Vec<u8>> {
        let c = self.conns.get_mut(i)?;
        let stack_isn = c.stack_isn?;
        if c.reset || (c.fin_acked && c.fin_ack_sent) {
            return None;
        }
        // After its FIN, the guest only sends the FIN again, or an ACK.
        let (start, len) = match c.fin_at {
            Some(n) => (n + usize::from(c.fin_acked), 0),
            None => (start, len),
        };
        if flags.fin && c.fin_at.is_none() {
            c.fin_at = Some(start + len);
        }
        let control = if flags.rst {
            TcpControl::Rst
        } else if c.fin_at.is_some() && !c.fin_acked {
            TcpControl::Fin
        } else if len > 0 {
            TcpControl::Psh
        } else {
            TcpControl::None
        };
        let payload: Vec<u8> = (start..start + len).map(guest_byte).collect();
        let ack = stack_isn + 1 + c.received + usize::from(c.received_fin);
        c.reset |= flags.rst;
        c.fin_ack_sent |= c.received_fin;
        Some(frames::tcp(
            GUEST_ADDR,
            *c.dst.ip(),
            TcpRepr {
                src_port: c.port,
                dst_port: c.dst.port(),
                control,
                seq_number: c.isn + 1 + start,
                ack_number: Some(ack),
                window_len: if flags.zero_window { 0 } else { 65535 },
                window_scale: None,
                max_seg_size: None,
                sack_permitted: false,
                sack_ranges: [None; 3],
                timestamp: None,
                payload: &payload,
            },
        ))
    }

    /// Takes a frame the stack sent the guest; returns whether it was for
    /// one of the connections. Panics on a byte of host data that is not
    /// [`host_byte`] at its offset.
    pub(crate) fn take(&mut self, frame: &[u8]) -> bool {
        let Some((src, repr, payload)) = frames::parse_tcp_from(frame) else {
            return false;
        };
        let from = SocketAddrV4::new(src, repr.src_port);
        let Some(c) =
            self.conns.iter_mut().rev().find(|c| c.port == repr.dst_port && c.dst == from)
        else {
            return false;
        };
        if repr.control == TcpControl::Rst {
            c.reset = true;
            return true;
        }
        if repr.control == TcpControl::Syn && c.stack_isn.is_none() {
            c.stack_isn = Some(repr.seq_number);
        }
        if let Some(ack) = repr.ack_number {
            let acked = seq_offset(ack, c.isn + 1);
            if acked >= 0 {
                let acked = acked as usize;
                match c.fin_at {
                    Some(n) => {
                        c.fin_acked |= acked > n;
                        c.acked = c.acked.max(acked.min(n));
                    }
                    None => c.acked = c.acked.max(acked),
                }
            }
        }
        let Some(isn) = c.stack_isn else {
            return true;
        };
        if repr.control == TcpControl::Syn {
            return true;
        }
        let at = seq_offset(repr.seq_number, isn + 1);
        if at < 0 {
            return true;
        }
        let at = at as usize;
        for (i, b) in payload.iter().enumerate() {
            assert_eq!(
                *b,
                host_byte(at + i),
                "host byte {} on guest port {} from {}",
                at + i,
                c.port,
                c.dst
            );
        }
        let end = at + payload.len();
        if at <= c.received && end > c.received {
            c.received = end;
        }
        if repr.control == TcpControl::Fin && end == c.received {
            c.received_fin = true;
        }
        true
    }

    /// Checks that each open connection's host stream got exactly the
    /// guest's bytes, in order, and that one both ends closed cleanly got
    /// all of them before it went; returns how many they got.
    pub(crate) fn check_host_side(&mut self, stack: &Stack<FakeHost>) -> usize {
        let mut total = 0;
        for c in self.conns.iter_mut().filter(|c| c.open()) {
            let Some(id) = stack.connection_id(c.guest(), c.dst) else {
                let closed = c.fin_acked && c.received_fin;
                if let (Some(id), Some(n), true) = (c.id.take(), c.fin_at, closed) {
                    let written = stack.host().written(id);
                    assert!(
                        written.len() == n,
                        "connection {id} (guest port {}) closed with {} of the guest's {n} bytes \
                         written to the host",
                        c.port,
                        written.len()
                    );
                }
                continue;
            };
            c.id = Some(id);
            let written = stack.host().written(id);
            total += written.len();
            for (i, b) in written.iter().enumerate() {
                assert_eq!(
                    *b,
                    guest_byte(i),
                    "guest byte {i} on connection {id} (guest port {})",
                    c.port
                );
            }
        }
        total
    }
}

/// Where the fuzzer's TCP guest connects: allowed by a name rule through a
/// host entry, allowed with host data, asking the decider, a host entry
/// through the gateway, allowed with more host data, and refused by the
/// host (port 14: `FakeHost` fails ports divisible by 7).
const GUEST_DSTS: [(Ipv4Addr, u16); 6] = [
    (Ipv4Addr::new(1, 2, 3, 4), 443),
    (Ipv4Addr::new(9, 9, 9, 9), 4),
    (Ipv4Addr::new(1, 2, 3, 4), 45000),
    (GATEWAY_ADDR, 8081),
    (Ipv4Addr::new(9, 9, 9, 9), 13),
    (Ipv4Addr::new(9, 9, 9, 9), 14),
];

/// The fuzzer's policy: some of everything, so that every path runs.
fn fuzz_policy() -> EgressPolicy {
    EgressPolicy::deny_all()
        .deny(HostPattern::Any, 25)
        .allow(HostPattern::name("*.test").unwrap(), 443)
        .allow(HostPattern::name("mock.svc").unwrap(), 8000..=9000)
        .allow(Ipv4Addr::LOCALHOST, 1..=1024)
        .ask(Ipv4Addr::new(1, 2, 3, 4), 40001..=50000)
        .allow(HostPattern::Any, 1..=40000)
}

/// The fuzzer's network: its policy, host entries, a decider the input
/// answers for (FakeHost never calls it), and a small log.
fn fuzz_shared() -> Arc<Shared> {
    let config = NetworkConfig::new(fuzz_policy())
        .with_hosts(
            Hosts::new()
                .with("mock.svc", Ipv4Addr::LOCALHOST)
                .unwrap()
                .with("one.test", Ipv4Addr::new(1, 2, 3, 4))
                .unwrap(),
        )
        .with_dns_ttl(std::time::Duration::from_secs(5))
        .with_log_capacity(16)
        .with_decider(Decider::new(|_| None).with_timeout(std::time::Duration::from_millis(300)));
    Shared::new(config).expect("a valid configuration")
}

/// What each record of a fuzz input does, by its first byte.
const OP_FRAME: u8 = 0;
const OP_IP: u8 = 1;
const OP_HOST: u8 = 2;
const OP_TIME: u8 = 3;
const OP_LOOKUP: u8 = 4;
const OP_DECIDE: u8 = 5;
const OP_CONNECT: u8 = 6;
const OP_SEGMENT: u8 = 7;
const OPS: u8 = 8;

/// Runs a fuzz input through a fresh stack. The input is records of an
/// operation byte, a 16-bit length and that many bytes: a raw guest frame;
/// an IPv4 payload from the guest (to the gateway or elsewhere, by its
/// first byte; protocol by its second); a host connection becoming ready;
/// time passing; a DNS lookup answered; the decider answering a question;
/// a [`TcpGuest`] connection opened, or a segment on one (data, FIN, RST,
/// a closed window, out of order). Checks the stack's invariants after
/// each, and every byte that reached either end.
pub fn guest_frames(data: &[u8]) {
    run(data);
}

/// What a run reached, for the tests that check the seeds do.
#[derive(Debug, Default)]
struct Reach {
    /// The most host data the guest's connections had received in order.
    to_guest: usize,
    /// The most guest data their host streams had been written.
    to_host: usize,
}

fn run(data: &[u8]) -> Reach {
    let mut reach = Reach::default();
    let policy = fuzz_shared();
    let options = Options { verify_checksums: false, tcp_buffer: 8 << 10 };
    let mut now = Instant::from_millis(0);
    let mut stack = Stack::new(policy.clone(), FakeHost::default(), 0, options, now);
    let mut guest = TcpGuest::default();
    stack.guest_frame(frames::arp_request());
    let mut rest = data;
    while rest.len() >= 3 {
        let op = rest[0] % OPS;
        let len = usize::from(u16::from_le_bytes([rest[1], rest[2]])).min(rest.len() - 3);
        let body = &rest[3..3 + len];
        rest = &rest[3 + len..];
        match op {
            OP_FRAME => stack.guest_frame(body.to_vec()),
            OP_IP if body.len() >= 2 => {
                let dst = match body[0] % 4 {
                    0 => GATEWAY_ADDR,
                    1 => Ipv4Addr::new(1, 2, 3, 4),
                    2 => Ipv4Addr::new(10, 0, 2, 3),
                    _ => Ipv4Addr::new(192, 168, 0, 1),
                };
                let proto = match body[1] % 4 {
                    0 => IpProtocol::Tcp,
                    1 => IpProtocol::Udp,
                    2 => IpProtocol::Icmp,
                    _ => IpProtocol::Unknown(body[1]),
                };
                let payload = &body[2..];
                stack.guest_frame(frames::ip(GUEST_ADDR, dst, proto, payload.len(), |b| {
                    b.copy_from_slice(payload)
                }));
            }
            OP_HOST => {
                let ids = stack.connection_ids();
                if !ids.is_empty() {
                    let id = ids[len % ids.len()];
                    let bits = body.first().copied().unwrap_or(0xff);
                    let ready = Ready {
                        readable: bits & 1 != 0,
                        writable: bits & 2 != 0,
                        closed: bits & 4 != 0,
                    };
                    stack.host_event(id, ready);
                }
            }
            OP_TIME => now += Duration::from_millis(len as u64 * 10),
            OP_LOOKUP => {
                if let Some((id, _)) = stack.host_mut().lookups.pop() {
                    let result = match body.first().map(|b| b % 3) {
                        Some(0) => Err(LookupError::NoName),
                        Some(1) => Err(LookupError::Failed),
                        _ => {
                            Ok(body.as_chunks::<4>().0.iter().map(|&c| Ipv4Addr::from(c)).collect())
                        }
                    };
                    stack.resolved(id, result);
                }
            }
            OP_DECIDE => {
                let ids = stack.deciding_ids();
                if !ids.is_empty() {
                    let id = ids[len % ids.len()];
                    let answer = match body.first().map(|b| b % 3) {
                        Some(0) => None,
                        Some(1) => Some(Verdict::Deny),
                        _ => Some(Verdict::Allow),
                    };
                    stack.decided(id, answer);
                }
            }
            OP_CONNECT => {
                let (addr, port) =
                    GUEST_DSTS[usize::from(body.first().copied().unwrap_or(0)) % GUEST_DSTS.len()];
                let guest_port = 50000 + (len % 64) as u16;
                let (_, syn) = guest.connect(guest_port, SocketAddrV4::new(addr, port));
                stack.guest_frame(syn);
            }
            OP_SEGMENT if !guest.conns.is_empty() => {
                let i = usize::from(body.first().copied().unwrap_or(0)) % guest.conns.len();
                let bits = body.get(1).copied().unwrap_or(0);
                let size = usize::from(u16::from_le_bytes([
                    body.get(2).copied().unwrap_or(0),
                    body.get(3).copied().unwrap_or(0),
                ])) % 9000;
                let flags =
                    Flags { fin: bits & 1 != 0, rst: bits & 2 != 0, zero_window: bits & 4 != 0 };
                let segment = if bits & 8 != 0 {
                    // Out of order: past a gap the guest fills later.
                    let start = guest.conns[i].acked + 1 + usize::from(bits >> 4) * 97;
                    guest.segment_at(i, start, size, flags)
                } else {
                    guest.segment(i, size, flags)
                };
                if let Some(frame) = segment {
                    stack.guest_frame(frame);
                }
            }
            _ => {}
        }
        now += Duration::from_millis(1);
        stack.poll(now);
        stack.check_invariants();
        while let Some(frame) = stack.take_frame() {
            assert!(frame.len() <= MAX_FRAME);
            guest.take(&frame);
        }
        let to_host = guest.check_host_side(&stack);
        let to_guest = guest.conns.iter().map(|c| c.received).sum();
        reach.to_host = reach.to_host.max(to_host);
        reach.to_guest = reach.to_guest.max(to_guest);
    }
    reach
}

/// Runs a fuzz input through the DNS query parser, and the replies built
/// from what it parsed.
pub fn dns_query(data: &[u8]) {
    match super::dns::parse_query(data) {
        Ok(q) => {
            let addrs = [Ipv4Addr::new(1, 2, 3, 4); 40];
            let reply = super::dns::reply(&q, super::dns::Rcode::NoError, &addrs, 60);
            assert!(reply.len() <= 512);
            assert_eq!(super::policy::normalize_name(&q.name).ok().as_deref(), Some(&*q.name));
        }
        Err(invalid) => {
            let _ = super::dns::error_reply(invalid);
        }
    }
}

/// Seed inputs for [`guest_frames`]: a session the fuzzer can mutate from.
pub fn seeds() -> Vec<Vec<u8>> {
    use smoltcp::wire::{TcpControl, TcpSeqNumber};
    let record = |op: u8, body: &[u8]| {
        let mut r = vec![op];
        r.extend_from_slice(&(body.len() as u16).to_le_bytes());
        r.extend_from_slice(body);
        r
    };
    let tcp = |dst: Ipv4Addr, port: u16, control, seq: i32, ack: Option<u32>, payload: &[u8]| {
        frames::tcp(
            GUEST_ADDR,
            dst,
            TcpRepr {
                src_port: 40000,
                dst_port: port,
                control,
                seq_number: TcpSeqNumber(seq),
                ack_number: ack.map(|a| TcpSeqNumber(a as i32)),
                window_len: 1024,
                window_scale: Some(2),
                max_seg_size: Some(1400),
                sack_permitted: true,
                sack_ranges: [None; 3],
                timestamp: None,
                payload,
            },
        )
    };
    let remote = Ipv4Addr::new(1, 2, 3, 4);
    let session = [
        record(OP_FRAME, &tcp(remote, 443, TcpControl::Syn, 0, None, b"")),
        record(OP_HOST, &[7]),
        record(OP_FRAME, &tcp(remote, 443, TcpControl::None, 1, Some(0), b"")),
        record(OP_FRAME, &tcp(remote, 443, TcpControl::Psh, 1, Some(0), b"GET / HTTP/1.0\r\n\r\n")),
        record(OP_HOST, &[1]),
        record(OP_TIME, &[]),
        record(OP_FRAME, &tcp(remote, 443, TcpControl::Fin, 19, Some(1), b"")),
        record(OP_FRAME, &tcp(GATEWAY_ADDR, 25, TcpControl::Syn, 0, None, b"")),
        record(
            OP_FRAME,
            &frames::udp(
                GUEST_ADDR,
                GATEWAY_ADDR,
                5353,
                53,
                &super::dns::query_bytes(1, "a.test", 1),
            ),
        ),
        record(OP_LOOKUP, &[2, 1, 2, 3, 4]),
        record(
            OP_FRAME,
            &frames::icmp(
                GUEST_ADDR,
                GATEWAY_ADDR,
                Icmpv4Repr::EchoRequest { ident: 1, seq_no: 1, data: b"x" },
            ),
        ),
        record(OP_IP, &[1, 0, 0x9c, 0x40, 0x01, 0xbb]),
        // A connection the decider is asked about, and its answer.
        record(OP_FRAME, &tcp(remote, 45000, TcpControl::Syn, 0, None, b"")),
        record(OP_DECIDE, &[2]),
        record(OP_HOST, &[3]),
        // A host entry's name, then a connection to it through the gateway.
        record(
            OP_FRAME,
            &frames::udp(
                GUEST_ADDR,
                GATEWAY_ADDR,
                5353,
                53,
                &super::dns::query_bytes(2, "mock.svc", 1),
            ),
        ),
        record(OP_FRAME, &tcp(GATEWAY_ADDR, 8080, TcpControl::Syn, 0, None, b"")),
        // Long enough for learnt names and the decider to run out.
        record(OP_TIME, &[0; 600]),
        record(OP_FRAME, &tcp(remote, 443, TcpControl::Syn, 0, None, b"")),
    ];
    // A TCP guest's connections: data both ways, with data out of order,
    // half-closed, waiting for the decider.
    let segment = |conn: u8, bits: u8, len: u16| {
        let [lo, hi] = len.to_le_bytes();
        record(OP_SEGMENT, &[conn, bits, lo, hi])
    };
    let guest = [
        record(OP_CONNECT, &[1]),
        record(OP_HOST, &[3]),
        segment(0, 0, 1000),
        record(OP_HOST, &[3]),
        segment(0, 0, 1000),
        segment(0, 0x18, 100),
        segment(0, 0, 2000),
        record(OP_HOST, &[3]),
        segment(0, 0, 0),
        record(OP_CONNECT, &[4]),
        record(OP_HOST, &[3]),
        segment(1, 0, 500),
        segment(0, 1, 16),
        record(OP_HOST, &[3]),
        segment(0, 0, 0),
        segment(1, 0, 0),
        record(OP_CONNECT, &[2]),
        record(OP_DECIDE, &[2]),
        record(OP_HOST, &[3]),
        segment(1, 4, 0),
        segment(0, 0, 0),
        segment(1, 0, 10),
    ];
    vec![
        session.concat(),
        session[..3].concat(),
        session[7..10].concat(),
        session[12..].concat(),
        guest.concat(),
        guest[..12].concat(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic generator, so that a failure reproduces.
    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// The seeds, then mutations of them (bytes flipped, inserted,
    /// removed, ranges duplicated), then plain random inputs.
    #[test]
    fn harness_survives_mutated_and_random_inputs() {
        let seeds = seeds();
        for seed in &seeds {
            guest_frames(seed);
        }
        let mut rng = XorShift(0x9e37_79b9_7f4a_7c15);
        for _ in 0..1500 {
            let mut input = seeds[rng.below(seeds.len())].clone();
            for _ in 0..1 + rng.below(8) {
                if input.is_empty() {
                    break;
                }
                let at = rng.below(input.len());
                match rng.below(4) {
                    0 => input[at] ^= 1 << rng.below(8),
                    1 => input.insert(at, rng.next() as u8),
                    2 => {
                        input.remove(at);
                    }
                    _ => {
                        let end = (at + rng.below(64)).min(input.len());
                        let copy = input[at..end].to_vec();
                        input.splice(at..at, copy);
                    }
                }
            }
            guest_frames(&input);
        }
        for _ in 0..300 {
            let len = rng.below(600);
            let input: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            guest_frames(&input);
            dns_query(&input);
        }
    }

    #[test]
    fn dns_harness_survives_mutated_queries() {
        let mut rng = XorShift(7);
        let base = super::super::dns::query_bytes(9, "www.example.test", 1);
        dns_query(&base);
        for _ in 0..3000 {
            let mut q = base.clone();
            for _ in 0..1 + rng.below(4) {
                let at = rng.below(q.len());
                q[at] = rng.next() as u8;
            }
            q.truncate(rng.below(q.len() + 1));
            dns_query(&q);
        }
    }

    /// The seeds reach the paths they are for: a host connection made and
    /// a lookup asked for.
    #[test]
    fn seeds_reach_connections_and_lookups() {
        let policy = fuzz_shared();
        assert!(policy.policy().rules().len() > 1);
        let mut stack =
            Stack::new(policy, FakeHost::default(), 0, Options::default(), Instant::from_millis(0));
        stack.guest_frame(frames::arp_request());
        let session = &seeds()[0];
        // The session's first record is the SYN.
        let len = usize::from(u16::from_le_bytes([session[1], session[2]]));
        stack.guest_frame(session[3..3 + len].to_vec());
        stack.poll(Instant::from_millis(1));
        assert_eq!(stack.host().connects.len(), 1);
    }

    /// The TCP guest's seed moves data both ways (so that the fuzzer
    /// starts from transfers, not only from handshakes).
    #[test]
    fn the_guest_seed_moves_data() {
        let seeds = seeds();
        let reach = run(&seeds[4]);
        assert!(reach.to_guest > 10_000, "{reach:?}");
        assert!(reach.to_host >= 4_000, "{reach:?}");
    }

    /// The new seeds reach the decider, the host entries and expiry: run
    /// the session by hand and look at the log.
    #[test]
    fn seeds_reach_the_decider_and_host_entries() {
        let shared = fuzz_shared();
        let mut stack = Stack::new(
            shared.clone(),
            FakeHost::default(),
            0,
            Options::default(),
            Instant::from_millis(0),
        );
        stack.guest_frame(frames::arp_request());
        let mut rest = &seeds()[0][..];
        let mut now = Instant::from_millis(0);
        let mut decided = false;
        while rest.len() >= 3 {
            let (op, len) = (rest[0], usize::from(u16::from_le_bytes([rest[1], rest[2]])));
            let body = &rest[3..3 + len];
            rest = &rest[3 + len..];
            match op {
                OP_FRAME => stack.guest_frame(body.to_vec()),
                OP_DECIDE => {
                    for id in stack.deciding_ids() {
                        stack.decided(id, Some(Verdict::Allow));
                        decided = true;
                    }
                }
                OP_TIME => now += Duration::from_millis(len as u64 * 10),
                _ => {}
            }
            now += Duration::from_millis(1);
            stack.poll(now);
            stack.check_invariants();
        }
        assert!(decided, "the decider was asked");
        let connects = &stack.host().connects;
        assert!(connects.contains(&SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 4), 45000)));
        assert!(
            connects.contains(&SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8080)),
            "the host entry's name allowed the gateway: {connects:?}"
        );
    }
}
