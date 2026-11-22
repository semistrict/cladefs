// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-537.3.6 (doc-21, doc-12's Replicated rows): replicated cells end to end, on extent daemons
//! of their own processes sharing a file bucket, as hosts do, each a member with a replica
//! directory (`CLADEFSD_REPLICAS`) and the others its peers. The bucket out of reach is its
//! directory moved aside, a file in its place: its writes fail, and are tried again, as a
//! bucket's that does not answer.
//!
//! - The writer's daemon killed and its directory, disk and replica directory, gone, every commit
//!   its guest was told durable (`COMMIT_UPLOADED`, which a replicated cell's waits for Replicated)
//!   while the bucket was out of reach is there after a take on another host.
//! - The writer's daemon killed and started again on the same directory, every such commit is
//!   there after its VM opens the cell again.
//! - The writer's daemon and both its members' killed, their directories gone, a take on the spare
//!   host is refused, and, forced (TASK-681), recovers from the bucket alone.
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

/// A daemon serving in `dir`, keeping its cells in the file bucket at `bucket`, its replica
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
        // Members replaced within the tests' patience, not the default 30 s.
        .env("CLADEFSD_REPLACE_AFTER", "1")
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

/// Makes a cell at `path` on `daemon`, a new cell replicated with f = 1, its guest's fsync and
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

/// Writes extents `sequences` of `client`'s, each committed durable: replicated, or uploaded.
fn write(client: &CellClient, sequences: std::ops::RangeInclusive<u64>) {
    for sequence in sequences {
        let extent = ExtentId { cell: client.cell(), sequence };
        client.write(extent, 0, &[VolatileSlice::from(&data(sequence)[..])]).unwrap();
        client.commit(Some(format!("root {sequence}").as_bytes())).unwrap();
    }
}

/// Reads extents `sequences` of `client`'s, each as [`write`] wrote it.
fn check(client: &CellClient, sequences: std::ops::RangeInclusive<u64>) {
    for sequence in sequences {
        let mut read = vec![0; EXTENT];
        let extent = ExtentId { cell: client.cell(), sequence };
        client.read(extent, 0, &mut [VolatileSliceMut::from(&mut read[..])]).unwrap();
        assert!(read == data(sequence), "extent {sequence} read otherwise");
    }
}

/// Makes the bucket at `bucket` out of reach of its writers, or in reach again: its directory
/// moved aside and a file in its place, which no write goes through, root's neither (the
/// verifying review of TASK-537.3.6: the test host's runs are root's, whom a read-only directory
/// stops not); then back.
fn reachable(bucket: &Path, reach: bool) {
    let aside = bucket.with_extension("aside");
    match reach {
        false => {
            std::fs::rename(bucket, &aside).unwrap();
            std::fs::write(bucket, b"out of reach").unwrap();
        }
        true => {
            std::fs::remove_file(bucket).unwrap();
            std::fs::rename(&aside, bucket).unwrap();
        }
    }
}

