// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: an open cell as an Fxfs device (`storage_device::Device`), the device of an image this
//! process mounts: an import's, the daemon's FUSE mount of cells', a host
//! tool's, a SQLite image's. Its reads, writes, frees and commits are the open cell's, as a guest's
//! requests of the extent daemon are (`cladefsd::device::DaemonDevice` is this over a
//! connection to the daemon): through the cell's page cache (`local_cell::pages`), under the one
//! memory budget, and nothing of its own beside it (board 8260).
//!
//! - Its reads are the cell's own, not the extent cache's (`OpenCell::read_uncached`): the
//!   filesystem mounted on it caches what it reads in its kernel's pages, which the extent cache
//!   would hold again, as windows' guests need it to and it does not; read through the extent
//!   cache, reading an image's large file back took 2.4 times as long. Unpacked 1 MiB parts are
//!   retained in the unified `bucket_cell::fetch::PartMemory` LRU, which the daemon's governor
//!   scales to half the remaining room while FUSE cells are attached so multi-file reads don't
//!   thrash zstd decompression (board 12313).
//!
//! - Flushing commits the cell, synced to the host's disk (`COMMIT_LOCAL`), within its uploader's
//!   lag; closing commits it uploaded, as a closed image's cell is whole in its bucket.
//! - Its commits run on the bucket runtime, off Fxfs's executor threads, as their disk work's
//!   `block_in_place` hands that runtime's other tasks on (board 5614).
//! - A commit starts a pass of the collector of the image's own bucket, if it is due (`collect`).
//! - The open cell's errnos are told back as the errors Fxfs looks for: a read of an extent the cell
//!   holds not (`ENOENT`) as [`NoExtent`], a write of one it may not write (`EPERM`) as
//!   [`SealedExtent`], which Fxfs writes again unpacked.

use super::collect::Collecting;
use anyhow::{Context as _, Error, ensure};
use async_trait::async_trait;
use cladefs_core::{Commit, Errno, OpenCell, VolatileSlice};
use std::sync::Arc;
use storage_device::buffer::{BufferFuture, BufferRef, MutableBufferRef, TransferBudget};
use storage_device::{Device, ExtentId, NoExtent, SealedExtent, WriteFlags, WriteOptions};

/// The most bytes of transfer buffers alive at once.
const TRANSFER_BUDGET: usize = 32 << 20;

/// An open cell, as a device. See the module docs.
pub struct OpenCellDevice {
    cell: Arc<OpenCell>,
    /// The collector of the image's own bucket in this process, if it keeps its cell in one.
    collecting: Option<Arc<Collecting>>,
    allocator: TransferBudget,
    block_count: u64,
    /// cladefs: what holds its cell, if given ([`OpenCellDevice::holding`]): dropped after `cell`,
    /// as fields drop in order, so that it lets the cell go last, closing it.
    #[allow(dead_code, reason = "held for its drop")]
    holder: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

impl OpenCellDevice {
    /// `cell` as a device, whose commits start passes of `collecting`, if given.
    pub fn new(cell: Arc<OpenCell>, collecting: Option<Arc<Collecting>>) -> Result<Self, Error> {
        let block_size = OpenCell::BLOCK_SIZE;
        let capacity = cell.info().map_err(Error::new).context("the cell's capacity")?.capacity;
        let allocator = TransferBudget::new(block_size as usize, TRANSFER_BUDGET);
        let block_count = capacity / block_size as u64;
        Ok(Self { cell, collecting, allocator, block_count, holder: None })
    }

    /// cladefs: `cell` as a device, which `holder` holds open and closes once let go: kept as long
    /// as the device, and let go after it lets `cell` go (cladefsd's embedded cells, board 13909).
    pub fn holding(
        cell: Arc<OpenCell>,
        holder: Arc<dyn std::any::Any + Send + Sync>,
    ) -> Result<Self, Error> {
        Ok(Self { holder: Some(holder), ..Self::new(cell, None)? })
    }

