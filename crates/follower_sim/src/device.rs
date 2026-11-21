// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Fxfs devices on an extent store (crates/local_cell, backlog doc-4): the writer's, on the
//! cell, and a follower's, on a view of it ([`local_cell::View`]), read-only, as of the
//! view's commit. Extents' I/O, commits and frees go to the cell directly, as its locks are
//! Shuttle's in the simulation (cladefs's own image device moves them off Fxfs's threads instead).

use anyhow::{Error, bail};
use async_trait::async_trait;
use fuchsia_sync::Mutex;
use local_cell::{Description, Dir, LocalCell, View};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use storage_device::buffer::{BufferFuture, BufferRef, MutableBufferRef};
use storage_device::buffer_allocator::{BufferAllocator, BufferSource};
use storage_device::{Device, ExtentId, Previous, ReadOptions, WriteFlags, WriteOptions};

/// The size of the transfer buffers' memory.
const TRANSFER_HEAP_SIZE: usize = 4 << 20;

fn allocator(description: &Description) -> BufferAllocator {
    BufferAllocator::new(description.block_size as usize, BufferSource::new(TRANSFER_HEAP_SIZE))
}

/// Lets go of what a follower's view held of the commits before the one it advanced to, when
/// dropped.
struct LetGo(Arc<Mutex<View>>, u64);

impl Drop for LetGo {
    fn drop(&mut self) {
        self.0.lock().let_go(self.1);
    }
}

/// The writer's device: the cell. A flush commits the cell, durably.
pub struct CellDevice<D: Dir> {
    cell: Arc<LocalCell<D>>,
    description: Description,
    allocator: BufferAllocator,
}

impl<D: Dir> CellDevice<D> {
    pub fn new(cell: Arc<LocalCell<D>>) -> Self {
        let description = cell.description();
        Self { allocator: allocator(&description), cell, description }
    }
}

#[async_trait]
impl<D: Dir + 'static> Device for CellDevice<D> {
    fn allocate_buffer(&self, size: usize) -> BufferFuture<'_> {
        self.allocator.allocate_buffer(size)
    }

    fn block_size(&self) -> u32 {
        self.description.block_size
    }

    fn block_count(&self) -> u64 {
        self.description.capacity / self.description.block_size as u64
    }

    async fn read_with_opts(
        &self,
        extent: ExtentId,
        offset: u64,
        mut buffer: MutableBufferRef<'_>,
        _read_opts: ReadOptions,
    ) -> Result<(), Error> {
        let mut data = vec![0; buffer.len()];
        self.cell.read(extent, offset, &mut data)?;
        buffer.copy_from_slice(&data);
        Ok(())
    }

    async fn write_with_opts(
        &self,
        extent: ExtentId,
        offset: u64,
        buffer: BufferRef<'_>,
        write_opts: WriteOptions<'_>,
    ) -> Result<(), Error> {
        self.cell.write(extent, offset, &buffer.to_vec()).await?;
        if write_opts.flags.contains(WriteFlags::POST_BARRIER) {
            match write_opts.root {
                Some(root) => self.commit_root(root).await?,
                None => self.flush().await?,
            }
        }
        Ok(())
    }

    fn cell(&self) -> u64 {
        self.description.cell.id
    }

    async fn next_sequence(&self) -> Result<u64, Error> {
        Ok(self.cell.info()?.next_sequence)
    }

    async fn free(&self, extent: ExtentId) -> Result<(), Error> {
        self.cell.free(extent)
    }

    async fn root(&self) -> Result<Vec<u8>, Error> {
        self.cell.root()
    }

    async fn commit_root(&self, root: &[u8]) -> Result<(), Error> {
        self.cell.commit(Some(root)).await
    }

    async fn close(&self) -> Result<(), Error> {
        self.flush().await
    }

    async fn flush(&self) -> Result<(), Error> {
        self.cell.commit(None).await
    }

    fn is_read_only(&self) -> bool {
        false
    }
}

