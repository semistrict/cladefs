// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A bucket of S3 or GCS, or of anything else the `object_store` crate reaches, under a prefix.
//!
//! Cells rely on conditional writes (doc-7, The head): a create where an object is must fail,
//! and an update of a version that is not the object's. `object_store` sends S3's `If-None-Match`
//! and `If-Match`, and GCS's generation preconditions; [`probe`] checks that a backend honours them
//! before anything relies on it, once for each bucket ([`ObjectBucket::probed`]).
//!
//! A bucket counts the requests it makes, by kind, those that fail too, which object storage bills,
//! and the bytes it uploads, by kind of object, and downloads ([`ObjectBucket::stats`]).
//!
//! cladefs: requests that fail, which the helpers of [`crate::bucket`] retry until they answer, it
//! counts by kind ([`Stats::failures`]) and logs, a warning naming the backend's error, the first
//! of a run of failures of a kind and then every [`FAILING_EVERY`], with how many failed, and once
//! the kind answers again (TASK-314): a bucket that keeps failing, full, out of reach or refusing
//! its credentials, stalls its cells' guests' commits at their lag bound (doc-7, decision 3), which
//! the log says why of. And once a write fails, writes of its kind take turns ([`Turns`],
//! TASK-382): one at a time, probing the bucket, until one is answered, then more as more are.
//! Updates take none: each is of its own key, which GCS refuses a write of within about a second of
//! the last, while it takes the others' (TASK-690). A bucket spaces its writes of a key as its
//! backend needs ([`Rewrites`]).
//!
//! cladefs: a level of keys listed by delimiter, it lists a page a request where the backend pages
//! (S3, GCS and Azure, [`PaginatedListStore`]), as many prefixes and objects as the page asks,
//! never the whole level (TASK-395.5): a tenant's millions of cells are listed a page at a time.
//! Files and memory list a level whole, which their pages are cut out of: a directory is read whole
//! by any listing of it, and memory has nothing cheaper.
//!
//! cladefs: every reader of a bucket reads it through an `ObjectBucket`, which warns of reading
//! what the collector's newest mark-only pass marked ([`crate::marks`], TASK-264): on wherever a
//! marks object is, as a bucket reads the newest every [`crate::marks::REFRESH`] alongside a read
//! of its own.

use crate::bucket::{
    Bucket, ByteRange, Error, LIST_PAGE, Listing, Meta, Object, Page, PutMode, Version,
};
use crate::client::Client;
use crate::layout::{self, Kind};
pub use crate::routed::{Resolve, RoutedBucket};
use futures::{StreamExt as _, TryStreamExt as _, stream};
use object_store::list::{PaginatedListOptions, PaginatedListStore};
use object_store::path::Path;
use object_store::{
    GetOptions, GetRange, ObjectMeta, ObjectStore, PutOptions, PutPayload, UpdateVersion,
};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A single bucket's objects under a prefix (for multi-bucket routing, wrap in [`RoutedBucket`]).
pub struct ObjectBucket {
    backend: Arc<dyn ObjectStore>,
    /// cladefs: the same backend, which lists a level of keys a page a request, if it does (S3,
    /// GCS, Azure): none for files and memory, which list it whole (TASK-395.5).
    pages: Option<Arc<dyn PaginatedListStore>>,
    prefix: String,
    counts: Counts,
    /// Whether [`ObjectBucket::probed`] found it honours conditional writes: what it found holds.
    honoured: std::sync::atomic::AtomicBool,
    /// cladefs: the checkpoints' states its recoveries decoded ([`Bucket::decoded`]).
    decoded: crate::recovery::Decoded,
    /// cladefs: what the collector's newest mark-only pass marked, which its reads warn of reaching
    /// (TASK-264).
    marks: crate::marks::Watch,
    /// cladefs: what gives this process's new cells their IDs, out of blocks of the bucket's
    /// (TASK-364).
    objectids: crate::objectid::Allocator,
    /// cladefs: how far apart its backend takes writes of one key, and when it last wrote each
    /// (TASK-690).
    rewrites: Rewrites,
    /// cladefs: its settings, as its identity says, once read, or written by its formatting
    /// (TASK-691): the defaults' for a bucket with none.
    settings: std::sync::OnceLock<crate::identity::Settings>,
}

/// cladefs: how long after GCS takes a write of an object it takes another: about one a second,
/// refusing more with `429 Too Many Requests` ("The rate of change requests to the object ...
/// exceeds the rate limit", Cloud Storage's quotas: "up to once per second" for one object). S3 and
/// Azure have no such limit (TASK-690).
pub const GCS_REWRITE_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// cladefs: how far apart a bucket's backend takes writes of one key, and when the bucket's last
/// write of each was answered, for each write of it to wait out the rest of that time first
/// (TASK-690): GCS's second ([`GCS_REWRITE_EVERY`]), none for other backends, which wait for
/// nothing.
#[derive(Debug, Default)]
struct Rewrites {
    every: std::time::Duration,
    written: std::sync::Mutex<Written>,
}

/// cladefs: when a bucket's last write of each key was answered, of those within
/// [`Rewrites::every`] of their last pruning, at least.
#[derive(Debug, Default)]
struct Written {
    at: std::collections::HashMap<Path, tokio::time::Instant>,
    /// How many it may hold before those past [`Rewrites::every`] are pruned: twice as many as it
    /// kept the last time, so that pruning costs each write a constant share.
    prune_at: usize,
}

impl Rewrites {
    /// The fewest keys whose writes are kept before any are pruned.
    const PRUNE_AT: usize = 1024;

    fn written(&self) -> std::sync::MutexGuard<'_, Written> {
        self.written.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Waits until the backend takes another write of `path`: [`Rewrites::every`] after its last
    /// was answered.
    async fn wait(&self, path: &Path) {
        if self.every.is_zero() {
            return;
        }
        let last = self.written().at.get(path).copied();
        if let Some(last) = last {
            tokio::time::sleep_until(last + self.every).await;
        }
    }

    /// A write of `path` was answered, now.
    fn wrote(&self, path: &Path) {
        if self.every.is_zero() {
            return;
        }
        let now = tokio::time::Instant::now();
        let mut written = self.written();
        if written.at.len() >= written.prune_at.max(Self::PRUNE_AT) {
            written.at.retain(|_, at| now.duration_since(*at) < self.every);
            written.prune_at = 2 * written.at.len();
        }
        written.at.insert(path.clone(), now);
    }
}

/// The kinds of request a bucket makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Request {
    Get,
    /// A GET of a range of an object.
    GetRange,
    Head,
    List,
    /// A PUT where no object is.
    Create,
    /// A PUT of an object's version.
    Update,
    Overwrite,
    Delete,
    /// cladefs: a listing of one level of keys, by delimiter (TASK-395.5).
    ListDelimited,
    /// cladefs: a batch of deletes (TASK-395.5): one request where the backend deletes many at once
    /// (S3's DeleteObjects, of up to 1,000; Azure's, of 256 each), one an object behind it
    /// where it does not (GCS, files, memory), which `object_store` does not say.
    DeleteBatch,
}

impl Request {
    pub const ALL: [Self; 10] = [
        Self::Get,
        Self::GetRange,
        Self::Head,
        Self::List,
        Self::Create,
        Self::Update,
        Self::Overwrite,
        Self::Delete,
        Self::ListDelimited,
        Self::DeleteBatch,
    ];

    /// cladefs: the name of its span ([`crate::trace`], TASK-258).
    pub fn span_name(self) -> &'static str {
        match self {
            Self::Get => "bucket.get",
            Self::GetRange => "bucket.get_range",
            Self::Head => "bucket.head",
            Self::List => "bucket.list",
            Self::Create => "bucket.create",
            Self::Update => "bucket.update",
            Self::Overwrite => "bucket.overwrite",
            Self::Delete => "bucket.delete",
            Self::ListDelimited => "bucket.list_delimited",
            Self::DeleteBatch => "bucket.delete_batch",
        }
    }

    /// cladefs: whether its writes take turns once one failed ([`Turns`], TASK-382): all but an
    /// update, of a key of its own, a cell's head, which says nothing of the backend by failing as
    /// GCS refuses a write of a key within about a second of the last, while it takes others'; it
    /// backs off alone, as its retries do (TASK-690).
    fn takes_turns(self) -> bool {
        self != Self::Update
    }

    /// Its name in [`Stats::line`].
    pub fn name(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::GetRange => "get-range",
            Self::Head => "head",
            Self::List => "list",
            Self::Create => "create",
            Self::Update => "update",
            Self::Overwrite => "overwrite",
            Self::Delete => "delete",
            Self::ListDelimited => "list-delimited",
            Self::DeleteBatch => "delete-batch",
        }
    }
}

/// The kinds of object a bucket uploads, by their keys (doc-7, Layout).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Upload {
    Segments,
    /// Extents stored on their own.
    Extents,
    Checkpoints,
    /// The collector's moves of what it compacted.
    Moves,
    /// Cells' records and heads, and whatever else.
    Other,
}

impl Upload {
    pub const ALL: [Self; 5] =
        [Self::Segments, Self::Extents, Self::Checkpoints, Self::Moves, Self::Other];

    fn of(key: &str) -> Self {
        match layout::kind(key) {
            Kind::Segment { .. } => Self::Segments,
            Kind::Extent { .. } => Self::Extents,
            Kind::Checkpoint { .. } => Self::Checkpoints,
            Kind::Moves { .. } => Self::Moves,
            _ => Self::Other,
        }
    }

    /// Its name in [`Stats::line`].
    pub fn name(self) -> &'static str {
        match self {
            Self::Segments => "segments",
            Self::Extents => "extents",
            Self::Checkpoints => "checkpoints",
            Self::Moves => "moves",
            Self::Other => "other",
        }
    }
}

#[derive(Default)]
struct Counts {
    requests: [AtomicU64; Request::ALL.len()],
    /// The nanoseconds the requests of each kind took, each from its start to its answer.
    request_ns: [AtomicU64; Request::ALL.len()],
    uploaded: [AtomicU64; Upload::ALL.len()],
    downloaded: AtomicU64,
    /// cladefs: the requests of each kind that failed, unanswered (TASK-314).
    failures: [AtomicU64; Request::ALL.len()],
    /// cladefs: the run of failures of each kind going on, if one is.
    failing: [Failing; Request::ALL.len()],
    /// cladefs: the turns the writes of each kind take, since one failed (TASK-382).
    turns: [Turns; Request::ALL.len()],
}

/// cladefs: the writes of a kind in flight at once once one failed (TASK-382): one, a probe of the
/// bucket, after [`backoff`] of the failures in a row since the last answer, until one is
/// answered; then one more for each answered, as TCP's slow start opens its window, doubling each
/// round trip, until [`Turns::OPEN`], when none waits for a turn any more.
///
/// Each write sends its object whole, every try of it (and `object_store` tries each several times
/// before it says it failed). Every write of a host's uploads, as many as its upload memory holds,
/// sending its object to a full or failing bucket again and again, each as its own backoff said,
/// was most of the extent daemon's CPU in the qualification's bucket soak: the kernel's copies into
/// the sockets and the pages it zeroed for them, 1,200 of its 1,580 s on the test host. And as the
/// bucket answers again, those waiting don't all go at once: RustFS on a slow disk refused such a
/// rush (`SlowDown`), failing each write it refused again, until the soak ran out of time.
#[derive(Default)]
struct Turns {
    window: std::sync::Mutex<Window>,
    /// Told as a write's turn ends, the window opens, or a write fails.
    changed: tokio::sync::Notify,
}

#[derive(Default)]
struct Window {
    /// The writes that may be in flight at once, taking turns; none, if no write takes a turn.
    limit: Option<u64>,
    /// The writes in flight with a turn.
    in_flight: u64,
    /// The writes that failed in a row, and when the latest did, which the next probe goes
    /// [`backoff`] after.
    failed: u32,
    last: Option<tokio::time::Instant>,
}

impl Turns {
    /// The window at which no write waits for its turn any more: the host's uploads' PUTs at once
    /// (`cladefs::bucket`'s `UPLOADS`).
    const OPEN: u64 = 64;

    fn window(&self) -> std::sync::MutexGuard<'_, Window> {
        self.window.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Waits for a write's turn, if writes take turns: until fewer than the window are in flight,
    /// and, after a failure, [`backoff`] of the failures in a row passed since the latest. Its turn
    /// lasts until what it returns is dropped, as the write is answered.
    async fn turn(&self) -> Option<Turn<'_>> {
        loop {
            // Told of a change from before the window is looked at, so that none goes unseen.
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let due = {
                let mut window = self.window();
                let Some(limit) = window.limit else { return None };
                let due = window.last.map(|last| last + backoff(window.failed));
                let now = tokio::time::Instant::now();
                if window.in_flight < limit && due.is_none_or(|due| due <= now) {
                    window.in_flight += 1;
                    return Some(Turn(self));
                }
                due.filter(|due| *due > now)
            };
            match due {
                Some(due) => {
                    let sleeping = std::pin::pin!(tokio::time::sleep_until(due));
                    futures::future::select(sleeping, &mut changed).await;
                }
                None => changed.await,
            }
        }
    }

    /// A write of the kind failed: one at a time from now on, each after [`backoff`].
    fn failed(&self) {
        let mut window = self.window();
        window.limit = Some(1);
        window.failed = window.failed.saturating_add(1);
        window.last = Some(tokio::time::Instant::now());
        drop(window);
        self.changed.notify_waiters();
    }

    /// A write of the kind was answered: one more in flight at once, or none waiting from
    /// [`Turns::OPEN`] on.
    fn answered(&self) {
        let mut window = self.window();
        let Some(limit) = window.limit else { return };
        (window.failed, window.last) = (0, None);
        window.limit = Some(limit + 1).filter(|limit| *limit < Self::OPEN);
        drop(window);
        self.changed.notify_waiters();
    }
}

/// A write's turn ([`Turns::turn`]), until it drops.
struct Turn<'a>(&'a Turns);

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.0.window().in_flight -= 1;
        self.0.changed.notify_waiters();
    }
}

/// cladefs: how often a run of failures of requests of a kind is logged again, with how many
/// failed, after its first (TASK-314).
pub const FAILING_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// cladefs: the run of failures of requests of one kind going on, if one is: since its first, until
/// one of the kind is answered (TASK-314).
#[derive(Default)]
struct Failing {
    /// Whether one is going on, which an answer looks at alone.
    on: std::sync::atomic::AtomicBool,
    run: std::sync::Mutex<Option<Run>>,
}

struct Run {
    began: tokio::time::Instant,
    failed: u64,
    /// When it was last logged, and how many had failed then.
    logged: tokio::time::Instant,
    failed_logged: u64,
}

impl Failing {
    /// A request of `request`'s kind, of `object`, failed with `error`, which it logs if it is the
    /// first of a run, or the run was last logged [`FAILING_EVERY`] ago.
    fn failed(&self, request: Request, object: &str, error: &object_store::Error) {
        let now = tokio::time::Instant::now();
        let mut run = self.run.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        self.on.store(true, Ordering::Relaxed);
        let kind = request.name();
        let Some(run) = run.as_mut() else {
            *run = Some(Run { began: now, failed: 1, logged: now, failed_logged: 1 });
            let error = cause(error);
            log::warn!(
                kind, object, error:%;
                "object storage: {kind} requests failing, retried until they answer: {object}: \
                 {error}"
            );
            return;
        };
        run.failed += 1;
        if now.duration_since(run.logged) >= FAILING_EVERY {
            let (failed, since) = (run.failed, now.duration_since(run.began).as_secs());
            let lately = run.failed - run.failed_logged;
            (run.logged, run.failed_logged) = (now, run.failed);
            let error = cause(error);
            log::warn!(
                kind, object, failed, error:%;
                "object storage: {kind} requests still failing: {failed} failed in {since} s, \
                 {lately} since last said; the latest, {object}: {error}"
            );
        }
    }

    /// A request of `request`'s kind was answered: a run of failures going on ends, which it logs.
    fn answered(&self, request: Request) {
        if !self.on.load(Ordering::Relaxed) {
            return;
        }
        let mut run = self.run.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        self.on.store(false, Ordering::Relaxed);
        if let Some(run) = run.take() {
            let (kind, failed) = (request.name(), run.failed);
            let since = run.began.elapsed().as_secs();
            log::info!(
                kind, failed;
                "object storage: {kind} requests answered again, {failed} having failed in \
                 {since} s"
            );
        }
    }
}

/// cladefs: what `error` says, and each error it says it came from that it does not say already: a
/// file bucket's full disk is an `io::Error` behind `object_store`'s own, which may not say it.
fn cause(error: &object_store::Error) -> String {
    let mut said = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(error) = source {
        let says = error.to_string();
        if !said.contains(&says) {
            said = format!("{said}: {says}");
        }
        source = error.source();
    }
    said
}

/// What a bucket did so far: its requests by kind and how long they took, the bytes it uploaded by
/// kind of object, and the bytes it downloaded; cladefs: and the requests that failed, by kind.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub requests: [u64; Request::ALL.len()],
    pub request_ns: [u64; Request::ALL.len()],
    pub uploaded: [u64; Upload::ALL.len()],
    pub downloaded: u64,
    pub failures: [u64; Request::ALL.len()],
}

impl Stats {
    pub fn requests(&self, request: Request) -> u64 {
        self.requests[request as usize]
    }

    /// cladefs: the requests of kind `request` that failed, unanswered, each retried (TASK-314):
    /// not those the protocol answers (no object, one there, another version), which are answers.
    pub fn failures(&self, request: Request) -> u64 {
        self.failures[request as usize]
    }

    /// The nanoseconds the requests of kind `request` took in all, each from its start to its
    /// answer (TASK-165.11): what waiting for the bucket costs, by kind.
    pub fn request_ns(&self, request: Request) -> u64 {
        self.request_ns[request as usize]
    }

    pub fn uploaded(&self, upload: Upload) -> u64 {
        self.uploaded[upload as usize]
    }

    /// The stats as one line, which `scripts/bench-report.py` reads from the extent daemon's log:
    /// `object-store:`, then `NAME=COUNT` for each kind of request, `up-NAME=BYTES` for each kind
    /// of object, `down=BYTES`, `NAME-ns=NANOSECONDS` for each kind of request, and cladefs's
    /// `failed-NAME=COUNT` for each kind of request (TASK-314).
    pub fn line(&self) -> String {
        let mut line = "object-store:".to_owned();
        for request in Request::ALL {
            line += &format!(" {}={}", request.name(), self.requests(request));
        }
        for upload in Upload::ALL {
            line += &format!(" up-{}={}", upload.name(), self.uploaded(upload));
        }
        line += &format!(" down={}", self.downloaded);
        for request in Request::ALL {
            line += &format!(" {}-ns={}", request.name(), self.request_ns(request));
        }
        for request in Request::ALL {
            line += &format!(" failed-{}={}", request.name(), self.failures(request));
        }
        line
    }
}

