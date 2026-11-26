// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Boots the test guest (`crates/libmachine/scripts/build-test-guest.sh`): an upstream kernel
//! as both a bzImage and a PVH vmlinux, and an initramfs whose init prints a
//! marker and powers off (or reboots, with `libmachine_action=reboot`).
//!
//! Tests return early with a note when /dev/kvm or the guest is missing.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

mod common;

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::kvm_available;
use libmachine::{Error, ExitReason, LinuxBoot, Machine, MachineConfig};

const INIT_MARKER: &str = "libmachine-init: up ";

fn guest_dir() -> PathBuf {
    std::env::var_os("LIBMACHINE_GUEST_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/guest"),
        PathBuf::from,
    )
}

/// The guest's directory, if /dev/kvm and a built guest are both there.
fn guest() -> Option<PathBuf> {
    if !kvm_available() {
        return None;
    }
    let dir = guest_dir();
    if ["bzImage", "vmlinux", "initramfs.cpio"].iter().all(|f| dir.join(f).is_file()) {
        Some(dir)
    } else {
        eprintln!(
            "skipping: no test guest in {} (run crates/libmachine/scripts/build-test-guest.sh)",
            dir.display()
        );
        None
    }
}

fn config() -> MachineConfig {
    MachineConfig { mem_size: 256 << 20, ..Default::default() }
}

fn boot(dir: &std::path::Path, kernel: &str, cmdline: &str) -> LinuxBoot {
    LinuxBoot {
        kernel: dir.join(kernel),
        initrd: Some(dir.join("initramfs.cpio")),
        cmdline: cmdline.into(),
    }
}

/// Boots, runs to the end, and returns the exit, the console and how long
/// creating and running the machine took.
fn run(boot: &LinuxBoot) -> (ExitReason, String, Duration) {
    run_with(config(), boot)
}

/// [`run`] with `config`.
fn run_with(config: MachineConfig, boot: &LinuxBoot) -> (ExitReason, String, Duration) {
    let start = Instant::now();
    let mut m = Machine::boot_linux(config, boot).expect("boot");
    let exit = m.run().expect("run");
    let elapsed = start.elapsed();
    let console = String::from_utf8_lossy(&m.serial_output()).into_owned();
    (exit, console, elapsed)
}

#[test]
fn bzimage_boots_to_init_and_powers_off() {
    let Some(dir) = guest() else { return };
    let (exit, console, _) = run(&boot(&dir, "bzImage", ""));
    assert_eq!(exit, ExitReason::Poweroff, "console:\n{console}");
    assert!(console.contains(INIT_MARKER), "console:\n{console}");
    assert!(console.contains("reboot: Power down"));
}

#[test]
fn pvh_vmlinux_boots_to_init_and_powers_off() {
    let Some(dir) = guest() else { return };
    let (exit, console, _) = run(&boot(&dir, "vmlinux", ""));
    assert_eq!(exit, ExitReason::Poweroff, "console:\n{console}");
    assert!(console.contains(INIT_MARKER), "console:\n{console}");
}

#[test]
fn reboot_ends_the_run_with_a_reset() {
    let Some(dir) = guest() else { return };
    for kernel in ["bzImage", "vmlinux"] {
        let (exit, console, _) = run(&boot(&dir, kernel, "libmachine_action=reboot"));
        assert_eq!(exit, ExitReason::Reset, "{kernel} console:\n{console}");
        assert!(console.contains(INIT_MARKER), "{kernel}");
    }
}

/// The platform the kernel sees is clean: ACPI loads, COM1 comes from the
/// DSDT, and the CPU model is self-consistent.
#[test]
fn kernel_sees_a_consistent_platform() {
    let Some(dir) = guest() else { return };
    let (_, console, _) = run(&boot(&dir, "vmlinux", ""));
    for bad in [
        "ACPI Error",
        "Unable to load the System Description Tables",
        "dependency check failure",
        "Power off not available",
        "PCI: Fatal",
        "not reserved",
    ] {
        assert!(!console.contains(bad), "found {bad:?} in console:\n{console}");
    }
    for good in [
        "ACPI: Using ACPI (MADT) for SMP configuration information",
        "00:00: ttyS0 at I/O 0x3f8",
        "ACPI: PM: Preparing to enter system sleep state S5",
    ] {
        assert!(console.contains(good), "missing {good:?} in console:\n{console}");
    }
}

