// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What cells need of an object store: objects under keys, read whole, listed by prefix, and
//! written with conditions (S3's `If-None-Match` and `If-Match`, GCS's generation preconditions).
//!
//! The helpers below retry requests the backend failed, which may or may not have taken effect, so
//! that callers see only answers: a write that landed and then failed, retried into a failed
//! condition, is recognised by reading the object back.

use bytes::Bytes;
use std::future::Future;
use std::pin::Pin;

/// cladefs: target-conditional `Send` bound for [`Bucket`] implementations and their futures:
/// `Send` everywhere native multi-threaded executors run, relaxed on single-threaded `wasm32`
/// where JS `Promise` futures (`JsFuture`) are `!Send`.
#[cfg(not(target_arch = "wasm32"))]
pub trait BucketSend: Send {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send> BucketSend for T {}

#[cfg(target_arch = "wasm32")]
pub trait BucketSend {}
#[cfg(target_arch = "wasm32")]
impl<T> BucketSend for T {}

/// cladefs: target-conditional `Sync` bound for [`Bucket`] implementations.
#[cfg(not(target_arch = "wasm32"))]
pub trait BucketSync: Sync {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Sync> BucketSync for T {}

#[cfg(target_arch = "wasm32")]
pub trait BucketSync {}
#[cfg(target_arch = "wasm32")]
impl<T> BucketSync for T {}

/// Boxed future with [`BucketSend`] (equivalent to `futures::future::BoxFuture` on native targets,
/// `LocalBoxFuture` on `wasm32`).
#[cfg(not(target_arch = "wasm32"))]
pub type BucketFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
#[cfg(target_arch = "wasm32")]
pub type BucketFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// An object's version, which a conditional update names: S3 names it by its ETag, GCS by its
/// generation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Version {
    pub e_tag: Option<String>,
    pub version: Option<String>,
}

/// How a write treats an object already at its key.
#[derive(Clone, Debug)]
pub enum PutMode {
    /// Replaces it.
    Overwrite,
    /// Fails with [`Error::AlreadyExists`] if there is one.
    Create,
    /// Fails with [`Error::Precondition`] unless there is one, of this version.
    Update(Version),
}

/// What a backend says of an object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    pub key: String,
    pub version: Version,
    pub size: u64,
    /// When it was written, by the backend's clock ([`Bucket::now`]).
    pub modified: u64,
}

/// cladefs: one level of the keys under a prefix, or a page of it, as a listing by delimiter finds
/// it ([`Bucket::list_delimited`], TASK-395.5).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    /// The prefixes, each ending with `/`, of the keys holding a `/` past the prefix listed, once
    /// each, up to and with the first such `/`, in their order.
    pub prefixes: Vec<String>,
    /// The objects whose keys hold no `/` past it, in the order of their keys.
    pub objects: Vec<Meta>,
    /// Where the next page starts ([`Page::after`]), if this is not the last: what the bucket
    /// says, which means nothing to its caller.
    pub next: Option<String>,
}

impl Listing {
    /// cladefs: the page `page` of this listing of a whole level, for a bucket that lists a level
    /// whole whatever page is asked (files, memory, the simulation's): the prefixes and objects
    /// after `page.after` (here the key of the last of the page before), in the order of their
    /// keys among each other, `page.max` of them at most, and the last one's key as where the next
    /// page starts, if any are left.
    #[cfg(feature = "full")]
    pub(crate) fn page(self, page: Page<'_>) -> Self {
        assert!((1..=LIST_PAGE).contains(&page.max), "a page of 1 to {LIST_PAGE}: {}", page.max);
        let after = |key: &str| page.after.is_none_or(|after| key > after);
        let mut prefixes = self.prefixes.into_iter().filter(|key| after(key)).peekable();
        let mut objects = self.objects.into_iter().filter(|meta| after(&meta.key)).peekable();
        let mut listing = Listing::default();
        let mut last = None;
        for _ in 0..page.max {
            let prefix_first = match (prefixes.peek(), objects.peek()) {
                (None, None) => break,
                (Some(prefix), Some(meta)) => *prefix < meta.key,
                (prefix, _) => prefix.is_some(),
            };
            if prefix_first {
                let prefix = prefixes.next().unwrap();
                last = Some(prefix.clone());
                listing.prefixes.push(prefix);
            } else {
                let meta = objects.next().unwrap();
                last = Some(meta.key.clone());
                listing.objects.push(meta);
            }
        }
        if prefixes.peek().is_some() || objects.peek().is_some() {
            listing.next = last;
        }
        listing
    }
}

/// cladefs: which page of a level of keys a listing by delimiter lists ([`Bucket::list_delimited`],
/// TASK-395.5): a level of millions of keys, a tenant's cells, is listed a page at a time, none of
/// its pages held but the one listed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Page<'a> {
    /// Where the page before ended, as its listing said ([`Listing::next`]): none for the first.
    pub after: Option<&'a str>,
    /// The most prefixes and objects, together, it holds: at least 1, and at most [`LIST_PAGE`].
    pub max: usize,
}

impl Page<'_> {
    /// The first page, of as many as a request of S3's lists.
    pub const FIRST: Page<'static> = Page { after: None, max: LIST_PAGE };
}

/// cladefs: the most prefixes and objects a page of a listing by delimiter holds: as many as S3's
/// and GCS's ListObjects give a request.
pub const LIST_PAGE: usize = 1000;

/// cladefs: the requests a listing of `objects` objects takes where a request lists a page of
/// [`LIST_PAGE`] (S3, GCS): one at least, an empty listing's (TASK-455).
pub fn list_pages(objects: usize) -> u64 {
    objects.div_ceil(LIST_PAGE).max(1) as u64
}

/// cladefs: the most objects [`Bucket::delete_batch`] is given at once: as many as S3's
/// DeleteObjects takes (TASK-395.5).
pub const DELETE_BATCH: usize = 1000;

/// cladefs: how many deletes of a batch a bucket with no batch delete has in flight at once.
const DELETES_AT_ONCE: usize = 32;

/// cladefs: how many batches [`delete_all`] has in flight at once.
const BATCHES_AT_ONCE: usize = 4;

/// An object, read whole.
#[derive(Clone, Debug)]
pub struct Object {
    pub bytes: Vec<u8>,
    pub meta: Meta,
}

impl Object {
    /// cladefs: the object a write of `bytes` at `key` left, of `version`, as of `now`, the
    /// backend's clock: what a read of it would find, until another's write (TASK-449).
    pub(crate) fn written(key: &str, bytes: Vec<u8>, version: Version, now: u64) -> Self {
        let meta = Meta { key: key.to_owned(), version, size: bytes.len() as u64, modified: now };
        Self { bytes, meta }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("no such object")]
    NotFound,
    #[error("an object exists at the key")]
    AlreadyExists,
    #[error("the object is not of the version named")]
    Precondition,
    /// The request failed. It may or may not have taken effect.
    #[error("the store is unavailable")]
    Unavailable,
    /// cladefs: the backend does not do what the request asks, as a filesystem's backend an update
    /// of a version (TASK-699): an answer, never retried, where [`Error::Unavailable`] is none.
    #[error("the store does not do what the request asks")]
    Unsupported,
    /// cladefs: refused, whatever is tried again (board 5861): an answer, never retried, as
    /// [`Error::Unsupported`] is, where [`Error::Unavailable`] is none. Retried, a request the
    /// bucket refuses each time hung its caller for good. A probe, and so a bucket's opening and a
    /// tool's, fails on it at once (board 5736), as every request does.
    #[error("the store refuses the request: {0}")]
    Refused(Refusal),
    /// cladefs: the store's credentials expired (`ExpiredToken`, board 5736): refreshed by the
    /// backend's credential provider as its next request goes, a cheap one first ([`readmitted`]).
    #[error("the store's credentials expired")]
    Expired,
}

