// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Round trips of the stack through its saved state, with fake host
//! streams that log what they are written and send a known byte at each
//! offset, and a guest that follows TCP and checks every byte it gets: so
//! that a byte lost, duplicated or damaged by a save shows, as does a
//! connection reset that should have carried on.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;

use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{Icmpv4Repr, TcpControl};

use super::super::{HostNet, Options, Ready, Stack, StackState};
use crate::net::dns;
use crate::net::fuzz::{FakeHost, Flags, TcpGuest, frames, guest_byte, save_and_restore};
use crate::net::log::DecidedBy;
use crate::net::policy::{EgressPolicy, HostPattern, Ports, Verdict};
use crate::net::{Decider, GATEWAY_ADDR, GUEST_ADDR, NetworkConfig, Shared};
use crate::snapshot::codec::{Decoder, Encoder};

/// Small buffers, so that transfers take many rounds.
const OPTIONS: Options = Options { verify_checksums: true, tcp_buffer: 4 << 10 };

const READY: Ready = Ready { readable: true, writable: true, closed: false };

/// A host with 40 000 bytes to send (FakeHost sends port % 5 * 10 000).
fn big() -> SocketAddrV4 {
    SocketAddrV4::new(Ipv4Addr::new(9, 9, 9, 9), 4)
}

/// A host with 30 000 bytes to send.
fn medium() -> SocketAddrV4 {
    SocketAddrV4::new(Ipv4Addr::new(9, 9, 9, 9), 13)
}

/// A stack, the guest's side of its link, and the clock.
struct Net {
    stack: Option<Stack<FakeHost>>,
    shared: Arc<Shared>,
    guest: TcpGuest,
    now: Instant,
    /// RSTs the stack sent the guest.
    rsts: usize,
    /// Frames for the guest other than TCP.
    others: usize,
}

impl Net {
    fn new(shared: Arc<Shared>) -> Self {
        let now = Instant::from_millis(0);
        let mut stack = Stack::new(shared.clone(), FakeHost::default(), 1, OPTIONS, now);
        stack.guest_frame(frames::arp_request());
        let mut net = Self {
            stack: Some(stack),
            shared,
            guest: TcpGuest::default(),
            now,
            rsts: 0,
            others: 0,
        };
        net.step();
        net
    }

    fn allow_all() -> Self {
        Self::new(Shared::new(NetworkConfig::new(EgressPolicy::allow_all())).unwrap())
    }

    fn s(&mut self) -> &mut Stack<FakeHost> {
        self.stack.as_mut().unwrap()
    }

    /// A poll at the current time, the frames left for the caller.
    fn poll(&mut self) {
        let now = self.now;
        self.s().poll(now);
    }

    fn set_now(&mut self) {
        let now = self.now;
        self.s().set_now(now);
    }

    /// A millisecond on: every host stream ready, a poll, and the frames
    /// for the guest taken.
    fn step(&mut self) {
        self.now += Duration::from_millis(1);
        let now = self.now;
        let s = self.s();
        for id in s.connection_ids() {
            s.host_event(id, READY);
        }
        s.poll(now);
        self.drain();
    }

    fn drain(&mut self) {
        while let Some(frame) = self.s().take_frame() {
            match frames::parse_tcp(&frame) {
                Some((r, _)) if r.control == TcpControl::Rst => self.rsts += 1,
                Some(_) => {}
                None => self.others += 1,
            }
            self.guest.take(&frame);
        }
        let s = self.stack.as_ref().unwrap();
        s.check_invariants();
        self.guest.check_host_side(s);
    }

    fn connect(&mut self, port: u16, dst: SocketAddrV4) -> usize {
        let (i, syn) = self.guest.connect(port, dst);
        self.s().guest_frame(syn);
        self.step();
        i
    }

    /// Sends up to `len` more of the guest's data on connection `i`, up to
    /// `limit` bytes in all.
    fn send(&mut self, i: usize, len: usize, limit: usize) {
        let len = len.min(limit.saturating_sub(self.guest.conns[i].acked));
        if let Some(frame) = self.guest.segment(i, len, Flags::default()) {
            self.s().guest_frame(frame);
        }
    }

