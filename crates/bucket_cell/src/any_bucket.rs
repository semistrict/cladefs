// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a bucket of any kind behind one type ([`AnyBucket`], board 14433): for what a host
//! picks at run time and passes around with no type parameter, as its fast WAL bucket (doc-29),
//! which is an object store's in production and the simulation's [`crate::testing::SimBucket`]
//! in the simulated world. Each request costs a box more than its bucket's own: nothing next to a
//! request over the network.

use crate::bucket::{Bucket, ByteRange, Error, Listing, Meta, Object, Page, PutMode, Version};
use bytes::Bytes;
use futures::future::BoxFuture;
use std::future::Future;
use std::sync::Arc;

/// [`Bucket`] with its futures boxed, so that it can be a trait object: implemented for every
/// bucket. Each takes what it asks for owned, so that its future borrows the bucket alone.
trait Erased: Send + Sync + 'static {
    fn get(&self, key: String) -> BoxFuture<'_, Result<Object, Error>>;
    fn get_if_changed(
        &self,
        key: String,
        if_none_match: Version,
    ) -> BoxFuture<'_, Result<Option<Object>, Error>>;
    fn get_range(&self, key: String, range: ByteRange) -> BoxFuture<'_, Result<Vec<u8>, Error>>;
    fn head(&self, key: String) -> BoxFuture<'_, Result<Meta, Error>>;
    fn list(&self, prefix: String) -> BoxFuture<'_, Result<Vec<Meta>, Error>>;
    fn list_delimited(
        &self,
        prefix: String,
        after: Option<String>,
        max: usize,
    ) -> BoxFuture<'_, Result<Listing, Error>>;
    fn put(
        &self,
        key: String,
        bytes: Bytes,
        mode: PutMode,
    ) -> BoxFuture<'_, Result<Version, Error>>;
    fn append(&self, key: String, offset: u64, bytes: Bytes) -> BoxFuture<'_, Result<u64, Error>>;
    fn delete(&self, key: String) -> BoxFuture<'_, Result<(), Error>>;
    fn delete_batch(&self, keys: Vec<String>) -> BoxFuture<'_, Result<(), Error>>;
    fn rewritable(&self, key: String) -> BoxFuture<'_, ()>;
    fn now(&self) -> u64;
    fn pause(&self, failures: u32) -> BoxFuture<'_, ()>;
    fn random_u64(&self) -> u64;
    fn ratchet(&self) -> &formats::Ratchet;
}

impl<B: Bucket> Erased for B {
    fn get(&self, key: String) -> BoxFuture<'_, Result<Object, Error>> {
        Box::pin(async move { Bucket::get(self, &key).await })
    }

    fn get_if_changed(
        &self,
        key: String,
        if_none_match: Version,
    ) -> BoxFuture<'_, Result<Option<Object>, Error>> {
        Box::pin(async move { Bucket::get_if_changed(self, &key, &if_none_match).await })
    }

    fn get_range(&self, key: String, range: ByteRange) -> BoxFuture<'_, Result<Vec<u8>, Error>> {
        Box::pin(async move { Bucket::get_range(self, &key, range).await })
    }

    fn head(&self, key: String) -> BoxFuture<'_, Result<Meta, Error>> {
        Box::pin(async move { Bucket::head(self, &key).await })
    }

    fn list(&self, prefix: String) -> BoxFuture<'_, Result<Vec<Meta>, Error>> {
        Box::pin(async move { Bucket::list(self, &prefix).await })
    }

    fn list_delimited(
        &self,
        prefix: String,
        after: Option<String>,
        max: usize,
    ) -> BoxFuture<'_, Result<Listing, Error>> {
        Box::pin(async move {
            let page = Page { after: after.as_deref(), max };
            Bucket::list_delimited(self, &prefix, page).await
        })
    }

    fn put(
        &self,
        key: String,
        bytes: Bytes,
        mode: PutMode,
    ) -> BoxFuture<'_, Result<Version, Error>> {
        Box::pin(async move { Bucket::put(self, &key, bytes, mode).await })
    }

    fn append(&self, key: String, offset: u64, bytes: Bytes) -> BoxFuture<'_, Result<u64, Error>> {
        Box::pin(async move { Bucket::append(self, &key, offset, bytes).await })
    }

    fn delete(&self, key: String) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move { Bucket::delete(self, &key).await })
    }

    fn delete_batch(&self, keys: Vec<String>) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
            Bucket::delete_batch(self, &keys).await
        })
    }

    fn rewritable(&self, key: String) -> BoxFuture<'_, ()> {
        Box::pin(async move { Bucket::rewritable(self, &key).await })
    }

    fn now(&self) -> u64 {
        Bucket::now(self)
    }

    fn pause(&self, failures: u32) -> BoxFuture<'_, ()> {
        Box::pin(Bucket::pause(self, failures))
    }

    fn random_u64(&self) -> u64 {
        Bucket::random_u64(self)
    }

    fn ratchet(&self) -> &formats::Ratchet {
        Bucket::ratchet(self)
    }
}

