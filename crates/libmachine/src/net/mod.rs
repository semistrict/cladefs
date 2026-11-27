// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The guest's network: a virtio-net card whose other end is a userspace
//! TCP/IP stack in this process (smoltcp), and the egress policy that
//! decides where the guest may connect.
//!
//! A machine has a network only if [`MachineConfig::network`] says so.
//! The guest then sits on a point-to-point link, `10.0.2.0/30`:
//!
//! | Address | What |
//! |---|---|
//! | [`GUEST_ADDR`] `10.0.2.2` | the guest, configured by the kernel command line (`ip=`) |
//! | [`GATEWAY_ADDR`] `10.0.2.1` | the VMM: the guest's default route and DNS server, and the host's loopback |
//!
//! The VMM terminates the guest's TCP connections and makes each one again
//! from the host, with an ordinary socket, if the [`EgressPolicy`] allows
//! its destination; a connection to the gateway address is made to the
//! host's `127.0.0.1`. A denied connection is refused at once (the guest
//! gets a RST). Nothing else leaves the process: no TAP device, no host
//! privileges, no packets of the guest's own on the host's network. The
//! VMM answers DNS queries (A records, from the host's resolver) for names
//! the policy could allow, and NXDOMAIN for the rest. UDP goes no further
//! than that DNS server, and the link is IPv4 only.
//!
//! Around the policy:
//! - [`Hosts`]: names the caller gives addresses, which the DNS server
//!   answers with (a mock service on the host's loopback, a fixed IP).
//! - [`Decider`]: a callback deciding the connections an
//!   [`Action::Ask`] rule hands it, with a timeout and a default.
//! - The connection log ([`Network::take_log`]) and counters
//!   ([`Network::stats`]).
//! - Names learnt from the DNS server last as long as the TTL it gave the
//!   guest ([`NetworkConfig::dns_ttl`]): after that, a name rule no longer
//!   matches the address the name had, for new connections.
//!
//! [`MachineConfig::network`]: crate::MachineConfig::network

mod decide;
mod dns;
#[cfg(any(test, feature = "fuzzing"))]
#[doc(hidden)]
pub mod fuzz;
pub(crate) mod host;
mod hosts;
mod log;
mod policy;
pub(crate) mod stack;

use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

pub use decide::{ConnectionRequest, Decider};
pub use hosts::{Hosts, MAX_ADDRS_PER_HOST};
pub use log::{ConnectionRecord, DecidedBy, NetworkStats};
pub use policy::{
    Action, Destination, EgressPolicy, HostPattern, PolicyMatch, Ports, Rule, Verdict, is_global,
};

use crate::error::{Error, Result};

/// The guest's address.
pub const GUEST_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
/// The VMM's address on the guest's link: its gateway and DNS server, and
/// the host's loopback.
pub const GATEWAY_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 1);
/// The link's prefix length (`255.255.255.252`).
pub const PREFIX_LEN: u8 = 30;
/// The link's MTU, in bytes of IP packet.
pub const MTU: u16 = 65520;

/// A machine's network, as [`MachineConfig::network`] asks for it.
///
/// ```
/// use std::net::Ipv4Addr;
/// use std::time::Duration;
/// use libmachine::net::{EgressPolicy, Hosts, HostPattern, NetworkConfig};
/// let config = NetworkConfig::new(
///     EgressPolicy::deny_all().allow(HostPattern::name("mock.service")?, 8080),
/// )
/// .with_hosts(Hosts::new().with("mock.service", Ipv4Addr::LOCALHOST)?)
/// .with_dns_ttl(Duration::from_secs(30));
/// # Ok::<(), libmachine::Error>(())
/// ```
///
/// [`MachineConfig::network`]: crate::MachineConfig::network
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkConfig {
    /// Where the guest may connect; by default nowhere.
    pub policy: EgressPolicy,
    /// Names the VMM's DNS server answers itself; by default none.
    pub hosts: Hosts,
    /// The TTL of every DNS answer the guest gets, and how long a name
    /// learnt from one keeps matching the policy's name rules for the
    /// addresses it gave. Whole seconds, from 1 s to a day; by default
    /// [`DEFAULT_DNS_TTL`](Self::DEFAULT_DNS_TTL).
    pub dns_ttl: Duration,
    /// How many connection records the log keeps until the caller takes
    /// them ([`Network::take_log`]); 0 keeps none. By default
    /// [`DEFAULT_LOG_CAPACITY`](Self::DEFAULT_LOG_CAPACITY).
    pub log_capacity: usize,
    /// Decides the connections ask rules hand it; by default none (they
    /// are denied).
    pub decider: Option<Decider>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            policy: EgressPolicy::default(),
            hosts: Hosts::default(),
            dns_ttl: Self::DEFAULT_DNS_TTL,
            log_capacity: Self::DEFAULT_LOG_CAPACITY,
            decider: None,
        }
    }
}

