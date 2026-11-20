// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-685.9 (doc-24): cladefsd under madsim, built with `RUSTFLAGS="--cfg madsim"`: two daemons
//! booted as simulated hosts, each its own node with its own environment, sharing a bucket in
//! memory, their control APIs on the simulated network. A cell made on one, its commits synced, is
//! taken by the other at a later placement's token. TASK-685.5: the bucket's requests delayed and
//! failed, before or after landing, and the bucket out of reach for stretches, as the seed says
//! (`bucket_cell::faults::Seeded`), which the daemons' retries ride out.

#![cfg(all(madsim, target_os = "linux"))]

use bucket_cell::object_bucket::ObjectBucket;
use cladefs::cladefsd::control::proto::{CreateCellRequest, StatCellRequest, SyncCellRequest};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// The control API's port, on each host's address.
const CONTROL: u16 = 7000;

/// The address of host `host`'s daemon's control API.
fn control(host: u8) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, host)), CONTROL)
}

/// Boots host `host`'s daemon, its stores under `root`, its cells in `bucket`, at placement token
/// `host`, and serves for as long as the host runs.
async fn boot(host: u8, root: &Path, bucket: Arc<ObjectBucket>) {
    let tenants = root.join("tenants").display().to_string();
    node_local::env::set_vars([
        ("CLADEFSD_BUCKET", "memory:///cells".to_owned()),
        ("CLADEFSD_TOKEN", host.to_string()),
        ("CLADEFSD_GRPC", control(host).to_string()),
        ("CLADEFSD_TENANTS", tenants),
    ]);
    cladefs::cladefsd::use_bucket(bucket);
    cladefs::cladefsd::boot(&root.join("vms.sock"), Some(256 << 20)).await.unwrap();
    // As cladefsd's main parks once it booted: a node whose init returns exits, its tasks with it.
    std::future::pending::<()>().await;
}