/// The head of `cell` in the bucket at `bucket`.
fn head(bucket: &Path, cell: CellId) -> bucket_cell::layout::Head {
    let url = format!("file://{}/", bucket.display());
    let bucket = Arc::new(bucket_cell::object_bucket::ObjectBucket::from_url(&url).unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(bucket_cell::head::get(&*bucket, cell)).unwrap().unwrap()
}

/// The configuration the head of `cell` names, in the bucket at `bucket`: its number, f, and its
/// members' addresses, the writer's first.
fn configuration(bucket: &Path, cell: CellId) -> (u64, u8, Vec<String>) {
    let configuration = head(bucket, cell).replication.unwrap().configuration.unwrap();
    let members = configuration.members.iter().map(|member| member.address.clone()).collect();
    (configuration.number, configuration.f, members)
}

/// A VM's connection to the cell at `path` on `daemon`: once the daemon, which made or took it
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
    let cell_dir = daemons[0].dir.join("store");
    let cell = create_replicated(&daemons[0], &cell_dir);
    let (number, f, members) = configuration(&bucket, cell);
    assert_eq!((number, f, members.len()), (1, 1, 3));

    let client = connect(&daemons[0], &cell_dir);
    write(&client, 1..=2);
    // The bucket out of reach: what is told durable from now on is on the members alone.
    let before = head(&bucket, cell).end;
    reachable(&bucket, false);
    // A guest committing all the while: the last extent each commit told durable.
    let acked = Arc::new(std::sync::atomic::AtomicU64::new(2));
    let committing = {
        let acked = acked.clone();
        std::thread::spawn(move || {
            for sequence in 3.. {
                let extent = ExtentId { cell: client.cell(), sequence };
                let written = client.write(extent, 0, &[VolatileSlice::from(&data(sequence)[..])]);
                let root = format!("root {sequence}");
                if written.and_then(|()| client.commit(Some(root.as_bytes()))).is_err() {
                    return;
                }
                acked.store(sequence, std::sync::atomic::Ordering::SeqCst);
            }
        })
    };
    until("commits told durable", || acked.load(std::sync::atomic::Ordering::SeqCst) >= 10);

    // The writer's host lost mid-workload, its disk with it.
    daemons[0].kill();
    let acked = acked.load(std::sync::atomic::Ordering::SeqCst);
    committing.join().unwrap();
    std::fs::remove_dir_all(&daemons[0].dir).unwrap();
    reachable(&bucket, true);
    assert_eq!(head(&bucket, cell).end, before, "commits after 2 uploaded: nothing to recover");

    let taken = daemons[1].dir.join("taken");
    daemons[1].control.create_cell(&taken, CAPACITY, cell, (Some(2), false)).unwrap();
    let client = connect(&daemons[1], &taken);
    check(&client, 1..=acked);
    // One host of three lost, no configuration of f = 1 is to be had: the taker serves degraded,
    // its commits told durable once the bucket holds them, and, the bucket holding all it
    // recovered, drops to f = 0.
    write(&client, 101..=102);
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
    write(&client, 103..=104);
    until("replicated again", || configuration(&bucket, cell).1 == 1);
    let (_, _, members) = configuration(&bucket, cell);
    assert_eq!(members[0], addresses[1]);
    assert_eq!(members.len(), 3);
    until("degraded no more", || !daemons[1].control.stat_cell(&taken).unwrap().degraded);
    let before = head(&bucket, cell).end;
    reachable(&bucket, false);
    write(&client, 105..=106);
    reachable(&bucket, true);
    assert_eq!(head(&bucket, cell).end, before, "told durable by the bucket, not the members");
    check(&client, 1..=acked);
    check(&client, 101..=106);
}

/// doc-12's row of cladefsd killed with its disk kept, for a replicated cell: its commits, told
/// durable while the bucket was out of reach, on its writer's disk and a member; the writer's
/// daemon killed mid-workload and started again on the same directory; the VM's opening of the
/// cell again is a take, which fences the members and recovers from them and its own write-ahead
/// log, and holds every one of them.
#[test]
fn replicated_commits_survive_their_daemon_killed_and_reopened() {
    let root = tempfile::tempdir().unwrap();
    let bucket = root.path().join("bucket");
    std::fs::create_dir(&bucket).unwrap();
    let (addresses, mut daemons) = hosts(root.path(), &bucket, 3);
    let cell_dir = daemons[0].dir.join("store");
    let cell = create_replicated(&daemons[0], &cell_dir);
    let client = connect(&daemons[0], &cell_dir);
    write(&client, 1..=2);
    let before = head(&bucket, cell).end;
    let (first, _, _) = configuration(&bucket, cell);
    reachable(&bucket, false);
    let acked = Arc::new(std::sync::atomic::AtomicU64::new(2));
    let committing = {
        let acked = acked.clone();
        std::thread::spawn(move || {
            for sequence in 3.. {
                let extent = ExtentId { cell: client.cell(), sequence };
                let written = client.write(extent, 0, &[VolatileSlice::from(&data(sequence)[..])]);
                let root = format!("root {sequence}");
                if written.and_then(|()| client.commit(Some(root.as_bytes()))).is_err() {
                    return;
                }
                acked.store(sequence, std::sync::atomic::Ordering::SeqCst);
            }
        })
    };
    until("commits told durable", || acked.load(std::sync::atomic::Ordering::SeqCst) >= 10);

    // The writer's daemon killed mid-workload, its disk kept, and started again on it.
    daemons[0].kill();
    let acked = acked.load(std::sync::atomic::Ordering::SeqCst);
    committing.join().unwrap();
    reachable(&bucket, true);
    assert_eq!(head(&bucket, cell).end, before, "commits after 2 uploaded: nothing to recover");
    // Its sockets gone, as a host's service manager clears its runtime directory: a daemon never
    // binds over one (`bind_owner_only`), which may be another's serving.
    std::fs::remove_file(&daemons[0].vms).unwrap();
    std::fs::remove_file(&daemons[0].control_socket).unwrap();
    daemons[0] = daemon(&daemons[0].dir.clone(), &bucket, &addresses);

    let client = connect(&daemons[0], &cell_dir);
    check(&client, 1..=acked);
    let (number, f, members) = configuration(&bucket, cell);
    assert!(number > first && f == 1, "the reopening took it: {number} after {first}, f {f}");
    assert_eq!(members[0], addresses[0]);
    // Its commits told durable once the bucket holds what it recovered, which no member of the new
    // configuration need hold (`Replicator::base`); then replicated again, told durable with the
    // bucket out of reach.
    write(&client, 101..=102);
    until("degraded no more", || !daemons[0].control.stat_cell(&cell_dir).unwrap().degraded);
    let before = head(&bucket, cell).end;
    reachable(&bucket, false);
    write(&client, 103..=104);
    reachable(&bucket, true);
    assert_eq!(head(&bucket, cell).end, before, "told durable by the bucket, not the members");
    check(&client, 1..=acked);
    check(&client, 101..=104);
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
    let cell_dir = daemons[0].dir.join("store");
    let cell = create_replicated(&daemons[0], &cell_dir);
    // Its VM's opening takes it again, in a configuration of its own.
    let client = connect(&daemons[0], &cell_dir);
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
        let stat = daemons[0].control.stat_cell(&cell_dir).unwrap();
        !stat.degraded && stat.members_up == 2 && stat.configuration > first
    });
    let before = head(&bucket, cell).end;
    reachable(&bucket, false);
    write(&client, 5..=8);
    reachable(&bucket, true);
    assert_eq!(head(&bucket, cell).end, before, "told durable by the bucket, not the members");

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
    let stat = daemons[0].control.stat_cell(&cell_dir).unwrap();
    assert!(stat.degraded, "{stat:?}");
    until("dropped to f = 0", || configuration(&bucket, cell).1 == 0);
    until("f = 0 said", || daemons[0].control.stat_cell(&cell_dir).unwrap().active_replicas == 0);
    write(&client, 13..=14);
    check(&client, 1..=14);
}

