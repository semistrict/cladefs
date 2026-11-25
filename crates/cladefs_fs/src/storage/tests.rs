// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: an image's cell kept in its bucket (backlog doc-14, TASK-165.6), forked and deleted
//! there, and its own bucket collected by this process among its commits, through its open cell
//! (board 8260).

use super::Storage;
use super::collect::quicken;
use super::model_tests::{BUSY, Files, holds};
use crate::{Filesystem, Owner};
use bucket_cell::object_bucket::RoutedBucket;
use bucket_cell::record::Made;
use cladefs_core::host_cell;
use cladefs_core::{OpenCell, VolatileSlice};
use futures::executor::block_on;
use rand::rngs::StdRng;
use rand::{RngExt as _, SeedableRng as _};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use storage_device::ExtentId;

const OWNER: Owner = Owner { uid: 1000, gid: 100 };
const SIZE: u64 = 64 << 20;
const MIB: usize = 1 << 20;

/// A formatted image at `path`, holding `file` with `contents`, closed.
async fn image_with(path: &Path, file: &[u8], contents: &[u8]) -> Storage {
    let fs = Filesystem::format(Storage::create(path, SIZE).await.unwrap()).await.unwrap();
    let (node, _) = fs.root().create(file, 0o644, OWNER).await.unwrap();
    node.write(0, contents).await.unwrap();
    fs.close().await.unwrap()
}

/// The bucket the cell directory at `path` names.
fn bucket_of(path: &Path) -> Arc<RoutedBucket> {
    host_fs::may_block(|| host_cell::bucket_at(path).unwrap().expect("a bucket"))
}

/// The extents `cell`'s state in `bucket` holds, by their lengths, those it inherited too,
/// and its root.
async fn kept(
    bucket: &RoutedBucket,
    cell: cladefs_core::CellId,
) -> (BTreeMap<ExtentId, u64>, Vec<u8>) {
    let recovered = async {
        let record = bucket_cell::head::get(bucket, cell).await?;
        bucket_cell::recover(bucket, cell, record.and_then(|record| record.origin())).await
    };
    let state = recovered.await.unwrap().state;
    let extents = state.extents.iter().map(|(extent, at)| (*extent, at.len)).collect();
    (extents, state.root.unwrap_or_default())
}

/// The extents `storage`'s cell holds, by their lengths, and its root.
fn held(storage: &Storage) -> (BTreeMap<ExtentId, u64>, Vec<u8>) {
    let local = storage.open_cell().local().clone();
    (local.extents().unwrap().into_iter().collect(), local.root().unwrap())
}

/// Writes `data` at 0 in `extent` of `cell`.
async fn write(cell: &OpenCell, extent: ExtentId, data: &[u8]) {
    cell.write(extent, 0, &[VolatileSlice::from(data)]).await.unwrap();
}

/// The bytes of the files of the bucket in the directory `dir`.
fn bucket_bytes(dir: &Path) -> u64 {
    let entries = std::fs::read_dir(dir).unwrap().map(|entry| entry.unwrap());
    (entries)
        .map(|entry| match entry.file_type().unwrap().is_dir() {
            true => bucket_bytes(&entry.path()),
            false => entry.metadata().unwrap().len(),
        })
        .sum()
}

/// TASK-165.6: an image keeps its cell in a file bucket of its own, beside its directory
/// (TASK-165.7), which its description names: closed, the cell holds what its cell directory
/// does.
#[tokio::test(flavor = "multi_thread")]
async fn images_keep_their_cells_in_a_file_bucket_of_their_own() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("image");
    let storage = block_on(image_with(&path, b"file", b"contents"));
    let own = dir.path().canonicalize().unwrap().join("image.bucket");
    let url = format!("file://{}/", own.display());
    assert_eq!(host_fs::may_block(|| local_cell::host_bucket(&path).unwrap()), Some(url));
    let cell = storage.open_cell().cell();
    let (extents, root) = kept(&bucket_of(&path), cell).await;
    assert!(!extents.is_empty() && !root.is_empty());
    assert_eq!((extents, root), held(&storage));
}

/// Forks closed `storage`'s cell to `path` and opens the fork's storage (board 10612).
pub(super) async fn fork_storage(storage: &Storage, path: &Path) -> Storage {
    let cell = storage.open_cell();
    let path = path.to_owned();
    let to = path.clone();
    super::on_bucket_runtime_on(cladefs_core::Host::process(), async move {
        cell.fork(&to, cell.cell().tenant).await.map(drop)
    })
    .await
    .unwrap();
    Storage::open(&path).await.unwrap()
}

