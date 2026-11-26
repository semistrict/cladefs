// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A dropped machine gives everything back: its vCPU threads exit, its KVM
//! file descriptors close and its guest RAM is unmapped, however it ended
//! (run to the end, stopped mid-run, timed out, never run).
//!
//! vCPU threads live as long as their machine, so anything they hold that
//! holds the machine back (a reference cycle, a thread nobody joins) would
//! keep every machine ever made alive. This test makes many and checks that
//! the process's threads, file descriptors and resident memory stay flat.
//! It is a test binary of its own so that no other test runs in the process
//! while it counts.
//!
//! Returns early with a note when /dev/kvm or the test guest is missing.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{kvm_available, machine_with, serial_print};
use libmachine::{ExitReason, LinuxBoot, Machine, MachineConfig};

const MIB: usize = 1 << 20;
/// Each machine's RAM, in line with every guest in the tests.
const RAM: usize = 256 * MIB;
/// How much of each machine's RAM the test fills, so that a machine whose
/// RAM is never unmapped shows in the resident memory.
const TOUCHED: usize = 64 * MIB;
const ROUNDS: usize = 6;

fn guest() -> Option<PathBuf> {
    if !kvm_available() {
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

fn pc(dir: &Path, vcpus: u32, cmdline: &str) -> Machine {
    let boot = LinuxBoot {
        kernel: dir.join("vmlinux"),
        initrd: Some(dir.join("initramfs.cpio")),
        cmdline: cmdline.into(),
    };
    let config = MachineConfig { mem_size: RAM, vcpus, ..Default::default() };
    Machine::boot_linux(config, &boot).expect("boot")
}

fn count_dir(path: &str) -> usize {
    std::fs::read_dir(path).expect(path).count()
}

/// `count` once it has settled: two reads 10 ms apart that agree (at most
/// 2 s). A thread just joined may still be listed in /proc/self/task until
/// the kernel has reaped it (pthread_join returns when the thread clears
/// its TID, before its task is released), so one read right after a join
/// can count it: seen once in a check run, 3 threads "before" and 2 after.
fn settled(count: impl Fn() -> usize) -> usize {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut last = count();
    loop {
        std::thread::sleep(Duration::from_millis(10));
        let now = count();
        if now == last || std::time::Instant::now() > deadline {
            return now;
        }
        last = now;
    }
}

/// The process's threads whose name starts with `vcpu`.
fn vcpu_threads() -> usize {
    settled(|| {
        std::fs::read_dir("/proc/self/task")
            .unwrap()
            .filter(|t| {
                let comm = t.as_ref().unwrap().path().join("comm");
                std::fs::read_to_string(comm).is_ok_and(|c| c.starts_with("vcpu"))
            })
            .count()
    })
}

fn resident() -> usize {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
    let pages: usize = statm.split_whitespace().nth(1).unwrap().parse().unwrap();
    pages * 4096
}

#[derive(Debug, Clone, Copy)]
struct Usage {
    threads: usize,
    fds: usize,
    resident: usize,
}

fn usage() -> Usage {
    Usage {
        threads: settled(|| count_dir("/proc/self/task")),
        fds: count_dir("/proc/self/fd"),
        resident: resident(),
    }
}

/// One round: a machine of each kind and each way of ending, each dropped
/// before the next is made, with its threads checked while it lives.
fn round(dir: &Path, fill: &[u8]) {
    // A bare machine, run to its HLT.
    let mut m =
        machine_with(MachineConfig { mem_size: RAM, ..Default::default() }, &serial_print(b"hi"));
    assert_eq!(m.run_for(Duration::from_secs(10)).unwrap(), ExitReason::Halt);
    assert_eq!(vcpu_threads(), 1);
    m.load(MIB as u64, fill).unwrap();
    drop(m);
    assert_eq!(vcpu_threads(), 0, "the bare machine's thread is gone");

    // A PC with 4 vCPUs, run until the guest powers off.
    let mut m = pc(dir, 4, "quiet");
    assert_eq!(m.run_for(Duration::from_secs(30)).unwrap(), ExitReason::Poweroff);
    assert_eq!(vcpu_threads(), 4);
    m.load(16 * MIB as u64, fill).unwrap();
    drop(m);
    assert_eq!(vcpu_threads(), 0, "the PC's threads are gone");

    // A PC with 4 vCPUs dropped while its guest is up: timed out, then
    // stopped.
    let mut m = pc(dir, 4, "quiet libmachine_action=hang");
    assert_eq!(m.run_for(Duration::from_millis(300)).unwrap(), ExitReason::TimedOut);
    m.stop_handle().stop();
    assert_eq!(m.run().unwrap(), ExitReason::Stopped);
    drop(m);
    assert_eq!(vcpu_threads(), 0);

    // A PC never run.
    let m = pc(dir, 2, "");
    assert_eq!(vcpu_threads(), 2);
    drop(m);
    assert_eq!(vcpu_threads(), 0);
}

#[test]
fn dropped_machines_give_back_their_threads_fds_and_memory() {
    let Some(dir) = guest() else { return };
    let fill = vec![0xa5u8; TOUCHED];
    // The first round sets up what lives as long as the process (the kick
    // signal's handler, allocator arenas for new threads).
    round(&dir, &fill);
    let before = usage();
    for _ in 0..ROUNDS {
        round(&dir, &fill);
    }
    let after = usage();
    assert_eq!(after.threads, before.threads, "{before:?} -> {after:?}");
    assert_eq!(after.fds, before.fds, "{before:?} -> {after:?}");
    // One leaked machine would hold TOUCHED; the margin is for the
    // allocator.
    assert!(
        after.resident < before.resident + TOUCHED / 2,
        "resident memory grew by {} MiB: {before:?} -> {after:?}",
        (after.resident.saturating_sub(before.resident)) / MIB
    );
}
