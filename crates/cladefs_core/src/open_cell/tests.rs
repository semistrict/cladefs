// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;
use crate::cache;
use crate::dax::{Access, SecondStage};
use crate::pages::{PhysLocation, page_size};
use futures::executor::block_on;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

/// An error of a read of `extent`, whose data is in the bucket.
fn bucket_data(extent: ExtentId) -> local_cell::BucketData {
    let location = extent_frame::segment::Location { key: "k".to_owned(), offset: 0, len: 1 };
    local_cell::BucketData { extent, location }
}

/// Each of the open cell's errors has its errno, as doc-4 has them, in context or not; any other
/// is EIO (TASK-160.12).
#[test]
fn each_open_cell_error_has_its_errno() {
    let extent = ExtentId { cell: 1, sequence: 2 };
    let cases: [(anyhow::Error, Errno); 13] = [
        // cladefs: one said already, a halt's, as it was (the review of 8cf431e95).
        (anyhow::Error::new(Errno::EACCES), Errno::EACCES),
        (NoExtent(extent).into(), Errno::ENOENT),
        (SealedExtent(extent).into(), Errno::EPERM),
        (SpentExtent(extent).into(), Errno::EPERM),
        (OtherCell(extent).into(), Errno::EPERM),
        (NoSpace.into(), Errno::ENOSPC),
        (TooSmall { capacity: 1, held: 2 }.into(), Errno::ENOSPC),
        (Unaligned { offset: 1, len: 2 }.into(), Errno::EINVAL),
        (Unsectored { offset: 1, len: 2 }.into(), Errno::EINVAL),
        (TooLong { offset: 1, len: 2 }.into(), Errno::EINVAL),
        (local_cell::LastSequence(ExtentId { cell: 1, sequence: u64::MAX }).into(), Errno::EINVAL),
        (bucket_data(extent).into(), Errno::EIO),
        (anyhow::anyhow!("the disk"), Errno::EIO),
    ];
    for (error, expected) in cases {
        let said = format!("{error:#}");
        assert_eq!(errno(error.context("in context")), expected, "{said}");
    }
}

/// Each of the uploader's failures has its errno, as doc-4 and doc-7 have them: a cell this
/// writer lost is EROFS, not EIO (TASK-160.12).
#[test]
fn each_uploader_failure_has_its_errno() {
    use bucket_cell::Error;
    use local_cell::upload::Failure;
    let cases = [
        (Failure::Bucket(Error::Fenced), Errno::EROFS),
        (Failure::Bucket(Error::Deleted), Errno::EROFS),
        (Failure::Bucket(Error::Stale), Errno::ESTALE),
        (Failure::Bucket(Error::Diverged), Errno::ESTALE),
        (Failure::Bucket(Error::NoCell), Errno::ENOENT),
        (Failure::VersionDropped, Errno::ENOENT),
        (Failure::Bucket(Error::IdTaken(bucket_cell::layout::test_cell(3))), Errno::EEXIST),
        (
            Failure::Bucket(Error::Format { cell: bucket_cell::layout::test_cell(3), format: 99 }),
            Errno::ENOTSUP,
        ),
        (Failure::Bucket(Error::Lost("a segment".into())), Errno::EIO),
        (Failure::Bucket(Error::Corrupt("a footer".into())), Errno::EIO),
        (Failure::Bucket(Error::Read("a file".into())), Errno::EIO),
        (Failure::msg("the uploader stopped"), Errno::EIO),
    ];
    for (failure, expected) in cases {
        let said = failure.to_string();
        assert_eq!(upload_errno(failure), expected, "{said}");
    }
}

const BLOCK: usize = OpenCell::BLOCK_SIZE as usize;

/// A new open cell of `capacity` bytes, in a directory of its own, kept in a memory bucket of
/// the directory's, which the open cells made in it share.
async fn cell(capacity: u64) -> (OpenCell, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    (made(&dir.path().join("store"), capacity).await, dir)
}

/// A new cell directory at `path`, of `capacity` bytes, kept in the memory bucket of the
/// directory `path` is in ([`cell`]).
async fn made(path: &Path, capacity: u64) -> OpenCell {
    let named = path.parent().unwrap().file_name().unwrap().to_string_lossy().into_owned();
    Opening::here()
        .unwrap()
        .create_kept(path, capacity, CellOf::New(0), &format!("memory:///{named}"))
        .await
        .unwrap()
}

/// A new open cell, as [`cell`] makes, whose uploader uploads nothing until a commit waits for
/// it or the hour is up: what commits made local alone ([`committed_here`]) seal
/// stays in its files, rather than leave for the bucket (TASK-165.9).
async fn held(capacity: u64) -> (OpenCell, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let named = dir.path().file_name().unwrap().to_string_lossy().into_owned();
    let keeping = local_cell::upload::Options {
        interval: std::time::Duration::from_secs(3600),
        ..Default::default()
    };
    let url = format!("memory:///{named}");
    let cell = host_cell::new_alone();
    let held = Opening::here()
        .unwrap()
        .with_keeping(keeping)
        .create_kept(dir.path().join("store"), capacity, CellOf::Named(cell), &url)
        .await;
    (held.unwrap(), dir)
}

/// Commits what `cell` wrote, local alone: its uploader not waited for.
async fn committed_here(cell: &OpenCell) {
    cell.commit_ordered(None).await.unwrap();
    cell.flush().await.unwrap();
}

fn id(cell: &OpenCell, sequence: u64) -> ExtentId {
    ExtentId { cell: cell.cell().id, sequence }
}

/// `len` bytes of `byte` plus their offset modulo 251.
fn pattern(byte: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| byte ^ (i % 251) as u8).collect()
}

async fn write(cell: &OpenCell, extent: ExtentId, offset: u64, data: &[u8]) -> Result<()> {
    // In two buffers, as a request's descriptors may hold it.
    let (first, second) = data.split_at(data.len() / 2);
    cell.write(extent, offset, &[VolatileSlice::from(first), VolatileSlice::from(second)]).await
}

async fn read(cell: &OpenCell, extent: ExtentId, offset: u64, len: usize) -> Result<Vec<u8>> {
    let mut data = vec![0; len];
    let (first, second) = data.split_at_mut(BLOCK.min(len));
    cell.read(extent, offset, &mut [VolatileSliceMut::from(first), VolatileSliceMut::from(second)])
        .await?;
    Ok(data)
}

/// What [`holding_follows_the_extents`] does to an open cell: writes `blocks` blocks at block
/// `block` of its cell's extent `sequence`, frees the extent `Free` picks of those it holds,
/// its own or inherited, commits, opens it again, or forks it and goes on with the fork.
#[derive(Clone, Debug)]
enum CellChange {
    Write { sequence: u64, block: u64, blocks: usize },
    Free(proptest::sample::Index),
    Commit,
    Reopen,
    Fork,
}

fn cell_change() -> impl proptest::strategy::Strategy<Value = CellChange> {
    use proptest::prelude::*;
    prop_oneof![
        4 => (1..12u64, 0..3u64, 1..3usize)
            .prop_map(|(sequence, block, blocks)| CellChange::Write { sequence, block, blocks }),
        3 => any::<proptest::sample::Index>().prop_map(CellChange::Free),
        2 => Just(CellChange::Commit),
        1 => Just(CellChange::Reopen),
        1 => Just(CellChange::Fork),
    ]
}

/// Panics unless what `cell` says it holds ([`OpenCell::holding`]), the counts kept as its
/// extents come and go, is what its extents, all walked, hold: how many, own and inherited, and
/// their bytes (TASK-454).
fn holding_is_recounted(cell: &OpenCell, after: &[CellChange]) {
    let cell_id = cell.cell().id;
    let held = cell.local.extents().unwrap();
    let own: Vec<_> = held.iter().filter(|(extent, _)| extent.cell == cell_id).collect();
    let bytes = |held: &mut dyn Iterator<Item = &(ExtentId, u64)>| -> u64 {
        held.map(|(_, len)| len).sum()
    };
    let (own_bytes, all_bytes) = (bytes(&mut own.iter().copied()), bytes(&mut held.iter()));
    let recounted = CellHolding {
        bytes: Whose { own: own_bytes, inherited: all_bytes - own_bytes },
        extents: Whose { own: own.len() as u64, inherited: (held.len() - own.len()) as u64 },
    };
    assert_eq!(cell.holding().unwrap(), recounted, "after {after:?}");
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(32))]

    /// TASK-454: an open cell's counts of its extents, own and inherited, and of their bytes,
    /// which it keeps on as they are written, freed, committed, opened again and forked, are
    /// what its extents hold, recounted after each change.
    #[test]
    fn holding_follows_the_extents(changes in proptest::collection::vec(cell_change(), 1..24)) {
        // Each case on a runtime of its own: proptest runs them on the test's thread.
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let mut path = dir.path().join("store");
            let mut cell = made(&path, 1 << 30).await;
            for (done, change) in changes.iter().enumerate() {
                match change {
                    CellChange::Write { sequence, block, blocks } => {
                        let data = pattern(*sequence as u8, blocks * BLOCK);
                        let at = block * BLOCK as u64;
                        // Refused if sealed or spent: the counts stand.
                        match write(&cell, id(&cell, *sequence), at, &data).await {
                            Ok(()) | Err(Errno::EPERM) => {}
                            Err(err) => panic!("writing: {err:?}"),
                        }
                    }
                    CellChange::Free(pick) => {
                        let held = cell.local.extents().unwrap();
                        if !held.is_empty() {
                            let (extent, _) = held[pick.index(held.len())];
                            cell.free(extent).unwrap();
                        }
                    }
                    CellChange::Commit => cell.commit(None).await.unwrap(),
                    CellChange::Reopen => {
                        drop(cell);
                        cell = OpenCell::open(&path).await.unwrap();
                    }
                    CellChange::Fork => {
                        cell.commit(None).await.unwrap();
                        let fork = dir.path().join(format!("fork-{done}"));
                        let child = crate::host_cell::fork_child(cell.cell());
                        cell.fork_to(&fork, child).await.unwrap();
                        drop(cell);
                        (path, cell) = (fork.clone(), OpenCell::open(&fork).await.unwrap());
                    }
                }
                holding_is_recounted(&cell, &changes[..=done]);
            }
        });
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn serves_extents() {
    let (cell, dir) = cell(1 << 20).await;
    let (a, b) = (id(&cell, 1), id(&cell, 2));
    write(&cell, a, 0, &pattern(1, 2 * BLOCK)).await.unwrap();
    write(&cell, b, BLOCK as u64, &pattern(2, BLOCK)).await.unwrap();
    assert_eq!(read(&cell, a, 0, 2 * BLOCK).await, Ok(pattern(1, 2 * BLOCK)));
    assert_eq!(
        read(&cell, b, 0, 2 * BLOCK).await,
        Ok([vec![0; BLOCK], pattern(2, BLOCK)].concat())
    );
    cell.commit(Some(b"root")).await.unwrap();
    write(&cell, id(&cell, 3), 0, &pattern(3, BLOCK)).await.unwrap();
    cell.free(b).unwrap();

    // As doc-4 has them: an extent not held, a sealed one, another cell's, unaligned I/O, and
    // too much.
    assert_eq!(read(&cell, b, 0, BLOCK).await, Err(Errno::ENOENT));
    assert_eq!(write(&cell, a, 2 * BLOCK as u64, &[0; BLOCK]).await, Err(Errno::EPERM));
    let other = ExtentId { cell: cell.cell().id + 1, sequence: 1 };
    assert_eq!(write(&cell, other, 0, &[0; BLOCK]).await, Err(Errno::EPERM));
    assert_eq!(write(&cell, id(&cell, 4), 1, &[0; BLOCK]).await, Err(Errno::EINVAL));
    assert_eq!(write(&cell, id(&cell, 4), 0, &vec![0; 1 << 20]).await, Err(Errno::ENOSPC));
    let info = cell.info().unwrap();
    assert_eq!((info.used, info.next_sequence), (3 * BLOCK as u64, 4));

    // What wasn't committed is gone once the open cell is opened again.
    let cell_id = cell.cell();
    drop(cell);
    let cell = OpenCell::open(dir.path().join("store")).await.unwrap();
    assert_eq!(cell.cell(), cell_id);
    assert_eq!(cell.root(), Ok(b"root".to_vec()));
    assert_eq!(read(&cell, b, BLOCK as u64, BLOCK).await, Ok(pattern(2, BLOCK)));
    assert_eq!(read(&cell, id(&cell, 3), 0, BLOCK).await, Err(Errno::ENOENT));
}

/// doc-24, step 9: an open cell its holders let go of ([`release`]) closes as its last
/// reference goes, and not before: let go of by one while another holds it, it writes and
/// commits still; let go of by the last, it closes, what it committed kept, and opens again
/// holding it.
#[tokio::test(flavor = "multi_thread")]
async fn cells_close_once_their_last_reference_is_released() {
    let (cell, dir) = cell(1 << 20).await;
    let (a, b) = (id(&cell, 1), id(&cell, 2));
    write(&cell, a, 0, &pattern(1, BLOCK)).await.unwrap();
    cell.commit(None).await.unwrap();
    let held = Arc::new(cell);
    release(held.clone()).await;
    write(&held, b, 0, &pattern(2, BLOCK)).await.unwrap();
    held.commit_ordered(None).await.unwrap();
    let cell_id = held.cell();
    release(held).await;
    let cell = OpenCell::open(dir.path().join("store")).await.unwrap();
    assert_eq!(cell.cell(), cell_id);
    assert_eq!(read(&cell, a, 0, BLOCK).await, Ok(pattern(1, BLOCK)));
    assert_eq!(read(&cell, b, 0, BLOCK).await, Ok(pattern(2, BLOCK)));
    cell.close().await;
}

