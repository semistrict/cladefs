// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Fuchsia's `src/storage/fxfs/src/object_store/journal/bootstrap_handle.rs`. cladefs: replaces
// `BootstrapObjectHandle` with two direct extent-store handles (boards 6923, 6926):
// - `ExtentObjectHandle`: reads the single atomic super-block extent named by `Root::super_block`.
// - `JournalExtentHandle`: maps block-aligned journal stream offsets directly to position-derived
//   extent IDs (`JournalSpan`) for both reading (parallel per-block reads, unwritten blocks reading
//   as zeros) and writing (direct `PRE_BARRIER`/`POST_BARRIER` + `root` writes without any
//   `DataObjectHandle`, `root_parent` extent records, or allocator mutations).

use crate::errors::FxfsError;
use crate::object_handle::{INVALID_OBJECT_ID, ObjectHandle, ReadObjectHandle};
use crate::object_store::journal::super_block::JournalSpan;
use crate::object_store::journal::{BLOCK_SIZE, JournalHandle};
use anyhow::{Error, anyhow, bail, ensure};
use async_trait::async_trait;
use fuchsia_sync::Mutex;
use futures::TryStreamExt as _;
use futures::future::try_join_all;
use futures::stream::FuturesOrdered;
use std::cmp::min;
use std::ops::Range;
use std::sync::Arc;
use storage_device::buffer::{BufferFuture, MutableBufferRef};
use storage_device::{Device, ExtentId, ExtentRange, NoExtent, WriteFlags, WriteOptions};
use storage_units::BlockSize;

/// Reads a single super-block stored in one `ExtentRange` named by `Root::super_block`.
pub struct ExtentObjectHandle {
    device: Arc<dyn Device>,
    extent: ExtentRange,
}

impl ExtentObjectHandle {
    pub fn new(device: Arc<dyn Device>, extent: ExtentRange) -> Self {
        Self { device, extent }
    }
}

impl ObjectHandle for ExtentObjectHandle {
    fn object_id(&self) -> u64 {
        INVALID_OBJECT_ID
    }

    fn allocate_buffer(&self, size: usize) -> BufferFuture<'_> {
        self.device.allocate_buffer(size)
    }

    fn block_size(&self) -> BlockSize {
        BLOCK_SIZE
    }
}

#[async_trait]
impl ReadObjectHandle for ExtentObjectHandle {
    async fn read_aligned(&self, offset: u64, buf: MutableBufferRef<'_>) -> Result<usize, Error> {
        let block_size = self.block_size();
        ensure!(block_size.is_aligned(offset), FxfsError::InvalidArgs);
        ensure!(block_size.is_aligned(buf.len() as u64), FxfsError::InvalidArgs);
        let size = self.extent.len();
        if offset >= size || buf.is_empty() {
            return Ok(0);
        }
        let to_read = min(size - offset, buf.len() as u64) as usize;
        let device_offset =
            self.extent.range.start.checked_add(offset).ok_or(FxfsError::Inconsistent)?;
        self.device.read(self.extent.extent, device_offset, buf.subslice_mut(..to_read)).await?;
        Ok(to_read)
    }

    fn get_size(&self) -> u64 {
        self.extent.len()
    }
}

impl JournalHandle for ExtentObjectHandle {
    fn try_push_stream_span(
        &mut self,
        _span: JournalSpan,
        _prev_cell: u64,
        _prev_end_sequence: u64,
    ) -> Result<Option<u64>, Error> {
        bail!(anyhow!(FxfsError::Inconsistent).context("Unexpected span on super-block handle"));
    }
}

/// Maps contiguous journal byte offsets directly to position-derived extent IDs via a list of
/// [`JournalSpan`]s. Shared via `Clone` (`Arc` interior) between `Journal` and readers (e.g.
/// `read_transactions_for_object`), or owned per-reader during replay.
#[derive(Clone)]
pub struct JournalExtentHandle {
    device: Arc<dyn Device>,
    spans: Arc<Mutex<Vec<JournalSpan>>>,
}

impl JournalExtentHandle {
    pub fn new(device: Arc<dyn Device>, spans: Vec<JournalSpan>) -> Self {
        Self { device, spans: Arc::new(Mutex::new(spans)) }
    }

    pub fn device(&self) -> &Arc<dyn Device> {
        &self.device
    }

