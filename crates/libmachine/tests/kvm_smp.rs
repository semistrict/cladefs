// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Machines with several vCPUs, booting the test guest
//! (`crates/libmachine/scripts/build-test-guest.sh`): the guest brings every vCPU online, sees
//! each in its place in the topology, and the machine stops and times out
//! with every vCPU busy or idle.
//!
//! Tests return early with a note when /dev/kvm or the guest is missing.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

mod common;

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use common::kvm_available;
use libmachine::{Error, ExitReason, LinuxBoot, Machine, MachineConfig};

fn guest() -> Option<PathBuf> {
    if !kvm_available() {
        return None;
    }
    let dir = std::env::var_os("LIBMACHINE_GUEST_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/guest"),
        PathBuf::from,
    );
    if ["bzImage", "vmlinux", "initramfs.cpio"].iter().all(|f| dir.join(f).is_file()) {
        Some(dir)
    } else {
        eprintln!(
            "skipping: no test guest in {} (run crates/libmachine/scripts/build-test-guest.sh)",
            dir.display()
        );
        None
    }
}

fn config(vcpus: u32) -> MachineConfig {
    MachineConfig { mem_size: 256 << 20, vcpus, ..Default::default() }
}

fn boot(dir: &Path, kernel: &str, cmdline: &str) -> LinuxBoot {
    LinuxBoot {
        kernel: dir.join(kernel),
        initrd: Some(dir.join("initramfs.cpio")),
        cmdline: cmdline.into(),
    }
}

/// Boots with `vcpus` vCPUs, runs to the end, and returns the exit and the
/// console.
fn run(dir: &Path, kernel: &str, cmdline: &str, vcpus: u32) -> (ExitReason, String) {
    let mut m = Machine::boot_linux(config(vcpus), &boot(dir, kernel, cmdline)).expect("boot");
    assert_eq!(m.vcpus(), vcpus);
    let exit = m.run_for(Duration::from_secs(30)).expect("run");
    (exit, String::from_utf8_lossy(&m.serial_output()).into_owned())
}

/// What init printed of `path` (`libmachine_cat`).
fn catted(console: &str, path: &str) -> String {
    let start = format!("libmachine-init: {path}:");
    let end = format!("libmachine-init: end of {path}");
    let lines: Vec<&str> = console.lines().collect();
    let from = lines
        .iter()
        .position(|l| *l == start)
        .unwrap_or_else(|| panic!("no {path} in console:\n{console}"));
    let to = from
        + lines[from..]
            .iter()
            .position(|l| *l == end)
            .unwrap_or_else(|| panic!("{path} unterminated in console:\n{console}"));
    lines[from + 1..to].join("\n")
}

const ONLINE: &str = "/sys/devices/system/cpu/online";

#[test]
fn linux_brings_every_vcpu_online() {
    let Some(dir) = guest() else { return };
    for kernel in ["vmlinux", "bzImage"] {
        for (vcpus, online) in [(1, "0"), (2, "0-1"), (4, "0-3")] {
            let (exit, console) = run(&dir, kernel, &format!("libmachine_cat={ONLINE}"), vcpus);
            assert_eq!(exit, ExitReason::Poweroff, "{kernel} x{vcpus}:\n{console}");
            assert_eq!(catted(&console, ONLINE), online, "{kernel} x{vcpus}");
            if vcpus > 1 {
                let brought_up = format!("smp: Brought up 1 node, {vcpus} CPUs");
                assert!(console.contains(&brought_up), "{kernel}:\n{console}");
            }
            for bad in ["CPU failed", "failed to come online", "TSC synchronization"] {
                assert!(!console.contains(bad), "{bad:?} in console:\n{console}");
            }
        }
    }
}

/// Every vCPU has its own APIC ID and is a core of one socket, with one
/// thread per core, as its CPUID topology leaves say.
#[test]
fn each_vcpu_has_its_place_in_the_topology() {
    let Some(dir) = guest() else { return };
    let (exit, console) = run(&dir, "vmlinux", "quiet libmachine_cat=/proc/cpuinfo", 4);
    assert_eq!(exit, ExitReason::Poweroff, "{console}");
    let cpuinfo = catted(&console, "/proc/cpuinfo");
    let cpus: Vec<&str> = cpuinfo.split("\n\n").filter(|c| !c.trim().is_empty()).collect();
    assert_eq!(cpus.len(), 4, "{cpuinfo}");
    for (i, cpu) in cpus.iter().enumerate() {
        let field = |name: &str| -> String {
            cpu.lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    (k.trim() == name).then(|| v.trim().to_owned())
                })
                .unwrap_or_else(|| panic!("no {name:?} for CPU {i}:\n{cpu}"))
        };
        let id = i.to_string();
        assert_eq!(field("processor"), id);
        assert_eq!(field("apicid"), id, "CPU {i}");
        assert_eq!(field("initial apicid"), id, "CPU {i}");
        assert_eq!(field("core id"), id, "CPU {i}");
        assert_eq!(field("physical id"), "0", "CPU {i}");
        assert_eq!(field("cpu cores"), "4", "CPU {i}");
        assert_eq!(field("siblings"), "4", "CPU {i}: no SMT");
    }
}

