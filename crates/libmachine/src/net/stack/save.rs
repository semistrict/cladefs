// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Saving a stack and restoring it: everything the guest can observe goes
//! into a [`StackState`], plain data with an encoding of its own version;
//! the host sockets are handed out beside it, keyed by connection ID, and
//! handed back to the restore.
//!
//! What the guest sees across a save and a restore:
//! - An open connection whose host socket is handed back carries on: its
//!   smoltcp socket is restored whole (sequence numbers, windows, options,
//!   the data in flight both ways, its timers with the time they had left).
//!   One whose host socket is not handed back (a clone of a snapshot, in
//!   another process) is reset towards the guest.
//! - A connection whose host connection was being made keeps waiting for
//!   it: with its host socket handed back, as before; without, the host
//!   connection is made again, to the same destination, with the verdict
//!   it had. The guest's SYN waits either way.
//! - A connection waiting for the decider is asked about again (the
//!   question asked before is answered to a thread that is gone), with the
//!   time it had left, under the same ID. Without a decider at the restore,
//!   it is refused, as a new one would be.
//! - DNS lookups in flight are made again, under the same ID; the guest
//!   gets its answer, later. Replies not yet sent go out after the restore.
//! - Names learnt from DNS keep the TTL they had left.
//! - Frames queued for the guest, and the guest's frames not yet taken,
//!   are kept.
//!
//! Every timer resumes with the time it had left: the time between the save
//! and the restore does not count. The log's start times are the wall
//! clock's, so a connection's logged duration includes that time.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddrV4};
use std::sync::Arc;

use smoltcp::socket::tcp::{
    self, AckDelaySnapshot, CongestionControl, RttSnapshot, Segment, SocketSnapshot, TimeOffset,
    TimerSnapshot,
};
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, EthernetAddress, EthernetFrame, EthernetProtocol,
    EthernetRepr, IpAddress, IpEndpoint, IpListenEndpoint, TcpSeqNumber,
};

use super::{
    Conn, DecidedBy, GUEST_MAC, HostNet, MAX_CONNECTIONS, MAX_FRAME, MAX_LOOKUPS, MAX_NAMED_ADDRS,
    MAX_NAMES_PER_ADDR, Meta, Options, Pending, Phase, Stack, bump, dns,
};
use crate::error::{Error, Result};
use crate::net::policy::{Action, HostPattern, Ports, Rule, Verdict};
use crate::net::{GATEWAY_ADDR, GUEST_ADDR, Shared};
use crate::snapshot::codec::{Decoder, Encoder};

/// The version of [`StackState`]'s encoding. A state of another version is
/// refused (no conversions: AGENTS.md).
pub(crate) const STATE_VERSION: u32 = 1;

/// The most DNS replies, and frames each way, a state may hold: well past
/// what a stack queues, so that only a damaged state is refused.
const MAX_QUEUED: usize = 4096;

/// A saved stack: what the guest can observe of it.
pub(crate) struct StackState {
    next_id: u64,
    /// Frames for the guest, in order.
    to_guest: Vec<Vec<u8>>,
    /// The guest's frames smoltcp has not taken yet.
    from_guest: Vec<Vec<u8>>,
    /// DNS replies not yet sent, in order.
    dns_out: Vec<(SocketAddrV4, Vec<u8>)>,
    /// Learnt names by address: each name, and the microseconds its TTL had
    /// left.
    names: Vec<(Ipv4Addr, Vec<(String, i64)>)>,
    /// DNS lookups in flight: their IDs, who asked, and the query.
    lookups: Vec<(u64, SocketAddrV4, dns::Query)>,
    deciding: Vec<SavedPending>,
    conns: Vec<SavedConn>,
}

struct SavedMeta {
    guest_port: u16,
    addr: Ipv4Addr,
    port: u16,
    names: Vec<String>,
    verdict: Verdict,
    decided_by: DecidedBy,
    /// When its SYN came, by the wall clock (microseconds since the Unix
    /// epoch).
    started: i64,
    connected: bool,
    to_host: u64,
    to_guest: u64,
}

struct SavedPending {
    id: u64,
    guest: SocketAddrV4,
    dst: SocketAddrV4,
    syn: Vec<u8>,
    /// Microseconds left until the decider's deadline.
    deadline: i64,
    default: Verdict,
    rule: usize,
    meta: SavedMeta,
}

enum SavedPhase {
    Connecting { syn: Vec<u8> },
    Open(Box<SocketSnapshot>),
}

