// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Fuchsia's `src/storage/fxfs/src/object_store/data_object_handle.rs`. cladefs's additions are
// in child modules, impl blocks of their own, so that this file keeps upstream's shape
// (TASK-160.24): `dax` (the extents a DAX window maps, backlog doc-13), `write_many` (the guest's
// writeback), `reflink` and `in_commits` (clones and punches of however many extents), and `tail`
// (zeroing past a file's end). Data lives in extents of an extent store (backlog doc-4), written
// copy-on-write; overwrite and preallocation modes are deleted (board 10600).

use crate::errors::FxfsError;
use crate::log::*;
use crate::object_handle::{
    LayerObject, ObjectHandle, ObjectProperties, ReadObjectHandle, WriteBytes, WriteObjectHandle,
};
use crate::object_store::object_manager::ObjectManager;
use crate::object_store::object_record::{
    AttributeKey, ObjectAttributes, ObjectItem, ObjectKey, ObjectKind, ObjectValue, Timestamp,
};
use crate::object_store::store_object_handle::NeedsTrim;
use crate::object_store::transaction::{
    self, AssocObj, AssociatedObject, LockKey, Mutation, ObjectStoreMutation, Operation, Options,
    ReadGuard, Transaction, lock_keys,
};
use crate::object_store::{
    AttributeId, HandleOptions, HandleOwner, MutableNodeAttributes, StoreObjectHandle, TrimMode,
    TrimResult,
};
use crate::range::RangeExt;
use anyhow::{Error, anyhow, bail, ensure};
use futures::TryStreamExt;
use std::cmp::min;
use std::future::Future;
use std::ops::{Deref, Range};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{self, AtomicU64};
use storage_device::ExtentRange;
use storage_device::buffer::{Buffer, BufferFuture, BufferRef, MutableBufferRef};
use storage_units::BlockSize;

// cladefs: its additions (TASK-160.24).
mod dax;
mod in_commits;
mod reflink;
mod tail;
#[cfg(test)]
mod testing;
mod write_many;
pub use dax::ExtentAccess;
pub use write_many::{FAIL_COMMITS, WriteAttrs};

/// How much data each transaction will cover when writing an attribute across batches. Pulled from
/// `FLUSH_BATCH_SIZE` in paged_object_handle.rs.
pub const WRITE_ATTR_BATCH_SIZE: usize = 524_288;

/// DataObjectHandle is a typed handle for file-like objects that store data in the default data
/// attribute. In addition to traditional files, this means things like the journal, superblocks,
/// and layer files.
///
/// It caches the content size of the data attribute it was configured for, and has helpers for
/// complex extent manipulation, as well as implementations of ReadObjectHandle and
/// WriteObjectHandle.
pub struct DataObjectHandle<S: HandleOwner> {
    handle: StoreObjectHandle<S>,
    attribute_id: AttributeId,
    content_size: AtomicU64,
}

/// Represents the mapping of a file's contents to the physical storage backing it.
#[derive(Debug, Clone)]
pub struct FileExtent {
    logical_offset: u64,
    device_range: ExtentRange,
}

impl FileExtent {
    pub fn new(logical_offset: u64, device_range: ExtentRange) -> Result<Self, Error> {
        // Ensure `device_range` is valid.
        let length = device_range.range.length()?;
        // Ensure no overflow when we calculate the end of the logical range.
        let _ = logical_offset.checked_add(length).ok_or(FxfsError::OutOfRange)?;
        Ok(Self { logical_offset, device_range })
    }
}

impl FileExtent {
    pub fn length(&self) -> u64 {
        // SAFETY: We verified that the device_range's length is valid in Self::new.
        unsafe { self.device_range.range.unchecked_length() }
    }

    pub fn logical_offset(&self) -> u64 {
        self.logical_offset
    }

    pub fn logical_range(&self) -> Range<u64> {
        // SAFETY: We verified logical_offset plus device_range length won't overflow in Self::new.
        unsafe { self.logical_offset..self.logical_offset.unchecked_add(self.length()) }
    }

    pub fn device_range(&self) -> &ExtentRange {
        &self.device_range
    }
}

impl<S: HandleOwner> Deref for DataObjectHandle<S> {
    type Target = StoreObjectHandle<S>;
    fn deref(&self) -> &Self::Target {
        &self.handle
    }
}

impl<S: HandleOwner> DataObjectHandle<S> {
    pub fn new(
        owner: Arc<S>,
        object_id: u64,
        attribute_id: AttributeId,
        size: u64,
        options: HandleOptions,
    ) -> Self {
        Self {
            handle: StoreObjectHandle::new(owner, object_id, options),
            attribute_id,
            content_size: AtomicU64::new(size),
        }
    }

    pub fn attribute_id(&self) -> AttributeId {
        self.attribute_id
    }

    /// Consumes the `DataObjectHandle` and returns the `StoreObjectHandle` that it contained.
    pub fn into_store_object_handle(self) -> StoreObjectHandle<S> {
        self.handle
    }

    // Returns a new aligned buffer (reading the head and tail blocks if necessary) with a copy of
    // the data from `buf`.
    async fn align_buffer(
        &self,
        offset: u64,
        buf: BufferRef<'_>,
    ) -> Result<(std::ops::Range<u64>, Buffer<'_>), Error> {
        self.handle.align_buffer(self.attribute_id(), offset, buf).await
    }

    /// Zeroes the given range.  The range must be aligned.  Returns the amount of data deallocated.
    pub async fn zero(
        &self,
        transaction: &mut Transaction<'_>,
        range: Range<u64>,
    ) -> Result<(), Error> {
        self.handle.zero(transaction, self.attribute_id(), range).await
    }

