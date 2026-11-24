// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Fuchsia's `src/storage/lib/storage_device/src/lib.rs`, without the block device and pinned
// buffers, which need FIDL protocols and BTIs, and without `FileBackedDevice`, which upstream uses
// for host tools and tests: it blocks its callers' threads, and trims by overwriting. Images are
// cladefs's own device (crates/cladefs/src/storage/image.rs). A device is an extent store (backlog
// doc-4): data lives in extents the filesystem names, which it frees whole (`Device::free`), in
// place of trimming ranges of a block device; extent IDs are the device's cell and a sequence
// number (`Device::cell`, `Device::next_sequence`). The root, the device's one mutable record,
// names where the filesystem starts (`Device::root`, `Device::commit_root`).

//! `storage_device` provides a higher-level portable API ([`Device`]) for interacting with block
//! devices.  This library also provides the [`Buffer`] type which is a contiguous, splittable
//! transfer buffer allocated out of a shared pool which can be used for I/O.
//!
//! The two main implementations are
//!   - [`block_device::BlockDevice`], which is backed by a [`block_client::Device`] and used on
//!     Fuchsia devices, and
//!   - [`file_backed_device::FileBackedDevice`], which is backed by a regular file and is portable.

use crate::buffer::{BufferFuture, BufferRef, MutableBufferRef};
use anyhow::{Error, bail};
use async_trait::async_trait;
use bitflags::bitflags;
use futures::channel::oneshot::{Sender, channel};
use std::future::Future;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

pub mod buffer;
pub mod buffer_allocator;
mod extent;

pub use extent::{ExtentId, ExtentRange, NoExtent, OpenExtents, OtherCell, SealedExtent};
pub mod fake_device;

/// cladefs: the most bytes a device's root holds ([`Device::commit_root`]): an extent device's,
/// doc-4's `max_root`, which the extent daemon refuses roots past (`EINVAL`) and Firecracker's
/// extent device carries no more of (TASK-149.17). A filesystem on extents keeps its root within it
/// (TASK-551).
pub const MAX_ROOT: usize = 4096;

bitflags! {
    /// Options that may be used for writes.
    #[repr(transparent)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct WriteFlags: u32 {
        const FORCE_ACCESS = 1;
        const PRE_BARRIER = 2;
        /// cladefs: a barrier follows the write, as a flush once it completes would: one request,
        /// where a device's requests are round trips to its host (doc-4).
        const POST_BARRIER = 4;
    }
}

/// How a write goes to the device ([`Device::write_with_opts`]). cladefs: Fuchsia's
/// `block_protocol` crate's, without its inline encryption, which cladefs leaves to its extents.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WriteOptions<'a> {
    pub flags: WriteFlags,
    /// cladefs: with `POST_BARRIER`, the root its barrier sets, as a commit of the root does
    /// ([`Device::commit_root`]): in the write's one request, where a device's requests are round
    /// trips to its host (doc-4).
    pub root: Option<&'a [u8]>,
}

#[async_trait]
/// Device is an abstract representation of an underlying block device.
pub trait Device: Send + Sync {
    /// Allocates a transfer buffer of at least `size` bytes for doing I/O with the device.
    /// The actual size of the buffer will be rounded up to a block-aligned size.
    /// Blocks until enough capacity is available in the buffer.
    fn allocate_buffer(&self, size: usize) -> BufferFuture<'_>;

    /// Cleans up any transfer buffers that are no longer in use.
    fn clean_transfer_buffer(&self) {}

    /// Returns the block size of the device. Buffers are aligned to block-aligned chunks.
    fn block_size(&self) -> u32;

    /// Returns the number of blocks of the device.
    fn block_count(&self) -> u64;

    /// Returns the size in bytes of the device.
    fn size(&self) -> u64 {
        self.block_size() as u64 * self.block_count()
    }

    /// Fills `buffer` with blocks read from `offset` in `extent`. Reading an extent the device
    /// doesn't hold fails with `NoExtent`.
    async fn read(
        &self,
        extent: ExtentId,
        offset: u64,
        buffer: MutableBufferRef<'_>,
    ) -> Result<(), Error>;

