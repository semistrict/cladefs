// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: what tests of the protocol need (the `testing` feature, TASK-88): a bucket whose
//! requests take effect when the test says ([`SimBucket`]): in any order, failing before or after
//! taking effect, or after whoever made them has gone; a writer's creates of segments, if it says,
//! only once an object at their keys was deleted. local_cell's simulation of doc-7's protocol runs
//! on it, as this crate's own tests do. cladefs: and a DEK objects framed outside a writer are
//! encrypted under, and a ring holding it ([`encrypting`], [`ring`]): nothing is framed in the
//! clear (TASK-411.28).

use crate::bucket::{Bucket, ByteRange, Error, Listing, Meta, Object, Page, PutMode, Version};
use crate::layout::{self, CellId, Kind};
use futures::channel::oneshot;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

enum Op {
    Get(String),
    GetRange(String, ByteRange),
    Head(String),
    List(String),
    /// cladefs: a page of a level of keys (TASK-395.5): where the page before ended, if any, and
    /// how many it holds at most.
    ListDelimited(String, Option<String>, usize),
    Put(String, Vec<u8>, PutMode),
    /// cladefs: offset-checked append (doc-29).
    Append(String, u64, Vec<u8>),
    Delete(String),
}

enum Reply {
    Object(Object),
    Bytes(Vec<u8>),
    Meta(Meta),
    List(Vec<Meta>),
    Listing(Listing),
    Version(Version),
    Offset(u64),
    Done,
}

struct Stored {
    bytes: Vec<u8>,
    version: u64,
    modified: u64,
    appends: u32,
}

struct Request {
    op: Op,
    /// cladefs: the bucket it goes to, of the simulation's regions (TASK-633.10): 0 for one bucket;
    /// and the bucket of the host that asked.
    region: u16,
    from: u16,
    reply: oneshot::Sender<Result<Reply, Error>>,
}

#[derive(Default)]
struct Inner {
    objects: BTreeMap<String, Stored>,
    versions: u64,
    clock: u64,
    queue: Vec<Request>,
    /// The writers whose creates of segments are held ([`SimBucket::hold`]): of each cell, those
    /// of this epoch and before.
    held: BTreeMap<CellId, u64>,
    /// The creates held, out of the queue, each until an object at its key is deleted.
    parked: Vec<Request>,
    /// Each request completed, in order, and what it gave ([`SimBucket::journal`]).
    journal: Vec<String>,
    /// cladefs: the regions' buckets down, whose requests fail without taking effect
    /// ([`SimBucket::down`], TASK-633.10).
    down: BTreeSet<u16>,
    /// cladefs: optional per-object append-part limit (S3 Express `MaxParts`, doc-29).
    max_appends: Option<u32>,
    /// cladefs: deterministic PRNG state for [`Bucket::random_u64`] so simulation seeds replay
    /// identically (board 12982).
    random_state: u64,
}

/// cladefs: the magic a fast WAL segment's header begins with (`local_cell::fast_wal::HEADER_MAGIC`,
/// format `fast wal` 1), by which a writer's create of one is told from a take's tombstone.
const FAST_WAL_HEADER: &[u8] = b"CFWAL001";

/// How a request the simulation completes fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    None,
    /// It fails without taking effect.
    Before,
    /// It takes effect, then fails.
    After,
}

///
/// cladefs: it may hold the buckets of several cloud regions (TASK-633.10), one queue of requests
/// for them all, so that a seed orders them all: a request goes to the bucket its key's cell's ID
/// numbers ([`layout::routed_to`]), as a routed bucket's do, and a key of no cell, or of a cell
/// tests name, to the bucket of the host asking ([`SimBucket::at`]); bucket 0 the one bucket of a
/// simulation of one, which routes nothing, its requests and journal as before regions. A region's
/// bucket may be down ([`SimBucket::down`]).
pub struct SimBucket {
    inner: Arc<Mutex<Inner>>,
    /// cladefs: the bucket of the host asking through it: where keys of no cell go.
    home: u16,
    /// cladefs: whether every request goes to `home`, whatever cell its key is of: the bucket's own
    /// twin ([`SimBucket::own`], TASK-633.8).
    pinned: bool,
    /// cladefs: per-host storage-version ratchet (doc-28).
    ratchet: Arc<formats::Ratchet>,
}

impl Default for SimBucket {
    fn default() -> Self {
        Self {
            inner: Arc::default(),
            home: 0,
            pinned: false,
            ratchet: Arc::new(formats::Ratchet::current()),
        }
    }
}

impl SimBucket {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// cladefs: the same buckets, with a custom storage-version ratchet (doc-28).
    pub fn with_ratchet(&self, ratchet: Arc<formats::Ratchet>) -> Arc<Self> {
        Arc::new(Self { inner: self.inner.clone(), home: self.home, pinned: self.pinned, ratchet })
    }

    /// cladefs: the same buckets, as a host whose bucket is `home`'s asks them (TASK-633.10).
    pub fn at(&self, home: u16) -> Arc<Self> {
        Arc::new(Self {
            inner: self.inner.clone(),
            home,
            pinned: false,
            ratchet: self.ratchet.clone(),
        })
    }

    /// cladefs: its host's bucket's own twin, every request there whatever cell its key is of, as
    /// `RoutedBucket::own` is (TASK-633.8).
    pub fn own(&self) -> Arc<Self> {
        Arc::new(Self {
            inner: self.inner.clone(),
            home: self.home,
            pinned: true,
            ratchet: self.ratchet.clone(),
        })
    }

    /// cladefs: its host's bucket's number.
    pub fn home(&self) -> u16 {
        self.home
    }

    /// cladefs: has bucket `region` be down, or up again: a request to it completed then fails
    /// without taking effect, whatever its completion says.
    pub fn down(&self, region: u16, down: bool) {
        let mut inner = self.inner.lock().unwrap();
        match down {
            true => inner.down.insert(region),
            false => inner.down.remove(&region),
        };
    }

    /// cladefs: sets the maximum number of appends per object before returning `Precondition`
    /// (`MaxParts`, doc-29).
    pub fn set_max_appends(&self, max_appends: Option<u32>) {
        self.inner.lock().unwrap().max_appends = max_appends;
    }

    /// cladefs: the bucket a request of `key` goes to: its cell's, else the host's; for a
    /// simulation of one bucket (0, [`SimBucket::new`]), that one, whatever its cells' numbers.
    fn region_of(&self, key: &str) -> u16 {
        match self.home {
            0 => 0,
            home if self.pinned => home,
            home => layout::routed_to(key).unwrap_or(home),
        }
    }

    /// cladefs: whether waiting request `index` goes to another region's bucket than its asker's.
    pub fn crosses(&self, index: usize) -> bool {
        let inner = self.inner.lock().unwrap();
        let request = &inner.queue[index];
        request.from != 0 && request.region != request.from
    }

    /// cladefs: the bucket waiting request `index` goes to.
    pub fn region(&self, index: usize) -> u16 {
        self.inner.lock().unwrap().queue[index].region
    }

    fn request(&self, op: Op) -> impl Future<Output = Result<Reply, Error>> + Send + use<> {
        let (reply, receiver) = oneshot::channel();
        let region = self.region_of(op.key());
        self.inner.lock().unwrap().enqueue(Request { op, region, from: self.home, reply });
        // A request dropped unanswered failed.
        async move { receiver.await.unwrap_or(Err(Error::Unavailable)) }
    }

    /// How many requests wait, but for those held.
    pub fn waiting(&self) -> usize {
        self.inner.lock().unwrap().queue.len()
    }

    /// Holds the creates of segments of `cell` by its writers of epoch `epoch` and before, cladefs:
    /// of fast WAL segments too,
    /// those waiting and those to come, each until a delete of an object at its key takes effect,
    /// when it waits again: a writer taken over, whose creates are in flight while the next one's
    /// segments take their places, which the collector deletes, landing then
    /// (TASK-79.18).
    pub fn hold(&self, cell: CellId, epoch: u64) {
        let inner = &mut *self.inner.lock().unwrap();
        let held = inner.held.entry(cell).or_default();
        *held = (*held).max(epoch);
        for request in std::mem::take(&mut inner.queue) {
            inner.enqueue(request);
        }
    }

    /// Lets every create held wait again, and holds none from now on.
    pub fn release(&self) {
        let inner = &mut *self.inner.lock().unwrap();
        inner.held.clear();
        inner.queue.append(&mut inner.parked);
    }

    /// Completes waiting request `index`, failing it as `fault` says.
    pub fn complete(&self, index: usize, fault: Fault) {
        let mut inner = self.inner.lock().unwrap();
        let request = inner.queue.swap_remove(index);
        let (region, asked) = (request.region, in_region(request.region, request.op.describe()));
        let fault = if inner.down.contains(&region) { Fault::Before } else { fault };
        let result = match fault {
            Fault::Before => Err(Error::Unavailable),
            Fault::None => inner.apply(region, request.op),
            Fault::After => inner.apply(region, request.op).and(Err(Error::Unavailable)),
        };
        let gave = match &result {
            Ok(_) => "done".to_owned(),
            Err(error) => format!("{error:?}"),
        };
        inner.journal.push(format!("{asked} {fault:?}: {gave}"));
        drop(inner);
        // Its maker may have gone; it took effect regardless.
        let _ = request.reply.send(result);
    }

