// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A host's reads of extents from a bucket (doc-7, Reading): by whole [`PART`]s of objects, each
//! a GET of its stored bytes, which the object's table says where are, checked against its CRC
//! and decompressed; one GET for a part however many reads want it at once; GETs in flight
//! bounded in number and bytes, by a [`Budget`] a host's fetchers share, where reads waiting for
//! their data go before read-ahead. An extent is read where its cell names it, or not at all: the
//! collector moves nothing (doc-27). Every read of an extent's data is a fetcher's: a host's, or
//! one of its own ([`read`]).
//!
//! A host's fetcher keeps the parts it fetches in the host's cache, as stored, and those it
//! fetched last in memory too, as fetched, which the first read that wants one unpacks: a part
//! read ahead is unpacked once, by the read it was read ahead of, not read back from the cache and
//! unpacked again (TASK-92.21). What its fetchers keep in memory, those parts and objects' tails,
//! and the GETs in flight, the host's fetchers share ([`Memory`]), as they share its budget and its
//! cache: they keep each once, and send one GET of each, however many of the host's cells read
//! it, as the forks of one base read its parts, and within bounds that hold for the host, however
//! many cells it serves.
//!
//! A checkpoint's extents are looked up a block at a time ([`Fetcher::lookup`]): its tail holds its
//! table, which says where the block that would hold an extent's entry is, read by the parts it is
//! in, as extents' data is (doc-7, Checkpoints).
//!
//! An extent is read where the footer of the object at its key says it is, not where the reader's
//! index does: keys of segments name no writer, so a writer taken over, its create landing late,
//! can create an object at a key the collector deleted, holding extents of its own. Neither CRCs
//! nor keys tell such an object from the one deleted; its footer, which names what it holds, does,
//! and an object that holds no longer what a reader's index says is read as one gone: lost there,
//! as its cell's writer may have relocated the extent since (doc-27). The footer and the table are
//! tied to one version of an object by their CRCs, which the trailer and the table hold, and the
//! parts by theirs, which the table holds.
//!
//! cladefs: what a host's fetchers keep, they keep as stored, encrypted, as a reader holding no key
//! reads it, and open with the keys of every cell the host serves ([`Memory::keys`],
//! `keys::HostRing`, TASK-411.3): an object's tail is opened, its MACs checked, once the host holds
//! its key, and a part unpacked by the tail so opened; a part encrypted under a key the host
//! doesn't hold yet is unread ([`Error::NoKey`]), and read once it does. An object in the clear is
//! corrupt (TASK-411.28); and an extent's own object whose footer names another extent than its
//! key is no object of its.

use crate::Error;
use crate::bucket::{self, Bucket, ByteRange};
use crate::cache::Cache;
use crate::checkpoint::Table;
use crate::keys::{self, HostRing};
use crate::layout::{self, Footer, Kind, Location, PART};
use crate::lru::LruMap;
use crate::object::{self, TAIL};
use crate::peer::{Peer, Peers};
use crate::trace::{self, Source};
use futures::channel::oneshot;
use futures::future::{BoxFuture, FutureExt as _, Shared};
use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use storage_device::ExtentId;

/// Whether a read waits for its data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    Demand,
    Ahead,
}

/// How many requests may be in flight, of how many bytes in all. One goes whatever its size.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub requests: usize,
    pub bytes: u64,
}

impl Limits {
    /// No bound: every request goes at once.
    pub const UNBOUNDED: Self = Self { requests: usize::MAX, bytes: u64::MAX };
}

/// The bytes of `extent`, at `location` in `bucket`, read once, by a fetcher of its own that keeps
/// nothing and bounds no GET: a ranged read of its object's tail, and of each part it is in,
/// checked and decompressed. A host reads through its [`Fetcher`], which keeps the parts it
/// fetches, under its budget. cladefs: holding `keys`, its cell's ring: every object's data is
/// encrypted (TASK-411.28).
pub async fn read<B: Bucket>(
    bucket: &Arc<B>,
    keys: &Arc<keys::Ring>,
    extent: ExtentId,
    location: &Location,
) -> Result<Vec<u8>, Error> {
    let memory = Memory::new();
    let _held = memory.keys().hold(keys.clone());
    let fetcher = Fetcher::new(bucket.clone(), Budget::new(Limits::UNBOUNDED), memory, None);
    fetcher.read(extent, location, 0, location.len, Priority::Demand).await
}

/// Requests in flight, and those waiting to go, demand first, each in the order it came: a host's
/// GETs, which hold the parts they fetch, or the PUTs its writers upload objects with, which hold
/// the objects' bytes ([`Writer::uploading_within`](crate::Writer::uploading_within)).
pub struct Budget {
    limits: Limits,
    queues: Mutex<Queues>,
}

/// cladefs: the budgets a fetcher's GETs go under (TASK-633.4): one for all, or one for each bucket
/// its keys' cells are of ([`crate::objectid::bucket_of`]), each of the same limits, so that a
/// bucket slow or failing, another cloud region's, holds up no GET of another bucket's. Cloned, it
/// is the same budgets, as a host's fetchers share them.
#[derive(Clone)]
pub enum Budgets {
    One(Arc<Budget>),
    PerBucket(Arc<(Limits, Mutex<std::collections::BTreeMap<u16, Arc<Budget>>>)>),
}

impl Budgets {
    /// One budget of `limits` for each bucket.
    pub fn per_bucket(limits: Limits) -> Self {
        Self::PerBucket(Arc::new((limits, Mutex::default())))
    }

    /// The budget a GET of the object at `key` goes under: its cell's bucket's, the cells tests
    /// name and keys of no cell's under number 0's.
    pub fn of(&self, key: &str) -> Arc<Budget> {
        match self {
            Self::One(budget) => budget.clone(),
            Self::PerBucket(per) => {
                let (limits, each) = &**per;
                let number = crate::layout::cell_of(key)
                    .map_or(0, |cell| crate::objectid::bucket_of(cell.id));
                let mut each = each.lock().unwrap();
                each.entry(number).or_insert_with(|| Budget::new(*limits)).clone()
            }
        }
    }
}

impl From<Arc<Budget>> for Budgets {
    fn from(budget: Arc<Budget>) -> Self {
        Self::One(budget)
    }
}

#[derive(Default)]
struct Queues {
    requests: usize,
    bytes: u64,
    /// cladefs: the most bytes in flight at once (TASK-317).
    peak: u64,
    tickets: u64,
    /// By [`Priority`].
    waiting: [VecDeque<Waiter>; 2],
}

struct Waiter {
    ticket: u64,
    bytes: u64,
    go: oneshot::Sender<Slot>,
}

/// A request's place in its budget, given up when it is dropped.
pub struct Slot {
    budget: Option<Arc<Budget>>,
    bytes: u64,
}

impl Drop for Slot {
    fn drop(&mut self) {
        if let Some(budget) = self.budget.take() {
            let mut queues = budget.queues.lock().unwrap();
            queues.requests -= 1;
            queues.bytes -= self.bytes;
            budget.admit(&mut queues);
        }
    }
}

impl Budget {
    pub fn new(limits: Limits) -> Arc<Self> {
        Arc::new(Self { limits, queues: Mutex::default() })
    }

    /// What it lets go at once.
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// cladefs: the bytes its requests in flight hold now, and the most they held at once
    /// (TASK-317): within its limit, but for a request larger than it, which goes alone.
    pub fn held(&self) -> (u64, u64) {
        let queues = self.queues.lock().unwrap();
        (queues.bytes, queues.peak)
    }

    /// A place for a request of `bytes`: its ticket, by which [`Budget::promote`] finds it, and the
    /// wait for it.
    pub(crate) fn enter(
        self: &Arc<Self>,
        priority: Priority,
        bytes: u64,
    ) -> (u64, impl Future<Output = Slot> + Send + use<>) {
        let (go, given) = oneshot::channel();
        let mut queues = self.queues.lock().unwrap();
        queues.tickets += 1;
        let ticket = queues.tickets;
        queues.waiting[priority as usize].push_back(Waiter { ticket, bytes, go });
        self.admit(&mut queues);
        // Its sender goes only with a slot, and the budget, which holds it, outlives this.
        (ticket, async move { given.await.expect("a waiter is given its place") })
    }

    /// cladefs: a place for `bytes` held outside a request, as long as what it returns is: what a
    /// writer's relocation round holds read (doc-27), in the budget of the host's uploads, behind
    /// them.
    pub async fn hold(self: &Arc<Self>, priority: Priority, bytes: u64) -> Slot {
        self.enter(priority, bytes).1.await
    }

    /// Moves the request of `ticket`, if it waits behind demand, up to it.
    fn promote(self: &Arc<Self>, ticket: u64) {
        let mut queues = self.queues.lock().unwrap();
        let ahead = &mut queues.waiting[Priority::Ahead as usize];
        if let Some(at) = ahead.iter().position(|waiter| waiter.ticket == ticket) {
            let waiter = ahead.remove(at).unwrap();
            queues.waiting[Priority::Demand as usize].push_back(waiter);
            self.admit(&mut queues);
        }
    }

    /// Removes a queued waiter (`ticket`) whose callers all gave up so it leaves the queue
    /// immediately (TASK-542, board 2480).
    fn cancel(self: &Arc<Self>, ticket: u64) {
        let mut queues = self.queues.lock().unwrap();
        for queue in &mut queues.waiting {
            if let Some(at) = queue.iter().position(|waiter| waiter.ticket == ticket) {
                queue.remove(at);
                return;
            }
        }
    }

    /// Lets the requests waiting first go, as far as the limits allow.
    fn admit(self: &Arc<Self>, queues: &mut Queues) {
        while let Some(queue) = queues.waiting.iter_mut().find(|queue| !queue.is_empty()) {
            let bytes = queue[0].bytes;
            let fits =
                queues.requests < self.limits.requests && queues.bytes + bytes <= self.limits.bytes;
            if queues.requests > 0 && !fits {
                return;
            }
            let waiter = queue.pop_front().unwrap();
            queues.requests += 1;
            queues.bytes += bytes;
            queues.peak = queues.peak.max(queues.bytes);
            let slot = Slot { budget: Some(self.clone()), bytes };
            // Its reader gave up: its place goes to the next.
            if let Err(mut slot) = waiter.go.send(slot) {
                slot.budget = None;
                queues.requests -= 1;
                queues.bytes -= bytes;
            }
        }
    }
}

/// An object's frame, where its parts are stored, and where in its data each extent it holds is,
/// as its footer says, or, a checkpoint's, its table: its tail. cladefs: as read, by a reader
/// holding no key, which a peer's asking goes by; and opened under the host's keys once they hold
/// its key (TASK-411.3).
#[derive(Debug)]
struct Tail {
    raw: object::Tail,
    /// Its parts as a reader holding its key reads them, its MACs checked: once the host holds it.
    opened: OnceLock<extent_frame::Parts>,
    /// Each extent's offset in its data, and its length.
    extents: HashMap<ExtentId, (u64, u64)>,
    /// A checkpoint's table, which its footer is.
    table: Option<Table>,
}

impl Tail {
    /// The tail of the object at `key`, as read, `tail`: a segment's footer names the extents
    /// sealed in it; an extent stored on its own is its
    /// object's whole data, which its key names; a checkpoint's footer is its table. Other objects
    /// hold no extents. cladefs: an extent's footer names it: one naming another is refused, a
    /// whole object copied to another key (TASK-411.3).
    fn of(key: &str, tail: object::Tail) -> Result<Self, Error> {
        let parts = &tail.parts;
        let footer = tail.footer();
        let (extents, table) = match layout::kind(key) {
            // cladefs: and those of the commit it goes on with, sealed in it too (TASK-92.41).
            Kind::Segment { .. } => (
                (Footer::decode(footer, layout::FORMAT)?.contents.runs())
                    .map(|&(extent, offset, len)| (extent, (offset, len)))
                    .collect(),
                None,
            ),
            Kind::Extent { cell, sequence, .. } => {
                let extent = ExtentId::new(cell.id, sequence);
                let named = layout::decode_own_extent_footer(footer)?;
                if named != extent {
                    return Err(Error::Corrupt(format!("{key} holds extent {named:?}")));
                }
                (HashMap::from([(extent, (0, parts.frame.data_len))]), None)
            }
            Kind::Checkpoint { .. } => (HashMap::new(), Some(Table::decode(footer)?)),
            _ => (HashMap::new(), None),
        };
        Ok(Self { raw: tail, opened: OnceLock::new(), extents, table })
    }

    /// Where its parts are stored, as read.
    fn parts(&self) -> &extent_frame::Parts {
        &self.raw.parts
    }

    /// Part `part` of the object at `key`, as read: where it is stored, and how long it is
    /// unpacked.
    fn part(&self, key: &str, part: u64) -> Result<extent_frame::Part, Error> {
        part_of(self.parts(), key, part)
    }

    /// cladefs: its parts, as a reader holding `ring`, the host's keys now, reads them: opened, its
    /// table and footer checked against their MACs, under its key if `ring` holds it, and kept so;
    /// as read otherwise, an encrypted one's unread. Refused if it is authenticated under a key it
    /// doesn't hold, and `ring` holds the object's cell encrypted (TASK-411.3).
    fn opened(&self, ring: &HostRing, key: &str) -> Result<Cow<'_, extent_frame::Parts>, Error> {
        if let Some(opened) = self.opened.get() {
            return Ok(Cow::Borrowed(opened));
        }
        let ring = ring.ring();
        let parts = self.raw.open(&keys::of_key(&ring, key))?;
        match parts.opens() {
            true => Ok(Cow::Borrowed(self.opened.get_or_init(|| parts))),
            false => Ok(Cow::Owned(parts)),
        }
    }

    /// About the bytes it takes in memory: where its parts are stored, where its extents are, and
    /// a checkpoint's table, which takes about what its footer does as read.
    fn size(&self) -> u64 {
        use std::mem::size_of;
        // A map's entries, and a byte of control each, in a table an eighth larger than they fill.
        let extents = self.extents.capacity() * (size_of::<(ExtentId, (u64, u64))>() + 1) * 8 / 7;
        // cladefs: as read, and opened, and its bytes as read (TASK-411.3).
        let parts = 2 * self.parts().stored.capacity() * size_of::<extent_frame::Stored>();
        let table = self.table.as_ref().map_or(0, |_| self.parts().frame.footer_len as usize);
        (size_of::<Self>() + extents + parts + table + self.raw.bytes.len()) as u64
    }
}

/// Part `part` of `parts`, the object at `key`'s: where it is stored, how long it is unpacked, and
/// how it is opened.
fn part_of(parts: &extent_frame::Parts, key: &str, part: u64) -> Result<extent_frame::Part, Error> {
    (parts.part(part)).map_err(|_| Error::Corrupt(format!("part {part} of {key}, past its end")))
}

/// What a host's fetchers keep in memory, which they share, as they share its budget and its
/// cache: objects' tails, and the parts fetched/unpacked last, each kept once for the host however
/// many of its cells read it, within bounds that hold for the host, not for each cell (TASK-92.21,
/// TASK-92.22, board 6026); and the GETs of parts in flight, which the reads of all of them that
/// want a part while its GET is in flight share (TASK-92.43).
pub struct Memory {
    tails: Mutex<Tails>,
    recent: Arc<PartMemory>,
    parts: Mutex<Parts>,
    /// cladefs: the keys of the cells the host serves, which what it keeps is opened with
    /// (TASK-411.3).
    keys: Arc<HostRing>,
    /// cladefs: where the host's fetchers found the parts and tails they fetched.
    fetches: Fetches,
}

impl Default for Memory {
    fn default() -> Self {
        Self::of(PartMemory::new(RECENT))
    }
}

/// cladefs: where a host's fetchers found the parts and the tails they fetched, since its start,
/// for its cells' reads and its peers' alike, each counted once however many reads shared it: of
/// each [`Source`], how many parts and their stored bytes, and how many tails. A part kept in memory
/// counts as one read found it there; a read joining another's fetch in flight counts nothing. A
/// tail counts only as read from the part cache or the bucket, never kept in memory, which every
/// read of an object finds; and by no bytes: one from the bucket may take two GETs, of more than it
/// keeps.
#[derive(Default)]
struct Fetches {
    parts: [Counter; 4],
    tails: [AtomicU64; 4],
}

#[derive(Default)]
struct Counter {
    count: AtomicU64,
    bytes: AtomicU64,
}

impl Counter {
    fn add(&self, bytes: u64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    fn load(&self) -> Tally {
        Tally {
            count: self.count.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
        }
    }
}

/// The place of `source` among counts kept one for each [`Source`].
fn at(source: Source) -> usize {
    source as usize - 1
}

/// cladefs: how many parts or tails a host's fetchers found at a [`Source`], and their stored bytes
/// ([`Memory::fetches`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    /// How many.
    pub count: u64,
    /// Their stored bytes, as the bucket holds them.
    pub bytes: u64,
}

/// cladefs: where a host's fetchers found the parts and tails they fetched ([`Memory::fetches`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FetchCounts {
    parts: [Tally; 4],
    tails: [u64; 4],
}

impl FetchCounts {
    /// The parts found at `source`.
    pub fn parts(&self, source: Source) -> Tally {
        self.parts[at(source)]
    }

    /// The tails read from `source`: none in memory, as those are not counted.
    pub fn tails(&self, source: Source) -> u64 {
        self.tails[at(source)]
    }
}

impl Memory {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// A fetcher memory sharing `recent` with local framed part unpacking (board 6026).
    pub fn with_parts(recent: Arc<PartMemory>) -> Arc<Self> {
        Arc::new(Self::of(recent))
    }

    /// A fetcher memory keeping parts in `recent`.
    fn of(recent: Arc<PartMemory>) -> Self {
        Self {
            tails: Mutex::default(),
            recent,
            parts: Mutex::default(),
            keys: Arc::default(),
            fetches: Fetches::default(),
        }
    }

    /// cladefs: where the host's fetchers found the parts and tails they fetched, since its start
    /// ([`Fetches`]).
    pub fn fetches(&self) -> FetchCounts {
        let Fetches { parts, tails } = &self.fetches;
        FetchCounts {
            parts: std::array::from_fn(|at| parts[at].load()),
            tails: std::array::from_fn(|at| tails[at].load(Ordering::Relaxed)),
        }
    }

    /// cladefs: counts a part of `bytes` stored bytes found at `source`.
    fn found_part(&self, source: Source, bytes: u64) {
        trace::note(Some(source));
        self.fetches.parts[at(source)].add(bytes);
    }

    /// cladefs: counts a tail read from `source`.
    fn found_tail(&self, source: Source) {
        self.fetches.tails[at(source)].fetch_add(1, Ordering::Relaxed);
    }

    /// Bytes held in cached object tails.
    pub fn tails_bytes(&self) -> u64 {
        self.tails.lock().unwrap().bytes
    }

    /// Evicts cached object tails least-recently-used first down to `target` bytes, returning the
    /// bytes still held.
    pub fn evict_tails_to(&self, target: u64) -> u64 {
        let mut tails = self.tails.lock().unwrap();
        tails.evict_to(target);
        tails.bytes
    }

    /// Bytes held in the unified recent/unpacked part cache.
    pub fn recent_bytes(&self) -> u64 {
        self.recent.bytes()
    }

    /// Peak bytes held in the unified recent/unpacked part cache.
    pub fn recent_peak(&self) -> u64 {
        self.recent.peak()
    }

    /// Current byte limit of the unified recent/unpacked part cache.
    pub fn recent_limit(&self) -> u64 {
        self.recent.limit()
    }

    /// Resizes the unified recent/unpacked part cache to `limit` bytes, evicting least-recently-used
    /// entries down to `limit`, and returns the bytes still held (board 12313).
    pub fn size_recent_to(&self, limit: u64) -> u64 {
        self.recent.size_to(limit)
    }

    /// cladefs: the keys of the cells the host serves, which its cells hold theirs in as they open
    /// (`local_cell::upload`, TASK-411.3).
    pub fn keys(&self) -> &Arc<HostRing> {
        &self.keys
    }

