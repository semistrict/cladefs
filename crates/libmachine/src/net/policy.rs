// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The egress policy: which destinations the guest's connections may reach.
//!
//! An ordered list of rules, each allowing or denying a host pattern and a
//! port range; the first rule that matches decides, and a connection no rule
//! matches is denied. A sandbox therefore starts with no network reach at
//! all, and every destination it gets is one the caller named.
//!
//! Two kinds of address are kept out of reach of the broad patterns ("any
//! host", and DNS names): the host's own loopback, private networks,
//! link-local addresses (among them cloud metadata services such as
//! 169.254.169.254) and other addresses that are not globally routable.
//! Only an IP or CIDR rule that covers such an address explicitly can allow
//! it. "Allow any host" thus means "the internet", not the caller's LAN or
//! its localhost services (server-side request forgery), and a name rule
//! cannot be turned onto a private address by a DNS server that answers
//! with one (DNS rebinding). The caller's own host entries
//! ([`Hosts`](super::Hosts)) are the exception: a name rule matches the
//! addresses an entry gives its name, whatever they are, because the caller
//! set that mapping, as it would write an IP rule.
//!
//! A rule may also ask: an [`Action::Ask`] rule hands the connection to the
//! caller's [`Decider`](super::Decider), which allows or denies it.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::ops::RangeInclusive;
use std::str::FromStr;

use crate::error::{Error, Result};

/// What is decided for a connection, in the end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verdict {
    Allow,
    Deny,
}

/// What a rule does with the connections it matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Allow,
    Deny,
    /// The caller's [`Decider`](super::Decider) decides; without one, the
    /// connection is denied.
    Ask,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Ask => "ask",
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<Verdict> for Action {
    fn from(v: Verdict) -> Self {
        match v {
            Verdict::Allow => Self::Allow,
            Verdict::Deny => Self::Deny,
        }
    }
}

/// The destinations a rule covers.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HostPattern {
    /// Every globally routable address (written `*`).
    Any,
    /// One address, routable or not.
    Ip(IpAddr),
    /// A network, routable or not (`10.0.0.0/8`); host bits are cleared.
    Cidr { addr: IpAddr, prefix: u8 },
    /// A DNS name, exactly (`example.com`): the globally routable addresses
    /// the guest resolved that name to through the VMM's DNS server.
    Name(String),
    /// Every name under a domain, not the domain itself (`*.example.com`).
    Subdomains(String),
}

impl HostPattern {
    /// A name pattern: `name`, or every name under `domain` if written
    /// `*.domain`. Case and a trailing dot do not matter.
    pub fn name(name: &str) -> Result<Self> {
        match name.strip_prefix("*.") {
            Some(domain) => Ok(Self::Subdomains(normalize_name(domain)?)),
            None => Ok(Self::Name(normalize_name(name)?)),
        }
    }

    /// Whether the pattern covers a connection to `dest`: names the guest
    /// resolved through DNS count only for a global address, the caller's
    /// host entries for any.
    fn matches(&self, dest: &Destination<'_>) -> bool {
        let addr = dest.addr;
        let named = |m: &dyn Fn(&str) -> bool| {
            (is_global(addr) && dest.names.iter().any(|n| m(n)))
                || dest.host_names.iter().any(|n| m(n))
        };
        match self {
            Self::Any => is_global(addr),
            Self::Ip(ip) => canonical(*ip) == canonical(addr),
            Self::Cidr { addr: net, prefix } => in_network(addr, *net, *prefix),
            Self::Name(n) => named(&|m| m == n),
            Self::Subdomains(d) => named(&|m| is_subdomain(m, d)),
        }
    }

    /// Whether the pattern speaks about the name `name`, as the DNS server
    /// asks: [`Any`](Self::Any) and the name patterns can; IP and CIDR
    /// patterns never do.
    fn matches_name(&self, name: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Name(n) => n == name,
            Self::Subdomains(d) => is_subdomain(name, d),
            Self::Ip(_) | Self::Cidr { .. } => false,
        }
    }
}

impl From<IpAddr> for HostPattern {
    fn from(addr: IpAddr) -> Self {
        Self::Ip(addr)
    }
}

impl From<Ipv4Addr> for HostPattern {
    fn from(addr: Ipv4Addr) -> Self {
        Self::Ip(addr.into())
    }
}