impl Error {
    /// cladefs: whether the request failed, the store giving no answer of what it holds: out of
    /// reach, refusing it, or its credentials expired.
    pub fn failed(self) -> bool {
        matches!(self, Self::Unavailable | Self::Refused(_) | Self::Expired)
    }
}

/// cladefs: why a bucket refuses a request for good ([`Error::Refused`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// Its credentials: none, or none it lets in (401, 403).
    #[error("its credentials are not let in")]
    Denied,
    /// The request itself: a key past the backend's limit, a request it takes as malformed (400
    /// and the like).
    #[error("it refuses the request itself")]
    Request,
    /// cladefs: its answer, one this build can't read: an identity corrupt, or of a format of
    /// another build's (board 5534). Asking again reads the same.
    #[error("its answer is not one this build reads")]
    Unreadable,
}

/// cladefs: a request of `key` that the bucket refuses for good ([`Error::Refused`]): what a call
/// that retries its requests while they fail returns rather than retrying it (board 5861).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{key}: {refusal}")]
pub struct Refused {
    pub key: String,
    pub refusal: Refusal,
}

impl Refused {
    pub(crate) fn of(key: &str, refusal: Refusal) -> Self {
        Self { key: key.to_owned(), refusal }
    }
}

impl From<Refused> for crate::Error {
    fn from(refused: Refused) -> Self {
        Self::Refused(refused)
    }
}

/// Which bytes of an object a ranged read reads: an object shorter than the range reads short.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ByteRange {
    Bounded(std::ops::Range<u64>),
    /// Its last so many bytes.
    Suffix(u64),
}

impl ByteRange {
    /// Whether it begins within an object of `size` bytes: a read of it reads some of the object.
    /// Backends refuse one that does not.
    pub(crate) fn begins_within(&self, size: u64) -> bool {
        match self {
            Self::Bounded(range) => range.start < size,
            Self::Suffix(len) => *len > 0 && size > 0,
        }
    }
}

/// A bucket of objects, or a prefix of one.
pub trait Bucket: BucketSend + BucketSync + 'static {
    fn get(&self, key: &str) -> impl Future<Output = Result<Object, Error>> + BucketSend;

    /// cladefs: reads the object at `key` only if its version differs from `if_none_match`
    /// (`If-None-Match` HTTP conditional GET). Returns `Ok(None)` when unchanged (304 Not
    /// Modified), `Ok(Some(object))` when changed, or `Err(Error::NotFound)` when missing.
    fn get_if_changed(
        &self,
        key: &str,
        if_none_match: &Version,
    ) -> impl Future<Output = Result<Option<Object>, Error>> + BucketSend {
        async move {
            let object = self.get(key).await?;
            if object.meta.version == *if_none_match {
                return Ok(None);
            }
            Ok(Some(object))
        }
    }

    fn get_range(
        &self,
        key: &str,
        range: ByteRange,
    ) -> impl Future<Output = Result<Vec<u8>, Error>> + BucketSend;

    fn head(&self, key: &str) -> impl Future<Output = Result<Meta, Error>> + BucketSend;

    /// The objects whose keys start with `prefix`, in the order of their keys.
    fn list(&self, prefix: &str) -> impl Future<Output = Result<Vec<Meta>, Error>> + BucketSend;

    /// cladefs: a page of one level of the keys under `prefix`, empty or ending with `/`, by
    /// delimiter `/` (S3's and GCS's `delimiter=/`, TASK-395.5): the objects right under it, and
    /// the prefixes of the rest, listing none of the objects under those; of them, `page.max` at
    /// most, after where `page` says, in the order of their keys, and where the next page starts,
    /// if any are left. Every bucket lists so: none falls back to listing every object under
    /// `prefix`, which costs a request a page of them. Pages are read one after another, as S3's
    /// are: a key created or deleted meanwhile is listed, or not, as the pages before it went.
    fn list_delimited(
        &self,
        prefix: &str,
        page: Page<'_>,
    ) -> impl Future<Output = Result<Listing, Error>> + BucketSend;

    fn put(
        &self,
        key: &str,
        bytes: Bytes,
        mode: PutMode,
    ) -> impl Future<Output = Result<Version, Error>> + BucketSend;

    /// cladefs: appends `bytes` to `key` at exact byte `offset` (doc-29): `offset == 0` creates
    /// the object if absent (failing with [`Error::Precondition`] or [`Error::AlreadyExists`] if
    /// present), and `offset > 0` appends at `offset` (`x-amz-write-offset-bytes` on S3 Express
    /// One Zone, `x-goog-write-offset` on GCS Rapid Storage), failing with [`Error::Precondition`]
    /// if the object is absent, its current length differs from `offset`, or the append-part limit
    /// was reached. Returns the new object size in bytes. Backends that cannot enforce offset
    /// preconditions return [`Error::Unsupported`].
    fn append(
        &self,
        key: &str,
        offset: u64,
        bytes: Bytes,
    ) -> impl Future<Output = Result<u64, Error>> + Send {
        let _ = (key, offset, bytes);
        std::future::ready(Err(Error::Unsupported))
    }

    /// Deletes the object at `key`. Deleting none is no error.
    fn delete(&self, key: &str) -> impl Future<Output = Result<(), Error>> + BucketSend;

    /// cladefs: deletes the objects at `keys`, at most [`DELETE_BATCH`] of them (TASK-395.5): in
    /// one request where the backend deletes many at once (S3's DeleteObjects), else, as here, one
    /// each, [`DELETES_AT_ONCE`] at once. Deleting none is no error. It fails if a delete did,
    /// which may or may not have taken effect, nor need the others have.
    fn delete_batch(&self, keys: &[&str]) -> impl Future<Output = Result<(), Error>> + BucketSend {
        use futures::StreamExt as _;
        futures::stream::iter(keys)
            .map(|key| self.delete(key))
            .buffer_unordered(DELETES_AT_ONCE)
            .fold(Ok(()), |result, deleted| std::future::ready(result.and(deleted)))
    }

    /// cladefs: waits until the backend takes another write of `key` (TASK-690): at once where it
    /// takes a key's writes as fast as they come. GCS takes about one a second of an object and
    /// refuses more (`429`), so its bucket waits out the second since it last wrote the key, as
    /// each write of the key does first anyway. For a writer to gather, until then, all its next
    /// write of the key is to say: a cell's head, its one key written again and again.
    fn rewritable(&self, key: &str) -> impl Future<Output = ()> + BucketSend {
        let _ = key;
        std::future::ready(())
    }

    /// The backend's clock, which [`Meta::modified`] is in, in milliseconds since the Unix epoch,
    /// never the host's own (board 8467): its time now, at least, and never ahead of it, so that a
    /// window measured from an object's date by it closes late, never early, and a lease taken by
    /// it ends by the backend's time for every host alike ([`crate::clock`]). A backend over
    /// HTTP's, as its responses date them; of files and memory, the host's, which they date
    /// objects by.
    fn now(&self) -> u64;

    /// Waits before a request that failed `failures` times in a row goes again.
    fn pause(&self, failures: u32) -> impl Future<Output = ()> + BucketSend {
        let _ = failures;
        std::future::ready(())
    }

    /// cladefs: the checkpoints' states the recoveries reading through it decoded, which they
    /// share, if it keeps them: a host's bucket does, so that the forks of a base it opens decode
    /// the checkpoint they start from once (TASK-92.55).
    fn decoded(&self) -> Option<&crate::recovery::Decoded> {
        None
    }

    /// cladefs: the mirrors in this bucket of versions of other buckets' cells, as forks into it
    /// see them ([`crate::mirror::Here`], TASK-633.8), if it routes requests to other buckets: a
    /// host's routed bucket does, and the simulation's of many.
    fn mirrors(&self) -> Option<&dyn crate::mirror::Here> {
        None
    }

    /// cladefs: draws a 64-bit random value (board 12982): production buckets use the process
    /// RNG; `SimBucket` overrides this deterministically so simulation seeds replay identically.
    /// The keyless build (`gc_wasm`) draws none, and has no `rand` (board 13575).
    #[cfg(feature = "full")]
    fn random_u64(&self) -> u64 {
        // Outside the simulation: `SimBucket` overrides this with a deterministic PRNG.
        rand::random()
    }

    /// cladefs: the numbers of every bucket an ephemeral cell's reference may live in, including
    /// any newly recorded in the root bucket's authoritative registry (board 9966, hotbed question
    /// 543). `None` for an unrouted bucket, whose caller reads its own root directly
    /// ([`crate::lifecycle::ephemeral_buckets`]).
    #[cfg(feature = "full")]
    fn ephemeral_buckets(
        &self,
    ) -> Option<BucketFuture<'_, Result<std::collections::BTreeSet<u16>, crate::Error>>> {
        None
    }

    /// cladefs: reads [`crate::layout::EVENTS`] from the authoritative root bucket (#1 when
    /// routed, else this bucket; board 11663). `None` for an unrouted bucket, whose caller reads
    /// [`crate::layout::EVENTS`] directly from `self` (preserving board 5532 local dry-runs).
    #[cfg(feature = "full")]
    fn events_object(&self) -> Option<BucketFuture<'_, Result<Option<Object>, crate::Error>>> {
        None
    }

    /// cladefs: the active storage-version ratchet for writes through this bucket (doc-28).
    /// Defaults to `Format::current` for standalone buckets without a host ratchet.
    fn ratchet(&self) -> &formats::Ratchet {
        formats::Ratchet::default_current()
    }
}

