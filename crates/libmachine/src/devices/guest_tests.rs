// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Boots the test guest (`crates/libmachine/scripts/build-test-guest.sh`) with the test virtio
//! device beside the virtio-rng and checks, from inside the guest, what
//! Linux made of the PCI bus: init's "devices" action reports it.
//!
//! In the crate rather than in `tests/` because the test device and the
//! transport's counters are internal. Skips with a note when /dev/kvm or
//! the guest is missing.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use vm_memory::MmapRegion;

use crate::devices::virtio::pci::CAP_SHARED_MEMORY_CFG;
use crate::devices::virtio::test_device::{TEST_CONFIG, TestDevice};
use crate::{ExitReason, LinuxBoot, Machine, MachineConfig};

const PREFIX: &str = "libmachine-init: ";
const HOST_BYTES: &[u8; 16] = b"host-wrote-this!";
const GUEST_BYTES: &[u8; 16] = b"guest-wrote-this";
/// Where the guest finds the devices: virtio-rng at 1, virtio-vsock at 2,
/// the test device at 3.
const RNG: &str = "0000:00:01.0";
const VSOCK: &str = "0000:00:02.0";
const TEST: &str = "0000:00:03.0";

/// The test guest's directory, if KVM and the guest are there (else a note).
pub(crate) fn guest() -> Option<PathBuf> {
    let kvm = std::fs::OpenOptions::new().read(true).write(true).open("/dev/kvm");
    if kvm.is_err() {
        eprintln!("skipping: /dev/kvm is not accessible");
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect()
}

/// What init reported of `kind`, the prefixes stripped.
fn reports<'a>(console: &'a str, kind: &str) -> Vec<&'a str> {
    let prefix = format!("{PREFIX}{kind} ");
    console.lines().filter_map(|l| l.strip_prefix(prefix.as_str())).collect()
}

fn report<'a>(console: &'a str, kind: &str, starting: &str) -> &'a str {
    reports(console, kind)
        .into_iter()
        .find(|l| l.starts_with(starting))
        .unwrap_or_else(|| panic!("no {kind} report for {starting:?} in console:\n{console}"))
}

/// The capabilities in a configuration space dump, as (ID, bytes from the
/// capability's start).
fn capabilities(config: &[u8]) -> Vec<(u8, &[u8])> {
    let mut caps = Vec::new();
    let mut off = usize::from(config[0x34]);
    while off != 0 && caps.len() < 48 {
        caps.push((config[off], &config[off..]));
        off = usize::from(config[off + 1]);
    }
    caps
}

