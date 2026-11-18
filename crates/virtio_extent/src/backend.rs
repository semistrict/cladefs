// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What serving the device's requests asks of its backend, its store: on Linux, cladefs's
//! `CellClient`, the device's connection to the extent daemon; a fake one in tests.

use std::future::Future;

use cladefs::{
    Commit, Errno, ExtentId, ExtentInfo, VolatileSlice, VolatileSliceMut, WriteBarriers,
};

/// The store a device's requests are served with ([`handle`](crate::handle)), its backend: each
/// method one of the device's requests, as cladefs's `CellClient` serves it, which its
/// documentation describes. Errors are the request's status.
pub trait Backend: Send + Sync {
    /// The branch the guest mints extents of.
    fn branch(&self) -> u64;
    /// Whether the store's window copies on write: a writer's (backlog doc-13).
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
    /// `MAP`: maps `len` bytes at `offset` in `extent` at `window_offset` in the window.
    fn map(
        &self,
        extent: ExtentId,
        offset: u64,
        len: u64,
        window_offset: u64,
    ) -> impl Future<Output = Result<(), Errno>> + Send;
    /// `MAP` with `MAP_ZEROS`: maps zeros at `len` bytes of the window at `window_offset`.
    fn map_zeros(
        &self,
        window_offset: u64,
        len: u64,
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
    /// `INFO`: the store's capacity, use and sequence numbers.
    fn info(&self) -> impl Future<Output = Result<ExtentInfo, Errno>> + Send;
    /// `STATS`: the guest's answer to the daemon's ask `ask`, its stats as text.
    fn answer_stats(
        &self,
        ask: u64,
        guest: Option<&str>,
    ) -> impl Future<Output = Result<(), Errno>> + Send;
    /// `RESIZED`: the guest took the shrink to `capacity` bytes it was asked to, if `took`.
    fn resized(&self, capacity: u64, took: bool) -> impl Future<Output = Result<(), Errno>> + Send;
    /// `ADVANCE`: a follower's view moves to its writer's latest commit: its number.
    fn advance(&self) -> impl Future<Output = Result<u64, Errno>> + Send;
    /// `LET_GO`: lets go of what the view held of the commits before the one numbered `commit`.
    fn let_go(&self, commit: u64) -> impl Future<Output = Result<(), Errno>> + Send;
    /// `PIN`: keeps `extent` whatever commit the view advances to.
    fn pin(&self, extent: ExtentId) -> impl Future<Output = Result<(), Errno>> + Send;
    /// `UNPIN`: lets go of `extent`'s pin.
    fn unpin(&self, extent: ExtentId) -> impl Future<Output = Result<(), Errno>> + Send;
}

#[cfg(target_os = "linux")]
impl Backend for cladefs::CellClient {
    fn branch(&self) -> u64 {
        cladefs::CellClient::branch(self)
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

    fn map(
        &self,
        extent: ExtentId,
        offset: u64,
        len: u64,
        window_offset: u64,
    ) -> impl Future<Output = Result<(), Errno>> + Send {
        self.map_async(extent, offset, len, window_offset)
    }

    fn map_zeros(
        &self,
        window_offset: u64,
        len: u64,
    ) -> impl Future<Output = Result<(), Errno>> + Send {
        self.map_zeros_async(window_offset, len)
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

    fn advance(&self) -> impl Future<Output = Result<u64, Errno>> + Send {
        self.advance_async()
    }

    fn let_go(&self, commit: u64) -> impl Future<Output = Result<(), Errno>> + Send {
        self.let_go_async(commit)
    }

    fn pin(&self, extent: ExtentId) -> impl Future<Output = Result<(), Errno>> + Send {
        self.pin_async(extent)
    }

    fn unpin(&self, extent: ExtentId) -> impl Future<Output = Result<(), Errno>> + Send {
        self.unpin_async(extent)
    }
}
