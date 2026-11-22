// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-685.4: under madsim, each simulated host's disk is its own, in memory; a host killed and
//! started again finds what was durable on it, and of what was not, what a crash kept, as the seed
//! says; and its locks gone. Built with `RUSTFLAGS="--cfg madsim"`.

#![cfg(madsim)]

use host_fs::FileExt as _;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What a host's boot found on its disk: each boot's.
type Found = Arc<Mutex<Vec<Boot>>>;

#[derive(Debug, PartialEq, Eq)]
struct Boot {
    host: u8,
    /// The synced file, as read.
    synced: Option<Vec<u8>>,
    /// Whether the directory made and synced in its parent is there.
    dir: bool,
    /// Whether its lock of the synced file was refused.
    lock_refused: bool,
}

/// The first boot writes: a file synced, its directory's entry synced, in a directory made and
/// synced; and a file written but synced never, its entry neither. Every boot reads them back and
/// locks the synced file, held for as long as the host runs.
async fn boot(host: u8, found: Found) {
    let root = std::path::PathBuf::from(format!("/disk{host}"));
    let first = !host_fs::exists(&root);
    if first {
        host_fs::create_dir_all(root.join("kept")).unwrap();
        host_fs::sync_dir(&root).unwrap();
        host_fs::sync_dir("/").unwrap();
        let synced = host_fs::File::create(root.join("kept/synced")).unwrap();
        synced.write_all_at(b"durable", 0).unwrap();
        synced.sync_all().unwrap();
        host_fs::sync_dir(root.join("kept")).unwrap();
        host_fs::write(root.join("kept/unsynced"), vec![7u8; 8192]).unwrap();
    }
    let synced = host_fs::read(root.join("kept/synced")).ok();
    let dir = host_fs::is_dir(root.join("kept"));
    let lock = host_fs::File::open(root.join("kept/synced")).and_then(host_fs::Flock::try_lock);
    let lock_refused = lock.as_ref().is_err();
    found.lock().unwrap().push(Boot { host, synced, dir, lock_refused });
    std::future::pending::<()>().await;
    drop(lock);
}

#[test]
fn a_hosts_disk_keeps_what_was_durable_through_its_kill() {
    let runtime = madsim::runtime::Runtime::new();
    runtime.block_on(async {
        let handle = madsim::runtime::Handle::current();
        let found: Found = Arc::default();
        let mut nodes = Vec::new();
        for host in 1..=2u8 {
            let found = found.clone();
            let node = handle
                .create_node()
                .name(format!("host{host}"))
                .ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, host)))
                .init(move || boot(host, found.clone()))
                .build();
            nodes.push(node);
        }
        madsim::time::sleep(Duration::from_secs(1)).await;
        node_local::restart(nodes[0].id()).await;
        madsim::time::sleep(Duration::from_secs(1)).await;
        let boot =
            |host| Boot { host, synced: Some(b"durable".to_vec()), dir: true, lock_refused: false };
        // Each host's disk its own; host 1's, crashed, what was durable kept, its lock gone.
        assert_eq!(*found.lock().unwrap(), [boot(1), boot(2), boot(1)]);
    });
}

/// What a crash keeps of what was never synced is the seed's: of many, some keep the unsynced
/// file's entry and some lose it, and each seed keeps the same twice.
#[test]
fn what_a_crash_keeps_is_the_seeds() {
    let kept = |seed: u64| {
        std::thread::spawn(move || {
            let runtime = madsim::runtime::Runtime::with_seed_and_config(seed, Default::default());
            runtime.block_on(async {
                let handle = madsim::runtime::Handle::current();
                let found: Found = Arc::default();
                let seen = Arc::new(Mutex::new(Vec::new()));
                let (found2, seen2) = (found.clone(), seen.clone());
                let node = handle
                    .create_node()
                    .name("host")
                    .ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)))
                    .init(move || {
                        let (found, seen) = (found2.clone(), seen2.clone());
                        async move {
                            seen.lock().unwrap().push(host_fs::exists("/disk1/kept/unsynced"));
                            boot(1, found).await
                        }
                    })
                    .build();
                madsim::time::sleep(Duration::from_secs(1)).await;
                node_local::restart(node.id()).await;
                madsim::time::sleep(Duration::from_secs(1)).await;
                let seen = seen.lock().unwrap().clone();
                seen[1]
            })
        })
        .join()
        .unwrap()
    };
    let runs: Vec<bool> = (1..=16).map(kept).collect();
    assert!(runs.contains(&true) && runs.contains(&false), "every seed kept the same: {runs:?}");
    assert_eq!(runs, (1..=16).map(kept).collect::<Vec<_>>());
}

