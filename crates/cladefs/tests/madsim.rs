// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-685.9 (doc-24): cladefsd under madsim, built with `RUSTFLAGS="--cfg madsim"`: two daemons
//! booted as simulated hosts, each its own node with its own environment, sharing a bucket in
//! memory, their control APIs on the simulated network. A cell made on one, its commits synced, is
//! taken by the other at a later placement's token.

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

#[madsim::test]
async fn two_daemons_boot_as_hosts_and_one_takes_the_others_cell() {
    // Once a process: a determinism check runs the test twice in it.
    let _ = cladefs::cladefsd::logging::to_stderr();
    // SAFETY: the simulation's one thread, before any node reads the environment.
    unsafe { std::env::set_var("CLADEFS_MASTER_KEY", "ab".repeat(32)) };
    let dir = tempfile::tempdir().unwrap();
    let bucket = Arc::new(ObjectBucket::new(Arc::new(object_store::memory::InMemory::new()), ""));
    let handle = madsim::runtime::Handle::current();
    let roots: Vec<PathBuf> = (1..=2).map(|host| dir.path().join(format!("host{host}"))).collect();
    for (host, root) in (1..=2u8).zip(&roots) {
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
            .build();
    }
    let client = handle.create_node().name("vm manager").ip("10.0.0.100".parse().unwrap()).build();
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
