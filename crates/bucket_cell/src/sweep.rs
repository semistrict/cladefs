// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a sweep of the collector over every cell of a bucket, as `cladefs-gc` runs it
//! (TASK-395.5): a pass over each cell, so many at once, beside the sweeps of any number of other
//! processes.
//!
//! The cells come from delimiter listings a page at a time ([`crate::cell::listed`]), each page
//! listed as the passes before it end: a sweep holds no more than a page of tenants, a page of
//! cells, and the passes in flight, [`Sweep::at_once`] of them, each over a cell of its own, which
//! holds what it read of its cell alone ([`Collector`] keeps nothing from one pass to the next).
//! Its memory is bounded by how many passes it runs at once times what a pass over the largest
//! cell takes, whatever the bucket holds.
//!
//! Each pass holds its cell by the cell's lease for [`Sweep::lease`], renewing it as it goes and
//! letting it go as it ends ([`Holding`]). A pass that finds its cell held by another, of this
//! process or another, leaves it, counted, and goes on to the next cell, waiting for nothing; so
//! does one whose cell another pass went over since the sweep began. So processes sweeping one
//! bucket at once split its cells between them, each cell passed over once, by one of them; and a
//! process gone leaves its cells to the next sweep after their leases expire, which takes them,
//! fencing its passes, should they still run.
//!
//! What each pass did, and the requests of each kind it made, it says as the pass ends
//! ([`Ran`]); what they did together, with the requests listing the cells made and the slowest
//! passes, once it ends ([`Swept`]).
//!
//! cladefs: a sweep may be bounded besides ([`Limits`], board 8529): in what its passes delete
//! together, objects and bytes ([`Budget`]), a pass past it deleting nothing and the sweep starting
//! no more; in the requests it makes a second, its passes' and its listing's together, each waiting
//! its turn; and in the passes in a row the bucket refuses, after which it starts no more. And it
//! may be asked to stop ([`Stop`], board 8513): it starts no more passes, and those running end,
//! letting their cells go. A sweep that stopped early says why ([`Swept::stopped`]), as one whose
//! listing of the cells the bucket refused does, or whose bucket's own identity it could not read
//! (board 8509).

use crate::bucket::{Bucket, ByteRange, Error as BucketError, Listing, Meta, Object, Page};
use crate::bucket::{PutMode, Refused, Version};
use crate::collector::{Budget, Collector, Holding, Marking, Report};
use crate::layout::CellId;
use crate::object_bucket::Request;
use crate::{Error, Left};
use futures::{FutureExt as _, StreamExt as _};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// What a sweep's passes do with what no live cell needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Delete it ([`Collector::collect_cell`]).
    Delete,
    /// Mark it, deleting nothing ([`Collector::mark_cell`], TASK-264).
    MarkOnly,
}

/// A sweep over every cell of a bucket. See the module docs.
#[derive(Clone)]
pub struct Sweep {
    pub mode: Mode,
    /// How many passes it runs at once, each over a cell of its own: at least 1.
    pub at_once: usize,
    /// How long each pass holds its cell, in milliseconds by the backend's clock, from its taking
    /// of the cell's lease or its last renewal of it ([`Holding::lease`]): how long a process gone
    /// holds its cells.
    pub lease: u64,
    /// How many of the slowest passes it says ([`Swept::slowest`]).
    pub slowest: usize,
    /// Whether each pass is a task of its own, spawned on the Tokio runtime the sweep runs in,
    /// which it must, so that passes' work runs on the runtime's threads at once (TASK-395.15);
    /// or all of them the sweep's own task's, one at a time between their requests (tests, a
    /// runtime of one thread).
    pub spawned: bool,
}

/// cladefs: what bounds a sweep besides its passes at once (boards 8529, 8513): nothing, by
/// default.
#[derive(Clone, Debug, Default)]
pub struct Limits {
    /// The most objects its passes delete together, and the most bytes of them, as listed
    /// ([`Budget`]): once a pass would go past either, it deletes nothing, nor does any pass after
    /// it, and the sweep starts no more ([`Stopped::Capped`]).
    pub deleted_objects: Option<u64>,
    pub deleted_bytes: Option<u64>,
    /// The most requests a second its passes and its listing of the cells make together, each
    /// waiting its turn: finite, and more than 0. A listing of many pages takes the turns of its
    /// pages after the first from the requests after it.
    pub requests_per_second: Option<f64>,
    /// After so many passes in a row the bucket refused ([`Error::Refused`]), it starts no more
    /// ([`Stopped::Refusing`]): at least 1. A bucket out of reach fails no pass: each request of
    /// it waits and goes again ([`Bucket::pause`]), and the pass with it.
    pub refused_in_a_row: Option<usize>,
    /// What asks it to start no more passes ([`Stopped::Asked`]).
    pub stop: Option<Arc<Stop>>,
}

/// cladefs: what asks a process's sweeps to stop (board 8513), as SIGTERM and SIGINT have
/// `cladefs-gc`'s: a sweep asked starts no more passes, and those running end, letting their cells
/// go ([`Stopped::Asked`]).
#[derive(Debug, Default)]
pub struct Stop {
    asked: AtomicBool,
    told: tokio::sync::Notify,
}

impl Stop {
    /// Asks it, waking whoever waits.
    pub fn ask(&self) {
        self.asked.store(true, Ordering::SeqCst);
        self.told.notify_waiters();
    }

    /// Whether it was asked.
    pub fn asked(&self) -> bool {
        self.asked.load(Ordering::SeqCst)
    }

    /// Waits until it is asked.
    pub async fn wait(&self) {
        // Made before the check, it is told of an asking after it, however late it is polled.
        let told = self.told.notified();
        if self.asked() {
            return;
        }
        told.await;
    }
}

/// cladefs: why a sweep stopped before it went over every cell (boards 8509, 8529, 8513).
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum Stopped {
    /// The bucket refused its listing of the cells (board 5861).
    #[error("the bucket refuses the listing of its cells: {0}")]
    Unlisted(Refused),
    /// It could not read its bucket's own identity, and so tell its cells from copies of other
    /// buckets' (TASK-633.8): it went over none.
    #[error("the bucket's own identity, unread: {0}")]
    Unidentified(Error),
    /// So many passes in a row the bucket refused ([`Limits::refused_in_a_row`]).
    #[error("{0} passes in a row the bucket refused")]
    Refusing(usize),
    /// A pass would have deleted past its bounds ([`Limits::deleted_objects`],
    /// [`Limits::deleted_bytes`]), and deleted nothing.
    #[error("a pass would have deleted past the sweep's bounds")]
    Capped,
    /// It was asked to ([`Stop`]).
    #[error("asked to stop")]
    Asked,
}

impl Stopped {
    /// What a stop of each reason is called, as `cladefs-gc --metrics` labels it, in order.
    pub const NAMES: [&str; 5] = ["unlisted", "unidentified", "refusing", "capped", "asked"];

    /// What it is called, one of [`Stopped::NAMES`].
    pub fn name(&self) -> &'static str {
        let index = match self {
            Self::Unlisted(_) => 0,
            Self::Unidentified(_) => 1,
            Self::Refusing(_) => 2,
            Self::Capped => 3,
            Self::Asked => 4,
        };
        Self::NAMES[index]
    }
}

/// cladefs: what spaces a sweep's requests (board 8529): one each `every` at most, whichever pass
/// makes it, each waiting for its turn.
#[derive(Debug)]
struct Pacer {
    every: Duration,
    /// The next request's turn: none before the first.
    next: std::sync::Mutex<Option<tokio::time::Instant>>,
}

impl Pacer {
    /// One of `per_second` requests a second, if given.
    fn of(per_second: Option<f64>) -> Option<Arc<Self>> {
        let per_second = per_second?;
        assert!(per_second.is_finite() && per_second > 0.0, "{per_second} requests a second");
        let every = Duration::from_secs_f64(per_second.recip());
        Some(Arc::new(Self { every, next: std::sync::Mutex::new(None) }))
    }

