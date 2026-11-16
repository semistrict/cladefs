// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: fills a bucket of files with cells of branches holding garbage, for measuring the
//! collector's sweeps over many cells (TASK-395.5; `scripts/gc-sweep.sh`).
//!
//! ```sh
//! cladefs-gc-fill DIR TENANTS CELLS SEGMENTS [LARGEST] [--vm EXTENTS POINTS | --deleted | --clones EXTENTS]
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
//! With `--deleted`, every cell's branch is deleted once written, as its host deletes it
//! (TASK-443.4): cells nothing lives in, of which a pass keeps the cell's object, its branch's
//! record and head, and its own lease, for good.
//!
//! With `--clones`, the cells are sandboxes made and deleted (TASK-443.4). Each tenant's first cell
//! is a base, its branch of LARGEST segments of EXTENTS extents each, as a VM's, forked at its last
//! to a point, the tenant's second cell; and CELLS sandboxes after them (`T/3` on), each a cell
//! whose branch is a clone of the point, as a host makes one (`branch::fork_point`: its record,
//! then its reference under the base's cell), which wrote SEGMENTS segments of garbage and a
//! checkpoint and was deleted. The pass over each retires its reference, leaving its mark under the
//! base's cell for good: what a pass over a popular base's cell lists.
//!
//! Each cell is written by a writer of its own into memory, then copied into DIR, its files
//! written without `fsync` (the measurement's bucket, not data to keep), many cells at once, one
//! a thread; a sandbox's memory starting from its base's and its point's objects, which it reads.
//! Sealed, as every cell is (TASK-411.28): under the host's KMS, as every cladefs process chooses
//! it (`extent_remote::bucket_kms::from_env`: `CLADEFS_KMS`'s, or the bucket KMS over DIR given
//! `CLADEFS_MASTER_KEY`), which the collector compacts it under; none chosen, it refuses to start.
//! It prints `cells N objects N bytes N largest-objects N largest-bytes N`: what it wrote, and the
//! largest cell's share, a base's with its point's.

use extent_remote::batch::{Batch, Commit};
use extent_remote::bucket::Bucket as _;
use extent_remote::keys::{self, Kms};
use extent_remote::layout::{Branch, Cell, Child, Origin};
use extent_remote::store::Store;
use extent_remote::{Bytes, Writer, branch, cell};
use object_store::ObjectStore as _;
use object_store::local::LocalFileSystem;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::{PutOptions, PutPayload};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use storage_device::ExtentId;

/// The extents each segment of garbage holds, and their bytes.
const EXTENTS: u64 = 4;
const EXTENT_BYTES: usize = 1024;

/// Who writes the cells, and the token of its placement; and who deletes their branches.
const WRITER: (u64, u64) = (10, 1);
const DELETER: u64 = 11;

/// The bytes of the extent `i` of a VM's segment `n` ([`Shape::Vm`]): 16 to 112, by a hash of
/// both, so that its checkpoints' entries, their offsets and lengths, compress as little as those
/// of a disk's extents of many lengths do, not as those of one length.
fn vm_extent_bytes(n: u64, i: u64) -> usize {
    let hash = (n << 32 | i).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 40;
    16 + (hash % 97) as usize
}

fn usage() -> String {
    "usage: cladefs-gc-fill DIR TENANTS CELLS SEGMENTS [LARGEST] \
     [--vm EXTENTS POINTS | --deleted | --clones EXTENTS]"
        .into()
}

/// What the cells are, beside branches of garbage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Branches of garbage alone.
    Garbage,
    /// The same, each deleted once written (TASK-443.4).
    Deleted,
    /// The largest a running VM's, of segments of `extents` extents, forked to `points` points
    /// (TASK-395.12).
    Vm { extents: u64, points: u64 },
    /// Sandboxes made and deleted, clones of a point of their tenant's base, whose segments hold
    /// `extents` extents (TASK-443.4).
    Clones { extents: u64 },
}

/// What the command line asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Args {
    dir: PathBuf,
    tenants: u64,
    cells: u64,
    segments: u64,
    largest: u64,
    kind: Kind,
}

