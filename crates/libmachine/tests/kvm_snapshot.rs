// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Snapshots end to end: a test guest (`crates/libmachine/scripts/build-test-guest.sh`) boots
//! with its agent, is snapshotted, and clones restored from the file run
//! commands, each on its own copy of the guest's memory, with its own RNG
//! state and the right time.
//!
//! Snapshot files go under target/snapshot-tests and are removed after.
//! Tests return early with a note when /dev/kvm or the guest is missing.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

mod common;

use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::kvm_available;
use libmachine::agent::{Agent, Command};
use libmachine::{
    CpuModel, Error, ExitReason, LinuxBoot, Machine, MachineConfig, Snapshot, StopHandle,
};

const MEM: usize = 256 << 20;

fn guest_dir() -> Option<PathBuf> {
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

fn boot(dir: &Path, vcpus: u32) -> Machine {
    let boot = LinuxBoot {
        kernel: dir.join("vmlinux"),
        initrd: Some(dir.join("initramfs.cpio")),
        cmdline: "quiet libmachine_action=agent".into(),
    };
    let config = MachineConfig { mem_size: MEM, vcpus, ..Default::default() };
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
        let agent =
            m.agent().expect("a PC machine has an agent").timeout(Some(Duration::from_secs(60)));
        let stop = m.stop_handle();
        let runner = std::thread::spawn(move || {
            let exit = m.run_for(Duration::from_secs(300)).expect("run");
            (exit, m)
        });
        Self { agent, stop, runner: Some(runner) }
    }

    fn ready(self) -> Self {
        if let Err(e) = self.agent.wait_ready(Duration::from_secs(60)) {
            let (exit, m) = self.stop();
            panic!(
                "agent not ready: {e}; exit {exit:?}, console:\n{}",
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
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(runner) = self.runner.take() {
            self.stop.stop();
            let _ = runner.join();
        }
    }
}

fn sha256_of(path: &Path) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path).unwrap();
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 1 << 20];
    loop {
        let n = f.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    hasher.finalize().to_vec()
}

fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64()
}

