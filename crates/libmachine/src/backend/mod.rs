// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Hypervisor backends.
//!
//! Each backend provides a `Vm` with the same inherent methods and the
//! backend-neutral [`Exit`] type. There is deliberately no trait yet: it gets
//! extracted when the second real backend (HVF, board 13063) shows what is common.

use crate::machine::ExitReason;

pub(crate) mod kick;
mod kvm;
pub use kvm::memory::{HostMemory, MemorySlot, MemorySlots};
pub(crate) use kvm::{Ram, Vcpu, VcpuState, Vm, VmState};

/// Which machine the backend builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Platform {
    /// No interrupt controller and no devices: for instruction-level tests.
    /// HLT exits to the VMM.
    Bare,
    /// A PC: in-kernel PIC, I/O APIC, local APICs and PIT. HLT waits for an
    /// interrupt inside the hypervisor.
    Pc,
}

/// One vCPU exit, before the device layer has looked at it.
pub(crate) enum Exit<'a> {
    /// Port write. `data` is the value written (1, 2 or 4 bytes).
    ///
    /// `data` holds `data.len() / size` accesses of `size` bytes (several
    /// for a string instruction such as `rep outsb`).
    IoOut { port: u16, size: u8, data: &'a [u8] },
    /// Port read. The device layer fills `data` before the next run.
    ///
    /// Like [`IoOut`](Exit::IoOut), `data` holds `data.len() / size`
    /// accesses of `size` bytes.
    IoIn { port: u16, size: u8, data: &'a mut [u8] },
    /// MMIO write outside RAM.
    MmioWrite { addr: u64, data: &'a [u8] },
    /// MMIO read outside RAM. The device layer fills `data`.
    MmioRead { addr: u64, data: &'a mut [u8] },
    /// A signal interrupted the run; re-enter the guest.
    Interrupted,
    /// A vCPU waiting for its startup IPI woke (INIT or SIPI) without
    /// running the guest; re-enter it.
    Startup,
    /// An exit the device layer does not handle; returned to the caller.
    Done(ExitReason),
}
