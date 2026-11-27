// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's extent store device on Hypervisor.framework (board 13553): cladefs's arm64 kernel
//! with the test initramfs and its agent mounts Fxfs from the device, its cell served by cladefs's
//! extent daemon embedded in the test's process (libcladefs), connected in memory, and its writes,
//! fsyncs and reads hold across a remount: without a DAX window, and with one (board 13069).
//!
//! The guest is where `hvf_linux` finds it (`crates/libmachine/scripts/arm64-guest.sh`); the test
//! returns early with a note when it is missing. A process has one daemon and one VM: one test.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use libmachine::agent::Command;
use libmachine::{
    ExitReason, ExtentCell, ExtentDaemon, ExtentDaemonConfig, ExtentStore, LinuxBoot, Machine,
    MachineConfig,
};
use virtio_extent::protocol::VIRTIO_EXTENT_F_DAX;

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

/// What [`mount_and_check`] saw: the smoke script's output, the extent driver's features as
/// sysfs prints them, the run's exit, its exit counts and the console.
struct Seen {
    smoke: libmachine::agent::Output,
    features: String,
    exit: libmachine::Result<ExitReason>,
    counts: libmachine::ExitCounts,
    console: String,
}

/// Boots a guest on 2 vCPUs with an extent store device on a new cell of the embedded daemon's, a
/// DAX window of `dax_window_size` bytes if given, which mounts Fxfs from it, writes and fsyncs a
/// file, and reads it back after a remount, from the cell the daemon committed, and after its
/// pages are dropped.
fn mount_and_check(dax_window_size: Option<u64>) -> Option<Seen> {
    let (kernel, initrd) = guest()?;
    // Its log on standard error, which a failed test shows.
    let _ = cladefs::cladefsd::logging::to_stderr();
    let dir = tempfile::tempdir().unwrap();
    let daemon = ExtentDaemon::start(ExtentDaemonConfig::local(dir.path()).memory(256 << 20))
        .expect("the embedded daemon");
    let store = ExtentStore {
        dax_window_size,
        ..ExtentStore::new("extents", ExtentCell::New { tenant: 1 }, 256 << 20)
    };
    let config = MachineConfig {
        mem_size: 512 << 20,
        vcpus: 2,
        extent_daemon: Some(Arc::new(daemon)),
        extent_stores: vec![store],
        ..Default::default()
    };
    let boot =
        LinuxBoot { kernel, initrd: Some(initrd), cmdline: "quiet libmachine_action=agent".into() };
    let mut m = Machine::boot_linux(config, &boot).expect("boot");
    let agent = m.agent().expect("a board has the agent's vsock");
    let stop = m.stop_handle();
    let runner = std::thread::spawn(move || {
        let exit = m.run_for(Duration::from_secs(300));
        (exit, m)
    });
    let sh = |script: &str| {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        agent.exec(&cmd)
    };
    let ran = agent.wait_ready(Duration::from_secs(30)).and_then(|()| {
        let features = sh("cat /sys/bus/virtio/drivers/virtio_extent/virtio*/features")?;
        let smoke = sh("set -e
             mkdir -p /mnt
             mount -t cladefs extents /mnt
             dd if=/dev/urandom of=/mnt/smoke bs=64k count=16 conv=fsync 2>/dev/null
             sha256sum /mnt/smoke > /tmp/smoke.sum
             umount /mnt
             mount -t cladefs extents /mnt
             sha256sum -c /tmp/smoke.sum
             echo 3 > /proc/sys/vm/drop_caches
             sha256sum -c /tmp/smoke.sum
             umount /mnt")?;
        Ok((features, smoke))
    });
    stop.stop();
    let (exit, m) = runner.join().expect("the run thread");
    let console = String::from_utf8_lossy(&m.serial_output()).into_owned();
    let (features, smoke) = ran.unwrap_or_else(|e| panic!("exec: {e}; console:\n{console}"));
    let features = String::from_utf8_lossy(&features.stdout).trim_end().to_owned();
    Some(Seen { smoke, features, exit, counts: m.exit_counts(), console })
}

/// Checks the smoke script read its file back, twice, and the run ended as stopped.
fn check_smoke(seen: &Seen) {
    let Seen { smoke, console, .. } = seen;
    assert_eq!(
        String::from_utf8_lossy(&smoke.stdout),
        "/mnt/smoke: OK\n/mnt/smoke: OK\n",
        "{smoke:?}; console:\n{console}"
    );
    assert!(smoke.status.success(), "{smoke:?}; console:\n{console}");
    assert!(matches!(seen.exit, Ok(ExitReason::Stopped)), "{:?}; console:\n{console}", seen.exit);
}

/// Without a DAX window, every read and write a request.
#[test]
fn a_guest_mounts_fxfs_from_the_embedded_daemon() {
    let Some(seen) = mount_and_check(None) else {
        return;
    };
    check_smoke(&seen);
    assert_eq!(&seen.features[VIRTIO_EXTENT_F_DAX as usize..][..1], "0", "{}", seen.features);
    assert_eq!(seen.counts.access_retried, 0, "{:?}", seen.counts);
}

/// With a DAX window (board 13069): the driver takes DAX, and the guest's reads fault the window's
/// pages in, which libmachine maps from the daemon's cache as cladefs's stage says; its writes make
/// them private.
#[test]
fn a_guest_reads_and_writes_through_a_dax_window() {
    let Some(seen) = mount_and_check(Some(64 << 20)) else {
        return;
    };
    check_smoke(&seen);
    assert_eq!(&seen.features[VIRTIO_EXTENT_F_DAX as usize..][..1], "1", "{}", seen.features);
    assert!(seen.counts.access_retried > 0, "no window fault: {:?}", seen.counts);
    eprintln!("{:?}", seen.counts);
}

/// A machine running on a thread of its own, its agent at hand, for the snapshot test.
struct Running {
    agent: libmachine::agent::Agent,
    stop: libmachine::StopHandle,
    runner: Option<std::thread::JoinHandle<(libmachine::Result<ExitReason>, Machine)>>,
}

impl Running {
    fn start(mut m: Machine) -> Self {
        let agent = m.agent().expect("a board has an agent").timeout(Some(Duration::from_secs(60)));
        let stop = m.stop_handle();
        let runner = std::thread::spawn(move || (m.run_for(Duration::from_secs(300)), m));
        agent.wait_ready(Duration::from_secs(60)).expect("the agent");
        Self { agent, stop, runner: Some(runner) }
    }

    /// Runs `script`: its standard output, or what went wrong, with the console.
    fn sh(&self, script: &str) -> Result<String, String> {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        match self.agent.exec(&cmd) {
            Ok(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).into()),
            Ok(out) => Err(format!("{script}: {out:?}")),
            Err(e) => Err(format!("{script}: {e}")),
        }
    }

    fn must(&self, script: &str) -> String {
        self.sh(script).unwrap_or_else(|e| panic!("{e}"))
    }

    fn stop(mut self) -> Machine {
        self.stop.stop();
        let (exit, m) = self.runner.take().unwrap().join().expect("the run thread");
        let console = String::from_utf8_lossy(&m.serial_output()).into_owned();
        assert!(matches!(exit, Ok(ExitReason::Stopped)), "{exit:?}; console:\n{console}");
        m
    }
}