    pub async fn txn_write<'a>(
        &'a self,
        transaction: &mut Transaction<'a>,
        offset: u64,
        buf: BufferRef<'_>,
    ) -> Result<(), Error> {
        if buf.is_empty() {
            return Ok(());
        }
        let (aligned, mut transfer_buf) = self.align_buffer(offset, buf).await?;
        self.multi_write(
            transaction,
            self.attribute_id(),
            std::slice::from_ref(&aligned),
            transfer_buf.as_mut(),
        )
        .await?;
        self.txn_grow_size(transaction, offset + buf.len() as u64, &[aligned]).await
    }

    // Writes to multiple ranges with data provided in `buf`.  The ranges must all be aligned and no
    // change to content size is applied; the caller is responsible for updating size if required.
    pub async fn multi_write<'a>(
        &'a self,
        transaction: &mut Transaction<'a>,
        attribute_id: AttributeId,
        ranges: &[Range<u64>],
        buf: MutableBufferRef<'_>,
    ) -> Result<(), Error> {
        // cladefs: box `multi_write` in a non-inlined frame so compaction and write paths stay
        // small on 16 KiB guest kernel stacks (TASK-26.9).
        crate::boxed(|| self.handle.multi_write(transaction, attribute_id, ranges, buf)).await
    }

    // Within a transaction, the size of the object might have changed, so get the size from there
    // if it exists, otherwise, fall back on the cached size.
    fn txn_get_size(&self, transaction: &Transaction<'_>) -> u64 {
        transaction
            .get_object_mutation(
                self.store().store_object_id,
                ObjectKey::attribute(
                    self.object_id(),
                    self.attribute_id(),
                    AttributeKey::Attribute,
                ),
            )
            .and_then(|m| {
                if let ObjectItem { value: ObjectValue::Attribute { size }, .. } = m.item {
                    Some(size)
                } else {
                    None
                }
            })
            .unwrap_or_else(|| self.get_size())
    }

    pub async fn txn_update_size<'a>(
        &'a self,
        transaction: &mut Transaction<'a>,
        new_size: u64,
    ) -> Result<(), Error> {
        let key =
            ObjectKey::attribute(self.object_id(), self.attribute_id(), AttributeKey::Attribute);
        let mut mutation = if let Some(mutation) =
            transaction.get_object_mutation(self.store().store_object_id(), key.clone())
        {
            mutation.clone()
        } else {
            ObjectStoreMutation {
                item: self.store().tree().find(&key).await?.ok_or(FxfsError::NotFound)?,
                op: Operation::ReplaceOrInsert,
            }
        };
        if let ObjectValue::Attribute { size } = &mut mutation.item.value {
            *size = new_size;
        } else {
            bail!(anyhow!(FxfsError::Inconsistent).context("Unexpected object value"));
        }
        transaction.add_with_object(
            self.store().store_object_id(),
            Mutation::ObjectStore(mutation),
            AssocObj::Borrowed(self),
        );
        Ok(())
    }

    async fn update_allocated_size(
        &self,
        transaction: &mut Transaction<'_>,
        allocated: u64,
        deallocated: u64,
    ) -> Result<(), Error> {
        self.handle.update_allocated_size(transaction, allocated, deallocated).await
    }

    pub async fn shrink<'a>(
        &'a self,
        transaction: &mut Transaction<'a>,
        size: u64,
    ) -> Result<NeedsTrim, Error> {
        let needs_trim = self.handle.shrink(transaction, self.attribute_id(), size).await?;
        self.txn_update_size(transaction, size).await?;
        Ok(needs_trim)
    }

    pub async fn grow<'a>(
        &'a self,
        transaction: &mut Transaction<'a>,
        old_size: u64,
        size: u64,
    ) -> Result<(), Error> {
        // Before growing the file, we must make sure that a previous trim has completed.
        let store = self.store();
        while matches!(
            store
                .trim_some(
                    transaction,
                    self.object_id(),
                    self.attribute_id(),
                    TrimMode::FromOffset(old_size)
                )
                .await?,
            TrimResult::Incomplete
        ) {
            transaction.commit_and_continue().await?;
        }
        // We might need to zero out the tail of the old last block.
        self.txn_zero_tail(transaction, old_size).await?;
        self.txn_update_size(transaction, size).await?;
        Ok(())
    }

    pub async fn update_attributes<'a>(
        &self,
        transaction: &mut Transaction<'a>,
        node_attributes: Option<&MutableNodeAttributes>,
        change_time: Option<Timestamp>,
    ) -> Result<(), Error> {
        self.handle.update_attributes(transaction, node_attributes, change_time).await
    }

    /// Get the default set of transaction options for this object. This is mostly the overall
    /// default, modified by any [`HandleOptions`] held by this handle.
    pub fn default_transaction_options<'b>(&self) -> Options<'b> {
        self.handle.default_transaction_options()
    }

    pub async fn new_transaction<'b>(&self) -> Result<Transaction<'b>, Error> {
        self.new_transaction_with_options(self.default_transaction_options()).await
    }

    pub async fn new_transaction_with_options<'b>(
        &self,
        options: Options<'b>,
    ) -> Result<Transaction<'b>, Error> {
        self.handle.new_transaction_with_options(self.attribute_id(), options).await
    }

    /// Flushes the underlying device.  This is expensive and should be used sparingly.
    pub async fn flush_device(&self) -> Result<(), Error> {
        self.handle.flush_device().await
    }

    /// Reads an entire attribute.
    pub async fn read_attr(&self, attribute_id: AttributeId) -> Result<Option<Box<[u8]>>, Error> {
        self.handle.read_attr(attribute_id).await
    }

    /// Writes an entire attribute.  This *always* uses the volume data key.
    pub async fn write_attr(&self, attribute_id: AttributeId, data: &[u8]) -> Result<(), Error> {
        // Must be different attribute otherwise cached size gets out of date.
        assert_ne!(attribute_id, self.attribute_id());
        let store = self.store();
        let mut transaction = self.new_transaction().await?;
        if self.handle.write_attr(&mut transaction, attribute_id, data).await?.0 {
            transaction.commit_and_continue().await?;
            while matches!(
                store
                    .trim_some(
                        &mut transaction,
                        self.object_id(),
                        attribute_id,
                        TrimMode::FromOffset(data.len() as u64),
                    )
                    .await?,
                TrimResult::Incomplete
            ) {
                transaction.commit_and_continue().await?;
            }
        }
        transaction.commit().await?;
        Ok(())
    }

    /// Truncates a file to a given size (growing/shrinking as required).
    ///
    /// Nb: Most code will want to call truncate() instead. This method is used
    /// to update the super block -- a case where we must borrow metadata space.
    pub async fn truncate_with_options(
        &self,
        options: Options<'_>,
        size: u64,
    ) -> Result<(), Error> {
        let mut transaction = self.new_transaction_with_options(options).await?;
        let old_size = self.get_size();
        if size == old_size {
            return Ok(());
        }
        if size < old_size {
            if self.shrink(&mut transaction, size).await?.0 {
                // The file needs to be trimmed.
                transaction.commit_and_continue().await?;
                let store = self.store();
                while matches!(
                    store
                        .trim_some(
                            &mut transaction,
                            self.object_id(),
                            self.attribute_id(),
                            TrimMode::FromOffset(size)
                        )
                        .await?,
                    TrimResult::Incomplete
                ) {
                    if let Err(error) = transaction.commit_and_continue().await {
                        warn!(error:?; "Failed to trim after truncate");
                        return Ok(());
                    }
                }
                if let Err(error) = transaction.commit().await {
                    warn!(error:?; "Failed to trim after truncate");
                }
                return Ok(());
            }
        } else {
            self.grow(&mut transaction, old_size, size).await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn get_properties(&self) -> Result<ObjectProperties, Error> {
        // We don't take a read guard here since the object properties are contained in a single
        // object, which cannot be inconsistent with itself. The LSM tree does not return
        // intermediate states for a single object.
        let value = self
            .store()
            .tree
            .find_value(&ObjectKey::object(self.object_id()))
            .await?
            .expect("Unable to find object record");
        match value {
            ObjectValue::Object {
                kind: ObjectKind::File { refs, .. },
                attributes:
                    ObjectAttributes {
                        creation_time,
                        modification_time,
                        posix_attributes,
                        allocated_size,
                        access_time,
                        change_time,
                        ..
                    },
            } => Ok(ObjectProperties {
                refs,
                allocated_size,
                data_attribute_size: self.get_size(),
                creation_time,
                modification_time,
                access_time,
                change_time,
                sub_dirs: 0,
                posix_attributes,
            }),
            _ => bail!(FxfsError::NotFile),
        }
    }

    /// Returns the set of file_offset->extent mappings for this file. The extents will be sorted by
    /// their logical offset within the file.
    ///
    /// *NOTE*: This operation is potentially expensive and should generally be avoided.
    pub async fn device_extents(&self) -> Result<Vec<FileExtent>, Error> {
        let tree = &self.store().tree;
        let layer_set = tree.layer_set();
        let mut merger = layer_set.merger();
        let stream = self.handle.extent_stream(&mut merger, self.attribute_id()).await?;
        let extents: Vec<FileExtent> = stream.try_collect().await?;
        Ok(extents)
    }

    /// Returns the contents of this object. This object must be < |limit| bytes in size.
    pub async fn contents(&self, limit: usize) -> Result<Box<[u8]>, Error> {
        let size = self.get_size();
        if size > limit as u64 {
            bail!("Object too big ({} > {})", size, limit);
        }
        self.read_bytes(0..size).await
    }

    /// Reads the contents of this object within `range`. `range` does not need to be block aligned.
    ///
    /// This method should be avoided in high-performance contexts. Use `read_aligned` instead.
    pub async fn read_bytes(&self, range: Range<u64>) -> Result<Box<[u8]>, Error> {
        const MAX_READ_BYTES_CHUNK_SIZE: usize = 2 * 1024 * 1024; // 2 MiB

        ensure!(range.start <= range.end, FxfsError::InvalidArgs);
        if range.is_empty() {
            return Ok(Box::default());
        }
        let fs = self.store().filesystem();
        let guard = fs
            .lock_manager()
            .read_lock(lock_keys![LockKey::object_attribute(
                self.store().store_object_id,
                self.object_id(),
                self.attribute_id(),
            )])
            .await;

        let size = self.get_size();
        if range.start >= size {
            return Ok(Box::default());
        }
        let end = std::cmp::min(range.end, size);
        let total_to_read = (end - range.start) as usize;
        let block_size = self.block_size();
        let aligned_start = block_size.align_down(range.start);
        let aligned_end = block_size.align_up(end).ok_or(FxfsError::TooBig)?;
        let total_aligned_len = aligned_end - aligned_start;

        let buf_size = std::cmp::min(total_aligned_len, MAX_READ_BYTES_CHUNK_SIZE as u64) as usize;
        let mut buf = self.allocate_buffer(buf_size).await;
        let mut out = Vec::with_capacity(total_to_read);
        let mut current_block_offset = aligned_start;

        while current_block_offset < end {
            let bytes_read =
                self.read_aligned_locked(current_block_offset, buf.as_mut(), &guard).await?;
            let chunk_start = current_block_offset;
            let chunk_end = current_block_offset + bytes_read as u64;
            let slice_start = std::cmp::max(range.start, chunk_start);
            let slice_end = std::cmp::min(end, chunk_end);
            let buf_offset = (slice_start - chunk_start) as usize;
            let to_copy = (slice_end - slice_start) as usize;
            buf.subslice(buf_offset..buf_offset + to_copy).append_to(&mut out);
            current_block_offset = current_block_offset.saturating_add(buf_size as u64);
        }
        Ok(out.into_boxed_slice())
    }

    /// Reads up to `buf.len()` bytes from `offset` while holding `guard`.
    ///
    /// Both `offset` and `buf.len()` must be aligned to the object's `block_size()`.
    ///
    /// `guard` must be an active read lock for this object's attribute.
    ///
    /// Returns the number of bytes read up to the object's size (or 0 if `offset >= size`). Holes/
    /// sparse extents within the read range are zero-filled. Callers should not make any
    /// assumptions about the contents of the buffer past the returned read amount.
    async fn read_aligned_locked(
        &self,
        offset: u64,
        mut buf: MutableBufferRef<'_>,
        guard: &ReadGuard<'_>,
    ) -> Result<usize, Error> {
        let block_size = self.block_size();
        debug_assert!(block_size.is_aligned(offset));
        debug_assert!(block_size.is_aligned(buf.len() as u64));

        let size = self.get_size();
        if offset >= size {
            return Ok(0);
        }
        let length = min(buf.len() as u64, size - offset) as usize;
        let aligned_length =
            block_size.align_up(length as u64).ok_or(FxfsError::Inconsistent)? as usize;
        buf = buf.subslice_mut(0..aligned_length);

        self.handle
            .read_aligned_unchecked(self.attribute_id(), offset, buf.reborrow(), guard)
            .await?;
        Ok(length)
    }

    /// Fills `buf` with bytes read from `offset` on the underlying device.
    ///
    /// Both `offset` and `buf.len()` must be aligned to the object's `block_size()`.
    ///
    /// Returns the number of bytes read. If `offset >= size`, returns 0. Holes/sparse extents
    /// within the read range are zero-filled. Callers should not make any assumptions about the
    /// contents of the buffer past the returned read amount.
    ///
    /// This is an inherent version of the `ReadObjectHandle::read_aligned` trait method which
    /// avoids boxing the returned `Future`.
    pub async fn read_aligned(
        &self,
        offset: u64,
        buf: MutableBufferRef<'_>,
    ) -> Result<usize, Error> {
        let block_size = self.block_size();
        ensure!(block_size.is_aligned(offset), FxfsError::InvalidArgs);
        ensure!(block_size.is_aligned(buf.len() as u64), FxfsError::InvalidArgs);
        let fs = self.store().filesystem();
        let guard = fs
            .lock_manager()
            .read_lock(lock_keys![LockKey::object_attribute(
                self.store().store_object_id,
                self.object_id(),
                self.attribute_id(),
            )])
            .await;
        self.read_aligned_locked(offset, buf, &guard).await
    }
}

impl<S: HandleOwner> AssociatedObject for DataObjectHandle<S> {
    fn will_apply_mutation(&self, mutation: &Mutation, _object_id: u64, _manager: &ObjectManager) {
        if let Mutation::ObjectStore(ObjectStoreMutation {
            item: ObjectItem { value: ObjectValue::Attribute { size }, .. },
            ..
        }) = mutation
        {
            self.content_size.store(*size, atomic::Ordering::Relaxed);
        }
    }
}

impl<S: HandleOwner> ObjectHandle for DataObjectHandle<S> {
    fn object_id(&self) -> u64 {
        self.handle.object_id()
    }

    fn allocate_buffer(&self, size: usize) -> BufferFuture<'_> {
        self.handle.allocate_buffer(size)
    }

    fn block_size(&self) -> BlockSize {
        self.handle.block_size()
    }
}