/// The command line `args`, after the program's name: the counts, each more than none, then the
/// kind's flag, if any.
fn parse(args: impl Iterator<Item = String>) -> Result<Args, String> {
    let args: Vec<String> = args.collect();
    let flags = args.iter().position(|arg| arg.starts_with("--")).unwrap_or(args.len());
    let (counts, flags) = args.split_at(flags);
    let count = |arg: &str| arg.parse().ok().filter(|&count: &u64| count > 0).ok_or_else(usage);
    let (dir, counts) = counts.split_first().ok_or_else(usage)?;
    let counts: Vec<u64> = counts.iter().map(|arg| count(arg)).collect::<Result<_, _>>()?;
    let (tenants, cells, segments, largest) = match counts[..] {
        [tenants, cells, segments] => (tenants, cells, segments, segments),
        [tenants, cells, segments, largest] => (tenants, cells, segments, largest),
        _ => return Err(usage()),
    };
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    let kind = match flags[..] {
        [] => Kind::Garbage,
        ["--deleted"] => Kind::Deleted,
        ["--vm", extents, points] => Kind::Vm { extents: count(extents)?, points: count(points)? },
        ["--clones", extents] => Kind::Clones { extents: count(extents)? },
        _ => return Err(usage()),
    };
    if let Kind::Vm { points, .. } = kind
        && points > largest
    {
        return Err(format!("{largest} segments fork to {largest} points at most"));
    }
    Ok(Args { dir: dir.into(), tenants, cells, segments, largest, kind })
}

/// What a cell's branch is.
#[derive(Clone, Copy, Debug)]
enum Shape {
    /// `segments` segments of garbage, and then deleted, if `deleted`.
    Garbage { segments: u64, deleted: bool },
    /// A VM's: `segments` segments of `extents` extents each, freeing none, forked to `points`
    /// points at segments evenly apart, each of a cell of the tenant's own, IDs from `first` on.
    Vm { segments: u64, extents: u64, points: u64, first: u64 },
}

/// What a cell's objects take.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Sum {
    objects: u64,
    bytes: u64,
}

impl std::ops::AddAssign for Sum {
    fn add_assign(&mut self, other: Self) {
        self.objects += other.objects;
        self.bytes += other.bytes;
    }
}

/// A base's point, as its clones find it ([`Kind::Clones`]): the point, the fork of the base it
/// is, and every object of the base's and the point's cells, in the order of their keys, which a
/// clone's bucket in memory starts from.
struct Point {
    branch: Branch,
    origin: Origin,
    objects: Vec<(String, Bytes)>,
}

impl Point {
    /// Whether the object at `key` is one of its own, which a clone of it never writes.
    fn holds(&self, key: &str) -> bool {
        self.objects.binary_search_by(|(held, _)| held.as_str().cmp(key)).is_ok()
    }
}

/// What a filling wrote.
struct Filled {
    cells: u64,
    total: Sum,
    /// The largest cell's share: tenant 1's first, a base's with its point's.
    largest: Sum,
    /// Each tenant's base's point, in the tenants' order ([`Kind::Clones`]).
    points: Vec<Point>,
}

fn main() -> Result<(), String> {
    let args = parse(std::env::args().skip(1))?;
    let filled = run(&args, &host_kms(&args.dir)?)?;
    println!(
        "cells {} objects {} bytes {} largest-objects {} largest-bytes {}",
        filled.cells,
        filled.total.objects,
        filled.total.bytes,
        filled.largest.objects,
        filled.largest.bytes
    );
    Ok(())
}

/// The host's KMS, as every cladefs process chooses it ([`extent_remote::bucket_kms::from_env`]):
/// the bucket KMS's over `dir`, made if missing, given master keys.
fn host_kms(dir: &std::path::Path) -> Result<Arc<Kms>, String> {
    std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let dir = dir.canonicalize().map_err(|err| format!("{}: {err}", dir.display()))?;
    let url = format!("file://{}/", dir.display());
    let kms = extent_remote::bucket_kms::from_env(|| Ok(Arc::new(Store::from_url(&url)?)))?;
    Ok(Kms::new(kms.shared()))
}

