// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! An image's store kept in its bucket (backlog doc-14, TASK-165.6): as every store's cell is,
//! in the bucket its description names, an image's own file bucket where it was made on no host
//! of cladefs's system (`host_cell::create`). An uploader takes the cell's head and uploads the
//! image's commits behind them, as the extent daemon's stores' are (`crate::bucket`); the image's
//! commits are durable once in its store, as before, and closing it waits for them to be uploaded.
//! An image's fork forks its cell in the bucket, as the daemon's forks do (`host_cell::fork`).
//! An image's own bucket's collector runs in the process, after commits ([`super::collect`]); a
//! host's bucket's is the host's.

use super::collect::{self, Collecting};
use crate::bucket;
use crate::error::{Error, anyhow_of};
use crate::host_cell::{self, CellOf, Opened};
use bucket_cell::cache::Cache;
use bucket_cell::cell::Kind;
use bucket_cell::layout::Child;
use local_cell::upload::{self, Uploader};
use local_cell::{HostDir, LocalCell};
use std::path::Path;
use std::sync::Arc;

/// An image's store, its cell kept in its bucket. See the module docs.
pub(crate) struct KeptImage {
    store: Arc<LocalCell<HostDir>>,
    /// The cell's uploader, which opened the store: dropped, it closes the store's open segment,
    /// uploads what the store made durable, then stops (TASK-192).
    uploader: Uploader<HostDir>,
    /// What reads the store's extents whose data is in the bucket.
    fetcher: Arc<bucket::HostFetcher>,
    /// The bucket's collector in this process, if it is the image's own, which no host's collector
    /// collects.
    collecting: Option<Arc<Collecting>>,
}

impl KeptImage {
    /// Makes a new image at `dir`, which must not exist, holding at most `capacity` bytes, of the
    /// cell `of` says ([`host_cell::create`]): whole if `whole`, or else unmade until its maker says it
    /// is ([`host_cell::create_unmade`], TASK-196), as an import's, once filled. Its uploader
    /// keeps its cell as `keeping` says: in a host's daemon, with the host's cache of the
    /// bucket's objects, which what the bucket acknowledged goes into, and its extents are read
    /// from (TASK-237); elsewhere, with none ([`host_cell::keeping`]).
    pub fn create(
        dir: &Path,
        (capacity, of, whole): (u64, CellOf, bool),
        keeping: upload::Options,
    ) -> anyhow::Result<Self> {
        let cache = keeping.cache.clone();
        let opened = host_cell::create_unmade(dir, capacity, of, Kind::Fxfs, None, keeping);
        let opened = opened.map_err(anyhow_of)?;
        if whole {
            opened.made()?;
        }
        Self::opened(opened, cache)
    }

