// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's extent store device on libmachine (board 13067): cladefs's guest kernel
//! (`CLADEFS_GUEST_KERNEL`) with the test initramfs and its agent (`LIBMACHINE_GUEST_DIR`) mounts
//! Fxfs from the device, its cell served by cladefs's extent daemon embedded in the test's process
//! (libcladefs, board 13553), with a DAX window, and its writes, fsyncs and reads hold across a
//! remount.
//!
//! Tests return early with a note when /dev/kvm, the kernel or the initramfs is missing;
//! scripts/test.sh's libmachine part provides them all, and fails on such a note.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use common::{cladefs_kernel, kvm_available, test_guest_dir};
use libmachine::agent::{Agent, Command};
use libmachine::{
    ExitReason, ExtentCell, ExtentDaemon, ExtentDaemonConfig, ExtentStore, LinuxBoot, Machine,
    MachineConfig,
};
use virtio_extent::protocol::VIRTIO_EXTENT_F_DAX;

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

/// cladefs's extent daemon, embedded in the test's process, keeping its disk and bucket in `dir`.
fn daemon(dir: &std::path::Path) -> ExtentDaemon {
    // Its log on standard error, which a failed test shows.
    let _ = cladefs::cladefsd::logging::to_stderr();
    ExtentDaemon::start(ExtentDaemonConfig::local(dir).memory(256 << 20)).expect("the daemon")
}

/// Runs `script` in the guest with the agent's shell: its standard output, the script's exit
/// checked.
fn sh(agent: &Agent, script: &str) -> String {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", script]);
    let out = agent.exec(&cmd).unwrap_or_else(|e| panic!("{script}: {e}"));
    assert!(out.status.success(), "{script}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

/// The features the extent driver takes, as Linux's sysfs prints a virtio device's: a character
/// a bit, from bit 0, of its 128, every one the device offers, as a driver of its protocol takes
/// all (`check_acked`).
fn acked(store: &ExtentStore) -> String {
    let config = virtio_extent::Config {
        id: store.id.clone(),
        tag: store.tag.clone(),
        path_on_host: String::new(),
        capacity: store.capacity,
        dax_window_size: store.dax_window_size,
        dax_private_max_bytes: store.dax_private_max_bytes,
        cladefsd_socket: String::new(),
        cache_type: virtio_extent::CacheType::Writeback,
    };
    // VIRTIO_RING_F_EVENT_IDX and VIRTIO_F_VERSION_1, the transport's.
    let features = config.features() | 1 << 29 | 1 << 32;
    (0..128u32).map(|bit| if bit < 64 && features & 1 << bit != 0 { '1' } else { '0' }).collect()
}

/// A guest on 2 vCPUs mounts Fxfs from an extent store device with a DAX window, its driver
/// taking every feature the device offers, DAX among them; a file it writes and fsyncs reads back
/// the same after a remount, from the cell the daemon committed, and after its pages are dropped,
/// through the window.
#[test]
fn a_guest_mounts_fxfs_from_the_device_with_dax() {
    let Some((kernel, initrd)) = guest() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let daemon = Arc::new(daemon(dir.path()));
    let store = ExtentStore {
        dax_window_size: Some(64 << 20),
        ..ExtentStore::new("extents", ExtentCell::New { tenant: 1 }, 256 << 20)
    };
    let expected_features = acked(&store);
    assert_eq!(&expected_features[VIRTIO_EXTENT_F_DAX as usize..][..1], "1");
    let config = MachineConfig {
        mem_size: 512 << 20,
        vcpus: 2,
        extent_daemon: Some(daemon),
        extent_stores: vec![store],
        ..Default::default()
    };
    let boot =
        LinuxBoot { kernel, initrd: Some(initrd), cmdline: "quiet libmachine_action=agent".into() };
    let mut m = Machine::boot_linux(config, &boot).expect("boot");
    let agent = m.agent().expect("a PC machine has an agent");
    let stop = m.stop_handle();
    let runner = std::thread::spawn(move || {
        let exit = m.run_for(Duration::from_secs(300));
        (exit, m)
    });
    let finish = |runner: std::thread::JoinHandle<_>| {
        stop.stop();
        let (exit, m): (libmachine::Result<ExitReason>, Machine) = runner.join().unwrap();
        (exit, String::from_utf8_lossy(&m.serial_output()).into_owned())
    };
    if let Err(e) = agent.wait_ready(Duration::from_secs(60)) {
        let (exit, console) = finish(runner);
        panic!("agent not ready: {e}; exit {exit:?}, console:\n{console}");
    }

    let features = sh(&agent, "cat /sys/bus/virtio/drivers/virtio_extent/virtio*/features");
    let smoke = sh(
        &agent,
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
    );
    let (exit, console) = finish(runner);
    assert_eq!(features.trim_end(), expected_features, "console:\n{console}");
    assert_eq!(smoke, "/mnt/smoke: OK\n/mnt/smoke: OK\n", "console:\n{console}");
    assert_eq!(exit.expect("run"), ExitReason::Stopped, "console:\n{console}");
}