    /// Snapshot of the current span list.
    pub fn spans(&self) -> Vec<JournalSpan> {
        self.spans.lock().clone()
    }

    pub fn span_count(&self) -> usize {
        self.spans.lock().len()
    }

    pub fn end_offset(&self) -> u64 {
        self.spans.lock().last().map_or(0, JournalSpan::end_offset)
    }

    pub fn last_span(&self) -> Option<JournalSpan> {
        self.spans.lock().last().cloned()
    }

    /// `(cell, end_sequence)` of the current last span, embedded in `JournalRecord::JournalSpan`
    /// so replay accepts only spans extending the exact active chain.
    pub fn last_end_key(&self) -> Option<(u64, u64)> {
        self.spans.lock().last().map(|s| (s.cell, s.end_sequence()))
    }

    /// Appends a newly minted span directly (live writer path).
    pub fn push_span(&self, span: JournalSpan) {
        let mut spans = self.spans.lock();
        if let Some(last) = spans.last() {
            debug_assert_eq!(last.end_offset(), span.start_offset, "non-contiguous journal span");
        }
        spans.push(span);
    }

    /// Replaces the entire span list (after mount rotation, thaw rotation, or compaction pruning).
    pub fn replace_spans(&self, new_spans: Vec<JournalSpan>) {
        *self.spans.lock() = new_spans;
    }

    /// Prunes spans ending at or before `freed_to`, trimming the leading span's front if
    /// `freed_to` falls inside it.
    pub fn prune_before(&self, freed_to: u64) {
        let mut spans = self.spans.lock();
        prune_spans_before(&mut spans, freed_to);
    }

    /// Collects all position-derived journal block `ExtentId`s for block offsets in `range`.
    pub fn extents_in(&self, range: Range<u64>) -> Vec<ExtentId> {
        let spans = self.spans.lock();
        extents_in_spans(&spans, range)
    }

    /// Writes `buf` (block-aligned) starting at journal `start` offset directly to the
    /// position-derived block extents, optionally with `PRE_BARRIER` on block 0 and `POST_BARRIER`
    /// + `root` on the final block (matching `DataObjectHandle::overwrite` ordering).
    pub async fn write_blocks(
        &self,
        start: u64,
        mut buf: MutableBufferRef<'_>,
        barrier_on_first_write: bool,
        barrier_after_last_write: bool,
        root: Option<&[u8]>,
    ) -> Result<(), Error> {
        let bs = BLOCK_SIZE.get();
        ensure!(BLOCK_SIZE.is_aligned(start), FxfsError::InvalidArgs);
        ensure!(BLOCK_SIZE.is_aligned(buf.len() as u64), FxfsError::InvalidArgs);
        if buf.is_empty() {
            return Ok(());
        }
        let block_count = (buf.len() as u64) / bs;
        let extents: Vec<ExtentId> = {
            let spans = self.spans.lock();
            let mut out = Vec::with_capacity(block_count as usize);
            for i in 0..block_count {
                let offset = start + i * bs;
                let extent = extent_at_in(&spans, offset).ok_or_else(|| {
                    anyhow!(FxfsError::Inconsistent)
                        .context(format!("No journal span covers offset {offset}"))
                })?;
                out.push(extent);
            }
            out
        };

        let mut writes = FuturesOrdered::new();
        let mut last_write = None;
        let mut first_write = barrier_on_first_write;
        for extent in extents {
            let (current_buf, remaining_buf) = buf.split_at_mut(bs as usize);
            let mut flags = if first_write {
                first_write = false;
                WriteFlags::PRE_BARRIER
            } else {
                WriteFlags::empty()
            };
            let is_last = remaining_buf.is_empty() && barrier_after_last_write;
            if is_last {
                flags |= WriteFlags::POST_BARRIER;
            }
            let write_root = if is_last { root } else { None };
            let options = WriteOptions { flags, root: write_root, ..Default::default() };
            let device = &self.device;
            let write = crate::boxed(move || async move {
                device.write_with_opts(extent, 0, current_buf.as_ref(), options).await
            });
            if is_last {
                last_write = Some(write);
            } else {
                writes.push_back(write);
            }
            buf = remaining_buf;
        }
        writes.try_collect::<Vec<()>>().await?;
        if let Some(last_write) = last_write {
            last_write.await?;
        }
        Ok(())
    }
}