    /// Waits for a request's turn, and takes it.
    async fn turn(&self) {
        let at = self.take(1);
        tokio::time::sleep_until(at).await;
    }

    /// Takes the turns of `requests` requests, waiting for none: the first's turn.
    fn take(&self, requests: u32) -> tokio::time::Instant {
        let mut next = self.next.lock().unwrap();
        let now = tokio::time::Instant::now();
        let at = next.map_or(now, |next| next.max(now));
        *next = Some(at + self.every.saturating_mul(requests));
        at
    }
}

/// cladefs: what a sweep goes by of its [`Limits`], its own: what its passes may still delete, and
/// the turns of its requests.
struct Bounds<'a> {
    limits: &'a Limits,
    budget: Option<Arc<Budget>>,
    pacer: Option<Arc<Pacer>>,
}

impl<'a> Bounds<'a> {
    fn of(limits: &'a Limits) -> Self {
        let bounded = limits.deleted_objects.is_some() || limits.deleted_bytes.is_some();
        let budget =
            bounded.then(|| Arc::new(Budget::new(limits.deleted_objects, limits.deleted_bytes)));
        Self { limits, budget, pacer: Pacer::of(limits.requests_per_second) }
    }

    /// Why the sweep is to start no more passes, `refused` passes in a row the bucket refused, if
    /// it is.
    fn halted(&self, refused: usize) -> Option<Stopped> {
        if self.limits.stop.as_ref().is_some_and(|stop| stop.asked()) {
            return Some(Stopped::Asked);
        }
        if self.capped() > 0 {
            return Some(Stopped::Capped);
        }
        let most = self.limits.refused_in_a_row.filter(|&most| refused >= most)?;
        Some(Stopped::Refusing(most))
    }

    /// The passes its budget refused.
    fn capped(&self) -> usize {
        self.budget.as_ref().map_or(0, |budget| budget.refused())
    }
}

/// The requests made, of each kind, as a [`crate::object_bucket::ObjectBucket`] counts them: a
/// batch of deletes one request, a page of a listing one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Requests(pub [u64; Request::ALL.len()]);

impl Requests {
    /// Those of `request`'s kind.
    pub fn of(&self, request: Request) -> u64 {
        self.0[request as usize]
    }

    /// Those of every kind.
    pub fn total(&self) -> u64 {
        self.0.iter().sum()
    }
}

impl std::ops::AddAssign for Requests {
    fn add_assign(&mut self, other: Self) {
        for (count, more) in self.0.iter_mut().zip(other.0) {
            *count += more;
        }
    }
}

/// What a pass came to.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// It went through, deleting: what it did.
    Collected(Report),
    /// It went through, marking.
    Marked(Marking),
    /// It left its cell to another pass, writing nothing.
    Left(Left),
    /// It failed: nothing it could not read it deleted, and the next sweep tries again.
    Failed(Error),
}

/// A pass over a cell, as a sweep ran it, once it ended.
#[derive(Clone, Debug)]
pub struct Ran {
    pub cell: CellId,
    pub outcome: Outcome,
    /// The requests it made.
    pub requests: Requests,
    /// How long it took.
    pub took: Duration,
}

/// What a sweep did: its passes', together.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Swept {
    /// The cells it ran a pass over, those that failed among them.
    pub visited: usize,
    /// cladefs: those of them its passes went over in full, neither skipping them, unchanged since
    /// their last passes, nor failing (board 8509): counted as they end, never derived.
    pub passed: usize,
    /// The cells it left to passes holding them.
    pub held: usize,
    /// The cells it left to passes that went over them since it began.
    pub swept: usize,
    /// The passes that failed.
    pub failed: usize,
    /// cladefs: the passes that left what they would delete to a later sweep, past its bounds
    /// ([`Budget`], board 8529).
    pub capped: usize,
    /// cladefs: why it stopped before it went over every cell, if it did (boards 8509, 8529,
    /// 8513).
    pub stopped: Option<Stopped>,
    /// What the passes that went through did: mark-only passes deleted nothing.
    pub report: Report,
    /// What mark-only passes marked, and its bytes, as listed; and what they found resurrected,
    /// each a bug, warned of ([`Marking::resurrected`]).
    pub marked: usize,
    pub marked_bytes: u64,
    pub resurrected: usize,
    /// The requests its passes made, and those listing the cells.
    pub requests: Requests,
    /// Its slowest passes, of those it ran, slowest first: [`Sweep::slowest`] of them at most.
    pub slowest: Vec<(CellId, Duration)>,
    /// How long it took.
    pub took: Duration,
}

impl Swept {
    /// cladefs: whether it did all it was to: went over every cell, no pass failing, none left past
    /// its bounds (board 8509).
    pub fn complete(&self) -> bool {
        self.failed == 0 && self.capped == 0 && self.stopped.is_none()
    }

    /// Counts `passed` in, keeping the `slowest` slowest passes.
    fn add(&mut self, passed: Ran, slowest: usize) {
        self.requests += passed.requests;
        let ran = match passed.outcome {
            Outcome::Collected(report) => {
                self.passed += usize::from(report.skipped == 0);
                self.report += report;
                true
            }
            Outcome::Marked(marking) => {
                self.passed += 1;
                self.report += marking.report;
                if let Some(marks) = &marking.marks {
                    self.marked += marks.marked.len();
                    self.marked_bytes += marks.marked.iter().map(|marked| marked.size).sum::<u64>();
                }
                self.resurrected += marking.resurrected.len();
                true
            }
            Outcome::Left(Left::Held) => {
                self.held += 1;
                false
            }
            Outcome::Left(Left::Swept) => {
                self.swept += 1;
                false
            }
            Outcome::Failed(_) => {
                self.failed += 1;
                true
            }
        };
        if !ran {
            return;
        }
        self.visited += 1;
        let at = self.slowest.partition_point(|&(_, took)| took >= passed.took);
        if at < slowest {
            self.slowest.insert(at, (passed.cell, passed.took));
            self.slowest.truncate(slowest);
        }
    }
}

impl Sweep {
    /// Sweeps every cell of `bucket`, [`Sweep::at_once`] passes at once, each as a holder of its
    /// own, telling `observe` of each pass as it ends: what they did together, with the requests
    /// listing the cells made.
    pub async fn run<B: Bucket>(&self, bucket: &Arc<B>, observe: impl FnMut(&Ran)) -> Swept {
        self.run_with_own(bucket, None::<&Arc<B>>, observe).await
    }

    /// cladefs: the sweep [`Sweep::run_with_own`] runs, or [`Sweep::run_cell`] runs of `named`,
    /// bounded as `limits` say (boards 8529, 8513).
    pub async fn run_limited<B: Bucket, O: Bucket>(
        &self,
        bucket: &Arc<B>,
        own: Option<&Arc<O>>,
        named: Option<CellId>,
        limits: &Limits,
        observe: impl FnMut(&Ran),
    ) -> Swept {
        let bounds = Bounds::of(limits);
        if let Some(cell) = named {
            let cells = futures::stream::iter([Ok(cell)]);
            return self.over(bucket, None::<(Arc<B>, u16)>, cells, &bounds, observe).await;
        }
        let copies = match own {
            None => None,
            Some(own) => match crate::identity::get(&**own).await {
                Ok(Some(identity)) => Some((own.clone(), identity.number)),
                Ok(None) => None,
                // cladefs: a sweep that did nothing, failed, saying why; never one of no cells
                // (board 8509).
                Err(error) => {
                    log::error!("cladefs-gc: the bucket's own identity, unread: {error}");
                    return Swept {
                        stopped: Some(Stopped::Unidentified(error)),
                        ..Swept::default()
                    };
                }
            },
        };
        let listing = Counted::new(bucket.clone(), bounds.pacer.clone());
        let cells = crate::cell::listed(&listing);
        let mut swept = self.over(bucket, copies, cells, &bounds, observe).await;
        swept.requests += listing.requests();
        swept
    }