    /// Commits the cell as `commit` says, `root` its root if given, on the bucket runtime; then
    /// starts a pass of the image's own bucket's collector, if one is due.
    async fn commit(&self, root: Option<&[u8]>, commit: Commit) -> Result<(), Error> {
        let (cell, root) = (self.cell.clone(), root.map(<[u8]>::to_vec));
        let committing = async move {
            match commit {
                Commit::Uploaded => cell.commit(root.as_deref()).await,
                Commit::Local => cell.commit_local(root.as_deref()).await,
                Commit::Ordered => cell.commit_ordered(root.as_deref()).await,
            }
        };
        let runtime = local_cell::upload::bucket_runtime_on(self.cell.host.local())?;
        // cladefs: `unconstrained` prevents coop-budget livelock when polled from `block_on` inside
        // a tokio task (board 6525).
        cladefs_core::bucket::unconstrained(runtime.spawn(committing)).await??;
        if let Some(collecting) = &self.collecting {
            collecting.after_commit();
        }
        Ok(())
    }
}

#[async_trait]
impl Device for OpenCellDevice {
    fn allocate_buffer(&self, size: usize) -> BufferFuture<'_> {
        self.allocator.allocate_buffer(size)
    }

    fn block_size(&self) -> u32 {
        OpenCell::BLOCK_SIZE
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }

    /// Blocks within the extent never written, and past its end, read as zeros.
    async fn read(
        &self,
        extent: ExtentId,
        offset: u64,
        mut buffer: MutableBufferRef<'_>,
    ) -> Result<(), Error> {
        let bytes = buffer.as_mut_slice();
        match self.cell.read_uncached(extent, offset, bytes).await {
            Ok(()) => Ok(()),
            Err(Errno::ENOENT) => Err(NoExtent(extent).into()),
            Err(errno) => Err(Error::new(errno).context(format!("reading {extent:?} at {offset}"))),
        }
    }

    async fn write_with_opts(
        &self,
        extent: ExtentId,
        offset: u64,
        buffer: BufferRef<'_>,
        write_opts: WriteOptions<'_>,
    ) -> Result<(), Error> {
        let flags = write_opts.flags;
        ensure!(
            (flags - (WriteFlags::PRE_BARRIER | WriteFlags::POST_BARRIER)).is_empty(),
            "write flags {flags:?}"
        );
        if flags.contains(WriteFlags::PRE_BARRIER) {
            self.commit(None, Commit::Ordered).await?;
        }
        let bytes = buffer.as_slice();
        let written = match self.cell.write(extent, offset, &[VolatileSlice::from(bytes)]).await {
            Ok(()) => Ok(()),
            Err(Errno::EPERM) => Err(Error::new(SealedExtent(extent)).context(Errno::EPERM)),
            Err(errno) => Err(Error::new(errno)),
        };
        written.with_context(|| format!("writing {extent:?} at {offset}"))?;
        if flags.contains(WriteFlags::POST_BARRIER) {
            self.commit(write_opts.root, Commit::Local).await?;
        }
        Ok(())
    }

    fn cell(&self) -> u64 {
        self.cell.cell().id
    }

    async fn next_sequence(&self) -> Result<u64, Error> {
        Ok(self.cell.info().map_err(Error::new)?.next_sequence)
    }

    async fn sequence_floor(&self) -> Result<u64, Error> {
        Ok(self.cell.info().map_err(Error::new)?.sequence_floor)
    }

    async fn free(&self, extent: ExtentId) -> Result<(), Error> {
        self.cell.free(extent).map_err(Error::new)
    }

    async fn root(&self) -> Result<Vec<u8>, Error> {
        self.cell.root().map_err(Error::new)
    }

    async fn commit_root(&self, root: &[u8]) -> Result<(), Error> {
        self.commit(Some(root), Commit::Local).await.context("committing the image's root")
    }

    /// Commits, uploaded: a closed image's cell is whole in its bucket.
    async fn close(&self) -> Result<(), Error> {
        self.commit(None, Commit::Uploaded).await.context("uploading the image's commits")
    }