impl From<Ipv6Addr> for HostPattern {
    fn from(addr: Ipv6Addr) -> Self {
        Self::Ip(addr.into())
    }
}

/// Parses `*`, an IP address, a CIDR network (`addr/prefix`), or a DNS name
/// (`*.domain` for every name under it).
impl FromStr for HostPattern {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        if s == "*" {
            return Ok(Self::Any);
        }
        if let Some((addr, prefix)) = s.split_once('/') {
            let invalid = || Error::InvalidConfig(format!("bad CIDR network {s:?}"));
            let addr: IpAddr = addr.parse().map_err(|_| invalid())?;
            let prefix: u8 = prefix.parse().map_err(|_| invalid())?;
            let max = if addr.is_ipv4() { 32 } else { 128 };
            if prefix > max {
                return Err(invalid());
            }
            return Ok(Self::Cidr { addr: mask(addr, prefix), prefix });
        }
        if let Ok(addr) = s.parse::<IpAddr>() {
            return Ok(Self::Ip(addr));
        }
        Self::name(s)
    }
}

impl fmt::Display for HostPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any => f.write_str("*"),
            Self::Ip(addr) => write!(f, "{addr}"),
            Self::Cidr { addr, prefix } => write!(f, "{addr}/{prefix}"),
            Self::Name(n) => f.write_str(n),
            Self::Subdomains(d) => write!(f, "*.{d}"),
        }
    }
}

/// A range of TCP or UDP ports, both ends included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ports {
    first: u16,
    last: u16,
}

impl Ports {
    /// Every port.
    pub const ANY: Ports = Ports { first: 0, last: u16::MAX };

    /// Ports `first` to `last`; an error if `first > last`.
    pub fn range(first: u16, last: u16) -> Result<Self> {
        if first > last {
            return Err(Error::InvalidConfig(format!("empty port range {first}-{last}")));
        }
        Ok(Self { first, last })
    }

    pub fn first(&self) -> u16 {
        self.first
    }

    pub fn last(&self) -> u16 {
        self.last
    }

    pub fn contains(&self, port: u16) -> bool {
        (self.first..=self.last).contains(&port)
    }

    fn is_any(&self) -> bool {
        *self == Self::ANY
    }
}

impl From<u16> for Ports {
    fn from(port: u16) -> Self {
        Self { first: port, last: port }
    }
}

/// An empty range (`10..=1`) covers no port at all.
impl From<RangeInclusive<u16>> for Ports {
    fn from(range: RangeInclusive<u16>) -> Self {
        Self::range(*range.start(), *range.end()).unwrap_or(Self { first: 1, last: 0 })
    }
}

/// Parses `*`, a port (`443`) or a range (`8000-8080`).
impl FromStr for Ports {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        if s == "*" {
            return Ok(Self::ANY);
        }
        let invalid = || Error::InvalidConfig(format!("bad port or port range {s:?}"));
        match s.split_once('-') {
            Some((a, b)) => {
                Self::range(a.parse().map_err(|_| invalid())?, b.parse().map_err(|_| invalid())?)
            }
            None => Ok(Self::from(s.parse::<u16>().map_err(|_| invalid())?)),
        }
    }
}

impl fmt::Display for Ports {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_any() {
            f.write_str("*")
        } else if self.first == self.last {
            write!(f, "{}", self.first)
        } else {
            write!(f, "{}-{}", self.first, self.last)
        }
    }
}

/// One rule of an [`EgressPolicy`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Rule {
    pub action: Action,
    pub host: HostPattern,
    pub ports: Ports,
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}:{}", self.action, self.host, self.ports)
    }
}

/// What the policy says about a connection: the action, and the index of
/// the rule that matched (None: no rule did, and the action is Deny).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyMatch {
    pub action: Action,
    pub rule: Option<usize>,
}

/// A connection the guest asks for, as the policy sees it.
#[derive(Debug, Clone, Copy)]
pub struct Destination<'a> {
    /// The host address the VMM would connect to (the guest's gateway
    /// address stands for the host's loopback, `127.0.0.1`).
    pub addr: IpAddr,
    pub port: u16,
    /// The names the guest resolved to `addr` through the VMM's DNS server
    /// (and that have not expired); they count only if `addr` is global.
    pub names: &'a [String],
    /// The names the caller's host entries give `addr`; they count whatever
    /// the address.
    pub host_names: &'a [String],
}

