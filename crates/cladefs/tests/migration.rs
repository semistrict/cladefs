// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Live migration of an ephemeral cell between two extent daemons sharing a bucket (backlog doc-25,
//! TASK-698.4): forked from a version on the first, written there while the second takes its kept
//! segments in rounds, handed over once its guest is gone, read whole on the second; its
//! reference then the second's alone. A hand-over while its store is open is refused, and one
//! asked again after it was accepted answers so again.

#![cfg(target_os = "linux")]

use cladefs::cladefsd::control::Client;
use cladefs::{CellClient, ExtentId, VolatileSlice, VolatileSliceMut};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const CAPACITY: u64 = 1 << 31;
const EXTENT: usize = 1 << 20;
/// Extents written before the migration begins: past eight segments of 32 MiB, so that some are
/// kept, which the rounds send while the guest runs.
const BEFORE: u64 = 300;
/// Extents written while the migration's rounds run.
const DURING: u64 = 40;

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A daemon of a host: its process, its VM socket, a client of its control API, and its
/// ephemeral cells' directory.
struct Daemon {
    _process: KillOnDrop,
    vms: PathBuf,
    control: Client,
    ephemeral: PathBuf,
}

/// A daemon serving in `dir`, keeping its stores' cells in the file bucket at `bucket`, its
/// ephemeral cells' directory in `dir`, serving its peers at `peers`' first address.
fn daemon(dir: &Path, bucket: &Path, peers: &[String]) -> Daemon {
    let (vms, control, ephemeral) =
        (dir.join("cladefsd.sock"), dir.join("control.sock"), dir.join("ephemeral"));
    let process = Command::new(env!("CARGO_BIN_EXE_cladefsd"))
        .arg(&vms)
        .env("CLADEFSD_BUCKET", format!("file://{}/", bucket.display()))
        .env("CLADEFSD_TOKEN", "1")
        .env("CLADEFSD_PART_CACHE", dir.join("cache"))
        .env("CLADEFSD_GRPC", &control)
        .env("CLADEFSD_EPHEMERAL", &ephemeral)
        .env("CLADEFSD_PEER_ADDRESS", &peers[0])
        .env("CLADEFSD_PEERS", peers.join(","))
        .spawn()
        .unwrap();
    let process = KillOnDrop(process);
    let deadline = Instant::now() + Duration::from_secs(30);
    while std::os::unix::net::UnixStream::connect(&vms).is_err() {
        assert!(Instant::now() < deadline, "the daemon never served");
        std::thread::sleep(Duration::from_millis(20));
    }
    let control = Client::connect(&control).unwrap();
    Daemon { _process: process, vms, control, ephemeral }
}

/// Two loopback addresses no one listens at yet.
fn addresses() -> [String; 2] {
    let listeners = [(); 2].map(|()| std::net::TcpListener::bind("127.0.0.1:0").unwrap());
    listeners.map(|listener| listener.local_addr().unwrap().to_string())
}

/// Bytes of extent `sequence` that don't compress.
fn data(sequence: u64) -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64 ^ sequence;
    (0..EXTENT)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

fn write(store: &CellClient, extent: ExtentId) {
    store.write(extent, 0, &[VolatileSlice::from(&data(extent.sequence)[..])]).unwrap();
    store.commit(None).unwrap();
}

fn read(store: &CellClient, extent: ExtentId) -> Vec<u8> {
    let mut read = vec![0; EXTENT];
    store.read(extent, 0, &mut [VolatileSliceMut::from(&mut read[..])]).unwrap();
    read
}

/// The incarnations the ephemeral cells' references in the bucket at `bucket` name.
fn referring(bucket: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut dirs = vec![bucket.to_owned()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if path.is_dir() {
                dirs.push(path);
            } else if let Some(rest) = name.strip_prefix("ephemeral.") {
                found.push(rest.split('.').next().unwrap().to_owned());
            }
        }
    }
    found.sort();
    found
}

