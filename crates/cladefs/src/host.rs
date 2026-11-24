// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Explicit per-host state for `cladefs` (board 10962), replacing all `node_local!` statics across
//! `ephemeral_host`, `bucket`, `host_cell`, `halt`, `evidence`, `slabs`, `cache`, and
//! `storage::collect`, and owning the underlying [`local_cell::Host`] (which in turn owns
//! [`bucket_cell::Host`]).
//!
//! Outside `madsim`, [`Host::process`] returns the process's default `Host` sharing
//! `local_cell::Host::process()` and `bucket_cell::Host::process()`. Under `madsim`, there is no
//! process-default fallback: every `cladefsd::Daemon` owns its own `Arc<Host>`, so simulated nodes
//! share zero mutable host state and dropping a node's `Arc<Daemon>` releases its arenas and state
//! deterministically.

use std::sync::Arc;
#[cfg(not(madsim))]
use std::sync::OnceLock;

/// Per-host context for one `cladefs` host / extent daemon.
pub struct Host {
    local: Arc<local_cell::Host>,
    pub(crate) ephemeral: crate::ephemeral_host::State,
    bucket: crate::bucket::HostState,
    host_cells: Arc<crate::host_cell::State>,
    pub(crate) halts: crate::halt::Registry,
    pub(crate) evidence: crate::evidence::HostEvidence,
    slabs: Arc<crate::slabs::HostSlabs>,
    cache: Arc<crate::cache::ExtentCache>,
    pub(crate) collectors: crate::storage::collect::Registry,
}

impl Host {
    /// Creates an isolated host context with fresh `local_cell::Host` and `bucket_cell::Host`.
    pub fn new() -> Arc<Self> {
        Self::with_local(local_cell::Host::new())
    }

    /// Creates a host context layered over `local`.
    pub fn with_local(local: Arc<local_cell::Host>) -> Arc<Self> {
        let bucket = crate::bucket::HostState::new(local.bucket().parts().clone());
        let slabs = crate::slabs::HostSlabs::new();
        let cache = crate::cache::ExtentCache::new(slabs.clone());
        Arc::new(Self {
            local,
            ephemeral: crate::ephemeral_host::State::new(),
            bucket,
            host_cells: Arc::new(crate::host_cell::State::new()),
            halts: crate::halt::Registry::new(),
            evidence: crate::evidence::HostEvidence::new(),
            slabs,
            cache,
            collectors: crate::storage::collect::Registry::new(),
        })
    }

    /// The process's default host context (outside `madsim`), sharing
    /// `local_cell::Host::process()`.
    #[cfg(not(madsim))]
    pub fn process() -> &'static Arc<Self> {
        // Outside the simulation: single default host for standalone CLI/tests.
        static PROCESS: OnceLock<Arc<Host>> = OnceLock::new();
        PROCESS.get_or_init(|| Self::with_local(local_cell::Host::process().clone()))
    }

    /// The underlying `local_cell::Host`.
    pub fn local(&self) -> &Arc<local_cell::Host> {
        &self.local
    }

    /// The underlying `bucket_cell::Host`.
    pub fn bucket_host(&self) -> &Arc<bucket_cell::Host> {
        self.local.bucket()
    }

    pub(crate) fn bucket_state(&self) -> &crate::bucket::HostState {
        &self.bucket
    }

    pub(crate) fn host_cells(&self) -> &Arc<crate::host_cell::State> {
        &self.host_cells
    }

    pub(crate) fn slabs(&self) -> &Arc<crate::slabs::HostSlabs> {
        &self.slabs
    }

    pub(crate) fn cache(&self) -> &Arc<crate::cache::ExtentCache> {
        &self.cache
    }
}
