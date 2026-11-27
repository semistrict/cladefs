// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Fuchsia's `src/storage/lib/storage_device/src/buffer.rs`, without buffers' VMOs, physical
// addresses and the pool they were carved from (`buffer_allocator.rs`), which only Fuchsia needs.
// cladefs (board 10541): a buffer is memory of the heap's, aligned to the device's blocks, and its
// views are plain slices: every buffer cladefs makes is its own, Fxfs's and its device's alone (on
// the host, `OpenCellDevice` and `DaemonDevice` copy them to and from the cell; in the guest
// kernel, the virtio driver maps `kmalloc` and `vmalloc` memory alike), so Fuchsia's untrusted
// pointer slices (storage_ptr_slice) and its fixed transfer pool had nothing left to guard. What
// the pool still gave, a bound on the bytes in flight, a device keeps with a [`TransferBudget`].

use event_listener::{Event, EventListener};
use fuchsia_sync::Mutex;
use futures::FutureExt as _;
use std::alloc::Layout;
use std::future::Future;
use std::ops::{Deref, DerefMut, RangeBounds};
use std::pin::Pin;
use std::ptr::NonNull;
use std::slice::SliceIndex;
use std::task::{Context, Poll};

/// The most a buffer is aligned to: a page, as the guest kernel's allocator aligns no further
/// (`cladefs_rt_alloc`), and as the host's `O_DIRECT` and the guest's DMA need no more.
const MAX_ALIGN: usize = 4096;

/// A range of a slice's, as `[T]::get` takes them.
pub trait SliceRange: Clone + RangeBounds<usize> + SliceIndex<[u8], Output = [u8]> {}
impl<T> SliceRange for T where T: Clone + RangeBounds<usize> + SliceIndex<[u8], Output = [u8]> {}

/// A device's bound on the bytes of its buffers alive at once: an allocation waits while the
/// others hold too many, as one of Fuchsia's transfer pool waited for room, so that I/O in flight
/// can't take memory without bound (the guest kernel's 16 MiB). One larger than the whole budget is
/// let through once no other buffer is alive, rather than never.
#[derive(Debug)]
pub struct TransferBudget {
    block_size: usize,
    limit: usize,
    used: Mutex<usize>,
    freed: Event,
}

impl TransferBudget {
    /// A budget of `limit` bytes of buffers aligned to `block_size`, a power of two.
    pub fn new(block_size: usize, limit: usize) -> Self {
        assert!(block_size.is_power_of_two(), "a block size of {block_size}");
        Self { block_size, limit, used: Mutex::new(0), freed: Event::new() }
    }

    /// The device's block size, which buffers' sizes round up to.
    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// The bytes of the buffers alive.
    pub fn used(&self) -> usize {
        *self.used.lock()
    }

    /// A buffer of `size` bytes, read as zeros, once the budget has room for it.
    pub fn allocate_buffer(&self, size: usize) -> BufferFuture<'_> {
        BufferFuture { budget: self, size, listener: None }
    }

    /// A buffer of `size` bytes, or what to wait on before asking again.
    fn try_allocate(&self, size: usize) -> Result<Buffer<'_>, EventListener> {
        let charged = size.max(1).next_multiple_of(self.block_size);
        {
            let mut used = self.used.lock();
            if *used != 0 && *used + charged > self.limit {
                return Err(self.freed.listen());
            }
            *used += charged;
        }
        let bytes = AlignedBytes::zeroed(charged, self.block_size.min(MAX_ALIGN));
        Ok(Buffer { bytes, len: size, charged, budget: self })
    }

    fn free(&self, charged: usize) {
        *self.used.lock() -= charged;
        self.freed.notify(usize::MAX);
    }
}

/// A future which resolves to a buffer once its device's [`TransferBudget`] has room for it.
#[must_use = "futures do nothing unless polled"]
pub struct BufferFuture<'a> {
    budget: &'a TransferBudget,
    size: usize,
    listener: Option<EventListener>,
}

impl<'a> Future for BufferFuture<'a> {
    type Output = Buffer<'a>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Buffer<'a>> {
        loop {
            if let Some(listener) = self.listener.as_mut() {
                futures::ready!(listener.poll_unpin(context));
                self.listener = None;
            }
            match self.budget.try_allocate(self.size) {
                Ok(buffer) => return Poll::Ready(buffer),
                Err(listener) => self.listener = Some(listener),
            }
        }
    }
}