    /// Writes the contents of `buffer` to `extent` at `offset`. An extent is written once: from
    /// its first write until the next barrier (a flush, a write with `PRE_BARRIER`, or one with
    /// `POST_BARRIER`, which seals the extent it writes too), after which writing it fails with
    /// `SealedExtent`. cladefs: a `POST_BARRIER` write's barrier sets the root to its options'
    /// `root`, if given, as `commit_root` would.
    fn write<'a>(
        &'a self,
        extent: ExtentId,
        offset: u64,
        buffer: BufferRef<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'a>> {
        self.write_with_opts(extent, offset, buffer, WriteOptions::default())
    }

    /// Writes the contents of `buffer` to `extent` at `offset`.
    async fn write_with_opts(
        &self,
        extent: ExtentId,
        offset: u64,
        buffer: BufferRef<'_>,
        write_opts: WriteOptions<'_>,
    ) -> Result<(), Error>;

    /// The cell whose extents the filesystem on the device mints: the first half of their IDs.
    fn cell(&self) -> u64;

    /// One more than the highest sequence number of any extent of the device's cell that the
    /// device holds or has held, freed or not: where minting resumes after a crash, which may
    /// leave extents written whose references the filesystem lost.
    async fn next_sequence(&self) -> Result<u64, Error>;

    /// cladefs: the lowest sequence number the filesystem may mint from: the device's cell may hold
    /// extents another writer minted below it that the device doesn't hold (backlog doc-7,
    /// The head). Minting starts past it, and past `next_sequence`, which alone bounds the extents
    /// the device holds.
    async fn sequence_floor(&self) -> Result<u64, Error> {
        Ok(0)
    }

    /// Frees `extent`, which nothing refers to any more. Freeing an extent the device does not hold
    /// does nothing.
    async fn free(&self, extent: ExtentId) -> Result<(), Error>;

    /// cladefs: frees `extents`, as `free` frees each: in as few requests as the device takes, and
    /// returns each's result, in order. By default, one `free` each, at once.
    async fn free_extents(&self, extents: &[ExtentId]) -> Vec<Result<(), Error>> {
        futures::future::join_all(extents.iter().map(|extent| self.free(*extent))).await
    }

    /// Whether `extent` is known to be open: minted since the last barrier, which seals it, so that
    /// it can be written again until the next (backlog doc-4, [`OpenExtents`]). One a barrier came
    /// after, which a write then reached, the device takes, and seals at the next, is not: it never
    /// is again. A device that doesn't know says it isn't.
    fn is_open(&self, _extent: ExtentId) -> bool {
        false
    }

    /// Records that `extent`, which the filesystem just minted and which nothing has written yet, is
    /// open: it is to be written before the next barrier, which seals it. A device that doesn't
    /// know whether extents are open ignores it.
    fn opened(&self, _extent: ExtentId) {}

    /// Returns the root: where the filesystem on the device starts. It is empty until set.
    async fn root(&self) -> Result<Vec<u8>, Error>;

    /// Flushes the device, and then sets the root to `root`, durably: a barrier, after which the
    /// root and everything written before it survive a crash. cladefs: a guest's extent-store device
    /// orders it only, as its other barriers, and makes it durable with the next durable commit,
    /// as doc-2 asks only of sync and unmount: a crash keeps the root and what came before it, or
    /// neither (doc-4, Ordering).
    async fn commit_root(&self, root: &[u8]) -> Result<(), Error>;

    /// Closes the block device. It is an error to continue using the device after this, but close
    /// itself is idempotent.
    async fn close(&self) -> Result<(), Error>;

    /// Flush the device.
    async fn flush(&self) -> Result<(), Error>;

    /// Reopens the device, making it usable again. (Only implemented for testing devices.)
    fn reopen(&self, _read_only: bool) {
        unreachable!();
    }
    /// Returns whether the device is read-only.
    fn is_read_only(&self) -> bool;

    /// cladefs: whether commits on this device are atomic across all writes completed before the
    /// barrier — no uncommitted write can survive a crash without every earlier uncommitted write
    /// (`local_cell`, doc-4, board 7537). When true, Fxfs with `barriers_enabled` does not need a
    /// `PRE_BARRIER` before journal writes to prevent data/journal reordering; `POST_BARRIER`
    /// alone orders and seals both data and journal atomically in one commit. Defaults to `false`
    /// (safe for devices whose crash model can reorder writes between barriers).
    fn atomic_commits(&self) -> bool {
        false
    }

    /// cladefs: the IDs of the extents the device holds, in order. (Only implemented for testing
    /// devices.)
    fn extents(&self) -> Vec<ExtentId> {
        unreachable!();
    }

    /// cladefs: how far `extent` is written, the end of its furthest write, if the device holds it.
    /// (Only implemented for testing devices.)
    fn extent_len(&self, _extent: ExtentId) -> Option<u64> {
        unreachable!();
    }

    /// Returns a snapshot of the device.
    fn snapshot(&self) -> Result<DeviceHolder, Error> {
        bail!("Not supported");
    }

    /// Returns a snapshot of the device as a fork of it whose cell is `cell`, which holds the
    /// device's extents and writes its own (backlog doc-4).
    fn fork(&self, _cell: u64) -> Result<DeviceHolder, Error> {
        bail!("Not supported");
    }

    /// Discards random blocks since the last flush.
    fn discard_random_since_last_flush(&self) -> Result<(), Error> {
        bail!("Not supported");
    }

    /// Poisons a device to panic on drop. Used to find hanging references.
    fn poison(&self) -> Result<(), Error> {
        bail!("Not supported");
    }
}

