// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-537.3.6 (doc-21, doc-12's Replicated rows): replicated cells end to end, on extent daemons
//! of their own processes sharing a file bucket, as hosts do, each a member with a replica
//! directory (`CLADEFSD_REPLICAS`) and the others its peers. The bucket out of reach is its
//! directory made read-only: its writes fail, and are tried again, as a bucket's that does not
//! answer.
//!
//! - The writer's daemon killed and its directory, disk and replica directory, gone, every commit
//!   its guest was told durable (`COMMIT_UPLOADED`, which a replicated cell's waits for Replicated)
//!   while the bucket was out of reach is there after a take on another host.
//! - A member's daemon killed, its writer replaces it, the head naming a configuration of the
//!   survivor and the spare, and its commits are replicated again with the bucket out of reach;
//!   both its members killed, it is degraded, its commits released by the bucket, and it drops to
//!   f = 0.

#![cfg(target_os = "linux")]

use cladefs::cladefsd::control::Client;
use cladefs::cladefsd::control::proto::{CellKind, CreateCellRequest, FsyncLevel};
use cladefs::{CellClient, CellId, ExtentId, VolatileSlice, VolatileSliceMut};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};

const CAPACITY: u64 = 1 << 30;
const EXTENT: usize = 64 << 10;

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A daemon of a host sharing a bucket: its process, its directory, its VM socket and a client of
/// its control API.
struct Daemon {
    process: Option<KillOnDrop>,
    dir: PathBuf,
    vms: PathBuf,
    control_socket: PathBuf,
    control: Client,
}

impl Daemon {
    /// Kills its process at once, as a host's crash does.
    fn kill(&mut self) {
        drop(self.process.take());
    }
}

/// A daemon serving in `dir`, keeping its stores' cells in the file bucket at `bucket`, its replica
/// directory in `dir`, serving its peers at `peers`' first address, and asking them all.
fn daemon(dir: &Path, bucket: &Path, peers: &[String]) -> Daemon {
    std::fs::create_dir_all(dir).unwrap();
    let (vms, control) = (dir.join("cladefsd.sock"), dir.join("control.sock"));
    let mut command = Command::new(env!("CARGO_BIN_EXE_cladefsd"));
    command
        .arg(&vms)
        .env("CLADEFSD_BUCKET", format!("file://{}/", bucket.display()))
        .env("CLADEFSD_TOKEN", "1")
        .env("CLADEFSD_PART_CACHE", dir.join("cache"))
        .env("CLADEFSD_STATS", dir.join("stats"))
        .env("CLADEFSD_GRPC", &control)
        .env("CLADEFSD_REPLICAS", dir.join("replicas"))
        .env("CLADEFSD_PEER_ADDRESS", &peers[0])
        .env("CLADEFSD_PEERS", peers.join(","));
    let process = KillOnDrop(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(30);
    while std::os::unix::net::UnixStream::connect(&vms).is_err() {
        assert!(Instant::now() < deadline, "the daemon never served");
        std::thread::sleep(Duration::from_millis(20));
    }
    Daemon {
        process: Some(process),
        dir: dir.to_owned(),
        vms,
        control: Client::connect(&control).unwrap(),
        control_socket: control,
    }
}

/// `count` loopback addresses no one listens at yet, for daemons' peers.
fn addresses(count: usize) -> Vec<String> {
    let listeners: Vec<_> =
        (0..count).map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap()).collect();
    listeners.iter().map(|listener| listener.local_addr().unwrap().to_string()).collect()
}

/// A daemon for each of `count` hosts in `root`, each the others' peer, sharing the bucket at
/// `bucket`: the hosts' addresses, and their daemons.
fn hosts(root: &Path, bucket: &Path, count: usize) -> (Vec<String>, Vec<Daemon>) {
    let addresses = addresses(count);
    let daemons = (0..count)
        .map(|me| {
            let mut peers = vec![addresses[me].clone()];
            peers.extend((0..count).filter(|&other| other != me).map(|o| addresses[o].clone()));
            daemon(&root.join(format!("host{me}")), bucket, &peers)
        })
        .collect();
    (addresses, daemons)
}