/// Fills `args`' directory, made if missing and empty, as `args` say, sealing its cells under
/// `kms`.
fn run(args: &Args, kms: &Arc<Kms>) -> Result<Filled, String> {
    let &Args { tenants, cells, segments, largest, kind, .. } = args;
    let dir = &args.dir;
    std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    if std::fs::read_dir(dir).map_err(|err| err.to_string())?.next().is_some() {
        return Err(format!("{}: not empty", dir.display()));
    }
    let files = LocalFileSystem::new_with_prefix(dir).map_err(|err| err.to_string())?;
    let first = Cell { tenant: 1, id: 1 };
    let mut filled =
        Filled { cells: 0, total: Sum::default(), largest: Sum::default(), points: Vec::new() };
    // The bases first, each with its point, which their tenants' sandboxes are clones of.
    if let Kind::Clones { extents } = kind {
        let base = Shape::Vm { segments: largest, extents, points: 1, first: 2 };
        for tenant in 1..=tenants {
            let cell = Cell { tenant, id: 1 };
            let (sum, point) = futures::executor::block_on(fill_base(&files, kms, cell, base))
                .map_err(|err| format!("cell {cell}: {err}"))?;
            filled.cells += 2;
            filled.total += sum;
            if cell == first {
                filled.largest = sum;
            }
            filled.points.push(point);
        }
    }
    let points = &filled.points;
    let shapes: Vec<(Cell, Shape, Option<&Point>)> = (1..=tenants)
        .flat_map(|tenant| (1..=cells).map(move |id| (tenant, id)))
        .map(|(tenant, id)| {
            let cell = Cell { tenant, id };
            match kind {
                Kind::Clones { .. } => {
                    let sandbox = Shape::Garbage { segments, deleted: true };
                    (Cell { tenant, id: id + 2 }, sandbox, Some(&points[tenant as usize - 1]))
                }
                Kind::Vm { extents, points } if cell == first => {
                    (cell, Shape::Vm { segments: largest, extents, points, first: cells + 1 }, None)
                }
                Kind::Garbage | Kind::Deleted | Kind::Vm { .. } => {
                    let segments = if cell == first { largest } else { segments };
                    (cell, Shape::Garbage { segments, deleted: kind == Kind::Deleted }, None)
                }
            }
        })
        .collect();
    let threads = std::thread::available_parallelism().map_or(4, |threads| threads.get());
    let next = AtomicUsize::new(0);
    let sums: Vec<Result<Vec<(Cell, Sum)>, String>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut sums = Vec::new();
                    while let Some(&(cell, shape, of)) =
                        shapes.get(next.fetch_add(1, Ordering::Relaxed))
                    {
                        let sum = futures::executor::block_on(fill(&files, kms, cell, shape, of))
                            .map_err(|err| format!("cell {cell}: {err}"))?;
                        sums.push((cell, sum));
                    }
                    Ok(sums)
                })
            })
            .collect();
        workers.into_iter().map(|worker| worker.join().expect("a filler panicked")).collect()
    });
    let (mut total, mut biggest) = (Sum::default(), None);
    for (cell, sum) in sums.into_iter().collect::<Result<Vec<_>, _>>()?.into_iter().flatten() {
        total += sum;
        if cell == first {
            biggest = Some(sum);
        }
    }
    filled.cells += shapes.len() as u64;
    if let Kind::Vm { points, .. } = kind {
        filled.cells += points;
    }
    filled.total += total;
    if let Some(biggest) = biggest {
        filled.largest = biggest;
    }
    Ok(filled)
}

/// A cell written into memory ([`write`]): its bucket, and the last point its branch was forked
/// to, if any, with the fork.
struct Written {
    bucket: Arc<Store>,
    point: Option<(Branch, Origin)>,
}

/// Writes `cell`, its branch as `shape` says, a clone of `of` if given, and a VM's points' cells,
/// into memory, then into `files`: what it took, `of`'s own objects apart.
async fn fill(
    files: &LocalFileSystem,
    kms: &Arc<Kms>,
    cell: Cell,
    shape: Shape,
    of: Option<&Point>,
) -> Result<Sum, String> {
    let written = write(kms, cell, shape, of).await?;
    Ok(copy(&written.bucket, files, of).await?.0)
}

/// Writes the base `cell`, its branch as `shape` says, forked to a point, into memory, then into
/// `files`: what it took, and its point, for its clones.
async fn fill_base(
    files: &LocalFileSystem,
    kms: &Arc<Kms>,
    cell: Cell,
    shape: Shape,
) -> Result<(Sum, Point), String> {
    let written = write(kms, cell, shape, None).await?;
    let (branch, origin) = written.point.ok_or("a base forked to no point")?;
    let (sum, objects) = copy(&written.bucket, files, None).await?;
    Ok((sum, Point { branch, origin, objects }))
}