impl Counts {
    /// Counts a request of kind `request`, made now, for the object at `path` (or the objects
    /// under it), and, once what it returns is dropped, as the request is answered, how long it
    /// took. cladefs: its span starts, a child of what made the request (TASK-258).
    fn request<'a>(&'a self, request: Request, path: Option<&'a Path>) -> Timed<'a> {
        self.requests[request as usize].fetch_add(1, Ordering::Relaxed);
        let span = crate::trace::Span::start(request.span_name());
        if let Some(path) = path {
            span.set_with(crate::trace::OBJECT_KEY, || path.to_string());
        }
        Timed { counts: self, request, started: std::time::Instant::now(), span, path }
    }
}

/// A request counted ([`Counts::request`]), whose time is counted as this drops; cladefs: and its
/// span ended, which [`Timed::answered`] tells how it went (TASK-258).
struct Timed<'a> {
    counts: &'a Counts,
    request: Request,
    started: std::time::Instant,
    span: crate::trace::Span,
    /// cladefs: its object, or the objects under it, as [`Timed::answered`] logs it.
    path: Option<&'a Path>,
}

impl Timed<'_> {
    /// `answered`, the request's answer, as cells take it ([`error`]), said on its span: its
    /// bytes, as `bytes` counts those of what it answered, or its error. cladefs: and logged, at
    /// debug, with its kind, object, time, and bytes or error (TASK-260): in the extent daemon,
    /// its log gives the record the guest request the work is for, if any (`cladefs::request`). A
    /// failure, unanswered, is counted and logged as [`Failing`] says, with the backend's error
    /// (TASK-314); an answer ends a run of them.
    fn answered<T>(
        &self,
        answered: object_store::Result<T>,
        bytes: impl Fn(&T) -> usize,
    ) -> Result<T, Error> {
        let kind = self.request.name();
        let object = || self.path.map_or_else(String::new, |path| path.to_string());
        let answered = answered.map_err(|failure| {
            let answer = error(&failure);
            if answer == Error::Unavailable {
                self.counts.failures[self.request as usize].fetch_add(1, Ordering::Relaxed);
                if self.request.takes_turns() {
                    self.counts.turns[self.request as usize].failed();
                }
                self.counts.failing[self.request as usize].failed(
                    self.request,
                    &object(),
                    &failure,
                );
            }
            answer
        });
        if !matches!(answered, Err(Error::Unavailable)) {
            self.counts.failing[self.request as usize].answered(self.request);
            if self.request.takes_turns() {
                self.counts.turns[self.request as usize].answered();
            }
        }
        let logged = log::log_enabled!(log::Level::Debug).then(object);
        let ms = || self.started.elapsed().as_secs_f64() * 1e3;
        match &answered {
            Ok(answer) => {
                let bytes = bytes(answer) as u64;
                self.span.set_count(crate::trace::BYTES, bytes);
                if let Some(object) = &logged {
                    let ms = ms();
                    log::debug!(kind, object:%, ms, bytes; "object storage: {kind} {object}");
                }
            }
            Err(error) => {
                self.span.fail(error);
                if let Some(object) = &logged {
                    let ms = ms();
                    log::debug!(
                        kind, object:%, ms, error:%; "object storage: {kind} {object}: {error}"
                    );
                }
            }
        }
        answered
    }
}

impl Drop for Timed<'_> {
    fn drop(&mut self) {
        let ns = u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.counts.request_ns[self.request as usize].fetch_add(ns, Ordering::Relaxed);
    }
}
impl ObjectBucket {
    /// The objects of `backend` under `prefix`, a path of segments separated by `/`, or empty.
    /// cladefs: it lists a level of keys whole, as files and memory do, cutting pages out of it.
    pub fn new(backend: Arc<dyn ObjectStore>, prefix: &str) -> Self {
        Self::paged(backend, None, prefix)
    }

    /// cladefs: the objects of `backend` under `prefix`, a level of which `pages`, the same
    /// backend, lists a page a request, if given (TASK-395.5): S3's, GCS's and Azure's.
    pub fn paged(
        backend: Arc<dyn ObjectStore>,
        pages: Option<Arc<dyn PaginatedListStore>>,
        prefix: &str,
    ) -> Self {
        let prefix = prefix.trim_matches('/').to_owned();
        let (counts, honoured, decoded) = Default::default();
        let (marks, objectids) = (crate::marks::Watch::new(), crate::objectid::Allocator::new());
        let (rewrites, settings) = Default::default();
        Self {
            backend,
            pages,
            prefix,
            counts,
            honoured,
            decoded,
            marks,
            objectids,
            rewrites,
            settings,
        }
    }

    /// cladefs: the bucket `reached` reaches ([`reach`]), spacing its writes of a key as its
    /// backend needs.
    fn reached(reached: Reached) -> Self {
        let Reached { objects, pages, prefix, rewrite_every } = reached;
        Self::paged(objects, pages, &prefix).rewriting_every(rewrite_every)
    }

    /// cladefs: the same bucket, each write of a key at least `every` after the last of it was
    /// answered, as its backend takes them no faster (TASK-690): GCS's [`GCS_REWRITE_EVERY`].
    pub fn rewriting_every(self, every: std::time::Duration) -> Self {
        Self { rewrites: Rewrites { every, ..Rewrites::default() }, ..self }
    }

    /// cladefs: wraps this bucket in a [`RoutedBucket`] routing other buckets' cells via `resolve`
    /// (board 6023).
    pub fn routed(self, resolve: Arc<dyn Resolve<ObjectBucket>>) -> RoutedBucket<ObjectBucket> {
        RoutedBucket::new(Arc::new(self), resolve)
    }

    /// cladefs: wraps this bucket in an unrouted [`RoutedBucket`].
    pub fn unrouted(self) -> RoutedBucket<ObjectBucket> {
        RoutedBucket::unrouted(Arc::new(self))
    }

    /// cladefs: its settings, as its identity says, read once (TASK-691): the defaults' for a
    /// bucket not formatted (1 head slot, or 10 for GCS where `rewrites.every > 0`, board 6020).
    async fn settings(&self) -> crate::identity::Settings {
        if let Some(settings) = self.settings.get() {
            return *settings;
        }
        let default_settings = if !self.rewrites.every.is_zero() {
            crate::identity::Settings { head_slots: crate::identity::GCS_HEAD_SLOTS }
        } else {
            crate::identity::Settings::default()
        };
        let mut failures = 0;
        let settings = loop {
            match self.object_get(layout::IDENTITY).await {
                Ok(object) => match crate::identity::parse(&object.bytes) {
                    Ok(identity) => break identity.settings,
                    Err(err) => {
                        log::warn!("bucket: reading its identity's settings: {err}");
                        break default_settings;
                    }
                },
                Err(Error::NotFound) => break default_settings,
                Err(_) => {
                    failures += 1;
                    tokio::time::sleep(backoff(failures)).await;
                }
            }
        };
        *self.settings.get_or_init(|| settings)
    }

    /// cladefs: a new object ID of the bucket's, which no other cell of any bucket has, nor will
    /// ([`crate::objectid`], TASK-364, TASK-633.9): refused if the bucket is of another format.
    pub async fn new_objectid(&self) -> Result<u64, crate::Error> {
        self.objectids.next(self).await
    }

    /// Checks that it honours the conditional writes cells rely on ([`probe`]), with an object
    /// of `nonce`'s, unless it found it does before: a host's cells share their bucket, which it
    /// checks once, at their first attach, not at each (doc-7, The head).
    pub async fn probed(&self, nonce: u64) -> Result<(), String> {
        if !self.honoured.load(Ordering::Relaxed) {
            probe(self, nonce).await?;
            self.honoured.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    pub fn stats(&self) -> Stats {
        let load = |count: &AtomicU64| count.load(Ordering::Relaxed);
        Stats {
            requests: self.counts.requests.each_ref().map(load),
            request_ns: self.counts.request_ns.each_ref().map(load),
            uploaded: self.counts.uploaded.each_ref().map(load),
            downloaded: load(&self.counts.downloaded),
            failures: self.counts.failures.each_ref().map(load),
        }
    }

    /// cladefs: reads the newest marks object of the collector's mark-only passes over `cell`, if
    /// it is another than the one it holds (TASK-264, TASK-395.2): a listing of the cell's
    /// `gc/marks/`, then a read of the newest. A bucket does every [`crate::marks::REFRESH`] for
    /// each cell it reads, alongside a read of it; its requests go uncounted
    /// ([`ObjectBucket::stats`]), being no reader's. A request failed, it keeps what it held, until
    /// the next; marks it can't decode, it warns of, holding none.
    pub async fn refresh_marks(&self, cell: layout::CellId) {
        let prefix = self.path(layout::marks_of(cell).trim_end_matches('/'));
        let listed = match self.backend.list(Some(&prefix)).try_collect::<Vec<_>>().await {
            Ok(listed) => listed,
            Err(error) => {
                log::debug!(error:%; "object storage: listing the collector's marks: {error}");
                return;
            }
        };
        let newest = (listed.into_iter().map(|meta| self.meta(meta)))
            .filter(|meta| matches!(layout::kind(&meta.key), Kind::Marks { .. }))
            .max_by(|a, b| a.key.cmp(&b.key));
        let Some(newest) = newest else { return self.marks.hold(cell, None) };
        if self.marks.holds(cell, &newest) {
            return;
        }
        let path = self.path(&newest.key);
        let read =
            async { self.backend.get_opts(&path, GetOptions::default()).await?.bytes().await };
        match read.await.map(|bytes| layout::decode::<layout::Marks>(&bytes)) {
            Ok(Ok(marks)) => self.marks.hold(cell, Some((&newest, marks))),
            Ok(Err(error)) => {
                log::warn!(key = newest.key, error:%; "bucket_cell: the collector's marks: {error}");
                self.marks.hold(cell, None);
            }
            Err(error) => log::debug!(error:%; "object storage: reading {path}: {error}"),
        }
    }

    /// cladefs: reads whether a mark-only pass marks the bucket ([`layout::MARKING`]), a HEAD,
    /// which says whether readers watch the cells' marks at all (TASK-395.4's review). A request
    /// failed, it keeps what it held, until the next.
    pub async fn refresh_marking(&self) {
        let head = GetOptions { head: true, ..GetOptions::default() };
        match self.backend.get_opts(&self.path(layout::MARKING), head).await {
            Ok(_) => self.marks.hold_marking(true),
            Err(object_store::Error::NotFound { .. }) => self.marks.hold_marking(false),
            Err(error) => {
                log::debug!(error:%; "object storage: reading whether the bucket is marked: {error}")
            }
        }
    }

    /// The reading of whether the bucket is marked again, if it is due, and of the marks of the
    /// cell `key` is under, a key or a prefix, if the bucket is marked and they are due
    /// ([`ObjectBucket::refresh_marks`]), which a read goes alongside: nothing, if neither is.
    fn marks_due(
        &self,
        key: &str,
    ) -> futures::future::OptionFuture<impl Future<Output = ()> + Send + '_> {
        let marking = self.marks.take_marking_refresh();
        let cell = layout::cell_of(key).filter(|&cell| self.marks.take_refresh(cell));
        (marking || cell.is_some())
            .then(|| async move {
                if marking {
                    self.refresh_marking().await;
                }
                if let Some(cell) = cell {
                    self.refresh_marks(cell).await;
                }
            })
            .into()
    }

    /// The bucket at `url`: `s3://bucket/prefix`, `gs://bucket/prefix`, `az://container/prefix`
    /// (Azure Blob Storage, TASK-46), `memory:///prefix`, or `file:///dir`, the directory `dir` of
    /// the host's filesystem ([`crate::local`], TASK-165.1), configured, credentials included, from
    /// the environment's `AWS_*`, `GOOGLE_*` or `AZURE_*` variables: for Azure, the account
    /// (`AZURE_STORAGE_ACCOUNT_NAME`) and its key (`AZURE_STORAGE_ACCOUNT_KEY`), a SAS token, or a
    /// service principal's or managed identity's, as `object_store`'s `MicrosoftAzureBuilder`
    /// reads them; the emulator, Azurite, with `AZURE_STORAGE_USE_EMULATOR=true`. Blob Storage's
    /// conditional writes (If-None-Match: *, If-Match on ETags) are those doc-7 relies on.
    pub fn from_url(url: &str) -> Result<Self, String> {
        Ok(Self::reached(reach(url, std::env::vars(), &Client::DEFAULT)?))
    }

    /// cladefs: the bucket numbered `number` at `url`, as [`ObjectBucket::from_url`] reaches it,
    /// but configured by the environment's variables for that bucket over the others
    /// ([`numbered_options`], TASK-633.3): its credentials, cloud region or endpoint, which the
    /// registry of buckets never holds.
    pub fn from_url_numbered(url: &str, number: u16) -> Result<Self, String> {
        let options = numbered_options(std::env::vars(), number);
        Ok(Self::reached(reach(url, options, &Client::DEFAULT)?))
    }

    fn path(&self, key: &str) -> Path {
        if self.prefix.is_empty() {
            Path::from(key)
        } else {
            Path::from(format!("{}/{key}", self.prefix))
        }
    }

    /// The path of the objects under the prefix `prefix` of keys, empty or ending with `/`, as
    /// `object_store` lists by them: whole segments, none for every object of the backend.
    fn prefix_path(&self, prefix: &str) -> Option<Path> {
        assert!(prefix.is_empty() || prefix.ends_with('/'), "a prefix of keys: {prefix}");
        match (self.prefix.is_empty(), prefix.trim_end_matches('/')) {
            (true, "") => None,
            (_, prefix) => Some(self.path(prefix)),
        }
    }

    /// The key at `location`, under its prefix.
    fn key<'a>(&self, location: &'a str) -> &'a str {
        match self.prefix.is_empty() {
            true => location,
            false => location
                .strip_prefix(&self.prefix)
                .map_or(location, |key| key.trim_start_matches('/')),
        }
    }

    fn meta(&self, meta: ObjectMeta) -> Meta {
        Meta {
            key: self.key(meta.location.as_ref()).to_owned(),
            version: Version { e_tag: meta.e_tag, version: meta.version },
            size: meta.size,
            modified: meta.last_modified.timestamp_millis().max(0) as u64,
        }
    }
}

/// The object store at `url`, as [`ObjectBucket::from_url`] reaches it, and the prefix the URL
/// names in it: for tests that put themselves between a backend and its objects. cladefs: a
/// degraded one, if the environment says so (`CLADEFS_BUCKET_LATENCY_MS`, `CLADEFS_BUCKET_FAULTS`:
/// [`crate::faults`]): an object store as far as a host's own may be, as the sandbox team's RustFS,
/// 25 ms away, which made Fxfs's commits wait (TASK-61, TASK-62), or one failing for a while
/// (TASK-330.3).
pub fn object_store_at(url: &str) -> Result<(Arc<dyn ObjectStore>, String), String> {
    object_store_with(url, std::env::vars(), &Client::DEFAULT)
}

/// The object store at `url`, configured by `options` as `object_store::parse_url_opts` would
/// configure it, but for its HTTP client: `client`'s, cladefs's own (TASK-205).
pub(crate) fn object_store_with(
    url: &str,
    options: impl IntoIterator<Item = (String, String)>,
    client: &Client,
) -> Result<(Arc<dyn ObjectStore>, String), String> {
    let Reached { objects, prefix, .. } = reach(url, options, client)?;
    Ok((objects, prefix))
}

/// cladefs: a backend as it lists a level of keys a page a request, if it does
/// ([`ObjectBucket::paged`]).
type Pages = Option<Arc<dyn PaginatedListStore>>;

/// cladefs: what a bucket's URL reaches ([`reach`]).
pub(crate) struct Reached {
    pub objects: Arc<dyn ObjectStore>,
    /// The same backend, which lists a level of keys a page a request, if it does: S3's, GCS's and
    /// Azure's; not a degraded one's ([`crate::faults`]), whose pages would go around its faults,
    /// which lists a level whole as files and memory do.
    pub pages: Pages,
    /// The prefix the URL names in it.
    pub prefix: String,
    /// cladefs: how far apart its backend takes writes of one key ([`Rewrites`], TASK-690): GCS's
    /// [`GCS_REWRITE_EVERY`], or a degraded backend's that refuses them faster
    /// ([`crate::faults::Fault::rewrite_every`]).
    pub rewrite_every: std::time::Duration,
}

/// cladefs: opens other buckets by the registry of buckets (TASK-633.3, TASK-633.4): the bucket
/// numbered `number`, as the registry copy in the home bucket's root records it, or, for one that
/// copy lacks, the root bucket's registry, the authority; at its URL, with the options `vars` give
/// it ([`numbered_options`]), refused unless its identity is the one recorded
/// ([`crate::Error::Mismatched`]); `NoBucket` for a number no registry records.
pub struct Registered {
    vars: Vec<(String, String)>,
}

impl Registered {
    /// Buckets opened with `vars`' options: the environment's, for a host
    /// ([`Registered::from_env`]).
    pub fn new(vars: impl IntoIterator<Item = (String, String)>) -> Self {
        Self { vars: vars.into_iter().collect() }
    }

    pub fn from_env() -> Self {
        Self::new(std::env::vars())
    }

    /// The bucket `entry` records, checked to be it.
    pub(crate) async fn reach(
        &self,
        entry: &crate::identity::Entry,
    ) -> Result<ObjectBucket, crate::Error> {
        let number = entry.number;
        let url = entry.details.url.as_deref().ok_or(crate::Error::NoBucket(number))?;
        let options = numbered_options(self.vars.iter().cloned(), number);
        let reached = reach(url, options, &Client::DEFAULT)
            .map_err(|err| crate::Error::Read(format!("bucket {number} at {url}: {err}")))?;
        let bucket = ObjectBucket::reached(reached);
        crate::identity::verified(entry, crate::identity::get(&bucket).await?)?;
        Ok(bucket)
    }

    /// What the registry records of the bucket numbered `number`, as `home` reaches it: its copy
    /// in `home`'s root, or, for a number that copy lacks, the root bucket's registry, the
    /// authority. `NoBucket` for a number neither records.
    pub async fn entry(
        &self,
        home: &impl Bucket,
        number: u16,
    ) -> Result<crate::identity::Entry, crate::Error> {
        use crate::identity::{self, ROOT};
        let copy = identity::registry(home).await?;
        if let Some(entry) = copy.as_ref().and_then(|copy| copy.bucket(number)) {
            return Ok(entry.clone());
        }
        let ours = identity::get(home).await?.ok_or(crate::Error::NoBucket(number))?;
        let root = copy.as_ref().and_then(|copy| copy.bucket(ROOT));
        let root = match (ours.number, root) {
            (ROOT, _) | (_, None) => return Err(crate::Error::NoBucket(number)),
            (_, Some(root)) => self.reach(root).await?,
        };
        let registry = identity::registry(&root).await?;
        let entry = registry.as_ref().and_then(|registry| registry.bucket(number));
        entry.cloned().ok_or(crate::Error::NoBucket(number))
    }
}