    /// What waiting request `index` asks: `get KEY START..END` for a ranged read, `get KEY last N`
    /// for its end, and the operation and key for the others.
    pub fn describe(&self, index: usize) -> String {
        let inner = self.inner.lock().unwrap();
        let request = &inner.queue[index];
        in_region(request.region, request.op.describe_key())
    }

    /// Every request completed, in the order they were, each as [`SimBucket::describe`] says,
    /// a put's with its bytes' length, and how it failed, if it did: what a run of a seed of a
    /// simulation asked of the bucket, which another run of that seed must ask the same.
    pub fn journal(&self) -> Vec<String> {
        self.inner.lock().unwrap().journal.clone()
    }

    /// Runs `future`, completing its requests as they come, none failing: what it gives, and what
    /// they asked, in order ([`SimBucket::describe`]).
    pub fn served<T>(&self, future: impl Future<Output = T>) -> (T, Vec<String>) {
        let mut future = std::pin::pin!(future);
        let mut context = Context::from_waker(futures::task::noop_waker_ref());
        let mut asked = Vec::new();
        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
                return (output, asked);
            }
            assert!(self.waiting() > 0, "waiting for no request");
            asked.push(self.describe(0));
            self.complete(0, Fault::None);
        }
    }

    /// Runs `future`, as [`SimBucket::served`] does, but completing in each round every request
    /// waiting: what it gives, and what each round asked, sorted.
    pub fn served_in_rounds<T>(&self, future: impl Future<Output = T>) -> (T, Vec<Vec<String>>) {
        let mut future = std::pin::pin!(future);
        let mut context = Context::from_waker(futures::task::noop_waker_ref());
        let mut rounds = Vec::new();
        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
                return (output, rounds);
            }
            assert!(self.waiting() > 0, "waiting for no request");
            let mut round: Vec<_> = (0..self.waiting()).map(|index| self.describe(index)).collect();
            round.sort();
            while self.waiting() > 0 {
                self.complete(0, Fault::None);
            }
            rounds.push(round);
        }
    }

    /// Runs `future`, as [`SimBucket::served`] does, until the `nth` request (from 1) whose
    /// description is `request` waits: then its maker dies, as a host does, that request and every
    /// other waiting failing without taking effect, and `future` is dropped. What was asked before,
    /// in order; and whether it got there, `future` not ending first.
    pub fn died_before<T>(
        &self,
        future: impl Future<Output = T>,
        (request, nth): (&str, usize),
    ) -> (Vec<String>, bool) {
        let mut future = std::pin::pin!(future);
        let mut context = Context::from_waker(futures::task::noop_waker_ref());
        let (mut asked, mut seen) = (Vec::new(), 0);
        loop {
            if future.as_mut().poll(&mut context).is_ready() {
                return (asked, false);
            }
            assert!(self.waiting() > 0, "waiting for no request");
            if seen + 1 == nth && (0..self.waiting()).any(|index| self.describe(index) == request) {
                while self.waiting() > 0 {
                    self.complete(0, Fault::Before);
                }
                return (asked, true);
            }
            let next = self.describe(0);
            seen += usize::from(next == request);
            asked.push(next);
            self.complete(0, Fault::None);
        }
    }

    /// The first waiting request whose description holds `text`.
    pub fn position(&self, text: &str) -> Option<usize> {
        (0..self.waiting()).find(|&index| self.describe(index).contains(text))
    }

    pub fn advance(&self, ticks: u64) {
        self.inner.lock().unwrap().clock += ticks;
    }

    /// cladefs: steps its clock back `ticks`, as NTP steps a host's (board 8469).
    pub fn step_back(&self, ticks: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.clock = inner.clock.checked_sub(ticks).expect("a clock stepped back past its start");
    }

    /// The keys of every object, of every region's bucket, as its bucket has it, in order.
    pub fn keys(&self) -> Vec<String> {
        let inner = self.inner.lock().unwrap();
        let mut keys: Vec<String> =
            inner.objects.keys().map(|stored| key_of(stored).to_owned()).collect();
        keys.sort();
        keys
    }

    /// cladefs: where every object is kept, a region's bucket but 0's under `@REGION/`.
    pub fn stored(&self) -> Vec<String> {
        self.inner.lock().unwrap().objects.keys().cloned().collect()
    }

    /// cladefs: the size of every object, by where it is kept ([`SimBucket::stored`]): a copy in
    /// one region's bucket and its original in another's apart.
    pub fn sizes(&self) -> BTreeMap<String, u64> {
        let inner = self.inner.lock().unwrap();
        (inner.objects.iter()).map(|(at, stored)| (at.clone(), stored.bytes.len() as u64)).collect()
    }
}

/// cladefs: the key of the object kept at `stored` ([`SimBucket::stored`]), whichever region's
/// bucket keeps it.
pub fn key_of(stored: &str) -> &str {
    match stored.strip_prefix('@').and_then(|rest| rest.split_once('/')) {
        Some((_, key)) => key,
        None => stored,
    }
}

/// cladefs: `described`, a request's description, of the bucket of region `region`: as it is for
/// bucket 0, else after `@REGION `.
fn in_region(region: u16, described: String) -> String {
    match region {
        0 => described,
        region => format!("@{region} {described}"),
    }
}

/// cladefs: whether the object kept at `stored` is of the bucket of region `region`: bucket 0's
/// are kept at their keys, which no other region's prefix `@REGION/` begins, so that a listing of
/// bucket 0 lists none of another region's, a fast WAL bucket's beside it among them (board
/// 14433).
fn of_region(region: u16, stored: &str) -> bool {
    region != 0 || !stored.starts_with('@')
}

/// cladefs: where the object at `key` of the bucket of region `region` is kept: at `key` for bucket
/// 0, else under `@REGION/`.
fn stored_at(region: u16, key: &str) -> String {
    match region {
        0 => key.to_owned(),
        region => format!("@{region}/{key}"),
    }
}

impl Op {
    /// cladefs: its key, or its prefix.
    fn key(&self) -> &str {
        match self {
            Op::Get(key)
            | Op::GetRange(key, _)
            | Op::Head(key)
            | Op::List(key)
            | Op::ListDelimited(key, ..)
            | Op::Put(key, ..)
            | Op::Append(key, ..)
            | Op::Delete(key) => key,
        }
    }

    /// The operation and its key ([`SimBucket::describe`]).
    fn describe_key(&self) -> String {
        match self {
            Op::GetRange(key, ByteRange::Bounded(range)) => format!("get {key} {range:?}"),
            Op::GetRange(key, ByteRange::Suffix(n)) => format!("get {key} last {n}"),
            Op::Get(key) => format!("get {key}"),
            Op::Head(key) => format!("head {key}"),
            Op::List(prefix) => format!("list {prefix}"),
            Op::ListDelimited(prefix, None, _) => format!("list-delimited {prefix}"),
            Op::ListDelimited(prefix, Some(after), _) => {
                format!("list-delimited {prefix} after {after}")
            }
            Op::Put(key, ..) => format!("put {key}"),
            Op::Append(key, offset, _) => format!("append {key} @{offset}"),
            Op::Delete(key) => format!("delete {key}"),
        }
    }

    /// As [`Op::describe_key`], a put's with its bytes' length and its mode, as the journal keeps
    /// it. cladefs: not a hash of its bytes, as before every object was encrypted (TASK-411.28):
    /// each is encrypted under keys and salts drawn at random, a run's bytes not another's, their
    /// lengths the same.
    fn describe(&self) -> String {
        match self {
            Op::Put(key, bytes, mode) => format!("put {key} {}B {mode:?}", bytes.len()),
            Op::Append(key, offset, bytes) => format!("append {key} @{offset} {}B", bytes.len()),
            op => op.describe_key(),
        }
    }
}

impl Inner {
    /// Queues `request`, or holds it, if it is a create of a segment by a writer whose creates are
    /// held ([`SimBucket::hold`]).
    fn enqueue(&mut self, request: Request) {
        // A create's footer is read only for a cell whose creates are held, as only the
        // recreated profile's are.
        let held = |key: &str, bytes: &[u8]| {
            let Kind::Segment { cell, .. } = layout::kind(key) else { return false };
            let Some(&held) = self.held.get(&cell) else { return false };
            layout::Segment::footer(bytes).is_ok_and(|footer| footer.epoch <= held)
        };
        // cladefs: and its creates of fast WAL segments, each beginning with its segment's header
        // (doc-29, board 14433): a writer taken over whose create lands only after a sweep deleted
        // what the take left at its key. Not a take's tombstone fencing the key empty.
        let held_wal = |key: &str, bytes: &[u8]| {
            let Kind::Wal { cell, epoch, .. } = layout::kind(key) else { return false };
            bytes.starts_with(FAST_WAL_HEADER)
                && self.held.get(&cell).is_some_and(|&held| epoch <= held)
        };
        match &request.op {
            Op::Put(key, bytes, PutMode::Create) if held(key, bytes) => self.parked.push(request),
            Op::Append(key, 0, bytes) if held_wal(key, bytes) => self.parked.push(request),
            _ => self.queue.push(request),
        }
    }