/// TASK-411.3: an open cell of a tenant whose KEK the host's KMS holds (`CLADEFS_KMS`, set here
/// as the test's process's): its cell made encrypted, its cell given a DEK, and no byte of what
/// it wrote in its bucket; read back, and opened again. TASK-411.4: its fork
/// into a new cell keeps the DEK it inherits rewrapped under its own cell's KEK, and reads what
/// it inherits. TASK-411.28: a tenant with no KEK there gets no open cell, refused as destroyed
/// (`EACCES`), and the open cell, the common tenant's, forks into no cell of its (`EACCES`),
/// nothing made.
#[tokio::test(flavor = "multi_thread")]
async fn encrypted_cells_serve_reopen_and_fork_into_encrypted_cells_alone() {
    let keys = tempfile::tempdir().unwrap();
    let kms = extent_keys::kms::LocalKms::open(keys.path().join("kms")).unwrap();
    kms.create(5).unwrap();
    kms.create(host_cell::DEFAULT_TENANT).unwrap();
    crate::bucket::set_kms(Arc::new(kms)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let cell = Opening::here().unwrap().create(&path, 1 << 20, CellOf::New(5)).await.unwrap();
    assert!(!cell.local.keys().is_empty());
    let cell = Arc::new(cell);
    let a = id(&cell, 1);
    let data = crate::test_data::noise(1, 2 * BLOCK);
    write(&cell, a, 0, &data).await.unwrap();
    cell.commit(Some(b"root")).await.unwrap();
    assert_eq!(read(&cell, a, 0, 2 * BLOCK).await, Ok(data.clone()));
    // Nothing of it in the bucket: every file under the open cell's own, as on the disk, what
    // the open cell's page cache holds written there first (TASK-433).
    let objects = host_fs::may_block(|| {
        local_cell::pages::shared().write_back_all().unwrap();
        let bucket = dir.path().join("store.bucket");
        let mut files = vec![bucket.clone()];
        let mut objects = 0;
        while let Some(at) = files.pop() {
            match host_fs::is_dir(&at) {
                true => files.extend(host_fs::read_dir(&at).unwrap().map(|e| e.unwrap().path())),
                false => {
                    let bytes = host_fs::read(&at).unwrap();
                    let held = bytes.windows(64).any(|window| window == &data[..64]);
                    assert!(!held, "{} holds the cell's data", at.display());
                    objects += 1;
                }
            }
        }
        objects
    });
    assert!(objects > 3, "{objects} objects");
    let child = host_cell::fork_child(cell.cell());
    cell.fork_to(dir.path().join("fork"), child).await.unwrap();
    let fork = OpenCell::open(dir.path().join("fork")).await.unwrap();
    assert_eq!(read(&fork, a, 0, 2 * BLOCK).await, Ok(data.clone()));
    let record = bucket_cell::head::get(&**fork.bucket(), child).await;
    let record = record.unwrap().unwrap();
    let inherited: Vec<_> = record.inherited.iter().map(|had| had.cell).collect();
    assert_eq!((record.deks.len(), inherited), (1, vec![cell.cell()]));
    drop(fork);
    let cell_id = cell.cell();
    drop(Arc::into_inner(cell).unwrap());
    let cell = OpenCell::open(&path).await.unwrap();
    assert_eq!((cell.cell(), cell.root()), (cell_id, Ok(b"root".to_vec())));
    assert_eq!(read(&cell, a, 0, 2 * BLOCK).await, Ok(data));

    // An open cell of a tenant with no KEK there: none, its cell refused, as destroyed.
    let none =
        Opening::here().unwrap().create(dir.path().join("none"), 1 << 20, CellOf::New(6)).await;
    let refused = none.err().expect("a cell of a tenant with no KEK");
    assert_eq!(refused.errno(), Some(Errno::EACCES), "{refused:#}");
    assert!(format!("{refused:#}").contains("tenant 6: no KEK in the KMS"), "{refused:#}");

    // The default tenant's, encrypted, into a cell of a tenant with no KEK.
    let common = dir.path().join("common");
    let common = Opening::here()
        .unwrap()
        .create(&common, 1 << 20, CellOf::New(host_cell::DEFAULT_TENANT))
        .await;
    let common = common.unwrap();
    assert!(!common.local.keys().is_empty());
    write(&common, id(&common, 1), 0, &pattern(2, BLOCK)).await.unwrap();
    common.commit(None).await.unwrap();
    let into = crate::host_cell::named(6);
    let refused = common.fork_to(dir.path().join("into-none"), into).await.unwrap_err();
    assert_eq!(refused.errno(), Some(Errno::EACCES), "{refused:#}");
    assert!(!host_fs::may_block(|| host_fs::exists(&dir.path().join("into-none"))));
    let found = bucket_cell::head::get(&**common.bucket(), into).await;
    assert_eq!(found.unwrap(), None);
}

/// TASK-411.18: an open cell of a tenant whose KEK is a key in Google Cloud KMS, `CLADEFS_KMS`
/// naming it as a host is configured (`gcp-kms://...`): its cell made encrypted, its KEK
/// wrapped by the tenant's key, which unwraps it to the KEK its cell's DEK is wrapped under;
/// written, read back, opened again and forked into an encrypted cell. Skipped, saying why,
/// without an access token (`extent_keys::testing::gcp`).
#[tokio::test(flavor = "multi_thread")]
async fn encrypted_cells_open_through_google_cloud_kms() {
    let test = "encrypted_cells_open_through_google_cloud_kms";
    let Some(admin) = extent_keys::testing::gcp::Admin::from_env(test) else { return };
    let url = admin.url("fxdax-test-{tenant}", None);
    admin.key("fxdax-test-5").await;
    // SAFETY: nextest runs each test in a process of its own, this one's other threads not
    // reading the environment yet.
    unsafe { std::env::set_var("CLADEFS_KMS", &url) };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let cell = Opening::here().unwrap().create(&path, 1 << 20, CellOf::New(5)).await.unwrap();
    let dek = *cell.local.keys().encrypting().0.expose();
    let a = id(&cell, 1);
    let data = crate::test_data::noise(1, 2 * BLOCK);
    write(&cell, a, 0, &data).await.unwrap();
    cell.commit(Some(b"root")).await.unwrap();
    assert_eq!(read(&cell, a, 0, 2 * BLOCK).await, Ok(data.clone()));

    let cell_id = cell.cell();
    let bucket = cell.bucket().clone();
    let object = bucket_cell::head::get(&*bucket, cell_id).await;
    let object = object.unwrap().unwrap();
    let [kek] = &object.keks[..] else { panic!("{:?}", object.keks) };
    assert!(kek.wrapped.0.starts_with(b"fxkr\x01\x07gcp-kms"), "{:?}", kek.wrapped);
    let kms = extent_keys::kms::from_url(&url).unwrap();
    let unwrapped = kms.unwrap_dyn(5, &kek.wrapped).await;
    let unwrapped = unwrapped.unwrap();
    let record = bucket_cell::head::get(&*bucket, cell_id).await;
    let record = record.unwrap().unwrap();
    let wrapped_dek = bucket_cell::keys::unwrap_dek(&unwrapped, cell_id, &record.deks[0]);
    assert_eq!(wrapped_dek.unwrap().expose(), &dek);
    let other = kms.unwrap_dyn(6, &kek.wrapped).await;
    assert_eq!(
        other.map(drop).unwrap_err().to_string(),
        "tenant 6: not authentic: a wrapped key, wrapped under another key"
    );

    drop(cell);
    let cell = OpenCell::open(&path).await.unwrap();
    assert_eq!((cell.cell(), cell.root()), (cell_id, Ok(b"root".to_vec())));
    assert_eq!(read(&cell, a, 0, 2 * BLOCK).await, Ok(data.clone()));
    let child = host_cell::fork_child(cell.cell());
    cell.fork_to(dir.path().join("fork"), child).await.unwrap();
    let fork = OpenCell::open(dir.path().join("fork")).await.unwrap();
    assert!(!fork.local.keys().is_empty());
    assert_eq!(read(&fork, a, 0, 2 * BLOCK).await, Ok(data));
}

/// cladefs: doc-27 (board 6214): a page-in's fetch that read where its extent is before the
/// cell's writer relocated it, the relocation acknowledged since and the object it read from
/// deleted, as the collector deletes what no state it keeps reads, reads where the extent is
/// again, and fetches it there. One whose object is gone where the cell holds it still is lost.
#[tokio::test(flavor = "multi_thread")]
async fn fetches_of_extents_relocated_meanwhile_read_where_they_went() {
    use bucket_cell::bucket::Bucket as _;
    let (cell, _dir) = cell(1 << 20).await;
    let extent = id(&cell, 1);
    write(&cell, extent, 0, &pattern(6, BLOCK)).await.unwrap();
    cell.commit(None).await.unwrap();
    let in_bucket = || match cell.local.place(extent).unwrap() {
        Some(local_cell::Place::Bucket(at)) => at,
        place => panic!("{extent:?} at {place:?}"),
    };
    let from = in_bucket();
    let relocating = local_cell::Relocating { extent, from: from.clone(), data: pattern(6, BLOCK) };
    assert_eq!(cell.local.relocate(vec![relocating]).await.unwrap().bytes, BLOCK as u64);
    cell.commit(None).await.unwrap();
    let to = in_bucket();
    assert_ne!(to.key, from.key);
    cell.bucket().delete(&from.key).await.unwrap();

    let fetch = async |at: &extent_frame::segment::Location| {
        let local = Arc::downgrade(&cell.local);
        cache::fetched(local, cell.fetcher(), extent, at.clone(), 0, BLOCK as u64).await
    };
    assert_eq!(fetch(&from).await.unwrap(), pattern(6, BLOCK));
    cell.bucket().delete(&to.key).await.unwrap();
    let lost = fetch(&to).await.unwrap_err();
    let lost = lost.downcast_ref::<local_cell::upload::Failure>();
    assert!(
        matches!(lost, Some(local_cell::upload::Failure::Bucket(bucket_cell::Error::Lost(_)))),
        "{lost:?}"
    );
}

/// Races `steps` on one extent, `rounds` times, a fresh extent each: `prepare` alone first,
/// then each step on a thread of its own, all released at once, then a commit, after which the
/// extent reads `expected` (TASK-119.19). The steps are ones whose outcome is the same
/// whichever comes first: what one does between another's steps is what such a race loses.
#[allow(clippy::disallowed_methods, reason = "two threads of the test's, racing")]
async fn race(
    cell: &OpenCell,
    rounds: u64,
    first: impl AsyncFn(ExtentId) + Sync,
    second: impl AsyncFn(ExtentId) + Sync,
    expected: &[u8],
) {
    // Each step on a thread of its own, at once, as two of a guest's requests are: its
    // outermost entry, its awaits on the test's runtime.
    let runtime = tokio::runtime::Handle::current();
    for sequence in 1..=rounds {
        let extent = id(cell, sequence);
        let all = std::sync::Barrier::new(2);
        tokio::task::block_in_place(|| {
            std::thread::scope(|scope| {
                let (all, runtime) = (&all, &runtime);
                scope.spawn(|| {
                    all.wait();
                    runtime.block_on(first(extent));
                });
                scope.spawn(|| {
                    all.wait();
                    runtime.block_on(second(extent));
                });
            });
        });
        cell.commit_ordered(None).await.unwrap();
        let read = read(cell, extent, 0, expected.len()).await;
        assert_eq!(read, Ok(expected.to_vec()), "extent {sequence}");
    }
}

/// How many times each race is run: the race TASK-121 lost lost within a few.
const ROUNDS: u64 = 2000;

/// TASK-153.16: a panic holding a lock of the open cell's, as a lazily made resource's `expect`
/// was in TASK-104, TASK-105 and TASK-150.20, halts the open cell: what it was serving, and
/// each request after it, fails with `EIO`, rather than panicking at the lock's poison; those
/// that take none of the locks the panic held too, so that nothing reads what it left half
/// done. Closed, the open cell opens anew as of its last commit, and serves.
#[tokio::test(flavor = "multi_thread")]
async fn a_panic_holding_a_lock_halts_the_open_cell() {
    let (cell, dir) = cell(1 << 26).await;
    let ps = page_size();
    let window = cell.create_dax_window(2 * ps, None).unwrap();
    let (sealed, open, next) = (id(&cell, 1), id(&cell, 2), id(&cell, 3));
    write(&cell, sealed, 0, &pattern(1, BLOCK)).await.unwrap();
    cell.commit(None).await.unwrap();
    write(&cell, open, 0, &pattern(2, BLOCK)).await.unwrap();

    crate::halt::tests::panic_holding("opening");
    let data = pattern(3, BLOCK);
    let writing = std::panic::AssertUnwindSafe(write(&cell, open, 0, &data));
    let panicked = futures::FutureExt::catch_unwind(writing).await;
    assert!(panicked.is_err(), "the write didn't panic");
    assert!(cell.is_halted());
    assert_eq!(read(&cell, open, 0, BLOCK).await, Err(Errno::EIO));
    assert_eq!(read(&cell, sealed, 0, BLOCK).await, Err(Errno::EIO));
    assert_eq!(write(&cell, next, 0, &pattern(4, BLOCK)).await, Err(Errno::EIO));
    assert_eq!(cell.free(sealed), Err(Errno::EIO));
    assert_eq!(cell.commit(None).await, Err(Errno::EIO));
    // The window's, which the panic didn't hold.
    assert_eq!(cell.map(sealed, 0, ps, ps).await, Err(Errno::EIO));
    assert_eq!(cell.unmap(0, ps), Err(Errno::EIO));
    assert_eq!(cell.window_mappings(), Err(Errno::EIO));
    // Not quiescent, as far as anyone knows.
    assert_eq!(cell.is_quiescent(), Err(Errno::EIO));
    // Nor what the control API changes of it: its capacity, its versions (TASK-611).
    let committed = cell.local.last_commit().unwrap();
    assert_eq!(cell.set_capacity(1 << 27).await, Err(Errno::EIO));
    let dropped = cell.drop_versions(&[1].into()).await.map_err(|err| err.errno());
    assert_eq!(dropped, Err(Some(Errno::EIO)));
    let shared = cell.share_version(1, true).await.map_err(|err| err.errno());
    assert_eq!(shared, Err(Some(Errno::EIO)));
    assert_eq!(cell.local.last_commit().unwrap(), committed, "the halted open cell committed");
    drop((window, cell));

    let cell = OpenCell::open(dir.path().join("store")).await.unwrap();
    assert!(!cell.is_halted());
    assert_eq!(read(&cell, sealed, 0, BLOCK).await, Ok(pattern(1, BLOCK)));
    assert_eq!(read(&cell, open, 0, BLOCK).await, Err(Errno::ENOENT));
    write(&cell, next, 0, &pattern(5, BLOCK)).await.unwrap();
    cell.commit(None).await.unwrap();
    assert_eq!(read(&cell, next, 0, BLOCK).await, Ok(pattern(5, BLOCK)));
}

/// A write to an extent not open racing a commit: committed by it, or by the next, which
/// seals it.
#[tokio::test(flavor = "multi_thread")]
async fn writes_racing_a_commit_are_committed() {
    let (cell, _dir) = cell(1 << 26).await;
    let data = pattern(3, BLOCK);
    let write = async |extent| write(&cell, extent, 0, &data).await.unwrap();
    let commit = async |_: ExtentId| cell.commit_ordered(None).await.unwrap();
    race(&cell, ROUNDS, write, commit, &data).await;
}

/// Commits ordered wait for a flush, which makes them local.
#[tokio::test(flavor = "multi_thread")]
async fn ordered_commits_wait_for_a_flush() {
    let (cell, _dir) = cell(1 << 20).await;
    write(&cell, id(&cell, 1), 0, &pattern(1, BLOCK)).await.unwrap();
    cell.commit_ordered(None).await.unwrap();
    assert!(cell.flush_pending());
    cell.flush().await.unwrap();
    assert!(!cell.flush_pending());
}

/// A read-only mapping of a sealed extent maps zeros past its last page, and only zeros if it
/// is all past it.
#[tokio::test(flavor = "multi_thread")]
async fn mappings_past_a_sealed_extents_end_map_zeros() {
    let (cell, _dir) = cell(1 << 20).await;
    let ps = page_size();
    let sealed = id(&cell, 1);
    write(&cell, sealed, 0, &pattern(3, 2 * ps as usize)).await.unwrap();
    cell.commit(None).await.unwrap();
    let _window = cell.create_dax_window(8 * ps, None).unwrap();
    cell.map(sealed, ps, 3 * ps, 0).await.unwrap();
    cell.map(sealed, 2 * ps, ps, 5 * ps).await.unwrap();
    let contents = MappingContents::Extent { extent: sealed, offset: ps };
    assert_eq!(
        cell.window_mappings().unwrap(),
        [
            WindowMapping { window_offset: 0, len: ps, contents },
            WindowMapping { window_offset: ps, len: 2 * ps, contents: MappingContents::Zeros },
            WindowMapping { window_offset: 5 * ps, len: ps, contents: MappingContents::Zeros },
        ]
    );
}

/// TASK-50.3: makes a cell directory at `path` of `extents` sealed extents of a MiB each, and
/// opens it in `arena`, with a window mapping them all, read-only.
async fn mapped_extents(path: &Path, extents: u64, arena: &Arc<Arena>) -> (OpenCell, DaxWindow) {
    const EXTENT: u64 = 1 << 20;
    let cell = OpenCell::create(path, 1 << 30).await.unwrap();
    for sequence in 1..=extents {
        write(&cell, id(&cell, sequence), 0, &pattern(sequence as u8, EXTENT as usize))
            .await
            .unwrap();
    }
    cell.commit(None).await.unwrap();
    drop(cell);
    let cell = Opening::here().unwrap().arena(Some(arena.clone())).open(path).await.unwrap();
    let window = cell.create_dax_window(extents * EXTENT, None).unwrap();
    for sequence in 1..=extents {
        cell.map(id(&cell, sequence), 0, EXTENT, (sequence - 1) * EXTENT).await.unwrap();
    }
    (cell, window)
}

/// TASK-50.3: reads `extents` extents of `mapped_extents`' through `window`, a page at a time,
/// twice over, checking their bytes, and calls `read` after each.
async fn read_twice(window: &DaxWindow, extents: u64, read: impl Fn(u64, u64)) {
    const EXTENT: usize = 1 << 20;
    let ps = page_size();
    for round in 0..2 {
        for sequence in 1..=extents {
            let expected = pattern(sequence as u8, EXTENT);
            for offset in (0..EXTENT as u64).step_by(ps as usize) {
                let at = (sequence - 1) * EXTENT as u64 + offset;
                let at_page = format!("round {round}, extent {sequence}, offset {offset:#x}");
                window.fault(at, Access::Read).await.expect(&at_page);
                // SAFETY: The page is mapped, and nothing else accesses it.
                let byte = unsafe { *window.host_base().add(at as usize) };
                assert_eq!(byte, expected[offset as usize], "{at_page}");
            }
            read(round, sequence);
        }
    }
}

/// TASK-50.3: an open cell whose arena is capped reads through its window more than the cap
/// holds: the extents asked for longest ago give their pages back, paged in again as they are
/// touched, and the arena stays within its cap.
#[tokio::test(flavor = "multi_thread")]
async fn capped_arenas_evict_their_least_used_extents() {
    let chunk = 2u64 << 20;
    let dir = tempfile::tempdir().unwrap();
    let held = Arena::new().unwrap();
    let arena = held.id();
    crate::slabs::set_arena_cap(arena, Some(4 * chunk));
    let (cell, window) = mapped_extents(&dir.path().join("store"), 16, &held).await;
    let first = id(&cell, 1);
    // What the cache holds of extent 1.
    let cached = || cache::find(arena, first).expect("extent 1 cached").held();
    read_twice(&window, 16, |round, sequence| {
        let held = crate::slabs::arena_bytes(arena);
        assert!(held <= 4 * chunk, "round {round}, extent {sequence}: {held}");
        // Once all were read, the one asked for longest ago gave its pages back, as its
        // first page's read in again.
        if (round, sequence) == (0, 16) {
            assert_eq!(cached(), 0, "extent 1 was not evicted");
        }
    })
    .await;
}

/// TASK-602 (board 2600): an extent the guest MAPs over its own range, again and again, as a
/// guest rebinding a file's blocks does, is on its window set once (spec rule registerOnce):
/// its window list had an entry a MAP, every one of them visited by each of its evictions. A
/// window that maps it anew after it unmapped it all is on it once again; one unmapped, off it.
#[tokio::test(flavor = "multi_thread")]
async fn maps_over_an_extents_own_range_put_its_window_on_it_once() {
    const EXTENT: u64 = 1 << 20;
    let dir = tempfile::tempdir().unwrap();
    let held = Arena::new().unwrap();
    let (cell, window) = mapped_extents(&dir.path().join("store"), 2, &held).await;
    for _ in 0..1000 {
        cell.map(id(&cell, 1), 0, EXTENT, 0).await.unwrap();
    }
    block_on(window.fault(0, Access::Read)).unwrap();
    let cached = cache::find(held.id(), id(&cell, 1)).expect("extent 1 cached");
    assert_eq!(cached.windows_held(), 1, "MAPs over its own range");
    // Over the other extent's range, and back.
    cell.map(id(&cell, 1), 0, EXTENT, EXTENT).await.unwrap();
    cell.unmap(0, EXTENT).unwrap();
    cell.map(id(&cell, 1), 0, EXTENT, 0).await.unwrap();
    assert_eq!(cached.windows_held(), 1, "mapped twice, then once");
    cell.unmap(0, 2 * EXTENT).unwrap();
    assert_eq!(cached.windows_held(), 0, "unmapped");
}

/// TASK-92.32: relief takes first what no window maps, though what one maps was used longer
/// ago, as a guest using its mappings gives no sign of it. TASK-92.33: extents it evicted,
/// which stay cached, are left out of the next pass.
#[tokio::test(flavor = "multi_thread")]
async fn relief_evicts_what_no_window_maps_first() {
    const EXTENT: u64 = 1 << 20;
    let dir = tempfile::tempdir().unwrap();
    let held = Arena::new().unwrap();
    let (cell, window) = mapped_extents(&dir.path().join("store"), 2, &held).await;
    let cached = |sequence| cache::find(held.id(), id(&cell, sequence)).expect("cached");
    for sequence in 1..=2 {
        block_on(window.fault((sequence - 1) * EXTENT, Access::Read)).unwrap();
    }
    let (mapped, unmapped) = (cached(1).held(), cached(2).held());
    cell.unmap(EXTENT, EXTENT).unwrap();

    let evicted = cache::evict_least_used(Some(held.id()), |evicted| evicted > 0);
    assert_eq!(evicted, unmapped * page_size());
    assert_eq!((cached(1).held(), cached(2).held()), (mapped, 0));
    assert_eq!(cached(2).eviction_order(), None);

    let evicted = cache::evict_least_used(Some(held.id()), |_| false);
    assert_eq!(evicted, mapped * page_size());
    assert_eq!(cached(1).eviction_order(), None);
}

/// TASK-92.32: of what windows map, relief takes what was faulted on longest ago, whichever
/// its one MAP came first.
#[tokio::test(flavor = "multi_thread")]
async fn relief_evicts_what_was_faulted_on_longest_ago_first() {
    const EXTENT: u64 = 1 << 20;
    let dir = tempfile::tempdir().unwrap();
    let held = Arena::new().unwrap();
    let (cell, window) = mapped_extents(&dir.path().join("store"), 2, &held).await;
    let cached = |sequence| cache::find(held.id(), id(&cell, sequence)).expect("cached");
    for sequence in [2, 1] {
        block_on(window.fault((sequence - 1) * EXTENT, Access::Read)).unwrap();
    }
    let (recent, oldest) = (cached(1).held(), cached(2).held());

    let evicted = cache::evict_least_used(Some(held.id()), |evicted| evicted > 0);
    assert_eq!(evicted, oldest * page_size());
    assert_eq!((cached(1).held(), cached(2).held()), (recent, 0));
}

/// TASK-50.3: physical memory run out, its arenas uncapped, evicts the extents asked for
/// longest ago, any arena's, rather than wait for memory for ever, and abort.
#[tokio::test(flavor = "multi_thread")]
async fn memory_run_out_evicts_the_least_used_extents() {
    let memory = 48u64 << 20;
    crate::pages::configure(crate::pages::Config::with_memory(memory)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (_cell, window) =
        mapped_extents(&dir.path().join("store"), 64, &Arena::new().unwrap()).await;
    read_twice(&window, 64, |_, _| ()).await;
    let peak = crate::slabs::arenas_peak_bytes();
    assert!(peak + crate::slabs::SLAB > memory, "memory never ran out: {peak} bytes at most");
}

/// An open cell holds a block at least; one asked for less is not made, nor its directory.
#[tokio::test(flavor = "multi_thread")]
async fn cells_hold_a_block_at_least() {
    let dir = tempfile::tempdir().unwrap();
    let block = OpenCell::BLOCK_SIZE as u64;
    let small = dir.path().join("small");
    let error = OpenCell::create(&small, block - 1).await.err().unwrap();
    assert_eq!(error.to_string(), "the cell is smaller than a block");
    assert!(!host_fs::may_block(|| host_fs::exists(&small)));
    let cell = OpenCell::create(dir.path().join("one"), block).await.unwrap();
    assert_eq!(cell.info().unwrap().capacity, block);
}

/// A fork that fails but for something at its path, here as its child's open cell, made, fails
/// to finish, says so, with no errno of `EEXIST`'s, and leaves nothing at its path.
#[tokio::test(flavor = "multi_thread")]
async fn forks_failing_but_at_their_path_say_so() {
    let (cell, dir) = cell(1 << 20).await;
    let fork = dir.path().join("fork");
    let made = fail::FailGuard::new("fork::made", "return").unwrap();
    let failed = cell
        .fork_to(&fork, crate::host_cell::fork_child(cell.cell()))
        .await
        .expect_err("the fork was made");
    drop(made);
    assert_eq!(failed.errno(), None, "{failed:#}");
    assert!(
        !host_fs::may_block(|| host_fs::exists(&fork)),
        "the failed fork's cell directory stays"
    );
}

/// TASK-39.1: the window's mappings, as a snapshot of the VM records them once nothing is
/// open, mapped again in a window of a fork of the open cell, map the same bytes at the same
/// offsets: mappings split by later ones keep their place in their extents.
#[tokio::test(flavor = "multi_thread")]
async fn window_mappings_map_the_same_again_in_a_fork() {
    let (cell, dir) = cell(1 << 20).await;
    let ps = page_size();
    let (sealed, open) = (id(&cell, 1), id(&cell, 2));
    write(&cell, sealed, 0, &pattern(1, 2 * ps as usize)).await.unwrap();
    cell.commit(None).await.unwrap();
    let window = cell.create_dax_window(8 * ps, None).unwrap();
    cell.map(sealed, 0, 2 * ps, 0).await.unwrap();
    cell.map(sealed, ps, ps, 4 * ps).await.unwrap();
    cell.map_zeros_at(2 * ps, ps).unwrap();
    // Zeros over the first page of the first mapping: what is left of it maps the extent's
    // second page.
    cell.map_zeros_at(0, ps).unwrap();
    write(&cell, open, 0, &pattern(2, ps as usize)).await.unwrap();
    assert_eq!(cell.is_quiescent(), Ok(false));
    assert_eq!(cell.map(open, 0, ps, 6 * ps).await, Err(Errno::EPERM), "an open extent mapped");
    cell.commit(None).await.unwrap();
    assert_eq!(cell.is_quiescent(), Ok(true));
    cell.map(open, 0, ps, 6 * ps).await.unwrap();
    let extent = |extent, offset| MappingContents::Extent { extent, offset };
    let mappings = cell.window_mappings().unwrap();
    assert_eq!(
        mappings,
        [
            WindowMapping { window_offset: 0, len: ps, contents: MappingContents::Zeros },
            WindowMapping { window_offset: ps, len: ps, contents: extent(sealed, ps) },
            WindowMapping { window_offset: 2 * ps, len: ps, contents: MappingContents::Zeros },
            WindowMapping { window_offset: 4 * ps, len: ps, contents: extent(sealed, ps) },
            WindowMapping { window_offset: 6 * ps, len: ps, contents: extent(open, 0) },
        ]
    );

    let path = dir.path().join("fork");
    cell.fork_to(&path, crate::host_cell::fork_child(cell.cell())).await.unwrap();
    let fork = OpenCell::open(&path).await.unwrap();
    let fork_window = fork.create_dax_window(8 * ps, None).unwrap();
    for mapping in &mappings {
        match mapping.contents {
            MappingContents::Extent { extent, offset } => {
                fork.map(extent, offset, mapping.len, mapping.window_offset).await
            }
            MappingContents::Zeros => fork.map_zeros_at(mapping.window_offset, mapping.len),
        }
        .unwrap();
    }
    assert_eq!(fork.window_mappings().unwrap(), mappings);
    for mapping in &mappings {
        let bytes = |window: &DaxWindow| {
            block_on(window.fault(mapping.window_offset, Access::Read)).unwrap();
            // SAFETY: The test faulted the page in, and nothing else accesses it.
            unsafe {
                std::slice::from_raw_parts(
                    window.host_base().add(mapping.window_offset as usize),
                    ps as usize,
                )
            }
            .to_vec()
        };
        assert!(bytes(&window) == bytes(&fork_window), "{mapping:?} maps other bytes");
    }
}

/// cladefs: a sealed extent stored compressed pages in a part (`extent_frame::PART`) at a time,
/// decompressing each once, however many faults read it.
#[tokio::test(flavor = "multi_thread")]
async fn page_ins_read_whole_parts() {
    let (cell, _dir) = held(64 << 20).await;
    let ps = page_size();
    let extent = id(&cell, 1);
    // Large enough to be stored on its own, compressed, in four parts.
    let len = 4 * extent_frame::PART as usize;
    write(&cell, extent, 0, &pattern(5, len)).await.unwrap();
    let window = cell.create_dax_window(len as u64, None).unwrap();
    committed_here(&cell).await;
    cell.map(extent, 0, len as u64, 0).await.unwrap();
    let before = stats::stats();
    // From its end, so that nothing is read ahead.
    block_on(async {
        let pages: Vec<u64> = (0..len as u64).step_by(ps as usize).collect();
        for page in pages.into_iter().rev() {
            window.fault(page, Access::Read).await.unwrap();
        }
    });
    let counted = stats::stats().since(&before);
    assert_eq!((counted.page_ins, counted.page_in_bytes, counted.unpacks), (4, len as u64, 4));
    // SAFETY: The test faulted the pages in, and nothing else accesses them.
    let bytes = unsafe { std::slice::from_raw_parts(window.host_base(), len) };
    assert!(*bytes == pattern(5, len)[..], "the window maps other data");
}

/// cladefs: page-ins of an extent sealed in a segment, at an offset that is not a part's, are
/// of the segment's parts, each read once: four for an extent of 3 MiB in four parts.
#[tokio::test(flavor = "multi_thread")]
async fn page_ins_read_the_parts_extents_are_in() {
    let (cell, _dir) = held(64 << 20).await;
    let ps = page_size();
    let (before, extent) = (id(&cell, 1), id(&cell, 2));
    write(&cell, before, 0, &pattern(4, 300 << 10)).await.unwrap();
    let len = 3 * extent_frame::PART as usize;
    write(&cell, extent, 0, &pattern(5, len)).await.unwrap();
    let window = cell.create_dax_window(len as u64, None).unwrap();
    committed_here(&cell).await;
    // Its segment closed, compressed in parts, the extent 300 KiB into the first: framed, which
    // no commit waits for (TASK-165.9).
    cell.local.close_segment().unwrap();
    committed_here(&cell).await;
    cell.local.wait_framed().await.unwrap();
    cell.map(extent, 0, len as u64, 0).await.unwrap();
    let counted = stats::stats();
    // From its end, so that nothing is read ahead.
    block_on(async {
        let pages: Vec<u64> = (0..len as u64).step_by(ps as usize).collect();
        for page in pages.into_iter().rev() {
            window.fault(page, Access::Read).await.unwrap();
        }
    });
    let counted = stats::stats().since(&counted);
    assert_eq!((counted.page_ins, counted.page_in_bytes, counted.unpacks), (4, len as u64, 4));
    // SAFETY: The test faulted the pages in, and nothing else accesses them.
    let bytes = unsafe { std::slice::from_raw_parts(window.host_base(), len) };
    assert!(*bytes == pattern(5, len)[..], "the window maps other data");
}

/// cladefs: a byte the host's disk damaged in a sealed extent's file, in a part stored
/// compressed or raw, fails the page-ins of the part, and so the faults on it and the guest's
/// reads of it, rather than mapping or giving another byte; the extent's other parts read as
/// written (TASK-153.11).
#[tokio::test(flavor = "multi_thread")]
async fn flipped_bytes_fail_page_ins_and_reads() {
    let (cell, dir) = held(64 << 20).await;
    let ps = page_size();
    let part = extent_frame::PART;
    // Large enough to be stored on their own, in four parts: compressed, and raw.
    let len = 4 * part as usize;
    let contents = [
        (id(&cell, 1), pattern(5, len), extent_frame::Encoding::Zstd),
        (id(&cell, 2), crate::test_data::noise(2, len), extent_frame::Encoding::Raw),
    ];
    for (extent, data, _) in &contents {
        write(&cell, *extent, 0, data).await.unwrap();
    }
    let window = cell.create_dax_window(2 * len as u64, None).unwrap();
    committed_here(&cell).await;
    for (at, (extent, data, encoding)) in contents.iter().enumerate() {
        // A byte of its third part, as stored: itself if raw, else the middle of the part's.
        let path = dir.path().join(format!(
            "store/extents/{}/{}",
            base32_id::Padded(extent.cell),
            base32_id::Padded(extent.sequence)
        ));
        let mut bytes = host_fs::may_block(|| host_fs::read(&path)).unwrap();
        let parts = extent_frame::Parts::of(&bytes, &extent_frame::NoKeys).unwrap();
        let opened = parts.part(2).unwrap();
        let (place, stored) = (opened.place, opened.stored);
        assert_eq!(stored.encoding, *encoding);
        let offset = place.start + 5 * ps + 7;
        let byte = match stored.encoding {
            extent_frame::Encoding::Raw => stored.offset + (offset - place.start),
            extent_frame::Encoding::Zstd => stored.offset + stored.len / 2,
        };
        bytes[byte as usize] ^= 0x10;
        host_fs::may_block(|| host_fs::write(&path, bytes)).unwrap();
        // Damaged behind the open cell's back: what the open cell's page cache holds of it,
        // clean, goes, so that its reads read the disk (TASK-433).
        local_cell::pages::shared().evict_to(0);

        let start = at as u64 * len as u64;
        cell.map(*extent, 0, len as u64, start).await.unwrap();
        let page = offset / ps * ps;
        assert_eq!(block_on(window.fault(start + page, Access::Read)), Err(Errno::EIO));
        assert_eq!(read(&cell, *extent, page, BLOCK).await, Err(Errno::EIO));
        assert_eq!(read(&cell, *extent, 0, len).await, Err(Errno::EIO));
        // Its first part is whole.
        block_on(window.fault(start, Access::Read)).unwrap();
        // SAFETY: The test faulted the page in, and nothing else accesses it.
        let first = unsafe {
            std::slice::from_raw_parts(window.host_base().add(start as usize), ps as usize)
        };
        assert!(*first == data[..ps as usize], "{extent:?}'s first page maps other data");
        assert_eq!(read(&cell, *extent, 0, BLOCK).await, Ok(data[..BLOCK].to_vec()));
    }
}

/// TASK-702.2: forks of a version opened for guests read their tenant's evidence of it, once an
/// interval for all the forks a host opens, once it holds a recording (board 5337), and the
/// first of them in the interval records what its guest reads of the version's extents,
/// written as its open cell closes; another tenant's forks read and write theirs alone.
#[tokio::test(flavor = "multi_thread")]
async fn forks_of_a_version_read_and_record_their_tenants_evidence() {
    use bucket_cell::evidence::{Prediction, UNIT, Unit};
    let (base, dir) = cell(64 << 20).await;
    let extent = id(&base, 1);
    write(&base, extent, 0, &pattern(5, 3 * UNIT as usize)).await.unwrap();
    base.commit(None).await.unwrap();
    let at = dir.path().join("version");
    base.uploaded_version_at(&at).await.unwrap();
    let version = Opening::here().unwrap().for_host().open(&at).await.unwrap();
    let fork = async |name: &str, tenant| {
        let path = dir.path().join(name);
        version.fork_to(&path, host_cell::named(tenant)).await.unwrap();
        path
    };
    let (first, second, theirs) =
        (fork("first", 5).await, fork("second", 5).await, fork("theirs", 6).await);
    let open = async |path: &Path| OpenCell::open(path).await.unwrap();
    let close = async |cell: OpenCell| cell.close().await;
    let predicted = async |cell: &OpenCell| cell.predicted().unwrap().await.unwrap();

    // The first of tenant 5's forks reads none, and records its reads; the second, opened
    // within the interval, records nothing, and, the first's read having found no recording,
    // reads it again (board 5337).
    let one = open(&first).await;
    assert_eq!(*predicted(&one).await, Prediction::default());
    let two = open(&second).await;
    // Reads after the openings, in order: the first 5 ms on, the next 9.
    let now = std::time::Instant::now();
    let at = |ms| now + std::time::Duration::from_millis(ms);
    assert!(one.records_reads() && two.records_reads());
    let recorded = |cell: &OpenCell| cell.evidence().unwrap().recorded();
    assert!(recorded(&one) && !recorded(&two));
    assert!(!one.predicted().unwrap().ptr_eq(&two.predicted().unwrap()));
    one.guest_read(extent, 2 * UNIT, 1, at(5));
    one.guest_read(extent, 0, 4096, at(9));
    one.guest_read(id(&one, 1), 0, 4096, at(1));
    two.guest_read(extent, UNIT, 1, at(1));
    let other = open(&theirs).await;
    assert!(recorded(&other));
    other.guest_read(extent, UNIT, 1, at(1));
    let (version_cell, origin) = (base.cell(), one.forked_from());
    drop(base);
    for cell in [one, two, other] {
        close(cell).await;
    }

    // Each tenant reads its own evidence alone, once the host reads it again.
    crate::evidence::forget();
    let units = async |cell: &OpenCell| -> Vec<(ExtentId, u64)> {
        let predicted = predicted(cell).await;
        assert_eq!(predicted.recordings, 1);
        predicted.units.iter().map(|&Unit { extent, mib, .. }| (extent, mib)).collect()
    };
    let (two, other) = (open(&second).await, open(&theirs).await);
    assert_eq!(units(&two).await, [(extent, 2), (extent, 0)]);
    assert_eq!(units(&other).await, [(extent, 1)]);
    // One read of a recording is kept for the interval: the next fork shares it.
    let again = open(&first).await;
    assert!(again.predicted().unwrap().ptr_eq(&two.predicted().unwrap()));
    close(again).await;
    let origin = origin.unwrap().origin;
    assert_eq!(origin.parent, version_cell);
    let bucket = two.bucket.clone();
    for cell in [two, other] {
        close(cell).await;
    }
    close(version).await;
    let evidence = bucket_cell::layout::evidence_of_cell(version_cell);
    let listed = bucket_cell::bucket::Bucket::list(&*bucket, &evidence).await.unwrap();
    let tenants: Vec<u64> = (listed.iter())
        .map(|meta| match bucket_cell::layout::kind(&meta.key) {
            bucket_cell::layout::Kind::Evidence { tenant, at, .. } if at == origin.at => tenant,
            kind => panic!("{}: {kind:?}", meta.key),
        })
        .collect();
    assert_eq!(tenants, [5, 6]);
}

/// cladefs: a part paged in is pages in a run of physical memory, which a window maps with one
/// host mapping, as the parts of an extent paged in at once are: a window of a fork maps a
/// base's extents, paged in, as cheaply as the base's window mapped them.
#[tokio::test(flavor = "multi_thread")]
async fn windows_map_a_part_paged_in_with_one_mapping() {
    let (base, dir) = cell(64 << 20).await;
    let extent = id(&base, 1);
    let part = extent_frame::PART;
    let parts = 8;
    let len = parts * part;
    write(&base, extent, 0, &pattern(5, len as usize)).await.unwrap();
    let window = base.create_dax_window(len, None).unwrap();
    base.commit(None).await.unwrap();
    let forked = dir.path().join("fork");
    base.fork_to(&forked, crate::host_cell::fork_child(base.cell())).await.unwrap();
    base.map(extent, 0, len, 0).await.unwrap();
    // Each part paged in by a fault of its own, at once, as a guest's threads fault.
    std::thread::scope(|scope| {
        for start in (0..len).step_by(part as usize) {
            let window = &window;
            scope.spawn(move || block_on(window.fault(start, Access::Read)).unwrap());
        }
    });
    let fork = OpenCell::open(&forked).await.unwrap();
    let stage = Arc::new(RunRecordingSecondStage::default());
    let _window = fork.create_dax_window(len, Some(stage.clone())).unwrap();
    fork.map(extent, 0, len, 0).await.unwrap();
    let runs = stage.0.lock().unwrap().clone();
    let in_one_run: Vec<bool> = (0..parts)
        .map(|at| {
            let (start, end) = (at * part, (at + 1) * part);
            runs.iter().any(|(offset, len)| *offset <= start && end <= offset + len)
        })
        .collect();
    assert_eq!(in_one_run, vec![true; parts as usize], "runs mapped: {runs:?}");
}

/// TASK-77: what a guest asks of its open cell is untrusted: any sequence of its requests, at
/// offsets past the window's end and the extents' or at the ends of a u64, fails or doesn't,
/// and panics nothing, and an extent it can't name reads as it was written. Seeded by
/// proptest, which says the sequence if one fails.
#[test]
fn serves_whatever_a_guest_asks() {
    use proptest::prelude::*;
    use proptest::test_runner::{Config, TestRunner};

    const WINDOW: u64 = 1 << 20;
    /// The first sequence no request names.
    const KEPT: u64 = 1 << 20;
    #[derive(Debug, Clone)]
    enum Request {
        Write(u64, u64, usize),
        Read(u64, u64, usize),
        Map(u64, u64, u64, u64),
        Unmap(u64, u64),
        Commit,
        Free(u64),
        Fork,
    }
    let at = || {
        prop_oneof![
            (0u64..64).prop_map(|page| page * BLOCK as u64),
            0u64..WINDOW,
            (0u64..4096).prop_map(|back| u64::MAX - back),
            any::<u64>(),
        ]
    };
    let sequence = || 0u64..16;
    let bytes = || 0usize..4 * BLOCK;
    // Those an open cell serves: of a few extents, whole blocks and pages, as a guest's are,
    // each of the others' extents, so that what one writes the next finds open, or sealed.
    let ps = page_size();
    let few = || 0u64..4;
    let page = move || (0u64..16).prop_map(move |page| page * ps);
    let request = prop_oneof![
        8 => (few(), page(), 1usize..4).prop_map(|(e, at, n)| Request::Write(e, at, n * BLOCK)),
        8 => (few(), page(), 1u64..4, page())
            .prop_map(move |(e, at, pages, to)| Request::Map(e, at, pages * ps, to)),
        8 => (sequence(), at(), bytes()).prop_map(|(e, at, len)| Request::Write(e, at, len)),
        8 => (sequence(), at(), bytes()).prop_map(|(e, at, len)| Request::Read(e, at, len)),
        8 => (sequence(), at(), at(), at())
            .prop_map(|(e, at, len, to)| Request::Map(e, at, len, to)),
        8 => (at(), at()).prop_map(|(to, len)| Request::Unmap(to, len)),
        8 => Just(Request::Commit),
        8 => sequence().prop_map(Request::Free),
        1 => Just(Request::Fork),
    ];
    let config = Config { cases: 200, failure_persistence: None, ..Config::default() };
    // Each case's requests awaited on the test's runtime, its outermost entry.
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    TestRunner::new(config)
        .run(&proptest::collection::vec(request, 1..64), |requests| {
            runtime.block_on(async {
                // Room for every extent it may open, so that nothing it did may fail a commit.
                let (cell, dir) = cell(1 << 27).await;
                let stage = Arc::new(RunRecordingSecondStage::default());
                let _window = cell.create_dax_window(WINDOW, Some(stage)).unwrap();
                let kept = id(&cell, KEPT);
                write(&cell, kept, 0, &pattern(9, BLOCK)).await.unwrap();
                cell.commit(None).await.unwrap();
                let mut forks = (1u64..).map(|fork| (dir.path().join(fork.to_string()), fork));
                for request in requests {
                    let extent = |sequence| id(&cell, sequence);
                    // Failing or not as it may: what it may not do is panic.
                    let _ = match request {
                        Request::Write(e, at, len) => {
                            write(&cell, extent(e), at, &vec![1; len]).await
                        }
                        Request::Read(e, at, len) => {
                            read(&cell, extent(e), at, len).await.map(drop)
                        }
                        Request::Map(e, at, len, to) => cell.map(extent(e), at, len, to).await,
                        Request::Unmap(to, len) => cell.unmap(to, len),
                        Request::Commit => cell.commit(None).await,
                        Request::Free(e) => cell.free(extent(e)),
                        Request::Fork => {
                            let (path, fork) = forks.next().unwrap();
                            cell.fork_to(
                                path,
                                crate::CellId { id: cell.cell().id + fork, ..cell.cell() },
                            )
                            .await
                            .map_err(|_| Errno::EIO)
                        }
                    };
                }
                // Nor leave a commit it can't make: a wedged open cell's guest gets nothing
                // more done.
                prop_assert_eq!(cell.commit(None).await, Ok(()));
                prop_assert_eq!(read(&cell, kept, 0, BLOCK).await, Ok(pattern(9, BLOCK)));
                Ok(())
            })
        })
        .unwrap();
}

/// A second stage recording the runs it maps: their offsets and lengths.
#[derive(Default)]
struct RunRecordingSecondStage(std::sync::Mutex<Vec<(u64, u64)>>);

impl SecondStage for RunRecordingSecondStage {
    fn map(&self, offset: u64, _: PhysLocation, len: u64) {
        self.0.lock().unwrap().push((offset, len));
    }

    fn unmap(&self, _: u64, _: u64, _: crate::dax::Private) {}
}

/// cladefs: an open cell of a cell kept in object storage, opened where its data is not, maps
/// the cell's extents: their pages are fetched as the window's faults page them in.
#[tokio::test(flavor = "multi_thread")]
async fn maps_extents_whose_data_is_in_object_storage() {
    let dir = tempfile::tempdir().unwrap();
    let ps = page_size();
    let len = ps as usize + BLOCK;
    let url = format!("memory:///maps-bucket-{:016x}", rand::random::<u64>());
    let first = Opening::here()
        .unwrap()
        .create_kept(dir.path().join("first"), 1 << 20, CellOf::New(0), &url)
        .await
        .unwrap();
    let extent = id(&first, 1);
    write(&first, extent, 0, &pattern(7, len)).await.unwrap();
    first.commit(None).await.unwrap();
    let cell_id = first.cell();
    drop(first);

    let cell = Opening::here()
        .unwrap()
        .create_kept(dir.path().join("second"), 1 << 20, CellOf::Named(cell_id), &url)
        .await;
    let cell = cell.unwrap();
    assert_eq!(read(&cell, extent, 0, len).await, Ok(pattern(7, len)));
    let window = cell.create_dax_window(4 * ps, None).unwrap();
    cell.map(extent, 0, 2 * ps, ps).await.unwrap();
    block_on(async {
        window.fault(ps, Access::Read).await.unwrap();
        window.fault(2 * ps, Access::Read).await.unwrap();
    });
    // SAFETY: The test faulted the pages in, and nothing else accesses them.
    let bytes =
        unsafe { std::slice::from_raw_parts(window.host_base().add(ps as usize), 2 * ps as usize) };
    assert!(bytes[..len] == pattern(7, len)[..], "the window maps other data");
    assert!(bytes[len..].iter().all(|byte| *byte == 0), "the padding isn't zeros");
}

/// A second stage recording the changes made to it, page by page: each, and the page.
#[derive(Default)]
struct RecordingSecondStage(std::sync::Mutex<Vec<(&'static str, u64)>>);

impl SecondStage for RecordingSecondStage {
    fn map(&self, offset: u64, _: PhysLocation, len: u64) {
        let mut ops = self.0.lock().unwrap();
        for page in (0..len).step_by(page_size() as usize) {
            ops.push(("map", offset + page));
        }
    }

    fn unmap(&self, offset: u64, len: u64, _: crate::dax::Private) {
        let mut ops = self.0.lock().unwrap();
        for page in (0..len).step_by(page_size() as usize) {
            ops.push(("unmap", offset + page));
        }
    }
}

/// The hypervisor's second stage follows the window: a sealed extent's pages are mapped as
/// the guest faults on them, or as they are mapped, if in memory; unmapping unmaps them; a
/// commit changes nothing there.
#[tokio::test(flavor = "multi_thread")]
async fn second_stage_follows_the_window() {
    let (cell, _dir) = cell(1 << 20).await;
    let ps = page_size();
    let stage = Arc::new(RecordingSecondStage::default());
    let window = cell.create_dax_window(4 * ps, Some(stage.clone())).unwrap();
    let take = || std::mem::take(&mut *stage.0.lock().unwrap());
    let extent = id(&cell, 1);
    write(&cell, extent, 0, &pattern(3, 2 * ps as usize)).await.unwrap();
    cell.commit(None).await.unwrap();
    cell.map(extent, 0, 2 * ps, 0).await.unwrap();
    assert_eq!(take(), [], "pages not in memory mapped");
    block_on(window.fault(ps, Access::Read)).unwrap();
    assert_eq!(take(), [("map", ps)]);
    // The extent's first page, paged in with the second, is mapped as the range is.
    cell.map(extent, 0, ps, 3 * ps).await.unwrap();
    assert_eq!(take(), [("map", 3 * ps)]);
    write(&cell, id(&cell, 2), 0, &pattern(4, BLOCK)).await.unwrap();
    cell.commit(None).await.unwrap();
    assert_eq!(take(), [], "a commit changed the window");
    cell.unmap(0, 2 * ps).unwrap();
    assert_eq!(take(), [("unmap", ps)]);
}

/// TASK-79.6: an extent freed once sealed is mapped, and written, no more: a fork
/// that shares its pages through the cache, by its ID, would read what its writer stored in
/// them as its own.
#[tokio::test(flavor = "multi_thread")]
async fn freed_extents_are_written_no_more() {
    let (cell, dir) = cell(1 << 20).await;
    let ps = page_size();
    let extent = id(&cell, 1);
    write(&cell, extent, 0, &pattern(6, ps as usize)).await.unwrap();
    cell.commit(None).await.unwrap();
    let fork_path = dir.path().join("fork");
    cell.fork_to(&fork_path, crate::host_cell::fork_child(cell.cell())).await.unwrap();
    let fork = OpenCell::open(&fork_path).await.unwrap();
    assert_eq!(read(&fork, extent, 0, BLOCK).await, Ok(pattern(6, BLOCK)));

    cell.free(extent).unwrap();
    cell.commit(None).await.unwrap();
    let window = cell.create_dax_window(ps, None).unwrap();
    assert_eq!(cell.map(extent, 0, ps, 0).await, Err(Errno::ENOENT));
    assert_eq!(write(&cell, extent, 0, &pattern(7, BLOCK)).await, Err(Errno::EPERM));
    drop(window);
    assert_eq!(read(&fork, extent, 0, BLOCK).await, Ok(pattern(6, BLOCK)));
}

/// TASK-79.7: an extent forks share in the cache pages in through any of them that holds it:
/// one that frees it, the one it was cached through, leaves the others' mappings of it paging
/// in through theirs.
#[tokio::test(flavor = "multi_thread")]
async fn forks_page_in_what_a_sibling_freed() {
    let (base, dir) = cell(1 << 20).await;
    let ps = page_size();
    let extent = id(&base, 1);
    write(&base, extent, 0, &pattern(6, ps as usize)).await.unwrap();
    base.commit(None).await.unwrap();
    let mut forks: Vec<OpenCell> = Vec::new();
    for fork in 1..=2 {
        let path = dir.path().join(format!("fork-{fork}"));
        base.fork_to(&path, crate::host_cell::fork_child(base.cell())).await.unwrap();
        forks.push(OpenCell::open(&path).await.unwrap());
    }
    drop(base);
    let windows: Vec<DaxWindow> =
        forks.iter().map(|fork| fork.create_dax_window(ps, None).unwrap()).collect();
    for fork in &forks {
        fork.map(extent, 0, ps, 0).await.unwrap();
    }
    forks[0].free(extent).unwrap();
    forks[0].commit(None).await.unwrap();
    block_on(windows[1].fault(0, Access::Read)).unwrap();
    // SAFETY: The test faulted the page in, and nothing writes it.
    let mapped = unsafe { std::slice::from_raw_parts(windows[1].host_base(), ps as usize) };
    assert!(mapped == pattern(6, ps as usize), "the window maps other data");
}

/// TASK-79.7: the cache keeps no open cell it read an extent through once it closes, nor its
/// files and disk space: with hundreds of forks, the daemon's file descriptors ran out.
#[tokio::test(flavor = "multi_thread")]
async fn the_cache_keeps_no_cell_closed() {
    let (cell, _dir) = cell(1 << 20).await;
    let ps = page_size();
    let extent = id(&cell, 1);
    write(&cell, extent, 0, &pattern(6, ps as usize)).await.unwrap();
    cell.commit(None).await.unwrap();
    assert_eq!(read(&cell, extent, 0, BLOCK).await, Ok(pattern(6, BLOCK)));
    assert!(cached(extent).is_some());
    let closed = Arc::downgrade(&cell.local);
    drop(cell);
    assert!(closed.upgrade().is_none(), "the cache keeps the cell");
}

/// TASK-104: the cache keeps no more extents than its bound, letting go of those asked for
/// longest ago that nothing else holds, and keeps those a window maps: under pgbench, extents
/// by the hundred thousand, those never asked for again would be most of what it holds.
#[tokio::test(flavor = "multi_thread")]
async fn the_cache_keeps_its_bound() {
    crate::Host::process().cache().most_cached().store(8, Ordering::Relaxed);
    let (cell, _dir) = cell(1 << 20).await;
    let ps = page_size();
    for sequence in 1..=20 {
        write(&cell, id(&cell, sequence), 0, &pattern(sequence as u8, ps as usize)).await.unwrap();
    }
    cell.commit(None).await.unwrap();
    let window = cell.create_dax_window(ps, None).unwrap();
    cell.map(id(&cell, 1), 0, ps, 0).await.unwrap();
    for sequence in 2..=20 {
        let extent = id(&cell, sequence);
        let cached_in = CachedIn::Arena(ArenaId::DEFAULT);
        let cached = cache::cached(&cell.local, cached_in, None, extent, ps);
        drop(cached.await.unwrap());
    }
    let mut kept: Vec<u64> = cache::keys().iter().map(|(_, extent)| extent.sequence).collect();
    kept.sort_unstable();
    assert_eq!(kept, [1, 14, 15, 16, 17, 18, 19, 20]);
    drop(window);
}

/// TASK-109.2: past its bound, with more extents than the bound held by a window's mappings,
/// which it can't let go of, the cache looks through its extents again only once it grows by
/// an eighth of its bound: it did at every lookup, each a sort of every extent cached.
#[tokio::test(flavor = "multi_thread")]
async fn the_cache_trims_what_windows_hold_once_per_eighth_of_its_bound() {
    const MOST: usize = 64;
    crate::Host::process().cache().most_cached().store(MOST, Ordering::Relaxed);
    let (cell, _dir) = cell(8 << 20).await;
    let ps = page_size();
    let (mapped, looked_up) = (2 * MOST as u64, 4 * MOST as u64);
    for sequence in 1..=mapped + looked_up {
        write(&cell, id(&cell, sequence), 0, &pattern(sequence as u8, ps as usize)).await.unwrap();
    }
    cell.commit(None).await.unwrap();
    let window = cell.create_dax_window(mapped * ps, None).unwrap();
    for sequence in 1..=mapped {
        cell.map(id(&cell, sequence), 0, ps, (sequence - 1) * ps).await.unwrap();
    }
    let before = stats::stats();
    for sequence in mapped + 1..=mapped + looked_up {
        let extent = id(&cell, sequence);
        let cached_in = CachedIn::Arena(ArenaId::DEFAULT);
        let cached = cache::cached(&cell.local, cached_in, None, extent, ps);
        drop(cached.await.unwrap());
    }
    let trims = stats::stats().since(&before).cache_trims;
    assert!(trims <= looked_up / (MOST as u64 / 8) + 1, "{trims} trims of the cache");
    let cached = cache::keys().len() as u64;
    assert!(cached <= mapped + MOST as u64 / 8, "{cached} extents cached");
    drop(window);
}

/// The process's cached sealed `extent`, if it is cached.
fn cached(extent: ExtentId) -> Option<Arc<CachedExtent>> {
    cache::find(ArenaId::DEFAULT, extent)
}

/// What one page-in reads: a part.
const READ_AHEAD: u64 = extent_frame::PART;

/// A source whose reads all wait until `readers` of them are waiting at once, then fill the
/// buffer with the `pattern` of their chunk.
struct Held {
    readers: usize,
    /// The reads waiting, and their wakers.
    waiting: Mutex<(usize, Vec<std::task::Waker>)>,
    /// Notified when all are.
    all: std::sync::Condvar,
}

struct HeldSource(Arc<Held>);

impl cache::Contents for HeldSource {
    fn read<'a>(
        &'a self,
        offset: u64,
        buf: &'a mut [u8],
    ) -> futures::future::BoxFuture<'a, std::result::Result<(), anyhow::Error>> {
        let held = &self.0;
        let mut counted = false;
        Box::pin(async move {
            std::future::poll_fn(|context| {
                let mut waiting = held.waiting.lock().unwrap();
                if !counted {
                    counted = true;
                    waiting.0 += 1;
                    if waiting.0 == held.readers {
                        waiting.1.drain(..).for_each(std::task::Waker::wake);
                        held.all.notify_all();
                    }
                }
                if waiting.0 >= held.readers {
                    return std::task::Poll::Ready(());
                }
                waiting.1.push(context.waker().clone());
                std::task::Poll::Pending
            })
            .await;
            buf.copy_from_slice(&pattern((offset / READ_AHEAD) as u8, buf.len()));
            Ok(())
        })
    }
}

/// cladefs (TASK-28.8): page-ins waiting for their extents' reads hold no thread: more reads
/// wait at once than the pager has threads, and all go on.
#[tokio::test(flavor = "multi_thread")]
async fn page_ins_wait_holding_no_thread() {
    let readers = 4 * cache::PAGER_THREADS;
    let held = Arc::new(Held {
        readers,
        waiting: Mutex::new((0, Vec::new())),
        all: std::sync::Condvar::new(),
    });
    let extent = Arc::new(CachedExtent::new(
        ArenaId::DEFAULT,
        ExtentId { cell: u64::MAX, sequence: u64::MAX },
        Box::new(HeldSource(held.clone())),
        readers as u64 * READ_AHEAD,
    ));
    let faults: Vec<_> = (0..readers as u64)
        .map(|chunk| {
            let extent = extent.clone();
            tokio::spawn(async move {
                let mut data = vec![0; BLOCK];
                extent.read(chunk * READ_AHEAD, &mut data).await.unwrap();
                data
            })
        })
        .collect();
    // Were page-ins to hold their threads while they waited, no more than the pager's would.
    let waiting = held.waiting.lock().unwrap();
    let (waiting, timeout) = held
        .all
        .wait_timeout_while(waiting, std::time::Duration::from_secs(30), |waiting| {
            waiting.0 < readers
        })
        .unwrap();
    assert!(!timeout.timed_out(), "only {} of {readers} reads waited at once", waiting.0);
    drop(waiting);
    for (chunk, fault) in faults.into_iter().enumerate() {
        assert_eq!(fault.await.unwrap(), pattern(chunk as u8, BLOCK));
    }
}

/// A source whose reads fail with `cause`.
struct FailingSource(&'static str);

impl cache::Contents for FailingSource {
    fn read<'a>(
        &'a self,
        _offset: u64,
        _buf: &'a mut [u8],
    ) -> futures::future::BoxFuture<'a, std::result::Result<(), anyhow::Error>> {
        let cause = self.0;
        Box::pin(async move { Err(anyhow::anyhow!(cause).context("fetching a part")) })
    }
}

/// What this process logs, kept: for tests of what a failure says.
struct KeptLog(Mutex<Vec<String>>);

impl log::Log for KeptLog {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        self.0.lock().unwrap().push(record.args().to_string());
    }

    fn flush(&self) {}
}

