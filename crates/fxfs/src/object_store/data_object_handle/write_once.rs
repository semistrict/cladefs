// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own, a child of data_object_handle.rs: data lives in extents of an extent store, written
//! once (backlog doc-4). `overwrite` writes blocks in place only as `in_place` says, which refuses
//! those a snapshot or a clone shares.

use super::DataObjectHandle;
use crate::object_store::HandleOwner;
use crate::object_store::extent_record::ExtentMode;
use anyhow::Error;
use storage_device::ExtentRange;

/// cladefs: whether a file's blocks may be written in place, and if not, why: the rule
/// ([`DataObjectHandle::in_place`]) for `overwrite`, which writes them through the device
/// (TASK-160.22).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InPlace {
    /// They may: their record is raw, and theirs alone.
    Writable,
    /// Their record keeps something of what they hold, which a write in place would leave stale:
    /// checksums (`ExtentMode::Cow`), which a replay of the journal checks a transaction's blocks
    /// against (barriers off), stopping where they don't match; or that they are an allocation's
    /// (`ExtentMode::Overwrite`), and which of them were written (`OverwritePartial`), the others
    /// reading as zeros. Fxfs's own writes of them go to new blocks (`multi_overwrite`).
    Recorded,
    /// Another record shares them: a clone's or a snapshot's (`Allocator::is_shared`).
    Shared,
}

impl<S: HandleOwner> DataObjectHandle<S> {
    /// cladefs: whether `blocks` of the file, which a record of `mode` holds, may be written in place
    /// (`InPlace`), as `overwrite` writes them: whether their extent is open isn't asked, as the
    /// device takes or refuses its writes itself.
    pub(super) async fn in_place(
        &self,
        mode: &ExtentMode,
        blocks: &ExtentRange,
    ) -> Result<InPlace, Error> {
        Ok(if !matches!(mode, ExtentMode::Raw) {
            InPlace::Recorded
        } else if self.store().allocator().is_shared(blocks.clone()).await? {
            InPlace::Shared
        } else {
            InPlace::Writable
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::filesystem::FxFilesystem;
    use crate::fsck::fsck;
    use crate::object_handle::{ObjectHandle, WriteObjectHandle};
    use crate::object_store::AttributeId;
    use crate::object_store::data_object_handle::tests::{
        TEST_DEVICE_BLOCK_SIZE, create_test_object,
    };
    use storage_device::DeviceHolder;
    use storage_device::fake_device::FakeDevice;

    /// cladefs: random single-block overwrites of a large allocated file, each in a transaction of
    /// its own (as fxfs-platform flushes overwrite ranges), read back, with the journal compacted.
    /// Each overwrite writes new blocks, and the old ones' extents live on while any of their
    /// blocks does.
    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_random_overwrites_of_an_allocated_file() {
        const FILE_SIZE: u64 = 128 << 20;
        const WRITES: usize = 8192;
        let device = DeviceHolder::new(FakeDevice::new(1 << 20, TEST_DEVICE_BLOCK_SIZE));
        let fs = FxFilesystem::new_empty(device).await.expect("new_empty failed");
        let object = create_test_object(fs.clone(), false).await;
        object.truncate(FILE_SIZE).await.expect("truncate failed");
        object.allocate(0..FILE_SIZE).await.expect("allocate failed");
        let bs = fs.block_size().get();
        let mut buf = object.allocate_buffer(bs as usize).await;
        buf.as_mut().fill(7);
        // A fixed pseudo-random sequence of blocks.
        let mut seed: u64 = 0x2545f4914f6cdd1d;
        let mut written = std::collections::HashSet::new();
        for _ in 0..WRITES {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let offset = (seed % (FILE_SIZE / bs)) * bs;
            let mut transaction = object.new_transaction().await.expect("new_transaction failed");
            object
                .multi_overwrite(
                    &mut transaction,
                    AttributeId::DATA,
                    &[offset..offset + bs],
                    buf.as_mut(),
                )
                .await
                .expect("multi_overwrite failed");
            transaction.commit().await.expect("commit failed");
            written.insert(offset);
        }
        fs.journal().force_compact().await.expect("compact failed");
        let mut read = object.allocate_buffer(bs as usize).await;
        for offset in (0..FILE_SIZE).step_by(bs as usize) {
            object.read_aligned(offset, read.as_mut()).await.expect("read failed");
            let byte = if written.contains(&offset) { 7 } else { 0 };
            assert_eq!(read.to_vec(), vec![byte; bs as usize], "block at {offset}");
        }
        fsck(fs.clone()).await.expect("fsck failed");
        fs.close().await.expect("close failed");
    }
}
