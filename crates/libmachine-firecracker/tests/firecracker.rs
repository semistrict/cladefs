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
    let kernel =
        std::env::var_os("CLADEFS_GUEST_KERNEL").map_or_else(|| dir.join("Image"), PathBuf::from);
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

/// libmachine-firecracker started as the tests start a VMM a snapshot is loaded into: its API
/// at `api`, no configuration, its console to `dir/<name>.console.txt`.
fn spawn_api(dir: &Path, name: &str, api: &Path, config: Option<&serde_json::Value>) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_libmachine-firecracker"));
    command.args(["--enable-pci", "--api-sock"]).arg(api);
    if let Some(config) = config {
        let path = dir.join(format!("{name}.json"));
        std::fs::write(&path, serde_json::to_vec_pretty(config).unwrap()).unwrap();
        command.arg("--config-file").arg(path);
    }
    let child = command
        .arg("--log-path")
        .arg(dir.join(format!("{name}.log")))
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(dir.join(format!("{name}.console.txt"))).unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while UnixStream::connect(api).is_err() {
        assert!(Instant::now() < deadline, "no API at {}", api.display());
        std::thread::sleep(Duration::from_millis(10));
    }
    Child(child)
}

/// A VMM process, killed when dropped, as the tests kill a snapshot's source.
struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Sends `method path` with `body` to the API at `api`, as the tests' client does (a connection
/// a request): the status code and the body.
fn request(api: &Path, method: &str, path: &str, body: &serde_json::Value) -> (u16, String) {
    let body = body.to_string();
    let mut stream = UnixStream::connect(api).unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader.read_line(&mut status).unwrap();
    let code = status.split_whitespace().nth(1).unwrap().parse().unwrap();
    let mut len = 0;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            len = value.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; len];
    std::io::Read::read_exact(&mut reader, &mut body).unwrap();
    (code, String::from_utf8(body).unwrap())
}

/// Waits until the guest's agent answers `CONNECT 100` at the vsock `uds`, as Firecracker's
/// protocol has it, `within` at most.
fn agent_answers(uds: &Path, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Ok(mut stream) = UnixStream::connect(uds) {
            stream.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            let mut line = String::new();
            if stream.write_all(b"CONNECT 100\n").is_ok()
                && BufReader::new(stream).read_line(&mut line).is_ok()
                && line.starts_with("OK ")
            {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Firecracker's snapshot API as the guest tests drive it: a snapshot refused while the microVM
/// runs, taken paused (its memory file the guest's RAM, flat, as Firecracker's), the source
/// killed; then loaded in fresh processes started with `--api-sock` alone, resumed at once or
/// paused until `Resumed`, its vsock at an override's path or the snapshot's, its balloon
/// patched; and what this VMM doesn't do refused.
#[test]
fn snapshots_load_in_new_processes() {
    let Some((kernel, initrd)) = guest() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    // Short, as Unix sockets' paths must be.
    let short =
        |name: &str| std::env::temp_dir().join(format!("lmfc-{}-{name}", std::process::id()));
    let (api, uds) = (short("a.api"), short("a.vsock"));
    let config = serde_json::json!({
        "boot-source": {
            "kernel_image_path": kernel,
            "initrd_path": initrd,
            "boot_args": "console=ttyS0 quiet libmachine_action=agent",
        },
        "machine-config": { "vcpu_count": 2, "mem_size_mib": 256 },
        "vsock": { "guest_cid": 3, "uds_path": uds },
        "balloon": { "amount_mib": 0, "deflate_on_oom": true, "free_page_reporting": true },
    });
    let source = spawn_api(dir.path(), "source", &api, Some(&config));
    assert!(agent_answers(&uds, Duration::from_secs(30)), "the source's agent");
    let (snapshot, memory) = (dir.path().join("snapshot"), dir.path().join("memory"));
    let create = serde_json::json!({
        "snapshot_type": "Full", "snapshot_path": snapshot, "mem_file_path": memory,
    });
    let (code, body) = request(&api, "PUT", "/snapshot/create", &create);
    assert_eq!(code, 400, "a running microVM's snapshot: {body}");
    assert!(body.contains("paused"), "{body}");
    assert_eq!(request(&api, "PATCH", "/vm", &serde_json::json!({ "state": "Paused" })).0, 204);
    let diff = serde_json::json!({
        "snapshot_type": "Diff", "snapshot_path": snapshot, "mem_file_path": memory,
    });
    assert_eq!(request(&api, "PUT", "/snapshot/create", &diff).0, 400);
    let (code, body) = request(&api, "PUT", "/snapshot/create", &create);
    assert_eq!(code, 204, "{body}");
    assert_eq!(std::fs::metadata(&memory).unwrap().len(), 256 << 20);
    drop(source);
    let _ = std::fs::remove_file(&api);

    // Resumed at once, its vsock at an override's path, its balloon patched.
    let (api, uds_b) = (short("b.api"), short("b.vsock"));
    let loaded = spawn_api(dir.path(), "b", &api, None);
    let (code, _) = request(&api, "PATCH", "/vm", &serde_json::json!({ "state": "Resumed" }));
    assert_eq!(code, 400, "no microVM to resume yet");
    let load = serde_json::json!({
        "snapshot_path": snapshot,
        "mem_backend": { "backend_type": "File", "backend_path": memory },
        "resume_vm": true,
        "vsock_override": { "uds_path": uds_b },
        "cladefs_overrides": [],
    });
    let (code, body) = request(&api, "PUT", "/snapshot/load", &load);
    assert_eq!(code, 204, "{body}");
    assert!(agent_answers(&uds_b, Duration::from_secs(30)), "the loaded microVM's agent");
    let (code, body) = request(&api, "PATCH", "/balloon", &serde_json::json!({ "amount_mib": 16 }));
    assert_eq!(code, 204, "{body}");
    assert_eq!(request(&api, "PUT", "/snapshot/load", &load).0, 400, "a second load");
    drop(loaded);
    let _ = std::fs::remove_file(&api);

    // Paused until resumed, its vsock where the snapshot had it.
    let api = short("c.api");
    let loaded = spawn_api(dir.path(), "c", &api, None);
    let cell = serde_json::json!({
        "snapshot_path": snapshot,
        "mem_backend": { "backend_type": "Cell", "backend_path": "0/0" },
    });
    let (code, body) = request(&api, "PUT", "/snapshot/load", &cell);
    assert_eq!(code, 400, "{body}");
    assert!(body.contains("memory cells"), "{body}");
    let load = serde_json::json!({
        "snapshot_path": snapshot,
        "mem_backend": { "backend_type": "File", "backend_path": memory },
        "resume_vm": false,
    });
    let (code, body) = request(&api, "PUT", "/snapshot/load", &load);
    assert_eq!(code, 204, "{body}");
    assert!(!agent_answers(&uds, Duration::from_secs(2)), "a paused microVM's agent answered");
    assert_eq!(request(&api, "PATCH", "/vm", &serde_json::json!({ "state": "Resumed" })).0, 204);
    assert!(agent_answers(&uds, Duration::from_secs(30)), "the resumed microVM's agent");
    drop(loaded);
    for path in [api, uds, uds_b] {
        let _ = std::fs::remove_file(path);
    }
}
