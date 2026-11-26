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
