// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Shared per-tenant quota registry (live weak handles, byte limits, and weights) used by both
//! [`super::memory`] and [`super::disk`] (board 11703).

use std::collections::HashMap;
use std::sync::{Arc, Weak};

/// Tracks live per-tenant objects `T` together with their configured byte limits and weights so
/// quotas set before a tenant is created apply automatically on registration.
pub(super) struct QuotaRegistry<T> {
    entries: Vec<Weak<T>>,
    limits: Option<HashMap<u64, u64>>,
    weights: Option<HashMap<u64, u64>>,
    default_weight: u64,
}

impl<T> QuotaRegistry<T> {
    pub(super) fn new(default_weight: u64) -> Self {
        Self { entries: Vec::new(), limits: None, weights: None, default_weight }
    }

    pub(super) fn alive(&self) -> impl Iterator<Item = Arc<T>> + '_ {
        self.entries.iter().filter_map(Weak::upgrade)
    }

    pub(super) fn register(&mut self, entry: &Arc<T>) {
        self.entries.retain(|alive| alive.strong_count() > 0);
        self.entries.push(Arc::downgrade(entry));
    }

    pub(super) fn limit(&self, tenant: u64) -> Option<u64> {
        self.limits.as_ref().and_then(|limits| limits.get(&tenant).copied())
    }

    pub(super) fn set_limit(&mut self, tenant: u64, bytes: Option<u64>) {
        let limits = self.limits.get_or_insert_with(HashMap::new);
        match bytes {
            Some(bytes) => {
                limits.insert(tenant, bytes);
            }
            None => {
                limits.remove(&tenant);
            }
        }
    }

    pub(super) fn weight(&self, tenant: u64) -> u64 {
        self.weights
            .as_ref()
            .and_then(|weights| weights.get(&tenant).copied())
            .unwrap_or(self.default_weight)
    }

    pub(super) fn set_weight(&mut self, tenant: u64, weight: u64) {
        self.weights.get_or_insert_with(HashMap::new).insert(tenant, weight);
    }
}