/// TASK-165.6: an image's fork forks the image's cell in the image's bucket, where it keeps
/// its own: a cell of its own there, whose record names the image's as its parent, and which
/// holds what the fork's cell directory does, closed.
#[test]
fn forks_cell_in_their_images_bucket() {
    block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let (path, fork_path) = (dir.path().join("image"), dir.path().join("fork"));
        let storage = image_with(&path, b"file", b"original").await;
        let parent = storage.open_cell().cell();
        let fork = fork_storage(&storage, &fork_path).await;
        let child = fork.open_cell().cell();
        let bucket = bucket_of(&fork_path);
        assert!(Arc::ptr_eq(&bucket, &bucket_of(&path)), "the fork's bucket is another");
        let record = bucket_cell::head::get(&*bucket, child).await.unwrap().unwrap();
        let Made::Clone(origin) = record.made else { panic!("{record:?}") };
        assert_eq!(origin.parent, parent);

        let fs = Filesystem::mount(fork).await.unwrap();
        let (file, _) = fs.root().lookup(b"file").await.unwrap();
        file.write(0, b"modified").await.unwrap();
        let fork = fs.close().await.unwrap();
        assert_eq!(kept(&bucket, child).await, held(&fork));
        assert_ne!(kept(&bucket, child).await, kept(&bucket, parent).await);
    })
}

/// TASK-165.6: deleting an image, or a fork, deletes its cell in its bucket too, which the
/// collector then reclaims what only it held of.
#[test]
fn deleting_images_deletes_their_cells() {
    block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let (path, fork_path) = (dir.path().join("image"), dir.path().join("fork"));
        let storage = image_with(&path, b"file", b"original").await;
        let fork = fork_storage(&storage, &fork_path).await;
        let child = fork.open_cell().cell();
        fork.close().await;
        storage.close().await;
        let bucket = bucket_of(&path);
        let record = |cell| bucket_cell::head::get(&*bucket, cell);
        assert!(!record(child).await.unwrap().unwrap().deleted());
        Storage::delete(&fork_path).await.unwrap();
        assert!(record(child).await.unwrap().unwrap().deleted());
    })
}

/// TASK-165.6: an image's commit starts a pass of its bucket's collector, on a thread of its
/// own, which the image's forks share; the next commits, within a minute of it, start none.
#[test]
fn commits_start_the_collector() {
    block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image");
        let storage = Storage::create(&path, SIZE).await.unwrap();
        let collecting = storage.collecting().expect("its own bucket's").clone();
        assert_eq!(collecting.passes(), 0);
        let fs = Filesystem::format(storage).await.unwrap();
        assert_eq!(collecting.passes(), 1);
        let (node, _) = fs.root().create(b"file", 0o644, OWNER).await.unwrap();
        node.write(0, b"contents").await.unwrap();
        let storage = fs.close().await.unwrap();
        assert_eq!(collecting.passes(), 1);
        let fork = fork_storage(&storage, &dir.path().join("fork")).await;
        let shared = fork.collecting().unwrap();
        assert!(Arc::ptr_eq(&shared, &collecting), "the fork's collector is another");
    })
}

/// TASK-165.6: an image dropped has let go of its cell, which this process opens again at once,
/// as the extent daemon does a cell an import made: its uploader's task, held up by the
/// failpoint after it said it stopped, keeps nothing of it open. That task said so before it
/// let go of the cell, and the daemon found the import's cell still open (`EIO`, CI
/// 42119298585).
#[tokio::test(flavor = "multi_thread")]
async fn images_dropped_let_go_of_their_cells() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("image");
    let scenario = fail::FailScenario::setup();
    fail::cfg("upload::stopped", "sleep(500)").unwrap();
    drop(Storage::create(&path, SIZE).await.unwrap());
    let reopened = host_cell::open(&path, host_cell::keeping().unwrap())
        .await
        .map(|opened| opened.local.description().cell);
    scenario.teardown();
    let cell = reopened.map_err(|err| format!("{err:#}"));
    assert_eq!(cell.map(|_| ()), Ok(()));
}

