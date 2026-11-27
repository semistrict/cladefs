// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: multi-bucket routing and cross-bucket mirroring (`RoutedBucket<B>`, backlog doc-23,
//! TASK-633.4, TASK-633.8, board 6023).
//!
//! Separates routing from single-bucket [`ObjectBucket`] so the home bucket is already unrouted
//! (`RoutedBucket::own`), eliminating shadow clones (`twin()`) and duplicate counts/marks/decoded
//! caches.

use crate::bucket::{
    Bucket, ByteRange, Error, Listing, Meta, Object, Page, PutMode, Refusal, Refused, Version,
};
use crate::layout;
use crate::object_bucket::{ObjectBucket, Registered, Stats};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a routed bucket goes by what it last listed of a cell's mirror markers here before
/// listing them again ([`RoutedBucket::mirrored`]).
const MIRRORED_FOR: Duration = Duration::from_secs(10);

/// Opens the bucket numbered `number`, other than `home`, for `home`'s requests of that bucket's
/// cells (TASK-633.4): [`Registered`] reads it in the registry of buckets.
pub trait Resolve<B: Bucket = ObjectBucket>: Send + Sync {
    fn open<'a>(
        &'a self,
        home: &'a B,
        number: u16,
    ) -> futures::future::BoxFuture<'a, Result<Arc<B>, crate::Error>>;
}

/// A routing bucket's other buckets: how it opens them, its own number once read, and those it
/// opened, each once.
struct Route<B: Bucket> {
    resolve: Arc<dyn Resolve<B>>,
    home: Mutex<Option<u16>>,
    members: Mutex<BTreeMap<u16, Arc<B>>>,
}

/// A bucket wrapper routing requests for other buckets' cells to their resolved member buckets
/// while serving local keys and mirrors directly from `home` (board 6023).
pub struct RoutedBucket<B: Bucket = ObjectBucket> {
    home: Arc<B>,
    route: Option<Route<B>>,
    mirrored: Mutex<HashMap<layout::CellId, (bool, Instant)>>,
    mirrors: crate::mirror::Mirrors,
}

impl<B: Bucket> RoutedBucket<B> {
    /// Routes requests for other buckets' cells via `resolve`, keeping `home`'s own unrouted.
    pub fn new(home: Arc<B>, resolve: Arc<dyn Resolve<B>>) -> Self {
        Self {
            home,
            route: Some(Route { resolve, home: Mutex::default(), members: Mutex::default() }),
            mirrored: Mutex::default(),
            mirrors: crate::mirror::Mirrors::default(),
        }
    }

    /// Wraps a single bucket with routing disabled.
    pub fn unrouted(home: Arc<B>) -> Self {
        Self {
            home,
            route: None,
            mirrored: Mutex::default(),
            mirrors: crate::mirror::Mirrors::default(),
        }
    }

    /// Its own unrouted home bucket: shared directly with zero shadow clone (board 6023).
    pub fn own(&self) -> Arc<B> {
        self.home.clone()
    }

    /// Whether this bucket routes requests for other buckets' cells.
    pub fn is_routed(&self) -> bool {
        self.route.is_some()
    }

    #[cfg(test)]
    pub(crate) fn mirrors_idle(&self) -> bool {
        self.mirrors.idle()
    }

    /// The bucket numbered `number`, opened by its resolver once. `NoBucket` if unrouted.
    pub async fn numbered(&self, number: u16) -> Result<Arc<B>, crate::Error> {
        let route = self.route.as_ref().ok_or(crate::Error::NoBucket(number))?;
        if let Some(member) = route.members.lock().unwrap().get(&number) {
            return Ok(member.clone());
        }
        let opened = route.resolve.open(&self.home, number).await?;
        Ok(route.members.lock().unwrap().entry(number).or_insert(opened).clone())
    }

    /// cladefs: the numbers of the buckets an ephemeral cell's reference may be in, which
    /// decommissioning a host lists and retires in (board 8517): those its home's root records
    /// ([`crate::lifecycle::ephemeral_buckets`]), and, routing, those the root bucket's registry
    /// records, the authority, which a copy its spread missed lacks, as [`Registered`] reads it.
    pub async fn ephemeral_buckets(&self) -> Result<BTreeSet<u16>, crate::Error> {
        use crate::identity::ROOT;
        let mut numbers = crate::lifecycle::ephemeral_buckets(&*self.home).await?;
        let own = crate::identity::get(&*self.home).await?.map(|identity| identity.number);
        if self.route.is_some() && own.is_some_and(|own| own != ROOT) {
            let root = crate::identity::registry(&*self.numbered(ROOT).await?).await?;
            numbers.extend(root.iter().flat_map(|root| &root.buckets).map(|entry| entry.number));
        }
        Ok(numbers)
    }

    async fn own_number(&self, route: &Route<B>) -> Result<Option<u16>, Error> {
        if let Some(number) = *route.home.lock().unwrap() {
            return Ok(Some(number));
        }
        let identity = match self.home.get(layout::IDENTITY).await {
            // cladefs: unreadable, read the same again: refused, never retried (board 5534).
            Ok(object) => crate::identity::parse(&object.bytes).map_err(|err| {
                log::error!("bucket: reading its own identity: {err}");
                Error::Refused(Refusal::Unreadable)
            })?,
            Err(Error::NotFound) => return Ok(None),
            Err(err) => return Err(err),
        };
        *route.home.lock().unwrap() = Some(identity.number);
        Ok(Some(identity.number))
    }

    /// Its own number, [`RoutedBucket::own_number`], asked until its bucket answers: refused for
    /// good, or its identity unreadable, refused. Never none for a bucket out of reach, which a
    /// fork would take for no mirror here, and refer in its version's bucket.
    async fn own_number_answered(&self, route: &Route<B>) -> Result<Option<u16>, Refused> {
        let mut failures = 0u32;
        loop {
            match self.own_number(route).await {
                Ok(number) => return Ok(number),
                Err(Error::Refused(refusal)) => {
                    return Err(Refused::of(layout::IDENTITY, refusal));
                }
                // Out of reach: asked again, as every request of a bucket is.
                Err(_) => {
                    failures = failures.saturating_add(1);
                    self.pause(failures).await;
                }
            }
        }
    }

