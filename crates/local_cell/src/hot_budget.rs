// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Node-wide memory accounting for in-flight staged commit buffers (Fast WAL, board 12331),
//! counted by the daemon's memory governor under its single memory budget.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Node-wide byte counter for staged in-memory commit buffers (Fast WAL, board 12331).
pub(crate) struct HotBudget {
    total: AtomicU64,
}

impl HotBudget {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self { total: AtomicU64::new(0) })
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }
}

/// Total bytes held in staged commit buffers on `host` (board 12331).
pub fn hot_bytes_on(host: &crate::Host) -> u64 {
    host.hot_budget.bytes()
}

/// Total bytes held in staged commit buffers on the process's default host outside `madsim`.
#[cfg(not(madsim))]
pub fn hot_bytes() -> u64 {
    hot_bytes_on(crate::Host::process())
}

/// RAII accounting guard for bytes held in memory against a host's [`HotBudget`].
pub(crate) struct Holding {
    bytes: u64,
    budget: Option<Arc<HotBudget>>,
}

impl Holding {
    /// Holds nothing, counted against `budget` when given.
    pub(crate) fn on(budget: Option<Arc<HotBudget>>) -> Self {
        Self { bytes: 0, budget }
    }

    /// Holds `bytes` more.
    pub(crate) fn hold(&mut self, bytes: u64) {
        if let Some(budget) = &self.budget {
            budget.total.fetch_add(bytes, Ordering::Relaxed);
        }
        self.bytes += bytes;
    }
}

impl Drop for Holding {
    fn drop(&mut self) {
        if let Some(budget) = &self.budget {
            budget.total.fetch_sub(self.bytes, Ordering::Relaxed);
        }
    }
}