/// cladefs: the options of the bucket numbered `number`, from the variables `vars` (TASK-633.3):
/// each `CLADEFSD_BUCKET_<number>_<NAME>` as `NAME`, over the variable `NAME` itself, so that a
/// host reaches buckets of other clouds, regions or accounts than its own's, each with its own
/// credentials (`CLADEFSD_BUCKET_2_AWS_REGION`, `CLADEFSD_BUCKET_3_GOOGLE_SERVICE_ACCOUNT`).
pub fn numbered_options(
    vars: impl IntoIterator<Item = (String, String)>,
    number: u16,
) -> Vec<(String, String)> {
    let prefix = format!("CLADEFSD_BUCKET_{number}_");
    let (own, others): (Vec<_>, Vec<_>) =
        vars.into_iter().partition(|(name, _)| name.starts_with(&prefix));
    let own: Vec<(String, String)> =
        own.into_iter().map(|(name, value)| (name[prefix.len()..].to_owned(), value)).collect();
    let overridden = |name: &str| own.iter().any(|(theirs, _)| theirs.eq_ignore_ascii_case(name));
    others.into_iter().filter(|(name, _)| !overridden(name)).chain(own.iter().cloned()).collect()
}

/// cladefs: the provider of the bucket at `url`, as the registry of buckets records it
/// ([`crate::identity::Details::provider`]): `file`, `memory`, `s3`, `gcs` or `azure`.
pub fn provider_of(url: &str) -> Result<&'static str, String> {
    use object_store::ObjectStoreScheme as Scheme;
    let parsed = url::Url::parse(url).map_err(|error| format!("{url}: {error}"))?;
    if parsed.scheme() == "file" {
        return Ok("file");
    }
    match Scheme::parse(&parsed).map_err(|error| format!("{url}: {error}"))?.0 {
        Scheme::Memory => Ok("memory"),
        Scheme::AmazonS3 => Ok("s3"),
        Scheme::GoogleCloudStorage => Ok("gcs"),
        Scheme::MicrosoftAzure => Ok("azure"),
        other => Err(format!("{url}: no buckets of {other:?}")),
    }
}

/// cladefs: the object store at `url`, as [`object_store_with`] reaches it, and how it lists a
/// level of keys a page at a time, if it does (TASK-395.5).
pub(crate) fn reach(
    url: &str,
    options: impl IntoIterator<Item = (String, String)>,
    client: &Client,
) -> Result<Reached, String> {
    let url = url::Url::parse(url).map_err(|error| format!("{url}: {error}"))?;
    let (backend, pages, path): (Arc<dyn ObjectStore>, _, Path) = match url.scheme() {
        // cladefs: `object_store`'s own refuses conditional updates (TASK-165.1).
        "file" => {
            let dir = url.to_file_path().map_err(|()| format!("{url}: no directory"))?;
            let local =
                crate::local::Local::new(&dir).map_err(|error| format!("{url}: {error}"))?;
            (Arc::new(local), None, Path::default())
        }
        _ => cloud(&url, options, client).map_err(|error| format!("{url}: {error}"))?,
    };
    let mut rewrite_every = match provider_of(url.as_str())? {
        "gcs" => GCS_REWRITE_EVERY,
        _ => std::time::Duration::ZERO,
    };
    let (objects, pages): (Arc<dyn ObjectStore>, _) = match crate::faults::Faults::from_env()? {
        Some(faults) => {
            rewrite_every = rewrite_every.max(faults.now().rewrite_every);
            (Arc::new(crate::faults::Faulty::new(backend, faults)), None)
        }
        None => (backend, pages),
    };
    Ok(Reached { objects, pages, prefix: path.as_ref().to_owned(), rewrite_every })
}

/// The backend of a URL of a cloud's, or of memory, the same backend as it lists a level of keys a
/// page a request, if it does, and the path it names in it, as `object_store::parse_url_opts`
/// makes them, but with `client`.
fn cloud(
    url: &url::Url,
    options: impl IntoIterator<Item = (String, String)>,
    client: &Client,
) -> object_store::Result<(Arc<dyn ObjectStore>, Pages, Path)> {
    use object_store::ObjectStoreScheme as Scheme;
    let (scheme, path) = Scheme::parse(url)?;
    let path = Path::parse(path)?;
    // The client's options first, which the environment's add to.
    macro_rules! built {
        ($builder:ty) => {{
            let builder = (<$builder>::new().with_url(url.as_str()))
                .with_client_options(client.options())
                .with_http_connector(*client);
            let builder = options.into_iter().fold(builder, |builder, (key, value)| {
                match key.to_ascii_lowercase().parse() {
                    Ok(key) => builder.with_config(key, value),
                    Err(_) => builder,
                }
            });
            let built = Arc::new(builder.build()?);
            (built.clone() as Arc<dyn ObjectStore>, Some(built as Arc<dyn PaginatedListStore>))
        }};
    }
    let (backend, pages) = match scheme {
        Scheme::Memory => {
            (Arc::new(object_store::memory::InMemory::new()) as Arc<dyn ObjectStore>, None)
        }
        Scheme::AmazonS3 => built!(object_store::aws::AmazonS3Builder),
        Scheme::GoogleCloudStorage => built!(object_store::gcp::GoogleCloudStorageBuilder),
        Scheme::MicrosoftAzure => built!(object_store::azure::MicrosoftAzureBuilder),
        other => {
            let source = format!("no buckets of {other:?}").into();
            return Err(object_store::Error::Generic { store: "cladefs", source });
        }
    };
    Ok((backend, pages, path))
}

/// What a request's failure means to cells. Failures other than the ones the protocol answers
/// are retried: the backend may be unreachable for a while, and cells wait (doc-7, decision 3).
fn error(error: &object_store::Error) -> Error {
    match error {
        object_store::Error::NotFound { .. } => Error::NotFound,
        object_store::Error::AlreadyExists { .. } => Error::AlreadyExists,
        object_store::Error::Precondition { .. } | object_store::Error::NotModified { .. } => {
            Error::Precondition
        }
        // cladefs: an answer, not a failure to answer (TASK-699).
        object_store::Error::NotImplemented { .. } | object_store::Error::NotSupported { .. } => {
            Error::Unsupported
        }
        _ => Error::Unavailable,
    }
}

/// cladefs: its requests of its own objects: those [`Bucket`]'s, routed, send here, or to another
/// bucket's ([`ObjectBucket::routed`]).
impl ObjectBucket {
    fn object_get(&self, key: &str) -> impl Future<Output = Result<Object, Error>> + Send {
        self.marks.reached(Request::Get, key);
        let refresh = self.marks_due(key);
        let (backend, path) = (self.backend.clone(), self.path(key));
        let get = async move {
            let timed = self.counts.request(Request::Get, Some(&path));
            let got = async {
                let result = backend.get_opts(&path, GetOptions::default()).await?;
                let meta = self.meta(result.meta.clone());
                let bytes = result.bytes().await?;
                self.counts.downloaded.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                Ok(Object { bytes: bytes.to_vec(), meta })
            };
            timed.answered(got.await, |object| object.bytes.len())
        };
        async move { futures::join!(get, refresh).0 }
    }

    fn object_get_range(
        &self,
        key: &str,
        range: ByteRange,
    ) -> impl Future<Output = Result<Vec<u8>, Error>> + Send {
        self.marks.reached(Request::GetRange, key);
        let refresh = self.marks_due(key);
        let (backend, path) = (self.backend.clone(), self.path(key));
        let range = match range {
            ByteRange::Bounded(range) => GetRange::Bounded(range),
            ByteRange::Suffix(n) => GetRange::Suffix(n),
        };
        let get = async move {
            let timed = self.counts.request(Request::GetRange, Some(&path));
            let got = async {
                let options = GetOptions { range: Some(range.clone()), ..GetOptions::default() };
                let result = match backend.get_opts(&path, options).await {
                    // A backend that reads no suffix of an object (Azure Blob Storage, TASK-46),
                    // which it says before any request: the object's size first, then the range
                    // it ends with.
                    Err(object_store::Error::NotSupported { .. })
                        if matches!(range, GetRange::Suffix(_)) =>
                    {
                        let GetRange::Suffix(len) = range else { unreachable!() };
                        let _head = self.counts.request(Request::Head, Some(&path));
                        let head = GetOptions { head: true, ..GetOptions::default() };
                        let size = backend.get_opts(&path, head).await?.meta.size;
                        let range = GetRange::Bounded(size.saturating_sub(len)..size);
                        let options = GetOptions { range: Some(range), ..GetOptions::default() };
                        backend.get_opts(&path, options).await?
                    }
                    result => result?,
                };
                let bytes = result.bytes().await?;
                self.counts.downloaded.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                Ok(bytes.to_vec())
            };
            timed.answered(got.await, Vec::len)
        };
        async move { futures::join!(get, refresh).0 }
    }

    fn object_head(&self, key: &str) -> impl Future<Output = Result<Meta, Error>> + Send {
        self.marks.reached(Request::Head, key);
        let refresh = self.marks_due(key);
        let (backend, path) = (self.backend.clone(), self.path(key));
        let head = async move {
            let timed = self.counts.request(Request::Head, Some(&path));
            let options = GetOptions { head: true, ..GetOptions::default() };
            let result = backend.get_opts(&path, options).await;
            timed.answered(result.map(|result| self.meta(result.meta)), |_| 0)
        };
        async move { futures::join!(head, refresh).0 }
    }

    fn object_list(&self, prefix: &str) -> impl Future<Output = Result<Vec<Meta>, Error>> + Send {
        let backend = self.backend.clone();
        let path = self.prefix_path(prefix);
        let refresh = self.marks_due(prefix);
        let prefix = prefix.to_owned();
        let list = async move {
            // A listing of many objects takes a request for each page of them (1,000 on S3 and
            // GCS), which `object_store` does not say: counted as its pages (TASK-455).
            let timed = self.counts.request(Request::List, path.as_ref());
            let listed = backend.list(path.as_ref()).try_collect::<Vec<ObjectMeta>>().await;
            let listed = timed.answered(listed, |_| 0)?;
            let more = crate::bucket::list_pages(listed.len()) - 1;
            self.counts.requests[Request::List as usize].fetch_add(more, Ordering::Relaxed);
            timed.span.set_count(crate::trace::OBJECTS, listed.len() as u64);
            let mut metas: Vec<Meta> = listed.into_iter().map(|meta| self.meta(meta)).collect();
            metas.sort_by(|a, b| a.key.cmp(&b.key));
            Ok(metas)
        };
        async move {
            let listed = futures::join!(list, refresh).0;
            if let Ok(metas) = &listed {
                self.marks.listed(&prefix, metas);
            }
            listed
        }
    }

    /// cladefs: a page a request where the backend pages ([`ObjectBucket::paged`]); else the level
    /// whole, which the page is cut out of ([`Listing::page`]).
    fn object_list_delimited(
        &self,
        prefix: &str,
        page: Page<'_>,
    ) -> impl Future<Output = Result<Listing, Error>> + Send {
        assert!((1..=LIST_PAGE).contains(&page.max), "a page of 1 to {LIST_PAGE}: {}", page.max);
        let (backend, pages) = (self.backend.clone(), self.pages.clone());
        let path = self.prefix_path(prefix);
        let refresh = self.marks_due(prefix);
        let prefix = prefix.to_owned();
        let (after, max) = (page.after.map(str::to_owned), page.max);
        let list = async move {
            let timed = self.counts.request(Request::ListDelimited, path.as_ref());
            let listed = match &pages {
                Some(pages) => {
                    let options = PaginatedListOptions {
                        delimiter: Some("/".into()),
                        max_keys: Some(max),
                        page_token: after.clone(),
                        ..PaginatedListOptions::default()
                    };
                    // The prefix with its delimiter, which a paged listing adds not.
                    let under = path.as_ref().map(|path| format!("{path}/"));
                    let paged = pages.list_paginated(under.as_deref(), options).await;
                    paged.map(|paged| (paged.result, paged.page_token))
                }
                // A request for each page of the level, which `object_store` does not say.
                None => backend.list_with_delimiter(path.as_ref()).await.map(|whole| (whole, None)),
            };
            let (listed, next) = timed.answered(listed, |_| 0)?;
            let mut prefixes: Vec<String> = (listed.common_prefixes.iter())
                .map(|path| format!("{}/", self.key(path.as_ref())))
                .collect();
            prefixes.sort();
            let mut objects: Vec<Meta> =
                listed.objects.into_iter().map(|meta| self.meta(meta)).collect();
            objects.sort_by(|a, b| a.key.cmp(&b.key));
            let listing = match &pages {
                Some(_) => Listing { prefixes, objects, next },
                None => Listing { prefixes, objects, next: None }
                    .page(Page { after: after.as_deref(), max }),
            };
            timed.span.set_count(crate::trace::OBJECTS, listing.objects.len() as u64);
            timed.span.set_count(crate::trace::PREFIXES, listing.prefixes.len() as u64);
            Ok(listing)
        };
        async move {
            let listed = futures::join!(list, refresh).0;
            if let Ok(listing) = &listed {
                self.marks.listed(&prefix, &listing.objects);
            }
            listed
        }
    }

    fn object_put(
        &self,
        key: &str,
        bytes: bytes::Bytes,
        mode: PutMode,
    ) -> impl Future<Output = Result<Version, Error>> + Send {
        let (backend, path) = (self.backend.clone(), self.path(key));
        let identity = key == layout::IDENTITY;
        let (request, mode) = match mode {
            PutMode::Overwrite => (Request::Overwrite, object_store::PutMode::Overwrite),
            PutMode::Create => (Request::Create, object_store::PutMode::Create),
            PutMode::Update(Version { e_tag, version }) => {
                (Request::Update, object_store::PutMode::Update(UpdateVersion { e_tag, version }))
            }
        };
        let upload = Upload::of(key);
        async move {
            // cladefs: as long after its last write of the key as the backend needs (TASK-690).
            self.rewrites.wait(&path).await;
            // cladefs: its turn, once a write of its kind failed (TASK-382).
            let _turn = match request.takes_turns() {
                true => self.counts.turns[request as usize].turn().await,
                false => None,
            };
            let timed = self.counts.request(request, Some(&path));
            let len = bytes.len() as u64;
            let options = PutOptions { mode, ..PutOptions::default() };
            let identity = identity.then(|| bytes.clone());
            let result = backend.put_opts(&path, PutPayload::from(bytes), options).await;
            self.rewrites.wrote(&path);
            // cladefs: its formatting's, or addition's, settings, as its readers go by them.
            if let (Ok(_), Some(bytes)) = (&result, identity)
                && let Ok(identity) = crate::identity::parse(&bytes)
            {
                let _ = self.settings.set(identity.settings);
            }
            let result = timed.answered(result, |_| len as usize)?;
            self.counts.uploaded[upload as usize].fetch_add(len, Ordering::Relaxed);
            Ok(Version { e_tag: result.e_tag, version: result.version })
        }
    }

    fn object_delete(&self, key: &str) -> impl Future<Output = Result<(), Error>> + Send {
        let (backend, path) = (self.backend.clone(), self.path(key));
        async move {
            let timed = self.counts.request(Request::Delete, Some(&path));
            let deleting = path.clone();
            let mut deleted =
                backend.delete_stream(stream::once(async move { Ok(deleting) }).boxed());
            let deleted = match deleted.next().await {
                Some(Err(object_store::Error::NotFound { .. })) | Some(Ok(_)) | None => Ok(()),
                Some(Err(other)) => Err(other),
            };
            timed.answered(deleted, |_| 0)
        }
    }

    /// cladefs: through `object_store`'s `delete_stream`, which deletes in batches where the
    /// backend has a batch delete (S3's DeleteObjects, Azure's), and several at once where it has
    /// not (TASK-395.5); cladefs's buckets of files take their lock once a batch
    /// ([`crate::local`]).
    fn object_delete_batch(&self, keys: &[&str]) -> impl Future<Output = Result<(), Error>> + Send {
        let backend = self.backend.clone();
        let paths: Vec<object_store::Result<Path>> =
            keys.iter().map(|key| Ok(self.path(key))).collect();
        async move {
            let timed = self.counts.request(Request::DeleteBatch, None);
            let objects = paths.len();
            // Every answer awaited, the first failure kept: the rest took effect, or failed too.
            let deleted = backend
                .delete_stream(stream::iter(paths).boxed())
                .fold(Ok(()), |result, deleted| {
                    std::future::ready(match deleted {
                        Ok(_) | Err(object_store::Error::NotFound { .. }) => result,
                        Err(failure) => result.and(Err(failure)),
                    })
                })
                .await;
            timed.span.set_count(crate::trace::OBJECTS, objects as u64);
            timed.answered(deleted, |_| 0)
        }
    }
}

impl Bucket for ObjectBucket {
    fn decoded(&self) -> Option<&crate::recovery::Decoded> {
        Some(&self.decoded)
    }

    fn get(&self, key: &str) -> impl Future<Output = Result<Object, Error>> + Send {
        self.object_get(key)
    }

    fn get_range(
        &self,
        key: &str,
        range: ByteRange,
    ) -> impl Future<Output = Result<Vec<u8>, Error>> + Send {
        self.object_get_range(key, range)
    }

    fn head(&self, key: &str) -> impl Future<Output = Result<Meta, Error>> + Send {
        self.object_head(key)
    }

    fn list(&self, prefix: &str) -> impl Future<Output = Result<Vec<Meta>, Error>> + Send {
        self.object_list(prefix)
    }

    fn list_delimited(
        &self,
        prefix: &str,
        page: Page<'_>,
    ) -> impl Future<Output = Result<Listing, Error>> + Send {
        self.object_list_delimited(prefix, page)
    }

    fn put(
        &self,
        key: &str,
        bytes: crate::Bytes,
        mode: PutMode,
    ) -> impl Future<Output = Result<Version, Error>> + Send {
        self.object_put(key, bytes, mode)
    }

    async fn head_slots(&self, _cell: layout::CellId) -> u64 {
        self.settings().await.head_slots
    }

    fn default_head_slots(&self) -> u64 {
        if !self.rewrites.every.is_zero() {
            crate::identity::GCS_HEAD_SLOTS
        } else {
            crate::identity::DEFAULT_HEAD_SLOTS
        }
    }

    async fn rewritable(&self, key: &str) {
        self.rewrites.wait(&self.path(key)).await
    }

    fn delete(&self, key: &str) -> impl Future<Output = Result<(), Error>> + Send {
        self.object_delete(key)
    }

    fn delete_batch(&self, keys: &[&str]) -> impl Future<Output = Result<(), Error>> + Send {
        self.object_delete_batch(keys)
    }

    fn now(&self) -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_millis() as u64)
    }

    fn pause(&self, failures: u32) -> impl Future<Output = ()> + Send {
        tokio::time::sleep(backoff(failures))
    }
}

/// How long a request that failed `failures` times in a row waits before it goes again: from 50 ms,
/// doubling, to 5 s.
fn backoff(failures: u32) -> std::time::Duration {
    std::time::Duration::from_millis((50u64 << failures.saturating_sub(1).min(7)).min(5_000))
}

/// Checks that `bucket` honours the conditional writes cells rely on, with an object of its
/// own at `probe/<nonce>`, which it removes: a create where an object is fails, so does an update
/// of a version that is not the object's, and one of the object's version succeeds. cladefs: a step
/// failing as a bucket out of reach does, `Unavailable`, says nothing of what the backend honours:
/// the probe begins again, with an object of its own anew, as a conditional write that failed so
/// may or may not have landed, backing off as the bucket's requests do (TASK-699).
pub async fn probe(bucket: &impl Bucket, nonce: u64) -> Result<(), String> {
    let mut failures = 0;
    loop {
        match probe_once(bucket, nonce.wrapping_add(failures.into())).await {
            Probed::Answered(answer) => return answer,
            Probed::Unavailable(key) => {
                failures += 1;
                log::info!("object storage: probing conditional writes: out of reach, again");
                // Best effort: what is left of a probe is its own object, which no cell names.
                let _ = bucket.delete(&key).await;
                tokio::time::sleep(backoff(failures)).await;
            }
        }
    }
}

