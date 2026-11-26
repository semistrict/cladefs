// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! In-flight byte accounting for a tenant's VM connections (TASK-149.35).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Where an [`InFlight`]'s default budget comes from when no per-tenant budget is set.
pub enum DefaultBudget {
    /// Fixed byte ceiling.
    Fixed(u64),
    /// Dynamically updated shared byte ceiling.
    Shared(Arc<AtomicU64>),
}

impl DefaultBudget {
    fn get(&self) -> u64 {
        match self {
            Self::Fixed(bytes) => *bytes,
            Self::Shared(shared) => shared.load(Ordering::Relaxed),
        }
    }
}

/// cladefs: what a tenant's connections have the daemon hold of its VM processes' requests together
/// (TASK-149.35).
pub struct InFlight {
    held: AtomicU64,
    /// Its budget, or 0, its default's.
    budget: AtomicU64,
    /// cladefs: the most the governor allows it, or `u64::MAX`.
    governed: AtomicU64,
    /// The most it held, which the tests measure.
    peak: AtomicU64,
    /// What says it held less, or its budget grew.
    less: event_listener::Event,
    /// cladefs: its budget unless set.
    default: DefaultBudget,
    /// cladefs: a tenant's, what every tenant's together hold within too.
    within: Option<Arc<InFlight>>,
}

impl InFlight {
    /// Creates an [`InFlight`] counter with the given default budget and optional parent ceiling.
    pub fn made(default: DefaultBudget, within: Option<Arc<InFlight>>) -> Self {
        Self {
            held: AtomicU64::new(0),
            budget: AtomicU64::new(0),
            governed: AtomicU64::new(u64::MAX),
            peak: AtomicU64::new(0),
            less: event_listener::Event::new(),
            default,
            within,
        }
    }

    /// cladefs: the governor allows it `bytes` at most, a payload's at least (board 8465).
    pub fn govern(&self, bytes: u64) {
        let bytes = bytes.max(crate::wire::MAX_PAYLOAD);
        if self.governed.swap(bytes, Ordering::Relaxed) < bytes {
            self.changed();
        }
    }

    /// What it holds.
    pub fn held(&self) -> u64 {
        self.held.load(Ordering::Relaxed)
    }

    /// The most it held.
    pub fn peak(&self) -> u64 {
        self.peak.load(Ordering::Relaxed)
    }

    /// Its budget: its own, or its default; no more than the governor allows.
    pub fn budget(&self) -> u64 {
        let budget = match self.budget.load(Ordering::Relaxed) {
            0 => self.default.get(),
            budget => budget,
        };
        budget.min(self.governed.load(Ordering::Relaxed))
    }

    /// Sets or clears an explicit per-tenant budget override.
    pub fn set_budget(&self, bytes: Option<u64>) {
        self.budget.store(bytes.unwrap_or(0), Ordering::Relaxed);
        self.changed();
    }

    /// Tells those waiting within its budget that there may be enough.
    pub fn changed(&self) {
        self.less.notify(usize::MAX);
    }

    /// Holds `bytes` more, within its budget or not: what the daemon queued for a VM process, which
    /// it holds whatever. cladefs: and every tenant's, a tenant's.
    pub fn add(&self, bytes: u64) {
        let held = self.held.fetch_add(bytes, Ordering::Relaxed) + bytes;
        self.peak.fetch_max(held, Ordering::Relaxed);
        if let Some(within) = &self.within {
            within.add(bytes);
        }
    }

    /// Holds `bytes` fewer. cladefs: and every tenant's, a tenant's.
    pub fn sub(&self, bytes: u64) {
        if bytes > 0 {
            self.held.fetch_sub(bytes, Ordering::Relaxed);
            self.changed();
            if let Some(within) = &self.within {
                within.sub(bytes);
            }
        }
    }

    /// Holds `bytes` more, if they are within its budget, or it holds nothing.
    fn try_take(&self, bytes: u64) -> bool {
        let budget = self.budget();
        let taken = self.held.try_update(Ordering::Relaxed, Ordering::Relaxed, |held| {
            (held == 0 || held + bytes <= budget).then_some(held + bytes)
        });
        if let Ok(held) = taken {
            self.peak.fetch_max(held + bytes, Ordering::Relaxed);
        }
        taken.is_ok()
    }

    /// Holds `bytes` more, once they are within its budget, or it holds nothing: waiting, holding
    /// no thread, until then. cladefs: a tenant's, then within every tenant's, as it is (board
    /// 8465): what it took of its own given back if it is dropped waiting for that.
    pub async fn take(&self, bytes: u64) {
        self.take_own(bytes).await;
        let Some(within) = &self.within else { return };
        /// What a tenant took of its own, given back unless every tenant's is taken too.
        struct Taken<'a>(&'a InFlight, u64);
        impl Drop for Taken<'_> {
            fn drop(&mut self) {
                self.0.held.fetch_sub(self.1, Ordering::Relaxed);
                self.0.changed();
            }
        }
        let taken = Taken(self, bytes);
        within.take_own(bytes).await;
        std::mem::forget(taken);
    }

    /// Holds `bytes` more of its own, as [`InFlight::take`] does, its budget alone looked at.
    async fn take_own(&self, bytes: u64) {
        loop {
            if self.try_take(bytes) {
                return;
            }
            let less = self.less.listen();
            if self.try_take(bytes) {
                return;
            }
            less.await;
        }
    }
}