/// The object at `key`, if any, its reads retried while they fail; refused, [`Refused`].
pub async fn get(bucket: &impl Bucket, key: &str) -> Result<Option<Object>, Refused> {
    let mut failures = 0;
    loop {
        match bucket.get(key).await {
            Ok(object) => return Ok(Some(object)),
            Err(Error::NotFound) => return Ok(None),
            Err(error) => failed(bucket, &mut failures, error, key).await?,
        }
    }
}

/// cladefs: what [`get_if_changed`] found at a key.
#[derive(Clone, Debug)]
pub enum IfChanged {
    /// The object is present and differs from the caller's last-seen version (or none was given).
    Changed(Object),
    /// The object's version matches `if_none_match` (304 Not Modified).
    Unchanged,
    /// No object exists at the key.
    Missing,
}

/// cladefs: reads `key` only if its version differs from `if_none_match`, retrying transient
/// failures like [`get`].
pub async fn get_if_changed(
    bucket: &impl Bucket,
    key: &str,
    if_none_match: Option<&Version>,
) -> Result<IfChanged, Refused> {
    let Some(version) = if_none_match else {
        return Ok(match get(bucket, key).await? {
            Some(object) => IfChanged::Changed(object),
            None => IfChanged::Missing,
        });
    };
    let mut failures = 0;
    loop {
        match bucket.get_if_changed(key, version).await {
            Ok(Some(object)) => return Ok(IfChanged::Changed(object)),
            Ok(None) => return Ok(IfChanged::Unchanged),
            Err(Error::NotFound) => return Ok(IfChanged::Missing),
            Err(error) => failed(bucket, &mut failures, error, key).await?,
        }
    }
}

/// Counts a failure, `error`, of a request of `key`, and waits before the request goes again:
/// credentials expired, until the store takes a cheap request of `key` again ([`readmitted`]).
/// A refusal ends the request (board 5861): the one a call met it found is its caller's to answer.
pub(crate) async fn failed(
    bucket: &impl Bucket,
    failures: &mut u32,
    error: Error,
    key: &str,
) -> Result<(), Refused> {
    *failures += 1;
    match error {
        Error::Refused(refusal) => Err(Refused::of(key, refusal)),
        Error::Expired => readmitted(bucket, key).await,
        _ => {
            bucket.pause(*failures).await;
            Ok(())
        }
    }
}

/// cladefs: waits, a request of `key` finding the store's credentials expired, until the store
/// answers a cheap one of `key` again, a HEAD, which refreshes them as it goes: never sending the
/// request's payload again meanwhile, a segment's 32 MiB each time (board 5736). Refused, it
/// ends, [`Refused`].
async fn readmitted(bucket: &impl Bucket, key: &str) -> Result<(), Refused> {
    let mut checks = 0;
    loop {
        checks += 1;
        bucket.pause(checks).await;
        match bucket.head(key).await {
            Ok(_) | Err(Error::NotFound) => return Ok(()),
            Err(Error::Refused(refusal)) => return Err(Refused::of(key, refusal)),
            // Out of reach, or the credentials expired still: it waits on.
            Err(_) => {}
        }
    }
}

/// A ranged read of an object that holds none of its range, which begins past the object's end
/// ([`get_range`]): an object not as its reader took it to be.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{key}: {range:?} past its end, at {size} bytes")]
pub(crate) struct Past {
    key: String,
    range: ByteRange,
    size: u64,
}

impl From<Past> for crate::Error {
    fn from(past: Past) -> Self {
        Self::Corrupt(past.to_string())
    }
}

