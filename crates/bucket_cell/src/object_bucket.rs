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

use crate::bucket::{
    Bucket, ByteRange, Error, LIST_PAGE, Listing, Meta, Object, Page, PutMode, Refusal, Version,
};
use crate::client::Client;
use crate::clock::Clock;
use crate::layout::{self, Kind};
pub use crate::routed::{Resolve, RoutedBucket};
use extent_keys::Zeroizing;
use extent_keys::kms::vars::{
    AWS_CREDENTIAL_VARS, AZURE_CREDENTIAL_VARS, GCP_CREDENTIAL_VARS, unnumbered,
};
use futures::{StreamExt as _, TryStreamExt as _, stream};
use object_store::list::{PaginatedListOptions, PaginatedListStore};
use object_store::path::Path;
use object_store::{
    GetOptions, GetRange, ObjectMeta, ObjectStore, PutOptions, PutPayload, UpdateVersion,
};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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
    /// cladefs: what gives this process's new cells their IDs, out of blocks of the bucket's
    /// (TASK-364).
    objectids: crate::objectid::Allocator,
    /// cladefs: how far apart its backend takes writes of one key, and when it last wrote each
    /// (TASK-690).
    rewrites: Rewrites,
    /// cladefs: its backend's clock, as it dates objects ([`Bucket::now`], board 8467).
    clock: Clock,
    /// cladefs: how it appends bytes at an exact write offset ([`Bucket::append`], doc-29), and
    /// whether [`ObjectBucket::probed_append`] already verified offset enforcement.
    appends: Appends,
    appends_honoured: std::sync::atomic::AtomicBool,
    /// cladefs: the most appends an object of an in-process store takes, as S3 Express's MaxParts
    /// has it ([`ObjectBucket::with_max_parts`], board 14565): none unless set, a store of the
    /// process's own (memory, as a simulation's) having no limit of its own.
    max_parts: Option<u32>,
    /// How many appends each key has taken, counted only under `max_parts`.
    parts: std::sync::Mutex<std::collections::HashMap<Path, u32>>,
    /// cladefs: active storage-version ratchet shared with the owning host (doc-28).
    ratchet: Arc<formats::Ratchet>,
}

/// cladefs: how a bucket performs offset-checked appends ([`Bucket::append`], doc-29).
#[derive(Clone, Debug)]
pub(crate) enum Appends {
    /// S3 Express One Zone (or S3-compatible server supporting `x-amz-write-offset-bytes`):
    /// unsigned endpoints inject the header through [`crate::client::appending`]; signed endpoints
    /// sign the PUT directly via [`object_store::aws::AwsAuthorizer`] using service `s3express`
    /// for directory buckets (`*--x-s3`) and `s3` otherwise.
    HttpS3 {
        objects: Arc<dyn ObjectStore>,
        s3: Arc<object_store::aws::AmazonS3>,
        client: reqwest::Client,
        region: String,
        s3_express: bool,
        skip_signature: bool,
    },
    /// Local file bucket, under the directory flock ([`crate::local::Local::append`]).
    Local(crate::local::Local),
    /// In-memory store in this process, serialized by `lock`.
    InProcess(Arc<tokio::sync::Mutex<()>>),
    /// Backend cannot perform offset-checked appends (with an explicit reason).
    Refused(&'static str),
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
    /// cladefs: an offset-checked append to a fast WAL object (doc-29).
    Append,
}

impl Request {
    pub const ALL: [Self; 11] = [
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
        Self::Append,
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
            Self::Append => "bucket.append",
        }
    }

    /// cladefs: whether its writes take turns once one failed ([`Turns`], TASK-382): all but an
    /// update, of a key of its own, a cell's head, which says nothing of the backend by failing as
    /// GCS refuses a write of a key within about a second of the last, while it takes others'; it
    /// backs off alone, as its retries do (TASK-690); and fast WAL appends, which handle
    /// degradation directly (doc-29).
    fn takes_turns(self) -> bool {
        !matches!(self, Self::Update | Self::Append)
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
            Self::Append => "append",
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
    /// Fast WAL segments (doc-29).
    Wal,
    /// Cells' records and heads, and whatever else.
    Other,
}

impl Upload {
    pub const ALL: [Self; 5] =
        [Self::Segments, Self::Extents, Self::Checkpoints, Self::Wal, Self::Other];

    fn of(key: &str) -> Self {
        match layout::kind(key) {
            Kind::Segment { .. } => Self::Segments,
            Kind::Extent { .. } => Self::Extents,
            Kind::Checkpoint { .. } => Self::Checkpoints,
            Kind::Wal { .. } => Self::Wal,
            _ => Self::Other,
        }
    }