/// Resolves a block-aligned `offset` within `spans`.
pub(super) fn extent_at_in(spans: &[JournalSpan], offset: u64) -> Option<ExtentId> {
    let idx = spans.partition_point(|s| s.end_offset() <= offset);
    let span = spans.get(idx)?;
    span.extent_at(offset)
}

/// Collects every block `ExtentId` in `spans` whose byte offset falls in `range` (block-aligned).
pub(super) fn extents_in_spans(spans: &[JournalSpan], range: Range<u64>) -> Vec<ExtentId> {
    if range.start >= range.end {
        return Vec::new();
    }
    let bs = BLOCK_SIZE.get();
    let mut out = Vec::new();
    for span in spans {
        let s_end = span.end_offset();
        if s_end <= range.start {
            continue;
        }
        if span.start_offset >= range.end {
            break;
        }
        let from = range.start.max(span.start_offset);
        let to = range.end.min(s_end);
        let start_block = (from - span.start_offset) / bs;
        let end_block = (to - span.start_offset) / bs;
        for b in start_block..end_block {
            out.push(ExtentId::new(span.cell, span.base_sequence + b));
        }
    }
    out
}

/// Drops spans ending `<= freed_to` and trims the front of the first remaining span if `freed_to`
/// falls strictly inside it. Preserves each span's `end_sequence()`.
pub(super) fn prune_spans_before(spans: &mut Vec<JournalSpan>, freed_to: u64) {
    let bs = BLOCK_SIZE.get();
    spans.retain(|s| s.end_offset() > freed_to);
    if let Some(first) = spans.first_mut() {
        if freed_to > first.start_offset {
            let trimmed_blocks = (freed_to - first.start_offset) / bs;
            first.start_offset = freed_to;
            first.base_sequence += trimmed_blocks;
            first.block_count -= trimmed_blocks;
        }
    }
}

/// Truncates `spans` so nothing extends past `end_offset` (block-aligned), dropping empty spans.
pub(super) fn truncate_spans_at(spans: &mut Vec<JournalSpan>, end_offset: u64) {
    let bs = BLOCK_SIZE.get();
    spans.retain(|s| s.start_offset < end_offset);
    if let Some(last) = spans.last_mut() {
        if last.end_offset() > end_offset {
            last.block_count = (end_offset - last.start_offset) / bs;
        }
    }
}

impl ObjectHandle for JournalExtentHandle {
    fn object_id(&self) -> u64 {
        INVALID_OBJECT_ID
    }

    fn allocate_buffer(&self, size: usize) -> BufferFuture<'_> {
        self.device.allocate_buffer(size)
    }

    fn block_size(&self) -> BlockSize {
        BLOCK_SIZE
    }
}

#[async_trait]
impl ReadObjectHandle for JournalExtentHandle {
    async fn read_aligned(&self, offset: u64, buf: MutableBufferRef<'_>) -> Result<usize, Error> {
        let block_size = self.block_size();
        let bs = block_size.get();
        ensure!(block_size.is_aligned(offset), FxfsError::InvalidArgs);
        ensure!(block_size.is_aligned(buf.len() as u64), FxfsError::InvalidArgs);
        let max_blocks = (buf.len() as u64) / bs;
        let extents: Vec<ExtentId> = {
            let spans = self.spans.lock();
            if let Some(first) = spans.first() {
                ensure!(offset >= first.start_offset, FxfsError::Inconsistent);
            } else {
                return Ok(0);
            }
            let mut out = Vec::with_capacity(max_blocks as usize);
            for i in 0..max_blocks {
                let Some(block_off) = offset.checked_add(i * bs) else {
                    break;
                };
                match extent_at_in(&spans, block_off) {
                    Some(extent) => out.push(extent),
                    None => break,
                }
            }
            out
        };
        let to_read = extents.len() * (bs as usize);
        if to_read == 0 {
            return Ok(0);
        }
        let mut rest = buf;
        let mut reads = Vec::with_capacity(extents.len());
        for extent in extents {
            let (mut part, tail) = rest.split_at_mut(bs as usize);
            rest = tail;
            reads.push(async move {
                match self.device.read(extent, 0, part.reborrow()).await {
                    Err(error) if error.downcast_ref::<NoExtent>().is_some() => part.fill(0),
                    result => result?,
                }
                Ok::<_, Error>(())
            });
        }
        try_join_all(reads).await?;
        Ok(to_read)
    }