/// cladefs: what a ranged read finds instead of its bytes: its range [`Past`] the object's end,
/// or the read [`Refused`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Unread {
    #[error(transparent)]
    Past(#[from] Past),
    #[error(transparent)]
    Refused(#[from] Refused),
}

impl From<Unread> for crate::Error {
    fn from(unread: Unread) -> Self {
        match unread {
            Unread::Past(past) => past.into(),
            Unread::Refused(refused) => refused.into(),
        }
    }
}

/// The bytes of `range` of the object at `key`, if there is one; of a range of no bytes, none,
/// asking nothing. `object_store` refuses a range that begins past the object's end as it fails
/// when the backend is out of reach (`Generic`, [`Error::Unavailable`] here): a read that fails
/// goes again only once the object's size shows the range begins within it, and is [`Past`] its end
/// otherwise.
pub(crate) async fn get_range(
    bucket: &impl Bucket,
    key: &str,
    range: ByteRange,
) -> Result<Option<Vec<u8>>, Unread> {
    if let ByteRange::Bounded(bounded) = &range {
        assert!(bounded.start <= bounded.end, "{key}: {bounded:?}, reversed");
        if bounded.is_empty() {
            return Ok(Some(Vec::new()));
        }
    }
    let mut failures = 0;
    loop {
        match bucket.get_range(key, range.clone()).await {
            Ok(bytes) => return Ok(Some(bytes)),
            Err(Error::NotFound) => return Ok(None),
            Err(Error::Refused(refusal)) => return Err(Refused::of(key, refusal).into()),
            // Failed: a HEAD tells a range past the object's end, which reads none, from a failure.
            Err(_) => match bucket.head(key).await {
                Err(Error::NotFound) => return Ok(None),
                Err(Error::Refused(refusal)) => return Err(Refused::of(key, refusal).into()),
                Ok(meta) if !range.begins_within(meta.size) => {
                    return Err(Past { key: key.to_owned(), range, size: meta.size }.into());
                }
                _ => failed(bucket, &mut failures, Error::Unavailable, key).await?,
            },
        }
    }
}

/// cladefs: what `objects`, a listing of a prefix holding `prefix`, by their keys, holds under
/// `prefix`, in the order of their keys: as a listing of `prefix` alone, made when that one was,
/// would list it (TASK-395.16).
pub(crate) fn under(objects: &std::collections::BTreeMap<String, Meta>, prefix: &str) -> Vec<Meta> {
    use std::ops::Bound;
    (objects.range::<str, _>((Bound::Included(prefix), Bound::Unbounded)))
        .take_while(|(key, _)| key.starts_with(prefix))
        .map(|(_, meta)| meta.clone())
        .collect()
}

pub async fn list(bucket: &impl Bucket, prefix: &str) -> Result<Vec<Meta>, Refused> {
    let mut failures = 0;
    loop {
        match bucket.list(prefix).await {
            Ok(list) => return Ok(list),
            Err(error) => failed(bucket, &mut failures, error, prefix).await?,
        }
    }
}

/// cladefs: the page `page` of the listing of `prefix` by delimiter ([`Bucket::list_delimited`]),
/// retried while it fails.
pub(crate) async fn list_delimited_page(
    bucket: &impl Bucket,
    prefix: &str,
    page: Page<'_>,
) -> Result<Listing, Refused> {
    let mut failures = 0;
    loop {
        match bucket.list_delimited(prefix, page).await {
            Ok(listing) => return Ok(listing),
            Err(error) => failed(bucket, &mut failures, error, prefix).await?,
        }
    }
}

/// cladefs: the prefixes of the listing of `prefix` by delimiter ([`Bucket::list_delimited`]), in
/// their order, a page at a time, each page listed once those before it are taken, and retried
/// while it fails: so that a level of millions, a tenant's cells, is held no more than a page of it
/// at a time (TASK-395.5, TASK-395.8). A request for every [`LIST_PAGE`] keys of the level.
/// cladefs: a page refused, the stream's last item, [`Refused`] (board 5861).
pub(crate) fn prefixes<B: Bucket>(
    bucket: &B,
    prefix: String,
) -> impl futures::Stream<Item = Result<String, Refused>> + BucketSend + '_ {
    use futures::StreamExt as _;
    // The page after the one listed last: none once the last was, or one was refused.
    let first = Some(None::<String>);
    futures::stream::unfold(first, move |after| {
        let prefix = prefix.clone();
        async move {
            let after = after?;
            let page = Page { after: after.as_deref(), max: LIST_PAGE };
            match list_delimited_page(bucket, &prefix, page).await {
                Ok(Listing { prefixes, next, .. }) => {
                    let prefixes: Vec<_> = prefixes.into_iter().map(Ok).collect();
                    Some((futures::stream::iter(prefixes), next.map(Some)))
                }
                Err(refused) => Some((futures::stream::iter(vec![Err(refused)]), None)),
            }
        }
    })
    .flatten()
}

/// cladefs: the whole listing of `prefix` by delimiter, its pages one after another, each retried
/// while it fails: for a level of a cell's, whose keys are few (its parents with references, a
/// parent's origins), never one of every cell's.
pub(crate) async fn list_delimited(bucket: &impl Bucket, prefix: &str) -> Result<Listing, Refused> {
    let mut whole = Listing::default();
    let mut after = None;
    loop {
        let page = Page { after: after.as_deref(), max: LIST_PAGE };
        let Listing { prefixes, objects, next } = list_delimited_page(bucket, prefix, page).await?;
        whole.prefixes.extend(prefixes);
        whole.objects.extend(objects);
        match next {
            Some(next) => after = Some(next),
            None => return Ok(whole),
        }
    }
}

/// cladefs: deletes the objects at `keys`, in batches of [`DELETE_BATCH`]
/// ([`Bucket::delete_batch`]), [`BATCHES_AT_ONCE`] at once, in any order, each batch retried whole
/// while it fails: deleting what is gone is no error (TASK-395.5).
/// cladefs: a batch refused, [`Refused`], the others deleted all the same (board 5861).
pub(crate) async fn delete_all(bucket: &impl Bucket, keys: &[&str]) -> Result<(), Refused> {
    use futures::StreamExt as _;
    // The batches' futures made first: a stream mapping them in holds the closure in its type,
    // which a future awaiting this one is then not seen to be `Send` with.
    let batches: Vec<_> =
        keys.chunks(DELETE_BATCH).map(|batch| delete_batch(bucket, batch)).collect();
    let deleted: Vec<Result<(), Refused>> =
        futures::stream::iter(batches).buffer_unordered(BATCHES_AT_ONCE).collect().await;
    deleted.into_iter().collect()
}

/// cladefs: deletes the objects at `keys` in one batch ([`Bucket::delete_batch`]), retried whole
/// while it fails. A function of its own, not a closure's future, so that a future awaiting
/// [`delete_all`] is seen to be `Send` (TASK-395.15: a sweep spawning its passes).
async fn delete_batch(bucket: &impl Bucket, keys: &[&str]) -> Result<(), Refused> {
    let mut failures = 0;
    let key = keys.first().copied().unwrap_or_default();
    loop {
        match bucket.delete_batch(keys).await {
            Ok(()) => return Ok(()),
            Err(error) => failed(bucket, &mut failures, error, key).await?,
        }
    }
}

/// cladefs: deletes the object at `key`, retried while it fails: deleting what is gone is no error.
/// One key's, with no stream of batches, so that a writer's future, which the uploader's runtime
/// needs `Send`, may await it (TASK-395.10).
pub(crate) async fn delete(bucket: &impl Bucket, key: &str) -> Result<(), Refused> {
    let mut failures = 0;
    loop {
        match bucket.delete(key).await {
            Ok(()) => return Ok(()),
            Err(error) => failed(bucket, &mut failures, error, key).await?,
        }
    }
}

/// What [`create`] found.
pub(crate) enum Created {
    /// The object holds what was written, of this version: this call wrote it, or one before it
    /// wrote the same.
    Created(Version),
    /// Another object was there.
    Exists(Object),
}