    /// The meta of the object kept at `at`, whose key is `key`.
    fn meta(&self, at: &str, key: &str) -> Option<Meta> {
        self.objects.get(at).map(|stored| Meta {
            key: key.to_owned(),
            version: Version { e_tag: Some(stored.version.to_string()), version: None },
            size: stored.bytes.len() as u64,
            modified: stored.modified,
        })
    }

    /// Applies `op` to the bucket of region `region`.
    fn apply(&mut self, region: u16, op: Op) -> Result<Reply, Error> {
        // cladefs: a key longer than buckets take, refused as they refuse it (board 5861).
        if op.key().len() > layout::MAX_KEY {
            return Err(Error::Refused(crate::bucket::Refusal::Request));
        }
        let at = |key: &str| stored_at(region, key);
        // The key of an object kept at `stored`, under `prefix`, kept at `kept`.
        let key_of =
            |prefix: &str, kept: &str, stored: &str| format!("{prefix}{}", &stored[kept.len()..]);
        match op {
            Op::Get(key) => {
                let meta = self.meta(&at(&key), &key).ok_or(Error::NotFound)?;
                Ok(Reply::Object(Object { bytes: self.objects[&at(&key)].bytes.clone(), meta }))
            }
            Op::GetRange(key, range) => {
                let bytes = &self.objects.get(&at(&key)).ok_or(Error::NotFound)?.bytes;
                let len = bytes.len() as u64;
                // Refused as `object_store` refuses it, as though the backend were out of reach: of
                // no bytes, or beginning past the object's end.
                let empty = matches!(&range, ByteRange::Bounded(range) if range.is_empty());
                if empty || !range.begins_within(len) {
                    return Err(Error::Unavailable);
                }
                let range = match range {
                    ByteRange::Bounded(range) => range.start..range.end.min(len),
                    ByteRange::Suffix(n) => len.saturating_sub(n)..len,
                };
                Ok(Reply::Bytes(bytes[range.start as usize..range.end as usize].to_vec()))
            }
            Op::Head(key) => self.meta(&at(&key), &key).map(Reply::Meta).ok_or(Error::NotFound),
            Op::List(prefix) => {
                let kept = at(&prefix);
                Ok(Reply::List(
                    (self.objects.range(kept.clone()..))
                        .take_while(|(stored, _)| stored.starts_with(&kept))
                        .filter(|(stored, _)| of_region(region, stored))
                        .map(|(stored, _)| {
                            self.meta(stored, &key_of(&prefix, &kept, stored)).unwrap()
                        })
                        .collect(),
                ))
            }
            Op::ListDelimited(prefix, after, max) => {
                assert!(prefix.is_empty() || prefix.ends_with('/'), "a prefix of keys: {prefix}");
                let kept = at(&prefix);
                let mut listing = Listing::default();
                let under = self.objects.range(kept.clone()..);
                let under = under.take_while(|(stored, _)| stored.starts_with(&kept));
                for (stored, _) in under.filter(|(stored, _)| of_region(region, stored)) {
                    let key = key_of(&prefix, &kept, stored);
                    match key[prefix.len()..].find('/') {
                        Some(slash) => {
                            let common = &key[..prefix.len() + slash + 1];
                            if listing.prefixes.last().is_none_or(|last| last != common) {
                                listing.prefixes.push(common.to_owned());
                            }
                        }
                        None => listing.objects.push(self.meta(stored, &key).unwrap()),
                    }
                }
                Ok(Reply::Listing(listing.page(Page { after: after.as_deref(), max })))
            }
            Op::Put(key, bytes, mode) => {
                let existing = self.objects.get(&at(&key)).map(|stored| stored.version);
                match (mode, existing) {
                    (PutMode::Create, Some(_)) => return Err(Error::AlreadyExists),
                    (PutMode::Update(_), None) => return Err(Error::Precondition),
                    (PutMode::Update(Version { e_tag, .. }), Some(existing))
                        if e_tag != Some(existing.to_string()) =>
                    {
                        return Err(Error::Precondition);
                    }
                    _ => {}
                }
                self.versions += 1;
                let stored =
                    Stored { bytes, version: self.versions, modified: self.clock, appends: 1 };
                self.objects.insert(at(&key), stored);
                Ok(Reply::Version(Version {
                    e_tag: Some(self.versions.to_string()),
                    version: None,
                }))
            }
            Op::Append(key, offset, bytes) => {
                let target = at(&key);
                if offset == 0 {
                    if self.objects.contains_key(&target) {
                        return Err(Error::Precondition);
                    }
                    let new_len = bytes.len() as u64;
                    self.versions += 1;
                    self.objects.insert(
                        target,
                        Stored { bytes, version: self.versions, modified: self.clock, appends: 1 },
                    );
                    return Ok(Reply::Offset(new_len));
                }
                let Some(existing) = self.objects.get_mut(&target) else {
                    return Err(Error::Precondition);
                };
                if existing.bytes.len() as u64 != offset {
                    return Err(Error::Precondition);
                }
                if self.max_appends.is_some_and(|max| existing.appends >= max) {
                    return Err(Error::Precondition);
                }
                existing.bytes.extend_from_slice(&bytes);
                existing.appends += 1;
                self.versions += 1;
                existing.version = self.versions;
                existing.modified = self.clock;
                Ok(Reply::Offset(existing.bytes.len() as u64))
            }
            Op::Delete(key) => {
                // The creates held at its key wait again once an object there is deleted.
                if self.objects.remove(&at(&key)).is_some() {
                    let (unparked, parked) =
                        std::mem::take(&mut self.parked).into_iter().partition(|request| {
                            request.region == region
                                && matches!(&request.op, Op::Put(at, ..) | Op::Append(at, ..)
                                    if *at == key)
                        });
                    self.parked = parked;
                    self.queue.extend::<Vec<_>>(unparked);
                }
                Ok(Reply::Done)
            }
        }
    }
}

macro_rules! reply {
    ($future:expr, $variant:ident) => {{
        let future = $future;
        async move {
            match future.await? {
                Reply::$variant(value) => Ok(value),
                _ => unreachable!(),
            }
        }
    }};
}

impl Bucket for SimBucket {
    fn get(&self, key: &str) -> impl Future<Output = Result<Object, Error>> + Send {
        reply!(self.request(Op::Get(key.to_owned())), Object)
    }

    fn get_range(
        &self,
        key: &str,
        range: ByteRange,
    ) -> impl Future<Output = Result<Vec<u8>, Error>> + Send {
        reply!(self.request(Op::GetRange(key.to_owned(), range)), Bytes)
    }

    fn head(&self, key: &str) -> impl Future<Output = Result<Meta, Error>> + Send {
        reply!(self.request(Op::Head(key.to_owned())), Meta)
    }

    fn list(&self, prefix: &str) -> impl Future<Output = Result<Vec<Meta>, Error>> + Send {
        reply!(self.request(Op::List(prefix.to_owned())), List)
    }

    fn list_delimited(
        &self,
        prefix: &str,
        page: Page<'_>,
    ) -> impl Future<Output = Result<Listing, Error>> + Send {
        let op = Op::ListDelimited(prefix.to_owned(), page.after.map(str::to_owned), page.max);
        reply!(self.request(op), Listing)
    }

    fn put(
        &self,
        key: &str,
        bytes: bytes::Bytes,
        mode: PutMode,
    ) -> impl Future<Output = Result<Version, Error>> + Send {
        reply!(self.request(Op::Put(key.to_owned(), bytes.into(), mode)), Version)
    }

    fn append(
        &self,
        key: &str,
        offset: u64,
        bytes: bytes::Bytes,
    ) -> impl Future<Output = Result<u64, Error>> + Send {
        reply!(self.request(Op::Append(key.to_owned(), offset, bytes.into())), Offset)
    }

    fn delete(&self, key: &str) -> impl Future<Output = Result<(), Error>> + Send {
        let future = self.request(Op::Delete(key.to_owned()));
        async move { future.await.map(|_| ()) }
    }

    fn now(&self) -> u64 {
        self.inner.lock().unwrap().clock
    }

    fn random_u64(&self) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        // SplitMix64 step from deterministic state so every replay of a seed produces the same
        // run IDs without touching process RNG (board 12982).
        inner.random_state = inner.random_state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = inner.random_state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn mirrors(&self) -> Option<&dyn crate::mirror::Here> {
        (self.home != 0 && !self.pinned).then_some(self as &dyn crate::mirror::Here)
    }

    #[cfg(feature = "full")]
    fn events_object(
        &self,
    ) -> Option<crate::bucket::BucketFuture<'_, Result<Option<Object>, crate::Error>>> {
        if self.home == 0 || self.pinned || self.home == crate::identity::ROOT {
            return None;
        }
        let root = self.at(crate::identity::ROOT);
        Some(Box::pin(async move { Ok(crate::bucket::get(&*root, layout::EVENTS).await?) }))
    }

    fn ratchet(&self) -> &formats::Ratchet {
        &self.ratchet
    }
}