/// A follower's device: a view of the cell, as of its commit, read-only. Extents its
/// commit doesn't hold read as `NoExtent`, so Fxfs's journal replay ends at the view's commit.
/// Advancing moves the view to the latest commit, holding what the commits since the one it was
/// as of held until the follower lets go of them: what applying them, and its readers, read.
/// Pins are the view's.
pub struct ViewDevice<D: Dir> {
    cell: Arc<LocalCell<D>>,
    view: Arc<Mutex<View>>,
    description: Description,
    allocator: BufferAllocator,
    /// The simulation's mutation of a follower's bug: reads the cell as it is, past the view.
    past_its_view: bool,
    /// Whether reads fail, as the tests of a failing batch have them.
    failing: Arc<AtomicBool>,
}

impl<D: Dir> ViewDevice<D> {
    pub fn new(cell: Arc<LocalCell<D>>, view: View) -> Self {
        let description = cell.description();
        let allocator = allocator(&description);
        Self {
            allocator,
            cell,
            view: Arc::new(Mutex::new(view)),
            description,
            past_its_view: false,
            failing: Arc::default(),
        }
    }

    /// A follower's device with a bug, the simulation's to catch: it reads extents of commits
    /// after its view's, as of now.
    pub fn reading_past_its_view(cell: Arc<LocalCell<D>>, view: View) -> Self {
        Self { past_its_view: true, ..Self::new(cell, view) }
    }

    /// Its view, which its advances move.
    pub fn view(&self) -> Arc<Mutex<View>> {
        self.view.clone()
    }

    /// What has its reads fail, while set.
    pub fn failing(&self) -> Arc<AtomicBool> {
        self.failing.clone()
    }
}

#[async_trait]
impl<D: Dir + 'static> Device for ViewDevice<D> {
    fn allocate_buffer(&self, size: usize) -> BufferFuture<'_> {
        self.allocator.allocate_buffer(size)
    }

    fn block_size(&self) -> u32 {
        self.description.block_size
    }

    fn block_count(&self) -> u64 {
        self.description.capacity / self.description.block_size as u64
    }

    async fn read_with_opts(
        &self,
        extent: ExtentId,
        offset: u64,
        mut buffer: MutableBufferRef<'_>,
        _read_opts: ReadOptions,
    ) -> Result<(), Error> {
        if self.failing.load(Ordering::SeqCst) {
            bail!("reads fail");
        }
        let mut data = vec![0; buffer.len()];
        if self.past_its_view {
            self.cell.read_kept(extent, offset, &mut data)?;
        } else {
            self.cell.read_view(&self.view.lock(), extent, offset, &mut data)?;
        }
        buffer.copy_from_slice(&data);
        Ok(())
    }

    async fn write_with_opts(
        &self,
        extent: ExtentId,
        _offset: u64,
        _buffer: BufferRef<'_>,
        _write_opts: WriteOptions<'_>,
    ) -> Result<(), Error> {
        bail!("a follower's device is read-only: a write of {extent:?}")
    }

    fn cell(&self) -> u64 {
        self.description.cell.id
    }

    async fn next_sequence(&self) -> Result<u64, Error> {
        Ok(self.cell.info()?.next_sequence)
    }

    async fn free(&self, extent: ExtentId) -> Result<(), Error> {
        bail!("a follower's device is read-only: a free of {extent:?}")
    }

    async fn root(&self) -> Result<Vec<u8>, Error> {
        Ok(self.view.lock().root().to_vec())
    }

    async fn commit_root(&self, _root: &[u8]) -> Result<(), Error> {
        bail!("a follower's device is read-only: a commit")
    }

    async fn close(&self) -> Result<(), Error> {
        Ok(())
    }

    async fn flush(&self) -> Result<(), Error> {
        Ok(())
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn advance(&self) -> Result<Previous, Error> {
        let mut view = self.view.lock();
        view.advance_keeping()?;
        Ok(Previous::new(LetGo(self.view.clone(), view.commit())))
    }

    async fn pin(&self, extent: ExtentId) -> Result<(), Error> {
        self.view.lock().pin(extent)
    }

    async fn unpin(&self, extent: ExtentId) {
        self.view.lock().unpin(extent)
    }
}
