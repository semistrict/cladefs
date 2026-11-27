// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Counting the vCPU exits a machine takes, by kind.
//!
//! Each vCPU counts its own exits in a block of counters that only its
//! thread writes; [`Machine::exit_counts`](crate::Machine::exit_counts) sums
//! the blocks when asked. An exit costs the vCPU two relaxed atomic accesses
//! to a cache line it owns (no locked instruction), against the microsecond
//! or more the exit itself takes.

use std::sync::atomic::{AtomicU64, Ordering};

/// How many vCPU exits of each kind a machine has taken since it was
/// created, summed over its vCPUs ([`Machine::exit_counts`]).
///
/// An exit here is a return from the hypervisor to libmachine (from
/// `hv_vcpu_run`): the exits a guest makes libmachine handle. Exits the
/// hypervisor handles by itself (interrupts, GIC accesses, page faults) are
/// not counted.
///
/// Counts only grow. To count a phase, subtract the counts before it from
/// those after it.
///
/// [`Machine::exit_counts`]: crate::Machine::exit_counts
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExitCounts {
    /// MMIO to the UART.
    pub serial: u64,
    /// MMIO to the arm64 board's real-time clock (a PL031).
    pub rtc: u64,
    /// MMIO to PCI configuration space (ECAM), whether or not a function is
    /// there.
    pub pci_config: u64,
    /// MMIO to a PCI device's registers: the BARs libmachine traps
    /// (virtio-PCI's configuration structures and MSI-X tables). Queue
    /// notifications reach their device by ioeventfd and take no exit.
    pub pci_bar: u64,
    /// MMIO that no device claims.
    pub unclaimed_mmio: u64,
    /// `hlt` (bare machines only: on the board the vCPU waits in the hypervisor).
    pub halt: u64,
    /// The hypervisor returned before or while running the guest because
    /// of a signal: a kick (the run ending, a stop request) or another.
    pub interrupted: u64,
    /// A hypercall (HVC) the guest made: PSCI, on the arm64 board.
    pub hypercall: u64,
    /// A wait for an interrupt (WFI) that left the guest: the vCPU thread then sleeps until one
    /// comes.
    pub wait: u64,
    /// A system register access the hypervisor left to libmachine (arm64:
    /// debug and OS lock registers, read as zero).
    pub system_register: u64,
    /// A guest access to an extent store device's DAX window that the vCPU retried: a fault on a
    /// page of the window, which cladefs mapped first (`virtio_extent::window_fault`).
    pub access_retried: u64,
    /// Anything else: a triple fault, an entry failure, an internal error,
    /// an exit libmachine does not handle, an error from the hypervisor.
    pub other: u64,
}

/// The kinds, in [`ExitCounts`]' field order (and [`ExitKind`]'s).
const NAMES: [&str; KINDS] = [
    "serial",
    "rtc",
    "pci_config",
    "pci_bar",
    "unclaimed_mmio",
    "halt",
    "interrupted",
    "hypercall",
    "wait",
    "system_register",
    "access_retried",
    "other",
];
const KINDS: usize = 12;

impl ExitCounts {
    /// Every kind's name (the field's) and count, in a fixed order.
    pub fn kinds(&self) -> [(&'static str, u64); KINDS] {
        let values = self.to_array();
        std::array::from_fn(|i| (NAMES[i], values[i]))
    }

    /// All exits, of every kind.
    pub fn total(&self) -> u64 {
        self.to_array().iter().sum()
    }

    fn to_array(self) -> [u64; KINDS] {
        [
            self.serial,
            self.rtc,
            self.pci_config,
            self.pci_bar,
            self.unclaimed_mmio,
            self.halt,
            self.interrupted,
            self.hypercall,
            self.wait,
            self.system_register,
            self.access_retried,
            self.other,
        ]
    }

    fn from_array(v: [u64; KINDS]) -> Self {
        let [
            serial,
            rtc,
            pci_config,
            pci_bar,
            unclaimed_mmio,
            halt,
            interrupted,
            hypercall,
            wait,
            system_register,
            access_retried,
            other,
        ] = v;
        Self {
            serial,
            rtc,
            pci_config,
            pci_bar,
            unclaimed_mmio,
            halt,
            interrupted,
            hypercall,
            wait,
            system_register,
            access_retried,
            other,
        }
    }

    fn zip(self, other: Self, f: impl Fn(u64, u64) -> u64) -> Self {
        let (a, b) = (self.to_array(), other.to_array());
        Self::from_array(std::array::from_fn(|i| f(a[i], b[i])))
    }
}

impl std::ops::Add for ExitCounts {
    type Output = Self;
    fn add(self, other: Self) -> Self {
        self.zip(other, u64::wrapping_add)
    }
}

/// The exits taken between two readings, `self` the later. Saturates at
/// zero, so a reading of another machine gives no wrapped-around counts.
impl std::ops::Sub for ExitCounts {
    type Output = Self;
    fn sub(self, earlier: Self) -> Self {
        self.zip(earlier, u64::saturating_sub)
    }
}

impl std::iter::Sum for ExitCounts {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::default(), |a, b| a + b)
    }
}

