// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Explicit per-host state for `bucket_cell` (board 10962): replaces `node_local!` statics for
//! host disks, governor wakeups, and the shared recent/unpacked part cache.

use crate::fetch::{PartMemory, RECENT};
use crate::host_disk::Disks;
use std::sync::Arc;
#[cfg(not(madsim))]
use std::sync::OnceLock;

/// Per-host state owned explicitly by a `local_cell::Host` (and in turn by `cladefs::Host` /
/// `cladefsd::Daemon`).
pub struct Host {
    pub(crate) disks: Disks,
    parts: Arc<PartMemory>,
}

impl Host {
    /// Creates a fresh, isolated `bucket_cell` host context.
    pub fn new() -> Arc<Self> {
        Arc::new(Self { disks: Disks::new(), parts: PartMemory::new(RECENT) })
    }

    /// The process's default `bucket_cell` host context outside `madsim`.
    #[cfg(not(madsim))]
    pub fn process() -> &'static Arc<Self> {
        // Outside the simulation: single default host context for standalone CLIs and tests.
        static PROCESS: OnceLock<Arc<Host>> = OnceLock::new();
        PROCESS.get_or_init(Self::new)
    }

    /// This host's unified recent/unpacked part cache (`PartMemory`).
    pub fn parts(&self) -> &Arc<PartMemory> {
        &self.parts
    }
}