    /// cladefs: as [`Sweep::run`], `bucket` routed, `own` its own bucket, unrouted, if given
    /// (TASK-633.8): a cell listed here of another bucket, by its ID, is a copy of that cell's
    /// mirrored versions, which a pass over the copy decides ([`Collector::collect_copy`]), not a
    /// pass over the cell, which is its own bucket's. None: every cell listed is passed over as
    /// this bucket's (a bucket that routes nothing, whose cells are all its own).
    pub async fn run_with_own<B: Bucket, O: Bucket>(
        &self,
        bucket: &Arc<B>,
        own: Option<&Arc<O>>,
        observe: impl FnMut(&Ran),
    ) -> Swept {
        self.run_limited(bucket, own, None, &Limits::default(), observe).await
    }

    /// A pass over `cell` of `bucket`, as a sweep of that cell alone runs it: what it did.
    pub async fn run_cell<B: Bucket>(
        &self,
        bucket: &Arc<B>,
        cell: CellId,
        observe: impl FnMut(&Ran),
    ) -> Swept {
        self.run_limited(bucket, None::<&Arc<B>>, Some(cell), &Limits::default(), observe).await
    }

    /// Passes over `cells` of `bucket`, [`Sweep::at_once`] at once, telling `observe` of each,
    /// bounded as `bounds` say.
    async fn over<B: Bucket, O: Bucket>(
        &self,
        bucket: &Arc<B>,
        copies: Option<(Arc<O>, u16)>,
        cells: impl futures::Stream<Item = Result<CellId, Refused>>,
        bounds: &Bounds<'_>,
        mut observe: impl FnMut(&Ran),
    ) -> Swept {
        assert!(self.at_once >= 1, "a sweep of no passes at once");
        let started = Instant::now();
        // What another pass did after this, it leaves be: by the backend's clock, as every pass
        // ends by it (board 8471).
        let since = bucket.now();
        let mut swept = Swept::default();
        // cladefs: the listing refused, the sweep ends there, failed, saying why (board 5861).
        let refused = std::sync::Mutex::new(None);
        // cladefs: halted, as its bounds say, it starts no more passes, saying why (boards 8529,
        // 8513); and the passes in a row the bucket refused, which may halt it.
        let halted = std::sync::Mutex::new(None);
        let refused_in_a_row = AtomicUsize::new(0);
        let cells = cells
            .take_while(|cell| {
                if let Err(error) = cell {
                    *refused.lock().unwrap() = Some(error.clone());
                }
                std::future::ready(cell.is_ok())
            })
            // Refused, kept above: the listing's last item.
            .filter_map(|cell| std::future::ready(cell.ok()))
            // Asked as each pass would start, after the outcomes of those before it are counted.
            .take_while(|_| {
                let halt = bounds.halted(refused_in_a_row.load(Ordering::Relaxed));
                let go = halt.is_none();
                if let Some(halt) = halt {
                    halted.lock().unwrap().get_or_insert(halt);
                }
                std::future::ready(go)
            });
        // The cells taken only as `buffer_unordered` has room for another pass: a page of them
        // listed as the passes before it end.
        let passes = cells.map(|cell| {
            let (sweep, bucket) = (self.clone(), bucket.clone());
            let copies = copies.clone();
            let (budget, pacer) = (bounds.budget.clone(), bounds.pacer.clone());
            let pass = async move {
                let bounded = (budget, pacer);
                sweep.pass(&bucket, copies, cell, since, bounded).await
            };
            match self.spawned {
                false => pass.boxed(),
                true => {
                    let task = tokio::spawn(pass);
                    async move {
                        match task.await {
                            Ok(ran) => ran,
                            #[cfg(not(madsim))]
                            Err(failed) => std::panic::resume_unwind(failed.into_panic()),
                            #[cfg(madsim)]
                            Err(failed) => panic!("a pass: {failed}"),
                        }
                    }
                    .boxed()
                }
            }
        });
        let mut passes = std::pin::pin!(passes.buffer_unordered(self.at_once));
        while let Some(passed) = passes.next().await {
            match passed.outcome {
                Outcome::Failed(Error::Refused(_)) => {
                    refused_in_a_row.fetch_add(1, Ordering::Relaxed)
                }
                _ => refused_in_a_row.swap(0, Ordering::Relaxed),
            };
            observe(&passed);
            swept.add(passed, self.slowest);
        }
        let refused = refused.lock().unwrap().take();
        if let Some(refused) = &refused {
            log::error!("cladefs-gc: listing the cells: the bucket refuses {refused}");
        }
        // cladefs: its own outcome, never a pass's: no count of passes goes past those it ran
        // (board 8509). A pass its budget refused stops it, however late.
        swept.capped = bounds.capped();
        let capped = (swept.capped > 0).then_some(Stopped::Capped);
        let halted = halted.lock().unwrap().take();
        swept.stopped = halted.or(refused.map(Stopped::Unlisted)).or(capped);
        swept.took = started.elapsed();
        swept
    }

    /// A pass over `cell`, of a sweep that began at `since`, as a holder of its own, deleting no
    /// more than its `budget` gives, and making its requests in their turns, as its `pacer` gives
    /// them, if given (board 8529).
    async fn pass<B: Bucket, O: Bucket>(
        &self,
        bucket: &Arc<B>,
        copies: Option<(Arc<O>, u16)>,
        cell: CellId,
        since: u64,
        (budget, pacer): (Option<Arc<Budget>>, Option<Arc<Pacer>>),
    ) -> Ran {
        let counted = Counted::new(bucket.clone(), pacer.clone());
        // cladefs: a cell of another bucket here is a copy of its versions (TASK-633.8).
        if let Some((own, number)) = copies
            && crate::layout::routed_to(&cell.prefix()).is_some_and(|of| of != number)
        {
            let started = Instant::now();
            let collector = Collector::new().budget(budget);
            // Its requests of its own bucket, counted and paced as those of this one.
            let own = Counted::new(own, pacer);
            let outcome = match self.mode {
                Mode::Delete => match collector.collect_copy(&counted, &own, number, cell).await {
                    Ok(report) => Outcome::Collected(report),
                    Err(error) => Outcome::failed(error),
                },
                // A mark-only pass marks nothing of a copy.
                Mode::MarkOnly => Outcome::Collected(Report::default()),
            };
            let mut requests = counted.requests();
            requests += own.requests();
            return Ran { cell, outcome, requests, took: started.elapsed() };
        }
        let holding = Holding { lease: self.lease, since: Some(since) };
        let collector = Collector::new().holding(holding).budget(budget);
        let holder = rand::random();
        let started = Instant::now();
        let outcome = match self.mode {
            Mode::Delete => match collector.collect_cell(&counted, cell, holder).await {
                Ok(report) => Outcome::Collected(report),
                Err(error) => Outcome::failed(error),
            },
            Mode::MarkOnly => match collector.mark_cell(&counted, cell, holder).await {
                Ok(marking) => Outcome::Marked(marking),
                Err(error) => Outcome::failed(error),
            },
        };
        Ran { cell, outcome, requests: counted.requests(), took: started.elapsed() }
    }
}

impl Outcome {
    /// What a pass that gave `error` came to.
    fn failed(error: Error) -> Self {
        match error {
            Error::Left(left) => Self::Left(left),
            error => Self::Failed(error),
        }
    }
}

/// A bucket that counts the requests made of it, by kind, as a
/// [`crate::object_bucket::ObjectBucket`] counts its own: each pass's, apart from the others' it
/// runs beside. cladefs: each in its turn, as its sweep's pacer gives them, if it has one (board
/// 8529).
struct Counted<B> {
    bucket: Arc<B>,
    counts: [AtomicU64; Request::ALL.len()],
    pacer: Option<Arc<Pacer>>,
}

impl<B: Bucket> Counted<B> {
    fn new(bucket: Arc<B>, pacer: Option<Arc<Pacer>>) -> Self {
        Self { bucket, counts: Default::default(), pacer }
    }