    async fn flush(&self) -> Result<(), Error> {
        self.commit(None, Commit::Local).await.context("flushing the image")
    }

    fn atomic_commits(&self) -> bool {
        // Each ordered commit seals prior writes together. The cell's WAL recovers a prefix of
        // whole commits, including the data, journal and root committed by a POST_BARRIER.
        true
    }

    fn is_read_only(&self) -> bool {
        self.cell.access().may_write().is_err()
    }
}

#[cfg(test)]
mod tests {
    use super::super::Storage;
    use super::*;
    use futures::executor::block_on;
    use std::path::PathBuf;

    const BLOCK: u32 = cladefs_core::host_cell::BLOCK_SIZE;
    const MIB: usize = 1 << 20;

    /// A new directory, the storage of a new image in it holding `capacity` bytes, and the
    /// image's path: in that order, so that a test's bindings of them drop the storage before the
    /// directory, as a pass of its bucket's collector may run as it drops, which waits for it, and
    /// which would retry for ever the requests of a bucket gone (TASK-428).
    async fn image(capacity: u64) -> (tempfile::TempDir, Storage, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image");
        (dir, Storage::create(&path, capacity).await.unwrap(), path)
    }

    fn extent(device: &dyn Device, sequence: u64) -> ExtentId {
        ExtentId::new(device.cell(), sequence)
    }

    /// Bytes each their offset modulo 251, from `offset`.
    fn pattern(offset: usize, len: usize) -> Vec<u8> {
        (offset..offset + len).map(|i| (i % 251) as u8).collect()
    }

    async fn write(device: &dyn Device, extent: ExtentId, offset: u64, data: &[u8]) {
        let mut buffer = device.allocate_buffer(data.len()).await;
        buffer.as_mut().copy_from_slice(data);
        device.write(extent, offset, buffer.as_ref()).await.unwrap();
    }

    async fn read(device: &dyn Device, extent: ExtentId, offset: u64, len: usize) -> Vec<u8> {
        let mut buffer = device.allocate_buffer(len).await;
        device.read(extent, offset, buffer.as_mut()).await.unwrap();
        let mut data = vec![0; len];
        buffer.as_ref().copy_to_slice(&mut data);
        data
    }

    /// What was written reads back, many writes at once and one large enough for a file of its
    /// own, flushed or not; blocks never written read as zeros; and an extent the cell holds not
    /// fails a read with [`NoExtent`], which Fxfs reads as zeros where an extent may be unwritten.
    #[tokio::test(flavor = "multi_thread")]
    async fn reads_what_was_written() {
        let (_dir, storage, _) = image(64 * MIB as u64).await;
        let device = storage.into_device();
        let device: &dyn Device = &**device;
        assert_eq!(device.block_count(), 64 * MIB as u64 / BLOCK as u64);
        block_on(async {
            let data: Vec<Vec<u8>> = (0..16).map(|i| pattern(i * MIB, MIB)).collect();
            futures::future::join_all(
                data.iter()
                    .enumerate()
                    .map(|(i, data)| write(device, extent(device, i as u64 + 1), 0, data)),
            )
            .await;
            write(device, extent(device, 17), 0, &pattern(7, 5 * MIB)).await;
            // Open, as written: the page cache's.
            assert!(read(device, extent(device, 17), 0, 5 * MIB).await == pattern(7, 5 * MIB));
            device.flush().await.unwrap();
            let all = futures::future::join_all(
                (0..16).map(|i| read(device, extent(device, i + 1), 0, MIB)),
            )
            .await;
            for (i, data) in all.iter().enumerate() {
                assert!(*data == pattern(i * MIB, MIB), "extent {i} differs");
            }
            assert!(read(device, extent(device, 17), 0, 5 * MIB).await == pattern(7, 5 * MIB));
            assert_eq!(device.next_sequence().await.unwrap(), 18);

            write(device, extent(device, 300), BLOCK as u64, &pattern(0, BLOCK as usize)).await;
            let zeros = vec![0; BLOCK as usize];
            assert_eq!(
                read(device, extent(device, 300), 0, 3 * BLOCK as usize).await,
                [zeros.clone(), pattern(0, BLOCK as usize), zeros].concat()
            );

            let mut buffer = device.allocate_buffer(BLOCK as usize).await;
            assert!(device.read(extent(device, 1), 1, buffer.as_mut()).await.is_err());
            let missing = extent(device, 18);
            let error = device.read(missing, 0, buffer.as_mut()).await.expect_err("read");
            assert_eq!(error.downcast_ref::<NoExtent>(), Some(&NoExtent(missing)));
            device.close().await.unwrap();
        });
    }

