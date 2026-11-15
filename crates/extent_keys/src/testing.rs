// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: what tests of encryption at rest need (the `testing` feature, backlog doc-16): a
//! KMS that counts the calls made of it ([`Counted`]), so that a test can say how many a path
//! costs: none on a fork's, one for a host taking a cell.

use crate::kms::{Error, Kms};
use crate::{Secret, Wrapped};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};

/// A KMS that counts the wraps and unwraps asked of it, refused or not, as they are asked: each is
/// a call a real KMS would be made, and billed for.
pub struct Counted<K> {
    inner: K,
    wraps: AtomicU64,
    unwraps: AtomicU64,
}

impl<K: Kms> Counted<K> {
    pub fn new(inner: K) -> Self {
        Self { inner, wraps: AtomicU64::new(0), unwraps: AtomicU64::new(0) }
    }

    /// The KMS it counts the calls of, for what [`Kms`] has not: a fake's revoking, say.
    pub fn inner(&self) -> &K {
        &self.inner
    }

    /// The wraps asked of it since it was made, or [`Counted::reset`].
    pub fn wraps(&self) -> u64 {
        self.wraps.load(Ordering::Relaxed)
    }

    /// The unwraps asked of it since it was made, or [`Counted::reset`].
    pub fn unwraps(&self) -> u64 {
        self.unwraps.load(Ordering::Relaxed)
    }

    /// Counts from zero again: after a test's setup, say.
    pub fn reset(&self) {
        self.wraps.store(0, Ordering::Relaxed);
        self.unwraps.store(0, Ordering::Relaxed);
    }
}

impl<K: Kms> Kms for Counted<K> {
    fn wrap(
        &self,
        tenant: u64,
        key: &Secret,
    ) -> impl Future<Output = Result<Wrapped, Error>> + Send {
        self.wraps.fetch_add(1, Ordering::Relaxed);
        self.inner.wrap(tenant, key)
    }

    fn unwrap(
        &self,
        tenant: u64,
        wrapped: &Wrapped,
    ) -> impl Future<Output = Result<Secret, Error>> + Send {
        self.unwraps.fetch_add(1, Ordering::Relaxed);
        self.inner.unwrap(tenant, wrapped)
    }
}