impl<S: HandleOwner> ReadObjectHandle for DataObjectHandle<S> {
    fn read_aligned<'a, 'b, 'c>(
        &'a self,
        offset: u64,
        buf: MutableBufferRef<'b>,
    ) -> Pin<Box<dyn Future<Output = Result<usize, Error>> + Send + 'c>>
    where
        'a: 'c,
        'b: 'c,
        Self: 'c,
    {
        Box::pin(DataObjectHandle::read_aligned(self, offset, buf))
    }

    fn get_size(&self) -> u64 {
        self.content_size.load(atomic::Ordering::Relaxed)
    }
}

impl<S: HandleOwner> LayerObject for DataObjectHandle<S> {}

impl<S: HandleOwner> WriteObjectHandle for DataObjectHandle<S> {
    async fn write_or_append(&self, offset: Option<u64>, buf: BufferRef<'_>) -> Result<u64, Error> {
        let offset = offset.unwrap_or_else(|| self.get_size());
        let mut transaction = self.new_transaction().await?;
        self.txn_write(&mut transaction, offset, buf).await?;
        let new_size = self.txn_get_size(&transaction);
        transaction.commit().await?;
        Ok(new_size)
    }

    async fn truncate(&self, size: u64) -> Result<(), Error> {
        self.truncate_with_options(self.default_transaction_options(), size).await
    }

    async fn flush(&self) -> Result<(), Error> {
        Ok(())
    }
}

/// Like object_handle::Writer, but allows custom transaction options to be set, and makes every
/// write go directly to the handle in a transaction.
pub struct DirectWriter<'a, S: HandleOwner> {
    handle: &'a DataObjectHandle<S>,
    options: transaction::Options<'a>,
    buffer: Buffer<'a>,
    offset: u64,
    buf_offset: usize,
}

const BUFFER_SIZE: usize = 1_048_576;

impl<S: HandleOwner> Drop for DirectWriter<'_, S> {
    fn drop(&mut self) {
        if self.buf_offset != 0 {
            warn!("DirectWriter: dropping data, did you forget to call complete?");
        }
    }
}

impl<'a, S: HandleOwner> DirectWriter<'a, S> {
    pub async fn new(
        handle: &'a DataObjectHandle<S>,
        options: transaction::Options<'a>,
    ) -> DirectWriter<'a, S> {
        Self {
            handle,
            options,
            buffer: handle.allocate_buffer(BUFFER_SIZE).await,
            offset: 0,
            buf_offset: 0,
        }
    }

    async fn flush(&mut self) -> Result<(), Error> {
        // cladefs: box flush stages in non-inlined frames so `DirectWriter` / `KeptWriter` /
        // `PersistentLayerWriter` stay small on 16 KiB guest kernel stacks (TASK-26.9).
        let mut transaction =
            crate::boxed(|| self.handle.new_transaction_with_options(self.options)).await?;
        crate::boxed(|| {
            self.handle.txn_write(
                &mut transaction,
                self.offset,
                self.buffer.subslice(0..self.buf_offset),
            )
        })
        .await?;
        crate::boxed(|| transaction.commit()).await?;
        self.offset += self.buf_offset as u64;
        self.buf_offset = 0;
        Ok(())
    }
}

impl<'a, S: HandleOwner> WriteBytes for DirectWriter<'a, S> {
    fn block_size(&self) -> BlockSize {
        self.handle.block_size()
    }

    async fn write_bytes(&mut self, mut buf: &[u8]) -> Result<(), Error> {
        while buf.len() > 0 {
            let to_do = std::cmp::min(buf.len(), BUFFER_SIZE - self.buf_offset);
            self.buffer
                .subslice_mut(self.buf_offset..self.buf_offset + to_do)
                .copy_from_slice(&buf[..to_do]);
            self.buf_offset += to_do;
            if self.buf_offset == BUFFER_SIZE {
                self.flush().await?;
            }
            buf = &buf[to_do..];
        }
        Ok(())
    }