/// The embedded daemon, and a guest on 2 vCPUs, with a balloon, an extent store device on a new
/// cell of the daemon's and a 64 MiB DAX window, which has mounted Fxfs from it at /mnt, written a
/// file there (its sum in /tmp/file.sum) and a copy of busybox, and left a shell of that busybox
/// waiting on /tmp/fifo: its text mapped into its page tables through the window, it runs those
/// pages again once a line comes, writing it to /tmp/heard.
fn boot_with_a_window() -> Option<(Arc<ExtentDaemon>, tempfile::TempDir, Running)> {
    let (kernel, initrd) = guest()?;
    let _ = cladefs::cladefsd::logging::to_stderr();
    let dir = tempfile::tempdir().unwrap();
    let daemon = Arc::new(
        ExtentDaemon::start(ExtentDaemonConfig::local(dir.path()).memory(256 << 20))
            .expect("the embedded daemon"),
    );
    let store = ExtentStore {
        dax_window_size: Some(64 << 20),
        ..ExtentStore::new("extents", ExtentCell::New { tenant: 1 }, 256 << 20)
    };
    let config = MachineConfig {
        mem_size: 512 << 20,
        vcpus: 2,
        extent_daemon: Some(daemon.clone()),
        extent_stores: vec![store],
        balloon: true,
        ..Default::default()
    };
    let boot =
        LinuxBoot { kernel, initrd: Some(initrd), cmdline: "quiet libmachine_action=agent".into() };
    let running = Running::start(Machine::boot_linux(config, &boot).expect("boot"));
    running.must(
        "set -e
         mkdir -p /mnt
         mount -t cladefs extents /mnt
         dd if=/dev/urandom of=/mnt/file bs=64k count=32 conv=fsync 2>/dev/null
         sha256sum /mnt/file > /tmp/file.sum
         cp /bin/busybox /mnt/busybox
         sync
         echo 3 > /proc/sys/vm/drop_caches
         sha256sum -c /tmp/file.sum >/dev/null
         mkfifo /tmp/fifo
         /mnt/busybox sh -c 'read x < /tmp/fifo; echo \"heard $x\" > /tmp/heard' \
             </dev/null >/dev/null 2>&1 &",
    );
    Some((daemon, dir, running))
}

