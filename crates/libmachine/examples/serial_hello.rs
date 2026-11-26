// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Assembles a tiny real-mode guest with iced-x86, runs it, and prints what
//! it wrote to the serial port. `cargo run --example serial_hello`

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub use kvm::main;

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn main() {
    eprintln!("libmachine runs on x86-64 Linux alone, for now (board 13063)");
    std::process::exit(1);
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod kvm {
    use iced_x86::code_asm::*;
    use libmachine::{ExitReason, Machine, MachineConfig};

    pub fn main() -> Result<(), Box<dyn std::error::Error>> {
        let mut a = CodeAssembler::new(16)?;
        a.mov(dx, 0x3f8)?; // COM1
        a.mov(al, b'A' as u32)?;
        a.out(dx, al)?;
        a.hlt()?;
        let code = a.assemble(0x1000)?;

        let mut m = Machine::new(MachineConfig::default())?;
        m.load(0x1000, &code)?;
        m.set_entry(0x1000)?;
        assert_eq!(m.run()?, ExitReason::Halt);
        assert_eq!(m.serial_output(), b"A");
        println!("guest wrote {:?}", String::from_utf8_lossy(&m.serial_output()));
        Ok(())
    }
}
