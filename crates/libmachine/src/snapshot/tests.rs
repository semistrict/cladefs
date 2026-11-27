// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The snapshot file without a machine: what is written, and every way a
//! file is refused.

use std::os::unix::fs::MetadataExt;

use super::*;
use crate::arch::x86_64::layout::memory_layout;

/// A directory of the test's own under target/, removed when dropped.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/snapshot-unit-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn header(mem_size: u64) -> MachineHeader {
    MachineHeader {
        cpu_model: CpuModel::Baseline,
        cpu_fingerprint: "f00d".into(),
        vcpus: 2,
        mem_size,
        layout: memory_layout(mem_size),
        balloon: false,
        disks: Vec::new(),
        extent_stores: Vec::new(),
        caller_data: Vec::new(),
    }
}

/// RAM with a few pages written, the rest zero.
fn ram(mem_size: u64) -> GuestMemoryMmap {
    let ranges: Vec<(GuestAddress, usize)> = crate::arch::x86_64::layout::ram_regions(mem_size)
        .into_iter()
        .map(|(start, size)| (GuestAddress(start), size as usize))
        .collect();
    let mem = GuestMemoryMmap::from_ranges(&ranges).unwrap();
    use vm_memory::Bytes;
    mem.write_slice(b"first page", GuestAddress(0)).unwrap();
    mem.write_slice(b"in the middle", GuestAddress(mem_size / 2)).unwrap();
    mem.write_slice(b"last", GuestAddress(mem_size - 4)).unwrap();
    mem
}

#[test]
fn a_snapshot_reads_back_what_was_written() {
    let dir = Dir::new("reads-back");
    let path = dir.0.join("s");
    let mem_size = 8 << 20;
    write(&path, &header(mem_size), b"the rest", &ram(mem_size), RamAt::Inline).unwrap();
    let s = Snapshot::open(&path).unwrap();
    assert_eq!(s.machine, header(mem_size));
    assert_eq!(s.config().vcpus, 2);
    assert_eq!(s.config().cpu_model, CpuModel::Baseline);
    assert!(s.config().network.is_none());
    assert_eq!(s.cpu_fingerprint(), "f00d");
    let mut rest = s.state();
    assert_eq!(rest.remaining(), 8);
    let mut b = [0; 8];
    for x in &mut b {
        *x = rest.u8().unwrap();
    }
    assert_eq!(&b, b"the rest");
    // RAM at its offset, 2 MiB aligned; zero pages are holes.
    let offset = s.ram_offsets()[0];
    assert_eq!(offset % RAM_ALIGN, 0);
    let file = std::fs::File::open(&path).unwrap();
    let mut page = [0; 13];
    file.read_exact_at(&mut page, offset + mem_size / 2).unwrap();
    assert_eq!(&page, b"in the middle");
    file.read_exact_at(&mut page[..4], offset + mem_size - 4).unwrap();
    assert_eq!(&page[..4], b"last");
    let meta = file.metadata().unwrap();
    assert_eq!(meta.len(), offset + mem_size);
    assert!(
        meta.blocks() * 512 < 1 << 20,
        "8 MiB of RAM with three pages written takes {} bytes",
        meta.blocks() * 512
    );
    assert_eq!(meta.mode() & 0o777, 0o400, "read-only: clones map it");
}

/// RAM above 4 GiB is a region of its own, at its own aligned offset.
#[test]
fn every_ram_region_has_its_place() {
    let layout = memory_layout(5 << 30);
    let offsets = ram_offsets(&layout, 4 << 20);
    assert_eq!(
        offsets.iter().map(|(r, o)| (r.start, r.size, *o)).collect::<Vec<_>>(),
        [(0, 3 << 30, 4 << 20), (4 << 30, 2 << 30, (3 << 30) + (4 << 20))]
    );
}

/// Writing over an existing snapshot replaces the file (a new inode):
/// machines restored from the old one keep mapping theirs.
#[test]
fn a_new_snapshot_replaces_the_old_file() {
    let dir = Dir::new("replaces");
    let path = dir.0.join("s");
    let mem_size = 4 << 20;
    write(&path, &header(mem_size), b"", &ram(mem_size), RamAt::Inline).unwrap();
    let old = std::fs::metadata(&path).unwrap().ino();
    let held = std::fs::File::open(&path).unwrap();
    write(&path, &header(mem_size), b"again", &ram(mem_size), RamAt::Inline).unwrap();
    assert_ne!(std::fs::metadata(&path).unwrap().ino(), old);
    assert_eq!(held.metadata().unwrap().ino(), old);
    assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 1, "no temporary file left");
}