/// TASK-165.6: an image kept in a host's bucket, as a cell the extent daemon made is, which
/// this process opens, has no collector here: the host's collects it.
#[tokio::test(flavor = "multi_thread")]
async fn images_in_a_hosts_bucket_are_not_collected_here() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let keeping = host_cell::keeping().unwrap();
    let made = host_cell::create_unmade(
        &path,
        SIZE,
        host_cell::CellOf::Named(bucket_cell::layout::test_cell(7)),
        bucket_cell::cell::Kind::Fxfs,
        Some("memory:///host"),
        keeping,
    )
    .await;
    host_fs::may_block(|| made.unwrap().made().unwrap());
    let storage = Storage::open(&path).await.unwrap();
    assert!(storage.collecting().is_none());
    let cell = storage.open_cell();
    write(&cell, ExtentId::new(7, 1), &[1; 4096]).await;
    drop(cell);
    // Committed by the device, uploaded, as closing a filesystem closes it: through the device,
    // whose commits would start a pass of a collector, had it one.
    storage.device.close().await.unwrap();
    storage.close().await;
    assert_eq!(
        kept(&bucket_of(&path), bucket_cell::layout::test_cell(7)).await.0,
        BTreeMap::from([(ExtentId::new(7, 1), 4096)])
    );
}

/// TASK-165.6: what an image freed, uploaded, and checkpointed as its uploader stops, a pass of
/// its bucket's collector reclaims; what it holds, the cell still holds.
#[tokio::test(flavor = "multi_thread")]
async fn the_collector_reclaims_what_images_freed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("image");
    let storage = Storage::create(&path, SIZE).await.unwrap();
    let cell = storage.open_cell();
    let id = cell.cell();
    let extent = |sequence| ExtentId::new(id.id, sequence);
    // Each stored on its own, in its bucket: 5 MiB, past the size for it.
    let data = cladefs_core::test_data::noise(1, 5 * MIB);
    for sequence in 0..4 {
        write(&cell, extent(sequence), &data).await;
    }
    // Uploaded before they are freed: a segment of the commits sealing and freeing them both
    // would hold none of them.
    cell.commit(None).await.unwrap();
    for sequence in 0..3 {
        cell.free(extent(sequence)).unwrap();
    }
    cell.commit(Some(b"root")).await.unwrap();
    drop(cell);
    storage.close().await;

    let own = dir.path().join("image.bucket");
    let before = bucket_bytes(&own);
    assert!(before >= 20 * MIB as u64, "{before} bytes");
    let storage = Storage::open(&path).await.unwrap();
    let report = storage.collecting().unwrap().pass_now();
    assert!(report.deleted >= 3, "{report:?}");
    let after = bucket_bytes(&own);
    assert!(after < 6 * MIB as u64, "{after} bytes");
    storage.close().await;
    let (extents, root) = kept(&bucket_of(&path), id).await;
    assert_eq!((extents, root), (BTreeMap::from([(extent(3), 5 * MIB as u64)]), b"root".into()));
}

/// `rounds` rounds, as `seed` says, of files of `fs`, which holds `files`, made, written over
/// and past their ends, and removed, each written synced, a commit; and each round's end, the
/// files checked. Files of 1 to 6 MiB of noise: those past 4 MiB in objects of their own, the
/// rest in segments, which their files' writing over half frees.
async fn churn(fs: &Filesystem, files: &mut Files, seed: u64, rounds: usize) {
    let mut rng = StdRng::seed_from_u64(seed);
    for round in 0..rounds {
        for step in 0..4 {
            let data = cladefs_core::test_data::noise(rng.random(), rng.random_range(1..=6) * MIB);
            let names: Vec<Vec<u8>> = files.keys().cloned().collect();
            match rng.random_range(0..3) {
                0 if !names.is_empty() => {
                    let name = &names[rng.random_range(0..names.len())];
                    fs.root().unlink(name).await.unwrap();
                    files.remove(name);
                    continue;
                }
                1 if !names.is_empty() => {
                    let name = &names[rng.random_range(0..names.len())];
                    let (node, _) = fs.root().lookup(name).await.unwrap();
                    let contents = files.get_mut(name).unwrap();
                    let at = rng.random_range(0..=contents.len() / MIB) * MIB;
                    node.write(at as u64, &data).await.unwrap();
                    contents.resize(contents.len().max(at + data.len()), 0);
                    contents[at..at + data.len()].copy_from_slice(&data);
                    node.fsync().await.unwrap();
                }
                _ => {
                    let name = format!("{seed}-{round}-{step}").into_bytes();
                    let (node, _) = fs.root().create(&name, 0o644, OWNER).await.unwrap();
                    node.write(0, &data).await.unwrap();
                    node.fsync().await.unwrap();
                    files.insert(name, data);
                }
            }
        }
        holds(fs, files, &format!("seed {seed}, round {round}")).await;
    }
}

