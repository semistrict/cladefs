// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A [`CellClient`] as the backend a VMM serves its guest's requests with
//! (`cladefs_core::extent_requests`, `virtio_extent`): each of doc-4's requests one of its calls of
//! the daemon, the guest's window mapped in this process.

use crate::CellClient;
use cladefs_core::extent_requests::{Backend, GuestBackend};
use cladefs_core::{
    Commit, Errno, ExtentId, ExtentInfo, VolatileSlice, VolatileSliceMut, WindowMapping,
    WriteBarriers,
};

impl Backend for CellClient {
    fn cell(&self) -> u64 {
        CellClient::cell(self)
    }

    fn copies_on_write(&self) -> bool {
        CellClient::copies_on_write(self)
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

    async fn map_many(&self, mappings: &[WindowMapping], warm: bool) -> Result<(), Errno> {
        match warm {
            true => self.map_many_warm_async(mappings).await,
            false => self.map_many_async(mappings).await,
        }
    }

    fn unmap(
        &self,
        window_offset: u64,
        len: u64,
    ) -> impl Future<Output = Result<(), Errno>> + Send {
        self.unmap_async(window_offset, len)
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
}

impl GuestBackend for CellClient {
    fn prefault(&self, window_offset: u64, len: u64) {
        CellClient::prefault(self, window_offset, len);
    }

    fn make_private(
        &self,
        window_offset: u64,
        len: u64,
    ) -> impl Future<Output = Result<(), Errno>> + Send {
        self.make_private_async(window_offset, len)
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