    /// cladefs: drops what it keeps of the objects of `scope`'s cells, as a host told a tenant's
    /// KEK is revoked or destroyed, or a cell shredded, does (doc-16, Shredding; TASK-411.7): their
    /// tails, opened under keys it may hold no more, and the parts fetched last, unpacked, in the
    /// clear. Read again, they are opened under the keys the host holds then: those other cells
    /// hold, as a fork of another tenant's keeps what it inherited, and no others
    /// ([`HostRing::forget`]). GETs in flight finish as they would. Returns how many tails and
    /// parts it dropped.
    pub fn forget_kept(&self, scope: keys::Scope) -> usize {
        self.forget_kept_where(|cell| scope.covers(cell))
    }

    /// cladefs: drops cached object tails and unpacked parts for every cell matching `matches`
    /// (board 11667: scoped multi-bucket per-KMS revocation). Returns how many tails and parts it
    /// dropped.
    pub fn forget_kept_where(&self, matches: impl Fn(layout::CellId) -> bool) -> usize {
        let of = |key: &str| layout::cell_of(key).is_some_and(&matches);
        let tails = {
            let mut tails = self.tails.lock().unwrap();
            let doomed: Vec<String> = tails.kept.keys().filter(|key| of(key)).cloned().collect();
            for key in &doomed {
                tails.remove(key, None);
            }
            doomed.len()
        };
        let parts = self.recent.forget_bucket(of);
        tails + parts
    }
}

/// How many bytes of objects' tails a host's fetchers keep in memory, those used least recently
/// going first: and kept in the cache, if they keep one, from which they are read again
/// (TASK-92.22).
const TAILS: u64 = 64 << 20;

/// Objects' tails the host's fetchers keep in memory, or the reads of them in flight, by key, least
/// recently used first, and the bytes they take ([`Tail::size`]): [`TAIL`] for a read in flight,
/// which reads as many at first. None is the tail of an object gone.
#[derive(Default)]
struct Tails {
    kept: LruMap<String, KeptTail>,
    bytes: u64,
}

struct KeptTail {
    /// Which read of it it is.
    read: u64,
    size: u64,
    flight: TailState,
}

// Only active callers own a pending GET and its budget slot.
enum TailState {
    /// Its read, and, if it went as read-ahead, its place in the budget it waits in, which a read
    /// that waits for it moves up to demand (TASK-702.3).
    Reading(
        futures::future::WeakShared<BoxFuture<'static, Sourced<Option<Arc<Tail>>>>>,
        Option<(Arc<Budget>, u64)>,
    ),
    Ready(Arc<Tail>),
}

impl Tails {
    /// The read of the tail of the object at `key`, kept or in flight, if there is one: used now,
    /// and moved up to demand if it waits behind it and `priority` is demand.
    fn get(&mut self, key: &str, priority: Priority) -> Option<Flight<Option<Arc<Tail>>>> {
        let kept = self.kept.get_mut(key)?;
        match &kept.flight {
            TailState::Reading(flight, ahead) => {
                if let Some((budget, ticket)) =
                    ahead.as_ref().filter(|_| priority == Priority::Demand)
                {
                    budget.promote(*ticket);
                }
                flight.upgrade()
            }
            TailState::Ready(tail) => {
                let kept = (Ok(Some(tail.clone())), Some(Source::Memory));
                Some(futures::future::ready(kept).boxed().shared())
            }
        }
    }

    /// The number of the next read of a tail.
    fn next_read(&mut self) -> u64 {
        self.kept.tick()
    }

    /// Keeps `flight`, read `read` of the tail of the object at `key`, now going, its place in a
    /// budget as read-ahead `ahead`, if it went so.
    fn insert(
        &mut self,
        key: &str,
        read: u64,
        flight: Flight<Option<Arc<Tail>>>,
        ahead: Option<(Arc<Budget>, u64)>,
    ) {
        self.remove(key, None);
        let flight = TailState::Reading(flight.downgrade().expect("new tail read"), ahead);
        self.kept.insert(key.to_owned(), KeptTail { read, size: 0, flight });
        self.settle(key, read, TAIL);
    }

    /// Read `read` of the tail of the object at `key` takes `size` bytes, unless it was dropped
    /// since: those used least recently go while they all take more than [`TAILS`].
    fn settle(&mut self, key: &str, read: u64, size: u64) {
        let Some(kept) = self.kept.peek_mut(key).filter(|kept| kept.read == read) else { return };
        self.bytes = self.bytes - kept.size + size;
        kept.size = size;
        self.evict_to(TAILS);
    }

    fn evict_to(&mut self, target: u64) {
        while self.bytes > target
            && let Some((_, dropped)) = self.kept.pop_oldest()
        {
            self.bytes -= dropped.size;
        }
    }

    /// Drops the tail of the object at `key`: if it is read `read`, if one is given.
    fn remove(&mut self, key: &str, read: Option<u64>) {
        if let Some(kept) = self.kept.peek(key)
            && read.is_none_or(|read| read == kept.read)
        {
            let kept = self.kept.remove(key).unwrap();
            self.bytes -= kept.size;
        }
    }
}

/// A GET, or what it found, which reads that come while it is in flight share; cladefs: with where
/// it went for it, which each of its readers notes as it lands ([`land`], TASK-672.2).
type Flight<T> = Shared<BoxFuture<'static, Sourced<T>>>;

/// What a flight found, and the farthest it went for it ([`trace::sourced`]).
type Sourced<T> = (Result<T, Error>, Option<Source>);

/// `read` in flight, gathering where it goes ([`trace::sourced`]), for its readers to share.
fn fly<T: Clone + 'static>(
    read: impl Future<Output = Result<T, Error>> + Send + 'static,
) -> Flight<T> {
    trace::sourced(read).boxed().shared()
}

/// What `flight` found, its reader noting where it went for it ([`trace::note`]): the reader that
/// started it and those that joined it alike, so that each read says it waited for the bucket.
async fn land<T: Clone>(flight: Flight<T>) -> Result<T, Error> {
    let (found, source) = flight.await;
    trace::note(source);
    found
}

/// A part of an object, read by its tail.
#[derive(Clone)]
enum FetchedPart {
    /// Its bytes (either stored ciphertext/compressed or already unpacked plaintext).
    Data(Fetched),
    /// The object is gone.
    Gone,
}

/// How many zstd-compressed bucket parts this process unpacked (decompressed) since it started
/// (board 12313): mirrors `local_cell::framed::unpacks()`.
// The process's count, every simulated host's together, as `local_cell::framed::UNPACKS` is.
static UNPACKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many zstd-compressed bucket parts this process unpacked (decompressed) since it started.
pub fn unpacks() -> u64 {
    UNPACKS.load(std::sync::atomic::Ordering::Relaxed)
}

#[derive(Clone)]
enum PartBacking {
    Slice(Arc<[u8]>),
    Boxed(Arc<Box<[u8]>>),
    Owned(Arc<dyn AsRef<[u8]> + Send + Sync>),
}

/// Unpacked part bytes kept in memory, counted by their held capacity (board 6026).
#[derive(Clone)]
pub struct PartBytes {
    bytes: PartBacking,
    held: u64,
}

impl PartBytes {
    /// Wraps an `Arc<[u8]>` slice without copying.
    pub fn from_arc(bytes: Arc<[u8]>) -> Self {
        let held = bytes.len() as u64;
        Self { bytes: PartBacking::Slice(bytes), held }
    }

    /// Wraps an owned `Box<[u8]>` without copying the slice bytes (board 12313), allowing
    /// [`PartMemory::take_spare_buf`] to reclaim and reuse the buffer on LRU eviction.
    pub fn from_box(bytes: Box<[u8]>) -> Self {
        let held = bytes.len() as u64;
        Self { bytes: PartBacking::Boxed(Arc::new(bytes)), held }
    }

    /// Wraps an arbitrary shared byte buffer (such as an aligned direct-I/O buffer) with its held
    /// allocation capacity in bytes.
    pub fn from_owned<T: AsRef<[u8]> + Send + Sync + 'static>(bytes: Arc<T>, held: u64) -> Self {
        Self { bytes: PartBacking::Owned(bytes), held }
    }

    /// The bytes held in memory for governor accounting.
    pub fn held(&self) -> u64 {
        self.held
    }
}

impl std::ops::Deref for PartBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match &self.bytes {
            PartBacking::Slice(bytes) => bytes,
            PartBacking::Boxed(bytes) => bytes,
            PartBacking::Owned(bytes) => (**bytes).as_ref(),
        }
    }
}

/// A part retained in a single representation (board 6026): either its stored bytes (fetched ahead
/// or for peer serving, before unpacking) or its unpacked plaintext bytes (after a demand read),
/// never both at once.
#[derive(Clone)]
enum Fetched {
    Stored { bytes: Arc<[u8]> },
    Unpacked(PartBytes),
}

impl Fetched {
    fn size(&self) -> u64 {
        match self {
            Self::Stored { bytes } => bytes.len() as u64,
            Self::Unpacked(plain) => plain.held(),
        }
    }
}

/// How many bytes of parts the unified recent/unpacked part cache keeps in memory (board 6026).
pub const RECENT: u64 = 64 << 20;

/// Per-entry key/tree metadata overhead charged for small stored parts so many tiny compressed
/// parts cannot overrun [`PartMemory`]'s budget.
const STORED_ENTRY_OVERHEAD: u64 = 128;

/// Key of a part in the unified [`PartMemory`] cache (board 6026).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PartKey {
    /// A part of a local framed file: `(opening_id, part_number)`.
    Local(u64, u64),
    /// A part of a bucket object: `(object_key, part_number)`. Shared via `Arc<str>` so `LruMap`
    /// keeps one copy of the key string.
    Bucket(Arc<str>, u64),
}

impl PartKey {
    fn stored_overhead(&self) -> u64 {
        match self {
            Self::Local(_, _) => STORED_ENTRY_OVERHEAD,
            Self::Bucket(key, _) => key.len() as u64 + STORED_ENTRY_OVERHEAD,
        }
    }
}

/// Shared, governed LRU cache of recent bucket parts and unpacked local framed parts (board 6026,
/// board 12313).
///
/// Uses `std::sync::Mutex` so it is safe to lock from `Drop` (such as `local_cell::framed::Framed`)
/// without creating Shuttle schedule points (TASK-675).
pub struct PartMemory {
    kept: Mutex<Recent>,
}

impl Default for PartMemory {
    fn default() -> Self {
        Self { kept: Mutex::new(Recent::new(RECENT)) }
    }
}

/// The process's default host's unified recent/unpacked part cache outside `madsim` (board 6026).
#[cfg(not(madsim))]
pub fn shared_parts() -> &'static Arc<PartMemory> {
    crate::Host::process().parts()
}

impl PartMemory {
    pub fn new(limit: u64) -> Arc<Self> {
        Arc::new(Self { kept: Mutex::new(Recent::new(limit)) })
    }

    /// Bytes currently held by cached parts.
    pub fn bytes(&self) -> u64 {
        self.kept.lock().unwrap().bytes
    }

    /// Peak bytes held by cached parts.
    pub fn peak(&self) -> u64 {
        self.kept.lock().unwrap().peak
    }

    /// Current byte limit of this cache.
    pub fn limit(&self) -> u64 {
        self.kept.lock().unwrap().limit
    }

    /// Updates the cache's byte limit to `limit`, evicting least-recently-used entries down to it,
    /// and returns bytes still held (board 12313).
    pub fn size_to(&self, limit: u64) -> u64 {
        let mut kept = self.kept.lock().unwrap();
        kept.limit = limit;
        kept.evict_to(limit);
        kept.bytes
    }

    /// Evicts least-recently-used parts until at most `target` bytes remain, returning bytes held.
    pub fn evict_to(&self, target: u64) -> u64 {
        let mut kept = self.kept.lock().unwrap();
        kept.evict_to(target);
        kept.bytes
    }

    /// When inserting a new `len`-byte unpacked part would exceed the cache's limit, reclaims the
    /// least-recently-used entry's `Box<[u8]>` if it is uniquely owned and has exact length `len`
    /// (board 12313). Reusing an already-faulted 1 MiB buffer avoids repeated `mmap`/`munmap` and
    /// anonymous page-fault churn when `M_MMAP_THRESHOLD == 1 MiB` without retaining idle memory.
    pub fn take_spare_buf(&self, len: usize) -> Option<Box<[u8]>> {
        let mut kept = self.kept.lock().unwrap();
        if kept.limit == 0 || kept.bytes.saturating_add(len as u64) <= kept.limit {
            return None;
        }
        let reusable = matches!(
            kept.parts.peek_oldest(),
            Some((_, Fetched::Unpacked(PartBytes { bytes: PartBacking::Boxed(arc), .. })))
                if arc.len() == len && Arc::strong_count(arc) == 1
        );
        if !reusable {
            return None;
        }
        let (id, dropped) = kept.parts.pop_oldest()?;
        kept.bytes -= Recent::entry_size(&id, &dropped);
        match dropped {
            Fetched::Unpacked(PartBytes { bytes: PartBacking::Boxed(arc), .. }) => {
                Arc::into_inner(arc)
            }
            _ => None,
        }
    }

    /// Looks up a local framed file's unpacked part.
    pub fn get_local(&self, file: u64, part: u64) -> Option<PartBytes> {
        let mut kept = self.kept.lock().unwrap();
        match kept.parts.get(&PartKey::Local(file, part)) {
            Some(Fetched::Unpacked(bytes)) => Some(bytes.clone()),
            _ => None,
        }
    }

    /// Inserts a local framed file's unpacked part.
    pub fn put_local(&self, file: u64, part: u64, bytes: PartBytes) {
        self.kept.lock().unwrap().put(PartKey::Local(file, part), Fetched::Unpacked(bytes));
    }

    /// Drops all unpacked parts belonging to local framed file opening `file`.
    pub fn forget_local(&self, file: u64) {
        let mut kept = self.kept.lock().unwrap();
        let dropped =
            kept.parts.remove_range(PartKey::Local(file, 0)..=PartKey::Local(file, u64::MAX));
        for part in dropped {
            kept.bytes -= part.size();
        }
    }

    fn get_bucket(&self, key: &str, part: u64) -> Option<Fetched> {
        self.kept.lock().unwrap().parts.get(&PartKey::Bucket(Arc::from(key), part)).cloned()
    }

    fn get_bucket_stored(&self, key: &str, part: u64) -> Option<Arc<[u8]>> {
        let mut kept = self.kept.lock().unwrap();
        match kept.parts.get(&PartKey::Bucket(Arc::from(key), part)) {
            Some(Fetched::Stored { bytes }) => Some(bytes.clone()),
            _ => None,
        }
    }

    fn forget_gen(&self) -> u64 {
        self.kept.lock().unwrap().forget_gen
    }

    fn put_bucket(&self, key: &str, part: u64, fetched: Fetched) {
        self.kept.lock().unwrap().put(PartKey::Bucket(Arc::from(key), part), fetched);
    }

    /// Inserts `fetched` only if no `forget_bucket` has run since `expected_gen` was captured
    /// (atomic under `self.kept` mutex, closing the shred/revocation re-insertion race).
    fn put_bucket_at_gen(&self, key: &str, part: u64, fetched: Fetched, expected_gen: u64) {
        let mut kept = self.kept.lock().unwrap();
        if kept.forget_gen != expected_gen {
            return;
        }
        kept.put(PartKey::Bucket(Arc::from(key), part), fetched);
    }

    /// Stores `fetched` if no unpacked plaintext entry already occupies `(key, part)`.
    fn put_bucket_if_not_unpacked(&self, key: &str, part: u64, fetched: Fetched) {
        let mut kept = self.kept.lock().unwrap();
        let id = PartKey::Bucket(Arc::from(key), part);
        if matches!(kept.parts.peek(&id), Some(Fetched::Unpacked(_))) {
            return;
        }
        kept.put(id, fetched);
    }

    fn forget_bucket(&self, mut matches: impl FnMut(&str) -> bool) -> usize {
        let mut kept = self.kept.lock().unwrap();
        kept.forget_gen += 1;
        let doomed: Vec<PartKey> = kept
            .parts
            .keys()
            .filter(|k| matches!(k, PartKey::Bucket(key, _) if matches(key)))
            .cloned()
            .collect();
        let count = doomed.len();
        for id in doomed {
            if let Some(dropped) = kept.parts.remove(&id) {
                kept.bytes -= Recent::entry_size(&id, &dropped);
            }
        }
        count
    }
}

/// The parts fetched or unpacked last, least recently used first, and the bytes they take.
struct Recent {
    parts: LruMap<PartKey, Fetched>,
    bytes: u64,
    peak: u64,
    limit: u64,
    forget_gen: u64,
}

impl Recent {
    fn new(limit: u64) -> Self {
        Self { parts: LruMap::new(), bytes: 0, peak: 0, limit, forget_gen: 0 }
    }

    fn entry_size(id: &PartKey, fetched: &Fetched) -> u64 {
        match fetched {
            Fetched::Stored { bytes } => bytes.len() as u64 + id.stored_overhead(),
            Fetched::Unpacked(plain) => plain.held(),
        }
    }

    fn put(&mut self, id: PartKey, fetched: Fetched) {
        self.bytes += Self::entry_size(&id, &fetched);
        if let Some(replaced) = self.parts.insert(id.clone(), fetched) {
            self.bytes -= Self::entry_size(&id, &replaced);
        }
        self.evict_to(self.limit);
        self.peak = self.peak.max(self.bytes);
    }

    fn evict_to(&mut self, target: u64) {
        while self.bytes > target
            && let Some((id, dropped)) = self.parts.pop_oldest()
        {
            self.bytes -= Self::entry_size(&id, &dropped);
        }
    }
}

/// What a read of a range of an object's data found in it.
#[derive(Debug, PartialEq, Eq)]
enum Found {
    Bytes(Vec<u8>),
    Gone,
}

/// The GETs of parts that reads wait for, the reads of a host's fetchers, each until its last
/// reader leaves it.
#[derive(Default)]
struct Parts {
    flights: HashMap<Flown, Waited>,
}

/// What a part's GET fetches, by which reads find it in flight: its object's key, the part's
/// number, whether it asks the part's owner first (TASK-406.2), and whether it requires stored
/// bytes (peer serving after disk-cache eviction coalesces its own single bucket GET rather than
/// returning an unpacked-only memory entry).
type Flown = (String, u64, bool, bool);

impl std::fmt::Debug for Budgets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::One(budget) => f.debug_tuple("One").field(&budget.limits()).finish(),
            Self::PerBucket(per) => f.debug_tuple("PerBucket").field(&per.0).finish(),
        }
    }
}

struct Waited {
    /// The budget it enters if it must GET from the bucket, its current priority, and its ticket
    /// once entered (deferred until after any peer ask so peer RPCs never hold GET budget slots,
    /// TASK-542 / board 2480).
    budget: Arc<Budget>,
    priority: Priority,
    ticket: Option<u64>,
    readers: usize,
    flight: Flight<FetchedPart>,
}

/// Reads extents from a bucket by parts, under a budget.
pub struct Fetcher<B: Bucket> {
    shared: Arc<Fetching<B>>,
    /// Where reads of each object stopped, and how far ahead of them parts are read.
    streams: Arc<Mutex<HashMap<String, Stream>>>,
    /// Runs read-ahead, which no read waits for.
    spawn: Option<Spawn>,
}

/// Runs a future to its end, no one waiting for it.
pub type Spawn = Arc<dyn Fn(BoxFuture<'static, ()>) + Send + Sync>;

impl<B: Bucket> Clone for Fetcher<B> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
            streams: self.streams.clone(),
            spawn: self.spawn.clone(),
        }
    }
}

/// Reads of an object, as read-ahead follows them.
struct Stream {
    /// The part after the last one read.
    next: u64,
    /// How many parts are read ahead of the next.
    window: u64,
    /// The part after the last one read ahead.
    ahead: u64,
}

