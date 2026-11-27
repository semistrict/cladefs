// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The guest's network as the VMM sees it: a smoltcp interface that is the
//! guest's gateway, terminating the guest's TCP connections and relaying
//! each to a host socket the VMM opens on the guest's behalf, and answering
//! its DNS queries.
//!
//! The stack does no I/O of its own: frames come in through
//! [`guest_frame`](Stack::guest_frame) and go out through
//! [`take_frame`](Stack::take_frame), host sockets are opened through a
//! [`HostNet`] whose readiness comes back through
//! [`host_event`](Stack::host_event), and the network thread (or a test, or
//! the fuzzer) drives it all with [`poll`](Stack::poll).
//!
//! A guest connection, from its SYN: the SYN is checked against the egress
//! policy and held while the host connection is made; once that succeeds, a
//! smoltcp socket listening on exactly that destination is created and the
//! SYN replayed into it, so the guest's handshake completes only when the
//! host's did. A denied or failed connection has its SYN replayed with no
//! socket listening, and smoltcp answers it with a RST: the guest's
//! `connect` fails at once with "connection refused". A SYN an ask rule
//! matches waits first for the caller's decider (on its own thread, through
//! [`HostNet::decide`]) or its deadline, whichever comes first.
//!
//! Every connection is logged when it ends (a denied one at once), with
//! what decided it and the bytes it moved.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, Checksum, ChecksumCapabilities, DeviceCapabilities, Medium};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, EthernetProtocol, HardwareAddress, IpAddress, IpCidr,
    IpEndpoint, IpListenEndpoint, IpProtocol, Ipv4Packet, TcpPacket,
};

use super::decide::{ConnectionRequest, Decider};
use super::dns::{self, Rcode};
use super::log::{ConnectionRecord, DecidedBy, bump};
use super::policy::{Action, Destination, Verdict};
use super::{GATEWAY_ADDR, GUEST_ADDR, MTU, PREFIX_LEN, Shared};

/// The gateway's MAC address, and the guest's (locally administered).
pub(crate) const GATEWAY_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 1];
pub(crate) const GUEST_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 2];
/// The largest Ethernet frame: the MTU and the Ethernet header (no FCS).
pub(crate) const MAX_FRAME: usize = MTU as usize + 14;
/// The most guest connections (open, or waiting for their host connection)
/// at once; a SYN past it is refused. Each holds two TCP_BUFFERs.
pub(crate) const MAX_CONNECTIONS: usize = 128;
/// Each direction's buffer of a relayed TCP connection: what is in flight
/// between the guest and the host socket.
pub(crate) const TCP_BUFFER: usize = 256 << 10;
/// The most DNS lookups in flight; a query past it gets SERVFAIL.
const MAX_LOOKUPS: usize = 32;
/// How many addresses the stack remembers names for.
const MAX_NAMED_ADDRS: usize = 4096;
const MAX_NAMES_PER_ADDR: usize = 8;
/// The most frames the stack holds for the guest before smoltcp waits.
const OUT_FRAMES: usize = 256;
/// The most DNS replies waiting to go out; past it, a reply is dropped and
/// the guest asks again.
const DNS_OUT: usize = 32;
/// Rounds of polling and relaying per call to `poll`, while data moves
/// (each sends at most a packet per socket); the network thread polls again
/// at once if more is due.
const POLL_ROUNDS: usize = 64;

/// A host socket's readiness, as epoll reports it.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Ready {
    pub(crate) readable: bool,
    pub(crate) writable: bool,
    /// An error or hang-up: reading and writing will say which.
    pub(crate) closed: bool,
}

/// A host connection the stack relays a guest connection to.
pub(crate) trait HostStream: Read + Write {
    /// Once the non-blocking connect is done (the socket became writable
    /// or failed): whether it succeeded.
    fn connect_result(&self) -> io::Result<()>;
    /// Sends FIN: the guest will write no more.
    fn shutdown_write(&self);
    /// Makes dropping the stream send RST: the guest reset its end.
    fn reset_on_drop(&self);
}

/// Why a name did not resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LookupError {
    /// The name does not exist (or has no IPv4 address).
    NoName,
    /// The resolver failed; the guest may ask again.
    Failed,
}

/// The host side: sockets and name resolution.
pub(crate) trait HostNet {
    type Stream: HostStream;
    /// Starts a non-blocking connection to `to`; its readiness comes back
    /// through [`Stack::host_event`] with `id`.
    fn connect(&mut self, id: u64, to: SocketAddrV4) -> io::Result<Self::Stream>;
    /// Starts resolving `name`; the answer comes back through
    /// [`Stack::resolved`] with `id`.
    fn resolve(&mut self, id: u64, name: String);
    /// Asks `decider` about `request`, elsewhere than on this thread; the
    /// answer (None: the callback panicked or gave none) comes back through
    /// [`Stack::decided`] with `id`, unless its deadline passed first.
    fn decide(&mut self, id: u64, request: ConnectionRequest, decider: Decider);
}

/// What a guest frame is, to the stack.
enum Disposition {
    /// smoltcp handles it.
    Pass,
    Drop,
    /// A connection request from `guest` to `dst`.
    Syn {
        guest: SocketAddrV4,
        dst: SocketAddrV4,
    },
}

enum Phase {
    /// The host connection is being made; the guest's SYN waits.
    Connecting { syn: Vec<u8> },
    /// Relaying through this smoltcp socket.
    Open(SocketHandle),
}

/// What the log will say about a connection.
struct Meta {
    guest_port: u16,
    /// The host address and port.
    addr: Ipv4Addr,
    port: u16,
    names: Vec<String>,
    verdict: Verdict,
    decided_by: DecidedBy,
    /// When its SYN came.
    started: Instant,
    connected: bool,
    to_host: u64,
    to_guest: u64,
}

/// A connection waiting for the decider.
struct Pending {
    guest: SocketAddrV4,
    dst: SocketAddrV4,
    syn: Vec<u8>,
    deadline: Instant,
    default: Verdict,
    meta: Meta,
}

struct Conn<S> {
    meta: Meta,
    stream: S,
    guest: SocketAddrV4,
    dst: SocketAddrV4,
    phase: Phase,
    /// The host socket may have data, or room, since it last said not.
    readable: bool,
    writable: bool,
    /// The host closed its end; FIN passed on to the guest.
    host_eof: bool,
    /// The guest closed its end; FIN passed on to the host.
    host_shut: bool,
    /// Remove once smoltcp has sent what it owes (the RST after an abort).
    remove: bool,
}

/// The names the guest resolved through the VMM, by address, for the
/// policy's name rules, each until its DNS answer's TTL runs out. Bounded:
/// past [`MAX_NAMED_ADDRS`] addresses, expired ones go first, then the one
/// whose names expire soonest.
#[derive(Default)]
struct Names {
    by_addr: HashMap<Ipv4Addr, Vec<(String, Instant)>>,
}

impl Names {
    /// The names `addr` has at `now` (expired ones are forgotten).
    fn get(&mut self, addr: Ipv4Addr, now: Instant) -> Vec<String> {
        let Some(names) = self.by_addr.get_mut(&addr) else {
            return Vec::new();
        };
        names.retain(|(_, expires)| *expires > now);
        let out = names.iter().map(|(n, _)| n.clone()).collect();
        if names.is_empty() {
            self.by_addr.remove(&addr);
        }
        out
    }

    /// `addr` has the name `name` until `expires` (or later, if it had it
    /// longer already).
    fn add(&mut self, addr: Ipv4Addr, name: &str, expires: Instant, now: Instant) {
        let names = self.by_addr.entry(addr).or_default();
        names.retain(|(_, e)| *e > now);
        if let Some((_, e)) = names.iter_mut().find(|(n, _)| n == name) {
            *e = (*e).max(expires);
            return;
        }
        if names.len() == MAX_NAMES_PER_ADDR {
            let soonest = (0..names.len()).min_by_key(|&i| names[i].1).unwrap_or(0);
            names.remove(soonest);
        }
        names.push((name.to_owned(), expires));
        if self.by_addr.len() > MAX_NAMED_ADDRS {
            self.by_addr.retain(|_, names| {
                names.retain(|(_, e)| *e > now);
                !names.is_empty()
            });
            while self.by_addr.len() > MAX_NAMED_ADDRS {
                let soonest = self
                    .by_addr
                    .iter()
                    .filter(|(a, _)| **a != addr)
                    .min_by_key(|(_, names)| names.iter().map(|(_, e)| *e).max())
                    .map(|(a, _)| *a);
                match soonest {
                    Some(a) => self.by_addr.remove(&a),
                    None => break,
                };
            }
        }
    }
}

/// smoltcp's view of the link: frames from the guest to take, frames for
/// the guest it made.
struct Phy {
    rx: VecDeque<Vec<u8>>,
    tx: VecDeque<Vec<u8>>,
    checksum: ChecksumCapabilities,
}

struct RxToken(Vec<u8>);
struct TxToken<'a>(&'a mut VecDeque<Vec<u8>>);

impl phy::RxToken for RxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl phy::TxToken for TxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut frame = vec![0; len];
        let r = f(&mut frame);
        self.0.push_back(frame);
        r
    }
}

impl phy::Device for Phy {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken<'a>;

