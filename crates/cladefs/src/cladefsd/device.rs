// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a cell the daemon serves as a device (`storage_device::Device`), the interface a
//! filesystem keeps itself on: for what keeps itself on the host's daemon from outside a VM, as a
//! SQLite cell does (crates/sqlite_cladefs, TASK-300). Its reads of sealed extents are the
//! daemon's, from its cache, which every cell and VM it serves shares; its writes, frees and
//! commits are requests of the daemon, as a VM's device's are. Its commits and flushes are local,
//! synced to the host's disk (`COMMIT_LOCAL`), as a database's on a local filesystem are, not
//! waiting for the bucket (the user, 2026-11-09, TASK-301); closing it waits for that.

use super::CellClient;
use crate::{Commit, VolatileSlice, VolatileSliceMut, WriteBarriers};
use anyhow::{Context as _, Error, ensure};
use async_trait::async_trait;
use std::sync::Arc;
use storage_device::buffer::{BufferFuture, BufferRef, MutableBufferRef};
use storage_device::buffer_allocator::{BufferAllocator, BufferSource};
use storage_device::{Device, ExtentId, ReadOptions, WriteFlags, WriteOptions};

/// The size of the transfer buffers' memory.
const TRANSFER_HEAP_SIZE: usize = 32 << 20;

/// A cell the daemon serves, as a device. See the module docs.
pub struct DaemonDevice {
    client: Arc<CellClient>,
    allocator: BufferAllocator,
    block_count: u64,
}

impl DaemonDevice {
    /// `client` as a device.
    pub fn new(client: Arc<CellClient>) -> Result<Self, Error> {
        let block_size = crate::host_cell::BLOCK_SIZE;
        let capacity = client.info().map_err(Error::new).context("the cell's capacity")?.capacity;
        let allocator =
            BufferAllocator::new(block_size as usize, BufferSource::new(TRANSFER_HEAP_SIZE));
        Ok(Self { client, allocator, block_count: capacity / block_size as u64 })
    }

    /// Its client of the cell.
    pub fn client(&self) -> &Arc<CellClient> {
        &self.client
    }
}

#[async_trait]
impl Device for DaemonDevice {
    fn allocate_buffer(&self, size: usize) -> BufferFuture<'_> {
        self.allocator.allocate_buffer(size)
    }

    fn block_size(&self) -> u32 {
        crate::host_cell::BLOCK_SIZE
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }

    async fn read_with_opts(
        &self,
        extent: ExtentId,
        offset: u64,
        mut buffer: MutableBufferRef<'_>,
        _read_opts: ReadOptions,
    ) -> Result<(), Error> {
        let bytes = buffer.try_as_mut_slice().expect("a buffer of the heap's");
        let mut slices = [VolatileSliceMut::from(bytes)];
        let read = self.client.read_async(extent, offset, &mut slices).await;
        read.map_err(Error::new).with_context(|| format!("reading {extent:?} at {offset}"))
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
        let barriers = WriteBarriers {
            before: flags.contains(WriteFlags::PRE_BARRIER),
            after: flags.contains(WriteFlags::POST_BARRIER).then_some(Commit::Local),
            root: write_opts.root,
        };
        let bytes = buffer.try_as_slice().expect("a buffer of the heap's");
        let slices = [VolatileSlice::from(bytes)];
        let written = self.client.write_with_barriers_async(extent, offset, &slices, barriers);
        written.await.map_err(Error::new).with_context(|| format!("writing {extent:?} at {offset}"))
    }

    fn cell(&self) -> u64 {
        self.client.cell()
    }

    async fn next_sequence(&self) -> Result<u64, Error> {
        Ok(self.client.info_async().await.map_err(Error::new)?.next_sequence)
    }

    async fn sequence_floor(&self) -> Result<u64, Error> {
        Ok(self.client.info_async().await.map_err(Error::new)?.sequence_floor)
    }

    async fn free(&self, extent: ExtentId) -> Result<(), Error> {
        self.client.free_async(extent).await.map_err(Error::new)
    }

    /// In as few requests as the daemon takes (`FREE_MANY`): if one fails, each is said to have,
    /// those freed before it too, which freeing again does nothing to.
    async fn free_extents(&self, extents: &[ExtentId]) -> Vec<Result<(), Error>> {
        match self.client.free_many_async(extents).await {
            Ok(()) => extents.iter().map(|_| Ok(())).collect(),
            Err(errno) => extents.iter().map(|_| Err(Error::new(errno))).collect(),
        }
    }

    async fn root(&self) -> Result<Vec<u8>, Error> {
        self.client.root_async().await.map_err(Error::new)
    }

    async fn commit_root(&self, root: &[u8]) -> Result<(), Error> {
        self.client.commit_async(Some(root), Commit::Local).await.map_err(Error::new)
    }

    /// Waits for what it committed to be in object storage, as a closed image's cell is whole
    /// in its bucket.
    async fn close(&self) -> Result<(), Error> {
        self.client.commit_async(None, Commit::Uploaded).await.map_err(Error::new)
    }

    async fn flush(&self) -> Result<(), Error> {
        self.client.commit_async(None, Commit::Local).await.map_err(Error::new)
    }

    fn is_read_only(&self) -> bool {
        false
    }
}