/// What a probe of conditional writes found ([`probe`]): an answer, or the bucket out of reach at
/// a step, its object's key.
enum Probed {
    Answered(Result<(), String>),
    Unavailable(String),
}

/// A probe of conditional writes, once, as [`probe`] makes it.
async fn probe_once(bucket: &impl Bucket, nonce: u64) -> Probed {
    let key = format!("probe/{}", base32_id::Padded(nonce));
    let refused = |what: &str, result: &dyn std::fmt::Debug| {
        Probed::Answered(Err(format!(
            "the store does not honour conditional writes: {what} gave {result:?}"
        )))
    };
    let unavailable = |result: &Result<Version, Error>| result == &Err(Error::Unavailable);
    let created = bucket.put(&key, bytes::Bytes::from_static(b"probe"), PutMode::Create).await;
    if unavailable(&created) {
        return Probed::Unavailable(key);
    }
    let version = match created {
        Ok(version) => version,
        Err(error) => return Probed::Answered(Err(format!("creating {key}: {error}"))),
    };
    let again = bucket.put(&key, bytes::Bytes::from_static(b"again"), PutMode::Create).await;
    if unavailable(&again) {
        return Probed::Unavailable(key);
    }
    if again != Err(Error::AlreadyExists) {
        return refused("a create where an object is", &again);
    }
    // No backend makes this ETag; GCS's generation 0 is no object's.
    let other =
        Version { e_tag: Some("\"cladefs-probe\"".to_owned()), version: Some("0".to_owned()) };
    let stale = bucket.put(&key, bytes::Bytes::from_static(b"stale"), PutMode::Update(other)).await;
    if unavailable(&stale) {
        return Probed::Unavailable(key);
    }
    if stale != Err(Error::Precondition) {
        return refused("an update of another version", &stale);
    }
    let updated =
        bucket.put(&key, bytes::Bytes::from_static(b"updated"), PutMode::Update(version)).await;
    if unavailable(&updated) {
        return Probed::Unavailable(key);
    }
    if updated.is_err() {
        return refused("an update of the object's version", &updated);
    }
    match bucket.delete(&key).await {
        Err(Error::Unavailable) => Probed::Unavailable(key),
        removed => Probed::Answered(removed.map_err(|error| format!("removing {key}: {error}"))),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::layout::test_cell;

    /// TASK-633.8: a version of a cell of another bucket forked into this one, mirrored here: its
    /// hold made in the parent's bucket, its objects copied, its marker made, its child's reference
    /// moved here (retired, marked, in the parent's bucket), its pin dropped; begun again, the
    /// mirror copies nothing more; a version the head does not name, given up, its hold and pin
    /// dropped. The child then recovers and reads what it inherited with the parent's bucket's
    /// objects of it gone. This bucket's collector keeps the copy while the parent's cell lives,
    /// then while the child refers to it, and deletes it, the hold with it, once neither does.
    #[test]
    fn versions_forked_into_another_bucket_are_mirrored_there() {
        use crate::collector::{Collector, Options};
        use crate::mirror::{Mirrored, mirror};
        use crate::testing::{VERSION, fork_version_given, ring_of};
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let two = Two::new(dir.path()).await;
            let bucket = two.routed();
            let parent = test_cell(2 << 48 | 1);
            let (at, epoch, extent) = Two::versioned(&bucket, parent).await;
            let origin = Origin { parent, at, epoch };
            let child = test_cell(1 << 48 | 2);
            fork_version_given(&*bucket, child, VERSION, origin, 7).await.unwrap();
            assert_eq!(two.refers(origin, child).await, (true, false, false));

            let own = bucket.own();
            let mirrored = mirror(&*bucket, &*own, 1, parent, at).await.unwrap();
            // The checkpoint at the version, and the segment holding its extent.
            assert_eq!(mirrored, Mirrored::Done { copied: 2, referred: 1, retired: 1 });
            let (hold, marker) = (layout::hold(parent, at, 1), layout::mirror(parent, at));
            assert!(two.other.object_get(&hold).await.is_ok(), "no hold");
            assert!(two.home.object_get(&marker).await.is_ok(), "no marker");
            assert!(two.home.object_get(&layout::pin(parent, at)).await.is_err(), "the pin stands");
            assert_eq!(two.refers(origin, child).await, (false, true, true));
            let again = mirror(&*bucket, &*own, 1, parent, at).await.unwrap();
            assert_eq!(again, Mirrored::Done { copied: 0, referred: 0, retired: 0 });
            // Its hold deleted, as B's collector deletes it with the copy, the marker read before:
            // a mirror begun again, finding the marker, writes none (`mirrorHoldsUnmarked`).
            two.other.object_delete(&hold).await.unwrap();
            let again = mirror(&*bucket, &*own, 1, parent, at).await.unwrap();
            assert_eq!(again, Mirrored::Done { copied: 0, referred: 0, retired: 0 });
            assert!(two.other.object_get(&hold).await.is_err(), "a hold written again");
            two.other
                .object_put(
                    &hold,
                    layout::encode(&layout::Reference::of(0)).into(),
                    PutMode::Overwrite,
                )
                .await
                .unwrap();
            let unnamed = at + 7;
            let dropped = mirror(&*bucket, &*own, 1, parent, unnamed).await.unwrap();
            assert_eq!(dropped, Mirrored::Unnamed);
            assert!(two.other.object_get(&layout::hold(parent, unnamed, 1)).await.is_err());
            assert!(two.home.object_get(&layout::pin(parent, unnamed)).await.is_err());
            assert!(two.home.object_get(&layout::mirror(parent, unnamed)).await.is_err());

            let listed = two.other.object_list(&parent.prefix()).await.unwrap();
            for meta in listed.into_iter().filter(|meta| crate::peer::served(&meta.key)) {
                two.other.object_delete(&meta.key).await.unwrap();
            }
            let fresh = two.routed();
            let inherited = recovery::recover(&*fresh, child, Some(origin)).await.unwrap().state;
            let ring = ring_of(&*fresh, child).await.unwrap();
            let read = crate::fetch::read(&fresh, &ring, extent, &inherited.extents[&extent]);
            assert_eq!(read.await.unwrap(), b"versioned");

            let (pass, fresh_own) =
                (Collector::new(Options { compact_below: 0.5, grace: 0 }), fresh.own());
            let report = pass.collect_copy(&*fresh, &*fresh_own, 1, parent).await.unwrap();
            assert_eq!(report.deleted, 0, "the parent's cell alive");
            lifecycle::delete(&*fresh, parent, 1, u64::MAX).await.unwrap();
            let report = pass.collect_copy(&*fresh, &*fresh_own, 1, parent).await.unwrap();
            assert_eq!((report.deleted, report.retired), (0, 0), "the child referring here");
            lifecycle::delete(&*fresh, child, 1, u64::MAX).await.unwrap();
            let report = pass.collect_copy(&*fresh, &*fresh_own, 1, parent).await.unwrap();
            assert_eq!(
                (report.deleted, report.retired),
                (0, 0),
                "the child's cell not passed over"
            );
            // The child's own passes: its cell dead, its reference retired here.
            for holder in [8, 9] {
                let options = Options { compact_below: 0.5, grace: 0 };
                crate::collector::collect_cell(&*fresh, child, holder, &options).await.unwrap();
            }
            let report = pass.collect_copy(&*fresh, &*fresh_own, 1, parent).await.unwrap();
            // Its copy and its marker.
            assert_eq!((report.deleted, report.retired), (3, 0), "nothing needing it");
            assert_eq!(two.home.object_list(&parent.prefix()).await.unwrap(), [], "left here");
            assert!(two.other.object_get(&hold).await.is_err(), "the hold stands");
        });
    }

    /// TASK-633.8: two buckets in files under `dir`: `home`, numbered 1, a host's, and `other`,
    /// numbered 2, its parent cells', each raw.
    struct Two {
        home_url: String,
        home: ObjectBucket,
        other: ObjectBucket,
    }

    /// cladefs: `cell`'s head in `bucket`'s own twin made `head`, over what is there, as a test's
    /// writer leaves it (TASK-691): every slot, each a copy of it newer than any there. Each slot's
    /// key and bytes.
    async fn put_head(bucket: &ObjectBucket, cell: layout::CellId, head: layout::Head) {
        for (key, bytes) in head_over(bucket, cell, head).await {
            bucket.object_put(&key, bytes.into(), PutMode::Overwrite).await.unwrap();
        }
    }

    /// cladefs: what [`put_head`] writes: each slot's key and bytes.
    async fn head_over(
        bucket: &ObjectBucket,
        cell: layout::CellId,
        head: layout::Head,
    ) -> Vec<(String, Vec<u8>)> {
        let after = match crate::head::get(bucket, cell).await {
            Ok(Some(found)) => found.swaps + 1,
            _ => 0,
        };
        (after..after + crate::identity::DEFAULT_HEAD_SLOTS)
            .map(|swaps| {
                let key = layout::head(cell, swaps % crate::identity::DEFAULT_HEAD_SLOTS);
                (key, layout::encode(&layout::Head { swaps, ..head.clone() }))
            })
            .collect()
    }

    /// cladefs: every slot's key of `cell`'s head (TASK-691).
    fn heads_of(cell: layout::CellId) -> Vec<String> {
        let mut keys: Vec<String> =
            (0..crate::identity::DEFAULT_HEAD_SLOTS).map(|slot| layout::head(cell, slot)).collect();
        keys.sort();
        keys
    }

    impl Two {
        async fn new(dir: &std::path::Path) -> Self {
            use crate::identity::{self, Details};
            let url = |name: &str| {
                std::fs::create_dir(dir.join(name)).unwrap();
                format!("file://{}", dir.join(name).display())
            };
            let (home_url, other_url) = (url("home"), url("other"));
            let (home, other) = (
                ObjectBucket::from_url(&home_url).unwrap(),
                ObjectBucket::from_url(&other_url).unwrap(),
            );
            identity::format(&home, &Details::default(), crate::identity::Asked::default())
                .await
                .unwrap();
            let details = Details { url: Some(other_url), ..Details::default() };
            identity::add(
                &home,
                &Details::default(),
                &other,
                details,
                crate::identity::Asked::default(),
            )
            .await
            .unwrap();
            Self { home_url, home, other }
        }

        /// A host's bucket, `home`, its requests of `other`'s cells sent there.
        fn routed(&self) -> Arc<RoutedBucket> {
            let resolve = Arc::new(Registered::new(Vec::<(String, String)>::new()));
            Arc::new(ObjectBucket::from_url(&self.home_url).unwrap().routed(resolve))
        }

        /// `parent`, a cell of `other`, made, with a version: where it is, the epoch of the
        /// checkpoint there, and the extent it holds, `versioned`.
        async fn versioned(
            bucket: &Arc<RoutedBucket>,
            parent: layout::CellId,
        ) -> (u64, u64, ExtentId) {
            lifecycle::create(&**bucket, parent, 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), parent, 10, 0).await.unwrap();
            let extent = ExtentId::new(parent.id, writer.chain().next_sequence());
            let extents = vec![(extent, b"versioned".to_vec())];
            let at = crate::testing::versioned_by(&writer, extents, crate::testing::VERSION).await;
            let head = crate::head::get(&**bucket, parent).await.unwrap().unwrap();
            (at, head.epoch_of(at).unwrap(), extent)
        }

        /// Whether the reference of the fork of `origin` to `child` is in `other`, in `home`, and
        /// its retirement's mark in `other`.
        async fn refers(&self, origin: Origin, child: layout::CellId) -> (bool, bool, bool) {
            let Origin { parent, at, .. } = origin;
            let reference = layout::reference(parent, at, child);
            (
                self.other.object_get(&reference).await.is_ok(),
                self.home.object_get(&reference).await.is_ok(),
                self.other.object_get(&layout::retired(parent, at, child)).await.is_ok(),
            )
        }
    }

    /// TASK-633.8: a bucket, `own`, its clock `ahead` of its own, a mirror's pin taken over by
    /// another mirror as its `taken_at`th conditional write lands, if any, and the keys it deletes
    /// noted, in order, in `deleted`.
    struct Watched<B: Bucket = ObjectBucket> {
        own: Arc<B>,
        ahead: u64,
        taken_at: Option<usize>,
        updates: Arc<std::sync::atomic::AtomicUsize>,
        deleted: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl<B: Bucket> Watched<B> {
        fn new(own: Arc<B>, ahead: u64, taken_at: Option<usize>) -> Self {
            let (updates, deleted) = Default::default();
            Self { own, ahead, taken_at, updates, deleted }
        }
    }

    impl<B: Bucket> Bucket for Watched<B> {
        async fn get(&self, key: &str) -> Result<Object, Error> {
            self.own.get(key).await
        }

        async fn get_range(&self, key: &str, range: ByteRange) -> Result<Vec<u8>, Error> {
            self.own.get_range(key, range).await
        }

        async fn head(&self, key: &str) -> Result<Meta, Error> {
            self.own.head(key).await
        }

        async fn list(&self, prefix: &str) -> Result<Vec<Meta>, Error> {
            self.own.list(prefix).await
        }

        async fn list_delimited(&self, prefix: &str, page: Page<'_>) -> Result<Listing, Error> {
            self.own.list_delimited(prefix, page).await
        }

        async fn put(
            &self,
            key: &str,
            bytes: crate::Bytes,
            mode: PutMode,
        ) -> Result<Version, Error> {
            let pin = matches!(layout::kind(key), layout::Kind::Pin { .. });
            if pin && matches!(mode, PutMode::Update(_)) {
                let updates = self.updates.fetch_add(1, Ordering::Relaxed) + 1;
                if self.taken_at == Some(updates) {
                    let another = layout::encode(&layout::Reference::of(u64::MAX));
                    self.own.put(key, another.into(), PutMode::Overwrite).await?;
                }
            }
            self.own.put(key, bytes, mode).await
        }

        async fn delete(&self, key: &str) -> Result<(), Error> {
            self.deleted.lock().unwrap().push(key.to_owned());
            self.own.delete(key).await
        }

        fn now(&self) -> u64 {
            self.own.now() + self.ahead
        }
    }

    /// TASK-633.8: a version cut after the parent's bucket's collector compacted a segment holding
    /// one of its extents, mirrored: what its state names of it, along the extent's moves, copied
    /// with the moves, so that the child reads the extent here, the parent's bucket's objects gone.
    #[test]
    fn versions_cut_after_compactions_are_mirrored_with_their_moves() {
        use crate::batch::Commit;
        use crate::collector::{Options, collect_cell};
        use crate::mirror::{Mirrored, mirror};
        use crate::testing::{VERSION, fork_version_given, ring_of, versioned_by};
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let two = Two::new(dir.path()).await;
            let bucket = two.routed();
            let parent = test_cell(2 << 48 | 1);
            lifecycle::create(&*bucket, parent, 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), parent, 10, 0).await.unwrap();
            let first = writer.chain().next_sequence();
            let (small, large) =
                (ExtentId::new(parent.id, first), ExtentId::new(parent.id, first + 1));
            let large_bytes: Vec<u8> = (0..20_000u32).map(|i| (i * 7 % 251) as u8).collect();
            let extents = vec![(small, b"small".to_vec()), (large, large_bytes)];
            async fn commit(writer: &Writer<RoutedBucket>, commit: Commit) {
                let objects = crate::batch::SegmentBuilder::from(commit).close(&mut writer.chain());
                let segment =
                    writer.add_segment(objects.frame(writer.keys().encrypting())).unwrap();
                writer.upload(segment).await.unwrap();
                writer.acknowledge().await.unwrap();
            }
            commit(&writer, Commit { tag: 1, extents, ..Commit::default() }).await;
            let segment = writer.location(small).unwrap().key;
            commit(&writer, Commit { tag: 2, frees: vec![large], ..Commit::default() }).await;
            writer.checkpoint().await.unwrap();
            let compacting = Options { compact_below: 0.5, grace: 0 };
            for holder in [8, 9] {
                collect_cell(&*bucket, parent, holder, &compacting).await.unwrap();
            }
            let moves = two.other.object_list(&layout::moves_of(&segment).unwrap()).await.unwrap();
            assert_eq!(moves.len(), 1, "{segment} not compacted");

            let later = ExtentId::new(parent.id, writer.chain().next_sequence());
            let at = versioned_by(&writer, vec![(later, b"later".to_vec())], VERSION).await;
            let origin = Origin { parent, at, epoch: writer.epoch() };
            let child = test_cell(1 << 48 | 2);
            fork_version_given(&*bucket, child, VERSION, origin, 7).await.unwrap();
            let mirrored = mirror(&*bucket, &*bucket.own(), 1, parent, at).await.unwrap();
            assert!(matches!(mirrored, Mirrored::Done { referred: 1, .. }), "{mirrored:?}");

            let listed = two.other.object_list(&parent.prefix()).await.unwrap();
            for meta in listed.into_iter().filter(|meta| crate::peer::served(&meta.key)) {
                two.other.object_delete(&meta.key).await.unwrap();
            }
            let fresh = two.routed();
            let inherited = recovery::recover(&*fresh, child, Some(origin)).await.unwrap().state;
            let ring = ring_of(&*fresh, child).await.unwrap();
            for (extent, bytes) in [(small, &b"small"[..]), (later, &b"later"[..])] {
                let read = crate::fetch::read(&fresh, &ring, extent, &inherited.extents[&extent]);
                assert_eq!(read.await.unwrap(), bytes);
            }
        });
    }

    /// TASK-633.8: a mirror's hold keeps, in the parent's bucket, what the version inherits from
    /// other cells there, which the mirror does not copy: the parent, itself a fork of a cell
    /// there, mirrored, then both deleted and their bucket's collector passing over them, the
    /// child here reads what the version inherited still (`mirrorHolds`).
    #[test]
    fn holds_keep_what_mirrored_versions_inherit_from_other_cells() {
        use crate::collector::{Options, collect_cell};
        use crate::mirror::{Mirrored, mirror};
        use crate::testing::{VERSION, fork_version_given, ring_of, versioned_by};
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let two = Two::new(dir.path()).await;
            let bucket = two.routed();
            let grand = test_cell(2 << 48 | 1);
            let (grand_at, grand_epoch, inherited) = Two::versioned(&bucket, grand).await;
            let parent = test_cell(2 << 48 | 3);
            let from = Origin { parent: grand, at: grand_at, epoch: grand_epoch };
            fork_version_given(&*bucket, parent, VERSION, from, 5).await.unwrap();
            let writer = Writer::open(bucket.clone(), parent, 10, 0).await.unwrap();
            let own = ExtentId::new(parent.id, writer.chain().next_sequence());
            let at = versioned_by(&writer, vec![(own, b"own".to_vec())], VERSION).await;
            let epoch = writer.epoch();
            drop(writer);
            let origin = Origin { parent, at, epoch };
            let child = test_cell(1 << 48 | 2);
            fork_version_given(&*bucket, child, VERSION, origin, 7).await.unwrap();
            let mirrored = mirror(&*bucket, &*bucket.own(), 1, parent, at).await.unwrap();
            assert!(matches!(mirrored, Mirrored::Done { referred: 1, .. }), "{mirrored:?}");

            for cell in [parent, grand] {
                lifecycle::delete(&*bucket, cell, 1, u64::MAX).await.unwrap();
            }
            let options = Options { compact_below: 0.5, grace: 0 };
            for holder in 8..12 {
                for cell in [parent, grand] {
                    collect_cell(&*bucket, cell, holder, &options).await.unwrap();
                }
            }
            let fresh = two.routed();
            let state = recovery::recover(&*fresh, child, Some(origin)).await.unwrap().state;
            let ring = ring_of(&*fresh, child).await.unwrap();
            for (extent, bytes) in [(inherited, &b"versioned"[..]), (own, &b"own"[..])] {
                let read = crate::fetch::read(&fresh, &ring, extent, &state.extents[&extent]);
                assert_eq!(read.await.unwrap(), bytes);
            }
        });
    }

    /// TASK-633.8 (review round 2): a child here of a mirrored version, deleted while a fork of a
    /// version of its own lives: its reference here stands, as its parent's own does in its bucket
    /// (`references_held`), the copy with it, and the hold; the parent deleted too and every
    /// collector passing, the grandchild reads what it inherited from both.
    #[test]
    fn references_here_hold_while_their_childrens_cells_hold_references() {
        use crate::collector::{Collector, Options, collect_cell};
        use crate::mirror::{Mirrored, mirror};
        use crate::testing::{VERSION, fork_version_given, ring_of, versioned_by};
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let two = Two::new(dir.path()).await;
            let bucket = two.routed();
            let parent = test_cell(2 << 48 | 1);
            let (at, epoch, inherited) = Two::versioned(&bucket, parent).await;
            let origin = Origin { parent, at, epoch };
            let child = test_cell(1 << 48 | 2);
            fork_version_given(&*bucket, child, VERSION, origin, 7).await.unwrap();
            let mirrored = mirror(&*bucket, &*bucket.own(), 1, parent, at).await.unwrap();
            assert!(matches!(mirrored, Mirrored::Done { referred: 1, .. }), "{mirrored:?}");
            let writer = Writer::open(bucket.clone(), child, 10, 0).await.unwrap();
            let own = ExtentId::new(child.id, writer.chain().next_sequence());
            let child_at = versioned_by(&writer, vec![(own, b"own".to_vec())], VERSION).await;
            let child_epoch = writer.epoch();
            drop(writer);
            let (grandchild, forked) = (
                test_cell(1 << 48 | 3),
                Origin { parent: child, at: child_at, epoch: child_epoch },
            );
            fork_version_given(&*bucket, grandchild, VERSION, forked, 9).await.unwrap();

            for cell in [child, parent] {
                lifecycle::delete(&*bucket, cell, 1, u64::MAX).await.unwrap();
            }
            let options = Options { compact_below: 0.5, grace: 0 };
            let copies = Collector::new(options);
            for holder in 8..12 {
                for cell in [child, parent] {
                    collect_cell(&*bucket, cell, holder, &options).await.unwrap();
                }
                copies.collect_copy(&*bucket, &*bucket.own(), 1, parent).await.unwrap();
            }
            let fresh = two.routed();
            let state = recovery::recover(&*fresh, grandchild, Some(forked)).await.unwrap().state;
            let ring = ring_of(&*fresh, grandchild).await.unwrap();
            for (extent, bytes) in [(inherited, &b"versioned"[..]), (own, &b"own"[..])] {
                let read = crate::fetch::read(&fresh, &ring, extent, &state.extents[&extent]);
                assert_eq!(read.await.unwrap(), bytes);
            }
        });
    }

    /// TASK-633.8: a process's mirrors of one version take turns, each taking its own as it is
    /// asked for: the second, asked for before the first runs, waits for it and finds the version
    /// marked; the turns go once done; none is wanted of a cell of this bucket's own.
    #[test]
    fn mirrors_of_a_version_take_turns() {
        use crate::mirror::Mirrored;
        use crate::testing::{VERSION, fork_version_given};
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let two = Two::new(dir.path()).await;
            let bucket = two.routed();
            let parent = test_cell(2 << 48 | 1);
            let (at, epoch, _) = Two::versioned(&bucket, parent).await;
            let child = test_cell(1 << 48 | 2);
            let origin = Origin { parent, at, epoch };
            fork_version_given(&*bucket, child, VERSION, origin, 7).await.unwrap();
            let (first, second) =
                (bucket.mirror_wanted(child, origin), bucket.mirror_wanted(child, origin));
            // The second polled first: it waits for the first's turn, taken as it was asked for.
            let (second, first) = futures::join!(second, first);
            assert!(matches!(first.unwrap(), Some(Mirrored::Done { referred: 1, .. })));
            assert_eq!(second.unwrap(), None, "the second's turn");
            assert!(bucket.mirrors_idle(), "turns left");
            let own = Origin { parent: test_cell(1 << 48 | 9), at: 3, epoch: 1 };
            assert_eq!(bucket.mirror_wanted(child, own).await.unwrap(), None);
        });
    }

    /// TASK-633.8: a mirror failing, as a request does that it can't go on past, lets its hold in
    /// the parent's bucket and its pin go; begun again, it is done.
    #[test]
    fn mirrors_failing_let_their_hold_and_pin_go() {
        use crate::mirror::{Mirrored, mirror};
        use crate::testing::{VERSION, fork_version_given};
        let scenario = fail::FailScenario::setup();
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let two = Two::new(dir.path()).await;
            let bucket = two.routed();
            let parent = test_cell(2 << 48 | 1);
            let (at, epoch, _) = Two::versioned(&bucket, parent).await;
            let child = test_cell(1 << 48 | 2);
            fork_version_given(&*bucket, child, VERSION, Origin { parent, at, epoch }, 7)
                .await
                .unwrap();
            let own = bucket.own();
            for step in ["mirror::checked", "mirror::copied"] {
                fail::cfg(step, "return").unwrap();
                let failed = mirror(&*bucket, &*own, 1, parent, at).await;
                fail::remove(step);
                assert!(failed.is_err(), "{step}: {failed:?}");
                let held = two.other.object_get(&layout::hold(parent, at, 1)).await;
                assert!(held.is_err(), "{step}");
                assert!(two.home.object_get(&layout::pin(parent, at)).await.is_err(), "{step}");
            }
            let mirrored = mirror(&*bucket, &*own, 1, parent, at).await.unwrap();
            assert!(matches!(mirrored, Mirrored::Done { referred: 1, .. }), "{mirrored:?}");
        });
        scenario.teardown();
    }

    /// TASK-633.8: a fork into a bucket refers there once its version's mirror is marked there, in
    /// the parent's bucket before; and one that referred there, the marker made before it read the
    /// parent's head, refers here too, then retires its reference there (`forksRecheck`,
    /// `forksLeaveA`): no mirror's listing need find it.
    #[test]
    fn forks_refer_where_their_versions_are_mirrored() {
        use crate::testing::{VERSION, fork_version};
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let two = Two::new(dir.path()).await;
            let bucket = two.routed();
            let parent = test_cell(2 << 48 | 1);
            let (at, epoch, _) = Two::versioned(&bucket, parent).await;
            let origin = Origin { parent, at, epoch };

            let before = test_cell(1 << 48 | 2);
            fork_version(&*bucket, before, VERSION, origin, 7).await.unwrap();
            assert_eq!(two.refers(origin, before).await, (true, false, false), "unmirrored");

            let (marker, marked) =
                (layout::mirror(parent, at), layout::encode(&layout::Mirror::NOW));
            let put = PutMode::Overwrite;
            two.home.object_put(&marker, marked.clone().into(), put.clone()).await.unwrap();
            let after = test_cell(1 << 48 | 3);
            fork_version(&*bucket, after, VERSION, origin, 7).await.unwrap();
            assert_eq!(two.refers(origin, after).await, (false, true, false), "mirrored");

            // The marker made as the fork referred in the parent's bucket.
            two.home.object_delete(&marker).await.unwrap();
            let scenario = fail::FailScenario::setup();
            let path = dir.path().join("home").join(&marker);
            fail::cfg_callback("version::referred", move || {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, &marked).unwrap();
            })
            .unwrap();
            let raced = test_cell(1 << 48 | 4);
            fork_version(&*bucket, raced, VERSION, origin, 7).await.unwrap();
            fail::remove("version::referred");
            assert_eq!(two.refers(origin, raced).await, (false, true, true), "marked meanwhile");

            // The version dropped after the fork read it named, before it read the head again: its
            // reference in the parent's bucket stays (`forksRereadBeforeLeaving`).
            let found = crate::head::get(&two.other, parent).await.unwrap().unwrap();
            let dropped = layout::Head { versions: Vec::new(), ..found };
            let other = dir.path().join("other");
            let written: Vec<_> = (head_over(&two.other, parent, dropped).await.into_iter())
                .map(|(key, bytes)| (other.join(key), bytes))
                .collect();
            fail::cfg_callback("version::rechecked", move || {
                for (path, bytes) in &written {
                    std::fs::write(path, bytes).unwrap();
                }
            })
            .unwrap();
            two.home.object_delete(&marker).await.unwrap();
            let path = dir.path().join("home").join(&marker);
            let marked = layout::encode(&layout::Mirror::NOW);
            fail::cfg_callback("version::referred", move || {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, &marked).unwrap();
            })
            .unwrap();
            let late = test_cell(1 << 48 | 5);
            fork_version(&*bucket, late, VERSION, origin, 7).await.unwrap();
            fail::remove("version::referred");
            fail::remove("version::rechecked");
            assert_eq!(two.refers(origin, late).await, (true, true, false), "dropped meanwhile");

            scenario.teardown();
        });
    }

    /// TASK-633.8: one mirror of a version holds its pin at a time: another's standing, a mirror
    /// does nothing; one whose pin another takes over, stale, gives up unmarked, that pin left; and
    /// a pin stale, its mirror's host dead, this bucket's collector begins the mirror again, which
    /// takes it over and is done.
    #[test]
    fn mirrors_hold_their_pins_alone() {
        use crate::collector::{Collector, Options};
        use crate::mirror::{Mirrored, PIN_STALE, mirror};
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let two = Two::new(dir.path()).await;
            let bucket = two.routed();
            let parent = test_cell(2 << 48 | 1);
            let (at, epoch, _) = Two::versioned(&bucket, parent).await;
            let origin = Origin { parent, at, epoch };
            let child = test_cell(1 << 48 | 2);
            crate::testing::fork_version(&*bucket, child, crate::testing::VERSION, origin, 7)
                .await
                .unwrap();
            let (own, pin, marker) =
                (bucket.own(), layout::pin(parent, at), layout::mirror(parent, at));
            let another = layout::encode(&layout::Reference::of(u64::MAX));
            let put = PutMode::Overwrite;
            two.home.object_put(&pin, another.clone().into(), put).await.unwrap();

            assert_eq!(mirror(&*bucket, &*own, 1, parent, at).await.unwrap(), Mirrored::UnderWay);
            assert!(two.home.object_get(&marker).await.is_err(), "marked under another's pin");

            // Its pin taken over as it refreshes it before its marker.
            let taken = Watched::new(own.clone(), PIN_STALE + 1, Some(2));
            let gave_up = mirror(&*bucket, &taken, 1, parent, at).await.unwrap();
            assert_eq!(gave_up, Mirrored::GaveUp("its pin, stale, taken over".to_owned()));
            assert!(two.home.object_get(&marker).await.is_err(), "marked, its pin taken over");
            assert_eq!(two.home.object_get(&pin).await.unwrap().bytes, another, "the pin taken");

            let later = Watched::new(own.clone(), PIN_STALE + 1, None);
            let options = Options { compact_below: 0.5, grace: 0 };
            let report = Collector::new(options).collect_copy(&*bucket, &later, 1, parent).await;
            assert_eq!(report.unwrap().deleted, 0);
            assert!(two.home.object_get(&marker).await.is_ok(), "not marked, begun again");
            assert!(two.home.object_get(&pin).await.is_err(), "the pin stands");
            assert_eq!(two.refers(origin, child).await, (false, true, true));

            // Its pin taken over as it lets it go, written again first: the other's left.
            two.home.object_put(&pin, another.clone().into(), PutMode::Overwrite).await.unwrap();
            let taken = Watched::new(own.clone(), PIN_STALE + 1, Some(2));
            let done = mirror(&*bucket, &taken, 1, parent, at).await.unwrap();
            assert_eq!(done, Mirrored::Done { copied: 0, referred: 0, retired: 0 });
            assert_eq!(two.home.object_get(&pin).await.unwrap().bytes, another, "the other's pin");
        });
    }

    /// TASK-633.8: a resolver opening every other bucket as the one at `url`, empty: what a host
    /// asks of a bucket out of reach then finds nothing there, at once, and its counts show it.
    struct Emptied(String);

    impl Resolve for Emptied {
        fn open<'a>(
            &'a self,
            _: &'a ObjectBucket,
            _: u16,
        ) -> futures::future::BoxFuture<'a, Result<Arc<ObjectBucket>, crate::Error>> {
            Box::pin(async move { Ok(Arc::new(ObjectBucket::from_url(&self.0).unwrap())) })
        }
    }

    /// TASK-633.8 (AC #4): a version mirrored here, its children here live with its bucket out of
    /// reach: a host started since, which knows nothing of the version but what this bucket holds,
    /// recovers a child and reads what it inherited, takes it, commits, and forks its own version
    /// into another child, which reads both; it asks the version's bucket nothing but listings of
    /// moves (a mirrored cell's are listed in both buckets, those of the other skipped when it
    /// fails).
    #[test]
    fn children_of_a_mirrored_version_live_with_its_bucket_out_of_reach() {
        use crate::mirror::{Mirrored, mirror};
        use crate::testing::{VERSION, fork_version_given, ring_of, versioned_by};
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let two = Two::new(dir.path()).await;
            let bucket = two.routed();
            let parent = test_cell(2 << 48 | 1);
            let (at, epoch, extent) = Two::versioned(&bucket, parent).await;
            let origin = Origin { parent, at, epoch };
            let child = test_cell(1 << 48 | 2);
            fork_version_given(&*bucket, child, VERSION, origin, 7).await.unwrap();
            let mirrored = mirror(&*bucket, &*bucket.own(), 1, parent, at).await.unwrap();
            assert!(matches!(mirrored, Mirrored::Done { referred: 1, .. }), "{mirrored:?}");

            std::fs::create_dir(dir.path().join("emptied")).unwrap();
            let emptied = Emptied(format!("file://{}", dir.path().join("emptied").display()));
            let home = ObjectBucket::from_url(&two.home_url).unwrap();
            let cut = Arc::new(home.routed(Arc::new(emptied)));
            let inherited = recovery::recover(&*cut, child, Some(origin)).await.unwrap().state;
            let ring = ring_of(&*cut, child).await.unwrap();
            let read = |ring, extent, location| crate::fetch::read(&cut, ring, extent, location);
            assert_eq!(
                read(&ring, extent, &inherited.extents[&extent]).await.unwrap(),
                b"versioned"
            );

            let writer = Writer::open(cut.clone(), child, 10, 0).await.unwrap();
            let own = ExtentId::new(child.id, writer.chain().next_sequence());
            let child_at = versioned_by(&writer, vec![(own, b"own".to_vec())], VERSION).await;
            let child_epoch = writer.epoch();
            drop(writer);
            let (grandchild, forked) = (
                test_cell(1 << 48 | 3),
                Origin { parent: child, at: child_at, epoch: child_epoch },
            );
            fork_version_given(&*cut, grandchild, VERSION, forked, 7).await.unwrap();
            let state = recovery::recover(&*cut, grandchild, Some(forked)).await.unwrap().state;
            let ring = ring_of(&*cut, grandchild).await.unwrap();
            assert_eq!(read(&ring, extent, &state.extents[&extent]).await.unwrap(), b"versioned");
            assert_eq!(read(&ring, own, &state.extents[&own]).await.unwrap(), b"own");

            let there = cut.stats_by_bucket().into_iter().find(|(number, _)| *number == 2);
            let asked: Vec<Request> = there.map_or_else(Vec::new, |(_, stats)| {
                (Request::ALL.into_iter())
                    .filter(|&request| request != Request::List && stats.requests(request) > 0)
                    .collect()
            });
            assert_eq!(asked, [], "asked of the version's bucket, out of reach");
        });
    }

    /// TASK-633.8 (AC #4): a mirror's host dying at each of its steps, then the version's cell
    /// deleted, and both buckets' collectors passing, this one's with the mirror's pin fresh, then
    /// stale (the mirror begun again, or given up, the version gone): nothing the child needs is
    /// gone, wherever it is read. A mirror marked before it died refers the child here, begun again
    /// if it died before moving it, and its hold keeps the version in its cell's bucket; one
    /// begun again after the version went leaves the child's reference there too. One that died
    /// before its marker,
    /// begun again, gives up, the version gone: its copy here goes, its hold with it, and the
    /// child's reference keeps the version there.
    #[test]
    fn mirrors_dying_at_each_step_leave_nothing_needed_gone() {
        use crate::collector::{Collector, Options};
        use crate::mirror::{Mirrored, PIN_STALE, mirror};
        use crate::testing::{VERSION, fork_version, read};
        use futures::executor::block_on;
        // Each step; whether the mirror was marked by then; and whether it retired the child's
        // reference in the parent's bucket before the version went. One marked, begun again after,
        // writes the child's reference here and leaves the one there, reading the head again
        // (`mirrorRereadsBeforeRetiring`). None, the mirror done.
        let steps = [
            ("mirror::pinned", false, false),
            ("mirror::held", false, false),
            ("mirror::checked", false, false),
            ("mirror::copying", false, false),
            ("mirror::copied", false, false),
            ("mirror::marked", true, false),
            ("mirror::moved", true, false),
            ("mirror::referred", true, true),
            ("", true, true),
        ];
        let scenario = fail::FailScenario::setup();
        for (step, marked, retired) in steps {
            let dir = tempfile::tempdir().unwrap();
            let two = block_on(Two::new(dir.path()));
            let bucket = two.routed();
            let parent = test_cell(2 << 48 | 1);
            let (at, epoch, extent) = block_on(Two::versioned(&bucket, parent));
            let origin = Origin { parent, at, epoch };
            let child = test_cell(1 << 48 | 2);
            block_on(fork_version(&*bucket, child, VERSION, origin, 7)).unwrap();

            if !step.is_empty() {
                fail::cfg(step, "panic").unwrap();
            }
            let own = bucket.own();
            let mirroring = || block_on(mirror(&*bucket, &*own, 1, parent, at));
            let died = std::panic::catch_unwind(std::panic::AssertUnwindSafe(mirroring));
            if !step.is_empty() {
                fail::remove(step);
            }
            match died {
                Ok(done) => assert!(step.is_empty(), "{step}: not died, {done:?}"),
                Err(_) => assert!(!step.is_empty(), "died, at no step"),
            }

            block_on(async {
                lifecycle::delete(&*bucket, parent, 1, u64::MAX).await.unwrap();
                let pass = || Collector::new(Options { compact_below: 0.5, grace: 0 });
                pass().collect_copy(&*bucket, &*own, 1, parent).await.unwrap();
                let later = Watched::new(own.clone(), PIN_STALE + 1, None);
                for _ in 0..2 {
                    pass().collect_copy(&*bucket, &later, 1, parent).await.unwrap();
                }
                for holder in [8, 9] {
                    pass().collect_cell(&*bucket, parent, holder).await.unwrap();
                }

                let fresh = two.routed();
                let state = recovery::recover(&*fresh, child, Some(origin)).await.unwrap().state;
                let location = &state.extents[&extent];
                assert_eq!(read(&fresh, extent, location).await.unwrap(), b"versioned", "{step}");
                let checkpoint = layout::checkpoint(parent, at, epoch);
                let there = two.other.object_get(&checkpoint).await.is_ok();
                let here = two.home.object_get(&checkpoint).await.is_ok();
                assert_eq!((there, here), (true, marked), "{step}: where the version is");
                let held = two.other.object_get(&layout::hold(parent, at, 1)).await.is_ok();
                assert_eq!(held, marked, "{step}: the hold");
                let reference = two.refers(origin, child).await;
                assert_eq!((reference.0, reference.1), (!retired, marked), "{step}: references");
                assert!(two.home.object_get(&layout::pin(parent, at)).await.is_err(), "{step}");
            });
            let again = block_on(mirror(&*bucket, &*own, 1, parent, at)).unwrap();
            // The reference left there written here again, and left there still.
            let expected = match marked {
                true => Mirrored::Done { copied: 0, referred: usize::from(!retired), retired: 0 },
                false => Mirrored::Unnamed,
            };
            assert_eq!(again, expected, "{step}: begun again");
            let reference = block_on(two.refers(origin, child));
            assert_eq!(reference.0, !retired, "{step}: begun again, the reference there");
        }
        scenario.teardown();
    }

    /// TASK-633.8: a pass over the copy here of another bucket's cell's versions keeps it whole
    /// while that cell lives, though its head names none of them, a child refers here, or a mirror
    /// is under way (a pin); retires here the references of children gone; and, nothing needing
    /// it, deletes its mirrors' holds in that cell's bucket, then its markers, then its objects,
    /// then the marks of references retired here: nothing of it left here, nothing else touched
    /// there.
    #[test]
    fn copies_of_other_buckets_versions_are_kept_while_needed() {
        use crate::collector::{Collector, Options};
        use crate::layout::{Head, Mirror, Version};
        use crate::record::Made;
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let two = Two::new(dir.path()).await;
            let (routed, (home, other)) = (two.routed(), (&two.home, &two.other));
            let own = routed.own();
            let (parent, child) = (test_cell(2 << 48 | 1), test_cell(1 << 48 | 5));
            let origin = Origin { parent, at: 3, epoch: 1 };
            async fn put(bucket: &ObjectBucket, key: &str, bytes: Vec<u8>) {
                bucket.object_put(key, bytes.into(), PutMode::Overwrite).await.unwrap();
            }
            async fn listed(bucket: &ObjectBucket, cell: layout::CellId) -> Vec<String> {
                let listed = bucket.object_list(&cell.prefix()).await.unwrap();
                listed.into_iter().map(|meta| meta.key).collect()
            }
            let version = Version { id: 9, at: 3, epoch: 1, shared: false };
            let named = Head { versions: vec![version], ..Head::of(Made::Root, 4) };
            put_head(other, parent, named).await;
            // And a version a mirror copied and never marked, its hold left.
            let copied = [
                layout::checkpoint(parent, 3, 1),
                layout::checkpoint(parent, 5, 1),
                layout::segment(parent, 1, 2),
            ];
            let unmarked = layout::hold(parent, 5, 1);
            put(other, &unmarked, layout::encode(&layout::Reference::of(0))).await;
            for key in &copied {
                put(home, key, b"copied".to_vec()).await;
            }
            let (marker, hold) = (layout::mirror(parent, 3), layout::hold(parent, 3, 1));
            put(home, &marker, layout::encode(&Mirror::NOW)).await;
            put(other, &hold, layout::encode(&layout::Reference::of(0))).await;
            let pass = || Collector::new(Options { compact_below: 0.5, grace: 0 });
            let all = listed(home, parent).await;

            put_head(other, parent, Head::of(Made::Root, 4)).await;
            let report = pass().collect_copy(&*routed, &*own, 1, parent).await.unwrap();
            assert_eq!((report.deleted, listed(home, parent).await), (0, all.clone()), "alive");
            let ended = Head { epoch: Head::FINAL, ..Head::of(Made::Root, 4) };
            put_head(other, parent, ended).await;
            put_head(home, child, Head::of(Made::Clone(origin), 4)).await;
            let reference = layout::reference(parent, 3, child);
            put(home, &reference, b"ref".to_vec()).await;
            let report = pass().collect_copy(&*routed, &*own, 1, parent).await.unwrap();
            assert_eq!((report.deleted, report.retired), (0, 0), "referred");
            // Deleted, its cell perhaps holding its own children's references still: the reference
            // stands, until its own pass retires it, its cell dead (`references_held`).
            let deleted = Head { epoch: Head::FINAL, ..Head::of(Made::Clone(origin), 4) };
            put_head(home, child, deleted).await;
            let report = pass().collect_copy(&*routed, &*own, 1, parent).await.unwrap();
            assert_eq!((report.deleted, report.retired), (0, 0), "a deleted child's");
            // Gone, its cell removed: retired here.
            for key in heads_of(child) {
                home.object_delete(&key).await.unwrap();
            }
            put(home, &layout::pin(parent, 3), b"pin".to_vec()).await;
            let report = pass().collect_copy(&*routed, &*own, 1, parent).await.unwrap();
            assert_eq!((report.deleted, report.retired), (0, 1), "pinned");
            assert!(home.object_get(&reference).await.is_err(), "a gone child's reference stands");

            home.object_delete(&layout::pin(parent, 3)).await.unwrap();
            let log = Arc::new(std::sync::Mutex::new(Vec::new()));
            let watched = Watched { deleted: log.clone(), ..Watched::new(routed.clone(), 0, None) };
            let watched_own =
                Watched { deleted: log.clone(), ..Watched::new(own.clone(), 0, None) };
            let report = pass().collect_copy(&watched, &watched_own, 1, parent).await.unwrap();
            // Its copy and its marker.
            assert_eq!(report.deleted, copied.len() + 1);
            assert_eq!(listed(home, parent).await, Vec::<String>::new(), "left here");
            assert_eq!(listed(other, parent).await, heads_of(parent), "touched there");
            let mark = layout::retired(parent, 3, child);
            let mut order = vec![hold, unmarked, marker];
            order.extend(copied);
            order.push(mark);
            assert_eq!(*log.lock().unwrap(), order, "deleted in that order");
        });
    }

    /// TASK-633.8: a collector's sweep of this bucket, given its own bucket, passes over the copy
    /// of another bucket's cell's versions as a copy: a mark-only one marks nothing of it; one
    /// deleting deletes it, nothing needing it, as a collector's pass over every cell does.
    #[test]
    fn sweeps_pass_over_copies_as_copies() {
        use crate::collector::{Collector, Options};
        use crate::layout::{Head, Mirror};
        use crate::record::Made;
        use crate::sweep::{Mode, Sweep};
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let two = Two::new(dir.path()).await;
            let (routed, (home, other)) = (two.routed(), (&two.home, &two.other));
            let own = routed.own();
            let parent = test_cell(2 << 48 | 1);
            let ended = Head { epoch: Head::FINAL, ..Head::of(Made::Root, 4) };
            put_head(other, parent, ended).await;
            let copy = || async {
                for (key, bytes) in [
                    (layout::checkpoint(parent, 3, 1), b"copied".to_vec()),
                    (layout::mirror(parent, 3), layout::encode(&Mirror::NOW)),
                ] {
                    home.object_put(&key, bytes.into(), PutMode::Overwrite).await.unwrap();
                }
            };
            let left = || async { home.object_list(&parent.prefix()).await.unwrap().len() };
            copy().await;
            let options = Options { compact_below: 0.5, grace: 0 };
            let sweep = |mode| Sweep {
                options,
                mode,
                at_once: 1,
                lease: 60_000,
                slowest: 1,
                spawned: false,
                kms: None,
            };
            let marking = sweep(Mode::MarkOnly).run_with_own(&routed, Some(&own), |_| {}).await;
            assert_eq!((marking.report.deleted, left().await), (0, 2), "marked");
            let there = other.object_list(&parent.prefix()).await.unwrap();
            let there: Vec<String> = there.into_iter().map(|meta| meta.key).collect();
            assert_eq!(there, heads_of(parent), "marked there");
            let swept = sweep(Mode::Delete).run_with_own(&routed, Some(&own), |_| {}).await;
            // Its copy and its marker.
            assert_eq!((swept.report.deleted, left().await), (2, 0), "swept");
            copy().await;
            let collected =
                Collector::new(options).collect_with_own(&*routed, Some(&*own), 9).await;
            assert_eq!((collected.unwrap().deleted, left().await), (2, 0), "collected");
        });
    }

    /// TASK-633.4: a bucket routing by the registry sends the requests of a cell of another bucket
    /// there, and keeps those of its own cells, of the cells tests name, and of no cell's: reads,
    /// writes, listings, and each bucket's share of a batch of deletions. A number no registry
    /// records is unavailable to its request, not answered here.
    #[test]
    fn requests_go_to_the_bucket_of_their_cell() {
        use crate::identity::{self, Details};
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let url = |name: &str| {
                std::fs::create_dir(dir.path().join(name)).unwrap();
                format!("file://{}", dir.path().join(name).display())
            };
            let (home_url, other_url) = (url("home"), url("other"));
            let (home, other) = (
                ObjectBucket::from_url(&home_url).unwrap(),
                ObjectBucket::from_url(&other_url).unwrap(),
            );
            identity::format(&home, &Details::default(), crate::identity::Asked::default())
                .await
                .unwrap();
            let details = Details { url: Some(other_url.clone()), ..Details::default() };
            identity::add(
                &home,
                &Details::default(),
                &other,
                details,
                crate::identity::Asked::default(),
            )
            .await
            .unwrap();
            let routed = ObjectBucket::from_url(&home_url)
                .unwrap()
                .routed(Arc::new(Registered::new(Vec::<(String, String)>::new())));

            let theirs = layout::CellId { tenant: 1, id: 2 << 48 | 5 };
            let ours = layout::CellId { tenant: 1, id: 1 << 48 | 5 };
            let named = layout::CellId { tenant: 1, id: 7 };
            for cell in [theirs, ours, named] {
                let key = layout::head(cell, 0);
                routed.put(&key, b"head".to_vec().into(), PutMode::Create).await.unwrap();
                assert_eq!(routed.get(&key).await.map(|object| object.bytes), Ok(b"head".to_vec()));
            }
            async fn at(bucket: &ObjectBucket, cell: layout::CellId) -> bool {
                bucket.object_get(&layout::head(cell, 0)).await.is_ok()
            }
            assert_eq!((at(&other, theirs).await, at(&home, theirs).await), (true, false));
            assert_eq!((at(&home, ours).await, at(&other, ours).await), (true, false));
            assert_eq!((at(&home, named).await, at(&other, named).await), (true, false));
            let listed = routed.list(&theirs.prefix()).await.unwrap();
            assert_eq!(
                listed.iter().map(|meta| meta.key.clone()).collect::<Vec<_>>(),
                [layout::head(theirs, 0)]
            );

            // TASK-633.8: its own bucket, unrouted, takes a key of another bucket's cell here.
            let pin = layout::pin(theirs, 3);
            let resolve = Arc::new(Registered::new(Vec::<(String, String)>::new()));
            let own = ObjectBucket::from_url(&home_url).unwrap().routed(resolve).own().clone();
            own.put(&pin, b"pin".to_vec().into(), PutMode::Create).await.unwrap();
            let (here, there) = (home.object_get(&pin).await, other.object_get(&pin).await);
            assert_eq!((here.is_ok(), there.is_ok()), (true, false));

            let (theirs_key, ours_key) = (layout::head(theirs, 0), layout::head(ours, 0));
            routed.delete_batch(&[&theirs_key, &ours_key]).await.unwrap();
            assert_eq!((at(&other, theirs).await, at(&home, ours).await), (false, false));

            let by_bucket = routed.stats_by_bucket();
            let numbers: Vec<u16> = by_bucket.iter().map(|(number, _)| *number).collect();
            assert_eq!(numbers, [1, 2]);
            // Its identity's, read as it was opened, and the head's.
            assert_eq!(by_bucket[1].1.requests(Request::Get), 2, "bucket 2's reads");
            assert_eq!(by_bucket[1].1.requests(Request::Create), 1, "bucket 2's one write");

            let unrecorded = layout::head(layout::CellId { tenant: 1, id: 9 << 48 | 1 }, 0);
            assert_eq!(routed.get(&unrecorded).await.map(drop), Err(Error::Unavailable));
            assert_eq!(routed.numbered(9).await.map(drop), Err(crate::Error::NoBucket(9)));
        });
    }

    /// TASK-633.3: a bucket's own variables, `CLADEFSD_BUCKET_<number>_<NAME>`, stand for `NAME`
    /// over the variable itself; other buckets' stay as they are, which no builder reads.
    #[test]
    fn numbered_options_take_their_buckets_own_variables_first() {
        let vars = [
            ("AWS_REGION", "us-east-1"),
            ("AWS_ACCESS_KEY_ID", "shared"),
            ("CLADEFSD_BUCKET_2_AWS_REGION", "eu-west-1"),
            ("CLADEFSD_BUCKET_3_AWS_REGION", "ap-south-1"),
        ]
        .map(|(name, value)| (name.to_owned(), value.to_owned()));
        let mut two = numbered_options(vars.clone(), 2);
        two.sort();
        let expected = [
            ("AWS_ACCESS_KEY_ID", "shared"),
            ("AWS_REGION", "eu-west-1"),
            ("CLADEFSD_BUCKET_3_AWS_REGION", "ap-south-1"),
        ]
        .map(|(name, value)| (name.to_owned(), value.to_owned()));
        assert_eq!(two, expected);
        let mut seven = numbered_options(vars, 7);
        seven.sort();
        assert!(seven.contains(&("AWS_REGION".to_owned(), "us-east-1".to_owned())), "{seven:?}");
    }

    /// Requests that failed wait from 50 ms, doubling, to at most 5 s.
    #[test]
    fn failed_requests_back_off() {
        let waits: Vec<_> = (1..=10).map(|failures| backoff(failures).as_millis()).collect();
        assert_eq!(waits, [50, 100, 200, 400, 800, 1600, 3200, 5000, 5000, 5000]);
    }

    /// TASK-690: a bucket of a backend that takes a key's writes a second apart, as GCS does,
    /// writes each key no sooner than that after its last write of it, and other keys at once; none
    /// is refused. And an update the backend refuses holds back no update of another key, as a
    /// failed write of a kind held back every write of its kind (TASK-382): each update is of its
    /// own key.
    #[tokio::test(start_paused = true)]
    async fn writes_of_a_key_are_spaced_as_its_backend_takes_them() {
        use crate::faults::{Fault, Faults, Faulty};
        use std::time::Duration;
        let second = Duration::from_secs(1);
        let faults = Faults::new(Duration::ZERO, None);
        faults.set(Fault { rewrite_every: second, ..Fault::default() });
        let objects = Arc::new(object_store::memory::InMemory::new());
        let gcs = Arc::new(Faulty::new(objects.clone(), faults.clone()));
        let spaced = ObjectBucket::new(gcs, "").rewriting_every(second);
        let start = tokio::time::Instant::now();
        let first = spaced.put("head", vec![1].into(), PutMode::Create).await.unwrap();
        spaced.put("head", vec![2].into(), PutMode::Update(first)).await.unwrap();
        assert_eq!(start.elapsed(), second);
        spaced.put("other", vec![1].into(), PutMode::Create).await.unwrap();
        assert_eq!(start.elapsed(), second);
        spaced.rewritable("head").await;
        assert_eq!(start.elapsed(), 2 * second);
        spaced.rewritable("never written").await;
        assert_eq!(start.elapsed(), 2 * second);
        assert_eq!(spaced.stats().failures(Request::Update), 0);

        let unspaced = ObjectBucket::new(Arc::new(Faulty::new(objects, faults)), "");
        let a = unspaced.put("a", vec![1].into(), PutMode::Create).await.unwrap();
        let refused = unspaced.put("a", vec![2].into(), PutMode::Update(a)).await;
        assert_eq!(refused, Err(Error::Unavailable));
        let head = unspaced.head("head").await.unwrap().version;
        let start = tokio::time::Instant::now();
        unspaced.put("head", vec![3].into(), PutMode::Update(head)).await.unwrap();
        assert_eq!(start.elapsed(), Duration::ZERO);
        assert_eq!(unspaced.stats().failures(Request::Update), 1);
    }

    /// TASK-382: a host's uploads, as many as its upload memory holds, each sending its object
    /// whole every try, to a full bucket: 48 creates failing for 92.5 s. Each tried on its own,
    /// they failed 25 times each, 1,200 objects sent; taking turns, the first fails and the others
    /// wait, the bucket probed as one create alone would try it. Answering again, a second a write,
    /// they go as more are answered: one, then two, four, and so on, not all at once.
    #[tokio::test(start_paused = true)]
    async fn writes_failing_take_turns_until_they_land() {
        use crate::faults::{Failing, Fault, Faults, Faulty};
        let faults = Faults::new(std::time::Duration::ZERO, None);
        let objects = Arc::new(object_store::memory::InMemory::new());
        let faulty = Arc::new(Faulty::new(objects, faults.clone()));
        let bucket = Arc::new(ObjectBucket::new(faulty, ""));
        faults.set(Fault { failing: Failing::NoSpace, ..Fault::default() });
        let start = tokio::time::Instant::now();
        let creates: Vec<_> = (0..48)
            .map(|segment| {
                let (bucket, key) = (bucket.clone(), layout::segment(test_cell(1), segment, 1));
                tokio::spawn(async move {
                    let created = crate::bucket::create(&*bucket, &key, vec![1, 2, 3]).await;
                    assert!(matches!(created, crate::bucket::Created::Created(_)), "{key}");
                    start.elapsed()
                })
            })
            .collect();
        tokio::time::sleep(std::time::Duration::from_millis(92_500)).await;
        // Reads answer meanwhile, a full bucket's, and take no turn.
        assert_eq!(bucket.get("other").await.map(|object| object.bytes), Err(Error::NotFound));
        let second = std::time::Duration::from_secs(1);
        faults.set(Fault { latency: second, failing: Failing::Not, ..Fault::default() });
        let mut landed = Vec::new();
        for create in creates {
            landed.push(create.await.unwrap());
        }
        landed.sort();
        // Failed at 0 s, then probed at 50 ms, 150 ms, ... 3.15 s, 6.35 s, then every 5 s to
        // 91.35 s; the probe at 96.35 s lands a second later, then two, four, eight, 16, and the
        // last 17 at once, each a second after the last.
        let stats = bucket.stats();
        assert_eq!((stats.requests(Request::Create), stats.failures(Request::Create)), (73, 25));
        let probe = std::time::Duration::from_millis(97_350);
        let rounds = [1, 2, 4, 8, 16, 17];
        let expected: Vec<_> = (rounds.iter().enumerate())
            .flat_map(|(round, writes)| std::iter::repeat_n(probe + second * round as u32, *writes))
            .collect();
        assert_eq!(landed, expected);
    }

    /// TASK-314: a bucket whose writes keep failing, its disk full, which `object_store` says only
    /// as a generic error: a create goes again, backing off to 5 s apart, until it lands once the
    /// bucket has room. Its failures are counted, and logged with the filesystem's error, the
    /// first at once, then every 30 s with how many failed, and as creates answer again; reads
    /// answer meanwhile, which ends no run of the creates'.
    #[tokio::test(start_paused = true)]
    async fn writes_failing_say_why_until_they_land() {
        let log = crate::marks::captured();
        use crate::faults::{Failing, Fault, Faults, Faulty};
        let faults = Faults::new(std::time::Duration::ZERO, None);
        let objects = Arc::new(object_store::memory::InMemory::new());
        let faulty = Arc::new(Faulty::new(objects, faults.clone()));
        let bucket = Arc::new(ObjectBucket::new(faulty, ""));
        faults.set(Fault { failing: Failing::NoSpace, ..Fault::default() });
        let key = layout::segment(test_cell(1), 0, 1);
        let creating = tokio::spawn({
            let (bucket, key) = (bucket.clone(), key.clone());
            async move { crate::bucket::create(&*bucket, &key, vec![1, 2, 3]).await }
        });
        tokio::time::sleep(std::time::Duration::from_secs(40)).await;
        assert_eq!(bucket.get("other").await.map(|object| object.bytes), Err(Error::NotFound));
        tokio::time::sleep(std::time::Duration::from_secs(55)).await;
        faults.set(Fault::default());
        assert!(matches!(creating.await.unwrap(), crate::bucket::Created::Created(_)));
        assert_eq!(bucket.get(&key).await.unwrap().bytes, [1, 2, 3]);

        // Failed at 0 s, 50 ms, 150 ms, ... 3.15 s, 6.35 s, then every 5 s to 91.35 s; landed at
        // 96.35 s.
        let stats = bucket.stats();
        assert_eq!((stats.requests(Request::Create), stats.failures(Request::Create)), (26, 25));
        assert_eq!(stats.failures(Request::Get), 0);
        assert!(stats.line().ends_with(
            " failed-get=0 failed-get-range=0 failed-head=0 failed-list=0 failed-create=25 \
             failed-update=0 failed-overwrite=0 failed-delete=0 failed-list-delimited=0 \
             failed-delete-batch=0"
        ));
        let object = format!("object={key}");
        let enospc = "error=Generic Faulty error: No space left on device (os error 28)";
        let lines: Vec<String> = (log.lock().unwrap().iter())
            .filter(|line| line.contains("object storage"))
            .cloned()
            .collect();
        assert_eq!(
            lines,
            [
                format!(
                    "WARN object storage: create requests failing, retried until they answer: \
                     {key}: Generic Faulty error: No space left on device (os error 28) kind=create \
                     {object} {enospc}"
                ),
                format!(
                    "WARN object storage: create requests still failing: 13 failed in 31 s, 12 \
                     since last said; the latest, {key}: Generic Faulty error: No space left on \
                     device (os error 28) kind=create {object} failed=13 {enospc}"
                ),
                format!(
                    "WARN object storage: create requests still failing: 19 failed in 61 s, 6 \
                     since last said; the latest, {key}: Generic Faulty error: No space left on \
                     device (os error 28) kind=create {object} failed=19 {enospc}"
                ),
                format!(
                    "WARN object storage: create requests still failing: 25 failed in 91 s, 6 \
                     since last said; the latest, {key}: Generic Faulty error: No space left on \
                     device (os error 28) kind=create {object} failed=25 {enospc}"
                ),
                "INFO object storage: create requests answered again, 25 having failed in 96 s \
                 kind=create failed=25"
                    .to_owned(),
            ]
        );
    }
    use crate::batch::{Commit, SegmentBuilder};
    use crate::bucket::Bucket;
    use crate::layout::Origin;
    use crate::s3_server::S3Server;
    use crate::writer::Writer;
    use crate::{Error as CellError, collector, lifecycle, recovery};
    use object_store::memory::InMemory;
    use storage_device::ExtentId;

    fn memory() -> Arc<ObjectBucket> {
        Arc::new(ObjectBucket::new(Arc::new(InMemory::new()), "cladefs/test"))
    }

    /// TASK-264: a bucket reads the collector's newest marks with requests it doesn't count, at its
    /// first read, alongside it, and warns as it gets or heads a key they name, of no other key;
    /// nor of one listed, which a caller may pass over. With no marks, it warns of nothing.
    /// TASK-395.2: a cell's marks, under its prefix, of each cell it reads, read at its first read
    /// of the cell.
    #[test]
    fn buckets_warn_of_reading_what_the_newest_marks_name() {
        use crate::layout::{CellId, Marked, Marks, TEST_CELL};
        let log = crate::marks::captured();
        let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let (old, marked) =
            (layout::checkpoint(test_cell(1), 3, 1), layout::checkpoint(test_cell(1), 4, 1));
        let other = CellId { tenant: 1, id: 2 };
        let theirs = layout::checkpoint(CellId { tenant: other.tenant, id: 2 }, 1, 1);
        let warned = || -> Vec<String> { log.lock().unwrap().clone() };
        futures::executor::block_on(async {
            let writer = ObjectBucket::new(objects.clone(), "p");
            for key in [&old, &marked, &layout::checkpoint(test_cell(1), 5, 1), &theirs] {
                writer.put(key, vec![1, 2].into(), PutMode::Create).await.unwrap();
            }
            let unmarked = ObjectBucket::new(objects.clone(), "p");
            unmarked.get(&marked).await.unwrap();
            unmarked.get(&marked).await.unwrap();
            assert_eq!(warned(), Vec::<String>::new());

            let at = |key: &String| Marked {
                key: key.clone(),
                size: 2,
                e_tag: None,
                version: None,
                modified: 0,
            };
            let (older, newest) = (Marks { epoch: 6, grace: 0, marked: vec![at(&old)] }, 7);
            let marks = Marks { epoch: newest, grace: 0, marked: vec![at(&marked)] };
            let put = |marks: &Marks| layout::encode(marks);
            let theirs_marks = Marks { epoch: 3, grace: 0, marked: vec![at(&theirs)] };
            for (cell, epoch, marks) in
                [(TEST_CELL, 6, &older), (TEST_CELL, newest, &marks), (other, 3, &theirs_marks)]
            {
                let key = layout::marks(cell, epoch);
                writer.put(&key, put(marks).into(), PutMode::Create).await.unwrap();
            }
            // The marks there, but the bucket not said marked: a reader watches no cell's
            // (TASK-395.4's review).
            let blind = ObjectBucket::new(objects.clone(), "p");
            blind.get(&marked).await.unwrap();
            blind.get(&marked).await.unwrap();
            assert!(!blind.marks.marking() && blind.marks.cells_watched() == 0);
            assert_eq!(warned(), Vec::<String>::new());
            writer.put(layout::MARKING, vec![1].into(), PutMode::Create).await.unwrap();

            let reader = ObjectBucket::new(objects.clone(), "p");
            // Its first read reads the marks, alongside: what it reads after, it warns of.
            reader.head(&layout::checkpoint(test_cell(1), 5, 1)).await.unwrap();
            assert_eq!(reader.stats().requests(Request::List), 0);
            assert_eq!(reader.stats().requests(Request::Head), 1);
            reader.get(&old).await.unwrap();
            reader.list(&layout::checkpoints(test_cell(1))).await.unwrap();
            assert_eq!(warned(), Vec::<String>::new());
            reader.head(&marked).await.unwrap();
            let range = ByteRange::Bounded(0..1);
            reader.get_range(&marked, range).await.unwrap();
            reader.get(&marked).await.unwrap();
            // The other cell's, at its first read of it, which it warns of.
            reader.get(&theirs).await.unwrap();
            reader.get(&theirs).await.unwrap();
            assert_eq!(reader.stats().requests(Request::List), 1, "its own listing alone");
        });
        let saying = "a reader reached what the collector's newest mark-only pass found no live \
                      cell needs, and would have deleted";
        let reached = |request: &str, key: &str, marks: String, kind: &str, epoch| {
            format!(
                "WARN bucket_cell: {request} {key}: {saying} ({marks}) key={key} kind={kind} \
                 request={request} marks={epoch}"
            )
        };
        let ours = "Checkpoint { cell: CellId { tenant: 1, id: 1 }, segment: 4, epoch: 1 }";
        let ours = |request| reached(request, &marked, layout::marks(TEST_CELL, 7), ours, 7);
        let other = "Checkpoint { cell: CellId { tenant: 1, id: 2 }, segment: 1, epoch: 1 }";
        let theirs =
            reached("get", &theirs, layout::marks(CellId { tenant: 1, id: 2 }, 3), other, 3);
        assert_eq!(warned(), [ours("head"), ours("get-range"), ours("get"), theirs]);
    }

    /// A bucket that fails its writes as one out of reach does, `Unavailable`, as `fails` says of
    /// each in turn: before it lands (`Some(false)`), after (`Some(true)`), or not (`None`).
    struct OutOfReach(
        crate::testing::Direct,
        std::sync::Mutex<std::collections::VecDeque<Option<bool>>>,
    );

    impl Bucket for OutOfReach {
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
            page: crate::bucket::Page<'_>,
        ) -> impl Future<Output = Result<Listing, crate::bucket::Error>> + Send {
            self.0.list_delimited(prefix, page)
        }

        fn put(
            &self,
            key: &str,
            bytes: bytes::Bytes,
            mode: PutMode,
        ) -> impl Future<Output = Result<Version, Error>> + Send {
            let fails = self.1.lock().unwrap().pop_front().flatten();
            let put = self.0.put(key, bytes, mode);
            async move {
                match fails {
                    None => put.await,
                    Some(false) => Err(Error::Unavailable),
                    Some(true) => put.await.and(Err(Error::Unavailable)),
                }
            }
        }

        fn delete(&self, key: &str) -> impl Future<Output = Result<(), Error>> + Send {
            self.0.delete(key)
        }

        fn now(&self) -> u64 {
            self.0.now()
        }
    }

    /// TASK-699: a probe meeting the bucket out of reach at its steps, its writes failing before
    /// they land or after, begins again, and finds a bucket that honours conditional writes does,
    /// whichever step met it: the create, the stale update, the update after landing. Nothing of
    /// its probes is left.
    #[tokio::test(start_paused = true)]
    async fn probes_ride_out_a_bucket_out_of_reach() {
        use crate::testing::{Direct, SimBucket};
        let sim = SimBucket::new();
        // Each attempt's writes: create, create again, stale update, update.
        let fails = [
            vec![Some(false)],
            vec![None, None, Some(false)],
            vec![None, None, None, Some(true)],
            vec![Some(true)],
        ];
        let fails = fails.into_iter().flatten().collect();
        let bucket = OutOfReach(Direct(sim.clone()), std::sync::Mutex::new(fails));
        assert_eq!(probe(&bucket, 1).await, Ok(()));
        assert!(bucket.1.lock().unwrap().is_empty(), "every failure met");
        assert_eq!(sim.keys(), Vec::<String>::new());
    }

    /// Adds a segment of `commit` alone to `writer`, closed on its chain: returns its number.
    fn add<B: Bucket>(writer: &Writer<B>, commit: Commit) -> u64 {
        writer
            .add_segment(
                SegmentBuilder::from(commit)
                    .close(&mut writer.chain())
                    .frame(writer.keys().encrypting()),
            )
            .unwrap()
    }

    /// A cell's life over `bucket`: written, forked, freed, checkpointed, taken over, collected;
    /// then what it and its fork freed, reclaimed.
    /// TASK-633.4: a child in one bucket forked from a version of a parent in another, through
    /// a bucket routing by the registry: the parent's objects, head, reference and checkpoint at
    /// the version in the parent's bucket alone, the child's head in its own; the child recovers
    /// what it inherited, and reads it, from the parent's bucket. The two of different kinds: the
    /// child's a file bucket, the parent's the S3 server's, reached with options of its number's
    /// alone (`CLADEFSD_BUCKET_2_*`), as another cloud's credentials are.
    #[test]
    fn children_in_one_bucket_read_their_parents_in_another() {
        use crate::identity::{self, Details};
        runtime().block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let home_url = format!("file://{}", dir.path().display());
            let server = S3Server::start(None);
            let other_url = format!("s3://{}/other", crate::s3_server::BUCKET);
            let (home, other) = (
                ObjectBucket::from_url(&home_url).unwrap(),
                s3(&server, "other", &Client::DEFAULT),
            );
            identity::format(&home, &Details::default(), crate::identity::Asked::default())
                .await
                .unwrap();
            let details = Details { url: Some(other_url.clone()), ..Details::default() };
            identity::add(
                &home,
                &Details::default(),
                &*other,
                details,
                crate::identity::Asked::default(),
            )
            .await
            .unwrap();
            let vars = server
                .options()
                .into_iter()
                .map(|(name, value)| (format!("CLADEFSD_BUCKET_2_{name}"), value));
            let routed = Arc::new(
                ObjectBucket::from_url(&home_url).unwrap().routed(Arc::new(Registered::new(vars))),
            );
            let (parent, child) = (test_cell(2 << 48 | 1), test_cell(1 << 48 | 2));

            lifecycle::create(&*routed, parent, 0).await.unwrap();
            let writer = Writer::open(routed.clone(), parent, 10, 1).await.unwrap();
            let small = ExtentId::new(parent.id, writer.chain().next_sequence());
            let large = ExtentId::new(parent.id, small.sequence + 1);
            let commit = Commit {
                tag: 1,
                extents: vec![(small, b"small".to_vec())],
                own: vec![(large.sequence, vec![7; 4096])],
                ..Commit::default()
            };
            let segment = add(&writer, commit);
            writer.upload(segment).await.unwrap();
            writer.acknowledge().await.unwrap();
            let forking = SegmentBuilder::default().close(&mut writer.chain());
            let fork = writer.add_segment(forking.frame(writer.keys().encrypting())).unwrap();
            writer.upload(fork).await.unwrap();
            writer.acknowledge().await.unwrap();
            let origin = Origin { parent, at: fork, epoch: writer.epoch() };
            crate::testing::forked_at(&*routed, child, origin).await;

            async fn at(bucket: &ObjectBucket, key: String) -> bool {
                bucket.object_get(&key).await.is_ok()
            }
            let (parents, childs) = (layout::head(parent, 0), layout::head(child, 0));
            assert_eq!(
                (at(&other, parents.clone()).await, at(&home, parents).await),
                (true, false)
            );
            assert_eq!((at(&home, childs.clone()).await, at(&other, childs).await), (true, false));
            let reference = layout::reference(parent, fork, child);
            assert!(at(&other, reference).await, "the reference is not in the parent's bucket");
            assert_eq!(home.object_list(&parent.prefix()).await.map(|listed| listed.len()), Ok(0));

            let inherited = recovery::recover(&*routed, child, Some(origin)).await.unwrap().state;
            assert_eq!(inherited.extents.keys().copied().collect::<Vec<_>>(), [small, large]);
            let location = &inherited.extents[&small];
            assert_eq!(crate::testing::read(&routed, small, location).await.unwrap(), b"small");
            let location = &inherited.extents[&large];
            assert_eq!(
                crate::testing::read(&routed, large, location).await.unwrap(),
                vec![7; 4096]
            );

            // TASK-633.4: a part of the parent's bucket is kept in the host's part cache as its own
            // bucket's are: fetched from the S3 server once, then read, and served to the region's
            // peers (`stored_part`), from the cache, by another fetcher of the host's.
            let cache_dir = tempfile::tempdir().unwrap();
            let cache = Arc::new(crate::cache::Cache::open(cache_dir.path(), 64 << 20).unwrap());
            let keys = crate::testing::ring_of(&*routed, child).await.unwrap();
            let fetcher = |memory: Arc<crate::fetch::Memory>| {
                std::mem::forget(memory.keys().hold(keys.clone()));
                let budget = crate::fetch::Budget::new(crate::fetch::Limits::UNBOUNDED);
                crate::fetch::Fetcher::new(routed.clone(), budget, memory, Some(cache.clone()))
            };
            let gets = || server.requests().get("get").copied().unwrap_or(0);
            let before = gets();
            let first = fetcher(crate::fetch::Memory::new());
            let read = first.read(large, location, 0, location.len, crate::fetch::Priority::Demand);
            assert_eq!(read.await.unwrap(), vec![7; 4096]);
            assert_eq!(gets() - before, 2, "the tail's GET and the part's");
            let fetched = gets();
            let second = fetcher(crate::fetch::Memory::new());
            let served = second.stored_part(&location.key, 0).await.unwrap().unwrap();
            assert!(!served.is_empty());
            assert_eq!(gets() - fetched, 0, "a GET of the parent's bucket for a part cached");

            // TASK-633.7: the child deleted, the collector of its bucket retires its reference in
            // its parent's bucket (unmarked: its fork was made by its restore alone).
            lifecycle::delete(&*routed, child, 13, u64::MAX).await.unwrap();
            let options = collector::Options { compact_below: 0.5, grace: 0 };
            crate::testing::collect_until_quiet(&*routed, &options).await.unwrap();
            let reference = layout::reference(parent, fork, child);
            assert!(!at(&other, reference).await, "the reference stands in the parent's bucket");
        });
    }

    pub(crate) async fn cell_lifecycle(bucket: Arc<impl Bucket>, root: u64) {
        let child = root + 1;
        lifecycle::create(&*bucket, test_cell(root), 0).await.unwrap();
        let first = Writer::open(bucket.clone(), test_cell(root), 10, 1).await.unwrap();
        let small = ExtentId::new(root, first.chain().next_sequence());
        let large = ExtentId::new(root, small.sequence + 1);
        let commit = Commit {
            tag: 1,
            extents: vec![(small, b"small".to_vec())],
            own: vec![(large.sequence, vec![7; 4096])],
            ..Commit::default()
        };
        let segment = add(&first, commit);
        first.upload(segment).await.unwrap();
        assert_eq!(first.acknowledge().await, Ok(Some(segment)));
        let forking = SegmentBuilder::default().close(&mut first.chain());
        let fork = first.add_segment(forking.frame(first.keys().encrypting())).unwrap();
        first.upload(fork).await.unwrap();
        first.acknowledge().await.unwrap();
        let origin = Origin { parent: test_cell(root), at: fork, epoch: first.epoch() };
        crate::testing::forked_at(&*bucket, test_cell(child), origin).await;
        let segment = add(&first, Commit { tag: 2, frees: vec![small], ..Commit::default() });
        first.upload(segment).await.unwrap();
        first.acknowledge().await.unwrap();
        assert_eq!(first.checkpoint().await, Ok(Some(segment)));
        // Looked up a block at a time, by ranged reads, by a host's fetcher.
        let checkpoint = crate::layout::checkpoint(test_cell(root), segment, 1);
        let tail = crate::object::tail(&*bucket, &checkpoint).await.unwrap().unwrap();
        let table = crate::checkpoint::Table::decode(tail.footer()).unwrap();
        assert_eq!(table.state().last_commit, Some(2));
        let budget = crate::fetch::Budget::new(crate::fetch::Limits::UNBOUNDED);
        let fetcher =
            crate::fetch::Fetcher::new(bucket.clone(), budget, crate::fetch::Memory::new(), None);
        assert!(fetcher.lookup(&checkpoint, large).await.unwrap().is_some());
        assert_eq!(fetcher.lookup(&checkpoint, small).await, Ok(None));

        // Another writer takes the cell; the first one's next segment is not the cell's.
        let second = Writer::open(bucket.clone(), test_cell(root), 11, 2).await.unwrap();
        let segment = add(&first, Commit { tag: 3, frees: vec![large], ..Commit::default() });
        let taken = match first.upload(segment).await {
            Ok(()) => first.acknowledge().await.map(|_| ()),
            Err(error) => Err(error),
        };
        assert_eq!(taken, Err(CellError::Fenced));
        assert_eq!(second.state().last_commit, Some(2));

        let state = recovery::recover(&*bucket, test_cell(root), None).await.unwrap().state;
        assert_eq!(state.extents.keys().copied().collect::<Vec<_>>(), [large]);
        let origin = Some(Origin { parent: test_cell(root), at: fork, epoch: first.epoch() });
        assert_eq!(
            crate::head::get(&*bucket, test_cell(child)).await.unwrap().unwrap().origin(),
            origin
        );
        let inherited = recovery::recover(&*bucket, test_cell(child), origin).await.unwrap().state;
        assert_eq!(inherited.extents.keys().copied().collect::<Vec<_>>(), [small, large]);

        // The collector keeps what the child inherited, though the parent freed it.
        let options = collector::Options { compact_below: 0.5, grace: 0 };
        collector::collect(&*bucket, 20, &options).await.unwrap();
        let location = &inherited.extents[&small];
        assert_eq!(crate::testing::read(&bucket, small, location).await.unwrap(), b"small");
        let location = &inherited.extents[&large];
        assert_eq!(crate::testing::read(&bucket, large, location).await.unwrap(), vec![7; 4096]);

        // Freed by the parent's writer, and by the child, which inherited them, and the child
        // deleted, its reference retired, nothing reaches them: the collector reclaims their
        // objects.
        let segment = add(&second, Commit { tag: 4, frees: vec![large], ..Commit::default() });
        second.upload(segment).await.unwrap();
        second.acknowledge().await.unwrap();
        second.checkpoint().await.unwrap();
        let heir = Writer::open(bucket.clone(), test_cell(child), 12, 1).await.unwrap();
        let segment = add(&heir, Commit { tag: 1, frees: vec![small, large], ..Commit::default() });
        heir.upload(segment).await.unwrap();
        heir.acknowledge().await.unwrap();
        heir.checkpoint().await.unwrap();
        drop(heir);
        lifecycle::delete(&*bucket, test_cell(child), 13, u64::MAX).await.unwrap();
        crate::testing::collect_until_quiet(&*bucket, &options).await.unwrap();
        for extent in [small, large] {
            let key = &inherited.extents[&extent].key;
            assert_eq!(bucket.head(key).await.map(|_| ()), Err(Error::NotFound), "{extent:?}");
        }
    }

    /// cladefs: the bucket of the directory `dir`, by its URL (TASK-165.1).
    pub(crate) fn file(dir: &std::path::Path) -> Arc<ObjectBucket> {
        let url = url::Url::from_directory_path(dir).unwrap();
        Arc::new(ObjectBucket::from_url(url.as_str()).unwrap())
    }

    #[test]
    fn memory_buckets_conform() {
        futures::executor::block_on(crate::bucket::conformance(&*memory(), "conformance"));
    }

    /// cladefs: a bucket on the host's filesystem does what every bucket must (TASK-165.1).
    #[test]
    fn file_buckets_conform() {
        let dir = tempfile::tempdir().unwrap();
        futures::executor::block_on(crate::bucket::conformance(&*file(dir.path()), "conformance"));
    }

    #[test]
    fn simulated_buckets_conform() {
        let bucket = crate::testing::Direct(crate::testing::SimBucket::new());
        futures::executor::block_on(crate::bucket::conformance(&bucket, "conformance"));
    }

    /// cladefs: `object_store`'s own bucket of a directory refuses conditional updates, which the
    /// probe finds; cladefs's honours them (TASK-165.1).
    #[test]
    fn file_buckets_honour_conditional_writes() {
        let dir = tempfile::tempdir().unwrap();
        let files = object_store::local::LocalFileSystem::new_with_prefix(dir.path()).unwrap();
        let theirs = ObjectBucket::new(Arc::new(files), "");
        let refused = futures::executor::block_on(probe(&theirs, 1)).unwrap_err();
        assert_eq!(
            refused,
            "the store does not honour conditional writes: an update of another version gave \
             Err(Unsupported)"
        );
        futures::executor::block_on(probe(&*file(dir.path()), 2)).unwrap();
    }

    #[test]
    fn cells_on_a_file_bucket() {
        let dir = tempfile::tempdir().unwrap();
        futures::executor::block_on(cell_lifecycle(file(dir.path()), 1));
    }

    /// cladefs: a bucket of the S3 server in memory (TASK-205), under `prefix`, through `client`.
    fn s3(server: &S3Server, prefix: &str, client: &Client) -> Arc<ObjectBucket> {
        let url = format!("s3://{}/{prefix}", crate::s3_server::BUCKET);
        Arc::new(ObjectBucket::reached(reach(&url, server.options(), client).unwrap()))
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap()
    }

    /// cladefs: a bucket's connections are tasks of the runtime that opened them (`reqwest`, as
    /// `object_store` uses it, spawns each on the runtime it is asked on): requests each on a
    /// runtime of their own, dropped after, as `local_cell::upload::block_on`'s were, connect anew
    /// each time, the one before closed with its runtime, a handshake more a request (TCP's, and on
    /// a bucket elsewhere TLS's too); on one runtime that lives on, they share one (TASK-449).
    #[test]
    fn requests_on_runtimes_dropped_after_connect_anew() {
        let serving = runtime();
        let server = serving.block_on(async { S3Server::start(None) });
        let bucket = s3(&server, "connections", &Client::DEFAULT);
        let get = || async { crate::bucket::get(&*bucket, "absent").await };
        let own = || {
            let own = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            assert!(own.block_on(get()).is_none());
        };
        // The first, which looks at the collector's marking too, at once.
        own();
        let connects = |request: &dyn Fn()| {
            let before = server.connections();
            request();
            server.connections() - before
        };
        assert_eq!([(); 3].map(|()| connects(&own)), [1, 1, 1]);
        let shared = runtime();
        let on_shared = || assert!(shared.block_on(get()).is_none());
        assert_eq!([(); 3].map(|()| connects(&on_shared)), [1, 0, 0]);
    }

    /// cladefs: S3 over HTTP, through cladefs's client, does what every bucket must, and cells live
    /// there: the server in memory the tests below slow is as S3 is to them (TASK-205).
    #[test]
    fn s3_buckets_conform() {
        runtime().block_on(async {
            let server = S3Server::start(None);
            let bucket = s3(&server, "conformance", &Client::DEFAULT);
            crate::bucket::conformance(&*bucket, "conformance").await;
            cell_lifecycle(s3(&server, "cells", &Client::DEFAULT), 1).await;
        });
    }

    /// cladefs: over S3, deletes go in DeleteObjects of 1,000 keys each, and a listing by delimiter
    /// is one ListObjectsV2 by `delimiter=/`, listing none of the objects under the prefixes it
    /// finds (TASK-395.5): as the server serves them, and the backend counts them.
    #[test]
    fn s3_deletes_in_batches_and_lists_by_delimiter() {
        runtime().block_on(async {
            let server = S3Server::start(None);
            let bucket = s3(&server, "batches", &Client::DEFAULT);
            let keys: Vec<String> =
                (0..2500).map(|n| format!("cells/{:02}/objects/{n:05}", n % 10)).collect();
            futures::stream::iter(&keys)
                .for_each_concurrent(64, |key| async {
                    bucket.put(key, vec![1].into(), PutMode::Create).await.unwrap();
                })
                .await;
            let level = bucket.list_delimited("cells/", Page::FIRST).await.unwrap();
            let cells: Vec<String> = (0..10).map(|n| format!("cells/{n:02}/")).collect();
            assert_eq!((level.prefixes, level.objects, level.next), (cells, vec![], None));
            // A level of 250 objects, pages of 100 asked: three requests, each going on from the
            // one before by S3's continuation token (TASK-395.5).
            let (mut pages, mut after) = (Vec::new(), None);
            loop {
                let page = Page { after: after.as_deref(), max: 100 };
                let listing = bucket.list_delimited("cells/03/objects/", page).await.unwrap();
                pages.push(listing.objects.into_iter().map(|meta| meta.key).collect::<Vec<_>>());
                match listing.next {
                    Some(next) => after = Some(next),
                    None => break,
                }
            }
            assert_eq!(pages.iter().map(Vec::len).collect::<Vec<_>>(), [100, 100, 50]);
            let mut third: Vec<String> =
                keys.iter().filter(|key| key.starts_with("cells/03/")).cloned().collect();
            third.sort();
            assert_eq!(pages.concat(), third);
            let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
            crate::bucket::delete_all(&*bucket, &keys).await;
            assert_eq!(bucket.list("cells/").await.unwrap(), vec![]);
            let served = server.requests();
            let served = ["list-delimited", "delete-objects", "delete"]
                .map(|kind| served.get(kind).copied().unwrap_or(0));
            assert_eq!(served, [4, 3, 0]);
            let stats = bucket.stats();
            let counted = [Request::ListDelimited, Request::DeleteBatch, Request::Delete]
                .map(|request| stats.requests(request));
            assert_eq!(counted, [4, 3, 0]);
        });
    }

    /// `len` bytes that do not compress, from a xorshift generator seeded with `seed`.
    fn noise(len: usize, mut seed: u64) -> Vec<u8> {
        (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect()
    }

    /// TASK-205: a writer's uploads over a link slower than what they have in flight at once, by
    /// the stall time, a host's upstream that its uploads share: each lands at its first attempt,
    /// and every commit is acknowledged. Under `object_store`'s timeout of a whole request, which
    /// they all ran out of together, part sent, none ever did.
    ///
    /// What a connection's socket buffers counts as sent ([`crate::client`]): over loopback, whose
    /// segments are 64 KiB, Linux lets a socket that the server reads slowly buffer up to 4 MiB,
    /// which took a request's 2 MiB whole and left nothing to move for seconds. So objects larger
    /// than that, and a stall longer than it takes a request's share of the link to carry it.
    #[test]
    fn uploads_land_over_a_slow_link() {
        const RATE: u64 = 16 << 20;
        const SEGMENTS: u64 = 8;
        const EXTENT: usize = 16 << 20;
        let client = Client { stall: std::time::Duration::from_secs(4), ..Client::DEFAULT };
        assert!(SEGMENTS * EXTENT as u64 >= 2 * RATE * client.stall.as_secs());
        assert!(RATE / SEGMENTS * client.stall.as_secs() >= 2 * (4 << 20));
        runtime().block_on(async {
            let server = S3Server::start(Some(RATE));
            let bucket = s3(&server, "slow", &client);
            lifecycle::create(&*bucket, test_cell(1), 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), test_cell(1), 10, 1).await.unwrap();
            let segments: Vec<u64> = (1..=SEGMENTS)
                .map(|tag| {
                    let sequence = writer.chain().next_sequence();
                    let own = vec![(sequence, noise(EXTENT, tag))];
                    add(&writer, Commit { tag, own, ..Commit::default() })
                })
                .collect();
            let creates = bucket.stats().requests(Request::Create);
            let uploads =
                futures::future::join_all(segments.iter().map(|segment| writer.upload(*segment)));
            let wait = std::time::Duration::from_secs(30);
            let uploaded = tokio::time::timeout(wait, uploads).await.expect("uploads landed");
            assert_eq!(uploaded, vec![Ok(()); SEGMENTS as usize]);
            assert_eq!(writer.acknowledge().await, Ok(segments.last().copied()));
            // A segment and its extent each, each once.
            let created = bucket.stats().requests(Request::Create) - creates;
            assert_eq!(created, 2 * SEGMENTS);
        });
    }

    /// TASK-205: an upload to a server that reads nothing of it fails, once it moved no bytes for
    /// the stall time, rather than wait for ever.
    #[test]
    fn stalled_uploads_time_out() {
        let client = Client { stall: std::time::Duration::from_secs(1), ..Client::DEFAULT };
        runtime().block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let server = S3Server::at(listener.local_addr().unwrap());
            tokio::spawn(async move {
                let mut held = vec![];
                loop {
                    held.push(listener.accept().await.unwrap().0);
                }
            });
            let bucket = s3(&server, "stalled", &client);
            let start = tokio::time::Instant::now();
            let put = bucket.put("object", noise(32 << 20, 1).into(), PutMode::Create);
            let put = tokio::time::timeout(10 * client.stall, put).await.expect("the upload ended");
            let took = start.elapsed();
            assert_eq!(put, Err(Error::Unavailable));
            assert!(took >= client.stall && took < 5 * client.stall, "failed after {took:?}");
        });
    }

    /// TASK-205: what cladefs's client costs at a bandwidth nothing limits, against
    /// `object_store`'s own: 32 objects of 16 MiB uploaded at once to the S3 server in memory, by
    /// each in turn, 25 times, then their median. Run with `--run-ignored only --no-capture`, in
    /// release.
    #[test]
    #[ignore = "a benchmark (TASK-205): --run-ignored only, in release"]
    fn client_throughput() {
        const OBJECTS: usize = 32;
        const SIZE: usize = 16 << 20;
        const ROUNDS: usize = 25;
        runtime().block_on(async {
            let server = S3Server::start(None);
            let theirs = |prefix: &str| {
                let builder = object_store::aws::AmazonS3Builder::new()
                    .with_url(format!("s3://{}/{prefix}", crate::s3_server::BUCKET))
                    .with_client_options(Client::DEFAULT.options());
                let builder =
                    server.options().into_iter().fold(builder, |builder, (key, value)| {
                        builder.with_config(key.parse().unwrap(), value)
                    });
                Arc::new(ObjectBucket::new(Arc::new(builder.build().unwrap()), prefix))
            };
            let buckets = [
                ("cladefs's", s3(&server, "ours", &Client::DEFAULT)),
                ("object_store's", theirs("theirs")),
            ];
            let body = noise(SIZE, 1);
            let mut rates = [vec![], vec![]];
            for round in 0..ROUNDS {
                for ((name, bucket), rates) in buckets.iter().zip(&mut rates) {
                    let start = std::time::Instant::now();
                    let puts = (0..OBJECTS).map(|object| {
                        let (bucket, body) = (&bucket, body.clone());
                        async move {
                            bucket.put(&format!("{object}"), body.into(), PutMode::Create).await
                        }
                    });
                    for put in futures::future::join_all(puts).await {
                        put.unwrap();
                    }
                    let took = start.elapsed();
                    let rate = (OBJECTS * SIZE) as f64 / took.as_secs_f64() / 1e6;
                    eprintln!("round {round}: {name} client: {took:?}, {rate:.0} MB/s");
                    rates.push(rate);
                    for object in 0..OBJECTS {
                        bucket.delete(&format!("{object}")).await.unwrap();
                    }
                }
            }
            for ((name, _), rates) in buckets.iter().zip(&mut rates) {
                rates.sort_by(f64::total_cmp);
                eprintln!("{name} client: median {:.0} MB/s", rates[ROUNDS / 2]);
            }
        });
    }

    /// Probes the bucket at `url`'s conditional writes, and runs cells' lifecycle there.
    fn over_a_real_bucket(url: &str) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let bucket = Arc::new(ObjectBucket::from_url(url).unwrap());
            probe(&*bucket, std::process::id().into()).await.unwrap();
            // Each run its own cells, and objects: of IDs no allocator gives, so that a collector
            // over the bucket finds them its own, not a bucket's of another number that it can't
            // reach (TASK-645). Even, so that the child's, the next, is one too.
            let root = (crate::objectid::named() & !1).max(2);
            crate::bucket::conformance(&*bucket, &format!("conformance-{root:x}")).await;
            cell_lifecycle(bucket, root).await;
        });
    }

    /// The same over a real S3 server: `scripts/test-rustfs.sh` starts RustFS and runs this with
    /// `CLADEFS_S3_URL` naming a bucket of it, and the `AWS_*` variables to reach it.
    #[test]
    #[ignore = "needs an S3 bucket: scripts/test-rustfs.sh, CI's object-stores job"]
    fn rustfs() {
        over_a_real_bucket(
            &std::env::var("CLADEFS_S3_URL").expect("CLADEFS_S3_URL names the bucket"),
        );
    }

    /// cladefs: the same over GCS, through cladefs's client (TASK-205): `CLADEFS_GCS_URL` names a
    /// bucket and a prefix, under which the run leaves its cells, reached with the environment's
    /// credentials. Then 32 objects of 8 MiB uploaded at once, which a host's upstream below 9 MB/s
    /// took longer to carry than `object_store`'s 30 s: each lands at its first attempt.
    #[test]
    #[ignore = "needs a GCS bucket and its credentials: CLADEFS_GCS_URL (TASK-205)"]
    fn gcs() {
        let url = std::env::var("CLADEFS_GCS_URL").expect("CLADEFS_GCS_URL names the bucket");
        over_a_real_bucket(&url);
        const OBJECTS: u64 = 32;
        const SIZE: usize = 8 << 20;
        runtime().block_on(async {
            let bucket = ObjectBucket::from_url(&url).unwrap();
            let run = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let keys: Vec<String> = (0..OBJECTS).map(|n| format!("uploads-{run:x}/{n}")).collect();
            let start = std::time::Instant::now();
            let puts = keys
                .iter()
                .zip(1..)
                .map(|(key, seed)| bucket.put(key, noise(SIZE, seed).into(), PutMode::Create));
            let landed: Vec<bool> =
                futures::future::join_all(puts).await.iter().map(Result::is_ok).collect();
            let took = start.elapsed();
            let rate = (OBJECTS as usize * SIZE) as f64 / took.as_secs_f64() / 1e6;
            eprintln!("{OBJECTS} objects of {SIZE} bytes at once: {took:?}, {rate:.1} MB/s");
            assert_eq!(landed, vec![true; OBJECTS as usize]);
            for key in &keys {
                bucket.delete(key).await.unwrap();
            }
        });
    }

    /// The same over Azure Blob Storage, as its emulator, Azurite, serves it (TASK-46):
    /// `scripts/test-azurite.sh` starts it and runs this with `CLADEFS_AZURE_URL` naming a
    /// container of it, and the `AZURE_*` variables to reach it.
    #[test]
    #[ignore = "needs Azurite: scripts/test-azurite.sh, CI's object-stores job"]
    fn azurite() {
        let url =
            std::env::var("CLADEFS_AZURE_URL").expect("CLADEFS_AZURE_URL names the container");
        over_a_real_bucket(&url);
    }
}
