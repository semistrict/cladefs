// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{
    Commit, Errno, ExtentId, ExtentInfo, VolatileSlice, VolatileSliceMut, WindowMapping,
    WriteBarriers,
};

/// The cell requests are served with ([`handle`](super::handle)), their backend: each method one
/// of doc-4's requests, as cladefs's cell serves it: in a VMM, its `CellClient` of the extent
/// daemon; in the daemon, the connection's open cell (board 14378). Errors are the request's
/// status.
pub trait Backend: Send + Sync {
    /// The cell its requests mint extents of.
    fn cell(&self) -> u64;
    /// Whether the cell's window copies on write: a writer's (backlog doc-13).
    fn copies_on_write(&self) -> bool;
    /// `READ`: the bytes at `offset` in `extent` into `buffers`, in order.
    fn read(
        &self,
        extent: ExtentId,
        offset: u64,
        buffers: &mut [VolatileSliceMut<'_>],
    ) -> impl Future<Output = Result<(), Errno>> + Send;
    /// `WRITE`: the bytes of `buffers`, in order, at `offset` in `extent`, ordered with
    /// `barriers`.
    fn write(
        &self,
        extent: ExtentId,
        offset: u64,
        buffers: &[VolatileSlice<'_>],
        barriers: WriteBarriers<'_>,
    ) -> impl Future<Output = Result<(), Errno>> + Send;
    /// `FREE`: frees `extents`, in order, the first error failing the rest.
    fn free_many(&self, extents: &[ExtentId]) -> impl Future<Output = Result<(), Errno>> + Send;
    /// `MAP`: maps `mappings`, in order, the first error failing the rest; each mapped range
    /// warmed in the daemon's extent cache first if `warm` (a process's `MAP_WARM`, board 11681).
    fn map_many(
        &self,
        mappings: &[WindowMapping],
        warm: bool,
    ) -> impl Future<Output = Result<(), Errno>> + Send;
    /// `UNMAP`: unmaps `len` bytes of the window at `window_offset`.
    fn unmap(&self, window_offset: u64, len: u64)
    -> impl Future<Output = Result<(), Errno>> + Send;
    /// `COMMIT`: commits what was written, setting the root to `root` if given.
    fn commit(
        &self,
        root: Option<&[u8]>,
        commit: Commit,
    ) -> impl Future<Output = Result<(), Errno>> + Send;
    /// `ROOT`: the root, as last committed.
    fn root(&self) -> impl Future<Output = Result<Vec<u8>, Errno>> + Send;
    /// `INFO`: the cell's capacity, use and sequence numbers.
    fn info(&self) -> impl Future<Output = Result<ExtentInfo, Errno>> + Send;
}

/// What a guest's requests ask of the cell besides: what its VMM does for it, the guest's window
/// being mapped in the VMM's process, and its answers to the daemon's asks.
pub trait GuestBackend: Backend {
    /// `MAP_PREFAULT`: maps `len` bytes of the window at `window_offset` for the guest ahead of its
    /// accesses. It must not wait.
    fn prefault(&self, window_offset: u64, len: u64);
    /// `PRIVATE`: makes `len` bytes of the window at `window_offset` private now.
    fn make_private(
        &self,
        window_offset: u64,
        len: u64,
    ) -> impl Future<Output = Result<(), Errno>> + Send;
    /// `STATS`: the guest's answer to the daemon's ask `ask`, its stats as text.
    fn answer_stats(
        &self,
        ask: u64,
        guest: Option<&str>,
    ) -> impl Future<Output = Result<(), Errno>> + Send;
    /// `RESIZED`: the guest took the shrink to `capacity` bytes it was asked to, if `took`.
    fn resized(&self, capacity: u64, took: bool) -> impl Future<Output = Result<(), Errno>> + Send;
}