/// Writes `bytes` at `key` unless an object is there: held once, however often it is tried
/// (TASK-317).
pub(crate) async fn create(
    bucket: &impl Bucket,
    key: &str,
    bytes: Vec<u8>,
) -> Result<Created, Refused> {
    let bytes = Bytes::from(bytes);
    let mut failures = 0;
    loop {
        match bucket.put(key, bytes.clone(), PutMode::Create).await {
            Ok(version) => return Ok(Created::Created(version)),
            // Perhaps this one's own write, which landed though its answer was lost; deleted
            // since, tried again.
            Err(Error::AlreadyExists) => {
                if let Some(found) = landed(bucket, key, &bytes).await? {
                    return Ok(found);
                }
            }
            Err(error) => failed(bucket, &mut failures, error, key).await?,
        }
    }
}

/// cladefs: what is at `key`, where a create of `bytes` found an object (board 5738): this write,
/// which landed though its answer was lost, or another's; none if it went since. A frame's is told
/// by its size and its trailer, which holds a salt drawn at random for that framing and the MACs of
/// its footer and table ([`extent_frame::trailer_of`]): a HEAD and a read of its last few hundred
/// bytes, where a segment's whole read cost 32 MiB, and the memory for them. Anything else, and
/// another's object, is read whole.
async fn landed(
    bucket: &impl Bucket,
    key: &str,
    bytes: &Bytes,
) -> Result<Option<Created>, Refused> {
    if let Some(trailer) = extent_frame::trailer_of(bytes) {
        let Some(meta) = head(bucket, key).await? else { return Ok(None) };
        if meta.size == bytes.len() as u64 {
            let suffix = ByteRange::Suffix(trailer.len() as u64);
            match get_range(bucket, key, suffix).await {
                Ok(Some(tail)) if tail == trailer => {
                    return Ok(Some(Created::Created(meta.version)));
                }
                Ok(None) => return Ok(None),
                Err(Unread::Refused(refused)) => return Err(refused),
                Ok(Some(_)) | Err(Unread::Past(_)) => {}
            }
        }
    }
    Ok(get(bucket, key).await?.map(|object| match object {
        object if *bytes == object.bytes => Created::Created(object.meta.version),
        object => Created::Exists(object),
    }))
}

/// cladefs: what the backend says of the object at `key`, if any, its HEADs retried while they
/// fail; refused, [`Refused`].
pub async fn head(bucket: &impl Bucket, key: &str) -> Result<Option<Meta>, Refused> {
    let mut failures = 0;
    loop {
        match bucket.head(key).await {
            Ok(meta) => return Ok(Some(meta)),
            Err(Error::NotFound) => return Ok(None),
            Err(error) => failed(bucket, &mut failures, error, key).await?,
        }
    }
}

/// What [`update`] found.
pub(crate) enum Updated {
    /// The object holds what was written, of this version.
    Updated(Version),
    /// The object changed since `version`.
    Changed,
}

/// Writes `bytes` at `key` if the object there is of `version`. `bytes` must differ from every
/// other writer's, so that a write that landed though its answer was lost is recognised.
pub(crate) async fn update(
    bucket: &impl Bucket,
    key: &str,
    bytes: Vec<u8>,
    version: &Version,
) -> Result<Updated, Refused> {
    let bytes = Bytes::from(bytes);
    let mut failures = 0;
    loop {
        match bucket.put(key, bytes.clone(), PutMode::Update(version.clone())).await {
            Ok(version) => return Ok(Updated::Updated(version)),
            Err(Error::Precondition | Error::NotFound) => {
                return Ok(match get(bucket, key).await? {
                    Some(object) if bytes == object.bytes => Updated::Updated(object.meta.version),
                    _ => Updated::Changed,
                });
            }
            Err(error) => failed(bucket, &mut failures, error, key).await?,
        }
    }
}

/// What [`append`] found (doc-29).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Appended {
    /// The append succeeded (or had already landed when retried after a lost response): the new
    /// object size in bytes is `offset + bytes.len()`.
    Appended(u64),
    /// The backend rejected the offset precondition (object already exists at offset 0, current
    /// length != offset, object missing at offset > 0, or part limit reached).
    Precondition,
}

/// cladefs: appends `bytes` to `key` at exact byte `offset` (doc-29), retrying transient failures
/// ([`Error::Unavailable`], [`Error::Expired`]). Never falls back to an unconditioned write:
/// [`Error::Unsupported`] is returned as [`Refusal::Request`].
pub async fn append(
    bucket: &impl Bucket,
    key: &str,
    offset: u64,
    bytes: Bytes,
) -> Result<Appended, Refused> {
    let mut failures = 0;
    loop {
        match bucket.append(key, offset, bytes.clone()).await {
            Ok(new_len) => return Ok(Appended::Appended(new_len)),
            Err(Error::Precondition | Error::AlreadyExists) => return Ok(Appended::Precondition),
            Err(Error::Unsupported) => return Err(Refused::of(key, Refusal::Request)),
            Err(error) => failed(bucket, &mut failures, error, key).await?,
        }
    }
}

/// cladefs: overwrites `key` with `bytes`, retrying transient failures (doc-29 tombstones,
/// [`crate::head`]).
pub async fn overwrite(bucket: &impl Bucket, key: &str, bytes: Bytes) -> Result<Version, Refused> {
    let mut failures = 0;
    loop {
        match bucket.put(key, bytes.clone(), PutMode::Overwrite).await {
            Ok(version) => return Ok(version),
            Err(error) => failed(bucket, &mut failures, error, key).await?,
        }
    }
}

/// cladefs: what the key of every probe's own object begins with
/// ([`crate::object_bucket::probe`], [`probe_append`]).
pub const PROBES: &str = "probe/";

/// cladefs: how long a probe's object stands, by its bucket's time, in milliseconds, before a
/// sweep takes it for left behind ([`reap_probes`]): an hour. A probe ends within seconds, or
/// begins again with an object of its own anew.
pub const PROBE_KEPT: u64 = 60 * 60 * 1000;