/// cladefs: the mirrors in its host's bucket, in its own bucket (TASK-633.8), as
/// `RoutedBucket`'s.
impl crate::mirror::Here for SimBucket {
    fn marked(
        &self,
        parent: CellId,
        at: u64,
    ) -> futures::future::BoxFuture<'_, Result<bool, crate::Error>> {
        let own = self.own();
        let mine = self.region_of(&parent.prefix()) == self.home;
        Box::pin(async move {
            if mine {
                return Ok(false);
            }
            let Some(object) = crate::bucket::get(&*own, &layout::mirror(parent, at)).await? else {
                return Ok(false);
            };
            Ok(layout::Mirror::decode(&object.bytes)?.is_marked())
        })
    }

    fn create<'a>(
        &'a self,
        key: &'a str,
        bytes: Vec<u8>,
    ) -> futures::future::BoxFuture<'a, Result<(), crate::bucket::Refused>> {
        let own = self.own();
        Box::pin(async move { crate::bucket::create(&*own, key, bytes).await.map(drop) })
    }

    fn retire<'a>(
        &'a self,
        keys: &'a [&'a str],
    ) -> futures::future::BoxFuture<'a, Result<usize, crate::bucket::Refused>> {
        // cladefs: each reference retired in its child's own bucket, as `RoutedBucket` does
        // (board 10446).
        let mut by_bucket: BTreeMap<u16, Vec<&str>> = BTreeMap::new();
        for &key in keys {
            let Kind::Reference { parent, child, .. } = layout::kind(key) else { continue };
            let child_region = self.region_of(&child.prefix());
            if self.region_of(&parent.prefix()) != child_region {
                by_bucket.entry(child_region).or_default().push(key);
            }
        }
        let targets: Vec<(Arc<SimBucket>, Vec<&str>)> =
            by_bucket.into_iter().map(|(region, group)| (self.at(region).own(), group)).collect();
        Box::pin(async move {
            let mut retired = 0;
            for (own, group) in targets {
                crate::lifecycle::retire(&*own, &group).await?;
                retired += group.len();
            }
            Ok(retired)
        })
    }
}

/// The same bucket, its requests taking effect at once and never failing: the simulation's view,
/// to check what the cells hold. cladefs: routed as its bucket's requests are, a region's bucket
/// down or not.
pub struct Direct(pub Arc<SimBucket>);

impl Direct {
    /// cladefs: a listing of a prefix of no cell's lists every region's bucket, as one: the
    /// simulation's view of them all (TASK-633.10), each object where its key routes it, as a
    /// routed bucket's requests reach it: not the copies a region's bucket keeps of other
    /// buckets' cells (TASK-633.8), which a view of its own bucket lists ([`SimBucket::own`]).
    fn apply(&self, op: Op) -> Result<Reply, Error> {
        let region = self.0.region_of(op.key());
        let mut inner = self.0.inner.lock().unwrap();
        match op {
            Op::List(prefix)
                if self.0.home != 0 && !self.0.pinned && layout::routed_to(&prefix).is_none() =>
            {
                let regions: BTreeSet<u16> = (inner.objects.keys())
                    .filter_map(|stored| stored.strip_prefix('@')?.split_once('/')?.0.parse().ok())
                    .collect();
                let mut all = Vec::new();
                for region in regions {
                    match inner.apply(region, Op::List(prefix.clone()))? {
                        Reply::List(listed) => all.extend(listed.into_iter().filter(|meta| {
                            layout::routed_to(&meta.key).is_none_or(|of| of == region)
                        })),
                        _ => unreachable!("a listing"),
                    }
                }
                all.sort_by(|a, b| a.key.cmp(&b.key));
                Ok(Reply::List(all))
            }
            op => inner.apply(region, op),
        }
    }
}

impl Bucket for Direct {
    fn get(&self, key: &str) -> impl Future<Output = Result<Object, Error>> + Send {
        reply!(std::future::ready(self.apply(Op::Get(key.to_owned()))), Object)
    }

    fn get_range(
        &self,
        key: &str,
        range: ByteRange,
    ) -> impl Future<Output = Result<Vec<u8>, Error>> + Send {
        reply!(std::future::ready(self.apply(Op::GetRange(key.to_owned(), range))), Bytes)
    }

    fn head(&self, key: &str) -> impl Future<Output = Result<Meta, Error>> + Send {
        reply!(std::future::ready(self.apply(Op::Head(key.to_owned()))), Meta)
    }

    fn list(&self, prefix: &str) -> impl Future<Output = Result<Vec<Meta>, Error>> + Send {
        reply!(std::future::ready(self.apply(Op::List(prefix.to_owned()))), List)
    }

    fn list_delimited(
        &self,
        prefix: &str,
        page: Page<'_>,
    ) -> impl Future<Output = Result<Listing, Error>> + Send {
        let op = Op::ListDelimited(prefix.to_owned(), page.after.map(str::to_owned), page.max);
        reply!(std::future::ready(self.apply(op)), Listing)
    }

    fn put(
        &self,
        key: &str,
        bytes: bytes::Bytes,
        mode: PutMode,
    ) -> impl Future<Output = Result<Version, Error>> + Send {
        reply!(std::future::ready(self.apply(Op::Put(key.to_owned(), bytes.into(), mode))), Version)
    }

    fn delete(&self, key: &str) -> impl Future<Output = Result<(), Error>> + Send {
        let result = self.apply(Op::Delete(key.to_owned())).map(|_| ());
        std::future::ready(result)
    }

    fn now(&self) -> u64 {
        self.0.now()
    }

    fn random_u64(&self) -> u64 {
        self.0.random_u64()
    }
}

/// cladefs: the ID of the version [`versioned`] names.
pub const VERSION: u64 = 0xa;

/// cladefs: what tests fork versions of (TASK-508.2): `cell`, made, its writer of epoch 10, which
/// commits `extents` in a segment that ends in the version [`VERSION`], uploaded and acknowledged.
/// The writer, and the version's origin.
pub async fn versioned<B: Bucket + 'static>(
    bucket: &Arc<B>,
    cell: CellId,
    extents: Vec<(storage_device::ExtentId, Vec<u8>)>,
) -> (crate::writer::Writer<B>, layout::Origin) {
    crate::lifecycle::create(&**bucket, cell, 0).await.unwrap();
    let writer = crate::writer::Writer::open(bucket.clone(), cell, 10, 0).await.unwrap();
    let at = versioned_by(&writer, extents, VERSION).await;
    let epoch = writer.epoch();
    (writer, layout::Origin { parent: cell, at, epoch })
}

/// cladefs: commits `extents` by `writer` in a segment that ends in the version `id`, uploaded and
/// acknowledged: the segment.
pub async fn versioned_by<B: Bucket + 'static>(
    writer: &crate::writer::Writer<B>,
    extents: Vec<(storage_device::ExtentId, Vec<u8>)>,
    id: u64,
) -> u64 {
    use crate::batch::{Commit, SegmentBuilder};
    let cut = layout::NewVersion { id, keep_last: None };
    let batch = SegmentBuilder::from(Commit { tag: id, extents, ..Commit::default() });
    let framed = batch.cut(cut, &mut writer.chain()).frame(writer.keys().encrypting());
    let at = writer.add_segment(framed).unwrap();
    writer.upload(at).await.unwrap();
    writer.acknowledge().await.unwrap();
    at
}

/// cladefs: a fork of the version `id` of `origin`'s cell to `child`, of `owner`, under the tests'
/// KMS ([`crate::lifecycle::fork_version`]).
pub async fn fork_version(
    bucket: &impl Bucket,
    child: CellId,
    id: u64,
    origin: layout::Origin,
    owner: u64,
) -> Result<(), crate::Error> {
    let kms = crate::keys::testing::host_kms();
    let parents = crate::keys::Parents::Kms(&kms, None);
    crate::lifecycle::fork_version(bucket, child, id, origin, owner, parents).await
}

/// cladefs: gives `child` what [`fork_version`] leaves, for tests that need a child recovering
/// from an origin, whatever version was there: the parent's checkpoint at the origin, which its
/// recovery reads (`recovery::checkpoint_at`); its reference, which keeps that state; and its
/// head, a clone's ([`crate::record::Made::Clone`]), never taken.
pub async fn forked_at(bucket: &impl Bucket, child: CellId, origin: layout::Origin) {
    let kms = crate::keys::testing::host_kms();
    let parents = crate::keys::Parents::Kms(&kms, None);
    crate::recovery::checkpoint_at(bucket, origin.parent, origin.at, parents).await.unwrap();
    let reference = layout::reference(origin.parent, origin.at, child);
    crate::bucket::create(bucket, &reference, layout::encode(&layout::Reference::of(0)))
        .await
        .unwrap();
    let head = layout::Head::of(crate::record::Made::Clone(origin), 0);
    make_head(bucket, child, head).await;
}

