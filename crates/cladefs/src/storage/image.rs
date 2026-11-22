// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! An Fxfs device on an image: a cell directory (crates/local_cell, backlog
//! doc-4), whose extents are packed into segments, or are files of their own if large, with an
//! index of them.
//!
//! It replaces Fuchsia's `FileBackedDevice`, which is for host tools and tests:
//!
//! - Extents' I/O bypasses the host's page cache where the host filesystem allows (`O_DIRECT` on
//!   Linux, `F_NOCACHE` on macOS).
//! - Extents' I/O runs off Fxfs's executor threads (see `io`), which only await it, as do commits,
//!   which sync the image.
//! - Flushing commits the cell: a barrier, durable, which seals the extents written before it.
//! - cladefs: the cell is kept in its bucket (see `kept`): its commits are uploaded behind
//!   them, and closing the device waits for them to be.

use super::gate::WriteGate;
use super::io::{Done, Io, IoFile, Op};
use super::kept::KeptImage;
use anyhow::{Context, Error, bail};
use async_trait::async_trait;
use local_cell::{Aligned, Description, HostDir, HostFile, LocalCell, Located, SECTOR};
use std::path::Path;
use std::sync::Arc;
use storage_device::buffer::{BufferFuture, BufferRef, MutableBufferRef};
use storage_device::buffer_allocator::{BufferAllocator, BufferSource};
use storage_device::{Device, ExtentId};
use storage_device::{ReadOptions, WriteFlags, WriteOptions};

/// The size of the transfer buffers' memory.
const TRANSFER_HEAP_SIZE: usize = 32 * 1024 * 1024;

/// An Fxfs device on an image. See the module docs.
pub(crate) struct ImageDevice {
    image: Arc<KeptImage>,
    /// The image's cell, the one `image` keeps.
    local: Arc<LocalCell<HostDir>>,
    description: Description,
    io: Io,
    allocator: BufferAllocator,
    /// The writes, frees and commits, which a copy of the image pauses.
    writes: Arc<WriteGate>,
}

impl ImageDevice {
    /// Makes a new image at `dir`, which must not exist, holding at most `capacity` bytes, of
    /// `cell`, whole if `whole`, its cell kept as `keeping` says ([`KeptImage::create`]), and
    /// its device, whose writes, frees and commits go through `writes`.
    pub async fn create(
        dir: &Path,
        made: (u64, crate::host_cell::CellOf, bool),
        keeping: local_cell::upload::Options,
        writes: Arc<WriteGate>,
    ) -> Result<Self, Error> {
        Self::new(KeptImage::create(dir, made, keeping).await?, writes)
    }

    /// The device of the image at `dir`, an image's or a fork's, whose writes, frees and commits
    /// go through `writes`.
    pub async fn open(dir: &Path, writes: Arc<WriteGate>) -> Result<Self, Error> {
        let image = KeptImage::open(dir).await;
        let image = image.with_context(|| format!("opening {}", dir.display()))?;
        Self::new(image, writes)
    }

    /// The image's cell, kept in its bucket.
    pub fn image(&self) -> Arc<KeptImage> {
        self.image.clone()
    }

    fn new(image: KeptImage, writes: Arc<WriteGate>) -> Result<Self, Error> {
        let local = image.cell().clone();
        let description = local.description();
        Ok(Self {
            image: Arc::new(image),
            local,
            description,
            io: Io::new().context("starting the image's I/O")?,
            allocator: BufferAllocator::new(
                description.block_size as usize,
                BufferSource::new(TRANSFER_HEAP_SIZE),
            ),
            writes,
        })
    }