    fn close(&mut self, i: usize) {
        let flags = Flags { fin: true, ..Flags::default() };
        if let Some(frame) = self.guest.segment(i, 0, flags) {
            self.s().guest_frame(frame);
        }
    }

    /// Saves and restores the stack, its streams handed back (the restored
    /// stack must save to the same state), or not (`clone`).
    fn save_restore(&mut self, clone: bool) -> Vec<u64> {
        let stack = self.stack.take().unwrap();
        let (stack, open) = save_and_restore(stack, &self.shared, OPTIONS, self.now, clone);
        self.stack = Some(stack);
        self.s().check_invariants();
        open
    }

    fn written(&mut self, i: usize) -> Vec<u8> {
        let c = &self.guest.conns[i];
        let (guest, dst) = (c.guest(), c.dst);
        let s = self.s();
        let id = s.connection_id(guest, dst).expect("the connection is there");
        s.host().written(id)
    }
}

fn guest_bytes(n: usize) -> Vec<u8> {
    (0..n).map(guest_byte).collect()
}

/// Saved and restored in the middle of transfers both ways, with data
/// received out of order, twice: every byte reaches the other end exactly
/// once, and nothing is reset.
#[test]
fn transfers_carry_on_across_a_restore() {
    let mut net = Net::allow_all();
    let a = net.connect(40001, big());
    let b = net.connect(40002, medium());
    net.step();
    assert!(net.guest.conns[a].open() && net.guest.conns[b].open());
    let target = 20_000;
    let mut saves = 0;
    for round in 0..400 {
        net.send(a, 1000, target);
        net.send(b, 700, target);
        if round == 2 {
            // Past a gap (beyond the 1000 bytes just sent): held in
            // smoltcp's reassembly across the save.
            let start = net.guest.conns[a].acked + 1500;
            let frame = net.guest.segment_at(a, start, 300, Flags::default()).unwrap();
            net.s().guest_frame(frame);
        }
        net.step();
        if round == 2 || round == 5 {
            for c in [a, b] {
                let conn = &net.guest.conns[c];
                assert!(conn.received > 0 && conn.received < 30_000, "{conn:?}");
                assert!(conn.acked > 0 && conn.acked < target, "{conn:?}");
            }
            net.save_restore(false);
            saves += 1;
        }
        let done = |c: &crate::net::fuzz::GuestConn, total| {
            c.received == total && c.received_fin && c.acked == target
        };
        if done(&net.guest.conns[a], 40_000) && done(&net.guest.conns[b], 30_000) {
            break;
        }
    }
    assert_eq!(saves, 2);
    // The host streams take the last of it at the next readiness.
    net.step();
    for (c, total) in [(a, 40_000), (b, 30_000)] {
        let conn = &net.guest.conns[c];
        assert_eq!(conn.received, total, "{conn:?}");
        assert!(conn.received_fin);
        assert_eq!(net.written(c), guest_bytes(target));
    }
    assert_eq!(net.rsts, 0);
    // Each save restores twice (the check that the restored stack saves to
    // the same state, then the stack used): two streams, four times.
    assert_eq!(net.s().host().adopted.len(), 8);
}

/// Half-closed connections both ways across a restore: the guest closed
/// its end while the host still sends, and the host closed its end while
/// the guest still sends. Both finish cleanly, every byte across.
#[test]
fn half_closed_connections_carry_on_across_a_restore() {
    let mut net = Net::allow_all();
    let a = net.connect(40001, big());
    let b = net.connect(40002, medium());
    net.step();
    // The guest sends 3000 bytes on a and closes; the host keeps sending.
    for _ in 0..3 {
        net.send(a, 1000, 3000);
        net.step();
    }
    net.close(a);
    // On b, the guest takes the host's 30 000 bytes and its FIN first.
    for _ in 0..200 {
        net.send(b, 0, 0);
        net.step();
        if net.guest.conns[b].received_fin {
            break;
        }
    }
    let conn = &net.guest.conns[b];
    assert!(conn.received == 30_000 && conn.received_fin, "{conn:?}");
    assert!(net.guest.conns[a].fin_acked);
    assert!(net.guest.conns[a].received < 40_000);
    net.save_restore(false);
    for _ in 0..400 {
        net.send(a, 0, 0);
        net.send(b, 1000, 12_000);
        net.step();
        let (ca, cb) = (&net.guest.conns[a], &net.guest.conns[b]);
        if ca.received_fin && cb.acked == 12_000 {
            break;
        }
    }
    let conn = &net.guest.conns[a];
    assert!(conn.received == 40_000 && conn.received_fin, "{conn:?}");
    // The host stream takes the last of it at the next readiness.
    net.send(b, 0, 0);
    net.step();
    assert_eq!(net.written(b), guest_bytes(12_000));
    // b closes too; then both connections are gone, neither reset.
    net.close(b);
    for _ in 0..5 {
        net.send(a, 0, 0);
        net.send(b, 0, 0);
        net.step();
    }
    assert_eq!(net.rsts, 0);
    assert!(net.s().connection_ids().is_empty(), "both closed");
    let log = net.shared.log().take();
    assert_eq!(log.len(), 2);
    let mut moved: Vec<(u16, u64, u64)> =
        log.iter().map(|r| (r.guest_port, r.bytes_to_host, r.bytes_to_guest)).collect();
    moved.sort_unstable();
    assert_eq!(
        moved,
        [(40001, 3000, 40_000), (40002, 12_000, 30_000)],
        "the log counts the bytes moved before the save too"
    );
}

