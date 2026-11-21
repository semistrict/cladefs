// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! For tests, here and in sqlite_cladefs': a window of memory of its own.

use std::alloc::Layout;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::executor::block_on;
use storage_device::Device;

use crate::{Contents, FileSetWindow, Run};

/// What a [`CopyingWindow`] holds where nothing is mapped: no page a test writes holds it whole.
pub const UNMAPPED: u8 = 0xab;

/// A window that copies the blocks it maps into memory of its own, as a daemon's maps its cache's
/// pages: what it unmaps reads as [`UNMAPPED`].
pub struct CopyingWindow {
    memory: NonNull<u8>,
    layout: Layout,
    /// Whether its maps fail, as a daemon's do when the bucket is out of reach.
    failing: Arc<AtomicBool>,
}

// SAFETY: written only as the file set maps, under its lock.
unsafe impl Send for CopyingWindow {}
// SAFETY: as Send's: shared, its memory is written only under the file set's lock.
unsafe impl Sync for CopyingWindow {}

impl CopyingWindow {
    /// A window of `size` bytes, aligned to `block_size`: no more than a test's file set maps, as
    /// each is filled when made, and a test may make one each time it opens its file set.
    pub fn new(size: u64, block_size: u32) -> Self {
        let layout = Layout::from_size_align(size as usize, block_size as usize);
        let layout = layout.expect("a layout");
        assert!(layout.size() > 0, "a window of no bytes");
        // SAFETY: a layout of some bytes.
        let memory = NonNull::new(unsafe { std::alloc::alloc(layout) }).expect("memory");
        // SAFETY: the memory allocated, `size` bytes.
        unsafe { memory.as_ptr().write_bytes(UNMAPPED, layout.size()) };
        Self { memory, layout, failing: Arc::default() }
    }

    /// What makes its maps fail while it is set, which the test keeps once the window is the file
    /// set's.
    pub fn failing(&self) -> Arc<AtomicBool> {
        self.failing.clone()
    }

    fn size(&self) -> u64 {
        self.layout.size() as u64
    }

    #[allow(clippy::mut_from_ref)]
    fn bytes(&self, at: u64, len: u64) -> &mut [u8] {
        assert!(at + len <= self.size());
        // SAFETY: within the memory, which the file set changes under its lock alone.
        unsafe {
            std::slice::from_raw_parts_mut(self.memory.as_ptr().add(at as usize), len as usize)
        }
    }
}

impl Drop for CopyingWindow {
    fn drop(&mut self) {
        // SAFETY: allocated with this layout.
        unsafe { std::alloc::dealloc(self.memory.as_ptr(), self.layout) };
    }
}

impl FileSetWindow for CopyingWindow {
    fn memory(&self) -> (NonNull<u8>, u64) {
        (self.memory, self.size())
    }

    fn map(&self, device: &dyn Device, runs: &[Run]) -> anyhow::Result<()> {
        anyhow::ensure!(!self.failing.load(Ordering::Relaxed), "a map made to fail");
        for run in runs {
            match run.contents {
                Contents::Extent { extent, offset } => {
                    let mut buffer = block_on(device.allocate_buffer(run.len as usize));
                    block_on(device.read(extent, offset, buffer.as_mut()))?;
                    buffer.as_ref().copy_to_slice(self.bytes(run.at, run.len));
                }
                Contents::Zeros => self.bytes(run.at, run.len).fill(0),
            }
        }
        Ok(())
    }

    fn unmap(&self, at: u64, len: u64) -> anyhow::Result<()> {
        self.bytes(at, len).fill(UNMAPPED);
        Ok(())
    }
}