/// cladefs: a page-in that fails says why, where the daemon logs it: what its read failed with,
/// not EIO alone, which was all a soak's checking daemon said of an extent of an open cell its
/// index placed in the bucket and that could not be read (TASK-217, GCE r757 round 40).
#[tokio::test(flavor = "multi_thread")]
async fn failed_page_ins_say_why() {
    static KEPT: KeptLog = KeptLog(Mutex::new(Vec::new()));
    log::set_logger(&KEPT).unwrap();
    log::set_max_level(log::LevelFilter::Error);
    let extent = Arc::new(CachedExtent::new(
        ArenaId::DEFAULT,
        ExtentId { cell: 7, sequence: 9 },
        Box::new(FailingSource("no such object at segments/7/1")),
        READ_AHEAD,
    ));
    let mut data = vec![0; BLOCK];
    assert_eq!(extent.read(0, &mut data).await, Err(Errno::EIO));
    let logged = KEPT.0.lock().unwrap().clone();
    assert!(
        logged.iter().any(|line| line.contains("fetching a part: no such object at segments/7/1")),
        "{logged:?}"
    );
}

/// An open cell holding `data` in one sealed extent, and the generation at `path` that pins it:
/// the extent, and the open cell's ancestry as the generation has it.
async fn generation(path: &Path, data: &[u8]) -> (ExtentId, Vec<u64>) {
    let publisher = made(&path.with_extension("publisher"), 1 << 20).await;
    let extent = id(&publisher, 1);
    write(&publisher, extent, 0, data).await.unwrap();
    publisher.commit(None).await.unwrap();
    // A cell of its own, as a cell has one cell (doc-20).
    publisher.fork_to(path, crate::host_cell::fork_child(publisher.cell())).await.unwrap();
    let ancestry = OpenCell::open(path).await.unwrap().ancestry();
    (extent, ancestry)
}

