// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: fills a bucket of files with cells of cells holding garbage, for measuring the
//! collector's sweeps over many cells (TASK-395.5; `scripts/gc-sweep.sh`).
//!
//! ```sh
//! cladefs-gc-fill DIR TENANTS CELLS SEGMENTS [LARGEST] [--vm EXTENTS VERSIONS [TAIL] | --deleted | --clones EXTENTS]
//! ```
//!
//! DIR, a directory, made if missing and empty, becomes a bucket of files (`file:///DIR/`) of
//! TENANTS tenants of CELLS cells each, each cell's object, its record, its head,
//! SEGMENTS segments and a checkpoint covering them all: SEGMENTS + 5 objects a cell, but the first
//! cell of tenant 1, the largest, whose cell has LARGEST segments (SEGMENTS by default). Each
//! segment holds four extents of 1 KiB, and the next frees three of them, every other one all
//! four: so a pass deletes about half of each cell's segments, those freed whole, and keeps the
//! rest, a quarter live, which writers would relocate out of (doc-27; the fill has none).
//!
//! With `--vm`, the largest cell is a running VM's, snapshotted many times (TASK-395.12): each of
//! its segments seals EXTENTS extents of 16 to 112 bytes and frees none, a disk filling, its checkpoints
//! naming every extent sealed before them, and its cell has VERSIONS versions, at segments evenly
//! apart, the last at its last: what a pass in full over the VM's cell recovers, a state a version.
//! With TAIL, its checkpoint is written TAIL segments before its last, not after it: a VM writing
//! since its last checkpoint, whose pass reads that checkpoint's parts, not its tail alone (board
//! 14523).
//!
//! With `--deleted`, every cell is deleted once written, as its host deletes it
//! (TASK-443.4): cells nothing lives in, which a pass removes whole (TASK-455).
//!
//! With `--clones`, the cells are sandboxes made and deleted (TASK-443.4). Each tenant's first cell
//! is a base, its cell of LARGEST segments of EXTENTS extents each, as a VM's, with a version at
//! its last, and the checkpoint there that its first restore into another cell writes; and CELLS
//! sandboxes after it (`T/2` on), each a cell whose cell is a clone of the version, as a host makes
//! one (`lifecycle::fork_version`: its record, then its reference under the base's cell), which wrote
//! SEGMENTS segments of garbage and a
//! checkpoint and was deleted. The pass over each retires its reference, unmarked, and removes its
//! cell (TASK-455): what a pass over a popular base's cell lists is its live clones' alone.
//!
//! Every cell but a VM's or a base's, of ID 1, is of an ID an
//! allocator gave, as hosts give them (`bucket_cell::objectid`).
//!
//! Each cell is written by a writer of its own into memory, then copied into DIR, its files
//! written without `fsync` (the measurement's bucket, not data to keep), many cells at once, one
//! a thread; a sandbox's memory starting from its base's objects, which it reads.
//! Encrypted, as every cell is (TASK-411.28): under the host's KMS, as every cladefs process chooses
//! it (`bucket_cell::bucket_kms::from_env`: `CLADEFS_KMS`'s, or the bucket KMS over DIR given
//! `CLADEFS_MASTER_KEY`); none chosen, it refuses to start.
//! It prints `cells N objects N bytes N largest-objects N largest-bytes N`: what it wrote, and the
//! largest cell's share.

// Every unsafe block says why it is sound, other threads included (TASK-444.4).
#![deny(clippy::undocumented_unsafe_blocks)]

use bucket_cell::batch::{Commit, SegmentBuilder};
use bucket_cell::bucket::Bucket as _;
use bucket_cell::keys::{self, CachingKms};
use bucket_cell::layout::{CellId, NewVersion, Origin};
use bucket_cell::object_bucket::ObjectBucket;
use bucket_cell::{Bytes, Writer, lifecycle};
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

/// Who writes the cells, and the token of its placement; and who deletes their cells.
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
     [--vm EXTENTS POINTS [TAIL] | --deleted | --clones EXTENTS]"
        .into()
}