/// A stack with an ask rule for 1.1.1.1 and a name rule for pypi.org.
fn asking() -> Net {
    let p = EgressPolicy::deny_all()
        .allow(HostPattern::name("pypi.org").unwrap(), 443)
        .ask(Ipv4Addr::new(1, 1, 1, 1), Ports::ANY)
        .allow(HostPattern::Any, Ports::ANY);
    let config =
        NetworkConfig::new(p).with_dns_ttl(std::time::Duration::from_secs(30)).with_decider(
            Decider::new(|_| unreachable!("FakeHost never calls it"))
                .with_timeout(std::time::Duration::from_secs(5)),
        );
    Net::new(Shared::new(config).unwrap())
}

fn ask_dns(net: &mut Net, id: u16, name: &str) {
    let query = dns::query_bytes(id, name, dns::TYPE_A);
    let frame = frames::udp(GUEST_ADDR, GATEWAY_ADDR, 5353, 53, &query);
    net.s().guest_frame(frame);
}

fn dns_answers(net: &mut Net) -> Vec<(u8, Vec<Ipv4Addr>)> {
    let mut out = Vec::new();
    while let Some(frame) = net.s().take_frame() {
        if let Some((_, 53, payload)) = frames::parse_udp(&frame) {
            out.push(dns::answers(&payload));
        }
    }
    out
}