// Arc<dyn Device> can easily be cloned and supports concurrent access, but sometimes exclusive
// access is required, in which case APIs should accept DeviceHolder.  It doesn't guarantee there
// aren't some users that hold an Arc<dyn Device> somewhere, but it does mean that something that
// accepts a DeviceHolder won't be sharing the device with something else that accepts a
// DeviceHolder.  For example, FxFilesystem accepts a DeviceHolder which means that you cannot
// create two FxFilesystem instances that are both sharing the same device.
pub struct DeviceHolder {
    device: ManuallyDrop<Arc<dyn Device>>,
    on_drop: OnceLock<Sender<DeviceHolder>>,
}

impl DeviceHolder {
    pub fn new(device: impl Device + 'static) -> Self {
        DeviceHolder { device: ManuallyDrop::new(Arc::new(device)), on_drop: OnceLock::new() }
    }

    // Ensures there are no dangling references to the device. Useful for tests to ensure orderly
    // shutdown.
    pub fn ensure_unique(&self) {
        assert_eq!(Arc::strong_count(&self.device), 1);
    }

    pub fn take_when_dropped(&self) -> impl Future<Output = DeviceHolder> + use<> {
        let (sender, receiver) = channel::<DeviceHolder>();
        self.on_drop
            .set(sender)
            .unwrap_or_else(|_| panic!("take_when_dropped should only be called once"));
        async { receiver.await.unwrap() }
    }
}

impl Drop for DeviceHolder {
    fn drop(&mut self) {
        if let Some(sender) = self.on_drop.take() {
            // SAFETY: `device` is not used again.
            let device = ManuallyDrop::new(unsafe { ManuallyDrop::take(&mut self.device) });
            // We don't care if this fails to send.
            let _ = sender.send(DeviceHolder { device, on_drop: OnceLock::new() });
        } else {
            // SAFETY: `device` is not used again.
            unsafe { ManuallyDrop::drop(&mut self.device) }
        }
    }
}

impl Deref for DeviceHolder {
    type Target = Arc<dyn Device>;

    fn deref(&self) -> &Self::Target {
        &self.device
    }
}

#[cfg(test)]
mod tests {
    use super::DeviceHolder;
    use crate::fake_device::FakeDevice;

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_take_when_dropped() {
        let holder = DeviceHolder::new(FakeDevice::new(1, 512));
        let fut = holder.take_when_dropped();
        std::mem::drop(holder);
        fut.await.ensure_unique();
    }
}
