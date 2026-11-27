// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The hypervisor backend: Hypervisor.framework on arm64 macOS.
//!
//! It provides a `Vm` and its `Vcpu`s, the backend-neutral [`Exit`] type, and a `kick` module
//! (what stops a vCPU from another thread).

use crate::exits::ExitKind;
use crate::machine::ExitReason;

pub(crate) mod hvf;
pub use hvf::ram::GuestRam;
pub(crate) use hvf::{Ram, Vcpu, VcpuState, Vm, VmState, kick};

/// Which machine the backend builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Platform {
    /// No interrupt controller and no devices: for instruction-level tests.
    /// HLT exits to the VMM.
    Bare,
    /// The board Linux boots on, with its interrupt controller and devices: an arm64 board (the
    /// framework's GICv3 and timers).
    Board,
}

/// One vCPU exit, before the device layer has looked at it.
pub(crate) enum Exit<'a> {
    /// MMIO write outside RAM.
    MmioWrite { addr: u64, data: &'a [u8] },
    /// MMIO read outside RAM. The device layer fills `data`.
    MmioRead { addr: u64, data: &'a mut [u8] },
    /// A signal interrupted the run; re-enter the guest.
    Interrupted,
    /// An exit the backend handled itself (a queue notification, a PSCI call, a WFI, a system
    /// register), counted as `kind`; re-enter the guest.
    Handled(ExitKind),
    /// An exit the device layer does not handle; returned to the caller.
    Done(ExitReason),
}