/// cladefs: makes `cell`'s head `head` ([`crate::head::birth`]), as a test's maker does: unless a
/// head is there already. The head as it made it.
pub async fn make_head(
    bucket: &impl Bucket,
    cell: CellId,
    head: layout::Head,
) -> Option<crate::head::Held> {
    crate::head::birth(bucket, cell, head).await.ok()
}

/// cladefs: takes `cell`'s head as `holder`, of the placement of `token`, giving it nothing
/// ([`crate::head::take`]): what a test's stale or late taker does.
pub async fn take(
    bucket: &impl Bucket,
    cell: CellId,
    holder: u64,
    token: u64,
) -> Result<crate::head::Held, crate::Error> {
    let taking = crate::head::Taking { holder, token, ..crate::head::Taking::default() };
    crate::head::take(bucket, cell, crate::head::read(bucket, cell).await?, &taking).await
}

/// cladefs: the live cells of every cell of `bucket`, as `cladefs-gc --cells` lists them
/// (TASK-395.4).
pub async fn live(bucket: &impl Bucket) -> Result<BTreeMap<CellId, layout::Head>, crate::Error> {
    use futures::StreamExt as _;
    let mut live = BTreeMap::new();
    let mut cells = std::pin::pin!(crate::cell::listed(bucket));
    while let Some(cell) = cells.next().await {
        let cell = cell?;
        if let Some(head) = crate::head::get(bucket, cell).await?.filter(|head| !head.deleted()) {
            live.insert(cell, head);
        }
    }
    Ok(live)
}

/// cladefs: sweeps of the collector over every cell of `bucket`, each as a holder of its own,
/// until two in a row delete nothing: their reports, summed. Two, as a pass
/// over a dead cell retires the references that kept what its parents' cells reclaim at their next,
/// and a pass retiring them again deletes nothing (TASK-395.4). It fails as a sweep fails, and
/// panics after [`SWEEPS`]: passes that never settle are a bug.
pub async fn collect_until_quiet(
    bucket: &impl Bucket,
) -> Result<crate::collector::Report, crate::Error> {
    let mut collector = crate::collector::Collector::new();
    let (mut total, mut quiet) = (crate::collector::Report::default(), 0);
    for sweep in 0..SWEEPS {
        let report = collector.collect(bucket, 1 << 40 | sweep).await?;
        total += report;
        quiet = if report.deleted == 0 { quiet + 1 } else { 0 };
        if quiet == 2 {
            return Ok(total);
        }
    }
    panic!("{SWEEPS} sweeps of the collector, which kept doing something: {total:?}");
}

/// cladefs: how many sweeps [`collect_until_quiet`] runs at most.
pub const SWEEPS: u64 = 32;

/// cladefs: what the collector should have collected of `bucket` and left (TASK-390), once
/// passes ran alone, the cells' writers all done: each a line, `garbage left: KEY`, an object no
/// live cell needs. The live cells are those whose records say so, recovered as their
/// records say they were made; one that fails to recover is passed over, as a claim's may.
///
/// It works out what the live cells need as doc-7 says, not as the collector does, so that a
/// collector that keeps too much fails it, as one that loses what is live fails the tests' reads:
/// the space a pass frees, not only what it keeps, is checked. It was the simulation's end
/// checks.
///
/// cladefs: per cell and conservatively, as doc-7 has the collector since TASK-395: each origin
/// a reference names, or a live cell's fork to another cell, is needed whole, its state's extents
/// and what recovering it reads, whatever its
/// children hold of it; a reference is garbage once its child is deleted, of that origin, in
/// another cell holding no live cell and no reference; one whose child has no record is garbage
/// at once, but a live cell's fork's, whose writers record it yet: no pass would ever retire it
/// (TASK-395.10); so is one whose child's record names another origin, or none, and one marked
/// retired, which its parent's cell's pass deletes (TASK-395.13); the collector's leases
/// stay, and references' marks. cladefs: but a dead cell's, none marked under it, whose head is a
/// root's or a clone's made, deleted, of an ID an allocator gave, which goes whole, head and lease
/// (TASK-455); and the lease of a cell of no head once nothing else is left.
pub async fn uncollected(bucket: &impl Bucket) -> Vec<String> {
    use layout::Head;
    let keys: BTreeSet<String> =
        crate::bucket::list(bucket, "").await.unwrap().into_iter().map(|meta| meta.key).collect();
    let mut records: BTreeMap<CellId, Head> = BTreeMap::new();
    let mut references = Vec::new();
    // cladefs: the holds of mirrors of its cells' versions in other buckets, each kept for as long
    // as its mirror's copy there (TASK-633.8, [`uncollected_copies`]): which keep their versions
    // whole here, as references do.
    let mut holds = BTreeSet::new();
    for key in &keys {
        match layout::kind(key) {
            Kind::Head { cell, .. } if !records.contains_key(&cell) => {
                if let Ok(Some(head)) = crate::head::get(bucket, cell).await {
                    records.insert(cell, head);
                }
            }
            Kind::Reference { parent, at, child } => references.push((key, parent, at, child)),
            Kind::Hold { parent, at, .. } => drop(holds.insert((parent, at))),
            _ => {}
        }
    }
    // A cell no live cell and no reference keeps, whose pass retires its children's references.
    let dead = |cell: layout::CellId| {
        records.iter().filter(|(of, _)| **of == cell).all(|(_, record)| record.deleted())
            && !references.iter().any(|(_, parent, _, _)| *parent == cell)
            && !holds.iter().any(|&(parent, _)| parent == cell)
    };
    // Whether a cell goes whole: dead, no mark under it, its head one it may go with.
    let marked_under = |cell: layout::CellId| {
        keys.iter().any(|key| match layout::kind(key) {
            Kind::Retired { parent, .. } => parent == cell,
            _ => false,
        })
    };
    let removed = |cell: layout::CellId| {
        let mut held = records.iter().filter(|(of, _)| **of == cell).peekable();
        held.peek().is_some()
            && dead(cell)
            && !marked_under(cell)
            && held.all(|(_, record)| record.removable())
    };
    let recorded = |cell: layout::CellId| records.keys().any(|of| *of == cell);
    let mut needed = BTreeSet::new();
    let mut versions: BTreeSet<(CellId, u64)> = BTreeSet::new();
    let mut retirable = BTreeSet::new();
    for (&cell, record) in &records {
        // Every cell's head stays, a deleted one's final, which fences its writers (TASK-174,
        // TASK-407): but that of a cell that goes whole (TASK-455).
        let head = layout::head(cell);
        if record.deleted() {
            if !removed(cell) {
                needed.insert(head);
            }
            continue;
        }
        needed.insert(head);
        let origin = record.origin();
        let Ok(recovered) = crate::recovery::recover(bucket, cell, origin).await else {
            continue;
        };
        let (epoch, end, checkpoint) = (recovered.epoch(), recovered.end(), recovered.checkpoint());
        let wal_epoch = recovered
            .head
            .as_ref()
            .and_then(|head| head.fast_wal.as_ref())
            .and_then(|fast_wal| fast_wal.active.as_ref())
            .map(|active| active.epoch);
        // cladefs: its versions, each its state at the segment its head names it at, kept whole as
        // an origin is (TASK-476).
        for version in recovered.head.iter().flat_map(|head| &head.versions) {
            versions.insert((cell, version.at));
        }
        needed.extend(recovered.touched);
        // What its head may yet name: of its epoch, or a later one, its segments and extents of
        // their own at or past its end, and its checkpoints newer than its own (TASK-407); plus
        // large own-extents of its active fast WAL epoch at or past its end (doc-29).
        needed.extend(
            keys.iter()
                .filter(|key| match layout::kind(key) {
                    layout::Kind::Segment { cell: of, segment, epoch: by } => {
                        of == cell && by >= epoch && segment >= end
                    }
                    layout::Kind::Extent { cell: of, segment, epoch: by, .. } => {
                        of == cell && (by >= epoch || Some(by) == wal_epoch) && segment >= end
                    }
                    layout::Kind::Checkpoint { cell: of, segment, epoch: by } => {
                        of == cell && by >= epoch && checkpoint.is_none_or(|ck| (segment, by) > ck)
                    }
                    _ => false,
                })
                .cloned(),
        );
        needed.extend(recovered.state.extents.values().map(|location| location.key.clone()));
    }
    // A reference stays while its child's record names its origin, until its cell's pass may
    // retire it, or while a live cell's fork may yet name it or take it back; any other is one
    // no pass retires, kept for ever with its origin (TASK-395.10), and garbage.
    let mut unnamed = BTreeSet::new();
    for &(key, parent, at, child) in &references {
        let named = records.get(&child).is_some_and(|record| {
            record.origin().is_some_and(|o| (o.parent, o.at) == (parent, at))
        });
        let retired = named && records[&child].deleted() && child != parent && dead(child);
        // Marked, retired or taken back before: a stale writer's, written after, garbage
        // (TASK-395.13, TASK-395.14).
        let marked = keys.contains(&layout::retired(parent, at, child));
        if retired {
            retirable.insert(key.clone());
        } else if !marked && named {
            versions.insert((parent, at));
        } else {
            unnamed.insert(key.clone());
        }
    }
    versions.extend(holds.iter().copied());
    // Each origin kept whole, once (TASK-395.4).
    let checkpoints = crate::recovery::Checkpoints::new(bucket, 0);
    for (parent, at) in versions {
        let Ok((state, read)) = crate::recovery::recover_at(&checkpoints, parent, at).await else {
            continue;
        };
        needed.extend(read);
        needed.extend(state.extents.values().map(|location| location.key.clone()));
    }
    let mut left = Vec::new();
    for key in &keys {
        // cladefs: blocks of object IDs taken, and the bucket's identity, for good (TASK-364,
        // TASK-633.9); and the collector's leases and marks, and references not yet retirable
        // (TASK-395); tenants' KEK objects, for good (TASK-411.27).
        let alone = |cell: layout::CellId| {
            let prefix = cell.prefix();
            keys.iter().filter(|other| other.starts_with(&prefix)).count() == 1
        };
        let kept = match layout::kind(key) {
            // cladefs: but a cell's that goes whole; and the lease of a cell of no head alone in
            // it, a removal stopped short of it (TASK-455).
            Kind::CollectorLease { cell } => !removed(cell) && (recorded(cell) || !alone(cell)),
            Kind::ObjectIds { .. }
            | Kind::Identity
            | Kind::Buckets
            | Kind::Kek { .. }
            | Kind::Retired { .. } => true,
            // cladefs: a host's tombstone, for good (board 8519).
            Kind::Decommissioned { .. } => true,
            // cladefs: as its mirror's copy says ([`uncollected_copies`]).
            Kind::Hold { .. } => true,
            // cladefs: fast WAL objects are managed by fast-WAL tombstones, never cold GC (doc-29).
            Kind::Wal { .. } => true,
            Kind::Reference { .. } => !retirable.contains(key) && !unnamed.contains(key),
            Kind::Unknown => key == layout::EVENTS || key == layout::FORMATS,
            _ => false,
        };
        if !kept && !needed.contains(key) {
            left.push(format!("garbage left: {key}"));
        }
    }
    left
}

