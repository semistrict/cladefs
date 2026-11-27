// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Runs cladefs's tests that need Linux and KVM on an Apple Silicon Mac: boots a Linux machine
//! with nested virtualization (KVM inside it), its root filesystem a cell of cladefs's daemon
//! embedded in this process, imported from the test environment's image
//! (crates/libmachine/scripts/linux-test-env.sh), and runs `scripts/test.sh` there, its output
//! here as it comes.
//!
//!     cargo run -p libmachine --example linux_test_env -- [PARTS...]
//!
//! PARTS are `scripts/test.sh`'s (`CLADEFS_TEST_PARTS`), `firecracker guest` by default. The
//! machine takes LIBMACHINE_VCPUS vCPUs (the host's, by default) and LIBMACHINE_MEM_GIB GiB (24).
//! Exits with the tests' status.

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    hvf::main()
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn main() {
    eprintln!(
        "linux_test_env runs on Apple Silicon Macs: a Linux host runs scripts/test.sh itself"
    );
    std::process::exit(2);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod hvf {
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use libmachine::agent::{Command, Event};
    use libmachine::{
        ExtentCell, ExtentDaemon, ExtentDaemonConfig, ExtentStore, LinuxBoot, Machine,
        MachineConfig,
    };

    /// The tenant the environment's cells are of.
    const TENANT: u64 = 1;
    /// The root cell's capacity: room for the workspaces' builds.
    const CAPACITY: u64 = 512 << 30;
    /// The root device's tag, which the kernel mounts as root.
    const ROOT_TAG: &str = "root";

    /// What the machine runs, as root, before the tests: the filesystems a Linux host has that
    /// libmachine's init mounts none of, and the environment the tests run in.
    const PREPARE: &str = r#"set -e
mount -t proc proc /proc 2>/dev/null || true
mount -t tmpfs tmpfs /tmp
mount -t tmpfs tmpfs /run
mkdir -p /dev/pts /dev/shm
mount -t devpts devpts /dev/pts
mount -t tmpfs tmpfs /dev/shm
mount -t cgroup2 cgroup2 /sys/fs/cgroup
hostname cladefs-linux-test-env
export HOME=/root USER=root PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export CARGO_HOME=/usr/local/cargo RUSTUP_HOME=/usr/local/rustup CARGO_NET_OFFLINE=true
export CLADEFS_GUEST_KERNEL=/opt/cladefs-guest/Image
cd /work
"#;

    pub fn main() -> Result<(), Box<dyn std::error::Error>> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let env_dir = std::env::var_os("LIBMACHINE_LINUX_TEST_ENV")
            .map_or_else(|| root.join("target/linux-test-env"), PathBuf::from);
        let image = env_dir.join("rootfs.tar");
        let guest = std::env::var_os("LIBMACHINE_GUEST_DIR")
            .map_or_else(|| root.join("target/arm64-guest"), PathBuf::from);
        let kernel = guest.join("Image");
        for file in [&image, &kernel] {
            if !file.is_file() {
                return Err(format!(
                    "no {} (crates/libmachine/scripts/linux-test-env.sh builds the image)",
                    file.display()
                )
                .into());
            }
        }
        let parts: Vec<String> = std::env::args().skip(1).collect();
        let parts = if parts.is_empty() { "firecracker guest".to_owned() } else { parts.join(" ") };
        let vcpus = match std::env::var("LIBMACHINE_VCPUS") {
            Ok(n) => n.parse()?,
            Err(_) => std::thread::available_parallelism()?.get() as u32,
        };
        let mem_gib: usize = match std::env::var("LIBMACHINE_MEM_GIB") {
            Ok(n) => n.parse()?,
            Err(_) => 24,
        };

        let _ = cladefs::cladefsd::logging::to_stderr();
        // The daemon's disk and bucket, made anew each run, as the image is imported anew.
        let daemon_dir = env_dir.join("daemon");
        if daemon_dir.exists() {
            std::fs::remove_dir_all(&daemon_dir)?;
        }
        std::fs::create_dir_all(&daemon_dir)?;
        let daemon = ExtentDaemon::start(
            ExtentDaemonConfig::local(&daemon_dir).memory(8 << 30).disk_headroom(8 << 30),
        )?;
        let started = Instant::now();
        let cell = daemon.import_image(TENANT, CAPACITY, &image, None)?;
        eprintln!(
            "linux_test_env: imported {} as cell {cell} in {:?}",
            image.display(),
            started.elapsed()
        );

        let config = MachineConfig {
            mem_size: mem_gib << 30,
            vcpus,
            nested_virtualization: true,
            extent_daemon: Some(Arc::new(daemon)),
            extent_stores: vec![ExtentStore::new(ROOT_TAG, ExtentCell::Existing(cell), CAPACITY)],
            ..Default::default()
        };
        let boot = LinuxBoot {
            kernel,
            initrd: None,
            cmdline: format!(
                "root={ROOT_TAG} rootfstype=cladefs rw init=/sbin/libmachine-init \
                 libmachine_action=agent"
            ),
        };
        let mut m = Machine::boot_linux(config, &boot)?;
        let agent = m.agent().expect("a board has the agent's vsock").timeout(None);
        let stop = m.stop_handle();
        let runner = std::thread::spawn(move || {
            let exit = m.run();
            (exit, String::from_utf8_lossy(&m.serial_output()).into_owned())
        });
        let ready = agent.wait_ready(Duration::from_secs(120));
        if let Err(err) = ready {
            stop.stop();
            let (exit, console) = runner.join().expect("the run thread");
            return Err(format!("the agent: {err}; exit {exit:?}; console:\n{console}").into());
        }
        eprintln!("linux_test_env: {vcpus} vCPUs, {mem_gib} GiB, running scripts/test.sh {parts}");
        let script =
            format!("{PREPARE}CLADEFS_TEST_PARTS='{parts}' scripts/test.sh --no-fail-fast 2>&1");
        let process = agent.spawn(Command::new("bash").args(["-c", &script]))?;
        let mut status = None;
        while let Some(event) = process.next_event()? {
            match event {
                Event::Stdout(bytes) => std::io::stdout().write_all(&bytes)?,
                Event::Stderr(bytes) => std::io::stderr().write_all(&bytes)?,
                Event::Exit(exit) => status = Some(exit),
            }
        }
        stop.stop();
        let (exit, _) = runner.join().expect("the run thread");
        eprintln!("linux_test_env: tests {status:?}, machine {exit:?}");
        let code = status.and_then(|status| status.code).unwrap_or(1);
        std::process::exit(code);
    }
}