/// What opens what open cells attach, one instance of each, as the extent daemon does.
fn one_instance_each() -> OpenBase {
    let open: Arc<tokio::sync::Mutex<HashMap<std::path::PathBuf, Arc<OpenCell>>>> = Arc::default();
    Arc::new(move |path| {
        let (open, path) = (open.clone(), path.to_owned());
        Box::pin(async move {
            let mut open = open.lock().await;
            if let Some(cell) = open.get(&path) {
                return Ok(cell.clone());
            }
            let cell = Arc::new(Opening::here().unwrap().open_base(&path).await.unwrap());
            open.insert(path, cell.clone());
            Ok(cell)
        })
    })
}

/// doc-10: a base attached reads and maps through, read-only, and nothing else does: not an
/// extent of a base not attached, nor one a base doesn't hold.
#[tokio::test(flavor = "multi_thread")]
async fn bases_attached_read_and_map_through() {
    let (cell, dir) = cell(1 << 20).await;
    let ps = page_size();
    let (base, other) = (dir.path().join("base"), dir.path().join("other"));
    let data = pattern(3, ps as usize);
    let (extent, ancestry) = generation(&base, &data).await;
    let (not_attached, _) = generation(&other, &data).await;
    let window = cell.create_dax_window(4 * ps, None).unwrap();

    assert_eq!(read(&cell, extent, 0, BLOCK).await, Err(Errno::ENOENT));
    assert_eq!(cell.map(extent, 0, ps, 0).await, Err(Errno::ENOENT));

    host_fs::may_block(|| cell.attach_base(&base, &ancestry).unwrap());
    assert_eq!(read(&cell, extent, 0, ps as usize).await, Ok(data.clone()));
    cell.map(extent, 0, ps, 0).await.unwrap();
    block_on(window.fault(0, Access::Read)).unwrap();
    // SAFETY: The test faulted the page in, and nothing writes it.
    let mapped = unsafe { std::slice::from_raw_parts(window.host_base(), ps as usize) };
    assert!(mapped == data, "the window maps other data");

    assert_eq!(read(&cell, not_attached, 0, BLOCK).await, Err(Errno::ENOENT));
    assert_eq!(cell.map(not_attached, 0, ps, ps).await, Err(Errno::ENOENT));
    let beyond = ExtentId { sequence: extent.sequence + 1, ..extent };
    assert_eq!(read(&cell, beyond, 0, BLOCK).await, Err(Errno::ENOENT));
    // A base's extent maps read-only anywhere, as every MAP does; it is never written.
    cell.map(extent, 0, ps, ps).await.unwrap();
    assert_eq!(write(&cell, extent, 0, &[0; BLOCK]).await, Err(Errno::EPERM));
    // Attaching it again changes nothing.
    host_fs::may_block(|| {
        cell.attach_base(&base, &ancestry).unwrap();
        assert_eq!(cell_dir::attached(&dir.path().join("store")).unwrap().len(), 1);
    });
}