struct SavedConn {
    id: u64,
    guest: SocketAddrV4,
    dst: SocketAddrV4,
    meta: SavedMeta,
    phase: SavedPhase,
    readable: bool,
    writable: bool,
    host_eof: bool,
    host_shut: bool,
    remove: bool,
}

fn damaged(what: &str) -> Error {
    Error::Snapshot(format!("the network stack's saved state is damaged: {what}"))
}

impl StackState {
    /// The IDs of the open connections it holds.
    #[cfg(any(test, feature = "fuzzing"))]
    pub(crate) fn open_ids(&self) -> Vec<u64> {
        self.conns.iter().filter(|c| matches!(c.phase, SavedPhase::Open(_))).map(|c| c.id).collect()
    }

    /// The state as bytes, its version first.
    pub(crate) fn encode(&self, e: &mut Encoder) {
        e.u32(STATE_VERSION);
        e.u64(self.next_id);
        for frames in [&self.to_guest, &self.from_guest] {
            e.count(frames.len());
            for frame in frames {
                e.bytes(frame);
            }
        }
        e.count(self.dns_out.len());
        for (to, reply) in &self.dns_out {
            sockaddr(e, *to);
            e.bytes(reply);
        }
        e.count(self.names.len());
        for (addr, names) in &self.names {
            e.u32(u32::from(*addr));
            e.count(names.len());
            for (name, left) in names {
                e.str(name);
                e.i64(*left);
            }
        }
        e.count(self.lookups.len());
        for (id, from, query) in &self.lookups {
            e.u64(*id);
            sockaddr(e, *from);
            query.save(e);
        }
        e.count(self.deciding.len());
        for p in &self.deciding {
            e.u64(p.id);
            sockaddr(e, p.guest);
            sockaddr(e, p.dst);
            e.bytes(&p.syn);
            e.i64(p.deadline);
            verdict(e, p.default);
            e.u64(p.rule as u64);
            meta(e, &p.meta);
        }
        e.count(self.conns.len());
        for c in &self.conns {
            e.u64(c.id);
            sockaddr(e, c.guest);
            sockaddr(e, c.dst);
            meta(e, &c.meta);
            for flag in [c.readable, c.writable, c.host_eof, c.host_shut, c.remove] {
                e.bool(flag);
            }
            match &c.phase {
                SavedPhase::Connecting { syn } => {
                    e.u8(0);
                    e.bytes(syn);
                }
                SavedPhase::Open(socket) => {
                    e.u8(1);
                    tcp_socket(e, socket);
                }
            }
        }
    }

    /// Reads what [`encode`](Self::encode) wrote; a state of another
    /// version is refused.
    pub(crate) fn decode(d: &mut Decoder<'_>) -> Result<Self> {
        let version = d.u32()?;
        if version != STATE_VERSION {
            return Err(Error::Snapshot(format!(
                "the snapshot holds version {version} of the network stack's state, \
                 this libmachine version {STATE_VERSION}"
            )));
        }
        let next_id = d.u64()?;
        let mut frames = || -> Result<Vec<Vec<u8>>> {
            let n = d.count(4)?;
            if n > MAX_QUEUED {
                return Err(damaged("too many frames"));
            }
            (0..n)
                .map(|_| {
                    let frame = d.bytes()?;
                    if frame.len() > MAX_FRAME {
                        return Err(damaged("a frame is larger than the MTU allows"));
                    }
                    Ok(frame.to_vec())
                })
                .collect()
        };
        let to_guest = frames()?;
        let from_guest = frames()?;
        let n = d.count(10)?;
        if n > MAX_QUEUED {
            return Err(damaged("too many DNS replies"));
        }
        let dns_out =
            (0..n).map(|_| Ok((read_sockaddr(d)?, d.bytes()?.to_vec()))).collect::<Result<_>>()?;
        let n = d.count(8)?;
        if n > MAX_NAMED_ADDRS {
            return Err(damaged("too many learnt names"));
        }
        let mut names = Vec::with_capacity(n);
        for _ in 0..n {
            let addr = Ipv4Addr::from(d.u32()?);
            let k = d.count(12)?;
            if k == 0 || k > MAX_NAMES_PER_ADDR {
                return Err(damaged("an address with no names, or too many"));
            }
            let list = (0..k).map(|_| Ok((d.str()?, d.i64()?))).collect::<Result<_>>()?;
            names.push((addr, list));
        }
        let n = d.count(14)?;
        if n > MAX_LOOKUPS {
            return Err(damaged("too many DNS lookups"));
        }
        let lookups = (0..n)
            .map(|_| Ok((d.u64()?, read_sockaddr(d)?, dns::Query::restore(d)?)))
            .collect::<Result<_>>()?;
        let n = d.count(40)?;
        if n > MAX_CONNECTIONS {
            return Err(damaged("too many connections"));
        }
        let deciding = (0..n)
            .map(|_| {
                Ok(SavedPending {
                    id: d.u64()?,
                    guest: read_sockaddr(d)?,
                    dst: read_sockaddr(d)?,
                    syn: d.bytes()?.to_vec(),
                    deadline: d.i64()?,
                    default: read_verdict(d)?,
                    rule: d.u64()? as usize,
                    meta: read_meta(d)?,
                })
            })
            .collect::<Result<_>>()?;
        let n = d.count(40)?;
        if n > MAX_CONNECTIONS {
            return Err(damaged("too many connections"));
        }
        let conns = (0..n)
            .map(|_| {
                let id = d.u64()?;
                let guest = read_sockaddr(d)?;
                let dst = read_sockaddr(d)?;
                let meta = read_meta(d)?;
                let [readable, writable, host_eof, host_shut, remove] =
                    [d.bool()?, d.bool()?, d.bool()?, d.bool()?, d.bool()?];
                let phase = match d.u8()? {
                    0 => SavedPhase::Connecting { syn: d.bytes()?.to_vec() },
                    1 => SavedPhase::Open(Box::new(read_tcp_socket(d)?)),
                    v => return Err(damaged(&format!("connection phase {v}"))),
                };
                Ok(SavedConn {
                    id,
                    guest,
                    dst,
                    meta,
                    phase,
                    readable,
                    writable,
                    host_eof,
                    host_shut,
                    remove,
                })
            })
            .collect::<Result<_>>()?;
        Ok(Self { next_id, to_guest, from_guest, dns_out, names, lookups, deciding, conns })
    }
}

