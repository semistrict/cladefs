// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Boots cladefs's guest kernel (Linux with Fxfs and the virtio extent-store driver, a PVH
//! vmlinux: the kernel Firecracker boots, `scripts/guest-kernel.sh`) with the test initramfs
//! (`crates/libmachine/scripts/build-test-guest.sh`).
//!
//! The kernel is `CLADEFS_GUEST_KERNEL`, the initramfs is in `LIBMACHINE_GUEST_DIR` (default
//! `target/guest`); scripts/test.sh's libmachine part sets both. Tests return early with a note when
//! /dev/kvm, the kernel or the initramfs is missing.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

mod common;

use std::path::PathBuf;

use common::{cladefs_kernel, kvm_available, test_guest_dir};
use libmachine::{ExitCounts, ExitReason, LinuxBoot, Machine, MachineConfig};

const INIT_MARKER: &str = "libmachine-init: up ";

/// cladefs's kernel and the test initramfs, if /dev/kvm and both are there.
fn guest() -> Option<(PathBuf, PathBuf)> {
    if !kvm_available() {
        return None;
    }
    let kernel = cladefs_kernel()?;
    let initrd = test_guest_dir().join("initramfs.cpio");
    if !initrd.is_file() {
        eprintln!(
            "skipping: no test initramfs at {} (run crates/libmachine/scripts/build-test-guest.sh)",
            initrd.display()
        );
        return None;
    }
    Some((kernel, initrd))
}

/// Boots cladefs's kernel with `cmdline`, runs it to the end, and returns the
/// exit and the console, its lines ending in "\n" (the guest's tty sends
/// "\r\n").
fn run(cmdline: &str) -> Option<(ExitReason, String)> {
    run_with_vcpus(cmdline, 1).map(|(exit, console, _)| (exit, console))
}

/// [`run`] with `vcpus` vCPUs; also returns the exits the machine took.
fn run_with_vcpus(cmdline: &str, vcpus: u32) -> Option<(ExitReason, String, ExitCounts)> {
    let (kernel, initrd) = guest()?;
    let boot = LinuxBoot { kernel, initrd: Some(initrd), cmdline: cmdline.into() };
    let config = MachineConfig { mem_size: 256 << 20, vcpus, ..Default::default() };
    let mut m = Machine::boot_linux(config, &boot).expect("boot");
    let exit = m.run().expect("run");
    let console = String::from_utf8_lossy(&m.serial_output()).replace("\r\n", "\n");
    Some((exit, console, m.exit_counts()))
}

/// What init printed of `path` (`libmachine_cat`), or None if it could not
/// read it.
fn catted<'a>(console: &'a str, path: &str) -> Option<&'a str> {
    let start = format!("libmachine-init: {path}:\n");
    let end = format!("libmachine-init: end of {path}\n");
    let from = console.find(&start)? + start.len();
    let to = from + console[from..].find(&end)?;
    Some(&console[from..to])
}

#[test]
fn cladefs_kernel_boots_to_init_and_powers_off() {
    let Some((exit, console)) = run("") else {
        return;
    };
    assert_eq!(exit, ExitReason::Poweroff, "console:\n{console}");
    assert!(console.contains(INIT_MARKER), "console:\n{console}");
    assert!(console.contains("reboot: Power down"), "console:\n{console}");
}

#[test]
fn cladefs_kernel_reboot_ends_the_run_with_a_reset() {
    let Some((exit, console)) = run("libmachine_action=reboot") else {
        return;
    };
    assert_eq!(exit, ExitReason::Reset, "console:\n{console}");
    assert!(console.contains(INIT_MARKER), "console:\n{console}");
}

/// The kernel is cladefs's: its Fxfs is registered and the extent-store driver is
/// on the virtio bus, waiting for its device (board 13067).
#[test]
fn cladefs_kernel_has_fxfs_and_the_extent_driver() {
    let filesystems = "/proc/filesystems";
    let driver = "/sys/bus/virtio/drivers/virtio_extent";
    let Some((exit, console)) = run(&format!("quiet libmachine_cat={filesystems},{driver}")) else {
        return;
    };
    assert_eq!(exit, ExitReason::Poweroff, "console:\n{console}");
    let fs = catted(&console, filesystems).unwrap_or_else(|| panic!("console:\n{console}"));
    assert!(
        fs.lines().any(|l| l.split_whitespace().last() == Some("cladefs")),
        "no cladefs in {filesystems}:\n{fs}"
    );
    // A driver's directory holds bind and unbind, whether or not a device
    // is bound to it.
    let entries = catted(&console, driver).unwrap_or_else(|| panic!("console:\n{console}"));
    assert!(entries.lines().any(|l| l == "bind"), "{driver} lists:\n{entries}");
}