    fn receive(&mut self, _: Instant) -> Option<(RxToken, TxToken<'_>)> {
        let frame = self.rx.pop_front()?;
        Some((RxToken(frame), TxToken(&mut self.tx)))
    }

    fn transmit(&mut self, _: Instant) -> Option<TxToken<'_>> {
        (self.tx.len() < OUT_FRAMES).then_some(TxToken(&mut self.tx))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = MAX_FRAME;
        caps.checksum = self.checksum.clone();
        caps
    }
}

/// How a stack is set up; the defaults are production's.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Options {
    /// Off only for the fuzzer, so that its mutated frames get past the
    /// checksums.
    pub(crate) verify_checksums: bool,
    /// Each direction's buffer of a relayed connection.
    pub(crate) tcp_buffer: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self { verify_checksums: true, tcp_buffer: TCP_BUFFER }
    }
}

/// The guest's network stack.
pub(crate) struct Stack<H: HostNet> {
    iface: Interface,
    phy: Phy,
    sockets: SocketSet<'static>,
    dns: SocketHandle,
    conns: HashMap<u64, Conn<H::Stream>>,
    /// Connections waiting for the decider.
    deciding: HashMap<u64, Pending>,
    /// Each connection, connecting or waiting for the decider, by (guest
    /// end, destination).
    tuples: HashMap<(SocketAddrV4, SocketAddrV4), u64>,
    /// Each DNS lookup in flight: who asked, and the query.
    lookups: HashMap<u64, (IpEndpoint, dns::Query)>,
    /// DNS replies to send, in order. The first is in the DNS socket when
    /// `dns_in_socket`: smoltcp holds at most one at a time.
    dns_out: VecDeque<(IpEndpoint, Vec<u8>)>,
    dns_in_socket: bool,
    names: Names,
    shared: Arc<Shared>,
    host: H,
    next_id: u64,
    options: Options,
    /// The time of the last poll or [`set_now`](Self::set_now).
    now: Instant,
    /// The wall clock at `epoch`, in microseconds since the Unix epoch, for
    /// the log's start times.
    wall_epoch: i64,
    epoch: Instant,
}

