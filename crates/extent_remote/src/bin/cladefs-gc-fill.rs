// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: fills a bucket of files with cells of branches holding garbage, for measuring the
//! collector's sweeps over many cells (TASK-395.5; `scripts/gc-sweep.sh`).
//!
//! ```sh
//! cladefs-gc-fill DIR TENANTS CELLS SEGMENTS [LARGEST] [--vm EXTENTS POINTS]
//! ```
//!
//! DIR, a directory, made if missing and empty, becomes a bucket of files (`file:///DIR/`) of
//! TENANTS tenants of CELLS cells each, each cell's object and one branch, its record, its lease,
//! SEGMENTS segments and a checkpoint covering them all: SEGMENTS + 5 objects a cell, but the first
//! cell of tenant 1, the largest, whose branch has LARGEST segments (SEGMENTS by default). Each
//! segment holds four extents of 1 KiB, and the next frees three of them, every other one all
//! four: so a pass deletes about half of each branch's segments, those freed whole, and compacts
//! the rest, a quarter live, once it compacts what is less than half live.
//!
//! With `--vm`, the largest cell is a running VM's, snapshotted many times (TASK-395.12): each of
//! its segments seals EXTENTS extents of 16 to 112 bytes and frees none, a disk filling, its checkpoints
//! naming every extent sealed before them, and its branch is forked to POINTS points, at segments
//! evenly apart, the last at its last, each a point of a cell of its own after the tenant's CELLS
//! (`1/CELLS+1` on), with the checkpoint at its fork point and its reference that recording it
//! writes: what a pass in full over the VM's cell recovers, a state a point.
//!
//! Each cell is written by a writer of its own into memory, then copied into DIR, its files
//! written without `fsync` (the measurement's bucket, not data to keep), many cells at once, one
//! a thread. It prints `cells N objects N bytes N largest-objects N largest-bytes N`: what it
//! wrote, and the largest cell's share.

use extent_remote::batch::{Batch, Commit};
use extent_remote::bucket::Bucket as _;
use extent_remote::layout::{Branch, Cell, Child};
use extent_remote::store::Store;
use extent_remote::{Writer, branch, cell};
use object_store::ObjectStore as _;
use object_store::local::LocalFileSystem;
use object_store::{PutOptions, PutPayload};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use storage_device::ExtentId;

/// The extents each segment of garbage holds, and their bytes.
const EXTENTS: u64 = 4;
const EXTENT_BYTES: usize = 1024;

/// The bytes of the extent `i` of a VM's segment `n` ([`Shape::Vm`]): 16 to 112, by a hash of
/// both, so that its checkpoints' entries, their offsets and lengths, compress as little as those
/// of a disk's extents of many lengths do, not as those of one length.
fn vm_extent_bytes(n: u64, i: u64) -> usize {
    let hash = (n << 32 | i).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 40;
    16 + (hash % 97) as usize
}

fn usage() -> String {
    "usage: cladefs-gc-fill DIR TENANTS CELLS SEGMENTS [LARGEST] [--vm EXTENTS POINTS]".into()
}

/// What a cell's branch is.
#[derive(Clone, Copy, Debug)]
enum Shape {
    /// `segments` segments of garbage.
    Garbage { segments: u64 },
    /// A VM's: `segments` segments of `extents` extents each, freeing none, forked to `points`
    /// points at segments evenly apart, each of a cell of the tenant's own, IDs from `first` on.
    Vm { segments: u64, extents: u64, points: u64, first: u64 },
}

/// What a cell's objects take.
#[derive(Clone, Copy, Debug, Default)]
struct Sum {
    objects: u64,
    bytes: u64,
}

fn main() -> Result<(), String> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let parse = |arg: Option<&String>| -> Result<u64, String> {
        arg.and_then(|arg| arg.parse().ok()).filter(|&count| count > 0).ok_or_else(usage)
    };
    let vm = match args.iter().position(|arg| arg == "--vm") {
        Some(at) if at + 3 <= args.len() => {
            let given: Vec<String> = args.drain(at..at + 3).collect();
            Some((parse(given.get(1))?, parse(given.get(2))?))
        }
        Some(_) => return Err(usage()),
        None => None,
    };
    let count = |at: usize| parse(args.get(at));
    if !(4..=5).contains(&args.len()) {
        return Err(usage());
    }
    let dir = std::path::PathBuf::from(&args[0]);
    let (tenants, cells, segments) = (count(1)?, count(2)?, count(3)?);
    let largest = if args.len() == 5 { count(4)? } else { segments };
    if vm.is_some_and(|(_, points)| points > largest) {
        return Err(format!("{largest} segments fork to {largest} points at most"));
    }
    std::fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    if std::fs::read_dir(&dir).map_err(|err| err.to_string())?.next().is_some() {
        return Err(format!("{}: not empty", dir.display()));
    }
    let files = LocalFileSystem::new_with_prefix(&dir).map_err(|err| err.to_string())?;
    let shapes: Vec<(Cell, Shape)> = (1..=tenants)
        .flat_map(|tenant| (1..=cells).map(move |id| Cell { tenant, id }))
        .map(|cell| {
            let shape = match (cell == (Cell { tenant: 1, id: 1 }), vm) {
                (false, _) => Shape::Garbage { segments },
                (true, None) => Shape::Garbage { segments: largest },
                (true, Some((extents, points))) => {
                    Shape::Vm { segments: largest, extents, points, first: cells + 1 }
                }
            };
            (cell, shape)
        })
        .collect();
    let points = vm.map_or(0, |(_, points)| points);
    let threads = std::thread::available_parallelism().map_or(4, |threads| threads.get());
    let next = AtomicUsize::new(0);
    let sums: Vec<Result<Vec<(Cell, Sum)>, String>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut sums = Vec::new();
                    while let Some(&(cell, shape)) =
                        shapes.get(next.fetch_add(1, Ordering::Relaxed))
                    {
                        let sum = futures::executor::block_on(fill(&files, cell, shape))
                            .map_err(|err| format!("cell {cell}: {err}"))?;
                        sums.push((cell, sum));
                    }
                    Ok(sums)
                })
            })
            .collect();
        workers.into_iter().map(|worker| worker.join().expect("a filler panicked")).collect()
    });
    let (mut total, mut biggest) = (Sum::default(), Sum::default());
    for (cell, sum) in sums.into_iter().collect::<Result<Vec<_>, _>>()?.into_iter().flatten() {
        total.objects += sum.objects;
        total.bytes += sum.bytes;
        if cell == (Cell { tenant: 1, id: 1 }) {
            biggest = sum;
        }
    }
    println!(
        "cells {} objects {} bytes {} largest-objects {} largest-bytes {}",
        shapes.len() as u64 + points,
        total.objects,
        total.bytes,
        biggest.objects,
        biggest.bytes
    );
    Ok(())
}