/// The most parts read ahead of a read.
const AHEAD: u64 = 16;

/// How many objects' reads read-ahead follows.
const STREAMS: usize = 1024;

/// What a part's GET needs.
struct Fetching<B: Bucket> {
    bucket: Arc<B>,
    budgets: Budgets,
    /// Objects' tails, or the reads of them, and, if it keeps the parts it fetches, those it
    /// fetched last, as fetched.
    memory: Arc<Memory>,
    /// Where it keeps the parts it fetches, as stored, and their objects' tails, if it does.
    cache: Option<Arc<Cache>>,
    /// cladefs: the hosts sharing its bucket, which it asks for the parts they own before the
    /// bucket, if it does (TASK-406.2).
    peers: Option<Arc<dyn Peers>>,
}

impl<B: Bucket> Fetcher<B> {
    /// Reads `bucket` under `budgets` (a budget for all, or one for each bucket: [`Budgets`]),
    /// keeping objects' tails in `memory`; and, with a `cache`, the parts it fetches there, those
    /// it fetched last in `memory` too.
    pub fn new(
        bucket: Arc<B>,
        budgets: impl Into<Budgets>,
        memory: Arc<Memory>,
        cache: Option<Arc<Cache>>,
    ) -> Self {
        let budgets = budgets.into();
        let shared = Arc::new(Fetching { bucket, budgets, memory, cache, peers: None });
        Self { shared, streams: Arc::default(), spawn: None }
    }

    /// cladefs: asks `peers`, the hosts sharing its bucket, for the parts another of them owns
    /// before the bucket (TASK-406.2, [`crate::peer`]).
    pub fn asking(mut self, peers: Arc<dyn Peers>) -> Self {
        Arc::get_mut(&mut self.shared).expect("a fetcher not cloned yet").peers = Some(peers);
        self
    }

    /// Reads ahead of reads that wait for their data, by `spawn`, into the cache: with none, it
    /// reads nothing ahead.
    pub fn reading_ahead(self, spawn: Spawn) -> Self {
        Self { spawn: Some(spawn), ..self }
    }

    /// `len` bytes at `offset` in `extent`, at `location`: as many as it holds. The object at a key
    /// holds the extent where its footer says. One there that names it not, or whose parts fail
    /// the CRCs its tail gives, is corrupt: a key names one object for good (TASK-407). One gone
    /// is [`Error::Lost`]: cladefs: its cell's writer may have relocated the extent since, which
    /// its reader looks up again (doc-27).
    pub async fn read(
        &self,
        extent: ExtentId,
        location: &Location,
        offset: u64,
        len: u64,
        priority: Priority,
    ) -> Result<Vec<u8>, Error> {
        let range = offset..offset.saturating_add(len);
        self.located(extent, location, range, priority, |key, tail, range| async move {
            if priority == Priority::Demand && !range.is_empty() {
                self.read_ahead(&key, range.start / PART, (range.end - 1) / PART);
            }
            Ok(match self.data(&key, &tail, range, priority).await? {
                Found::Bytes(bytes) => Some(bytes),
                Found::Gone => None,
            })
        })
        .await
    }

    /// cladefs: whether it keeps the parts it fetches in a cache, which [`Fetcher::fetch_ahead`]
    /// fetches into: what a read of them after it then reads, with no request (board 6225).
    pub fn keeps_parts(&self) -> bool {
        self.shared.cache.is_some()
    }

    /// cladefs: fetches the parts that hold the `len` bytes at `offset` in `extent`, at `location`,
    /// into the cache, as read-ahead: behind reads that
    /// wait for their data, its object's tail too, from the host that owns each first, if another
    /// does, and not unpacked (TASK-702.3). How many it fetched: by flights of its own, not those
    /// the cache or memory held, or reads in flight were fetching; none past the extent's end, or
    /// if the fetcher keeps nothing, having no cache.
    pub async fn fetch_ahead(
        &self,
        extent: ExtentId,
        location: &Location,
        offset: u64,
        len: u64,
    ) -> Result<u64, Error> {
        let Some(cache) = &self.shared.cache else { return Ok(0) };
        let range = offset..offset.saturating_add(len);
        let ahead = Priority::Ahead;
        self.located(extent, location, range, ahead, |key, tail, range| async move {
            let mut fetched = 0;
            for part in tail.parts().frame.parts_of(range) {
                if cache.holds(&key, part) || self.shared.recent_part(&key, part).is_some() {
                    continue;
                }
                let owner = self.shared.peers.as_ref().and_then(|peers| peers.owner(&key, part));
                // A flight of its own, which the cache and memory, just looked at, held nothing
                // for.
                let (flight, new, _reading) = self.join(&key, part, &tail, ahead, owner)?;
                match land(flight).await? {
                    FetchedPart::Data(_) => fetched += u64::from(new),
                    FetchedPart::Gone => return Ok(None),
                }
            }
            Ok(Some(fetched))
        })
        .await
    }

    /// What `found` makes of the bytes `range` of `extent` (as many as it holds), at `location`:
    /// given the key of the object holding it, the object's tail, read at `priority`, and the
    /// bytes' range in its data; none if the object is gone, [`Error::Lost`] then. The object at a
    /// key holds the extent where its footer says; one that names it not is corrupt.
    async fn located<T, F: Future<Output = Result<Option<T>, Error>>>(
        &self,
        extent: ExtentId,
        location: &Location,
        range: Range<u64>,
        priority: Priority,
        found: impl FnOnce(String, Arc<Tail>, Range<u64>) -> F,
    ) -> Result<T, Error> {
        let key = &location.key;
        let lost = || Error::Lost(format!("{extent:?} in {key}"));
        let tail = land(self.shared.tail_at(key, priority)).await?.ok_or_else(lost)?;
        let Some(&(at, held)) = tail.extents.get(&extent) else {
            return Err(Error::Corrupt(format!("{key} holds no {extent:?}, named there")));
        };
        let held = at + range.start.min(held)..at + range.end.min(held);
        found(key.clone(), tail, held).await?.ok_or_else(lost)
    }

    /// Where `extent` is as of the checkpoint at `key`, if it holds it, looked up a block at a time
    /// (doc-7, Checkpoints), rather than read whole: the checkpoint's tail, kept as other objects'
    /// are, holds its table, which says where the block that would hold the extent's entry is; the
    /// block is read by the parts it is in, as extents' data is, within the budget, and kept in the
    /// cache. One whose parts fail the CRCs its tail gives is corrupt. A checkpoint gone is lost.
    pub async fn lookup(&self, key: &str, extent: ExtentId) -> Result<Option<Location>, Error> {
        let tail = land(self.shared.tail(key)).await?.ok_or_else(|| Error::Lost(key.to_owned()))?;
        let Some(table) = &tail.table else {
            return Err(Error::Corrupt(format!("{key} holds no checkpoint")));
        };
        let Some(block) = table.block(extent) else { return Ok(None) };
        let range = block.offset..block.offset + u64::from(block.len);
        match self.data(key, &tail, range, Priority::Demand).await? {
            Found::Bytes(bytes) => table.find(&bytes, block, extent),
            Found::Gone => Err(Error::Lost(key.to_owned())),
        }
    }

    /// The bytes `range` of the data of the object at `key`, whose tail is `tail`: read by the
    /// parts that hold them.
    async fn data(
        &self,
        key: &str,
        tail: &Arc<Tail>,
        range: Range<u64>,
        priority: Priority,
    ) -> Result<Found, Error> {
        if range.start > range.end || range.end > tail.parts().frame.data_len {
            return Err(Error::Corrupt(format!("{range:?} past the end of {key}")));
        }
        // cladefs: opened under the host's keys (TASK-411.3).
        let forget_gen = self.shared.memory.recent.forget_gen();
        let opened = tail.opened(&self.shared.memory.keys, key)?;
        let numbers = tail.parts().frame.parts_of(range.clone());
        let parts = numbers.clone().map(|part| self.part(key, part, tail, priority));
        let parts = futures::future::try_join_all(parts).await?;
        let Range { start, end } = range;
        let mut bytes = Vec::with_capacity((end - start) as usize);
        for (number, part) in numbers.zip(parts) {
            let opened_part = part_of(&opened, key, number)?;
            let part = match part {
                FetchedPart::Data(Fetched::Unpacked(plain)) => {
                    opened_part.check_open()?;
                    plain
                }
                FetchedPart::Data(Fetched::Stored { bytes }) => {
                    let plain = if opened_part.as_is() {
                        PartBytes::from_arc(bytes)
                    } else {
                        let len = opened_part.len() as usize;
                        let mut out = self
                            .shared
                            .memory
                            .recent
                            .take_spare_buf(len)
                            .unwrap_or_else(|| vec![0; len].into_boxed_slice());
                        match opened_part.unpack_copy_into(&bytes, &mut out) {
                            Ok(()) => {
                                if opened_part.stored.encoding == extent_frame::Encoding::Zstd {
                                    UNPACKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                }
                                PartBytes::from_box(out)
                            }
                            Err(extent_frame::Error::Corrupt(err)) => {
                                return Err(Error::Corrupt(format!(
                                    "part {number} of {key}: {err}"
                                )));
                            }
                            Err(err) => return Err(Error::from(err)),
                        }
                    };
                    if self.shared.cache.is_some() {
                        self.shared.memory.recent.put_bucket_at_gen(
                            key,
                            number,
                            Fetched::Unpacked(plain.clone()),
                            forget_gen,
                        );
                    }
                    plain
                }
                FetchedPart::Gone => return Ok(Found::Gone),
            };
            let at = start + bytes.len() as u64;
            let from = ((at % PART) as usize).min(part.len());
            let to = (from + (end - at) as usize).min(part.len());
            bytes.extend_from_slice(&part[from..to]);
        }
        Ok(Found::Bytes(bytes))
    }

    /// Reads ahead of a read of parts `first..=last` of the object at `key`: the part after them,
    /// and twice as many as the time before, up to [`AHEAD`], while reads go on where the last
    /// stopped, so that a sequential reader finds its parts kept, and a random one their
    /// neighbours. Parts the cache holds are not read again, from the bucket or the cache
    /// (TASK-225).
    fn read_ahead(&self, key: &str, first: u64, last: u64) {
        let Some(spawn) = self.spawn.as_ref().filter(|_| self.shared.cache.is_some()) else {
            return;
        };
        let parts = {
            let mut streams = self.streams.lock().unwrap();
            if streams.len() >= STREAMS && !streams.contains_key(key) {
                streams.clear();
            }
            let stream = (streams.entry(key.to_owned())).or_insert(Stream {
                next: u64::MAX,
                window: 0,
                ahead: 0,
            });
            let sequential = first <= stream.next && stream.next <= last + 1;
            if sequential {
                stream.window = (stream.window * 2).clamp(1, AHEAD);
            } else {
                stream.window = 1;
                stream.ahead = 0;
            }
            let parts = stream.ahead.max(last + 1)..last + 1 + stream.window;
            stream.next = last + 1;
            stream.ahead = stream.ahead.max(parts.end);
            parts
        };
        if parts.is_empty() {
            return;
        }
        let (this, key) = (self.clone(), key.to_owned());
        spawn(Box::pin(async move {
            // None past the object's end.
            let Ok(Some(tail)) = land(this.shared.tail(&key)).await else { return };
            let parts = parts.start..parts.end.min(tail.parts().frame.parts());
            let held = |part: &u64| match (&this.shared.cache, tail.part(&key, *part)) {
                (Some(cache), Ok(_)) => cache.holds(&key, *part),
                _ => false,
            };
            let parts = parts.filter(|part| !held(part));
            let reads = parts.map(|part| this.part(&key, part, &tail, Priority::Ahead));
            for read in futures::future::join_all(reads).await {
                if let Err(err) = read {
                    log::debug!("cladefs: reading {key} ahead: {err}");
                }
            }
        }));
    }

    /// cladefs: the stored bytes of part `part` of the object at `key`, which a peer asks for
    /// (TASK-406.2): kept in this host's memory or its cache, or fetched from the bucket by one
    /// GET, however many ask at once, and kept then, never asking another peer. None if the object
    /// is gone, or holds no such part. The asker checks them against the CRC its own tail gives.
    pub async fn stored_part(&self, key: &str, part: u64) -> Result<Option<Arc<[u8]>>, Error> {
        let Some(tail) = land(self.shared.tail(key)).await? else { return Ok(None) };
        if tail.part(key, part).is_err() {
            return Ok(None);
        }
        if let Some(bytes) = self.shared.recent_stored(key, part) {
            self.shared.memory.found_part(Source::Memory, bytes.len() as u64);
            return Ok(Some(bytes));
        }
        match self.flight_kind(key, part, &tail, Priority::Demand, None, true).await? {
            FetchedPart::Data(Fetched::Stored { bytes }) => Ok(Some(bytes)),
            FetchedPart::Data(Fetched::Unpacked(_)) => unreachable!("stored flight yields stored"),
            FetchedPart::Gone => Ok(None),
        }
    }

    /// Part `part` of the object at `key`, whose tail is `tail`: kept in the host's memory, which
    /// needs no GET, nor the lock of those in flight, which the host's fetchers share; or the GET
    /// for it in flight ([`Fetcher::flight`]), which asks the host that owns it first, if another
    /// does (TASK-406.2).
    async fn part(
        &self,
        key: &str,
        part: u64,
        tail: &Arc<Tail>,
        priority: Priority,
    ) -> Result<FetchedPart, Error> {
        if let Some(fetched) = self.shared.recent_part(key, part) {
            self.shared.memory.found_part(Source::Memory, tail.part(key, part)?.stored.len);
            return Ok(FetchedPart::Data(fetched));
        }
        let owner = self.shared.peers.as_ref().and_then(|peers| peers.owner(key, part));
        self.flight(key, part, tail, priority, owner).await
    }

    /// Part `part` of the object at `key`, whose tail is `tail`, by the GET for it in flight, this
    /// fetcher's or another of the host's, asking `owner` first or not as it does ([`Flown`]), or
    /// a new one. The GET goes, and gives up its
    /// place in the budget, once no read waits for it.
    async fn flight(
        &self,
        key: &str,
        part: u64,
        tail: &Arc<Tail>,
        priority: Priority,
        owner: Option<Arc<dyn Peer>>,
    ) -> Result<FetchedPart, Error> {
        self.flight_kind(key, part, tail, priority, owner, false).await
    }

    async fn flight_kind(
        &self,
        key: &str,
        part: u64,
        tail: &Arc<Tail>,
        priority: Priority,
        owner: Option<Arc<dyn Peer>>,
        need_stored: bool,
    ) -> Result<FetchedPart, Error> {
        let (flight, _, _reading) =
            self.join_kind(key, part, tail, priority, owner, need_stored)?;
        land(flight).await
    }

    /// The flight of part `part` of the object at `key`, as [`Fetcher::flight`] has it: whether
    /// it is new, this read's; and this read's place among its readers, which it holds while it
    /// waits.
    fn join(
        &self,
        key: &str,
        part: u64,
        tail: &Arc<Tail>,
        priority: Priority,
        owner: Option<Arc<dyn Peer>>,
    ) -> Result<(Flight<FetchedPart>, bool, Reading<'_>), Error> {
        self.join_kind(key, part, tail, priority, owner, false)
    }

    fn join_kind(
        &self,
        key: &str,
        part: u64,
        tail: &Arc<Tail>,
        priority: Priority,
        owner: Option<Arc<dyn Peer>>,
        need_stored: bool,
    ) -> Result<(Flight<FetchedPart>, bool, Reading<'_>), Error> {
        tail.part(key, part)?;
        let id: Flown = (key.to_owned(), part, owner.is_some(), need_stored);
        let parts = &self.shared.memory.parts;
        let mut new = false;
        let flight = {
            let mut parts = parts.lock().unwrap();
            let waited = parts.flights.entry(id.clone()).or_insert_with(|| {
                new = true;
                let budget = self.shared.budgets.of(key);
                let (shared, tail, key, id) =
                    (self.shared.clone(), tail.clone(), key.to_owned(), id.clone());
                let flight =
                    async move { shared.part(&key, part, &tail, id, owner, need_stored).await };
                Waited { budget, priority, ticket: None, readers: 0, flight: fly(flight) }
            });
            if priority == Priority::Demand {
                waited.priority = Priority::Demand;
                if let Some(ticket) = waited.ticket {
                    waited.budget.promote(ticket);
                }
            }
            waited.readers += 1;
            waited.flight.clone()
        };
        Ok((flight, new, Reading { parts, id }))
    }
}

/// Runs `io`, which blocks on files, as a task of a multi-threaded runtime may: in
/// `block_in_place`, which hands the runtime's other tasks to another of its threads meanwhile, as
/// those of the runtime a host's cells share need, which their fetches and uploads run on
/// (TASK-92.21). On an executor of another kind, or none, it runs as it is.
#[allow(clippy::disallowed_methods, reason = "the work waits on files alone")]
fn blocking<T>(io: impl FnOnce() -> T) -> T {
    // TASK-685.1: madsim's one thread runs it as it is.
    #[cfg(madsim)]
    return host_fs::may_block(io);
    #[cfg(not(madsim))]
    match tokio::runtime::Handle::try_current().map(|runtime| runtime.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => {
            tokio::task::block_in_place(|| host_fs::may_block(io))
        }
        _ => host_fs::may_block(io),
    }
}

/// A read waiting for a part's flight, or done with it: one of the readers its flight counts, which
/// goes when they all have.
struct Reading<'a> {
    parts: &'a Mutex<Parts>,
    id: Flown,
}

impl Drop for Reading<'_> {
    fn drop(&mut self) {
        let mut parts = self.parts.lock().unwrap();
        let waited = parts.flights.get_mut(&self.id).expect("a flight its readers keep");
        waited.readers -= 1;
        if waited.readers == 0 {
            let removed = parts.flights.remove(&self.id).expect("a flight its readers keep");
            if let Some(ticket) = removed.ticket {
                removed.budget.cancel(ticket);
            }
        }
    }
}

/// Unpacks an owned stored part buffer in place (avoiding copying the stored ciphertext and
/// avoiding copying the unpacked 1 MiB plaintext into `Arc<[u8]>`, board 12313).
fn unpack_owned(
    opened_part: &extent_frame::Part,
    key: &str,
    number: u64,
    mut stored: Vec<u8>,
    recent: &PartMemory,
) -> Result<PartBytes, Error> {
    let len = opened_part.len() as usize;
    let map_err = |err| match err {
        extent_frame::Error::Corrupt(err) => {
            Error::Corrupt(format!("part {number} of {key}: {err}"))
        }
        err => Error::from(err),
    };
    match opened_part.stored.encoding {
        extent_frame::Encoding::Raw => {
            let opened_len = opened_part.open(&mut stored).map_err(map_err)?.len();
            if opened_len != len {
                return Err(Error::Corrupt(format!(
                    "part {number} of {key}: an object's raw part, of the wrong length"
                )));
            }
            stored.truncate(len);
            Ok(PartBytes::from_box(stored.into_boxed_slice()))
        }
        extent_frame::Encoding::Zstd => {
            let mut out =
                recent.take_spare_buf(len).unwrap_or_else(|| vec![0; len].into_boxed_slice());
            opened_part.unpack_into(&mut stored, &mut out).map_err(map_err)?;
            UNPACKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(PartBytes::from_box(out))
        }
    }
}