/// What the cells are, beside cells of garbage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Cells of garbage alone.
    Garbage,
    /// The same, each deleted once written (TASK-443.4).
    Deleted,
    /// The largest a running VM's, of segments of `extents` extents, with `versions` versions
    /// (TASK-395.12), its checkpoint `tail` segments before its last.
    Vm { extents: u64, versions: u64, tail: u64 },
    /// Sandboxes made and deleted, clones of a version of their tenant's base, whose segments hold
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
    // Not a number, or none: the usage says what one is.
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
        ["--vm", extents, versions] => {
            Kind::Vm { extents: count(extents)?, versions: count(versions)?, tail: 0 }
        }
        ["--vm", extents, versions, tail] => {
            Kind::Vm { extents: count(extents)?, versions: count(versions)?, tail: count(tail)? }
        }
        ["--clones", extents] => Kind::Clones { extents: count(extents)? },
        _ => return Err(usage()),
    };
    if let Kind::Vm { versions, .. } = kind
        && versions > largest
    {
        return Err(format!("{largest} segments end in {largest} versions at most"));
    }
    if let Kind::Vm { tail, .. } = kind
        && tail >= largest
    {
        return Err(format!("{largest} segments have {} after a checkpoint at most", largest - 1));
    }
    Ok(Args { dir: dir.into(), tenants, cells, segments, largest, kind })
}