/// Writes `cell`, its branch as `shape` says, and a VM's points' cells, into a bucket in memory:
/// one starting from `of`'s objects, the branch a clone of that point, if given.
async fn write(
    kms: &Arc<Kms>,
    cell: Cell,
    shape: Shape,
    of: Option<&Point>,
) -> Result<Written, String> {
    let memory = Arc::new(InMemory::new());
    for (key, bytes) in of.into_iter().flat_map(|point| &point.objects) {
        let (path, payload) = (Path::from(key.as_str()), PutPayload::from(bytes.clone()));
        memory
            .put_opts(&path, payload, PutOptions::default())
            .await
            .map_err(|err| err.to_string())?;
    }
    let bucket = Arc::new(Store::new(memory, ""));
    let failed = |err: extent_remote::Error| err.to_string();
    make_cell(&bucket, kms, cell).await.map_err(failed)?;
    let id = Branch { cell, id: 1 };
    match of {
        None => branch::create(&*bucket, id, 0).await.map_err(failed)?,
        Some(point) => branch::fork_point(&*bucket, id, point.branch, point.origin, WRITER.0)
            .await
            .map_err(failed)?,
    }
    let writer = Writer::open_under(bucket.clone(), id, WRITER, kms).await.map_err(failed)?;
    let (segments, per_segment) = match shape {
        Shape::Garbage { segments, .. } => (segments, EXTENTS),
        Shape::Vm { segments, extents, .. } => (segments, extents),
    };
    let mut held: Vec<ExtentId> = Vec::new();
    let mut forked = None;
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
        let point = point_at(shape, cell.tenant, n);
        let segment = match point {
            Some(point) => {
                make_cell(&bucket, kms, point.cell).await.map_err(failed)?;
                batch.fork(Child { branch: point, owner: 1, point: true }, &mut writer.chain())
            }
            None => batch.close(&mut writer.chain()),
        };
        let seq = writer.add_segment(segment.frame(writer.keys().sealing())).map_err(failed)?;
        writer.upload(seq).await.map_err(failed)?;
        writer.acknowledge().await.map_err(failed)?;
        if let Some(point) = point {
            forked = Some((point, Origin { parent: id, at: seq }));
        }
    }
    writer.checkpoint().await.map_err(failed)?;
    drop(writer);
    if let Shape::Garbage { deleted: true, .. } = shape {
        // As a host deletes a branch (`extent_store::upload::delete_branch`).
        branch::delete(&*bucket, id, DELETER, WRITER.1, kms).await.map_err(failed)?;
        cell::forget(&*bucket, cell, id.id).await.map_err(failed)?;
    }
    Ok(Written { bucket, point: forked })
}

/// Makes `cell` in `bucket` with a KEK of `kms`, the host's: its branch's record is given its DEK
/// by its writer.
async fn make_cell(bucket: &Store, kms: &Arc<Kms>, cell: Cell) -> Result<(), extent_remote::Error> {
    let (_, kek) = keys::new_cell_kek(kms, cell.tenant).await?;
    cell::create(bucket, cell, WRITER, kek).await.map(drop)
}