/// Zeroed memory of the heap's, aligned.
struct AlignedBytes {
    ptr: NonNull<u8>,
    layout: Layout,
}

// SAFETY: The memory is the value's alone, as a `Box<[u8]>`'s is.
unsafe impl Send for AlignedBytes {}
// SAFETY: As above: shared, it is only read.
unsafe impl Sync for AlignedBytes {}

impl AlignedBytes {
    fn zeroed(size: usize, align: usize) -> Self {
        let layout = Layout::from_size_align(size, align).expect("a buffer's layout");
        assert!(layout.size() > 0);
        // SAFETY: The layout is of more than no bytes.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        let ptr = NonNull::new(ptr).unwrap_or_else(|| std::alloc::handle_alloc_error(layout));
        Self { ptr, layout }
    }

    fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` is valid for `layout.size()` bytes, initialized (zeroed at first), and
        // borrowed with `self`.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.layout.size()) }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: As `as_slice`, borrowed exclusively with `self`.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.layout.size()) }
    }
}

impl Drop for AlignedBytes {
    fn drop(&mut self) {
        // SAFETY: What `zeroed` allocated, with the same layout, which nothing borrows now.
        unsafe { std::alloc::dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

/// A buffer for I/O with a device: its own memory, block-aligned, which reads as zeros at first.
/// It counts against its device's [`TransferBudget`] until dropped.
pub struct Buffer<'a> {
    bytes: AlignedBytes,
    len: usize,
    charged: usize,
    budget: &'a TransferBudget,
}

impl std::fmt::Debug for Buffer<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Buffer").field("len", &self.len).finish()
    }
}

impl Drop for Buffer<'_> {
    fn drop(&mut self) {
        self.budget.free(self.charged);
    }
}

impl Deref for Buffer<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.bytes.as_slice()[..self.len]
    }
}

impl DerefMut for Buffer<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.bytes.as_mut_slice()[..self.len]
    }
}

impl<'a> Buffer<'a> {
    /// The whole buffer, read-only.
    pub fn as_ref(&self) -> BufferRef<'_> {
        BufferRef(self)
    }

    /// The whole buffer.
    pub fn as_mut(&mut self) -> MutableBufferRef<'_> {
        MutableBufferRef(self)
    }

    /// The bytes of `range`, read-only.
    pub fn subslice<R: SliceRange>(&self, range: R) -> BufferRef<'_> {
        BufferRef(&self[range])
    }

    /// The bytes of `range`.
    pub fn subslice_mut<R: SliceRange>(&mut self, range: R) -> MutableBufferRef<'_> {
        MutableBufferRef(&mut self[range])
    }

    /// The buffer's bytes.
    pub fn as_slice(&self) -> &[u8] {
        self
    }

    /// The buffer's bytes.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self
    }

    /// Copies the buffer into the start of `dest`, which is at least as long.
    pub fn copy_to_slice(&self, dest: &mut [u8]) {
        dest[..self.len].copy_from_slice(self);
    }

    /// Copies `src`, as long as the buffer, into it.
    pub fn copy_from_buffer(&mut self, src: BufferRef<'_>) {
        self.copy_from_slice(&src);
    }

    /// The budget the buffer counts against.
    pub fn budget(&self) -> &'a TransferBudget {
        self.budget
    }
}

/// A read-only view of a buffer's bytes.
#[derive(Clone, Copy, Debug)]
pub struct BufferRef<'a>(&'a [u8]);

impl<'a> From<&'a [u8]> for BufferRef<'a> {
    fn from(bytes: &'a [u8]) -> Self {
        Self(bytes)
    }
}

impl Deref for BufferRef<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.0
    }
}

impl<'a> BufferRef<'a> {
    /// The view's bytes, for as long as the buffer is borrowed.
    pub fn as_slice(&self) -> &'a [u8] {
        self.0
    }

    /// The bytes of `range`.
    pub fn subslice<R: SliceRange>(&self, range: R) -> BufferRef<'a> {
        BufferRef(&self.0[range])
    }

    /// The bytes before `mid`, and those from it.
    pub fn split_at(&self, mid: usize) -> (BufferRef<'a>, BufferRef<'a>) {
        let (left, right) = self.0.split_at(mid);
        (BufferRef(left), BufferRef(right))
    }

    /// The view in chunks of `chunk_size` bytes, the last perhaps shorter.
    pub fn chunks(self, chunk_size: usize) -> impl ExactSizeIterator<Item = BufferRef<'a>> {
        self.0.chunks(chunk_size).map(BufferRef)
    }

    /// Copies the view into the start of `dest`, which is at least as long.
    pub fn copy_to_slice(&self, dest: &mut [u8]) {
        dest[..self.0.len()].copy_from_slice(self.0);
    }

    /// Appends the view's bytes to `dest`.
    pub fn append_to(&self, dest: &mut Vec<u8>) {
        dest.extend_from_slice(self.0);
    }
}

/// A view of a buffer's bytes.
#[derive(Debug)]
pub struct MutableBufferRef<'a>(&'a mut [u8]);

impl<'a> From<&'a mut [u8]> for MutableBufferRef<'a> {
    fn from(bytes: &'a mut [u8]) -> Self {
        Self(bytes)
    }
}

impl Deref for MutableBufferRef<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.0
    }
}

impl DerefMut for MutableBufferRef<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        self.0
    }
}

impl<'a> MutableBufferRef<'a> {
    /// The view, read-only.
    pub fn as_ref(&self) -> BufferRef<'_> {
        BufferRef(self.0)
    }

    /// The view, borrowed again.
    pub fn reborrow(&mut self) -> MutableBufferRef<'_> {
        MutableBufferRef(self.0)
    }

    /// The view's bytes.
    pub fn as_slice(&self) -> &[u8] {
        self.0
    }

    /// The view's bytes.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.0
    }

    /// The bytes of `range`, read-only.
    pub fn subslice<R: SliceRange>(&self, range: R) -> BufferRef<'_> {
        BufferRef(&self.0[range])
    }

    /// The bytes of `range`.
    pub fn subslice_mut<R: SliceRange>(self, range: R) -> MutableBufferRef<'a> {
        MutableBufferRef(&mut self.0[range])
    }

    /// The bytes before `mid`, and those from it, read-only.
    pub fn split_at(&self, mid: usize) -> (BufferRef<'_>, BufferRef<'_>) {
        let (left, right) = self.0.split_at(mid);
        (BufferRef(left), BufferRef(right))
    }

    /// The bytes before `mid`, and those from it.
    pub fn split_at_mut(self, mid: usize) -> (MutableBufferRef<'a>, MutableBufferRef<'a>) {
        let (left, right) = self.0.split_at_mut(mid);
        (MutableBufferRef(left), MutableBufferRef(right))
    }

    /// The view in chunks of `chunk_size` bytes, the last perhaps shorter.
    pub fn chunks_mut(
        self,
        chunk_size: usize,
    ) -> impl ExactSizeIterator<Item = MutableBufferRef<'a>> {
        self.0.chunks_mut(chunk_size).map(MutableBufferRef)
    }

    /// Copies the view into the start of `dest`, which is at least as long.
    pub fn copy_to_slice(&self, dest: &mut [u8]) {
        dest[..self.0.len()].copy_from_slice(self.0);
    }

    /// Copies `src`, as long as the view, into it.
    pub fn copy_from_buffer(&mut self, src: BufferRef<'_>) {
        self.0.copy_from_slice(&src);
    }
}

#[cfg(test)]
mod tests {
    use super::TransferBudget;
    use futures::FutureExt as _;

    /// An allocation past the budget waits for a buffer to go, and one larger than the whole
    /// budget gets through once it is alone: the interleavings a device's transfers rely on.
    #[fuchsia_async::run_singlethreaded(test)]
    async fn allocations_wait_for_room_in_the_budget() {
        let budget = TransferBudget::new(512, 2048);
        let first = budget.allocate_buffer(1500).await;
        assert_eq!((first.len(), budget.used()), (1500, 1536));
        assert!(first.as_ptr().align_offset(512) == 0 && first.iter().all(|&b| b == 0));

        let mut second = Box::pin(budget.allocate_buffer(1024));
        assert!((&mut second).now_or_never().is_none(), "allocated past the budget");
        drop(first);
        let second = second.await;
        assert_eq!(budget.used(), 1024);

        let mut whole = Box::pin(budget.allocate_buffer(4096));
        assert!((&mut whole).now_or_never().is_none(), "allocated past the budget");
        drop(second);
        let whole = whole.await;
        assert_eq!((whole.len(), budget.used()), (4096, 4096));
        drop(whole);
        assert_eq!(budget.used(), 0);
    }
}