impl<H: HostNet> Stack<H> {
    /// A stack applying `shared`'s policy, host entries and decider, and
    /// logging to it; its TCP sequence numbers and ports from `seed`.
    pub(crate) fn new(
        shared: Arc<Shared>,
        host: H,
        seed: u64,
        options: Options,
        now: Instant,
    ) -> Self {
        let checksum = if options.verify_checksums {
            ChecksumCapabilities::default()
        } else {
            let mut c = ChecksumCapabilities::ignored();
            c.ipv4 = Checksum::Tx;
            c.udp = Checksum::Tx;
            c.tcp = Checksum::Tx;
            c.icmpv4 = Checksum::Tx;
            c
        };
        let mut phy = Phy { rx: VecDeque::new(), tx: VecDeque::new(), checksum };
        let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(GATEWAY_MAC)));
        config.random_seed = seed;
        let mut iface = Interface::new(config, &mut phy, now);
        iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(IpAddress::Ipv4(GATEWAY_ADDR), PREFIX_LEN))
                .expect("one address");
        });
        // AnyIP with a default route through our own address: the
        // interface takes packets for every destination, which is what
        // lets it terminate the guest's connections to any host.
        iface.set_any_ip(true);
        iface.routes_mut().add_default_ipv4_route(GATEWAY_ADDR).expect("one route");

        let mut sockets = SocketSet::new(Vec::new());
        let mut dns = udp::Socket::new(
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 32], vec![0; 16 << 10]),
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 32], vec![0; 16 << 10]),
        );
        dns.bind(IpListenEndpoint { addr: Some(IpAddress::Ipv4(GATEWAY_ADDR)), port: 53 })
            .expect("bind the DNS server");
        let dns = sockets.add(dns);
        Self {
            iface,
            phy,
            sockets,
            dns,
            conns: HashMap::new(),
            deciding: HashMap::new(),
            tuples: HashMap::new(),
            lookups: HashMap::new(),
            dns_out: VecDeque::new(),
            dns_in_socket: false,
            names: Names::default(),
            shared,
            host,
            next_id: 0,
            options,
            now,
            wall_epoch: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_micros() as i64),
            epoch: now,
        }
    }

    /// `at`, by the wall clock, in microseconds since the Unix epoch.
    fn wall_micros(&self, at: Instant) -> i64 {
        self.wall_epoch + (at.total_micros() - self.epoch.total_micros())
    }

    /// Sets the stack's clock, for what happens before the next poll
    /// (frames from the guest, answers): the network thread calls it when
    /// it wakes, so that a SYN after a long idle sees the real time.
    pub(crate) fn set_now(&mut self, now: Instant) {
        self.now = self.now.max(now);
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub(crate) fn host(&self) -> &H {
        &self.host
    }

    #[cfg(any(test, feature = "fuzzing"))]
    pub(crate) fn host_mut(&mut self) -> &mut H {
        &mut self.host
    }

    /// The connections' IDs, for tests and the fuzzer.
    #[cfg(any(test, feature = "fuzzing"))]
    pub(crate) fn connection_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self.conns.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// The IDs of the connections waiting for the decider.
    #[cfg(any(test, feature = "fuzzing"))]
    pub(crate) fn deciding_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self.deciding.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// The connection (connecting, open or waiting for the decider) from
    /// `guest` to `dst`, if any.
    #[cfg(any(test, feature = "fuzzing"))]
    pub(crate) fn connection_id(&self, guest: SocketAddrV4, dst: SocketAddrV4) -> Option<u64> {
        self.tuples.get(&(guest, dst)).copied()
    }

    /// A frame from the guest.
    pub(crate) fn guest_frame(&mut self, frame: Vec<u8>) {
        match self.inspect(&frame) {
            Disposition::Pass => self.phy.rx.push_back(frame),
            Disposition::Drop => {}
            Disposition::Syn { guest, dst } => self.syn(guest, dst, frame),
        }
    }

    /// The next frame for the guest.
    pub(crate) fn take_frame(&mut self) -> Option<Vec<u8>> {
        self.phy.tx.pop_front()
    }

    /// Whether frames for the guest are waiting.
    pub(crate) fn has_frames(&self) -> bool {
        !self.phy.tx.is_empty()
    }

    /// Looks at a guest frame before smoltcp does: what only the VMM
    /// decides (connections, spoofing, pings to elsewhere) is taken out.
    fn inspect(&self, frame: &[u8]) -> Disposition {
        let Ok(eth) = EthernetFrame::new_checked(frame) else {
            return Disposition::Drop;
        };
        match eth.ethertype() {
            EthernetProtocol::Arp => return Disposition::Pass,
            EthernetProtocol::Ipv4 => {}
            // IPv6 and anything else: the guest's link is IPv4 only.
            _ => return Disposition::Drop,
        }
        let Ok(ip) = Ipv4Packet::new_checked(eth.payload()) else {
            return Disposition::Drop;
        };
        // Only the guest's address, unfragmented (smoltcp is built
        // without reassembly; the guest's MTU fits any packet anyway).
        if ip.src_addr() != GUEST_ADDR || ip.more_frags() || ip.frag_offset() != 0 {
            return Disposition::Drop;
        }
        let dst = ip.dst_addr();
        match ip.next_header() {
            // Echo replies from elsewhere would claim the internet answers.
            IpProtocol::Icmp if dst != GATEWAY_ADDR => Disposition::Drop,
            IpProtocol::Tcp => {
                let Ok(tcp) = TcpPacket::new_checked(ip.payload()) else {
                    return Disposition::Drop;
                };
                if !tcp.syn() || tcp.ack() {
                    return Disposition::Pass;
                }
                if self.options.verify_checksums
                    && !tcp.verify_checksum(&ip.src_addr().into(), &dst.into())
                {
                    return Disposition::Drop;
                }
                // Not a host to connect to: smoltcp drops or refuses it.
                let subnet = IpCidr::new(IpAddress::Ipv4(GATEWAY_ADDR), PREFIX_LEN);
                if dst.is_broadcast()
                    || dst.is_multicast()
                    || dst.is_unspecified()
                    || (subnet.contains_addr(&IpAddress::Ipv4(dst)) && dst != GATEWAY_ADDR)
                    || tcp.dst_port() == 0
                {
                    return Disposition::Pass;
                }
                Disposition::Syn {
                    guest: SocketAddrV4::new(ip.src_addr(), tcp.src_port()),
                    dst: SocketAddrV4::new(dst, tcp.dst_port()),
                }
            }
            _ => Disposition::Pass,
        }
    }

    /// A connection request: the host connection starts if the policy
    /// allows it, and the SYN waits for it; if an ask rule matches, the SYN
    /// waits for the decider first.
    fn syn(&mut self, guest: SocketAddrV4, dst: SocketAddrV4, frame: Vec<u8>) {
        if let Some(id) = self.tuples.get(&(guest, dst)) {
            // A retransmission: dropped while deciding or connecting (the
            // replay will come), else smoltcp's to answer.
            if let Some(Phase::Open(_)) = self.conns.get(id).map(|c| &c.phase) {
                self.phy.rx.push_back(frame);
            }
            return;
        }
        let host = host_addr(*dst.ip());
        let learnt = self.names.get(host, self.now);
        let mut names = self.shared.hosts().names_for(host);
        let (matched, rule) = {
            let policy = self.shared.policy();
            let matched = policy.check(&Destination {
                addr: host.into(),
                port: dst.port(),
                names: &learnt,
                host_names: &names,
            });
            let rule = matched.rule.map(|i| (i, policy.rules()[i].clone()));
            (matched, rule)
        };
        for name in learnt {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        let by_rule = match rule {
            Some((index, rule)) => DecidedBy::Rule { index, rule },
            None => DecidedBy::NoRule,
        };
        let meta = Meta {
            guest_port: guest.port(),
            addr: host,
            port: dst.port(),
            names,
            verdict: Verdict::Deny,
            decided_by: DecidedBy::NoRule,
            started: self.now,
            connected: false,
            to_host: 0,
            to_guest: 0,
        };
        if matched.action == Action::Deny {
            return self.refuse(frame, meta, by_rule);
        }
        if self.tuples.len() >= MAX_CONNECTIONS {
            return self.refuse(frame, meta, DecidedBy::ConnectionLimit);
        }
        if matched.action == Action::Allow {
            return self.connect(guest, dst, frame, meta, by_rule);
        }
        let Some(decider) = self.shared.decider() else {
            return self.refuse(frame, meta, DecidedBy::NoCallback);
        };
        let id = self.next_id();
        bump(&self.shared.counters.decisions);
        let request = ConnectionRequest {
            guest_port: guest.port(),
            addr: host,
            port: dst.port(),
            names: meta.names.clone(),
            rule: matched.rule.unwrap_or(0),
        };
        let deadline = self.now + Duration::from_micros(decider.timeout().as_micros() as u64);
        let default = decider.default_verdict();
        self.deciding.insert(id, Pending { guest, dst, syn: frame, deadline, default, meta });
        self.tuples.insert((guest, dst), id);
        self.host.decide(id, request, decider);
    }

    /// Refuses a connection: its SYN goes to smoltcp, which no socket
    /// listens for, so it answers RST.
    fn refuse(&mut self, syn: Vec<u8>, mut meta: Meta, by: DecidedBy) {
        self.phy.rx.push_back(syn);
        meta.verdict = Verdict::Deny;
        meta.decided_by = by;
        bump(&self.shared.counters.connections_denied);
        self.log(meta);
    }

    /// Starts the host connection for an allowed connection.
    fn connect(
        &mut self,
        guest: SocketAddrV4,
        dst: SocketAddrV4,
        syn: Vec<u8>,
        mut meta: Meta,
        by: DecidedBy,
    ) {
        meta.verdict = Verdict::Allow;
        meta.decided_by = by;
        bump(&self.shared.counters.connections_allowed);
        let id = self.next_id();
        match self.host.connect(id, SocketAddrV4::new(meta.addr, meta.port)) {
            Ok(stream) => {
                self.conns.insert(
                    id,
                    Conn {
                        meta,
                        stream,
                        guest,
                        dst,
                        phase: Phase::Connecting { syn },
                        readable: false,
                        writable: false,
                        host_eof: false,
                        host_shut: false,
                        remove: false,
                    },
                );
                self.tuples.insert((guest, dst), id);
                self.count_open();
            }
            Err(_) => {
                self.phy.rx.push_back(syn);
                bump(&self.shared.counters.connections_failed);
                self.log(meta);
            }
        }
    }

    /// The decider's answer about connection `id` (None: it panicked or
    /// gave none, and the default applies). Ignored if the connection's
    /// deadline passed first.
    pub(crate) fn decided(&mut self, id: u64, answer: Option<Verdict>) {
        let Some(pending) = self.deciding.remove(&id) else {
            return;
        };
        self.tuples.remove(&(pending.guest, pending.dst));
        match answer {
            Some(verdict) => self.settle(pending, verdict, DecidedBy::Callback),
            None => {
                bump(&self.shared.counters.decisions_failed);
                let default = pending.default;
                self.settle(pending, default, DecidedBy::CallbackFailed);
            }
        }
    }

    fn settle(&mut self, p: Pending, verdict: Verdict, by: DecidedBy) {
        match verdict {
            Verdict::Allow => self.connect(p.guest, p.dst, p.syn, p.meta, by),
            Verdict::Deny => self.refuse(p.syn, p.meta, by),
        }
    }

    /// Applies the default to the connections whose decider ran out of
    /// time.
    fn expire_decisions(&mut self) {
        let now = self.now;
        let mut due: Vec<u64> =
            self.deciding.iter().filter(|(_, p)| p.deadline <= now).map(|(&id, _)| id).collect();
        due.sort_unstable();
        for id in due {
            if let Some(p) = self.deciding.remove(&id) {
                self.tuples.remove(&(p.guest, p.dst));
                bump(&self.shared.counters.decisions_timed_out);
                let default = p.default;
                self.settle(p, default, DecidedBy::CallbackTimeout);
            }
        }
    }

    /// When the next decider deadline is, from `now`.
    pub(crate) fn decision_delay(&self, now: Instant) -> Option<Duration> {
        self.deciding
            .values()
            .map(|p| if p.deadline > now { p.deadline - now } else { Duration::ZERO })
            .min()
    }

    fn count_open(&self) {
        self.shared.counters.connections_open.store(self.conns.len() as u64, Ordering::Relaxed);
    }

    /// Logs a connection that ended.
    fn log(&self, meta: Meta) {
        let wall = self.wall_micros(meta.started);
        let started = if wall >= 0 {
            UNIX_EPOCH + std::time::Duration::from_micros(wall as u64)
        } else {
            UNIX_EPOCH
        };
        let lasted = (self.now.total_micros() - meta.started.total_micros()).max(0);
        self.shared.record(ConnectionRecord {
            guest_port: meta.guest_port,
            addr: meta.addr,
            port: meta.port,
            names: meta.names,
            verdict: meta.verdict,
            decided_by: meta.decided_by,
            connected: meta.connected,
            bytes_to_host: meta.to_host,
            bytes_to_guest: meta.to_guest,
            started,
            duration: std::time::Duration::from_micros(lasted as u64),
        });
    }

    /// A host socket's readiness changed.
    pub(crate) fn host_event(&mut self, id: u64, ready: Ready) {
        let Some(conn) = self.conns.get_mut(&id) else {
            return;
        };
        conn.readable |= ready.readable || ready.closed;
        conn.writable |= ready.writable || ready.closed;
        let done = ready.writable || ready.closed;
        let Phase::Connecting { syn } = &mut conn.phase else {
            return;
        };
        if !done {
            return;
        }
        let syn = std::mem::take(syn);
        let connected = conn.stream.connect_result().is_ok();
        conn.meta.connected = connected;
        let dst = conn.dst;
        // The replayed SYN goes first, before any frame that came after.
        self.phy.rx.push_front(syn);
        if !connected {
            // With no socket listening, smoltcp refuses the replayed SYN.
            bump(&self.shared.counters.connections_failed);
            self.forget(id);
            return;
        }
        let mut socket = self.new_socket();
        let listen = IpListenEndpoint { addr: Some(IpAddress::Ipv4(*dst.ip())), port: dst.port() };
        if socket.listen(listen).is_err() {
            self.forget(id);
            return;
        }
        let handle = self.sockets.add(socket);
        if let Some(conn) = self.conns.get_mut(&id) {
            conn.phase = Phase::Open(handle);
        }
    }

    /// A smoltcp socket for a relayed connection.
    fn new_socket(&self) -> tcp::Socket<'static> {
        let buffer = self.options.tcp_buffer;
        let mut socket = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0; buffer]),
            tcp::SocketBuffer::new(vec![0; buffer]),
        );
        socket.set_nagle_enabled(false);
        socket.set_ack_delay(None);
        socket
    }

    /// The answer to DNS lookup `id`.
    pub(crate) fn resolved(&mut self, id: u64, result: Result<Vec<Ipv4Addr>, LookupError>) {
        let Some((endpoint, query)) = self.lookups.remove(&id) else {
            return;
        };
        let ttl = self.dns_ttl();
        let reply = match result {
            Ok(addrs) => {
                let expires = self.now + Duration::from_secs(u64::from(ttl));
                for addr in &addrs {
                    self.names.add(*addr, &query.name, expires, self.now);
                }
                dns::reply(&query, Rcode::NoError, &addrs, ttl)
            }
            Err(LookupError::NoName) => dns::reply(&query, Rcode::NxDomain, &[], 0),
            Err(LookupError::Failed) => dns::reply(&query, Rcode::ServFail, &[], 0),
        };
        self.send_dns(&reply, endpoint);
    }

    /// The TTL of the DNS server's answers, in seconds.
    fn dns_ttl(&self) -> u32 {
        u32::try_from(self.shared.dns_ttl.as_secs()).unwrap_or(u32::MAX)
    }

    /// When the stack next needs [`poll`](Self::poll), if nothing else
    /// happens first: smoltcp's timers, and the decider's deadlines.
    pub(crate) fn poll_delay(&mut self, now: Instant) -> Option<Duration> {
        let smoltcp = self.iface.poll_delay(now, &self.sockets);
        match (smoltcp, self.decision_delay(now)) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Lets smoltcp take the guest's frames and make its own, answers DNS
    /// queries, and relays data between guest and host connections, until
    /// nothing moves (or a few rounds have passed).
    pub(crate) fn poll(&mut self, now: Instant) {
        self.set_now(now);
        let now = self.now;
        self.expire_decisions();
        for _ in 0..POLL_ROUNDS {
            self.iface.poll(now, &mut self.phy, &mut self.sockets);
            self.flow_dns();
            // What these queue (replies, relayed data, closes) goes out on
            // the next round's poll; and smoltcp sends one packet per socket
            // a round, so go on while it has more to send at once.
            let answered = self.serve_dns();
            let relayed = self.relay();
            let sending = self.phy.tx.len() < OUT_FRAMES
                && self.iface.poll_delay(now, &self.sockets) == Some(Duration::ZERO);
            if !answered && !relayed && !sending {
                break;
            }
        }
    }

    /// Answers the DNS queries received; returns whether there were any.
    fn serve_dns(&mut self) -> bool {
        let mut any = false;
        loop {
            let socket = self.sockets.get_mut::<udp::Socket>(self.dns);
            let Ok((data, meta)) = socket.recv() else {
                return any;
            };
            let (data, endpoint) = (data.to_vec(), meta.endpoint);
            self.dns_query(&data, endpoint);
            any = true;
        }
    }

    fn dns_query(&mut self, data: &[u8], from: IpEndpoint) {
        let query = match dns::parse_query(data) {
            Ok(q) => q,
            Err(invalid) => {
                if let Some(reply) = dns::error_reply(invalid) {
                    self.send_dns(&reply, from);
                }
                return;
            }
        };
        bump(&self.shared.counters.dns_queries);
        let resolves = self.shared.policy().resolves(&query.name);
        let ttl = self.dns_ttl();
        let reply = if !resolves {
            bump(&self.shared.counters.dns_refused);
            dns::reply(&query, Rcode::NxDomain, &[], 0)
        } else if query.qclass != dns::CLASS_IN {
            dns::reply(&query, Rcode::NotImp, &[], 0)
        } else if query.qtype != dns::TYPE_A {
            // The guest's network is IPv4 only: other types (AAAA, ...)
            // have no data, so resolvers move on to A at once.
            dns::reply(&query, Rcode::NoError, &[], ttl)
        } else if let Some(addrs) = self.shared.hosts().guest_addrs(&query.name) {
            // The caller's entry, never the host's resolver; the policy
            // matches its names through the entries, not as learnt names.
            dns::reply(&query, Rcode::NoError, &addrs, ttl)
        } else if self.lookups.len() >= MAX_LOOKUPS {
            dns::reply(&query, Rcode::ServFail, &[], 0)
        } else {
            let id = self.next_id();
            let name = query.name.clone();
            self.lookups.insert(id, (from, query));
            self.host.resolve(id, name);
            return;
        };
        self.send_dns(&reply, from);
    }

    fn send_dns(&mut self, reply: &[u8], to: IpEndpoint) {
        // A full queue drops the reply; the guest asks again.
        if self.dns_out.len() < DNS_OUT {
            self.dns_out.push_back((to, reply.to_vec()));
            self.flow_dns();
        }
    }

    /// Moves the next DNS reply into the DNS socket once smoltcp has sent
    /// the one before.
    fn flow_dns(&mut self) {
        let socket = self.sockets.get_mut::<udp::Socket>(self.dns);
        if socket.send_queue() != 0 {
            return;
        }
        if std::mem::take(&mut self.dns_in_socket) {
            self.dns_out.pop_front();
        }
        while let Some((to, reply)) = self.dns_out.front() {
            if socket.send_slice(reply, *to).is_ok() {
                self.dns_in_socket = true;
                return;
            }
            // Cannot be sent (larger than the socket's buffer): dropped.
            self.dns_out.pop_front();
        }
    }

    /// Moves data between each open connection's smoltcp socket and its
    /// host socket, passes closes on, and removes finished connections.
    /// Returns whether anything moved.
    fn relay(&mut self) -> bool {
        let mut progress = false;
        let mut finished = Vec::new();
        for (&id, conn) in &mut self.conns {
            let Phase::Open(handle) = conn.phase else {
                continue;
            };
            let socket = self.sockets.get_mut::<tcp::Socket>(handle);
            if conn.remove {
                // Aborted: removed once its RST has gone out (smoltcp then
                // forgets the remote end), not before: a full queue for the
                // guest holds the RST back, and removing the socket would
                // leave the guest's end open for good.
                if socket.remote_endpoint().is_none() {
                    finished.push(id);
                }
                continue;
            }
            match socket.state() {
                // The replayed SYN did not reach the socket: never use a
                // listener another connection's SYN could reach.
                tcp::State::Listen => {
                    finished.push(id);
                    continue;
                }
                // Both ends closed (not reset: a reset empties the receive
                // buffer); the guest's last data, and its FIN, go to the
                // host before the connection does. In TIME-WAIT, smoltcp's
                // timer bounds the wait for a host that never takes it; in
                // CLOSED (after LAST-ACK), the host stream's own failure or
                // close does.
                tcp::State::Closed | tcp::State::TimeWait
                    if !conn.host_shut
                        && (socket.state() == tcp::State::TimeWait || socket.recv_queue() > 0) =>
                {
                    progress |= relay_one(conn, socket, &self.shared);
                    if conn.host_shut {
                        finished.push(id);
                    }
                    continue;
                }
                tcp::State::Closed | tcp::State::TimeWait => {
                    if !(conn.host_eof && conn.host_shut) {
                        // The guest reset the connection: so do we.
                        conn.stream.reset_on_drop();
                    }
                    finished.push(id);
                    continue;
                }
                _ => {}
            }
            progress |= relay_one(conn, socket, &self.shared);
        }
        for id in finished {
            self.forget(id);
            progress = true;
        }
        progress
    }

    /// Removes connection `id` and its smoltcp socket, closing the host
    /// socket.
    fn forget(&mut self, id: u64) {
        if let Some(conn) = self.conns.remove(&id) {
            self.tuples.remove(&(conn.guest, conn.dst));
            if let Phase::Open(handle) = conn.phase {
                self.sockets.remove(handle);
            }
            self.count_open();
            self.log(conn.meta);
        }
    }

    /// Checks the stack's bookkeeping, for tests and the fuzzer.
    #[cfg(any(test, feature = "fuzzing"))]
    pub(crate) fn check_invariants(&self) {
        assert!(self.conns.len() + self.deciding.len() <= MAX_CONNECTIONS);
        assert_eq!(self.conns.len() + self.deciding.len(), self.tuples.len());
        for (key, id) in &self.tuples {
            match (self.conns.get(id), self.deciding.get(id)) {
                (Some(conn), None) => assert_eq!((conn.guest, conn.dst), *key),
                (None, Some(p)) => assert_eq!((p.guest, p.dst), *key),
                _ => panic!("connection {id} is in neither table, or in both"),
            }
        }
        let counters = self.shared.counters.snapshot();
        assert_eq!(counters.connections_open, self.conns.len() as u64);
        assert!(counters.decisions_timed_out + counters.decisions_failed <= counters.decisions);
        let log = self.shared.log();
        assert!(log.len() <= log.capacity());
        drop(log);
        assert!(self.lookups.len() <= MAX_LOOKUPS);
        assert!(self.names.by_addr.len() <= MAX_NAMED_ADDRS);
        assert!(self.dns_out.len() <= DNS_OUT);
        let dns = self.sockets.get::<udp::Socket>(self.dns);
        assert!(!self.dns_in_socket || !self.dns_out.is_empty());
        assert!(dns.send_queue() == 0 || self.dns_in_socket);
        let open = self.conns.values().filter(|c| matches!(c.phase, Phase::Open(_))).count();
        // The DNS socket, and one per open connection.
        assert_eq!(self.sockets.iter().count(), open + 1);
        for frame in &self.phy.tx {
            assert!(frame.len() <= MAX_FRAME, "{} byte frame", frame.len());
        }
    }
}