/// doc-10: VMs mapping a base share its pages: the cache's one copy, for the attached and for
/// an open cell that holds the extent itself.
#[tokio::test(flavor = "multi_thread")]
async fn windows_share_bases_pages() {
    let dir = tempfile::tempdir().unwrap();
    let ps = page_size();
    let base = dir.path().join("base");
    let (extent, ancestry) = generation(&base, &pattern(4, ps as usize)).await;
    let mut cells: Vec<OpenCell> = Vec::new();
    for at in 0..2 {
        cells.push(OpenCell::create(dir.path().join(format!("{at}")), 1 << 20).await.unwrap());
    }
    let windows: Vec<DaxWindow> =
        cells.iter().map(|cell| cell.create_dax_window(ps, None).unwrap()).collect();
    let open = one_instance_each();
    for cell in &cells {
        host_fs::may_block(|| {
            cell.open_bases(open.clone()).unwrap();
            cell.attach_base(&base, &ancestry).unwrap();
        });
        cell.map(extent, 0, ps, 0).await.unwrap();
    }
    // Counted before anything pages in: a page-in holds the extent too, a while after the
    // fault it serves returns (TASK-157), as `windows_share_extents_pages` counts.
    let extent_cached = cached(extent).expect("the extent is cached");
    // The cache's, the test's, and each window's mapping.
    assert_eq!(Arc::strong_count(&extent_cached), 4);
    for window in &windows {
        block_on(window.fault(0, Access::Read)).unwrap();
        // SAFETY: The test faulted the page in, and nothing writes it.
        let mapped = unsafe { std::slice::from_raw_parts(window.host_base(), ps as usize) };
        assert!(mapped == pattern(4, ps as usize), "a window maps other data");
    }
    assert!(Arc::ptr_eq(&cached(extent).unwrap(), &extent_cached), "cached anew");
    let mut held = vec![0; ps as usize];
    extent_cached.read(0, &mut held).await.unwrap();
    assert!(held == pattern(4, ps as usize), "the cache holds other data");
}

