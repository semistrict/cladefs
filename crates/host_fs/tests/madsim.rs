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
    runtime.add_simulator::<node_local::Plugin>();
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
                .init(move || {
                    node_local::starting();
                    boot(host, found.clone())
                })
                .build();
            nodes.push(node);
        }
        madsim::time::sleep(Duration::from_secs(1)).await;
        handle.kill(nodes[0].id());
        handle.restart(nodes[0].id());
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
            runtime.add_simulator::<node_local::Plugin>();
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
                        node_local::starting();
                        let (found, seen) = (found2.clone(), seen2.clone());
                        async move {
                            seen.lock().unwrap().push(host_fs::exists("/disk1/kept/unsynced"));
                            boot(1, found).await
                        }
                    })
                    .build();
                madsim::time::sleep(Duration::from_secs(1)).await;
                handle.kill(node.id());
                handle.restart(node.id());
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
