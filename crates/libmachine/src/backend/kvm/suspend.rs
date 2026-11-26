// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What a suspension ([`crate::suspend`]) asks of KVM about a vCPU: whether
//! it is idle. (The restore that wakes it tells the guest's kvmclock it was
//! paused, as every restore does.)

use kvm_bindings::KVM_MP_STATE_HALTED;

use super::Vcpu;
use crate::error::{Result, kvm_err};

/// The local APIC's interrupt request register: eight 32-bit words, 16
/// bytes apart (Intel SDM vol. 3A, table 11-1).
const APIC_IRR: usize = 0x200;

impl Vcpu {
    /// Whether the vCPU is idle, between runs: halted, waiting for an
    /// interrupt, with none waiting in its local APIC to be taken (one that
    /// arrived as the vCPU was stopped would have woken it).
    pub(crate) fn idle(&self) -> Result<bool> {
        let mp = self.fd.get_mp_state().map_err(kvm_err("get the MP state"))?;
        if mp.mp_state != KVM_MP_STATE_HALTED {
            return Ok(false);
        }
        let lapic = self.fd.get_lapic().map_err(kvm_err("get the local APIC"))?;
        Ok((0..8).all(|i| {
            let at = APIC_IRR + 16 * i;
            lapic.regs[at..at + 4].iter().all(|&b| b == 0)
        }))
    }
}
