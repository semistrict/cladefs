// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Hypervisor backends: KVM on x86-64 Linux, Hypervisor.framework on arm64 macOS.
//!
//! Each provides a `Vm` and its `Vcpu`s with the same inherent methods, the backend-neutral
//! [`Exit`] type, and a `kick` module (what stops a vCPU from another thread). The two differ in
//! what the platform is (a PC, an arm64 board), which `machine` and `arch` say, so they share
//! no trait: each machine is built for its own backend.

use crate::exits::ExitKind;
use crate::machine::ExitReason;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod kvm;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub use kvm::memory::{HostMemory, MemorySlot, MemorySlots};
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) use kvm::{Ram, Vcpu, VcpuState, Vm, VmState, kick};

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) mod hvf;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) use hvf::{Ram, Vcpu, VcpuState, Vm, VmState, kick};

/// Which machine the backend builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Platform {
    /// No interrupt controller and no devices: for instruction-level tests.
    /// HLT exits to the VMM.
    Bare,
    /// The board Linux boots on, with its interrupt controllers and devices: a PC (in-kernel
    /// PIC, I/O APIC, local APICs and PIT; HLT waits for an interrupt inside the hypervisor) on
    /// x86-64, an arm64 board (the framework's GICv3 and timers) on arm64.
    Board,
}

/// One vCPU exit, before the device layer has looked at it.
pub(crate) enum Exit<'a> {
    /// Port write (x86). `data` is the value written (1, 2 or 4 bytes).
    ///
    /// `data` holds `data.len() / size` accesses of `size` bytes (several
    /// for a string instruction such as `rep outsb`).
    #[cfg(target_arch = "x86_64")]
    IoOut { port: u16, size: u8, data: &'a [u8] },
    /// Port read. The device layer fills `data` before the next run.
    ///
    /// Like [`IoOut`](Exit::IoOut), `data` holds `data.len() / size`
    /// accesses of `size` bytes.
    #[cfg(target_arch = "x86_64")]
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
    /// An exit the backend handled itself (HVF: a queue notification, a PSCI call, a WFI, a
    /// system register), counted as `kind`; re-enter the guest.
    #[cfg_attr(target_os = "linux", allow(dead_code, reason = "KVM handles these in the kernel"))]
    Handled(ExitKind),
    /// A guest access KVM failed (`EFAULT`), where it said if it did: an extent store device's
    /// DAX window changing its memory, which the vCPU loop retries (`virtio_extent::vcpu`).
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    AccessFailed(Option<virtio_extent::vcpu::MemoryFault>),
    /// An exit the device layer does not handle; returned to the caller.
    Done(ExitReason),
}
