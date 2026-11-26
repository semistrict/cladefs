// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A bare arm64 machine on Hypervisor.framework (board 13063): a few instructions, assembled
//! (dynasm, never raw bytes), that write to the UART and power off through PSCI.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use dynasmrt::dynasm;
use libmachine::{ExitReason, Machine, MachineConfig};

/// Where the program goes: RAM's start.
const ENTRY: u64 = 0x8000_0000;

/// Assembles `program`, for `ENTRY`.
fn assemble(
    program: impl FnOnce(&mut dynasmrt::VecAssembler<dynasmrt::aarch64::Aarch64Relocation>),
) -> Vec<u8> {
    let mut ops = dynasmrt::VecAssembler::new(ENTRY as usize);
    program(&mut ops);
    ops.finalize().expect("assemble the guest program")
}

/// Whether this host runs Hypervisor.framework's VMs (`kern.hv_support`): GitHub's macOS runners,
/// themselves VMs, don't (board 13847). Tests return early, with a note, where it doesn't.
fn hypervisor() -> bool {
    let support = std::process::Command::new("sysctl").args(["-n", "kern.hv_support"]).output();
    let ok = support.is_ok_and(|out| out.status.success() && out.stdout.trim_ascii() == b"1");
    if !ok {
        eprintln!("skipping: this host has no Hypervisor.framework (kern.hv_support)");
    }
    ok
}

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