    /// The image at `dir`, or the fork at `dir`, a store of its own holding its cell alone
    /// (TASK-165.7): its uploader takes its cell's head as a copy never placed takes it where
    /// this process is no host's daemon ([`host_cell::keeping`]).
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        let keeping = host_cell::keeping().map_err(anyhow_of)?;
        Self::opened(host_cell::open(dir, keeping).map_err(anyhow_of)?, None)
    }

    /// `opened`, its cell taken in its bucket, its extents there read through `cache`, if given.
    fn opened(opened: Opened, cache: Option<Arc<Cache>>) -> anyhow::Result<Self> {
        let Opened { store, bucket, uploader } = opened;
        let url = store.bucket().unwrap_or_default().to_owned();
        let fetcher = bucket::HostFetcher::new(bucket.clone(), cache).map_err(anyhow_of)?;
        let own = host_cell::keeps_own_bucket(&store);
        let collecting = own.then(|| collect::of(&url, &bucket));
        Ok(Self { store, uploader, fetcher, collecting })
    }

    pub fn store(&self) -> &Arc<LocalCell<HostDir>> {
        &self.store
    }

    /// What reads the store's extents whose data is in the bucket, as a fork's inherited extents
    /// are (TASK-165.7).
    pub fn fetcher(&self) -> Arc<bucket::HostFetcher> {
        self.fetcher.clone()
    }

    /// What follows a commit of the store's: it fails once the uploader has, the cell another
    /// writer's or deleted, or its bucket out of reach for good, rather than the image go on as
    /// a cell no bucket has; it waits while more of its commits wait to be uploaded than the
    /// lag's bound; and the bucket's collector runs, if it is due.
    pub fn committed(&self) -> anyhow::Result<()> {
        if let Some(failed) = self.uploader.failed() {
            return Err(anyhow::Error::new(failed).context("the image's cell is not uploaded"));
        }
        self.uploader.wait_lag(self.store.last_commit()?)?;
        if let Some(collecting) = &self.collecting {
            collecting.after_commit();
        }
        Ok(())
    }

    /// Waits until the store's commits are uploaded: so a closed image's cell is whole in its
    /// bucket.
    pub fn wait_uploaded(&self) -> anyhow::Result<()> {
        Ok(self.uploader.wait_uploaded(self.store.last_commit()?)?)
    }

    /// Forks the store's cell, as of its last commit, to a new cell, the one cell of a new
    /// cell of its tenant's (TASK-364), whose handle is a new directory at `path`, in its bucket
    /// too ([`host_cell::fork`]): `held` is let go of once the fork's commit is made.
    pub fn fork<H>(&self, path: &Path, held: H) -> Result<(), Error> {
        let tenant = self.store.description().cell.tenant;
        let cell = host_cell::new_cell(self.uploader.bucket(), tenant).map_err(Error::from)?;
        let child = Child { cell, owner: rand::random() };
        host_cell::fork(&self.store, &self.uploader, path, child, |_| Ok(()), held)
    }

    /// The bucket's collector in this process, if it is the image's own.
    #[cfg(test)]
    pub fn collecting(&self) -> Option<&Arc<Collecting>> {
        self.collecting.as_ref()
    }
}

/// Deletes the image at `path`, or the fork at `path`, which nothing in this process has open
/// ([`host_cell::delete`]): its cell, in the bucket its description names, first, as
/// a copy never placed deletes it where this process is no host's daemon ([`host_cell::token`]).
pub(crate) fn delete(path: &Path) -> Result<(), Error> {
    let bucket = host_cell::bucket_at(path).map_err(Error::new)?;
    host_cell::delete(path, bucket.as_deref(), host_cell::token)
}

#[cfg(test)]
mod tests {
    use super::super::collect::quicken;
    use super::super::model_tests::{BUSY, Files, holds};
    use super::super::{Kind, Storage};
    use super::*;
    use crate::{Filesystem, Owner};
    use bucket_cell::object_bucket::ObjectBucket;
    use bucket_cell::record::Kind as Record;
    use futures::executor::block_on;
    use rand::rngs::StdRng;
    use rand::{RngExt as _, SeedableRng as _};
    use std::collections::BTreeMap;
    use storage_device::ExtentId;

    const OWNER: Owner = Owner { uid: 1000, gid: 100 };
    const SIZE: u64 = 64 << 20;
    const MIB: usize = 1 << 20;

    /// The image `storage` is of.
    fn image(storage: &Storage) -> &Arc<KeptImage> {
        let Kind::Image { image, .. } = &storage.kind else { panic!("memory") };
        image
    }

    /// A formatted image at `path`, holding `file` with `contents`, closed.
    async fn image_with(path: &Path, file: &[u8], contents: &[u8]) -> Storage {
        let fs = Filesystem::format(Storage::create(path, SIZE).unwrap()).await.unwrap();
        let (node, _) = fs.root().create(file, 0o644, OWNER).await.unwrap();
        node.write(0, contents).await.unwrap();
        fs.close().await.unwrap()
    }

    /// The bucket the store at `path` names.
    fn bucket_of(path: &Path) -> Arc<ObjectBucket> {
        host_cell::bucket_at(path).unwrap().expect("a bucket")
    }