/// TASK-681 (doc-21's row "Writer's host and a peer lost", past it): a replicated cell's writer's
/// host and both its members' lost, their disks with them, the commits told durable since the
/// bucket was last reachable on them alone; the spare host's take is refused, no member answering
/// its fence; forced (`CreateCell`'s `force`), it takes the cell from the bucket, and says past
/// which commit what was told replicated may be lost.
#[test]
fn takes_of_cells_whose_members_are_lost_go_on_by_force() {
    let root = tempfile::tempdir().unwrap();
    let bucket = root.path().join("bucket");
    std::fs::create_dir(&bucket).unwrap();
    let (addresses, mut daemons) = hosts(root.path(), &bucket, 4);
    let cell_dir = daemons[0].dir.join("store");
    let cell = create_replicated(&daemons[0], &cell_dir);
    let client = connect(&daemons[0], &cell_dir);
    write(&client, 1..=2);
    daemons[0].control.sync_cell(&cell_dir).unwrap();
    let uploaded = daemons[0].control.stat_cell(&cell_dir).unwrap().uploaded;
    let (_, _, members) = configuration(&bucket, cell);
    reachable(&bucket, false);
    write(&client, 3..=4);
    drop(client);

    // The writer's host and its members' lost, their disks with them.
    let lost: Vec<usize> =
        members.iter().map(|member| addresses.iter().position(|a| a == member).unwrap()).collect();
    for &host in &lost {
        daemons[host].kill();
        std::fs::remove_dir_all(&daemons[host].dir).unwrap();
    }
    reachable(&bucket, true);
    let spare = (0..4).find(|host| !lost.contains(host)).unwrap();

    let refused = daemons[spare].dir.join("refused");
    let failure = daemons[spare].control.create_cell(&refused, CAPACITY, cell, (Some(2), false));
    let failure = failure.err().unwrap();
    assert_eq!(failure.errno(), Some(cladefs::Errno::EIO), "{failure}");

    let taken = daemons[spare].dir.join("taken");
    daemons[spare].control.create_cell(&taken, CAPACITY, cell, (Some(3), true)).unwrap();
    let client = connect(&daemons[spare], &taken);
    check(&client, 1..=2);
    let stat = daemons[spare].control.stat_cell(&taken).unwrap();
    assert_eq!(stat.forced_after, Some(uploaded), "{stat:?}");
    write(&client, 5..=6);
    check(&client, 5..=6);
}
