// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What serving the device's requests asks of its backend, its cell: cladefs's `CellClient`, the device's connection to the extent daemon; a fake one in tests.

use std::future::Future;

use cladefs::{
    Commit, Errno, ExtentId, ExtentInfo, VolatileSlice, VolatileSliceMut, WindowMapping,
    WriteBarriers,
};

/// The cell a device's requests are served with ([`handle`](crate::handle)), its backend: each
/// method one of the device's requests, as cladefs's `CellClient` serves it, which its
/// documentation describes. Errors are the request's status.
pub trait Backend: Send + Sync {
    /// The cell the guest mints extents of.
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
    /// `MAP`: maps `mappings`, in order, the first error failing the rest.
    fn map_many(
        &self,
        mappings: &[WindowMapping],
    ) -> impl Future<Output = Result<(), Errno>> + Send;
    /// `MAP_PREFAULT`: maps `len` bytes of the window at `window_offset` for the guest ahead of its
    /// accesses. It must not wait.
    fn prefault(&self, window_offset: u64, len: u64);
    /// `UNMAP`: unmaps `len` bytes of the window at `window_offset`.
    fn unmap(&self, window_offset: u64, len: u64)
    -> impl Future<Output = Result<(), Errno>> + Send;
    /// `PRIVATE`: makes `len` bytes of the window at `window_offset` private now.
    fn make_private(
        &self,
        window_offset: u64,
        len: u64,
    ) -> impl Future<Output = Result<(), Errno>> + Send;
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
    /// `STATS`: the guest's answer to the daemon's ask `ask`, its stats as text.
    fn answer_stats(
        &self,
        ask: u64,
        guest: Option<&str>,
    ) -> impl Future<Output = Result<(), Errno>> + Send;
    /// `RESIZED`: the guest took the shrink to `capacity` bytes it was asked to, if `took`.
    fn resized(&self, capacity: u64, took: bool) -> impl Future<Output = Result<(), Errno>> + Send;
}

impl Backend for cladefs::CellClient {
    fn cell(&self) -> u64 {
        cladefs::CellClient::cell(self)
    }

    fn copies_on_write(&self) -> bool {
        cladefs::CellClient::copies_on_write(self)
    }

    fn read(
        &self,
        extent: ExtentId,
        offset: u64,
        buffers: &mut [VolatileSliceMut<'_>],
    ) -> impl Future<Output = Result<(), Errno>> + Send {
        self.read_async(extent, offset, buffers)
    }

    fn write(
        &self,
        extent: ExtentId,
        offset: u64,
        buffers: &[VolatileSlice<'_>],
        barriers: WriteBarriers<'_>,
    ) -> impl Future<Output = Result<(), Errno>> + Send {
        self.write_with_barriers_async(extent, offset, buffers, barriers)
    }

    fn free_many(&self, extents: &[ExtentId]) -> impl Future<Output = Result<(), Errno>> + Send {
        self.free_many_async(extents)
    }

    fn map_many(
        &self,
        mappings: &[WindowMapping],
    ) -> impl Future<Output = Result<(), Errno>> + Send {
        self.map_many_async(mappings)
    }

    fn prefault(&self, window_offset: u64, len: u64) {
        cladefs::CellClient::prefault(self, window_offset, len);
    }

    fn unmap(
        &self,
        window_offset: u64,
        len: u64,
    ) -> impl Future<Output = Result<(), Errno>> + Send {
        self.unmap_async(window_offset, len)
    }

    fn make_private(
        &self,
        window_offset: u64,
        len: u64,
    ) -> impl Future<Output = Result<(), Errno>> + Send {
        self.make_private_async(window_offset, len)
    }

    fn commit(
        &self,
        root: Option<&[u8]>,
        commit: Commit,
    ) -> impl Future<Output = Result<(), Errno>> + Send {
        self.commit_async(root, commit)
    }

    fn root(&self) -> impl Future<Output = Result<Vec<u8>, Errno>> + Send {
        self.root_async()
    }

    fn info(&self) -> impl Future<Output = Result<ExtentInfo, Errno>> + Send {
        self.info_async()
    }

    fn answer_stats(
        &self,
        ask: u64,
        guest: Option<&str>,
    ) -> impl Future<Output = Result<(), Errno>> + Send {
        self.answer_stats_async(ask, guest)
    }

    fn resized(&self, capacity: u64, took: bool) -> impl Future<Output = Result<(), Errno>> + Send {
        self.resized_async(capacity, took)
    }
}