/// What waits at a save: a connection whose host connection is being
/// made, one waiting for the decider (asked again, with the time it had
/// left), a DNS lookup (made again), and a DNS answer not yet sent. Each
/// completes after the restore.
#[test]
fn waiting_connections_and_lookups_carry_on_across_a_restore() {
    let mut net = asking();
    // Connecting: the host connection is not made yet.
    let (c, syn) = net.guest.connect(40001, medium());
    net.s().guest_frame(syn);
    net.poll();
    let connecting = net.s().connection_ids();
    assert_eq!(connecting.len(), 1);
    // Waiting for the decider, asked at 1 ms.
    let one = SocketAddrV4::new(Ipv4Addr::new(1, 1, 1, 1), 443);
    let (d, syn) = net.guest.connect(40002, one);
    net.s().guest_frame(syn);
    // A lookup in flight, and another answered but not yet sent.
    ask_dns(&mut net, 1, "pypi.org");
    ask_dns(&mut net, 2, "files.pypi.org");
    net.poll();
    assert!(dns_answers(&mut net).is_empty());
    let deciding = net.s().deciding_ids();
    assert_eq!(deciding.len(), 1);
    let lookups = net.s().host().lookups.clone();
    assert_eq!(lookups.len(), 2);
    let pypi = Ipv4Addr::new(151, 101, 0, 223);
    let files = lookups.iter().find(|(_, n)| n == "files.pypi.org").unwrap().0;
    net.s().resolved(files, Ok(vec![Ipv4Addr::new(151, 101, 0, 1)]));

    // Saved at 2 s; the restoring stack's clock is at 100 s.
    net.now = Instant::from_secs(2);
    net.set_now();
    let (state, streams, mut host) = net.stack.take().unwrap().save();
    host.lookups.clear();
    host.decisions.clear();
    let restored_at = Instant::from_secs(100);
    net.now = restored_at;
    let s =
        Stack::restore(net.shared.clone(), host, 9, OPTIONS, restored_at, state, streams).unwrap();
    net.stack = Some(s);
    assert_eq!(net.s().host().adopted, connecting);
    assert_eq!(net.s().connection_ids(), connecting);
    assert_eq!(net.s().deciding_ids(), deciding);
    let asked: Vec<u64> = net.s().host().decisions.iter().map(|q| q.0).collect();
    assert_eq!(asked, deciding, "the decider is asked again, same ID");
    let request = &net.s().host().decisions[0].1;
    assert_eq!((request.addr, request.port, request.rule), (*one.ip(), 443, 1));
    let again: Vec<String> = net.s().host().lookups.iter().map(|l| l.1.clone()).collect();
    assert_eq!(again, ["pypi.org"], "the lookup in flight is made again");
    // The answer queued before the save goes out now.
    net.poll();
    assert_eq!(dns_answers(&mut net), [(0, vec![Ipv4Addr::new(151, 101, 0, 1)])]);
    // The decider's deadline: 5 s after it was asked at 1 ms, so 3.001 s
    // after the restore.
    assert_eq!(net.s().poll_delay(restored_at), Some(Duration::from_micros(3_001_000)));
    // The lookup answers; the name allows its address, for the TTL left.
    let id = net.s().host().lookups[0].0;
    net.s().resolved(id, Ok(vec![pypi]));
    net.poll();
    assert_eq!(dns_answers(&mut net), [(0, vec![pypi])]);
    // The decider allows; the host connections complete; both handshakes
    // finish with the guest.
    net.s().decided(deciding[0], Some(Verdict::Allow));
    for _ in 0..3 {
        net.step();
    }
    assert!(net.guest.conns[c].open() && net.guest.conns[d].open());
    for _ in 0..100 {
        net.send(c, 500, 2000);
        net.send(d, 0, 0);
        net.step();
    }
    assert_eq!(net.guest.conns[c].received, 30_000);
    assert_eq!(net.written(c), guest_bytes(2000));
    assert_eq!(net.rsts, 0);
}

/// Learnt names keep the TTL they had left at the save, by the restored
/// stack's clock.
#[test]
fn learnt_names_keep_their_ttl() {
    let p = EgressPolicy::deny_all().allow(HostPattern::name("pypi.org").unwrap(), 443);
    let config = NetworkConfig::new(p).with_dns_ttl(std::time::Duration::from_secs(30));
    let mut net = Net::new(Shared::new(config).unwrap());
    ask_dns(&mut net, 1, "pypi.org");
    net.s().poll(Instant::from_secs(1));
    let id = net.s().host().lookups[0].0;
    let pypi = Ipv4Addr::new(151, 101, 0, 223);
    net.s().resolved(id, Ok(vec![pypi]));
    net.s().poll(Instant::from_secs(1));
    dns_answers(&mut net);
    // 30 s TTL from 1 s; saved at 10 s: 21 s left.
    net.now = Instant::from_secs(10);
    net.set_now();
    net.now = Instant::from_secs(500);
    net.save_restore(false);
    let connects = |net: &mut Net, port, at: i64| {
        let before = net.s().host().connects.len();
        net.now = Instant::from_secs(at);
        let now = net.now;
        net.s().set_now(now);
        let (_, syn) = net.guest.connect(port, SocketAddrV4::new(pypi, 443));
        net.s().guest_frame(syn);
        net.step();
        net.s().host().connects.len() > before
    };
    assert!(connects(&mut net, 40001, 520));
    assert!(!connects(&mut net, 40002, 522), "expired 21 s after the restore");
}

/// Fills the queue for the guest past what smoltcp may add to it: pings
/// the gateway answers, not taken.
fn fill_the_queue(net: &mut Net) {
    for seq_no in 0..300 {
        net.s().guest_frame(frames::icmp(
            GUEST_ADDR,
            GATEWAY_ADDR,
            Icmpv4Repr::EchoRequest { ident: 1, seq_no, data: b"x" },
        ));
    }
    let now = net.now;
    net.s().poll(now);
}