impl<B: Bucket> Fetching<B> {
    /// Part `part` of the object at `key`, whose tail is `tail`, checked against the CRC the tail
    /// gives: kept, in memory, as one fetched since its first reader looked there is, or in the
    /// cache; or fetched from `owner`, the host that owns it, if another does (without holding a
    /// local GET budget slot so two hosts asking each other cannot deadlock, TASK-542 / board
    /// 2480), or from the bucket once the budget lets its GET go, and kept then; unpacked only
    /// once a read wants it ([`Fetched`]).
    async fn part(
        self: &Arc<Self>,
        key: &str,
        part: u64,
        tail: &Tail,
        id: Flown,
        owner: Option<Arc<dyn Peer>>,
        need_stored: bool,
    ) -> Result<FetchedPart, Error> {
        let opened = tail.part(key, part)?;
        let stored = opened.stored;
        let cached = if need_stored {
            self.recent_stored(key, part).map(|bytes| Fetched::Stored { bytes })
        } else {
            self.recent_part(key, part)
        };
        if let Some(fetched) = cached {
            self.memory.found_part(Source::Memory, stored.len);
            return Ok(FetchedPart::Data(fetched));
        }
        let bytes = match self.kept(key, part)? {
            Some(bytes) => {
                self.memory.found_part(Source::PartCache, stored.len);
                bytes
            }
            None => {
                let from_peer = match owner {
                    Some(owner) => match owner.part(key, part).await {
                        Some(bytes)
                            if bytes.len() as u64 == stored.len && stored.check(&bytes).is_ok() =>
                        {
                            owner.gave(stored.len);
                            self.memory.found_part(Source::Peer, stored.len);
                            Some(bytes)
                        }
                        Some(_) => {
                            log::warn!(
                                "cladefs: part {part} of {key}, from its owner, fails its CRC"
                            );
                            owner.failed();
                            None
                        }
                        None => None,
                    },
                    None => None,
                };
                let fetched = match from_peer {
                    Some(bytes) => Some(bytes),
                    None => {
                        let budget = self.budgets.of(key);
                        let given = {
                            let mut parts = self.memory.parts.lock().unwrap();
                            let priority =
                                parts.flights.get(&id).map_or(Priority::Demand, |w| w.priority);
                            let (ticket, given) = budget.enter(priority, PART);
                            if let Some(waited) = parts.flights.get_mut(&id) {
                                waited.ticket = Some(ticket);
                            }
                            given
                        };
                        let slot = given.await;
                        let fetched = self.fetch_bucket(key, part, &stored).await;
                        drop(slot);
                        let fetched = fetched?;
                        if fetched.is_some() {
                            self.memory.found_part(Source::Bucket, stored.len);
                        }
                        fetched
                    }
                };
                let Some(fetched) = fetched else { return Ok(FetchedPart::Gone) };
                self.keep(key, part, &fetched);
                fetched
            }
        };
        // For demand reads (`!need_stored`), unpack once inside the shared flight in place so
        // concurrent readers share one decrypt+decompress, avoid copying the stored/unpacked
        // buffers, and never insert `Stored` into `PartMemory` only to replace it immediately
        // (board 12313). Read-ahead (`Priority::Ahead`) and peer serving (`need_stored`) still
        // retain `Stored` without decompressing.
        let demand = !need_stored
            && self
                .memory
                .parts
                .lock()
                .unwrap()
                .flights
                .get(&id)
                .is_some_and(|w| w.priority == Priority::Demand);
        if demand {
            let forget_gen = self.memory.recent.forget_gen();
            match tail.opened(&self.memory.keys, key) {
                Ok(opened_parts) => {
                    let opened_part = part_of(&opened_parts, key, part)?;
                    if !opened_part.as_is() && opened_part.check_open().is_ok() {
                        let plain =
                            unpack_owned(&opened_part, key, part, bytes, &self.memory.recent)?;
                        let fetched = Fetched::Unpacked(plain);
                        if self.cache.is_some() {
                            self.memory.recent.put_bucket_at_gen(
                                key,
                                part,
                                fetched.clone(),
                                forget_gen,
                            );
                        }
                        return Ok(FetchedPart::Data(fetched));
                    }
                }
                Err(Error::NoKey(_)) => {}
                Err(err) => return Err(err),
            }
        }
        let fetched = Fetched::Stored { bytes: bytes.into() };
        if self.cache.is_some() {
            if need_stored {
                self.memory.recent.put_bucket_if_not_unpacked(key, part, fetched.clone());
            } else {
                self.memory.recent.put_bucket(key, part, fetched.clone());
            }
        }
        Ok(FetchedPart::Data(fetched))
    }

    /// The stored bytes of part `part` of the object at `key`, stored as `stored` says, fetched by
    /// a GET of the bucket's and checked against its CRC. None if the object is gone. The bucket's
    /// failing the CRC, or past the object's end, are corrupt: the object at a key is the one its
    /// tail is of, for good (TASK-407).
    async fn fetch_bucket(
        &self,
        key: &str,
        part: u64,
        stored: &extent_frame::Stored,
    ) -> Result<Option<Vec<u8>>, Error> {
        let range = ByteRange::Bounded(stored.range());
        trace::note(Some(Source::Bucket));
        match bucket::get_range(&*self.bucket, key, range).await {
            Ok(Some(fetched)) if stored.check(&fetched).is_ok() => Ok(Some(fetched)),
            Ok(None) => Ok(None),
            Ok(Some(_)) => Err(Error::Corrupt(format!("part {part} of {key}, by its CRC"))),
            Err(bucket::Unread::Past(_)) => {
                Err(Error::Corrupt(format!("part {part} of {key}, past its end")))
            }
            Err(bucket::Unread::Refused(refused)) => Err(refused.into()),
        }
    }

    /// Part `part` of the object at `key`, if fetched last and kept in the host's memory.
    fn recent_part(&self, key: &str, part: u64) -> Option<Fetched> {
        self.cache.as_ref()?;
        self.memory.recent.get_bucket(key, part)
    }

    /// Stored bytes of part `part` of the object at `key`, if kept unpacked-not-yet in memory.
    fn recent_stored(&self, key: &str, part: u64) -> Option<Arc<[u8]>> {
        self.cache.as_ref()?;
        self.memory.recent.get_bucket_stored(key, part)
    }

    /// What `io` does with the files of its cache, if it keeps one: as a task that blocks may,
    /// holding up none of the runtime's others ([`blocking`]).
    fn cached<T>(&self, io: impl FnOnce(&Cache) -> T) -> Option<T> {
        let cache = self.cache.as_deref()?;
        Some(blocking(|| io(cache)))
    }

    /// The stored bytes of part `part` of the object at `key`, if the cache has them: checked,
    /// those that fail dropped from the cache ([`Cache::part`]).
    fn kept(&self, key: &str, part: u64) -> Result<Option<Vec<u8>>, Error> {
        self.cached(|cache| cache.part(key, part))
            .unwrap_or(Ok(None))
            .map_err(|err| Error::Read(format!("part {part} of {key} from the part cache: {err}")))
    }

    /// Keeps `bytes`, part `part`'s stored bytes, checked, in the cache.
    fn keep(&self, key: &str, part: u64, bytes: &[u8]) {
        if let Some(Err(err)) = self.cached(|cache| cache.keep_part(key, part, bytes)) {
            log::warn!("cladefs: keeping part {part} of {key} in the cache: {err}");
        }
    }

    /// The tail of the object at `key`: kept, or in flight, or fetched now, under a place in the
    /// budget.
    fn tail(self: &Arc<Self>, key: &str) -> Flight<Option<Arc<Tail>>> {
        self.tail_at(key, Priority::Demand)
    }

    /// The tail of the object at `key`, as [`Fetching::tail`] has it, read at `priority` if it is
    /// read now: read-ahead's goes behind reads that wait for theirs, until one waits for it.
    fn tail_at(self: &Arc<Self>, key: &str, priority: Priority) -> Flight<Option<Arc<Tail>>> {
        let mut tails = self.memory.tails.lock().unwrap();
        if let Some(tail) = tails.get(key, priority) {
            return tail;
        }
        let (shared, key_owned, read) = (self.clone(), key.to_owned(), tails.next_read());
        let budget = self.budgets.of(key);
        let (ticket, given) = budget.enter(priority, TAIL);
        let ahead = (priority == Priority::Ahead).then_some((budget, ticket));
        let tail = async move {
            let fetched = shared.read_tail(&key_owned, given).await;
            let mut tails = shared.memory.tails.lock().unwrap();
            match &fetched {
                Ok(Some((tail, size))) => {
                    if let Some(entry) =
                        tails.kept.peek_mut(&key_owned).filter(|entry| entry.read == read)
                    {
                        entry.flight = TailState::Ready(tail.clone());
                        tails.settle(&key_owned, read, *size);
                    }
                }
                // What failed, or found the object gone, is not kept.
                _ => tails.remove(&key_owned, Some(read)),
            }
            fetched.map(|fetched| fetched.map(|(tail, _)| tail))
        };
        let tail = fly(tail);
        tails.insert(key, read, tail.clone(), ahead);
        tail
    }

    /// The tail of the object at `key`, if there is one, and the bytes it takes in memory: the
    /// cache's copy of it, or one read from the bucket, once `given` gives it its place in the
    /// budget, and kept in the cache then.
    async fn read_tail(
        &self,
        key: &str,
        given: impl Future<Output = Slot>,
    ) -> Result<Option<(Arc<Tail>, u64)>, Error> {
        let tail = match self.kept_tail(key)? {
            Some(tail) => {
                trace::note(Some(Source::PartCache));
                self.memory.found_tail(Source::PartCache);
                tail
            }
            None => {
                let slot = given.await;
                trace::note(Some(Source::Bucket));
                let read = object::tail(&*self.bucket, key).await;
                drop(slot);
                let Some(tail) = read? else { return Ok(None) };
                self.memory.found_tail(Source::Bucket);
                self.keep_tail(key, &tail);
                tail
            }
        };
        let tail = Tail::of(key, tail)?;
        let size = tail.size();
        Ok(Some((Arc::new(tail), size)))
    }

    /// The tail of the object at `key`, if the cache has it: checked, one that fails dropped from
    /// the cache ([`Cache::tail`]).
    fn kept_tail(&self, key: &str) -> Result<Option<object::Tail>, Error> {
        self.cached(|cache| cache.tail(key))
            .unwrap_or(Ok(None))
            .map_err(|err| Error::Read(format!("the tail of {key} from the part cache: {err}")))
    }

    /// Keeps `tail`, the tail of the object at `key`, as read, in the cache.
    fn keep_tail(&self, key: &str, tail: &object::Tail) {
        if let Some(Err(err)) = self.cached(|cache| cache.keep_tail(key, tail)) {
            log::warn!("cladefs: keeping the tail of {key} in the cache: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    mod audit_tests;

    use super::*;
    use crate::bucket::PutMode;
    use crate::layout::test_cell;
    use crate::layout::{self, Frame};
    use crate::object::TAIL;
    use crate::testing::{Direct, Fault, SimBucket};
    use futures::executor::{LocalPool, block_on};
    use futures::task::LocalSpawnExt as _;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// The keys of a host serving the tests' cell: the tests' DEK (`testing::encrypting`), which
    /// their objects are encrypted under (TASK-411.28).
    fn ring() -> Arc<keys::Ring> {
        crate::testing::ring([test_cell(1)])
    }

    /// A host's memory, holding [`ring`] for as long as it lives.
    fn memory() -> Arc<Memory> {
        let memory = Memory::new();
        std::mem::forget(memory.keys().hold(ring()));
        memory
    }

    const KEY: &str = "tenants/0000000000001/cells/0000000000001/\
                       segments/0000000000001.0000000000000";

    /// Bytes that compress.
    fn pattern(len: u64) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8 ^ (i / PART) as u8).collect()
    }

    /// Bytes that don't, from a xorshift generator: their objects' parts are stored raw, each at
    /// its offset in the data.
    fn noise(len: u64) -> Vec<u8> {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    /// A segment's object of `data`, whose footer names `extents` in it, each at an offset, of a
    /// length.
    fn segment(data: &[u8], extents: Vec<(ExtentId, u64, u64)>) -> Vec<u8> {
        let contents = layout::Contents { extents, ..layout::Contents::default() };
        let footer = layout::Chain::new(test_cell(1), 1, 0, 0).close(contents);
        layout::Segment { data: data.to_vec(), footer }
            .encode(crate::testing::encrypting(test_cell(1)))
    }

    /// A segment's object of `data`, all of which is [`EXTENT`].
    fn whole(data: &[u8]) -> Vec<u8> {
        segment(data, vec![(EXTENT, 0, data.len() as u64)])
    }

    /// A bucket holding `bytes` at [`KEY`], its requests completed by the test.
    fn holding(bytes: Vec<u8>) -> Arc<SimBucket> {
        let bucket = SimBucket::new();
        block_on(Direct(bucket.clone()).put(KEY, bytes.into(), PutMode::Create)).unwrap();
        bucket
    }

    /// A bucket holding a segment of `data` at [`KEY`], all of which is [`EXTENT`].
    fn bucket(data: &[u8]) -> Arc<SimBucket> {
        holding(whole(data))
    }

    /// The bytes `bucket` holds at [`KEY`]: each segment framed encrypted under a salt of its own
    /// (TASK-411.28), the object a test made, as it is.
    fn stored_at(bucket: &Arc<SimBucket>) -> Vec<u8> {
        block_on(Direct(bucket.clone()).get(KEY)).unwrap().bytes.to_vec()
    }

    /// How long the tail of the segment of `data` at [`KEY`] is: what a cache keeps of it but its
    /// parts.
    fn tail_len(data: &[u8]) -> u64 {
        let tail = Frame::of(&whole(data)).unwrap().tail();
        tail.end - tail.start
    }

    fn location(len: u64) -> Location {
        Location { key: KEY.into(), offset: 0, len }
    }

    const EXTENT: ExtentId = ExtentId::new(1, 1);

    type Read = Result<Vec<u8>, Error>;

    /// Reads, each spawned, whose results land in the returned cells.
    struct Reads {
        pool: LocalPool,
        results: Vec<Rc<RefCell<Option<Read>>>>,
        aborts: Vec<futures::future::AbortHandle>,
        /// What fetchers spawned, to be run.
        spawned: Arc<Mutex<Vec<BoxFuture<'static, ()>>>>,
    }

    impl Reads {
        fn new() -> Self {
            let spawned = Arc::default();
            Self { pool: LocalPool::new(), results: Vec::new(), aborts: Vec::new(), spawned }
        }

        /// A spawner for fetchers' read-ahead, which runs with the reads.
        fn spawn(&self) -> Spawn {
            let spawned = self.spawned.clone();
            Arc::new(move |future| spawned.lock().unwrap().push(future))
        }

        /// Runs the reads, and what fetchers spawned, until they all wait.
        fn run(&mut self) {
            loop {
                self.pool.run_until_stalled();
                let spawned = std::mem::take(&mut *self.spawned.lock().unwrap());
                if spawned.is_empty() {
                    return;
                }
                for future in spawned {
                    self.pool.spawner().spawn_local(future).unwrap();
                }
            }
        }

        /// Reads `len` bytes at `offset` in [`EXTENT`], at `location`, with `fetcher`
        /// ([`Reads::read_of`]).
        fn read(
            &mut self,
            fetcher: &Rc<Fetcher<SimBucket>>,
            location: Location,
            offset: u64,
            len: u64,
            priority: Priority,
        ) -> usize {
            self.read_of(fetcher, EXTENT, location, offset, len, priority)
        }

        /// Reads `len` bytes at `offset` in `extent`, at `location`, with `fetcher`, running the
        /// reads until they all wait: the read, by which its result is found.
        fn read_of(
            &mut self,
            fetcher: &Rc<Fetcher<SimBucket>>,
            extent: ExtentId,
            location: Location,
            offset: u64,
            len: u64,
            priority: Priority,
        ) -> usize {
            let fetcher = fetcher.clone();
            let (result, abort) = self.spawned(async move {
                fetcher.read(extent, &location, offset, len, priority).await
            });
            self.results.push(result);
            self.aborts.push(abort);
            self.results.len() - 1
        }

        /// Fetches the parts of [`EXTENT`], at `location`, holding the `len` bytes at `offset`
        /// ahead, with `fetcher`, running the reads until they all wait: where its result lands.
        fn fetch_ahead(
            &mut self,
            fetcher: &Rc<Fetcher<SimBucket>>,
            location: Location,
            offset: u64,
            len: u64,
        ) -> Rc<RefCell<Option<Result<u64, Error>>>> {
            let fetcher = fetcher.clone();
            let fetched = async move { fetcher.fetch_ahead(EXTENT, &location, offset, len).await };
            self.spawned(fetched).0
        }

        /// Spawns `future`, abortable, and runs the reads until they all wait: where its output
        /// lands, and what aborts it.
        fn spawned<T: 'static>(
            &mut self,
            future: impl Future<Output = T> + 'static,
        ) -> (Rc<RefCell<Option<T>>>, futures::future::AbortHandle) {
            let result = Rc::new(RefCell::new(None));
            let into = result.clone();
            let (future, abort) = futures::future::abortable(async move {
                *into.borrow_mut() = Some(future.await);
            });
            self.pool.spawner().spawn_local(future.map(drop)).unwrap();
            self.run();
            (result, abort)
        }

        /// Abandons read `read`.
        fn abandon(&mut self, read: usize) {
            self.aborts[read].abort();
            self.run();
        }

        fn result(&self, read: usize) -> Option<Read> {
            self.results[read].borrow().clone()
        }
    }

    /// Completes the waiting requests, each as it comes, until none waits.
    fn complete_all(bucket: &SimBucket, reads: &mut Reads) -> Vec<String> {
        let mut completed = Vec::new();
        while bucket.waiting() > 0 {
            completed.push(bucket.describe(0));
            bucket.complete(0, Fault::None);
            reads.run();
        }
        completed
    }

    fn limits(requests: usize) -> Arc<Budget> {
        Budget::new(Limits { requests, bytes: u64::MAX })
    }

    /// Enters a GET of `bytes` on demand into `budget`: its slot, if it went at once, or the wait
    /// for it.
    fn enter(budget: &Arc<Budget>, bytes: u64) -> Result<Slot, BoxFuture<'static, Slot>> {
        let (_, given) = budget.enter(Priority::Demand, bytes);
        let mut given = given.boxed();
        match (&mut given).now_or_never() {
            Some(slot) => Ok(slot),
            None => Err(given),
        }
    }

    /// TASK-633.4: under budgets per bucket, a bucket's GETs filling its budget hold up none of
    /// another bucket's, which one budget for all would; keys of one bucket's cells, and of no
    /// cell's, share theirs.
    #[test]
    fn budgets_per_bucket_hold_up_no_other_buckets_gets() {
        use crate::layout::{CellId, head};
        let limits = Limits { requests: 1, bytes: 10 };
        let (one, two) =
            (CellId { tenant: 1, id: 1 << 48 | 3 }, CellId { tenant: 1, id: 2 << 48 | 3 });
        let per = Budgets::per_bucket(limits);
        let _full = enter(&per.of(&head(two)), 1).ok().unwrap();
        assert!(enter(&per.of(&head(one)), 1).is_ok(), "bucket 1's GET waited for bucket 2's");
        assert!(enter(&per.of(&head(two)), 1).is_err(), "bucket 2's budget let two GETs go");
        assert!(Arc::ptr_eq(&per.of(&head(two)), &per.of(&crate::layout::segments(two))));
        assert!(Arc::ptr_eq(&per.of("identity"), &per.of(&head(CellId { tenant: 1, id: 7 }))));
        let shared = Budgets::from(Budget::new(limits));
        let _full = enter(&shared.of(&head(two)), 1).ok().unwrap();
        assert!(enter(&shared.of(&head(one)), 1).is_err(), "one budget for all let two GETs go");
    }

    /// The GETs and bytes `budget` counts in flight.
    fn in_flight(budget: &Budget) -> (usize, u64) {
        let queues = budget.queues.lock().unwrap();
        (queues.requests, queues.bytes)
    }

    /// GETs go while both their number and their bytes are within the limits.
    #[test]
    fn gets_go_within_both_limits() {
        let budget = Budget::new(Limits { requests: 2, bytes: 10 });
        let first = enter(&budget, 1).ok().unwrap();
        let _second = enter(&budget, 1).ok().unwrap();
        let third = enter(&budget, 1).err().unwrap();
        drop(first);
        assert!(third.now_or_never().is_some());

        let budget = Budget::new(Limits { requests: 4, bytes: 10 });
        let _six = enter(&budget, 6).ok().unwrap();
        let _four = enter(&budget, 4).ok().unwrap();
        assert!(enter(&budget, 1).is_err());
    }