/// Wakes the waiting shell with `line`, and waits for what it heard.
const WAKE: &str = "echo {line} > /tmp/fifo
     while [ ! -s /tmp/heard ]; do sleep 0.1; done
     cat /tmp/heard";

/// doc-4's snapshots on Hypervisor.framework (board 13139): refused unless the guest's Fxfs is
/// frozen, the machine running on after the refusal; taken frozen, a version of the cell made.
/// Each machine restored from it forks the cell from the version: the guest, thawed, reads its
/// file back and runs the window's programs again, and its writes stay in its own fork.
#[test]
fn a_snapshot_forks_the_cell() {
    let Some((daemon, _dir, running)) = boot_with_a_window() else {
        return;
    };
    let snapshots = tempfile::tempdir().unwrap();
    let path = snapshots.path().join("snapshot");

    // Not frozen: refused, and the machine carries on with its device.
    let mut m = running.stop();
    assert!(m.exit_counts().access_retried > 0, "the guest touched no page of its DAX window");
    let refused = m.snapshot(&path).expect_err("an unfrozen guest's snapshot").to_string();
    assert!(refused.contains("not frozen"), "{refused}");
    let running = Running::start(m);
    running.must("echo after-the-refusal > /mnt/later && sync && cat /mnt/later >/dev/null");
    running.must("fsfreeze --freeze /mnt");
    let mut m = running.stop();
    m.snapshot(&path).expect("a frozen guest's snapshot");
    // The machine runs on after it, its device too.
    let running = Running::start(m);
    running.must("fsfreeze --unfreeze /mnt && echo original > /mnt/original && sync");
    drop(running.stop());

    let snapshot = libmachine::Snapshot::open(&path).unwrap();
    let saved = snapshot.config();
    assert_eq!(saved.extent_stores[0].cell, ExtentCell::New { tenant: 1 });
    assert_eq!(saved.extent_stores[0].dax_window_size, Some(64 << 20));
    for clone in 0..2 {
        let config = MachineConfig { extent_daemon: Some(daemon.clone()), ..snapshot.config() };
        let running = Running::start(Machine::restore(&snapshot, config).expect("restore"));
        let seen = running.must(&format!(
            "set -e
             fsfreeze --unfreeze /mnt
             {wake}
             /mnt/busybox echo through-the-window
             sha256sum -c /tmp/file.sum
             cat /mnt/later
             echo {clone} > /mnt/clone-{clone}
             sync
             ls /mnt",
            wake = WAKE.replace("{line}", &clone.to_string()),
        ));
        assert_eq!(
            seen,
            format!(
                "heard {clone}\nthrough-the-window\n/mnt/file: OK\nafter-the-refusal\n\
                 busybox\nclone-{clone}\nfile\nlater\n"
            ),
            "clone {clone}"
        );
        drop(running.stop());
    }
}

/// A snapshot for a live migration (board 13139): the guest synced, not frozen, its window's
/// private pages rebound; no version made, the cell handed over whole, which the machine restored
/// from it opens again. The guest's page tables still map the window as they did, the shell's
/// text among them, which the restored window must map again where it was, as the snapshot
/// recorded it: the guest asks for none of it.
#[test]
fn a_migration_snapshot_maps_the_window_again() {
    let Some((daemon, _dir, running)) = boot_with_a_window() else {
        return;
    };
    running.must(
        r#"set -e
         sync
         count() {
             awk -v k=$1 '$1 == k { n = split($2, a, ","); s = 0; for (i = 1; i <= n; i++) s += a[i]; print s }' \
                 /proc/fs/cladefs/extents
         }
         echo 1 > /sys/module/cladefs/parameters/dax_rebind_now
         asked=$(count dax_rebinds_asked)
         while [ "$(count dax_rebinds_done)" -lt "$asked" ]; do sleep 0.01; done"#,
    );
    let snapshots = tempfile::tempdir().unwrap();
    let path = snapshots.path().join("snapshot");
    let mut m = running.stop();
    let options = libmachine::SnapshotOptions { for_migration: true, ..Default::default() };
    m.snapshot_with(&path, &options).expect("a migration's snapshot");
    // The cell goes with the machine, for the restore to take.
    drop(m);

    let snapshot = libmachine::Snapshot::open(&path).unwrap();
    let config = MachineConfig { extent_daemon: Some(daemon.clone()), ..snapshot.config() };
    let running = Running::start(Machine::restore(&snapshot, config).expect("restore"));
    let seen = running.must(&format!(
        "set -e
         {wake}
         sha256sum -c /tmp/file.sum
         echo moved > /mnt/moved
         sync
         ls /mnt",
        wake = WAKE.replace("{line}", "after-the-move"),
    ));
    assert_eq!(seen, "heard after-the-move\n/mnt/file: OK\nbusybox\nfile\nmoved\n");
    let m = running.stop();
    eprintln!("{:?}", m.exit_counts());
}