/// cladefs: what bucket `number`, whose own bucket is `own`, keeps of other buckets' cells that it
/// should not, its collectors done ([`uncollected`]'s, of the copies of mirrors, TASK-633.8), and
/// what it keeps short: `bucket` routed, as its host's requests go, its listing of no cell's
/// prefix every bucket's own cells' objects, as [`Direct`]'s.
///
/// The copy of a cell of another bucket goes whole once its cell is dead (no head, or a deleted
/// one), no mirror pins it, and no child here refers to it (`collect_copy`): until then it stays,
/// but its references whose children are gone or name another origin, which each pass retires.
/// A pin is garbage once nothing runs, as every mirror lets its pin go and a collector begins a
/// stale one's again. A hold of this bucket's in another stays while its version is marked here,
/// and no longer; a marker is never without its hold (`HoldsMarked`, which keeps there what the
/// version inherits), nor its copy short: the checkpoint at the version, what recovering it read
/// of its cell, and each of its cell's extents its state names, where it is here. And a version marked here leaves no live child here depending on its cell's
/// bucket alone (specs/buckets): each refers here.
pub async fn uncollected_copies(
    bucket: &impl Bucket,
    own: &impl Bucket,
    number: u16,
) -> Vec<String> {
    use crate::layout::Origin;
    let mut left = Vec::new();
    let here: BTreeSet<String> =
        crate::bucket::list(own, "").await.unwrap().into_iter().map(|meta| meta.key).collect();
    let mut copies: BTreeMap<CellId, Vec<&String>> = BTreeMap::new();
    for key in &here {
        if layout::routed_to(key).is_some_and(|of| of != number)
            && let Some(cell) = layout::owner(key)
        {
            copies.entry(cell).or_default().push(key);
        }
    }
    let mut marked_mirrors = BTreeSet::new();
    for (&cell, keys) in &copies {
        let alive =
            matches!(crate::head::get(bucket, cell).await, Ok(Some(head)) if !head.deleted());
        let mut pinned = false;
        let mut stray = Vec::new();
        for &key in keys {
            match layout::kind(key) {
                Kind::Pin { .. } => {
                    pinned = true;
                    left.push(format!("pin left: @{number}/{key}"));
                }
                Kind::Mirror { .. } => {
                    if let Ok(Some(obj)) = crate::bucket::get(own, key).await {
                        match layout::Mirror::decode(&obj.bytes) {
                            Ok(m) if m.is_marked() => {
                                marked_mirrors.insert(key.clone());
                            }
                            Ok(_) => {}
                            Err(err) => left.push(format!("marker @{number}/{key}: {err}")),
                        }
                    }
                }
                Kind::Reference { parent, at, child } => {
                    let marked = here.contains(&layout::retired(parent, at, child));
                    let head = crate::head::get(bucket, child).await.ok().flatten();
                    let names = head.as_ref().is_some_and(|head| {
                        head.origin().is_some_and(|o| (o.parent, o.at) == (parent, at))
                    });
                    // Its child deleted and dead, as [`uncollected`] has it of an original: its
                    // pass retires it here, as there (TASK-633.8).
                    let deleted = head.as_ref().is_some_and(|head| head.deleted());
                    if marked || !names || (deleted && dead(bucket, child).await) {
                        stray.push(key);
                    }
                }
                _ => {}
            }
        }
        let references =
            keys.iter().filter(|key| matches!(layout::kind(key), Kind::Reference { .. }));
        let standing = references.count() > stray.len();
        if !(alive || pinned || standing) {
            left.extend(keys.iter().map(|key| format!("copy left: @{number}/{key}")));
            continue;
        }
        left.extend(stray.iter().map(|key| format!("garbage left: @{number}/{key}")));
        for &key in keys {
            let Kind::Mirror { at, .. } = layout::kind(key) else { continue };
            if !marked_mirrors.contains(key) {
                continue;
            }
            if let Err(short) = copied_whole(bucket, own, number, cell, at, &here).await {
                left.push(format!("marker @{number}/{key}: {short}"));
            }
        }
    }
    // Its holds in other buckets, every one's own cells' listed; and its own cells.
    let mut children = BTreeSet::new();
    for meta in crate::bucket::list(bucket, "").await.unwrap() {
        match layout::kind(&meta.key) {
            Kind::Hold { parent, at, bucket: of }
                if of == number && !marked_mirrors.contains(&layout::mirror(parent, at)) =>
            {
                left.push(format!("hold left: {}", meta.key));
            }
            Kind::Head { cell, .. } if layout::routed_to(&meta.key) == Some(number) => {
                children.insert(cell);
            }
            _ => {}
        }
    }
    for child in children {
        let Ok(Some(head)) = crate::head::get(bucket, child).await else { continue };
        let Some(Origin { parent, at, .. }) = head.origin().filter(|_| !head.deleted()) else {
            continue;
        };
        if marked_mirrors.contains(&layout::mirror(parent, at))
            && !here.contains(&layout::reference(parent, at, child))
        {
            left.push(format!(
                "{child} refers to {parent}'s version at {at:x} in its bucket alone, marked here"
            ));
        }
    }
    left
}

/// cladefs: whether `cell` holds no reference of its children's, nor a hold of a mirror of one of
/// its versions in another bucket, in its own bucket, where `bucket` routes it ([`uncollected`]'s
/// dead cell, whose records are deleted).
async fn dead(bucket: &impl Bucket, cell: CellId) -> bool {
    let listed = crate::bucket::list(bucket, &cell.prefix()).await.unwrap();
    !listed.iter().any(|meta| {
        matches!(
            layout::kind(&meta.key),
            Kind::Reference { parent, .. } | Kind::Hold { parent, .. } if parent == cell
        )
    })
}

/// cladefs: whether the copy in bucket `number`, whose own bucket is `own`, holding `here`, of
/// `cell`'s version at `at` is whole, as its marker says ([`uncollected_copies`]): what it lacks if
/// not, or the hold missing.
async fn copied_whole(
    bucket: &impl Bucket,
    own: &impl Bucket,
    number: u16,
    cell: CellId,
    at: u64,
    here: &BTreeSet<String>,
) -> Result<(), String> {
    let prefix = layout::holds(cell, at, number);
    let holds = crate::bucket::list(bucket, &prefix).await.unwrap();
    let has_v33 = holds.iter().any(|meta| {
        matches!(
            layout::kind(&meta.key),
            Kind::Hold { parent, at: held_at, bucket: of }
                if parent == cell && held_at == at && of == number
        )
    });
    let has_legacy = crate::bucket::head(bucket, &layout::legacy_hold(cell, at, number))
        .await
        .unwrap()
        .is_some();
    if !has_v33 && !has_legacy {
        return Err(format!("its hold under {prefix} gone"));
    }
    let marker = crate::bucket::get(own, &layout::mirror(cell, at)).await.unwrap();
    let marker = marker.ok_or("gone")?;
    let decoded = layout::Mirror::decode(&marker.bytes).map_err(|err| err.to_string())?;
    if !decoded.is_marked() {
        return Err("unmarked".to_owned());
    }
    // The version, as its cell's bucket keeps it whole, its hold there.
    let checkpoints = crate::recovery::Checkpoints::new(bucket, 0);
    let (state, read) = crate::recovery::recover_at(&checkpoints, cell, at)
        .await
        .map_err(|err| format!("its version not to be recovered: {err}"))?;
    let ours = |key: &String| layout::owner(key) == Some(cell);
    let needed: Vec<String> = read.into_iter().filter(ours).collect();
    if let Some(lacking) = needed.iter().find(|key| !here.contains(*key)) {
        return Err(format!("{lacking} not copied"));
    }
    let has_checkpoint = here.iter().any(|key| {
        matches!(layout::kind(key), Kind::Checkpoint { cell: c, segment, .. } if c == cell && segment == at)
    });
    if !has_checkpoint {
        return Err(format!("checkpoint for {cell} at {at} not copied"));
    }
    for (&extent, location) in state.extents.iter().filter(|(_, location)| ours(&location.key)) {
        if !here.contains(&location.key) {
            return Err(format!("{extent:?} at {} not copied", location.key));
        }
    }
    Ok(())
}

