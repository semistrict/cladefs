// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TASK-537.3.3 (doc-21): what a member holds of a replicated cell's writer's logs recovers. A
//! store's commits, made after its replicator started, are sent to its members; its host lost,
//! what one member holds, written into a new store's directory as its write-ahead log
//! (`replicate::restore`), opens holding every one of them, beside the bucket's.

mod common;

use bucket_cell::fetch::{Budget, Limits};
use bucket_cell::layout::test_cell;
use bucket_cell::object_bucket::ObjectBucket;
use local_cell::replicate::{self, LinkError, MemberLink, Replicator};
use local_cell::upload::{self, Runtime, Uploader};
use local_cell::{Description, LocalCell, MemDir, Options, Recovering};
use object_store::memory::InMemory;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use storage_device::ExtentId;

const BLOCK: usize = 4096;
const URL: &str = "memory:///replicated";
const DESCRIPTION: Description =
    Description { block_size: BLOCK as u32, capacity: 16 << 20, cell: test_cell(3) };

/// Small segments, so that commits span them, and a small bound for extents stored on their own,
/// so that some are.
/// Flushes of either kind, holding their runs or checking them, as the crash tests' (TASK-301).
const OPTIONS: Options = Options {
    max_extent: 48 << 10,
    segment_size: 16 << 10,
    segment_age: Duration::from_secs(3600),
    large_extent: 24 << 10,
    open_files: 3,
    view_lag: None,
    view_kept_bytes: None,
    held_most: 8 << 10,
};

/// A member in memory.
#[derive(Default)]
struct Member {
    logs: Mutex<BTreeMap<u64, Vec<u8>>>,
}

impl MemberLink for Member {
    fn append(&self, log: u64, offset: u64, bytes: &[u8]) -> Result<u64, LinkError> {
        let mut logs = self.logs.lock().unwrap();
        let held = logs.entry(log).or_default();
        if offset > held.len() as u64 {
            return Ok(held.len() as u64);
        }
        held.truncate(offset as usize);
        held.extend_from_slice(bytes);
        Ok(held.len() as u64)
    }

    fn drop_through(&self, through: u64) -> Result<(), LinkError> {
        self.logs.lock().unwrap().retain(|&log, _| log > through);
        Ok(())
    }
}

/// A member out of reach.
struct Down;

impl MemberLink for Down {
    fn append(&self, _: u64, _: u64, _: &[u8]) -> Result<u64, LinkError> {
        Err(LinkError::Down("out of reach".into()))
    }

    fn drop_through(&self, _: u64) -> Result<(), LinkError> {
        Err(LinkError::Down("out of reach".into()))
    }
}

struct Host {
    runtime: tokio::runtime::Runtime,
    bucket: Arc<ObjectBucket>,
}

impl Host {
    fn open(
        &self,
        recovering: Recovering<MemDir>,
        holder: u64,
    ) -> (Arc<LocalCell<MemDir>>, Uploader<MemDir>) {
        let runtime = Runtime {
            handle: self.runtime.handle().clone(),
            uploads: Budget::new(Limits::UNBOUNDED),
            keys: bucket_cell::keys::testing::host(),
        };
        // Nothing uploaded but what is asked for: the members hold the rest.
        let options =
            upload::Options { interval: Duration::from_secs(3600), ..upload::Options::default() };
        upload::open(recovering, OPTIONS, self.bucket.clone(), (holder, options), &runtime).unwrap()
    }
}

fn until(what: &str, done: impl Fn() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < Duration::from_secs(20), "waited for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

impl Host {
    /// Every extent `store` holds, read from it, or from the bucket where it says it is, and its
    /// root.
    fn contents(&self, store: &LocalCell<MemDir>) -> (BTreeMap<ExtentId, Vec<u8>>, Vec<u8>) {
        let mut extents = BTreeMap::new();
        for (extent, len) in store.extents().unwrap() {
            let mut data = vec![0; len as usize];
            if let Err(err) = store.read(extent, 0, &mut data) {
                let local_cell::BucketData { location, .. } = err
                    .downcast_ref::<local_cell::BucketData>()
                    .cloned()
                    .unwrap_or_else(|| panic!("{err:#}"));
                let read = bucket_cell::testing::read(&self.bucket, extent, &location);
                data = self.runtime.block_on(read).unwrap();
            }
            extents.insert(extent, data);
        }
        (extents, store.root().unwrap())
    }
}

#[test]
fn what_a_member_holds_recovers_on_another_host() {
    let runtime =
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build();
    let runtime = runtime.unwrap();
    let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
    let host = Host { runtime, bucket };
    let cell = DESCRIPTION.cell;
    host.runtime.block_on(bucket_cell::lifecycle::create(&*host.bucket, cell, 1)).unwrap();

    let dir = MemDir::new();
    LocalCell::make(&dir, DESCRIPTION, Some(URL), None).unwrap();
    let (store, uploader) = host.open(LocalCell::recover_unmade(dir).unwrap(), 1);
    store.made().unwrap();
    // What it made opening, uploaded, its segment closed: what no member holds is in the bucket.
    let base = store.last_commit().unwrap();
    uploader.wait_uploaded(base).unwrap();
    store.close_segment().unwrap();
    let member = Arc::new(Member::default());
    let links: Vec<Arc<dyn MemberLink>> = vec![member.clone(), Arc::new(Down)];
    let replicator = Replicator::start(1, base, store.spool().unwrap(), links, uploader.waker());
    store.replicate_to(replicator.clone());

    // Commits spanning segments, and extents stored on their own.
    let mut sequence = store.info().unwrap().next_sequence;
    for round in 0..12u64 {
        let len = if round % 3 == 0 { 40 << 10 } else { BLOCK * (1 + round as usize % 4) };
        let extent = ExtentId { cell: cell.id, sequence };
        sequence += 1;
        store.write(extent, 0, &common::noise(round, len)).unwrap();
        store.commit_ordered(Some(format!("root {round}").as_bytes())).unwrap();
    }
    store.flush().unwrap();
    let last = store.last_commit().unwrap();
    until("the commits replicated", || replicator.told(uploader.last_uploaded()) >= last);
    uploader.wait_replicated(last).unwrap();
    let expected = host.contents(&store);
    assert!(member.logs.lock().unwrap().keys().any(|log| log & 1 == 1), "own extents sent");
    // What it sends is spooled on the store's disk, not held in memory (TASK-537.3.3).
    assert!(replicator.spooled() > 0);

    // The host is lost: what the member holds, and the bucket, are all there is.
    drop(uploader);
    drop(store);
    drop(replicator);
    let logs: Vec<(u64, Vec<u8>)> = member.logs.lock().unwrap().clone().into_iter().collect();
    let elsewhere = MemDir::new();
    LocalCell::make(&elsewhere, DESCRIPTION, Some(URL), None).unwrap();
    // As a take reads it: the last commit the head names, which members' logs go on from, or
    // begin before, having dropped what it names later than it named it.
    let recovered = host.runtime.block_on(bucket_cell::recover(&*host.bucket, cell, None)).unwrap();
    replicate::restore(&elsewhere, cell, &logs, recovered.state.last_commit).unwrap();
    let (taken, uploader) = host.open(LocalCell::recover_unmade(elsewhere).unwrap(), 2);
    taken.made().unwrap();
    assert!(taken.last_commit().unwrap() >= last);
    assert_eq!(host.contents(&taken), expected);
    drop(uploader);
}