/// What a cell is.
#[derive(Clone, Copy, Debug)]
enum Shape {
    /// `segments` segments of garbage, and then deleted, if `deleted`.
    Garbage { segments: u64, deleted: bool },
    /// A VM's: `segments` segments of `extents` extents each, freeing none, with `versions`
    /// versions at segments evenly apart, the last at the last, its checkpoint `tail` segments
    /// before its last.
    Vm { segments: u64, extents: u64, versions: u64, tail: u64 },
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

/// A base's version, as its clones find it ([`Kind::Clones`]): its ID, its origin, and every
/// object of the base's cell, in the order of their keys, which a clone's bucket in memory starts
/// from.
struct Version {
    id: u64,
    origin: Origin,
    objects: Vec<(String, Bytes)>,
}

impl Version {
    /// Whether the object at `key` is one of its own, which a clone of it never writes.
    fn holds(&self, key: &str) -> bool {
        self.objects.binary_search_by(|(held, _)| held.as_str().cmp(key)).is_ok()
    }
}

/// What a filling wrote.
struct Filled {
    cells: u64,
    total: Sum,
    /// The largest cell's share: tenant 1's first.
    largest: Sum,
    /// Each tenant's base's version, in the tenants' order ([`Kind::Clones`]).
    bases: Vec<Version>,
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

/// The host's KMS, as every cladefs process chooses it ([`bucket_cell::bucket_kms::from_env`]):
/// the bucket KMS's over `dir`, made if missing, given master keys.
fn host_kms(dir: &std::path::Path) -> Result<Arc<CachingKms>, String> {
    std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let dir = dir.canonicalize().map_err(|err| format!("{}: {err}", dir.display()))?;
    let url = format!("file://{}/", dir.display());
    let kms = bucket_cell::bucket_kms::from_env(|| Ok(Arc::new(ObjectBucket::from_url(&url)?)))?;
    Ok(CachingKms::new(kms.shared()))
}

/// Fills `args`' directory, made if missing and empty, as `args` say, encrypting its cells under
/// `kms`.
fn run(args: &Args, kms: &Arc<CachingKms>) -> Result<Filled, String> {
    let &Args { tenants, cells, segments, largest, kind, .. } = args;
    let dir = &args.dir;
    std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    if std::fs::read_dir(dir).map_err(|err| err.to_string())?.next().is_some() {
        return Err(format!("{}: not empty", dir.display()));
    }
    let files = LocalFileSystem::new_with_prefix(dir).map_err(|err| err.to_string())?;
    let first = CellId { tenant: 1, id: 1 };
    let mut filled =
        Filled { cells: 0, total: Sum::default(), largest: Sum::default(), bases: Vec::new() };
    // The bases first, each with its version, which their tenants' sandboxes are clones of.
    if let Kind::Clones { extents } = kind {
        let base = Shape::Vm { segments: largest, extents, versions: 1, tail: 0 };
        for tenant in 1..=tenants {
            let cell = CellId { tenant, id: 1 };
            let (sum, version) = futures::executor::block_on(fill_base(&files, kms, cell, base))
                .map_err(|err| format!("cell {cell}: {err}"))?;
            filled.cells += 1;
            filled.total += sum;
            if cell == first {
                filled.largest = sum;
            }
            filled.bases.push(version);
        }
    }
    let bases = &filled.bases;
    let shapes: Vec<(CellId, Shape, Option<&Version>)> = (1..=tenants)
        .flat_map(|tenant| (1..=cells).map(move |id| (tenant, id)))
        .map(|(tenant, id)| {
            let cell = CellId { tenant, id };
            match kind {
                Kind::Clones { .. } => {
                    let sandbox = Shape::Garbage { segments, deleted: true };
                    (CellId { tenant, id: id + 1 }, sandbox, Some(&bases[tenant as usize - 1]))
                }
                Kind::Vm { extents, versions, tail } if cell == first => {
                    (cell, Shape::Vm { segments: largest, extents, versions, tail }, None)
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
    let sums: Vec<Result<Vec<(CellId, Sum)>, String>> = std::thread::scope(|scope| {
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
    filled.total += total;
    if let Some(biggest) = biggest {
        filled.largest = biggest;
    }
    Ok(filled)
}

/// A cell written into memory ([`write`]): its bucket, and its cell's last version, if any, by
/// its ID and origin.
struct Written {
    bucket: Arc<ObjectBucket>,
    version: Option<(u64, Origin)>,
}

/// Writes `cell`, its cell as `shape` says, a clone of `of` if given, into memory, then into
/// `files`: what it took, `of`'s own objects apart.
async fn fill(
    files: &LocalFileSystem,
    kms: &Arc<CachingKms>,
    cell: CellId,
    shape: Shape,
    of: Option<&Version>,
) -> Result<Sum, String> {
    let written = write(kms, cell, shape, of).await?;
    Ok(copy(&written.bucket, files, of).await?.0)
}

/// Writes the base `cell`, its cell as `shape` says, with a version, into memory, and the
/// checkpoint at the version that its first restore into another cell writes, then into `files`:
/// what it took, and its version, for its clones.
async fn fill_base(
    files: &LocalFileSystem,
    kms: &Arc<CachingKms>,
    cell: CellId,
    shape: Shape,
) -> Result<(Sum, Version), String> {
    let written = write(kms, cell, shape, None).await?;
    let (id, origin) = written.version.ok_or("a base of no version")?;
    let keys = keys::Parents::Kms(kms, None);
    bucket_cell::recovery::checkpoint_at(&*written.bucket, origin.parent, origin.at, keys)
        .await
        .map_err(|err| err.to_string())?;
    let (sum, objects) = copy(&written.bucket, files, None).await?;
    Ok((sum, Version { id, origin, objects }))
}

/// Writes `cell`, its cell as `shape` says, into a bucket in memory: one starting from `of`'s
/// objects, the cell a clone of that version, if given.
async fn write(
    kms: &Arc<CachingKms>,
    cell: CellId,
    shape: Shape,
    of: Option<&Version>,
) -> Result<Written, String> {
    let memory = Arc::new(InMemory::new());
    for (key, bytes) in of.into_iter().flat_map(|version| &version.objects) {
        let (path, payload) = (Path::from(key.as_str()), PutPayload::from(bytes.clone()));
        memory
            .put_opts(&path, payload, PutOptions::default())
            .await
            .map_err(|err| err.to_string())?;
    }
    let bucket = Arc::new(ObjectBucket::new(memory, ""));
    let failed = |err: bucket_cell::Error| err.to_string();
    let id = cell;
    match of {
        None => drop(lifecycle::create(&*bucket, id, 0).await.map_err(failed)?),
        Some(version) => {
            let keys = keys::Parents::Kms(kms, None);
            lifecycle::fork_version(&*bucket, id, version.id, version.origin, WRITER.0, keys)
                .await
                .map_err(failed)?
        }
    }
    let writer = Writer::open_under(bucket.clone(), id, WRITER, kms).await.map_err(failed)?;
    let (segments, per_segment, tail) = match shape {
        Shape::Garbage { segments, .. } => (segments, EXTENTS, 0),
        Shape::Vm { segments, extents, tail, .. } => (segments, extents, tail),
    };
    let mut held: Vec<ExtentId> = Vec::new();
    let mut versioned = None;
    for n in 0..segments {
        let first = writer.chain().next_sequence();
        let extents: Vec<(ExtentId, Vec<u8>)> = (0..per_segment)
            .map(|i| {
                let len = match shape {
                    Shape::Garbage { .. } => EXTENT_BYTES,
                    Shape::Vm { .. } => vm_extent_bytes(n, i),
                };
                let bytes = (0..len).map(|at| ((at as u64 * 7 + n + i) % 251) as u8);
                (ExtentId::new(id.id, first + i), bytes.collect())
            })
            .collect();
        let batch = match shape {
            Shape::Garbage { .. } => {
                // The segment before freed whole if it was an even one, else but for its first
                // extent.
                let kept = usize::from(n % 2 == 0 && n > 0);
                let frees = held.split_off(kept.min(held.len()));
                held = extents.iter().map(|(extent, _)| *extent).collect();
                SegmentBuilder::from(Commit { tag: n + 1, extents, frees, ..Commit::default() })
            }
            Shape::Vm { .. } => {
                SegmentBuilder::from(Commit { tag: n + 1, extents, ..Commit::default() })
            }
        };
        let version = version_at(shape, n);
        let closed = match version {
            Some(id) => batch.cut(NewVersion { id, keep_last: None }, &mut writer.chain()),
            None => batch.close(&mut writer.chain()),
        };
        let segment =
            writer.add_segment(closed.frame(writer.keys().encrypting())).map_err(failed)?;
        writer.upload(segment).await.map_err(failed)?;
        writer.acknowledge().await.map_err(failed)?;
        if let Some(version) = version {
            versioned = Some((version, Origin { parent: id, at: segment, epoch: writer.epoch() }));
        }
        if n + 1 + tail == segments && tail > 0 {
            writer.checkpoint().await.map_err(failed)?;
        }
    }
    if tail == 0 {
        writer.checkpoint().await.map_err(failed)?;
    }
    drop(writer);
    if let Shape::Garbage { deleted: true, .. } = shape {
        // As a host deletes a cell (`local_cell::upload::delete_cell`).
        lifecycle::delete(&*bucket, id, DELETER, WRITER.1).await.map_err(failed)?;
    }
    Ok(Written { bucket, version: versioned })
}

/// Copies every object of `bucket`, one in memory, into `files`, but those of `but`, which the
/// bucket started from: what the copied take, and each with its bytes, in the order of their keys.
async fn copy(
    bucket: &ObjectBucket,
    files: &LocalFileSystem,
    but: Option<&Version>,
) -> Result<(Sum, Vec<(String, Bytes)>), String> {
    let mut sum = Sum::default();
    let mut objects = Vec::new();
    for meta in bucket.list("").await.map_err(|err| err.to_string())? {
        if but.is_some_and(|version| version.holds(&meta.key)) {
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

/// The ID of the version a VM's cell, as `shape` says, has at the end of its segment `n`,
/// counting from 0, if any: the `versions` of its `segments` evenly apart, the last at the last,
/// numbered from 1.
fn version_at(shape: Shape, n: u64) -> Option<u64> {
    let Shape::Vm { segments, versions, .. } = shape else { return None };
    // The versions the segments up to n's end have, which steps up `versions` times over
    // `segments`, the last time at the last.
    let (before, after) = (n * versions / segments, (n + 1) * versions / segments);
    (after > before).then_some(after)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bucket_cell::layout::{self, Kind as Key};
    use bucket_cell::object_bucket::Request;
    use bucket_cell::sweep::{Mode, Sweep, Swept};

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
            Ok(args([1, 1, 1, 160], Kind::Vm { extents: 1024, versions: 100, tail: 0 }))
        );
        assert_eq!(
            parsed(&["d", "1", "1", "1", "160", "--vm", "1024", "100", "20"]),
            Ok(args([1, 1, 1, 160], Kind::Vm { extents: 1024, versions: 100, tail: 20 }))
        );
        assert_eq!(
            parsed(&["d", "1", "1", "1", "160", "--vm", "1024", "100", "160"]),
            Err("160 segments have 159 after a checkpoint at most".into())
        );
        assert_eq!(
            parsed(&["d", "1", "3", "4", "--clones", "16"]),
            Ok(args([1, 3, 4, 4], Kind::Clones { extents: 16 }))
        );
        assert_eq!(
            parsed(&["d", "1", "1", "1", "2", "--vm", "8", "3"]),
            Err("2 segments end in 2 versions at most".into())
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
    fn kms() -> Arc<CachingKms> {
        CachingKms::new(known())
    }

    /// A sweep of every cell of `bucket`, a pass at a time, deleting what is free at once.
    fn sweep(bucket: &Arc<ObjectBucket>) -> Swept {
        let sweep =
            Sweep { mode: Mode::Delete, at_once: 1, lease: 600_000, slowest: 0, spawned: false };
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
    fn keys(bucket: &ObjectBucket, prefix: &str) -> Vec<String> {
        let listed = futures::executor::block_on(bucket.list(prefix)).unwrap();
        listed.into_iter().map(|meta| meta.key).collect()
    }

    /// TASK-443.4, TASK-455: what `--deleted` and `--clones` fill, as sweeps find it. A dead cell,
    /// its cell a root or a clone of an ID an allocator gave, is removed whole by its first pass,
    /// and costs the sweeps after it nothing: they list no cell of it. A sandbox's reference is
    /// retired unmarked, leaving its base's cell nothing of it: that cell's pass lists no
    /// reference, and skips once a sweep went by without a sandbox made or retired, a look
    /// costing the lease's read and the listing.
    #[test]
    fn dead_cells_are_removed_whole_and_cost_later_sweeps_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let url = |dir: &std::path::Path| format!("file://{}/", dir.display());

        // Roots, deleted: each removed by its first pass; the sweeps after list no tenant.
        let deleted =
            Args { dir: directory.path().join("deleted"), ..args([1, 3, 2, 2], Kind::Deleted) };
        assert_eq!(run(&deleted, &kms()).unwrap().cells, 3);
        let bucket = Arc::new(ObjectBucket::from_url(&url(&deleted.dir)).unwrap());
        let swept = [sweep(&bucket), sweep(&bucket), sweep(&bucket)];
        assert_eq!(swept.each_ref().map(|swept| swept.report.removed), [3, 0, 0]);
        assert_eq!(swept.each_ref().map(|swept| swept.visited), [3, 0, 0]);
        assert_eq!(requests(&swept[1]), [0, 0, 0, 1, 1]);
        assert_eq!(requests(&swept[2]), [0, 0, 0, 1, 1]);
        assert_eq!(keys(&bucket, "tenants/"), Vec::<String>::new());

        // Sandboxes of a version of a base, deleted: the base's cell 1, theirs after.
        let clones = Args {
            dir: directory.path().join("clones"),
            ..args([1, 3, 2, 2], Kind::Clones { extents: 4 })
        };
        let filled = run(&clones, &kms()).unwrap();
        assert_eq!(filled.cells, 4);
        let bucket = Arc::new(ObjectBucket::from_url(&url(&clones.dir)).unwrap());
        let swept = [sweep(&bucket), sweep(&bucket), sweep(&bucket)];
        assert_eq!(swept.each_ref().map(|swept| swept.report.removed), [3, 0, 0]);
        // The base's second pass in full, for its sandboxes' retirements after its first.
        assert_eq!(swept.each_ref().map(|swept| swept.report.skipped), [0, 0, 1]);
        assert_eq!(swept.each_ref().map(|swept| swept.report.retired), [3, 0, 0]);
        // A look at the base's cell: its lease read, its listing.
        assert_eq!(requests(&swept[2]), [1, 0, 1, 2, 4]);
        for id in 2..=4 {
            let cell = CellId { tenant: 1, id };
            assert_eq!(keys(&bucket, &cell.prefix()), Vec::<String>::new());
        }
        let base = CellId { tenant: 1, id: 1 };
        let of_base = |bucket: &ObjectBucket| {
            let keys = keys(bucket, &base.prefix());
            let count =
                |kind: fn(&Key) -> bool| keys.iter().filter(|key| kind(&layout::kind(key))).count();
            [
                count(|kind| matches!(kind, Key::Reference { .. })),
                count(|kind| matches!(kind, Key::Retired { .. })),
            ]
        };
        // No reference, no mark of a sandbox.
        assert_eq!(of_base(&bucket), [0, 0]);

        // A sandbox more, made and deleted: the base's pass goes in full for its reference, then
        // for its retirement; the sandbox's removes it.
        let files = LocalFileSystem::new_with_prefix(&clones.dir).unwrap();
        let sandbox = Shape::Garbage { segments: 2, deleted: true };
        let cell = CellId { tenant: 1, id: 5 };
        let sum = futures::executor::block_on(fill(
            &files,
            &kms(),
            cell,
            sandbox,
            Some(&filled.bases[0]),
        ));
        // Its head, two segments, a checkpoint, and its reference.
        assert_eq!(sum.unwrap().objects, 5);
        assert_eq!(of_base(&bucket), [1, 0]);
        let swept = [sweep(&bucket), sweep(&bucket), sweep(&bucket)];
        assert_eq!(swept.each_ref().map(|swept| swept.report.removed), [1, 0, 0]);
        assert_eq!(swept.each_ref().map(|swept| swept.report.skipped), [0, 0, 1]);
        assert_eq!(swept.each_ref().map(|swept| swept.report.retired), [1, 0, 0]);
        assert_eq!(requests(&swept[2]), [1, 0, 1, 2, 4]);
        assert_eq!(keys(&bucket, &cell.prefix()), Vec::<String>::new());
        assert_eq!(of_base(&bucket), [0, 0]);
    }
}