    /// A GET larger than the bytes allowed goes alone.
    #[test]
    fn a_get_over_the_bytes_goes_alone() {
        let budget = Budget::new(Limits { requests: 4, bytes: 10 });
        let large = enter(&budget, 20).ok().unwrap();
        let after = enter(&budget, 1).err().unwrap();
        drop(large);
        assert!(after.now_or_never().is_some());
    }

    /// A GET whose read gave up waiting gives its place, once given it, to the next.
    #[test]
    fn gets_given_up_give_back_their_places() {
        let budget = Budget::new(Limits { requests: 1, bytes: 100 });
        let three = enter(&budget, 3).ok().unwrap();
        drop(enter(&budget, 2).err().unwrap());
        drop(three);
        assert_eq!(in_flight(&budget), (0, 0));
    }

    /// Reads fetch the object's tail, once, then the parts that hold what they want, whole.
    #[test]
    fn reads_fetch_whole_parts() {
        let data = noise(2 * PART + 300);
        let bucket = bucket(&data);
        let fetcher = Rc::new(Fetcher::new(bucket.clone(), limits(8), memory(), None));
        let mut reads = Reads::new();
        let location = location(data.len() as u64);
        let read = reads.read(&fetcher, location.clone(), PART - 10, 20, Priority::Demand);
        let requests = complete_all(&bucket, &mut reads);
        let parts = [format!("get {KEY} 0..{PART}"), format!("get {KEY} {PART}..{}", 2 * PART)];
        assert_eq!(
            requests,
            [format!("get {KEY} last {TAIL}"), parts[0].clone(), parts[1].clone()]
        );
        let want = data[(PART - 10) as usize..(PART + 10) as usize].to_vec();
        assert_eq!(reads.result(read), Some(Ok(want)));

        // The tail is kept; past the extent's end, a read reads short.
        let read = reads.read(&fetcher, location, 2 * PART + 200, 1000, Priority::Demand);
        let requests = complete_all(&bucket, &mut reads);
        assert_eq!(requests, [format!("get {KEY} {}..{}", 2 * PART, 2 * PART + 300)]);
        assert_eq!(reads.result(read), Some(Ok(data[(2 * PART + 200) as usize..].to_vec())));
        assert!(fetcher.shared.memory.parts.lock().unwrap().flights.is_empty());
    }

    /// Reads of one part while its GET is in flight share it.
    #[test]
    fn reads_of_a_part_share_its_get() {
        let data = noise(PART);
        let bucket = bucket(&data);
        let fetcher = Rc::new(Fetcher::new(bucket.clone(), limits(8), memory(), None));
        let mut reads = Reads::new();
        let location = location(PART);
        let all: Vec<_> = (0..5)
            .map(|read| reads.read(&fetcher, location.clone(), read * 100, 100, Priority::Demand))
            .collect();
        assert_eq!(complete_all(&bucket, &mut reads).len(), 2);
        for read in all {
            let at = read * 100;
            assert_eq!(reads.result(read), Some(Ok(data[at..at + 100].to_vec())));
        }
    }

    /// No more GETs than the budget allows are in flight; reads waiting for their data go before
    /// read-ahead, even read-ahead of a part they then want.
    #[test]
    fn demand_goes_before_read_ahead_within_the_budget() {
        let data = noise(6 * PART);
        let bucket = bucket(&data);
        let fetcher = Rc::new(Fetcher::new(bucket.clone(), limits(1), memory(), None));
        let mut reads = Reads::new();
        let location = location(6 * PART);
        let first = reads.read(&fetcher, location.clone(), 0, 1, Priority::Demand);
        // Its tail's GET goes, under its part's place, then its part's.
        assert_eq!(bucket.waiting(), 1);
        assert_eq!(complete_all(&bucket, &mut reads).len(), 2);
        assert_eq!(reads.result(first), Some(Ok(data[..1].to_vec())));

        let ahead: Vec<_> = (1..4)
            .map(|part| reads.read(&fetcher, location.clone(), part * PART, 1, Priority::Ahead))
            .collect();
        let demand = reads.read(&fetcher, location.clone(), 5 * PART, 1, Priority::Demand);
        let wanted = reads.read(&fetcher, location.clone(), 3 * PART, 1, Priority::Demand);
        assert_eq!(bucket.waiting(), 1);
        let order = complete_all(&bucket, &mut reads);
        let get = |part: u64| format!("get {KEY} {}..{}", part * PART, (part + 1) * PART);
        assert_eq!(order, [get(1), get(5), get(3), get(2)]);
        for (read, part) in ahead.into_iter().chain([demand, wanted]).zip([1, 2, 3, 5, 3]) {
            let at = (part * PART) as usize;
            assert_eq!(reads.result(read), Some(Ok(data[at..at + 1].to_vec())));
        }
    }

    /// TASK-702.3: parts fetched ahead go as many at once as the budget lets them, behind reads
    /// that wait for their data, into the cache; a part it holds is not fetched again.
    #[test]
    fn parts_fetched_ahead_go_many_at_once_behind_demand() {
        let data = noise(12 * PART);
        let bucket = bucket(&data);
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(dir.path(), 16 * PART).unwrap());
        let fetcher =
            Rc::new(Fetcher::new(bucket.clone(), limits(8), memory(), Some(cache.clone())));
        let mut reads = Reads::new();
        let location = location(12 * PART);
        let first = reads.read(&fetcher, location.clone(), 0, 1, Priority::Demand);
        let get = |part: u64| format!("get {KEY} {}..{}", part * PART, (part + 1) * PART);
        assert_eq!(complete_all(&bucket, &mut reads), [format!("get {KEY} last {TAIL}"), get(0)]);
        assert_eq!(reads.result(first), Some(Ok(data[..1].to_vec())));

        let ahead: Vec<_> = (1..11)
            .map(|part| reads.fetch_ahead(&fetcher, location.clone(), part * PART, 1))
            .collect();
        let waiting = |bucket: &SimBucket| {
            let mut waiting: Vec<_> = (0..bucket.waiting()).map(|at| bucket.describe(at)).collect();
            waiting.sort();
            waiting
        };
        let sorted = |parts: &[u64]| {
            let mut gets: Vec<_> = parts.iter().map(|part| get(*part)).collect();
            gets.sort();
            gets
        };
        assert_eq!(waiting(&bucket), sorted(&[1, 2, 3, 4, 5, 6, 7, 8]));
        let demand = reads.read(&fetcher, location.clone(), 11 * PART, 1, Priority::Demand);
        assert_eq!(bucket.waiting(), 8);
        // The first place free goes to the read waiting for its data, not to those ahead.
        let done = bucket.describe(0);
        bucket.complete(0, Fault::None);
        reads.run();
        assert!(!waiting(&bucket).contains(&done));
        let fetching: Vec<_> = (1..9).filter(|part| get(*part) != done).chain([11]).collect();
        assert_eq!(waiting(&bucket), sorted(&fetching));
        complete_all(&bucket, &mut reads);
        assert_eq!(reads.result(demand), Some(Ok(data[11 * PART as usize..][..1].to_vec())));
        for fetched in ahead {
            assert_eq!(*fetched.borrow(), Some(Ok(1)));
        }
        assert!((1..12).all(|part| cache.holds(KEY, part)));

