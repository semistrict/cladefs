// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What cladefs's tests share of waiting for a writer's checkpoints (TASK-339): a store's uploader
//! writes a checkpoint every so many segments ([`local_cell::upload::CHECKPOINT_EVERY`]) once it
//! has uploaded them, after the commits that made them returned. A collector's pass that lists the
//! bucket before the checkpoint lands reads the branch's state from before it, and keeps what the
//! checkpoint frees: tests that collected at once raced it, and lost on a loaded host (main's macOS
//! CI, `forks_read_what_they_inherited_compacted_twice`).

use crate::OpenCell;
use bucket_cell::object_bucket::ObjectBucket;
use std::time::{Duration, Instant};

/// The newest checkpoint of `branch` that `bucket` lists, by its sequence number.
pub(crate) fn newest(bucket: &ObjectBucket, branch: crate::CellId) -> Option<u64> {
    let prefix = bucket_cell::layout::checkpoints(branch);
    let listed = futures::executor::block_on(bucket_cell::Bucket::list(bucket, &prefix));
    (listed.unwrap().iter())
        .filter_map(|meta| match bucket_cell::layout::kind(&meta.key) {
            bucket_cell::layout::Kind::Checkpoint { segment, .. } => Some(segment),
            _ => None,
        })
        .max()
}

/// Runs `commits`, which commit `store` into a checkpoint, and waits for its writer's checkpoint
/// past the newest before them to be in `bucket`, 60 s at most.
pub(crate) fn after(store: &OpenCell, bucket: &ObjectBucket, commits: impl FnOnce()) {
    let before = newest(bucket, store.branch());
    commits();
    let deadline = Instant::now() + Duration::from_secs(60);
    while newest(bucket, store.branch()) <= before {
        assert!(
            Instant::now() < deadline,
            "branch {}'s writer checkpointed nothing past {before:?} within 60 s",
            store.branch()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Commits `store` [`local_cell::upload::CHECKPOINT_EVERY`] times, a segment each, and waits for
/// the checkpoint they make to be in `bucket` ([`after`]).
pub(crate) fn checkpointed(store: &OpenCell, bucket: &ObjectBucket) {
    after(store, bucket, || {
        for _ in 0..local_cell::upload::CHECKPOINT_EVERY {
            store.commit(None).unwrap();
        }
    });
}