impl<H: HostNet> Stack<H> {
    /// Saves the stack at its current time: its state, the host streams of
    /// its connections by ID, and its host side. The stack is gone after;
    /// [`restore`](Self::restore) makes it again.
    pub(crate) fn save(mut self) -> (StackState, HashMap<u64, H::Stream>, H) {
        let now = self.now;
        let left = |at: Instant| at.total_micros() - now.total_micros();
        let mut names: Vec<(Ipv4Addr, Vec<(String, i64)>)> = self
            .names
            .by_addr
            .iter()
            .map(|(addr, list)| {
                let live = list
                    .iter()
                    .filter(|(_, expires)| *expires > now)
                    .map(|(name, expires)| (name.clone(), left(*expires)))
                    .collect();
                (*addr, live)
            })
            .filter(|(_, list): &(Ipv4Addr, Vec<_>)| !list.is_empty())
            .collect();
        names.sort_by_key(|(addr, _)| *addr);
        let mut lookups: Vec<(u64, SocketAddrV4, dns::Query)> = self
            .lookups
            .drain()
            .map(|(id, (from, query))| (id, endpoint_addr(from), query))
            .collect();
        lookups.sort_by_key(|l| l.0);
        let mut deciding: Vec<SavedPending> = self
            .deciding
            .iter()
            .map(|(&id, p)| SavedPending {
                id,
                guest: p.guest,
                dst: p.dst,
                syn: p.syn.clone(),
                deadline: left(p.deadline),
                default: p.default,
                rule: p.rule,
                meta: self.save_meta(&p.meta),
            })
            .collect();
        deciding.sort_by_key(|p| p.id);
        let mut streams = HashMap::new();
        let mut ids: Vec<u64> = self.conns.keys().copied().collect();
        ids.sort_unstable();
        let mut conns = Vec::with_capacity(ids.len());
        for id in ids {
            let meta = self.save_meta(&self.conns[&id].meta);
            let conn = self.conns.get_mut(&id).expect("listed");
            let phase = match &conn.phase {
                Phase::Connecting { syn } => SavedPhase::Connecting { syn: syn.clone() },
                Phase::Open(handle) => SavedPhase::Open(Box::new(
                    self.sockets.get::<tcp::Socket>(*handle).snapshot(now),
                )),
            };
            if let Some(stream) = conn.stream.take() {
                streams.insert(id, stream);
            }
            conns.push(SavedConn {
                id,
                guest: conn.guest,
                dst: conn.dst,
                meta,
                phase,
                readable: conn.readable,
                writable: conn.writable,
                host_eof: conn.host_eof,
                host_shut: conn.host_shut,
                remove: conn.remove,
            });
        }
        let state = StackState {
            next_id: self.next_id,
            to_guest: self.phy.tx.drain(..).collect(),
            from_guest: self.phy.rx.drain(..).collect(),
            dns_out: self.dns_out.drain(..).map(|(to, reply)| (endpoint_addr(to), reply)).collect(),
            names,
            lookups,
            deciding,
            conns,
        };
        (state, streams, self.host)
    }