    /// Reads `len` bytes at `offset` in `file`: the first `needed` of them, and the rest as far as
    /// the file goes. The I/O reads into a buffer of its own, which it gives back: the caller's
    /// memory is never the host's, which a future leaked would leave it (TASK-24).
    async fn read_file(
        &self,
        file: &Arc<HostFile>,
        len: usize,
        needed: usize,
        offset: u64,
    ) -> Result<Aligned, Error> {
        // What the cell's page cache holds dirty there, on the disk first (TASK-433).
        file.settle(offset..offset + len as u64)?;
        let mut read: Option<Aligned> = None;
        let mut done = 0;
        while done < needed {
            let at = offset + done as u64;
            let op = Op::Read { len: len - done, offset: at };
            let Done { result, buffer } = self.io.run(file.clone() as IoFile, op).await;
            let transferred = match result {
                Ok(0) => bail!("the image's file ended at {at:#x}"),
                Ok(transferred) => transferred,
                Err(err) => return Err(Error::new(err).context(format!("I/O at {at:#x}"))),
            };
            match &mut read {
                // Read whole, as it is but for a short read.
                None if transferred >= needed => return Ok(buffer),
                None => read = Some(buffer),
                Some(read) => read.as_mut_slice()[done..done + transferred]
                    .copy_from_slice(&buffer.as_slice()[..transferred]),
            }
            done += transferred;
        }
        Ok(read.unwrap_or_else(|| Aligned::new(len)))
    }

    /// Writes `data`, the bytes of `write`, to its file, from a buffer of the I/O's own, as
    /// `read_file` reads. cladefs: as the file stores them, encrypted once if the cell is
    /// (`Write::stored`, TASK-411.5); a write stopping short is written again from the last whole
    /// sector it wrote, so that what is written again is whole sectors of the same bytes.
    async fn write_file(
        &self,
        write: &local_cell::Write<'_, HostDir>,
        data: BufferRef<'_>,
    ) -> Result<(), Error> {
        let mut owned = Aligned::new(data.len());
        data.copy_to_slice(owned.as_mut_slice());
        let stored = write.stored(owned)?;
        let bytes = stored.bytes.as_slice();
        // No page of the cell's page cache left there to hide what this writes (TASK-433).
        stored.file.settle_for_write(stored.offset..stored.offset + bytes.len() as u64)?;
        let mut done = 0;
        while done < bytes.len() {
            let at = stored.offset + done as u64;
            let mut owned = Aligned::new(bytes.len() - done);
            owned.as_mut_slice().copy_from_slice(&bytes[done..]);
            let op = Op::Write { data: owned, offset: at };
            match self.io.run(stored.file.clone() as IoFile, op).await.result {
                Ok(0) => bail!("the image's file took nothing at {at:#x}"),
                Ok(transferred) if transferred == bytes.len() - done => done = bytes.len(),
                Ok(transferred) => {
                    let sectors = (done + transferred) / SECTOR as usize * SECTOR as usize;
                    if sectors == done {
                        bail!("the image's file took {transferred} bytes at {at:#x}: no sector");
                    }
                    done = sectors;
                }
                Err(err) => return Err(Error::new(err).context(format!("I/O at {at:#x}"))),
            }
        }
        Ok(())
    }

    /// Commits the cell, setting its root to `root` if given, and does what follows a commit
    /// ([`KeptImage::committed`]): collects its bucket if that is due; off the executor's threads.
    async fn commit(&self, root: Option<Vec<u8>>) -> Result<(), Error> {
        let _writing = self.writes.write().await;
        let image = self.image.clone();
        // cladefs: a task of the bucket runtime's, off the executor's threads: the flush's disk
        // work, in `block_in_place` there, hands that runtime's other tasks on (board 5614).
        let committing = async move {
            image.cell().commit(root.as_deref()).await?;
            image.committed().await
        };
        let runtime = local_cell::upload::bucket_runtime()?;
        // cladefs: `unconstrained` prevents coop-budget livelock when polled from `block_on` inside
        // a tokio task (board 6525).
        crate::bucket::unconstrained(runtime.spawn(committing)).await.map_err(Error::new)?
    }
}

