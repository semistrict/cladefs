// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! For tests, here and in sqlite_extents': a window of memory of its own.

use std::alloc::Layout;
use std::ptr::NonNull;

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
}

// SAFETY: written only as the file set maps, under its lock.
unsafe impl Send for CopyingWindow {}
// SAFETY: as Send's: shared, its memory is written only under the file set's lock.
unsafe impl Sync for CopyingWindow {}

impl CopyingWindow {
    /// Four regions of 16 MiB, with the default options.
    pub const SIZE: u64 = 64 << 20;

    /// A window of [`Self::SIZE`] bytes, aligned to `block_size`.
    pub fn new(block_size: u32) -> Self {
        let layout = Layout::from_size_align(Self::SIZE as usize, block_size as usize);
        let layout = layout.expect("a layout");
        // SAFETY: a layout of some bytes.
        let memory = NonNull::new(unsafe { std::alloc::alloc(layout) }).expect("memory");
        // SAFETY: the memory allocated, `SIZE` bytes.
        unsafe { memory.as_ptr().write_bytes(UNMAPPED, Self::SIZE as usize) };
        Self { memory, layout }
    }

    #[allow(clippy::mut_from_ref)]
    fn bytes(&self, at: u64, len: u64) -> &mut [u8] {
        assert!(at + len <= Self::SIZE);
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
        (self.memory, Self::SIZE)
    }

    fn map(&self, device: &dyn Device, runs: &[Run]) -> anyhow::Result<()> {
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