/// The template: a booted guest with two vCPUs and its agent, a file in its
/// tmpfs, snapshotted between runs. Clones restored concurrently each run
/// commands, see the template's file, change it without the others or the
/// snapshot file seeing it, draw different random numbers, have the right
/// time and see the VM generation ID change.
#[test]
fn clones_of_a_booted_guest_run_on_their_own() {
    let Some(guest) = guest_dir() else { return };
    let dir = Dir::new("clones");
    let path = dir.file("template");

    let template = Running::start(boot(&guest, 2)).ready();
    template.sh("echo template > /tmp/marker");
    let template_random = template.sh("head -c 16 /dev/urandom | od -An -tx1");
    let (exit, mut m) = template.stop();
    assert_eq!(exit, ExitReason::Stopped);
    let start = Instant::now();
    m.snapshot(&path).expect("snapshot");
    eprintln!("snapshot of a {} MiB guest with 2 vCPUs: {:?}", MEM >> 20, start.elapsed());
    // The template runs on after its snapshot.
    let template = Running::start(m);
    assert_eq!(template.sh("cat /tmp/marker"), "template\n");
    drop(template);

    let before = sha256_of(&path);
    let snapshot = Snapshot::open(&path).unwrap();
    assert_eq!(snapshot.config().vcpus, 2);
    // Long enough that a clock left at the snapshot's time shows.
    std::thread::sleep(Duration::from_secs(3));

    // Restored at once, from three threads.
    let clones: Vec<Running> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..3)
            .map(|i| {
                let snapshot = &snapshot;
                s.spawn(move || {
                    let start = Instant::now();
                    let m = Machine::restore(snapshot, snapshot.config()).expect("restore");
                    eprintln!("clone {i} restored in {:?}", start.elapsed());
                    Running::start(m)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let results: Vec<(String, String)> = std::thread::scope(|s| {
        let handles: Vec<_> = clones
            .iter()
            .enumerate()
            .map(|(i, c)| {
                s.spawn(move || {
                    assert_eq!(c.sh("cat /tmp/marker"), "template\n", "clone {i}");
                    c.sh(&format!("echo clone{i} > /tmp/marker"));
                    let random = c.sh("head -c 16 /dev/urandom | od -An -tx1");
                    let host_before = now_secs();
                    let guest_time: f64 = c.sh("date +%s").trim().parse().unwrap();
                    let host_after = now_secs();
                    assert!(
                        guest_time >= host_before.floor() - 1.0 && guest_time <= host_after + 1.0,
                        "clone {i}: the guest says {guest_time}, the host {host_before}..{host_after}"
                    );
                    let cpus = c.sh("cat /sys/devices/system/cpu/online");
                    (random, cpus)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for (i, c) in clones.iter().enumerate() {
        assert_eq!(
            c.sh("cat /tmp/marker"),
            format!("clone{i}\n"),
            "each clone sees only its own write"
        );
        assert_eq!(results[i].1.trim(), "0-1", "both vCPUs run");
        // The VM generation ID changed, and the guest's kernel reseeded
        // (Linux's vmgenid driver, through the ACPI event).
        let start = Instant::now();
        loop {
            let log = c.sh("dmesg");
            if log.contains("crng reseeded due to virtual machine fork") {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "clone {i}: no VMGENID reseed in its log:\n{log}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let randoms: Vec<&String> = results.iter().map(|r| &r.0).collect();
    for (i, a) in randoms.iter().enumerate() {
        assert_ne!(**a, template_random);
        for b in &randoms[i + 1..] {
            assert_ne!(a, b, "two clones drew the same random bytes");
        }
    }
    drop(clones);
    assert_eq!(sha256_of(&path), before, "the snapshot file is unchanged");

    // A clone that had never run since the snapshot comes back the same.
    let again = Running::start(Machine::restore(&snapshot, snapshot.config()).unwrap());
    assert_eq!(again.sh("cat /tmp/marker"), "template\n");
}

/// A clone can be snapshotted in turn, and its clones work too (their TSC
/// and clock move on from the clone's).
#[test]
fn a_clone_can_be_snapshotted_again() {
    let Some(guest) = guest_dir() else { return };
    let dir = Dir::new("again");
    let template = Running::start(boot(&guest, 1)).ready();
    template.sh("echo one > /tmp/generation");
    let (_, mut m) = template.stop();
    m.snapshot(dir.file("first")).unwrap();
    let first = Snapshot::open(dir.file("first")).unwrap();
    let clone = Running::start(Machine::restore(&first, first.config()).unwrap());
    clone.sh("echo two >> /tmp/generation");
    let uptime_before: f64 = clone.sh("cut -d' ' -f1 /proc/uptime").trim().parse().unwrap();
    let (_, mut c) = clone.stop();
    c.snapshot(dir.file("second")).unwrap();
    let second = Snapshot::open(dir.file("second")).unwrap();
    let grandchild = Running::start(Machine::restore(&second, second.config()).unwrap());
    assert_eq!(grandchild.sh("cat /tmp/generation"), "one\ntwo\n");
    let uptime_after: f64 = grandchild.sh("cut -d' ' -f1 /proc/uptime").trim().parse().unwrap();
    assert!(
        uptime_after >= uptime_before,
        "monotonic time went back: {uptime_before} then {uptime_after}"
    );
    drop(grandchild);
    // Removing the file of a machine that is gone is fine; one still
    // mapped keeps its inode.
    std::fs::remove_file(dir.file("first")).unwrap();
}

/// Restore refuses a machine other than the snapshot's: vCPU count, RAM
/// size (the address map), CPU model; and snapshots are refused for bare
/// and networked machines. (A snapshot taken before the first run is a
/// snapshot like any other, and quick to take.)
#[test]
fn mismatches_are_refused() {
    let Some(guest) = guest_dir() else { return };
    let dir = Dir::new("mismatch");
    let mut m = boot(&guest, 2);
    m.snapshot(dir.file("s")).unwrap();
    let s = Snapshot::open(dir.file("s")).unwrap();
    let refusal = |config: MachineConfig| match Machine::restore(&s, config) {
        Err(Error::Snapshot(m)) => m,
        Err(other) => panic!("expected a snapshot refusal, got {other}"),
        Ok(_) => panic!("restored a machine unlike the snapshot's"),
    };
    let m1 = refusal(MachineConfig { vcpus: 1, ..s.config() });
    assert!(m1.contains("2 vCPUs"), "{m1}");
    let m2 = refusal(MachineConfig { mem_size: MEM * 2, ..s.config() });
    assert!(m2.contains("memory map"), "{m2}");
    // The baseline model differs from the host's on any host this runs on
    // (it has no AVX-512, or fewer leaves): the fingerprints differ.
    let baseline = MachineConfig { cpu_model: CpuModel::Baseline, ..s.config() };
    match Machine::restore(&s, baseline) {
        Err(Error::Snapshot(m)) => assert!(m.contains("fingerprint"), "{m}"),
        Err(Error::CpuModel(_)) => {} // this host cannot provide the baseline
        Err(other) => panic!("{other}"),
        Ok(_) => panic!("restored a host-model snapshot as baseline"),
    }
    // The right machine restores, and boots (the snapshot was taken before
    // the first run).
    let clone = Running::start(Machine::restore(&s, s.config()).unwrap()).ready();
    assert_eq!(clone.sh("nproc").trim(), "2");
    drop(clone);

    let mut bare = Machine::new(MachineConfig::default()).unwrap();
    assert!(matches!(
        bare.snapshot(dir.file("bare")),
        Err(Error::Snapshot(m)) if m.contains("PC machines")
    ));
    let boot_cfg = LinuxBoot {
        kernel: guest.join("vmlinux"),
        initrd: Some(guest.join("initramfs.cpio")),
        cmdline: "quiet libmachine_action=agent".into(),
    };
    let mut networked = Machine::boot_linux(
        MachineConfig {
            mem_size: MEM,
            network: Some(libmachine::net::NetworkConfig::new(
                libmachine::net::EgressPolicy::deny_all(),
            )),
            ..Default::default()
        },
        &boot_cfg,
    )
    .unwrap();
    assert!(matches!(
        networked.snapshot(dir.file("net")),
        Err(Error::Snapshot(m)) if m.contains("network")
    ));
    assert!(!dir.file("net").exists() && !dir.file("bare").exists());
}

/// How long a restore takes (to a machine ready to run) and how long until
/// its agent answers, reported for the record; under 10 ms is the
/// aspiration, not a requirement.
#[test]
fn restore_latency() {
    let Some(guest) = guest_dir() else { return };
    let dir = Dir::new("latency");
    let template = Running::start(boot(&guest, 1)).ready();
    let (_, mut m) = template.stop();
    let start = Instant::now();
    m.snapshot(dir.file("s")).unwrap();
    let snapshot_time = start.elapsed();
    drop(m);
    let start = Instant::now();
    let s = Snapshot::open(dir.file("s")).unwrap();
    let open_time = start.elapsed();
    let mut times: Vec<Duration> = (0..20)
        .map(|_| {
            let start = Instant::now();
            let m = Machine::restore(&s, s.config()).unwrap();
            let t = start.elapsed();
            drop(m);
            t
        })
        .collect();
    times.sort();
    // From the restore to the first command, in its steps (board 13177), over
    // a few clones: the guest taking the vsock's transport reset (the
    // first connection to the agent's port waits for it), the agent's first
    // answer (after its reseed) and the first command.
    let mut steps: Vec<[Duration; 4]> = (0..5)
        .map(|_| {
            let start = Instant::now();
            let m = Machine::restore(&s, s.config()).unwrap();
            let restored = start.elapsed();
            let vsock = m.vsock().unwrap();
            let clone = Running::start(m);
            drop(
                vsock
                    .connect(libmachine::agent::DEFAULT_PORT, Duration::from_secs(10))
                    .expect("connect to the agent's port"),
            );
            let connected = start.elapsed();
            clone.agent.ping().unwrap();
            let answered = start.elapsed();
            clone.sh("true");
            [restored, connected, answered, start.elapsed()]
        })
        .collect();
    steps.sort_by_key(|s| s[2]);
    let median = steps[steps.len() / 2];
    eprintln!(
        "restore latency ({} MiB, 1 vCPU): snapshot {snapshot_time:?}, open {open_time:?}; \
         restore median {:?}, p90 {:?}, min {:?}, max {:?} over {}; the median of {} clones \
         (by first answer), from the restore's start: restored {:?}, the guest took the vsock \
         reset (first connection) {:?}, its agent answered (reseeded) {:?}, ran a command {:?}",
        MEM >> 20,
        times[times.len() / 2],
        times[times.len() * 9 / 10],
        times[0],
        times[times.len() - 1],
        times.len(),
        steps.len(),
        median[0],
        median[1],
        median[2],
        median[3],
    );
}
