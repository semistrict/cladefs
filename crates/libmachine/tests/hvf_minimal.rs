// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A bare arm64 machine on Hypervisor.framework (board 13063): a few instructions, assembled
//! (dynasm, never raw bytes), that write to the UART and power off through PSCI.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

mod common;

use common::{ENTRY, assemble, hypervisor};
use dynasmrt::dynasm;
use libmachine::{ExitReason, Machine, MachineConfig};

#[test]
fn writes_to_the_uart_and_powers_off_through_psci() {
    if !hypervisor() {
        return;
    }
    let code = assemble(|ops| {
        dynasm!(ops
            ; .arch aarch64
            // The UART's transmit register (0x0802_0000).
            ; movz x1, 0x0802, lsl 16
            ; movz w0, 0x6f
            ; strb w0, [x1]
            ; movz w0, 0x6b
            ; strb w0, [x1]
            // PSCI SYSTEM_OFF (0x8400_0008).
            ; movz x0, 0x0008
            ; movk x0, 0x8400, lsl 16
            ; hvc 0
            ; b 0
        );
    });
    let mut m =
        Machine::new(MachineConfig { mem_size: 64 << 20, ..Default::default() }).expect("machine");
    m.load(ENTRY, &code).expect("load");
    m.set_entry(ENTRY).expect("entry");
    assert_eq!(m.run().expect("run"), ExitReason::Poweroff);
    assert_eq!(m.serial_output(), b"ok");
    let counts = m.exit_counts();
    assert_eq!((counts.serial, counts.hypercall), (2, 0), "{counts:?}");
}