    async fn numbered_answered(&self, number: u16, key: &str) -> Result<Arc<B>, Refused> {
        let mut failures = 0u32;
        loop {
            match self.numbered(number).await {
                Ok(member) => return Ok(member),
                Err(crate::Error::Refused(refusal)) => return Err(refusal),
                // Out of reach while reading identity or the registry: asked again, as every
                // request of a bucket is.
                Err(err) if crate::identity::unreached(&err) => {
                    failures = failures.saturating_add(1);
                    self.pause(failures).await;
                }
                // cladefs: unrecorded, mismatched, corrupt, wrong-format or malformed-URL buckets
                // fail for good, never retried forever (board 10446, hotbed review 94).
                Err(err) => {
                    log::error!("bucket: reaching bucket {number}, for {key}: {err}");
                    return Err(Refused::of(key, Refusal::Unreadable));
                }
            }
        }
    }

    /// Whether this bucket holds a mirror of any version of `cell`, a cell of another bucket. Its
    /// markers' listing failing fails it (board 5526).
    async fn mirrored(&self, cell: layout::CellId) -> Result<bool, Error> {
        if let Some(&(held, listed)) = self.mirrored.lock().unwrap().get(&cell)
            && listed.elapsed() < MIRRORED_FOR
        {
            return Ok(held);
        }
        let listed = Instant::now();
        let markers = self.home.list(&layout::mirrors(cell)).await?;
        let mut held = false;
        for meta in &markers {
            if !matches!(layout::kind(&meta.key), layout::Kind::Mirror { .. }) {
                continue;
            }
            match self.home.get(&meta.key).await {
                Ok(object) => {
                    // Unreadable or future-format markers are not treated as marked here; the
                    // explicit `Here::marked` path surfaces `Error::BucketFormat`.
                    if layout::Mirror::decode(&object.bytes).is_ok_and(|m| m.is_marked()) {
                        held = true;
                        break;
                    }
                }
                Err(Error::NotFound) => continue,
                Err(err) => return Err(err),
            }
        }
        self.mirrored.lock().unwrap().insert(cell, (held, listed));
        Ok(held)
    }

    async fn read_here_first(&self, key: &str) -> Result<bool, Error> {
        let Some(route) = &self.route else { return Ok(false) };
        let Some(number) = layout::routed_to(key) else { return Ok(false) };
        if !crate::peer::served(key) || self.own_number(route).await? == Some(number) {
            return Ok(false);
        }
        let Some(cell) = layout::cell_of(key) else { return Ok(false) };
        self.mirrored(cell).await
    }

    async fn member(&self, key: &str) -> Result<Option<Arc<B>>, Error> {
        let Some(route) = &self.route else { return Ok(None) };
        let Some(number) = layout::routed_to(key) else { return Ok(None) };
        if self.own_number(route).await? == Some(number) {
            return Ok(None);
        }
        self.numbered(number).await.map(Some).map_err(|err| {
            log::warn!("bucket: reaching bucket {number}, for {key}: {err}");
            Error::Unavailable
        })
    }

    /// Mirrors `origin` into this bucket if `child` is of this bucket and `origin.parent` is of
    /// another (TASK-633.8).
    pub fn mirror_wanted(
        self: &Arc<Self>,
        child: layout::CellId,
        origin: layout::Origin,
    ) -> impl Future<Output = Result<Option<crate::mirror::Mirrored>, crate::Error>> + Send + 'static
    {
        let turn = self.route.as_ref().and_then(|_| self.mirrors.turn(origin));
        let bucket = self.clone();
        async move {
            let Some(turn) = turn else { return Ok(None) };
            let route = bucket.route.as_ref().expect("routed");
            let number = bucket
                .own_number(route)
                .await
                .map_err(|err| crate::Error::Read(format!("its identity: {err}")))?;
            let Some(number) = number else { return Ok(None) };
            turn.mirror(&*bucket, &*bucket.home, number, child, origin).await
        }
    }

    /// Mirrors the version `cell` was forked from into this bucket if needed (TASK-633.8).
    pub async fn mirror_for(
        self: Arc<Self>,
        cell: layout::CellId,
    ) -> Result<Option<(layout::Origin, crate::mirror::Mirrored)>, crate::Error> {
        if self.route.is_none() {
            return Ok(None);
        }
        let Some(origin) = crate::head::get(&*self, cell).await?.and_then(|head| head.origin())
        else {
            return Ok(None);
        };
        let mirrored = self.mirror_wanted(cell, origin).await?;
        Ok(mirrored.map(|mirrored| (origin, mirrored)))
    }
}

impl RoutedBucket<ObjectBucket> {
    /// Requests and bytes of the home bucket.
    pub fn stats(&self) -> Stats {
        self.home.stats()
    }

    /// Requests and bytes of the home bucket and every member bucket opened so far, by number.
    pub fn stats_by_bucket(&self) -> Vec<(u16, Stats)> {
        let own = self.route.as_ref().and_then(|route| *route.home.lock().unwrap());
        let mut all = vec![(own.unwrap_or(0), self.home.stats())];
        if let Some(route) = &self.route {
            let members = route.members.lock().unwrap();
            all.extend(members.iter().map(|(number, member)| (*number, member.stats())));
        }
        all
    }

    /// Allocates a new cell ID from the home bucket's allocator.
    pub async fn new_objectid(&self) -> Result<u64, crate::Error> {
        self.home.new_objectid().await
    }

    /// Checks conditional write support on the home bucket.
    pub async fn probed(&self, nonce: u64) -> Result<(), String> {
        self.home.probed(nonce).await
    }
}

impl From<ObjectBucket> for RoutedBucket<ObjectBucket> {
    fn from(bucket: ObjectBucket) -> Self {
        Self::unrouted(Arc::new(bucket))
    }
}

impl From<Arc<ObjectBucket>> for RoutedBucket<ObjectBucket> {
    fn from(bucket: Arc<ObjectBucket>) -> Self {
        Self::unrouted(bucket)
    }
}

impl<B: Bucket> Bucket for RoutedBucket<B> {
    fn decoded(&self) -> Option<&crate::recovery::Decoded> {
        self.home.decoded()
    }

    fn mirrors(&self) -> Option<&dyn crate::mirror::Here> {
        self.route.as_ref().map(|_| self as &dyn crate::mirror::Here)
    }

