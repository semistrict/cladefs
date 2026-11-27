// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A machine counts its vCPU exits by kind (`Machine::exit_counts`); what a
//! real-mode guest does shows up exactly.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

mod common;

use std::time::{Duration, Instant};

use common::{asm16, kvm_available, machine_with};
use iced_x86::code_asm::*;
use libmachine::{ExitCounts, ExitReason, MachineConfig};

/// COM1's scratch register: the UART keeps what is written, prints nothing.
const COM1_SCRATCH: u16 = 0x3ff;

#[test]
fn every_exit_is_counted_by_its_kind() {
    if !kvm_available() {
        return;
    }
    let code = asm16(|a| {
        a.mov(dx, 0x3f8)?;
        a.mov(al, b'x' as u32)?;
        for _ in 0..3 {
            a.out(dx, al)?; // COM1: serial
        }
        a.mov(dx, 0x3fd)?;
        a.in_(al, dx)?; // COM1 line status: serial
        a.out(0x80, al)?; // no device: unclaimed I/O, ends the run
        a.in_(al, 0x81)?; // and again, reading
        a.mov(ax, 0xf000)?;
        a.mov(ds, ax)?;
        a.mov(byte_ptr(0), 1)?; // past the end of RAM: unclaimed MMIO
        a.mov(al, byte_ptr(0))?; // and reading
        a.hlt()
    });
    let config = MachineConfig { mem_size: 0x10000, ..Default::default() };
    let mut m = machine_with(config, &code);
    assert_eq!(m.exit_counts(), ExitCounts::default(), "nothing before a run");
    let mut exits = Vec::new();
    loop {
        let exit = m.run().unwrap();
        exits.push(exit.clone());
        if exit == ExitReason::Halt {
            break;
        }
        assert!(exits.len() < 10, "{exits:?}");
    }
    assert_eq!(exits.len(), 5, "{exits:?}");
    let c = m.exit_counts();
    let expected = [
        ("serial", 4),
        ("rtc", 0),
        ("acpi", 0),
        ("unclaimed_io", 2),
        ("pci_config", 0),
        ("pci_bar", 0),
        ("unclaimed_mmio", 2),
        ("halt", 1),
        // Each run ended on the vCPU's own exit: no kick reached KVM_RUN.
        ("interrupted", 0),
        ("startup", 0),
        ("hypercall", 0),
        ("wait", 0),
        ("system_register", 0),
        ("access_retried", 0),
        ("other", 0),
    ];
    assert_eq!(c.kinds(), expected, "{c}");
    assert_eq!(c.total(), 9);
}

/// A timeout stops the vCPU with exactly one kick, and reading the counts
/// while the machine is stopped gives the same answer twice.
#[test]
fn a_timeout_is_one_interrupted_exit() {
    if !kvm_available() {
        return;
    }
    let code = asm16(|a| {
        let mut top = a.create_label();
        a.set_label(&mut top)?;
        a.jmp(top)
    });
    let mut m = machine_with(MachineConfig::default(), &code);
    for runs in 1..=3 {
        assert_eq!(m.run_for(Duration::from_millis(20)).unwrap(), ExitReason::TimedOut);
        let c = m.exit_counts();
        assert_eq!((c.interrupted, c.total()), (runs, runs), "{c}");
        assert_eq!(m.exit_counts(), c);
    }
}

/// Reports what one exit costs (a port write the UART handles, from the
/// guest's `out` back into the guest); `cargo test -- --nocapture` shows
/// it. board 13123 measured counting with it: no difference beyond noise.
#[test]
fn exit_round_trip_time() {
    if !kvm_available() {
        return;
    }
    const EXITS: u32 = 200_000;
    let code = asm16(|a| {
        let mut top = a.create_label();
        a.mov(ecx, EXITS)?;
        a.mov(dx, u32::from(COM1_SCRATCH))?;
        a.set_label(&mut top)?;
        a.out(dx, al)?;
        a.dec(ecx)?;
        a.jnz(top)?;
        a.hlt()
    });
    let mut best = Duration::MAX;
    for _ in 0..3 {
        let mut m = machine_with(MachineConfig::default(), &code);
        let start = Instant::now();
        assert_eq!(m.run().unwrap(), ExitReason::Halt);
        best = best.min(start.elapsed());
        assert_eq!(m.exit_counts().serial, u64::from(EXITS));
    }
    let per_exit = best / EXITS;
    eprintln!("exit round trip (port write to the UART, counted): {} ns", per_exit.as_nanos());
    assert!(per_exit < Duration::from_micros(100), "{per_exit:?} an exit");
}