/// TASK-234: an extent of a base its publisher's open cell, open in its tenant's arena, has
/// cached is read into the default arena, which a base's pages are of, from those pages, not
/// paged in again, and they go: one copy, the default arena's, which the publisher's open cell
/// reads from then on. The first sandbox on a generation of the Linux tree paged 1.8 GB in,
/// 14.5 s, the daemon holding it as the publisher's open cell's (GCE r941).
#[tokio::test(flavor = "multi_thread")]
async fn a_bases_extent_its_publisher_has_cached_moves_to_the_default_arena() {
    let dir = tempfile::tempdir().unwrap();
    let ps = page_size();
    let len = 4 * extent_frame::PART as usize;
    let data = pattern(7, len);
    let (publisher_path, base) = (dir.path().join("publisher"), dir.path().join("base"));
    let publisher = made(&publisher_path, 1 << 24).await;
    let extent = id(&publisher, 1);
    write(&publisher, extent, 0, &data).await.unwrap();
    publisher.commit(None).await.unwrap();
    publisher.fork_to(&base, crate::host_cell::fork_child(publisher.cell())).await.unwrap();
    drop(publisher);
    let opened = OpenCell::open(&base).await.unwrap();
    host_fs::may_block(|| opened.make_public_attach_only().unwrap());
    drop(opened);
    let tenant = Arena::new().unwrap();
    let publisher =
        Opening::here().unwrap().arena(Some(tenant.clone())).open(&publisher_path).await.unwrap();
    // The publisher's VM maps its first page, from the tenant's copy.
    let window = publisher.create_dax_window(len as u64, None).unwrap();
    publisher.map(extent, 0, len as u64, 0).await.unwrap();
    block_on(window.fault(0, Access::Read)).unwrap();
    assert_eq!(read(&publisher, extent, 0, len).await, Ok(data.clone()));
    let pages = len as u64 / ps;
    let held = |arena| cache::find(arena, extent).map(|cached| cached.held());
    assert_eq!(held(tenant.id()), Some(pages));

    let ancestry = OpenCell::open(&base).await.unwrap().ancestry();
    let sandbox = made(&dir.path().join("sandbox"), 1 << 20).await;
    host_fs::may_block(|| {
        sandbox.open_bases(one_instance_each()).unwrap();
        sandbox.attach_base(&base, &ancestry).unwrap();
    });
    let sandbox_window = sandbox.create_dax_window(len as u64, None).unwrap();
    let page_ins = stats::stats().page_ins;
    // Moved as the sandbox maps it, so that its MAP maps every page, none faulted on: moved
    // as asked for, the first sandbox on the Linux tree took 4,422 faults (GCE r991).
    sandbox.map(extent, 0, len as u64, 0).await.unwrap();
    assert_eq!((held(ArenaId::DEFAULT), held(tenant.id())), (Some(pages), None));
    assert_eq!(read(&sandbox, extent, 0, len).await, Ok(data.clone()));
    assert_eq!(stats::stats().page_ins - page_ins, 0, "paged in again");
    drop(sandbox_window);

    assert_eq!(read(&publisher, extent, 0, len).await, Ok(data.clone()));
    // Its window, the tenant's page evicted from it, faults onto the default arena's.
    block_on(window.fault(0, Access::Read)).unwrap();
    // SAFETY: The test faulted the page in, and nothing writes it.
    let mapped = unsafe { std::slice::from_raw_parts(window.host_base(), ps as usize) };
    assert!(mapped == &data[..ps as usize], "the publisher's window maps other data");
    assert_eq!(stats::stats().page_ins - page_ins, 0, "the publisher paged in again");
    let copies = cache::keys().into_iter().filter(|(_, cached)| *cached == extent).count();
    assert_eq!(copies, 1);
}