    fn count(&self, request: Request) {
        self.counts[request as usize].fetch_add(1, Ordering::Relaxed);
    }

    fn requests(&self) -> Requests {
        Requests(self.counts.each_ref().map(|count| count.load(Ordering::Relaxed)))
    }

    /// Waits for a request's turn, if paced.
    async fn turn(&self) {
        if let Some(pacer) = &self.pacer {
            pacer.turn().await;
        }
    }
}

impl<B: Bucket> Bucket for Counted<B> {
    fn get(&self, key: &str) -> impl Future<Output = Result<Object, BucketError>> + Send {
        self.count(Request::Get);
        async move {
            self.turn().await;
            self.bucket.get(key).await
        }
    }

    fn get_range(
        &self,
        key: &str,
        range: ByteRange,
    ) -> impl Future<Output = Result<Vec<u8>, BucketError>> + Send {
        self.count(Request::GetRange);
        async move {
            self.turn().await;
            self.bucket.get_range(key, range).await
        }
    }

    fn head(&self, key: &str) -> impl Future<Output = Result<Meta, BucketError>> + Send {
        self.count(Request::Head);
        async move {
            self.turn().await;
            self.bucket.head(key).await
        }
    }

    /// A request a page of [`crate::bucket::LIST_PAGE`] objects, as S3 and GCS serve it, whatever
    /// the bucket under it does (TASK-455): paced so, its pages after the first taking the turns
    /// of the requests after it.
    async fn list(&self, prefix: &str) -> Result<Vec<Meta>, BucketError> {
        self.turn().await;
        let listed = self.bucket.list(prefix).await;
        let objects = listed.as_ref().map_or(0, Vec::len);
        let pages = crate::bucket::list_pages(objects);
        self.counts[Request::List as usize].fetch_add(pages, Ordering::Relaxed);
        if let Some(pacer) = &self.pacer {
            pacer.take(pages.saturating_sub(1).min(u32::MAX.into()) as u32);
        }
        listed
    }

    fn list_delimited(
        &self,
        prefix: &str,
        page: Page<'_>,
    ) -> impl Future<Output = Result<Listing, BucketError>> + Send {
        self.count(Request::ListDelimited);
        async move {
            self.turn().await;
            self.bucket.list_delimited(prefix, page).await
        }
    }

    fn put(
        &self,
        key: &str,
        bytes: bytes::Bytes,
        mode: PutMode,
    ) -> impl Future<Output = Result<Version, BucketError>> + Send {
        self.count(match mode {
            PutMode::Create => Request::Create,
            PutMode::Update(_) => Request::Update,
            PutMode::Overwrite => Request::Overwrite,
        });
        async move {
            self.turn().await;
            self.bucket.put(key, bytes, mode).await
        }
    }

    fn delete(&self, key: &str) -> impl Future<Output = Result<(), BucketError>> + Send {
        self.count(Request::Delete);
        async move {
            self.turn().await;
            self.bucket.delete(key).await
        }
    }

    fn delete_if(
        &self,
        key: &str,
        version: &Version,
    ) -> impl Future<Output = Result<(), BucketError>> + Send {
        self.count(Request::Delete);
        async move {
            self.turn().await;
            self.bucket.delete_if(key, version).await
        }
    }

    /// One request, as a backend with batch deletes makes it, whatever the bucket under it does.
    fn delete_batch(&self, keys: &[&str]) -> impl Future<Output = Result<(), BucketError>> + Send {
        self.count(Request::DeleteBatch);
        async move {
            self.turn().await;
            self.bucket.delete_batch(keys).await
        }
    }

    fn rewritable(&self, key: &str) -> impl Future<Output = ()> + Send {
        self.bucket.rewritable(key)
    }

    fn head_slots(
        &self,
        cell: crate::layout::CellId,
    ) -> impl Future<Output = Result<u64, crate::bucket::Refused>> + Send {
        self.bucket.head_slots(cell)
    }

    fn now(&self) -> u64 {
        self.bucket.now()
    }

    fn pause(&self, failures: u32) -> impl Future<Output = ()> + Send {
        self.bucket.pause(failures)
    }

    fn decoded(&self) -> Option<&crate::recovery::Decoded> {
        self.bucket.decoded()
    }

    fn mirrors(&self) -> Option<&dyn crate::mirror::Here> {
        self.bucket.mirrors()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::{Commit, SegmentBuilder};
    use crate::layout::{self, CellId};
    use crate::object_bucket::ObjectBucket;
    use crate::testing::{Direct, Fault, SimBucket};
    use crate::{Writer, lifecycle};
    use futures::executor::block_on;
    use rand::{RngExt as _, SeedableRng as _};
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};
    use std::task::{Context, Poll};
    use storage_device::ExtentId;

    /// A minute: longer than any pass of these tests.
    const LEASE: u64 = 60_000;

    /// The `n` cells of the tests' buckets, across three tenants.
    fn cells(n: u64) -> Vec<CellId> {
        let mut cells: Vec<CellId> = (0..n).map(|id| CellId { tenant: 1 + id % 3, id }).collect();
        cells.sort();
        cells
    }