#[test]
fn ephemeral_cells_migrate_live_between_hosts() {
    let dir = tempfile::tempdir().unwrap();
    let bucket = dir.path().join("bucket");
    let hosts = [dir.path().join("source"), dir.path().join("target")];
    for path in [&bucket, &hosts[0], &hosts[1]] {
        std::fs::create_dir(path).unwrap();
    }
    let [source_at, target_at] = addresses();
    let source = daemon(&hosts[0], &bucket, &[source_at.clone(), target_at.clone()]);
    let target = daemon(&hosts[1], &bucket, &[target_at.clone(), source_at]);

    // A version on the source, its parent's extent inherited by the ephemeral cell forked from it.
    let base = hosts[0].join("base");
    let store = CellClient::connect(&source.vms, &base, CAPACITY).unwrap();
    let inherited = ExtentId { cell: store.cell(), sequence: 1 };
    write(&store, inherited);
    drop(store);
    let version = source.control.make_version(&base, None).unwrap();
    source.control.sync_cell(&base).unwrap();
    let stated = source.control.stat_cell(&base).unwrap();
    let from = cladefs::CellVersion { tenant: stated.tenant, cell: stated.cell, version };
    let here = hosts[0].join("ephemeral-cell");
    let child = source.control.fork_ephemeral(from, &here).unwrap();

    let store = CellClient::connect(&source.vms, &here, CAPACITY).unwrap();
    let own = |sequence| ExtentId { cell: child.id, sequence };
    for sequence in 1..=BEFORE {
        write(&store, own(sequence));
    }
    let there = hosts[1].join("ephemeral-cell");
    source.control.migrate_cell(&here, &target_at, &there).unwrap();
    for sequence in BEFORE + 1..=BEFORE + DURING {
        write(&store, own(sequence));
    }
    std::thread::sleep(Duration::from_secs(2));
    // Open still, the store is not handed over.
    let busy = source.control.hand_over_cell(&here).unwrap_err();
    assert_eq!(busy.errno(), Some(cladefs::Errno::EBUSY));
    drop(store);
    source.control.hand_over_cell(&here).unwrap();
    assert!(!here.exists(), "the source's store goes once handed over");
    let incarnation = std::fs::read_to_string(target.ephemeral.join("incarnation")).unwrap();
    let incarnation: u64 = incarnation.trim().parse().unwrap();
    let named = referring(&bucket);
    assert_eq!(named.len(), 1, "one reference: {named:?}");
    let noted =
        |daemon: &Daemon| std::fs::read_dir(daemon.ephemeral.join("cells")).unwrap().count();
    assert_eq!((noted(&source), noted(&target)), (0, 1), "the cell is the target's alone");

    let store = CellClient::connect(&target.vms, &there, CAPACITY).unwrap();
    assert_eq!(store.cell(), child.id);
    assert!(read(&store, inherited) == data(1), "the parent's extent read otherwise");
    for sequence in 1..=BEFORE + DURING {
        assert!(read(&store, own(sequence)) == data(sequence), "extent {sequence} read otherwise");
    }
    let next = own(BEFORE + DURING + 1);
    write(&store, next);
    drop(store);
    let store = CellClient::connect(&target.vms, &there, CAPACITY).unwrap();
    assert!(read(&store, next) == data(next.sequence));
    drop(store);
    println!(
        "{} MiB migrated, the target's incarnation {incarnation:x}, its reference {named:?}",
        BEFORE + DURING
    );
}

/// A hand-over the target never answers leaves the cell given up on the source, its store opened
/// there no more; the target restarted gives up the migration it was taking, and its reference;
/// asked again, it refuses, and the source serves the cell again, holding all it held, the only
/// one referring to its origin.
#[test]
fn hand_overs_refused_leave_the_source_serving() {
    let dir = tempfile::tempdir().unwrap();
    let bucket = dir.path().join("bucket");
    let hosts = [dir.path().join("source"), dir.path().join("target")];
    for path in [&bucket, &hosts[0], &hosts[1]] {
        std::fs::create_dir(path).unwrap();
    }
    let [source_at, target_at] = addresses();
    let source = daemon(&hosts[0], &bucket, &[source_at.clone(), target_at.clone()]);
    let target_peers = [target_at.clone(), source_at];
    let target = daemon(&hosts[1], &bucket, &target_peers);

    let base = hosts[0].join("base");
    let store = CellClient::connect(&source.vms, &base, CAPACITY).unwrap();
    let inherited = ExtentId { cell: store.cell(), sequence: 1 };
    write(&store, inherited);
    drop(store);
    let version = source.control.make_version(&base, None).unwrap();
    source.control.sync_cell(&base).unwrap();
    let stated = source.control.stat_cell(&base).unwrap();
    let from = cladefs::CellVersion { tenant: stated.tenant, cell: stated.cell, version };
    let here = hosts[0].join("ephemeral-cell");
    let child = source.control.fork_ephemeral(from, &here).unwrap();
    let own = |sequence| ExtentId { cell: child.id, sequence };
    let store = CellClient::connect(&source.vms, &here, CAPACITY).unwrap();
    for sequence in 1..=4 {
        write(&store, own(sequence));
    }
    drop(store);
    let there = hosts[1].join("ephemeral-cell");
    source.control.migrate_cell(&here, &target_at, &there).unwrap();
    assert_eq!(referring(&bucket).len(), 2, "the source's and the target's");

    drop(target);
    let unanswered = source.control.hand_over_cell(&here).unwrap_err();
    assert_eq!(unanswered.errno(), Some(cladefs::Errno::ENOTCONN));
    assert!(CellClient::connect(&source.vms, &here, CAPACITY).is_err(), "given up, it opens not");

    let target = daemon(&hosts[1], &bucket, &target_peers);
    assert_eq!(referring(&bucket).len(), 1, "the target's given up as it started again");
    let refused = source.control.hand_over_cell(&here).unwrap_err();
    assert_eq!(refused.errno(), Some(cladefs::Errno::EAGAIN));
    assert!(!there.exists());
    let noted =
        |daemon: &Daemon| std::fs::read_dir(daemon.ephemeral.join("cells")).unwrap().count();
    assert_eq!((noted(&source), noted(&target)), (1, 0), "the cell is the source's alone");
    let store = CellClient::connect(&source.vms, &here, CAPACITY).unwrap();
    assert!(read(&store, inherited) == data(1));
    for sequence in 1..=4 {
        assert!(read(&store, own(sequence)) == data(sequence), "extent {sequence} read otherwise");
    }
}