    /// Its name in [`Stats::line`].
    pub fn name(self) -> &'static str {
        match self {
            Self::Segments => "segments",
            Self::Extents => "extents",
            Self::Checkpoints => "checkpoints",
            Self::Wal => "wal",
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
            if answer.failed() {
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
        if !answered.as_ref().is_err_and(|answer| answer.failed()) {
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
    /// cladefs: it lists a level of keys whole, as files and memory do, cutting pages out of it;
    /// and dates objects by the host's clock, as they do ([`Clock::Host`]): a backend that dates
    /// them by its own, a cloud's, is reached by its URL ([`ObjectBucket::from_url`]).
    pub fn new(backend: Arc<dyn ObjectStore>, prefix: &str) -> Self {
        Self::paged(backend, None, prefix)
    }

    /// cladefs: the objects of `backend` under `prefix`, a level of which `pages`, the same
    /// backend, lists a page a request, if given (TASK-395.5): S3's, GCS's and Azure's. Dated by
    /// the host's clock, as [`ObjectBucket::new`]'s.
    pub fn paged(
        backend: Arc<dyn ObjectStore>,
        pages: Option<Arc<dyn PaginatedListStore>>,
        prefix: &str,
    ) -> Self {
        let prefix = prefix.trim_matches('/').to_owned();
        let (counts, honoured, decoded) = Default::default();
        let objectids = crate::objectid::Allocator::new();
        let rewrites = Default::default();
        let appends = Appends::InProcess(Arc::new(tokio::sync::Mutex::new(())));
        Self {
            backend,
            pages,
            prefix,
            counts,
            honoured,
            decoded,
            objectids,
            rewrites,
            clock: Clock::Host,
            appends,
            appends_honoured: std::sync::atomic::AtomicBool::new(false),
            max_parts: None,
            parts: Default::default(),
            ratchet: Arc::new(formats::Ratchet::current()),
        }
    }

    /// Attaches a shared host [`formats::Ratchet`] to this bucket (doc-28).
    pub fn with_ratchet(self, ratchet: Arc<formats::Ratchet>) -> Self {
        Self { ratchet, ..self }
    }

    /// cladefs: the bucket `reached` reaches ([`reach`]), spacing its writes of a key as its
    /// backend needs.
    fn reached(reached: Reached) -> Self {
        let Reached { objects, pages, prefix, rewrite_every, clock, appends } = reached;
        let bucket = Self::paged(objects, pages, &prefix).rewriting_every(rewrite_every);
        Self { clock, appends, ..bucket }
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

    /// cladefs: it, its in-process appends refused past `parts` to an object, as S3 Express refuses
    /// a part past its MaxParts (board 14565): a simulation's fast WAL bucket, whose writers must
    /// roll their segments before it, keeping room for a `Rolled` record and a taker's `Fence`.
    pub fn with_max_parts(mut self, parts: u32) -> Self {
        self.max_parts = Some(parts);
        self
    }

    /// cladefs: wraps this bucket in an unrouted [`RoutedBucket`].
    pub fn unrouted(self) -> RoutedBucket<ObjectBucket> {
        RoutedBucket::unrouted(Arc::new(self))
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

    /// cladefs: verifies that this bucket honours offset-checked appends
    /// ([`crate::bucket::probe_append`], doc-29), once per bucket instance. Fails hard if the
    /// backend rejects appends or ignores write offsets.
    pub async fn probed_append(&self, nonce: u64) -> Result<(), String> {
        if !self.appends_honoured.load(Ordering::Relaxed) {
            crate::bucket::probe_append(self, nonce).await?;
            self.appends_honoured.store(true, Ordering::Relaxed);
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

    /// The bucket at `url`: `s3://bucket/prefix`, `gs://bucket/prefix`, `az://container/prefix`
    /// (Azure Blob Storage, TASK-46), `memory:///prefix`, or `file:///dir`, the directory `dir` of
    /// the host's filesystem ([`crate::local`], TASK-165.1), configured, credentials included, from
    /// the environment's `AWS_*`, `GOOGLE_*` or `AZURE_*` variables: for Azure, the account
    /// (`AZURE_STORAGE_ACCOUNT_NAME`) and its key (`AZURE_STORAGE_ACCOUNT_KEY`), a SAS token, or a
    /// service principal's or managed identity's, as `object_store`'s `MicrosoftAzureBuilder`
    /// reads them; the emulator, Azurite, with `AZURE_STORAGE_USE_EMULATOR=true`. Blob Storage's
    /// conditional writes (If-None-Match: *, If-Match on ETags) are those doc-7 relies on.
    pub fn from_url(url: &str) -> Result<Self, String> {
        let options = resolve_option_values(
            env_bucket_options().into_iter().filter(|(name, _)| unnumbered(name) == name.as_str()),
        )?;
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
    let options = resolve_option_values(
        env_bucket_options().into_iter().filter(|(name, _)| unnumbered(name) == name.as_str()),
    )?;
    object_store_with(url, options, &Client::DEFAULT)
}

/// The object store at `url`, configured by `options` as `object_store::parse_url_opts` would
/// configure it, but for its HTTP client: `client`'s, cladefs's own (TASK-205).
pub(crate) fn object_store_with<V: AsRef<str>>(
    url: &str,
    options: impl IntoIterator<Item = (String, V)>,
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
    /// Azure's, degraded as its objects are if they are ([`crate::faults::FaultyPages`]).
    pub pages: Pages,
    /// The prefix the URL names in it.
    pub prefix: String,
    /// cladefs: how far apart its backend takes writes of one key ([`Rewrites`], TASK-690): GCS's
    /// [`GCS_REWRITE_EVERY`], or a degraded backend's that refuses them faster
    /// ([`crate::faults::Fault::rewrite_every`]).
    pub rewrite_every: std::time::Duration,
    /// cladefs: its clock, as it dates objects (board 8467): the host's, of files and memory; of a
    /// cloud's, as its responses date them.
    pub clock: Clock,
    /// cladefs: how it performs offset-checked appends (doc-29).
    pub appends: Appends,
}

/// Whether `name` (case-insensitive) is a configuration option recognized by one of the cloud
/// `object_store` builders (S3, GCS, Azure): what [`Registered`] retains from the process
/// environment (board 9684), excluding unrelated secrets (`CLADEFS_MASTER_KEY`, KMS tokens, etc.).
fn is_bucket_option(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.parse::<object_store::aws::AmazonS3ConfigKey>().is_ok()
        || lower.parse::<object_store::gcp::GoogleConfigKey>().is_ok()
        || lower.parse::<object_store::azure::AzureConfigKey>().is_ok()
}

/// Additional `object_store`-specific S3/GCS/Azure credential option names beyond
/// `extent_keys::kms::vars`'s cloud credential groups (board 9684, hotbed review 117): any
/// variable in a cloud's group set by a numbered bucket replaces the host's variables in that
/// group as a whole. Shared aliases (such as `TOKEN` and `SKIP_SIGNATURE`) appear in every cloud
/// group that accepts them so overriding any cloud drops its shared host aliases too.
const S3_EXTRA_CREDENTIAL_VARS: &[&str] = &[
    "ACCESS_KEY_ID",
    "SECRET_ACCESS_KEY",
    "SESSION_TOKEN",
    "AWS_TOKEN",
    "TOKEN",
    "AWS_SKIP_SIGNATURE",
    "SKIP_SIGNATURE",
    "AWS_DEFAULT_PROFILE",
    "AWS_METADATA_ENDPOINT",
    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
];

const GCS_EXTRA_CREDENTIAL_VARS: &[&str] = &[
    "SERVICE_ACCOUNT",
    "SERVICE_ACCOUNT_PATH",
    "SERVICE_ACCOUNT_KEY",
    "GOOGLE_SERVICE_ACCOUNT",
    "GOOGLE_SERVICE_ACCOUNT_PATH",
    "GOOGLE_SERVICE_ACCOUNT_KEY",
    "APPLICATION_CREDENTIALS",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_BEARER_TOKEN",
    "GOOGLE_SKIP_SIGNATURE",
    "SKIP_SIGNATURE",
];

const AZURE_EXTRA_CREDENTIAL_VARS: &[&str] = &[
    "AZURE_STORAGE_ACCOUNT_KEY",
    "AZURE_STORAGE_ACCESS_KEY",
    "AZURE_STORAGE_MASTER_KEY",
    "ACCOUNT_KEY",
    "ACCESS_KEY",
    "MASTER_KEY",
    "AZURE_STORAGE_SAS_KEY",
    "AZURE_STORAGE_SAS_TOKEN",
    "SAS_KEY",
    "SAS_TOKEN",
    "AZURE_STORAGE_TOKEN",
    "AZURE_STORAGE_BEARER_TOKEN",
    "BEARER_TOKEN",
    "TOKEN",
    "AZURE_STORAGE_CLIENT_ID",
    "AZURE_STORAGE_CLIENT_SECRET",
    "AZURE_STORAGE_TENANT_ID",
    "AZURE_STORAGE_AUTHORITY_HOST",
    "AZURE_STORAGE_CONNECTION_STRING",
    "CONNECTION_STRING",
    "AZURE_USE_AZURE_CLI",
    "AZURE_SKIP_SIGNATURE",
    "SKIP_SIGNATURE",
    "AZURE_MSI_ENDPOINT",
    "AZURE_MSI_RESOURCE_ID",
    "AZURE_OBJECT_ID",
    "AZURE_FEDERATED_TOKEN_FILE",
    "AZURE_USE_WORKLOAD_IDENTITY",
];

const AWS_GROUP: u8 = 1 << 0;
const GCP_GROUP: u8 = 1 << 1;
const AZURE_GROUP: u8 = 1 << 2;

/// Bitmask of cloud credential groups (`AWS_GROUP`, `GCP_GROUP`, `AZURE_GROUP`) that recognize
/// `name` (case-insensitive). A shared alias like `TOKEN` or `SKIP_SIGNATURE` sets every cloud's
/// bit that accepts it (hotbed review 117).
fn credential_groups(name: &str) -> u8 {
    let contains = |list: &[&str]| list.iter().any(|var| var.eq_ignore_ascii_case(name));
    let mut mask = 0;
    if contains(AWS_CREDENTIAL_VARS) || contains(S3_EXTRA_CREDENTIAL_VARS) {
        mask |= AWS_GROUP;
    }
    if contains(GCP_CREDENTIAL_VARS) || contains(GCS_EXTRA_CREDENTIAL_VARS) {
        mask |= GCP_GROUP;
    }
    if contains(AZURE_CREDENTIAL_VARS) || contains(AZURE_EXTRA_CREDENTIAL_VARS) {
        mask |= AZURE_GROUP;
    }
    mask
}

/// Whether a numbered bucket's own variable `ours` replaces the host's `theirs`: same name
/// (case-insensitive) or overlapping cloud credential group (board 9684, hotbed review 117).
fn replaces_host_option(ours: &str, theirs: &str) -> bool {
    ours.eq_ignore_ascii_case(theirs) || (credential_groups(ours) & credential_groups(theirs)) != 0
}

/// A retained bucket-option value: valid UTF-8 wrapped in [`Zeroizing`], or `Invalid(env_name)`
/// recording that a recognized variable held non-UTF-8 bytes so opening a bucket selecting it
/// fails explicitly instead of silently falling back to host credentials (hotbed review 117).
#[derive(Clone)]
enum OptionValue {
    Valid(Zeroizing<String>),
    Invalid(String),
}

/// Filters `(OsString, OsString)` environment pairs down to recognized bucket-option variables
/// (host and `CLADEFSD_BUCKET_<N>_*`), zeroizing secret bytes and marking non-UTF-8 values as
/// [`OptionValue::Invalid`] so they are refused rather than silently dropped (board 9684,
/// hotbed review 117).
fn filter_os_bucket_options(
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<(String, OptionValue)> {
    let mut kept = Vec::new();
    for (name, value) in vars {
        let Some(name) = name.to_str().filter(|name| is_bucket_option(unnumbered(name))) else {
            continue;
        };
        match value.into_string() {
            Ok(text) => kept.push((name.to_owned(), OptionValue::Valid(Zeroizing::new(text)))),
            Err(non_utf8) => {
                // Scrub raw secret bytes, then retain a sentinel so opening a bucket that selects
                // this variable refuses it instead of falling back to host credentials.
                drop(Zeroizing::new(non_utf8.into_encoded_bytes()));
                kept.push((name.to_owned(), OptionValue::Invalid(name.to_owned())));
            }
        }
    }
    kept
}

/// Reads only bucket-option variables (host and `CLADEFSD_BUCKET_<N>_*`) from the process
/// environment, wrapping values in [`Zeroizing`] so secrets are scrubbed on drop (board 9684).
fn env_bucket_options() -> Vec<(String, OptionValue)> {
    filter_os_bucket_options(node_local::env::vars_os())
}

/// Resolves selected [`OptionValue`] entries to UTF-8 strings, failing if any selected option
/// carried non-UTF-8 bytes (hotbed review 117).
fn resolve_option_values(
    vars: impl IntoIterator<Item = (String, OptionValue)>,
) -> Result<Vec<(String, Zeroizing<String>)>, String> {
    let mut resolved = Vec::new();
    for (name, value) in vars {
        match value {
            OptionValue::Valid(text) => resolved.push((name, text)),
            OptionValue::Invalid(env_name) => {
                return Err(format!("{env_name}: value is not valid UTF-8"));
            }
        }
    }
    Ok(resolved)
}

/// cladefs: opens other buckets by the registry of buckets (TASK-633.3, TASK-633.4): the bucket
/// numbered `number`, as the registry copy in the home bucket's root records it, or, for one that
/// copy lacks, the root bucket's registry, the authority; at its URL, with the options `vars` give
/// it ([`numbered_options`]), refused unless its identity is the one recorded
/// ([`crate::Error::Mismatched`]); `NoBucket` for a number no registry records.
///
/// Retains only bucket-option variables in a shared zeroized snapshot (`Arc`), never unrelated
/// process secrets (board 9684).
#[derive(Clone)]
pub struct Registered {
    vars: Arc<[(String, OptionValue)]>,
}

impl std::fmt::Debug for Registered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.vars.iter().map(|(name, _)| name)).finish()
    }
}

impl Registered {
    /// Buckets opened with `vars`' options: the environment's, for a host
    /// ([`Registered::from_env`]). Retains only variables bucket options read, zeroized on drop.
    pub fn new(vars: impl IntoIterator<Item = (String, String)>) -> Self {
        let filtered: Vec<(String, OptionValue)> = vars
            .into_iter()
            .filter_map(|(name, value)| {
                let value = Zeroizing::new(value);
                is_bucket_option(unnumbered(&name)).then_some((name, OptionValue::Valid(value)))
            })
            .collect();
        Self { vars: filtered.into() }
    }

    pub fn from_env() -> Self {
        Self { vars: env_bucket_options().into() }
    }

    #[cfg(test)]
    fn from_os_vars(
        vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
    ) -> Self {
        Self { vars: filter_os_bucket_options(vars).into() }
    }

    /// Opens the bucket at `url` using the host's unnumbered options in this shared snapshot.
    pub fn open_url(&self, url: &str) -> Result<ObjectBucket, String> {
        let options = resolve_option_values(
            self.vars
                .iter()
                .filter(|(name, _)| unnumbered(name) == name.as_str())
                .map(|(name, value)| (name.clone(), value.clone())),
        )?;
        Ok(ObjectBucket::reached(reach(url, options, &Client::DEFAULT)?))
    }

    /// Opens the bucket numbered `number` at `url` using this shared snapshot.
    pub fn open_url_numbered(&self, url: &str, number: u16) -> Result<ObjectBucket, String> {
        let options =
            resolve_option_values(numbered_option_values(self.vars.iter().cloned(), number))?;
        Ok(ObjectBucket::reached(reach(url, options, &Client::DEFAULT)?))
    }

    /// The bucket `entry` records, checked to be it.
    pub(crate) async fn reach(
        &self,
        entry: &crate::identity::Entry,
    ) -> Result<ObjectBucket, crate::Error> {
        let number = entry.number;
        let url = entry.details.url.as_deref().ok_or(crate::Error::NoBucket(number))?;
        let options =
            resolve_option_values(numbered_option_values(self.vars.iter().cloned(), number))
                .map_err(|err| crate::Error::Read(format!("bucket {number} at {url}: {err}")))?;
        let reached = reach(url, options, &Client::DEFAULT)
            .map_err(|err| crate::Error::Read(format!("bucket {number} at {url}: {err}")))?;
        let bucket = ObjectBucket::reached(reached);
        crate::identity::verified(entry, crate::identity::get_once(&bucket).await?)?;
        Ok(bucket)
    }

    /// What the registry records of the bucket numbered `number`, as `home` reaches it: its copy
    /// in `home`'s root, or, for a number that copy lacks, the root bucket's registry, the
    /// authority. `NoBucket` for a number neither records. cladefs: each read asked once, a bucket
    /// out of reach failing it, as it fails the request it is for, which goes on without it or
    /// tries again (board 5528); never waiting for that bucket.
    pub async fn entry(
        &self,
        home: &impl Bucket,
        number: u16,
    ) -> Result<crate::identity::Entry, crate::Error> {
        use crate::identity::{self, ROOT};
        let copy = identity::registry_once(home).await?;
        if let Some(entry) = copy.as_ref().and_then(|copy| copy.bucket(number)) {
            return Ok(entry.clone());
        }
        let ours = identity::get_once(home).await?.ok_or(crate::Error::NoBucket(number))?;
        let root = copy.as_ref().and_then(|copy| copy.bucket(ROOT));
        let root = match (ours.number, root) {
            (ROOT, _) | (_, None) => return Err(crate::Error::NoBucket(number)),
            (_, Some(root)) => self.reach(root).await?,
        };
        let registry = identity::registry_once(&root).await?;
        let entry = registry.as_ref().and_then(|registry| registry.bucket(number));
        entry.cloned().ok_or(crate::Error::NoBucket(number))
    }
}

/// cladefs: the options of the bucket numbered `number`, from the variables `vars` (TASK-633.3):
/// each `CLADEFSD_BUCKET_<number>_<NAME>` as `NAME`, over the variable `NAME` itself, so that a
/// host reaches buckets of other clouds, regions or accounts than its own's, each with its own
/// credentials (`CLADEFSD_BUCKET_2_AWS_REGION`, `CLADEFSD_BUCKET_3_GOOGLE_SERVICE_ACCOUNT`).
///
/// Replaces cloud credential groups as a whole (board 9684): a bucket supplying any credential
/// variable of a cloud inherits none of the host's credentials for that cloud, and never sees
/// another bucket's `CLADEFSD_BUCKET_<M>_*` variables.
pub fn numbered_options(
    vars: impl IntoIterator<Item = (String, String)>,
    number: u16,
) -> Vec<(String, String)> {
    numbered_generic_options(vars, number)
}

fn numbered_option_values(
    vars: impl IntoIterator<Item = (String, OptionValue)>,
    number: u16,
) -> Vec<(String, OptionValue)> {
    numbered_generic_options(vars, number)
}

fn numbered_generic_options<V: Clone>(
    vars: impl IntoIterator<Item = (String, V)>,
    number: u16,
) -> Vec<(String, V)> {
    let prefix = format!("CLADEFSD_BUCKET_{number}_");
    let mut own = Vec::new();
    let mut host = Vec::new();
    for (name, value) in vars {
        if let Some(rest) = name.strip_prefix(&prefix) {
            own.push((rest.to_owned(), value));
        } else if unnumbered(&name) == name.as_str() {
            host.push((name, value));
        }
    }
    let replaced = |name: &str| own.iter().any(|(theirs, _)| replaces_host_option(theirs, name));
    let mut result: Vec<(String, V)> =
        host.into_iter().filter(|(name, _)| !replaced(name)).collect();
    result.extend(own);
    result
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
pub(crate) fn reach<V: AsRef<str>>(
    url: &str,
    options: impl IntoIterator<Item = (String, V)>,
    client: &Client,
) -> Result<Reached, String> {
    let url = url::Url::parse(url).map_err(|error| format!("{url}: {error}"))?;
    let (backend, pages, path, clock, appends): (Arc<dyn ObjectStore>, _, Path, _, _) =
        match url.scheme() {
            // cladefs: `object_store`'s own refuses conditional updates (TASK-165.1).
            "file" => {
                let dir = url.to_file_path().map_err(|()| format!("{url}: no directory"))?;
                let local =
                    crate::local::Local::new(&dir).map_err(|error| format!("{url}: {error}"))?;
                let appends = Appends::Local(local.clone());
                (Arc::new(local), None, Path::default(), Clock::Host, appends)
            }
            _ => cloud(&url, options, client).map_err(|error| format!("{url}: {error}"))?,
        };
    let mut rewrite_every = match provider_of(url.as_str())? {
        "gcs" => GCS_REWRITE_EVERY,
        _ => std::time::Duration::ZERO,
    };
    // cladefs: degraded as the environment says, its pages too (board 5740), and its appends over
    // HTTP; a file bucket's, under its lock, not.
    let (objects, pages, appends): (Arc<dyn ObjectStore>, _, _) =
        match crate::faults::Faults::from_env()? {
            Some(faults) => {
                rewrite_every = rewrite_every.max(faults.now().rewrite_every);
                let pages = pages.map(|pages| -> Arc<dyn PaginatedListStore> {
                    Arc::new(crate::faults::FaultyPages::new(pages, faults.clone()))
                });
                let appends = match appends {
                    Appends::HttpS3 { objects, s3, client, region, s3_express, skip_signature } => {
                        let objects = Arc::new(crate::faults::Faulty::new(objects, faults.clone()));
                        Appends::HttpS3 { objects, s3, client, region, s3_express, skip_signature }
                    }
                    appends => appends,
                };
                (Arc::new(crate::faults::Faulty::new(backend, faults)), pages, appends)
            }
            None => (backend, pages, appends),
        };
    let prefix = path.as_ref().to_owned();
    Ok(Reached { objects, pages, prefix, rewrite_every, clock, appends })
}

/// cladefs: what [`cloud`] reaches.
type Cloud = (Arc<dyn ObjectStore>, Pages, Path, Clock, Appends);

/// The backend of a URL of a cloud's, or of memory, the same backend as it lists a level of keys a
/// page a request, if it does, the path it names in it, as `object_store::parse_url_opts` makes
/// them, but with `client`; its clock: a cloud's, as its responses date them, memory's the
/// host's; and how it appends (doc-29).
fn cloud<V: AsRef<str>>(
    url: &url::Url,
    options: impl IntoIterator<Item = (String, V)>,
    client: &Client,
) -> object_store::Result<Cloud> {
    use object_store::ObjectStoreScheme as Scheme;
    let options: Vec<(String, V)> = options.into_iter().collect();
    let skip_signature = options.iter().any(|(key, value)| {
        (key.eq_ignore_ascii_case("aws_skip_signature")
            || key.eq_ignore_ascii_case("skip_signature"))
            && matches!(
                value.as_ref().to_ascii_lowercase().as_str(),
                "1" | "true" | "on" | "yes" | "y"
            )
    });
    let (scheme, path) = Scheme::parse(url)?;
    let path = Path::parse(path)?;
    let served = Arc::new(crate::clock::Served::default());
    // cladefs: what the bucket's own requests name, which its clock hears the dates of (board
    // 8467's review): `s3://bucket`'s, `gs://bucket`'s, `az://container`'s host. A URL of another
    // form (`https://...`) names it in ways of its backend's own, refused.
    let bucket = || match url.scheme() {
        "s3" | "s3a" | "gs" | "az" => url.host_str().map(str::to_owned),
        _ => None,
    };
    let bucket = || {
        bucket().ok_or_else(|| object_store::Error::Generic {
            store: "cladefs",
            source: "a bucket is named by its URL's host: s3://BUCKET, gs://BUCKET, az://CONTAINER"
                .into(),
        })
    };
    // The client's options first, which the environment's add to.
    macro_rules! configured {
        ($builder:ty) => {{
            let builder = (<$builder>::new().with_url(url.as_str()))
                .with_client_options(client.options())
                .with_http_connector(crate::client::Connector {
                    client: *client,
                    clock: served.clone(),
                    bucket: bucket()?,
                });
            options.iter().fold(builder, |builder, (key, value)| {
                match key.to_ascii_lowercase().parse() {
                    Ok(key) => builder.with_config(key, value.as_ref()),
                    // Not one of this backend's: the environment holds every cloud's, each builder
                    // taking its own.
                    Err(_) => builder,
                }
            })
        }};
    }
    // A backend built, as it lists a page a request.
    fn paged<S: ObjectStore + PaginatedListStore + 'static>(
        built: S,
    ) -> (Arc<dyn ObjectStore>, Pages) {
        let built = Arc::new(built);
        (built.clone() as Arc<dyn ObjectStore>, Some(built as Arc<dyn PaginatedListStore>))
    }
    let (backend, pages, clock, appends) = match scheme {
        Scheme::Memory => {
            // cladefs: the simulation's cloud at this URL, if it serves one, which every host of
            // the simulation reaches by it (`crate::clouds`, board 14521); else a store of this
            // opening's own.
            #[cfg(madsim)]
            let served = crate::clouds::at(url);
            #[cfg(not(madsim))]
            let served: Option<(Arc<dyn ObjectStore>, Arc<tokio::sync::Mutex<()>>)> = None;
            let (memory, appending) = served.unwrap_or_else(|| {
                (Arc::new(object_store::memory::InMemory::new()), Arc::default())
            });
            let appends = Appends::InProcess(appending);
            (memory, None, Clock::Host, appends)
        }
        Scheme::AmazonS3 => {
            let allow_http = options.iter().any(|(k, v)| {
                (k.eq_ignore_ascii_case("aws_allow_http") || k.eq_ignore_ascii_case("allow_http"))
                    && matches!(
                        v.as_ref().to_ascii_lowercase().as_str(),
                        "1" | "true" | "on" | "yes" | "y"
                    )
            });
            // Over HTTPS, cladefs objects are already AEAD-authenticated and TLS-protected; avoid
            // hashing multi-megabyte PUT bodies synchronously with SHA-256 on Tokio workers before
            // sending bytes (explicit AWS_UNSIGNED_PAYLOAD=false still overrides this).
            let mut builder = object_store::aws::AmazonS3Builder::new()
                .with_url(url.as_str())
                .with_client_options(client.options())
                .with_http_connector(crate::client::Connector {
                    client: *client,
                    clock: served.clone(),
                    bucket: bucket()?,
                });
            if !allow_http {
                builder = builder.with_unsigned_payload(true);
            }
            let mut builder = options.iter().fold(builder, |builder, (key, value)| {
                match key.to_ascii_lowercase().parse() {
                    Ok(key) => builder.with_config(key, value.as_ref()),
                    // Not one of this backend's: the environment holds every cloud's, each builder
                    // taking its own.
                    Err(_) => builder,
                }
            });
            let bucket_name = url.host_str().unwrap_or("");
            let explicit_express = options.iter().rev().find_map(|(k, v)| {
                if k.eq_ignore_ascii_case("aws_s3_express") || k.eq_ignore_ascii_case("s3_express")
                {
                    Some(matches!(
                        v.as_ref().to_ascii_lowercase().as_str(),
                        "1" | "true" | "on" | "yes" | "y"
                    ))
                } else {
                    None
                }
            });
            let has_custom_endpoint = options.iter().any(|(k, _)| {
                k.eq_ignore_ascii_case("aws_endpoint") || k.eq_ignore_ascii_case("endpoint")
            });
            let s3_express = explicit_express.unwrap_or_else(|| bucket_name.ends_with("--x-s3"));
            if s3_express && explicit_express.is_none() && !has_custom_endpoint {
                builder = builder.with_s3_express(true);
            }
            let region = builder
                .get_config_value(&object_store::aws::AmazonS3ConfigKey::Region)
                .unwrap_or_else(|| "us-east-1".to_owned());
            let s3 = Arc::new(builder.build()?);
            let backend: Arc<dyn ObjectStore> = s3.clone();
            let pages: Pages = Some(s3.clone() as Arc<dyn PaginatedListStore>);
            let mut client_opts = client.options();
            for (key, value) in &options {
                let lower = key.to_ascii_lowercase();
                let trimmed = lower.strip_prefix("aws_").unwrap_or(&lower);
                if let Ok(key) = trimmed.parse::<object_store::ClientConfigKey>() {
                    client_opts = client_opts.with_config(key, value.as_ref());
                }
            }
            let http_client = crate::client::reqwest_client(&client_opts)?;
            let appends = Appends::HttpS3 {
                objects: backend.clone(),
                s3,
                client: http_client,
                region,
                s3_express,
                skip_signature,
            };
            (backend, pages, Clock::Served(served), appends)
        }
        Scheme::GoogleCloudStorage => {
            let builder = configured!(object_store::gcp::GoogleCloudStorageBuilder);
            let (backend, pages) = paged(builder.build()?);
            let appends = Appends::Refused(
                "GCS Rapid Storage requires gRPC BidiWriteObject appendable writes, which are not \
                 yet implemented",
            );
            (backend, pages, Clock::Served(served), appends)
        }
        Scheme::MicrosoftAzure => {
            let builder = configured!(object_store::azure::MicrosoftAzureBuilder);
            let (backend, pages) = paged(builder.build()?);
            let appends =
                Appends::Refused("Azure Blob Storage does not support cladefs fast WAL appends");
            (backend, pages, Clock::Served(served), appends)
        }
        other => {
            let source = format!("no buckets of {other:?}").into();
            return Err(object_store::Error::Generic { store: "cladefs", source });
        }
    };
    Ok((backend, pages, path, clock, appends))
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
        // cladefs: refusals no retry changes (board 5861): the credentials, or the request; a
        // probe fails on one at once (board 5736).
        object_store::Error::PermissionDenied { .. }
        | object_store::Error::Unauthenticated { .. } => Error::Refused(Refusal::Denied),
        object_store::Error::InvalidPath { .. } => Error::Refused(Refusal::Request),
        object_store::Error::Generic { source, .. } => failure_of(source.as_ref()),
        _ => Error::Unavailable,
    }
}

/// cladefs: what a generic failure of `object_store`'s, `source`, says (board 5736, 5861): the
/// store's credentials expired, which a cheap request refreshes, [`Error::Expired`]; a refusal no
/// retry changes ([`refusal_of`]); anything else, the backend out of reach or failing,
/// [`Error::Unavailable`].
fn failure_of(source: &(dyn std::error::Error + 'static)) -> Error {
    let expired = ["ExpiredToken", "TokenRefreshRequired", "RequestExpired"];
    let mut cause = Some(source);
    while let Some(error) = cause {
        let said = error.to_string();
        if expired.iter().any(|code| said.contains(code)) {
            return Error::Expired;
        }
        cause = error.source();
    }
    refusal_of(source).map_or(Error::Unavailable, Error::Refused)
}

/// cladefs: the refusal `source`, a failed request's cause, is, if it is one no retry changes:
/// a client error of HTTP's (4xx) but those that go away, a timeout's, a throttle's and a conflict's
/// (408, 429, 409), and a range past an object's end (416), which [`crate::bucket::get_range`]
/// tells by the object's size; or a file bucket's file system refusing it, its permissions or a name too long
/// (board 5861). A server's error, a network's, anything else, is the bucket out of reach.
fn refusal_of(source: &(dyn std::error::Error + 'static)) -> Option<Refusal> {
    let mut cause = Some(source);
    while let Some(error) = cause {
        if let Some(status) = http_status(error) {
            let passes = [408, 409, 416, 429].contains(&status);
            return match status {
                401 | 403 => Some(Refusal::Denied),
                400..=499 if !passes => Some(Refusal::Request),
                _ => None,
            };
        }
        if let Some(io) = error.downcast_ref::<std::io::Error>() {
            return match io.kind() {
                std::io::ErrorKind::PermissionDenied => Some(Refusal::Denied),
                std::io::ErrorKind::InvalidFilename | std::io::ErrorKind::InvalidInput => {
                    Some(Refusal::Request)
                }
                _ => None,
            };
        }
        cause = error.source();
    }
    None
}

/// cladefs: the HTTP status `error` says a response had, if it is object_store's error of one: its
/// `RequestError::Status`, whose type is private to object_store, which says it as `Server returned
/// non-2xx status code: 400 Bad Request: ...` (object_store 0.14, client/retry.rs). Pinned by
/// `refusals_are_told_from_failures` against a server of its own.
fn http_status(error: &(dyn std::error::Error + 'static)) -> Option<u16> {
    let text = error.to_string();
    let status = text.strip_prefix("Server returned non-2xx status code: ")?;
    // No status, or not a number: not an HTTP answer.
    status.get(..3)?.parse().ok()
}

/// cladefs: its requests of its own objects: those [`Bucket`]'s, routed, send here, or to another
/// bucket's ([`ObjectBucket::routed`]).
impl ObjectBucket {
    fn object_get(&self, key: &str) -> impl Future<Output = Result<Object, Error>> + Send {
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
        get
    }

    fn object_get_if_changed(
        &self,
        key: &str,
        if_none_match: &Version,
    ) -> impl Future<Output = Result<Option<Object>, Error>> + Send {
        let (backend, path, version) =
            (self.backend.clone(), self.path(key), if_none_match.clone());
        async move {
            let timed = self.counts.request(Request::Get, Some(&path));
            let options =
                GetOptions { if_none_match: version.e_tag.clone(), ..GetOptions::default() };
            let got = async {
                let result = match backend.get_opts(&path, options).await {
                    Err(object_store::Error::NotModified { .. }) => return Ok(None),
                    // A local/dev backend that does not implement conditional GET: fall back to a
                    // plain GET and compare versions locally, as object_get_range does for Azure
                    // suffix reads.
                    Err(object_store::Error::NotSupported { .. }) => {
                        backend.get_opts(&path, GetOptions::default()).await?
                    }
                    result => result?,
                };
                let meta = self.meta(result.meta.clone());
                if meta.version == version {
                    return Ok(None);
                }
                let bytes = result.bytes().await?;
                self.counts.downloaded.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                Ok(Some(Object { bytes: bytes.to_vec(), meta }))
            };
            timed.answered(got.await, |object| object.as_ref().map_or(0, |o| o.bytes.len()))
        }
    }

    fn object_get_range(
        &self,
        key: &str,
        range: ByteRange,
    ) -> impl Future<Output = Result<Vec<u8>, Error>> + Send {
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
        get
    }

    fn object_head(&self, key: &str) -> impl Future<Output = Result<Meta, Error>> + Send {
        let (backend, path) = (self.backend.clone(), self.path(key));
        let head = async move {
            let timed = self.counts.request(Request::Head, Some(&path));
            let options = GetOptions { head: true, ..GetOptions::default() };
            let result = backend.get_opts(&path, options).await;
            timed.answered(result.map(|result| self.meta(result.meta)), |_| 0)
        };
        head
    }

    fn object_list(&self, prefix: &str) -> impl Future<Output = Result<Vec<Meta>, Error>> + Send {
        let backend = self.backend.clone();
        let path = self.prefix_path(prefix);
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
        list
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
        list
    }

    fn object_put(
        &self,
        key: &str,
        bytes: bytes::Bytes,
        mode: PutMode,
    ) -> impl Future<Output = Result<Version, Error>> + Send {
        let (backend, path) = (self.backend.clone(), self.path(key));
        let (request, mode) = match mode {
            PutMode::Overwrite => (Request::Overwrite, object_store::PutMode::Overwrite),
            PutMode::Create => (Request::Create, object_store::PutMode::Create),
            PutMode::Update(Version { e_tag, version }) => {
                (Request::Update, object_store::PutMode::Update(UpdateVersion { e_tag, version }))
            }
        };
        let upload = Upload::of(key);
        async move {
            // cladefs: in a simulation, now and then held a long while before it goes out: a zombie
            // writer's put landing after its successor's take (board 14565).
            if crate::buggify!(0.1) {
                zombie_hold().await;
            }
            // cladefs: a create of a segment, the write a successor's of the same number races
            // (`epoch_keys`), held more often by a site of its own (board 14565): a zombie's
            // landing past its successor's take, or the successor's past a zombie's.
            if upload == Upload::Segments
                && matches!(request, Request::Create)
                && crate::buggify!(0.5)
            {
                zombie_hold().await;
            }
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
            let result = backend.put_opts(&path, PutPayload::from(bytes), options).await;
            self.rewrites.wrote(&path);
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
    /// cladefs: appends `bytes` to `key` at exact byte `offset` ([`Bucket::append`], doc-29).
    fn object_append(
        &self,
        key: &str,
        offset: u64,
        bytes: bytes::Bytes,
    ) -> impl Future<Output = Result<u64, Error>> + Send {
        let path = self.path(key);
        let upload = Upload::of(key);
        async move {
            // cladefs: as a put's, a zombie's append landing late, past its successor's fence or
            // sweep (board 14565).
            if crate::buggify!(0.1) {
                zombie_hold().await;
            }
            let timed = self.counts.request(Request::Append, Some(&path));
            let len = bytes.len() as u64;
            let result = self.appended(&path, offset, bytes).await;
            let result = timed.answered(result, |_| len as usize)?;
            self.counts.uploaded[upload as usize].fetch_add(len, Ordering::Relaxed);
            Ok(result)
        }
    }

    /// How many appends `path` has taken, as counted ([`ObjectBucket::parts`]).
    fn parts_of(&self, path: &Path) -> u32 {
        self.parts.lock().unwrap().get(path).copied().unwrap_or(0)
    }

    async fn appended(
        &self,
        path: &Path,
        offset: u64,
        bytes: bytes::Bytes,
    ) -> object_store::Result<u64> {
        let len = bytes.len() as u64;
        let new_len = offset.checked_add(len).ok_or_else(|| object_store::Error::InvalidPath {
            source: object_store::path::Error::EmptySegment { path: path.to_string() },
        })?;
        match &self.appends {
            Appends::Local(local) => local.append(path, offset, bytes).await,
            Appends::InProcess(lock) => {
                let _alone = lock.lock().await;
                // cladefs: an object takes at most `max_parts` appends, if set, as S3 Express's
                // MaxParts has it (board 14565): the part past it refused as an offset's mismatch
                // is, which a fast WAL writer keeping no room for a `Rolled` record and a taker's
                // `Fence` (`wal_rolls_before_limit`) finds its segment full to.
                let counting = self.max_parts;
                if let Some(most) = counting
                    && offset != 0
                    && self.parts_of(path) >= most
                {
                    return Err(object_store::Error::Precondition {
                        path: path.to_string(),
                        source: format!("MaxPartsExceeded: {most} parts").into(),
                    });
                }
                let appended = async {
                    if offset == 0 {
                        let create = PutOptions {
                            mode: object_store::PutMode::Create,
                            ..PutOptions::default()
                        };
                        return match self
                            .backend
                            .put_opts(path, PutPayload::from(bytes), create)
                            .await
                        {
                            Ok(_) => Ok(len),
                            Err(object_store::Error::AlreadyExists { path, source }) => {
                                Err(object_store::Error::Precondition { path, source })
                            }
                            Err(other) => Err(other),
                        };
                    }
                    let existing = match self.backend.get_opts(path, GetOptions::default()).await {
                        Ok(got) => got.bytes().await?,
                        Err(object_store::Error::NotFound { .. }) => {
                            return Err(object_store::Error::Precondition {
                                path: path.to_string(),
                                source: format!("missing object for append at offset {offset}")
                                    .into(),
                            });
                        }
                        Err(other) => return Err(other),
                    };
                    if existing.len() as u64 != offset {
                        return Err(object_store::Error::Precondition {
                            path: path.to_string(),
                            source: format!("length {} != offset {offset}", existing.len()).into(),
                        });
                    }
                    let mut combined = Vec::with_capacity(existing.len() + bytes.len());
                    combined.extend_from_slice(&existing);
                    combined.extend_from_slice(&bytes);
                    let opts = PutOptions {
                        mode: object_store::PutMode::Overwrite,
                        ..PutOptions::default()
                    };
                    self.backend.put_opts(path, PutPayload::from(combined), opts).await?;
                    Ok::<u64, object_store::Error>(new_len)
                }
                .await;
                if counting.is_some() && appended.is_ok() {
                    let mut parts = self.parts.lock().unwrap();
                    // A create at offset 0 is the object's first part, of a key written anew.
                    let parts = parts.entry(path.clone()).or_insert(0);
                    *parts = if offset == 0 { 1 } else { *parts + 1 };
                }
                appended
            }
            Appends::HttpS3 { objects, s3, client, region, s3_express, skip_signature } => {
                if offset == 0 {
                    let create =
                        PutOptions { mode: object_store::PutMode::Create, ..PutOptions::default() };
                    return match objects.put_opts(path, PutPayload::from(bytes), create).await {
                        Ok(_) => Ok(len),
                        Err(object_store::Error::AlreadyExists { path, source }) => {
                            Err(object_store::Error::Precondition { path, source })
                        }
                        Err(other) => Err(other),
                    };
                }
                let name = reqwest::header::HeaderName::from_static("x-amz-write-offset-bytes");
                let value =
                    reqwest::header::HeaderValue::from_str(&offset.to_string()).map_err(|err| {
                        object_store::Error::Generic { store: "cladefs", source: Box::new(err) }
                    })?;
                if *skip_signature {
                    let opts = PutOptions {
                        mode: object_store::PutMode::Overwrite,
                        ..PutOptions::default()
                    };
                    let (res, sent) = crate::client::appending(
                        (name, value),
                        path.to_string(),
                        objects.put_opts(path, PutPayload::from(bytes), opts),
                    )
                    .await;
                    assert!(sent || res.is_err(), "{path}: appended without write-offset header");
                    res?;
                    return Ok(new_len);
                }
                // Sign the PUT directly via `AwsAuthorizer` (rather than `signed_url_opts`, which
                // rejects `x-amz-*` headers and hardcodes service `"s3"`): S3 Express directory
                // buckets (`*--x-s3`) require SigV4 credential scope `s3express` with
                // `x-amz-write-offset-bytes` included in `SignedHeaders`.
                use object_store::signer::Signer as _;
                let mut base = s3
                    .signed_url(hyper::Method::PUT, path, std::time::Duration::from_secs(60))
                    .await?;
                base.set_query(None);
                let cred = s3.credentials().get_credential().await?;
                let mut http_req = hyper::Request::builder()
                    .method(hyper::Method::PUT)
                    .uri(base.as_str())
                    .header(&name, &value)
                    .body(object_store::client::HttpRequestBody::from(bytes.clone()))
                    .map_err(|err| object_store::Error::Generic {
                        store: "cladefs",
                        source: Box::new(err),
                    })?;
                let service = if *s3_express { "s3express" } else { "s3" };
                object_store::aws::AwsAuthorizer::new(&cred, service, region)
                    .try_authorize(&mut http_req, None)?;
                let (parts, _) = http_req.into_parts();
                let mut req = client.put(base).body(bytes);
                for (header_name, header_value) in parts.headers {
                    if let Some(header_name) = header_name {
                        req = req.header(header_name, header_value);
                    }
                }
                let response = req.send().await.map_err(|err| object_store::Error::Generic {
                    store: "cladefs",
                    source: Box::new(err.without_url()),
                })?;
                let status = response.status();
                if status.is_success() {
                    if let Some(raw) = response.headers().get("x-amz-object-size") {
                        let reported: u64 = raw
                            .to_str()
                            .map_err(|err| object_store::Error::Generic {
                                store: "cladefs",
                                source: format!("{path}: invalid x-amz-object-size header: {err}")
                                    .into(),
                            })?
                            .parse()
                            .map_err(|err| object_store::Error::Generic {
                                store: "cladefs",
                                source: format!("{path}: unparseable x-amz-object-size: {err}")
                                    .into(),
                            })?;
                        if reported != new_len {
                            return Err(object_store::Error::Generic {
                                store: "cladefs",
                                source: format!(
                                    "{path}: x-amz-object-size {reported} != expected {new_len}"
                                )
                                .into(),
                            });
                        }
                        return Ok(reported);
                    }
                    if *s3_express {
                        return Err(object_store::Error::Generic {
                            store: "cladefs",
                            source: format!(
                                "{path}: S3 Express append response missing x-amz-object-size"
                            )
                            .into(),
                        });
                    }
                    return Ok(new_len);
                }
                // Error body text is best-effort diagnostics; an unreadable body leaves text empty
                // so HTTP 400 falls through to a retryable generic error.
                let text = response.text().await.unwrap_or_default();
                let is_offset_precondition = matches!(status.as_u16(), 404 | 409 | 412)
                    || (status.as_u16() == 400
                        && (text.contains("InvalidWriteOffset")
                            || text.contains("MaxPartsExceeded")
                            || text.contains("PreconditionFailed")
                            || text.contains("ConditionalRequestConflict")));
                if is_offset_precondition {
                    return Err(object_store::Error::Precondition {
                        path: path.to_string(),
                        source: format!("HTTP {status}: {text}").into(),
                    });
                }
                if matches!(status.as_u16(), 401 | 403) {
                    return Err(object_store::Error::PermissionDenied {
                        path: path.to_string(),
                        source: format!("HTTP {status}: {text}").into(),
                    });
                }
                Err(object_store::Error::Generic {
                    store: "cladefs",
                    source: format!("Server returned non-2xx status code: {status}: {text}").into(),
                })
            }
            Appends::Refused(reason) => Err(object_store::Error::NotSupported {
                source: format!("offset-checked append of {path}: {reason}").into(),
            }),
        }
    }
}

/// cladefs: the longest a write is held by [`zombie_hold`].
const ZOMBIE_HOLD: std::time::Duration = std::time::Duration::from_secs(8);

/// cladefs: BUGGIFY's (board 14565, doc-24): waits up to [`ZOMBIE_HOLD`] of the clock's time, as
/// the run's seed says, before a write goes out, so that a writer taken over lands its create,
/// swap or append only after its successor's take, fence or sweep of the key: a zombie request, as
/// a slow network or a paused process makes (the rules `head_swap`, `epoch_keys`, `wal_fences`,
/// `wal_offset_checked` and the sweeps' exist for it). Its callers ask `buggify!` first, which is
/// never true outside a simulation's run that switched it on: built normally the branch is gone.
async fn zombie_hold() {
    crate::buggify::hold(ZOMBIE_HOLD).await;
}

impl Bucket for ObjectBucket {
    fn decoded(&self) -> Option<&crate::recovery::Decoded> {
        Some(&self.decoded)
    }

    fn get(&self, key: &str) -> impl Future<Output = Result<Object, Error>> + Send {
        self.object_get(key)
    }

    fn get_if_changed(
        &self,
        key: &str,
        if_none_match: &Version,
    ) -> impl Future<Output = Result<Option<Object>, Error>> + Send {
        self.object_get_if_changed(key, if_none_match)
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

    fn append(
        &self,
        key: &str,
        offset: u64,
        bytes: crate::Bytes,
    ) -> impl Future<Output = Result<u64, Error>> + Send {
        self.object_append(key, offset, bytes)
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
        self.clock.now()
    }

    fn pause(&self, failures: u32) -> impl Future<Output = ()> + Send {
        tokio::time::sleep(backoff(failures))
    }

    fn ratchet(&self) -> &formats::Ratchet {
        &self.ratchet
    }
}

/// How long a request that failed `failures` times in a row waits before it goes again: from 50 ms,
/// doubling, to 5 s.
fn backoff(failures: u32) -> std::time::Duration {
    std::time::Duration::from_millis((50u64 << failures.saturating_sub(1).min(7)).min(5_000))
}

/// Checks that `bucket` honours the conditional writes cells rely on, with an object of its
/// own at `probe/<nonce>`, which it removes: a create where an object is fails, so does an update
/// of a version that is not the object's, and one of the object's version succeeds. cladefs: and
/// that its clock is its backend's ([`Bucket::now`], board 8467): within [`crate::clock::SKEW`] of
/// the date the backend gave the object as it was updated, as a read of it says, so that no window
/// measured against the backend's dates closes early by more ([`crate::clock::checked`]). A step
/// failing as a bucket out of reach does, `Unavailable`, says nothing of what the backend honours:
/// the probe begins again, with an object of its own anew, as a conditional write that failed so
/// may or may not have landed, backing off as the bucket's requests do (TASK-699). A refusal fails it
/// at once, so that a bucket's opening and a tool's do, rather than hanging; credentials expired,
/// it goes again once a cheap request refreshed them, and fails if that one finds them expired
/// still (board 5736).
pub async fn probe(bucket: &impl Bucket, nonce: u64) -> Result<(), String> {
    let mut failures = 0;
    loop {
        match probe_once(bucket, nonce.wrapping_add(failures.into())).await {
            Probed::Answered(answer) => return answer,
            Probed::Expired(key) => {
                failures += 1;
                match bucket.head(&key).await {
                    Err(error @ (Error::Expired | Error::Refused(_))) => {
                        return Err(format!("probing conditional writes at {key}: {error}"));
                    }
                    _ => log::info!("object storage: probing: credentials expired, again"),
                }
            }
            Probed::Unavailable(key) => {
                failures += 1;
                log::info!("object storage: probing conditional writes: out of reach, again");
                // Best effort: what is left of a probe is its own object, which no cell names, and
                // `cladefs-gc` deletes (`bucket::reap_probes`, board 14544).
                let _ = bucket.delete(&key).await;
                tokio::time::sleep(backoff(failures)).await;
            }
        }
    }
}

/// What a probe of conditional writes found ([`probe`]): an answer, or the bucket out of reach at
/// a step, or its credentials expired, its object's key.
enum Probed {
    Answered(Result<(), String>),
    Unavailable(String),
    Expired(String),
}

/// A probe of conditional writes, once, as [`probe`] makes it.
async fn probe_once(bucket: &impl Bucket, nonce: u64) -> Probed {
    let key = format!("{}{}", crate::bucket::PROBES, base32_id::Padded(nonce));
    let refused = |what: &str, result: &dyn std::fmt::Debug| {
        Probed::Answered(Err(format!(
            "the store does not honour conditional writes: {what} gave {result:?}"
        )))
    };
    // A step failing, as the store answered nothing of what it honours.
    let failed = |result: &Result<Version, Error>| match result {
        Err(Error::Unavailable) => Some(Probed::Unavailable(key.clone())),
        Err(Error::Expired) => Some(Probed::Expired(key.clone())),
        Err(error @ Error::Refused(_)) => {
            let refused = format!("probing conditional writes at {key}: {error}");
            Some(Probed::Answered(Err(refused)))
        }
        _ => None,
    };
    let created = bucket.put(&key, bytes::Bytes::from_static(b"probe"), PutMode::Create).await;
    if let Some(failed) = failed(&created) {
        return failed;
    }
    let version = match created {
        Ok(version) => version,
        Err(error) => return Probed::Answered(Err(format!("creating {key}: {error}"))),
    };
    let again = bucket.put(&key, bytes::Bytes::from_static(b"again"), PutMode::Create).await;
    if let Some(failed) = failed(&again) {
        return failed;
    }
    if again != Err(Error::AlreadyExists) {
        return refused("a create where an object is", &again);
    }
    // No backend makes this ETag; GCS's generation 0 is no object's.
    let other =
        Version { e_tag: Some("\"cladefs-probe\"".to_owned()), version: Some("0".to_owned()) };
    let stale = bucket.put(&key, bytes::Bytes::from_static(b"stale"), PutMode::Update(other)).await;
    if let Some(failed) = failed(&stale) {
        return failed;
    }
    if stale != Err(Error::Precondition) {
        return refused("an update of another version", &stale);
    }
    let started = std::time::Instant::now();
    let updated =
        bucket.put(&key, bytes::Bytes::from_static(b"updated"), PutMode::Update(version)).await;
    if let Some(failed) = failed(&updated) {
        return failed;
    }
    if updated.is_err() {
        return refused("an update of the object's version", &updated);
    }
    // cladefs: its clock, against the date its backend gave what it just wrote (board 8467).
    match bucket.head(&key).await {
        Ok(meta) => {
            if let Err(off) = crate::clock::checked(meta.modified, bucket.now(), started.elapsed())
            {
                return Probed::Answered(Err(format!("{key}: {off}")));
            }
        }
        Err(Error::Unavailable) => return Probed::Unavailable(key),
        Err(Error::Expired) => return Probed::Expired(key),
        Err(error) => return Probed::Answered(Err(format!("reading {key}: {error}"))),
    }
    match bucket.delete(&key).await {
        Err(Error::Unavailable | Error::Expired) => Probed::Unavailable(key),
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
        use crate::collector::Collector;
        use crate::mirror::{Here as _, Mirrored, mirror};
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
            let (holds_prefix, marker) = (layout::holds(parent, at, 1), layout::mirror(parent, at));
            let held = two.other.object_list(&holds_prefix).await.unwrap();
            assert_eq!(held.len(), 1, "expected one per-run hold");
            let hold = held[0].key.clone();
            assert!(two.home.object_get(&marker).await.is_ok(), "no marker");
            assert!(
                two.home.object_list(&layout::pins(parent, at)).await.unwrap().is_empty(),
                "the pin stands"
            );
            assert_eq!(two.refers(origin, child).await, (false, true, true));
            let again = mirror(&*bucket, &*own, 1, parent, at).await.unwrap();
            assert_eq!(again, Mirrored::Done { copied: 0, referred: 0, retired: 0 });
            // Its hold deleted, as B's collector deletes it with the copy, the marker read before:
            // a mirror begun again, finding the marker, writes none (`mirrorHoldsUnmarked`).
            two.other.object_delete(&hold).await.unwrap();
            let again = mirror(&*bucket, &*own, 1, parent, at).await.unwrap();
            assert_eq!(again, Mirrored::Done { copied: 0, referred: 0, retired: 0 });
            assert!(
                two.other.object_list(&holds_prefix).await.unwrap().is_empty(),
                "a hold written again"
            );
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
            assert!(
                two.other.object_list(&layout::holds(parent, unnamed, 1)).await.unwrap().is_empty()
            );
            assert!(two.home.object_list(&layout::pins(parent, unnamed)).await.unwrap().is_empty());
            assert!(!bucket.marked(parent, unnamed).await.unwrap(), "unnamed version marked");

            let listed = two.other.object_list(&parent.prefix()).await.unwrap();
            for meta in listed.into_iter().filter(|meta| crate::peer::served(&meta.key)) {
                two.other.object_delete(&meta.key).await.unwrap();
            }
            let fresh = two.routed();
            let inherited = recovery::recover(&*fresh, child, Some(origin)).await.unwrap().state;
            let ring = ring_of(&*fresh, child).await.unwrap();
            let read = crate::fetch::read(&fresh, &ring, extent, &inherited.extents[&extent]);
            assert_eq!(read.await.unwrap(), b"versioned");

            let (pass, fresh_own) = (Collector::new(), fresh.own());
            let report = pass.collect_copy(&*fresh, &*fresh_own, 1, parent).await.unwrap();
            assert_eq!(report.deleted, 0, "the parent's cell alive");
            lifecycle::delete(&*fresh, parent, 1, u64::MAX).await.unwrap();
            let report = pass.collect_copy(&*fresh, &*fresh_own, 1, parent).await.unwrap();
            assert_eq!((report.deleted, report.retired), (0, 0), "the child referring here");
            lifecycle::delete(&*fresh, child, 1, u64::MAX).await.unwrap();
            // While the deleted child still holds a grandchild's reference, the copy stands
            // (`references_held`); once that goes, the child's pass retires its reference here.
            let grandchild_ref = layout::reference(child, 1, test_cell(1 << 48 | 9));
            two.home
                .object_put(&grandchild_ref, b"ref".to_vec().into(), PutMode::Create)
                .await
                .unwrap();
            let report = pass.collect_copy(&*fresh, &*fresh_own, 1, parent).await.unwrap();
            assert_eq!(
                (report.deleted, report.retired),
                (0, 0),
                "the deleted child still holds a reference"
            );
            two.home.object_delete(&grandchild_ref).await.unwrap();
            // The child's own passes: its cell dead, its reference retired here.
            for holder in [8, 9] {
                crate::collector::collect_cell(&*fresh, child, holder).await.unwrap();
            }
            let report = pass.collect_copy(&*fresh, &*fresh_own, 1, parent).await.unwrap();
            // Its copy, its marker, and the Copying marker slot left by the unnamed mirror.
            assert_eq!((report.deleted, report.retired), (4, 0), "nothing needing it");
            assert_eq!(two.home.object_list(&parent.prefix()).await.unwrap(), [], "left here");
            assert!(
                two.other.object_list(&holds_prefix).await.unwrap().is_empty(),
                "the hold stands"
            );
        });
    }

    /// TASK-633.8: two buckets in files under `dir`: `home`, numbered 1, a host's, and `other`,
    /// numbered 2, its parent cells', each raw.
    struct Two {
        home_url: String,
        home: ObjectBucket,
        other: ObjectBucket,
    }

    /// cladefs: writes `head` as `cell`'s head, whatever is there.
    async fn put_head(bucket: &ObjectBucket, cell: layout::CellId, head: layout::Head) {
        let (key, bytes) = head_over(cell, &head);
        bucket.object_put(&key, bytes.into(), PutMode::Overwrite).await.unwrap();
    }

    /// cladefs: what [`put_head`] writes: the head's key and bytes.
    fn head_over(cell: layout::CellId, head: &layout::Head) -> (String, Vec<u8>) {
        (layout::head(cell), layout::encode(head))
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
            identity::format(&home, &Details::default()).await.unwrap();
            let details = Details { url: Some(other_url), ..Details::default() };
            identity::add(&home, &Details::default(), &other, details).await.unwrap();
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
    /// another mirror as its `taken_at`th conditional write lands, an update or (board 9229) a
    /// delete, if any, and the keys it deletes noted, in order, in `deleted`.
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

        /// Counts a conditional write of `key`, if it is a pin's, and reaps the pin first if it
        /// is the `taken_at`th.
        async fn taken(&self, key: &str) -> Result<(), Error> {
            if !matches!(layout::kind(key), layout::Kind::Pin { .. }) {
                return Ok(());
            }
            let updates = self.updates.fetch_add(1, Ordering::Relaxed) + 1;
            if self.taken_at == Some(updates) {
                self.own.delete(key).await?;
            }
            Ok(())
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
            if matches!(mode, PutMode::Update(_)) {
                self.taken(key).await?;
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

    /// TASK-633.8: a mirror's hold keeps, in the parent's bucket, what the version inherits from
    /// other cells there, which the mirror does not copy: the parent, itself a fork of a cell
    /// there, mirrored, then both deleted and their bucket's collector passing over them, the
    /// child here reads what the version inherited still (`mirrorHolds`).
    #[test]
    fn holds_keep_what_mirrored_versions_inherit_from_other_cells() {
        use crate::collector::collect_cell;
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
            for holder in 8..12 {
                for cell in [parent, grand] {
                    collect_cell(&*bucket, cell, holder).await.unwrap();
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
        use crate::collector::{Collector, collect_cell};
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
            let copies = Collector::new();
            for holder in 8..12 {
                for cell in [child, parent] {
                    collect_cell(&*bucket, cell, holder).await.unwrap();
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

    /// TASK-633.8: a child's cell opening here asks for its version's mirror
    /// ([`RoutedBucket::mirror_for`]), the host that forked it perhaps dead before it did: the
    /// version mirrored, once; none for a cell of no origin.
    #[test]
    fn opening_a_child_mirrors_its_version() {
        use crate::mirror::Mirrored;
        use crate::testing::{VERSION, fork_version_given};
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let two = Two::new(dir.path()).await;
            let bucket = two.routed();
            let parent = test_cell(2 << 48 | 1);
            let (at, epoch, _) = Two::versioned(&bucket, parent).await;
            let origin = Origin { parent, at, epoch };
            let child = test_cell(1 << 48 | 2);
            fork_version_given(&*bucket, child, VERSION, origin, 7).await.unwrap();
            assert_eq!(bucket.clone().mirror_for(parent).await.unwrap(), None, "a root");
            let mirrored = bucket.clone().mirror_for(child).await.unwrap();
            let named = Some((origin, Mirrored::Done { copied: 2, referred: 1, retired: 1 }));
            assert_eq!(mirrored, named, "the version it was forked from");
            assert_eq!(bucket.clone().mirror_for(child).await.unwrap(), None, "mirrored again");
            assert_eq!(two.refers(origin, child).await, (false, true, true));
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
                assert!(
                    two.other.object_list(&layout::holds(parent, at, 1)).await.unwrap().is_empty(),
                    "{step}"
                );
                assert!(
                    two.home.object_list(&layout::pins(parent, at)).await.unwrap().is_empty(),
                    "{step}"
                );
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
            let (key, bytes) = head_over(parent, &dropped);
            let written = [(other.join(key), bytes)];
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

    /// TASK-633.8, board 11699: a best-effort prefix LIST skips copying when another mirror holds
    /// a non-stale pin (per-run or legacy); a mirror whose pin is reaped before its marker gives
    /// up unmarked, releasing its own hold; a stale pin in B is resumed and reaped by B's
    /// collector; and a mirror finishing deletes only its own per-run pin, leaving any other pin.
    #[test]
    fn mirrors_hold_their_pins_alone() {
        use crate::collector::Collector;
        use crate::mirror::{Here as _, Mirrored, PIN_STALE, mirror};
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
                (bucket.own(), layout::pin(parent, at, 0x99), layout::mirror(parent, at));
            let another = layout::encode(&layout::Reference::of(u64::MAX));
            let put = PutMode::Overwrite;
            two.home.object_put(&pin, another.clone().into(), put).await.unwrap();

            assert_eq!(mirror(&*bucket, &*own, 1, parent, at).await.unwrap(), Mirrored::UnderWay);
            assert!(two.home.object_get(&marker).await.is_err(), "marked under another's pin");

            // Its own pin reaped as it refreshes it before its marker: gives up unmarked, releases
            // its own hold, and leaves the stale pin of the earlier run untouched.
            let taken = Watched::new(own.clone(), PIN_STALE + 1, Some(1));
            let gave_up = mirror(&*bucket, &taken, 1, parent, at).await.unwrap();
            assert_eq!(gave_up, Mirrored::GaveUp("its pin, stale, taken over".to_owned()));
            assert!(!bucket.marked(parent, at).await.unwrap(), "marked, its pin taken over");
            assert!(
                two.other.object_list(&layout::holds(parent, at, 1)).await.unwrap().is_empty(),
                "hold leaked after pin lost before mark"
            );
            assert_eq!(two.home.object_get(&pin).await.unwrap().bytes, another, "the other's pin");

            // B's collector sees the stale pin, resumes the mirror (marking & moving refs), and
            // reaps the stale pin.
            let later = Watched::new(own.clone(), PIN_STALE + 1, None);
            let report = Collector::new().collect_copy(&*bucket, &later, 1, parent).await;
            assert_eq!(report.unwrap().deleted, 0);
            assert!(bucket.marked(parent, at).await.unwrap(), "not marked, begun again");
            assert!(
                two.home.object_list(&layout::pins(parent, at)).await.unwrap().is_empty(),
                "the stale pin stands"
            );
            assert_eq!(two.refers(origin, child).await, (false, true, true));

            // Finishing when another (stale) pin stands: this run deletes only its own per-run pin
            // with plain unconditional DELETE, leaving the other's pin intact (board 11699).
            two.home.object_put(&pin, another.clone().into(), PutMode::Overwrite).await.unwrap();
            let done = mirror(&*bucket, &later, 1, parent, at).await.unwrap();
            assert_eq!(done, Mirrored::Done { copied: 0, referred: 0, retired: 0 });
            assert_eq!(two.home.object_get(&pin).await.unwrap().bytes, another, "the other's pin");
        });
    }

    /// TASK-633.8 (AC #4): a version mirrored here, its children here live with its bucket out of
    /// reach: a host started since, which knows nothing of the version but what this bucket holds,
    /// recovers a child and reads what it inherited, takes it, commits, and forks its own version
    /// into another child, which reads both; it asks the version's bucket nothing. board 5528:
    /// found as a host finds it, by the registry, out of reach at once.
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

            // board 5528: the version's bucket out of reach, as the registry finds it, every
            // request of a bucket opened from now on failing as one out of service.
            let home = ObjectBucket::from_url(&two.home_url).unwrap();
            let faults = dir.path().join("faults");
            std::fs::write(&faults, "failing=unavailable").unwrap();
            // SAFETY: nextest runs each test in a process of its own, whose other threads don't
            // read the environment.
            unsafe { std::env::set_var("CLADEFS_BUCKET_FAULTS", &faults) };
            let cut =
                Arc::new(home.routed(Arc::new(Registered::new(Vec::<(String, String)>::new()))));
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
                (Request::ALL.into_iter()).filter(|&request| stats.requests(request) > 0).collect()
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
        use crate::collector::Collector;
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
                let pass = || Collector::new();
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
                let held =
                    !two.other.object_list(&layout::holds(parent, at, 1)).await.unwrap().is_empty();
                assert_eq!(held, marked, "{step}: the hold");
                let reference = two.refers(origin, child).await;
                assert_eq!((reference.0, reference.1), (!retired, marked), "{step}: references");
                assert!(
                    two.home.object_list(&layout::pins(parent, at)).await.unwrap().is_empty(),
                    "{step}"
                );
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

    /// TASK-633.8: a bucket holding a mirror of a version of another bucket's cell reads that
    /// cell's objects written once here first, the cell's own bucket for those not copied. A
    /// bucket holding no marker of the cell reads its bucket alone.
    #[test]
    fn objects_of_versions_mirrored_here_are_read_here_first() {
        use crate::layout::Mirror;
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let two = Two::new(dir.path()).await;
            let (home, other) = (&two.home, &two.other);
            let parent = test_cell(2 << 48 | 1);
            let (copied, only_there) =
                (layout::segment(parent, 1, 2), layout::segment(parent, 2, 2));
            async fn put(bucket: &ObjectBucket, key: &str, bytes: &[u8]) {
                bucket.object_put(key, bytes.to_vec().into(), PutMode::Overwrite).await.unwrap();
            }
            for key in [&copied, &only_there] {
                put(other, key, b"there").await;
            }
            put(home, &copied, b"here").await;
            // No marker here: the cell's bucket alone.
            assert_eq!(two.routed().get(&copied).await.unwrap().bytes, b"there");
            put(home, &layout::mirror(parent, 2), &layout::encode(&Mirror::NOW)).await;
            // A bucket made since, which lists the markers anew.
            let marked = two.routed();
            assert_eq!(marked.get(&copied).await.unwrap().bytes, b"here");
            assert_eq!(marked.get(&only_there).await.unwrap().bytes, b"there");
        });
    }

    /// TASK-633.8: a pass over the copy here of another bucket's cell's versions keeps it whole
    /// while that cell lives, though its head names none of them, a child refers here, or a mirror
    /// is under way (a pin); retires here the references of children gone; and, nothing needing
    /// it, deletes its mirrors' holds in that cell's bucket, then its markers, then its objects,
    /// then the marks of references retired here: nothing of it left here, nothing else touched
    /// there.
    #[test]
    fn copies_of_other_buckets_versions_are_kept_while_needed() {
        use crate::collector::Collector;
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
            // And a version a mirror copied and never marked, its per-run hold left.
            let copied = [
                layout::checkpoint(parent, 3, 1),
                layout::checkpoint(parent, 5, 1),
                layout::segment(parent, 1, 2),
            ];
            let unmarked = layout::hold(parent, 5, 1, 0x11);
            put(other, &unmarked, layout::encode(&layout::Reference::of(0))).await;
            for key in &copied {
                put(home, key, b"copied".to_vec()).await;
            }
            // Both a legacy v31/v32 single-key hold and a v33 per-run hold stand in A for version
            // 3: prefix LIST in GcUnhold deletes both before the marker (board 11699).
            let (marker, legacy_hold, run_hold) = (
                layout::mirror(parent, 3),
                layout::legacy_hold(parent, 3, 1),
                layout::hold(parent, 3, 1, 0x22),
            );
            put(home, &marker, layout::encode(&Mirror::NOW)).await;
            put(other, &legacy_hold, layout::encode(&layout::Reference::of(0))).await;
            put(other, &run_hold, layout::encode(&layout::Reference::of(0x22))).await;
            let pass = || Collector::new();
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
            // Deleted, its cell holding its own child's reference still: the reference stands
            // (`references_held`), until its cell holds none (`copyChecksDead`, board 10446).
            let deleted = Head { epoch: Head::FINAL, ..Head::of(Made::Clone(origin), 4) };
            put_head(home, child, deleted).await;
            let grandchild_ref = layout::reference(child, 1, test_cell(1 << 48 | 9));
            put(home, &grandchild_ref, b"ref".to_vec()).await;
            let report = pass().collect_copy(&*routed, &*own, 1, parent).await.unwrap();
            assert_eq!((report.deleted, report.retired), (0, 0), "a deleted child holding a ref");
            // Its own child's reference gone: dead, though its head stays, so retired here.
            home.object_delete(&grandchild_ref).await.unwrap();
            // cladefs: a pass past its sweep's budget, which would retire the reference and delete
            // the copy whole, deletes none of it, the reference neither: all it would delete taken
            // at once, or none (board 8529).
            let before = (listed(home, parent).await, listed(other, parent).await);
            let budget = Some(Arc::new(crate::collector::Budget::new(Some(1), None)));
            let capped = pass().budget(budget.clone());
            let report = capped.collect_copy(&*routed, &*own, 1, parent).await.unwrap();
            let after = (listed(home, parent).await, listed(other, parent).await);
            assert_eq!((report, after), (crate::collector::Report::default(), before), "capped");
            assert_eq!(budget.unwrap().refused(), 1);
            let pin = layout::pin(parent, 3, 0x33);
            put(home, &pin, b"pin".to_vec()).await;
            let report = pass().collect_copy(&*routed, &*own, 1, parent).await.unwrap();
            assert_eq!((report.deleted, report.retired), (0, 1), "pinned");
            assert!(home.object_get(&reference).await.is_err(), "a gone child's reference stands");

            home.object_delete(&pin).await.unwrap();
            let log = Arc::new(std::sync::Mutex::new(Vec::new()));
            let watched = Watched { deleted: log.clone(), ..Watched::new(routed.clone(), 0, None) };
            let watched_own =
                Watched { deleted: log.clone(), ..Watched::new(own.clone(), 0, None) };
            let report = pass().collect_copy(&watched, &watched_own, 1, parent).await.unwrap();
            // Its copy and its marker.
            assert_eq!(report.deleted, copied.len() + 1);
            assert_eq!(listed(home, parent).await, Vec::<String>::new(), "left here");
            assert_eq!(listed(other, parent).await, vec![layout::head(parent)], "touched there");
            let mark = layout::retired(parent, 3, child);
            let mut order = vec![legacy_hold, run_hold, unmarked, marker];
            order.extend(copied);
            order.push(mark);
            assert_eq!(*log.lock().unwrap(), order, "deleted in that order");
        });
    }

    /// board 11699: `collect_copy` reaps stale pins of both v33 per-run and legacy v31/v32
    /// single-key formats: when unmarked and unnamed, deleting each stale pin's matching hold in
    /// A before deleting the stale pin in B; when marked, deleting the stale pin in B while
    /// keeping the hold in A for `GcUnhold`.
    #[test]
    fn collect_copy_reaps_stale_legacy_and_per_run_pins_with_matching_holds() {
        use crate::collector::Collector;
        use crate::layout::Head;
        use crate::mirror::PIN_STALE;
        use crate::record::Made;
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let two = Two::new(dir.path()).await;
            let (routed, (home, other)) = (two.routed(), (&two.home, &two.other));
            let own = routed.own();
            let parent = test_cell(2 << 48 | 1);
            // Parent stays alive, with version 3 dropped (unnamed) so whole == false and stale pin
            // reaping alone must clean up the stale pins and their unmarked holds.
            put_head(other, parent, Head::of(Made::Root, 4)).await;

            let legacy_pin = layout::legacy_pin(parent, 3);
            let legacy_hold = layout::legacy_hold(parent, 3, 1);
            let run_pin = layout::pin(parent, 3, 0x55);
            let run_hold = layout::hold(parent, 3, 1, 0x55);
            let bytes = layout::encode(&layout::Reference::of(0));
            for (bucket, key) in
                [(home, &legacy_pin), (other, &legacy_hold), (home, &run_pin), (other, &run_hold)]
            {
                bucket.object_put(key, bytes.clone().into(), PutMode::Create).await.unwrap();
            }

            let later = Watched::new(own.clone(), PIN_STALE + 1, None);
            Collector::new().collect_copy(&*routed, &later, 1, parent).await.unwrap();
            assert!(
                home.object_list(&layout::pins(parent, 3)).await.unwrap().is_empty()
                    && matches!(home.object_head(&legacy_pin).await, Err(Error::NotFound)),
                "stale pins left in B"
            );
            assert!(
                other.object_list(&layout::holds(parent, 3, 1)).await.unwrap().is_empty()
                    && matches!(other.object_head(&legacy_hold).await, Err(Error::NotFound)),
                "unmarked holds left in A"
            );
        });
    }

    /// board 10446 (`copyChecksDead`): a dead child whose head stays in its bucket (a `Forking`
    /// head never made, or a `Clone` kept by a `retired/` mark under its own prefix) is passed
    /// over once by `collect_cell`, which retires its reference in both buckets and records its
    /// dead digest so later passes skip it. Once the parent is deleted, `collect_copy` deletes the
    /// copy and its `retired/` marks so the prefix goes empty; a late write of the child's
    /// reference here (from a paused mirror or fork) then lands with no mark here. `collect_copy`
    /// still retires it because the child is deleted and holds no references of its own, leaving
    /// nothing behind.
    #[test]
    fn dead_childs_late_reference_after_marks_deleted_is_retired_by_collect_copy() {
        use crate::collector::{Collector, collect_cell};
        use crate::layout::{Head, Mirror, Retired};
        use crate::record::Made;
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let two = Two::new(dir.path()).await;
            let (routed, (home, other)) = (two.routed(), (&two.home, &two.other));
            let own = routed.own();
            let parent = test_cell(2 << 48 | 1);
            let origin = Origin { parent, at: 3, epoch: 1 };
            let ended_parent = Head { epoch: Head::FINAL, ..Head::of(Made::Root, 4) };
            put_head(other, parent, ended_parent).await;

            // Case 1: a deleted `Forking` child (not removable).
            // Case 2: a deleted `Clone` child kept by a `retired/` mark under its own prefix.
            let forking = test_cell(1 << 48 | 10);
            let cloned = test_cell(1 << 48 | 11);
            put_head(
                home,
                forking,
                Head { epoch: Head::FINAL, ..Head::of(Made::Forking { origin, version: 1 }, 5) },
            )
            .await;
            put_head(home, cloned, Head { epoch: Head::FINAL, ..Head::of(Made::Clone(origin), 6) })
                .await;
            home.object_put(
                &layout::retired(cloned, 1, test_cell(1 << 48 | 99)),
                layout::encode(&Retired::NOW).into(),
                PutMode::Create,
            )
            .await
            .unwrap();

            for child in [forking, cloned] {
                let reference = layout::reference(parent, 3, child);
                other
                    .object_put(&reference, b"ref".to_vec().into(), PutMode::Create)
                    .await
                    .unwrap();
                // The child's own pass retires in both buckets and records its dead digest.
                let report = collect_cell(&*routed, child, 1).await.unwrap();
                assert_eq!(report.removed, 0, "head kept for {child}");
                assert!(
                    home.object_get(&layout::retired(parent, 3, child)).await.is_ok(),
                    "retired mark written in home for {child}"
                );
                // A second pass skips the unchanged dead child.
                let skipped = collect_cell(&*routed, child, 2).await.unwrap();
                assert_eq!(skipped.skipped, 1, "dead child skipped for {child}");

                // `collect_copy` cleans up the copy and its retired marks.
                home.object_put(
                    &layout::mirror(parent, 3),
                    layout::encode(&Mirror::NOW).into(),
                    PutMode::Overwrite,
                )
                .await
                .unwrap();
                let swept =
                    Collector::new().collect_copy(&*routed, &*own, 1, parent).await.unwrap();
                assert_eq!(swept.deleted, 1);
                assert_eq!(home.object_list(&parent.prefix()).await.unwrap(), [], "marks cleared");

                // Now a paused mirror or fork lands its late create of the reference in home,
                // with no `retired/` mark there and the child skipped by `collect_cell`.
                home.object_put(&reference, b"ref".to_vec().into(), PutMode::Create).await.unwrap();
                let skipped_again = collect_cell(&*routed, child, 3).await.unwrap();
                assert_eq!(skipped_again.skipped, 1, "still skipped for {child}");

                let cleaned =
                    Collector::new().collect_copy(&*routed, &*own, 1, parent).await.unwrap();
                assert_eq!(cleaned.retired, 1, "late reference retired for {child}");
                assert_eq!(
                    home.object_list(&parent.prefix()).await.unwrap(),
                    [],
                    "nothing left in home for {child}"
                );
            }
        });
    }

    /// board 10446: `collect_cell` called through a routed bucket whose home is `@2` on a dead
    /// child of `@3` forked from `@1` must retire the child's mirror reference in `@3` (the
    /// child's own bucket), never in `@2`.
    #[test]
    fn collect_cell_through_another_bucket_retires_mirror_reference_in_childs_bucket() {
        use crate::collector::collect_cell;
        use crate::layout::Head;
        use crate::record::Made;
        use crate::testing::{Direct, SimBucket, make_head};
        futures::executor::block_on(async {
            let sim = SimBucket::new();
            let parent = test_cell(1 << 48 | 2);
            let child = test_cell(3 << 48 | 5);
            let origin = Origin { parent, at: 2, epoch: 1 };
            let direct = Direct(sim.at(1));
            make_head(
                &direct,
                child,
                Head { epoch: Head::FINAL, ..Head::of(Made::Forking { origin, version: 1 }, 7) },
            )
            .await
            .unwrap();
            let reference = layout::reference(parent, 2, child);
            Direct(sim.at(1).own())
                .put(&reference, b"ref".to_vec().into(), PutMode::Create)
                .await
                .unwrap();
            Direct(sim.at(3).own())
                .put(&reference, b"ref".to_vec().into(), PutMode::Create)
                .await
                .unwrap();

            // Collect `child` (home `@3`) through `@2`'s routed bucket.
            let via_two = sim.at(2);
            let (report, _) = sim.served(collect_cell(&*via_two, child, 9));
            let report = report.unwrap();
            assert_eq!(report.retired, 2, "retired in @1 and @3");

            let stored = sim.stored();
            assert!(!stored.contains(&format!("@1/{reference}")), "retired in @1");
            assert!(!stored.contains(&format!("@3/{reference}")), "retired in @3");
            let mark = layout::retired(parent, 2, child);
            assert!(stored.contains(&format!("@3/{mark}")), "marked in @3");
            assert!(!stored.contains(&format!("@2/{mark}")), "must not mark in @2");
        });
    }

    /// board 10446 (hotbed review 94): cloud object stores (GCS, Azure) may return an empty first
    /// delimited page with `next: Some(...)`. `collect_copy`'s `has_references` must follow `next`
    /// so a deleted child with a live grandchild reference on a later page keeps the copy whole.
    #[test]
    fn collect_copy_follows_empty_first_page_when_checking_child_references() {
        use crate::bucket::{ByteRange, Error as BucketError, Listing, Meta, Object, Page};
        use crate::collector::Collector;
        use crate::layout::{Head, Mirror};
        use crate::record::Made;

        struct EmptyFirstPage {
            inner: Arc<RoutedBucket>,
            target_prefix: String,
        }

        impl Bucket for EmptyFirstPage {
            fn get(&self, key: &str) -> impl Future<Output = Result<Object, BucketError>> + Send {
                self.inner.get(key)
            }
            fn get_range(
                &self,
                key: &str,
                range: ByteRange,
            ) -> impl Future<Output = Result<Vec<u8>, BucketError>> + Send {
                self.inner.get_range(key, range)
            }
            fn head(&self, key: &str) -> impl Future<Output = Result<Meta, BucketError>> + Send {
                self.inner.head(key)
            }
            fn list(
                &self,
                prefix: &str,
            ) -> impl Future<Output = Result<Vec<Meta>, BucketError>> + Send {
                self.inner.list(prefix)
            }
            fn list_delimited(
                &self,
                prefix: &str,
                page: Page<'_>,
            ) -> impl Future<Output = Result<Listing, BucketError>> + Send {
                let empty_first = prefix == self.target_prefix && page.after.is_none();
                let next_page = if prefix == self.target_prefix && page.after == Some("empty-page")
                {
                    Page { after: None, ..page }
                } else {
                    page
                };
                async move {
                    if empty_first {
                        return Ok(Listing {
                            prefixes: Vec::new(),
                            objects: Vec::new(),
                            next: Some("empty-page".to_owned()),
                        });
                    }
                    self.inner.list_delimited(prefix, next_page).await
                }
            }
            fn put(
                &self,
                key: &str,
                bytes: crate::Bytes,
                mode: PutMode,
            ) -> impl Future<Output = Result<crate::bucket::Version, BucketError>> + Send
            {
                self.inner.put(key, bytes, mode)
            }
            fn delete(&self, key: &str) -> impl Future<Output = Result<(), BucketError>> + Send {
                self.inner.delete(key)
            }
            fn now(&self) -> u64 {
                self.inner.now()
            }
        }

        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let two = Two::new(dir.path()).await;
            let (routed, (home, other)) = (two.routed(), (&two.home, &two.other));
            let own = routed.own();
            let parent = test_cell(2 << 48 | 1);
            let child = test_cell(1 << 48 | 5);
            let origin = Origin { parent, at: 3, epoch: 1 };
            put_head(other, parent, Head { epoch: Head::FINAL, ..Head::of(Made::Root, 4) }).await;
            put_head(home, child, Head { epoch: Head::FINAL, ..Head::of(Made::Clone(origin), 4) })
                .await;
            let reference = layout::reference(parent, 3, child);
            home.object_put(&reference, b"ref".to_vec().into(), PutMode::Create).await.unwrap();
            let marker = layout::mirror(parent, 3);
            home.object_put(&marker, layout::encode(&Mirror::NOW).into(), PutMode::Create)
                .await
                .unwrap();
            let grandchild_ref = layout::reference(child, 1, test_cell(1 << 48 | 9));
            home.object_put(&grandchild_ref, b"ref".to_vec().into(), PutMode::Create)
                .await
                .unwrap();

            let wrapped =
                EmptyFirstPage { inner: routed, target_prefix: layout::references(child) };
            let report = Collector::new().collect_copy(&wrapped, &*own, 1, parent).await.unwrap();
            assert_eq!(
                (report.deleted, report.retired),
                (0, 0),
                "empty first page with continuation must not drop live grandchild's parent copy"
            );
            assert!(home.object_get(&reference).await.is_ok(), "child reference kept");
            assert!(home.object_get(&marker).await.is_ok(), "mirror marker kept");
        });
    }

    /// TASK-633.8: a collector's sweep of this bucket, given its own bucket, passes over the copy
    /// of another bucket's cell's versions as a copy, deleting it, nothing needing it, as a
    /// collector's pass over every cell does.
    #[test]
    fn sweeps_pass_over_copies_as_copies() {
        use crate::collector::Collector;
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
            let sweep =
                Sweep { mode: Mode::Delete, at_once: 1, lease: 60_000, slowest: 1, spawned: false };
            let there = other.object_list(&parent.prefix()).await.unwrap();
            let there: Vec<String> = there.into_iter().map(|meta| meta.key).collect();
            assert_eq!(there, vec![layout::head(parent)], "there");
            let swept = sweep.run_with_own(&routed, Some(&own), |_| {}).await;
            // Its copy and its marker.
            assert_eq!((swept.report.deleted, left().await), (2, 0), "swept");
            copy().await;
            let collected = Collector::new().collect_with_own(&*routed, Some(&*own), 9).await;
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
            identity::format(&home, &Details::default()).await.unwrap();
            let details = Details { url: Some(other_url.clone()), ..Details::default() };
            identity::add(&home, &Details::default(), &other, details).await.unwrap();
            let routed = ObjectBucket::from_url(&home_url)
                .unwrap()
                .routed(Arc::new(Registered::new(Vec::<(String, String)>::new())));

            let theirs = layout::CellId { tenant: 1, id: 2 << 48 | 5 };
            let ours = layout::CellId { tenant: 1, id: 1 << 48 | 5 };
            let named = layout::CellId { tenant: 1, id: 7 };
            for cell in [theirs, ours, named] {
                let key = layout::head(cell);
                routed.put(&key, b"head".to_vec().into(), PutMode::Create).await.unwrap();
                assert_eq!(routed.get(&key).await.map(|object| object.bytes), Ok(b"head".to_vec()));
            }
            async fn at(bucket: &ObjectBucket, cell: layout::CellId) -> bool {
                bucket.object_get(&layout::head(cell)).await.is_ok()
            }
            assert_eq!((at(&other, theirs).await, at(&home, theirs).await), (true, false));
            assert_eq!((at(&home, ours).await, at(&other, ours).await), (true, false));
            assert_eq!((at(&home, named).await, at(&other, named).await), (true, false));
            let listed = routed.list(&theirs.prefix()).await.unwrap();
            assert_eq!(
                listed.iter().map(|meta| meta.key.clone()).collect::<Vec<_>>(),
                [layout::head(theirs)]
            );

            // TASK-633.8: its own bucket, unrouted, takes a key of another bucket's cell here.
            let pin = layout::pin(theirs, 3, 1);
            let resolve = Arc::new(Registered::new(Vec::<(String, String)>::new()));
            let own = ObjectBucket::from_url(&home_url).unwrap().routed(resolve).own().clone();
            own.put(&pin, b"pin".to_vec().into(), PutMode::Create).await.unwrap();
            let (here, there) = (home.object_get(&pin).await, other.object_get(&pin).await);
            assert_eq!((here.is_ok(), there.is_ok()), (true, false));

            let (theirs_key, ours_key) = (layout::head(theirs), layout::head(ours));
            routed.delete_batch(&[&theirs_key, &ours_key]).await.unwrap();
            assert_eq!((at(&other, theirs).await, at(&home, ours).await), (false, false));

            let by_bucket = routed.stats_by_bucket();
            let numbers: Vec<u16> = by_bucket.iter().map(|(number, _)| *number).collect();
            assert_eq!(numbers, [1, 2]);
            // Its identity's, read as it was opened, and the head's.
            assert_eq!(by_bucket[1].1.requests(Request::Get), 2, "bucket 2's reads");
            assert_eq!(by_bucket[1].1.requests(Request::Create), 1, "bucket 2's one write");

            let unrecorded = layout::head(layout::CellId { tenant: 1, id: 9 << 48 | 1 });
            assert_eq!(routed.get(&unrecorded).await.map(drop), Err(Error::Unavailable));
            assert_eq!(routed.numbered(9).await.map(drop), Err(crate::Error::NoBucket(9)));
        });
    }

    /// TASK-633.3 & board 9684: a bucket's own variables, `CLADEFSD_BUCKET_<number>_<NAME>`, stand
    /// for `NAME` over the variable itself; other buckets' variables are excluded; and cloud
    /// credential groups replace the host's group as a whole so host session tokens / profiles
    /// never leak into a numbered bucket with its own credentials.
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
        let expected = [("AWS_ACCESS_KEY_ID", "shared"), ("AWS_REGION", "eu-west-1")]
            .map(|(name, value)| (name.to_owned(), value.to_owned()));
        assert_eq!(two, expected);
        let mut seven = numbered_options(vars, 7);
        seven.sort();
        assert!(seven.contains(&("AWS_REGION".to_owned(), "us-east-1".to_owned())), "{seven:?}");
        assert!(!seven.iter().any(|(name, _)| name.starts_with("CLADEFSD_BUCKET_")), "{seven:?}");
    }

    /// board 9684: `Registered` retains only bucket-option variables (never `CLADEFS_MASTER_KEY`,
    /// KMS tokens, or unrelated secrets), shares its snapshot via `Arc` on clone, prints no secret
    /// values in `Debug`, and `numbered_options` never mixes host and per-bucket credentials.
    #[test]
    fn registered_filters_unrelated_secrets_and_numbered_options_isolate_credential_groups() {
        let env = [
            ("CLADEFS_MASTER_KEY", "deadbeef"),
            ("CLADEFS_KMS_OPENBAO_TOKEN", "s.vault-secret"),
            ("UNRELATED_API_SECRET", "top-secret"),
            ("AWS_REGION", "us-east-1"),
            ("AWS_ACCESS_KEY_ID", "host-key"),
            ("AWS_SECRET_ACCESS_KEY", "host-secret"),
            ("AWS_SESSION_TOKEN", "host-session"),
            ("AWS_PROFILE", "host-profile"),
            ("AZURE_STORAGE_ACCOUNT_KEY", "host-az-key"),
            ("AZURE_STORAGE_SAS_TOKEN", "host-az-sas"),
            ("TOKEN", "host-shared-token"),
            ("SKIP_SIGNATURE", "true"),
            ("GOOGLE_SERVICE_ACCOUNT", "/host/sa.json"),
            ("CLADEFSD_BUCKET_2_AWS_ACCESS_KEY_ID", "b2-key"),
            ("CLADEFSD_BUCKET_2_AWS_SECRET_ACCESS_KEY", "b2-secret"),
            ("CLADEFSD_BUCKET_2_CLADEFS_MASTER_KEY", "b2-master-secret"),
            ("CLADEFSD_BUCKET_3_AZURE_STORAGE_ACCOUNT_KEY", "b3-az-key"),
            ("CLADEFSD_BUCKET_4_GOOGLE_BEARER_TOKEN", "b4-gcp-token"),
        ]
        .map(|(name, value)| (name.to_owned(), value.to_owned()));

        let reg = Registered::new(env.clone());
        let cloned = reg.clone();
        assert!(Arc::ptr_eq(&reg.vars, &cloned.vars), "Registered clones share one Arc snapshot");
        let kept_names: Vec<&str> = reg.vars.iter().map(|(n, _)| n.as_str()).collect();
        for secret in [
            "CLADEFS_MASTER_KEY",
            "CLADEFS_KMS_OPENBAO_TOKEN",
            "UNRELATED_API_SECRET",
            "CLADEFSD_BUCKET_2_CLADEFS_MASTER_KEY",
        ] {
            assert!(!kept_names.contains(&secret), "secret {secret} retained: {kept_names:?}");
        }
        assert!(kept_names.contains(&"AWS_REGION"), "{kept_names:?}");
        assert!(kept_names.contains(&"CLADEFSD_BUCKET_2_AWS_ACCESS_KEY_ID"), "{kept_names:?}");
        let dbg = format!("{reg:?}");
        for secret_val in ["host-secret", "host-session", "b2-secret", "b3-az-key"] {
            assert!(!dbg.contains(secret_val), "Debug leaked value {secret_val}: {dbg}");
        }

        // Bucket 2 sets its own AWS key + secret: must keep AWS_REGION and its own AWS creds,
        // must NOT inherit host AWS_SESSION_TOKEN or AWS_PROFILE, and must not see bucket 3/4 vars.
        let b2 = numbered_options(env.clone(), 2);
        let has = |opts: &[(String, String)], k: &str, v: &str| {
            opts.iter().any(|(name, val)| name == k && val == v)
        };
        let has_key = |opts: &[(String, String)], k: &str| opts.iter().any(|(name, _)| name == k);
        assert!(has(&b2, "AWS_REGION", "us-east-1"), "{b2:?}");
        assert!(has(&b2, "AWS_ACCESS_KEY_ID", "b2-key"), "{b2:?}");
        assert!(has(&b2, "AWS_SECRET_ACCESS_KEY", "b2-secret"), "{b2:?}");
        assert!(!has_key(&b2, "AWS_SESSION_TOKEN"), "leaked host AWS_SESSION_TOKEN: {b2:?}");
        assert!(!has_key(&b2, "AWS_PROFILE"), "leaked host AWS_PROFILE: {b2:?}");
        assert!(!has_key(&b2, "TOKEN"), "leaked shared host TOKEN to AWS bucket: {b2:?}");
        assert!(!has_key(&b2, "SKIP_SIGNATURE"), "leaked SKIP_SIGNATURE to AWS bucket: {b2:?}");
        assert!(!b2.iter().any(|(n, _)| n.starts_with("CLADEFSD_BUCKET_")), "{b2:?}");
        // Unrelated clouds still inherit host creds when bucket 2 did not override them.
        assert!(has(&b2, "AZURE_STORAGE_ACCOUNT_KEY", "host-az-key"), "{b2:?}");

        // Bucket 3 overrides Azure storage account key -> drops host Azure SAS token AND shared
        // aliases (`TOKEN`, `SKIP_SIGNATURE`) that Azure also accepts (hotbed review 117).
        let b3 = numbered_options(env.clone(), 3);
        assert!(has(&b3, "AZURE_STORAGE_ACCOUNT_KEY", "b3-az-key"), "{b3:?}");
        assert!(!has_key(&b3, "AZURE_STORAGE_SAS_TOKEN"), "leaked host Azure SAS: {b3:?}");
        assert!(!has_key(&b3, "TOKEN"), "leaked shared host TOKEN to Azure bucket: {b3:?}");
        assert!(!has_key(&b3, "SKIP_SIGNATURE"), "leaked SKIP_SIGNATURE to Azure bucket: {b3:?}");

        // Bucket 4 overrides Google bearer token -> drops host GOOGLE_SERVICE_ACCOUNT & shared.
        let b4 = numbered_options(env, 4);
        assert!(has(&b4, "GOOGLE_BEARER_TOKEN", "b4-gcp-token"), "{b4:?}");
        assert!(!has_key(&b4, "GOOGLE_SERVICE_ACCOUNT"), "leaked host GCP SA: {b4:?}");
        assert!(!has_key(&b4, "SKIP_SIGNATURE"), "leaked SKIP_SIGNATURE to GCP bucket: {b4:?}");

        // Non-UTF-8 recognized numbered credential must refuse opening that bucket rather than
        // silently dropping the override and falling back to host credentials (hotbed review 117).
        #[cfg(unix)]
        {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt as _;
            let malformed = Registered::from_os_vars([
                (OsString::from("AZURE_STORAGE_ACCOUNT_NAME"), OsString::from("hostacct")),
                (OsString::from("AZURE_STORAGE_ACCOUNT_KEY"), OsString::from("aG9zdGtleQ==")),
                (
                    OsString::from("CLADEFSD_BUCKET_2_AZURE_STORAGE_ACCOUNT_KEY"),
                    OsString::from_vec(vec![0xff]),
                ),
            ]);
            let refused = malformed
                .open_url_numbered("az://container", 2)
                .err()
                .expect("malformed numbered credential must be refused");
            assert!(
                refused.contains("CLADEFSD_BUCKET_2_AZURE_STORAGE_ACCOUNT_KEY")
                    && refused.contains("UTF-8"),
                "expected refusal naming malformed numbered var, got: {refused}"
            );
        }
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

    /// Board 5736: a bucket whose store refuses its requests, `401` or `403`, fails its probe at
    /// once, as its opening then does, rather than retrying for ever, as every request does (board
    /// 5861, `refusals_are_told_from_failures`); one whose credentials expired, and stay so after a
    /// cheap request, fails it too. A create under way, its
    /// credentials expired, sends its object again only once a cheap request is answered: once
    /// failed, once landed, the HEADs between failing.
    #[tokio::test(start_paused = true)]
    async fn refusals_fail_probes_and_wait_on_cheap_requests() {
        use crate::faults::{Failing, Fault, Faults, Faulty};
        let faults = Faults::new(std::time::Duration::ZERO, None);
        let objects = Arc::new(object_store::memory::InMemory::new());
        let bucket = ObjectBucket::new(Arc::new(Faulty::new(objects, faults.clone())), "");
        let key = format!("probe/{}", base32_id::Padded(1));
        let refused = Error::Refused(crate::bucket::Refusal::Denied);
        let refused = format!("probing conditional writes at {key}: {refused}");
        let expired = format!("probing conditional writes at {key}: {}", Error::Expired);
        for (failing, said) in [
            (Failing::Unauthenticated, &refused),
            (Failing::Forbidden, &refused),
            (Failing::ExpiredToken, &expired),
        ] {
            faults.set(Fault { failing, ..Fault::default() });
            assert_eq!(probe(&bucket, 1).await.as_ref(), Err(said), "{failing:?}");
        }

        faults.set(Fault { failing: Failing::ExpiredToken, ..Fault::default() });
        let stats = |request| (bucket.stats().requests(request), bucket.stats().failures(request));
        let (creates, heads) = (stats(Request::Create), stats(Request::Head));
        let refreshed = async {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            faults.set(Fault::default());
        };
        let (created, ()) =
            tokio::join!(crate::bucket::create(&bucket, "segment", vec![7; 1 << 20]), refreshed);
        assert!(matches!(created.unwrap(), crate::bucket::Created::Created(_)));
        let since = |(requests, failures): (u64, u64), (before, failed_before): (u64, u64)| {
            (requests - before, failures - failed_before)
        };
        assert_eq!(since(stats(Request::Create), creates), (2, 1));
        let (heads, failed) = since(stats(Request::Head), heads);
        assert_eq!(failed, heads - 1, "every HEAD but the last failed, of {heads}");
        // Backing off from 50 ms, doubling to 5 s: the twelfth at 31.35 s, after the refresh.
        assert_eq!(heads, 12);
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
                    let created =
                        crate::bucket::create(&*bucket, &key, vec![1, 2, 3]).await.unwrap();
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
        let log = crate::testing::captured();
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
        assert!(matches!(creating.await.unwrap().unwrap(), crate::bucket::Created::Created(_)));
        assert_eq!(bucket.get(&key).await.unwrap().bytes, [1, 2, 3]);

        // Failed at 0 s, 50 ms, 150 ms, ... 3.15 s, 6.35 s, then every 5 s to 91.35 s; landed at
        // 96.35 s.
        let stats = bucket.stats();
        assert_eq!((stats.requests(Request::Create), stats.failures(Request::Create)), (26, 25));
        assert_eq!(stats.failures(Request::Get), 0);
        assert!(stats.line().ends_with(
            " failed-get=0 failed-get-range=0 failed-head=0 failed-list=0 failed-create=25 \
             failed-update=0 failed-overwrite=0 failed-delete=0 failed-list-delimited=0 \
             failed-delete-batch=0 failed-append=0"
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

    /// CLADEFS_BUCKET_LATENCY_MS adds its milliseconds to each request, as a farther object store's
    /// would (TASK-62): a put and a get, two of them; and each request's time is counted, by its
    /// kind (TASK-165.11).
    #[test]
    fn requests_wait_the_latency_set() {
        // SAFETY: nextest runs each test in a process of its own, whose other threads don't read
        // the environment yet.
        unsafe { std::env::set_var("CLADEFS_BUCKET_LATENCY_MS", "50") };
        let bucket = ObjectBucket::from_url("memory:///latency").unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let start = std::time::Instant::now();
        runtime.block_on(async {
            bucket.put("a", vec![1].into(), PutMode::Create).await.unwrap();
            assert_eq!(bucket.get("a").await.unwrap().bytes, vec![1]);
        });
        let took = start.elapsed();
        assert!(took >= std::time::Duration::from_millis(100), "two requests took {took:?}");
        let stats = bucket.stats();
        let timed = |request| std::time::Duration::from_nanos(stats.request_ns(request));
        for request in [Request::Create, Request::Get] {
            assert!(timed(request) >= std::time::Duration::from_millis(50), "{request:?}");
        }
        assert_eq!(timed(Request::List), std::time::Duration::ZERO);
    }

    /// A bucket counts its requests by kind, failed ones too, and the bytes it uploads, by kind of
    /// object, and downloads.
    #[test]
    fn counts_requests_and_bytes() {
        let bucket = memory();
        futures::executor::block_on(async {
            let segment = layout::segment(test_cell(1), 0, 1);
            let version = bucket.put(&segment, vec![1; 100].into(), PutMode::Create).await.unwrap();
            assert!(bucket.put(&segment, vec![2; 7].into(), PutMode::Create).await.is_err());
            bucket.put(&segment, vec![3; 50].into(), PutMode::Update(version)).await.unwrap();
            bucket
                .put(&layout::checkpoint(test_cell(1), 0, 1), vec![4; 30].into(), PutMode::Create)
                .await
                .unwrap();
            bucket
                .put(&layout::head(test_cell(1)), vec![5; 9].into(), PutMode::Overwrite)
                .await
                .unwrap();
            let extent =
                layout::extent(layout::TEST_CELL, 1, 0, storage_device::ExtentId::new(1, 5));
            bucket.put(&extent, vec![6; 11].into(), PutMode::Create).await.unwrap();
            assert_eq!(bucket.get(&segment).await.unwrap().bytes.len(), 50);
            let range = ByteRange::Bounded(10..30);
            assert_eq!(bucket.get_range(&segment, range).await.unwrap().len(), 20);
            assert!(bucket.get(&layout::segment(test_cell(1), 9, 1)).await.is_err());
            bucket.head(&segment).await.unwrap();
            bucket.list("segments/").await.unwrap();
            bucket.list_delimited("segments/", crate::bucket::Page::FIRST).await.unwrap();
            bucket.delete(&segment).await.unwrap();
            bucket.delete_batch(&[&extent]).await.unwrap();
        });
        let stats = bucket.stats();
        let requests: Vec<_> =
            Request::ALL.iter().map(|request| (request.name(), stats.requests(*request))).collect();
        assert_eq!(
            requests,
            [
                ("get", 2),
                ("get-range", 1),
                ("head", 1),
                ("list", 1),
                ("create", 4),
                ("update", 1),
                ("overwrite", 1),
                ("delete", 1),
                ("list-delimited", 1),
                ("delete-batch", 1),
                ("append", 0),
            ]
        );
        assert_eq!(stats.uploaded, [150, 11, 30, 0, 9]);
        assert_eq!(stats.downloaded, 70);
        // cladefs: what the protocol answers fails no request (TASK-314): the create where an
        // object was, the get of none.
        assert_eq!(stats.failures, [0; Request::ALL.len()]);
        // Its line, each kind of request's time after the rest, then its failures: as of times
        // and failures set here, as each request's time is its own.
        let failures = [0, 1, 0, 0, 3, 0, 0, 2, 0, 4, 5];
        let request_ns = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110];
        let stats = Stats { request_ns, failures, ..stats };
        assert_eq!(
            stats.line(),
            "object-store: get=2 get-range=1 head=1 list=1 create=4 update=1 overwrite=1 delete=1 \
             list-delimited=1 delete-batch=1 append=0 \
             up-segments=150 up-extents=11 up-checkpoints=30 up-wal=0 up-other=9 down=70 \
             get-ns=10 get-range-ns=20 head-ns=30 list-ns=40 create-ns=50 update-ns=60 \
             overwrite-ns=70 delete-ns=80 list-delimited-ns=90 delete-batch-ns=100 append-ns=110 \
             failed-get=0 failed-get-range=1 failed-head=0 failed-list=0 failed-create=3 \
             failed-update=0 failed-overwrite=0 failed-delete=2 failed-list-delimited=0 \
             failed-delete-batch=4 failed-append=5"
        );
    }

    /// A bucket that writes whatever it is asked to, conditions or not.
    struct Careless(crate::testing::Direct);

    impl Bucket for Careless {
        crate::bucket::delegate_bucket_methods!(except_put 0);

        fn put(
            &self,
            key: &str,
            bytes: bytes::Bytes,
            _mode: PutMode,
        ) -> impl Future<Output = Result<Version, Error>> + Send {
            self.0.put(key, bytes, PutMode::Overwrite)
        }
    }

    /// The probe passes a bucket that honours conditional writes, and fails one that does not.
    #[test]
    fn probes_find_buckets_without_conditional_writes() {
        use crate::testing::{Direct, SimBucket};
        let sim = SimBucket::new();
        futures::executor::block_on(async {
            assert_eq!(probe(&Direct(sim.clone()), 1).await, Ok(()));
            let refused = probe(&Careless(Direct(sim.clone())), 2).await.unwrap_err();
            assert!(refused.starts_with("the store does not honour conditional writes: a create"));
        });
        assert_eq!(sim.keys(), ["probe/0000000000002"]);
    }

    /// A bucket that fails its writes as one out of reach does, `Unavailable`, as `fails` says of
    /// each in turn: before it lands (`Some(false)`), after (`Some(true)`), or not (`None`).
    struct OutOfReach(
        crate::testing::Direct,
        std::sync::Mutex<std::collections::VecDeque<Option<bool>>>,
    );

    impl Bucket for OutOfReach {
        crate::bucket::delegate_bucket_methods!(except_put 0);

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

    /// TASK-92.6: a bucket probed once is not probed again: the second asks nothing.
    #[test]
    fn buckets_are_probed_once() {
        let bucket = memory();
        futures::executor::block_on(bucket.probed(1)).unwrap();
        let probing = bucket.stats();
        let writes = [Request::Create, Request::Update, Request::Delete];
        let asked = writes.map(|request| (request.name(), probing.requests(request)));
        assert_eq!(asked, [("create", 2), ("update", 2), ("delete", 1)]);
        futures::executor::block_on(bucket.probed(2)).unwrap();
        assert_eq!(bucket.stats(), probing);
    }

    /// TASK-92.55: recoveries through a host's bucket decode a checkpoint once: the forks of a
    /// base opened one after another start from its newest checkpoint, the second reading it not
    /// again, but the head, and the same state: kept by its key, which names one object, ever
    /// (TASK-407).
    #[test]
    fn recoveries_share_the_checkpoints_they_decode() {
        let bucket = memory();
        futures::executor::block_on(async {
            lifecycle::create(&*bucket, test_cell(1), 0).await.unwrap();
            let writer = Writer::open(bucket.clone(), test_cell(1), 10, 1).await.unwrap();
            let extent = ExtentId::new(1, writer.chain().next_sequence());
            let commit = Commit { tag: 1, extents: vec![(extent, vec![1])], ..Commit::default() };
            let segment = add(&writer, commit);
            writer.upload(segment).await.unwrap();
            writer.acknowledge().await.unwrap();
            assert_eq!(writer.checkpoint().await, Ok(Some(segment)));
            let gets = || bucket.stats().requests(Request::Get);
            let mut read = Vec::new();
            let mut states = Vec::new();
            for _ in 0..2 {
                let before = gets();
                states.push(recovery::recover(&*bucket, test_cell(1), None).await.unwrap().state);
                read.push(gets() - before);
            }
            // cladefs: the head's, one object (doc-27 step 4).
            assert_eq!(read, [2, 1], "the head's and the checkpoint's GETs, by recovery");
            assert_eq!(states[0], states[1]);
            assert_eq!(states[0].extents.keys().copied().collect::<Vec<_>>(), [extent]);
        });
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
            identity::format(&home, &Details::default()).await.unwrap();
            let details = Details { url: Some(other_url.clone()), ..Details::default() };
            identity::add(&home, &Details::default(), &*other, details).await.unwrap();
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
            let (parents, childs) = (layout::head(parent), layout::head(child));
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
            let cache = Arc::new(
                host_fs::may_block(|| crate::cache::Cache::open(cache_dir.path(), 64 << 20))
                    .unwrap(),
            );
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
            crate::testing::collect_until_quiet(&*routed).await.unwrap();
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
        collector::collect(&*bucket, 20).await.unwrap();
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
        crate::testing::collect_until_quiet(&*bucket).await.unwrap();
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
    pub(crate) fn s3(server: &S3Server, prefix: &str, client: &Client) -> Arc<ObjectBucket> {
        let url = format!("s3://{}/{prefix}", crate::s3_server::BUCKET);
        Arc::new(ObjectBucket::reached(reach(&url, server.options(), client).unwrap()))
    }

    pub(crate) fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap()
    }

    /// Board 8467's review: its clock hears the dates of the bucket's own requests' responses
    /// alone, which name it, as its host's first label or one of its path's first two segments,
    /// at no link-local address: not those of requests naming it nowhere, as its credentials' do
    /// (an instance's metadata service's `/latest/...`, STS's, OAuth's `/token`), a service an hour
    /// ahead of the bucket's backend dating them.
    #[test]
    fn only_the_buckets_own_requests_date_it() {
        use object_store::client::{HttpConnector as _, HttpRequestBody};
        let url = |url: &str| reqwest::Url::parse(url).unwrap();
        let named = [
            "https://s3.us-east-1.amazonaws.com/cladefs/cells/1",
            "https://cladefs.s3.us-east-1.amazonaws.com/cells/1",
            "https://storage.googleapis.com/cladefs?list-type=2",
            "http://127.0.0.1:10000/devstoreaccount1/cladefs/cells/1",
        ];
        let unnamed = [
            "http://169.254.169.254/cladefs/meta-data/iam/security-credentials/",
            "http://[fd00:ec2::254]/cladefs/api/token",
            "https://sts.us-east-1.amazonaws.com/",
            "https://oauth2.googleapis.com/token",
            "https://login.microsoftonline.com/tenant/oauth2/v2.0/token",
            "https://storage.googleapis.com/a/b/cladefs",
        ];
        for at in named {
            assert!(crate::client::names("cladefs", &url(at)), "{at}");
        }
        for at in unnamed {
            assert!(!crate::client::names("cladefs", &url(at)), "{at}");
        }

        let serving = runtime();
        let server = serving.block_on(async { S3Server::start(None) });
        let served = Arc::new(crate::clock::Served::default());
        let bucket = crate::s3_server::BUCKET.to_owned();
        let connector =
            crate::client::Connector { client: Client::DEFAULT, clock: served.clone(), bucket };
        let client = connector.connect(&Client::DEFAULT.options().with_allow_http(true)).unwrap();
        let options = server.options();
        let endpoint = &options.iter().find(|(key, _)| key == "aws_endpoint").unwrap().1;
        let get = |path: &str| {
            let request = hyper::Request::get(format!("{endpoint}/{path}"));
            serving
                .block_on(client.execute(request.body(HttpRequestBody::empty()).unwrap()))
                .unwrap();
        };
        server.clock_ahead(3_600_000);
        get("latest/api/token");
        get("token");
        assert_eq!(served.now(), 0);
        server.clock_ahead(0);
        get("cladefs/absent");
        let host = crate::clock::Clock::Host.now();
        assert!(served.now() <= host && served.now() + 2_000 >= host, "{}", served.now());
    }

    /// Board 8467: a bucket over HTTP keeps its backend's clock, as its responses date it, whatever
    /// the host's: none before its first response; then never ahead of the backend's, and within
    /// a second and a round trip of it, of the date its backend gives what it writes too, the
    /// backend's clock an hour behind the host's or ahead, or stepped forward a day. Its probe
    /// passes it; it refuses one whose backend dates no response, and one whose clock reads ahead
    /// of the backend's by more than [`crate::clock::SKEW`], as the host's clock did.
    #[test]
    fn buckets_keep_their_backends_clock() {
        use crate::clock::{Clock, SKEW};
        let serving = runtime();
        let server = serving.block_on(async { S3Server::start(None) });
        let (hour, day) = (3_600_000, 86_400_000);
        let served = |ahead: i64| Clock::Host.now().checked_add_signed(ahead).unwrap();
        // Read as the backend's clock reads `served`: at most a second and a round trip behind it.
        let reads = |now: u64, served: u64| {
            assert!(now <= served && now + 2_000 >= served, "{now} for {served}");
        };
        for ahead in [-hour, hour] {
            server.clock_ahead(ahead);
            let bucket = s3(&server, "clock", &Client::DEFAULT);
            assert_eq!(bucket.now(), 0);
            let put = bucket.put("dated", vec![1].into(), PutMode::Overwrite);
            serving.block_on(put).unwrap();
            let modified = serving.block_on(bucket.head("dated")).unwrap().modified;
            reads(bucket.now(), served(ahead));
            assert!(modified <= bucket.now() && bucket.now() < modified + 2_000, "{modified}");
            server.clock_ahead(ahead + day);
            serving.block_on(bucket.head("dated")).unwrap();
            reads(bucket.now(), served(ahead + day));
            assert_eq!(serving.block_on(probe(&*bucket, 1)), Ok(()));
        }

        server.dating(false);
        let undated = s3(&server, "undated", &Client::DEFAULT);
        let key = format!("probe/{}", base32_id::Padded(1));
        let none = "the bucket's clock has no time: none of its backend's responses carried a \
                    date it could read (`Date`)";
        assert_eq!(serving.block_on(probe(&*undated, 1)), Err(format!("{key}: {none}")));
        server.dating(true);
        let ahead = Watched::new(s3(&server, "ahead", &Client::DEFAULT), SKEW + 2_000, None);
        let refused = serving.block_on(probe(&ahead, 1)).unwrap_err();
        let said = "ms ahead of the date its backend gave it";
        assert!(refused.starts_with(&key) && refused.contains(said), "{refused}");
    }

    /// board 5861: what a bucket refuses for good, its helpers that retry failures till they pass
    /// return at once, saying it, a request each: a key past S3's 1,024 bytes, the request's
    /// (`400 Bad Request`, as the collector's moves of moves once made, TASK-572); and every
    /// request, credentials the bucket no longer takes (`403 Forbidden`, S3's; `401
    /// Unauthorized`, GCS's), the credentials'. A failure that passes, a server's (5xx), stays
    /// one: `object_store` retries it first, for minutes, which this test waits for no more than
    /// the collector does.
    #[test]
    fn refusals_are_told_from_failures() {
        use crate::bucket::{Refusal, Refused};
        let serving = runtime();
        let server = serving.block_on(async { S3Server::start(None) });
        let bucket = s3(&server, "refusals", &Client::DEFAULT);
        let refused = |key: &str, refusal| Err(Refused { key: key.to_owned(), refusal });

        // Its key, under the bucket's prefix, one byte past S3's longest.
        let long = "k".repeat(layout::MAX_KEY - "refusals/".len() + 1);
        let created = serving.block_on(crate::bucket::create(&*bucket, &long, vec![1]));
        assert_eq!(created.err(), refused(&long, Refusal::Request).err());
        let fits = &long[1..];
        let created = serving.block_on(crate::bucket::create(&*bucket, fits, vec![1]));
        assert!(matches!(created, Ok(crate::bucket::Created::Created(_))));

        server.answering(Some(403));
        let got = serving.block_on(crate::bucket::get(&*bucket, fits)).map(|_| ());
        assert_eq!(got, refused(fits, Refusal::Denied));
        server.answering(Some(401));
        let listed = serving.block_on(crate::bucket::list(&*bucket, "")).map(|_| ());
        assert_eq!(listed, refused("", Refusal::Denied));
        server.answering(Some(400));
        let deleted = serving.block_on(crate::bucket::delete(&*bucket, fits));
        assert_eq!(deleted, refused(fits, Refusal::Request));
        // None retried: the get's one; the listing's one; the delete's one.
        let answered = [("too-long", 1), ("put", 1), ("answered", 3)];
        assert_eq!(server.requests(), std::collections::BTreeMap::from(answered));
    }

    /// board 5528: what the registry records of a bucket, found by a bucket whose copy of it lacks
    /// that bucket, is read in the root bucket's, asked once: the root out of reach, it fails at
    /// once, as the request it is for does, rather than waiting for the root; the root back, it
    /// finds it.
    #[test]
    fn registry_misses_needing_a_root_out_of_reach_fail_at_once() {
        use crate::identity::{self, Details};
        runtime().block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let path = |number: u16| dir.path().join(format!("bucket-{number}"));
            let url = |number| format!("file://{}", path(number).display());
            let [root, second, third] = [1, 2, 3].map(|number| {
                std::fs::create_dir(path(number)).unwrap();
                ObjectBucket::from_url(&url(number)).unwrap()
            });
            let root_details = Details { url: Some(url(1)), ..Details::default() };
            identity::format(&root, &root_details).await.unwrap();
            let added = |number| Details { url: Some(url(number)), ..Details::default() };
            let (_, registry) =
                identity::add(&root, &root_details, &second, added(2)).await.unwrap();
            identity::spread(&registry, &second).await.unwrap();
            identity::add(&root, &root_details, &third, added(3)).await.unwrap();
            let registered = Registered::new(Vec::new());

            // Buckets reached from now on out of reach, the root among them, as their faults say:
            // each request failing as a bucket out of service answers it.
            let faults = dir.path().join("faults");
            std::fs::write(&faults, "failing=unavailable").unwrap();
            // SAFETY: nextest runs each test in a process of its own, whose other threads don't
            // read the environment.
            unsafe { std::env::set_var("CLADEFS_BUCKET_FAULTS", &faults) };
            let found = registered.entry(&second, 3).await.map(|entry| entry.number);
            let unread = |found: &Result<u16, crate::Error>| match found {
                Err(crate::Error::Read(why)) => why.starts_with(layout::IDENTITY),
                _ => false,
            };
            assert!(unread(&found), "{found:?}");

            std::fs::write(&faults, "").unwrap();
            let found = registered.entry(&second, 3).await.map(|entry| entry.number);
            assert_eq!(found, Ok(3));
        });
    }

    /// board 5861, TASK-572: a sweep over a bucket refusing its requests, as one whose credentials
    /// were revoked, ends, its listing of the cells or its pass over one failed, rather than
    /// retrying either for ever, as `cladefs-gc --every` would have, collecting nothing more.
    #[test]
    fn sweeps_end_on_a_bucket_refusing_them() {
        use crate::sweep::{Mode, Sweep};
        let serving = runtime();
        let server = serving.block_on(async { S3Server::start(None) });
        let bucket = s3(&server, "refused-sweeps", &Client::DEFAULT);
        let cell = test_cell(1);
        serving.block_on(lifecycle::create(&*bucket, cell, 1)).unwrap();
        server.answering(Some(403));
        let sweep =
            Sweep { mode: Mode::Delete, at_once: 1, lease: 60_000, slowest: 0, spawned: false };
        // cladefs: the listing's refusal its own outcome, no pass's (board 8509).
        let swept = serving.block_on(sweep.run(&bucket, |_| {}));
        assert_eq!((swept.visited, swept.failed), (0, 0));
        let Some(crate::sweep::Stopped::Unlisted(_)) = swept.stopped else { panic!("{swept:?}") };
        assert!(!swept.complete());
        let swept = serving.block_on(sweep.run_cell(&bucket, cell, |_| {}));
        assert_eq!((swept.visited, swept.failed, swept.stopped), (1, 1, None));
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
            assert!(own.block_on(get()).unwrap().is_none());
        };
        // The first, at once.
        own();
        let connects = |request: &dyn Fn()| {
            let before = server.connections();
            request();
            server.connections() - before
        };
        assert_eq!([(); 3].map(|()| connects(&own)), [1, 1, 1]);
        // A thread's, as each own runtime is: on a multi-threaded one, a connection's task, on a
        // worker of its own, could still be ending the last response when the next request looked
        // for an idle connection, and that one connected anew on a loaded host (board 7058).
        let shared = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let on_shared = || assert!(shared.block_on(get()).unwrap().is_none());
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
            crate::bucket::delete_all(&*bucket, &keys).await.unwrap();
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

    /// TASK-208: the latency every request to `bucket` waits, CLADEFS_BUCKET_LATENCY_MS, set before
    /// it was made.
    const LATE: u64 = 5;

    /// TASK-208: a bucket made under CLADEFS_BUCKET_LATENCY_MS, `bucket`, does what every bucket
    /// must, its requests each [`LATE`] ms late, one after another.
    async fn conforms_late(bucket: &ObjectBucket) {
        let started = std::time::Instant::now();
        crate::bucket::conformance(bucket, "conformance").await;
        let requests: u64 = bucket.stats().requests.iter().sum();
        let took = started.elapsed();
        assert!(
            took >= std::time::Duration::from_millis(requests * LATE),
            "{requests} requests took {took:?}"
        );
    }

    /// TASK-208: sets CLADEFS_BUCKET_LATENCY_MS to [`LATE`].
    fn late() {
        // SAFETY: nextest runs each test in a process of its own, whose other threads don't read
        // the environment yet.
        unsafe { std::env::set_var("CLADEFS_BUCKET_LATENCY_MS", LATE.to_string()) };
    }

    /// TASK-208: under CLADEFS_BUCKET_LATENCY_MS, a bucket of files conforms, late: its reads,
    /// which object_store's throttle, before TASK-330.3, panicked on (`unimplemented!`), as well as
    /// its writes.
    #[test]
    fn file_buckets_conform_late() {
        late();
        let dir = tempfile::tempdir().unwrap();
        runtime().block_on(conforms_late(&file(dir.path())));
    }

    /// TASK-208: under CLADEFS_BUCKET_LATENCY_MS, a bucket over S3 conforms, late: its listings by
    /// delimiter, a page a request, too.
    #[test]
    fn s3_buckets_conform_late() {
        late();
        runtime().block_on(async {
            let server = S3Server::start(None);
            conforms_late(&s3(&server, "conformance", &Client::DEFAULT)).await;
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
            let run = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
            let run = run.as_nanos();
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

    /// doc-29: offset-checked append conformance and `probed_append` over S3 HTTP
    /// (`x-amz-write-offset-bytes` + `MaxParts`), local `file://`, and in-memory `memory://`.
    #[test]
    fn append_probe_and_max_parts_over_s3_local_and_memory() {
        runtime().block_on(async {
            // In-memory bucket
            let mem = ObjectBucket::new(Arc::new(object_store::memory::InMemory::new()), "mem");
            mem.probed_append(1).await.unwrap();

            // Local file:// bucket
            let dir = tempfile::tempdir().unwrap();
            let file_url = format!("file://{}", dir.path().display());
            let local = ObjectBucket::from_url(&file_url).unwrap();
            local.probed_append(2).await.unwrap();

            // S3Server over real HTTP with x-amz-write-offset-bytes and MaxParts = 3
            let server = S3Server::start(None);
            let bucket = s3(&server, "fastwal", &Client::DEFAULT);
            bucket.probed_append(3).await.unwrap();

            server.set_max_appends(3);
            let key = "wal/seg0";
            assert_eq!(bucket.append(key, 0, crate::Bytes::from_static(b"aaa")).await.unwrap(), 3);
            assert_eq!(bucket.append(key, 3, crate::Bytes::from_static(b"bb")).await.unwrap(), 5);
            assert_eq!(bucket.append(key, 5, crate::Bytes::from_static(b"c")).await.unwrap(), 6);
            // 4th append exceeds max_appends (3) -> Precondition!
            assert_eq!(
                bucket.append(key, 6, crate::Bytes::from_static(b"d")).await,
                Err(crate::bucket::Error::Precondition)
            );
            assert_eq!(bucket.get(key).await.unwrap().bytes.as_slice(), b"aaabbc");
        });
    }

    /// doc-29: signed S3 Express directory-bucket appends sign with service `s3express`, bind
    /// `x-amz-write-offset-bytes` in `SignedHeaders`, and require `x-amz-object-size`.
    #[test]
    fn signed_s3_express_append_uses_s3express_scope_and_signs_write_offset() {
        use crate::Bytes;
        use http_body_util::{BodyExt as _, Full};
        use hyper::body::Incoming;
        use hyper::service::service_fn;
        use hyper_util::rt::TokioIo;
        use std::sync::Mutex;

        runtime().block_on(async {
            let captured: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
            let return_size = Arc::new(std::sync::atomic::AtomicBool::new(true));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();

            let captured_srv = captured.clone();
            let return_size_srv = return_size.clone();
            let server = tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else { break };
                    let captured = captured_srv.clone();
                    let return_size = return_size_srv.clone();
                    tokio::spawn(async move {
                        let svc = service_fn(move |req: hyper::Request<Incoming>| {
                            let captured = captured.clone();
                            let return_size = return_size.clone();
                            async move {
                                let auth = req
                                    .headers()
                                    .get("authorization")
                                    .and_then(|v| v.to_str().ok())
                                    .unwrap_or("")
                                    .to_owned();
                                let offset = req
                                    .headers()
                                    .get("x-amz-write-offset-bytes")
                                    .and_then(|v| v.to_str().ok())
                                    .unwrap_or("")
                                    .to_owned();
                                // Drain the PUT body before replying so HTTP/1.1 keep-alive stays clean.
                                drop(req.into_body().collect().await);
                                captured.lock().unwrap().push((auth, offset));
                                let mut resp = hyper::Response::builder().status(200);
                                if return_size.load(Ordering::Relaxed) {
                                    resp = resp.header("x-amz-object-size", "9");
                                }
                                Ok::<_, hyper::Error>(resp.body(Full::new(Bytes::new())).unwrap())
                            }
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), svc)
                            .await;
                    });
                }
            });

            let url = "s3://fastwal--use1-az4--x-s3/prefix";
            let options = [
                ("aws_endpoint".to_owned(), format!("http://{addr}")),
                ("aws_region".to_owned(), "us-east-1".to_owned()),
                ("aws_access_key_id".to_owned(), "AKIAIOSFODNN7EXAMPLE".to_owned()),
                (
                    "aws_secret_access_key".to_owned(),
                    "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_owned(),
                ),
                ("aws_allow_http".to_owned(), "true".to_owned()),
            ];
            let bucket = ObjectBucket::reached(reach(url, options, &Client::DEFAULT).unwrap());
            let new_len = bucket.append("wal/0", 5, Bytes::from_static(b"abcd")).await.unwrap();
            assert_eq!(new_len, 9);

            let seen = captured.lock().unwrap().clone();
            assert_eq!(seen.len(), 1);
            let (auth, offset) = &seen[0];
            assert_eq!(offset, "5");
            assert!(
                auth.contains("/us-east-1/s3express/aws4_request"),
                "expected s3express credential scope, got {auth}"
            );
            assert!(
                auth.contains("x-amz-write-offset-bytes"),
                "expected x-amz-write-offset-bytes in SignedHeaders, got {auth}"
            );

            // Missing x-amz-object-size on S3 Express must fail rather than silently succeed.
            return_size.store(false, Ordering::Relaxed);
            assert!(bucket.append("wal/0", 5, Bytes::from_static(b"abcd")).await.is_err());

            server.abort();
        });
    }
}