/// Board 5654: calls changing a host's disk fail as the faults say, before they change anything,
/// with EIO, or, those adding data, ENOSPC; calls that read never; and once healed, none.
#[test]
fn calls_changing_a_disk_fail_as_its_faults_say() {
    let runtime = madsim::runtime::Runtime::new();
    let handle = runtime.create_node().name("host").ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
    let node = handle.build();
    runtime
        .block_on(node.spawn(async {
            let errno = |result: std::io::Result<()>| result.unwrap_err().raw_os_error().unwrap();
            host_fs::create_dir_all("/disk/d").unwrap();
            let file = host_fs::File::create("/disk/d/f").unwrap();
            file.write_all_at(b"kept", 0).unwrap();

            host_fs::faults::set(host_fs::faults::Odds { fail: 1.0 });
            assert!([libc::EIO, libc::ENOSPC].contains(&errno(file.write_all_at(b"gone", 0))));
            assert_eq!(errno(file.sync_all()), libc::EIO);
            assert_eq!(errno(host_fs::rename("/disk/d/f", "/disk/d/g")), libc::EIO);
            assert_eq!(errno(host_fs::remove_file("/disk/d/f")), libc::EIO);
            assert!([libc::EIO, libc::ENOSPC].contains(&errno(host_fs::create_dir("/disk/e"))));
            let created = host_fs::File::create_new("/disk/d/new").map(drop);
            assert!([libc::EIO, libc::ENOSPC].contains(&errno(created)));
            // Nothing changed; and what reads reads.
            assert_eq!(host_fs::read("/disk/d/f").unwrap(), b"kept");
            let names: Vec<_> = host_fs::read_dir("/disk/d")
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(names, ["f"]);

            host_fs::faults::set(host_fs::faults::Odds::NONE);
            file.write_all_at(b"made", 0).unwrap();
            file.sync_all().unwrap();
            host_fs::rename("/disk/d/f", "/disk/d/g").unwrap();
            assert_eq!(host_fs::read("/disk/d/g").unwrap(), b"made");
        }))
        .unwrap();
}

/// Board 5654: which calls fail is the seed's: the same twice, other seeds otherwise.
#[test]
fn which_calls_fail_is_the_seeds() {
    let failed = |seed: u64| {
        std::thread::spawn(move || {
            let runtime = madsim::runtime::Runtime::with_seed_and_config(seed, Default::default());
            let node =
                runtime.create_node().name("host").ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
            let node = node.build();
            runtime
                .block_on(node.spawn(async {
                    host_fs::create_dir_all("/disk").unwrap();
                    let file = host_fs::File::create("/disk/f").unwrap();
                    host_fs::faults::set(host_fs::faults::Odds { fail: 0.5 });
                    (0..32u64).map(|at| file.write_all_at(b"x", at).is_err()).collect::<Vec<_>>()
                }))
                .unwrap()
        })
        .join()
        .unwrap()
    };
    let (one, again, other) = (failed(1), failed(1), failed(2));
    assert_eq!(one, again);
    assert!(one.contains(&true) && one.contains(&false), "{one:?}");
    assert_ne!(one, other);
}

/// Board 5890: a crash damages no byte a write leaves as it was, the sector's others included
/// (powersafe overwrite, decision-3). A log's append writes back whole pages, writing again the
/// bytes of the record synced before it, which a crash must keep, whatever it makes of the rest:
/// over many seeds, the bytes written again are always kept, and the new ones are now and then
/// garbage.
#[test]
fn a_crash_damages_no_byte_a_write_leaves_as_it_was() {
    const SYNCED: usize = 700;
    let crashed = |seed: u64| {
        std::thread::spawn(move || {
            let runtime = madsim::runtime::Runtime::with_seed_and_config(seed, Default::default());
            runtime.block_on(async {
                let handle = madsim::runtime::Handle::current();
                let read = Arc::new(Mutex::new(Vec::new()));
                let read2 = read.clone();
                let node = handle
                    .create_node()
                    .name("host")
                    .ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)))
                    .init(move || {
                        let read = read2.clone();
                        async move {
                            if host_fs::exists("/log") {
                                read.lock().unwrap().push(host_fs::read("/log").unwrap());
                            } else {
                                let log = host_fs::File::create("/log").unwrap();
                                log.write_all_at(&[b'a'; SYNCED], 0).unwrap();
                                log.sync_all().unwrap();
                                host_fs::sync_dir("/").unwrap();
                                // The record synced, written again, and the next after it.
                                let mut page = vec![b'a'; SYNCED];
                                page.resize(1024, b'b');
                                log.write_all_at(&page, 0).unwrap();
                            }
                            std::future::pending::<()>().await
                        }
                    })
                    .build();
                madsim::time::sleep(Duration::from_secs(1)).await;
                node_local::restart(node.id()).await;
                madsim::time::sleep(Duration::from_secs(1)).await;
                let read = read.lock().unwrap().clone();
                read.into_iter().next().expect("the boot after the crash read the log")
            })
        })
        .join()
        .unwrap()
    };
    let logs: Vec<Vec<u8>> = (1..=64).map(crashed).collect();
    for (seed, log) in (1..=64).zip(&logs) {
        assert_eq!(&log[..SYNCED], &[b'a'; SYNCED][..], "seed {seed} damaged what was synced");
    }
    let garbage = |log: &Vec<u8>| log.len() == 1024 && log[SYNCED..].iter().any(|&b| b != b'b');
    assert!(logs.iter().any(garbage), "no seed's crash made garbage of the new bytes");
}