    /// The extents `cell`'s state in `bucket` holds, by their lengths, those it inherited too,
    /// and its root.
    fn kept(bucket: &ObjectBucket, cell: crate::CellId) -> (BTreeMap<ExtentId, u64>, Vec<u8>) {
        let recovered = local_cell::upload::block_on(async {
            let record = bucket_cell::lifecycle::record(bucket, cell).await?;
            bucket_cell::recover(bucket, cell, record.and_then(|record| record.origin())).await
        });
        let state = recovered.unwrap().unwrap().state;
        let extents = state.extents.iter().map(|(extent, at)| (*extent, at.len)).collect();
        (extents, state.root.unwrap_or_default())
    }

    /// The extents `image`'s store holds, by their lengths, and its root.
    fn held(image: &KeptImage) -> (BTreeMap<ExtentId, u64>, Vec<u8>) {
        let store = image.store();
        (store.extents().unwrap().into_iter().collect(), store.root().unwrap())
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
    /// (TASK-165.7), which its description names: closed, the cell holds what its store does.
    #[test]
    fn images_keep_their_cells_in_a_file_bucket_of_their_own() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image");
        let storage = block_on(image_with(&path, b"file", b"contents"));
        let own = dir.path().canonicalize().unwrap().join("image.bucket");
        let url = format!("file://{}/", own.display());
        assert_eq!(local_cell::host_bucket(&path).unwrap(), Some(url));
        let image = image(&storage);
        let cell = image.store().description().cell;
        let (extents, root) = kept(&bucket_of(&path), cell);
        assert!(!extents.is_empty() && !root.is_empty());
        assert_eq!((extents, root), held(image));
    }