/// TASK-234: a tenant's copy of an extent moves to the default arena only for a read through a
/// published generation: one not published, read without an arena as a base is, takes nothing
/// of the tenant's, which keeps its copy, and pages in its own, as every VM process maps the
/// default arena.
#[tokio::test(flavor = "multi_thread")]
async fn an_extent_of_no_published_generation_stays_its_tenants() {
    let dir = tempfile::tempdir().unwrap();
    let ps = page_size();
    let len = 4 * extent_frame::PART as usize;
    let data = pattern(8, len);
    let (publisher_path, base) = (dir.path().join("publisher"), dir.path().join("base"));
    let publisher = made(&publisher_path, 1 << 24).await;
    let extent = id(&publisher, 1);
    write(&publisher, extent, 0, &data).await.unwrap();
    publisher.commit(None).await.unwrap();
    publisher.fork_to(&base, crate::host_cell::fork_child(publisher.cell())).await.unwrap();
    drop(publisher);
    let tenant = Arena::new().unwrap();
    let publisher =
        Opening::here().unwrap().arena(Some(tenant.clone())).open(&publisher_path).await.unwrap();
    assert_eq!(read(&publisher, extent, 0, len).await, Ok(data.clone()));
    let pages = len as u64 / ps;
    let held = |arena| cache::find(arena, extent).map(|cached| cached.held());

    let ancestry = OpenCell::open(&base).await.unwrap().ancestry();
    let sandbox = made(&dir.path().join("sandbox"), 1 << 20).await;
    host_fs::may_block(|| {
        sandbox.open_bases(one_instance_each()).unwrap();
        sandbox.attach_base(&base, &ancestry).unwrap();
    });
    let page_ins = stats::stats().page_ins;
    assert_eq!(read(&sandbox, extent, 0, len).await, Ok(data));
    assert_eq!(stats::stats().page_ins - page_ins, 4, "one page-in a part");
    assert_eq!((held(tenant.id()), held(ArenaId::DEFAULT)), (Some(pages), Some(pages)));
}

/// TASK-234: what says an extent is reachable from a published generation says it of that
/// extent alone: the cache moves no other's pages with it.
#[tokio::test(flavor = "multi_thread")]
#[should_panic(expected = "a tenant's copy moved for another extent")]
async fn a_public_reach_moves_its_own_extent_alone() {
    let (cell, _dir) = cell(1 << 20).await;
    let (reached, other) = (id(&cell, 1), id(&cell, 2));
    write(&cell, other, 0, &[1; BLOCK]).await.unwrap();
    cell.commit(None).await.unwrap();
    let cached_in = CachedIn::Public(PublicReach(reached));
    let cached = cache::cached(&cell.local, cached_in, None, other, BLOCK as u64);
    drop(cached.await);
}

/// doc-10: what a generation pins stays, whatever its open cell does next.
#[tokio::test(flavor = "multi_thread")]
async fn bases_outlast_what_their_cells_free() {
    let (cell, dir) = cell(1 << 20).await;
    let (publisher_path, base) = (dir.path().join("publisher"), dir.path().join("base"));
    let publisher = made(&publisher_path, 1 << 20).await;
    let extent = id(&publisher, 1);
    write(&publisher, extent, 0, &pattern(5, BLOCK)).await.unwrap();
    publisher.commit(None).await.unwrap();
    publisher.fork_to(&base, crate::host_cell::fork_child(publisher.cell())).await.unwrap();
    publisher.free(extent).unwrap();
    publisher.commit(None).await.unwrap();
    assert_eq!(read(&publisher, extent, 0, BLOCK).await, Err(Errno::ENOENT));

    let ancestry = OpenCell::open(&base).await.unwrap().ancestry();
    host_fs::may_block(|| cell.attach_base(&base, &ancestry).unwrap());
    assert_eq!(read(&cell, extent, 0, BLOCK).await, Ok(pattern(5, BLOCK)));
}

/// doc-10: what is attached stays attached to forks, which a restored VM's open cell is, and
/// through reopening, and doesn't reach back to the open cell a fork came from.
#[tokio::test(flavor = "multi_thread")]
async fn bases_stay_attached_through_forks_and_reopening() {
    let (cell, dir) = cell(1 << 20).await;
    let path = dir.path().join("store");
    let (base, later) = (dir.path().join("base"), dir.path().join("later"));
    let (extent, ancestry) = generation(&base, &pattern(6, BLOCK)).await;
    let (only_the_fork, later_ancestry) = generation(&later, &pattern(7, BLOCK)).await;
    let open = one_instance_each();
    host_fs::may_block(|| {
        cell.open_bases(open.clone()).unwrap();
        cell.attach_base(&base, &ancestry).unwrap();
    });
    cell.commit(None).await.unwrap();

    let fork = dir.path().join("fork");
    cell.fork_to(&fork, crate::host_cell::fork_child(cell.cell())).await.unwrap();
    let forked = OpenCell::open(&fork).await.unwrap();
    host_fs::may_block(|| forked.open_bases(open.clone()).unwrap());
    assert_eq!(read(&forked, extent, 0, BLOCK).await, Ok(pattern(6, BLOCK)));

    host_fs::may_block(|| forked.attach_base(&later, &later_ancestry).unwrap());
    assert_eq!(read(&forked, only_the_fork, 0, BLOCK).await, Ok(pattern(7, BLOCK)));
    assert_eq!(read(&cell, only_the_fork, 0, BLOCK).await, Err(Errno::ENOENT));

    drop(cell);
    let cell = OpenCell::open(&path).await.unwrap();
    assert_eq!(read(&cell, extent, 0, BLOCK).await, Err(Errno::ENOENT), "not routed yet");
    host_fs::may_block(|| cell.open_bases(open).unwrap());
    assert_eq!(read(&cell, extent, 0, BLOCK).await, Ok(pattern(6, BLOCK)));
}

/// doc-10: hundreds attached cost nothing until read: what reads one opens that one alone, and
/// finding it takes a lookup of its cell.
#[tokio::test(flavor = "multi_thread")]
async fn bases_attached_open_when_read() {
    const ATTACHED: usize = 300;
    let (cell, dir) = cell(1 << 20).await;
    let mut bases: Vec<(ExtentId, Vec<u64>, std::path::PathBuf)> = Vec::new();
    for at in 0..ATTACHED {
        let path = dir.path().join(format!("base-{at}"));
        let (extent, ancestry) = generation(&path, &pattern(at as u8, BLOCK)).await;
        host_fs::may_block(|| cell.attach_base(&path, &ancestry).unwrap());
        bases.push((extent, ancestry, path));
    }
    assert_eq!(cell.bases_opened(), 0);
    assert_eq!(cell.bases.routed_cells(), 2 * ATTACHED);

    let (extent, ..) = &bases[ATTACHED / 2];
    assert_eq!(read(&cell, *extent, 0, BLOCK).await, Ok(pattern((ATTACHED / 2) as u8, BLOCK)));
    assert_eq!(cell.bases_opened(), 1);
    drop(bases);
}

/// doc-10: an open cell's ancestors are never routed: what it inherited it holds, or freed.
#[tokio::test(flavor = "multi_thread")]
async fn bases_are_not_routed_for_what_a_cell_inherited() {
    let (cell, dir) = cell(1 << 20).await;
    let inherited = id(&cell, 1);
    write(&cell, inherited, 0, &pattern(9, BLOCK)).await.unwrap();
    cell.commit(None).await.unwrap();
    let (fork, base) = (dir.path().join("fork"), dir.path().join("base"));
    cell.fork_to(&fork, crate::host_cell::fork_child(cell.cell())).await.unwrap();
    let base_cell = crate::host_cell::fork_child(cell.cell());
    cell.fork_to(&base, base_cell).await.unwrap();
    let forked = OpenCell::open(&fork).await.unwrap();
    let ancestry = OpenCell::open(&base).await.unwrap().ancestry();
    host_fs::may_block(|| forked.attach_base(&base, &ancestry).unwrap());
    assert!(!forked.bases.routes(inherited.cell));
    assert!(forked.bases.routes(base_cell.id));
    forked.free(inherited).unwrap();
    forked.commit(None).await.unwrap();
    assert_eq!(read(&forked, inherited, 0, BLOCK).await, Err(Errno::ENOENT));
}

/// TASK-160.10: an open cell whose directory can't be listed for its public markers, as with as
/// many files open as the process may (`EMFILE`), fails to open, whichever way, with `EIO`:
/// read as none, a public base opened private, its writer's, which a VM process opened to
/// write.
#[tokio::test(flavor = "multi_thread")]
async fn cells_whose_public_markers_go_unread_fail_to_open() {
    let (base, dir) = cell(1 << 20).await;
    host_fs::may_block(|| base.make_public().unwrap());
    drop(base);
    let path = dir.path().join("store");
    fail::cfg("extents::public_cells", "return").unwrap();
    let opened = [
        OpenCell::open(&path).await.map(|cell| cell.is_public()),
        Opening::here().unwrap().open_base(&path).await.map(|cell| cell.is_public()),
        Opening::here().unwrap().for_host().open(&path).await.map(|cell| cell.is_public()),
    ];
    fail::remove("extents::public_cells");
    for opened in opened {
        assert_eq!(opened.map_err(|err| err.errno()), Err(Some(Errno::EIO)));
    }
    assert!(OpenCell::open(&path).await.unwrap().is_public());
}

/// cladefs TASK-164.1 (backlog doc-13): a window whose stage copies on write maps sealed
/// extents and zeros alone, read-only: an extent written since the last commit it refuses to
/// map (`EPERM`), the open extent's bytes left in the open cell. A cell has one window. Its
/// writes, private pages, are its VM process's
/// (doc-26: `dax::uffd`'s tests).
#[tokio::test(flavor = "multi_thread")]
async fn copying_windows_map_sealed_extents_alone() {
    let (cell, _dir) = cell(1 << 24).await;
    let ps = page_size();
    let stage = || Arc::new(crate::dax::tests::Recorded::default());
    let _window = cell.create_copying_dax_window(4 * ps, stage()).unwrap();
    let (sealed, open, never) = (id(&cell, 1), id(&cell, 2), id(&cell, 3));
    write(&cell, sealed, 0, &pattern(1, BLOCK)).await.unwrap();
    cell.commit(None).await.unwrap();
    write(&cell, open, 0, &pattern(2, BLOCK)).await.unwrap();
    assert_eq!(cell.map(open, 0, ps, 0).await, Err(Errno::EPERM));
    assert_eq!(open_maps_refused(), 1, "an open extent's MAP uncounted");
    assert_eq!(cell.map(never, 0, ps, 0).await, Err(Errno::ENOENT));
    assert_eq!(cell.local.extent(open).map_err(errno), Ok(Some((BLOCK as u64, true))));
    assert_eq!(read(&cell, open, 0, BLOCK).await, Ok(pattern(2, BLOCK)));
    cell.map(sealed, 0, ps, 0).await.unwrap();

    let again = cell.create_copying_dax_window(ps, stage());
    assert_eq!(again.err().and_then(|error| error.errno()), Some(Errno::EEXIST));
}

/// TASK-539.12: the host's disk full, as making an open cell's file finds it, is `ENOSPC` to
/// the guest; an open cell failed for it says so, and is `EIO`.
#[test]
fn a_full_disk_is_enospc_and_a_failed_extent_store_eio() {
    let full = std::io::Error::from_raw_os_error(libc::ENOSPC);
    assert_eq!(errno(anyhow::Error::new(full).context("creating a raw file")), Errno::ENOSPC);
    let failed = anyhow::anyhow!("the extent store failed: framing a segment: no space");
    assert_eq!(errno(failed), Errno::EIO);
}

/// Reads of sealed extents come from the extent cache (TASK-32): a fork reading what its base
/// read before reads nothing from the open cell. Past an extent's end, zeros, as the open cell
/// reads.
#[tokio::test(flavor = "multi_thread")]
async fn reads_of_sealed_extents_come_from_the_cache() {
    let (cell, dir) = cell(1 << 20).await;
    let extent = id(&cell, 1);
    write(&cell, extent, 0, &pattern(1, 2 * BLOCK)).await.unwrap();
    // Open: from the open cell.
    let before = stats::stats();
    assert_eq!(read(&cell, extent, 0, 2 * BLOCK).await, Ok(pattern(1, 2 * BLOCK)));
    assert_eq!(stats::stats().since(&before).read_stored_bytes, 2 * BLOCK as u64);
    cell.commit(None).await.unwrap();

    let before = stats::stats();
    let tail = [pattern(1, 2 * BLOCK)[BLOCK..].to_vec(), vec![0; BLOCK]].concat();
    assert_eq!(read(&cell, extent, BLOCK as u64, 2 * BLOCK).await, Ok(tail));
    let first = stats::stats().since(&before);
    assert_eq!((first.read_stored_bytes, first.page_ins), (0, 1));

    let path = dir.path().join("fork");
    cell.fork_to(&path, crate::host_cell::fork_child(cell.cell())).await.unwrap();
    let fork = OpenCell::open(&path).await.unwrap();
    let before = stats::stats();
    assert_eq!(read(&fork, extent, 0, 2 * BLOCK).await, Ok(pattern(1, 2 * BLOCK)));
    let again = stats::stats().since(&before);
    assert_eq!((again.read_stored_bytes, again.page_ins, again.page_in_bytes), (0, 0, 0));
}

