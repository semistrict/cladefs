// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Memory a block device maps its flushed blocks into, read-only, as a VM's DAX window maps
//! its filesystem's (backlog doc-13): so that a read copies from memory, with no request of the
//! device, and SQLite's memory-mapped I/O (`xFetch`) reads pages in place, copying nothing.
//!
//! The window is laid out as the contents are: block `k` at `k` blocks into it, mapped to the block
//! of the extent that holds it, or to zeros if a hole. A flush maps what it changed again, before
//! any read sees the contents as of it (`block_device`).

use anyhow::Error;
use std::ops::Range;
use std::ptr::NonNull;
use storage_device::{Device, ExtentId};

/// What a run of a window maps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Contents {
    /// A sealed extent's bytes, from `offset`.
    Extent { extent: ExtentId, offset: u64 },
    /// Zeros.
    Zeros,
}

/// Ranges of the contents, in order, and what each holds as of the last flush: none past the end.
pub type Extents = Vec<(Range<u64>, Option<Contents>)>;

/// A run of a window: `len` bytes at `at` mapping `contents`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub at: u64,
    pub len: u64,
    pub contents: Contents,
}

/// A block device's window. Offsets and lengths are multiples of the device's block size, which is
/// the window's page size.
///
/// A block device drops its extent device before its window, whether it opened or not
/// ([`crate::BlockDevice::open_mapped`]): a window whose memory may go only once what maps it
/// has, as a daemon's cell's, which its extent device holds too, frees it then (board 8551).
pub trait Window: Send + Sync {
    /// The memory: its first byte, and how many it holds.
    fn memory(&self) -> (NonNull<u8>, u64);

    /// Maps each run, read-only, replacing what was mapped there: blocks of `device`'s sealed
    /// extents, or zeros.
    fn map(&self, device: &dyn Device, runs: &[Run]) -> Result<(), Error>;

    /// Unmaps `len` bytes at `at`: nothing reads them until they are mapped again.
    fn unmap(&self, at: u64, len: u64) -> Result<(), Error>;
}