#[async_trait]
impl Device for ImageDevice {
    fn allocate_buffer(&self, size: usize) -> BufferFuture<'_> {
        self.allocator.allocate_buffer(size)
    }

    fn block_size(&self) -> u32 {
        self.description.block_size
    }

    fn block_count(&self) -> u64 {
        self.description.capacity / self.description.block_size as u64
    }

    /// Blocks within the extent never written, and past its end, read as zeros.
    async fn read_with_opts(
        &self,
        extent: ExtentId,
        offset: u64,
        mut buffer: MutableBufferRef<'_>,
        _read_opts: ReadOptions,
    ) -> Result<(), Error> {
        let parts = match self.local.locate(extent, offset, buffer.len() as u64) {
            // cladefs: in the image's bucket, as a fork holds what it inherited (TASK-165.7).
            Err(ref error)
                if let Some(in_bucket) = error.downcast_ref::<local_cell::BucketData>() =>
            {
                let location = in_bucket.location.clone();
                let fetcher = self.image.fetcher();
                let fetched = fetcher.read(extent, location, offset, buffer.len() as u64).await?;
                let (len, end) = (fetched.len(), buffer.len());
                buffer.reborrow().subslice_mut(..len).copy_from_slice(&fetched);
                buffer.reborrow().subslice_mut(len..end).fill(0);
                return Ok(());
            }
            parts => parts?,
        };
        let mut at = 0;
        for part in parts {
            match part {
                // A raw file's part, through the descriptor bypassing the host's page cache,
                // which the cell's own reads of raw files take through the cache
                // (`File::read_cached`, TASK-91.17): such a read writes back first what it reads
                // that is dirty yet, which is slower, not other bytes. This device is an
                // in-process image's, which imports, host tools and tests use; guests' reads are
                // cladefsd's, through the cell's. cladefs: the sectors they are in, decrypted, if
                // the cell is encrypted (TASK-411.5).
                Located::Data { file, offset, len, .. } => {
                    let range = file.read_range(offset, len);
                    let read_len = (range.end - range.start) as usize;
                    let mut read =
                        self.read_file(file.file(), read_len, read_len, range.start).await?;
                    file.decrypt(range.start, read.as_mut_slice());
                    let (skip, len) = ((offset - range.start) as usize, len as usize);
                    let part = &read.as_slice()[skip..skip + len];
                    buffer.reborrow().subslice_mut(at..at + len).copy_from_slice(part);
                    at += len;
                }
                // Its stored bytes, in whole blocks, to check and unpack.
                Located::Packed { part, skip, len } => {
                    let range = part.read_range();
                    let read_len = (range.end - range.start) as usize;
                    let needed = part.stored().len as usize;
                    let stored = self.read_file(&part.file, read_len, needed, range.start).await?;
                    let bytes = self.local.unpack(&part, stored)?;
                    let (skip, len) = (skip as usize, len as usize);
                    let mut part = buffer.reborrow().subslice_mut(at..at + len);
                    part.copy_from_slice(&bytes[skip..skip + len]);
                    at += len;
                }
                Located::Unpacked { bytes, skip, len } => {
                    let (skip, len) = (skip as usize, len as usize);
                    let mut part = buffer.reborrow().subslice_mut(at..at + len);
                    part.copy_from_slice(&bytes[skip..skip + len]);
                    at += len;
                }
                Located::Zeros(len) => {
                    buffer.reborrow().subslice_mut(at..at + len as usize).fill(0);
                    at += len as usize;
                }
            }
        }
        Ok(())
    }

    async fn write_with_opts(
        &self,
        extent: ExtentId,
        offset: u64,
        buffer: BufferRef<'_>,
        write_opts: WriteOptions<'_>,
    ) -> Result<(), Error> {
        if write_opts.flags.contains(WriteFlags::POST_BARRIER) {
            // The write, then the flush, or commit of the root, that follows it.
            self.write_with_opts(extent, offset, buffer, WriteOptions::default()).await?;
            return match write_opts.root {
                Some(root) => self.commit_root(root).await,
                None => self.flush().await,
            };
        }
        let _writing = self.writes.write().await;
        // The cell holds a write back while a commit waits for those in progress, which may be
        // futures of this very task, as Fxfs joins its writes: this thread must not wait, as they
        // can't end while it does, and the commit would wait for them for ever (TASK-98). A
        // thread of its own waits, and the write is tried again.
        let write = loop {
            if let Some(write) = self.local.try_begin_write(extent, offset, buffer.len() as u64)? {
                break write;
            }
            // cladefs: awaited, holding no thread (board 2944).
            self.local.wait_until_writes_resume().await;
        };
        self.write_file(&write, buffer).await?;
        write.finish();
        Ok(())
    }

    fn cell(&self) -> u64 {
        self.description.cell.id
    }

    async fn next_sequence(&self) -> Result<u64, Error> {
        Ok(self.local.info()?.next_sequence)
    }

    async fn free(&self, extent: ExtentId) -> Result<(), Error> {
        let _writing = self.writes.write().await;
        self.local.free(extent)
    }

    async fn root(&self) -> Result<Vec<u8>, Error> {
        self.local.root()
    }

    async fn commit_root(&self, root: &[u8]) -> Result<(), Error> {
        self.commit(Some(root.to_vec())).await.context("committing the image's root")
    }

    /// Flushes, and waits for the image's commits to be uploaded: a closed image's cell is whole
    /// in its bucket.
    async fn close(&self) -> Result<(), Error> {
        self.flush().await?;
        self.image.wait_uploaded().await.context("uploading the image's commits")
    }

    async fn flush(&self) -> Result<(), Error> {
        self.commit(None).await.context("flushing the image")
    }

    fn is_read_only(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use std::path::PathBuf;
    use storage_device::NoExtent;

    const BLOCK: u32 = crate::host_cell::BLOCK_SIZE;
    const MIB: usize = 1 << 20;
    const CELL: crate::CellId = bucket_cell::layout::test_cell(7);

    /// A new directory, a device on a new image in it holding `capacity` bytes, and the image: in
    /// that order, so that a test's bindings of them drop the device before the directory, as a
    /// pass of its bucket's collector may run as it drops, which waits for it, and which would
    /// retry for ever the requests of a bucket gone (TASK-428).
    async fn device(capacity: u64) -> (tempfile::TempDir, ImageDevice, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("image");
        let made = (capacity, crate::host_cell::CellOf::Named(CELL), true);
        let keeping = crate::host_cell::keeping().unwrap();
        (dir, ImageDevice::create(&image, made, keeping, Default::default()).await.unwrap(), image)
    }

    fn extent(sequence: u64) -> ExtentId {
        ExtentId::new(CELL.id, sequence)
    }

    /// Bytes each their offset modulo 251, from `offset`.
    fn pattern(offset: usize, len: usize) -> Vec<u8> {
        (offset..offset + len).map(|i| (i % 251) as u8).collect()
    }

    async fn write(device: &ImageDevice, extent: ExtentId, offset: u64, data: &[u8]) {
        let mut buffer = device.allocate_buffer(data.len()).await;
        buffer.as_mut().copy_from_slice(data);
        device.write(extent, offset, buffer.as_ref()).await.unwrap();
    }

    async fn read(device: &ImageDevice, extent: ExtentId, offset: u64, len: usize) -> Vec<u8> {
        let mut buffer = device.allocate_buffer(len).await;
        device.read(extent, offset, buffer.as_mut()).await.unwrap();
        contents(buffer.as_ref())
    }

    fn contents(buffer: BufferRef<'_>) -> Vec<u8> {
        let mut data = vec![0; buffer.len()];
        buffer.copy_to_slice(&mut data);
        data
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reads_what_was_written() {
        let (_dir, device, _) = device(64 * MIB as u64).await;
        assert_eq!(device.block_count(), 64 * MIB as u64 / BLOCK as u64);
        assert_eq!(device.cell(), CELL.id);
        block_on(async {
            // Many at once, which all complete, and one large enough for a file of its own.
            let data: Vec<Vec<u8>> = (0..16).map(|i| pattern(i * MIB, MIB)).collect();
            futures::future::join_all(
                data.iter().enumerate().map(|(i, data)| write(&device, extent(i as u64), 0, data)),
            )
            .await;
            write(&device, extent(16), 0, &pattern(7, 5 * MIB)).await;
            device.flush().await.unwrap();
            let all =
                futures::future::join_all((0..16).map(|i| read(&device, extent(i), 0, MIB))).await;
            for (i, data) in all.iter().enumerate() {
                assert!(*data == pattern(i * MIB, MIB), "extent {i} differs");
            }
            assert!(read(&device, extent(16), 0, 5 * MIB).await == pattern(7, 5 * MIB));
            assert_eq!(device.next_sequence().await.unwrap(), 17);

            // Blocks never written read as zeros, within an extent and past its end.
            write(&device, extent(300), BLOCK as u64, &pattern(0, BLOCK as usize)).await;
            let zeros = vec![0; BLOCK as usize];
            assert_eq!(
                read(&device, extent(300), 0, 3 * BLOCK as usize).await,
                [zeros.clone(), pattern(0, BLOCK as usize), zeros].concat()
            );

            // Unaligned I/O, and I/O on extents the image doesn't hold, fail.
            let mut buffer = device.allocate_buffer(BLOCK as usize).await;
            assert!(device.read(extent(0), 1, buffer.as_mut()).await.is_err());
            let error = device.read(extent(17), 0, buffer.as_mut()).await.expect_err("read");
            assert_eq!(error.downcast_ref::<NoExtent>(), Some(&NoExtent(extent(17))));
        });
    }

    /// cladefs: a byte the host's disk damaged in a sealed extent's file, in a part stored
    /// compressed or raw, fails the device's reads of the part, as imports and host tools read an
    /// image, rather than giving another byte; its other parts read as written (TASK-153.11).
    #[tokio::test(flavor = "multi_thread")]
    async fn flipped_bytes_fail_reads() {
        let (_dir, device, image) = device(64 * MIB as u64).await;
        block_on(async {
            // Large enough to be stored on their own, in five parts: compressed, and raw.
            let contents = [
                (extent(1), pattern(0, 5 * MIB), extent_frame::Encoding::Zstd),
                (extent(2), crate::test_data::noise(2, 5 * MIB), extent_frame::Encoding::Raw),
            ];
            for (extent, data, _) in &contents {
                write(&device, *extent, 0, data).await;
            }
            device.flush().await.unwrap();
            for (extent, data, encoding) in &contents {
                // The middle of its fourth part's stored bytes: of its data at 3.5 MiB, if raw.
                let path = image.join(format!(
                    "extents/{}/{}",
                    base32_id::Padded(CELL.id),
                    base32_id::Padded(extent.sequence)
                ));
                let mut bytes = std::fs::read(&path).unwrap();
                let stored =
                    extent_frame::Parts::of(&bytes, &extent_frame::NoKeys).unwrap().stored[3];
                assert_eq!(stored.encoding, *encoding);
                bytes[(stored.offset + stored.len / 2) as usize] ^= 0x10;
                std::fs::write(&path, bytes).unwrap();
                // Damaged behind the cell's back: what the cell's page cache holds of it, clean,
                // goes, so that its reads read the disk (TASK-433).
                local_cell::pages::shared().evict_to(0);

                let mut buffer = device.allocate_buffer(BLOCK as usize).await;
                let at = (3 * MIB + MIB / 2) as u64;
                let Err(error) = device.read(*extent, at, buffer.as_mut()).await else {
                    panic!("{extent:?} read a flipped byte");
                };
                let corrupt = extent_frame::Corrupt("an object's part, by its CRC".into());
                assert_eq!(error.downcast_ref(), Some(&corrupt), "{error:#}");
                let first = read(&device, *extent, 0, BLOCK as usize).await;
                assert!(first == data[..BLOCK as usize], "{extent:?}'s first block");
            }
        });
    }

    /// A write the cell holds back while a commit waits for a write of the executor's own, on an
    /// executor of one thread, goes on once that write ends: it does not hold the thread the write
    /// would end on, which the commit would wait for for ever.
    #[tokio::test(flavor = "multi_thread")]
    #[allow(clippy::disallowed_methods, reason = "a write in progress, as a commit waits for")]
    async fn writes_held_back_by_a_commit_leave_the_executor_running() {
        let (_dir, device, _) = device(16 * MIB as u64).await;
        let local = device.local.clone();
        let device = Arc::new(device);
        let executor = crate::executor::Executor::new(1);
        // A write of the executor's in progress, which ends once told to.
        let (go, went) = futures::channel::oneshot::channel::<()>();
        let (began, beginning) = std::sync::mpsc::channel();
        let held = local.clone();
        executor.spawn(async move {
            let write = held.begin_write(extent(1), 0, BLOCK as u64).await.unwrap();
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
            write(&writing, extent(2), 0, &pattern(0, BLOCK as usize)).await;
            wrote.send(()).unwrap();
        });
        go.send(()).unwrap();
        let waited = written.recv_timeout(std::time::Duration::from_secs(30));
        assert_eq!(waited, Ok(()), "the held write never went on");
        committing.await.unwrap();
    }

    /// TASK-98: as above, with the write in progress a future of the held write's own task, as
    /// Fxfs joins its writes: the held write waits off the task's thread, which the other future
    /// ends on. cladefsctl import hung so, every executor thread waiting in `begin_write` for a
    /// commit that waited for the writes those threads had begun.
    #[tokio::test(flavor = "multi_thread")]
    #[allow(clippy::disallowed_methods, reason = "a write in progress, as a commit waits for")]
    async fn writes_held_back_by_a_commit_leave_their_own_task_running() {
        let (_dir, device, _) = device(16 * MIB as u64).await;
        let local = device.local.clone();
        let device = Arc::new(device);
        let executor = crate::executor::Executor::new(1);
        let (go, went) = futures::channel::oneshot::channel::<()>();
        let (held, holding) = futures::channel::oneshot::channel::<()>();
        let (began, beginning) = std::sync::mpsc::channel();
        let (writing, entered) = std::sync::mpsc::channel();
        let (wrote, written) = std::sync::mpsc::channel();
        let in_task = local.clone();
        executor.spawn(async move {
            // A write in progress, which ends once told to.
            let in_progress = async {
                let write = in_task.begin_write(extent(1), 0, BLOCK as u64).await.unwrap();
                began.send(()).unwrap();
                let _ = went.await;
                write.finish();
            };
            // And beside it, once a commit holds writes back, another.
            let held_back = async {
                let _ = holding.await;
                // In the same poll as the write begins: the other is told to end only after.
                writing.send(()).unwrap();
                write(&device, extent(2), 0, &pattern(0, BLOCK as usize)).await;
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
    }

    /// An image holds what was flushed when it is opened again, and what wasn't is gone.
    #[tokio::test(flavor = "multi_thread")]
    async fn extents_survive_reopening() {
        let (_dir, device, image) = device(16 * MIB as u64).await;
        block_on(async {
            write(&device, extent(3), 0, &pattern(0, MIB)).await;
            write(&device, extent(259), 0, &pattern(1, 2 * BLOCK as usize)).await;
            device.flush().await.unwrap();
            write(&device, extent(260), 0, &pattern(2, BLOCK as usize)).await;
        });
        drop(device);
        let device = ImageDevice::open(&image, Default::default()).await.unwrap();
        block_on(async {
            assert_eq!(read(&device, extent(3), 0, MIB).await, pattern(0, MIB));
            assert_eq!(
                read(&device, extent(259), 0, 2 * BLOCK as usize).await,
                pattern(1, 2 * BLOCK as usize)
            );
            assert_eq!(device.next_sequence().await.unwrap(), 260);
            let mut buffer = device.allocate_buffer(BLOCK as usize).await;
            assert!(device.read(extent(260), 0, buffer.as_mut()).await.is_err());
        });
        assert_eq!(device.local.info().unwrap().used, (MIB + 2 * BLOCK as usize) as u64);
    }

    /// cladefs: an image of an encrypted cell (TASK-411.5): its device writes extents' data, in its
    /// open segment's raw file and a large extent's own, by I/O of its own, encrypted
    /// (`Write::stored`), and reads them back decrypted, open, sealed and after reopening; no file
    /// of the image holds them in the clear at any of those versions, each once the image does
    /// nothing behind its device's callers ([`quiesced`]).
    #[tokio::test(flavor = "multi_thread")]
    async fn encrypted_images_hold_no_plaintext() {
        let keys = tempfile::tempdir().unwrap();
        let kms = extent_keys::kms::LocalKms::open(keys.path().join("kms")).unwrap();
        kms.create(5).unwrap();
        crate::bucket::set_kms(Arc::new(kms)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("image");
        let made = (64 * MIB as u64, crate::host_cell::CellOf::New(5), true);
        let keeping = crate::host_cell::keeping().unwrap();
        let device = ImageDevice::create(&image, made, keeping, Default::default()).await.unwrap();
        assert!(device.local.keys().encrypts());
        let cell = device.cell();
        let (small, large) = (ExtentId::new(cell, 1), ExtentId::new(cell, 2));
        let small_data = crate::test_data::noise(1, 4 * BLOCK as usize);
        let large_data = crate::test_data::noise(2, 5 * MIB);
        let in_the_clear = async |device: &ImageDevice| {
            quiesced(device).await;
            // As on the disk, what the cell's page cache holds written there first (TASK-433).
            local_cell::pages::shared().write_back_all().unwrap();
            let pieces = [&small_data[..64], &large_data[..64], &large_data[MIB..MIB + 64]];
            crate::testing::files_holding(dir.path(), &pieces)
        };
        let reads = |device: &ImageDevice| {
            block_on(async {
                assert!(read(device, small, 0, small_data.len()).await == small_data);
                assert!(read(device, large, 0, large_data.len()).await == large_data);
            })
        };
        block_on(async {
            write(&device, small, 0, &small_data).await;
            write(&device, large, 0, &large_data).await;
        });
        reads(&device);
        assert_eq!(in_the_clear(&device).await, Vec::<String>::new(), "written");
        block_on(device.flush()).unwrap();
        reads(&device);
        assert_eq!(in_the_clear(&device).await, Vec::<String>::new(), "flushed");
        drop(device);
        let device = ImageDevice::open(&image, Default::default()).await.unwrap();
        reads(&device);
        assert_eq!(in_the_clear(&device).await, Vec::<String>::new(), "reopened");
    }

    /// cladefs: `device`, once its image does nothing to its directory, nor to its bucket's, behind
    /// its callers (TASK-463): its commits uploaded and acknowledged, the uploader's closings for
    /// them framed and what the acknowledgements let go of gone; the cell's own work done, its
    /// framings made durable, their files renamed their own, and the next segment's log prepared
    /// ([`LocalCell::wait_idle`]); and the pass of the bucket's collector its commits started, if
    /// one runs. Walked before, a file listed could go before it was read, `NotFound` (CI run
    /// 43729753505).
    async fn quiesced(device: &ImageDevice) {
        device.image.wait_uploaded().await.unwrap();
        device.local.wait_idle().await.unwrap();
        if let Some(collecting) = device.image.collecting() {
            collecting.collected();
        }
    }

    /// Freeing an extent gives back the capacity it took.
    #[tokio::test(flavor = "multi_thread")]
    async fn frees_make_room() {
        let (_dir, device, _) = device(4 * MIB as u64).await;
        block_on(async {
            write(&device, extent(1), 0, &pattern(0, 3 * MIB)).await;
            device.flush().await.unwrap();
            let mut buffer = device.allocate_buffer(2 * MIB).await;
            assert!(device.write(extent(2), 0, buffer.as_ref()).await.is_err(), "overfilled");

            device.free(extent(1)).await.unwrap();
            device.free(extent(1)).await.expect("freeing a freed extent failed");
            device.flush().await.unwrap();
            assert!(device.read(extent(1), 0, buffer.as_mut()).await.is_err());
            device.write(extent(2), 0, buffer.as_ref()).await.expect("write failed");
            // Freeing doesn't make sequence numbers available again.
            assert_eq!(device.next_sequence().await.unwrap(), 3);
        });
    }

    /// The root is set durably, and kept across opening the image again.
    #[tokio::test(flavor = "multi_thread")]
    async fn roots() {
        let (_dir, device, image) = device(MIB as u64).await;
        block_on(async {
            assert_eq!(device.root().await.unwrap(), Vec::<u8>::new());
            write(&device, extent(1), 0, &pattern(0, BLOCK as usize)).await;
            device.commit_root(b"first").await.unwrap();
            device.commit_root(b"second").await.unwrap();
            assert_eq!(device.root().await.unwrap(), b"second");
        });
        drop(device);
        let device = ImageDevice::open(&image, Default::default()).await.unwrap();
        assert_eq!(block_on(device.root()).unwrap(), b"second");
        assert_eq!(
            block_on(read(&device, extent(1), 0, BLOCK as usize)),
            pattern(0, BLOCK as usize)
        );
    }
}