/// TASK-291.3: an image and its fork, each written from a thread of its own, commit while
/// passes of their bucket's collector start after their commits, as often as one ends, and
/// delete at once: after each round each holds what was written to it, the fork what it
/// inherited too; and opened again, after passes alone that find nothing left to collect, the
/// same.
#[tokio::test(flavor = "multi_thread")]
async fn passes_run_among_an_image_and_its_forks_commits() {
    quicken(BUSY);
    let dir = tempfile::tempdir().unwrap();
    let (path, fork_path) = (dir.path().join("image"), dir.path().join("fork"));
    let storage = Storage::create(&path, SIZE).await.unwrap();
    let collecting = storage.collecting().unwrap().clone();
    let mut files = Files::new();
    let (fs, fork) = block_on(async {
        let fs = Filesystem::format(storage).await.unwrap();
        churn(&fs, &mut files, 1, 2).await;
        let storage = fs.close().await.unwrap();
        let fork = Filesystem::mount(fork_storage(&storage, &fork_path).await).await.unwrap();
        (Filesystem::mount(storage).await.unwrap(), fork)
    });
    let mut forked = files.clone();
    std::thread::scope(|scope| {
        scope.spawn(|| block_on(churn(&fork, &mut forked, 2, 6)));
        block_on(churn(&fs, &mut files, 3, 6));
    });
    block_on(async {
        drop(fork.close().await.unwrap());
        // Wait for any pre-close pass to finish so `fs.close()`'s commit always starts a pass
        // after `fork`'s clean-close checkpoint lands (board 11691: faster inline close can
        // otherwise finish before `fork.close()`'s pass exits).
        let _ = collecting.collected();
        drop(fs.close().await.unwrap());
    });
    // The passes among the commits deleted.
    let (passes, collected) = (collecting.passes(), collecting.collected());
    assert!(passes > 4, "{passes} passes");
    assert!(collected.deleted > 0, "{collected:?}, {passes} passes");
    drop(collecting);
    // Passes alone leave nothing the passes among the commits should have collected: those
    // delete at once, with no grace to leave a pass alone anything (b5664c64b), and its cell
    // unchanged since the last of them, a pass alone skips it (board 10508).
    let collecting = Storage::open(&path).await.unwrap().collecting().unwrap().clone();
    assert_eq!(collecting.collected_alone(), Vec::<String>::new(), "collected alone");
    drop(collecting);
    block_on(async {
        for (path, files) in [(&path, &files), (&fork_path, &forked)] {
            let fs = Filesystem::mount(Storage::open(path).await.unwrap()).await.unwrap();
            holds(&fs, files, &format!("{} opened again", path.display())).await;
            drop(fs.close().await.unwrap());
        }
    });
}