    fn ephemeral_buckets(
        &self,
    ) -> Option<crate::bucket::BucketFuture<'_, Result<BTreeSet<u16>, crate::Error>>> {
        Some(Box::pin(self.ephemeral_buckets()))
    }

    fn events_object(
        &self,
    ) -> Option<crate::bucket::BucketFuture<'_, Result<Option<Object>, crate::Error>>> {
        Some(Box::pin(async move {
            let auth = crate::events::authority(self).await?;
            Ok(crate::bucket::get(&*auth, crate::layout::EVENTS).await?)
        }))
    }

    fn random_u64(&self) -> u64 {
        self.home.random_u64()
    }

    fn ratchet(&self) -> &formats::Ratchet {
        self.home.ratchet()
    }

    async fn get(&self, key: &str) -> Result<Object, Error> {
        // cladefs: the cell's bucket asked only where the mirror copied not what is asked, never
        // as this bucket fails (board 8547): its failure is the read's, asked again as it is.
        if self.read_here_first(key).await? {
            match self.home.get(key).await {
                Err(Error::NotFound) => {}
                answered => return answered,
            }
        }
        match self.member(key).await? {
            Some(member) => member.get(key).await,
            None => self.home.get(key).await,
        }
    }

    async fn get_if_changed(
        &self,
        key: &str,
        if_none_match: &Version,
    ) -> Result<Option<Object>, Error> {
        if self.read_here_first(key).await? {
            match self.home.get_if_changed(key, if_none_match).await {
                Err(Error::NotFound) => {}
                answered => return answered,
            }
        }
        match self.member(key).await? {
            Some(member) => member.get_if_changed(key, if_none_match).await,
            None => self.home.get_if_changed(key, if_none_match).await,
        }
    }

    async fn get_range(&self, key: &str, range: ByteRange) -> Result<Vec<u8>, Error> {
        // cladefs: the cell's bucket asked only where the mirror copied not what is asked, never
        // as this bucket fails (board 8547): its failure is the read's, asked again as it is.
        if self.read_here_first(key).await? {
            match self.home.get_range(key, range.clone()).await {
                Err(Error::NotFound) => {}
                answered => return answered,
            }
        }
        match self.member(key).await? {
            Some(member) => member.get_range(key, range).await,
            None => self.home.get_range(key, range).await,
        }
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        // cladefs: the cell's bucket asked only where the mirror copied not what is asked, never
        // as this bucket fails (board 8547): its failure is the read's, asked again as it is.
        if self.read_here_first(key).await? {
            match self.home.head(key).await {
                Err(Error::NotFound) => {}
                answered => return answered,
            }
        }
        match self.member(key).await? {
            Some(member) => member.head(key).await,
            None => self.home.head(key).await,
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Meta>, Error> {
        match self.member(prefix).await? {
            Some(member) => member.list(prefix).await,
            None => self.home.list(prefix).await,
        }
    }

    async fn list_delimited(&self, prefix: &str, page: Page<'_>) -> Result<Listing, Error> {
        match self.member(prefix).await? {
            Some(member) => member.list_delimited(prefix, page).await,
            None => self.home.list_delimited(prefix, page).await,
        }
    }

    async fn put(&self, key: &str, bytes: crate::Bytes, mode: PutMode) -> Result<Version, Error> {
        match self.member(key).await? {
            Some(member) => member.put(key, bytes, mode).await,
            None => self.home.put(key, bytes, mode).await,
        }
    }

    async fn append(&self, key: &str, offset: u64, bytes: crate::Bytes) -> Result<u64, Error> {
        match self.member(key).await? {
            Some(member) => member.append(key, offset, bytes).await,
            None => self.home.append(key, offset, bytes).await,
        }
    }

    async fn rewritable(&self, key: &str) {
        match self.member(key).await {
            Ok(Some(member)) => member.rewritable(key).await,
            // Out of reach: spaced as this bucket's writes are; the write itself goes to its own
            // bucket, which answers for it.
            Ok(None) | Err(_) => self.home.rewritable(key).await,
        }
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        match self.member(key).await? {
            Some(member) => member.delete(key).await,
            None => self.home.delete(key).await,
        }
    }

    async fn delete_batch(&self, keys: &[&str]) -> Result<(), Error> {
        if self.route.is_none() {
            return self.home.delete_batch(keys).await;
        }
        let mut here = Vec::new();
        let mut there: Vec<(Arc<B>, Vec<&str>)> = Vec::new();
        for &key in keys {
            match self.member(key).await? {
                None => here.push(key),
                Some(member) => match there.iter_mut().find(|(m, _)| Arc::ptr_eq(m, &member)) {
                    Some((_, keys)) => keys.push(key),
                    None => there.push((member, vec![key])),
                },
            }
        }
        if !here.is_empty() {
            self.home.delete_batch(&here).await?;
        }
        for (member, keys) in there {
            member.delete_batch(&keys).await?;
        }
        Ok(())
    }

    /// cladefs: the least of the clocks of the buckets it reached that read a time (board 8467's
    /// review): a time it reads is compared with dates of, and written into, the bucket a request
    /// routes to, which it can't name; each bucket's clock reads its backend's time at most, so the
    /// least reads each's at most, a window in any closing late, never early. A clock that reads
    /// no time (0) yet is a bucket it never had an answer of, whose dates it compares none of.
    fn now(&self) -> u64 {
        let members = self.route.as_ref().map(|route| route.members.lock().unwrap());
        let members = members.iter().flat_map(|members| members.values());
        let clocks = std::iter::once(self.home.now()).chain(members.map(|member| member.now()));
        clocks.filter(|&now| now > 0).min().unwrap_or(0)
    }

    fn pause(&self, failures: u32) -> impl Future<Output = ()> + Send {
        self.home.pause(failures)
    }
}

impl<B: Bucket> crate::mirror::Here for RoutedBucket<B> {
    fn marked(
        &self,
        parent: layout::CellId,
        at: u64,
    ) -> futures::future::BoxFuture<'_, Result<bool, crate::Error>> {
        Box::pin(async move {
            let Some(route) = &self.route else { return Ok(false) };
            let Some(of) = layout::routed_to(&parent.prefix()) else { return Ok(false) };
            if self.own_number_answered(route).await?.is_none_or(|number| number == of) {
                return Ok(false);
            }
            let marker = layout::mirror(parent, at);
            let Some(object) = crate::bucket::get(&*self.home, &marker).await? else {
                return Ok(false);
            };
            if !layout::Mirror::decode(&object.bytes)?.is_marked() {
                return Ok(false);
            }
            self.mirrored.lock().unwrap().insert(parent, (true, Instant::now()));
            self.mirrors.marked(parent, at);
            Ok(true)
        })
    }

    fn create<'a>(
        &'a self,
        key: &'a str,
        bytes: Vec<u8>,
    ) -> futures::future::BoxFuture<'a, Result<(), crate::bucket::Refused>> {
        Box::pin(async move {
            crate::bucket::create(&*self.home, key, bytes).await?;
            Ok(())
        })
    }

    fn retire<'a>(
        &'a self,
        keys: &'a [&'a str],
    ) -> futures::future::BoxFuture<'a, Result<usize, crate::bucket::Refused>> {
        Box::pin(async move {
            let Some(route) = &self.route else { return Ok(0) };
            // cladefs: its identity asked until its bucket answers, as `marked` asks it (board
            // 8547): none only for a bucket never formatted, whose references are all its own.
            let Some(number) = self.own_number_answered(route).await? else { return Ok(0) };
            // cladefs: each reference retired in its child's own bucket, where mirrors of its
            // parent's versions for that child live, never in an unrelated home bucket if
            // `collect_cell` was called through another bucket's router (board 10446).
            let mut by_bucket: std::collections::BTreeMap<u16, Vec<&str>> =
                std::collections::BTreeMap::new();
            for &key in keys {
                let layout::Kind::Reference { parent, child, .. } = layout::kind(key) else {
                    continue;
                };
                let child_bucket = layout::routed_to(&child.prefix()).unwrap_or(number);
                if layout::routed_to(&parent.prefix()).is_some_and(|of| of != child_bucket) {
                    by_bucket.entry(child_bucket).or_default().push(key);
                }
            }
            let mut retired = 0;
            for (target, group) in by_bucket {
                if target == number {
                    crate::lifecycle::retire(&*self.home, &group).await?;
                } else {
                    let member = self.numbered_answered(target, group[0]).await?;
                    crate::lifecycle::retire(&*member, &group).await?;
                }
                retired += group.len();
            }
            Ok(retired)
        })
    }
}