/// Writes `cell`, its branch as `shape` says, and a VM's points' cells, into memory, then into
/// `files`: what it took.
async fn fill(files: &LocalFileSystem, cell: Cell, shape: Shape) -> Result<Sum, String> {
    let memory = Arc::new(object_store::memory::InMemory::new());
    let bucket = Arc::new(Store::new(memory, ""));
    let failed = |err: extent_remote::Error| err.to_string();
    cell::create(&*bucket, cell, (10, 1), None).await.map_err(failed)?;
    let id = Branch { cell, id: 1 };
    branch::create(&*bucket, id, 0).await.map_err(failed)?;
    let writer = Writer::open(bucket.clone(), id, 10, 1).await.map_err(failed)?;
    let (segments, per_segment) = match shape {
        Shape::Garbage { segments } => (segments, EXTENTS),
        Shape::Vm { segments, extents, .. } => (segments, extents),
    };
    let mut held: Vec<ExtentId> = Vec::new();
    for n in 0..segments {
        let first = writer.chain().next_sequence();
        let extents: Vec<(ExtentId, Vec<u8>)> = (0..per_segment)
            .map(|i| {
                let len = match shape {
                    Shape::Garbage { .. } => EXTENT_BYTES,
                    Shape::Vm { .. } => vm_extent_bytes(n, i),
                };
                let bytes = (0..len).map(|at| ((at as u64 * 7 + n + i) % 251) as u8);
                (ExtentId::new(1, first + i), bytes.collect())
            })
            .collect();
        let batch = match shape {
            Shape::Garbage { .. } => {
                // The segment before freed whole if it was an even one, else but for its first
                // extent.
                let kept = usize::from(n % 2 == 0 && n > 0);
                let frees = held.split_off(kept.min(held.len()));
                held = extents.iter().map(|(extent, _)| *extent).collect();
                Batch::from(Commit { tag: n + 1, extents, frees, ..Commit::default() })
            }
            Shape::Vm { .. } => Batch::from(Commit { tag: n + 1, extents, ..Commit::default() }),
        };
        let segment = match point_at(shape, cell.tenant, n) {
            Some(point) => {
                cell::create(&*bucket, point.cell, (10, 1), None).await.map_err(failed)?;
                batch.fork(Child { branch: point, owner: 1, point: true }, &mut writer.chain())
            }
            None => batch.close(&mut writer.chain()),
        };
        let seq = writer.add_segment(segment.frame()).map_err(failed)?;
        writer.upload(seq).await.map_err(failed)?;
        writer.acknowledge().await.map_err(failed)?;
    }
    writer.checkpoint().await.map_err(failed)?;
    let mut sum = Sum::default();
    for meta in bucket.list("").await.map_err(|err| err.to_string())? {
        let object = bucket.get(&meta.key).await.map_err(|err| err.to_string())?;
        let path = object_store::path::Path::from(meta.key.as_str());
        let payload = PutPayload::from(object.bytes);
        files
            .put_opts(&path, payload, PutOptions::default())
            .await
            .map_err(|err| err.to_string())?;
        sum.objects += 1;
        sum.bytes += meta.size;
    }
    Ok(sum)
}

/// The point a VM's branch, as `shape` says, of a cell of `tenant`'s, is forked to at the end of
/// its segment `n`, counting from 0, if any: the `points` of its `segments` evenly apart, the last
/// at the last.
fn point_at(shape: Shape, tenant: u64, n: u64) -> Option<Branch> {
    let Shape::Vm { segments, points, first, .. } = shape else { return None };
    // The points the segments up to n's end have forked to, which steps up `points` times over
    // `segments`, the last time at the last.
    let (before, after) = (n * points / segments, (n + 1) * points / segments);
    (after > before).then(|| Branch { cell: Cell { tenant, id: first + after - 1 }, id: 1 })
}
