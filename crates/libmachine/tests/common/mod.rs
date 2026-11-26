// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Helpers shared by the KVM integration tests: guest programs are written
//! with iced-x86's code assembler, never as raw bytes.
#![allow(dead_code)]

use std::path::PathBuf;

use iced_x86::IcedError;
use iced_x86::code_asm::*;
use libmachine::{Machine, MachineConfig};

/// Where test programs are loaded and start, in 16-bit real mode.
pub const ENTRY: u64 = 0x1000;

/// Whether /dev/kvm can be used. Tests return early (with a note) when it
/// cannot, so the suite passes on CI machines without virtualization.
pub fn kvm_available() -> bool {
    let ok = std::fs::OpenOptions::new().read(true).write(true).open("/dev/kvm").is_ok();
    if !ok {
        eprintln!("skipping: /dev/kvm is not accessible");
    }
    ok
}

/// The test guest's directory (`crates/libmachine/scripts/build-test-guest.sh`): its kernels
/// (bzImage, vmlinux) and its initramfs (initramfs.cpio). `LIBMACHINE_GUEST_DIR`, by default
/// the workspace's target/guest.
pub fn test_guest_dir() -> PathBuf {
    std::env::var_os("LIBMACHINE_GUEST_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/guest"),
        PathBuf::from,
    )
}

/// cladefs's guest kernel, the one Firecracker boots (`CLADEFS_GUEST_KERNEL`, which
/// `scripts/guest-kernel.sh` prints the path of), if it is there (else a note).
pub fn cladefs_kernel() -> Option<PathBuf> {
    let Some(kernel) = std::env::var_os("CLADEFS_GUEST_KERNEL").map(PathBuf::from) else {
        eprintln!("skipping: no CLADEFS_GUEST_KERNEL (scripts/guest-kernel.sh prints its path)");
        return None;
    };
    if !kernel.is_file() {
        eprintln!("skipping: no cladefs kernel at {}", kernel.display());
        return None;
    }
    Some(kernel)
}

/// Assembles the 16-bit real-mode code `program` emits, to run at ENTRY.
pub fn asm16(program: impl FnOnce(&mut CodeAssembler) -> Result<(), IcedError>) -> Vec<u8> {
    let mut a = CodeAssembler::new(16).expect("16-bit assembler");
    program(&mut a).expect("assemble guest program");
    a.assemble(ENTRY).expect("encode guest program")
}

/// Loads `code` at ENTRY and points the vCPU at it.
pub fn load(m: &mut Machine, code: &[u8]) {
    m.load(ENTRY, code).expect("load code");
    m.set_entry(ENTRY).expect("set entry");
}

/// A machine with `code` loaded at ENTRY.
pub fn machine_with(config: MachineConfig, code: &[u8]) -> Machine {
    let mut m = Machine::new(config).expect("create machine");
    load(&mut m, code);
    m
}

/// Assembles `program` followed by `data`. The program gets the data's
/// guest address. (iced-x86 can only address labels RIP-relative, which 16-bit
/// code lacks, so the address is computed: a first pass measures the code,
/// whose length does not depend on the address.)
pub fn asm16_with_data(
    data: &[u8],
    program: impl Fn(&mut CodeAssembler, u16) -> Result<(), IcedError>,
) -> Vec<u8> {
    let len = asm16(|a| program(a, 0)).len();
    let data_addr = u16::try_from(ENTRY as usize + len).expect("data below 64 KiB");
    let mut code = asm16(|a| program(a, data_addr));
    assert_eq!(code.len(), len, "code length changed with the data address");
    code.extend_from_slice(data);
    code
}

/// A program that writes `text` to COM1 and halts.
pub fn serial_print(text: &[u8]) -> Vec<u8> {
    let mut msg = text.to_vec();
    msg.push(0);
    asm16_with_data(&msg, |a, msg_addr| {
        let mut next = a.create_label();
        let mut done = a.create_label();
        a.mov(si, msg_addr as u32)?;
        a.mov(dx, 0x3f8)?; // COM1 transmit register
        a.set_label(&mut next)?;
        a.lodsb()?;
        a.test(al, al)?;
        a.jz(done)?;
        a.out(dx, al)?;
        a.jmp(next)?;
        a.set_label(&mut done)?;
        a.hlt()
    })
}
