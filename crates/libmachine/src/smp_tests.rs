// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Several vCPUs with the test guest (`crates/libmachine/scripts/build-test-guest.sh`), from
//! inside the crate, because which vCPU ended a run and the CPU time of each
//! vCPU thread are internal. Skips with a note when /dev/kvm or the guest
//! is missing.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::{ExitReason, LinuxBoot, Machine, MachineConfig};

fn guest() -> Option<PathBuf> {
    if std::fs::OpenOptions::new().read(true).write(true).open("/dev/kvm").is_err() {
        eprintln!("skipping: /dev/kvm is not accessible");
        return None;
    }
    let dir = std::env::var_os("LIBMACHINE_GUEST_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/guest"),
        PathBuf::from,
    );
    if ["vmlinux", "initramfs.cpio"].iter().all(|f| dir.join(f).is_file()) {
        Some(dir)
    } else {
        eprintln!(
            "skipping: no test guest in {} (run crates/libmachine/scripts/build-test-guest.sh)",
            dir.display()
        );
        None
    }
}

fn machine(dir: &std::path::Path, vcpus: u32, cmdline: &str) -> Machine {
    let boot = LinuxBoot {
        kernel: dir.join("vmlinux"),
        initrd: Some(dir.join("initramfs.cpio")),
        cmdline: cmdline.into(),
    };
    let config = MachineConfig { mem_size: 256 << 20, vcpus, ..Default::default() };
    Machine::boot_linux(config, &boot).expect("boot")
}

fn console(m: &Machine) -> String {
    String::from_utf8_lossy(&m.serial_output()).into_owned()
}

/// Generous bound for "promptly", so a loaded host does not flake.
const PROMPT: Duration = Duration::from_secs(2);

/// Linux powers off and resets from its reboot CPU, which `reboot=sN`
/// chooses: the run ends from whichever vCPU writes the ACPI register.
#[test]
fn poweroff_and_reset_from_any_vcpu_end_the_run() {
    let Some(dir) = guest() else { return };
    for (cmdline, exit, vcpu) in [
        ("quiet", ExitReason::Poweroff, 0),
        ("quiet reboot=s1", ExitReason::Poweroff, 1),
        ("quiet reboot=s3", ExitReason::Poweroff, 3),
        ("quiet reboot=s2 libmachine_action=reboot", ExitReason::Reset, 2),
    ] {
        let mut m = machine(&dir, 4, cmdline);
        let got = m.run_for(Duration::from_secs(30)).unwrap();
        assert_eq!(got, exit, "{cmdline}:\n{}", console(&m));
        assert_eq!(m.last_exit_vcpu(), Some(vcpu), "{cmdline}");
    }
}

/// Runs a guest that keeps every CPU busy until init says so.
fn spinning(dir: &std::path::Path, vcpus: u32) -> Machine {
    let mut m = machine(dir, vcpus, "quiet libmachine_action=spin");
    let marker = format!("libmachine-init: spinning on {vcpus} CPUs");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !console(&m).contains(&marker) {
        assert!(Instant::now() < deadline, "console:\n{}", console(&m));
        assert_eq!(m.run_for(Duration::from_millis(100)).unwrap(), ExitReason::TimedOut);
    }
    m
}

fn deltas(after: &[Duration], before: &[Duration]) -> Vec<Duration> {
    after.iter().zip(before).map(|(a, b)| *a - *b).collect()
}

/// Each vCPU runs on a thread of its own, and a stop or a timeout stops
/// every one of them: while the machine is stopped, no vCPU thread uses
/// CPU time.
#[test]
fn every_vcpu_thread_runs_and_all_stop() {
    let Some(dir) = guest() else { return };
    let mut m = spinning(&dir, 4);
    assert_eq!(m.vcpu_thread_cpu_times().len(), 4);

    // A timeout while all four spin.
    let before = m.vcpu_thread_cpu_times();
    assert_eq!(m.run_for(Duration::from_millis(400)).unwrap(), ExitReason::TimedOut);
    let after = m.vcpu_thread_cpu_times();
    for (i, d) in deltas(&after, &before).iter().enumerate() {
        assert!(*d > Duration::from_millis(40), "vCPU {i}'s thread ran the guest for only {d:?}");
    }
    let quiet = |m: &Machine, what: &str| {
        let start = m.vcpu_thread_cpu_times();
        std::thread::sleep(Duration::from_millis(200));
        for (i, d) in deltas(&m.vcpu_thread_cpu_times(), &start).iter().enumerate() {
            assert!(
                *d < Duration::from_millis(20),
                "after {what}, vCPU {i}'s thread still used {d:?}"
            );
        }
    };
    quiet(&m, "the timeout");

    // A stop while all four spin.
    let handle = m.stop_handle();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        let at = Instant::now();
        handle.stop();
        at
    });
    assert_eq!(m.run().unwrap(), ExitReason::Stopped);
    let returned = Instant::now();
    let stopped_at = stopper.join().unwrap();
    assert!(returned - stopped_at < PROMPT);
    quiet(&m, "the stop");

    // All four resume.
    let before = m.vcpu_thread_cpu_times();
    assert_eq!(m.run_for(Duration::from_millis(300)).unwrap(), ExitReason::TimedOut);
    for (i, d) in deltas(&m.vcpu_thread_cpu_times(), &before).iter().enumerate() {
        assert!(*d > Duration::from_millis(30), "vCPU {i} did not resume: {d:?}");
    }
}