impl NetworkConfig {
    pub const DEFAULT_DNS_TTL: Duration = Duration::from_secs(60);
    pub const MAX_DNS_TTL: Duration = Duration::from_secs(24 * 60 * 60);
    pub const DEFAULT_LOG_CAPACITY: usize = 1024;
    pub const MAX_LOG_CAPACITY: usize = 1 << 20;

    /// A network applying `policy`, the rest by default.
    pub fn new(policy: EgressPolicy) -> Self {
        Self { policy, ..Self::default() }
    }

    pub fn with_hosts(mut self, hosts: Hosts) -> Self {
        self.hosts = hosts;
        self
    }

    pub fn with_dns_ttl(mut self, ttl: Duration) -> Self {
        self.dns_ttl = ttl;
        self
    }

    pub fn with_log_capacity(mut self, capacity: usize) -> Self {
        self.log_capacity = capacity;
        self
    }

    pub fn with_decider(mut self, decider: Decider) -> Self {
        self.decider = Some(decider);
        self
    }

    /// Whether the configuration is one a machine can have (as making the
    /// machine checks).
    pub fn validate(&self) -> Result<()> {
        if self.dns_ttl < Duration::from_secs(1)
            || self.dns_ttl > Self::MAX_DNS_TTL
            || self.dns_ttl.subsec_nanos() != 0
        {
            return Err(Error::InvalidConfig(format!(
                "dns_ttl must be whole seconds from 1 s to a day, not {:?}",
                self.dns_ttl
            )));
        }
        if self.log_capacity > Self::MAX_LOG_CAPACITY {
            return Err(Error::InvalidConfig(format!(
                "log_capacity is at most {}, not {}",
                Self::MAX_LOG_CAPACITY,
                self.log_capacity
            )));
        }
        Ok(())
    }
}

/// What the [`Network`] handle, the virtio-net device and the network
/// thread share.
#[derive(Debug)]
pub(crate) struct Shared {
    pub(crate) policy: RwLock<EgressPolicy>,
    pub(crate) hosts: RwLock<Hosts>,
    pub(crate) decider: RwLock<Option<Decider>>,
    /// Fixed for the machine's life.
    pub(crate) dns_ttl: Duration,
    pub(crate) log: Mutex<log::Log>,
    pub(crate) counters: log::Counters,
}

impl Shared {
    pub(crate) fn new(config: NetworkConfig) -> Result<Arc<Self>> {
        config.validate()?;
        Ok(Arc::new(Self {
            policy: RwLock::new(config.policy),
            hosts: RwLock::new(config.hosts),
            decider: RwLock::new(config.decider),
            dns_ttl: config.dns_ttl,
            log: Mutex::new(log::Log::new(config.log_capacity)),
            counters: log::Counters::default(),
        }))
    }

    pub(crate) fn policy(&self) -> std::sync::RwLockReadGuard<'_, EgressPolicy> {
        self.policy.read().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn hosts(&self) -> std::sync::RwLockReadGuard<'_, Hosts> {
        self.hosts.read().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn decider(&self) -> Option<Decider> {
        self.decider.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(crate) fn log(&self) -> std::sync::MutexGuard<'_, log::Log> {
        self.log.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Logs a finished connection.
    pub(crate) fn record(&self, record: ConnectionRecord) {
        if self.log().push(record) {
            log::bump(&self.counters.log_dropped);
        }
    }
}

/// The handle to a machine's network ([`Machine::network`]): usable from
/// any thread, including while another runs the machine.
///
/// [`Machine::network`]: crate::Machine::network
#[derive(Debug, Clone)]
pub struct Network {
    shared: Arc<Shared>,
}

impl Network {
    pub(crate) fn new(config: NetworkConfig) -> Result<Self> {
        Ok(Self { shared: Shared::new(config)? })
    }

    pub(crate) fn shared(&self) -> Arc<Shared> {
        self.shared.clone()
    }