    /// A write of an extent sealed by a commit fails with [`SealedExtent`], in its chain, which
    /// Fxfs's packed writes take to write unpacked.
    #[tokio::test(flavor = "multi_thread")]
    async fn writes_of_sealed_extents_say_so() {
        let (_dir, storage, _) = image(16 * MIB as u64).await;
        let device = storage.into_device();
        let device: &dyn Device = &**device;
        block_on(async {
            let sealed = extent(device, 1);
            write(device, sealed, 0, &pattern(0, BLOCK as usize)).await;
            device.flush().await.unwrap();
            let mut buffer = device.allocate_buffer(BLOCK as usize).await;
            buffer.as_mut().copy_from_slice(&pattern(1, BLOCK as usize));
            let error = device.write(sealed, 0, buffer.as_ref()).await.expect_err("write");
            assert!(error.chain().any(|cause| cause.is::<SealedExtent>()), "{error:#}");
            device.close().await.unwrap();
        });
    }

    /// An image holds what was flushed when it is opened again, and what wasn't is gone.
    #[tokio::test(flavor = "multi_thread")]
    async fn extents_survive_reopening() {
        let (_dir, storage, path) = image(16 * MIB as u64).await;
        let device = storage.into_device();
        let cell = device.cell();
        block_on(async {
            write(&**device, ExtentId::new(cell, 3), 0, &pattern(0, MIB)).await;
            write(&**device, ExtentId::new(cell, 259), 0, &pattern(1, 2 * BLOCK as usize)).await;
            device.flush().await.unwrap();
            write(&**device, ExtentId::new(cell, 260), 0, &pattern(2, BLOCK as usize)).await;
        });
        drop(device);
        let storage = Storage::open(&path).await.unwrap();
        // The write never flushed takes no room.
        let used = storage.open_cell().local().info().unwrap().used;
        assert_eq!(used, (MIB + 2 * BLOCK as usize) as u64);
        let device = storage.into_device();
        block_on(async {
            assert_eq!(read(&**device, ExtentId::new(cell, 3), 0, MIB).await, pattern(0, MIB));
            assert_eq!(
                read(&**device, ExtentId::new(cell, 259), 0, 2 * BLOCK as usize).await,
                pattern(1, 2 * BLOCK as usize)
            );
            assert_eq!(device.next_sequence().await.unwrap(), 260);
            let mut buffer = device.allocate_buffer(BLOCK as usize).await;
            assert!(device.read(ExtentId::new(cell, 260), 0, buffer.as_mut()).await.is_err());
            device.close().await.unwrap();
        });
    }

    /// Freeing an extent gives back the capacity it took, and its sequence number is not given
    /// out again.
    #[tokio::test(flavor = "multi_thread")]
    async fn frees_make_room() {
        let (_dir, storage, _) = image(4 * MIB as u64).await;
        let device = storage.into_device();
        let device: &dyn Device = &**device;
        block_on(async {
            write(device, extent(device, 1), 0, &pattern(0, 3 * MIB)).await;
            device.flush().await.unwrap();
            let mut buffer = device.allocate_buffer(2 * MIB).await;
            assert!(device.write(extent(device, 2), 0, buffer.as_ref()).await.is_err(), "overfull");
            device.free(extent(device, 1)).await.unwrap();
            device.free(extent(device, 1)).await.expect("freeing a freed extent failed");
            device.flush().await.unwrap();
            assert!(device.read(extent(device, 1), 0, buffer.as_mut()).await.is_err());
            device.write(extent(device, 2), 0, buffer.as_ref()).await.expect("write failed");
            assert_eq!(device.next_sequence().await.unwrap(), 3);
            device.close().await.unwrap();
        });
    }

