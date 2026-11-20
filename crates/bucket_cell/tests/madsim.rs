// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-685.1's spike (doc-24): the bucket protocol under madsim, built with
//! `RUSTFLAGS="--cfg madsim"`: hosts as madsim nodes, each taking a cell's head from a bucket in
//! memory they share, at times the seed decides; one killed and started again, which takes it
//! back. A run of a seed replays (`MADSIM_TEST_CHECK_DETERMINISM=1`).

#![cfg(madsim)]

use bucket_cell::layout::CellId;
use bucket_cell::object_bucket::ObjectBucket;
use bucket_cell::{head, lifecycle};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const CELL: CellId = CellId { tenant: 1, id: 3 };

/// Takes `cell`'s head as `holder`, of `token`'s placement: the epoch it took it at.
async fn take(bucket: &ObjectBucket, holder: u64, token: u64) -> Result<u64, String> {
    let cell = CELL;
    let taking = head::Taking { holder, token, ..head::Taking::default() };
    let read = head::read(bucket, cell).await;
    head::take(bucket, cell, read, &taking)
        .await
        .map(|held| held.head.epoch)
        .map_err(|e| e.to_string())
}

#[madsim::test]
async fn hosts_take_a_cells_head_in_turn_and_one_killed_takes_it_back() {
    let bucket = Arc::new(ObjectBucket::new(Arc::new(object_store::memory::InMemory::new()), ""));
    lifecycle::create(&*bucket, CELL, 1).await.unwrap();
    let handle = madsim::runtime::Handle::current();
    let taken = Arc::new(Mutex::new(Vec::new()));
    let mut nodes = Vec::new();
    for host in 1..=3u8 {
        let (bucket, taken) = (bucket.clone(), taken.clone());
        let node = handle
            .create_node()
            .name(format!("host{host}"))
            .ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, host)))
            .init(move || {
                let (bucket, taken) = (bucket.clone(), taken.clone());
                async move {
                    // A random wait, the seed's, then a taking at its placement's token.
                    let wait = madsim::rand::random::<u64>() % 1000;
                    madsim::time::sleep(Duration::from_millis(wait)).await;
                    let token = u64::from(host);
                    let epoch = take(&bucket, 100 + u64::from(host), token).await;
                    taken.lock().unwrap().push((host, epoch));
                }
            })
            .build();
        nodes.push(node);
    }
    madsim::time::sleep(Duration::from_secs(2)).await;
    {
        let taken = taken.lock().unwrap();
        assert_eq!(taken.len(), 3, "{taken:?}");
        // A taking refused is one whose token an earlier placement's exceeds (TASK-39.7).
        let epochs: Vec<u64> = taken.iter().filter_map(|(_, epoch)| epoch.clone().ok()).collect();
        let mut sorted = epochs.clone();
        sorted.dedup();
        assert_eq!(sorted.len(), epochs.len(), "each taking its own epoch: {taken:?}");
    }
    // host3, of the highest token, killed and started again: it takes the cell back, a later epoch.
    handle.kill(nodes[2].id());
    handle.restart(nodes[2].id());
    madsim::time::sleep(Duration::from_secs(2)).await;
    let taken = taken.lock().unwrap();
    assert_eq!(taken.len(), 4, "{taken:?}");
    let (host, again) = taken.last().cloned().unwrap();
    assert_eq!(host, 3);
    let before = taken.iter().filter(|(host, _)| *host == 3).next().unwrap().1.clone().unwrap();
    assert!(again.clone().unwrap() > before, "{taken:?}");
}