/// cladefs: a reader reading an extent from its start has the parts after the one it reads read
/// ahead of it, before it faults on them.
#[tokio::test(flavor = "multi_thread")]
async fn page_ins_read_ahead_of_a_reader_reading_on() {
    let (cell, _dir) = held(64 << 20).await;
    let extent = id(&cell, 1);
    let len = 4 * extent_frame::PART as usize;
    write(&cell, extent, 0, &pattern(5, len)).await.unwrap();
    let window = cell.create_dax_window(len as u64, None).unwrap();
    committed_here(&cell).await;
    cell.map(extent, 0, len as u64, 0).await.unwrap();
    let before = stats::stats();
    block_on(window.fault(0, Access::Read)).unwrap();
    // The first part, and the two after it, read ahead, in time.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let read = || stats::stats().since(&before).page_in_bytes;
    while read() < 3 * extent_frame::PART && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let counted = stats::stats().since(&before);
    assert_eq!((counted.page_ins, counted.page_in_bytes), (3, 3 * extent_frame::PART));
}

/// TASK-702.3: a fork of a version opened for a guest fetches what its tenant's evidence of the
/// version predicts into the host's cache, ahead of its guest: the parts of the extents its
/// open cell holds alone, not those of one the version holds and the fork freed. Its guest's
/// boot is measured against the prediction.
#[tokio::test(flavor = "multi_thread")]
async fn forks_of_a_version_fetch_their_prediction_ahead() {
    use bucket_cell::evidence::UNIT;
    let (base, dir) = cell(64 << 20).await;
    let extent = id(&base, 1);
    write(&base, extent, 0, &pattern(5, 4 * UNIT as usize)).await.unwrap();
    // An extent the version holds, which the second fork frees.
    let freed = id(&base, 2);
    write(&base, freed, 0, &pattern(6, UNIT as usize)).await.unwrap();
    base.commit(None).await.unwrap();
    let at = dir.path().join("version");
    base.uploaded_version_at(&at).await.unwrap();
    drop(base);
    let version = Opening::here().unwrap().for_host().open(&at).await.unwrap();
    let fork = async |name: &str| {
        let path = dir.path().join(name);
        version.fork_to(&path, host_cell::named(5)).await.unwrap();
        path
    };
    let (first, second) = (fork("first").await, fork("second").await);

    // The first boot reads two of the extent's units, and the other extent's.
    let one = OpenCell::open(&first).await.unwrap();
    let now = std::time::Instant::now();
    let at = |ms| now + std::time::Duration::from_millis(ms);
    one.guest_read(extent, 3 * UNIT, 1, at(1));
    one.guest_read(extent, UNIT, 1, at(2));
    one.guest_read(freed, 0, 1, at(3));
    one.close().await;
    let worked_on = Opening::here().unwrap().for_host().open(&second).await.unwrap();
    worked_on.free(freed).unwrap();
    worked_on.commit(None).await.unwrap();
    worked_on.close().await;

    // The next, opened where the host keeps the parts it fetches, fetches the two ahead.
    crate::evidence::forget();
    let cache =
        host_fs::may_block(|| bucket_cell::cache::Cache::open(&dir.path().join("cache"), 1 << 30))
            .unwrap();
    let cache = Arc::new(cache);
    let here = Opening::here().unwrap();
    let keeping = local_cell::upload::Options { cache: Some(cache.clone()), ..here.keeping };
    let opening = Opening { keeping, ..here };
    let before = stats::stats();
    let two = OpenCell::open_with(&second, opening).await.unwrap();
    two.evidence().unwrap().prefetched().await;
    assert_eq!(stats::stats().since(&before).prefetches, 2);
    assert_ne!(cache.bytes(), 0);

    // Its boot reads one of them, and a unit not predicted.
    two.guest_read(extent, 3 * UNIT, 1, at(1));
    two.guest_read(extent, 2 * UNIT, 1, at(2));
    two.close().await;
    let counted = stats::stats().since(&before);
    let measured = (counted.predicted_read, counted.predicted_unread, counted.read_unpredicted);
    assert_eq!(measured, (1, 2, 1));
    version.close().await;
}

/// cladefs: freeing an extent maps zeros where the window mapped it, read-only, rather than
/// what kept its pages; others stay.
#[tokio::test(flavor = "multi_thread")]
async fn freeing_an_extent_maps_zeros_in_its_place() {
    let (cell, _dir) = cell(1 << 20).await;
    let ps = page_size();
    let window = cell.create_dax_window(8 * ps, None).unwrap();
    let (sealed, kept) = (id(&cell, 1), id(&cell, 2));
    write(&cell, sealed, 0, &pattern(1, 2 * ps as usize)).await.unwrap();
    write(&cell, kept, 0, &pattern(2, ps as usize)).await.unwrap();
    cell.commit(None).await.unwrap();
    cell.map(sealed, 0, 2 * ps, 0).await.unwrap();
    cell.map(kept, 0, ps, 4 * ps).await.unwrap();
    assert!(window.is_mapped(0) && window.is_mapped(ps));

    let sealed_pages = cached(sealed).unwrap();
    let weak = Arc::downgrade(&sealed_pages);
    drop(sealed_pages);
    cell.free(sealed).unwrap();
    // Mapped, but no longer keeping the extent's pages: the sealed extent is gone.
    assert!(window.is_mapped(0) && window.is_mapped(ps));
    assert!(weak.upgrade().is_none(), "the window still keeps the freed extent's pages");
    assert!(cached(sealed).is_none());
    // Zeros, read-only: an open cell faults rather than writing them.
    assert_eq!(block_on(window.fault(0, Access::Write)), Err(Errno::EACCES));
    block_on(window.fault(0, Access::Read)).unwrap();
    // SAFETY: The test faulted the pages in, and nothing else accesses them.
    let bytes = unsafe { std::slice::from_raw_parts(window.host_base(), ps as usize) };
    assert!(bytes.iter().all(|byte| *byte == 0), "a freed extent's range doesn't read zeros");
    assert!(window.is_mapped(4 * ps), "an extent not freed was unmapped");
}

/// TASK-636 (board 2702), TASK-550 (board 2496), TASK-582 (board 2560): every mutating request on
/// an `OpenCell` passes through `admit(Changing::*)`, so halted (`EIO`), shredded (`EACCES`),
/// read-only/version (`EROFS`), and fenced (`EROFS`) stores refuse every mutation with the right
/// errno, and `commit_local` fails once unuploaded commits pass the lag bound.
#[tokio::test(flavor = "multi_thread")]
async fn standing_gate_refuses_every_mutation_when_halted_shredded_fenced_or_readonly() {
    use crate::dax::SecondStage;
    struct DummyStage;
    impl SecondStage for DummyStage {
        fn map(&self, _: u64, _: crate::slabs::PhysLocation, _: u64) {}
        fn unmap(&self, _: u64, _: u64, _: crate::dax::Private) {}
    }

    async fn check_mutations(
        cell: &OpenCell,
        dir: &Path,
        label: &str,
        expected_contents: Errno,
        expected_capacity: Option<Errno>,
        expected_cell: Option<Errno>,
    ) {
        let ext = id(cell, 99);
        let base_dir = dir.join("dummy-base");
        let fork_dir = dir.join(format!("fork-{label}"));
        let child = crate::host_cell::fork_child(cell.cell());
        let barriers =
            crate::WriteBarriers { before: false, after: Some(crate::Commit::Ordered), root: None };
        let stage: Arc<dyn SecondStage> = Arc::new(DummyStage);
        let mut cases: Vec<(&str, Option<Errno>, Errno)> = vec![
            ("write", write(cell, ext, 0, &[1; BLOCK]).await.err(), expected_contents),
            (
                "write_with_barriers",
                cell.write_with_barriers(
                    ext,
                    0,
                    &[VolatileSlice::from(&[1u8; BLOCK][..])],
                    barriers,
                )
                .await
                .err(),
                expected_contents,
            ),
            ("free", local_cell::upload::blocking(|| cell.free(ext)).err(), expected_contents),
            ("commit", cell.commit(None).await.err(), expected_contents),
            ("commit_local", cell.commit_local(None).await.err(), expected_contents),
            ("commit_ordered", cell.commit_ordered(None).await.err(), expected_contents),
            (
                "create_copying_dax_window",
                cell.create_copying_dax_window(page_size(), stage).err().and_then(|e| e.errno()),
                expected_contents,
            ),
        ];
        if let Some(expected) = expected_capacity {
            cases.push(("set_capacity", cell.set_capacity(2 << 20).await.err(), expected));
        }
        if let Some(expected) = expected_cell {
            cases.extend([
                (
                    "make_version",
                    cell.make_version(None).await.err().and_then(|e| e.errno()),
                    expected,
                ),
                (
                    "drop_versions",
                    cell.drop_versions(&[1].into()).await.err().and_then(|e| e.errno()),
                    expected,
                ),
                (
                    "share_version",
                    cell.share_version(1, true).await.err().and_then(|e| e.errno()),
                    expected,
                ),
                (
                    "fork_to",
                    cell.fork_to(&fork_dir, child).await.err().and_then(|e| e.errno()),
                    expected,
                ),
                (
                    "attach_base",
                    local_cell::upload::blocking(|| cell.attach_base(&base_dir, &[1])).err(),
                    expected,
                ),
                (
                    "make_public",
                    local_cell::upload::blocking(|| cell.make_public()).err(),
                    expected,
                ),
                (
                    "make_public_attach_only",
                    local_cell::upload::blocking(|| cell.make_public_attach_only()).err(),
                    expected,
                ),
            ]);
        }
        for (op, actual, expected) in cases {
            assert_eq!(actual, Some(expected), "{label}: {op}");
        }
    }

    // 1. Halted cell -> EIO on every mutation.
    let (halted, dir) = cell(1 << 20).await;
    halted.halt.halt(|| "halted by test".to_owned());
    check_mutations(&halted, dir.path(), "halted", Errno::EIO, Some(Errno::EIO), Some(Errno::EIO))
        .await;
    halted.close().await;

    // 2. Shredded cell -> EACCES on every mutation.
    let (shredded, dir) = cell(1 << 20).await;
    crate::bucket::forget(bucket_cell::keys::Scope::Cell(shredded.cell()), "shredded by test");
    check_mutations(
        &shredded,
        dir.path(),
        "shredded",
        Errno::EACCES,
        Some(Errno::EACCES),
        Some(Errno::EACCES),
    )
    .await;
    shredded.close().await;

    // 3. Public base (read-only contents) -> EPERM on every Contents mutation.
    let (public, dir) = cell(1 << 20).await;
    let public_path = dir.path().join("store");
    local_cell::upload::blocking(|| public.make_public()).unwrap();
    public.close().await;
    let ro = OpenCell::open(&public_path).await.unwrap();
    check_mutations(&ro, dir.path(), "public", Errno::EPERM, None, None).await;
    ro.close().await;

    // 4. Version (read-only contents and capacity) -> EROFS on Contents and Capacity mutations.
    let (base, dir) = cell(1 << 20).await;
    base.commit(None).await.unwrap();
    let version_path = dir.path().join("version");
    base.uploaded_version_at(&version_path).await.unwrap();
    let version = Opening::here().unwrap().for_host().open(&version_path).await.unwrap();
    check_mutations(&version, dir.path(), "version", Errno::EROFS, Some(Errno::EROFS), None).await;
    version.close().await;
    base.close().await;

    // 4. Fenced cell (another host took its bucket head) -> EROFS on mutating requests.
    let bucket_dir = tempfile::tempdir().unwrap();
    let bucket_path = bucket_dir.path().join("bucket");
    std::fs::create_dir(&bucket_path).unwrap();
    let url = format!("file://{}", bucket_path.display());
    let first_path = bucket_dir.path().join("first");
    let second_path = bucket_dir.path().join("second");
    let first =
        take_in_bucket(&first_path, crate::host_cell::CellOf::New(0), 1 << 20, &url, 1).await;
    let cell_id = first.cell();
    // Second host takes the same cell at token 2 and writes a commit, fencing `first`.
    let second_cell =
        take_in_bucket(&second_path, crate::host_cell::CellOf::Taken(cell_id), 1 << 20, &url, 2)
            .await;
    write(&second_cell, id(&second_cell, 1), 0, &[2; BLOCK]).await.unwrap();
    second_cell.commit(None).await.unwrap();
    second_cell.wait_uploaded().await.unwrap();
    second_cell.close().await;
    // Trigger `first`'s uploader to observe the fence.
    let _ = first.commit(None).await;
    check_mutations(
        &first,
        bucket_dir.path(),
        "fenced",
        Errno::EROFS,
        Some(Errno::EROFS),
        Some(Errno::EROFS),
    )
    .await;
    first.close().await;
}

/// Creates and opens a cell directory at `path` kept in `bucket` at `token`.
async fn take_in_bucket(
    path: &std::path::Path,
    of: crate::host_cell::CellOf,
    capacity: u64,
    bucket: &str,
    token: u64,
) -> OpenCell {
    let host = crate::Host::process();
    let mut keeping = Opening::here().unwrap().keeping;
    keeping.token = token;
    let opened = crate::host_cell::create_unmade_on(
        host,
        path,
        capacity,
        of,
        bucket_cell::cell::Kind::Fxfs,
        Some(bucket),
        keeping.clone(),
    )
    .await
    .unwrap();
    local_cell::upload::blocking(|| opened.made()).unwrap();
    opened.close().await;
    local_cell::upload::blocking(|| local_cell::place_host_on(host.local(), path, token, false))
        .unwrap();
    Opening::keeping(keeping).for_host().open(path).await.unwrap()
}
