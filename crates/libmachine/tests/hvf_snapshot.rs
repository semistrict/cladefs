// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Snapshots on Hypervisor.framework (board 13399): the arm64 test guest boots with its agent on
//! two vCPUs, is snapshotted, and clones restored from the file one after another (a process has
//! one VM) run commands, each on its own copy of the guest's memory, both vCPUs online, their
//! timers firing, the guest's clock going on from the snapshot's.
//!
//! The kernel and initramfs are as `hvf_linux.rs` finds them; snapshot files go under
//! target/snapshot-tests and are removed after. Tests return early with a note when the guest is
//! missing.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use libmachine::agent::{Agent, Command};
use libmachine::{
    ExitReason, LinuxBoot, Machine, MachineConfig, Snapshot, SnapshotOptions, StopHandle,
};

const MEM: usize = 512 << 20;

/// Hypervisor.framework gives a process one VM: one test's machine at a time.
fn one_machine() -> MutexGuard<'static, ()> {
    static ONE: Mutex<()> = Mutex::new(());
    ONE.lock().unwrap_or_else(|e| e.into_inner())
}

fn guest() -> Option<(PathBuf, PathBuf)> {
    let dir = std::env::var_os("LIBMACHINE_GUEST_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/arm64-guest"),
        PathBuf::from,
    );
    let kernel =
        std::env::var_os("CLADEFS_GUEST_KERNEL").map_or_else(|| dir.join("Image"), PathBuf::from);
    let initrd = dir.join("initramfs.cpio");
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

/// A directory of the test's own for snapshot files, removed when dropped.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/snapshot-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn boot((kernel, initrd): &(PathBuf, PathBuf), vcpus: u32) -> Machine {
    let boot = LinuxBoot {
        kernel: kernel.clone(),
        initrd: Some(initrd.clone()),
        cmdline: "quiet libmachine_action=agent".into(),
    };
    let config = MachineConfig { mem_size: MEM, vcpus, balloon: true, ..Default::default() };
    Machine::boot_linux(config, &boot).expect("boot")
}

/// A machine running on a thread of its own, its agent at hand.
struct Running {
    agent: Agent,
    stop: StopHandle,
    runner: Option<JoinHandle<(ExitReason, Machine)>>,
}

impl Running {
    fn start(mut m: Machine) -> Self {
        let agent = m.agent().expect("a board has an agent").timeout(Some(Duration::from_secs(60)));
        let stop = m.stop_handle();
        let runner = std::thread::spawn(move || {
            let exit = m.run_for(Duration::from_secs(300)).expect("run");
            (exit, m)
        });
        Self { agent, stop, runner: Some(runner) }
    }

    /// Waits for the agent of `what` machine.
    fn ready(self, what: &str) -> Self {
        if let Err(e) = self.agent.wait_ready(Duration::from_secs(60)) {
            let (exit, m) = self.stop();
            panic!(
                "{what}: agent not ready: {e}; exit {exit:?}, {:?}, console:\n{}",
                m.exit_counts(),
                String::from_utf8_lossy(&m.serial_output())
            );
        }
        self
    }

    /// Stops the run: its outcome, and the machine.
    fn stop(mut self) -> (ExitReason, Machine) {
        self.stop.stop();
        self.runner.take().unwrap().join().expect("runner")
    }