/// Every PC machine has a virtio-rng on its PCI bus: the guest enumerates
/// the bus from ACPI, binds the device over modern virtio-PCI, and reads
/// entropy through a queue whose interrupts arrive by MSI-X.
#[test]
fn guest_reads_entropy_from_the_virtio_rng() {
    let Some(dir) = guest() else { return };
    for kernel in ["vmlinux", "bzImage"] {
        let (exit, console, _) = run(&boot(&dir, kernel, "libmachine_action=devices"));
        assert_eq!(exit, ExitReason::Poweroff, "{kernel} console:\n{console}");
        let reports = |kind: &str| -> Vec<String> {
            let prefix = format!("libmachine-init: {kind} ");
            console
                .lines()
                .filter_map(|l| l.strip_prefix(prefix.as_str()).map(str::to_owned))
                .collect()
        };
        assert!(console.contains("PCI host bridge to bus 0000:00"), "console:\n{console}");
        let pci = reports("pci");
        assert!(
            pci.iter().any(|l| l.starts_with("0000:00:01.0 vendor=0x1af4 device=0x1044 ")
                && l.ends_with("driver=virtio-pci")),
            "{pci:?}"
        );
        let virtio = reports("virtio");
        assert!(
            virtio.iter().any(|l| l.contains("device=0x0004") && l.contains("driver=virtio_rng")),
            "{virtio:?}"
        );
        let hwrng = reports("hwrng");
        assert_eq!(hwrng.len(), 1, "console:\n{console}");
        assert_eq!(hwrng[0].len(), 64, "32 bytes in hex: {}", hwrng[0]);
        let irqs = reports("irq");
        assert!(
            irqs.iter().any(|l| l.contains("PCI-MSIX-0000:00:01.0") && l.ends_with("-input")),
            "the queue's own MSI-X vector: {irqs:?}"
        );
    }
}

/// Reports how long a boot takes, with one vCPU and with four (bringing up
/// the APs and stopping them at power-off is part of it); `cargo test --
/// --nocapture` shows it.
#[test]
fn boot_time() {
    let Some(dir) = guest() else { return };
    for (kernel, vcpus) in [("vmlinux", 1), ("bzImage", 1), ("vmlinux", 4)] {
        let config = MachineConfig { vcpus, ..config() };
        // Warm the page cache so the first boot does not measure disk reads.
        run_with(config.clone(), &boot(&dir, kernel, "quiet"));
        let (exit, console, elapsed) = run_with(config, &boot(&dir, kernel, "quiet"));
        assert_eq!(exit, ExitReason::Poweroff);
        let uptime = console.lines().find_map(|l| l.strip_prefix(INIT_MARKER)).unwrap_or("?");
        eprintln!(
            "boot time, {kernel} x{vcpus} vCPU (quiet): {:.1} ms from creating the machine to \
             power-off; kernel uptime at init {uptime}",
            elapsed.as_secs_f64() * 1e3
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "{kernel} took {elapsed:?} to boot and power off"
        );
    }
}

/// The most vCPU exits a quiet boot to power-off may take, by kernel and
/// vCPU count. An exit-heavy regression (describing too many PCI buses,
/// polling a device, chatty serial output) fails here, with the counts by
/// kind, before it shows up as slower boots.
///
/// Measured (board 13123, 6 boots each): 780-819 exits with the virtio-rng
/// alone. Raised when board 13089's vsock joined every PC machine: each virtio-PCI
/// function costs a boot about 160 configuration space and 120 register
/// exits as Linux probes and sets it up. Measured after (3 boots each):
/// 1,168-1,207 with one or four vCPUs: serial ~200, unclaimed port I/O ~250,
/// configuration space 482, virtio-PCI registers 240. The budget is the
/// highest count plus about a fifth, which kernel or timing changes stay
/// under, and which a doubling of any large kind (or board 13111's 8,160 extra
/// configuration space exits) breaks. Raise it only for a change that adds
/// exits on purpose, saying why.
const QUIET_BOOT_EXITS: &[(&str, u32, u64)] =
    &[("vmlinux", 1, 1_450), ("bzImage", 1, 1_450), ("vmlinux", 4, 1_450)];

#[test]
fn a_quiet_boot_stays_within_its_exit_budget() {
    let Some(dir) = guest() else { return };
    for &(kernel, vcpus, budget) in QUIET_BOOT_EXITS {
        let config = MachineConfig { vcpus, ..config() };
        let mut m = Machine::boot_linux(config, &boot(&dir, kernel, "quiet")).expect("boot");
        assert_eq!(m.run().expect("run"), ExitReason::Poweroff);
        let counts = m.exit_counts();
        eprintln!("quiet boot, {kernel} x{vcpus} vCPU: {counts}");
        assert!(
            counts.total() <= budget,
            "{kernel} x{vcpus}: {counts}, over the budget of {budget}"
        );
    }
}

#[test]
fn console_streams_while_the_guest_runs() {
    let Some(dir) = guest() else { return };

    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);
    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let streamed = Shared::default();
    let mut m = Machine::boot_linux(config(), &boot(&dir, "vmlinux", "quiet")).unwrap();
    m.set_console(streamed.clone());
    assert_eq!(m.run().unwrap(), ExitReason::Poweroff);
    assert_eq!(*streamed.0.lock().unwrap(), m.serial_output());
}

#[test]
fn elf_without_pvh_entry_is_refused() {
    let Some(dir) = guest() else { return };
    // The static init binary is an ELF with no PVH note.
    let boot = LinuxBoot { kernel: dir.join("init"), initrd: None, cmdline: String::new() };
    match Machine::boot_linux(config(), &boot) {
        Err(Error::Boot(msg)) => assert!(msg.contains("PVH"), "{msg}"),
        other => panic!("expected a boot error, got {other:?}"),
    }
}