    fn save_meta(&self, m: &Meta) -> SavedMeta {
        SavedMeta {
            guest_port: m.guest_port,
            addr: m.addr,
            port: m.port,
            names: m.names.clone(),
            verdict: m.verdict,
            decided_by: m.decided_by.clone(),
            started: self.wall_micros(m.started),
            connected: m.connected,
            to_host: m.to_host,
            to_guest: m.to_guest,
        }
    }

    fn restore_meta(&self, m: SavedMeta) -> Meta {
        Meta {
            guest_port: m.guest_port,
            addr: m.addr,
            port: m.port,
            names: m.names,
            verdict: m.verdict,
            decided_by: m.decided_by,
            started: self.instant_at(m.started),
            connected: m.connected,
            to_host: m.to_host,
            to_guest: m.to_guest,
        }
    }

    /// A stack made again from `state` at `now`, applying `shared`'s policy
    /// as [`new`](Self::new) does, with the host streams handed back in
    /// `streams` (keyed by connection ID; others are closed). See the
    /// module's notes for what happens to each kind of connection.
    pub(crate) fn restore(
        shared: Arc<Shared>,
        host: H,
        seed: u64,
        options: Options,
        now: Instant,
        state: StackState,
        mut streams: HashMap<u64, H::Stream>,
    ) -> Result<Self> {
        if state.conns.len() + state.deciding.len() > MAX_CONNECTIONS {
            return Err(damaged("too many connections"));
        }
        let mut s = Self::new(shared, host, seed, options, now);
        // The guest's link address, which the saved stack had learnt (or
        // would have, at the guest's first frame): without it, smoltcp
        // would drop the first frame for the guest to ask for it.
        s.phy.rx.push_back(guest_arp_request());
        s.iface.poll(now, &mut s.phy, &mut s.sockets);
        s.phy.tx.clear();

        s.next_id = state.next_id;
        s.phy.tx = state.to_guest.into();
        s.phy.rx = state.from_guest.into();
        s.dns_out =
            state.dns_out.into_iter().map(|(to, reply)| (IpEndpoint::from(to), reply)).collect();
        s.flow_dns();
        let at = |left: i64| Instant::from_micros(now.total_micros() + left);
        for (addr, list) in state.names {
            let list = list.into_iter().map(|(n, left)| (n, at(left))).collect();
            if s.names.by_addr.insert(addr, list).is_some() {
                return Err(damaged("an address listed twice among learnt names"));
            }
        }
        for (id, from, query) in state.lookups {
            let name = query.name.clone();
            if id >= s.next_id || s.lookups.insert(id, (from.into(), query)).is_some() {
                return Err(damaged("a DNS lookup's ID"));
            }
            s.host.resolve(id, name);
        }

        let mut pending = Vec::new();
        for p in state.deciding {
            let meta = s.restore_meta(p.meta);
            s.claim(p.id, p.guest, p.dst)?;
            s.deciding.insert(
                p.id,
                Pending {
                    guest: p.guest,
                    dst: p.dst,
                    syn: p.syn,
                    deadline: at(p.deadline),
                    default: p.default,
                    rule: p.rule,
                    meta,
                },
            );
            pending.push(p.id);
        }

        for c in state.conns {
            let meta = s.restore_meta(c.meta);
            s.claim(c.id, c.guest, c.dst)?;
            let handed = streams.remove(&c.id);
            let stream = handed.filter(|st| s.host.adopt(c.id, st).is_ok());
            let (phase, stream, remove) = match c.phase {
                SavedPhase::Connecting { syn } => {
                    let stream = match stream {
                        Some(st) => st,
                        None => match s.host.connect(c.id, SocketAddrV4::new(meta.addr, meta.port))
                        {
                            Ok(st) => st,
                            Err(_) => {
                                // Refused as a failed connect is.
                                s.tuples.remove(&(c.guest, c.dst));
                                s.phy.rx.push_back(syn);
                                bump(&s.shared.counters.connections_failed);
                                s.log(meta);
                                continue;
                            }
                        },
                    };
                    (Phase::Connecting { syn }, Some(stream), c.remove)
                }
                SavedPhase::Open(snapshot) => {
                    let mut socket = s.new_socket();
                    socket
                        .restore(&snapshot, now)
                        .map_err(|e| damaged(&format!("a TCP socket: {e}")))?;
                    let ends = (IpEndpoint::from(c.dst), IpEndpoint::from(c.guest));
                    if snapshot.tuple.is_some_and(|t| t != ends) {
                        return Err(damaged("a TCP socket's endpoints are another connection's"));
                    }
                    let remove = c.remove || stream.is_none();
                    if stream.is_none() {
                        // No host end: reset the guest's end.
                        socket.abort();
                    }
                    (Phase::Open(s.sockets.add(socket)), stream, remove)
                }
            };
            s.conns.insert(
                c.id,
                Conn {
                    meta,
                    stream,
                    guest: c.guest,
                    dst: c.dst,
                    phase,
                    readable: c.readable,
                    writable: c.writable,
                    host_eof: c.host_eof,
                    host_shut: c.host_shut,
                    remove,
                },
            );
        }
        s.count_open();

        // Ask the decider again: the answers to the saved stack's questions
        // go to a thread that is gone.
        match s.shared.decider() {
            Some(decider) => {
                for id in pending {
                    let request = s.deciding[&id].request();
                    s.host.decide(id, request, decider.clone());
                }
            }
            None => {
                for id in pending {
                    if let Some(p) = s.deciding.remove(&id) {
                        s.tuples.remove(&(p.guest, p.dst));
                        let syn = p.syn.clone();
                        let meta = p.meta;
                        s.refuse(syn, meta, DecidedBy::NoCallback);
                    }
                }
            }
        }
        Ok(s)
    }