    fn get_size(&self) -> u64 {
        self.end_offset()
    }
}

impl JournalHandle for JournalExtentHandle {
    fn try_push_stream_span(
        &mut self,
        span: JournalSpan,
        prev_cell: u64,
        prev_end_sequence: u64,
    ) -> Result<Option<u64>, Error> {
        span.validate()?;
        let mut spans = self.spans.lock();
        if let Some(last) = spans.last() {
            if last.cell == prev_cell
                && last.end_sequence() == prev_end_sequence
                && last.end_offset() == span.start_offset
            {
                let start = span.start_offset;
                spans.push(span);
                return Ok(Some(start));
            }
        }
        // Otherwise this span was either already persisted in `Root` or superseded by a later
        // mount/thaw rotation whose `Root` is authoritative.
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::{ExtentObjectHandle, JournalExtentHandle, prune_spans_before, truncate_spans_at};
    use crate::errors::FxfsError;
    use crate::object_handle::ReadObjectHandle as _;
    use crate::object_store::journal::super_block::JournalSpan;
    use crate::object_store::journal::{BLOCK_SIZE, JournalHandle as _};
    use std::sync::Arc;
    use storage_device::fake_device::{DEFAULT_CELL, FakeDevice};
    use storage_device::{Device as _, ExtentId, ExtentRange};

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_extent_object_handle_read() {
        let bs = BLOCK_SIZE.get();
        let device = Arc::new(FakeDevice::new(16, bs as u32));
        let extent = ExtentId::new(DEFAULT_CELL, 7);
        let mut buf = device.allocate_buffer(2 * bs as usize).await;
        buf.subslice_mut(..bs as usize).fill(1);
        buf.subslice_mut(bs as usize..).fill(2);
        device.write(extent, 0, buf.as_ref()).await.unwrap();

        let handle = ExtentObjectHandle::new(device.clone(), ExtentRange::new(extent, 0..2 * bs));
        assert_eq!(handle.get_size(), 2 * bs);
        let mut out = device.allocate_buffer(bs as usize).await;
        assert_eq!(handle.read_aligned(0, out.as_mut()).await.unwrap(), bs as usize);
        assert_eq!(out.to_vec(), vec![1u8; bs as usize]);
        assert_eq!(handle.read_aligned(bs, out.as_mut()).await.unwrap(), bs as usize);
        assert_eq!(out.to_vec(), vec![2u8; bs as usize]);
        assert_eq!(handle.read_aligned(2 * bs, out.as_mut()).await.unwrap(), 0);
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_journal_extent_handle_spans_and_bounds() {
        let bs = BLOCK_SIZE.get();
        let device = Arc::new(FakeDevice::new(32, bs as u32));
        let s1 =
            JournalSpan { start_offset: bs, block_count: 2, cell: DEFAULT_CELL, base_sequence: 10 };
        let mut handle = JournalExtentHandle::new(device.clone(), vec![s1.clone()]);
        let mut out = device.allocate_buffer(bs as usize).await;
        let err = handle.read_aligned(0, out.as_mut()).await.expect_err("before first span");
        assert!(FxfsError::Inconsistent.matches(&err), "{err:?}");

        // Stream span with matching prev key is appended; stale prev key is ignored.
        let s2 = JournalSpan {
            start_offset: 3 * bs,
            block_count: 2,
            cell: DEFAULT_CELL,
            base_sequence: 20,
        };
        assert_eq!(handle.try_push_stream_span(s2.clone(), DEFAULT_CELL, 999).unwrap(), None);
        assert_eq!(
            handle.try_push_stream_span(s2.clone(), DEFAULT_CELL, s1.end_sequence()).unwrap(),
            Some(3 * bs)
        );
        assert_eq!(handle.end_offset(), 5 * bs);

        let mut spans = handle.spans();
        prune_spans_before(&mut spans, 2 * bs);
        assert_eq!(spans[0].start_offset, 2 * bs);
        assert_eq!(spans[0].base_sequence, 11);
        assert_eq!(spans[0].end_sequence(), 12);
        truncate_spans_at(&mut spans, 4 * bs);
        assert_eq!(spans.last().unwrap().end_offset(), 4 * bs);
    }
}