#[test]
fn linux_finds_the_devices_and_uses_them_without_vcpu_exits() {
    let Some(dir) = guest() else { return };
    let shm = Arc::new(MmapRegion::new(2 << 20).unwrap());
    // SAFETY: the region is 2 MiB of this process's memory, and nothing
    // else uses it yet.
    unsafe { std::ptr::copy_nonoverlapping(HOST_BYTES.as_ptr(), shm.as_ptr(), HOST_BYTES.len()) };

    let cmdline = format!(
        "libmachine_action=devices \
         libmachine_peek={TEST}/resource2:0:16,{TEST}/resource0:8192:16 \
         libmachine_poke={TEST}/resource2:4096:{}",
        hex(GUEST_BYTES)
    );
    let boot = LinuxBoot {
        kernel: dir.join("vmlinux"),
        initrd: Some(dir.join("initramfs.cpio")),
        cmdline,
    };
    let config = MachineConfig { mem_size: 256 << 20, ..Default::default() };
    let device = Box::new(TestDevice::new(shm.clone()));
    let mut m = Machine::boot_linux_with(config, &boot, vec![device]).unwrap();
    let exit = m.run_for(Duration::from_secs(60)).unwrap();
    let console = String::from_utf8_lossy(&m.serial_output()).into_owned();
    assert_eq!(exit, ExitReason::Poweroff, "console:\n{console}");
    assert!(console.contains("libmachine-init: devices done"), "console:\n{console}");

    // The shared memory BAR is device memory in the one slot table: recorded
    // in the layout snapshots use, and no other mapping may overlap it.
    let layout = m.memory_layout();
    let bar = *layout
        .of_kind(crate::RegionKind::DeviceMemory)
        .find(|r| r.size == shm.size() as u64)
        .unwrap_or_else(|| panic!("no shared memory BAR in {layout:?}"));
    assert!(
        layout
            .of_kind(crate::RegionKind::Mmio64)
            .any(|w| w.start <= bar.start && bar.end() <= w.end()),
        "BAR {bar:?} outside the 64-bit window"
    );
    let over = crate::HostMemory::new(4096).unwrap();
    assert!(
        m.memory_slots().map(bar.start, over).is_err(),
        "mapping over the BAR at {:#x} was allowed",
        bar.start
    );

    // The host bridge, from ACPI, with its bus range and windows.
    for good in [
        "ACPI: PCI Root Bridge [PCI0] (domain 0000 [bus 00])",
        "PCI host bridge to bus 0000:00",
        "root bus resource [mem 0xc0000000-0xdfffffff window]",
        "root bus resource [mem 0x10000000000-0x1ffffffffff window]",
    ] {
        assert!(console.contains(good), "missing {good:?} in console:\n{console}");
    }
    for bad in [
        "PCI: Fatal",
        "not reserved",
        "ACPI Error",
        "can't claim",
        "can't assign",
        "no compatible bridge window",
        "failed",
    ] {
        assert!(!console.contains(bad), "found {bad:?} in console:\n{console}");
    }

    // The functions, bound to virtio-pci. The test device's type is past
    // 63: the last transitional ID, its type the subsystem ID.
    let rng = report(&console, "pci", RNG);
    for part in ["vendor=0x1af4", "device=0x1044", "subsystem_device=0x0004", "driver=virtio-pci"] {
        assert!(rng.contains(part), "{part} in {rng}");
    }
    let vsock = report(&console, "pci", VSOCK);
    for part in ["vendor=0x1af4", "device=0x1053", "subsystem_device=0x0013", "driver=virtio-pci"] {
        assert!(vsock.contains(part), "{part} in {vsock}");
    }
    let test = report(&console, "pci", TEST);
    for part in [
        "vendor=0x1af4",
        "device=0x103f",
        "subsystem_vendor=0x1af4",
        "subsystem_device=0x4658",
        "driver=virtio-pci",
    ] {
        assert!(test.contains(part), "{part} in {test}");
    }
    // Virtio took the types from them.
    let virtio = reports(&console, "virtio");
    assert!(
        virtio.iter().any(|l| l.contains("device=0x0004") && l.contains("driver=virtio_rng")),
        "{virtio:?}"
    );
    assert!(virtio.iter().any(|l| l.contains("device=0x4658")), "{virtio:?}");
    assert!(
        virtio.iter().any(|l| l.contains("device=0x0013")
            && l.contains("driver=vmw_vsock_virtio_transport")),
        "{virtio:?}"
    );

    // BAR 2, 64-bit and prefetchable, where libmachine put it.
    let bar2 = report(&console, "pci-resource", &format!("{TEST} 2 "));
    assert!(bar2.contains("0x0000010000000000 0x00000100001fffff"), "{bar2}");

    // The configuration space as Linux read it through ECAM holds the
    // shared memory capability.
    let config = unhex(report(&console, "pci-config", TEST).split_whitespace().nth(1).unwrap());
    assert_eq!(config.len(), 256);
    let shm_cap = capabilities(&config)
        .into_iter()
        .find(|(id, c)| *id == 0x09 && c[3] == CAP_SHARED_MEMORY_CFG)
        .expect("a shared memory capability")
        .1;
    assert_eq!((shm_cap[2], shm_cap[4], shm_cap[5]), (24, 2, 0), "cap64, BAR 2, region 0");
    assert_eq!(&shm_cap[12..16], &(2u32 << 20).to_le_bytes(), "length");

    // The guest reads the host's bytes in the shared memory, and the device
    // configuration through BAR 0; the host sees what the guest wrote.
    let shm_peek = report(&console, "peek", &format!("{TEST}/resource2 0 "));
    assert!(shm_peek.ends_with(&hex(HOST_BYTES)), "{shm_peek}");
    let cfg_peek = report(&console, "peek", &format!("{TEST}/resource0 8192 "));
    assert!(cfg_peek.ends_with(&hex(TEST_CONFIG)), "{cfg_peek}");
    assert!(console.contains(&format!("poke {TEST}/resource2 4096 ok")), "console:\n{console}");
    let mut written = [0u8; 16];
    // SAFETY: the guest has stopped; the region is 2 MiB.
    unsafe { std::ptr::copy_nonoverlapping(shm.as_ptr().add(4096), written.as_mut_ptr(), 16) };
    assert_eq!(&written, GUEST_BYTES);

    // The virtio-rng gave the guest entropy...
    let hwrng = reports(&console, "hwrng");
    assert!(hwrng.len() == 1 && hwrng[0].len() == 64, "{hwrng:?}");
    assert_ne!(hwrng[0], "0".repeat(64));
    // ...through a queue with an MSI-X vector of its own, apart from the
    // configuration vector, and interrupts delivered.
    let irqs: Vec<&str> = reports(&console, "irq")
        .into_iter()
        .filter(|l| l.contains(&format!("PCI-MSIX-{RNG}")))
        .collect();
    assert_eq!(irqs.len(), 2, "{irqs:?}");
    assert!(irqs.iter().any(|l| l.ends_with("-config")), "{irqs:?}");
    let queue = irqs.iter().find(|l| !l.ends_with("-config")).unwrap();
    let count: u64 = queue.split_whitespace().nth(1).unwrap().parse().unwrap();
    assert!(count > 0, "{queue}");

    // Linux enumerates the bus with a bounded number of configuration space
    // exits. Describing more buses than the platform has, for one, had x86
    // Linux probe each for peer root bridges: 8160 exits, ~60 ms of boot.
    let config_accesses = m.exit_counts().pci_config;
    assert!(config_accesses < 2000, "{config_accesses} config space accesses");
    eprintln!("configuration space accesses during the boot: {config_accesses}");

    // Every notification reached the device through its ioeventfd: none
    // exited to the vCPU thread.
    let stats = m.pci().unwrap().stats(1);
    assert!(stats.notifies > 0, "{stats:?}");
    assert!(stats.interrupts > 0, "{stats:?}");
    assert_eq!(stats.notify_exits, 0, "{stats:?}");
}