/// Console lines the platform still causes, each with the task that removes
/// it: delete the entry with the fix.
const KNOWN: &[&str] = &[];

/// The platform cladefs's kernel sees is clean: ACPI loads, COM1 comes from
/// the DSDT, the CPU model is consistent, and nothing warns. This kernel
/// runs far more of the kernel's platform code than the minimal test kernel
/// (CPU vendor code, cpufreq, i8042, perf), so it is the stricter check.

#[test]
fn cladefs_kernel_sees_a_consistent_platform() {
    let Some((_, console)) = run("") else { return };
    let console: String = console
        .lines()
        .filter(|l| !KNOWN.iter().any(|k| l.ends_with(k)))
        .map(|l| format!("{l}\n"))
        .collect();
    for bad in [
        "ACPI Error",
        "ACPI BIOS Error",
        "ACPI Warning",
        "Unable to load the System Description Tables",
        "PCI: Fatal",
        "dependency check failure",
        "Power off not available",
        "[Firmware Bug]",
        "the BIOS",
        "amd_pstate",
        "intel_pstate",
        "WARNING:",
        "Call Trace",
        "BUG:",
        "Oops",
        "Kernel panic",
    ] {
        assert!(!console.contains(bad), "found {bad:?} in console:\n{console}");
    }
    // The guest has no PMU (board 13121): Linux falls back to software events
    // ("PMU not available due to virtualization, using software events only"
    // or "no PMU driver, software events only") rather than naming a driver.
    if let Some(line) = console.lines().find(|l| l.contains("Performance Events:")) {
        assert!(line.contains("software events only"), "the guest found a PMU: {line}");
    }
    for good in [
        "ACPI: Using ACPI (MADT) for SMP configuration information",
        "00:00: ttyS0 at I/O 0x3f8",
        "ACPI: PM: Preparing to enter system sleep state S5",
        "fxfs: Rust runtime ready",
    ] {
        assert!(console.contains(good), "missing {good:?} in console:\n{console}");
    }
}

/// cladefs's kernel, the one libmachine ships, brings up every vCPU.
#[test]
fn cladefs_kernel_brings_up_every_vcpu() {
    let online = "/sys/devices/system/cpu/online";
    let Some((exit, console, _)) = run_with_vcpus(&format!("quiet libmachine_cat={online}"), 4)
    else {
        return;
    };
    assert_eq!(exit, ExitReason::Poweroff, "console:\n{console}");
    assert_eq!(catted(&console, online), Some("0-3\n"), "console:\n{console}");
}

/// The most vCPU exits a quiet boot of cladefs's kernel to power-off may
/// take, with 1 and 4 vCPUs. Measured after board 13089's vsock joined every PC
/// machine (3 boots each): 1,225-1,252 (the test kernel's exits, plus some
/// 50 more unclaimed port accesses from the platform drivers it has). The
/// budget is the highest count plus about a fifth; see `kvm_linux.rs`'s
/// `QUIET_BOOT_EXITS` for the reasoning and for why it was raised.
const QUIET_BOOT_EXITS: &[(u32, u64)] = &[(1, 1_500), (4, 1_500)];

#[test]
fn cladefs_kernel_quiet_boot_stays_within_its_exit_budget() {
    for &(vcpus, budget) in QUIET_BOOT_EXITS {
        let Some((exit, console, counts)) = run_with_vcpus("quiet", vcpus) else {
            return;
        };
        assert_eq!(exit, ExitReason::Poweroff, "console:\n{console}");
        eprintln!("quiet boot, cladefs kernel x{vcpus} vCPU: {counts}");
        assert!(counts.total() <= budget, "x{vcpus}: {counts}, over the budget of {budget}");
    }
}