/// Copies every object of `bucket`, one in memory, into `files`, but those of `but`, which the
/// bucket started from: what the copied take, and each with its bytes, in the order of their keys.
async fn copy(
    bucket: &Store,
    files: &LocalFileSystem,
    but: Option<&Point>,
) -> Result<(Sum, Vec<(String, Bytes)>), String> {
    let mut sum = Sum::default();
    let mut objects = Vec::new();
    for meta in bucket.list("").await.map_err(|err| err.to_string())? {
        if but.is_some_and(|point| point.holds(&meta.key)) {
            continue;
        }
        let object = bucket.get(&meta.key).await.map_err(|err| err.to_string())?;
        let bytes = Bytes::from(object.bytes);
        let path = Path::from(meta.key.as_str());
        let payload = PutPayload::from(bytes.clone());
        files
            .put_opts(&path, payload, PutOptions::default())
            .await
            .map_err(|err| err.to_string())?;
        sum += Sum { objects: 1, bytes: meta.size };
        objects.push((meta.key, bytes));
    }
    Ok((sum, objects))
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

#[cfg(test)]
mod tests {
    use super::*;
    use extent_remote::collector::Options;
    use extent_remote::layout::{self, Kind as Key};
    use extent_remote::store::Request;
    use extent_remote::sweep::{Mode, Sweep, Swept};

    fn parsed(args: &[&str]) -> Result<Args, String> {
        parse(args.iter().map(|arg| (*arg).to_owned()))
    }

    fn args(counts: [u64; 4], kind: Kind) -> Args {
        let [tenants, cells, segments, largest] = counts;
        Args { dir: "d".into(), tenants, cells, segments, largest, kind }
    }

    #[test]
    fn command_lines_name_the_counts_then_a_kind() {
        assert_eq!(parsed(&["d", "2", "3", "4"]), Ok(args([2, 3, 4, 4], Kind::Garbage)));
        assert_eq!(parsed(&["d", "2", "3", "4", "9"]), Ok(args([2, 3, 4, 9], Kind::Garbage)));
        assert_eq!(
            parsed(&["d", "2", "3", "4", "--deleted"]),
            Ok(args([2, 3, 4, 4], Kind::Deleted))
        );
        assert_eq!(
            parsed(&["d", "1", "1", "1", "160", "--vm", "1024", "100"]),
            Ok(args([1, 1, 1, 160], Kind::Vm { extents: 1024, points: 100 }))
        );
        assert_eq!(
            parsed(&["d", "1", "3", "4", "--clones", "16"]),
            Ok(args([1, 3, 4, 4], Kind::Clones { extents: 16 }))
        );
        assert_eq!(
            parsed(&["d", "1", "1", "1", "2", "--vm", "8", "3"]),
            Err("2 segments fork to 2 points at most".into())
        );
        let refused: [&[&str]; 9] = [
            &[],
            &["d", "1", "1"],
            &["d", "1", "1", "1", "1", "1"],
            &["d", "0", "1", "1"],
            &["d", "1", "1", "1", "--vm", "8"],
            &["d", "1", "1", "1", "--clones"],
            &["d", "1", "1", "1", "--clones", "0"],
            &["d", "1", "1", "1", "--deleted", "--clones", "4"],
            &["d", "1", "1", "1", "--other"],
        ];
        for refused in refused {
            assert_eq!(parsed(refused), Err(usage()), "{refused:?}");
        }
    }

    /// The tests' KMS (`extent_keys::testing::KnownKms`), whose KEKs every test and sweep shares.
    fn known() -> extent_keys::kms::Shared {
        Arc::new(extent_keys::testing::KnownKms)
    }

    /// A host's KMS over the tests' ([`known`]).
    fn kms() -> Arc<Kms> {
        Kms::new(known())
    }

    /// A sweep of every cell of `bucket`, a pass at a time, deleting what is free at once.
    fn sweep(bucket: &Arc<Store>) -> Swept {
        let sweep = Sweep {
            options: Options { compact_below: 0.5, grace: 0 },
            mode: Mode::Delete,
            at_once: 1,
            lease: 600_000,
            slowest: 0,
            spawned: false,
            kms: Some(known()),
        };
        futures::executor::block_on(sweep.run(bucket, |_| {}))
    }

    /// The requests `swept` made: GETs, updates, LISTs, delimiter LISTs, and all of them.
    fn requests(swept: &Swept) -> [u64; 5] {
        let of = |request| swept.requests.of(request);
        let kinds = [Request::Get, Request::Update, Request::List, Request::ListDelimited];
        let [gets, updates, lists, delimited] = kinds.map(of);
        [gets, updates, lists, delimited, swept.requests.total()]
    }

    /// The keys of the objects of `bucket` under `prefix`, in order.
    fn keys(bucket: &Store, prefix: &str) -> Vec<String> {
        let listed = futures::executor::block_on(bucket.list(prefix)).unwrap();
        listed.into_iter().map(|meta| meta.key).collect()
    }

    /// TASK-443.4: what `--deleted` and `--clones` fill, as sweeps find it. A dead cell keeps four
    /// objects for good, its object, its branch's record and head, and its collector's lease, and
    /// costs every sweep after the second five requests: two GETs and two updates of its lease, and
    /// its listing. A sandbox's retirement leaves its mark under its base's cell, whose pass skips
    /// only once two sweeps went by without a sandbox made or retired.
    #[test]
    fn dead_cells_keep_four_objects_and_cost_a_sweep_five_requests() {
        let directory = tempfile::tempdir().unwrap();
        let url = |dir: &std::path::Path| format!("file://{}/", dir.display());
        let left = |cell: Cell| {
            let branch = Branch { cell, id: 1 };
            let mut keys = [
                layout::cell(cell),
                layout::gc_lease(cell),
                layout::head(branch),
                layout::record(branch),
            ];
            keys.sort();
            keys.to_vec()
        };

        // Roots, deleted: none skipped until its own deletes are a sweep behind it.
        let deleted =
            Args { dir: directory.path().join("deleted"), ..args([1, 3, 2, 2], Kind::Deleted) };
        assert_eq!(run(&deleted, &kms()).unwrap().cells, 3);
        let bucket = Arc::new(Store::from_url(&url(&deleted.dir)).unwrap());
        let swept = [sweep(&bucket), sweep(&bucket), sweep(&bucket)];
        assert_eq!(swept.each_ref().map(|swept| swept.report.skipped), [0, 0, 3]);
        assert_eq!(requests(&swept[2]), [6, 6, 3, 2, 17]);
        for id in 1..=3 {
            let cell = Cell { tenant: 1, id };
            assert_eq!(keys(&bucket, &cell.prefix()), left(cell));
        }

        // Sandboxes of a point of a base, deleted: the base's cell 1, the point's 2, theirs after.
        let clones = Args {
            dir: directory.path().join("clones"),
            ..args([1, 3, 2, 2], Kind::Clones { extents: 4 })
        };
        let filled = run(&clones, &kms()).unwrap();
        assert_eq!(filled.cells, 5);
        let bucket = Arc::new(Store::from_url(&url(&clones.dir)).unwrap());
        let swept = [sweep(&bucket), sweep(&bucket), sweep(&bucket)];
        assert_eq!(swept.each_ref().map(|swept| swept.report.skipped), [0, 1, 5]);
        // Each sandbox's second pass, in full for its own deletes, retires its reference again,
        // finding its mark there and the reference gone.
        assert_eq!(swept.each_ref().map(|swept| swept.report.retired), [3, 3, 0]);
        assert_eq!(requests(&swept[2]), [10, 10, 5, 2, 27]);
        for id in 3..=5 {
            let cell = Cell { tenant: 1, id };
            assert_eq!(keys(&bucket, &cell.prefix()), left(cell));
        }
        let base = Cell { tenant: 1, id: 1 };
        let of_base = |bucket: &Store| {
            let keys = keys(bucket, &base.prefix());
            let count = |kind: fn(&Key<'_>) -> bool| {
                keys.iter().filter(|key| kind(&layout::kind(key))).count()
            };
            [
                count(|kind| matches!(kind, Key::Reference { .. })),
                count(|kind| matches!(kind, Key::Retired { .. })),
            ]
        };
        // The point's reference, and a mark a sandbox.
        assert_eq!(of_base(&bucket), [1, 3]);

        // A sandbox more, made and deleted: the base's pass goes in full for its reference, then
        // for its retirement, the sandbox's for its own deletes.
        let files = LocalFileSystem::new_with_prefix(&clones.dir).unwrap();
        let sandbox = Shape::Garbage { segments: 2, deleted: true };
        let cell = Cell { tenant: 1, id: 6 };
        let sum = futures::executor::block_on(fill(
            &files,
            &kms(),
            cell,
            sandbox,
            Some(&filled.points[0]),
        ));
        // Its cell's object, its record, its head, two segments, a checkpoint, and its reference.
        assert_eq!(sum.unwrap().objects, 7);
        assert_eq!(of_base(&bucket), [2, 3]);
        let swept = [sweep(&bucket), sweep(&bucket), sweep(&bucket)];
        assert_eq!(swept.each_ref().map(|swept| swept.report.skipped), [4, 4, 6]);
        assert_eq!(swept.each_ref().map(|swept| swept.report.retired), [1, 1, 0]);
        assert_eq!(requests(&swept[2]), [12, 12, 6, 2, 32]);
        assert_eq!(keys(&bucket, &cell.prefix()), left(cell));
        assert_eq!(of_base(&bucket), [1, 4]);
    }
}