    /// Records connection `id` from `guest` to `dst` as the restore puts it
    /// back: refused if its ID or its ends are another's.
    fn claim(&mut self, id: u64, guest: SocketAddrV4, dst: SocketAddrV4) -> Result<()> {
        if id >= self.next_id
            || self.conns.contains_key(&id)
            || self.deciding.contains_key(&id)
            || self.tuples.insert((guest, dst), id).is_some()
        {
            return Err(damaged("two connections with one ID, or one pair of ends"));
        }
        Ok(())
    }
}

/// The ARP request the guest sends for its gateway.
fn guest_arp_request() -> Vec<u8> {
    let arp = ArpRepr::EthernetIpv4 {
        operation: ArpOperation::Request,
        source_hardware_addr: EthernetAddress(GUEST_MAC),
        source_protocol_addr: GUEST_ADDR,
        target_hardware_addr: EthernetAddress([0; 6]),
        target_protocol_addr: GATEWAY_ADDR,
    };
    let eth = EthernetRepr {
        src_addr: EthernetAddress(GUEST_MAC),
        dst_addr: EthernetAddress::BROADCAST,
        ethertype: EthernetProtocol::Arp,
    };
    let mut buf = vec![0; eth.buffer_len() + arp.buffer_len()];
    let mut frame = EthernetFrame::new_unchecked(&mut buf[..]);
    eth.emit(&mut frame);
    arp.emit(&mut ArpPacket::new_unchecked(frame.payload_mut()));
    buf
}

fn endpoint_addr(e: IpEndpoint) -> SocketAddrV4 {
    let IpAddress::Ipv4(addr) = e.addr;
    SocketAddrV4::new(addr, e.port)
}

fn sockaddr(e: &mut Encoder, a: SocketAddrV4) {
    e.u32(u32::from(*a.ip()));
    e.u16(a.port());
}

fn read_sockaddr(d: &mut Decoder<'_>) -> Result<SocketAddrV4> {
    Ok(SocketAddrV4::new(Ipv4Addr::from(d.u32()?), d.u16()?))
}

fn verdict(e: &mut Encoder, v: Verdict) {
    e.u8(match v {
        Verdict::Allow => 0,
        Verdict::Deny => 1,
    });
}

fn read_verdict(d: &mut Decoder<'_>) -> Result<Verdict> {
    match d.u8()? {
        0 => Ok(Verdict::Allow),
        1 => Ok(Verdict::Deny),
        v => Err(damaged(&format!("verdict {v}"))),
    }
}

fn ip(e: &mut Encoder, addr: IpAddr) {
    match addr {
        IpAddr::V4(a) => {
            e.u8(4);
            e.u32(u32::from(a));
        }
        IpAddr::V6(a) => {
            e.u8(6);
            e.bytes(&a.octets());
        }
    }
}

fn read_ip(d: &mut Decoder<'_>) -> Result<IpAddr> {
    match d.u8()? {
        4 => Ok(Ipv4Addr::from(d.u32()?).into()),
        6 => {
            let octets: [u8; 16] = d.bytes()?.try_into().map_err(|_| damaged("an IPv6 address"))?;
            Ok(Ipv6Addr::from(octets).into())
        }
        v => Err(damaged(&format!("IP version {v}"))),
    }
}

