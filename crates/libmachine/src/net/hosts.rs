// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Host entries: names the caller gives addresses, which the VMM's DNS
//! server answers with instead of asking the host's resolver (like
//! `/etc/hosts`), subject to the egress policy like any other name.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use super::policy::normalize_name;
use super::{GATEWAY_ADDR, PREFIX_LEN};
use crate::error::{Error, Result};

/// The most addresses one name may have: what fits in a DNS answer over
/// UDP for any name is more, but an entry is a mapping the caller writes
/// by hand.
pub const MAX_ADDRS_PER_HOST: usize = 16;

/// Names and the IPv4 addresses the VMM connects to for them.
///
/// An address is one the VMM can connect to from the host: `127.0.0.1` is
/// the host's loopback, which the guest reaches through its gateway
/// address, so the VMM's DNS answers it as `10.0.2.1`. Other loopback
/// addresses (the guest cannot reach them), the guest's own link
/// `10.0.2.0/30`, `0.0.0.0`, multicast and broadcast addresses are refused.
///
/// A name rule of the policy matches an entry's addresses whatever they
/// are, private and loopback ones included: the caller set the mapping, as
/// it would write an IP rule. `*` still matches only global addresses.
///
/// ```
/// use std::net::Ipv4Addr;
/// use libmachine::net::Hosts;
/// let hosts = Hosts::new()
///     .with("mock.service", Ipv4Addr::LOCALHOST)?
///     .with("db.internal", Ipv4Addr::new(192, 168, 1, 20))?;
/// assert_eq!(hosts.get("MOCK.service."), [Ipv4Addr::LOCALHOST]);
/// # Ok::<(), libmachine::Error>(())
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hosts {
    by_name: BTreeMap<String, Vec<Ipv4Addr>>,
}

impl Hosts {
    /// No entries.
    pub fn new() -> Self {
        Self::default()
    }

    /// Gives `name` the address `addr` too (a name may have several).
    pub fn insert(&mut self, name: &str, addr: Ipv4Addr) -> Result<()> {
        let name = normalize_name(name)?;
        check_addr(addr)?;
        let addrs = self.by_name.entry(name).or_default();
        if addrs.contains(&addr) {
            return Ok(());
        }
        if addrs.len() == MAX_ADDRS_PER_HOST {
            return Err(Error::InvalidConfig(format!(
                "a host entry has at most {MAX_ADDRS_PER_HOST} addresses"
            )));
        }
        addrs.push(addr);
        Ok(())
    }

    /// [`insert`](Self::insert), for chaining.
    pub fn with(mut self, name: &str, addr: Ipv4Addr) -> Result<Self> {
        self.insert(name, addr)?;
        Ok(self)
    }

    /// Removes `name`'s entry; whether there was one.
    pub fn remove(&mut self, name: &str) -> bool {
        normalize_name(name).is_ok_and(|n| self.by_name.remove(&n).is_some())
    }

    /// `name`'s addresses (none if it has no entry).
    pub fn get(&self, name: &str) -> &[Ipv4Addr] {
        match normalize_name(name) {
            Ok(n) => self.by_name.get(&n).map_or(&[], Vec::as_slice),
            Err(_) => &[],
        }
    }

    /// The entries, by name.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &[Ipv4Addr])> {
        self.by_name.iter().map(|(n, a)| (n.as_str(), a.as_slice()))
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// The addresses as the guest is told them, for a name already
    /// normalized (as DNS queries are).
    pub(crate) fn guest_addrs(&self, name: &str) -> Option<Vec<Ipv4Addr>> {
        self.by_name.get(name).map(|addrs| addrs.iter().map(|&a| guest_addr(a)).collect())
    }

    /// The names whose entries give `addr` (an address the VMM connects
    /// to), for the policy's name rules.
    pub(crate) fn names_for(&self, addr: Ipv4Addr) -> Vec<String> {
        self.by_name
            .iter()
            .filter(|(_, addrs)| addrs.contains(&addr))
            .map(|(n, _)| n.clone())
            .collect()
    }
}

/// The address the guest uses for a host address: the gateway for the
/// host's loopback.
fn guest_addr(addr: Ipv4Addr) -> Ipv4Addr {
    if addr == Ipv4Addr::LOCALHOST { GATEWAY_ADDR } else { addr }
}

fn check_addr(addr: Ipv4Addr) -> Result<()> {
    let link = u32::from(GATEWAY_ADDR) & (u32::MAX << (32 - u32::from(PREFIX_LEN)));
    let on_link = u32::from(addr) & (u32::MAX << (32 - u32::from(PREFIX_LEN))) == link;
    let why = if addr.is_loopback() && addr != Ipv4Addr::LOCALHOST {
        "the guest reaches the host's loopback only as 127.0.0.1"
    } else if on_link {
        "it is on the guest's own link (use 127.0.0.1 for the host's loopback)"
    } else if addr.is_unspecified() || addr.is_multicast() || addr.is_broadcast() {
        "it is not a host to connect to"
    } else {
        return Ok(());
    };
    Err(Error::InvalidConfig(format!("host entry address {addr}: {why}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_are_normalized_and_validated() {
        let mut h = Hosts::new();
        h.insert("Mock.Test.", Ipv4Addr::LOCALHOST).unwrap();
        h.insert("mock.test", Ipv4Addr::LOCALHOST).unwrap();
        h.insert("mock.test", Ipv4Addr::new(192, 168, 0, 9)).unwrap();
        assert_eq!(h.get("MOCK.test"), [Ipv4Addr::LOCALHOST, Ipv4Addr::new(192, 168, 0, 9)]);
        assert_eq!(
            h.guest_addrs("mock.test").unwrap(),
            [GATEWAY_ADDR, Ipv4Addr::new(192, 168, 0, 9)],
            "the host's loopback is the gateway to the guest"
        );
        assert_eq!(h.names_for(Ipv4Addr::LOCALHOST), ["mock.test"]);
        assert!(h.names_for(Ipv4Addr::new(1, 1, 1, 1)).is_empty());
        for bad in [
            Ipv4Addr::new(127, 0, 0, 2),
            Ipv4Addr::new(10, 0, 2, 1),
            Ipv4Addr::new(10, 0, 2, 2),
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::BROADCAST,
            Ipv4Addr::new(224, 0, 0, 1),
        ] {
            assert!(h.insert("x.test", bad).is_err(), "{bad}");
        }
        assert!(h.insert("bad name", Ipv4Addr::LOCALHOST).is_err());
        for i in 0..MAX_ADDRS_PER_HOST as u8 - 2 {
            h.insert("mock.test", Ipv4Addr::new(1, 1, 1, i)).unwrap();
        }
        assert!(h.insert("mock.test", Ipv4Addr::new(2, 2, 2, 2)).is_err());
        assert!(h.remove("MOCK.TEST"));
        assert!(!h.remove("mock.test"));
        assert!(h.is_empty());
    }
}