/// Makes a store at `path` on `daemon`, a new cell replicated with f = 1, its guest's fsync and
/// sync waiting for Replicated.
fn create_replicated(daemon: &Daemon, path: &Path) -> CellId {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let made = runtime.block_on(async {
        let mut client = cladefs::cladefsd::control::connect(&daemon.control_socket).await.unwrap();
        let request = CreateCellRequest {
            path: path.to_str().unwrap().to_owned(),
            capacity: CAPACITY,
            kind: CellKind::Fxfs.into(),
            replicas: Some(1),
            fsync: FsyncLevel::Replicated.into(),
            ..CreateCellRequest::default()
        };
        client.create_cell(request).await.unwrap().into_inner()
    });
    CellId { tenant: made.tenant, id: made.cell }
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

/// Writes extents `sequences` of `store`'s, each committed durable: replicated, or uploaded.
fn write(store: &CellClient, sequences: std::ops::RangeInclusive<u64>) {
    for sequence in sequences {
        let extent = ExtentId { cell: store.cell(), sequence };
        store.write(extent, 0, &[VolatileSlice::from(&data(sequence)[..])]).unwrap();
        store.commit(Some(format!("root {sequence}").as_bytes())).unwrap();
    }
}

/// Reads extents `sequences` of `store`'s, each as [`write`] wrote it.
fn check(store: &CellClient, sequences: std::ops::RangeInclusive<u64>) {
    for sequence in sequences {
        let mut read = vec![0; EXTENT];
        let extent = ExtentId { cell: store.cell(), sequence };
        store.read(extent, 0, &mut [VolatileSliceMut::from(&mut read[..])]).unwrap();
        assert!(read == data(sequence), "extent {sequence} read otherwise");
    }
}

/// Makes the bucket at `bucket` out of reach of its writers, or in reach again: its directories
/// and files read-only, or not.
fn reachable(bucket: &Path, writable: bool) {
    let mode = if writable { "u+w" } else { "a-w" };
    let status = Command::new("chmod").args(["-R", mode]).arg(bucket).status().unwrap();
    assert!(status.success());
}

/// The configuration the head of `cell` names, in the bucket at `bucket`: its number, f, and its
/// members' addresses, the writer's first.
fn configuration(bucket: &Path, cell: CellId) -> (u64, u8, Vec<String>) {
    let url = format!("file://{}/", bucket.display());
    let bucket = Arc::new(bucket_cell::object_bucket::ObjectBucket::from_url(&url).unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let head = runtime.block_on(bucket_cell::head::get(&*bucket, cell)).unwrap().unwrap();
    let configuration = head.replication.unwrap().configuration.unwrap();
    let members = configuration.members.iter().map(|member| member.address.clone()).collect();
    (configuration.number, configuration.f, members)
}

/// A VM's connection to the store at `path` on `daemon`: once the daemon, which made or took it
/// through its control API, has let it go (EBUSY until then).
fn connect(daemon: &Daemon, path: &Path) -> CellClient {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match CellClient::connect(&daemon.vms, path, CAPACITY) {
            Ok(client) => return client,
            Err(err) if err.errno() == Some(cladefs::Errno::EBUSY) => {
                assert!(Instant::now() < deadline, "{}: {err}", path.display());
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => panic!("{}: {err}", path.display()),
        }
    }
}

fn until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(Instant::now() < deadline, "waited for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// doc-12's Replicated rows, TASK-537.3.6's AC 1: a replicated cell's commits, told durable while
/// the bucket was out of reach, on its writer and one member; the writer's daemon killed and its
/// directory gone, its disk and its replica directory with it; another host takes the cell,
/// fencing the members, and holds every one of them.
#[test]
fn replicated_commits_survive_their_writer_and_its_disk() {
    let root = tempfile::tempdir().unwrap();
    let bucket = root.path().join("bucket");
    std::fs::create_dir(&bucket).unwrap();
    let (addresses, mut daemons) = hosts(root.path(), &bucket, 3);
    let store = daemons[0].dir.join("store");
    let cell = create_replicated(&daemons[0], &store);
    let (number, f, members) = configuration(&bucket, cell);
    assert_eq!((number, f, members.len()), (1, 1, 3));

    let client = connect(&daemons[0], &store);
    write(&client, 1..=2);
    // The bucket out of reach: what is told durable from now on is on the members alone.
    reachable(&bucket, false);
    write(&client, 3..=10);
    drop(client);

    // The writer's host lost, its disk with it.
    daemons[0].kill();
    std::fs::remove_dir_all(&daemons[0].dir).unwrap();
    reachable(&bucket, true);

    let taken = daemons[1].dir.join("taken");
    daemons[1].control.create_cell(&taken, CAPACITY, cell, Some(2)).unwrap();
    let client = connect(&daemons[1], &taken);
    check(&client, 1..=10);
    // One host of three lost, no configuration of f = 1 is to be had: the taker serves degraded,
    // its commits told durable once the bucket holds them, and, the bucket holding all it
    // recovered, drops to f = 0.
    write(&client, 11..=12);
    let stat = daemons[1].control.stat_cell(&taken).unwrap();
    assert!(stat.degraded && stat.active_replicas == 0, "{stat:?}");
    until("dropped to f = 0", || configuration(&bucket, cell).1 == 0);
    let (number, _, alone) = configuration(&bucket, cell);
    assert_eq!(alone, [addresses[1].clone()]);
    // As StatCell says, the head naming it (Codex's review).
    until("the configuration said", || {
        daemons[1].control.stat_cell(&taken).unwrap().configuration == number
    });

    // The lost host back, a disk of its own new: the cell is replicated again, of all three.
    let mut peers = vec![addresses[0].clone()];
    peers.extend(addresses[1..].iter().cloned());
    daemons[0] = daemon(&daemons[0].dir.clone(), &bucket, &peers);
    write(&client, 13..=14);
    until("replicated again", || configuration(&bucket, cell).1 == 1);
    let (_, _, members) = configuration(&bucket, cell);
    assert_eq!(members[0], addresses[1]);
    assert_eq!(members.len(), 3);
    until("degraded no more", || !daemons[1].control.stat_cell(&taken).unwrap().degraded);
    reachable(&bucket, false);
    write(&client, 15..=16);
    reachable(&bucket, true);
    check(&client, 1..=16);
}

/// TASK-537.3.6's AC 2: a member's daemon killed, the writer replaces it with the spare host, and
/// its commits are replicated again, told durable with the bucket out of reach; both members of
/// that configuration killed, it is degraded, its commits told durable once the bucket holds them,
/// and it drops to f = 0.
#[test]
fn lost_members_are_replaced_then_the_bucket_releases_commits() {
    let root = tempfile::tempdir().unwrap();
    let bucket = root.path().join("bucket");
    std::fs::create_dir(&bucket).unwrap();
    let (addresses, mut daemons) = hosts(root.path(), &bucket, 4);
    let store = daemons[0].dir.join("store");
    let cell = create_replicated(&daemons[0], &store);
    // Its VM's opening takes it again, in a configuration of its own.
    let client = connect(&daemons[0], &store);
    let (first, _, members) = configuration(&bucket, cell);
    let host = |address: &String| addresses.iter().position(|a| a == address).unwrap();
    let lost = host(&members[1]);
    let spare = (1..4).find(|h| !members.contains(&addresses[*h])).unwrap();

    write(&client, 1..=2);
    daemons[lost].kill();
    // The writer finds it lost as it sends what it commits, and replaces it within a second.
    write(&client, 3..=4);
    until("the member replaced", || configuration(&bucket, cell).0 > first);
    let (_, f, now) = configuration(&bucket, cell);
    assert_eq!(f, 1);
    assert!(now.contains(&addresses[spare]) && !now.contains(&addresses[lost]), "{now:?}");
    until("degraded no more", || {
        let stat = daemons[0].control.stat_cell(&store).unwrap();
        !stat.degraded && stat.members_up == 2 && stat.configuration > first
    });
    reachable(&bucket, false);
    write(&client, 5..=8);
    reachable(&bucket, true);

    // Both members lost, and the bucket out of reach: a commit told durable waits for the bucket
    // (Codex's review), and is told so once it holds it.
    for member in &now[1..] {
        daemons[host(member)].kill();
    }
    reachable(&bucket, false);
    let waiting = std::thread::spawn(move || {
        write(&client, 9..=9);
        client
    });
    std::thread::sleep(Duration::from_secs(2));
    assert!(!waiting.is_finished(), "told durable on the writer's disk alone");
    reachable(&bucket, true);
    let client = waiting.join().unwrap();
    write(&client, 10..=12);
    let stat = daemons[0].control.stat_cell(&store).unwrap();
    assert!(stat.degraded, "{stat:?}");
    until("dropped to f = 0", || configuration(&bucket, cell).1 == 0);
    until("f = 0 said", || daemons[0].control.stat_cell(&store).unwrap().active_replicas == 0);
    write(&client, 13..=14);
    check(&client, 1..=14);
}