impl Resolve<ObjectBucket> for Registered {
    fn open<'a>(
        &'a self,
        home: &'a ObjectBucket,
        number: u16,
    ) -> futures::future::BoxFuture<'a, Result<Arc<ObjectBucket>, crate::Error>> {
        Box::pin(async move { Ok(Arc::new(self.reach(&self.entry(home, number).await?).await?)) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Direct, SimBucket};
    use futures::executor::block_on;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A cell of bucket 2, whose version at 3 bucket 1 holds a mirror of.
    const PARENT: layout::CellId = layout::CellId { tenant: 1, id: 2 << 48 | 5 };

    /// A bucket of the simulation's, whose reads of `failing` fail as `fails` says, each in turn,
    /// and which counts the requests it is asked.
    struct Scripted {
        objects: Direct,
        failing: String,
        fails: Mutex<VecDeque<Error>>,
        requests: AtomicUsize,
    }

    impl Scripted {
        fn new(failing: &str) -> Self {
            let (fails, requests) = Default::default();
            Self { objects: Direct(SimBucket::new()), failing: failing.to_owned(), fails, requests }
        }

        /// What a read of `key` fails with, if it does: counted either way.
        fn fails(&self, key: &str) -> Option<Error> {
            self.requests.fetch_add(1, Ordering::Relaxed);
            (key == self.failing).then(|| self.fails.lock().unwrap().pop_front()).flatten()
        }
    }

    impl Bucket for Scripted {
        async fn get(&self, key: &str) -> Result<Object, Error> {
            match self.fails(key) {
                Some(error) => Err(error),
                None => self.objects.get(key).await,
            }
        }

        async fn get_range(&self, key: &str, range: ByteRange) -> Result<Vec<u8>, Error> {
            match self.fails(key) {
                Some(error) => Err(error),
                None => self.objects.get_range(key, range).await,
            }
        }

        async fn head(&self, key: &str) -> Result<Meta, Error> {
            match self.fails(key) {
                Some(error) => Err(error),
                None => self.objects.head(key).await,
            }
        }

        async fn list(&self, prefix: &str) -> Result<Vec<Meta>, Error> {
            match self.fails(prefix) {
                Some(error) => Err(error),
                None => self.objects.list(prefix).await,
            }
        }

        async fn list_delimited(&self, prefix: &str, page: Page<'_>) -> Result<Listing, Error> {
            self.requests.fetch_add(1, Ordering::Relaxed);
            self.objects.list_delimited(prefix, page).await
        }

        async fn put(
            &self,
            key: &str,
            bytes: crate::Bytes,
            mode: PutMode,
        ) -> Result<Version, Error> {
            self.requests.fetch_add(1, Ordering::Relaxed);
            self.objects.put(key, bytes, mode).await
        }

        async fn delete(&self, key: &str) -> Result<(), Error> {
            self.requests.fetch_add(1, Ordering::Relaxed);
            self.objects.delete(key).await
        }

        fn now(&self) -> u64 {
            self.objects.now()
        }
    }

    /// Opens bucket 2 as the one it holds.
    struct Opens(Arc<Scripted>);

    impl Resolve<Scripted> for Opens {
        fn open<'a>(
            &'a self,
            _: &'a Scripted,
            number: u16,
        ) -> futures::future::BoxFuture<'a, Result<Arc<Scripted>, crate::Error>> {
            assert_eq!(number, 2, "bucket 2 alone");
            Box::pin(async move { Ok(self.0.clone()) })
        }
    }

    /// board 8547: a read of an object of a cell of another bucket mirrored here, its get, ranged
    /// get or HEAD, goes to the cell's bucket only where this one has no copy of it (NotFound):
    /// this bucket out of reach, or refusing it, the read fails so, the cell's bucket asked
    /// nothing, the cross-region request and its egress never paid for this bucket's failure.
    #[test]
    fn mirrored_reads_go_to_their_cells_bucket_only_for_what_is_not_here() {
        let key = layout::segment(PARENT, 1, 1);
        let (home, member) = (Arc::new(Scripted::new(&key)), Arc::new(Scripted::new(&key)));
        block_on(async {
            let details = crate::identity::Details::default();
            crate::identity::format(&home.objects, &details).await.unwrap();
            let marker = layout::encode(&layout::Mirror::NOW);
            home.objects
                .put(&layout::mirror(PARENT, 3), marker.into(), PutMode::Create)
                .await
                .unwrap();
            member.objects.put(&key, vec![7; 10].into(), PutMode::Create).await.unwrap();
        });
        let routed = RoutedBucket::new(home.clone(), Arc::new(Opens(member.clone())));
        let read = |which: &str| {
            block_on(async {
                match which {
                    "get" => routed.get(&key).await.map(drop),
                    "get-range" => routed.get_range(&key, ByteRange::Bounded(0..4)).await.map(drop),
                    _ => routed.head(&key).await.map(drop),
                }
            })
        };
        for which in ["get", "get-range", "head"] {
            for error in [Error::Unavailable, Error::Refused(Refusal::Denied)] {
                home.fails.lock().unwrap().push_back(error);
                assert_eq!(read(which), Err(error), "{which}");
                assert_eq!(member.requests.load(Ordering::Relaxed), 0, "{which}, {error}");
            }
            // Not copied here: read there.
            home.fails.lock().unwrap().push_back(Error::NotFound);
            assert_eq!(read(which), Ok(()), "{which}");
            assert_eq!(member.requests.swap(0, Ordering::Relaxed), 1, "{which}");
        }
    }

    /// board 8547: a routed bucket retiring references of a mirror here, its identity out of reach,
    /// asks it again until it answers, and retires them then: never takes the failure for "none to
    /// retire", reporting none retired.
    #[test]
    fn retiring_asks_its_identity_until_it_answers() {
        use crate::mirror::Here as _;
        let home = Arc::new(Scripted::new(layout::IDENTITY));
        let member = Arc::new(Scripted::new(""));
        let child = layout::CellId { tenant: 1, id: 1 << 48 | 9 };
        let reference = layout::reference(PARENT, 3, child);
        block_on(async {
            let details = crate::identity::Details::default();
            crate::identity::format(&home.objects, &details).await.unwrap();
            let bytes = layout::encode(&layout::Reference::of(1));
            home.objects.put(&reference, bytes.into(), PutMode::Create).await.unwrap();
        });
        home.fails.lock().unwrap().extend([Error::Unavailable, Error::Unavailable]);
        let routed = RoutedBucket::new(home.clone(), Arc::new(Opens(member)));
        let retired = block_on(routed.retire(&[reference.as_str()]));
        assert_eq!(retired, Ok(1));
        assert!(home.fails.lock().unwrap().is_empty(), "its identity asked until it answered");
        let gone = block_on(home.objects.head(&reference)).map(drop);
        assert_eq!(gone, Err(Error::NotFound), "{reference} not retired");

        // Unreadable, read the same again: refused.
        let identity = block_on(home.objects.get(layout::IDENTITY)).unwrap().meta.version;
        let garbage = home.objects.put(layout::IDENTITY, vec![1].into(), PutMode::Update(identity));
        block_on(garbage).unwrap();
        let routed = RoutedBucket::new(home.clone(), Arc::new(Opens(Arc::new(Scripted::new("")))));
        let refused = block_on(routed.retire(&[reference.as_str()]));
        assert_eq!(refused, Err(Refused::of(layout::IDENTITY, Refusal::Unreadable)));
    }

    /// board 10446 (hotbed review 94): `RoutedBucket::retire` routes a foreign child's reference
    /// to the child's own bucket, never to `home`, and refuses permanent resolution errors
    /// (`NoBucket`) rather than spinning forever.
    #[test]
    fn retiring_routes_to_child_bucket_and_refuses_permanent_errors() {
        use crate::mirror::Here as _;
        let home = Arc::new(Scripted::new(""));
        let bucket3 = Arc::new(Scripted::new(""));
        let child_in_3 = layout::CellId { tenant: 1, id: 3 << 48 | 5 };
        let reference = layout::reference(PARENT, 3, child_in_3);
        block_on(async {
            let details = crate::identity::Details::default();
            crate::identity::format(&home.objects, &details).await.unwrap();
            let bytes = layout::encode(&layout::Reference::of(1));
            bucket3.objects.put(&reference, bytes.into(), PutMode::Create).await.unwrap();
        });
        // Bucket 3 reachable: retired in bucket 3, home untouched.
        let members = BTreeMap::from([(3, bucket3.clone())]);
        let routed = RoutedBucket::new(home.clone(), Arc::new(Members(members)));
        assert_eq!(block_on(routed.retire(&[reference.as_str()])), Ok(1));
        assert_eq!(block_on(bucket3.objects.head(&reference)).map(drop), Err(Error::NotFound));
        let mark = layout::retired(PARENT, 3, child_in_3);
        assert!(block_on(bucket3.objects.head(&mark)).is_ok(), "marked in bucket 3");
        assert_eq!(block_on(home.objects.head(&mark)).map(drop), Err(Error::NotFound));

        // Bucket 3 unrecorded (`NoBucket`): refused immediately, never hangs.
        let missing = RoutedBucket::new(home, Arc::new(Members(BTreeMap::new())));
        assert_eq!(
            block_on(missing.retire(&[reference.as_str()])),
            Err(Refused::of(&reference, Refusal::Unreadable))
        );
    }

    /// Opens each bucket of the numbers it holds as the one it holds.
    struct Members(BTreeMap<u16, Arc<Scripted>>);

    impl Resolve<Scripted> for Members {
        fn open<'a>(
            &'a self,
            _: &'a Scripted,
            number: u16,
        ) -> futures::future::BoxFuture<'a, Result<Arc<Scripted>, crate::Error>> {
            let found = self.0.get(&number).cloned().ok_or(crate::Error::NoBucket(number));
            Box::pin(std::future::ready(found))
        }
    }

    /// Board 8517: ephemeral cells forked through daemons of two home buckets, 1 and 2, of
    /// parents of a third, 3, have their references and index entries in bucket 3 alone,
    /// whichever's daemon wrote them; a reference written through one is retired through another,
    /// its entry with it (`Host::retire`, by the bucket its journal names). Decommissioning their
    /// incarnation through a daemon of any of the three, the registry's buckets each, finds and
    /// retires every one, counting them, and their entries, and leaves its tombstone in every
    /// bucket, which refuses a reference of the incarnation after; a bucket refusing its listing
    /// fails it, never answering that it retired none.
    #[test]
    fn ephemeral_references_are_decommissioned_from_any_bucket() {
        use crate::layout::{CellId, Kind, Origin};
        use crate::lifecycle::{self, Decommissioned};
        let incarnation = 0x2b;
        let indexed_there = layout::ephemeral_indexes(3, incarnation);
        let parent = |n: u64| CellId { tenant: 1, id: 3 << 48 | n };
        let origin = |n| Origin { parent: parent(n), at: 7, epoch: 1 };
        let child = |n: u64| CellId { tenant: 1, id: 1 << 48 | 0x100 | n };
        for decommissioner in [1, 2, 3] {
            let buckets: BTreeMap<u16, Arc<Scripted>> = [(1, ""), (2, ""), (3, &*indexed_there)]
                .into_iter()
                .map(|(number, failing)| (number, Arc::new(Scripted::new(failing))))
                .collect();
            block_on(async {
                let details = crate::identity::Details::default();
                let root = &buckets[&1].objects;
                crate::identity::format(root, &details).await.unwrap();
                let mut registry = None;
                for number in [2, 3] {
                    let new = &buckets[&number].objects;
                    let added = crate::identity::add(root, &details, new, details.clone());
                    let (identity, added) = added.await.unwrap();
                    assert_eq!(identity.number, number);
                    registry = Some(added);
                }
                // Bucket 2 holds no copy of the registry, its spread cut short: the root's, the
                // authority, records every bucket.
                for number in [1, 3] {
                    let registry = registry.as_ref().unwrap();
                    crate::identity::spread(registry, &buckets[&number].objects).await.unwrap();
                }
            });
            let routed = |home: u16| {
                RoutedBucket::new(buckets[&home].clone(), Arc::new(Members(buckets.clone())))
            };
            let held = |number: u16| {
                let listed = block_on(buckets[&number].objects.list("")).unwrap();
                let count = |of: fn(&Kind) -> bool| {
                    listed.iter().filter(|meta| of(&layout::kind(&meta.key))).count()
                };
                (
                    count(|kind| matches!(kind, Kind::Ephemeral { .. })),
                    count(|kind| matches!(kind, Kind::EphemeralIndex { .. })),
                    count(|kind| matches!(kind, Kind::Decommissioned { .. })),
                )
            };
            let (one, two) = (routed(1), routed(2));
            block_on(async {
                for n in 0..3 {
                    lifecycle::refer_ephemeral(&one, origin(n), incarnation, child(n))
                        .await
                        .unwrap();
                }
                for n in 3..5 {
                    lifecycle::refer_ephemeral(&two, origin(n), incarnation, child(n))
                        .await
                        .unwrap();
                }
            });
            assert_eq!((held(1), held(2), held(3)), ((0, 0, 0), (0, 0, 0), (5, 5, 0)));
            block_on(lifecycle::unrefer_ephemeral(&two, origin(0), incarnation, child(0))).unwrap();
            assert_eq!(held(3), (4, 4, 0), "written through bucket 1's, retired through 2's");

            let through = routed(decommissioner);
            let numbers = block_on(through.ephemeral_buckets()).unwrap();
            assert_eq!(numbers, BTreeSet::from([crate::identity::NAMED, 1, 2, 3]));
            let listed = block_on(lifecycle::ephemeral_references(&through, &numbers, incarnation));
            assert_eq!(listed.map(|listed| listed.len()), Ok(4));
            if decommissioner == 3 {
                buckets[&3].fails.lock().unwrap().push_back(Error::Refused(Refusal::Denied));
                let refused =
                    block_on(lifecycle::decommission(&through, &numbers, incarnation, 100));
                let refusal = Refused::of(&indexed_there, Refusal::Denied);
                assert_eq!(refused, Err(crate::Error::Refused(refusal)));
                assert_eq!(held(3).0, 4, "nothing retired");
            }
            let decommissioned =
                block_on(lifecycle::decommission(&through, &numbers, incarnation, 100));
            assert_eq!(decommissioned, Ok(Decommissioned { retired: 4, left: 0 }));
            // Its tombstone in each bucket; bucket 0's, of the cells tests name, in its home.
            let home_named = |number| usize::from(number == decommissioner);
            assert_eq!(held(1), (0, 0, 1 + home_named(1)));
            assert_eq!(held(2), (0, 0, 1 + home_named(2)));
            assert_eq!(held(3), (0, 0, 1 + home_named(3)));
            let late = block_on(lifecycle::refer_ephemeral(&one, origin(9), incarnation, child(9)));
            assert_eq!(late, Err(crate::Error::Decommissioned(incarnation)));
            assert_eq!(held(3), (0, 0, 1 + home_named(3)), "taken back");

            // Board 9966 AC #1: a bucket added after decommissioning copies the root's tombstones
            // (and not active ephemeral index entries of other incarnations) before writing its
            // identity, so referring the decommissioned incarnation against a parent there is
            // refused with `Error::Decommissioned`.
            let active_incarnation = incarnation + 2;
            let root_parent = CellId { tenant: 1, id: 1 << 48 | 7 };
            let root_origin = Origin { parent: root_parent, at: 7, epoch: 1 };
            block_on(lifecycle::refer_ephemeral(&one, root_origin, active_incarnation, child(10)))
                .unwrap();

            let bucket4 = Arc::new(Scripted::new(""));
            block_on(async {
                let details = crate::identity::Details::default();
                let root = &buckets[&1].objects;
                let (id4, registry) =
                    crate::identity::add(root, &details, &bucket4.objects, details.clone())
                        .await
                        .unwrap();
                assert_eq!(id4.number, 4);
                crate::identity::spread(&registry, &bucket4.objects).await.unwrap();
            });
            let mut buckets_with_4 = buckets.clone();
            buckets_with_4.insert(4, bucket4);
            let held4 = || {
                let listed = block_on(buckets_with_4[&4].objects.list("")).unwrap();
                let count = |of: fn(&Kind) -> bool| {
                    listed.iter().filter(|meta| of(&layout::kind(&meta.key))).count()
                };
                (
                    count(|kind| matches!(kind, Kind::Ephemeral { .. })),
                    count(|kind| matches!(kind, Kind::EphemeralIndex { .. })),
                    count(|kind| matches!(kind, Kind::Decommissioned { .. })),
                )
            };
            assert_eq!(held4(), (0, 0, 1), "root tombstone copied, active index entry not copied");
            let one_with_4 = RoutedBucket::new(
                buckets_with_4[&1].clone(),
                Arc::new(Members(buckets_with_4.clone())),
            );
            let parent4 = CellId { tenant: 1, id: 4 << 48 | 1 };
            let origin4 = Origin { parent: parent4, at: 7, epoch: 1 };
            let refused4 =
                block_on(lifecycle::refer_ephemeral(&one_with_4, origin4, incarnation, child(11)));
            assert_eq!(refused4, Err(crate::Error::Decommissioned(incarnation)));
            assert_eq!(held4(), (0, 0, 1), "taken back in bucket 4");
            let allowed4 = block_on(lifecycle::refer_ephemeral(
                &one_with_4,
                origin4,
                active_incarnation,
                child(12),
            ));
            assert_eq!(allowed4, Ok(()), "active incarnation may still refer in bucket 4");
        }
    }

    /// Resolver that verifies each member bucket's identity object matches the root's registry,
    /// like `Registered::reach`, so routing to a bucket whose `identity::add` has updated
    /// `root/buckets` but not yet written `new/identity` fails with `Mismatched` (and retries).
    struct VerifyingMembers<B: Bucket> {
        root: Arc<B>,
        members: BTreeMap<u16, Arc<B>>,
    }

    impl<B: Bucket> Resolve<B> for VerifyingMembers<B> {
        fn open<'a>(
            &'a self,
            _: &'a B,
            number: u16,
        ) -> futures::future::BoxFuture<'a, Result<Arc<B>, crate::Error>> {
            let member = self.members.get(&number).cloned();
            let root = self.root.clone();
            Box::pin(async move {
                let member = member.ok_or(crate::Error::NoBucket(number))?;
                let registry = crate::identity::registry_once(&*root)
                    .await?
                    .ok_or(crate::Error::NoBucket(number))?;
                let entry = registry.bucket(number).ok_or(crate::Error::NoBucket(number))?;
                crate::identity::verified(entry, crate::identity::get_once(&*member).await?)?;
                Ok(member)
            })
        }
    }

    /// Board 9966 AC #2: a bucket added concurrently with `lifecycle::decommission` ends up with
    /// the incarnation's tombstone and any ephemeral references written there are retired, across
    /// both orderings of the root Dekker barrier and even when decommissioning runs through a
    /// non-root daemon whose local registry copy has not received the new bucket yet.
    #[test]
    fn bucket_added_concurrently_with_decommission_is_tombstoned_and_retired() {
        use crate::layout::{CellId, Kind, Origin};
        use crate::lifecycle::{self, Decommissioned};
        use crate::testing::Fault;
        use std::task::{Context, Poll};

        let incarnation = 0x3b;
        let parent3 = CellId { tenant: 1, id: 3 << 48 | 1 };
        let origin3 = Origin { parent: parent3, at: 7, epoch: 1 };
        let child = |n: u64| CellId { tenant: 1, id: 1 << 48 | 0x200 | n };

        let count_kinds = |bucket: &Direct| {
            let listed = block_on(bucket.list("")).unwrap();
            let count = |of: fn(&Kind) -> bool| {
                listed.iter().filter(|meta| of(&layout::kind(&meta.key))).count()
            };
            (
                count(|kind| matches!(kind, Kind::Ephemeral { .. })),
                count(|kind| matches!(kind, Kind::EphemeralIndex { .. })),
                count(|kind| matches!(kind, Kind::Decommissioned { .. })),
            )
        };

        // Interleaving 1: `identity::add` updates `root/buckets`, lists root tombstones before
        // `decommission` writes the root tombstone (copying none), and finishes writing bucket 3's
        // identity without spreading the registry to bucket 2; a live host refers in bucket 3;
        // then `decommission` runs from `routed(decommissioner)` with the stale pre-add bucket set
        // `{0, 1, 2}` (and also with `ROOT` omitted on a separate check). Its re-read of the root
        // registry discovers bucket 3, tombstones it, and retires the reference there.
        for (decommissioner, omit_root) in [(1, false), (2, false), (2, true)] {
            let buckets: BTreeMap<u16, Arc<Scripted>> =
                [1, 2, 3].into_iter().map(|n| (n, Arc::new(Scripted::new("")))).collect();
            let details = crate::identity::Details::default();
            block_on(async {
                let root = &buckets[&1].objects;
                crate::identity::format(root, &details).await.unwrap();
                let (id2, reg2) =
                    crate::identity::add(root, &details, &buckets[&2].objects, details.clone())
                        .await
                        .unwrap();
                assert_eq!(id2.number, 2);
                // Spread reg2 (only {1, 2}) to bucket 2; never spread bucket 3 to bucket 2.
                crate::identity::spread(&reg2, &buckets[&2].objects).await.unwrap();
            });
            let routed = |home: u16| {
                RoutedBucket::new(
                    buckets[&home].clone(),
                    Arc::new(VerifyingMembers {
                        root: buckets[&1].clone(),
                        members: buckets.clone(),
                    }),
                )
            };
            let through = routed(decommissioner);
            let mut stale_numbers = block_on(through.ephemeral_buckets()).unwrap();
            assert_eq!(stale_numbers, BTreeSet::from([crate::identity::NAMED, 1, 2]));
            if omit_root {
                stale_numbers.remove(&crate::identity::ROOT);
            }

            // Concurrent `identity::add` of bucket 3 finishes before `decommission` writes any
            // tombstone, leaving bucket 2's registry copy stale at {1, 2}.
            block_on(async {
                let (id3, _) = crate::identity::add(
                    &buckets[&1].objects,
                    &details,
                    &buckets[&3].objects,
                    details.clone(),
                )
                .await
                .unwrap();
                assert_eq!(id3.number, 3);
            });
            assert_eq!(count_kinds(&buckets[&3].objects), (0, 0, 0));

            // A live host of `incarnation` forks from a parent in bucket 3 before decommission
            // reaches bucket 3.
            let one = routed(1);
            block_on(lifecycle::refer_ephemeral(&one, origin3, incarnation, child(1))).unwrap();
            assert_eq!(count_kinds(&buckets[&3].objects), (1, 1, 0));

            let done =
                block_on(lifecycle::decommission(&through, &stale_numbers, incarnation, 100))
                    .unwrap();
            assert_eq!(done, Decommissioned { retired: 1, left: 0 });
            assert_eq!(count_kinds(&buckets[&3].objects), (0, 0, 1));
            let refused =
                block_on(lifecycle::refer_ephemeral(&one, origin3, incarnation, child(2)));
            assert_eq!(refused, Err(crate::Error::Decommissioned(incarnation)));
            assert_eq!(count_kinds(&buckets[&3].objects), (0, 0, 1));
        }

        // Interleaving 2: true mid-flight overlap on `SimBucket` — `identity::add` updates
        // `root/buckets` and pauses before listing root tombstones and before writing bucket 3's
        // identity; `decommission` (from non-root bucket 2, with stale `{0, 1, 2}`) writes root's
        // tombstone, re-reads root's registry, sees bucket 3, and hits `Mismatched` -> retries
        // until `identity::add` copies the root tombstone and writes bucket 3's identity.
        let sim1 = SimBucket::new();
        let sim2 = SimBucket::new();
        let sim3 = SimBucket::new();
        let details = crate::identity::Details::default();
        block_on(async {
            let (d1, d2) = (Direct(sim1.clone()), Direct(sim2.clone()));
            crate::identity::format(&d1, &details).await.unwrap();
            let (_, reg2) =
                crate::identity::add(&d1, &details, &d2, details.clone()).await.unwrap();
            crate::identity::spread(&reg2, &d2).await.unwrap();
        });
        let members = BTreeMap::from([(1, sim1.clone()), (2, sim2.clone()), (3, sim3.clone())]);
        let through = RoutedBucket::new(
            sim2.clone(),
            Arc::new(VerifyingMembers { root: sim1.clone(), members: members.clone() }),
        );
        let stale_numbers = BTreeSet::from([crate::identity::NAMED, 1, 2]);

        let mut add_fut = Box::pin(crate::identity::add(&*sim1, &details, &*sim3, details.clone()));
        let mut dec_fut =
            Box::pin(lifecycle::decommission(&through, &stale_numbers, incarnation, 100));
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);

        // Step `add_fut` until it has updated `root/buckets` and enqueued the `list-delimited`
        // of `ephemeral_hosts(ROOT)` (holding that listing in `sim1`'s queue).
        let list_root_tombstones =
            format!("list-delimited {}", layout::ephemeral_hosts(crate::identity::ROOT));
        loop {
            assert!(add_fut.as_mut().poll(&mut cx).is_pending());
            let held = (0..sim1.waiting()).any(|i| sim1.describe(i) == list_root_tombstones);
            if held {
                break;
            }
            for sim in [&sim1, &sim2, &sim3] {
                if sim.waiting() > 0 {
                    sim.complete(0, Fault::None);
                }
            }
        }
        assert!(sim3.waiting() == 0, "bucket 3 identity not written yet");

        // Now step `dec_fut` while holding `add_fut`'s `list-delimited` on `sim1`: `dec_fut`
        // writes the root tombstone, re-reads `root/buckets`, discovers bucket 3, and observes
        // bucket 3 missing `identity` (retrying via `Unavailable`).
        let mut saw_bucket3_identity_probe = false;
        for _ in 0..64 {
            assert!(dec_fut.as_mut().poll(&mut cx).is_pending());
            let mut progressed = false;
            for idx in 0..sim1.waiting() {
                if sim1.describe(idx) != list_root_tombstones {
                    sim1.complete(idx, Fault::None);
                    progressed = true;
                    break;
                }
            }
            if sim2.waiting() > 0 {
                sim2.complete(0, Fault::None);
                progressed = true;
            }
            if sim3.waiting() > 0 {
                if sim3.describe(0).contains(layout::IDENTITY) {
                    saw_bucket3_identity_probe = true;
                }
                sim3.complete(0, Fault::None);
                progressed = true;
            }
            if saw_bucket3_identity_probe || !progressed {
                break;
            }
        }
        assert!(
            saw_bucket3_identity_probe,
            "decommission re-read root registry and probed bucket 3"
        );

        // Now release all requests and drive both futures to completion.
        let mut add_done = None;
        let mut dec_done = None;
        for _ in 0..500 {
            if add_done.is_none()
                && let Poll::Ready(res) = add_fut.as_mut().poll(&mut cx)
            {
                add_done = Some(res);
            }
            if dec_done.is_none()
                && let Poll::Ready(res) = dec_fut.as_mut().poll(&mut cx)
            {
                dec_done = Some(res);
            }
            if add_done.is_some() && dec_done.is_some() {
                break;
            }
            for sim in [&sim1, &sim2, &sim3] {
                while sim.waiting() > 0 {
                    sim.complete(0, Fault::None);
                }
            }
        }
        let (id3, _) = add_done.expect("add finished").unwrap();
        assert_eq!(id3.number, 3);
        assert_eq!(
            dec_done.expect("decommission finished").unwrap(),
            Decommissioned { retired: 0, left: 0 }
        );
        drop(add_fut);
        drop(dec_fut);
        assert_eq!(count_kinds(&Direct(sim3.clone())), (0, 0, 1));

        let one = RoutedBucket::new(
            Arc::new(Direct(sim1.clone())),
            Arc::new(VerifyingMembers {
                root: Arc::new(Direct(sim1.clone())),
                members: BTreeMap::from([
                    (1, Arc::new(Direct(sim1))),
                    (2, Arc::new(Direct(sim2))),
                    (3, Arc::new(Direct(sim3))),
                ]),
            }),
        );
        assert_eq!(
            block_on(lifecycle::refer_ephemeral(&one, origin3, incarnation, child(3))),
            Err(crate::Error::Decommissioned(incarnation))
        );
    }
}