/// A clone (no host streams handed back): each open connection is reset
/// towards the guest, its RST out before it goes even from a full queue; a
/// connection whose host connection was being made has it made again, and
/// its handshake completes.
#[test]
fn a_clone_resets_open_connections() {
    let mut net = Net::allow_all();
    let a = net.connect(40001, big());
    let b = net.connect(40002, medium());
    for _ in 0..3 {
        net.send(a, 1000, 5000);
        net.send(b, 1000, 5000);
        net.step();
    }
    let (c, syn) = net.guest.connect(40003, medium());
    net.s().guest_frame(syn);
    net.poll();
    fill_the_queue(&mut net);
    let others = net.others;
    let connects = net.s().host().connects.len();
    let open = net.save_restore(true);
    assert_eq!(open.len(), 2);
    assert_eq!(net.s().host().connects.len(), connects + 1, "made again");
    // The queue is still full: the RSTs wait, and so do the connections.
    net.poll();
    assert_eq!(net.s().open_ids(), open, "removed only once the RST is out");
    for _ in 0..4 {
        net.step();
    }
    assert!(net.guest.conns[a].reset && net.guest.conns[b].reset);
    assert_eq!(net.others - others, 300, "the echo replies queued at the save");
    assert!(net.s().open_ids().iter().all(|id| !open.contains(id)));
    assert!(net.guest.conns[c].open(), "the connecting one completes");
    for _ in 0..60 {
        net.send(c, 1000, 3000);
        net.step();
    }
    assert_eq!(net.guest.conns[c].received, 30_000);
    assert_eq!(net.written(c), guest_bytes(3000));
    let log = net.shared.log().take();
    let reset: Vec<u16> = log.iter().map(|r| r.guest_port).collect();
    assert!(reset.contains(&40001) && reset.contains(&40002), "{log:?}");
}

/// A restore without a decider refuses the connections that waited for
/// one, as a new connection would be.
#[test]
fn without_a_decider_waiting_connections_are_refused() {
    let mut net = asking();
    let (d, syn) = net.guest.connect(40002, SocketAddrV4::new(Ipv4Addr::new(1, 1, 1, 1), 443));
    net.s().guest_frame(syn);
    net.step();
    let (state, streams, host) = net.stack.take().unwrap().save();
    *net.shared.decider.write().unwrap() = None;
    let s = Stack::restore(net.shared.clone(), host, 9, OPTIONS, net.now, state, streams);
    net.stack = Some(s.unwrap());
    net.step();
    assert!(net.guest.conns[d].reset);
    let log = net.shared.log().take();
    assert_eq!(log[0].decided_by, DecidedBy::NoCallback);
}

fn encoded(state: &StackState) -> Vec<u8> {
    let mut e = Encoder::default();
    state.encode(&mut e);
    e.finish()
}

/// A state of another version is refused, and so is every damaged prefix
/// of one, with an error rather than a panic.
#[test]
fn other_versions_and_damaged_states_are_refused() {
    let mut net = asking();
    let a = net.connect(40001, big());
    net.send(a, 100, 100);
    net.step();
    ask_dns(&mut net, 1, "pypi.org");
    let (_, syn) = net.guest.connect(40002, SocketAddrV4::new(Ipv4Addr::new(1, 1, 1, 1), 443));
    net.s().guest_frame(syn);
    net.step();
    let (state, _, _) = net.stack.take().unwrap().save();
    let bytes = encoded(&state);
    let mut other = bytes.clone();
    other[..4].copy_from_slice(&(super::STATE_VERSION + 1).to_le_bytes());
    let err = StackState::decode(&mut Decoder::new(&other)).err().unwrap();
    assert!(err.to_string().contains("version 2"), "{err}");
    for n in 0..bytes.len() {
        let mut d = Decoder::new(&bytes[..n]);
        assert!(StackState::decode(&mut d).and_then(|_| d.end()).is_err(), "a {n}-byte prefix");
    }
    let mut d = Decoder::new(&bytes);
    StackState::decode(&mut d).unwrap();
    d.end().unwrap();
}