fn rule(e: &mut Encoder, r: &Rule) {
    e.u8(match r.action {
        Action::Allow => 0,
        Action::Deny => 1,
        Action::Ask => 2,
    });
    match &r.host {
        HostPattern::Any => e.u8(0),
        HostPattern::Ip(addr) => {
            e.u8(1);
            ip(e, *addr);
        }
        HostPattern::Cidr { addr, prefix } => {
            e.u8(2);
            ip(e, *addr);
            e.u8(*prefix);
        }
        HostPattern::Name(n) => {
            e.u8(3);
            e.str(n);
        }
        HostPattern::Subdomains(d) => {
            e.u8(4);
            e.str(d);
        }
    }
    e.u16(r.ports.first());
    e.u16(r.ports.last());
}

fn read_rule(d: &mut Decoder<'_>) -> Result<Rule> {
    let action = match d.u8()? {
        0 => Action::Allow,
        1 => Action::Deny,
        2 => Action::Ask,
        v => return Err(damaged(&format!("rule action {v}"))),
    };
    let host = match d.u8()? {
        0 => HostPattern::Any,
        1 => HostPattern::Ip(read_ip(d)?),
        2 => HostPattern::Cidr { addr: read_ip(d)?, prefix: d.u8()? },
        3 => HostPattern::Name(d.str()?),
        4 => HostPattern::Subdomains(d.str()?),
        v => return Err(damaged(&format!("host pattern {v}"))),
    };
    let (first, last) = (d.u16()?, d.u16()?);
    // An empty range is only ever 1-0 (`Ports::from`).
    let ports = Ports::range(first, last).unwrap_or(Ports::from(first..=last));
    Ok(Rule { action, host, ports })
}

fn decided_by(e: &mut Encoder, by: &DecidedBy) {
    match by {
        DecidedBy::Rule { index, rule: r } => {
            e.u8(0);
            e.u64(*index as u64);
            rule(e, r);
        }
        DecidedBy::NoRule => e.u8(1),
        DecidedBy::Callback => e.u8(2),
        DecidedBy::CallbackTimeout => e.u8(3),
        DecidedBy::CallbackFailed => e.u8(4),
        DecidedBy::NoCallback => e.u8(5),
        DecidedBy::ConnectionLimit => e.u8(6),
    }
}

fn read_decided_by(d: &mut Decoder<'_>) -> Result<DecidedBy> {
    Ok(match d.u8()? {
        0 => DecidedBy::Rule { index: d.u64()? as usize, rule: read_rule(d)? },
        1 => DecidedBy::NoRule,
        2 => DecidedBy::Callback,
        3 => DecidedBy::CallbackTimeout,
        4 => DecidedBy::CallbackFailed,
        5 => DecidedBy::NoCallback,
        6 => DecidedBy::ConnectionLimit,
        v => return Err(damaged(&format!("decided by {v}"))),
    })
}

fn meta(e: &mut Encoder, m: &SavedMeta) {
    e.u16(m.guest_port);
    e.u32(u32::from(m.addr));
    e.u16(m.port);
    e.count(m.names.len());
    for name in &m.names {
        e.str(name);
    }
    verdict(e, m.verdict);
    decided_by(e, &m.decided_by);
    e.i64(m.started);
    e.bool(m.connected);
    e.u64(m.to_host);
    e.u64(m.to_guest);
}

fn read_meta(d: &mut Decoder<'_>) -> Result<SavedMeta> {
    let guest_port = d.u16()?;
    let addr = Ipv4Addr::from(d.u32()?);
    let port = d.u16()?;
    let n = d.count(4)?;
    let names = (0..n).map(|_| d.str()).collect::<Result<_>>()?;
    Ok(SavedMeta {
        guest_port,
        addr,
        port,
        names,
        verdict: read_verdict(d)?,
        decided_by: read_decided_by(d)?,
        started: d.i64()?,
        connected: d.bool()?,
        to_host: d.u64()?,
        to_guest: d.u64()?,
    })
}

fn opt<T>(e: &mut Encoder, v: Option<T>, mut f: impl FnMut(&mut Encoder, T)) {
    e.bool(v.is_some());
    if let Some(v) = v {
        f(e, v);
    }
}