/// The daemon at `host`'s control API, once it answers.
async fn connect(
    host: u8,
) -> cladefs::cladefsd::control::proto::cladefsd_client::CladefsdClient<tonic::transport::Channel> {
    let mut failed = None;
    for _ in 0..50 {
        match cladefs::cladefsd::control::connect(control(host)).await {
            Ok(client) => return client,
            Err(err) => failed = Some(err),
        }
        madsim::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("host {host}'s control API never answered: {failed:?}");
}

/// What every test's hosts share: their logger, their KMS's master key, and a bucket in memory,
/// its requests delayed and failed as the seed says.
fn world() -> (tempfile::TempDir, Arc<ObjectBucket>) {
    // Once a process: a determinism check runs the test twice in it.
    let _ = cladefs::cladefsd::logging::to_stderr();
    // SAFETY: the simulation's one thread, before any node reads the environment.
    unsafe { std::env::set_var("CLADEFS_MASTER_KEY", "ab".repeat(32)) };
    let odds = bucket_cell::faults::Odds {
        delay_most: Duration::from_millis(50),
        fail_before: 0.05,
        fail_after: 0.05,
        out_of_reach: 0.01,
        out_for: Duration::from_millis(500),
    };
    let objects = Arc::new(object_store::memory::InMemory::new());
    let seeded = bucket_cell::faults::Seeded::new(objects, madsim::rand::random(), odds);
    let seeded = bucket_cell::faults::SeededStore(Arc::new(seeded));
    (tempfile::tempdir().unwrap(), Arc::new(ObjectBucket::new(Arc::new(seeded), "")))
}

/// Hosts 1 to `count`, booted, their stores under `dir`'s `hostN`, their cells in `bucket`: their
/// nodes, and their roots.
fn hosts(
    count: u8,
    dir: &Path,
    bucket: &Arc<ObjectBucket>,
) -> (Vec<madsim::runtime::NodeHandle>, Vec<PathBuf>) {
    let handle = madsim::runtime::Handle::current();
    let roots: Vec<PathBuf> = (1..=count).map(|host| dir.join(format!("host{host}"))).collect();
    let nodes = (1..=count)
        .zip(&roots)
        .map(|(host, root)| {
            std::fs::create_dir_all(root).unwrap();
            let (root, bucket) = (root.clone(), bucket.clone());
            handle
                .create_node()
                .name(format!("host{host}"))
                .ip(control(host).ip())
                .init(move || {
                    let (root, bucket) = (root.clone(), bucket.clone());
                    async move { boot(host, &root, bucket).await }
                })
                .build()
        })
        .collect();
    (nodes, roots)
}

/// A guest of this host's daemon, once it serves.
async fn guest() -> cladefs::cladefsd::guest::Guest {
    while !cladefs::cladefsd::serving() {
        madsim::time::sleep(Duration::from_millis(100)).await;
    }
    cladefs::cladefsd::guest::Guest::connect().unwrap()
}

/// The VM manager's node, which calls the hosts' control APIs.
fn manager() -> madsim::runtime::NodeHandle {
    let handle = madsim::runtime::Handle::current();
    handle.create_node().name("vm manager").ip("10.0.0.100".parse().unwrap()).build()
}

#[madsim::test]
async fn two_daemons_boot_as_hosts_and_one_takes_the_others_cell() {
    let (dir, bucket) = world();
    let (_nodes, roots) = hosts(2, dir.path(), &bucket);
    let client = manager();
    let (first, second) = (roots[0].join("store"), roots[1].join("store"));
    client
        .spawn(async move {
            let mut one = connect(1).await;
            let made = one
                .create_cell(CreateCellRequest {
                    path: first.display().to_string(),
                    capacity: 64 << 20,
                    ..CreateCellRequest::default()
                })
                .await
                .unwrap()
                .into_inner();
            let path = first.display().to_string();
            one.sync_cell(SyncCellRequest { path: path.clone() }).await.unwrap();
            let mut two = connect(2).await;
            let taken = two
                .create_cell(CreateCellRequest {
                    path: second.display().to_string(),
                    capacity: 64 << 20,
                    token: Some(2),
                    tenant: made.tenant,
                    cell: made.cell,
                    ..CreateCellRequest::default()
                })
                .await
                .unwrap()
                .into_inner();
            assert_eq!((taken.tenant, taken.cell), (made.tenant, made.cell));
            let stat = two.stat_cell(StatCellRequest { path: second.display().to_string() });
            assert_eq!(stat.await.unwrap().into_inner().capacity, 64 << 20);
        })
        .await
        .unwrap();
}

/// TASK-685.6: a simulated guest of host 1's writes a block and commits it, uploaded; once it is
/// gone and host 2 has taken its cell, a guest of host 2's reads the block as written: through the
/// daemons' own handling of a VM process's requests, its connection in memory.
#[madsim::test]
async fn a_guests_commit_uploaded_on_one_host_reads_back_on_another() {
    let (dir, bucket) = world();
    let (nodes, roots) = hosts(2, dir.path(), &bucket);
    let (first, second) = (roots[0].join("store"), roots[1].join("store"));
    let block: Vec<u8> = (0..4096u32).map(|at| (at * 7 % 251) as u8).collect();
    let written = block.clone();
    let cell = nodes[0]
        .spawn(async move {
            let guest = guest().await;
            let cell = guest.open(&first, 64 << 20).await.unwrap();
            let extent = cladefs::ExtentId { cell, sequence: 1 };
            guest.write(extent, 0, &written).await.unwrap();
            guest.commit(cladefs::Commit::Uploaded).await.unwrap();
            guest.close();
            cell
        })
        .await
        .unwrap();
    manager()
        .spawn(async move {
            let mut two = connect(2).await;
            let taking = CreateCellRequest {
                path: second.display().to_string(),
                capacity: 64 << 20,
                token: Some(2),
                cell,
                ..CreateCellRequest::default()
            };
            // Its first host's teardown may hold the cell's head a while yet: taken once it lets go.
            loop {
                match two.create_cell(taking.clone()).await {
                    Ok(_) => break,
                    Err(status) => {
                        log::info!("taking the cell: {status:?}");
                        madsim::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        })
        .await
        .unwrap();
    let second = roots[1].join("store");
    let read = nodes[1]
        .spawn(async move {
            let guest = guest().await;
            assert_eq!(guest.open(&second, 64 << 20).await.unwrap(), cell);
            let extent = cladefs::ExtentId { cell, sequence: 1 };
            guest.read(extent, 0, 4096).await.unwrap()
        })
        .await
        .unwrap();
    assert!(read == block, "host 2 read another block than host 1's guest committed");
}
