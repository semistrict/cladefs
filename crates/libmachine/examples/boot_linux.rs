// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Boots a Linux kernel and prints its console and how long it took.
//!
//!     cargo run --release --example boot_linux -- KERNEL [INITRD] [CMDLINE]
//!
//! `crates/libmachine/scripts/build-test-guest.sh` builds a kernel and initramfs to try.

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub use kvm::main;

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn main() {
    eprintln!("libmachine runs on x86-64 Linux alone, for now (board 13063)");
    std::process::exit(1);
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod kvm {
    use std::time::Instant;

    use libmachine::{LinuxBoot, Machine, MachineConfig};

    pub fn main() -> Result<(), Box<dyn std::error::Error>> {
        let mut args = std::env::args().skip(1);
        let kernel = args.next().ok_or("usage: boot_linux KERNEL [INITRD] [CMDLINE]")?;
        let boot = LinuxBoot {
            kernel: kernel.into(),
            initrd: args.next().map(Into::into),
            cmdline: args.next().unwrap_or_default(),
        };
        let config = MachineConfig { mem_size: 256 << 20, ..Default::default() };

        let start = Instant::now();
        let mut m = Machine::boot_linux(config, &boot)?;
        let created = start.elapsed();
        m.set_console(std::io::stdout());
        let exit = m.run()?;
        let total = start.elapsed();

        println!("---");
        println!("exit: {exit:?}");
        println!("machine created and kernel loaded: {created:.2?}");
        println!("until the guest stopped:           {total:.2?}");
        Ok(())
    }
}
