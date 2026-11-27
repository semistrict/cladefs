// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! libmachine-firecracker boots cladefs's arm64 kernel with libmachine's test initramfs from a
//! Firecracker configuration file, as cladefs's guest tests run it: its console on standard
//! output, its disks and its vsock as Firecracker has them, its exit as Firecracker's.
//!
//! The guest is where libmachine's HVF tests find it (crates/libmachine/scripts/arm64-guest.sh);
//! the tests return early with a note when it is missing.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// What init prints once it runs.
const INIT_MARKER: &str = "libmachine-init: up ";

fn guest() -> Option<(PathBuf, PathBuf)> {
    let dir = std::env::var_os("LIBMACHINE_GUEST_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/arm64-guest"),
        PathBuf::from,
    );
    let kernel = std::env::var_os("CLADEFS_GUEST_KERNEL")
        .map_or_else(|| dir.join("Image"), PathBuf::from);
    let initrd = dir.join("initramfs.cpio");
    if !kernel.is_file() || !initrd.is_file() {
        eprintln!("skipping: no arm64 guest at {} and {}", kernel.display(), initrd.display());
        return None;
    }
    Some((kernel, initrd))
}

/// libmachine-firecracker, as the tests run it, booting `config`, written to `dir`: its console
/// to `dir/console.txt`.
fn spawn(dir: &Path, config: &serde_json::Value) -> std::process::Child {
    let path = dir.join("firecracker.json");
    std::fs::write(&path, serde_json::to_vec_pretty(config).unwrap()).unwrap();
    Command::new(env!("CARGO_BIN_EXE_libmachine-firecracker"))
        .args(["--enable-pci", "--no-api", "--config-file"])
        .arg(&path)
        .arg("--log-path")
        .arg(dir.join("firecracker.log"))
        .args(["--level", "Warning"])
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(dir.join("console.txt")).unwrap())
        .spawn()
        .unwrap()
}

/// A guest booted from a configuration file with a disk reads the disk's size, prints it on its
/// console, which is the process's standard output, and powers off: the process exits 0.
#[test]
fn boots_from_a_firecracker_configuration() {
    let Some((kernel, initrd)) = guest() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("disk.img");
    std::fs::File::create(&disk).unwrap().set_len(8 << 20).unwrap();
    let config = serde_json::json!({
        "boot-source": {
            "kernel_image_path": kernel,
            "initrd_path": initrd,
            "boot_args": "console=ttyS0 libmachine_cat=/sys/block/vda/size",
        },
        "machine-config": { "vcpu_count": 2, "mem_size_mib": 256 },
        "drives": [{
            "drive_id": "scratch",
            "path_on_host": disk,
            "is_root_device": false,
            "is_read_only": false,
            "cache_type": "Unsafe",
        }],
        "pmem": [],
        "cladefs": [],
    });
    let status = spawn(dir.path(), &config).wait().unwrap();
    let console = std::fs::read_to_string(dir.path().join("console.txt")).unwrap();
    assert!(status.success(), "{status}; console:\n{console}");
    assert!(console.contains(INIT_MARKER), "console:\n{console}");
    // 8 MiB in 512-byte sectors.
    assert!(
        console.contains("libmachine-init: /sys/block/vda/size:\n16384\n")
            || console.contains("libmachine-init: /sys/block/vda/size:\r\n16384\r\n"),
        "console:\n{console}"
    );
}

/// The vsock answers at its `uds_path` in Firecracker's protocol: `CONNECT <port>` to the guest's
/// agent is answered `OK <port>`.
#[test]
fn its_vsock_speaks_firecrackers_protocol() {
    let Some((kernel, initrd)) = guest() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    // Short, as a Unix socket's path must be.
    let uds = std::env::temp_dir().join(format!("lmfc-{}.vsock", std::process::id()));
    let config = serde_json::json!({
        "boot-source": {
            "kernel_image_path": kernel,
            "initrd_path": initrd,
            "boot_args": "console=ttyS0 libmachine_action=agent",
        },
        "machine-config": { "vcpu_count": 1, "mem_size_mib": 256 },
        "vsock": { "guest_cid": 3, "uds_path": uds },
    });
    let mut child = spawn(dir.path(), &config);
    let deadline = Instant::now() + Duration::from_secs(30);
    let reply = loop {
        assert!(Instant::now() < deadline, "no answer at {}", uds.display());
        let Ok(mut stream) = UnixStream::connect(&uds) else {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        stream.write_all(b"CONNECT 100\n").unwrap();
        let mut line = String::new();
        // Refused (closed) until the agent listens.
        if BufReader::new(stream).read_line(&mut line).is_ok() && !line.is_empty() {
            break line;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    child.kill().unwrap();
    child.wait().unwrap();
    let _ = std::fs::remove_file(&uds);
    let console = std::fs::read_to_string(dir.path().join("console.txt")).unwrap();
    assert!(reply.starts_with("OK "), "{reply:?}; console:\n{console}");
}