/// cladefs: the DEK tests frame objects under outside a writer, of every cell (TASK-411.28:
/// nothing is framed in the clear): a reader holding no key reads their metadata as ever, and one
/// holding [`ring`] their data.
pub fn dek() -> &'static extent_frame::Secret {
    static DEK: std::sync::LazyLock<extent_frame::Secret> =
        std::sync::LazyLock::new(|| extent_frame::Secret::from_bytes(&[0x7e; 32]));
    &DEK
}

/// cladefs: how a test frames an object of `cell` outside a writer: under [`dek`], as the cell's
/// DEK of the first generation.
pub fn encrypting(cell: CellId) -> layout::EncryptionKey<'static> {
    (dek(), extent_frame::KeyId { cell: cell.id, generation: crate::keys::FIRST })
}

/// cladefs: a ring holding [`dek`] as the DEK of each of `cells`, which a test's reader reads
/// what [`encrypting`] encrypted under, encrypting as the first's writer.
pub fn ring(cells: impl IntoIterator<Item = CellId>) -> Arc<crate::keys::Ring> {
    let mut cells = cells.into_iter();
    let first = cells.next().expect("a cell, at least");
    let key = |cell: CellId| encrypting(cell).1;
    let others = cells.map(|cell| (key(cell), extent_frame::Secret::from_bytes(dek().expose())));
    let dek = extent_frame::Secret::from_bytes(dek().expose());
    crate::keys::Ring::encrypting_for_tests(first.id, crate::keys::FIRST, dek, others)
}

/// cladefs: the cell whose object is at `key`, by its key.
fn cell_of_key(key: &str) -> Option<CellId> {
    match layout::kind(key) {
        Kind::Segment { cell, .. } | Kind::Extent { cell, .. } | Kind::Checkpoint { cell, .. } => {
            Some(cell)
        }
        _ => None,
    }
}

/// cladefs: [`fork_version`], the child's head given its keys, as a host's fork gives them
/// ([`crate::keys::inheriting`], TASK-411.4): its KEK, its DEK and those it inherits, so that its
/// ring reads no head of its parent's (TASK-633.5), which may be in a bucket out of reach.
pub async fn fork_version_given(
    bucket: &impl Bucket,
    child: CellId,
    id: u64,
    origin: layout::Origin,
    owner: u64,
) -> Result<(), crate::Error> {
    let kms = crate::keys::testing::host_kms();
    let source = ring_of(bucket, origin.parent).await?;
    let forked = layout::Child { cell: child, owner };
    let mut inheriting = crate::keys::inheriting(&kms, &source, forked).await?;
    let parents = crate::keys::Parents::Kms(&kms, None);
    let lifecycle = crate::lifecycle::fork_version_keyed(
        bucket,
        child,
        id,
        origin,
        (owner, Some(&mut inheriting)),
        parents,
    );
    lifecycle.await?;
    inheriting.made();
    Ok(())
}

/// cladefs: the ring a reader of `cell` holds, as a host opening it reads it: its head,
/// its KEK unwrapped by the tests' KMS (`keys::testing`), which every test writer encrypts under
/// (TASK-411.28).
pub async fn ring_of(
    bucket: &impl Bucket,
    cell: CellId,
) -> Result<Arc<crate::keys::Ring>, crate::Error> {
    ring_under(bucket, cell, crate::keys::testing::host_kms()).await
}

/// cladefs: [`ring_of`], its KEK unwrapped by `kms`, a host's that its writer encrypted under.
async fn ring_under(
    bucket: &impl Bucket,
    cell: CellId,
    kms: Arc<crate::keys::CachingKms>,
) -> Result<Arc<crate::keys::Ring>, crate::Error> {
    let head = crate::head::get(bucket, cell).await?.ok_or(crate::Error::NoCell)?;
    let keys = crate::keys::CellKeys::of(cell, &head, kms);
    crate::keys::ring(bucket, &keys, cell, &head, false).await
}

/// cladefs: [`crate::fetch::read`] holding the ring of the cell whose object `location` names
/// ([`ring_of`]): what a test reads an extent's data by, as a host serving its cell would.
pub async fn read<B: Bucket>(
    bucket: &Arc<B>,
    extent: storage_device::ExtentId,
    location: &layout::Location,
) -> Result<Vec<u8>, crate::Error> {
    read_under(bucket, extent, location, crate::keys::testing::kms()).await
}