/// "1234 exits: serial 1000, pci_config 230, interrupted 4": the total, then
/// each kind that has any.
impl std::fmt::Display for ExitCounts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} exits", self.total())?;
        let mut sep = ": ";
        for (name, n) in self.kinds() {
            if n > 0 {
                write!(f, "{sep}{name} {n}")?;
                sep = ", ";
            }
        }
        Ok(())
    }
}

/// One kind of exit, as a vCPU counts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum ExitKind {
    Serial,
    Rtc,
    PciConfig,
    PciBar,
    UnclaimedMmio,
    Halt,
    Interrupted,
    Hypercall,
    Wait,
    SystemRegister,
    AccessRetried,
    Other,
}

/// One vCPU's counters. Only the vCPU's thread counts; anyone may read.
#[derive(Debug, Default)]
pub(crate) struct ExitCounters([AtomicU64; KINDS]);

impl ExitCounters {
    /// Counts one exit of `kind`. A load and a store rather than a
    /// read-modify-write: there is one writer, so no count is lost, and a
    /// reader sees each counter's latest value or a recent one.
    #[inline]
    pub(crate) fn count(&self, kind: ExitKind) {
        let c = &self.0[kind as usize];
        c.store(c.load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed);
    }

    pub(crate) fn read(&self) -> ExitCounts {
        ExitCounts::from_array(std::array::from_fn(|i| self.0[i].load(Ordering::Relaxed)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [ExitKind; KINDS] = [
        ExitKind::Serial,
        ExitKind::Rtc,
        ExitKind::PciConfig,
        ExitKind::PciBar,
        ExitKind::UnclaimedMmio,
        ExitKind::Halt,
        ExitKind::Interrupted,
        ExitKind::Hypercall,
        ExitKind::Wait,
        ExitKind::SystemRegister,
        ExitKind::AccessRetried,
        ExitKind::Other,
    ];

    /// Each kind lands in its own field, under its own name: counting kind
    /// i a distinct number of times shows up as that number under the
    /// field named for it, and nowhere else.
    #[test]
    fn each_kind_has_its_own_field_and_name() {
        let c = ExitCounters::default();
        for (i, &kind) in ALL.iter().enumerate() {
            assert_eq!(kind as usize, i);
            for _ in 0..(1u64 << i) {
                c.count(kind);
            }
        }
        let counts = c.read();
        assert_eq!(counts.total(), (1 << KINDS) - 1);
        let named: Vec<(&str, u64)> = counts.kinds().to_vec();
        let expected: Vec<(&str, u64)> = [
            ("serial", counts.serial),
            ("rtc", counts.rtc),
            ("pci_config", counts.pci_config),
            ("pci_bar", counts.pci_bar),
            ("unclaimed_mmio", counts.unclaimed_mmio),
            ("halt", counts.halt),
            ("interrupted", counts.interrupted),
            ("hypercall", counts.hypercall),
            ("wait", counts.wait),
            ("system_register", counts.system_register),
            ("access_retried", counts.access_retried),
            ("other", counts.other),
        ]
        .to_vec();
        assert_eq!(named, expected);
        for (i, (_, n)) in named.iter().enumerate() {
            assert_eq!(*n, 1 << i, "{named:?}");
        }
    }

    #[test]
    fn counts_add_subtract_and_display() {
        let a = ExitCounters::default();
        a.count(ExitKind::Serial);
        a.count(ExitKind::Serial);
        a.count(ExitKind::Interrupted);
        let b = ExitCounters::default();
        b.count(ExitKind::PciConfig);
        let sum: ExitCounts = [a.read(), b.read()].into_iter().sum();
        assert_eq!((sum.serial, sum.interrupted, sum.pci_config), (2, 1, 1));
        assert_eq!(sum.to_string(), "4 exits: serial 2, pci_config 1, interrupted 1");
        assert_eq!(sum - a.read(), b.read());
        assert_eq!(a.read() - sum, ExitCounts::default(), "saturates");
        assert_eq!(ExitCounts::default().to_string(), "0 exits");
    }
}