fn refused(path: &Path) -> String {
    match Snapshot::open(path) {
        Err(Error::Snapshot(m)) => m,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn other_versions_and_damaged_files_are_refused() {
    let dir = Dir::new("refused");
    let path = dir.0.join("s");
    let mem_size = 4 << 20;
    write(&path, &header(mem_size), b"state", &ram(mem_size), RamAt::Inline).unwrap();
    let good = std::fs::read(&path).unwrap();
    let put = |bytes: &[u8]| {
        let p = dir.0.join("bad");
        let _ = std::fs::remove_file(&p);
        std::fs::write(&p, bytes).unwrap();
        p
    };

    // Another format version, older or newer: never read.
    for version in [0u32, FORMAT_VERSION + 1] {
        let mut bad = good.clone();
        bad[8..12].copy_from_slice(&version.to_le_bytes());
        let m = refused(&put(&bad));
        assert!(m.contains(&format!("format version {version}")), "{m}");
        assert!(m.contains("never converted"), "{m}");
    }
    // Not a snapshot at all; too short for a header.
    assert!(refused(&put(&b"#!/bin/sh\necho no snapshot\n".repeat(8))).contains("does not start"));
    assert!(refused(&put(&good[..40])).contains("shorter"));
    // A state whose checksum fails.
    let mut bad = good.clone();
    bad[HEADER_LEN + 3] ^= 1;
    assert!(refused(&put(&bad)).contains("checksum"));
    // A state length past the file.
    let mut bad = good.clone();
    bad[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(refused(&put(&bad)).contains("damaged"));
    // Cut short in its RAM.
    assert!(refused(&put(&good[..good.len() - 4096])).contains("before its RAM's end"));
    // A missing file is the OS's error.
    assert!(matches!(Snapshot::open(dir.0.join("missing")), Err(Error::Io { .. })));
}

#[test]
fn a_restore_for_another_machine_is_refused() {
    let dir = Dir::new("mismatch");
    let path = dir.0.join("s");
    let mem_size = 4 << 20;
    write(&path, &header(mem_size), b"", &ram(mem_size), RamAt::Inline).unwrap();
    let s = Snapshot::open(&path).unwrap();
    let config = s.config();
    let layout = |c: &MachineConfig| memory_layout(c.mem_size as u64);
    s.check(&config, &layout(&config)).unwrap();

    let other_vcpus = MachineConfig { vcpus: 1, ..config.clone() };
    let m = match s.check(&other_vcpus, &layout(&other_vcpus)) {
        Err(Error::Snapshot(m)) => m,
        other => panic!("{other:?}"),
    };
    assert!(m.contains("2 vCPUs") && m.contains("would have 1"), "{m}");

    let other_ram = MachineConfig { mem_size: 8 << 20, ..config.clone() };
    assert!(matches!(
        s.check(&other_ram, &layout(&other_ram)),
        Err(Error::Snapshot(m)) if m.contains("memory map")
    ));
    // The same RAM with another address map (as another libmachine might
    // lay it out) is refused too: the whole map is compared.
    let mut moved = layout(&config);
    moved.regions[1].start += 4096;
    assert!(matches!(
        s.check(&config, &moved),
        Err(Error::Snapshot(m)) if m.contains("memory map")
    ));
    let networked = MachineConfig {
        network: Some(crate::net::NetworkConfig::new(crate::net::EgressPolicy::deny_all())),
        ..config.clone()
    };
    assert!(matches!(
        s.check(&networked, &layout(&networked)),
        Err(Error::Snapshot(m)) if m.contains("network")
    ));
    assert!(matches!(
        s.check_cpu(CpuModel::Host, "beef"),
        Err(Error::Snapshot(m)) if m.contains("f00d") && m.contains("beef")
    ));
    s.check_cpu(CpuModel::Baseline, "f00d").unwrap();
}