    /// The policy in force.
    pub fn policy(&self) -> EgressPolicy {
        self.shared.policy().clone()
    }

    /// Replaces the policy. It applies to connections and DNS queries from
    /// now on; connections already open stay open.
    pub fn set_policy(&self, policy: EgressPolicy) {
        *self.shared.policy.write().unwrap_or_else(|e| e.into_inner()) = policy;
    }

    /// The host entries in force.
    pub fn hosts(&self) -> Hosts {
        self.shared.hosts().clone()
    }

    /// Replaces the host entries, for DNS queries and connections from now
    /// on.
    pub fn set_hosts(&self, hosts: Hosts) {
        *self.shared.hosts.write().unwrap_or_else(|e| e.into_inner()) = hosts;
    }

    /// The decider in force, if any.
    pub fn decider(&self) -> Option<Decider> {
        self.shared.decider()
    }

    /// Sets (or, with None, removes) the decider for connections ask rules
    /// hand it from now on. Questions already asked are answered by the
    /// decider they were asked of.
    pub fn set_decider(&self, decider: Option<Decider>) {
        *self.shared.decider.write().unwrap_or_else(|e| e.into_inner()) = decider;
    }

    /// Takes the connection records logged since the last call, oldest
    /// first.
    pub fn take_log(&self) -> Vec<ConnectionRecord> {
        self.shared.log().take()
    }

    /// How many records the log keeps at most.
    pub fn log_capacity(&self) -> usize {
        self.shared.log().capacity()
    }

    /// The counters since the machine started.
    pub fn stats(&self) -> NetworkStats {
        self.shared.counters.snapshot()
    }

    /// The TTL of the DNS server's answers.
    pub fn dns_ttl(&self) -> Duration {
        self.shared.dns_ttl
    }
}

/// The guest's host name, which the kernel command line sets with its
/// address. Without one the kernel names the guest after its address
/// (`10.0.2.2`), and resolvers take `0.2.2` for its DNS domain, searching
/// `name.0.2.2` for every single-label name.
pub const GUEST_HOSTNAME: &str = "guest";

/// The kernel command line parameter that configures the guest's address,
/// host name, route and DNS server (`CONFIG_IP_PNP`): the first network
/// device, static, no autoconfiguration.
pub(crate) fn kernel_ip_param() -> String {
    let mask = Ipv4Addr::from(u32::MAX << (32 - u32::from(PREFIX_LEN)));
    format!("ip={GUEST_ADDR}::{GATEWAY_ADDR}:{mask}:{GUEST_HOSTNAME}::off:{GATEWAY_ADDR}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_parameter() {
        assert_eq!(kernel_ip_param(), "ip=10.0.2.2::10.0.2.1:255.255.255.252:guest::off:10.0.2.1");
    }

    #[test]
    fn policy_changes_through_the_handle() {
        let net = Network::new(NetworkConfig::default()).unwrap();
        let clone = net.clone();
        clone.set_policy(EgressPolicy::allow_all());
        assert_eq!(net.policy(), EgressPolicy::allow_all());
        let hosts = Hosts::new().with("a.test", Ipv4Addr::LOCALHOST).unwrap();
        clone.set_hosts(hosts.clone());
        assert_eq!(net.hosts(), hosts);
        let decider = Decider::new(|_| None);
        clone.set_decider(Some(decider.clone()));
        assert_eq!(net.decider(), Some(decider));
        assert_eq!(net.stats(), NetworkStats::default());
        assert!(net.take_log().is_empty());
        assert_eq!(net.log_capacity(), NetworkConfig::DEFAULT_LOG_CAPACITY);
        assert_eq!(net.dns_ttl(), NetworkConfig::DEFAULT_DNS_TTL);
    }

    #[test]
    fn configs_are_validated() {
        let bad = |c: NetworkConfig| Network::new(c).is_err();
        assert!(bad(NetworkConfig::default().with_dns_ttl(Duration::ZERO)));
        assert!(bad(NetworkConfig::default().with_dns_ttl(Duration::from_millis(1500))));
        assert!(bad(NetworkConfig::default().with_dns_ttl(NetworkConfig::MAX_DNS_TTL * 2)));
        assert!(bad(
            NetworkConfig::default().with_log_capacity(NetworkConfig::MAX_LOG_CAPACITY + 1)
        ));
        assert!(!bad(NetworkConfig::default()
            .with_dns_ttl(Duration::from_secs(1))
            .with_log_capacity(0)));
    }
}