/// Every rule a log record can name comes back as it was.
#[test]
fn rules_round_trip() {
    let rules = EgressPolicy::deny_all()
        .allow(HostPattern::Any, Ports::ANY)
        .deny("10.0.0.0/8".parse::<HostPattern>().unwrap(), 25)
        .ask(
            "::1".parse::<HostPattern>().unwrap(),
            Ports::from(std::ops::RangeInclusive::new(1, 0)),
        )
        .allow(HostPattern::name("*.example.com").unwrap(), 8000..=9000)
        .allow(HostPattern::name("1.2.3.4.test").unwrap(), 1)
        .allow(Ipv4Addr::new(1, 2, 3, 4), 443)
        .rules()
        .to_vec();
    for r in rules {
        let mut e = Encoder::default();
        super::rule(&mut e, &r);
        let bytes = e.finish();
        let mut d = Decoder::new(&bytes);
        assert_eq!(super::read_rule(&mut d).unwrap(), r);
        d.end().unwrap();
    }
}

/// A host stream handed back that cannot be taken on is treated as not
/// handed back; streams for connections the state does not hold are
/// closed.
#[test]
fn unknown_streams_are_dropped() {
    let mut net = Net::allow_all();
    let a = net.connect(40001, big());
    net.step();
    let (state, mut streams, mut host) = net.stack.take().unwrap().save();
    let stray = host.connect(999, medium()).unwrap();
    streams.insert(999, stray);
    let s = Stack::restore(net.shared.clone(), host, 9, OPTIONS, net.now, state, streams);
    net.stack = Some(s.unwrap());
    assert_eq!(net.s().connection_ids().len(), 1);
    net.step();
    assert!(net.guest.conns[a].open());
}

/// The guest's last data, sent with its FIN after the host closed its end,
/// reaches the host before the connection goes, though the host stream
/// takes it in several writes. (The connection used to be removed as soon
/// as smoltcp reached TIME-WAIT, dropping what the host had not taken.)
#[test]
fn data_before_the_guests_last_fin_reaches_the_host() {
    let mut net = Net::allow_all();
    let b = net.connect(40002, medium());
    for _ in 0..200 {
        net.send(b, 0, 0);
        net.step();
        if net.guest.conns[b].received_fin {
            break;
        }
    }
    assert!(net.guest.conns[b].received_fin);
    net.send(b, 0, 0);
    net.step();
    // 3000 bytes (within the stack's window) and FIN in one segment:
    // TIME-WAIT at once.
    let fin = Flags { fin: true, ..Flags::default() };
    let frame = net.guest.segment(b, 3000, fin).unwrap();
    net.s().guest_frame(frame);
    for _ in 0..5 {
        net.step();
    }
    assert!(net.guest.conns[b].fin_acked);
    assert!(net.s().connection_ids().is_empty());
    assert_eq!(net.s().host().written(0), guest_bytes(3000));
    assert_eq!(net.rsts, 0);
}

/// The guest's last data, sent with its FIN before the host closed its end,
/// reaches the host though the host took none of it until the connection
/// had closed both ways (CLOSED, after LAST-ACK). (Found by the fuzzer: the
/// connection used to be removed at CLOSED, dropping the data.)
#[test]
fn data_before_the_guests_first_fin_reaches_a_slow_host() {
    let mut net = Net::allow_all();
    let b = net.connect(40002, medium());
    net.send(b, 0, 0);
    net.step();
    net.s().host().stalled.store(true, std::sync::atomic::Ordering::Relaxed);
    let fin = Flags { fin: true, ..Flags::default() };
    let frame = net.guest.segment(b, 3000, fin).unwrap();
    net.s().guest_frame(frame);
    for _ in 0..100 {
        net.send(b, 0, 0);
        net.step();
        if net.guest.conns[b].received_fin {
            break;
        }
    }
    net.send(b, 0, 0);
    net.step();
    let conn = &net.guest.conns[b];
    assert!(conn.received == 30_000 && conn.received_fin && conn.fin_acked, "{conn:?}");
    assert!(net.s().host().written(0).is_empty(), "the host took nothing yet");
    assert_eq!(net.s().connection_ids(), [0], "kept until the host takes it");
    net.s().host().stalled.store(false, std::sync::atomic::Ordering::Relaxed);
    for _ in 0..3 {
        net.step();
    }
    assert!(net.s().connection_ids().is_empty());
    assert_eq!(net.s().host().written(0), guest_bytes(3000));
    assert_eq!(net.rsts, 0);
}