    /// Fills each of `cells` of `bucket` with a cell whose checkpoint covers what a pass deletes:
    /// two segments, one holding what the other freed, beside the fence it keeps.
    async fn fill<B: Bucket>(bucket: &Arc<B>, cells: &[CellId]) {
        for &cell in cells {
            lifecycle::create(&**bucket, cell, 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), cell, 10, 1).await.unwrap();
            let first = writer.chain().next_sequence();
            let dead = ExtentId::new(1, first);
            let commits = [
                Commit { tag: 1, extents: vec![(dead, vec![7; 4096])], ..Commit::default() },
                Commit { tag: 2, frees: vec![dead], ..Commit::default() },
            ];
            for commit in commits {
                let framed = SegmentBuilder::from(commit)
                    .close(&mut writer.chain())
                    .frame(writer.keys().encrypting());
                let segment = writer.add_segment(framed).unwrap();
                writer.upload(segment).await.unwrap();
                writer.acknowledge().await.unwrap();
            }
            writer.checkpoint().await.unwrap();
        }
    }

    /// Every object of `bucket`, and its size.
    async fn sizes(bucket: &impl Bucket) -> BTreeMap<String, u64> {
        let listed = bucket.list("").await.unwrap();
        listed.into_iter().map(|meta| (meta.key, meta.size)).collect()
    }

    /// The cell each request waiting at `bucket` is of, if it is any's: a pass's, as a pass asks
    /// nothing outside its cell here.
    fn waiting_cells(bucket: &SimBucket) -> BTreeSet<CellId> {
        (0..bucket.waiting())
            .filter_map(|index| {
                let asked = bucket.describe(index);
                layout::cell_of(asked.split_whitespace().nth(1)?)
            })
            .collect()
    }

    /// Runs `future` until it waits on what never comes, then drops it: a process gone. Polled
    /// again and again, as an executor would, as streams of many futures yield, waking their task.
    fn stalled<T: std::fmt::Debug>(future: impl Future<Output = T>) {
        let mut future = std::pin::pin!(future);
        let mut context = Context::from_waker(futures::task::noop_waker_ref());
        for _ in 0..1000 {
            if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
                panic!("ended: {output:?}");
            }
        }
    }

    /// Runs `future` over `bucket`, completing a request waiting at random at each step, as
    /// `seed` picks it, the backend's clock a tick on for each: what it gives.
    fn served_at_random<T>(bucket: &SimBucket, future: impl Future<Output = T>, seed: u64) -> T {
        let mut random = rand::rngs::StdRng::seed_from_u64(seed);
        let mut future = std::pin::pin!(future);
        let mut context = Context::from_waker(futures::task::noop_waker_ref());
        let mut idle = 0;
        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
                return output;
            }
            // A stream of many futures yields, waking its task, none of its requests answered:
            // so far, and no further.
            if bucket.waiting() == 0 {
                idle += 1;
                assert!(idle < 1000, "waiting for no request");
                continue;
            }
            idle = 0;
            bucket.complete(random.random_range(0..bucket.waiting()), Fault::None);
            bucket.advance(1);
        }
    }

    fn sweep(at_once: usize) -> Sweep {
        Sweep { mode: Mode::Delete, at_once, lease: LEASE, slowest: 3, spawned: false }
    }

    /// Two processes sweeping one bucket at once split its cells: each cell passed over once, by
    /// one of them, so never by both at once; each leaves to the other the cells the other holds,
    /// or went over since it began; and between them they delete what a sweep alone would.
    #[test]
    fn sweeps_at_once_split_the_cells() {
        let cells = cells(24);
        for seed in 0..8 {
            let bucket = SimBucket::new();
            block_on(fill(&Arc::new(Direct(bucket.clone())), &cells));
            let passes = RefCell::new(Vec::new());
            let swept = served_at_random(
                &bucket,
                futures::future::join(
                    sweep(3).run(&bucket, |passed| passes.borrow_mut().push((0, passed.clone()))),
                    sweep(3).run(&bucket, |passed| passes.borrow_mut().push((1, passed.clone()))),
                ),
                seed,
            );
            let passes = passes.into_inner();
            let collected: Vec<&(usize, Ran)> = (passes.iter())
                .filter(|(_, passed)| matches!(passed.outcome, Outcome::Collected(_)))
                .collect();
            let mut by_cell: Vec<CellId> =
                collected.iter().map(|(_, passed)| passed.cell).collect();
            by_cell.sort();
            assert_eq!(by_cell, cells, "seed {seed}: each cell passed over once");
            let ran = |which| collected.iter().filter(|(by, _)| *by == which).count();
            let (a, b) = swept;
            assert!(ran(0) > 0 && ran(1) > 0, "seed {seed}: {} and {}", ran(0), ran(1));
            assert_eq!((a.visited, a.failed, b.visited, b.failed), (ran(0), 0, ran(1), 0));
            // What one left, the other ran: each went over every cell, the other's taken or not.
            assert_eq!((a.held + a.swept, b.held + b.swept), (b.visited, a.visited), "seed {seed}");
            assert_eq!(a.report.deleted + b.report.deleted, 2 * cells.len());
        }
    }

    /// A sweep runs no more passes at once than it is asked: never more cells' requests waiting
    /// at once, and as many, once there are cells enough.
    #[test]
    fn sweeps_run_so_many_passes_at_once() {
        for at_once in [1, 4] {
            let bucket = SimBucket::new();
            block_on(fill(&Arc::new(Direct(bucket.clone())), &cells(20)));
            let sweeping = sweep(at_once);
            let mut future = std::pin::pin!(sweeping.run(&bucket, |_| {}));
            let mut context = Context::from_waker(futures::task::noop_waker_ref());
            let mut most = 0;
            let swept = loop {
                if let Poll::Ready(swept) = future.as_mut().poll(&mut context) {
                    break swept;
                }
                let waiting = waiting_cells(&bucket);
                assert!(waiting.len() <= at_once, "{at_once} at once: {waiting:?}");
                most = most.max(waiting.len());
                while bucket.waiting() > 0 {
                    bucket.complete(0, Fault::None);
                }
            };
            assert_eq!(most, at_once);
            assert_eq!((swept.visited, swept.report.deleted), (20, 40));
        }
    }

    /// A bucket whose deletes never end, as a process gone mid-pass leaves them.
    struct Gone(Direct);

    impl Bucket for Gone {
        fn get(&self, key: &str) -> impl Future<Output = Result<Object, BucketError>> + Send {
            self.0.get(key)
        }

        fn get_range(
            &self,
            key: &str,
            range: ByteRange,
        ) -> impl Future<Output = Result<Vec<u8>, BucketError>> + Send {
            self.0.get_range(key, range)
        }

        fn head(&self, key: &str) -> impl Future<Output = Result<Meta, BucketError>> + Send {
            self.0.head(key)
        }

        fn list(
            &self,
            prefix: &str,
        ) -> impl Future<Output = Result<Vec<Meta>, BucketError>> + Send {
            self.0.list(prefix)
        }

        fn list_delimited(
            &self,
            prefix: &str,
            page: Page<'_>,
        ) -> impl Future<Output = Result<Listing, BucketError>> + Send {
            self.0.list_delimited(prefix, page)
        }

        fn put(
            &self,
            key: &str,
            bytes: bytes::Bytes,
            mode: PutMode,
        ) -> impl Future<Output = Result<Version, BucketError>> + Send {
            self.0.put(key, bytes, mode)
        }

        fn delete(&self, _: &str) -> impl Future<Output = Result<(), BucketError>> + Send {
            std::future::pending()
        }

        fn delete_if(
            &self,
            key: &str,
            version: &Version,
        ) -> impl Future<Output = Result<(), BucketError>> + Send {
            let _ = (key, version);
            std::future::pending()
        }

        fn delete_batch(&self, _: &[&str]) -> impl Future<Output = Result<(), BucketError>> + Send {
            std::future::pending()
        }

        fn now(&self) -> u64 {
            self.0.now()
        }
    }

    /// A bucket that tells `note` of each request's key as it is made.
    struct Noting<F> {
        bucket: Arc<ObjectBucket>,
        note: F,
    }

    impl<F: Fn(&str) + Send + Sync + 'static> Bucket for Noting<F> {
        fn get(&self, key: &str) -> impl Future<Output = Result<Object, BucketError>> + Send {
            (self.note)(key);
            self.bucket.get(key)
        }

        fn get_range(
            &self,
            key: &str,
            range: ByteRange,
        ) -> impl Future<Output = Result<Vec<u8>, BucketError>> + Send {
            (self.note)(key);
            self.bucket.get_range(key, range)
        }

        fn head(&self, key: &str) -> impl Future<Output = Result<Meta, BucketError>> + Send {
            (self.note)(key);
            self.bucket.head(key)
        }

        fn list(
            &self,
            prefix: &str,
        ) -> impl Future<Output = Result<Vec<Meta>, BucketError>> + Send {
            (self.note)(prefix);
            self.bucket.list(prefix)
        }

        fn list_delimited(
            &self,
            prefix: &str,
            page: Page<'_>,
        ) -> impl Future<Output = Result<Listing, BucketError>> + Send {
            (self.note)(prefix);
            self.bucket.list_delimited(prefix, page)
        }

        fn put(
            &self,
            key: &str,
            bytes: bytes::Bytes,
            mode: PutMode,
        ) -> impl Future<Output = Result<Version, BucketError>> + Send {
            (self.note)(key);
            self.bucket.put(key, bytes, mode)
        }

        fn delete(&self, key: &str) -> impl Future<Output = Result<(), BucketError>> + Send {
            (self.note)(key);
            self.bucket.delete(key)
        }

        fn delete_if(
            &self,
            key: &str,
            version: &Version,
        ) -> impl Future<Output = Result<(), BucketError>> + Send {
            (self.note)(key);
            self.bucket.delete_if(key, version)
        }

        /// One request, as [`Counted`] counts it.
        fn delete_batch(
            &self,
            keys: &[&str],
        ) -> impl Future<Output = Result<(), BucketError>> + Send {
            (self.note)(keys.first().copied().unwrap_or_default());
            self.bucket.delete_batch(keys)
        }

        fn now(&self) -> u64 {
            self.bucket.now()
        }
    }

    /// TASK-395.15: a sweep spawning its passes runs each as a task of its own, on the runtime's
    /// threads, and does what one running them all in its own task does: before, all of a sweep's
    /// passes shared its task, and so one thread, which a sweep at 16 at once could keep busy.
    #[test]
    fn spawned_sweeps_run_each_pass_as_a_task_of_its_own() {
        let cells = cells(10);
        let runtime =
            tokio::runtime::Builder::new_multi_thread().worker_threads(4).build().unwrap();
        let swept = |spawned: bool| {
            let memory =
                Arc::new(ObjectBucket::new(Arc::new(object_store::memory::InMemory::new()), ""));
            // For each request of a cell's, the Tokio task that made it.
            let tasks = Arc::new(std::sync::Mutex::new(BTreeSet::new()));
            let noted = tasks.clone();
            let note = move |key: &str| {
                if let Some(cell) = layout::cell_of(key) {
                    noted.lock().unwrap().insert((cell, tokio::task::try_id()));
                }
            };
            let bucket = Arc::new(Noting { bucket: memory.clone(), note });
            runtime.block_on(fill(&memory, &cells));
            let sweep = Sweep { spawned, ..sweep(4) };
            let swept =
                runtime.block_on(runtime.spawn(async move { sweep.run(&bucket, |_| {}).await }));
            let swept = swept.unwrap();
            let tasks = std::mem::take(&mut *tasks.lock().unwrap());
            (swept.visited, swept.report.deleted, tasks)
        };
        let (visited, deleted, tasks) = swept(true);
        assert_eq!((visited, deleted), (10, 20));
        let by_cell: BTreeSet<CellId> = tasks.iter().map(|&(cell, _)| cell).collect();
        let by_task: BTreeSet<Option<tokio::task::Id>> = tasks.iter().map(|&(_, id)| id).collect();
        assert_eq!((by_cell.len(), by_task.len(), tasks.len()), (10, 10, 10), "{tasks:?}");
        assert!(by_task.iter().all(Option::is_some));
        let (visited, deleted, tasks) = swept(false);
        assert_eq!((visited, deleted), (10, 20));
        let by_task: BTreeSet<Option<tokio::task::Id>> = tasks.iter().map(|&(_, id)| id).collect();
        assert_eq!((tasks.len(), by_task.len()), (10, 1), "{tasks:?}");
    }

    /// A process gone mid-sweep leaves the cells its passes held: the next sweep leaves them to
    /// those passes until their leases expire, and the one after takes them and collects them.
    #[test]
    fn cells_of_a_process_gone_are_collected_once_their_leases_expire() {
        let cells = cells(12);
        let sim = SimBucket::new();
        let bucket = Arc::new(Direct(sim.clone()));
        block_on(fill(&bucket, &cells));
        let before = block_on(sizes(&*bucket));
        // Its passes, three at once, each stuck deleting: then it is gone.
        let gone = Arc::new(Gone(Direct(sim.clone())));
        {
            let three = sweep(3);
            stalled(three.run(&gone, |_| {}));
        }
        assert_eq!(block_on(sizes(&*bucket)).len(), before.len() + 3, "three cells' leases");
        sim.advance(1);
        let next = block_on(sweep(3).run(&bucket, |_| {}));
        assert_eq!((next.visited, next.held, next.swept, next.failed), (9, 3, 0, 0));
        assert_eq!(next.report.deleted, 18);
        // Their leases expired: the next sweep takes them.
        sim.advance(LEASE);
        let after = block_on(sweep(3).run(&bucket, |_| {}));
        assert_eq!((after.visited, after.held, after.swept, after.failed), (12, 0, 0, 0));
        assert_eq!(after.report.deleted, 6);
        let left = block_on(sizes(&*bucket));
        let leases = cells.iter().map(|&cell| layout::collector_lease(cell));
        let deleted = |key: &&String| {
            matches!(layout::kind(key), layout::Kind::Segment { segment: 0..=1, .. })
        };
        let kept: BTreeSet<String> =
            (before.keys().filter(|key| !deleted(key)).cloned()).chain(leases).collect();
        assert_eq!(left.keys().cloned().collect::<BTreeSet<_>>(), kept);
    }

    /// A sweep reports what its passes did as the bucket counts it: the objects gone and their
    /// bytes, and every request of each kind
    /// the bucket made, its passes' and its listings' of the cells; each pass's adding up to the
    /// whole; and its slowest passes, slowest first.
    #[test]
    fn sweeps_report_what_the_bucket_counts() {
        let cells = cells(10);
        let bucket =
            Arc::new(ObjectBucket::new(Arc::new(object_store::memory::InMemory::new()), ""));
        // Formatted, as a host's bucket is: its head slots read once (board 5534).
        let (details, asked) = Default::default();
        block_on(crate::identity::format(&*bucket, &details, asked)).unwrap();
        block_on(fill(&bucket, &cells));
        let before = block_on(sizes(&*bucket));
        let counted = bucket.stats();
        let sweep = Sweep { at_once: 4, ..sweep(1) };
        let mut passes = Vec::new();
        let swept = block_on(sweep.run(&bucket, |passed| passes.push(passed.clone())));
        let stats = bucket.stats();
        let after = block_on(sizes(&*bucket));

        let gone: Vec<(&String, &u64)> =
            before.iter().filter(|(key, _)| !after.contains_key(*key)).collect();
        let expected = Report {
            deleted: gone.len(),
            retired: 0,
            deleted_bytes: gone.iter().map(|(_, size)| **size).sum(),
            skipped: 0,
            removed: 0,
        };
        assert_eq!(swept.report, expected);
        // Of each cell: the segment holding what the next freed, and that one.
        assert_eq!(expected.deleted, 20);
        assert_eq!((swept.visited, swept.held, swept.swept, swept.failed), (10, 0, 0, 0));
        let mut made = Requests::default();
        for request in Request::ALL {
            made.0[request as usize] = stats.requests(request) - counted.requests(request);
        }
        assert_eq!(swept.requests, made);
        // The cells listed: a page of tenants, and a page of each tenant's cells.
        let passes_listed: u64 =
            passes.iter().map(|passed| passed.requests.of(Request::ListDelimited)).sum();
        assert_eq!(swept.requests.of(Request::ListDelimited) - passes_listed, 4);
        let mut summed = Report::default();
        for passed in &passes {
            let Outcome::Collected(report) = passed.outcome else { panic!("{passed:?}") };
            summed += report;
        }
        assert_eq!(summed, expected);
        let mut slowest: Vec<(CellId, Duration)> =
            passes.iter().map(|passed| (passed.cell, passed.took)).collect();
        slowest.sort_by_key(|&(_, took)| std::cmp::Reverse(took));
        assert_eq!(swept.slowest.len(), 3);
        assert_eq!(
            swept.slowest.iter().map(|(_, took)| *took).collect::<Vec<_>>(),
            slowest[..3].iter().map(|(_, took)| *took).collect::<Vec<_>>()
        );
    }

    /// A pass leaves a cell another holds, unexpired, writing nothing; one whose last pass ended
    /// since its sweep began, too; and takes one whose holder's lease expired, fencing it. A
    /// collector holding cells for no time takes them whatever held them last but another's lease
    /// unexpired, and lets go of nothing, as its passes hold nothing.
    #[test]
    fn passes_leave_cells_held_or_swept_since() {
        let sim = SimBucket::new();
        let bucket = Arc::new(Direct(sim.clone()));
        let cell = cells(1)[0];
        block_on(fill(&bucket, &[cell]));
        // Who holds the lease, at what epoch, until when, and when the last pass to go through
        // ended.
        let lease = |bucket: &Direct| {
            let lease = block_on(crate::passed::read(bucket, cell)).unwrap().unwrap();
            (lease.epoch, lease.holder, lease.until, lease.ended)
        };
        let holding = |lease, since| Holding { lease, since };
        let pass = |holding: Holding, holder| {
            let collector = Collector::new().holding(holding);
            block_on(collector.collect_cell(&*bucket, cell, holder))
        };
        sim.advance(10);
        // Gone mid-pass: its lease held till 10 + LEASE.
        let gone = Arc::new(Gone(Direct(sim.clone())));
        let collector = Collector::new().holding(holding(LEASE, None));
        stalled(collector.collect_cell(&*gone, cell, 0x51));
        let held = (1, 0x51, 10 + LEASE, 0);
        assert_eq!(lease(&bucket), held);
        assert_eq!(pass(holding(LEASE, None), 0x52), Err(Error::Left(Left::Held)));
        assert_eq!(pass(holding(0, None), 0x53), Err(Error::Left(Left::Held)));
        assert_eq!(lease(&bucket), held);
        // Expired: taken, fenced, gone through, let go.
        sim.advance(LEASE);
        let report = pass(holding(LEASE, Some(5)), 0x54).unwrap();
        assert_eq!(report.deleted, 2);
        let now = 10 + LEASE;
        assert_eq!(lease(&bucket), (2, 0x54, 0, now));
        // A sweep that began before it ended leaves the cell; one after takes it.
        assert_eq!(pass(holding(LEASE, Some(now - 1)), 0x55), Err(Error::Left(Left::Swept)));
        // In full, the cell changed by the deletes of the pass before.
        assert_eq!(pass(holding(LEASE, Some(now)), 0x56), Ok(Report::default()));
        // The cell unchanged since the pass before, skipped (TASK-395.6), by a look: the lease
        // read, not taken, holding for no time or for a time (TASK-455).
        sim.advance(1);
        let skipped = Report { skipped: 1, ..Report::default() };
        assert_eq!(pass(holding(0, None), 0x57), Ok(skipped));
        assert_eq!(lease(&bucket), (3, 0x56, 0, now));
        sim.advance(1);
        assert_eq!(pass(holding(LEASE, None), 0x58), Ok(skipped));
        assert_eq!(lease(&bucket), (3, 0x56, 0, now));
    }
    /// TASK-455's AC #4: a listing is counted as the pages S3 and GCS serve it in, 1,000 keys a
    /// page, one at least: before, one, however many.
    #[test]
    fn listings_are_counted_by_their_pages() {
        let bucket = Arc::new(Direct(SimBucket::new()));
        block_on(async {
            for n in 0..2001 {
                crate::bucket::create(&*bucket, &format!("many/{n:05}"), vec![1]).await.unwrap();
            }
        });
        let counted = Counted::new(bucket, None);
        for (prefix, objects, pages) in [("none/", 0, 1), ("many/0000", 10, 1), ("many/", 2001, 3)]
        {
            let before = counted.requests().of(Request::List);
            let listed = block_on(counted.list(prefix)).unwrap();
            assert_eq!(listed.len(), objects, "{prefix}");
            assert_eq!(counted.requests().of(Request::List) - before, pages, "{prefix}");
        }
        assert_eq!([0, 1, 1000, 1001, 2000].map(crate::bucket::list_pages), [1, 1, 1, 2, 2]);
    }

    /// TASK-455: cells whose cells, roots of IDs an allocator gave, were deleted go whole in the
    /// sweep that first passes over them, and cost the sweeps after it the listing of the bucket's
    /// tenants alone, a request, however many there were: none of their objects is left.
    #[test]
    fn dead_cells_go_whole_and_cost_later_sweeps_a_listing_however_many() {
        for n in [4u64, 40] {
            let bucket = Arc::new(Direct(SimBucket::new()));
            let ids = crate::objectid::Allocator::new();
            block_on(async {
                for id in 1..=n {
                    let cell = CellId { tenant: 1, id };
                    let cell =
                        CellId { tenant: cell.tenant, id: ids.next(&*bucket).await.unwrap() };
                    lifecycle::create(&*bucket, cell, 0).await.unwrap();
                    let writer = Writer::open(bucket.clone(), cell, 10, 1).await.unwrap();
                    let extent = ExtentId::new(cell.id, writer.chain().next_sequence());
                    let commit = Commit {
                        tag: 1,
                        extents: vec![(extent, vec![7; 64])],
                        ..Commit::default()
                    };
                    let framed = SegmentBuilder::from(commit)
                        .close(&mut writer.chain())
                        .frame(writer.keys().encrypting());
                    let segment = writer.add_segment(framed).unwrap();
                    writer.upload(segment).await.unwrap();
                    writer.acknowledge().await.unwrap();
                    writer.checkpoint().await.unwrap();
                    drop(writer);
                    lifecycle::delete(&*bucket, cell, 11, 1).await.unwrap();
                }
            });
            let swept = [0; 3].map(|_| block_on(sweep(1).run(&bucket, |_| {})));
            let removed = swept.each_ref().map(|swept| (swept.report.removed, swept.visited));
            assert_eq!(removed, [(n as usize, n as usize), (0, 0), (0, 0)], "{n} cells");
            let mut listing = Requests::default();
            listing.0[Request::ListDelimited as usize] = 1;
            assert_eq!([swept[1].requests, swept[2].requests], [listing, listing], "{n} cells");
            let left = block_on(bucket.list("tenants/")).unwrap();
            assert_eq!(left, Vec::new(), "{n} cells");
        }
    }

    /// TASK-395.5 with TASK-395.6: two processes sweeping one bucket by turns, each a pass over an
    /// idle cell, skip it by what the other's pass recorded: whoever went over it last, the record
    /// stays the one the last pass in full wrote, as a pass that skips records nothing new, and
    /// what holds and lets go of the lease is outside the listing's digest. A skipping pass is a
    /// look: the lease's read (a GET) and one LIST, writing nothing (TASK-455), so that a sweep
    /// begun before it, after the pass in full, looks too. A write to the
    /// cell makes the next pass, either process's, go in full, and the other's skip after it.
    #[test]
    fn processes_by_turns_skip_an_idle_cell_by_each_others_records() {
        let sim = SimBucket::new();
        let bucket = Arc::new(Direct(sim.clone()));
        let cell = cells(1)[0];
        block_on(fill(&bucket, &[cell]));
        let recorded = || block_on(crate::passed::read(&*bucket, cell)).unwrap().unwrap().passed;
        let (first, second) = (sweep(1), sweep(1));
        let run = |process: &Sweep| {
            sim.advance(1);
            block_on(process.run_cell(&bucket, cell, |_| {}))
        };
        let skipped = Report { skipped: 1, ..Report::default() };
        let mut idle = Requests::default();
        for (request, count) in [(Request::Get, 1), (Request::List, 1)] {
            idle.0[request as usize] = count;
        }
        // In full, deleting; in full again, the cell changed by those deletes, and settled.
        assert_eq!(run(&first).report.deleted, 2);
        let settling = run(&second);
        assert_eq!((settling.report, settling.visited), (Report::default(), 1));
        let settled = recorded().unwrap();
        assert!(settled.listing.is_some(), "{settled:?}");
        for (turn, process) in [&first, &second, &first, &second].into_iter().enumerate() {
            let swept = run(process);
            assert_eq!((swept.report, swept.requests), (skipped, idle), "turn {turn}");
            assert_eq!(recorded().as_ref(), Some(&settled), "turn {turn}");
        }
        // A sweep that began before the last look, after the pass in full, looks too: a look
        // records nothing, ending nothing (TASK-455).
        let began = bucket.now() - 1;
        let late = Collector::new().holding(Holding { lease: LEASE, since: Some(began) });
        assert_eq!(block_on(late.collect_cell(&*bucket, cell, 0x77)), Ok(skipped));

        // A reference written in the cell: the next pass, either's, goes over it in full.
        let reference = layout::reference(cell, 1, CellId { tenant: cell.tenant, id: 2 });
        let bytes = layout::encode(&layout::Reference::of(2));
        block_on(crate::bucket::create(&*bucket, &reference, bytes)).unwrap();
        assert_eq!(run(&first).report, Report::default());
        let rewritten = recorded().unwrap();
        assert_ne!(rewritten.listing, settled.listing);
        assert_eq!(run(&second).report, skipped);
        assert_eq!(recorded(), Some(rewritten));
    }

    /// A bucket that refuses every request but listings by delimiter, as one whose credentials
    /// no longer reach its cells' objects: what a pass asks first, a GET of its cell's lease, it
    /// refuses.
    struct Refusing(Direct);

    impl Refusing {
        fn refused<T>() -> std::future::Ready<Result<T, BucketError>> {
            std::future::ready(Err(BucketError::Refused(crate::bucket::Refusal::Denied)))
        }
    }

    impl Bucket for Refusing {
        fn get(&self, _: &str) -> impl Future<Output = Result<Object, BucketError>> + Send {
            Self::refused()
        }

        fn get_range(
            &self,
            _: &str,
            _: ByteRange,
        ) -> impl Future<Output = Result<Vec<u8>, BucketError>> + Send {
            Self::refused()
        }

        fn head(&self, _: &str) -> impl Future<Output = Result<Meta, BucketError>> + Send {
            Self::refused()
        }

        fn list(&self, _: &str) -> impl Future<Output = Result<Vec<Meta>, BucketError>> + Send {
            Self::refused()
        }

        fn list_delimited(
            &self,
            prefix: &str,
            page: Page<'_>,
        ) -> impl Future<Output = Result<Listing, BucketError>> + Send {
            self.0.list_delimited(prefix, page)
        }

        fn put(
            &self,
            _: &str,
            _: bytes::Bytes,
            _: PutMode,
        ) -> impl Future<Output = Result<Version, BucketError>> + Send {
            Self::refused()
        }

        fn delete(&self, _: &str) -> impl Future<Output = Result<(), BucketError>> + Send {
            Self::refused()
        }

        fn delete_if(
            &self,
            _: &str,
            _: &Version,
        ) -> impl Future<Output = Result<(), BucketError>> + Send {
            Self::refused()
        }

        fn delete_batch(&self, _: &[&str]) -> impl Future<Output = Result<(), BucketError>> + Send {
            Self::refused()
        }

        fn now(&self) -> u64 {
            self.0.now()
        }
    }

    /// board 8529's AC #1: a sweep's passes delete no more together than its bounds, of objects
    /// or of bytes, let them: a pass that would go past one deletes nothing, nor does any after
    /// it, and the sweep starts no more, saying so; and records nothing new, so that the next
    /// sweep, unbounded, deletes what it left, and the rest.
    #[test]
    fn sweeps_delete_no_more_than_their_bounds() {
        let cells = cells(10);
        // Bounds, and the passes run, the objects deleted.
        let cases = [
            (Limits { deleted_objects: Some(5), ..Limits::default() }, 3, 4),
            (Limits { deleted_objects: Some(6), ..Limits::default() }, 4, 6),
            (Limits { deleted_bytes: Some(1), ..Limits::default() }, 1, 0),
        ];
        for (limits, visited, deleted) in cases {
            let bucket = Arc::new(Direct(SimBucket::new()));
            block_on(fill(&bucket, &cells));
            // The cells' objects, their leases, the collector's, aside.
            let objects = || {
                let lease = |key: &&String| {
                    matches!(layout::kind(key), layout::Kind::CollectorLease { .. })
                };
                block_on(sizes(&*bucket)).keys().filter(|key| !lease(key)).count()
            };
            let before = objects();
            let own = None::<&Arc<Direct>>;
            let swept = block_on(sweep(1).run_limited(&bucket, own, None, &limits, |_| {}));
            let said = (swept.visited, swept.report.deleted, swept.capped, swept.stopped.clone());
            assert_eq!(said, (visited, deleted, 1, Some(Stopped::Capped)), "{limits:?}");
            assert!(!swept.complete(), "{limits:?}");
            assert_eq!(objects(), before - deleted, "{limits:?}");
            let after = block_on(sweep(1).run(&bucket, |_| {}));
            assert_eq!(after.report.deleted, 2 * cells.len() - deleted, "{limits:?}");
            assert!(after.complete(), "{after:?}");
        }
    }

    /// board 8529's AC #3: after so many passes in a row the bucket refused, a sweep starts no
    /// more, saying so; unbounded, it runs a pass over every cell, each refused.
    #[test]
    fn sweeps_stop_after_passes_in_a_row_refused() {
        let cells = cells(12);
        let direct = Direct(SimBucket::new());
        block_on(fill(&Arc::new(Direct(direct.0.clone())), &cells));
        let bucket = Arc::new(Refusing(direct));
        let limits = Limits { refused_in_a_row: Some(3), ..Limits::default() };
        let own = None::<&Arc<Refusing>>;
        let swept = block_on(sweep(1).run_limited(&bucket, own, None, &limits, |_| {}));
        let said = (swept.visited, swept.failed, swept.passed, swept.stopped);
        assert_eq!(said, (3, 3, 0, Some(Stopped::Refusing(3))));
        let swept = block_on(sweep(1).run(&bucket, |_| {}));
        let said = (swept.visited, swept.failed, swept.passed, swept.stopped);
        assert_eq!(said, (12, 12, 0, None));
    }

    /// board 8509's AC #3: a sweep that could not read its bucket's own identity, and so tell its
    /// cells from copies, goes over none and says why, failed: before, it was a sweep of no
    /// cells, as if the bucket held none.
    #[test]
    fn sweeps_fail_on_their_buckets_identity_unread() {
        let bucket = Arc::new(Direct(SimBucket::new()));
        block_on(fill(&bucket, &cells(2)));
        let own = Arc::new(Refusing(Direct(SimBucket::new())));
        let swept = block_on(sweep(1).run_with_own(&bucket, Some(&own), |_| {}));
        assert_eq!((swept.visited, swept.failed), (0, 0));
        let Some(Stopped::Unidentified(Error::Refused(refused))) = &swept.stopped else {
            panic!("{swept:?}")
        };
        assert_eq!(refused.key, crate::layout::IDENTITY);
        assert!(!swept.complete());
    }

    /// board 8513's AC #1: a sweep asked to stop starts no more passes, and says so: asked before
    /// it began, none; asked as its first pass ended, that one alone.
    #[test]
    fn sweeps_asked_to_stop_start_no_more_passes() {
        let bucket = Arc::new(Direct(SimBucket::new()));
        block_on(fill(&bucket, &cells(6)));
        let stop = Arc::new(Stop::default());
        let limits = Limits { stop: Some(stop.clone()), ..Limits::default() };
        let run = |observe: &dyn Fn(&Ran)| {
            block_on(sweep(1).run_limited(&bucket, None::<&Arc<Direct>>, None, &limits, observe))
        };
        let swept = run(&|_| stop.ask());
        assert_eq!((swept.visited, swept.report.deleted), (1, 2));
        assert_eq!(swept.stopped, Some(Stopped::Asked));
        let swept = run(&|_| {});
        assert_eq!((swept.visited, swept.stopped), (0, Some(Stopped::Asked)));
    }

    /// board 8529's AC #2: a sweep making so many requests a second at most makes them in their
    /// turns, its passes' and its listing's together, each the pace's time after the one before,
    /// as the bucket under it sees them: as many as it counts.
    #[test]
    fn paced_sweeps_make_their_requests_in_their_turns() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        let memory =
            Arc::new(ObjectBucket::new(Arc::new(object_store::memory::InMemory::new()), ""));
        runtime.block_on(fill(&memory, &cells(4)));
        let made: Arc<std::sync::Mutex<Vec<tokio::time::Instant>>> = Arc::default();
        let noted = made.clone();
        let note = move |_: &str| noted.lock().unwrap().push(tokio::time::Instant::now());
        let bucket = Arc::new(Noting { bucket: memory, note });
        let limits = Limits { requests_per_second: Some(20.0), ..Limits::default() };
        let (began, swept) = runtime.block_on(async {
            let began = tokio::time::Instant::now();
            let own = None::<&Arc<Direct>>;
            (began, sweep(2).run_limited(&bucket, own, None, &limits, |_| {}).await)
        });
        assert_eq!((swept.visited, swept.report.deleted, swept.stopped), (4, 8, None));
        let made = made.lock().unwrap().clone();
        assert_eq!(made.len() as u64, swept.requests.total());
        let turns: Vec<Duration> = made.iter().map(|at| *at - began).collect();
        let expected: Vec<Duration> =
            (0..made.len() as u32).map(|turn| Duration::from_millis(50) * turn).collect();
        assert_eq!(turns, expected);
    }
}