        let again = reads.fetch_ahead(&fetcher, location.clone(), 3 * PART + 5, 1);
        assert_eq!((bucket.waiting(), &*again.borrow()), (0, &Some(Ok(0))));
    }

    /// TASK-702.3: what is fetched ahead is every part the bytes asked for are in, an extent's
    /// unit spanning two where the extent begins within a part; a read of them then sends no GET.
    #[test]
    fn units_fetched_ahead_take_every_part_they_span() {
        let data = noise(3 * PART);
        let location = Location { key: KEY.into(), offset: PART / 2, len: 2 * PART };
        let bucket = holding(segment(&data, vec![(EXTENT, location.offset, location.len)]));
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(dir.path(), 16 * PART).unwrap());
        let fetcher = Rc::new(Fetcher::new(bucket.clone(), limits(8), memory(), Some(cache)));
        let mut reads = Reads::new();
        let fetched = reads.fetch_ahead(&fetcher, location.clone(), 0, PART);
        let mut asked = complete_all(&bucket, &mut reads);
        asked.sort();
        let get = |part: u64| format!("get {KEY} {}..{}", part * PART, (part + 1) * PART);
        assert_eq!(asked, [get(0), get(1), format!("get {KEY} last {TAIL}")]);
        assert_eq!(*fetched.borrow(), Some(Ok(2)));
        let read = reads.read(&fetcher, location, 0, PART, Priority::Demand);
        assert_eq!(bucket.waiting(), 0);
        let at = (PART / 2) as usize;
        assert_eq!(reads.result(read), Some(Ok(data[at..at + PART as usize].to_vec())));
    }

    /// TASK-702.3: a part fetched ahead whose GET another read sent counts as none fetched.
    #[test]
    fn parts_another_read_fetches_count_as_none_fetched_ahead() {
        let data = noise(2 * PART);
        let bucket = bucket(&data);
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(dir.path(), 16 * PART).unwrap());
        let fetcher = Rc::new(Fetcher::new(bucket.clone(), limits(8), memory(), Some(cache)));
        let mut reads = Reads::new();
        let location = location(2 * PART);
        let first = reads.fetch_ahead(&fetcher, location.clone(), PART, 1);
        let second = reads.fetch_ahead(&fetcher, location.clone(), PART, 1);
        let get = format!("get {KEY} {PART}..{}", 2 * PART);
        assert_eq!(complete_all(&bucket, &mut reads), [format!("get {KEY} last {TAIL}"), get]);
        assert_eq!((&*first.borrow(), &*second.borrow()), (&Some(Ok(1)), &Some(Ok(0))));
    }

    /// TASK-702.3: an object's tail read for what is fetched ahead goes behind reads that wait for
    /// their data, until a read waits for it too, which moves it up to demand.
    #[test]
    fn tails_read_ahead_go_behind_demand_until_a_read_waits_for_them() {
        for waited in [false, true] {
            let data = noise(2 * PART);
            let other = layout::segment(test_cell(1), 2, 1);
            let bucket = bucket(&data);
            let direct = Direct(bucket.clone());
            block_on(direct.put(&other, whole(&data).into(), PutMode::Create)).unwrap();
            let dir = tempfile::tempdir().unwrap();
            let cache = Arc::new(Cache::open(dir.path(), 16 * PART).unwrap());
            let fetcher = Rc::new(Fetcher::new(bucket.clone(), limits(1), memory(), Some(cache)));
            let mut reads = Reads::new();
            let theirs = Location { key: other.clone(), offset: 0, len: 2 * PART };
            // A read of the first part of KEY's object holds the budget's one place.
            reads.read(&fetcher, location(2 * PART), 0, 1, Priority::Demand);
            bucket.complete(0, Fault::None);
            reads.run();
            assert_eq!(bucket.describe(0), format!("get {KEY} 0..{PART}"));
            reads.fetch_ahead(&fetcher, theirs.clone(), 0, 1);
            if waited {
                reads.read(&fetcher, theirs, 0, 1, Priority::Demand);
            }
            reads.read(&fetcher, location(2 * PART), PART, 1, Priority::Demand);
            bucket.complete(0, Fault::None);
            reads.run();
            let next = match waited {
                true => format!("get {other} last {TAIL}"),
                false => format!("get {KEY} {PART}..{}", 2 * PART),
            };
            assert_eq!((bucket.waiting(), bucket.describe(0)), (1, next), "waited: {waited}");
            complete_all(&bucket, &mut reads);
        }
    }

    /// A GET no read waits for any longer gives up its place in the budget, and is forgotten.
    #[test]
    fn gets_no_read_waits_for_go() {
        let data = noise(3 * PART);
        let bucket = bucket(&data);
        let fetcher = Rc::new(Fetcher::new(bucket.clone(), limits(1), memory(), None));
        let mut reads = Reads::new();
        let location = location(3 * PART);
        reads.read(&fetcher, location.clone(), 0, 1, Priority::Demand);
        complete_all(&bucket, &mut reads);

        let first = reads.read(&fetcher, location.clone(), 0, 1, Priority::Demand);
        let waiting = reads.read(&fetcher, location.clone(), PART, 1, Priority::Ahead);
        let given = reads.read(&fetcher, location.clone(), 2 * PART, 1, Priority::Ahead);
        reads.abandon(waiting);
        assert_eq!(fetcher.shared.memory.parts.lock().unwrap().flights.len(), 2);
        // The first's GET went, and its answer comes to none; its place goes to the next.
        reads.abandon(first);
        assert_eq!(fetcher.shared.memory.parts.lock().unwrap().flights.len(), 1);
        let order = complete_all(&bucket, &mut reads);
        let get = |part: u64| format!("get {KEY} {}..{}", part * PART, (part + 1) * PART);
        assert_eq!(order, [get(0), get(2)]);
        assert_eq!(reads.result(given), Some(Ok(data[2 * PART as usize..][..1].to_vec())));
        assert!(fetcher.shared.memory.parts.lock().unwrap().flights.is_empty());
    }

    /// A part that fails its CRC is corrupt, and the read fails: the object at a key is the one its
    /// tail is of, for good (TASK-407). Before, it was fetched again, once, by its object's tail
    /// fetched anew, as an object recreated at its key would have it read.
    #[test]
    fn parts_failing_their_crcs_are_corrupt() {
        let data = noise(PART + 100);
        let bucket = bucket(&data);
        let fetcher = Rc::new(Fetcher::new(bucket.clone(), limits(8), memory(), None));
        let mut reads = Reads::new();
        let mut flipped = whole(&data);
        flipped[PART as usize + 7] ^= 1;
        block_on(Direct(bucket.clone()).put(KEY, flipped.into(), PutMode::Overwrite)).unwrap();
        let read = reads.read(&fetcher, location(PART + 100), PART, 100, Priority::Demand);
        let (tail, part) =
            (format!("get {KEY} last {TAIL}"), format!("get {KEY} {PART}..{}", PART + 100));
        assert_eq!(complete_all(&bucket, &mut reads), [tail, part]);
        let corrupt = format!("part 1 of {KEY}, by its CRC");
        assert_eq!(reads.result(read), Some(Err(Error::Corrupt(corrupt))));
    }

    /// Parts fetched are kept, and their objects' tails, and read kept, with no GET.
    #[test]
    fn reads_keep_what_they_fetch() {
        let data = noise(2 * PART);
        let bucket = bucket(&data);
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(dir.path(), 8 * PART).unwrap());
        let fetcher =
            Rc::new(Fetcher::new(bucket.clone(), limits(8), memory(), Some(cache.clone())));
        let mut reads = Reads::new();
        let read = reads.read(&fetcher, location(2 * PART), PART + 5, 10, Priority::Demand);
        assert_eq!(complete_all(&bucket, &mut reads).len(), 2);
        let want = Some(Ok(data[(PART + 5) as usize..][..10].to_vec()));
        assert_eq!(reads.result(read), want);
        assert_eq!(cache.bytes(), PART + tail_len(&data));

        let read = reads.read(&fetcher, location(2 * PART), PART + 5, 10, Priority::Demand);
        assert_eq!(bucket.waiting(), 0);
        assert_eq!(reads.result(read), want);
    }

    /// TASK-672.2: a read says the farthest it went for its data ([`trace::sourced`]): the bucket,
    /// for the read whose GET it was and for one that joined that GET in flight alike; the host's
    /// memory, read again; the part cache, read by another host's fetcher sharing it, whose memory
    /// holds none of it.
    #[test]
    fn reads_say_how_far_they_went() {
        trace::enable(true);
        let data = noise(PART);
        let bucket = bucket(&data);
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(dir.path(), 8 * PART).unwrap());
        let fetcher = |memory: &Arc<Memory>| {
            Rc::new(Fetcher::new(bucket.clone(), limits(8), memory.clone(), Some(cache.clone())))
        };
        let mut reads = Reads::new();
        let want = data[5..15].to_vec();
        let read = |fetcher: &Rc<Fetcher<SimBucket>>, reads: &mut Reads| {
            let landed = Rc::new(RefCell::new(None));
            let (fetcher, into, want) = (fetcher.clone(), landed.clone(), want.clone());
            let read = async move {
                let at = location(PART);
                let (read, source) =
                    trace::sourced(fetcher.read(EXTENT, &at, 5, 10, Priority::Demand)).await;
                assert_eq!(read, Ok(want));
                *into.borrow_mut() = Some(source);
            };
            reads.pool.spawner().spawn_local(read).unwrap();
            reads.run();
            landed
        };
        let host = memory();
        let (first, joined) =
            (read(&fetcher(&host), &mut reads), read(&fetcher(&host), &mut reads));
        assert_eq!(complete_all(&bucket, &mut reads).len(), 2, "a tail's GET and a part's");
        assert_eq!(*first.borrow(), Some(Some(Source::Bucket)));
        assert_eq!(*joined.borrow(), Some(Some(Source::Bucket)));
        assert_eq!(*read(&fetcher(&host), &mut reads).borrow(), Some(Some(Source::Memory)));
        let other = read(&fetcher(&memory()), &mut reads);
        assert_eq!(bucket.waiting(), 0);
        assert_eq!(*other.borrow(), Some(Some(Source::PartCache)));
    }

    /// A part kept that fails its CRC is dropped and fetched; fetched failing it too, the read
    /// fails, corrupt. Read by a fetcher with a memory of its own each time, which holds none of
    /// it: the part, and its object's tail, are in the cache only.
    #[test]
    fn parts_kept_failing_their_crcs_are_fetched() {
        let data = noise(PART);
        let bucket = bucket(&data);
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(dir.path(), 8 * PART).unwrap());
        let fetcher =
            || Rc::new(Fetcher::new(bucket.clone(), limits(8), memory(), Some(cache.clone())));
        let mut reads = Reads::new();
        reads.read(&fetcher(), location(PART), 0, 10, Priority::Demand);
        complete_all(&bucket, &mut reads);
        let flip = |bytes: &mut Vec<u8>| bytes[3] ^= 1;
        let mut kept = std::fs::read(cache.path(KEY)).unwrap();
        flip(&mut kept);
        std::fs::write(cache.path(KEY), kept).unwrap();

        // The bucket's is whole.
        let read = reads.read(&fetcher(), location(PART), 0, 10, Priority::Demand);
        let part = format!("get {KEY} 0..{PART}");
        assert_eq!(complete_all(&bucket, &mut reads), std::slice::from_ref(&part));
        assert_eq!(reads.result(read), Some(Ok(data[..10].to_vec())));
        // As stored, encrypted.
        let object = stored_at(&bucket);
        let stored = extent_frame::Parts::of(&object, &extent_frame::NoKeys).unwrap().stored[0];
        let stored = object[stored.range().start as usize..stored.range().end as usize].to_vec();
        assert_eq!(cache.part(KEY, 0).unwrap(), Some(stored));

        // Neither is.
        let mut kept = std::fs::read(cache.path(KEY)).unwrap();
        flip(&mut kept);
        std::fs::write(cache.path(KEY), kept).unwrap();
        let mut object = stored_at(&bucket);
        flip(&mut object);
        block_on(Direct(bucket.clone()).put(KEY, object.into(), PutMode::Overwrite)).unwrap();
        let read = reads.read(&fetcher(), location(PART), 0, 10, Priority::Demand);
        assert_eq!(complete_all(&bucket, &mut reads), [part]);
        let corrupt = format!("part 0 of {KEY}, by its CRC");
        assert_eq!(reads.result(read), Some(Err(Error::Corrupt(corrupt))));
        assert_eq!(cache.part(KEY, 0).unwrap(), None);
    }

    /// Reads read ahead the part after them, and twice as many each time they go on where the
    /// last stopped; a read elsewhere reads only the part after it ahead again.
    #[test]
    fn read_ahead_grows_along_sequential_reads() {
        let data = noise(12 * PART);
        let bucket = bucket(&data);
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(dir.path(), 64 * PART).unwrap());
        let mut reads = Reads::new();
        let fetcher = Fetcher::new(bucket.clone(), limits(8), memory(), Some(cache.clone()));
        let fetcher = Rc::new(fetcher.reading_ahead(reads.spawn()));
        let location = location(12 * PART);
        let get = |part: u64| format!("get {KEY} {}..{}", part * PART, (part + 1) * PART);
        let mut read_part = |part: u64| {
            let read = reads.read(&fetcher, location.clone(), part * PART, 10, Priority::Demand);
            let mut requests = complete_all(&bucket, &mut reads);
            requests.sort();
            let at = (part * PART) as usize;
            assert_eq!(reads.result(read), Some(Ok(data[at..at + 10].to_vec())));
            requests
        };
        assert_eq!(read_part(0), [get(0), get(1), format!("get {KEY} last {TAIL}")]);
        assert_eq!(read_part(1), [get(2), get(3)]);
        assert_eq!(read_part(2), [get(4), get(5), get(6)]);
        // Not past the object's end.
        assert_eq!(read_part(3), [get(10), get(11), get(7), get(8), get(9)]);
        assert_eq!(read_part(7), Vec::<String>::new());
        assert_eq!(cache.bytes(), 12 * PART + tail_len(&data));
    }

    /// TASK-225: another fetcher of the host's, following reads afresh, reads ahead none of the
    /// parts the cache holds: neither from the bucket nor from the cache. Here the cache's copies
    /// of the parts are spoilt, so that a read of one there fails its CRC and is fetched again:
    /// only the part the read wants is, not the one after it, read ahead.
    #[test]
    fn read_ahead_reads_nothing_the_cache_holds() {
        let data = noise(4 * PART);
        let bucket = bucket(&data);
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(dir.path(), 64 * PART).unwrap());
        let location = location(4 * PART);
        let mut reads = Reads::new();
        let first = Fetcher::new(bucket.clone(), limits(8), memory(), Some(cache.clone()));
        let first = Rc::new(first.reading_ahead(reads.spawn()));
        for part in 0..2 {
            reads.read(&first, location.clone(), part * PART, 10, Priority::Demand);
        }
        // Parts 0 and 1, read, and 2 and 3, read ahead; and the tail.
        assert_eq!(complete_all(&bucket, &mut reads).len(), 5);
        let file = std::fs::OpenOptions::new().write(true).open(cache.path(KEY)).unwrap();
        std::os::unix::fs::FileExt::write_all_at(&file, &vec![0; 4 * PART as usize], 0).unwrap();
        let other = Fetcher::new(bucket.clone(), limits(8), memory(), Some(cache.clone()));
        let other = Rc::new(other.reading_ahead(reads.spawn()));
        let read = reads.read(&other, location, 2 * PART, 10, Priority::Demand);
        let part = format!("get {KEY} {}..{}", 2 * PART, 3 * PART);
        assert_eq!(complete_all(&bucket, &mut reads), [part]);
        let at = 2 * PART as usize;
        assert_eq!(reads.result(read), Some(Ok(data[at..at + 10].to_vec())));
    }

    /// A part stored compressed is fetched with one ranged GET of its stored bytes, checked and
    /// decompressed, and kept compressed.
    #[test]
    fn compressed_parts_are_fetched_and_kept_so() {
        let data = pattern(2 * PART);
        let bucket = bucket(&data);
        let parts = extent_frame::Parts::of(&whole(&data), &extent_frame::NoKeys).unwrap();
        let stored = parts.stored[1];
        assert_eq!(stored.encoding, extent_frame::Encoding::Zstd);
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(dir.path(), 8 * PART).unwrap());
        let fetcher =
            Rc::new(Fetcher::new(bucket.clone(), limits(8), memory(), Some(cache.clone())));
        let mut reads = Reads::new();
        let read = reads.read(&fetcher, location(2 * PART), PART + 5, 10, Priority::Demand);
        let range = stored.range();
        assert_eq!(
            complete_all(&bucket, &mut reads),
            [format!("get {KEY} last {TAIL}"), format!("get {KEY} {}..{}", range.start, range.end)]
        );
        let want = Some(Ok(data[(PART + 5) as usize..][..10].to_vec()));
        assert_eq!(reads.result(read), want);
        assert_eq!(cache.bytes(), stored.len + tail_len(&data));
        assert!(stored.len < PART / 100, "{} bytes stored", stored.len);

        let read = reads.read(&fetcher, location(2 * PART), PART + 5, 10, Priority::Demand);
        assert_eq!(bucket.waiting(), 0);
        assert_eq!(reads.result(read), want);
    }

    /// Where [`EXTENT`] is in the segment at [`KEY`] in the tests of an object gone.
    const HELD: (u64, u64) = (1000, 1000);

    /// cladefs: a read by a fetcher that kept the tail of an object deleted since, its extents
    /// relocated by its cell's writer (doc-27), finds its part gone, and the extent lost there,
    /// which its reader looks up again: as the object at a key is the one its tail is of, for good
    /// (TASK-407), a tail kept is never another's.
    #[test]
    fn reads_by_tails_kept_of_objects_gone_are_lost() {
        let data = noise(3000);
        let bucket = holding(segment(&data, vec![(EXTENT, HELD.0, HELD.1)]));
        let fetcher = Rc::new(Fetcher::new(bucket.clone(), limits(8), memory(), None));
        let at = Location { key: KEY.into(), offset: HELD.0, len: HELD.1 };
        let mut reads = Reads::new();
        let read = reads.read(&fetcher, at.clone(), 100, 50, Priority::Demand);
        complete_all(&bucket, &mut reads);
        assert_eq!(reads.result(read), Some(Ok(data[1100..1150].to_vec())));
        block_on(Direct(bucket.clone()).delete(KEY)).unwrap();
        let read = reads.read(&fetcher, at, 100, 50, Priority::Demand);
        assert_eq!(complete_all(&bucket, &mut reads), [format!("get {KEY} 0..3000")]);
        assert_eq!(reads.result(read), Some(Err(Error::Lost(format!("{EXTENT:?} in {KEY}")))));
    }

    /// A read of an extent by a fetcher of its own ([`read`]) asks for its object's tail, then for
    /// the parts the extent is in; with the object gone, the extent is lost.
    #[test]
    fn reads_on_their_own_fetch_the_tail_then_the_parts() {
        let data = noise(PART + 300);
        let location = Location { key: KEY.into(), offset: PART - 100, len: 300 };
        let bucket = holding(segment(&data, vec![(EXTENT, location.offset, location.len)]));
        let (read_back, asked) = bucket.served(read(&bucket, &ring(), EXTENT, &location));
        assert_eq!(read_back, Ok(data[(PART - 100) as usize..(PART + 200) as usize].to_vec()));
        let parts = [format!("get {KEY} 0..{PART}"), format!("get {KEY} {PART}..{}", PART + 300)];
        assert_eq!(asked, [format!("get {KEY} last {TAIL}"), parts[0].clone(), parts[1].clone()]);

        block_on(Direct(bucket.clone()).delete(KEY)).unwrap();
        let (lost, asked) = bucket.served(read(&bucket, &ring(), EXTENT, &location));
        assert_eq!(lost, Err(Error::Lost(format!("{EXTENT:?} in {KEY}"))));
        assert_eq!(asked, [format!("get {KEY} last {TAIL}")]);
    }

    /// A range of an object's data is read from the parts it touches; an empty one reads none; one
    /// reversed, or past the data's end, is corrupt, and read not.
    #[test]
    fn data_is_read_by_the_parts_a_range_touches() {
        let data = pattern(2 * PART + 100);
        let bucket = bucket(&data);
        let fetcher = Fetcher::new(bucket.clone(), limits(8), memory(), None);
        let tail = bucket.served(land(fetcher.shared.tail(KEY))).0.unwrap().unwrap();
        let read = |range| bucket.served(fetcher.data(KEY, &tail, range, Priority::Demand));

        let (read_back, asked) = read(PART - 5..PART + 5);
        let want = data[(PART - 5) as usize..(PART + 5) as usize].to_vec();
        assert_eq!((read_back, asked.len()), (Ok(Found::Bytes(want)), 2));
        assert_eq!(read(7..7), (Ok(Found::Bytes(Vec::new())), Vec::new()));
        let end = data.len() as u64;
        for range in [Range { start: 8, end: 7 }, end - 1..end + 1] {
            let corrupt = format!("{range:?} past the end of {KEY}");
            assert_eq!(read(range), (Err(Error::Corrupt(corrupt)), Vec::new()));
        }
    }

    /// TASK-90: an extent is looked up in a checkpoint by the block of its table that would hold
    /// its entry, read by its parts, which are kept, as the checkpoint's tail is; before the
    /// table's first entry, no block is read. A checkpoint gone is lost; an object that is none
    /// holds no table.
    #[test]
    fn lookups_read_blocks_of_checkpoints_by_their_parts() {
        let mut state = crate::recovery::State::new(test_cell(1));
        let location = Location { key: KEY.into(), offset: 7, len: 9 };
        state.extents.insert(EXTENT, location.clone());
        let checkpoint = layout::checkpoint(test_cell(1), 3, 1);
        let bytes =
            crate::checkpoint::encode(&state, crate::testing::encrypting(layout::test_cell(1)));
        let bucket = SimBucket::new();
        block_on(Direct(bucket.clone()).put(&checkpoint, bytes.clone().into(), PutMode::Create))
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(dir.path(), 8 * PART).unwrap());
        let fetcher = Fetcher::new(bucket.clone(), limits(8), memory(), Some(cache));
        let lookup = |key: &str, extent| bucket.served(fetcher.lookup(key, extent));

        let stored =
            extent_frame::Parts::of(&bytes, &extent_frame::NoKeys).unwrap().stored[0].range();
        let read =
            [format!("get {checkpoint} last {TAIL}"), format!("get {checkpoint} {stored:?}")];
        assert_eq!(lookup(&checkpoint, EXTENT), (Ok(Some(location.clone())), read.to_vec()));
        assert_eq!(lookup(&checkpoint, EXTENT), (Ok(Some(location)), Vec::new()));
        assert_eq!(lookup(&checkpoint, ExtentId::new(1, 0)), (Ok(None), Vec::new()));

        block_on(Direct(bucket.clone()).delete(&checkpoint)).unwrap();
        let fetcher = Fetcher::new(bucket.clone(), limits(8), memory(), None);
        let (gone, _) = bucket.served(fetcher.lookup(&checkpoint, EXTENT));
        assert_eq!(gone, Err(Error::Lost(checkpoint)));
        block_on(Direct(bucket.clone()).put(KEY, whole(&noise(100)).into(), PutMode::Create))
            .unwrap();
        let (none, _) = bucket.served(fetcher.lookup(KEY, EXTENT));
        assert_eq!(none, Err(Error::Corrupt(format!("{KEY} holds no checkpoint"))));
    }

    /// A read that ends where a part does fetches no part after it.
    #[test]
    fn reads_ending_with_a_part_fetch_none_after_it() {
        let data = noise(2 * PART + 300);
        let bucket = bucket(&data);
        let fetcher = Rc::new(Fetcher::new(bucket.clone(), limits(8), memory(), None));
        let mut reads = Reads::new();
        let read = reads.read(&fetcher, location(2 * PART + 300), PART - 10, 10, Priority::Demand);
        assert_eq!(
            complete_all(&bucket, &mut reads),
            [format!("get {KEY} last {TAIL}"), format!("get {KEY} 0..{PART}")]
        );
        assert_eq!(
            reads.result(read),
            Some(Ok(data[(PART - 10) as usize..PART as usize].to_vec()))
        );
    }

    /// TASK-79.18: a read of an extent in a segment naming a thousand extents, as one of 32 MiB of
    /// extents of 32 KiB does, asks for its object's tail once, then for its part: the footer,
    /// which says where the extent is, comes with the first read of the tail.
    #[test]
    fn reads_of_segments_naming_many_extents_read_their_tails_at_once() {
        let data = noise(PART);
        let others = (1..1024).map(|sequence| (ExtentId::new(2, sequence), sequence * 1024, 1024));
        let bucket =
            holding(segment(&data, [(EXTENT, 0, 1024)].into_iter().chain(others).collect()));
        let fetcher = Rc::new(Fetcher::new(bucket.clone(), limits(8), memory(), None));
        let mut reads = Reads::new();
        let read = reads.read(&fetcher, location(1024), 0, 10, Priority::Demand);
        assert_eq!(
            complete_all(&bucket, &mut reads),
            [format!("get {KEY} last {TAIL}"), format!("get {KEY} 0..{PART}")]
        );
        assert_eq!(reads.result(read), Some(Ok(data[..10].to_vec())));
    }

    /// An object whose tail is longer than a tail's first read, its footer naming more extents than
    /// fit, is read with a second, of the whole of its footer and table.
    #[test]
    fn long_tails_are_read_whole() {
        let data = noise(PART);
        let named = TAIL / 32;
        let others = (1..=named).map(|sequence| (ExtentId::new(2, sequence), sequence, 1));
        let bytes = segment(&data, [(EXTENT, 0, PART)].into_iter().chain(others).collect());
        let frame = Frame::of(&bytes).unwrap();
        let tail = frame.tail();
        assert!(tail.end - tail.start > TAIL);
        let bucket = holding(bytes);
        let fetcher = Rc::new(Fetcher::new(bucket.clone(), limits(8), memory(), None));
        let mut reads = Reads::new();
        let read = reads.read(&fetcher, location(PART), PART - 10, 10, Priority::Demand);
        assert_eq!(
            complete_all(&bucket, &mut reads),
            [
                format!("get {KEY} last {TAIL}"),
                format!("get {KEY} {tail:?}"),
                format!("get {KEY} 0..{PART}")
            ]
        );
        assert_eq!(reads.result(read), Some(Ok(data[(PART - 10) as usize..].to_vec())));
    }

    /// An object shorter than a tail's read whose trailer says another length is corrupt.
    #[test]
    fn short_objects_whose_trailers_lie_are_corrupt() {
        let data = noise(100);
        let bucket = bucket(&data);
        let mut longer = vec![0; 10];
        longer.extend_from_slice(&whole(&data));
        block_on(Direct(bucket.clone()).put(KEY, longer.into(), PutMode::Overwrite)).unwrap();
        let fetcher = Rc::new(Fetcher::new(bucket.clone(), limits(8), memory(), None));
        let mut reads = Reads::new();
        let read = reads.read(&fetcher, location(100), 0, 10, Priority::Demand);
        complete_all(&bucket, &mut reads);
        assert_eq!(reads.result(read), Some(Err(Error::Corrupt(format!("{KEY}, by its trailer")))));
    }

    /// Objects' tails are kept, each, while reads go from one object to another; past [`TAILS`]
    /// bytes of them, those used least recently are forgotten, one at a time, a read in flight
    /// counting [`TAIL`] bytes, and read anew.
    #[test]
    fn tails_are_kept_for_each_object() {
        let data = noise(100);
        let bucket = bucket(&data);
        let other = &layout::segment(test_cell(1), 1, 1);
        block_on(Direct(bucket.clone()).put(other, whole(&data).into(), PutMode::Create)).unwrap();
        let fetcher = Rc::new(Fetcher::new(bucket.clone(), limits(8), memory(), None));
        let mut reads = Reads::new();
        let mut read = |key: &str| {
            let location = Location { key: key.into(), offset: 0, len: 100 };
            let read = reads.read(&fetcher, location, 0, 10, Priority::Demand);
            let requests = complete_all(&bucket, &mut reads);
            assert_eq!(reads.result(read), Some(Ok(data[..10].to_vec())));
            requests
        };
        assert_eq!(read(KEY), [format!("get {KEY} last {TAIL}"), format!("get {KEY} 0..100")]);
        assert_eq!(
            read(other),
            [format!("get {other} last {TAIL}"), format!("get {other} 0..100")]
        );
        assert_eq!(read(other), [format!("get {other} 0..100")]);
        assert_eq!(read(KEY), [format!("get {KEY} 0..100")]);

        // Reads in flight of nearly as many bytes as are kept; this one's tail read again, used
        // last; then another read in flight: the other's tail, used least recently, goes, then
        // the first read in flight.
        let kept = |key: &str| fetcher.shared.memory.tails.lock().unwrap().kept.peek(key).is_some();
        let key = |number: u64| layout::segment(test_cell(2), number as u64, 1);
        let reading = TAILS / TAIL;
        for number in 0..reading - 1 {
            drop(fetcher.shared.tail(&key(number)));
        }
        assert!(kept(KEY) && kept(other));
        assert_eq!(read(KEY), [format!("get {KEY} 0..100")]);
        drop(fetcher.shared.tail(&key(reading - 1)));
        let kept: Vec<_> = [other, KEY, &key(0), &key(1)].map(kept).into();
        assert_eq!(kept, [false, true, false, true]);
        assert_eq!(
            read(other),
            [format!("get {other} last {TAIL}"), format!("get {other} 0..100")]
        );
    }

    /// TASK-92.21: a demand read of a part read ahead takes it from what the read ahead fetched:
    /// neither from the bucket again nor from the cache's copy of it, removed here, so that
    /// reading it there would show as a GET.
    #[test]
    fn demand_reads_of_parts_read_ahead_take_what_was_fetched() {
        let data = pattern(4 * PART);
        let bucket = bucket(&data);
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(dir.path(), 64 * PART).unwrap());
        let mut reads = Reads::new();
        let fetcher = Fetcher::new(bucket.clone(), limits(8), memory(), Some(cache.clone()));
        let fetcher = Rc::new(fetcher.reading_ahead(reads.spawn()));
        let location = location(4 * PART);
        let parts = extent_frame::Parts::of(&whole(&data), &extent_frame::NoKeys).unwrap();
        let get = |part: usize| format!("get {KEY} {:?}", parts.stored[part].range());
        let mut read_at = |at: u64| {
            let read = reads.read(&fetcher, location.clone(), at, 10, Priority::Demand);
            let mut asked = complete_all(&bucket, &mut reads);
            asked.sort();
            assert_eq!(reads.result(read), Some(Ok(data[at as usize..][..10].to_vec())));
            asked
        };
        let mut first = vec![get(0), get(1), format!("get {KEY} last {TAIL}")];
        first.sort();
        assert_eq!(read_at(0), first);
        std::fs::remove_file(cache.path(KEY)).unwrap();
        let mut ahead = vec![get(2), get(3)];
        ahead.sort();
        assert_eq!(read_at(PART), ahead);
    }

    /// TASK-92.21: a benchmark of reads of an object's data from the bucket, one after another,
    /// through a fetcher reading ahead into a cache: of 64 KiB each, and of a part each, as a
    /// pager's page-ins are; how long they take, the data compressing as an image's might, at
    /// about a third. `cargo nextest run -p bucket_cell --run-ignored only
    /// bench_sequential_reads --no-capture`.
    #[test]
    #[ignore = "a benchmark (TASK-92.21): --run-ignored only, alone"]
    fn bench_sequential_reads() {
        const LEN: u64 = 64 * PART;
        // Each 1 KiB a run of a byte, then noise: about a third of it compresses away.
        let noise = noise(LEN);
        let data: Vec<u8> = (0..LEN as usize)
            .map(|i| if i % 1024 < 340 { (i / 1024) as u8 } else { noise[i] })
            .collect();
        for read_len in [64 << 10, PART] {
            let bucket = bucket(&data);
            let dir = tempfile::tempdir().unwrap();
            let cache = Arc::new(Cache::open(dir.path(), 2 * LEN).unwrap());
            let mut reads = Reads::new();
            let fetcher = Fetcher::new(bucket.clone(), limits(64), memory(), Some(cache.clone()));
            let fetcher = Rc::new(fetcher.reading_ahead(reads.spawn()));
            let location = location(LEN);
            let started = std::time::Instant::now();
            for at in (0..LEN).step_by(read_len as usize) {
                let read = reads.read(&fetcher, location.clone(), at, read_len, Priority::Demand);
                complete_all(&bucket, &mut reads);
                let read = reads.result(read).unwrap().unwrap();
                assert_eq!(read, data[at as usize..(at + read_len) as usize]);
            }
            let took = started.elapsed();
            println!("{} reads of {read_len} bytes, one after another: {took:?}", LEN / read_len);
        }
    }

    /// TASK-92.22: a fetcher reading parts another kept in the cache they share, which its memory
    /// holds not, as once the host's memory let them go, reads them, and the tails of their
    /// objects, from the cache: it asks the bucket for nothing.
    #[test]
    fn fetchers_sharing_a_cache_read_the_tails_kept_there() {
        let data = noise(2 * PART);
        let bucket = bucket(&data);
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(dir.path(), 8 * PART).unwrap());
        let fetcher =
            || Rc::new(Fetcher::new(bucket.clone(), limits(8), memory(), Some(cache.clone())));
        let mut reads = Reads::new();
        let want = Some(Ok(data[(PART + 5) as usize..][..10].to_vec()));
        let read = reads.read(&fetcher(), location(2 * PART), PART + 5, 10, Priority::Demand);
        let part = format!("get {KEY} {PART}..{}", 2 * PART);
        assert_eq!(complete_all(&bucket, &mut reads), [format!("get {KEY} last {TAIL}"), part]);
        assert_eq!(reads.result(read), want);

        let read = reads.read(&fetcher(), location(2 * PART), PART + 5, 10, Priority::Demand);
        assert_eq!(complete_all(&bucket, &mut reads), Vec::<String>::new());
        assert_eq!(reads.result(read), want);
    }

    /// TASK-165.4: a host restarted, its fetcher's memory and its cache new, reads what the cache
    /// kept before from it, asking the bucket for nothing: the object's tail, and its parts.
    #[test]
    fn reads_after_a_restart_take_what_the_cache_kept() {
        let data = noise(2 * PART);
        let bucket = bucket(&data);
        let dir = tempfile::tempdir().unwrap();
        let read = |reading: &[u64]| {
            let cache = Arc::new(Cache::open(dir.path(), 8 * PART).unwrap());
            let fetcher = Rc::new(Fetcher::new(bucket.clone(), limits(8), memory(), Some(cache)));
            let mut reads = Reads::new();
            let started: Vec<_> = (reading.iter())
                .map(|&at| (at, reads.read(&fetcher, location(2 * PART), at, 10, Priority::Demand)))
                .collect();
            let asked = complete_all(&bucket, &mut reads);
            for (at, read) in started {
                assert_eq!(reads.result(read), Some(Ok(data[at as usize..][..10].to_vec())));
            }
            asked
        };
        let parts = extent_frame::Parts::of(&whole(&data), &extent_frame::NoKeys).unwrap();
        let get = |part: usize| format!("get {KEY} {:?}", parts.stored[part].range());
        assert_eq!(read(&[5]), [format!("get {KEY} last {TAIL}"), get(0)]);
        assert_eq!(read(&[5]), Vec::<String>::new());
        assert_eq!(read(&[5, PART + 5]), [get(1)]);
        assert_eq!(read(&[5, PART + 5]), Vec::<String>::new());
    }

    /// TASK-165.4: a part a host's cache kept before it restarted whose bytes changed since, as a
    /// write a crash tore leaves them, is fetched again, once: the cache's copy fails its CRC, and
    /// goes; the one fetched is kept.
    #[test]
    fn parts_torn_before_a_restart_are_fetched_again() {
        let data = noise(PART);
        let bucket = bucket(&data);
        let dir = tempfile::tempdir().unwrap();
        let read = || {
            let cache = Arc::new(Cache::open(dir.path(), 8 * PART).unwrap());
            let fetcher = Rc::new(Fetcher::new(bucket.clone(), limits(8), memory(), Some(cache)));
            let mut reads = Reads::new();
            let read = reads.read(&fetcher, location(PART), 100, 10, Priority::Demand);
            let asked = complete_all(&bucket, &mut reads);
            assert_eq!(reads.result(read), Some(Ok(data[100..110].to_vec())));
            asked
        };
        let part = format!("get {KEY} 0..{PART}");
        assert_eq!(read(), [format!("get {KEY} last {TAIL}"), part.clone()]);
        let path = dir.path().join("cladefs-parts").join(KEY);
        let file = std::fs::File::options().write(true).open(path).unwrap();
        std::os::unix::fs::FileExt::write_all_at(&file, &[0; 4096], 8192).unwrap();
        assert_eq!(read(), [part]);
        assert_eq!(read(), Vec::<String>::new());
    }

    /// TASK-92.22: reads of more objects, one after another, than a fetcher once kept the tails
    /// of, 4,096, then of them all again, find each object's tail, and part, kept: the second time
    /// they ask the bucket for nothing.
    #[test]
    fn reads_of_many_objects_find_their_tails_kept() {
        const OBJECTS: u64 = 4097;
        let data = noise(100);
        let bucket = SimBucket::new();
        let key = |object: u64| layout::segment(test_cell(1), object as u64, 1);
        for object in 0..OBJECTS {
            block_on(Direct(bucket.clone()).put(
                &key(object),
                whole(&data).into(),
                PutMode::Create,
            ))
            .unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(dir.path(), 64 * PART).unwrap());
        let fetcher =
            Rc::new(Fetcher::new(bucket.clone(), limits(8), memory(), Some(cache.clone())));
        let mut reads = Reads::new();
        let mut read_all = || -> usize {
            let mut asked = 0;
            for object in 0..OBJECTS {
                let location = Location { key: key(object), offset: 0, len: 100 };
                let read = reads.read(&fetcher, location, 0, 10, Priority::Demand);
                asked += complete_all(&bucket, &mut reads).len();
                assert_eq!(reads.result(read), Some(Ok(data[..10].to_vec())));
            }
            asked
        };
        assert_eq!(read_all(), 2 * OBJECTS as usize);
        assert_eq!(read_all(), 0);
    }

    /// The bytes of parts, and of tails, `fetchers` hold in memory, each memory they keep them in
    /// counted once.
    fn held(fetchers: &[Rc<Fetcher<SimBucket>>]) -> (u64, u64) {
        let mut counted = std::collections::HashSet::new();
        let memories = fetchers.iter().map(|fetcher| &fetcher.shared.memory);
        let memories = memories.filter(|memory| counted.insert(Arc::as_ptr(memory)));
        memories.fold((0, 0), |(parts, tails), memory| {
            let held = (memory.recent_bytes(), memory.tails_bytes());
            (parts + held.0, tails + held.1)
        })
    }

    /// `count` fetchers of one host, as its cells' are, sharing its budget, its memory and
    /// `cache`.
    fn host(
        bucket: &Arc<SimBucket>,
        cache: &Arc<Cache>,
        count: usize,
    ) -> Vec<Rc<Fetcher<SimBucket>>> {
        let (budget, memory) = (limits(8), memory());
        let fetcher =
            || Fetcher::new(bucket.clone(), budget.clone(), memory.clone(), Some(cache.clone()));
        (0..count).map(|_| Rc::new(fetcher())).collect()
    }

    /// TASK-92.43: a host's fetchers reading one part at once, as the forks of a base booting
    /// together read it, send one GET of it, as they send one of its object's tail.
    #[test]
    fn fetchers_of_a_host_reading_a_part_at_once_send_one_get_of_it() {
        let data = noise(PART);
        let bucket = bucket(&data);
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(dir.path(), 8 * PART).unwrap());
        let fetchers = host(&bucket, &cache, 4);
        let mut reads = Reads::new();
        let all: Vec<_> = (fetchers.iter())
            .map(|fetcher| reads.read(fetcher, location(PART), 0, 10, Priority::Demand))
            .collect();
        let mut asked = complete_all(&bucket, &mut reads);
        asked.sort();
        assert_eq!(asked, [format!("get {KEY} 0..{PART}"), format!("get {KEY} last {TAIL}")]);
        for read in all {
            assert_eq!(reads.result(read), Some(Ok(data[..10].to_vec())));
        }
    }

    /// TASK-92.43: a read of a part another of the host's fetchers is reading ahead, its GET
    /// waiting in that fetcher's budget, moves it up to demand there, where it waits: here a
    /// budget of one GET, which the read the other reads ahead of holds.
    #[test]
    fn reads_of_a_part_another_fetcher_reads_ahead_move_its_get_up_where_it_waits() {
        let data = noise(2 * PART);
        let bucket = bucket(&data);
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(dir.path(), 8 * PART).unwrap());
        let (memory, budget) = (memory(), limits(1));
        let mut reads = Reads::new();
        let ahead =
            Fetcher::new(bucket.clone(), budget.clone(), memory.clone(), Some(cache.clone()));
        let ahead = Rc::new(ahead.reading_ahead(reads.spawn()));
        let other = Rc::new(Fetcher::new(bucket.clone(), limits(8), memory, Some(cache)));
        let first = reads.read(&ahead, location(2 * PART), 0, 10, Priority::Demand);
        // The tail; then the first part's GET goes, and the second's, read ahead, waits.
        assert_eq!((bucket.waiting(), bucket.describe(0)), (1, format!("get {KEY} last {TAIL}")));
        bucket.complete(0, Fault::None);
        reads.run();
        let waiting = |priority| budget.queues.lock().unwrap().waiting[priority as usize].len();
        assert_eq!((waiting(Priority::Demand), waiting(Priority::Ahead)), (0, 1));
        let second = reads.read(&other, location(2 * PART), PART, 10, Priority::Demand);
        assert_eq!((waiting(Priority::Demand), waiting(Priority::Ahead)), (1, 0));
        let mut asked = complete_all(&bucket, &mut reads);
        asked.sort();
        let parts = [format!("get {KEY} 0..{PART}"), format!("get {KEY} {PART}..{}", 2 * PART)];
        assert_eq!(asked, parts);
        assert_eq!(reads.result(first), Some(Ok(data[..10].to_vec())));
        assert_eq!(reads.result(second), Some(Ok(data[PART as usize..][..10].to_vec())));
    }

    /// TASK-92.21: a host's fetchers, one for each of its cells, keep no more than [`RECENT`]
    /// bytes of parts in memory in all, however many of them there are: here four, each reading
    /// 24 MiB of an object of its own.
    #[test]
    fn fetchers_of_a_host_hold_no_more_parts_in_memory_than_its_bound() {
        const PARTS: u64 = 24;
        let data = noise(PARTS * PART);
        let bucket = SimBucket::new();
        let key = |object: u64| layout::segment(test_cell(1), object as u64, 1);
        for object in 0..4 {
            block_on(Direct(bucket.clone()).put(
                &key(object),
                whole(&data).into(),
                PutMode::Create,
            ))
            .unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(dir.path(), 8 * PART).unwrap());
        let fetchers = host(&bucket, &cache, 4);
        let mut reads = Reads::new();
        for (object, fetcher) in (0..).zip(&fetchers) {
            let location = Location { key: key(object), offset: 0, len: PARTS * PART };
            for part in 0..PARTS {
                let read = reads.read(fetcher, location.clone(), part * PART, 10, Priority::Demand);
                complete_all(&bucket, &mut reads);
                let at = (part * PART) as usize;
                assert_eq!(reads.result(read), Some(Ok(data[at..at + 10].to_vec())));
            }
        }
        let (parts, _) = held(&fetchers);
        assert!(parts <= RECENT, "{} MiB of parts held, past {} MiB", parts >> 20, RECENT >> 20);
    }

    /// TASK-92.22: a host's fetchers keep no more than [`TAILS`] bytes of tails in memory in all,
    /// however many of them there are: here two, each reading [`TAILS`] bytes of them, as reads in
    /// flight, which count the bytes they read at first.
    #[test]
    fn fetchers_of_a_host_hold_no_more_tails_in_memory_than_its_bound() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(dir.path(), 8 * PART).unwrap());
        let fetchers = host(&SimBucket::new(), &cache, 2);
        for (fetcher, cell) in fetchers.iter().zip(2..) {
            for number in 0..TAILS / TAIL {
                drop(fetcher.shared.tail(&layout::segment(test_cell(cell), number as u64, 1)));
            }
        }
        let (_, tails) = held(&fetchers);
        assert!(tails <= TAILS, "{} MiB of tails held, past {} MiB", tails >> 20, TAILS >> 20);
    }

    /// TASK-92.21, TASK-92.22: a fetcher of another of the host's cells, as a fork's cell's of
    /// its base's, reads what one read last from the host's memory: the parts, and their object's
    /// tail, the cache's copies of them removed here, so that reading them anew would show as GETs.
    #[test]
    fn fetchers_of_a_host_share_what_they_hold_in_memory() {
        let data = pattern(2 * PART);
        let bucket = bucket(&data);
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(dir.path(), 8 * PART).unwrap());
        let fetchers = host(&bucket, &cache, 2);
        let (base, fork) = (&fetchers[0], &fetchers[1]);
        let mut reads = Reads::new();
        let want = Some(Ok(data[(PART - 5) as usize..][..10].to_vec()));
        let read = reads.read(base, location(2 * PART), PART - 5, 10, Priority::Demand);
        assert_eq!(complete_all(&bucket, &mut reads).len(), 3);
        assert_eq!(reads.result(read), want);
        std::fs::remove_file(cache.path(KEY)).unwrap();

        let read = reads.read(fork, location(2 * PART), PART - 5, 10, Priority::Demand);
        assert_eq!(complete_all(&bucket, &mut reads), Vec::<String>::new());
        assert_eq!(reads.result(read), want);
        // One copy of the parts, unpacked only (board 6026: stored bytes released on unpack).
        let stored = extent_frame::Parts::of(&whole(&data), &extent_frame::NoKeys).unwrap().stored;
        assert_eq!(held(&fetchers).0, stored.len() as u64 * PART);
    }

    /// Reads within the part the last read ended in go on from it, and read further ahead; a read
    /// elsewhere, after it or before, reads only the part after it ahead.
    #[test]
    fn read_ahead_follows_reads_within_a_part_and_restarts_elsewhere() {
        let data = noise(12 * PART);
        let bucket = bucket(&data);
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(dir.path(), 64 * PART).unwrap());
        let mut reads = Reads::new();
        let fetcher = Fetcher::new(bucket.clone(), limits(8), memory(), Some(cache));
        let fetcher = Rc::new(fetcher.reading_ahead(reads.spawn()));
        let location = location(12 * PART);
        let get = |part: u64| format!("get {KEY} {}..{}", part * PART, (part + 1) * PART);
        let mut read_at = |at: u64| {
            let read = reads.read(&fetcher, location.clone(), at, 10, Priority::Demand);
            let mut requests = complete_all(&bucket, &mut reads);
            requests.sort();
            assert_eq!(reads.result(read), Some(Ok(data[at as usize..][..10].to_vec())));
            requests
        };
        assert_eq!(read_at(0), [get(0), get(1), format!("get {KEY} last {TAIL}")]);
        assert_eq!(read_at(100), [get(2)]);
        assert_eq!(read_at(8 * PART), [get(8), get(9)]);
        assert_eq!(read_at(5 * PART), [get(5), get(6)]);
    }

    /// TASK-92.21: a read of the cache, which blocks on its files, holds up no other task of the
    /// runtime the host's fetches run on, here one of a single thread: the file the cache keeps
    /// the read's part and its object's tail in is a pipe, which the read waits to open, as for a
    /// disk slow to answer, until the runtime's other task opens it to write. What the read finds
    /// in it then is no part, and it reads the bucket.
    #[test]
    fn reads_of_the_cache_hold_up_no_other_task_of_their_runtime() {
        use std::os::unix::fs::OpenOptionsExt as _;
        use std::sync::atomic::{AtomicBool, Ordering};
        let data = noise(PART);
        let bucket = bucket(&data);
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(dir.path(), 8 * PART).unwrap());
        let mut reads = Reads::new();
        let keeping = Fetcher::new(bucket.clone(), limits(8), memory(), Some(cache.clone()));
        reads.read(&Rc::new(keeping), location(PART), 0, 10, Priority::Demand);
        complete_all(&bucket, &mut reads);
        let pipe = cache.path(KEY);
        std::fs::remove_file(&pipe).unwrap();
        let path = std::ffi::CString::new(pipe.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a mkfifo of a C string.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        // Opening the pipe to write, which lets a read waiting to open it go on: none waiting,
        // it fails.
        let open = |pipe: &std::path::Path| {
            drop(std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(pipe))
        };

        let runtime =
            tokio::runtime::Builder::new_multi_thread().worker_threads(1).build().unwrap();
        let fetcher = Fetcher::new(Arc::new(Direct(bucket)), limits(8), memory(), Some(cache));
        let done = Arc::new(AtomicBool::new(false));
        let (finished, read) = std::sync::mpsc::channel();
        let reading = done.clone();
        runtime.spawn(async move {
            let read = fetcher.read(EXTENT, &location(PART), 0, 10, Priority::Demand).await;
            reading.store(true, Ordering::Relaxed);
            finished.send(read).unwrap();
        });
        let (opening, at) = (done.clone(), pipe.clone());
        runtime.spawn(async move {
            while !opening.load(Ordering::Relaxed) {
                open(&at);
                tokio::task::yield_now().await;
            }
        });
        // Had the read held the runtime's one thread, the other task would not run, nor the read
        // end: once its wait is past, whatever came, the read is let go.
        let read = read.recv_timeout(std::time::Duration::from_secs(30));
        while !done.load(Ordering::Relaxed) {
            open(&pipe);
            std::thread::yield_now();
        }
        assert_eq!(read, Ok(Ok(data[..10].to_vec())));
    }

    /// TASK-92.21: a benchmark of reads of parts the cache keeps, on a runtime of four threads, as
    /// the bucket runtime may be, each a task of its own, all at once: how long 1,024 reads of a
    /// part take, each by a fetcher whose memory holds nothing, so that it reads the part, and its
    /// object's tail, from the cache. `cargo nextest run -p bucket_cell --run-ignored only
    /// bench_reads_of_the_cache_on_a_runtime --no-capture`.
    #[test]
    #[ignore = "a benchmark (TASK-92.21): --run-ignored only, alone"]
    fn bench_reads_of_the_cache_on_a_runtime() {
        const PARTS: u64 = 64;
        let data = noise(PARTS * PART);
        let bucket = bucket(&data);
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(dir.path(), 2 * PARTS * PART).unwrap());
        let keeping = Fetcher::new(bucket.clone(), limits(64), memory(), Some(cache.clone()));
        let (keeping, mut reads) = (Rc::new(keeping), Reads::new());
        for part in 0..PARTS {
            reads.read(&keeping, location(PARTS * PART), part * PART, 1, Priority::Demand);
            complete_all(&bucket, &mut reads);
        }
        let runtime =
            tokio::runtime::Builder::new_multi_thread().worker_threads(4).build().unwrap();
        let bucket = Arc::new(Direct(bucket));
        let started = std::time::Instant::now();
        let reads: Vec<_> = (0..16 * PARTS)
            .map(|read| {
                let fetcher =
                    Fetcher::new(bucket.clone(), limits(64), memory(), Some(cache.clone()));
                let at = read % PARTS * PART;
                let read = async move {
                    fetcher.read(EXTENT, &location(PARTS * PART), at, PART, Priority::Demand).await
                };
                (at, runtime.spawn(read))
            })
            .collect();
        for (at, read) in reads {
            let read = runtime.block_on(read).unwrap();
            assert!(read == Ok(data[at as usize..(at + PART) as usize].to_vec()), "at {at}");
        }
        println!("{} reads of a part kept: {:?}", 16 * PARTS, started.elapsed());
    }

    /// TASK-92.43's review: a benchmark of reads of parts the host's memory holds, as the page-ins
    /// of a base's forks are of what they share, by eight fetchers of a host at once, each a task
    /// of a runtime of eight threads, as the bucket runtime may be: how long 32 Ki reads of 4 KiB
    /// each take each fetcher, none asking the bucket for anything, so that what the locks the
    /// host's fetchers share cost their reads shows. `cargo nextest run -p bucket_cell --release
    /// --run-ignored only bench_reads_of_parts_in_memory_by_a_hosts_fetchers --no-capture`.
    #[test]
    #[ignore = "a benchmark (TASK-92.21): --run-ignored only, in release"]
    fn bench_reads_of_parts_in_memory_by_a_hosts_fetchers() {
        const PARTS: u64 = 16;
        const FETCHERS: u64 = 8;
        const READS: u64 = 32 << 10;
        const LEN: u64 = 4 << 10;
        let data = Arc::new(noise(PARTS * PART));
        let bucket = bucket(&data);
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(dir.path(), 2 * PARTS * PART).unwrap());
        let (budget, memory) = (limits(64), memory());
        let warming =
            Fetcher::new(bucket.clone(), budget.clone(), memory.clone(), Some(cache.clone()));
        let (warming, mut reads) = (Rc::new(warming), Reads::new());
        for part in 0..PARTS {
            reads.read(&warming, location(PARTS * PART), part * PART, 1, Priority::Demand);
            complete_all(&bucket, &mut reads);
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(FETCHERS as usize)
            .build()
            .unwrap();
        let bucket = Arc::new(Direct(bucket));
        let started = std::time::Instant::now();
        let reading: Vec<_> = (0..FETCHERS)
            .map(|index| {
                let fetcher = Fetcher::new(
                    bucket.clone(),
                    budget.clone(),
                    memory.clone(),
                    Some(cache.clone()),
                );
                let data = data.clone();
                runtime.spawn(async move {
                    for read in 0..READS {
                        let at = (read * 7 + index * 13) % (PARTS * PART / LEN) * LEN;
                        let location = location(PARTS * PART);
                        let read = fetcher.read(EXTENT, &location, at, LEN, Priority::Demand).await;
                        assert!(read.unwrap() == data[at as usize..][..LEN as usize], "at {at}");
                    }
                })
            })
            .collect();
        for read in reading {
            runtime.block_on(read).unwrap();
        }
        let took = started.elapsed();
        let each = took / (FETCHERS * READS) as u32;
        println!(
            "{FETCHERS} fetchers, {READS} reads of {LEN} bytes each: {took:?}, {each:?} a read"
        );
    }

    /// Read-ahead follows the reads of up to [`STREAMS`] objects; past them, it forgets them all.
    #[test]
    fn read_ahead_follows_so_many_objects() {
        let reads = Reads::new();
        let bucket = SimBucket::new();
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(dir.path(), PART).unwrap());
        let fetcher =
            Fetcher::new(bucket, limits(8), memory(), Some(cache)).reading_ahead(reads.spawn());
        let streams = || fetcher.streams.lock().unwrap().len();
        let key = |number: usize| layout::segment(test_cell(1), number as u64, 1);
        for number in 0..STREAMS {
            fetcher.read_ahead(&key(number), 0, 0);
        }
        assert_eq!(streams(), STREAMS);
        fetcher.read_ahead(&key(0), 1, 1);
        assert_eq!(streams(), STREAMS);
        fetcher.read_ahead(&key(STREAMS), 0, 0);
        assert_eq!(streams(), 1);
    }

    /// cladefs: a host that owns every part, another's fetcher, which serves them as a peer is
    /// served ([`Fetcher::stored_part`]); or one that gives wrong bytes, of the right length.
    struct Owner {
        fetcher: Option<Arc<Fetcher<SimBucket>>>,
        asked: std::sync::atomic::AtomicUsize,
        failed: std::sync::atomic::AtomicUsize,
    }

    impl Owner {
        fn of(fetcher: Option<Arc<Fetcher<SimBucket>>>) -> Arc<Self> {
            Arc::new(Self { fetcher, asked: Default::default(), failed: Default::default() })
        }

        fn counts(&self) -> (usize, usize) {
            use std::sync::atomic::Ordering::Relaxed;
            (self.asked.load(Relaxed), self.failed.load(Relaxed))
        }
    }

    impl Peers for Arc<Owner> {
        fn owner(&self, _: &str, _: u64) -> Option<Arc<dyn Peer>> {
            Some(self.clone())
        }
    }

    impl Peer for Owner {
        fn part(&self, key: &str, part: u64) -> BoxFuture<'static, Option<Vec<u8>>> {
            self.asked.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let Some(fetcher) = self.fetcher.clone() else {
                return futures::future::ready(Some(vec![0xa5; PART as usize])).boxed();
            };
            let key = key.to_owned();
            // Where it went for the part its own to note, as another host's are (TASK-672.2).
            let serving = trace::sourced(async move { fetcher.stored_part(&key, part).await });
            async move { Some(serving.await.0.unwrap()?.to_vec()) }.boxed()
        }

        fn gave(&self, _: u64) {}

        fn failed(&self) {
            self.failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// cladefs: a host that asks no peer, as one serving them asks none.
    struct Alone;

    impl Peers for Alone {
        fn owner(&self, key: &str, part: u64) -> Option<Arc<dyn Peer>> {
            panic!("part {part} of {key}, asked of a peer by a fetcher serving it");
        }
    }

    /// TASK-406.2: parts another host owns are fetched from it, which fetches each from the bucket
    /// once, keeps it, and asks no peer for it; a third host's read of them fetches nothing of them
    /// from the bucket. Only tails are each host's own GETs.
    #[test]
    fn parts_another_host_owns_come_from_it() {
        let data = noise(2 * PART);
        let bucket = bucket(&data);
        let dirs = [(); 3].map(|()| tempfile::tempdir().unwrap());
        let cache = |at: usize| Some(Arc::new(Cache::open(dirs[at].path(), 8 * PART).unwrap()));
        let owning = Arc::new(
            Fetcher::new(bucket.clone(), limits(8), memory(), cache(0)).asking(Arc::new(Alone)),
        );
        let owner = Owner::of(Some(owning));
        let asker = |at: usize| {
            let fetcher = Fetcher::new(bucket.clone(), limits(8), memory(), cache(at));
            Rc::new(fetcher.asking(Arc::new(owner.clone())))
        };
        let mut reads = Reads::new();
        let (tail, parts) = (
            format!("get {KEY} last {TAIL}"),
            [0, 1].map(|part| format!("get {KEY} {}..{}", part * PART, (part + 1) * PART)),
        );

        let read = reads.read(&asker(1), location(2 * PART), 0, 2 * PART, Priority::Demand);
        let mut completed = complete_all(&bucket, &mut reads);
        completed.sort();
        let mut want = vec![parts[0].clone(), parts[1].clone(), tail.clone(), tail.clone()];
        want.sort();
        assert_eq!(completed, want, "the asker's tail, the owner's, and each part once");
        assert_eq!(reads.result(read), Some(Ok(data.clone())));
        assert_eq!(owner.counts(), (2, 0));

        let read = reads.read(&asker(2), location(2 * PART), 0, 2 * PART, Priority::Demand);
        assert_eq!(complete_all(&bucket, &mut reads), [tail], "the third host's tail alone");
        assert_eq!(reads.result(read), Some(Ok(data)));
        assert_eq!(owner.counts(), (4, 0));
    }

    /// TASK-672.2: a read of a part another host owns says it went to that peer, once its object's
    /// tail, the host's own GET, is in memory: the first read went to the bucket for the tail.
    #[test]
    fn reads_of_parts_a_peer_gave_say_so() {
        trace::enable(true);
        let data = noise(2 * PART);
        let bucket = bucket(&data);
        let owning = Fetcher::new(bucket.clone(), limits(8), memory(), None);
        let owner = Owner::of(Some(Arc::new(owning.asking(Arc::new(Alone)))));
        let fetcher = Fetcher::new(bucket.clone(), limits(8), memory(), None);
        let fetcher = Rc::new(fetcher.asking(Arc::new(owner.clone())));
        let mut reads = Reads::new();
        let sourced = |offset: u64, reads: &mut Reads| {
            let landed = Rc::new(RefCell::new(None));
            let (fetcher, into) = (fetcher.clone(), landed.clone());
            let read = async move {
                let at = location(2 * PART);
                let read = fetcher.read(EXTENT, &at, offset, 10, Priority::Demand);
                let (read, source) = trace::sourced(read).await;
                *into.borrow_mut() = Some((read.map(|bytes| bytes.len()), source));
            };
            reads.pool.spawner().spawn_local(read).unwrap();
            reads.run();
            complete_all(&bucket, reads);
            landed.borrow_mut().take()
        };
        assert_eq!(sourced(0, &mut reads), Some((Ok(10), Some(Source::Bucket))));
        assert_eq!(sourced(PART, &mut reads), Some((Ok(10), Some(Source::Peer))));
        assert_eq!(owner.counts(), (2, 0));
    }

    /// TASK-406.2: a part its owner gives wrong, failing the CRC the asker's tail gives it, is
    /// fetched from the bucket, and its owner told it failed.
    #[test]
    fn parts_their_owner_gives_wrong_come_from_the_bucket() {
        let data = noise(2 * PART);
        let bucket = bucket(&data);
        let owner = Owner::of(None);
        let fetcher = Fetcher::new(bucket.clone(), limits(8), memory(), None);
        let fetcher = Rc::new(fetcher.asking(Arc::new(owner.clone())));
        let mut reads = Reads::new();
        let read = reads.read(&fetcher, location(2 * PART), 0, 2 * PART, Priority::Demand);
        assert_eq!(complete_all(&bucket, &mut reads).len(), 3, "the tail, and each part");
        assert_eq!(reads.result(read), Some(Ok(data)));
        assert_eq!(owner.counts(), (2, 2));
    }

    /// TASK-406.2, TASK-407: a peer asks for a part by its object's key and its number alone, and
    /// is given its stored bytes; none of a part the object holds not, past its end.
    #[test]
    fn peers_are_given_the_parts_they_ask_for() {
        let data = noise(2 * PART);
        let bucket = bucket(&data);
        let fetcher = Arc::new(Fetcher::new(bucket.clone(), limits(8), memory(), None));
        let mut pool = LocalPool::new();
        let object = stored_at(&bucket);
        let parts = extent_frame::Parts::of(&object, &extent_frame::NoKeys).unwrap();
        let stored = parts.part(1).unwrap().stored;
        let spawner = pool.spawner();
        let serving = |part| {
            let fetcher = fetcher.clone();
            spawner
                .spawn_local_with_handle(async move { fetcher.stored_part(KEY, part).await })
                .unwrap()
        };
        let served = serving(1);
        pool.run_until_stalled();
        bucket.complete(0, Fault::None);
        pool.run_until_stalled();
        bucket.complete(0, Fault::None);
        let served = pool.run_until(served).unwrap().expect("the part");
        assert_eq!(&*served, &object[stored.range().start as usize..][..stored.len as usize]);
        let past = serving(2);
        assert_eq!(pool.run_until(past), Ok(None), "the tail kept: no GET");
    }

    /// TASK-542 (board 2480): two hosts that each share one GET budget (`requests: 2`) between
    /// their local fetcher and their peer-serving fetcher, and each queue 4 demand reads of parts
    /// the other host serves (no timeout), must finish every read without deadlocking.
    #[test]
    fn peers_reading_each_others_parts_do_not_deadlock_their_budgets() {
        const PARTS: u64 = 8;
        let data = noise(PARTS * PART);
        let bucket = bucket(&data);

        let budget_a = Budget::new(Limits { requests: 2, bytes: 2 * PART });
        let budget_b = Budget::new(Limits { requests: 2, bytes: 2 * PART });
        let mem_a = memory();
        let mem_b = memory();

        let serve_a = Arc::new(Fetcher::new(bucket.clone(), budget_a.clone(), mem_a.clone(), None));
        let serve_b = Arc::new(Fetcher::new(bucket.clone(), budget_b.clone(), mem_b.clone(), None));
        let owner_a = Owner::of(Some(serve_a));
        let owner_b = Owner::of(Some(serve_b));

        // Host A asks Host B for every part; Host B asks Host A for every part.
        let local_a = Rc::new(
            Fetcher::new(bucket.clone(), budget_a, mem_a, None).asking(Arc::new(owner_b.clone())),
        );
        let local_b = Rc::new(
            Fetcher::new(bucket.clone(), budget_b, mem_b, None).asking(Arc::new(owner_a.clone())),
        );

        let mut reads = Reads::new();
        let mut handles_a = Vec::new();
        let mut handles_b = Vec::new();
        for part in 0..4u64 {
            handles_a.push((
                part,
                reads.read(&local_a, location(PARTS * PART), part * PART, PART, Priority::Demand),
            ));
            handles_b.push((
                part + 4,
                reads.read(
                    &local_b,
                    location(PARTS * PART),
                    (part + 4) * PART,
                    PART,
                    Priority::Demand,
                ),
            ));
        }

        complete_all(&bucket, &mut reads);

        for (part, handle) in handles_a.into_iter().chain(handles_b) {
            let expected = data[(part * PART) as usize..((part + 1) * PART) as usize].to_vec();
            assert_eq!(reads.result(handle), Some(Ok(expected)), "part {part} did not finish");
        }
        assert_eq!(owner_a.counts(), (4, 0));
        assert_eq!(owner_b.counts(), (4, 0));
    }

    /// TASK-542 (board 2480, AC #2): dropping a queued `stored_part` future cancels its waiter in
    /// the serving host's GET budget immediately so no orphaned bucket GET fires after the
    /// blocking slot releases.
    #[test]
    fn dropped_peer_part_request_leaves_its_budget_without_getting() {
        let data = noise(2 * PART);
        let bucket = bucket(&data);
        let budget = Budget::new(Limits { requests: 1, bytes: PART });
        let fetcher = Arc::new(Fetcher::new(bucket.clone(), budget.clone(), memory(), None));
        let mut pool = LocalPool::new();
        let spawner = pool.spawner();

        // Warm the object's tail first so `stored_part` goes straight to part GET budgeting.
        let warm = {
            let fetcher = fetcher.clone();
            spawner
                .spawn_local_with_handle(async move { fetcher.stored_part(KEY, 0).await })
                .unwrap()
        };
        pool.run_until_stalled();
        bucket.complete(0, Fault::None); // tail
        pool.run_until_stalled();
        bucket.complete(0, Fault::None); // part 0
        assert!(pool.run_until(warm).unwrap().is_some());

        // Occupy the single budget slot, queue a `stored_part(KEY, 1)`, then drop it before the
        // slot frees.
        let slot = pool.run_until(budget.hold(Priority::Demand, PART));
        let mut queued = Box::pin(fetcher.stored_part(KEY, 1));
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(queued.as_mut().poll(&mut cx).is_pending());
        drop(queued);

        // Releasing the blocking slot must not trigger any bucket GET for the cancelled part 1.
        drop(slot);
        pool.run_until_stalled();
        assert_eq!(bucket.waiting(), 0, "cancelled stored_part issued an orphaned GET");
    }

    /// board 12313: `PartMemory::size_to` grows past `RECENT` and shrinks under governor pressure,
    /// tracks `peak`, and `take_spare_buf` recycles uniquely-owned LRU `Box<[u8]>` buffers when
    /// full without retaining idle memory.
    #[test]
    fn part_memory_sizes_dynamically_tracks_peak_and_recycles_buffers() {
        let len = 4096usize;
        let parts = PartMemory::new(2 * len as u64);
        assert_eq!((parts.limit(), parts.bytes(), parts.peak()), (2 * len as u64, 0, 0));

        // Fill to limit: no spare buffer recycled while below/at limit without overflow.
        let b0 = vec![1u8; len].into_boxed_slice();
        let b0_ptr = b0.as_ptr();
        parts.put_local(1, 0, PartBytes::from_box(b0));
        assert!(parts.take_spare_buf(len).is_none());
        parts.put_local(1, 1, PartBytes::from_box(vec![2u8; len].into_boxed_slice()));
        assert_eq!((parts.bytes(), parts.peak()), (2 * len as u64, 2 * len as u64));

        // Next insert of `len` would exceed limit: `take_spare_buf` reclaims oldest (part 0) buffer
        // in place (same allocation pointer, zero new heap allocation).
        let recycled = parts.take_spare_buf(len).expect("recycled oldest buffer");
        assert_eq!(recycled.as_ptr(), b0_ptr);
        assert_eq!(parts.bytes(), len as u64);
        assert!(parts.get_local(1, 0).is_none());
        parts.put_local(1, 2, PartBytes::from_box(recycled));

        // Grow past initial limit and verify peak updates.
        parts.size_to(4 * len as u64);
        assert_eq!(parts.limit(), 4 * len as u64);
        parts.put_local(1, 3, PartBytes::from_box(vec![3u8; len].into_boxed_slice()));
        parts.put_local(1, 4, PartBytes::from_box(vec![4u8; len].into_boxed_slice()));
        assert_eq!((parts.bytes(), parts.peak()), (4 * len as u64, 4 * len as u64));

        // Shrink under pressure evicts LRU entries and lowers limit; peak stays high-water mark.
        assert_eq!(parts.size_to(len as u64), len as u64);
        assert_eq!(
            (parts.limit(), parts.bytes(), parts.peak()),
            (len as u64, len as u64, 4 * len as u64)
        );
        assert_eq!(parts.size_to(0), 0);
        assert!(parts.take_spare_buf(len).is_none());
    }

    /// board 12313: concurrent `Priority::Demand` reads of one compressed part coalesce into a
    /// single zstd decompression, while `Priority::Ahead` fetches without decompressing until a
    /// demand read arrives.
    #[test]
    fn concurrent_demand_reads_coalesce_zstd_unpack_and_ahead_stays_stored() {
        let data = pattern(2 * PART);
        let bucket = bucket(&data);
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::open(dir.path(), 8 * PART).unwrap());
        let fetchers = host(&bucket, &cache, 4);
        let mut reads = Reads::new();

        // 1. Four concurrent demand readers of part 0 coalesce into ONE unpack.
        let before = unpacks();
        let handles: Vec<_> = fetchers
            .iter()
            .enumerate()
            .map(|(i, f)| {
                reads.read(f, location(2 * PART), (i as u64) * 4096, 128, Priority::Demand)
            })
            .collect();
        complete_all(&bucket, &mut reads);
        for (i, h) in handles.into_iter().enumerate() {
            let off = i * 4096;
            assert_eq!(reads.result(h), Some(Ok(data[off..off + 128].to_vec())));
        }
        assert_eq!(
            unpacks() - before,
            1,
            "four concurrent demand readers must share one zstd unpack"
        );

        // 2. `fetch_ahead` of part 1 fetches stored bytes without decompressing; demand unpacks once.
        let before_ahead = unpacks();
        let fetched = reads.fetch_ahead(&fetchers[0], location(2 * PART), PART, PART);
        complete_all(&bucket, &mut reads);
        assert_eq!(*fetched.borrow(), Some(Ok(1)));
        assert_eq!(unpacks() - before_ahead, 0, "fetch_ahead must not decompress");

        let h = reads.read(&fetchers[1], location(2 * PART), PART + 64, 128, Priority::Demand);
        complete_all(&bucket, &mut reads);
        assert_eq!(reads.result(h), Some(Ok(data[PART as usize + 64..][..128].to_vec())));
        assert_eq!(unpacks() - before_ahead, 1, "first demand read after fetch_ahead unpacks once");

        // Subsequent demand read hits unpacked PartMemory with zero additional unpacks.
        let h2 = reads.read(&fetchers[2], location(2 * PART), PART + 1024, 128, Priority::Demand);
        complete_all(&bucket, &mut reads);
        assert_eq!(reads.result(h2), Some(Ok(data[PART as usize + 1024..][..128].to_vec())));
        assert_eq!(unpacks() - before_ahead, 1, "cached unpacked part must not re-decompress");
    }
}