    /// The root is set durably, and kept across opening the image again.
    #[tokio::test(flavor = "multi_thread")]
    async fn roots() {
        let (_dir, storage, path) = image(MIB as u64).await;
        let device = storage.into_device();
        let cell = device.cell();
        block_on(async {
            assert_eq!(device.root().await.unwrap(), Vec::<u8>::new());
            write(&**device, ExtentId::new(cell, 1), 0, &pattern(0, BLOCK as usize)).await;
            device.commit_root(b"first").await.unwrap();
            device.commit_root(b"second").await.unwrap();
            assert_eq!(device.root().await.unwrap(), b"second");
        });
        drop(device);
        let device = Storage::open(&path).await.unwrap().into_device();
        block_on(async {
            assert_eq!(device.root().await.unwrap(), b"second");
            let read = read(&**device, ExtentId::new(cell, 1), 0, BLOCK as usize).await;
            assert_eq!(read, pattern(0, BLOCK as usize));
            device.close().await.unwrap();
        });
    }

    /// cladefs: a byte the host's disk damaged in a sealed extent's file, in a part stored
    /// compressed or raw, fails the device's reads of the part, as imports and host tools read an
    /// image, rather than giving another byte; its other parts read as written (TASK-153.11).
    #[tokio::test(flavor = "multi_thread")]
    async fn flipped_bytes_fail_reads() {
        use cladefs_core::test_data::noise;
        let (_dir, storage, path) = image(64 * MIB as u64).await;
        let device = storage.into_device();
        let cell = device.cell();
        block_on(async {
            // Large enough to be stored on their own, in five parts: compressed, and raw.
            let contents = [
                (ExtentId::new(cell, 1), pattern(0, 5 * MIB), extent_frame::Encoding::Zstd),
                (ExtentId::new(cell, 2), noise(2, 5 * MIB), extent_frame::Encoding::Raw),
            ];
            for (extent, data, _) in &contents {
                write(&**device, *extent, 0, data).await;
            }
            device.flush().await.unwrap();
            let at = (3 * MIB + MIB / 2) as u64;
            for (extent, data, encoding) in &contents {
                // Read whole first, from the disk: what fails after is the flip's.
                local_cell::pages::shared().evict_to(0);
                let part = read(&**device, *extent, at, BLOCK as usize).await;
                assert!(part == data[at as usize..][..BLOCK as usize], "{extent:?} at {at}");
                // The middle of its fourth part's stored bytes: of its data at 3.5 MiB, if raw.
                let file = path.join(format!(
                    "extents/{}/{}",
                    base32_id::Padded(cell),
                    base32_id::Padded(extent.sequence)
                ));
                let mut bytes = std::fs::read(&file).unwrap();
                let stored =
                    extent_frame::Parts::of(&bytes, &extent_frame::NoKeys).unwrap().stored[3];
                assert_eq!(stored.encoding, *encoding);
                bytes[(stored.offset + stored.len / 2) as usize] ^= 0x10;
                std::fs::write(&file, bytes).unwrap();
                // Damaged behind the cell's back: what the cell's page cache holds of it, clean,
                // goes, and the parts it keeps unpacked, so that its reads read the disk
                // (TASK-433).
                local_cell::pages::shared().evict_to(0);
                local_cell::unpacked().evict_to(0);

                let mut buffer = device.allocate_buffer(BLOCK as usize).await;
                let Err(error) = device.read(*extent, at, buffer.as_mut()).await else {
                    panic!("{extent:?} read a flipped byte");
                };
                assert_eq!(error.downcast_ref(), Some(&Errno::EIO), "{error:#}");
                let first = read(&**device, *extent, 0, BLOCK as usize).await;
                assert!(first == data[..BLOCK as usize], "{extent:?}'s first block");
            }
        });
        // Not closed: its uploads read the damaged files, and fail, as they should.
    }

