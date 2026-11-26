// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! KVM's mapping of guest memory for the guest ahead of its accesses (`KVM_PRE_FAULT_MEMORY`),
//! the [`PrefaultGuestMemory`] a VMM on KVM gives the device (README.md, "The DAX window"). It is
//! a vCPU ioctl, which waits for the vCPU to leave `KVM_RUN`, so a vCPU of its own does it, which
//! the guest does not know of and which never runs: the VMM creates it, with the CPUID its guest's
//! vCPUs have (the second-stage tables' depth follows the guest's physical address width), as
//! only it can, and hands over its descriptor. vCPUs share the second-stage tables of the guest's
//! memory, while none is in SMM or nested guest mode.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};

use log::warn;

use crate::window::PrefaultGuestMemory;

/// The capability a VMM checks (`KVM_CHECK_EXTENSION`) before making a [`KvmPrefault`]: from
/// `include/uapi/linux/kvm.h`.
pub const KVM_CAP_PRE_FAULT_MEMORY: u32 = 236;

#[allow(clippy::cast_possible_truncation)]
const KVM_PRE_FAULT_MEMORY: libc::c_ulong =
    (3 << 30) | ((size_of::<KvmPreFaultMemory>() as libc::c_ulong) << 16) | (0xae << 8) | 0xd5;

#[repr(C)]
struct KvmPreFaultMemory {
    gpa: u64,
    size: u64,
    flags: u64,
    padding: [u64; 5],
}

/// Maps guest memory for the guest with a vCPU of its own.
#[derive(Debug)]
pub struct KvmPrefault {
    vcpu: OwnedFd,
    /// Whether a failure has been logged, which is logged only once.
    failed: AtomicBool,
}

impl KvmPrefault {
    /// Over `vcpu`, a vCPU of the guest's VM that no vCPU of the guest's is, its CPUID set as
    /// theirs, which never runs; its KVM has [`KVM_CAP_PRE_FAULT_MEMORY`].
    pub fn new(vcpu: OwnedFd) -> Self {
        Self { vcpu, failed: AtomicBool::new(false) }
    }
}

impl PrefaultGuestMemory for KvmPrefault {
    fn prefault(&self, guest_addr: u64, len: u64) {
        let mut range = KvmPreFaultMemory { gpa: guest_addr, size: len, flags: 0, padding: [0; 5] };
        // KVM maps what it can and updates the range to what is left, which a signal, or
        // contention, can leave.
        while range.size > 0 {
            // SAFETY: Passes the vCPU's ioctl the structure it takes, which it updates.
            let ret = unsafe {
                libc::ioctl(self.vcpu.as_raw_fd(), KVM_PRE_FAULT_MEMORY as _, &raw mut range)
            };
            if ret < 0 {
                let err = io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(libc::EINTR | libc::EAGAIN) => continue,
                    // The guest faults on what is left as it would have.
                    _ => {
                        if !self.failed.swap(true, Ordering::Relaxed) {
                            warn!(
                                "pre-faulting guest memory at {:#x}, {:#x} bytes: {err}",
                                range.gpa, range.size
                            );
                        }
                        return;
                    }
                }
            }
        }
    }
}