    /// TASK-165.6: an image's fork forks the image's cell in the image's bucket, where it keeps
    /// its own: a cell of its own there, whose record names the image's as its parent, and which
    /// holds what the fork's store does, closed.
    #[test]
    fn forks_cell_in_their_images_bucket() {
        block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let (path, fork_path) = (dir.path().join("image"), dir.path().join("fork"));
            let storage = image_with(&path, b"file", b"original").await;
            let parent = image(&storage).store().description().cell;
            let fork = storage.fork_to(&fork_path).await.unwrap();
            let child = image(&fork).store().description().cell;
            let bucket = bucket_of(&fork_path);
            assert!(Arc::ptr_eq(&bucket, &bucket_of(&path)), "the fork's bucket is another");
            let record = bucket_cell::lifecycle::record(&*bucket, child).await.unwrap().unwrap();
            let Record::Fork(origin) = record.kind else { panic!("{record:?}") };
            assert_eq!(origin.parent, parent);

            let fs = Filesystem::mount(fork).await.unwrap();
            let (file, _) = fs.root().lookup(b"file").await.unwrap();
            file.write(0, b"modified").await.unwrap();
            let fork = fs.close().await.unwrap();
            assert_eq!(kept(&bucket, child), held(image(&fork)));
            assert_ne!(kept(&bucket, child), kept(&bucket, parent));
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
            let fork = storage.fork_to(&fork_path).await.unwrap();
            let child = image(&fork).store().description().cell;
            drop((fork, storage));
            let bucket = bucket_of(&path);
            let record = |cell| bucket_cell::lifecycle::record(&*bucket, cell);
            assert!(!record(child).await.unwrap().unwrap().deleted);
            Storage::delete(&fork_path).unwrap();
            assert!(record(child).await.unwrap().unwrap().deleted);
        })
    }

    /// TASK-165.6: an image's commit starts a pass of its bucket's collector, on a thread of its
    /// own, which the image's forks share; the next commits, within a minute of it, start none.
    #[test]
    fn commits_start_the_collector() {
        block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("image");
            let storage = Storage::create(&path, SIZE).unwrap();
            let collecting = image(&storage).collecting().expect("its own bucket's").clone();
            assert_eq!(collecting.passes(), 0);
            let fs = Filesystem::format(storage).await.unwrap();
            assert_eq!(collecting.passes(), 1);
            let (node, _) = fs.root().create(b"file", 0o644, OWNER).await.unwrap();
            node.write(0, b"contents").await.unwrap();
            let storage = fs.close().await.unwrap();
            assert_eq!(collecting.passes(), 1);
            let fork = storage.fork_to(dir.path().join("fork")).await.unwrap();
            let shared = image(&fork).collecting().unwrap();
            assert!(Arc::ptr_eq(shared, &collecting), "the fork's collector is another");
        })
    }

    /// TASK-165.6: an image dropped has let go of its store, which this process opens again at
    /// once, as the extent daemon does a store an import made: its uploader's task, held up by the
    /// failpoint after it said it stopped, keeps nothing of it open. That task said so before it let
    /// go of the store, and the daemon found the import's store still open (`EIO`, CI 42119298585).
    #[test]
    fn images_dropped_let_go_of_their_stores() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image");
        let scenario = fail::FailScenario::setup();
        fail::cfg("upload::stopped", "sleep(500)").unwrap();
        drop(Storage::create(&path, SIZE).unwrap());
        let reopened = host_cell::open(&path, host_cell::keeping().unwrap())
            .map(|opened| opened.store.description().cell);
        scenario.teardown();
        let cell = reopened.map_err(|err| format!("{err:#}"));
        assert_eq!(cell.map(|_| ()), Ok(()));
    }

    /// TASK-165.6: an image kept in a host's bucket, as a store the extent daemon made is, which
    /// this process opens, has no collector here: the host's collects it.
    #[test]
    fn images_in_a_hosts_bucket_are_not_collected_here() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store");
        let keeping = host_cell::keeping().unwrap();
        let made = host_cell::create_unmade(
            &path,
            SIZE,
            CellOf::Named(bucket_cell::layout::test_cell(7)),
            bucket_cell::cell::Kind::Fxfs,
            Some("memory:///host"),
            keeping,
        );
        made.unwrap().made().unwrap();
        let image = KeptImage::open(&path).unwrap();
        assert!(image.collecting().is_none());
        image.store().write(ExtentId::new(7, 1), 0, &[1; 4096]).unwrap();
        image.store().commit(None).unwrap();
        image.committed().unwrap();
        image.wait_uploaded().unwrap();
        assert_eq!(
            kept(&bucket_of(&path), bucket_cell::layout::test_cell(7)).0,
            BTreeMap::from([(ExtentId::new(7, 1), 4096)])
        );
    }

    /// TASK-165.6: what an image freed, uploaded, and checkpointed as its uploader stops, a pass of
    /// its bucket's collector reclaims; what it holds, the cell still holds.
    #[test]
    fn the_collector_reclaims_what_images_freed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image");
        let image = KeptImage::create(
            &path,
            (SIZE, CellOf::Named(bucket_cell::layout::test_cell(7)), true),
            host_cell::keeping().unwrap(),
        )
        .unwrap();
        let extent = |sequence| ExtentId::new(7, sequence);
        // Each stored on its own, in its bucket: 5 MiB, past the size for it.
        let data = crate::test_data::noise(1, 5 * MIB);
        for sequence in 0..4 {
            image.store().write(extent(sequence), 0, &data).unwrap();
        }
        image.store().commit(None).unwrap();
        image.committed().unwrap();
        // Uploaded before they are freed: a segment of the commits sealing and freeing them both
        // would hold none of them.
        image.wait_uploaded().unwrap();
        for sequence in 0..3 {
            image.store().free(extent(sequence)).unwrap();
        }
        image.store().commit(Some(b"root")).unwrap();
        image.committed().unwrap();
        image.wait_uploaded().unwrap();
        drop(image);

        let own = dir.path().join("image.bucket");
        let before = bucket_bytes(&own);
        assert!(before >= 20 * MIB as u64, "{before} bytes");
        let image = KeptImage::open(&path).unwrap();
        let report = image.collecting().unwrap().pass_now();
        assert!(report.deleted >= 3, "{report:?}");
        let after = bucket_bytes(&own);
        assert!(after < 6 * MIB as u64, "{after} bytes");
        let (extents, root) = kept(&bucket_of(&path), bucket_cell::layout::test_cell(7));
        assert_eq!(
            (extents, root),
            (BTreeMap::from([(extent(3), 5 * MIB as u64)]), b"root".into())
        );
    }

    /// `rounds` rounds, as `seed` says, of files of `fs`, which holds `files`, made, written over
    /// and past their ends, and removed, each written synced, a commit; and each round's end, the
    /// files checked. Files of 1 to 6 MiB of noise: those past 4 MiB in objects of their own, the
    /// rest in segments, which their files' writing over half frees.
    async fn churn(fs: &Filesystem, files: &mut Files, seed: u64, rounds: usize) {
        let mut rng = StdRng::seed_from_u64(seed);
        for round in 0..rounds {
            for step in 0..4 {
                let data = crate::test_data::noise(rng.random(), rng.random_range(1..=6) * MIB);
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
    /// compact and delete at once: after each round each holds what was written to it, the fork
    /// what it inherited too; and opened again, after a pass alone, the same.
    #[test]
    fn passes_run_among_an_image_and_its_forks_commits() {
        quicken(BUSY);
        let dir = tempfile::tempdir().unwrap();
        let (path, fork_path) = (dir.path().join("image"), dir.path().join("fork"));
        let storage = Storage::create(&path, SIZE).unwrap();
        let collecting = image(&storage).collecting().unwrap().clone();
        let mut files = Files::new();
        let (fs, fork) = block_on(async {
            let fs = Filesystem::format(storage).await.unwrap();
            churn(&fs, &mut files, 1, 2).await;
            let fork = Filesystem::mount(fs.fork_storage_to(&fork_path).await.unwrap());
            (fs, fork.await.unwrap())
        });
        let mut forked = files.clone();
        std::thread::scope(|scope| {
            scope.spawn(|| block_on(churn(&fork, &mut forked, 2, 6)));
            block_on(churn(&fs, &mut files, 3, 6));
        });
        block_on(async {
            drop(fork.close().await.unwrap());
            drop(fs.close().await.unwrap());
        });
        // The passes among the commits compacted and deleted.
        let (passes, collected) = (collecting.passes(), collecting.collected());
        assert!(passes > 4, "{passes} passes");
        assert!(collected.compacted > 0 && collected.deleted > 0, "{collected:?}, {passes} passes");
        drop(collecting);
        let report = image(&Storage::open(&path).unwrap()).collecting().unwrap().pass_now();
        assert!(report.deleted > 0, "{report:?}");
        block_on(async {
            for (path, files) in [(&path, &files), (&fork_path, &forked)] {
                let fs = Filesystem::mount(Storage::open(path).unwrap()).await.unwrap();
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
        block_on(async {
            let fs = Filesystem::format(Storage::create(&path, SIZE).unwrap()).await.unwrap();
            churn(&fs, &mut files, 1, 2).await;
            let fork = Filesystem::mount(fs.fork_storage_to(&fork_path).await.unwrap());
            let fork = fork.await.unwrap();
            forked = files.clone();
            churn(&fork, &mut forked, 2, 1).await;
            // Passes held up from the next commit's on: both closed and dropped as one runs.
            fail::cfg("collect::pass", "sleep(500)").unwrap();
            churn(&fork, &mut forked, 3, 1).await;
            drop(fork.close().await.unwrap());
            drop(fs.close().await.unwrap());
        });
        // Opened again, they share a collector this holds, which counts what their commits'
        // passes, still held up, delete as they are closed and dropped again.
        let mut collecting = None;
        block_on(async {
            for (path, files) in [(&path, &files), (&fork_path, &forked)] {
                let storage = Storage::open(path).unwrap();
                collecting.get_or_insert_with(|| image(&storage).collecting().unwrap().clone());
                let fs = Filesystem::mount(storage).await.unwrap();
                holds(&fs, files, &format!("{} opened again", path.display())).await;
                drop(fs.close().await.unwrap());
            }
        });
        scenario.teardown();
        let collecting = collecting.unwrap();
        let (started, alone) = (collecting.collected(), collecting.pass_now());
        assert!(started.deleted + alone.deleted > 0, "{started:?}, then alone {alone:?}");
        assert_eq!(collecting.collected_alone(), Vec::<String>::new(), "collected alone");
        drop(collecting);
        block_on(async {
            let fs = Filesystem::mount(Storage::open(&fork_path).unwrap()).await.unwrap();
            holds(&fs, &forked, "the fork after the pass").await;
            drop(fs.close().await.unwrap());
        });
    }
}