/// cladefs: whether `key` is a probe's own object's: under [`PROBES`], its nonce alone, as
/// `base32_id::Padded` spells it, an append's probe's after `append-`.
pub fn probe(key: &str) -> bool {
    let Some(name) = key.strip_prefix(PROBES) else { return false };
    let nonce = name.strip_prefix("append-").unwrap_or(name);
    !nonce.is_empty() && nonce.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

/// cladefs: whether the object `meta` lists is a probe's left behind, as of `now` by its bucket's
/// time ([`PROBE_KEPT`]).
pub fn probe_left(meta: &Meta, now: u64) -> bool {
    probe(&meta.key) && meta.modified.saturating_add(PROBE_KEPT) < now
}

/// cladefs: deletes the probes' objects left behind in `bucket` (board 14544): how many. A probe
/// removes its own object as it ends, but one stopped short, its host killed, or its bucket out of
/// reach as it removed it, leaves it there, where no cell names it and no pass over a cell lists
/// it: for good, but for this, which `cladefs-gc` does after a sweep of every cell.
pub async fn reap_probes(bucket: &impl Bucket) -> Result<usize, Refused> {
    let listed = list(bucket, PROBES).await?;
    // Read once the bucket answered, as its clock reads none before.
    let now = bucket.now();
    let left: Vec<&str> = (listed.iter())
        .filter(|meta| probe_left(meta, now))
        .map(|meta| meta.key.as_str())
        .collect();
    delete_all(bucket, &left).await?;
    Ok(left.len())
}

/// cladefs: verifies that `bucket` enforces offset-checked appends (`Bucket::append`) before a
/// daemon relies on it as a fast WAL store (doc-29). Fails hard if the backend rejects appends as
/// unsupported or ignores offset preconditions (no silent fallback).
pub async fn probe_append(bucket: &impl Bucket, nonce: u64) -> Result<(), String> {
    let mut failures = 0u32;
    loop {
        let key =
            format!("{PROBES}append-{}", base32_id::Padded(nonce.wrapping_add(failures.into())));
        let result = probe_append_once(bucket, &key).await;
        // Best-effort cleanup of probe object regardless of outcome.
        let _ = bucket.delete(&key).await;
        match result {
            Ok(()) => return Ok(()),
            Err(ProbeAppendErr::Transient) => {
                failures = failures.saturating_add(1);
                if failures >= 8 {
                    return Err(format!(
                        "fast WAL bucket probe at {key} failed after {failures} transient errors"
                    ));
                }
                bucket.pause(failures).await;
            }
            Err(ProbeAppendErr::Fatal(why)) => return Err(why),
        }
    }
}

enum ProbeAppendErr {
    Transient,
    Fatal(String),
}

async fn probe_append_once(bucket: &impl Bucket, key: &str) -> Result<(), ProbeAppendErr> {
    let classify = |step: &str, err: Error| match err {
        Error::Unavailable | Error::Expired => ProbeAppendErr::Transient,
        other => ProbeAppendErr::Fatal(format!("fast WAL probe ({step}) at {key}: {other}")),
    };
    let len = bucket
        .append(key, 0, Bytes::from_static(b"hello"))
        .await
        .map_err(|err| classify("create at offset 0", err))?;
    if len != 5 {
        return Err(ProbeAppendErr::Fatal(format!(
            "fast WAL probe at {key}: create at offset 0 returned len {len}, expected 5"
        )));
    }
    match bucket.append(key, 0, Bytes::from_static(b"again")).await {
        Err(Error::Precondition | Error::AlreadyExists) => {}
        Err(err) => return Err(classify("duplicate create at offset 0", err)),
        Ok(got) => {
            return Err(ProbeAppendErr::Fatal(format!(
                "fast WAL bucket does not enforce offset-0 exclusivity at {key}: got len {got}"
            )));
        }
    }
    for wrong in [3u64, 99u64] {
        match bucket.append(key, wrong, Bytes::from_static(b"bad")).await {
            Err(Error::Precondition) => {}
            Err(err) => return Err(classify("wrong-offset append", err)),
            Ok(got) => {
                return Err(ProbeAppendErr::Fatal(format!(
                    "fast WAL bucket ignored write offset {wrong} at {key}: got len {got}"
                )));
            }
        }
    }
    let len = bucket
        .append(key, 5, Bytes::from_static(b" world"))
        .await
        .map_err(|err| classify("append at offset 5", err))?;
    if len != 11 {
        return Err(ProbeAppendErr::Fatal(format!(
            "fast WAL probe at {key}: append at offset 5 returned len {len}, expected 11"
        )));
    }
    let got = bucket.get(key).await.map_err(|err| classify("read back", err))?;
    if got.bytes != b"hello world" {
        return Err(ProbeAppendErr::Fatal(format!(
            "fast WAL probe at {key}: read back {:?}, expected b\"hello world\"",
            got.bytes
        )));
    }
    // Overwrite with 16-byte tombstone and verify stale offset 11 is rejected.
    bucket
        .put(key, Bytes::from_static(b"CLADEFS_FENCED!\n"), PutMode::Overwrite)
        .await
        .map_err(|err| classify("tombstone overwrite", err))?;
    match bucket.append(key, 11, Bytes::from_static(b"zombie")).await {
        Err(Error::Precondition) => {}
        Err(err) => return Err(classify("stale append after tombstone", err)),
        Ok(got) => {
            return Err(ProbeAppendErr::Fatal(format!(
                "fast WAL bucket accepted stale append after tombstone at {key}: got len {got}"
            )));
        }
    }
    Ok(())
}

/// cladefs: what every bucket must do, which cells rely on (TASK-165.1), with objects under
/// `root`, which it leaves as it found them: a bucket in memory, on the host's filesystem, the
/// simulation's, and S3's and Azure's servers alike run it.
/// Delegates pass-through [`Bucket`] methods to `self.$inner` for test wrapper buckets that
/// intercept only `get` (`except_get`), or every read of an object's bytes, whole and ranged
/// (`except_reads`: `get` and `get_range`), as a recovery may read a checkpoint either way.
#[cfg(test)]
macro_rules! delegate_bucket_methods {
    (except_get $inner:tt) => {
        fn get_range(
            &self,
            key: &str,
            range: crate::bucket::ByteRange,
        ) -> impl std::future::Future<Output = Result<Vec<u8>, crate::bucket::Error>> + Send {
            self.$inner.get_range(key, range)
        }

        crate::bucket::delegate_bucket_methods!(except_reads $inner);
    };
    (except_reads $inner:tt) => {
        fn head(
            &self,
            key: &str,
        ) -> impl std::future::Future<Output = Result<crate::bucket::Meta, crate::bucket::Error>> + Send {
            self.$inner.head(key)
        }

        fn list(
            &self,
            prefix: &str,
        ) -> impl std::future::Future<Output = Result<Vec<crate::bucket::Meta>, crate::bucket::Error>> + Send {
            self.$inner.list(prefix)
        }

        fn list_delimited(
            &self,
            prefix: &str,
            page: crate::bucket::Page<'_>,
        ) -> impl std::future::Future<Output = Result<crate::bucket::Listing, crate::bucket::Error>> + Send {
            self.$inner.list_delimited(prefix, page)
        }

        fn put(
            &self,
            key: &str,
            bytes: bytes::Bytes,
            mode: crate::bucket::PutMode,
        ) -> impl std::future::Future<Output = Result<crate::bucket::Version, crate::bucket::Error>> + Send {
            self.$inner.put(key, bytes, mode)
        }

        fn delete(
            &self,
            key: &str,
        ) -> impl std::future::Future<Output = Result<(), crate::bucket::Error>> + Send {
            self.$inner.delete(key)
        }

        fn now(&self) -> u64 {
            self.$inner.now()
        }
    };
    (except_put $inner:tt) => {
        fn get(
            &self,
            key: &str,
        ) -> impl std::future::Future<Output = Result<crate::bucket::Object, crate::bucket::Error>> + Send {
            self.$inner.get(key)
        }

        fn get_range(
            &self,
            key: &str,
            range: crate::bucket::ByteRange,
        ) -> impl std::future::Future<Output = Result<Vec<u8>, crate::bucket::Error>> + Send {
            self.$inner.get_range(key, range)
        }

        fn head(
            &self,
            key: &str,
        ) -> impl std::future::Future<Output = Result<crate::bucket::Meta, crate::bucket::Error>> + Send {
            self.$inner.head(key)
        }

        fn list(
            &self,
            prefix: &str,
        ) -> impl std::future::Future<Output = Result<Vec<crate::bucket::Meta>, crate::bucket::Error>> + Send {
            self.$inner.list(prefix)
        }

        fn list_delimited(
            &self,
            prefix: &str,
            page: crate::bucket::Page<'_>,
        ) -> impl std::future::Future<Output = Result<crate::bucket::Listing, crate::bucket::Error>> + Send {
            self.$inner.list_delimited(prefix, page)
        }

        fn delete(
            &self,
            key: &str,
        ) -> impl std::future::Future<Output = Result<(), crate::bucket::Error>> + Send {
            self.$inner.delete(key)
        }

        fn now(&self) -> u64 {
            self.$inner.now()
        }
    };
}

#[cfg(test)]
pub(crate) use delegate_bucket_methods;

#[cfg(all(test, feature = "full"))]
pub(crate) async fn conformance(bucket: &impl Bucket, root: &str) {
    let key = &format!("{root}/object");
    // A version no object has: GCS's generation 0 is none, but a write conditional on it is one
    // where no object is, which creates one (TASK-205); its generations are times, in µs.
    let none = Version {
        e_tag: Some("\"cladefs-conformance\"".to_owned()),
        version: Some("1".to_owned()),
    };

    // Nothing there: read as none, deleted as nothing, updated from no version.
    assert_eq!(bucket.get(key).await.map(|object| object.bytes), Err(Error::NotFound));
    assert_eq!(bucket.head(key).await.map(|meta| meta.size), Err(Error::NotFound));
    assert_eq!(bucket.get_range(key, ByteRange::Bounded(0..1)).await, Err(Error::NotFound));
    assert_eq!(bucket.delete(key).await, Ok(()));
    let update = bucket.put(key, vec![1].into(), PutMode::Update(none.clone())).await;
    assert_eq!(update, Err(Error::Precondition));

    // Created once, and read as it was written, whole or by ranges, reading short past its end.
    let written: Vec<u8> = (0..100).collect();
    let first = bucket.put(key, written.clone().into(), PutMode::Create).await.unwrap();
    assert_eq!(bucket.put(key, vec![2].into(), PutMode::Create).await, Err(Error::AlreadyExists));
    let object = bucket.get(key).await.unwrap();
    let meta = (object.meta.key.as_str(), object.meta.size, &object.meta.version);
    assert_eq!((object.bytes, meta), (written, (key.as_str(), 100, &first)));
    let read = |range| bucket.get_range(key, range);
    assert_eq!(read(ByteRange::Bounded(10..20)).await, Ok((10..20).collect()));
    assert_eq!(read(ByteRange::Bounded(90..120)).await, Ok((90..100).collect()));
    assert_eq!(read(ByteRange::Suffix(10)).await, Ok((90..100).collect()));
    assert_eq!(read(ByteRange::Suffix(200)).await, Ok((0..100).collect()));

    // Updated only from its version, which an update makes another.
    assert_eq!(
        bucket.put(key, vec![3].into(), PutMode::Update(none)).await,
        Err(Error::Precondition)
    );
    let second = bucket.put(key, vec![4; 50].into(), PutMode::Update(first.clone())).await.unwrap();
    assert_ne!(second, first);
    assert_eq!(
        bucket.put(key, vec![5].into(), PutMode::Update(first)).await,
        Err(Error::Precondition)
    );
    let meta = bucket.head(key).await.unwrap();
    assert_eq!((meta.size, meta.version), (50, second.clone()));
    assert_eq!(bucket.get(key).await.unwrap().bytes, vec![4; 50]);
    bucket.put(key, vec![6; 7].into(), PutMode::Update(second)).await.unwrap();

    // Listed by prefix, in the order of their keys, the prefix a whole segment of them.
    let listed = [format!("{root}/list/b"), format!("{root}/list/a"), format!("{root}/list/c/d")];
    for key in listed.iter().chain([&format!("{root}/listing")]) {
        bucket.put(key, key.as_bytes().to_vec().into(), PutMode::Create).await.unwrap();
    }
    let keys: Vec<String> = (bucket.list(&format!("{root}/list/")).await.unwrap().into_iter())
        .map(|meta| meta.key)
        .collect();
    assert_eq!(keys, [listed[1].as_str(), &listed[0], &listed[2]]);
    let all: Vec<(String, u64)> = (bucket.list(&format!("{root}/")).await.unwrap().into_iter())
        .map(|meta| (meta.key, meta.size))
        .collect();
    let mut expected: Vec<(String, u64)> =
        listed.iter().map(|key| (key.clone(), key.len() as u64)).collect();
    let listing = format!("{root}/listing");
    expected.extend([(listing.clone(), listing.len() as u64), (key.clone(), 7)]);
    expected.sort();
    assert_eq!(all, expected);

    // cladefs: listed a level at a time, by delimiter (TASK-395.5): the objects right under the
    // prefix and the prefixes of the rest, each once, none of what is under them; a prefix whole
    // segments, as above. Then deleted in a batch, which deletes what is gone as nothing.
    let level = |prefix: String| async move {
        let listing = bucket.list_delimited(&prefix, Page::FIRST).await.unwrap();
        assert_eq!(listing.next, None, "{prefix}: one page");
        let objects: Vec<(String, u64)> =
            listing.objects.into_iter().map(|meta| (meta.key, meta.size)).collect();
        (listing.prefixes, objects)
    };
    let deep = [format!("{root}/list/c/e/f"), format!("{root}/list/g/h")];
    for key in &deep {
        bucket.put(key, vec![9; 3].into(), PutMode::Create).await.unwrap();
    }
    let sized = |key: &String| (key.clone(), key.len() as u64);
    assert_eq!(
        level(format!("{root}/list/")).await,
        (
            vec![format!("{root}/list/c/"), format!("{root}/list/g/")],
            vec![sized(&listed[1]), sized(&listed[0])]
        )
    );
    assert_eq!(
        level(format!("{root}/list/c/")).await,
        (vec![format!("{root}/list/c/e/")], vec![sized(&listed[2])])
    );
    assert_eq!(
        level(format!("{root}/")).await,
        (
            vec![format!("{root}/list/")],
            vec![(listing.clone(), listing.len() as u64), (key.clone(), 7)]
        )
    );
    assert_eq!(level(format!("{root}/nothing/")).await, (vec![], vec![]));
    // A page at a time, of so many prefixes and objects together at most, each page going on from
    // where the one before ended, in the order of their keys, prefixes and objects among each
    // other: the level whole, each once, no page empty, the last saying none follows.
    for max in [1, 2, 3] {
        let (mut pages, mut after) = (Vec::new(), None::<String>);
        loop {
            let page = Page { after: after.as_deref(), max };
            let listing = bucket.list_delimited(&format!("{root}/list/"), page).await.unwrap();
            let mut entries: Vec<String> = listing.prefixes.clone();
            entries.extend(listing.objects.iter().map(|meta| meta.key.clone()));
            entries.sort();
            assert!(!entries.is_empty() && entries.len() <= max, "{max}: {entries:?}");
            pages.push(entries);
            match listing.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        let whole: Vec<String> = pages.iter().flatten().cloned().collect();
        let expected =
            [&listed[1], &listed[0], &format!("{root}/list/c/"), &format!("{root}/list/g/")];
        // GCS and Azure may give fewer than asked, never more.
        assert_eq!(whole, expected.map(String::as_str), "pages of {max}: {pages:?}");
    }
    let gone = format!("{root}/list/gone");
    let batch: Vec<&str> = deep.iter().map(String::as_str).chain([gone.as_str()]).collect();
    assert_eq!(bucket.delete_batch(&batch).await, Ok(()));
    assert_eq!(level(format!("{root}/list/c/")).await, (vec![], vec![sized(&listed[2])]));
    assert_eq!(bucket.delete_batch(&[]).await, Ok(()));

    // Deleted, and gone: created anew after.
    for key in listed.iter().chain([&format!("{root}/listing"), key]) {
        assert_eq!(bucket.delete(key).await, Ok(()));
    }
    assert_eq!(bucket.head(key).await.map(|meta| meta.size), Err(Error::NotFound));
    assert_eq!(bucket.delete(key).await, Ok(()));
    assert_eq!(bucket.list(&format!("{root}/")).await.map(|list| list.len()), Ok(0));
    bucket.put(key, vec![8].into(), PutMode::Create).await.unwrap();
    assert_eq!(bucket.get(key).await.unwrap().bytes, [8]);
    bucket.delete(key).await.unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use std::sync::Mutex;

    /// A bucket whose GETs fail so many times, then find nothing, and that records its pauses.
    struct Flaky {
        failing: Mutex<u32>,
        pauses: Mutex<Vec<u32>>,
    }

    impl Bucket for Flaky {
        fn get(&self, _: &str) -> impl Future<Output = Result<Object, Error>> + Send {
            let mut failing = self.failing.lock().unwrap();
            let result = match *failing {
                0 => Err(Error::NotFound),
                _ => Err(Error::Unavailable),
            };
            *failing = failing.saturating_sub(1);
            std::future::ready(result)
        }

        fn get_range(
            &self,
            _: &str,
            _: ByteRange,
        ) -> impl Future<Output = Result<Vec<u8>, Error>> + Send {
            std::future::ready(Err(Error::Unavailable))
        }

        fn head(&self, _: &str) -> impl Future<Output = Result<Meta, Error>> + Send {
            std::future::ready(Err(Error::Unavailable))
        }

        fn list(&self, _: &str) -> impl Future<Output = Result<Vec<Meta>, Error>> + Send {
            std::future::ready(Err(Error::Unavailable))
        }

        fn list_delimited(
            &self,
            _: &str,
            _: Page<'_>,
        ) -> impl Future<Output = Result<Listing, Error>> + Send {
            std::future::ready(Err(Error::Unavailable))
        }

        fn put(
            &self,
            _: &str,
            _: Bytes,
            _: PutMode,
        ) -> impl Future<Output = Result<Version, Error>> + Send {
            std::future::ready(Err(Error::Unavailable))
        }

        fn delete(&self, _: &str) -> impl Future<Output = Result<(), Error>> + Send {
            std::future::ready(Err(Error::Unavailable))
        }

        fn now(&self) -> u64 {
            0
        }

        fn pause(&self, failures: u32) -> impl Future<Output = ()> + Send {
            self.pauses.lock().unwrap().push(failures);
            std::future::ready(())
        }
    }

    /// A request the backend failed goes again, after a pause that counts the failures in a row.
    #[test]
    fn failed_requests_pause_counting_their_failures() {
        let bucket = Flaky { failing: Mutex::new(3), pauses: Mutex::default() };
        assert_eq!(block_on(get(&bucket, "key")).map(|object| object.is_none()), Ok(true));
        assert_eq!(*bucket.pauses.lock().unwrap(), [1, 2, 3]);
    }

    /// Objects in memory, of `object_store`'s, whose requests the test fails where they would go
    /// again: what `object_store` refuses but for the protocol's conditions, it says as it says the
    /// backend is out of reach, which is retried.
    struct Unretried(crate::object_bucket::ObjectBucket);

    impl Bucket for Unretried {
        fn get(&self, key: &str) -> impl Future<Output = Result<Object, Error>> + Send {
            self.0.get(key)
        }

        fn get_range(
            &self,
            key: &str,
            range: ByteRange,
        ) -> impl Future<Output = Result<Vec<u8>, Error>> + Send {
            self.0.get_range(key, range)
        }

        fn head(&self, key: &str) -> impl Future<Output = Result<Meta, Error>> + Send {
            self.0.head(key)
        }

        fn list(&self, prefix: &str) -> impl Future<Output = Result<Vec<Meta>, Error>> + Send {
            self.0.list(prefix)
        }

        fn list_delimited(
            &self,
            prefix: &str,
            page: Page<'_>,
        ) -> impl Future<Output = Result<Listing, Error>> + Send {
            self.0.list_delimited(prefix, page)
        }

        fn put(
            &self,
            key: &str,
            bytes: Bytes,
            mode: PutMode,
        ) -> impl Future<Output = Result<Version, Error>> + Send {
            self.0.put(key, bytes, mode)
        }

        fn delete(&self, key: &str) -> impl Future<Output = Result<(), Error>> + Send {
            self.0.delete(key)
        }

        fn now(&self) -> u64 {
            self.0.now()
        }

        fn pause(&self, failures: u32) -> impl Future<Output = ()> + Send {
            async move { panic!("a request goes again, failed {failures} times") }
        }
    }

    /// TASK-79.40: a ranged read starting past the end of an object, which `object_store` refuses
    /// as it says the backend is out of reach, goes not again for ever: the object's size says why,
    /// and the object is corrupt. So an object's tail whose trailer says it is longer than it is.
    #[test]
    fn ranges_past_an_objects_end_go_not_again() {
        use crate::layout;
        use extent_frame::Frame;
        let memory = std::sync::Arc::new(object_store::memory::InMemory::new());
        let bucket = Unretried(crate::object_bucket::ObjectBucket::new(memory, ""));
        // A tail longer than a first read of it, its object cut short before it: its data, which
        // does not compress, from a xorshift generator.
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let data: Vec<u8> = (0..3 * crate::object::TAIL)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        let long = layout::with_footer(
            &data,
            &vec![2u8; crate::object::TAIL as usize + 5000],
            crate::testing::encrypting(layout::test_cell(1)),
        );
        let tail = Frame::of(&long).unwrap().tail();
        let short = long[(tail.end - tail.start) as usize..].to_vec();
        let size = short.len() as u64;
        assert!(size >= crate::object::TAIL);
        block_on(bucket.put("key", short.clone().into(), PutMode::Create)).unwrap();
        let read = block_on(crate::object::tail(&bucket, "key"));
        let past = format!("key: {:?} past its end, at {size} bytes", ByteRange::Bounded(tail));
        assert_eq!(read, Err(crate::Error::Corrupt(past)));

        // One that ends past it reads short; one of no bytes, which `object_store` refuses too,
        // reads none, asking nothing; one of an object gone, nothing.
        let read = |key: &str, range| block_on(get_range(&bucket, key, ByteRange::Bounded(range)));
        let tail = short[(size - 10) as usize..].to_vec();
        assert_eq!(read("key", size - 10..size + 10), Ok(Some(tail)));
        let past = Past { key: "key".into(), range: ByteRange::Bounded(size..size + 1), size };
        assert_eq!(read("key", size..size + 1), Err(Unread::Past(past)));
        assert_eq!(read("key", 5..5), Ok(Some(Vec::new())));
        assert_eq!(read("gone", 0..10), Ok(None));
    }
}