#[test]
fn invalid_vcpu_counts_are_refused() {
    if !kvm_available() {
        return;
    }
    let boot = LinuxBoot { kernel: "unused".into(), initrd: None, cmdline: String::new() };
    for vcpus in [0, MachineConfig::MAX_VCPUS + 1, u32::MAX] {
        match Machine::boot_linux(config(vcpus), &boot) {
            Err(Error::InvalidConfig(msg)) => assert!(msg.contains("vcpus"), "{msg}"),
            other => panic!("{vcpus} vCPUs: expected a configuration error, got {other:?}"),
        }
    }
    // A bare machine has one vCPU.
    let bare = MachineConfig { vcpus: 2, ..Default::default() };
    match Machine::new(bare) {
        Err(Error::InvalidConfig(msg)) => assert!(msg.contains("one vCPU"), "{msg}"),
        other => panic!("expected a configuration error, got {other:?}"),
    }
}

/// The fingerprint names the CPU model, not the topology: machines of one
/// model share it whatever their vCPU count. The CPUID table vCPU 0 gets
/// counts every vCPU.
#[test]
fn fingerprint_does_not_depend_on_the_vcpu_count() {
    let Some(dir) = guest() else { return };
    let machines: Vec<Machine> = [1, 2, 4]
        .into_iter()
        .map(|n| Machine::boot_linux(config(n), &boot(&dir, "vmlinux", "")).unwrap())
        .collect();
    assert_eq!(machines[0].cpu_fingerprint(), machines[1].cpu_fingerprint());
    assert_eq!(machines[0].cpu_fingerprint(), machines[2].cpu_fingerprint());
    for m in &machines {
        let leaf1 = m.cpuid(1, 0).unwrap();
        assert_eq!((leaf1.ebx >> 16) & 0xff, m.vcpus(), "logical processors");
        assert_eq!(leaf1.ebx >> 24, 0, "vCPU 0's APIC ID");
    }
}

const PROMPT: Duration = Duration::from_secs(2);

/// An idle guest's vCPUs all wait in HLT inside KVM, and the APs that went
/// idle after boot too: a timeout and a stop reach every one.
#[test]
fn idle_smp_guest_times_out_and_stops() {
    let Some(dir) = guest() else { return };
    let mut m =
        Machine::boot_linux(config(4), &boot(&dir, "vmlinux", "quiet libmachine_action=hang"))
            .unwrap();
    let start = Instant::now();
    assert_eq!(m.run_for(Duration::from_millis(500)).unwrap(), ExitReason::TimedOut);
    assert!(start.elapsed() < Duration::from_millis(500) + PROMPT);
    let console = String::from_utf8_lossy(&m.serial_output()).into_owned();
    assert!(console.contains("libmachine-init: up "), "{console}");

    let handle = m.stop_handle();
    let stopper = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        handle.stop();
    });
    let start = Instant::now();
    assert_eq!(m.run().unwrap(), ExitReason::Stopped);
    assert!(start.elapsed() < PROMPT);
    stopper.join().unwrap();
}

/// A machine stopped before its APs were started keeps them waiting: the
/// guest brings them up after it resumes, however often it is stopped.
#[test]
fn a_boot_cut_into_slices_still_brings_up_every_vcpu() {
    let Some(dir) = guest() else { return };
    let mut m =
        Machine::boot_linux(config(4), &boot(&dir, "vmlinux", &format!("libmachine_cat={ONLINE}")))
            .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let exit = loop {
        match m.run_for(Duration::from_millis(2)).unwrap() {
            ExitReason::TimedOut => assert!(Instant::now() < deadline, "the guest never finished"),
            other => break other,
        }
    };
    let console = String::from_utf8_lossy(&m.serial_output()).into_owned();
    assert_eq!(exit, ExitReason::Poweroff, "{console}");
    assert_eq!(catted(&console, ONLINE), "0-3");
}