    fn sh(&self, script: &str) -> String {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        let out = self.agent.exec(&cmd).unwrap_or_else(|e| panic!("{script}: {e}"));
        assert!(out.status.success(), "{script}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    /// The guest's CLOCK_MONOTONIC, from its uptime, in seconds.
    fn uptime(&self) -> f64 {
        let out = self.sh("cat /proc/uptime");
        out.split_whitespace().next().unwrap().parse().unwrap()
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

/// How long the snapshot waits on disk before its clones run: time that must not pass for them.
const IN_THE_FILE: Duration = Duration::from_secs(4);

/// Boots, leaves a mark in the guest's memory, snapshots it (its RAM in the snapshot's file, or
/// in a file of its own), and checks three clones: each sees the mark and not the others', both
/// vCPUs online, a `sleep` that takes its second (the timers fire), and a clock that went on
/// from the snapshot's, not through the time the snapshot spent in its file.
fn snapshot_and_clone(split: bool) {
    let Some(guest) = guest() else { return };
    let _one = one_machine();
    let dir = Dir::new(if split { "hvf-split" } else { "hvf-clones" });
    let (path, ram) = (dir.file("snapshot"), dir.file("memory"));

    let running = Running::start(boot(&guest, 2)).ready("booted");
    running.sh("echo before-the-snapshot > /tmp/mark");
    let uptime_then = running.uptime();
    let (exit, mut m) = running.stop();
    assert_eq!(exit, ExitReason::Stopped);
    let options = SnapshotOptions {
        ram: split.then_some(ram.as_path()),
        caller_data: b"ours",
        ..Default::default()
    };
    m.snapshot_with(&path, &options).expect("snapshot");
    // The machine runs on after its snapshot; then it goes, as a process has one VM.
    let running = Running::start(m).ready("after its snapshot");
    assert_eq!(running.sh("cat /tmp/mark"), "before-the-snapshot\n");
    drop(running);
    let taken = Instant::now();

    let snapshot = match split {
        true => {
            assert!(Snapshot::open(&path).is_err(), "its RAM is elsewhere");
            assert_eq!(std::fs::metadata(&ram).unwrap().len(), MEM as u64);
            Snapshot::open_split(&path, &ram).unwrap()
        }
        false => Snapshot::open(&path).unwrap(),
    };
    assert_eq!(snapshot.caller_data(), b"ours");
    assert_eq!(snapshot.config().vcpus, 2);
    assert!(snapshot.config().balloon);
    std::thread::sleep(IN_THE_FILE.saturating_sub(taken.elapsed()));
    for clone in 0..3 {
        let started = Instant::now();
        let m = Machine::restore(&snapshot, snapshot.config()).expect("restore");
        let running = Running::start(m).ready(&format!("clone {clone}"));
        let uptime_now = running.uptime();
        let since = started.elapsed().as_secs_f64();
        assert!(
            uptime_then <= uptime_now && uptime_now - uptime_then <= since + 1.0,
            "clone {clone}: the guest's clock went from {uptime_then} s to {uptime_now} s in a \
             restore that took {since} s"
        );
        assert_eq!(running.sh("cat /tmp/mark; ls /tmp"), "before-the-snapshot\nmark\n");
        running.sh(&format!("echo clone-{clone} > /tmp/clone-{clone}"));
        assert_eq!(running.sh("cat /sys/devices/system/cpu/online"), "0-1\n");
        let slept = Instant::now();
        running.sh("sleep 1");
        let slept = slept.elapsed();
        assert!(slept >= Duration::from_millis(900), "a 1 s sleep took {slept:?}");
        assert!(slept < Duration::from_secs(10), "a 1 s sleep took {slept:?}");
        // Work on both vCPUs.
        assert_eq!(
            running.sh("taskset -c 1 cat /proc/self/stat | cut -d' ' -f39; nproc"),
            "1\n2\n"
        );
        drop(running);
    }
}

#[test]
fn clones_run_from_a_snapshot() {
    snapshot_and_clone(false);
}

#[test]
fn clones_run_from_a_snapshot_with_its_ram_apart() {
    snapshot_and_clone(true);
}

/// A restore refuses a machine that isn't the snapshot's.
#[test]
fn a_restore_refuses_another_machine() {
    let Some(guest) = guest() else { return };
    let _one = one_machine();
    let dir = Dir::new("hvf-refusals");
    let path = dir.file("snapshot");
    let running = Running::start(boot(&guest, 1)).ready("booted");
    let (_, mut m) = running.stop();
    m.snapshot(&path).unwrap();
    drop(m);
    let s = Snapshot::open(&path).unwrap();
    let refusal = |config: MachineConfig| {
        let err = Machine::restore(&s, config).expect_err("refused").to_string();
        eprintln!("{err}");
        err
    };
    assert!(refusal(MachineConfig { vcpus: 2, ..s.config() }).contains("1 vCPUs"));
    assert!(refusal(MachineConfig { mem_size: MEM * 2, ..s.config() }).contains("memory map"));
    assert!(refusal(MachineConfig { balloon: false, ..s.config() }).contains("balloon"));
    let disk = dir.file("disk");
    std::fs::File::create(&disk).unwrap().set_len(1 << 20).unwrap();
    let disks = vec![libmachine::Disk::new(&disk)];
    assert!(refusal(MachineConfig { disks, ..s.config() }).contains("disks"));
    // The snapshot's own machine restores.
    let running = Running::start(Machine::restore(&s, s.config()).unwrap()).ready("restored");
    assert_eq!(running.sh("echo up"), "up\n");
}

/// A snapshot file of its own is read only: machines map it.
#[test]
fn the_files_are_read_only() {
    let Some(guest) = guest() else { return };
    let _one = one_machine();
    let dir = Dir::new("hvf-read-only");
    let running = Running::start(boot(&guest, 1)).ready("booted");
    let (_, mut m) = running.stop();
    let ram = dir.file("memory");
    let options = SnapshotOptions { ram: Some(&ram), ..Default::default() };
    m.snapshot_with(dir.file("snapshot"), &options).unwrap();
    for file in [dir.file("snapshot"), ram] {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o400, "{}", Path::new(&file).display());
    }
}

/// Many clones from one snapshot taken with both vCPUs busy, each restored, answering on both
/// vCPUs and dropped: every one answers, its timers firing on both.
#[test]
fn every_clone_answers() {
    let Some(guest) = guest() else { return };
    let _one = one_machine();
    let dir = Dir::new("hvf-many");
    let path = dir.file("snapshot");
    let running = Running::start(boot(&guest, 2)).ready("booted");
    // Both vCPUs busy and idle in turn as the snapshot comes.
    running.sh("(while :; do taskset -c 1 true; sleep 0.001; done) </dev/null >/dev/null 2>&1 &");
    std::thread::sleep(Duration::from_millis(200));
    let (_, mut m) = running.stop();
    m.snapshot(&path).unwrap();
    drop(m);
    let snapshot = Snapshot::open(&path).unwrap();
    for clone in 0..40 {
        let m = Machine::restore(&snapshot, snapshot.config()).expect("restore");
        let running = Running::start(m);
        if let Err(e) = running.agent.wait_ready(Duration::from_secs(20)) {
            let (exit, m) = running.stop();
            panic!(
                "clone {clone}: {e}; exit {exit:?}; console:\n{}",
                String::from_utf8_lossy(&m.serial_output())
            )
        }
        assert_eq!(running.sh("taskset -c 0 sleep 0.05; taskset -c 1 sleep 0.05; echo ok"), "ok\n");
    }
}