fn read_opt<T>(
    d: &mut Decoder<'_>,
    mut f: impl FnMut(&mut Decoder<'_>) -> Result<T>,
) -> Result<Option<T>> {
    if d.bool()? { Ok(Some(f(d)?)) } else { Ok(None) }
}

fn seq(e: &mut Encoder, s: TcpSeqNumber) {
    e.u32(s.0 as u32);
}

fn read_seq(d: &mut Decoder<'_>) -> Result<TcpSeqNumber> {
    Ok(TcpSeqNumber(d.u32()? as i32))
}

fn duration(e: &mut Encoder, v: Duration) {
    e.u64(v.total_micros());
}

fn read_duration(d: &mut Decoder<'_>) -> Result<Duration> {
    Ok(Duration::from_micros(d.u64()?))
}

fn offset(e: &mut Encoder, t: TimeOffset) {
    e.i64(t.0);
}

fn read_offset(d: &mut Decoder<'_>) -> Result<TimeOffset> {
    Ok(TimeOffset(d.i64()?))
}

fn ip_endpoint(e: &mut Encoder, ep: IpEndpoint) {
    sockaddr(e, endpoint_addr(ep));
}

fn read_ip_endpoint(d: &mut Decoder<'_>) -> Result<IpEndpoint> {
    Ok(read_sockaddr(d)?.into())
}

const TCP_STATES: [tcp::State; 11] = [
    tcp::State::Closed,
    tcp::State::Listen,
    tcp::State::SynSent,
    tcp::State::SynReceived,
    tcp::State::Established,
    tcp::State::FinWait1,
    tcp::State::FinWait2,
    tcp::State::CloseWait,
    tcp::State::Closing,
    tcp::State::LastAck,
    tcp::State::TimeWait,
];

/// The largest window scale (RFC 7323, section 2.3).
const MAX_WINDOW_SHIFT: u8 = 14;

fn tcp_socket(e: &mut Encoder, s: &SocketSnapshot) {
    let state = TCP_STATES.iter().position(|t| *t == s.state).unwrap_or(0);
    e.u8(state as u8);
    match s.timer {
        TimerSnapshot::Idle { keep_alive_at } => {
            e.u8(0);
            opt(e, keep_alive_at, offset);
        }
        TimerSnapshot::Retransmit { expires_at, delay } => {
            e.u8(1);
            offset(e, expires_at);
            duration(e, delay);
        }
        TimerSnapshot::FastRetransmit => e.u8(2),
        TimerSnapshot::Close { expires_at } => {
            e.u8(3);
            offset(e, expires_at);
        }
    }
    e.u32(s.rtt.rtt);
    e.u32(s.rtt.deviation);
    opt(e, s.rtt.timing, |e, (at, sq)| {
        offset(e, at);
        seq(e, sq);
    });
    opt(e, s.rtt.max_seq_sent, seq);
    e.u8(s.rtt.rto_count);
    e.u64(s.rx_capacity as u64);
    e.bytes(&s.rx_data);
    e.count(s.rx_segments.len());
    for segment in &s.rx_segments {
        e.u64(segment.offset as u64);
        e.bytes(&segment.data);
    }
    e.bool(s.rx_fin_received);
    e.bytes(&s.tx_data);
    opt(e, s.timeout, duration);
    opt(e, s.keep_alive, duration);
    opt(e, s.hop_limit, |e, h| e.u8(h));
    opt(e, s.listen_endpoint.addr, |e, a| {
        let IpAddress::Ipv4(a) = a;
        e.u32(u32::from(a));
    });
    e.u16(s.listen_endpoint.port);
    opt(e, s.tuple, |e, (local, remote)| {
        ip_endpoint(e, local);
        ip_endpoint(e, remote);
    });
    seq(e, s.local_seq_no);
    seq(e, s.remote_seq_no);
    seq(e, s.remote_last_seq);
    opt(e, s.remote_last_ack, seq);
    e.u16(s.remote_last_win);
    e.u8(s.remote_win_shift);
    e.u64(s.remote_win_len as u64);
    opt(e, s.remote_win_scale, |e, v| e.u8(v));
    e.bool(s.remote_has_sack);
    e.u64(s.remote_mss as u64);
    opt(e, s.remote_last_ts, offset);
    opt(e, s.local_rx_last_seq, seq);
    opt(e, s.local_rx_last_ack, seq);
    e.u8(s.local_rx_dup_acks);
    opt(e, s.ack_delay, duration);
    match s.ack_delay_timer {
        AckDelaySnapshot::Idle => e.u8(0),
        AckDelaySnapshot::Waiting(at) => {
            e.u8(1);
            offset(e, at);
        }
        AckDelaySnapshot::Immediate => e.u8(2),
    }
    offset(e, s.challenge_ack_timer);
    e.bool(s.nagle);
    // smoltcp is built without Reno and Cubic: None is the only algorithm.
    let CongestionControl::None = s.congestion_control;
    e.u8(0);
    e.bool(s.timestamps);
    e.u32(s.last_remote_tsval);
}

fn read_tcp_socket(d: &mut Decoder<'_>) -> Result<SocketSnapshot> {
    let state = *TCP_STATES.get(usize::from(d.u8()?)).ok_or_else(|| damaged("a TCP state"))?;
    let timer = match d.u8()? {
        0 => TimerSnapshot::Idle { keep_alive_at: read_opt(d, read_offset)? },
        1 => TimerSnapshot::Retransmit { expires_at: read_offset(d)?, delay: read_duration(d)? },
        2 => TimerSnapshot::FastRetransmit,
        3 => TimerSnapshot::Close { expires_at: read_offset(d)? },
        v => return Err(damaged(&format!("TCP timer {v}"))),
    };
    let rtt = RttSnapshot {
        rtt: d.u32()?,
        deviation: d.u32()?,
        timing: read_opt(d, |d| Ok((read_offset(d)?, read_seq(d)?)))?,
        max_seq_sent: read_opt(d, read_seq)?,
        rto_count: d.u8()?,
    };
    let rx_capacity = d.u64()? as usize;
    let rx_data = d.bytes()?.to_vec();
    let n = d.count(12)?;
    let rx_segments = (0..n)
        .map(|_| Ok(Segment { offset: d.u64()? as usize, data: d.bytes()?.to_vec() }))
        .collect::<Result<_>>()?;
    let rx_fin_received = d.bool()?;
    let tx_data = d.bytes()?.to_vec();
    let timeout = read_opt(d, read_duration)?;
    let keep_alive = read_opt(d, read_duration)?;
    let hop_limit = read_opt(d, |d| d.u8())?;
    let listen_endpoint = IpListenEndpoint {
        addr: read_opt(d, |d| Ok(IpAddress::Ipv4(Ipv4Addr::from(d.u32()?))))?,
        port: d.u16()?,
    };
    let tuple = read_opt(d, |d| Ok((read_ip_endpoint(d)?, read_ip_endpoint(d)?)))?;
    let snapshot = SocketSnapshot {
        state,
        timer,
        rtt,
        rx_capacity,
        rx_data,
        rx_segments,
        rx_fin_received,
        tx_data,
        timeout,
        keep_alive,
        hop_limit,
        listen_endpoint,
        tuple,
        local_seq_no: read_seq(d)?,
        remote_seq_no: read_seq(d)?,
        remote_last_seq: read_seq(d)?,
        remote_last_ack: read_opt(d, read_seq)?,
        remote_last_win: d.u16()?,
        remote_win_shift: d.u8()?,
        remote_win_len: d.u64()? as usize,
        remote_win_scale: read_opt(d, |d| d.u8())?,
        remote_has_sack: d.bool()?,
        remote_mss: d.u64()? as usize,
        remote_last_ts: read_opt(d, read_offset)?,
        local_rx_last_seq: read_opt(d, read_seq)?,
        local_rx_last_ack: read_opt(d, read_seq)?,
        local_rx_dup_acks: d.u8()?,
        ack_delay: read_opt(d, read_duration)?,
        ack_delay_timer: match d.u8()? {
            0 => AckDelaySnapshot::Idle,
            1 => AckDelaySnapshot::Waiting(read_offset(d)?),
            2 => AckDelaySnapshot::Immediate,
            v => return Err(damaged(&format!("ack delay timer {v}"))),
        },
        challenge_ack_timer: read_offset(d)?,
        nagle: d.bool()?,
        congestion_control: match d.u8()? {
            0 => CongestionControl::None,
            v => return Err(damaged(&format!("congestion control {v}"))),
        },
        timestamps: d.bool()?,
        last_remote_tsval: d.u32()?,
    };
    // What smoltcp would otherwise trip over: shifts past the largest
    // window scale, a connected state with no remote end, an MSS of 0.
    let connected = !matches!(state, tcp::State::Closed | tcp::State::Listen);
    if snapshot.remote_win_shift > MAX_WINDOW_SHIFT
        || snapshot.remote_win_scale.is_some_and(|s| s > MAX_WINDOW_SHIFT)
        || (connected && snapshot.tuple.is_none())
        || snapshot.remote_mss == 0
    {
        return Err(damaged("a TCP socket's state is inconsistent"));
    }
    Ok(snapshot)
}

#[cfg(test)]
mod tests;
