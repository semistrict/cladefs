// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Boots a guest with libmachine's agent and runs a command in it.
//!
//!     cargo run --example agent_exec -- KERNEL INITRD
//!
//! `crates/libmachine/scripts/build-test-guest.sh` builds a kernel and an initramfs with the
//! agent (target/guest/vmlinux and target/guest/initramfs.cpio).

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub use kvm::main;

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn main() {
    eprintln!("libmachine runs on x86-64 Linux alone, for now (board 13063)");
    std::process::exit(1);
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod kvm {
    use std::time::Duration;

    use libmachine::agent::Command;
    use libmachine::{LinuxBoot, Machine, MachineConfig};

    pub fn main() -> Result<(), Box<dyn std::error::Error>> {
        let mut args = std::env::args().skip(1);
        let usage = "usage: agent_exec KERNEL INITRD";
        let boot = LinuxBoot {
            kernel: args.next().ok_or(usage)?.into(),
            initrd: Some(args.next().ok_or(usage)?.into()),
            cmdline: "quiet libmachine_action=agent".into(),
        };
        let config = MachineConfig { mem_size: 256 << 20, ..Default::default() };

        let mut m = Machine::boot_linux(config, &boot)?;
        let agent = m.agent().expect("PC machines have an agent");
        let stop = m.stop_handle();
        // The agent answers only while the machine runs: run it on its own thread.
        let runner = std::thread::spawn(move || m.run());

        agent.wait_ready(Duration::from_secs(30))?;
        let out = agent.exec(Command::new("sh").args(["-c", "echo hello from $(uname -s)"]))?;
        print!("{}", String::from_utf8_lossy(&out.stdout));
        agent.write_file("/tmp/x", b"data", 0o644)?;
        assert_eq!(agent.read_file("/tmp/x")?, b"data");

        stop.stop();
        println!("run ended: {:?}", runner.join().expect("runner thread")?);
        Ok(())
    }
}
