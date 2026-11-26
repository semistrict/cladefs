// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's extent store device on Hypervisor.framework (board 13553): cladefs's arm64 kernel
//! with the test initramfs and its agent mounts Fxfs from the device, its cell served by cladefs's
//! extent daemon embedded in the test's process (libcladefs), connected in memory, and its writes,
//! fsyncs and reads hold across a remount. No DAX window yet (board 13069).
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

/// A guest on 2 vCPUs mounts Fxfs from an extent store device on a new cell of the embedded
/// daemon's; a file it writes and fsyncs reads back the same after a remount, from the cell the
/// daemon committed, and after its pages are dropped.
#[test]
fn a_guest_mounts_fxfs_from_the_embedded_daemon() {
    let Some((kernel, initrd)) = guest() else {
        return;
    };
    // Its log on standard error, which a failed test shows.
    let _ = cladefs::cladefsd::logging::to_stderr();
    let dir = tempfile::tempdir().unwrap();
    let daemon = ExtentDaemon::start(ExtentDaemonConfig::local(dir.path()).memory(256 << 20))
        .expect("the embedded daemon");
    let store = ExtentStore::new("extents", ExtentCell::New { tenant: 1 }, 256 << 20);
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
        (exit, String::from_utf8_lossy(&m.serial_output()).into_owned())
    });
    let ready = agent.wait_ready(Duration::from_secs(30));
    let smoke = ready.and_then(|()| {
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            "set -e
             mkdir -p /mnt
             mount -t cladefs extents /mnt
             dd if=/dev/urandom of=/mnt/smoke bs=64k count=16 conv=fsync 2>/dev/null
             sha256sum /mnt/smoke > /tmp/smoke.sum
             umount /mnt
             mount -t cladefs extents /mnt
             sha256sum -c /tmp/smoke.sum
             echo 3 > /proc/sys/vm/drop_caches
             sha256sum -c /tmp/smoke.sum
             umount /mnt",
        ]);
        agent.exec(&cmd)
    });
    stop.stop();
    let (exit, console) = runner.join().expect("the run thread");
    let smoke = smoke.unwrap_or_else(|e| panic!("exec: {e}; console:\n{console}"));
    assert_eq!(
        String::from_utf8_lossy(&smoke.stdout),
        "/mnt/smoke: OK\n/mnt/smoke: OK\n",
        "{smoke:?}; console:\n{console}"
    );
    assert!(smoke.status.success(), "{smoke:?}; console:\n{console}");
    assert_eq!(exit.expect("run"), ExitReason::Stopped, "console:\n{console}");
}