/// cladefs: [`read`], the cell's KEK unwrapped by `kms`, the KMS its writer encrypted under: a
/// daemon's of another crate's tests, whose host encrypts under its own tests' KMS rather than
/// [`crate::keys::testing::kms`] (board 13675).
pub async fn read_under<B: Bucket>(
    bucket: &Arc<B>,
    extent: storage_device::ExtentId,
    location: &layout::Location,
    kms: extent_keys::kms::Shared,
) -> Result<Vec<u8>, crate::Error> {
    let cell = cell_of_key(&location.key)
        .ok_or_else(|| crate::Error::Corrupt(format!("{} is of no cell", location.key)))?;
    let keys = ring_under(&**bucket, cell, crate::keys::CachingKms::new(kms)).await?;
    crate::fetch::read(bucket, &keys, extent, location).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::test_cell;
    use futures::executor::block_on;

    /// TASK-633.10: the simulation's buckets of regions, one queue for all. A request goes to the
    /// bucket its key's cell numbers, a key of no cell, or of a cell tests name, to the asking
    /// host's; a region's bucket down fails its requests alone, without effect. A simulation of
    /// one bucket, 0, routes nothing, as before regions.
    #[test]
    fn requests_go_to_the_bucket_of_their_region() {
        let sim = SimBucket::new();
        let (first, second) = (sim.at(1), sim.at(2));
        let theirs = CellId { tenant: 1, id: 2 << 48 | 5 };
        let named = CellId { tenant: 1, id: 7 };
        let put = |bucket: &SimBucket, key: &str| {
            let bytes = bytes::Bytes::from_static(b"x");
            bucket.served(bucket.put(key, bytes, PutMode::Create)).1
        };
        let head = layout::head(theirs);
        assert_eq!(put(&first, &head), [format!("@2 put {head}")]);
        assert_eq!(put(&first, layout::IDENTITY), ["@1 put identity"]);
        assert_eq!(put(&second, layout::IDENTITY), ["@2 put identity"]);
        assert_eq!(put(&first, &layout::head(named)), [format!("@1 put {}", layout::head(named))]);
        let listed = second.served(second.list(&theirs.prefix())).0.unwrap();
        assert_eq!(listed.iter().map(|meta| meta.key.clone()).collect::<Vec<_>>(), [head.clone()]);
        assert_eq!(
            sim.stored(),
            [
                "@1/identity".to_owned(),
                format!("@1/{}", layout::head(named)),
                "@2/identity".into(),
                format!("@2/{head}"),
            ]
        );

        // TASK-633.8: a host's own bucket takes a key of another bucket's cell in its bucket.
        let own = first.own();
        let pin = layout::pin(theirs, 3, 1);
        assert_eq!(put(&own, &pin), [format!("@1 put {pin}")]);

        sim.down(2, true);
        let (got, asked) = first.served(first.get(&head));
        assert_eq!(
            (got.map(drop), asked),
            (Err(Error::Unavailable), vec![format!("@2 get {head}")])
        );
        assert!(first.served(first.get(layout::IDENTITY)).0.is_ok(), "bucket 1 down too");
        assert_eq!(sim.journal().last().unwrap(), "@1 get identity None: done");
        assert_eq!(
            sim.journal()[sim.journal().len() - 2],
            format!("@2 get {head} Before: Unavailable")
        );
        sim.down(2, false);
        assert!(first.served(first.get(&head)).0.is_ok());

        // One bucket: bucket 0's, routing nothing, as before regions.
        let alone = SimBucket::new();
        assert_eq!(put(&alone, &head), [format!("put {head}")]);
        assert_eq!(alone.keys(), [head]);
    }

    /// TASK-633.10: a mirror is wanted in its child's bucket alone, of a version of another
    /// bucket's cell ([`crate::mirror::wanted`]): a host of a third bucket opening the child, or
    /// one of the parent's, asks nothing of any bucket; one of the child's mirrors the version,
    /// which, not there, is unnamed.
    #[test]
    fn mirrors_are_wanted_in_their_childs_bucket_alone() {
        use crate::mirror::{Mirrored, wanted};
        let sim = SimBucket::new();
        let cell = |id| CellId { tenant: 1, id };
        let (parent, child) = (cell(1 << 48 | 5), cell(2 << 48 | 6));
        let origin = layout::Origin { parent, at: 3, epoch: 1 };
        let wanted_by = |number: u16, child: CellId| {
            let bucket = sim.at(number);
            let (wanted, asked) =
                bucket.served(wanted(&*bucket, &*bucket.own(), number, child, origin));
            (wanted.unwrap(), asked)
        };
        assert_eq!(wanted_by(3, child), (None, Vec::new()), "a third bucket's host");
        assert_eq!(wanted_by(1, cell(1 << 48 | 7)), (None, Vec::new()), "the parent's bucket");
        let (wanted, asked) = wanted_by(2, child);
        assert_eq!(wanted, Some(Mirrored::Unnamed));
        assert_eq!(
            asked[..2],
            [
                format!("@2 get {}", layout::mirror(parent, 3)),
                format!("@2 list {}", layout::pins(parent, 3)),
            ]
        );
        assert!(asked[2].starts_with(&format!("@2 put {}", layout::pins(parent, 3))), "{asked:?}");
    }

    /// TASK-79.40: the simulation refuses the ranged reads `object_store` refuses, as it does, as
    /// though the backend were out of reach: of no bytes, or starting past the object's end, as its
    /// objects in memory and S3's and GCS's HTTP answers alike; and, as those answers, a suffix of
    /// no bytes, or of an object of none. A range ending past the object's end reads short.
    #[test]
    fn ranges_are_refused_as_object_stores_refuse_them() {
        let sim = Direct(SimBucket::new());
        let memory = std::sync::Arc::new(object_store::memory::InMemory::new());
        let memory = crate::object_bucket::ObjectBucket::new(memory, "");
        for bucket in [&sim as &dyn Ranged, &memory] {
            bucket.put_bytes("key", (0..100).collect());
            bucket.put_bytes("empty", Vec::new());
        }
        for range in [0..10, 90..110, 99..100, 100..110, 150..160, 10..10] {
            let range = ByteRange::Bounded(range);
            let (simulated, refused) = (sim.read("key", &range), memory.read("key", &range));
            assert_eq!(simulated, refused, "{range:?}");
        }
        assert_eq!(sim.read("key", &ByteRange::Suffix(10)), Ok((90..100).collect()));
        assert_eq!(sim.read("key", &ByteRange::Suffix(200)), Ok((0..100).collect()));
        assert_eq!(sim.read("key", &ByteRange::Suffix(0)), Err(Error::Unavailable));
        assert_eq!(sim.read("empty", &ByteRange::Suffix(10)), Err(Error::Unavailable));
    }

    /// board 5861: the simulation refuses a request of a key longer than buckets take, for good,
    /// as they refuse it: a create, a read, a listing; one of the longest it serves.
    #[test]
    fn keys_past_the_longest_are_refused_as_buckets_refuse_them() {
        use crate::bucket::{Bucket as _, PutMode, Refusal};
        use futures::executor::block_on;
        let bucket = Direct(SimBucket::new());
        let long = "k".repeat(layout::MAX_KEY + 1);
        let refused = Err(Error::Refused(Refusal::Request));
        let put = block_on(bucket.put(&long, vec![1].into(), PutMode::Create)).map(drop);
        assert_eq!(put, refused);
        assert_eq!(block_on(bucket.get(&long)).map(drop), refused);
        assert_eq!(block_on(bucket.list(&long)).map(drop), refused);
        let longest = &long[1..];
        block_on(bucket.put(longest, vec![1].into(), PutMode::Create)).unwrap();
        assert_eq!(block_on(bucket.get(longest)).unwrap().bytes, [1]);
    }

    /// TASK-79.73: a create of a cell's segment, its writer's creates held, waits until a delete
    /// of an object at its key takes effect, then waits as any request does; a delete of none
    /// lets it go no more than any other request does.
    #[test]
    fn held_creates_wait_for_objects_at_their_keys_to_be_deleted() {
        let bucket = SimBucket::new();
        bucket.hold(test_cell(2), 1);
        let key = layout::segment(test_cell(2), 4, 1);
        let footer = layout::Chain::new(test_cell(2), 1, 4, 0).close(layout::Contents::default());
        let stale = layout::Segment { data: Vec::new(), footer }.encode(encrypting(test_cell(1)));
        let created = bucket.put(&key, stale.into(), PutMode::Create);
        assert_eq!(bucket.waiting(), 0);
        let deleted = bucket.delete(&key);
        bucket.complete(0, Fault::None);
        assert_eq!((block_on(deleted), bucket.waiting()), (Ok(()), 0));

        block_on(Direct(bucket.clone()).put(&key, vec![1].into(), PutMode::Create)).unwrap();
        let deleted = bucket.delete(&key);
        bucket.complete(0, Fault::None);
        assert_eq!((block_on(deleted), bucket.describe(0)), (Ok(()), format!("put {key}")));
        bucket.complete(0, Fault::None);
        let version = Version { e_tag: Some("2".to_owned()), version: None };
        assert_eq!(block_on(created), Ok(version));
    }

    /// A create that lands and then fails, retried, finds its own object and succeeds; one that
    /// finds another's does not.
    #[test]
    fn ambiguous_creates_are_resolved_by_reading_back() {
        use crate::bucket::{Created, create};
        use futures::executor::LocalPool;
        use futures::task::LocalSpawnExt as _;
        use std::cell::RefCell;
        use std::rc::Rc;

        let sim = SimBucket::new();
        let mut pool = LocalPool::new();
        let results = Rc::new(RefCell::new(Vec::new()));
        for (key, bytes) in [("ours", b"ours"), ("theirs", b"ours")] {
            let (sim, results) = (sim.clone(), results.clone());
            pool.spawner()
                .spawn_local(async move {
                    let created = create(&*sim, key, bytes.to_vec()).await.unwrap();
                    results.borrow_mut().push((key, matches!(created, Created::Created(_))));
                })
                .unwrap();
        }
        pool.run_until_stalled();
        // "theirs" holds another's object before its create lands.
        block_on(Direct(sim.clone()).put("theirs", b"theirs".to_vec().into(), PutMode::Create))
            .unwrap();
        // The first create of each lands and fails; what follows succeeds.
        let mut first = true;
        while sim.waiting() > 0 {
            sim.complete(0, if first { Fault::After } else { Fault::None });
            first = false;
            pool.run_until_stalled();
        }
        let mut results = results.borrow().clone();
        results.sort();
        assert_eq!(results, [("ours", true), ("theirs", false)]);
    }

    /// A bucket the test above reads ranges of, at once.
    trait Ranged {
        fn put_bytes(&self, key: &str, bytes: Vec<u8>);
        fn read(&self, key: &str, range: &ByteRange) -> Result<Vec<u8>, Error>;
    }

    impl<B: Bucket> Ranged for B {
        fn put_bytes(&self, key: &str, bytes: Vec<u8>) {
            block_on(self.put(key, bytes.into(), PutMode::Create)).unwrap();
        }

        fn read(&self, key: &str, range: &ByteRange) -> Result<Vec<u8>, Error> {
            block_on(self.get_range(key, range.clone()))
        }
    }
}

/// What the tests' log took, each record a line: its level, its message, then its key-values,
/// ` key=value` each. Logs from the first call on, at `info` and graver, once a process, as a log
/// is (nextest runs each test in a process of its own).
#[cfg(test)]
pub(crate) fn captured() -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
    use std::sync::{Arc, Mutex, OnceLock};

    struct Captured(Arc<Mutex<Vec<String>>>);

    struct Pairs<'a>(&'a mut String);

    impl<'kvs> log::kv::VisitSource<'kvs> for Pairs<'_> {
        fn visit_pair(
            &mut self,
            key: log::kv::Key<'kvs>,
            value: log::kv::Value<'kvs>,
        ) -> Result<(), log::kv::Error> {
            self.0.push_str(&format!(" {key}={value}"));
            Ok(())
        }
    }

    impl log::Log for Captured {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.level() <= log::Level::Info
        }

        fn log(&self, record: &log::Record<'_>) {
            if self.enabled(record.metadata()) {
                let mut line = format!("{} {}", record.level(), record.args());
                record.key_values().visit(&mut Pairs(&mut line)).unwrap();
                self.0.lock().unwrap().push(line);
            }
        }

        fn flush(&self) {}
    }

    static LINES: OnceLock<Arc<Mutex<Vec<String>>>> = OnceLock::new();
    LINES
        .get_or_init(|| {
            let lines = Arc::default();
            log::set_boxed_logger(Box::new(Captured(Arc::clone(&lines))))
                .expect("the test's process has no log yet");
            log::set_max_level(log::LevelFilter::Info);
            lines
        })
        .clone()
}
