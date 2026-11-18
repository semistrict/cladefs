// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: what a collector's pass over a cell costs as its cell keeps more versions
//! (TASK-476, doc-18 Collection). A cell of EXTENTS extents, checkpointed every CHECKPOINT_EVERY
//! segments, as a writer does, takes N versions, each after a commit rewriting CHANGED extents: two
//! segments a version, the commit's and the cut's. A dry run over its cell, which keeps each
//! version's state as a pass does, is timed, and its requests of the bucket counted by kind. The
//! bucket is the process's memory, so that what is timed is the collector's own work, and the
//! requests are what a real bucket's round trips multiply. Say:
//!
//! ```sh
//! cargo run --release -p cladefs-examples --example versions-pass-bench -- [N...]
//! ```
//!
//! N defaults to 0 1 4 16 64 256 (`extent_bucket::layout::MAX_VERSIONS`).

use extent_bucket::collector::{Collector, Options};
use extent_bucket::keys::CachingKms;
use extent_bucket::layout::{Branch, Cell, NewVersion};
use extent_bucket::object_bucket::{ObjectBucket, Request, Stats};
use extent_bucket::{Commit, SegmentBuilder, Writer, branch};
use futures::executor::block_on;
use object_store::memory::InMemory;
use std::sync::Arc;
use std::time::{Duration, Instant};
use storage_device::ExtentId;

const EXTENTS: u64 = 4096;
const CHANGED: u64 = 16;
const EXTENT: usize = 64;
const CHECKPOINT_EVERY: u64 = 64;
const PASSES: usize = 5;
const CELL: Cell = Cell { tenant: 1, id: 1 };

fn main() {
    let args: Vec<usize> = std::env::args().skip(1).map(|arg| arg.parse().unwrap()).collect();
    let counts = match args.is_empty() {
        true => vec![0, 1, 4, 16, 64, 256],
        false => args,
    };
    println!(
        "a dry run over a cell whose cell of {EXTENTS} extents keeps N versions, each after a \
         commit of {CHANGED} extents, checkpointed every {CHECKPOINT_EVERY} segments; median of \
         {PASSES} passes"
    );
    println!(
        "{:>8} {:>8} {:>10} {:>8} {:>10} {:>8} {:>8} {:>12}",
        "versions", "segments", "pass ms", "gets", "get-range", "heads", "lists", "downloaded"
    );
    for versions in counts {
        bench(versions);
    }
}

/// Makes the cell with `versions` versions and prints what a pass over its cell costs.
fn bench(versions: usize) {
    let bucket = Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), ""));
    let segments = block_on(fill(&bucket, versions));
    let collector = Collector::new(Options { compact_below: 0.0, grace: 3_600_000 });
    let mut took = Vec::with_capacity(PASSES);
    let mut stats = None;
    for _ in 0..PASSES {
        let before = bucket.stats();
        let started = Instant::now();
        let dry = block_on(collector.dry_run_cell(&*bucket, CELL)).unwrap();
        took.push(started.elapsed());
        assert!(!dry.fenced);
        stats = Some(since(&bucket.stats(), &before));
    }
    took.sort();
    let stats = stats.unwrap();
    let count = |request: Request| stats.requests[request as usize];
    println!(
        "{versions:>8} {segments:>8} {:>10.2} {:>8} {:>10} {:>8} {:>8} {:>12}",
        ms(took[PASSES / 2]),
        count(Request::Get),
        count(Request::GetRange),
        count(Request::Head),
        count(Request::List),
        stats.downloaded,
    );
}

/// Writes the branch, branch 1 of `CELL`: its extents, then `versions` versions, each after a commit
/// rewriting `CHANGED` of them. Returns how many segments it wrote.
async fn fill(bucket: &Arc<ObjectBucket>, versions: usize) -> u64 {
    let branch = Branch { cell: CELL, id: 1 };
    branch::create(&**bucket, branch, 0).await.unwrap();
    // The tests' KMS: every branch is encrypted, and so read by the pass.
    let kms = CachingKms::new(Arc::new(extent_keys::testing::MemoryKms::new()));
    let writer = Writer::open_under(bucket.clone(), branch, (10, 0), &kms).await.unwrap();
    let data = vec![0x5a; EXTENT];
    let first = writer.chain().next_sequence();
    let mut live: Vec<ExtentId> =
        (0..EXTENTS).map(|n| ExtentId::new(branch.id, first + n)).collect();
    let extents = live.iter().map(|&extent| (extent, data.clone())).collect();
    let mut segments = 0;
    let mut add = async |segment: SegmentBuilder, cut: Option<NewVersion>| {
        let mut chain = writer.chain();
        let unframed = match cut {
            Some(cut) => segment.cut(cut, &mut chain),
            None => segment.close(&mut chain),
        };
        let at = writer.add_segment(unframed.frame(writer.keys().encrypting())).unwrap();
        writer.upload(at).await.unwrap();
        writer.acknowledge().await.unwrap();
        segments += 1;
        if segments % CHECKPOINT_EVERY == 0 {
            writer.checkpoint().await.unwrap();
        }
    };
    add(SegmentBuilder::from(Commit { tag: 1, extents, ..Commit::default() }), None).await;
    let mut next = first + EXTENTS;
    for version in 0..versions as u64 {
        // A rolling change: the oldest CHANGED extents rewritten as new ones.
        let frees: Vec<ExtentId> = live.drain(..CHANGED as usize).collect();
        let written: Vec<ExtentId> =
            (0..CHANGED).map(|n| ExtentId::new(branch.id, next + n)).collect();
        next += CHANGED;
        live.extend(&written);
        let extents = written.iter().map(|&extent| (extent, data.clone())).collect();
        let tag = 2 + 2 * version;
        let commit = Commit { tag, extents, frees, ..Commit::default() };
        add(SegmentBuilder::from(commit), None).await;
        let cut = NewVersion { id: 1 + version, keep_last: None };
        add(SegmentBuilder::from(Commit { tag: tag + 1, ..Commit::default() }), Some(cut)).await;
    }
    assert_eq!(writer.versions().await.len(), versions);
    segments
}

/// What `after` counted since `before`.
fn since(after: &Stats, before: &Stats) -> Stats {
    let minus = |a: &[u64; Request::ALL.len()], b: &[u64; Request::ALL.len()]| {
        std::array::from_fn(|n| a[n] - b[n])
    };
    Stats {
        requests: minus(&after.requests, &before.requests),
        request_ns: minus(&after.request_ns, &before.request_ns),
        uploaded: std::array::from_fn(|n| after.uploaded[n] - before.uploaded[n]),
        downloaded: after.downloaded - before.downloaded,
        failures: minus(&after.failures, &before.failures),
    }
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e3
}
