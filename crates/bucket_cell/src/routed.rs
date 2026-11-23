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

    /// Whether this bucket holds a mirror of any version of `cell`, a cell of another bucket. Its
    /// markers' listing failing fails it: taken for none, it would leave a mirror's moves out of
    /// the cell's listings for as long as it is kept (board 5526).
    async fn mirrored(&self, cell: layout::CellId) -> Result<bool, Error> {
        if let Some(&(held, listed)) = self.mirrored.lock().unwrap().get(&cell)
            && listed.elapsed() < MIRRORED_FOR
        {
            return Ok(held);
        }
        let listed = Instant::now();
        let markers = self.home.list(&layout::mirrors(cell)).await?;
        let held = markers
            .iter()
            .any(|meta| matches!(layout::kind(&meta.key), layout::Kind::Mirror { .. }));
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

    async fn lists_here_too(&self, prefix: &str) -> Result<bool, Error> {
        let Some(route) = &self.route else { return Ok(false) };
        let (Some(number), Some(cell)) = (layout::routed_to(prefix), layout::cell_of(prefix))
        else {
            return Ok(false);
        };
        if !prefix.starts_with(&layout::moves_prefix(cell))
            || self.own_number(route).await? == Some(number)
        {
            return Ok(false);
        }
        self.mirrored(cell).await
    }

    /// Whether a listing of moves at `prefix` of a cell mirrored here, `here` what this bucket
    /// listed, is whole without what the cell's bucket holds, which failed to list: the mirror's
    /// copies answer for an object it copied, which the children here read here, with the cell's
    /// bucket out of reach (TASK-633.8, AC #4); for one it did not copy, the cell's bucket alone
    /// can, and its failure fails the listing, which [`crate::bucket::list`] asks again: the moves
    /// made there since taken for none, its reads would find the object gone, lost (board 5526).
    /// A listing of the moves of several objects, [`crate::moves::MovesMaps::read`]'s, a
    /// checkpoint's, is whole of those the mirror copied: what it misses of the others it misses
    /// as it would moves written after it, which readers follow from the object's older location.
    async fn answered_by_the_mirror(&self, prefix: &str, here: &[Meta]) -> Result<bool, Error> {
        let Some(object) = layout::moved_object(prefix) else { return Ok(true) };
        if !here.is_empty() {
            return Ok(true);
        }
        match self.home.head(&object).await {
            Ok(_) => Ok(true),
            Err(Error::NotFound) => Ok(false),
            Err(err) => Err(err),
        }
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

    pub async fn refresh_marks(&self, cell: layout::CellId) {
        self.home.refresh_marks(cell).await
    }

    pub async fn refresh_marking(&self) {
        self.home.refresh_marking().await
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

    async fn get(&self, key: &str) -> Result<Object, Error> {
        if self.read_here_first(key).await?
            && let Ok(object) = self.home.get(key).await
        {
            return Ok(object);
        }
        match self.member(key).await? {
            Some(member) => member.get(key).await,
            None => self.home.get(key).await,
        }
    }

    async fn get_range(&self, key: &str, range: ByteRange) -> Result<Vec<u8>, Error> {
        if self.read_here_first(key).await?
            && let Ok(bytes) = self.home.get_range(key, range.clone()).await
        {
            return Ok(bytes);
        }
        match self.member(key).await? {
            Some(member) => member.get_range(key, range).await,
            None => self.home.get_range(key, range).await,
        }
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        if self.read_here_first(key).await?
            && let Ok(meta) = self.home.head(key).await
        {
            return Ok(meta);
        }
        match self.member(key).await? {
            Some(member) => member.head(key).await,
            None => self.home.head(key).await,
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Meta>, Error> {
        if self.lists_here_too(prefix).await? {
            let mut listed = self.home.list(prefix).await?;
            let there = match self.member(prefix).await {
                Ok(Some(member)) => member.list(prefix).await,
                Ok(None) => Ok(Vec::new()),
                Err(err) => Err(err),
            };
            let there = match there {
                Ok(there) => there,
                Err(err) => {
                    self.answered_by_the_mirror(prefix, &listed).await?.then_some(()).ok_or(err)?;
                    Vec::new()
                }
            };
            let here: BTreeSet<String> = listed.iter().map(|meta| meta.key.clone()).collect();
            listed.extend(there.into_iter().filter(|meta| !here.contains(&meta.key)));
            listed.sort_by(|a, b| a.key.cmp(&b.key));
            return Ok(listed);
        }
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

    async fn head_slots(&self, cell: layout::CellId) -> Result<u64, Refused> {
        let key = layout::heads(cell);
        let mut failures = 0u32;
        loop {
            match self.member(&key).await {
                Ok(Some(member)) => return member.head_slots(cell).await,
                Ok(None) => return self.home.head_slots(cell).await,
                Err(Error::Refused(refusal)) => return Err(Refused::of(&key, refusal)),
                // cladefs: its bucket out of reach: asked again, never another bucket's count
                // taken for it (board 5522).
                Err(_) => {
                    failures = failures.saturating_add(1);
                    self.pause(failures).await;
                }
            }
        }
    }

    fn default_head_slots(&self) -> u64 {
        self.home.default_head_slots()
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

    async fn delete_if(&self, key: &str, version: &Version) -> Result<(), Error> {
        match self.member(key).await? {
            Some(member) => member.delete_if(key, version).await,
            None => self.home.delete_if(key, version).await,
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
    ) -> futures::future::BoxFuture<'_, Result<bool, crate::bucket::Refused>> {
        Box::pin(async move {
            let Some(route) = &self.route else { return Ok(false) };
            let Some(of) = layout::routed_to(&parent.prefix()) else { return Ok(false) };
            if self.own_number_answered(route).await?.is_none_or(|number| number == of) {
                return Ok(false);
            }
            let marker = layout::mirror(parent, at);
            if crate::bucket::get(&*self.home, &marker).await?.is_none() {
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
            let Ok(Some(number)) = self.own_number(route).await else { return Ok(0) };
            let theirs: Vec<&str> = (keys.iter().copied())
                .filter(|key| layout::routed_to(key).is_some_and(|of| of != number))
                .collect();
            if !theirs.is_empty() {
                crate::lifecycle::retire(&*self.home, &theirs).await?;
            }
            Ok(theirs.len())
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