/// TASK-291.3: an image and its fork, closed and dropped while a pass of their bucket's
/// collector, held up, runs, leave their bucket whole: opened again, each holds what it did;
/// and the passes after, those their commits start as they are opened again, held up as they
/// are closed and dropped again, then one alone, delete what they freed, leaving nothing they
/// should have collected. The passes after, not the one alone: their commits' passes, as
/// often as one ends, may leave it nothing (TASK-328).
#[test]
fn images_dropped_as_their_pass_runs_leave_their_bucket_whole() {
    quicken(BUSY);
    let dir = tempfile::tempdir().unwrap();
    let (path, fork_path) = (dir.path().join("image"), dir.path().join("fork"));
    let scenario = fail::FailScenario::setup();
    let (mut files, mut forked) = (Files::new(), Files::new());
    let first = block_on(async {
        let storage = Storage::create(&path, SIZE).await.unwrap();
        let collecting = storage.collecting().unwrap().clone();
        let fs = Filesystem::format(storage).await.unwrap();
        churn(&fs, &mut files, 1, 2).await;
        let storage = fs.close().await.unwrap();
        let fork = Filesystem::mount(fork_storage(&storage, &fork_path).await).await.unwrap();
        let fs = Filesystem::mount(storage).await.unwrap();
        forked = files.clone();
        churn(&fork, &mut forked, 2, 1).await;
        // Passes held up from the next commit's on: both closed and dropped as one runs.
        fail::cfg("collect::pass", "sleep(500)").unwrap();
        churn(&fork, &mut forked, 3, 1).await;
        drop(fork.close().await.unwrap());
        drop(fs.close().await.unwrap());
        // Wait for the held-up pass and record what it deleted if both images closed and wrote
        // their clean-close checkpoints before `sleep(500)` elapsed (board 11691).
        collecting.collected()
    });
    // Opened again, they share a collector this holds, which counts what their commits'
    // passes, still held up, delete as they are closed and dropped again.
    let mut collecting = None;
    block_on(async {
        for (path, files) in [(&path, &files), (&fork_path, &forked)] {
            let storage = Storage::open(path).await.unwrap();
            collecting.get_or_insert_with(|| storage.collecting().unwrap().clone());
            let fs = Filesystem::mount(storage).await.unwrap();
            holds(&fs, files, &format!("{} opened again", path.display())).await;
            drop(fs.close().await.unwrap());
        }
    });
    scenario.teardown();
    let collecting = collecting.unwrap();
    let (started, alone) = (collecting.collected(), collecting.pass_now());
    assert!(
        first.deleted + started.deleted + alone.deleted > 0,
        "first {first:?}, started {started:?}, then alone {alone:?}"
    );
    assert_eq!(collecting.collected_alone(), Vec::<String>::new(), "collected alone");
    drop(collecting);
    block_on(async {
        let fs = Filesystem::mount(Storage::open(&fork_path).await.unwrap()).await.unwrap();
        holds(&fs, &forked, "the fork after the pass").await;
        drop(fs.close().await.unwrap());
    });
}

/// TASK-576 (board 2548): `Storage::take` on an existing cell directory applies the caller's
/// fencing token and opens through `host_cell::take`, dropping any diverged local log and
/// catching up to the bucket head.
#[tokio::test(flavor = "multi_thread")]
async fn storage_take_reattaches_existing_store_at_new_token_past_diverged_log() {
    let dir = tempfile::tempdir().unwrap();
    let bucket_dir = dir.path().join("bucket");
    std::fs::create_dir(&bucket_dir).unwrap();
    let url = format!("file://{}", bucket_dir.display());
    let first_path = dir.path().join("first");
    let second_path = dir.path().join("second");

    // Take a new cell at token 1, format and write a file, then close.
    let storage =
        Storage::take(&first_path, (host_cell::CellOf::New(0), SIZE), &url, Some(1)).await.unwrap();
    let cell = storage.cell().unwrap();
    let fs = Filesystem::format(storage).await.unwrap();
    let (node, _) = fs.root().create(b"one", 0o644, OWNER).await.unwrap();
    node.write(0, b"first-host").await.unwrap();
    node.fsync().await.unwrap();
    drop(fs.close().await.unwrap());

    // Another checkout takes the same cell at token 2 and writes a newer file.
    let second = Storage::take(&second_path, (host_cell::CellOf::Taken(cell), SIZE), &url, Some(2))
        .await
        .unwrap();
    let fs2 = Filesystem::mount(second).await.unwrap();
    let (node, _) = fs2.root().create(b"two", 0o644, OWNER).await.unwrap();
    node.write(0, b"second-host").await.unwrap();
    node.fsync().await.unwrap();
    drop(fs2.close().await.unwrap());

    // Re-take on `first_path` (which still exists at token 1) with token 3: must honor token 3
    // and see `two` from the bucket head.
    let retaken = Storage::take(&first_path, (host_cell::CellOf::Taken(cell), SIZE), &url, Some(3))
        .await
        .unwrap();
    let bucket = bucket_of(&first_path);
    let head = bucket_cell::head::get(&*bucket, cell).await.unwrap().unwrap();
    assert_eq!(head.token, 3);
    let fs3 = Filesystem::mount(retaken).await.unwrap();
    let (two, _) = fs3.root().lookup(b"two").await.unwrap();
    let mut buf = [0u8; 11];
    assert_eq!(two.read(0, &mut buf).await.unwrap(), 11);
    assert_eq!(&buf, b"second-host");
    drop(fs3.close().await.unwrap());
}