/// A machine running on a thread of its own, its agent at hand; stopped
/// (and its run joined) when dropped.
struct Running {
    agent: libmachine::agent::Agent,
    stop: libmachine::StopHandle,
    runner: Option<std::thread::JoinHandle<(ExitReason, Machine)>>,
}

impl Running {
    fn start(mut m: Machine) -> Self {
        let agent = m
            .agent()
            .expect("a PC machine has an agent")
            .timeout(Some(std::time::Duration::from_secs(60)));
        let stop = m.stop_handle();
        let runner = std::thread::spawn(move || {
            let exit = m.run_for(std::time::Duration::from_secs(300)).expect("run");
            (exit, m)
        });
        Self { agent, stop, runner: Some(runner) }
    }

    fn stop(mut self) -> (ExitReason, Machine) {
        self.stop.stop();
        self.runner.take().unwrap().join().expect("runner")
    }

    fn sh(&self, script: &str) -> String {
        let mut cmd = libmachine::agent::Command::new("sh");
        cmd.args(["-c", script]);
        let out = self.agent.exec(&cmd).unwrap_or_else(|e| panic!("{script}: {e}"));
        assert!(out.status.success(), "{script}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(runner) = self.runner.take() {
            self.stop.stop();
            let _ = runner.join();
        }
    }
}

/// board 13181: cladefs's kernel, booted with the agent, snapshotted between runs;
/// two clones restored from it at once each run commands on their own copy
/// (the template's file, their own writes), with their own random numbers
/// (its VMGENID driver reseeded: the kernel needs CONFIG_VMGENID, which
/// scripts/build-guest-kernel.sh checks) and the right time.
#[test]
fn a_snapshot_of_cladefss_kernel_restores_and_runs_commands() {
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    let Some((kernel, initrd)) = guest() else {
        return;
    };
    let boot =
        LinuxBoot { kernel, initrd: Some(initrd), cmdline: "quiet libmachine_action=agent".into() };
    let config = MachineConfig { mem_size: 256 << 20, vcpus: 2, ..Default::default() };
    let template = Running::start(Machine::boot_linux(config, &boot).expect("boot"));
    template.agent.wait_ready(Duration::from_secs(60)).expect("the agent answers");
    template.sh("echo cladefs > /tmp/marker");
    let template_random = template.sh("head -c 16 /dev/urandom | od -An -tx1");
    let (exit, mut m) = template.stop();
    assert_eq!(exit, ExitReason::Stopped);
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/snapshot-tests")
        .join("cladefs");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("template");
    m.snapshot(&path).expect("snapshot cladefs's kernel");
    drop(m);
    let snapshot = libmachine::Snapshot::open(&path).unwrap();
    std::thread::sleep(Duration::from_secs(2));

    let clones: Vec<(Running, Duration)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let snapshot = &snapshot;
                s.spawn(move || {
                    let start = Instant::now();
                    let clone =
                        Running::start(Machine::restore(snapshot, snapshot.config()).unwrap());
                    clone.agent.ping().expect("the clone's agent answers");
                    (clone, start.elapsed())
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut randoms = Vec::new();
    for (i, (c, answered)) in clones.iter().enumerate() {
        eprintln!("cladefs clone {i}: its agent answered {answered:?} after the restore began");
        assert_eq!(c.sh("cat /tmp/marker"), "cladefs\n");
        c.sh(&format!("echo clone{i} > /tmp/marker"));
        randoms.push(c.sh("head -c 16 /dev/urandom | od -An -tx1"));
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64();
        let guest: f64 = c.sh("date +%s").trim().parse().unwrap();
        assert!((guest - now).abs() <= 2.0, "clone {i}: {guest} against {now}");
        assert_eq!(c.sh("nproc").trim(), "2");
    }
    for (i, (c, _)) in clones.iter().enumerate() {
        assert_eq!(c.sh("cat /tmp/marker"), format!("clone{i}\n"));
        let start = Instant::now();
        while !c.sh("dmesg").contains("crng reseeded due to virtual machine fork") {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "cladefs clone {i}: no VMGENID reseed in its log"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    assert_ne!(randoms[0], randoms[1]);
    assert!(!randoms.contains(&template_random));
    drop(clones);
    let _ = std::fs::remove_dir_all(&dir);
}