/// Which destinations the guest may connect to: rules in order, the first
/// that matches deciding, and anything no rule matches denied. The default
/// policy allows nothing.
///
/// ```
/// use libmachine::net::{EgressPolicy, HostPattern, Ports};
/// let policy = EgressPolicy::deny_all()
///     .allow(HostPattern::name("*.pypi.org")?, 443)
///     .allow("127.0.0.1".parse::<std::net::IpAddr>()?, 8080)
///     .deny("*".parse::<HostPattern>()?, 25)
///     .allow(HostPattern::Any, 443)
///     .ask("0.0.0.0/0".parse::<HostPattern>()?, Ports::ANY);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EgressPolicy {
    rules: Vec<Rule>,
}

impl EgressPolicy {
    /// No destination at all (the default).
    pub fn deny_all() -> Self {
        Self::default()
    }

    /// Every globally routable address, on every port; still not the host's
    /// loopback or private networks (see the [module](self) notes).
    pub fn allow_all() -> Self {
        Self::deny_all().allow(HostPattern::Any, Ports::ANY)
    }

    /// Appends a rule allowing `host` on `ports`.
    pub fn allow(self, host: impl Into<HostPattern>, ports: impl Into<Ports>) -> Self {
        self.with(Action::Allow, host.into(), ports.into())
    }

    /// Appends a rule denying `host` on `ports`.
    pub fn deny(self, host: impl Into<HostPattern>, ports: impl Into<Ports>) -> Self {
        self.with(Action::Deny, host.into(), ports.into())
    }

    /// Appends a rule handing connections to `host` on `ports` to the
    /// caller's [`Decider`](super::Decider) (denied if there is none).
    pub fn ask(self, host: impl Into<HostPattern>, ports: impl Into<Ports>) -> Self {
        self.with(Action::Ask, host.into(), ports.into())
    }

    fn with(mut self, action: Action, host: HostPattern, ports: Ports) -> Self {
        self.rules.push(Rule { action, host, ports });
        self
    }

    /// Appends `rule`.
    pub fn push(&mut self, rule: Rule) {
        self.rules.push(rule);
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// What the policy says about a connection to `dest`: the first rule
    /// that matches, or Deny.
    pub fn check(&self, dest: &Destination<'_>) -> PolicyMatch {
        self.rules.iter().position(|r| r.ports.contains(dest.port) && r.host.matches(dest)).map_or(
            PolicyMatch { action: Action::Deny, rule: None },
            |i| PolicyMatch { action: self.rules[i].action, rule: Some(i) },
        )
    }

    /// Whether the VMM's DNS server resolves `name` (lowercase, no trailing
    /// dot) for the guest: only if a rule could allow connections to it
    /// (an allow or an ask rule).
    /// The first rule that speaks about the name decides, unless it denies
    /// only some ports, in which case later rules may still allow others.
    pub fn resolves(&self, name: &str) -> bool {
        for rule in self.rules.iter().filter(|r| r.host.matches_name(name)) {
            match rule.action {
                Action::Allow | Action::Ask => return true,
                Action::Deny if rule.ports.is_any() => return false,
                Action::Deny => {}
            }
        }
        false
    }
}

impl fmt::Display for EgressPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, r) in self.rules.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{r}")?;
        }
        if !self.rules.is_empty() {
            f.write_str(", ")?;
        }
        f.write_str("deny *")
    }
}

/// A name as rules hold it: lowercase, no trailing dot, made of DNS labels
/// of letters, digits, `-` and `_`.
pub(crate) fn normalize_name(name: &str) -> Result<String> {
    let name = name.strip_suffix('.').unwrap_or(name).to_ascii_lowercase();
    let label_ok = |l: &str| {
        (1..=63).contains(&l.len())
            && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    };
    if name.is_empty() || name.len() > 253 || !name.split('.').all(label_ok) {
        return Err(Error::InvalidConfig(format!("bad DNS name {name:?}")));
    }
    Ok(name)
}

fn is_subdomain(name: &str, domain: &str) -> bool {
    name.len() > domain.len() + 1
        && name.ends_with(domain)
        && name.as_bytes()[name.len() - domain.len() - 1] == b'.'
}

