// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What the guest connected to: a record per connection, in a bounded log
//! the caller drains ([`Network::take_log`](super::Network::take_log)), and
//! counters per machine ([`Network::stats`](super::Network::stats)).
//!
//! The log is a ring: when it is full, the oldest record goes and
//! [`NetworkStats::log_dropped`] counts it, so a caller that never drains it
//! costs a bounded amount of memory and loses only the oldest history.

use std::collections::VecDeque;
use std::fmt;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use super::policy::{Rule, Verdict};

/// What decided a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecidedBy {
    /// This rule of the policy then in force (its index, and the rule).
    Rule { index: usize, rule: Rule },
    /// No rule matched: denied by default.
    NoRule,
    /// The decider answered.
    Callback,
    /// The decider did not answer in time: its default applied.
    CallbackTimeout,
    /// The decider panicked or gave no answer: its default applied.
    CallbackFailed,
    /// An ask rule matched, but no decider is set: denied.
    NoCallback,
    /// The machine had as many connections as it may: denied.
    ConnectionLimit,
}

impl fmt::Display for DecidedBy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rule { index, rule } => write!(f, "rule {index} ({rule})"),
            Self::NoRule => f.write_str("no rule"),
            Self::Callback => f.write_str("callback"),
            Self::CallbackTimeout => f.write_str("callback timeout"),
            Self::CallbackFailed => f.write_str("callback failed"),
            Self::NoCallback => f.write_str("no callback"),
            Self::ConnectionLimit => f.write_str("connection limit"),
        }
    }
}

/// One guest connection, allowed or denied, logged when it ended (a denied
/// one at once).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionRecord {
    /// The guest's port.
    pub guest_port: u16,
    /// The host address the VMM connected to, or would have
    /// (`127.0.0.1` when the guest connected to its gateway).
    pub addr: Ipv4Addr,
    pub port: u16,
    /// The names the address went by for the guest when it connected: the
    /// caller's host entries, then the names it resolved through the VMM's
    /// DNS.
    pub names: Vec<String>,
    pub verdict: Verdict,
    pub decided_by: DecidedBy,
    /// Whether the host connection was made (false for every denied one,
    /// and for allowed ones the host refused or could not reach).
    pub connected: bool,
    /// Bytes from the guest that went to the host.
    pub bytes_to_host: u64,
    /// Bytes from the host that went to the guest.
    pub bytes_to_guest: u64,
    /// When the guest's SYN arrived.
    pub started: SystemTime,
    /// From the guest's SYN until the connection ended (until it was
    /// refused, for a denied one: the decider's time included).
    pub duration: Duration,
}

/// A machine's network counters, since it started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetworkStats {
    /// Connections allowed (by a rule or the decider).
    pub connections_allowed: u64,
    /// Connections denied, for whatever reason.
    pub connections_denied: u64,
    /// Allowed connections whose host connection failed.
    pub connections_failed: u64,
    /// Host connections being made or open now.
    pub connections_open: u64,
    /// Bytes relayed from the guest to the host, open connections included.
    pub bytes_to_host: u64,
    /// Bytes relayed from the host to the guest, open connections included.
    pub bytes_to_guest: u64,
    /// DNS queries the guest sent the VMM.
    pub dns_queries: u64,
    /// DNS queries answered NXDOMAIN because no rule could allow the name.
    pub dns_refused: u64,
    /// Connections the decider was asked about.
    pub decisions: u64,
    /// Of them, how many it did not answer in time.
    pub decisions_timed_out: u64,
    /// Of them, how many it answered with a panic or None.
    pub decisions_failed: u64,
    /// Log records dropped because the log was full.
    pub log_dropped: u64,
}

/// The counters, as the network thread updates them.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    pub(crate) connections_allowed: AtomicU64,
    pub(crate) connections_denied: AtomicU64,
    pub(crate) connections_failed: AtomicU64,
    pub(crate) connections_open: AtomicU64,
    pub(crate) bytes_to_host: AtomicU64,
    pub(crate) bytes_to_guest: AtomicU64,
    pub(crate) dns_queries: AtomicU64,
    pub(crate) dns_refused: AtomicU64,
    pub(crate) decisions: AtomicU64,
    pub(crate) decisions_timed_out: AtomicU64,
    pub(crate) decisions_failed: AtomicU64,
    pub(crate) log_dropped: AtomicU64,
}

/// Adds one to a counter.
pub(crate) fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

impl Counters {
    pub(crate) fn snapshot(&self) -> NetworkStats {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        NetworkStats {
            connections_allowed: get(&self.connections_allowed),
            connections_denied: get(&self.connections_denied),
            connections_failed: get(&self.connections_failed),
            connections_open: get(&self.connections_open),
            bytes_to_host: get(&self.bytes_to_host),
            bytes_to_guest: get(&self.bytes_to_guest),
            dns_queries: get(&self.dns_queries),
            dns_refused: get(&self.dns_refused),
            decisions: get(&self.decisions),
            decisions_timed_out: get(&self.decisions_timed_out),
            decisions_failed: get(&self.decisions_failed),
            log_dropped: get(&self.log_dropped),
        }
    }
}

/// The ring of records.
#[derive(Debug)]
pub(crate) struct Log {
    records: VecDeque<ConnectionRecord>,
    capacity: usize,
}

impl Log {
    pub(crate) fn new(capacity: usize) -> Self {
        Self { records: VecDeque::with_capacity(capacity.min(1024)), capacity }
    }

    /// Appends `record`; whether an older one (or this one, with no room
    /// at all) was dropped for it.
    pub(crate) fn push(&mut self, record: ConnectionRecord) -> bool {
        if self.capacity == 0 {
            return true;
        }
        let dropped = self.records.len() == self.capacity;
        if dropped {
            self.records.pop_front();
        }
        self.records.push_back(record);
        dropped
    }

    pub(crate) fn take(&mut self) -> Vec<ConnectionRecord> {
        self.records.drain(..).collect()
    }

    #[cfg(any(test, feature = "fuzzing"))]
    pub(crate) fn len(&self) -> usize {
        self.records.len()
    }

    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(port: u16) -> ConnectionRecord {
        ConnectionRecord {
            guest_port: 1,
            addr: Ipv4Addr::LOCALHOST,
            port,
            names: vec![],
            verdict: Verdict::Deny,
            decided_by: DecidedBy::NoRule,
            connected: false,
            bytes_to_host: 0,
            bytes_to_guest: 0,
            started: SystemTime::UNIX_EPOCH,
            duration: Duration::ZERO,
        }
    }

    #[test]
    fn the_log_keeps_the_newest_records() {
        let mut log = Log::new(3);
        let dropped: Vec<bool> = (0..5).map(|p| log.push(record(p))).collect();
        assert_eq!(dropped, [false, false, false, true, true]);
        assert_eq!(log.len(), 3);
        let ports: Vec<u16> = log.take().iter().map(|r| r.port).collect();
        assert_eq!(ports, [2, 3, 4]);
        assert_eq!(log.len(), 0);
        let mut none = Log::new(0);
        assert!(none.push(record(1)));
        assert!(none.take().is_empty());
    }

    #[test]
    fn decided_by_reads_well() {
        let rule = Rule {
            action: super::super::Action::Ask,
            host: super::super::HostPattern::Any,
            ports: 443.into(),
        };
        assert_eq!(DecidedBy::Rule { index: 2, rule }.to_string(), "rule 2 (ask *:443)");
        assert_eq!(DecidedBy::CallbackTimeout.to_string(), "callback timeout");
    }
}
