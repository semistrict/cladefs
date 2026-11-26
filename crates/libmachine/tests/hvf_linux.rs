// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The arm64 board on Hypervisor.framework (board 13063), booting cladefs's arm64 kernel with
//! the test initramfs: to init and off, a reset, several vCPUs (PSCI), and the agent over vsock.
//!
//! The kernel is `CLADEFS_GUEST_KERNEL`, else `target/arm64-guest/Image`; the initramfs is in
//! `LIBMACHINE_GUEST_DIR`, else `target/arm64-guest` (`crates/libmachine/scripts/arm64-guest.sh`
//! fetches both). Tests return early with a note when they are missing. A process has one VM:
//! the tests take turns.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use libmachine::agent::Command;
use libmachine::{ExitReason, LinuxBoot, Machine, MachineConfig};

/// What init prints once it runs.
const INIT_MARKER: &str = "libmachine-init: up ";

/// Hypervisor.framework gives a process one VM: one test's machine at a time.
fn one_machine() -> MutexGuard<'static, ()> {
    static ONE: Mutex<()> = Mutex::new(());
    ONE.lock().unwrap_or_else(|e| e.into_inner())
}

fn guest_dir() -> PathBuf {
    std::env::var_os("LIBMACHINE_GUEST_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/arm64-guest"),
        PathBuf::from,
    )
}

/// cladefs's arm64 kernel and the test initramfs, if both are there.
fn guest() -> Option<(PathBuf, PathBuf)> {
    let kernel = std::env::var_os("CLADEFS_GUEST_KERNEL")
        .map_or_else(|| guest_dir().join("Image"), PathBuf::from);
    let initrd = guest_dir().join("initramfs.cpio");
    if !kernel.is_file() || !initrd.is_file() {
        eprintln!(
            "skipping: no arm64 guest at {} and {} (run crates/libmachine/scripts/arm64-guest.sh)",
            kernel.display(),
            initrd.display()
        );
        return None;
    }
    Some((kernel, initrd))
}

fn config(vcpus: u32) -> MachineConfig {
    MachineConfig { mem_size: 512 << 20, vcpus, ..Default::default() }
}

/// Boots with `cmdline` and `vcpus` vCPUs, runs to the end (30 s at most), and returns the exit
/// and the console, its lines ending in "\n".
fn run(cmdline: &str, vcpus: u32) -> Option<(ExitReason, String)> {
    let (kernel, initrd) = guest()?;
    let _one = one_machine();
    let boot = LinuxBoot { kernel, initrd: Some(initrd), cmdline: cmdline.into() };
    let mut m = Machine::boot_linux(config(vcpus), &boot).expect("boot");
    let exit = m.run_for(Duration::from_secs(30)).expect("run");
    let console = String::from_utf8_lossy(&m.serial_output()).replace("\r\n", "\n");
    eprintln!("{cmdline:?}, {vcpus} vCPUs: {:?}", m.exit_counts());
    Some((exit, console))
}

#[test]
fn boots_to_init_and_powers_off() {
    let Some((exit, console)) = run("", 1) else {
        return;
    };
    assert_eq!(exit, ExitReason::Poweroff, "console:\n{console}");
    assert!(console.contains(INIT_MARKER), "console:\n{console}");
    assert!(console.contains("reboot: Power down"), "console:\n{console}");
}

#[test]
fn a_reboot_ends_the_run_with_a_reset() {
    let Some((exit, console)) = run("libmachine_action=reboot", 1) else {
        return;
    };
    assert_eq!(exit, ExitReason::Reset, "console:\n{console}");
    assert!(console.contains(INIT_MARKER), "console:\n{console}");
}

/// The guest powers its other vCPUs on through PSCI, and they all come online.
#[test]
fn every_vcpu_comes_online() {
    let Some((exit, console)) = run("libmachine_cat=/sys/devices/system/cpu/online", 4) else {
        return;
    };
    assert_eq!(exit, ExitReason::Poweroff, "console:\n{console}");
    assert!(
        console.contains("libmachine-init: /sys/devices/system/cpu/online:\n0-3\n"),
        "console:\n{console}"
    );
}

/// The guest agent answers over the vsock, on a virtio-PCI device whose interrupts are MSIs
/// through the GIC's MSI frame.
#[test]
fn the_agent_runs_a_command() {
    let Some((kernel, initrd)) = guest() else {
        return;
    };
    let _one = one_machine();
    let boot =
        LinuxBoot { kernel, initrd: Some(initrd), cmdline: "quiet libmachine_action=agent".into() };
    let mut m = Machine::boot_linux(config(2), &boot).expect("boot");
    let agent = m.agent().expect("a board has the agent's vsock");
    let stop = m.stop_handle();
    let runner = std::thread::spawn(move || {
        let exit = m.run_for(Duration::from_secs(60));
        (exit, String::from_utf8_lossy(&m.serial_output()).into_owned())
    });
    let ready = agent.wait_ready(Duration::from_secs(30));
    let out = ready.and_then(|()| agent.exec(Command::new("sh").args(["-c", "echo hi; uname -m"])));
    stop.stop();
    let (exit, console) = runner.join().expect("the run thread");
    let out = out.unwrap_or_else(|e| panic!("exec: {e}; console:\n{console}"));
    assert_eq!(out.stdout, b"hi\naarch64\n", "console:\n{console}");
    assert_eq!(exit.expect("run"), ExitReason::Stopped);
}

/// With nested virtualization the guest's kernel boots at EL2 and brings KVM up: the guest has
/// /dev/kvm, to run VMs of its own (Apple M3 and later).
#[test]
fn nested_virtualization_gives_the_guest_kvm() {
    let Some((kernel, initrd)) = guest() else {
        return;
    };
    let _one = one_machine();
    let boot =
        LinuxBoot { kernel, initrd: Some(initrd), cmdline: "quiet libmachine_action=agent".into() };
    let config = MachineConfig { nested_virtualization: true, ..config(2) };
    let mut m = Machine::boot_linux(config, &boot).expect("boot");
    let agent = m.agent().expect("a board has the agent's vsock");
    let stop = m.stop_handle();
    let runner = std::thread::spawn(move || {
        let exit = m.run_for(Duration::from_secs(60));
        (exit, String::from_utf8_lossy(&m.serial_output()).into_owned())
    });
    let ready = agent.wait_ready(Duration::from_secs(30));
    let out = ready.and_then(|()| {
        agent.exec(
            Command::new("sh").args(["-c", "test -c /dev/kvm && echo kvm; dmesg | grep 'kvm \\['"]),
        )
    });
    stop.stop();
    let (exit, console) = runner.join().expect("the run thread");
    let out = out.unwrap_or_else(|e| panic!("exec: {e}; exit {exit:?}; console:\n{console}"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!("{stdout}");
    assert!(stdout.starts_with("kvm\n"), "{stdout}; console:\n{console}");
    assert_eq!(exit.expect("run"), ExitReason::Stopped);
}