/// An IPv4-mapped IPv6 address as the IPv4 address it is.
fn canonical(addr: IpAddr) -> IpAddr {
    match addr {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(addr, IpAddr::V4),
        v4 => v4,
    }
}

fn mask(addr: IpAddr, prefix: u8) -> IpAddr {
    match addr {
        IpAddr::V4(a) => {
            let bits = u32::from(a);
            let m = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
            IpAddr::V4((bits & m).into())
        }
        IpAddr::V6(a) => {
            let bits = u128::from(a);
            let m = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
            IpAddr::V6((bits & m).into())
        }
    }
}

fn in_network(addr: IpAddr, net: IpAddr, prefix: u8) -> bool {
    let addr = canonical(addr);
    match (addr, net) {
        (IpAddr::V4(_), IpAddr::V4(_)) | (IpAddr::V6(_), IpAddr::V6(_)) => {
            mask(addr, prefix) == net
        }
        _ => false,
    }
}

/// Whether `addr` is globally routable: not loopback, unspecified, private,
/// shared (carrier-grade NAT), link-local, documentation, benchmarking,
/// multicast, broadcast or reserved (the IANA special-purpose registries).
pub fn is_global(addr: IpAddr) -> bool {
    match canonical(addr) {
        IpAddr::V4(a) => {
            let special: [(u32, u8); 14] = [
                (0x0000_0000, 8),  // "this network"
                (0x0a00_0000, 8),  // private
                (0x6440_0000, 10), // shared address space (CGNAT)
                (0x7f00_0000, 8),  // loopback
                (0xa9fe_0000, 16), // link-local (cloud metadata)
                (0xac10_0000, 12), // private
                (0xc000_0000, 24), // IETF protocol assignments
                (0xc000_0200, 24), // documentation
                (0xc058_6300, 24), // 6to4 relay anycast (deprecated)
                (0xc0a8_0000, 16), // private
                (0xc612_0000, 15), // benchmarking
                (0xc633_6400, 24), // documentation
                (0xcb00_7100, 24), // documentation
                (0xe000_0000, 3),  // multicast, reserved, broadcast
            ];
            let bits = u32::from(a);
            !special.iter().any(|&(net, prefix)| {
                let m = u32::MAX << (32 - u32::from(prefix));
                bits & m == net
            })
        }
        IpAddr::V6(a) => {
            let bits = u128::from(a);
            let special: [(u128, u8); 9] = [
                (0, 127),                     // unspecified, loopback
                (0x0064_ff9b_0001 << 80, 48), // local-use NAT64
                (0x0100 << 112, 64),          // discard-only
                (0x2001_0000 << 96, 23),      // IETF protocol assignments
                (0x2001_0db8 << 96, 32),      // documentation
                (0x2002 << 112, 16),          // 6to4 (deprecated)
                (0xfc00 << 112, 7),           // unique local
                (0xfe80 << 112, 10),          // link-local
                (0xff00 << 112, 8),           // multicast
            ];
            !special.iter().any(|&(net, prefix)| {
                let m = u128::MAX << (128 - u32::from(prefix));
                bits & m == net
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn check(p: &EgressPolicy, addr: &str, port: u16, names: &[&str]) -> Verdict {
        check_with(p, addr, port, names, &[])
    }

    fn check_with(
        p: &EgressPolicy,
        addr: &str,
        port: u16,
        names: &[&str],
        host_names: &[&str],
    ) -> Verdict {
        let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        let host_names: Vec<String> = host_names.iter().map(|s| s.to_string()).collect();
        let m =
            p.check(&Destination { addr: ip(addr), port, names: &names, host_names: &host_names });
        match m.action {
            Action::Allow => Verdict::Allow,
            Action::Deny => Verdict::Deny,
            Action::Ask => panic!("ask in a test without one"),
        }
    }

    #[test]
    fn default_denies_everything() {
        let p = EgressPolicy::default();
        for addr in ["1.1.1.1", "127.0.0.1", "10.0.0.1", "2606:4700::1111"] {
            assert_eq!(check(&p, addr, 443, &[]), Verdict::Deny, "{addr}");
        }
        assert!(!p.resolves("example.com"));
        assert_eq!(p.to_string(), "deny *");
    }

    #[test]
    fn first_matching_rule_wins() {
        let p =
            EgressPolicy::deny_all().deny(ip("1.1.1.1"), 22).allow(HostPattern::Any, Ports::ANY);
        assert_eq!(check(&p, "1.1.1.1", 22, &[]), Verdict::Deny);
        assert_eq!(check(&p, "1.1.1.1", 443, &[]), Verdict::Allow);
        assert_eq!(check(&p, "8.8.8.8", 22, &[]), Verdict::Allow);
        assert_eq!(p.to_string(), "deny 1.1.1.1:22, allow *:*, deny *");
    }

    #[test]
    fn any_and_names_never_reach_non_global_addresses() {
        let p = EgressPolicy::allow_all().allow(HostPattern::name("db.example").unwrap(), 5432);
        for addr in [
            "127.0.0.1",
            "0.0.0.0",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fe80::1",
            "fd00::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            assert_eq!(check(&p, addr, 5432, &["db.example"]), Verdict::Deny, "{addr}");
        }
        for addr in ["1.1.1.1", "8.8.4.4", "2606:4700::1111", "::ffff:1.1.1.1"] {
            assert_eq!(check(&p, addr, 80, &[]), Verdict::Allow, "{addr}");
        }
    }

    #[test]
    fn explicit_ip_and_cidr_rules_reach_any_address() {
        let p = EgressPolicy::deny_all()
            .allow(ip("127.0.0.1"), 8080)
            .allow("192.168.0.0/16".parse::<HostPattern>().unwrap(), 1000..=2000);
        assert_eq!(check(&p, "127.0.0.1", 8080, &[]), Verdict::Allow);
        assert_eq!(check(&p, "::ffff:127.0.0.1", 8080, &[]), Verdict::Allow);
        assert_eq!(check(&p, "127.0.0.1", 8081, &[]), Verdict::Deny);
        assert_eq!(check(&p, "127.0.0.2", 8080, &[]), Verdict::Deny);
        assert_eq!(check(&p, "192.168.4.5", 1500, &[]), Verdict::Allow);
        assert_eq!(check(&p, "192.168.4.5", 2001, &[]), Verdict::Deny);
        assert_eq!(check(&p, "192.169.4.5", 1500, &[]), Verdict::Deny);
    }

    #[test]
    fn name_rules_match_resolved_names() {
        let p = EgressPolicy::deny_all()
            .allow(HostPattern::name("Example.COM.").unwrap(), 443)
            .allow(HostPattern::name("*.cdn.example").unwrap(), Ports::ANY);
        assert_eq!(check(&p, "93.184.216.34", 443, &["example.com"]), Verdict::Allow);
        assert_eq!(check(&p, "93.184.216.34", 80, &["example.com"]), Verdict::Deny);
        assert_eq!(check(&p, "93.184.216.34", 443, &[]), Verdict::Deny);
        assert_eq!(check(&p, "93.184.216.34", 443, &["www.example.com"]), Verdict::Deny);
        assert_eq!(check(&p, "1.2.3.4", 1, &["a.b.cdn.example"]), Verdict::Allow);
        assert_eq!(check(&p, "1.2.3.4", 1, &["cdn.example"]), Verdict::Deny);
        assert_eq!(check(&p, "1.2.3.4", 1, &["xcdn.example"]), Verdict::Deny);
    }

    #[test]
    fn dns_resolves_only_names_a_rule_could_allow() {
        let p = EgressPolicy::deny_all()
            .deny(HostPattern::name("evil.example").unwrap(), Ports::ANY)
            .deny(HostPattern::name("mail.example").unwrap(), 25)
            .allow(HostPattern::name("*.example").unwrap(), 443)
            .allow(ip("1.2.3.4"), Ports::ANY);
        assert!(!p.resolves("evil.example"));
        assert!(p.resolves("mail.example"), "port 443 is still allowed");
        assert!(p.resolves("www.example"));
        assert!(!p.resolves("example"));
        assert!(!p.resolves("other.test"), "IP rules do not speak about names");
        assert!(EgressPolicy::allow_all().resolves("anything.test"));
    }

    /// Host entries are the caller's: a name rule matches their addresses,
    /// private or loopback ones included; learnt names still only reach
    /// global addresses, and `*` never reaches a non-global one.
    #[test]
    fn name_rules_match_host_entries_on_any_address() {
        let p = EgressPolicy::deny_all()
            .allow(HostPattern::name("mock.test").unwrap(), 8080)
            .allow(HostPattern::name("*.svc").unwrap(), Ports::ANY)
            .allow(HostPattern::Any, Ports::ANY);
        assert_eq!(check_with(&p, "127.0.0.1", 8080, &[], &["mock.test"]), Verdict::Allow);
        assert_eq!(
            check_with(&p, "127.0.0.1", 8081, &[], &["mock.test"]),
            Verdict::Deny,
            "only the rule's ports"
        );
        assert_eq!(check_with(&p, "10.1.2.3", 1, &[], &["db.svc"]), Verdict::Allow);
        assert_eq!(
            check_with(&p, "127.0.0.1", 8080, &["mock.test"], &[]),
            Verdict::Deny,
            "a name learnt from DNS never reaches loopback"
        );
        assert_eq!(
            check_with(&p, "127.0.0.1", 8080, &[], &["other.test"]),
            Verdict::Deny,
            "* does not reach loopback through an entry"
        );
    }

    /// An ask rule is matched like the others, first match winning; no
    /// matching rule still denies; DNS resolves names an ask rule covers.
    #[test]
    fn ask_rules() {
        let p = EgressPolicy::deny_all()
            .allow(ip("1.1.1.1"), 443)
            .deny(ip("1.1.1.1"), Ports::ANY)
            .ask(HostPattern::name("*.example").unwrap(), 443)
            .ask("0.0.0.0/0".parse::<HostPattern>().unwrap(), 1..=1024);
        let at = |addr: &str, port, names: &[&str]| {
            let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
            p.check(&Destination { addr: ip(addr), port, names: &names, host_names: &[] })
        };
        let m = |action, rule| PolicyMatch { action, rule };
        assert_eq!(at("1.1.1.1", 443, &[]), m(Action::Allow, Some(0)));
        assert_eq!(at("1.1.1.1", 80, &[]), m(Action::Deny, Some(1)));
        assert_eq!(at("93.184.216.34", 443, &["www.example"]), m(Action::Ask, Some(2)));
        assert_eq!(at("127.0.0.1", 80, &[]), m(Action::Ask, Some(3)));
        assert_eq!(at("127.0.0.1", 8080, &[]), m(Action::Deny, None));
        assert!(p.resolves("www.example"));
        assert!(!p.resolves("other.test"));
        assert_eq!(
            p.to_string(),
            "allow 1.1.1.1:443, deny 1.1.1.1:*, ask *.example:443, ask 0.0.0.0/0:1-1024, deny *"
        );
    }

    #[test]
    fn parsing() {
        assert_eq!("*".parse::<HostPattern>().unwrap(), HostPattern::Any);
        assert_eq!(
            "10.1.2.3/8".parse::<HostPattern>().unwrap(),
            HostPattern::Cidr { addr: ip("10.0.0.0"), prefix: 8 }
        );
        assert_eq!("fd00::1/0".parse::<HostPattern>().unwrap().to_string(), "::/0");
        assert_eq!("::1".parse::<HostPattern>().unwrap(), HostPattern::Ip(ip("::1")));
        assert_eq!(
            "*.Foo.Bar".parse::<HostPattern>().unwrap(),
            HostPattern::Subdomains("foo.bar".into())
        );
        for bad in ["", "1.2.3.4/33", "::/129", "x/8", "a..b", "-/", "has space", "*.", "**.x"] {
            assert!(bad.parse::<HostPattern>().is_err(), "{bad:?}");
        }
        assert!(HostPattern::name(&"a".repeat(64)).is_err());
        assert_eq!("*".parse::<Ports>().unwrap(), Ports::ANY);
        assert_eq!("443".parse::<Ports>().unwrap(), Ports::from(443));
        assert_eq!("80-90".parse::<Ports>().unwrap(), Ports::range(80, 90).unwrap());
        for bad in ["", "90-80", "65536", "a", "1-"] {
            assert!(bad.parse::<Ports>().is_err(), "{bad:?}");
        }
        #[allow(clippy::reversed_empty_ranges)]
        let empty = Ports::from(10..=1);
        assert!(!(0..=u16::MAX).any(|p| empty.contains(p)));
    }
}