    /// A write the cell holds back while a commit waits for a write of the executor's own, on an
    /// executor of one thread, goes on once that write ends: it does not hold the thread the write
    /// would end on, which the commit would wait for for ever.
    #[tokio::test(flavor = "multi_thread")]
    #[allow(clippy::disallowed_methods, reason = "a write in progress, as a commit waits for")]
    async fn writes_held_back_by_a_commit_leave_the_executor_running() {
        let (_dir, storage, _) = image(16 * MIB as u64).await;
        let local = storage.open_cell().local().clone();
        let device = Arc::new(storage.into_device());
        let cell = device.cell();
        let executor = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        // A write of the executor's in progress, which ends once told to.
        let (go, went) = futures::channel::oneshot::channel::<()>();
        let (began, beginning) = std::sync::mpsc::channel();
        let held = local.clone();
        executor.spawn(async move {
            let write = held.begin_write(ExtentId::new(cell, 1), 0, BLOCK as u64).await.unwrap();
            began.send(()).unwrap();
            let _ = went.await;
            write.finish();
        });
        beginning.recv().unwrap();
        let committing = tokio::spawn({
            let local = local.clone();
            async move { local.commit(None).await.unwrap() }
        });
        while !local.holding_writes() {
            std::thread::yield_now();
        }
        let (wrote, written) = std::sync::mpsc::channel();
        let writing = device.clone();
        executor.spawn(async move {
            write(&***writing, ExtentId::new(cell, 2), 0, &pattern(0, BLOCK as usize)).await;
            wrote.send(()).unwrap();
        });
        go.send(()).unwrap();
        let waited = written.recv_timeout(std::time::Duration::from_secs(30));
        assert_eq!(waited, Ok(()), "the held write never went on");
        committing.await.unwrap();
        executor.shutdown_background();
    }

    /// TASK-98: as above, with the write in progress a future of the held write's own task, as
    /// Fxfs joins its writes: the held write waits off the task's thread, which the other future
    /// ends on. cladefsctl import hung so, every executor thread waiting in `begin_write` for a
    /// commit that waited for the writes those threads had begun.
    #[tokio::test(flavor = "multi_thread")]
    #[allow(clippy::disallowed_methods, reason = "a write in progress, as a commit waits for")]
    async fn writes_held_back_by_a_commit_leave_their_own_task_running() {
        let (_dir, storage, _) = image(16 * MIB as u64).await;
        let local = storage.open_cell().local().clone();
        let device = Arc::new(storage.into_device());
        let cell = device.cell();
        let executor = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (go, went) = futures::channel::oneshot::channel::<()>();
        let (held, holding) = futures::channel::oneshot::channel::<()>();
        let (began, beginning) = std::sync::mpsc::channel();
        let (writing, entered) = std::sync::mpsc::channel();
        let (wrote, written) = std::sync::mpsc::channel();
        let in_task = local.clone();
        executor.spawn(async move {
            // A write in progress, which ends once told to.
            let in_progress = async {
                let extent = ExtentId::new(cell, 1);
                let write = in_task.begin_write(extent, 0, BLOCK as u64).await.unwrap();
                began.send(()).unwrap();
                let _ = went.await;
                write.finish();
            };
            // And beside it, once a commit holds writes back, another.
            let held_back = async {
                let _ = holding.await;
                // In the same poll as the write begins: the other is told to end only after.
                writing.send(()).unwrap();
                write(&***device, ExtentId::new(cell, 2), 0, &pattern(0, BLOCK as usize)).await;
            };
            futures::join!(in_progress, held_back);
            wrote.send(()).unwrap();
        });
        beginning.recv().unwrap();
        let committing = tokio::spawn({
            let local = local.clone();
            async move { local.commit(None).await.unwrap() }
        });
        while !local.holding_writes() {
            std::thread::yield_now();
        }
        held.send(()).unwrap();
        entered.recv().unwrap();
        go.send(()).unwrap();
        let waited = written.recv_timeout(std::time::Duration::from_secs(30));
        assert_eq!(waited, Ok(()), "the task's writes never ended");
        committing.await.unwrap();
        executor.shutdown_background();
    }

