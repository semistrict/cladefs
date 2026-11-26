// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Runs small real-mode guests on the KVM backend.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

mod common;

use common::{ENTRY, asm16, kvm_available, load, machine_with, serial_print};
use iced_x86::BlockEncoderOptions;
use iced_x86::code_asm::*;
use libmachine::{Error, ExitReason, Machine, MachineConfig};

#[test]
fn guest_writes_serial_and_halts() {
    if !kvm_available() {
        return;
    }
    let mut m = machine_with(MachineConfig::default(), &serial_print(b"Hi\n"));
    assert_eq!(m.run().unwrap(), ExitReason::Halt);
    assert_eq!(m.serial_output(), b"Hi\n");
    assert_eq!(m.take_serial_output(), b"Hi\n");
    assert!(m.serial_output().is_empty());
}

#[test]
fn registers_reflect_guest_computation() {
    if !kvm_available() {
        return;
    }
    let code = asm16(|a| {
        a.mov(ax, 0x1234)?;
        a.mov(bx, 0x5678)?;
        a.add(ax, bx)?;
        a.hlt()
    });
    let mut m = machine_with(MachineConfig::default(), &code);
    assert_eq!(m.run().unwrap(), ExitReason::Halt);
    let regs = m.registers().unwrap();
    assert_eq!(regs.rax & 0xffff, 0x68ac);
    assert_eq!(regs.rbx & 0xffff, 0x5678);
    assert_eq!(regs.rip, ENTRY + code.len() as u64, "rip points past hlt");
}

#[test]
fn guest_can_poll_serial_line_status() {
    if !kvm_available() {
        return;
    }
    let code = asm16(|a| {
        a.mov(dx, 0x3fd)?; // COM1 line status register
        a.in_(al, dx)?;
        a.hlt()
    });
    let mut m = machine_with(MachineConfig::default(), &code);
    assert_eq!(m.run().unwrap(), ExitReason::Halt);
    assert_eq!(m.registers().unwrap().rax & 0xff, 0x60, "transmitter empty");
}

#[test]
fn unknown_port_is_reported() {
    if !kvm_available() {
        return;
    }
    let code = asm16(|a| {
        a.mov(al, 1)?;
        a.out(0x80, al)?;
        a.hlt()
    });
    let mut m = machine_with(MachineConfig::default(), &code);
    assert_eq!(m.run().unwrap(), ExitReason::UnhandledIo { port: 0x80, size: 1, write: true });
    // The guest can be resumed after an unhandled access.
    assert_eq!(m.run().unwrap(), ExitReason::Halt);
}

/// Between runs the vCPU's state is the one the guest resumes from: an
/// access that ended the run is complete (KVM completes one only on the
/// next KVM_RUN, unless told to), so its value is in the register and RIP
/// is past the instruction. A snapshot taken between runs relies on it.
/// Before, registers() showed RIP at the `in`, AL as before it.
#[test]
fn an_access_that_ended_the_run_is_complete_between_runs() {
    if !kvm_available() {
        return;
    }
    let mut a = CodeAssembler::new(16).unwrap();
    a.mov(al, 0x5a).unwrap();
    a.in_(al, 0x80).unwrap();
    let mut after_in = a.create_label();
    a.set_label(&mut after_in).unwrap();
    a.hlt().unwrap();
    let result =
        a.assemble_options(ENTRY, BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS).unwrap();
    let after_in = result.label_ip(&after_in).unwrap();
    let mut m = machine_with(MachineConfig::default(), &result.inner.code_buffer);
    assert_eq!(m.run().unwrap(), ExitReason::UnhandledIo { port: 0x80, size: 1, write: false });
    let regs = m.registers().unwrap();
    assert_eq!(regs.rip, after_in, "past the in");
    assert_eq!(regs.rax & 0xff, 0, "the zero an unhandled port reads as");
    assert_eq!(m.run().unwrap(), ExitReason::Halt);
    assert_eq!(m.registers().unwrap().rip, after_in + 1);
}

#[test]
fn write_outside_ram_is_reported_as_mmio() {
    if !kvm_available() {
        return;
    }
    let code = asm16(|a| {
        a.mov(ax, 0xf000)?;
        a.mov(ds, ax)?;
        a.mov(byte_ptr(0), 1)?; // physical 0xf0000, past the end of RAM
        a.hlt()
    });
    let config = MachineConfig { mem_size: 0x10000, ..Default::default() };
    let mut m = machine_with(config, &code);
    assert_eq!(m.run().unwrap(), ExitReason::UnhandledMmio { addr: 0xf0000, size: 1, write: true });
}

#[test]
fn two_machines_in_one_process() {
    if !kvm_available() {
        return;
    }
    let mut a = machine_with(MachineConfig::default(), &serial_print(b"A"));
    let mut b = machine_with(MachineConfig::default(), &serial_print(b"B"));
    assert_eq!(b.run().unwrap(), ExitReason::Halt);
    assert_eq!(a.run().unwrap(), ExitReason::Halt);
    assert_eq!(a.serial_output(), b"A");
    assert_eq!(b.serial_output(), b"B");
}

#[test]
fn machines_run_concurrently_on_threads() {
    if !kvm_available() {
        return;
    }
    let handles: Vec<_> = (0..8u8)
        .map(|i| {
            std::thread::spawn(move || {
                let mut m = machine_with(MachineConfig::default(), &serial_print(&[b'0' + i]));
                assert_eq!(m.run().unwrap(), ExitReason::Halt);
                m.take_serial_output()
            })
        })
        .collect();
    for (i, h) in handles.into_iter().enumerate() {
        assert_eq!(h.join().unwrap(), vec![b'0' + i as u8]);
    }
}

#[test]
fn guest_memory_round_trips() {
    if !kvm_available() {
        return;
    }
    let mut m = Machine::new(MachineConfig::default()).unwrap();
    m.load(0x2000, b"hello").unwrap();
    assert_eq!(m.read_memory(0x2000, 5).unwrap(), b"hello");
    assert!(matches!(m.load(m.mem_size() as u64, b"x"), Err(Error::GuestMemory(_))));
}

#[test]
fn invalid_config_is_rejected() {
    for mem_size in [0, 1000] {
        assert!(matches!(
            Machine::new(MachineConfig { mem_size, ..Default::default() }),
            Err(Error::InvalidConfig(_))
        ));
    }
    if !kvm_available() {
        return;
    }
    let mut m = Machine::new(MachineConfig::default()).unwrap();
    assert!(matches!(m.set_entry(0x10000), Err(Error::InvalidConfig(_))));
    load(&mut m, &serial_print(b""));
}