/// A bucket of any kind ([`AnyBucket::new`]), its requests its bucket's.
#[derive(Clone)]
pub struct AnyBucket(Arc<dyn Erased>);

impl AnyBucket {
    pub fn new(bucket: Arc<impl Bucket>) -> Self {
        Self(bucket)
    }
}

impl std::fmt::Debug for AnyBucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnyBucket").finish_non_exhaustive()
    }
}

impl Bucket for AnyBucket {
    fn get(&self, key: &str) -> impl Future<Output = Result<Object, Error>> + Send {
        self.0.get(key.to_owned())
    }

    fn get_if_changed(
        &self,
        key: &str,
        if_none_match: &Version,
    ) -> impl Future<Output = Result<Option<Object>, Error>> + Send {
        self.0.get_if_changed(key.to_owned(), if_none_match.clone())
    }

    fn get_range(
        &self,
        key: &str,
        range: ByteRange,
    ) -> impl Future<Output = Result<Vec<u8>, Error>> + Send {
        self.0.get_range(key.to_owned(), range)
    }

    fn head(&self, key: &str) -> impl Future<Output = Result<Meta, Error>> + Send {
        self.0.head(key.to_owned())
    }

    fn list(&self, prefix: &str) -> impl Future<Output = Result<Vec<Meta>, Error>> + Send {
        self.0.list(prefix.to_owned())
    }

    fn list_delimited(
        &self,
        prefix: &str,
        page: Page<'_>,
    ) -> impl Future<Output = Result<Listing, Error>> + Send {
        self.0.list_delimited(prefix.to_owned(), page.after.map(str::to_owned), page.max)
    }

    fn put(
        &self,
        key: &str,
        bytes: Bytes,
        mode: PutMode,
    ) -> impl Future<Output = Result<Version, Error>> + Send {
        self.0.put(key.to_owned(), bytes, mode)
    }

    fn append(
        &self,
        key: &str,
        offset: u64,
        bytes: Bytes,
    ) -> impl Future<Output = Result<u64, Error>> + Send {
        self.0.append(key.to_owned(), offset, bytes)
    }

    fn delete(&self, key: &str) -> impl Future<Output = Result<(), Error>> + Send {
        self.0.delete(key.to_owned())
    }

    fn delete_batch(&self, keys: &[&str]) -> impl Future<Output = Result<(), Error>> + Send {
        self.0.delete_batch(keys.iter().map(|&key| key.to_owned()).collect())
    }

    fn rewritable(&self, key: &str) -> impl Future<Output = ()> + Send {
        self.0.rewritable(key.to_owned())
    }

    fn now(&self) -> u64 {
        self.0.now()
    }

    fn pause(&self, failures: u32) -> impl Future<Output = ()> + Send {
        self.0.pause(failures)
    }

    fn random_u64(&self) -> u64 {
        self.0.random_u64()
    }

    fn ratchet(&self) -> &formats::Ratchet {
        self.0.ratchet()
    }
}