/// The host address a guest destination stands for: the gateway is the
/// host's loopback; anything else is itself.
fn host_addr(dst: Ipv4Addr) -> Ipv4Addr {
    if dst == GATEWAY_ADDR { Ipv4Addr::LOCALHOST } else { dst }
}

/// Moves what can move between one connection's sockets, counting the
/// bytes; returns whether anything did.
fn relay_one<S: HostStream>(
    conn: &mut Conn<S>,
    socket: &mut tcp::Socket<'_>,
    shared: &Shared,
) -> bool {
    let (to_host, to_guest) = (conn.meta.to_host, conn.meta.to_guest);
    let progress = relay_data(conn, socket);
    let counters = &shared.counters;
    counters.bytes_to_host.fetch_add(conn.meta.to_host - to_host, Ordering::Relaxed);
    counters.bytes_to_guest.fetch_add(conn.meta.to_guest - to_guest, Ordering::Relaxed);
    progress
}

fn relay_data<S: HostStream>(conn: &mut Conn<S>, socket: &mut tcp::Socket<'_>) -> bool {
    let stream = &mut conn.stream;
    let mut progress = false;
    // Host to guest.
    while conn.readable && !conn.host_eof && socket.can_send() {
        let read = socket.send(|buf| match stream.read(buf) {
            Ok(n) => (n, Ok(n)),
            Err(e) => (0, Err(e)),
        });
        match read {
            Ok(Ok(0)) => {
                conn.host_eof = true;
                socket.close();
                progress = true;
            }
            Ok(Ok(n)) => {
                conn.meta.to_guest += n as u64;
                progress = true;
            }
            Ok(Err(e)) if e.kind() == io::ErrorKind::WouldBlock => conn.readable = false,
            Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
            Ok(Err(_)) => {
                socket.abort();
                conn.remove = true;
                return true;
            }
            Err(_) => break,
        }
    }
    // Data the host sends while the guest cannot take it yet (its handshake
    // is not finished, or the buffer is full) stays in the host socket: the
    // readable flag stays set until a read would block. A guest that resets
    // the connection closes the socket (state Closed, in `relay`).
    // Guest to host.
    while conn.writable && socket.can_recv() {
        let written = socket.recv(|buf| match stream.write(buf) {
            Ok(n) => (n, Ok(n)),
            Err(e) => (0, Err(e)),
        });
        match written {
            Ok(Ok(0)) => {
                socket.abort();
                conn.remove = true;
                return true;
            }
            Ok(Ok(n)) => {
                conn.meta.to_host += n as u64;
                progress = true;
            }
            Ok(Err(e)) if e.kind() == io::ErrorKind::WouldBlock => conn.writable = false,
            Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
            Ok(Err(_)) => {
                socket.abort();
                conn.remove = true;
                return true;
            }
            Err(_) => break,
        }
    }
    // The guest sent FIN and everything before it went to the host.
    let guest_closed = matches!(
        socket.state(),
        tcp::State::CloseWait
            | tcp::State::LastAck
            | tcp::State::Closing
            | tcp::State::TimeWait
            | tcp::State::Closed
    );
    if guest_closed && !conn.host_shut && socket.recv_queue() == 0 {
        stream.shutdown_write();
        conn.host_shut = true;
        progress = true;
    }
    progress
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::time::Duration as StdDuration;

    use smoltcp::wire::{
        ArpOperation, ArpPacket, ArpRepr, Icmpv4Packet, Icmpv4Repr, TcpControl, TcpRepr,
        TcpSeqNumber,
    };

    use super::*;
    use crate::net::fuzz::{FakeHost, frames};
    use crate::net::host::RealStreams;
    use crate::net::{EgressPolicy, HostPattern, Hosts, NetworkConfig, Ports};

    fn policy(p: EgressPolicy) -> Arc<Shared> {
        Shared::new(NetworkConfig::new(p)).unwrap()
    }

    /// The guest's side of the link, building frames as Linux would.
    struct Guest {
        now: Instant,
    }

    impl Guest {
        fn new() -> Self {
            Self { now: Instant::from_millis(0) }
        }

        fn tick(&mut self) -> Instant {
            self.now += Duration::from_millis(1);
            self.now
        }
    }

    fn arp_request() -> Vec<u8> {
        frames::arp(ArpRepr::EthernetIpv4 {
            operation: ArpOperation::Request,
            source_hardware_addr: EthernetAddress(GUEST_MAC),
            source_protocol_addr: GUEST_ADDR,
            target_hardware_addr: EthernetAddress([0; 6]),
            target_protocol_addr: GATEWAY_ADDR,
        })
    }

    /// The frames the stack sent: parsed as (TCP control, payload, ack) for
    /// TCP, and kept whole otherwise.
    fn tcp_out<H: HostNet>(stack: &mut Stack<H>) -> Vec<(TcpRepr<'static>, Vec<u8>)> {
        let mut out = Vec::new();
        while let Some(frame) = stack.take_frame() {
            if let Some((repr, payload)) = frames::parse_tcp(&frame) {
                out.push((repr, payload));
            }
        }
        out
    }

    fn syn(port: u16, dst: Ipv4Addr, dport: u16) -> Vec<u8> {
        frames::tcp(
            GUEST_ADDR,
            dst,
            TcpRepr {
                src_port: port,
                dst_port: dport,
                control: TcpControl::Syn,
                seq_number: TcpSeqNumber(1000),
                ack_number: None,
                window_len: 65535,
                window_scale: Some(7),
                max_seg_size: Some(1460),
                sack_permitted: false,
                sack_ranges: [None; 3],
                timestamp: None,
                payload: &[],
            },
        )
    }

    fn segment(
        port: u16,
        dst: SocketAddrV4,
        control: TcpControl,
        seq: i32,
        ack: TcpSeqNumber,
        payload: &[u8],
    ) -> Vec<u8> {
        frames::tcp(
            GUEST_ADDR,
            *dst.ip(),
            TcpRepr {
                src_port: port,
                dst_port: dst.port(),
                control,
                seq_number: TcpSeqNumber(1000 + seq),
                ack_number: Some(ack),
                window_len: 65535,
                window_scale: None,
                max_seg_size: None,
                sack_permitted: false,
                sack_ranges: [None; 3],
                timestamp: None,
                payload,
            },
        )
    }

    #[test]
    fn answers_arp_for_the_gateway_only_from_the_guest() {
        let mut g = Guest::new();
        let mut s = Stack::new(
            policy(EgressPolicy::deny_all()),
            FakeHost::default(),
            1,
            Options::default(),
            g.now,
        );
        s.guest_frame(arp_request());
        s.poll(g.tick());
        let reply = s.take_frame().expect("an ARP reply");
        let eth = EthernetFrame::new_checked(&reply[..]).unwrap();
        assert_eq!(eth.dst_addr(), EthernetAddress(GUEST_MAC));
        let arp = ArpRepr::parse(&ArpPacket::new_checked(eth.payload()).unwrap()).unwrap();
        let ArpRepr::EthernetIpv4 { operation, source_hardware_addr, source_protocol_addr, .. } =
            arp
        else {
            unreachable!()
        };
        assert_eq!(operation, ArpOperation::Reply);
        assert_eq!(source_hardware_addr, EthernetAddress(GATEWAY_MAC));
        assert_eq!(source_protocol_addr, GATEWAY_ADDR);
        assert!(s.take_frame().is_none());
    }

    #[test]
    fn denied_connections_are_refused_at_once() {
        let mut g = Guest::new();
        let host = FakeHost::default();
        let mut s =
            Stack::new(policy(EgressPolicy::deny_all()), host, 1, Options::default(), g.now);
        s.guest_frame(arp_request());
        s.guest_frame(syn(40000, Ipv4Addr::new(1, 1, 1, 1), 443));
        s.poll(g.tick());
        let out = tcp_out(&mut s);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0.control, TcpControl::Rst);
        assert_eq!(out[0].0.dst_port, 40000);
        assert!(s.host().connects.is_empty(), "no host connection tried");
        s.check_invariants();
    }

    #[test]
    fn failed_host_connections_are_refused_after_the_attempt() {
        let mut g = Guest::new();
        let host = FakeHost { refuse: true, ..Default::default() };
        let mut s =
            Stack::new(policy(EgressPolicy::allow_all()), host, 1, Options::default(), g.now);
        s.guest_frame(arp_request());
        s.guest_frame(syn(40000, Ipv4Addr::new(1, 1, 1, 1), 443));
        s.poll(g.tick());
        assert!(tcp_out(&mut s).is_empty(), "the SYN waits for the host");
        assert_eq!(s.host().connects, [SocketAddrV4::new(Ipv4Addr::new(1, 1, 1, 1), 443)]);
        // A retransmitted SYN while connecting is dropped.
        s.guest_frame(syn(40000, Ipv4Addr::new(1, 1, 1, 1), 443));
        s.poll(g.tick());
        assert!(tcp_out(&mut s).is_empty());
        s.host_event(0, Ready { readable: true, writable: true, closed: true });
        s.poll(g.tick());
        let out = tcp_out(&mut s);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0.control, TcpControl::Rst);
        s.check_invariants();
        assert!(s.connection_ids().is_empty());
    }

    #[test]
    fn the_connection_limit_refuses_more() {
        let mut g = Guest::new();
        let mut s = Stack::new(
            policy(EgressPolicy::allow_all()),
            FakeHost::default(),
            1,
            Options::default(),
            g.now,
        );
        s.guest_frame(arp_request());
        for port in 0..MAX_CONNECTIONS as u16 + 1 {
            s.guest_frame(syn(10000 + port, Ipv4Addr::new(1, 1, 1, 1), 443));
        }
        s.poll(g.tick());
        assert_eq!(s.connection_ids().len(), MAX_CONNECTIONS);
        let out = tcp_out(&mut s);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0.control, TcpControl::Rst);
        assert_eq!(out[0].0.dst_port, 10000 + MAX_CONNECTIONS as u16);
        s.check_invariants();
    }

    #[test]
    fn spoofed_fragmented_and_ipv6_frames_are_dropped() {
        let mut g = Guest::new();
        let mut s = Stack::new(
            policy(EgressPolicy::allow_all()),
            FakeHost::default(),
            1,
            Options::default(),
            g.now,
        );
        s.guest_frame(arp_request());
        s.poll(g.tick());
        s.take_frame();
        let mut spoofed = syn(1, Ipv4Addr::new(1, 1, 1, 1), 80);
        spoofed[14 + 12..14 + 16].copy_from_slice(&[10, 0, 2, 3]);
        s.guest_frame(spoofed);
        let mut fragment = syn(1, Ipv4Addr::new(1, 1, 1, 1), 80);
        fragment[14 + 6] |= 0x20; // more fragments
        s.guest_frame(fragment);
        let mut v6 = syn(1, Ipv4Addr::new(1, 1, 1, 1), 80);
        v6[12..14].copy_from_slice(&[0x86, 0xdd]);
        s.guest_frame(v6);
        let mut bad_sum = syn(1, Ipv4Addr::new(1, 1, 1, 1), 80);
        let last = bad_sum.len() - 1;
        bad_sum[last] ^= 0xff;
        s.guest_frame(bad_sum);
        s.poll(g.tick());
        assert!(s.take_frame().is_none());
        assert!(s.host().connects.is_empty());
    }

    #[test]
    fn pings_are_answered_for_the_gateway_only() {
        let mut g = Guest::new();
        let mut s = Stack::new(
            policy(EgressPolicy::allow_all()),
            FakeHost::default(),
            1,
            Options::default(),
            g.now,
        );
        s.guest_frame(arp_request());
        s.poll(g.tick());
        s.take_frame();
        let echo = |dst| {
            frames::icmp(
                GUEST_ADDR,
                dst,
                Icmpv4Repr::EchoRequest { ident: 1, seq_no: 1, data: b"ping" },
            )
        };
        s.guest_frame(echo(Ipv4Addr::new(8, 8, 8, 8)));
        s.poll(g.tick());
        assert!(s.take_frame().is_none());
        s.guest_frame(echo(GATEWAY_ADDR));
        s.poll(g.tick());
        let reply = s.take_frame().expect("an echo reply");
        let eth = EthernetFrame::new_checked(&reply[..]).unwrap();
        let ip = Ipv4Packet::new_checked(eth.payload()).unwrap();
        let icmp = Icmpv4Packet::new_checked(ip.payload()).unwrap();
        assert!(matches!(
            Icmpv4Repr::parse(&icmp, &ChecksumCapabilities::default()).unwrap(),
            Icmpv4Repr::EchoReply { .. }
        ));
    }

    /// DNS through the stack: allowed names are looked up and remembered
    /// for the policy's name rules; others are NXDOMAIN without a lookup.
    #[test]
    fn dns_answers_and_names_feed_the_policy() {
        let mut g = Guest::new();
        let p =
            EgressPolicy::deny_all().allow(crate::net::HostPattern::name("pypi.org").unwrap(), 443);
        let mut s = Stack::new(policy(p), FakeHost::default(), 1, Options::default(), g.now);
        s.guest_frame(arp_request());
        s.poll(g.tick());
        s.take_frame();
        let ask = |s: &mut Stack<FakeHost>, id, name: &str, qtype| {
            s.guest_frame(frames::udp(
                GUEST_ADDR,
                GATEWAY_ADDR,
                5353,
                53,
                &dns::query_bytes(id, name, qtype),
            ));
        };
        let answers = |s: &mut Stack<FakeHost>| {
            let mut out = Vec::new();
            while let Some(frame) = s.take_frame() {
                let (src, sport, payload) = frames::parse_udp(&frame).expect("UDP");
                assert_eq!((src, sport), (GATEWAY_ADDR, 53));
                out.push(dns::answers(&payload));
            }
            out
        };
        ask(&mut s, 1, "evil.test", dns::TYPE_A);
        ask(&mut s, 2, "PyPI.org", 28);
        s.poll(g.tick());
        assert_eq!(answers(&mut s), [(3, vec![]), (0, vec![])]);
        assert!(s.host().lookups.is_empty());
        ask(&mut s, 3, "pypi.org", dns::TYPE_A);
        s.poll(g.tick());
        assert!(answers(&mut s).is_empty(), "waiting for the lookup");
        let (id, name) = s.host_mut().lookups.pop().unwrap();
        assert_eq!(name, "pypi.org");
        let addr = Ipv4Addr::new(151, 101, 0, 223);
        s.resolved(id, Ok(vec![addr]));
        s.poll(g.tick());
        assert_eq!(answers(&mut s), [(0, vec![addr])]);
        // The name now allows its address, on the rule's port only.
        s.guest_frame(syn(40001, addr, 443));
        s.guest_frame(syn(40002, addr, 80));
        s.guest_frame(syn(40003, Ipv4Addr::new(151, 101, 0, 224), 443));
        s.poll(g.tick());
        assert_eq!(s.host().connects, [SocketAddrV4::new(addr, 443)]);
        let refused: Vec<u16> = tcp_out(&mut s).iter().map(|(r, _)| r.dst_port).collect();
        assert_eq!(refused, [40002, 40003]);
        // A failed lookup.
        ask(&mut s, 4, "pypi.org", dns::TYPE_A);
        s.poll(g.tick());
        let (id, _) = s.host_mut().lookups.pop().unwrap();
        s.resolved(id, Err(LookupError::Failed));
        s.poll(g.tick());
        assert_eq!(answers(&mut s), [(2, vec![])]);
        // DNS to anywhere else gets port unreachable.
        s.guest_frame(frames::udp(
            GUEST_ADDR,
            Ipv4Addr::new(8, 8, 8, 8),
            5353,
            53,
            &dns::query_bytes(5, "pypi.org", dns::TYPE_A),
        ));
        s.poll(g.tick());
        let frame = s.take_frame().expect("ICMP");
        assert!(frames::parse_udp(&frame).is_none());
        s.check_invariants();
    }

    /// A whole connection relayed to a real host socket on loopback,
    /// through the gateway address: handshake after the host's, data both
    /// ways, and FINs passed on.
    #[test]
    fn relays_a_connection_to_a_host_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let p = EgressPolicy::deny_all().allow(Ipv4Addr::LOCALHOST, port);
        let mut g = Guest::new();
        let mut s = Stack::new(policy(p), RealStreams::default(), 1, Options::default(), g.now);
        s.guest_frame(arp_request());
        s.poll(g.tick());
        s.take_frame();

        let dst = SocketAddrV4::new(GATEWAY_ADDR, port);
        s.guest_frame(syn(40000, GATEWAY_ADDR, port));
        s.poll(g.tick());
        assert!(tcp_out(&mut s).is_empty());
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(StdDuration::from_secs(5))).unwrap();
        // RealStreams has no epoll: tell the stack the socket is ready.
        let ids = s.connection_ids();
        let ready = Ready { readable: true, writable: true, closed: false };
        s.host_event(ids[0], ready);
        s.poll(g.tick());
        let out = tcp_out(&mut s);
        assert_eq!(out.len(), 1);
        let synack = &out[0].0;
        assert_eq!(synack.control, TcpControl::Syn);
        assert_eq!(synack.ack_number, Some(TcpSeqNumber(1001)));
        let their = synack.seq_number + 1;

        // Guest to host.
        s.guest_frame(segment(40000, dst, TcpControl::None, 1, their, b""));
        s.guest_frame(segment(40000, dst, TcpControl::Psh, 1, their, b"hello"));
        s.poll(g.tick());
        let mut buf = [0; 5];
        peer.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hello");

        // Host to guest.
        peer.write_all(b"world").unwrap();
        let mut data = Vec::new();
        for _ in 0..100 {
            s.host_event(ids[0], ready);
            s.poll(g.tick());
            for (repr, payload) in tcp_out(&mut s) {
                assert_eq!(repr.dst_port, 40000);
                data.extend(payload);
            }
            if data.len() >= 5 {
                break;
            }
            std::thread::sleep(StdDuration::from_millis(5));
        }
        assert_eq!(data, b"world");

        // The guest closes: the host reads EOF.
        s.guest_frame(segment(40000, dst, TcpControl::Fin, 6, their + 5, b""));
        s.poll(g.tick());
        assert_eq!(peer.read(&mut buf).unwrap(), 0);
        // The host closes: the guest gets FIN, and the connection goes.
        drop(peer);
        let mut fin = None;
        for _ in 0..100 {
            s.host_event(ids[0], ready);
            s.poll(g.tick());
            if let Some((repr, _)) =
                tcp_out(&mut s).into_iter().find(|(r, _)| r.control == TcpControl::Fin)
            {
                fin = Some(repr);
                break;
            }
            std::thread::sleep(StdDuration::from_millis(5));
        }
        let fin = fin.expect("FIN to the guest");
        s.guest_frame(segment(40000, dst, TcpControl::None, 7, fin.seq_number + 1, b""));
        s.poll(g.tick());
        s.poll(g.tick());
        assert!(s.connection_ids().is_empty());
        s.check_invariants();
    }

    /// The host sends, and closes, before the guest has finished its
    /// handshake: the data and the FIN wait for the guest, whatever the
    /// state of its connection when the host socket became readable.
    #[test]
    fn host_data_before_the_guest_handshake_completes() {
        for early_event in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let p = EgressPolicy::deny_all().allow(Ipv4Addr::LOCALHOST, port);
            let mut g = Guest::new();
            let mut s = Stack::new(policy(p), RealStreams::default(), 1, Options::default(), g.now);
            s.guest_frame(arp_request());
            s.guest_frame(syn(40000, GATEWAY_ADDR, port));
            s.poll(g.tick());
            s.take_frame();
            let (mut peer, _) = listener.accept().unwrap();
            peer.write_all(b"early").unwrap();
            drop(peer);
            std::thread::sleep(StdDuration::from_millis(20));
            let ids = s.connection_ids();
            let ready = Ready { readable: true, writable: true, closed: true };
            // The connect completes; the guest gets SYN-ACK.
            s.host_event(ids[0], ready);
            s.poll(g.tick());
            if early_event {
                // Another readiness edge while the guest is in SYN-RECEIVED.
                s.host_event(ids[0], ready);
                s.poll(g.tick());
            }
            let out = tcp_out(&mut s);
            let synack = out.iter().find(|(r, _)| r.control == TcpControl::Syn).unwrap();
            let their = synack.0.seq_number + 1;
            let dst = SocketAddrV4::new(GATEWAY_ADDR, port);
            s.guest_frame(segment(40000, dst, TcpControl::None, 1, their, b""));
            s.poll(g.tick());
            let out = tcp_out(&mut s);
            let data: Vec<u8> = out.iter().flat_map(|(_, p)| p.clone()).collect();
            assert_eq!(data, b"early", "early_event {early_event}: {out:?}");
            assert!(
                out.iter().any(|(r, _)| r.control == TcpControl::Fin),
                "early_event {early_event}: {out:?}"
            );
        }
    }

    #[test]
    fn a_policy_change_applies_to_new_connections() {
        let shared = policy(EgressPolicy::deny_all());
        let mut g = Guest::new();
        let mut s = Stack::new(shared.clone(), FakeHost::default(), 1, Options::default(), g.now);
        s.guest_frame(arp_request());
        s.guest_frame(syn(1, Ipv4Addr::new(9, 9, 9, 9), 53));
        s.poll(g.tick());
        assert!(s.host().connects.is_empty());
        *shared.policy.write().unwrap() = EgressPolicy::allow_all();
        s.guest_frame(syn(2, Ipv4Addr::new(9, 9, 9, 9), 53));
        s.poll(g.tick());
        assert_eq!(s.host().connects.len(), 1);
    }

    #[test]
    fn names_are_bounded() {
        let t = |s: i64| Instant::from_secs(s);
        let mut names = Names::default();
        // Each address learnt a second later, expiring a minute after.
        for i in 0..MAX_NAMED_ADDRS as u32 + 10 {
            names.add(Ipv4Addr::from(i), "a.test", t(60 + i64::from(i)), t(0));
        }
        assert_eq!(names.by_addr.len(), MAX_NAMED_ADDRS);
        assert!(names.get(Ipv4Addr::from(0), t(0)).is_empty(), "the one expiring soonest went");
        assert_eq!(names.get(Ipv4Addr::from(10), t(0)), ["a.test"]);
        for i in 0..20 {
            names.add(Ipv4Addr::from(u32::MAX), &format!("n{i}.test"), t(100 + i), t(0));
        }
        let kept = names.get(Ipv4Addr::from(u32::MAX), t(0));
        assert_eq!(kept.len(), MAX_NAMES_PER_ADDR);
        assert_eq!(kept[0], "n12.test", "the names expiring soonest went");
        // Expired addresses make room before live ones go.
        let now = t(10_000);
        names.add(Ipv4Addr::new(9, 9, 9, 9), "b.test", t(10_060), now);
        assert_eq!(names.by_addr.len(), 1);
    }

    /// A name learnt from DNS allows its address for as long as the TTL
    /// the guest was given, then no more; asking again extends it; a
    /// connection opened before it expired stays open.
    #[test]
    fn learnt_names_expire_with_their_ttl() {
        let p = EgressPolicy::deny_all().allow(HostPattern::name("pypi.org").unwrap(), 443);
        let shared =
            Shared::new(NetworkConfig::new(p).with_dns_ttl(std::time::Duration::from_secs(30)))
                .unwrap();
        let mut g = Guest::new();
        let mut s = Stack::new(shared.clone(), FakeHost::default(), 1, Options::default(), g.now);
        s.guest_frame(arp_request());
        s.poll(g.tick());
        s.take_frame();
        let addr = Ipv4Addr::new(151, 101, 0, 223);
        let resolve = |s: &mut Stack<FakeHost>, now| {
            s.set_now(now);
            s.guest_frame(frames::udp(
                GUEST_ADDR,
                GATEWAY_ADDR,
                5353,
                53,
                &dns::query_bytes(1, "pypi.org", dns::TYPE_A),
            ));
            s.poll(now);
            let (id, _) = s.host_mut().lookups.pop().unwrap();
            s.resolved(id, Ok(vec![addr]));
            s.poll(now);
            let frame = s.take_frame().expect("the answer");
            let (_, _, payload) = frames::parse_udp(&frame).unwrap();
            // The TTL is the answer's last record's TTL field.
            let ttl = &payload[payload.len() - 10..payload.len() - 6];
            assert_eq!(u32::from_be_bytes(ttl.try_into().unwrap()), 30);
        };
        let connects = |s: &mut Stack<FakeHost>, port, now| {
            let before = s.host().connects.len();
            s.set_now(now);
            s.guest_frame(syn(port, addr, 443));
            s.poll(now);
            tcp_out(s);
            s.host().connects.len() > before
        };
        resolve(&mut s, Instant::from_secs(1));
        assert!(connects(&mut s, 40001, Instant::from_secs(30)));
        // 31 s: 30 s after the answer, expired.
        assert!(!connects(&mut s, 40002, Instant::from_secs(31)));
        assert_eq!(s.connection_ids().len(), 1, "the connection made in time is still there");
        resolve(&mut s, Instant::from_secs(40));
        assert!(connects(&mut s, 40003, Instant::from_secs(69)));
        assert!(!connects(&mut s, 40004, Instant::from_secs(70)));
        let log = shared.log().take();
        let denied: Vec<_> = log.iter().filter(|r| r.verdict == Verdict::Deny).collect();
        assert_eq!(denied.len(), 2);
        assert!(denied.iter().all(|r| r.names.is_empty() && r.decided_by == DecidedBy::NoRule));
        s.check_invariants();
    }

    /// The caller's host entries: answered without asking the host's
    /// resolver, only for names the policy could allow, the host's
    /// loopback as the gateway; a name rule then reaches the entry's
    /// loopback address.
    #[test]
    fn host_entries_answer_dns_and_feed_the_policy() {
        let p = EgressPolicy::deny_all()
            .allow(HostPattern::name("mock.test").unwrap(), 8080)
            .allow(HostPattern::name("*.example").unwrap(), Ports::ANY);
        let hosts = Hosts::new()
            .with("mock.test", Ipv4Addr::LOCALHOST)
            .unwrap()
            .with("hidden.test", Ipv4Addr::new(1, 2, 3, 4))
            .unwrap();
        let shared = Shared::new(NetworkConfig::new(p).with_hosts(hosts)).unwrap();
        let mut g = Guest::new();
        let mut s = Stack::new(shared.clone(), FakeHost::default(), 1, Options::default(), g.now);
        s.guest_frame(arp_request());
        s.poll(g.tick());
        s.take_frame();
        let ask = |s: &mut Stack<FakeHost>, name: &str| {
            s.guest_frame(frames::udp(
                GUEST_ADDR,
                GATEWAY_ADDR,
                5353,
                53,
                &dns::query_bytes(7, name, dns::TYPE_A),
            ));
        };
        ask(&mut s, "MOCK.test");
        ask(&mut s, "hidden.test");
        s.poll(g.tick());
        let mut answers = Vec::new();
        while let Some(frame) = s.take_frame() {
            answers.push(dns::answers(&frames::parse_udp(&frame).unwrap().2));
        }
        assert_eq!(answers, [(0, vec![GATEWAY_ADDR]), (3, vec![])]);
        assert!(s.host().lookups.is_empty(), "entries never reach the resolver");
        // By the entry's name, the rule's port only.
        s.guest_frame(syn(40001, GATEWAY_ADDR, 8080));
        s.guest_frame(syn(40002, GATEWAY_ADDR, 8081));
        s.poll(g.tick());
        assert_eq!(s.host().connects, [SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8080)]);
        // Runtime changes apply to the next query.
        shared.hosts.write().unwrap().remove("mock.test");
        ask(&mut s, "mock.test");
        s.poll(g.tick());
        tcp_out(&mut s);
        let frame = s.take_frame();
        assert!(frame.is_none(), "now the host's resolver is asked");
        assert_eq!(s.host().lookups.len(), 1);
        let stats = shared.counters.snapshot();
        assert_eq!((stats.dns_queries, stats.dns_refused), (3, 1));
        assert_eq!((stats.connections_allowed, stats.connections_denied), (1, 1));
        let log = shared.log().take();
        assert_eq!(log.len(), 1, "the denied one; the other is open");
        assert_eq!(log[0].names, ["mock.test"]);
        assert_eq!(log[0].port, 8081);
        s.check_invariants();
    }

    /// A stack whose policy asks about every connection to 1.1.1.1, with
    /// a decider the test answers for (FakeHost records the questions).
    fn asking(decider: Option<Decider>) -> (Arc<Shared>, Stack<FakeHost>, Guest) {
        let p = EgressPolicy::deny_all()
            .deny(Ipv4Addr::new(1, 1, 1, 1), 25)
            .ask(Ipv4Addr::new(1, 1, 1, 1), Ports::ANY);
        let mut config = NetworkConfig::new(p);
        config.decider = decider;
        let shared = Shared::new(config).unwrap();
        let g = Guest::new();
        let mut s = Stack::new(shared.clone(), FakeHost::default(), 1, Options::default(), g.now);
        s.guest_frame(arp_request());
        s.poll(g.now);
        s.take_frame();
        (shared, s, g)
    }

    fn decider(timeout_ms: u64, default: Verdict) -> Decider {
        Decider::new(|_| unreachable!("the stack never calls it itself"))
            .with_timeout(std::time::Duration::from_millis(timeout_ms))
            .with_default(default)
    }

    #[test]
    fn the_decider_allows_and_denies() {
        let (shared, mut s, mut g) = asking(Some(decider(5000, Verdict::Deny)));
        let one = Ipv4Addr::new(1, 1, 1, 1);
        s.guest_frame(syn(40001, one, 443));
        s.guest_frame(syn(40002, one, 80));
        s.guest_frame(syn(40003, one, 25));
        s.poll(g.tick());
        // Port 25 denied by its rule at once; the others wait.
        let refused: Vec<u16> = tcp_out(&mut s).iter().map(|(r, _)| r.dst_port).collect();
        assert_eq!(refused, [40003]);
        assert!(s.host().connects.is_empty());
        let asked: Vec<(u64, u16, usize)> =
            s.host().decisions.iter().map(|(id, r)| (*id, r.port, r.rule)).collect();
        assert_eq!(asked.len(), 2);
        assert!(asked.iter().all(|&(_, _, rule)| rule == 1));
        // A retransmitted SYN while deciding is dropped, not asked again.
        s.guest_frame(syn(40001, one, 443));
        s.poll(g.tick());
        assert!(tcp_out(&mut s).is_empty());
        assert_eq!(s.host().decisions.len(), 2);
        s.check_invariants();
        s.decided(asked[0].0, Some(Verdict::Allow));
        s.decided(asked[1].0, Some(Verdict::Deny));
        s.poll(g.tick());
        assert_eq!(s.host().connects, [SocketAddrV4::new(one, 443)]);
        let refused: Vec<u16> = tcp_out(&mut s).iter().map(|(r, _)| r.dst_port).collect();
        assert_eq!(refused, [40002]);
        // A second answer for the same question changes nothing.
        s.decided(asked[1].0, Some(Verdict::Allow));
        assert_eq!(s.host().connects.len(), 1);
        let log = shared.log().take();
        let by: Vec<(u16, Verdict, DecidedBy)> =
            log.iter().map(|r| (r.port, r.verdict, r.decided_by.clone())).collect();
        assert_eq!(
            by,
            [
                (
                    25,
                    Verdict::Deny,
                    DecidedBy::Rule { index: 0, rule: shared.policy().rules()[0].clone() }
                ),
                (80, Verdict::Deny, DecidedBy::Callback),
            ]
        );
        assert_eq!(log[1].duration, std::time::Duration::from_millis(2));
        let stats = shared.counters.snapshot();
        assert_eq!(stats.decisions, 2);
        assert_eq!((stats.connections_allowed, stats.connections_denied), (1, 2));
        s.check_invariants();
    }

    /// A decider that never answers: at its deadline (the stack's clock,
    /// not a sleep) the default applies, whichever it is, and the network
    /// thread's wait includes the deadline; a late answer is ignored.
    #[test]
    fn a_decider_that_does_not_answer_gets_its_default() {
        for default in [Verdict::Deny, Verdict::Allow] {
            let (shared, mut s, _) = asking(Some(decider(500, default)));
            let one = Ipv4Addr::new(1, 1, 1, 1);
            s.set_now(Instant::from_millis(100));
            s.guest_frame(syn(40001, one, 443));
            s.poll(Instant::from_millis(100));
            assert_eq!(s.poll_delay(Instant::from_millis(350)), Some(Duration::from_millis(250)));
            s.poll(Instant::from_millis(599));
            assert!(tcp_out(&mut s).is_empty() && s.host().connects.is_empty());
            s.poll(Instant::from_millis(600));
            let refused = tcp_out(&mut s);
            match default {
                Verdict::Deny => {
                    assert_eq!(refused.len(), 1);
                    assert_eq!(refused[0].0.control, TcpControl::Rst);
                }
                Verdict::Allow => {
                    assert!(refused.is_empty());
                    assert_eq!(s.host().connects, [SocketAddrV4::new(one, 443)]);
                }
            }
            let (id, _) = s.host().decisions[0];
            s.decided(id, Some(Verdict::Allow));
            s.poll(Instant::from_millis(700));
            assert!(s.host().connects.len() <= 1, "the late answer is ignored");
            let stats = shared.counters.snapshot();
            assert_eq!(stats.decisions_timed_out, 1);
            if default == Verdict::Deny {
                let log = shared.log().take();
                assert_eq!(log[0].decided_by, DecidedBy::CallbackTimeout);
                assert_eq!(log[0].duration, std::time::Duration::from_millis(500));
            }
            s.check_invariants();
        }
    }

    #[test]
    fn a_failed_or_missing_decider_denies_or_defaults() {
        // No decider at all: denied at once.
        let (shared, mut s, mut g) = asking(None);
        s.guest_frame(syn(40001, Ipv4Addr::new(1, 1, 1, 1), 443));
        s.poll(g.tick());
        assert_eq!(tcp_out(&mut s).len(), 1);
        assert_eq!(shared.log().take()[0].decided_by, DecidedBy::NoCallback);
        // A decider that failed (panicked, or answered None): its default.
        let (shared, mut s, mut g) = asking(Some(decider(500, Verdict::Allow)));
        s.guest_frame(syn(40001, Ipv4Addr::new(1, 1, 1, 1), 443));
        s.poll(g.tick());
        let (id, _) = s.host().decisions[0];
        s.decided(id, None);
        s.poll(g.tick());
        assert_eq!(s.host().connects.len(), 1);
        assert_eq!(shared.counters.snapshot().decisions_failed, 1);
        s.check_invariants();
    }

    /// Connections waiting for the decider count against the limit.
    #[test]
    fn waiting_connections_count_against_the_limit() {
        let (shared, mut s, mut g) = asking(Some(decider(5000, Verdict::Deny)));
        for port in 0..MAX_CONNECTIONS as u16 + 1 {
            s.guest_frame(syn(10000 + port, Ipv4Addr::new(1, 1, 1, 1), 443));
        }
        s.poll(g.tick());
        assert_eq!(s.deciding_ids().len(), MAX_CONNECTIONS);
        assert_eq!(tcp_out(&mut s).len(), 1);
        assert_eq!(shared.log().take()[0].decided_by, DecidedBy::ConnectionLimit);
        s.check_invariants();
    }

    /// The log's records of a relayed connection count the bytes each way
    /// and its duration, by the stack's clock.
    #[test]
    fn the_log_counts_bytes_and_time() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let shared = policy(EgressPolicy::deny_all().allow(Ipv4Addr::LOCALHOST, port));
        let mut s = Stack::new(
            shared.clone(),
            RealStreams::default(),
            1,
            Options::default(),
            Instant::from_millis(0),
        );
        s.guest_frame(arp_request());
        s.poll(Instant::from_millis(1));
        s.take_frame();
        let dst = SocketAddrV4::new(GATEWAY_ADDR, port);
        s.guest_frame(syn(40000, GATEWAY_ADDR, port));
        s.poll(Instant::from_millis(2));
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(StdDuration::from_secs(5))).unwrap();
        let id = s.connection_ids()[0];
        let ready = Ready { readable: true, writable: true, closed: false };
        s.host_event(id, ready);
        s.poll(Instant::from_millis(3));
        let their = tcp_out(&mut s)[0].0.seq_number + 1;
        s.guest_frame(segment(40000, dst, TcpControl::Psh, 1, their, b"hello"));
        s.poll(Instant::from_millis(4));
        let mut buf = [0; 5];
        peer.read_exact(&mut buf).unwrap();
        peer.write_all(b"abc").unwrap();
        drop(peer);
        std::thread::sleep(StdDuration::from_millis(20));
        s.host_event(id, ready);
        s.poll(Instant::from_millis(5));
        let out = tcp_out(&mut s);
        // The FIN may carry the data: acknowledge both.
        let (fin, data) = out.iter().find(|(r, _)| r.control == TcpControl::Fin).unwrap();
        let fin_ack = fin.seq_number + data.len() + 1;
        let stats = shared.counters.snapshot();
        assert_eq!((stats.bytes_to_host, stats.bytes_to_guest), (5, 3));
        assert_eq!(stats.connections_open, 1);
        s.guest_frame(segment(40000, dst, TcpControl::Fin, 6, fin_ack, b""));
        s.poll(Instant::from_millis(1000));
        s.poll(Instant::from_millis(1001));
        assert!(s.connection_ids().is_empty());
        let log = shared.log().take();
        assert_eq!(log.len(), 1);
        let r = &log[0];
        assert_eq!((r.addr, r.port, r.guest_port), (Ipv4Addr::LOCALHOST, port, 40000));
        assert_eq!((r.bytes_to_host, r.bytes_to_guest), (5, 3));
        assert!(r.connected);
        assert_eq!(r.verdict, Verdict::Allow);
        assert!(matches!(r.decided_by, DecidedBy::Rule { index: 0, .. }));
        assert_eq!(r.duration, StdDuration::from_millis(999), "{r:?}");
        assert_eq!(shared.counters.snapshot().connections_open, 0);
        s.check_invariants();
    }
}
