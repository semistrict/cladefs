// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-260: a read for a guest request of what only object storage holds logs the requests
//! it makes of the bucket, on the bucket runtime, with the guest request.

// cladefs: the daemon's, on Linux and macOS (board 13065).
#![cfg(not(madsim))]

use bucket_cell::object_bucket::ObjectBucket;
use cladefs::request::{RequestId, within};
use cladefs::{CellOf, ExtentId, Host, Opening, VolatileSlice, VolatileSliceMut};
use object_store::memory::InMemory;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn reads_of_object_storage_are_logged_with_their_guest_request() {
    const BLOCK: usize = 4096;
    let lines = cladefs::cladefsd::logging::capture();
    let dir = tempfile::tempdir().unwrap();
    let host = Host::process();
    let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "").unrouted());
    let url = format!("memory:///bucket-logging-{}", std::process::id());
    cladefs_core::host_cell::use_bucket_on(host, &url, bucket.clone());
    let keeping = local_cell::upload::Options {
        interval: Duration::from_secs(3600),
        upload_closed: false,
        ..cladefs_core::host_cell::keeping().unwrap()
    };

    let cell = cladefs_core::host_cell::new_alone();
    let first = Opening::here()
        .unwrap()
        .with_keeping(keeping.clone())
        .create_kept(dir.path().join("first"), 1 << 30, CellOf::Named(cell), &url)
        .await
        .unwrap();
    let pattern: Vec<u8> = (0..BLOCK).map(|i| 1u8 ^ (i % 251) as u8).collect();
    let extent = ExtentId { cell: cell.id, sequence: 1 };
    first.write(extent, 0, &[VolatileSlice::from(&pattern[..])]).await.unwrap();
    first.commit(None).await.unwrap();
    drop(first);

    let second = Opening::here()
        .unwrap()
        .with_keeping(keeping)
        .create_kept(dir.path().join("second"), 1 << 30, CellOf::Named(cell), &url)
        .await
        .unwrap();
    lines.lock().unwrap().clear();
    let request =
        RequestId { guest: NonZeroU64::new(42).unwrap(), open_cell: Some("/stores/vm1".into()) };
    let mut data = vec![0; BLOCK];
    within(Some(request), async {
        second.read(extent, 0, &mut [VolatileSliceMut::from(&mut data[..])]).await
    })
    .await
    .unwrap();
    assert_eq!(data, pattern);
    let mut made: Vec<(String, String)> = (lines.lock().unwrap().iter())
        .filter_map(|line| line.split_once(' '))
        .filter_map(|(_, line)| {
            line.strip_prefix("DEBUG bucket_cell::object_bucket: object storage: ")
        })
        .map(|line| {
            let kind = line.split(' ').next().unwrap().to_owned();
            (kind, line.split_once(" request_id=").map_or("none", |(_, id)| id).to_owned())
        })
        .collect();
    made.dedup();
    assert_eq!(made, [("get-range".to_owned(), "42 store=/stores/vm1".to_owned())]);
    drop(second);
}