#[test]
fn unknown_kernel_format_is_refused() {
    let Some(dir) = guest() else { return };
    let boot =
        LinuxBoot { kernel: dir.join("initramfs.cpio"), initrd: None, cmdline: String::new() };
    match Machine::boot_linux(config(), &boot) {
        Err(Error::Boot(msg)) => assert!(msg.contains("not an ELF vmlinux or a bzImage"), "{msg}"),
        other => panic!("expected a boot error, got {other:?}"),
    }
}

#[test]
fn initrd_that_does_not_fit_is_refused() {
    let Some(dir) = guest() else { return };
    let big = std::env::temp_dir().join(format!("libmachine-big-initrd-{}", std::process::id()));
    std::fs::File::create(&big).unwrap().set_len(64 << 20).unwrap();
    let boot = LinuxBoot {
        kernel: dir.join("vmlinux"),
        initrd: Some(big.clone()),
        cmdline: String::new(),
    };
    let small = MachineConfig { mem_size: 48 << 20, ..Default::default() };
    let result = Machine::boot_linux(small, &boot);
    std::fs::remove_file(&big).unwrap();
    match result {
        Err(Error::Boot(msg)) => assert!(msg.contains("does not fit"), "{msg}"),
        other => panic!("expected a boot error, got {other:?}"),
    }
}

#[test]
fn missing_files_name_the_path() {
    if !kvm_available() {
        return;
    }
    let boot =
        LinuxBoot { kernel: "/nonexistent/vmlinux".into(), initrd: None, cmdline: String::new() };
    match Machine::boot_linux(config(), &boot) {
        Err(e @ Error::Io { .. }) => assert!(e.to_string().contains("/nonexistent/vmlinux")),
        other => panic!("expected an I/O error, got {other:?}"),
    }
}

#[test]
fn too_much_ram_is_refused() {
    let boot = LinuxBoot { kernel: "unused".into(), initrd: None, cmdline: String::new() };
    // 1 TiB - 1 GiB is the most: RAM from 4 GiB would reach the 64-bit
    // MMIO window at 1 TiB.
    let config = MachineConfig { mem_size: (1 << 40) - (1 << 30) + 4096, ..Default::default() };
    match Machine::boot_linux(config, &boot) {
        Err(Error::InvalidConfig(msg)) => assert!(msg.contains("at most"), "{msg}"),
        other => panic!("expected a configuration error, got {other:?}"),
    }
}

/// The RAM the guest's init found in /proc/meminfo, in bytes.
fn guest_mem_total(console: &str) -> u64 {
    let kb: u64 = console
        .lines()
        .find_map(|l| l.strip_prefix("libmachine-init: MemTotal "))
        .and_then(|rest| rest.strip_suffix(" kB"))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("no MemTotal line in console:\n{console}"));
    kb * 1024
}

/// Past 3 GiB, RAM continues at 4 GiB: the kernel finds it in the e820
/// table (bzImage) or the PVH memory map, and uses it.
#[test]
fn linux_uses_ram_past_the_mmio_hole() {
    let Some(dir) = guest() else { return };
    for (gib, high_ram) in [
        (4u64, "[mem 0x0000000100000000-0x000000013fffffff] usable"),
        (6, "[mem 0x0000000100000000-0x00000001bfffffff] usable"),
    ] {
        for kernel in ["bzImage", "vmlinux"] {
            let mem_size = gib << 30;
            let config = MachineConfig { mem_size: mem_size as usize, ..Default::default() };
            let mut m = Machine::boot_linux(config, &boot(&dir, kernel, "")).expect("boot");
            let exit = m.run_for(Duration::from_secs(30)).expect("run");
            let console = String::from_utf8_lossy(&m.serial_output()).into_owned();
            assert_eq!(exit, ExitReason::Poweroff, "{kernel}, {gib} GiB:\n{console}");
            for line in ["[mem 0x0000000000100000-0x00000000bfffffff] usable", high_ram] {
                assert!(
                    console.contains(line),
                    "{kernel}, {gib} GiB: no {line:?} in console:\n{console}"
                );
            }
            // MemTotal is RAM less what the kernel keeps (its image, page
            // structs): well over the 3 GiB below the hole.
            let total = guest_mem_total(&console);
            assert!(
                total > mem_size / 10 * 9 && total <= mem_size,
                "{kernel}, {gib} GiB: MemTotal is {total} bytes"
            );
        }
    }
}

/// Below the hole nothing changes: one RAM region up to the RAM's end.
#[test]
fn linux_sees_all_ram_below_the_hole() {
    let Some(dir) = guest() else { return };
    let (exit, console, _) = run(&boot(&dir, "bzImage", ""));
    assert_eq!(exit, ExitReason::Poweroff, "console:\n{console}");
    assert!(
        console.contains("[mem 0x0000000000100000-0x000000000fffffff] usable"),
        "console:\n{console}"
    );
    assert!(!console.contains("[mem 0x0000000100000000"), "console:\n{console}");
    let total = guest_mem_total(&console);
    assert!(total > 200 << 20 && total <= 256 << 20, "MemTotal {total}");
}