    /// cladefs: an image of an encrypted cell (TASK-411.5): what its device writes, in its open
    /// segment's raw file and a large extent's own, goes to the disk encrypted, and reads back
    /// decrypted, open, sealed and after reopening; no file of the image holds it in the clear at
    /// any of those versions, each once the image does nothing behind its device's callers
    /// ([`quiesced`]).
    #[tokio::test(flavor = "multi_thread")]
    async fn encrypted_images_hold_no_plaintext() {
        let keys = tempfile::tempdir().unwrap();
        let kms = extent_keys::kms::LocalKms::open(keys.path().join("kms")).unwrap();
        kms.create(5).unwrap();
        cladefs_core::bucket::set_kms(Arc::new(kms)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image");
        let keeping = cladefs_core::host_cell::keeping().unwrap();
        let storage = Storage::make_on(
            cladefs_core::Host::process(),
            &path,
            (64 * MIB as u64, 5),
            bucket_cell::cell::Kind::Fxfs,
            true,
            keeping,
        )
        .await
        .unwrap();
        assert!(storage.open_cell().local().keys().encrypts());
        let cell = storage.open_cell().cell().id;
        let (small, large) = (ExtentId::new(cell, 1), ExtentId::new(cell, 2));
        let small_data = cladefs_core::test_data::noise(1, 4 * BLOCK as usize);
        let large_data = cladefs_core::test_data::noise(2, 5 * MIB);
        let in_the_clear = async |storage: &Storage| {
            quiesced(storage).await;
            // As on the disk, what the cell's page cache holds written there first (TASK-433).
            host_fs::may_block(|| local_cell::pages::shared().write_back_all().unwrap());
            let pieces = [&small_data[..64], &large_data[..64], &large_data[MIB..MIB + 64]];
            cladefs_core::testing::files_holding(dir.path(), &pieces)
        };
        let reads = |device: &dyn Device| {
            block_on(async {
                assert!(read(device, small, 0, small_data.len()).await == small_data);
                assert!(read(device, large, 0, large_data.len()).await == large_data);
            })
        };
        let device: &dyn Device = &**storage.device;
        block_on(async {
            write(device, small, 0, &small_data).await;
            write(device, large, 0, &large_data).await;
        });
        reads(device);
        assert_eq!(in_the_clear(&storage).await, Vec::<String>::new(), "written");
        block_on(device.flush()).unwrap();
        reads(device);
        assert_eq!(in_the_clear(&storage).await, Vec::<String>::new(), "flushed");
        storage.close().await;
        let storage = Storage::open(&path).await.unwrap();
        reads(&**storage.device);
        assert_eq!(in_the_clear(&storage).await, Vec::<String>::new(), "reopened");
        storage.close().await;
    }

    /// cladefs: `storage`, once its image does nothing to its directory, nor to its bucket's,
    /// behind its callers (TASK-463): its commits uploaded and acknowledged, the uploader's closings
    /// for them framed and what the acknowledgements let go of gone; the cell's own work done, its
    /// framings made durable, their files renamed their own, and the next segment's log prepared
    /// (`LocalCell::wait_idle`); and the pass of the bucket's collector its commits started, if
    /// one runs. Walked before, a file listed could go before it was read, `NotFound` (CI run
    /// 43729753505).
    async fn quiesced(storage: &Storage) {
        let cell = storage.open_cell();
        cell.wait_uploaded().await.unwrap();
        cell.local().wait_idle().await.unwrap();
        if let Some(collecting) = storage.collecting() {
            collecting.collected();
        }
    }
}