    async fn complete(mut self) -> Result<u64, Error> {
        self.flush().await?;
        Ok(self.offset + self.buf_offset as u64)
    }

    async fn skip(&mut self, amount: u64) -> Result<(), Error> {
        if (BUFFER_SIZE - self.buf_offset) as u64 > amount {
            self.buffer.subslice_mut(self.buf_offset..self.buf_offset + amount as usize).fill(0);
            self.buf_offset += amount as usize;
        } else {
            self.flush().await?;
            self.offset += amount;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::errors::FxfsError;
    use crate::filesystem::{FxFilesystem, FxFilesystemBuilder, OpenFxFilesystem, SyncOptions};
    use crate::fsck::{FsckOptions, fsck_volume_with_options, fsck_with_options};
    use crate::object_handle::{
        ObjectHandle, ObjectProperties, ReadObjectHandle, WriteObjectHandle,
    };
    use crate::object_store::directory::replace_child;
    use crate::object_store::object_record::{ObjectKey, ObjectValue, Timestamp};
    use crate::object_store::transaction::{Mutation, Options, TRANSACTION_ROOM, lock_keys};
    use crate::object_store::volume::root_volume;
    use crate::object_store::{
        AttributeId, DataObjectHandle, Directory, HandleOptions, LockKey, MutableNodeAttributes,
        NewChildStoreOptions, ObjectStore, PosixAttributes,
    };
    use assert_matches::assert_matches;
    use fuchsia_async as fasync;
    use fuchsia_sync::Mutex;
    use futures::FutureExt;
    use futures::channel::oneshot::channel;
    use futures::stream::{FuturesUnordered, StreamExt};
    use std::ops::Range;
    use std::sync::Arc;
    use std::time::Duration;
    use storage_device::DeviceHolder;
    use storage_device::fake_device::FakeDevice;

    pub(super) const TEST_DEVICE_BLOCK_SIZE: u32 = 512;

    const TEST_DATA_OFFSET: u64 = 5000;
    const TEST_DATA: &[u8] = b"hello";
    const TEST_OBJECT_SIZE: u64 = 5678;
    const TEST_OBJECT_ALLOCATED_SIZE: u64 = 4096;
    const TEST_OBJECT_NAME: &str = "foo";

    pub(super) async fn test_filesystem() -> OpenFxFilesystem {
        let device = DeviceHolder::new(FakeDevice::new(8192, TEST_DEVICE_BLOCK_SIZE));
        FxFilesystem::new_empty(device).await.expect("new_empty failed")
    }

    pub(super) async fn create_test_object(
        fs: Arc<FxFilesystem>,
        write_object_test_data: bool,
    ) -> DataObjectHandle<ObjectStore> {
        let store = fs.root_store();
        let object;
        let mut transaction = fs
            .root_store()
            .new_transaction(
                lock_keys![LockKey::object(
                    store.store_object_id(),
                    store.root_directory_object_id()
                )],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        object = ObjectStore::create_object(&store, &mut transaction, HandleOptions::default())
            .await
            .expect("create_object failed");

        let root_directory =
            Directory::open(&store, store.root_directory_object_id()).await.expect("open failed");
        root_directory
            .add_child_file(&mut transaction, TEST_OBJECT_NAME, &object)
            .await
            .expect("add_child_file failed");

        if write_object_test_data {
            let align = TEST_DATA_OFFSET as usize % TEST_DEVICE_BLOCK_SIZE as usize;
            let mut buf = object.allocate_buffer(align + TEST_DATA.len()).await;
            buf.subslice_mut(align..buf.len()).copy_from_slice(TEST_DATA);
            object
                .txn_write(&mut transaction, TEST_DATA_OFFSET, buf.subslice(align..buf.len()))
                .await
                .expect("write failed");
        }
        transaction.commit().await.expect("commit failed");
        object.truncate(TEST_OBJECT_SIZE).await.expect("truncate failed");
        object
    }

    pub(super) async fn test_filesystem_and_object_with(
        write_object_test_data: bool,
    ) -> (OpenFxFilesystem, DataObjectHandle<ObjectStore>) {
        let fs = test_filesystem().await;
        let object = create_test_object(fs.clone(), write_object_test_data).await;
        (fs, object)
    }

    async fn test_filesystem_and_object() -> (OpenFxFilesystem, DataObjectHandle<ObjectStore>) {
        test_filesystem_and_object_with(true).await
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_zero_buf_len_read() {
        let (fs, object) = test_filesystem_and_object().await;
        let mut buf = object.allocate_buffer(0).await;
        assert_eq!(object.read_aligned(0u64, buf.as_mut()).await.expect("read failed"), 0);
        fs.close().await.expect("Close failed");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_beyond_eof_read() {
        let (fs, object) = test_filesystem_and_object().await;
        let offset = TEST_OBJECT_SIZE as usize - 2;
        let align = (offset as u64 % fs.block_size()) as usize;
        let len: usize = 2;
        let block_size = fs.block_size().get() as usize;

        // Unaligned buffer should fail.
        let mut unaligned_buf = object.allocate_buffer(align + len + 1).await;
        assert_matches!(
            object.read_aligned((offset - align) as u64, unaligned_buf.as_mut()).await,
            Err(e) if FxfsError::InvalidArgs.matches(&e)
        );

        let mut buf = object.allocate_buffer(block_size).await;
        buf.fill(123u8);
        assert_eq!(
            object.read_aligned((offset - align) as u64, buf.as_mut()).await.expect("read failed"),
            align + len
        );
        assert_eq!(&buf[align..align + len], &vec![0u8; len][..]);

        // Unaligned offset should fail.
        assert_matches!(
            object.read_aligned((offset - align + 1) as u64, buf.as_mut()).await,
            Err(e) if FxfsError::InvalidArgs.matches(&e)
        );

        // Reading starting at or after EOF should return 0.
        let aligned_eof = fs.block_size().align_up(TEST_OBJECT_SIZE).unwrap();
        assert_eq!(object.read_aligned(aligned_eof, buf.as_mut()).await.expect("read failed"), 0);

        // Also test read_bytes past EOF.
        let data = object
            .read_bytes(offset as u64..offset as u64 + len as u64 + 1)
            .await
            .expect("read_bytes failed");
        assert_eq!(&data[..], &vec![0u8; len]);

        fs.close().await.expect("Close failed");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_beyond_eof_read_from() {
        let (fs, object) = test_filesystem_and_object().await;
        let handle = &*object;
        let offset = TEST_OBJECT_SIZE as usize - 2;
        let align = (offset as u64 % fs.block_size()) as usize;
        let len: usize = 2;
        let block_size = fs.block_size().get() as usize;

        // Unaligned buffer should fail.
        let mut unaligned_buf = object.allocate_buffer(align + len + 1).await;
        assert_matches!(
            handle
                .read_aligned(AttributeId::DATA, (offset - align) as u64, unaligned_buf.as_mut())
                .await,
            Err(e) if FxfsError::InvalidArgs.matches(&e)
        );

        let mut buf = object.allocate_buffer(block_size).await;
        buf.fill(123u8);
        assert_eq!(
            handle
                .read_aligned(AttributeId::DATA, (offset - align) as u64, buf.as_mut())
                .await
                .expect("read failed"),
            align + len
        );
        assert_eq!(&buf[align..align + len], &vec![0u8; len][..]);

        // Unaligned offset should fail.
        assert_matches!(
            handle
                .read_aligned(AttributeId::DATA, (offset - align + 1) as u64, buf.as_mut())
                .await,
            Err(e) if FxfsError::InvalidArgs.matches(&e)
        );

        // Reading starting at or after EOF should return 0.
        let aligned_eof = fs.block_size().align_up(TEST_OBJECT_SIZE).unwrap();
        assert_eq!(
            handle
                .read_aligned(AttributeId::DATA, aligned_eof, buf.as_mut())
                .await
                .expect("read failed"),
            0
        );

        fs.close().await.expect("Close failed");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_beyond_eof_read_aligned_unchecked() {
        let (fs, object) = test_filesystem_and_object().await;
        let offset = TEST_OBJECT_SIZE as usize - 2;
        let align = (offset as u64 % fs.block_size()) as usize;
        let block_size = fs.block_size().get() as usize;
        let mut buf = object.allocate_buffer(block_size).await;
        buf.fill(123u8);
        let guard = fs
            .lock_manager()
            .read_lock(lock_keys![LockKey::object_attribute(
                object.store().store_object_id,
                object.object_id(),
                AttributeId::DATA,
            )])
            .await;
        object
            .read_aligned_unchecked(
                AttributeId::DATA,
                (offset - align) as u64,
                buf.as_mut(),
                &guard,
            )
            .await
            .expect("read failed");
        assert_eq!(&buf[align..block_size], &vec![0u8; block_size - align][..]);

        // Reading entirely past EOF with read_aligned_unchecked fills with zeros.
        let aligned_eof = fs.block_size().align_up(TEST_OBJECT_SIZE).unwrap();
        buf.fill(123u8);
        object
            .read_aligned_unchecked(AttributeId::DATA, aligned_eof, buf.as_mut(), &guard)
            .await
            .expect("read failed");
        assert_eq!(buf.to_vec(), vec![0u8; block_size]);
        fs.close().await.expect("Close failed");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_read_sparse() {
        let (fs, object) = test_filesystem_and_object().await;
        // Deliberately read not right to eof.
        let len = TEST_OBJECT_SIZE as usize - 1;
        let data = object.read_bytes(0..len as u64).await.expect("read failed");
        let mut expected = vec![0; len];
        let offset = TEST_DATA_OFFSET as usize;
        expected[offset..offset + TEST_DATA.len()].copy_from_slice(TEST_DATA);
        assert_eq!(&data[..], &expected[..]);

        let aligned_len = fs.block_size().align_up(TEST_OBJECT_SIZE).unwrap() as usize;
        let mut buf = object.allocate_buffer(aligned_len).await;
        buf.fill(123u8);
        assert_eq!(
            object.read_aligned(0, buf.as_mut()).await.expect("read failed"),
            TEST_OBJECT_SIZE as usize
        );
        let mut expected_aligned = vec![0; TEST_OBJECT_SIZE as usize];
        expected_aligned[offset..offset + TEST_DATA.len()].copy_from_slice(TEST_DATA);
        assert_eq!(&buf[0..TEST_OBJECT_SIZE as usize], &expected_aligned[..]);
        fs.close().await.expect("Close failed");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_read_after_writes_interspersed_with_flush() {
        let (fs, object) = test_filesystem_and_object().await;

        object.owner().flush().await.expect("flush failed");

        // Write more test data to the first block fo the file.
        let mut buf = object.allocate_buffer(TEST_DATA.len()).await;
        buf.copy_from_slice(TEST_DATA);
        object.write_or_append(Some(0u64), buf.as_ref()).await.expect("write failed");

        let len = TEST_OBJECT_SIZE as usize - 1;
        let data = object.read_bytes(0..len as u64).await.expect("read failed");

        let mut expected = vec![0u8; len];
        let offset = TEST_DATA_OFFSET as usize;
        expected[offset..offset + TEST_DATA.len()].copy_from_slice(TEST_DATA);
        expected[..TEST_DATA.len()].copy_from_slice(TEST_DATA);
        assert_eq!(&data[..], &expected);
        fs.close().await.expect("Close failed");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_read_after_truncate_and_extend() {
        let (fs, object) = test_filesystem_and_object().await;

        // Arrange for there to be <extent><deleted-extent><extent>.
        let mut buf = object.allocate_buffer(TEST_DATA.len()).await;
        buf.copy_from_slice(TEST_DATA);
        // This adds an extent at 0..512.
        object.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");
        // This deletes 512..1024.
        object.truncate(3).await.expect("truncate failed");
        let data = b"foo";
        let offset = 1500u64;
        let align = (offset % fs.block_size()) as usize;
        let mut buf = object.allocate_buffer(align + data.len()).await;
        buf.subslice_mut(align..buf.len()).copy_from_slice(data);
        // This adds 1024..1536.
        object
            .write_or_append(Some(1500), buf.subslice(align..buf.len()))
            .await
            .expect("write failed");

        const LEN1: usize = 1503;
        let data1 = object.read_bytes(0..LEN1 as u64).await.expect("read failed");
        let mut expected = [0; LEN1];
        expected[..3].copy_from_slice(&TEST_DATA[..3]);
        expected[1500..].copy_from_slice(b"foo");
        assert_eq!(&data1[..], &expected);

        // Also test a read that ends midway through the deleted extent.
        const LEN2: usize = 601;
        let data2 = object.read_bytes(0..LEN2 as u64).await.expect("read failed");
        assert_eq!(&data2[..], &expected[..LEN2]);
        fs.close().await.expect("Close failed");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_read_whole_blocks_with_multiple_objects() {
        let (fs, object) = test_filesystem_and_object().await;
        let block_size = object.block_size().get() as usize;
        let mut buffer = object.allocate_buffer(block_size).await;
        buffer.fill(0xaf);
        object.write_or_append(Some(0), buffer.as_ref()).await.expect("write failed");

        let store = object.owner();
        let mut transaction = fs
            .root_store()
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new_transaction failed");
        let object2 =
            ObjectStore::create_object(&store, &mut transaction, HandleOptions::default())
                .await
                .expect("create_object failed");
        transaction.commit().await.expect("commit failed");
        let mut ef_buffer = object.allocate_buffer(block_size).await;
        ef_buffer.fill(0xef);
        object2.write_or_append(Some(0), ef_buffer.as_ref()).await.expect("write failed");

        let mut buffer = object.allocate_buffer(block_size).await;
        buffer.fill(0xaf);
        object
            .write_or_append(Some(block_size as u64), buffer.as_ref())
            .await
            .expect("write failed");
        object.truncate(3 * block_size as u64).await.expect("truncate failed");
        object2
            .write_or_append(Some(block_size as u64), ef_buffer.as_ref())
            .await
            .expect("write failed");

        let mut buffer = object.allocate_buffer(4 * block_size).await;
        buffer.fill(123);
        assert_eq!(
            object.read_aligned(0, buffer.as_mut()).await.expect("read failed"),
            3 * block_size
        );
        assert_eq!(&buffer[0..2 * block_size], &vec![0xaf; 2 * block_size][..]);
        assert_eq!(&buffer[2 * block_size..3 * block_size], &vec![0; block_size][..]);
        assert_eq!(
            object2.read_aligned(0, buffer.as_mut()).await.expect("read failed"),
            2 * block_size
        );
        assert_eq!(&buffer[0..2 * block_size], &vec![0xef; 2 * block_size][..]);
        fs.close().await.expect("Close failed");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_alignment() {
        let (fs, object) = test_filesystem_and_object().await;

        struct AlignTest {
            fill: u8,
            object: DataObjectHandle<ObjectStore>,
            mirror: Vec<u8>,
        }

        impl AlignTest {
            async fn new(object: DataObjectHandle<ObjectStore>) -> Self {
                let mirror =
                    object.read_bytes(0..object.get_size()).await.expect("read failed").into_vec();
                Self { fill: 0, object, mirror }
            }

            // Fills |range| of self.object with a byte value (self.fill) and mirrors the same
            // operation to an in-memory copy of the object.
            // Each subsequent call bumps the value of fill.
            // It is expected that the object and its mirror maintain identical content.
            async fn test(&mut self, range: Range<u64>) {
                let mut buf = self.object.allocate_buffer((range.end - range.start) as usize).await;
                self.fill += 1;
                buf.fill(self.fill);
                self.object
                    .write_or_append(Some(range.start), buf.as_ref())
                    .await
                    .expect("write_or_append failed");
                if range.end > self.mirror.len() as u64 {
                    self.mirror.resize(range.end as usize, 0);
                }
                self.mirror[range.start as usize..range.end as usize].fill(self.fill);
                let data = self
                    .object
                    .read_bytes(0..self.mirror.len() as u64 + 1)
                    .await
                    .expect("read failed");
                assert_eq!(&data[..], self.mirror.as_slice());
            }
        }

        let block_size = object.block_size().get();
        let mut align = AlignTest::new(object).await;

        // Fill the object to start with (with 1).
        align.test(0..2 * block_size + 1).await;

        // Unaligned head (fills with 2, overwrites that with 3).
        align.test(1..block_size).await;
        align.test(1..2 * block_size).await;

        // Unaligned tail (fills with 4 and 5).
        align.test(0..block_size - 1).await;
        align.test(0..2 * block_size - 1).await;

        // Both unaligned (fills with 6 and 7).
        align.test(1..block_size - 1).await;
        align.test(1..2 * block_size - 1).await;

        fs.close().await.expect("Close failed");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_read_bytes_chunked() {
        // 16MiB device size.
        let device = DeviceHolder::new(FakeDevice::new(32768, TEST_DEVICE_BLOCK_SIZE));
        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");
        let object = create_test_object(fs.clone(), false).await;

        const FILE_SIZE: usize = 5 * 1024 * 1024;
        let mut data = vec![0u8; FILE_SIZE];
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = (i % 251) as u8;
        }

        const WRITE_CHUNK_SIZE: usize = 1024 * 1024;
        let mut buf = object.allocate_buffer(WRITE_CHUNK_SIZE).await;
        for chunk in (0..FILE_SIZE).step_by(WRITE_CHUNK_SIZE) {
            let end = std::cmp::min(chunk + WRITE_CHUNK_SIZE, FILE_SIZE);
            buf.subslice_mut(..end - chunk).copy_from_slice(&data[chunk..end]);
            object
                .write_or_append(Some(chunk as u64), buf.subslice(..end - chunk))
                .await
                .expect("write failed");
        }

        // Read full file (> 2 MiB, requires multiple 2 MiB chunks).
        let read_data = object.read_bytes(0..FILE_SIZE as u64).await.expect("read_bytes failed");
        assert_eq!(&read_data[..], &data[..]);

        // Read an unaligned range that spans multiple 2 MiB chunks.
        let range = 123_456..4_718_592;
        let read_data = object.read_bytes(range.clone()).await.expect("read_bytes failed");
        assert_eq!(&read_data[..], &data[range.start as usize..range.end as usize]);

        // Read an unaligned range that straddles the 2 MiB boundary.
        let range = (2 * 1024 * 1024 - 500)..(2 * 1024 * 1024 + 500);
        let read_data = object.read_bytes(range.clone()).await.expect("read_bytes failed");
        assert_eq!(&read_data[..], &data[range.start as usize..range.end as usize]);

        // Read past EOF.
        let range = (FILE_SIZE as u64 - 100)..(FILE_SIZE as u64 + 1000);
        let read_data = object.read_bytes(range).await.expect("read_bytes failed");
        assert_eq!(&read_data[..], &data[(FILE_SIZE - 100)..]);

        // Read entirely past EOF.
        let range = (FILE_SIZE as u64 + 10)..(FILE_SIZE as u64 + 100);
        let read_data = object.read_bytes(range).await.expect("read_bytes failed");
        assert!(read_data.is_empty());

        // Test contents() method.
        assert!(object.contents(FILE_SIZE - 1).await.is_err());
        let contents = object.contents(usize::MAX).await.expect("contents failed");
        assert_eq!(&contents[..], &data[..]);

        // Test read_bytes with empty ranges.
        let empty = object.read_bytes(0..0).await.expect("read_bytes failed");
        assert!(empty.is_empty());
        let empty = object.read_bytes(100..100).await.expect("read_bytes failed");
        assert!(empty.is_empty());

        fs.close().await.expect("Close failed");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_truncate_deallocates_old_extents() {
        let (fs, object) = test_filesystem_and_object().await;
        let mut buf = object.allocate_buffer(5 * fs.block_size().get() as usize).await;
        buf.fill(0xaa);
        object.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");

        let allocator = fs.allocator();
        let allocated_before = allocator.get_allocated_bytes();
        object.truncate(fs.block_size().get()).await.expect("truncate failed");
        let allocated_after = allocator.get_allocated_bytes();
        assert!(
            allocated_after < allocated_before,
            "before = {} after = {}",
            allocated_before,
            allocated_after
        );
        fs.close().await.expect("Close failed");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_truncate_zeroes_tail_block() {
        let (fs, object) = test_filesystem_and_object().await;

        WriteObjectHandle::truncate(&object, TEST_DATA_OFFSET + 3).await.expect("truncate failed");
        WriteObjectHandle::truncate(&object, TEST_DATA_OFFSET + TEST_DATA.len() as u64)
            .await
            .expect("truncate failed");

        let mut buf = object.allocate_buffer(fs.block_size().get() as usize).await;
        let offset = (TEST_DATA_OFFSET % fs.block_size()) as usize;
        object
            .read_aligned(TEST_DATA_OFFSET - offset as u64, buf.as_mut())
            .await
            .expect("read failed");

        let mut expected = TEST_DATA.to_vec();
        expected[3..].fill(0);
        assert_eq!(&buf[offset..offset + expected.len()], &expected[..]);
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_trim() {
        // Format a new filesystem. cladefs: of 16 MiB, not 8: the object below, of more extents than
        // a transaction can free, left the second pass's truncate, which reserves room for its
        // transaction, as upstream's does, less of 8 MiB than that, now and then, as compactions
        // went (TASK-181).
        let device = DeviceHolder::new(FakeDevice::new(32768, TEST_DEVICE_BLOCK_SIZE));
        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");
        let block_size = fs.block_size();
        root_volume(fs.clone())
            .await
            .expect("root_volume failed")
            .new_volume("test", NewChildStoreOptions::default())
            .await
            .expect("volume failed");
        fs.close().await.expect("close failed");
        let device = fs.take_device().await;
        device.reopen(false);

        // To test trim, we open the filesystem and set up a post commit hook that runs after every
        // transaction.  When the hook triggers, we can fsck the volume, take a snapshot of the
        // device and check that it gets replayed correctly on the snapshot.  We can check that the
        // graveyard trims the file as expected.
        #[derive(Default)]
        struct Context {
            store: Option<Arc<ObjectStore>>,
            object_id: Option<u64>,
        }
        let shared_context = Arc::new(Mutex::new(Context::default()));

        // cladefs: more extents than a transaction has room to free, each more than 16 bytes of it.
        let object_size = (TRANSACTION_ROOM / 16) * 2 * block_size;

        // Wait for an object to get tombstoned by the graveyard.
        async fn expect_tombstoned(store: &Arc<ObjectStore>, object_id: u64) {
            loop {
                // cladefs: its record, not `open_object`: that looks up its size, whose record a
                // tombstone in a later layer doesn't hide, once a flush has moved it to an earlier.
                if store.tree().find(&ObjectKey::object(object_id)).await.expect("find").is_none() {
                    break;
                }
                // The graveyard should eventually tombstone the object.
                fasync::Timer::new(std::time::Duration::from_millis(100)).await;
            }
        }

        // Checks to see if the object needs to be trimmed.
        async fn needs_trim(store: &Arc<ObjectStore>) -> Option<DataObjectHandle<ObjectStore>> {
            let root_directory = Directory::open(store, store.root_directory_object_id())
                .await
                .expect("open failed");
            let oid = root_directory.lookup("foo").await.expect("lookup failed");
            if let Some((oid, _)) = oid {
                let object = ObjectStore::open_object(store, oid, HandleOptions::default())
                    .await
                    .expect("open_object failed");
                let props = object.get_properties().await.expect("get_properties failed");
                if props.allocated_size > 0 && props.data_attribute_size == 0 {
                    Some(object)
                } else {
                    None
                }
            } else {
                None
            }
        }

        let shared_context_clone = shared_context.clone();
        let post_commit = move || {
            let store = shared_context_clone.lock().store.as_ref().cloned().unwrap();
            let shared_context = shared_context_clone.clone();
            async move {
                // First run fsck on the current filesystem. cladefs: without its lock, which the
                // commit this follows holds. The filesystems mounted below are checked with
                // theirs, as their own work goes on meanwhile (compactions, the graveyard's reap
                // and trims): a commit of it that overtook a check without the lock had the check
                // find a journal block missing from the allocator and another extra, or an object
                // orphaned (TASK-176).
                let options = |no_lock| FsckOptions {
                    fail_on_warning: true,
                    no_lock,
                    on_error: Box::new(|err| println!("fsck error: {:?}", err)),
                    ..Default::default()
                };
                let (unlocked, locked) = (options(true), options(false));
                let fs = store.filesystem();

                fsck_with_options(fs.clone(), &unlocked).await.expect("fsck_with_options failed");
                fsck_volume_with_options(fs.as_ref(), &unlocked, store.store_object_id())
                    .await
                    .expect("fsck_volume_with_options failed");

                // Now check that we can replay this correctly.
                fs.sync(SyncOptions { flush_device: true, ..Default::default() })
                    .await
                    .expect("sync failed");
                let device = fs.device().snapshot().expect("snapshot failed");

                let object_id = shared_context.lock().object_id.clone();

                let fs2 = FxFilesystemBuilder::new()
                    .skip_initial_reap(object_id.is_none())
                    .open(device)
                    .await
                    .expect("open failed");

                // If the "foo" file exists check that allocated size matches content size.
                let root_vol = root_volume(fs2.clone()).await.expect("root_volume failed");
                let store = root_vol.volume("test").await.expect("volume failed");

                if let Some(oid) = object_id {
                    // For the second pass, the object should get tombstoned.
                    expect_tombstoned(&store, oid).await;
                } else if let Some(object) = needs_trim(&store).await {
                    // Extend the file and make sure that it is correctly trimmed.
                    object.truncate(object_size).await.expect("truncate failed");
                    let mut buf = object.allocate_buffer(block_size.get() as usize).await;
                    object
                        .read_aligned(object_size - block_size * 2, buf.as_mut())
                        .await
                        .expect("read failed");
                    assert_eq!(buf.to_vec(), vec![0; block_size.get() as usize]);

                    // Remount, this time with the graveyard performing an initial reap and the
                    // object should get trimmed.
                    let fs = FxFilesystem::open(fs.device().snapshot().expect("snapshot failed"))
                        .await
                        .expect("open failed");
                    let root_vol = root_volume(fs.clone()).await.expect("root_volume failed");
                    let store = root_vol.volume("test").await.expect("volume failed");
                    while needs_trim(&store).await.is_some() {
                        // The object has been truncated, but still has some data allocated to
                        // it.  The graveyard should trim the object eventually.
                        fasync::Timer::new(std::time::Duration::from_millis(100)).await;
                    }

                    // Run fsck.
                    fsck_with_options(fs.clone(), &locked).await.expect("fsck_with_options failed");
                    fsck_volume_with_options(fs.as_ref(), &locked, store.store_object_id())
                        .await
                        .expect("fsck_volume_with_options failed");
                    fs.close().await.expect("close failed");
                }

                // Run fsck on fs2.
                fsck_with_options(fs2.clone(), &locked).await.expect("fsck_with_options failed");
                fsck_volume_with_options(fs2.as_ref(), &locked, store.store_object_id())
                    .await
                    .expect("fsck_volume_with_options failed");
                fs2.close().await.expect("close failed");
            }
            .boxed()
        };

        let fs = FxFilesystemBuilder::new()
            .post_commit_hook(post_commit)
            .open(device)
            .await
            .expect("open failed");

        let root_vol = root_volume(fs.clone()).await.expect("root_volume failed");
        let store = root_vol.volume("test").await.expect("volume failed");

        shared_context.lock().store = Some(store.clone());

        let root_directory =
            Directory::open(&store, store.root_directory_object_id()).await.expect("open failed");

        let object;
        let mut transaction = fs
            .root_store()
            .new_transaction(
                lock_keys![LockKey::object(
                    store.store_object_id(),
                    store.root_directory_object_id()
                )],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        object = root_directory
            .create_child_file(&mut transaction, "foo")
            .await
            .expect("create_object failed");
        transaction.commit().await.expect("commit failed");

        let mut transaction = fs
            .root_store()
            .new_transaction(
                lock_keys![LockKey::object(store.store_object_id(), object.object_id())],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");

        // Two passes: first with a regular object, and then with that object moved into the
        // graveyard.
        let mut pass = 0;
        loop {
            // Create enough extents in it such that when we truncate the object it will require
            // more than one transaction.
            let mut buf = object.allocate_buffer(5).await;
            buf.fill(1);
            // Write every other block.
            for offset in (0..object_size).into_iter().step_by((2 * block_size) as usize) {
                object
                    .txn_write(&mut transaction, offset, buf.as_ref())
                    .await
                    .expect("write failed");
                if transaction.is_full() {
                    transaction.commit_and_continue().await.expect("commit failed");
                }
            }
            transaction.commit().await.expect("commit failed");
            // This should take up more than one transaction.
            WriteObjectHandle::truncate(&object, 0).await.expect("truncate failed");

            if pass == 1 {
                break;
            }

            // Store the object ID so that we can make sure the object is always tombstoned
            // after remount (see above).
            shared_context.lock().object_id = Some(object.object_id());

            transaction = fs
                .root_store()
                .new_transaction(
                    lock_keys![
                        LockKey::object(store.store_object_id(), store.root_directory_object_id()),
                        LockKey::object(store.store_object_id(), object.object_id()),
                    ],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");

            // Move the object into the graveyard.
            replace_child(&mut transaction, None, (&root_directory, "foo"))
                .await
                .expect("replace_child failed");
            store.add_to_graveyard(&mut transaction, object.object_id());

            pass += 1;
        }

        fs.close().await.expect("Close failed");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_adjust_refs() {
        let (fs, object) = test_filesystem_and_object().await;
        let store = object.owner();
        let mut transaction = fs
            .root_store()
            .new_transaction(
                lock_keys![LockKey::object(store.store_object_id(), object.object_id())],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        assert_eq!(
            store
                .adjust_refs(&mut transaction, object.object_id(), 1)
                .await
                .expect("adjust_refs failed"),
            false
        );
        transaction.commit().await.expect("commit failed");

        let allocator = fs.allocator();
        let allocated_before = allocator.get_allocated_bytes();
        let mut transaction = fs
            .root_store()
            .new_transaction(
                lock_keys![LockKey::object(store.store_object_id(), object.object_id())],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        assert_eq!(
            store
                .adjust_refs(&mut transaction, object.object_id(), -2)
                .await
                .expect("adjust_refs failed"),
            true
        );
        transaction.commit().await.expect("commit failed");

        assert_eq!(allocator.get_allocated_bytes(), allocated_before);

        store
            .tombstone_object(
                object.object_id(),
                Options { borrow_metadata_space: true, ..Default::default() },
                None,
            )
            .await
            .expect("purge failed");

        assert_eq!(allocated_before - allocator.get_allocated_bytes(), fs.block_size());

        // We need to remove the directory entry, too, otherwise fsck will complain
        {
            let mut transaction = fs
                .root_store()
                .new_transaction(
                    lock_keys![LockKey::object(
                        store.store_object_id(),
                        store.root_directory_object_id()
                    )],
                    Options::default(),
                )
                .await
                .expect("new_transaction failed");
            let root_directory = Directory::open(&store, store.root_directory_object_id())
                .await
                .expect("open failed");
            transaction.add(
                store.store_object_id(),
                Mutation::replace_or_insert_object(
                    ObjectKey::child(root_directory.object_id(), TEST_OBJECT_NAME),
                    ObjectValue::None,
                ),
            );
            transaction.commit().await.expect("commit failed");
        }

        fsck_with_options(
            fs.clone(),
            &FsckOptions {
                fail_on_warning: true,
                on_error: Box::new(|err| println!("fsck error: {:?}", err)),
                ..Default::default()
            },
        )
        .await
        .expect("fsck_with_options failed");

        fs.close().await.expect("Close failed");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_locks() {
        let (fs, object) = test_filesystem_and_object().await;
        let (send1, recv1) = channel();
        let (send2, recv2) = channel();
        let (send3, recv3) = channel();
        let done = Mutex::new(false);
        let mut futures = FuturesUnordered::new();
        futures.push(
            async {
                let mut t = object.new_transaction().await.expect("new_transaction failed");
                send1.send(()).unwrap(); // Tell the next future to continue.
                send3.send(()).unwrap(); // Tell the last future to continue.
                recv2.await.unwrap();
                let mut buf = object.allocate_buffer(5).await;
                buf.copy_from_slice(b"hello");
                object.txn_write(&mut t, 0, buf.as_ref()).await.expect("write failed");
                // This is a halting problem so all we can do is sleep.
                fasync::Timer::new(Duration::from_millis(100)).await;
                assert!(!*done.lock());
                t.commit().await.expect("commit failed");
            }
            .boxed(),
        );
        futures.push(
            async {
                recv1.await.unwrap();
                // Reads should not block.
                let data = object
                    .read_bytes(TEST_DATA_OFFSET..(TEST_DATA_OFFSET + TEST_DATA.len() as u64))
                    .await
                    .expect("read failed");
                assert_eq!(&*data, TEST_DATA);
                // Tell the first future to continue.
                send2.send(()).unwrap();
            }
            .boxed(),
        );
        futures.push(
            async {
                // This should block until the first future has completed.
                recv3.await.unwrap();
                let _t = object.new_transaction().await.expect("new_transaction failed");
                let data = object.read_bytes(0..5).await.expect("read failed");
                assert_eq!(&*data, b"hello");
            }
            .boxed(),
        );
        while let Some(()) = futures.next().await {}
        fs.close().await.expect("Close failed");
    }

    #[fuchsia_async::run(10, test)]
    async fn test_racy_reads() {
        let fs = test_filesystem().await;
        let object;
        let mut transaction = fs
            .root_store()
            .new_transaction(lock_keys![], Options::default())
            .await
            .expect("new_transaction failed");
        let store = fs.root_store();
        object = Arc::new(
            ObjectStore::create_object(&store, &mut transaction, HandleOptions::default())
                .await
                .expect("create_object failed"),
        );
        transaction.commit().await.expect("commit failed");
        for _ in 0..100 {
            let cloned_object = object.clone();
            let writer = fasync::Task::spawn(async move {
                let mut buf = cloned_object.allocate_buffer(10).await;
                buf.fill(123);
                cloned_object.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");
            });
            let cloned_object = object.clone();
            let reader = fasync::Task::spawn(async move {
                let wait_time = rand::random_range(0..5);
                fasync::Timer::new(Duration::from_millis(wait_time)).await;
                let mut buf =
                    cloned_object.allocate_buffer(cloned_object.block_size().get() as usize).await;
                buf.fill(23);
                let amount =
                    cloned_object.read_aligned(0, buf.as_mut()).await.expect("read failed");
                // If we succeed in reading data, it must include the write; i.e. if we see the size
                // change, we should see the data too.  For this to succeed it requires locking on
                // the read size to ensure that when we read the size, we get the extents changed in
                // that same transaction.
                if amount != 0 {
                    assert_eq!(amount, 10);
                    assert_eq!(&buf[0..10], &[123; 10]);
                }
            });
            writer.await;
            reader.await;
            object.truncate(0).await.expect("truncate failed");
        }
        fs.close().await.expect("Close failed");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_allocated_size() {
        let (fs, object) = test_filesystem_and_object_with(true).await;

        let before = object.get_properties().await.expect("get_properties failed").allocated_size;
        let mut buf = object.allocate_buffer(5).await;
        buf.copy_from_slice(b"hello");
        object.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");
        let after = object.get_properties().await.expect("get_properties failed").allocated_size;
        assert_eq!(after, before + fs.block_size());

        // Do the same write again and there should be no change.
        object.write_or_append(Some(0), buf.as_ref()).await.expect("write failed");
        assert_eq!(
            object.get_properties().await.expect("get_properties failed").allocated_size,
            after
        );

        // write a new block past the initial test extent at TEST_DATA_OFFSET (block 1)...
        let before = after;
        let mut block = object.allocate_buffer(fs.block_size().get() as usize).await;
        block.fill(0xab);
        object
            .write_or_append(Some(fs.block_size().get() * 2), block.as_ref())
            .await
            .expect("write failed");
        let after = object.get_properties().await.expect("get_properties failed").allocated_size;
        assert_eq!(after, before + fs.block_size());

        // truncate...
        let before = after;
        let size = object.get_size();
        object.truncate(size - fs.block_size()).await.expect("extend failed");
        let after = object.get_properties().await.expect("get_properties failed").allocated_size;
        assert_eq!(after, before - fs.block_size());

        fs.close().await.expect("Close failed");
    }

    #[fuchsia_async::run(10, test)]
    async fn test_zero() {
        let (fs, object) = test_filesystem_and_object().await;
        let expected_size = object.get_size();
        let mut transaction = object.new_transaction().await.expect("new_transaction failed");
        object.zero(&mut transaction, 0..fs.block_size() * 10).await.expect("zero failed");
        transaction.commit().await.expect("commit failed");
        assert_eq!(object.get_size(), expected_size);
        let mut buf = object.allocate_buffer((fs.block_size() * 10) as usize).await;
        assert_eq!(
            object.read_aligned(0, buf.as_mut()).await.expect("read failed") as u64,
            expected_size
        );
        assert_eq!(&buf[0..expected_size as usize], vec![0u8; expected_size as usize].as_slice());
        fs.close().await.expect("Close failed");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_properties() {
        let (fs, object) = test_filesystem_and_object().await;
        const CRTIME: Timestamp = Timestamp::from_nanos(1234);
        const MTIME: Timestamp = Timestamp::from_nanos(5678);
        const CTIME: Timestamp = Timestamp::from_nanos(8765);

        // ObjectProperties can be updated through `update_attributes`.
        // `get_properties` should reflect the latest changes.
        let mut transaction = object.new_transaction().await.expect("new_transaction failed");
        object
            .update_attributes(
                &mut transaction,
                Some(&MutableNodeAttributes {
                    creation_time: Some(CRTIME.as_nanos()),
                    modification_time: Some(MTIME.as_nanos()),
                    mode: Some(111),
                    gid: Some(222),
                    ..Default::default()
                }),
                None,
            )
            .await
            .expect("update_attributes failed");
        const MTIME_NEW: Timestamp = Timestamp::from_nanos(12345678);
        object
            .update_attributes(
                &mut transaction,
                Some(&MutableNodeAttributes {
                    modification_time: Some(MTIME_NEW.as_nanos()),
                    gid: Some(333),
                    rdev: Some(444),
                    ..Default::default()
                }),
                Some(CTIME),
            )
            .await
            .expect("update_timestamps failed");
        transaction.commit().await.expect("commit failed");

        let properties = object.get_properties().await.expect("get_properties failed");
        assert_matches!(
            properties,
            ObjectProperties {
                refs: 1u64,
                allocated_size: TEST_OBJECT_ALLOCATED_SIZE,
                data_attribute_size: TEST_OBJECT_SIZE,
                creation_time: CRTIME,
                modification_time: MTIME_NEW,
                posix_attributes: Some(PosixAttributes { mode: 111, gid: 333, rdev: 444, .. }),
                change_time: CTIME,
                ..
            }
        );
        fs.close().await.expect("Close failed");
    }

    #[fuchsia_async::run(10, test)]
    async fn test_read_write_attr() {
        let (_fs, object) = test_filesystem_and_object().await;
        let data = [0xffu8; 16_384];
        object.write_attr(AttributeId(20), &data).await.expect("write_attr failed");
        let rdata = object
            .read_attr(AttributeId(20))
            .await
            .expect("read_attr failed")
            .expect("no attribute data found");
        assert_eq!(&data[..], &rdata[..]);

        assert_eq!(object.read_attr(AttributeId(21)).await.expect("read_attr failed"), None);
    }

    #[fuchsia_async::run(10, test)]
    async fn test_read_write_attr_unaligned() {
        let (_fs, object) = test_filesystem_and_object().await;
        let data_unaligned = [0x55u8; 5000];
        object.write_attr(AttributeId(22), &data_unaligned).await.expect("write_attr failed");
        let rdata = object
            .read_attr(AttributeId(22))
            .await
            .expect("read_attr failed")
            .expect("no attribute data found");
        assert_eq!(&data_unaligned[..], &rdata[..]);
    }

    #[fuchsia_async::run(10, test)]
    async fn test_read_write_attr_empty() {
        let (_fs, object) = test_filesystem_and_object().await;
        object.write_attr(AttributeId(23), &[]).await.expect("write_attr failed");
        let rdata = object
            .read_attr(AttributeId(23))
            .await
            .expect("read_attr failed")
            .expect("no attribute data found");
        assert!(rdata.is_empty());
    }
}
