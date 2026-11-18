// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a store's branch kept in object storage, and its uploader (backlog doc-7, Uploads;
//! doc-14; TASK-28.5, TASK-165.9), which runs on the host's runtime, its uploads within the host's
//! budget ([`Runtime`]). cladefs's `bucket` keeps the host's configuration, budgets, runtime and
//! errno mapping.
//!
//! **Opening a store** ([`open`]) is the same whether its branch was here, elsewhere or never
//! anywhere (doc-14, Opening a store): the branch's writer takes its head, with the epoch of
//! its cell's taking by the store's placement, or the host's, recovers it and fences its end
//! (`extent_bucket::Writer`); the store opens as the bucket holds the branch, its write-ahead log
//! put after the branch's end by its commits, what of it landed acknowledged and the rest stamped
//! anew on the writer's chain ([`Recovering::open`]). A log another writer's commits moved past
//! refuses to open (`extent_bucket::Error::Diverged`, `ESTALE`) until `cladefsctl take` drops it;
//! one that plainly diverged before the head is taken refuses before, fencing no writer. A branch
//! with no record is made, a root (`extent_bucket::branch::create`).
//!
//! **A point** takes no head (doc-7, Points): its store holds its state, recovered, which nothing
//! writes, and no log; nothing it does goes to the bucket but its forks' records
//! ([`Uploader::forked`], `extent_bucket::branch::fork_point`). Any number of hosts hold one at
//! once.
//!
//! **The uploader** hands the store's write-ahead log's segments, framed, whose commits are durable
//! ([`Store::closed`]), to the writer (`Writer::add_segment`), each the object it is in the bucket,
//! or stamped anew after a takeover; uploads them, [`IN_FLIGHT`] at once at most, each read only as
//! its upload goes, within the host's budget; and acknowledges them in order once a read of the
//! head shows them the branch's ([`Store::acknowledged`]): their extents are the bucket's from then
//! on, their files the host's cache's. The store closes its open segment for the uploader when a
//! commit waits for its upload, at a fork, as its interval runs out, and as the uploader stops:
//! dropping it uploads what the store made durable, its open segment's commits too (TASK-192).
//! Commits waiting to be durable (`COMMIT_UPLOADED`) complete once acknowledged.
//!
//! A fork the store makes is a fork of the branch once the segment its commit ends is
//! acknowledged, which gives the child its record, and checkpointed ([`Uploader::forked`]): the
//! child's store is made only then, opening it recovering the child from the bucket. A fork that
//! fails before is deleted in the bucket, as the fork's alone ([`Uploader::unfork`]).
//!
//! A failure of its writer's stops the uploader for good, and the commits waiting for uploads fail
//! with it from then on: its writer fenced, its swap of the branch's head refused as a later writer
//! took it, its branch deleted, or the log diverged, another writer's commits past it. Only opening
//! the store anew takes the branch again then. (Until TASK-407, a writer displaced, an earlier
//! writer's segment landed late where its next went, took the branch again, TASK-204: no segment
//! lands where another's goes now, each at a key of its writer's epoch; doc-7, The head.)

use crate::{Dir, DirHold, File, HeldParts, Opening, Recovering, Store, read_object};
use event_listener::{Event, Listener as _};
use extent_bucket::cache::Cache;
use extent_bucket::fetch::Budget;
use extent_bucket::keys::{self, CellKeys, Ring, RingHold};
use extent_bucket::known::Learned;
use extent_bucket::layout::{Branch, Chain, Child, Origin, epoch_of};
use extent_bucket::object_bucket::ObjectBucket;
use extent_bucket::record::{Forked, Kind, Record};
use extent_bucket::wal::TakenBranch;
use extent_bucket::{Body, Objects, Recovered, Taker, Writer, branch, cell};
use futures::StreamExt as _;
use futures::future::{BoxFuture, Either, FutureExt as _};
use futures::stream::FuturesOrdered;
use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// How many of a store's segments upload at once: as many of a segment's size as the host's
/// uploads may hold at once ([`UPLOAD_BYTES`]), so that a store catching up uploads as many at once
/// as the host's budget lets it, about ten keeping 1 GB/s going with PUTs of 0.35 s, and holds no
/// more of its segments' files open for them (TASK-79.19).
pub const IN_FLIGHT: usize = (UPLOAD_BYTES / SEGMENT) as usize;

/// About the most bytes of a segment: a store's segments' size
/// (`extent_store::Options::segment_size`).
const SEGMENT: u64 = 32 << 20;

/// How many segments go between checkpoints, by default ([`Options::checkpoint_every`]).
pub const CHECKPOINT_EVERY: u64 = 64;

/// The most bytes of objects a host's uploads hold at once: its budget's ([`Runtime::uploads`]).
pub const UPLOAD_BYTES: u64 = 512 << 20;

/// A step of the uploader's, which the failpoint of its name fails (the `fail` crate's): the
/// function it is in returns a [`Failure`] there, or whatever a test set the failpoint to do
/// happens.
macro_rules! failing {
    ($name:literal) => {
        fail::fail_point!($name, |_| Err(Failure::msg(concat!("failpoint ", $name))))
    };
}

/// What a host gives its stores' uploaders: the runtime they run on, sized by the host, not by
/// how many stores it serves, as each waits on object storage mostly, and does its disk work in
/// `block_in_place`, which leaves the runtime's threads to the others (TASK-34); and the budget of
/// the PUTs of objects they upload, each holding its object's bytes while it goes, bounded in
/// bytes, [`UPLOAD_BYTES`], so that a host whose stores are far behind holds no more than these
/// at once, whatever their number (TASK-79.19), and in number.
///
/// cladefs: and the keys it encrypts and reads under (TASK-411.3): its KMS, if it has one, and the
/// keys its fetchers share, which each store's are held in while it is open.
#[derive(Clone)]
pub struct Runtime {
    pub handle: tokio::runtime::Handle,
    pub uploads: Arc<Budget>,
    pub keys: keys::Host,
}

/// How many stores' uploaders run in this process ([`open`]): until each, its store dropped, has
/// uploaded what its store made durable, and stopped.
static UPLOADERS: std::sync::LazyLock<tokio::sync::watch::Sender<usize>> =
    std::sync::LazyLock::new(|| tokio::sync::watch::channel(0).0);

/// How many stores' uploaders run in this process, as it changes: what an extent daemon's drain
/// waits to see none of (cladefs TASK-141.4).
pub fn uploaders() -> tokio::sync::watch::Receiver<usize> {
    UPLOADERS.subscribe()
}

/// Runs `future` to its end, on the calling thread, as what is called from no runtime's task does:
/// in the context of the bucket runtime ([`bucket_runtime`]), whose workers run what it spawns,
/// the connections to buckets it opens among them, which live on after it in their buckets' pools.
/// cladefs: on that runtime, not one of its own (TASK-449's review): a runtime of each call's own
/// closed the connections it opened as it returned, each call connecting anew (extent_bucket's
/// `requests_on_runtimes_dropped_after_connect_anew`); one of the calls' own beside the host's
/// served, from its own few threads, the sockets of the connections they left in the pool, which
/// the host's uploads and fetches went on to use.
pub fn block_on<F: std::future::Future>(future: F) -> Result<F::Output, Failure> {
    Ok(bucket_runtime()?.block_on(future))
}

/// cladefs: the runtime the host's uploaders and fetches run on (TASK-34, TASK-449's review), one for the
/// process: every store's uploader and fetches, and the calls from no runtime's task
/// ([`block_on`]), so that one runtime serves every connection to the host's buckets. A bucket's
/// connections are tasks of the runtime they opened on (`reqwest`'s, as `object_store` uses it,
/// spawns them there), which stay in the bucket's pool for the next request, whatever its
/// runtime. Sized by the host, not by how many stores it serves, as each store waits on object
/// storage mostly, and does its disk work in `block_in_place`, which leaves the runtime's threads
/// to the others. Its threads hold their lists of the shared locks they take from their start
/// ([`crate::shared`]). Made at its first need; a making that fails, a thread not spawned, is
/// made anew at the next, not kept failed for the process's life.
pub fn bucket_runtime() -> Result<&'static tokio::runtime::Runtime, Failure> {
    static BUCKET_RUNTIME: Retried<tokio::runtime::Runtime> = Retried::new();
    BUCKET_RUNTIME.get_or_make(|| {
        failing!("upload::bucket_runtime");
        let threads = std::thread::available_parallelism().map_or(2, |cpus| cpus.get().clamp(2, 8));
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(threads)
            .thread_name("cladefs bucket")
            .on_thread_start(crate::shared::prepare_thread)
            .enable_all()
            .build()
            .map_err(|err| Failure::local(err, "the bucket runtime"))
    })
}

/// cladefs: a value made at its first need and kept once made, a making that fails keeping
/// nothing: the next need makes it anew (TASK-449's review), one making at a time.
struct Retried<T> {
    made: std::sync::OnceLock<T>,
    making: Mutex<()>,
}

impl<T> Retried<T> {
    const fn new() -> Self {
        Self { made: std::sync::OnceLock::new(), making: Mutex::new(()) }
    }

    fn get_or_make<E>(&self, make: impl FnOnce() -> Result<T, E>) -> Result<&T, E> {
        if let Some(made) = self.made.get() {
            return Ok(made);
        }
        let _making = self.making.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(made) = self.made.get() {
            return Ok(made);
        }
        let made = make()?;
        Ok(self.made.get_or_init(|| made))
    }
}

/// Deletes `branch` in `bucket` (`extent_bucket::branch::delete`), as the placement whose fencing
/// token is `token`: its writer stops at its next acknowledgement, no writer takes it again, and
/// the collector reclaims what no other branch inherited from it, as its forks' branches live on
/// (TASK-39.4). Deleting it again does nothing. A later placement's branch, whose token is higher,
/// is not deleted (`extent_bucket::Error::Stale`, TASK-45.1).
///
/// cladefs: its lease ended at its cell's epoch, a placement a later one took the cell from deleting
/// nothing (`Stale`), and the cell's other volumes' writers going on; then the volume naming it is
/// taken out of the cell (`extent_bucket::cell::forget`, TASK-364). An encrypted branch's forks to
/// other cells it records get its checkpoint at their fork points under its keys, its cell's KEK
/// unwrapped by `kms`, the host's (`branch::delete`, TASK-411.4).
pub fn delete_branch(
    bucket: &ObjectBucket,
    branch: Branch,
    token: u64,
    kms: &Arc<keys::CachingKms>,
) -> Result<(), Failure> {
    Ok(block_on(async {
        branch::delete(bucket, branch, rand::random(), token, kms).await?;
        cell::forget(bucket, branch.cell, branch.id).await
    })??)
}

/// Why uploading stopped, or a branch could not be uploaded to, or opened: the error as it was,
/// typed, which cladefs's `bucket` maps to its errno, in the one place.
#[derive(Clone, Debug, thiserror::Error)]
pub enum Failure {
    /// What object storage's protocol found of the branch, as `extent_bucket` says it: another
    /// writer took it (`Fenced`), it was deleted, a later placement holds it
    /// (`Stale`, TASK-39.7), another writer's commits moved it on from the store's write-ahead log
    /// (`Diverged`), another branch has a fork's child's ID (`IdTaken`, TASK-79.82), or what it
    /// needs is gone or corrupt.
    #[error(transparent)]
    Bucket(#[from] extent_bucket::Error),
    /// The point a store is of was deleted: it forks no more, and its forks live on.
    #[error("the point was deleted")]
    PointDeleted,
    /// This host's own: its files, a runtime, the uploader gone, as the error says.
    #[error("{0:#}")]
    Local(Arc<anyhow::Error>),
}

impl Failure {
    /// This host's failure, saying `message`.
    pub fn msg(message: impl std::fmt::Display + std::fmt::Debug + Send + Sync + 'static) -> Self {
        Self::Local(Arc::new(anyhow::Error::msg(message)))
    }

    /// This host's failure of `what`, with `error`.
    pub fn local(
        error: impl std::error::Error + Send + Sync + 'static,
        what: impl std::fmt::Display + Send + Sync + 'static,
    ) -> Self {
        Self::Local(Arc::new(anyhow::Error::new(error).context(what)))
    }
}

/// Failures are the same as they say, this host's own as their errors' messages do.
impl PartialEq for Failure {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Bucket(error), Self::Bucket(other)) => error == other,
            (Self::PointDeleted, Self::PointDeleted) => true,
            (Self::Local(error), Self::Local(other)) => {
                format!("{error:#}") == format!("{other:#}")
            }
            (Self::Bucket(_) | Self::PointDeleted | Self::Local(_), _) => false,
        }
    }
}

impl Eq for Failure {}

/// The store's errors are this host's, but what object storage's protocol found of the branch, as
/// a write-ahead log diverged from it.
impl From<anyhow::Error> for Failure {
    fn from(error: anyhow::Error) -> Self {
        match error.downcast::<extent_bucket::Error>() {
            Ok(error) => Self::Bucket(error),
            Err(error) => Self::Local(Arc::new(error)),
        }
    }
}

/// How an uploader uploads.
#[derive(Clone, Debug)]
pub struct Options {
    /// How long local commits wait at most before they are uploaded.
    pub interval: Duration,
    /// How many commits may wait to be uploaded: the next waits, while object storage is out of
    /// reach, rather than the lag grow without bound (doc-7, decision 3).
    pub max_lag: u64,
    /// How many bytes of the store's write-ahead log may wait to be acknowledged, likewise: what
    /// the local disk holds until uploaded.
    pub max_lag_bytes: u64,
    /// The host's cache of the bucket's objects, which its stores share: what the bucket
    /// acknowledged of a store's goes into it (TASK-165.8).
    pub cache: Option<Arc<Cache>>,
    /// cladefs: what holds the parts of segments packed as they fill, which its stores share, if
    /// the host has it ([`crate::Opening::held`], TASK-400): else each its own.
    pub held: Option<Arc<HeldParts>>,
    /// The fencing token of a store never placed: its host's placement's.
    pub token: u64,
    /// cladefs: how many segments go between checkpoints, which recoveries start from: the segments
    /// a checkpoint covers are no longer read by them, and the collector may compact them.
    pub checkpoint_every: u64,
    /// cladefs: how long the commits in a segment the guest is filling wait past the interval for
    /// its next commit, which seals what it writes in it in place, before the interval closes it
    /// anyway (`Store::close_now_unless_filling`, TASK-92.41): a commit waiting for its upload
    /// closes it at once. 30 s: eight VMs writing 768 MiB each on one disk with their bucket took
    /// 10 s and more each, the interval closing the segments they filled at 10 s, their commits
    /// copying out a segment's worth each.
    pub filling_wait: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(1),
            max_lag: 4096,
            max_lag_bytes: 1 << 30,
            cache: None,
            held: None,
            token: 0,
            checkpoint_every: CHECKPOINT_EVERY,
            filling_wait: Duration::from_secs(30),
        }
    }
}

/// Opens the store `recovering` recovered, keeping its branch in `bucket` (see the module's
/// docs): as `holder` of the branch's head, which must differ from every other taking's, with
/// `store_options`, its uploader uploading as `options` say, on `runtime`. Returns the store and
/// its uploader; or why it can't be opened: its log diverged, its branch deleted, a later
/// placement's, or out of reach.
pub fn open<D: Dir>(
    recovering: Recovering<D>,
    store_options: crate::Options,
    bucket: Arc<ObjectBucket>,
    (holder, options): (u64, Options),
    runtime: &Runtime,
) -> Result<(Arc<Store<D>>, Uploader<D>), Failure> {
    let sleep: Clock = Arc::new(|interval| tokio::time::sleep(interval).boxed());
    let shared = Shared::new(options, runtime.uploads.clone(), sleep, runtime.keys.clone());
    let (started, start) = std::sync::mpsc::channel();
    let (stop, stopped) = std::sync::mpsc::channel();
    let (uploader, uploads) = (shared.clone(), bucket.clone());
    // What the host learned for this opening, taken as it begins (TASK-449's review).
    let learned = learned(&*bucket, &recovering);
    UPLOADERS.send_modify(|running| *running += 1);
    // A task, not a thread of its own (TASK-34).
    runtime.handle.spawn(async move {
        // Its conditional writes checked before any is relied on (doc-7, The head).
        match uploads.probed(holder).await {
            Ok(()) => {
                let recovering = (recovering, learned);
                uploader.run(recovering, store_options, uploads, holder, started).await
            }
            Err(err) => started.send(Err(Failure::msg(err))).unwrap(),
        }
        // The store let go of before the uploader says it stopped: so that dropping this returns
        // with nothing of the uploader's holding the store open, which the process may open again
        // at once (`Dir::claim` refusing a second opening), as cladefsd does a store an image it
        // imported made (TASK-165.6).
        drop(uploader);
        UPLOADERS.send_modify(|running| *running -= 1);
        let _ = stop.send(());
        fail::fail_point!("upload::stopped");
    });
    let opened = match start.recv() {
        Ok(Ok(store)) => store,
        Ok(Err(err)) => {
            let _ = stopped.recv();
            return Err(err);
        }
        Err(_) => return Err(Failure::msg("the uploader stopped")),
    };
    let dir = opened.hold_dir()?;
    let uploader = Uploader { shared, bucket, stopped: Mutex::new(Some(stopped)), _dir: dir };
    Ok((opened, uploader))
}

/// A store's uploader (see the module's docs). Dropping it closes the store's open segment, if its
/// store is open still, uploads what the store made durable, then stops.
pub struct Uploader<D: Dir> {
    shared: Arc<Shared<D>>,
    /// The bucket the store's branch is kept in.
    bucket: Arc<ObjectBucket>,
    /// Says the uploader stopped, once it has.
    stopped: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    /// The store's directory, held taken for this process until the uploader stopped, last of its
    /// fields to go (TASK-129).
    _dir: DirHold<D>,
}

pub(crate) struct Shared<D: Dir, B: extent_bucket::Bucket = ObjectBucket> {
    /// The store, once opened.
    store: std::sync::OnceLock<Arc<Store<D>>>,
    /// The branch's writer, once it took the branch, and as it takes it again (TASK-204).
    writer: Mutex<Option<Arc<Writer<B>>>>,
    /// The fencing token its writers take the branch's head with, once one did.
    token: std::sync::OnceLock<u64>,
    /// The point the store is of, if it is one.
    point: std::sync::OnceLock<Point>,
    /// The origins of the forks of the segments the uploader took, by their children: what a fork
    /// that fails deletes its child by (TASK-165.7).
    forks: Mutex<std::collections::BTreeMap<Branch, Origin>>,
    progress: Mutex<Progress>,
    /// Notified as the progress changes: what waits for uploads listens to it, blocking or not
    /// (TASK-43.6).
    changed: Event,
    /// Wakes the uploader: a commit waits, or it is to stop.
    wake: tokio::sync::Notify,
    options: Options,
    /// The host's budget of uploads, which its writer uploads within.
    uploads: Arc<Budget>,
    /// When its interval is up ([`Clock`]).
    clock: Clock,
    /// The holders of its takings after the first, and the owners of what it makes: from the
    /// first taking's holder on ([`Shared::holder`]).
    holders: Mutex<Holders>,
    /// cladefs: the host's keys ([`Runtime::keys`], TASK-411.3).
    keys: keys::Host,
    /// cladefs: the store's keys, held among the host's for as long as the uploader lives.
    held: Mutex<Option<RingHold>>,
}

/// The holders an uploader's takings of its branch's head after the first take, and the owners of
/// what it makes, each to differ from every other taking's or maker's: a sequence the first
/// taking's holder, which its caller drew, seeds (SplitMix64, a bijection of its state, which steps
/// by an odd constant), as unlikely to meet another's as a draw of their own; and the same in a
/// run of a seed of the simulation as in the next, whose seed draws the first (TASK-380).
#[derive(Default)]
struct Holders(u64);

impl Holders {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

/// What an uploader's interval runs out on: a sleep of the host's runtime's, or, in the
/// simulation, of its scheduler's, which says when each runs out (TASK-88).
pub(crate) type Clock = Arc<dyn Fn(Duration) -> BoxFuture<'static, ()> + Send + Sync>;

/// Runs `work`, which blocks on the store's disk, on the uploaders' runtime: on its thread, which
/// hands its other tasks to another meanwhile; or as it is, on a thread no runtime runs, as the
/// simulation's (`tokio::task::block_in_place` runs it so there).
#[allow(clippy::disallowed_methods, reason = "the work waits on the disk alone")]
fn blocking<T>(work: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(work)
}

/// A point a store holds: what its record names, and the point. cladefs: or a version of its
/// branch, of which it is a copy, and the segment its branch's head names it at (TASK-476): its
/// forks fork the version, which its branch's head must name still.
struct Point {
    origin: Origin,
    of: Forked,
}

/// cladefs: what a commit stalled behind object storage waits for, said as it stalls: the bytes not
/// uploaded, and of them those past the lag bound, which a stall over the bound waits on alone,
/// the commits not uploaded and the oldest's age, which a slow upload shows (TASK-92.52).
fn stalled(pending: u64, max: u64, commits: u64, oldest: Option<Duration>) -> String {
    let oldest = oldest.map_or("none seen yet".to_owned(), |age| {
        format!("the oldest seen {} ms ago", age.as_millis())
    });
    format!(
        "cladefs: commits wait for object storage: {pending} bytes not uploaded, {} past the lag \
         bound of {max}; {commits} commits not uploaded, {oldest}",
        pending.saturating_sub(max)
    )
}

#[derive(Default)]
struct Progress {
    /// The last epoch uploaded.
    uploaded: u64,
    /// The last epoch uploaded whose forks are made in object storage: their children have their
    /// records, and a checkpoint at them, which their recovery starts from.
    forked: u64,
    /// Why uploading stopped, if it did.
    failed: Option<Failure>,
    /// Whether a commit waits to be uploaded: the next batch goes now.
    wanted: bool,
    /// How many commits waited for the lag to shrink.
    stalls: u64,
    /// The lowest sequence number the guest may mint: its writer's epoch's range (doc-7).
    next_sequence: u64,
    stop: bool,
    /// Whether, stopping, it stops at once: uploading nothing more, its uploads in flight given up
    /// on, no checkpoint written ([`Uploader::cut_short`]).
    cut: bool,
    /// The writer's taking of the branch's head, once it took it.
    taking: Option<Taking>,
    /// The store's latest commits not uploaded, as the uploader saw them, and when it did: the
    /// first is when the oldest commit waiting was seen first (TASK-50.5).
    seen: VecDeque<(u64, SystemTime)>,
}

/// cladefs: a store's versions ([`Remote::versions`], TASK-476), by their commits' levels.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Versions {
    /// Those its branch's head names, uploaded, oldest first.
    pub uploaded: Vec<extent_bucket::layout::Version>,
    /// Those its commits cut that the head names not yet, local, by their commits' numbers.
    pub local: Vec<(u64, extent_bucket::layout::NewVersion)>,
}

/// A writer's taking of its branch's head: with the fencing token it took it with, of the epoch,
/// and when.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Taking {
    /// The fencing token it was taken with: the epoch of its cell's taking by this placement
    /// (`extent_bucket::cell::Held::epoch`, TASK-364).
    pub token: u64,
    /// Its epoch: one more than the taking's before.
    pub epoch: u64,
    /// When the writer took it.
    pub taken: SystemTime,
}

/// What a store's uploader has done, and has left to do (TASK-50.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UploadStatus {
    /// Its writer's taking of its branch's head, once taken.
    pub taking: Option<Taking>,
    /// The epoch of the store's latest commit uploaded.
    pub uploaded: u64,
    /// The bytes of its write-ahead log not yet acknowledged.
    pub pending_bytes: u64,
    /// When the uploader first saw the oldest commit not yet uploaded, if one is not: within its
    /// interval of when the commit was made.
    pub oldest_pending: Option<SystemTime>,
    /// How many commits waited for the lag to shrink ([`Uploader::stalls`]).
    pub stalls: u64,
}

impl Progress {
    /// Notes the store's commits through `epoch` uploaded.
    fn uploaded_through(&mut self, epoch: u64) {
        self.uploaded = self.uploaded.max(epoch);
        while self.seen.front().is_some_and(|(seen, _)| *seen <= self.uploaded) {
            self.seen.pop_front();
        }
    }

    /// Notes the forks of the store's commits through `epoch`, uploaded, made in object storage.
    fn forked_through(&mut self, epoch: u64) {
        self.forked = self.forked.max(epoch);
    }
}

impl<D: Dir> Uploader<D> {
    /// The store.
    fn store(&self) -> &Arc<Store<D>> {
        self.shared.store()
    }

    /// Waits until the commits up to `epoch` are uploaded and acknowledged.
    pub fn wait_uploaded(&self, epoch: u64) -> Result<(), Failure> {
        let shared = &self.shared;
        if shared.ask(epoch, false)? {
            return Ok(());
        }
        shared.wait(|progress| shared.reached(progress, epoch, false))
    }

    /// Returns once the commits up to `epoch` are uploaded and acknowledged, as
    /// [`Uploader::wait_uploaded`] does: a future, which holds no thread while it waits, as for a
    /// sync that object storage keeps waiting (TASK-43.6).
    pub fn uploaded(
        &self,
        epoch: u64,
    ) -> impl std::future::Future<Output = Result<(), Failure>> + Send + use<D> {
        let shared = self.shared.clone();
        async move {
            if shared.ask(epoch, false)? {
                return Ok(());
            }
            loop {
                let changed = {
                    let progress = shared.progress.lock().unwrap();
                    if let Some(reached) = shared.reached(&progress, epoch, false) {
                        return reached;
                    }
                    shared.changed.listen()
                };
                changed.await;
            }
        }
    }

    /// The epoch of the store's latest commit uploaded and acknowledged, as the uploader says,
    /// whether or not it stopped.
    pub fn last_uploaded(&self) -> u64 {
        self.shared.last_uploaded()
    }

    /// What the uploader has done, and has left to do.
    pub fn status(&self) -> Result<UploadStatus, Failure> {
        let shared = &self.shared;
        shared.observe()?;
        let pending_bytes = self.store().wal_bytes()?;
        let progress = shared.progress.lock().unwrap();
        Ok(UploadStatus {
            taking: progress.taking,
            uploaded: progress.uploaded,
            pending_bytes,
            oldest_pending: progress.seen.front().map(|(_, seen)| *seen),
            stalls: progress.stalls,
        })
    }

    /// Waits, if more commits wait to be uploaded than `Options::max_lag`, with `epoch`'s, or the
    /// write-ahead log holds more than `Options::max_lag_bytes`, until they no longer do.
    pub fn wait_lag(&self, epoch: u64) -> Result<(), Failure> {
        let (shared, epoch) = (&self.shared, epoch.saturating_sub(self.shared.options.max_lag));
        if shared.ask(epoch, true)? {
            return Ok(());
        }
        shared.wait(|progress| shared.reached(progress, epoch, true))
    }

    /// Makes the fork the store made of its branch with its commit `epoch`
    /// ([`Store::commit_fork`]) a fork of the branch in object storage, once the commits up to the
    /// fork are uploaded: the segment the fork's commit ends names `child`, whose record the
    /// acknowledgement makes, naming the branch and the segment as its origin, and a checkpoint at
    /// the fork is where its recovery starts. `child`'s ID may be another branch's by then, which
    /// took it after [`Uploader::child_free`] found it free (`extent_bucket::Error::IdTaken`): the
    /// fork failed, and the uploader goes on.
    ///
    /// A fork of a point commits nothing: the child's record is the fork, and fails if the point
    /// was deleted ([`Failure::PointDeleted`]; `branch::fork_point`).
    ///
    /// cladefs: a fork into another cell, `inheriting` given ([`Uploader::inheriting`]), then gives
    /// its child its keys, once its record is made: its cell made with the KEK from the host's
    /// pool, its record given its DEK and those it inherits, rewrapped under that KEK
    /// (`keys::give_inherited`, TASK-411.4). Failing, the fork fails, which its host deletes.
    ///
    /// cladefs: a fork of a point this host made says what it left of the child to the child's
    /// store's first opening here ([`Learned`], TASK-449): its record, and that it has no head. Its
    /// cell the child's store makes, encrypted, as it opens (every cell is, TASK-411.28; a fork
    /// holding it, made, at once with the reference's write is TASK-485's).
    pub fn forked(
        &self,
        epoch: u64,
        child: Child,
        inheriting: Option<keys::Inheriting>,
    ) -> Result<(), Failure> {
        failing!("upload::forked");
        let Made { mut record, cell } = self.fork_made(epoch, child)?;
        if let Some(inheriting) = inheriting {
            let holder = self.shared.holder();
            let given = block_on(keys::give_inherited(&*self.bucket, inheriting, holder))??;
            record = record.map(|_| given);
        }
        if let (Some(record), Some(known)) = (record, extent_bucket::Bucket::known(&*self.bucket)) {
            known.learn(child.branch, Learned { record, cell, headless: true });
        }
        Ok(())
    }

    /// Makes the fork the store made of its branch with its commit `epoch` a fork of the branch in
    /// object storage, its child's record and checkpoint ([`Uploader::forked`]). A fork of a point
    /// says what it made.
    fn fork_made(&self, epoch: u64, child: Child) -> Result<Made, Failure> {
        let shared = &self.shared;
        if let Some(point) = shared.point.get() {
            let origin = point.origin;
            let holding = async { None };
            let bucket = &*self.bucket;
            let (child, owner) = (child.branch, child.owner);
            let forking = async {
                match point.of {
                    Forked::Point(of) => {
                        branch::fork_point_with(bucket, child, of, origin, owner, holding).await
                    }
                    // cladefs: a version's, its checkpoint at the version, for a child of another
                    // cell, sealed under the store's keys, its branch's (TASK-476).
                    Forked::Version(id) => {
                        let keys = keys::Parents::Held(self.store().keys());
                        branch::fork_version_with(bucket, child, id, origin, owner, keys, holding)
                            .await
                    }
                }
            };
            return match block_on(forking)? {
                Err(extent_bucket::Error::Deleted) => Err(Failure::PointDeleted),
                Err(err) => Err(err.into()),
                Ok((record, cell)) => Ok(Made { record, cell: cell.flatten() }),
            };
        }
        let child = child.branch;
        // Its commit's segment uploads now, if it is not yet uploaded.
        shared.ask(epoch, false)?;
        shared.wait(|progress| match progress.forked >= epoch {
            true => Some(Ok(())),
            false => progress.failed.clone().map(Err),
        })?;
        match shared.writer() {
            Some(writer) if writer.refused(child) => {
                Err(extent_bucket::Error::IdTaken(child).into())
            }
            _ => Ok(Made::default()),
        }
    }

    /// cladefs: what a fork of the store to `child` gives its child, asked for before anything of the
    /// fork is made, its commit or its record, so that a fork refused makes nothing (TASK-411.4):
    /// if `child` is of another cell, no point, and the store's keys, which it inherits, are of
    /// branches of other cells than its own, those keys and a KEK for the child's cell from the
    /// host's pool; none otherwise. Refused if the host's KMS last heard a tenant of theirs
    /// revoked or destroyed, or as the KMS refuses a KEK (`keys::inheriting`). No KMS call is made
    /// with the pool not empty.
    pub fn inheriting(&self, child: Child) -> Result<Option<keys::Inheriting>, Failure> {
        let store = self.store();
        let (source, parent) = (store.keys(), store.description().branch);
        let kms = &self.shared.keys.kms;
        Ok(block_on(keys::inheriting(kms, source, parent, child))??)
    }

    /// Deletes `child` in object storage, a fork the store made that [`Uploader::forked`] failed to
    /// make: its segment may land after all, its writer taken over before acknowledging it, and the
    /// branch's next writer gives the child a record then, which would keep what it inherited for
    /// ever, no store holding it. Deleted first, it gets a record saying so, which the segment
    /// landing finds (TASK-79.78). Only a branch the fork made is deleted, its record naming the
    /// fork's owner: one of another's, which took the child's ID, is left as it is (TASK-79.82). As
    /// the fork's segment gave the child its origin, if the uploader took it; or of no parent, if
    /// it never did. It is deleted as the host's placement (`Options::token`).
    ///
    /// cladefs: a fork of a point, this store a copy of it, as a fork of the point, naming the point's
    /// origin: its record deleted then names it, so that its cell's pass of the collector retires
    /// the reference the fork wrote under the origin's cell (TASK-395.3). As a branch of its own,
    /// naming none, the reference would stay for ever.
    pub fn unfork(&self, child: Child) -> Result<(), Failure> {
        let origin = self.shared.fork_origin(child.branch);
        let forked = match (self.shared.point.get(), origin, child.point) {
            (Some(point), ..) => Kind::Forking { origin: point.origin, of: point.of },
            (None, None, _) => Kind::Own,
            (None, Some(origin), true) => Kind::Point(origin),
            (None, Some(origin), false) => Kind::Fork(origin),
        };
        let (holder, token) = (self.shared.holder(), self.shared.options.token);
        let deleting = branch::delete_fork(
            &*self.bucket,
            child.branch,
            forked,
            child.owner,
            (holder, token),
            &self.shared.keys.kms,
        );
        if !block_on(deleting)?? {
            log::warn!(
                "cladefs: leaving branch {} in object storage, another's: a fork to its ID failed \
                 here",
                child.branch
            );
        }
        Ok(())
    }

    /// Whether `child`'s ID is free in object storage for a fork the store is making of its
    /// branch, before the fork's commit: unless another branch has it, as a guest may give any,
    /// whose record its parent's writers, finding it another's, would fail on for ever. One GET of
    /// the child's record. cladefs: a fork of a point needs none: its child's record's create finds
    /// any there, and refuses it alike (`branch::fork_point`, TASK-449).
    pub fn child_free(&self, child: Branch) -> Result<bool, Failure> {
        let found = block_on(branch::record(&*self.bucket, child))??;
        failing!("upload::child_free");
        Ok(found.is_none())
    }

    /// cladefs: the store's versions (doc-18, TASK-476): those its branch's head names, uploaded,
    /// as its writer last swapped it, oldest first; then those its commits cut that the head names
    /// not yet, local, each by its commit's number, which [`Remote::uploaded`] waits for. None of a
    /// point's or a version's copy, which has no writer.
    pub fn versions(&self) -> Versions {
        let uploaded = match self.shared.writer() {
            Some(writer) => block_on(writer.versions()).unwrap_or_default(),
            None => Vec::new(),
        };
        let local = self.store().local_versions(self.last_uploaded());
        let local = (local.into_iter())
            .filter(|(_, cut)| uploaded.iter().all(|version| version.id != cut.id))
            .collect();
        Versions { uploaded, local }
    }

    /// cladefs: drops the versions `ids` names, uploaded or local: the head swapped to name them
    /// no more, and no acknowledgement naming those local (`Writer::drop_versions`, TASK-476).
    /// Fails with no writer, of a point's or a version's copy.
    pub fn drop_versions(&self, ids: &BTreeSet<u64>) -> Result<(), Failure> {
        let writer = self.shared.writer().ok_or_else(|| Failure::msg("no writer: no versions"))?;
        self.store().forget_versions(ids);
        Ok(block_on(writer.drop_versions(ids))??)
    }

    /// cladefs: shares the uploaded version `id`, if `shared`, or no longer: other tenants' cells may
    /// restore it while it is (`Writer::share_version`, TASK-476).
    pub fn share_version(&self, id: u64, shared: bool) -> Result<(), Failure> {
        let writer = self.shared.writer().ok_or_else(|| Failure::msg("no writer: no versions"))?;
        Ok(block_on(writer.share_version(id, shared))??)
    }

    /// Whether the store holds a point: nothing it does goes to object storage but its forks'
    /// records.
    pub fn is_point(&self) -> bool {
        self.shared.point.get().is_some()
    }

    /// The lowest sequence number the store's guest may mint extents' IDs from, which the writer's
    /// epoch gives: no writer of the branch before minted from there.
    pub fn next_sequence(&self) -> u64 {
        self.shared.next_sequence()
    }

    /// The bucket the store's branch is kept in.
    pub fn bucket(&self) -> &ObjectBucket {
        &self.bucket
    }

    /// How many commits waited for the lag to shrink.
    pub fn stalls(&self) -> u64 {
        self.shared.progress.lock().unwrap().stalls
    }

    /// Why uploading stopped, if it did: the store's commits are its branch's no more.
    pub fn failed(&self) -> Option<Failure> {
        self.shared.failed()
    }

    /// What has the uploader, once it stops, or as it is stopping, stop at once (TASK-216): as a
    /// writer opening the store again here waits for it, which the store's write-ahead log keeps
    /// what it hadn't uploaded for, and whose opening uploads it. Stopped so, its uploads in flight
    /// are given up on, as a crashed host's are, landing or not, and its writer writes no
    /// checkpoint. Its uploader gone, it does nothing. Stopping as dropping it does otherwise, it
    /// uploads all its store made durable first, which takes as long as the bucket takes.
    pub fn cut_short(&self) -> impl Fn() + Send + Sync + 'static {
        let shared = Arc::downgrade(&self.shared);
        move || {
            let Some(shared) = shared.upgrade() else { return };
            shared.progress.lock().unwrap().cut = true;
            shared.wake.notify_one();
        }
    }

    /// Has the uploader, stopping, upload all its store made durable first, as it does unless
    /// cut short ([`Uploader::cut_short`]): for a store another writer took over, as it was, its
    /// uploader going on, whose going later no writer waits for.
    pub fn drain_on_stop(&self) {
        self.shared.progress.lock().unwrap().cut = false;
    }
}

impl<D: Dir> Drop for Uploader<D> {
    fn drop(&mut self) {
        self.shared.stop();
        if let Some(stopped) = self.stopped.get_mut().unwrap().take() {
            let _ = stopped.recv();
        }
    }
}

impl<D: Dir, B: extent_bucket::Bucket> Shared<D, B> {
    /// An uploader that uploads as `options` say, within `uploads`, its interval running out on
    /// `clock`: started by [`Shared::run`]. cladefs: encrypting and reading under `keys`, the host's
    /// (TASK-411.3).
    pub(crate) fn new(
        options: Options,
        uploads: Arc<Budget>,
        clock: Clock,
        keys: keys::Host,
    ) -> Arc<Self> {
        Arc::new(Self {
            store: std::sync::OnceLock::new(),
            writer: Default::default(),
            token: Default::default(),
            point: Default::default(),
            forks: Default::default(),
            progress: Mutex::new(Progress::default()),
            changed: Event::new(),
            wake: tokio::sync::Notify::new(),
            options,
            uploads,
            clock,
            holders: Mutex::default(),
            keys,
            held: Mutex::default(),
        })
    }

    /// cladefs: holds `ring`, the store's keys, among the host's, which its fetchers read under,
    /// from now on.
    fn hold_ring(&self, ring: &Arc<Ring>) {
        *self.held.lock().unwrap() = Some(self.keys.ring.hold(ring.clone()));
    }

    /// cladefs: the keys of `cell` as read now, to be held by `placement` next
    /// ([`CellKeys::to_hold`]): none, of a cell with no object, a point's.
    async fn cell_keys(
        &self,
        bucket: &B,
        cell: extent_bucket::layout::Cell,
        placement: Option<u64>,
    ) -> Result<CellKeys, Failure> {
        let kms = self.keys.kms.clone();
        Ok(match (cell::read(bucket, cell).await?, placement) {
            (Some(object), Some(placement)) => CellKeys::to_hold(cell, &object, kms, placement),
            (Some(object), None) => CellKeys::of(cell, &object, kms),
            (None, _) => CellKeys::objectless(cell, kms),
        })
    }

    /// A holder, or an owner, of the uploader's own, after its first taking's ([`Holders`]).
    fn holder(&self) -> u64 {
        self.holders.lock().unwrap().next()
    }

    /// The epoch of the store's latest commit uploaded and acknowledged.
    pub(crate) fn last_uploaded(&self) -> u64 {
        self.progress.lock().unwrap().uploaded
    }

    /// The epoch of the store's latest commit whose forks are made, their children's records
    /// and the checkpoint at them in the bucket.
    #[cfg(all(test, feature = "shuttle"))]
    pub(crate) fn forked(&self) -> u64 {
        self.progress.lock().unwrap().forked
    }

    /// The lowest sequence number the store's guest may mint extents' IDs from.
    pub(crate) fn next_sequence(&self) -> u64 {
        self.progress.lock().unwrap().next_sequence
    }

    /// Why uploading stopped, if it did.
    pub(crate) fn failed(&self) -> Option<Failure> {
        self.progress.lock().unwrap().failed.clone()
    }

    /// The branch's writer, once one took it.
    pub(crate) fn writer(&self) -> Option<Arc<Writer<B>>> {
        self.writer.lock().unwrap().clone()
    }

    /// Where the fork to `child` is in the branch, once the uploader took the segment its commit
    /// ends.
    pub(crate) fn fork_origin(&self, child: Branch) -> Option<Origin> {
        self.forks.lock().unwrap().get(&child).copied()
    }

    /// Has the uploader stop: once it uploaded what its store made durable, its open segment's
    /// commits too, unless cut short ([`Uploader::cut_short`]).
    pub(crate) fn stop(&self) {
        self.progress.lock().unwrap().stop = true;
        self.changed.notify(usize::MAX);
        self.wake.notify_one();
    }

    /// Waits until the uploader is woken, or `interval` runs out first: whether it was woken.
    async fn woken(&self, interval: Duration) -> bool {
        let notified = std::pin::pin!(self.wake.notified());
        let ran_out = (self.clock)(interval);
        matches!(futures::future::select(notified, ran_out).await, Either::Left(_))
    }

    fn store(&self) -> &Arc<Store<D>> {
        self.store.get().expect("the uploader opened its store")
    }

    /// Notes the store's latest commit, if it isn't uploaded, as seen now.
    fn observe(&self) -> Result<(), Failure> {
        let epoch = self.store().last_commit()?;
        let mut progress = self.progress.lock().unwrap();
        let newer = progress.seen.back().is_none_or(|(seen, _)| *seen < epoch);
        if epoch > progress.uploaded && newer {
            progress.seen.push_back((epoch, SystemTime::now()));
        }
        Ok(())
    }

    /// Whether the commits up to `epoch` are uploaded and acknowledged, and, if `stall`, the bytes
    /// waiting within `Options::max_lag_bytes`: if not, asks the uploader for them, counting a
    /// stall if `stall`.
    pub(crate) fn ask(&self, epoch: u64, stall: bool) -> Result<bool, Failure> {
        self.observe()?;
        let mut progress = self.progress.lock().unwrap();
        if self.within(progress.uploaded, epoch, stall)? {
            return Ok(true);
        }
        progress.wanted = true;
        if stall {
            progress.stalls += 1;
            let pending = self.store().wal_bytes()?;
            let commits = epoch.saturating_sub(progress.uploaded);
            let oldest = progress.seen.front().and_then(|(_, seen)| seen.elapsed().ok());
            let max = self.options.max_lag_bytes;
            log::warn!("{}", stalled(pending, max, commits, oldest));
        }
        drop(progress);
        self.wake.notify_one();
        Ok(false)
    }

    /// What waiting for `epoch` as [`Shared::ask`] says finds in `progress`: done, or failed, or,
    /// if nothing yet, `None`.
    fn reached(&self, progress: &Progress, epoch: u64, stall: bool) -> Option<Result<(), Failure>> {
        match self.within(progress.uploaded, epoch, stall) {
            Ok(true) => Some(Ok(())),
            Ok(false) => progress.failed.clone().map(Err),
            Err(err) => Some(Err(err)),
        }
    }

    /// Whether, `uploaded` up to, the commits up to `epoch` are, and, if `stall`, the bytes of the
    /// write-ahead log within `Options::max_lag_bytes`: bytes only bound how far commits go ahead,
    /// not when one is durable.
    fn within(&self, uploaded: u64, epoch: u64, stall: bool) -> Result<bool, Failure> {
        let max_bytes = self.options.max_lag_bytes;
        Ok(uploaded >= epoch && (!stall || self.store().wal_bytes()? <= max_bytes))
    }

    /// Waits, blocking, until `reached` finds in the progress what it waits for, done or failed.
    fn wait(
        &self,
        reached: impl Fn(&Progress) -> Option<Result<(), Failure>>,
    ) -> Result<(), Failure> {
        loop {
            let changed = {
                let progress = self.progress.lock().unwrap();
                if let Some(reached) = reached(&progress) {
                    return reached;
                }
                self.changed.listen()
            };
            changed.wait();
        }
    }

    /// Runs the uploader of the store `recovering` recovered, opening it first, with what the host
    /// learned of its branch for this opening, if anything ([`learned`]).
    pub(crate) async fn run(
        &self,
        recovering: (Recovering<D>, Option<Learned>),
        options: crate::Options,
        bucket: Arc<B>,
        holder: u64,
        started: std::sync::mpsc::Sender<Result<Arc<Store<D>>, Failure>>,
    ) {
        let interval = self.options.interval;
        self.holders.lock().unwrap().0 = holder;
        let writer = match self.start(recovering, options, &bucket, holder).await {
            Ok(writer) => {
                started.send(Ok(self.store().clone())).unwrap();
                writer
            }
            Err(err) => return started.send(Err(err)).unwrap(),
        };
        // A point's store has nothing to upload.
        let Some(writer) = writer else {
            while !self.progress.lock().unwrap().stop {
                self.wake.notified().await;
            }
            return;
        };
        let mut segments = 0;
        let mut in_flight = InFlight::default();
        // Whether the next gather closes the store's open segment: for a commit waiting for its
        // upload, or a fork, or as the interval is up, or the uploader stops; and whether the last
        // took segments, after which the store may hold more closed, which the next takes,
        // closing none for them (TASK-92.15).
        // cladefs: and whether a commit waits for it, or a fork, or the uploader stops: closing
        // whatever it holds, not only once the guest is filling it no more (TASK-92.41).
        let (mut close, mut more, mut urgent) = (false, false, false);
        loop {
            if let Err(err) = self.observe() {
                return self.fail(&writer, err);
            }
            let (stop, cut) = {
                let mut progress = self.progress.lock().unwrap();
                urgent |= std::mem::take(&mut progress.wanted);
                close |= urgent;
                (progress.stop, progress.cut)
            };
            // Cut short (TASK-216): what it has in flight, and what it would have taken, the
            // store's log keeps, for the store's next opening.
            if stop && cut {
                return self.fail(&writer, Failure::msg("the uploader stopped, cut short"));
            }
            if (close || more || stop) && in_flight.len() < IN_FLIGHT {
                let closing = (close || stop, urgent || stop);
                let gathered = blocking(|| self.gather(&writer, &mut in_flight, closing));
                let gathered = match gathered {
                    Ok(gathered) => gathered,
                    Err(err) => return self.fail(&writer, err),
                };
                // Done with once closed; the segments the store had closed taking the room, it
                // closes the next time.
                close &= !gathered.closed;
                urgent &= !gathered.closed;
                more = gathered.took;
                if more {
                    continue;
                }
                if stop && in_flight.is_empty() {
                    // A checkpoint on clean shutdown (doc-7, Checkpoints).
                    if let Err(err) = writer.checkpoint().await {
                        log::warn!("cladefs: checkpointing branch {}: {err}", writer.branch());
                    }
                    return self.fail(&writer, Failure::msg("the uploader stopped"));
                }
            }
            let woken = self.woken(interval);
            if in_flight.is_empty() {
                close |= !woken.await;
                continue;
            }
            let uploaded =
                match futures::future::select(in_flight.next(), std::pin::pin!(woken)).await {
                    Either::Left((uploaded, _)) => uploaded.expect("an upload in flight"),
                    Either::Right((woken, _)) => {
                        close |= !woken;
                        continue;
                    }
                };
            let acked = match uploaded {
                Ok(()) => self.acknowledged(&writer, &mut in_flight).await,
                Err(err) => Err(err.into()),
            };
            if let Err(err) = acked {
                return self.fail(&writer, err);
            }
            segments += 1;
            if segments % self.options.checkpoint_every == 0 {
                // A test's: the checkpoint landing late, after the commits it covers returned, as on
                // a loaded host, which a collector's pass then runs beside (TASK-339).
                fail::fail_point!("upload::checkpointing");
                if let Err(err) = writer.checkpoint().await {
                    log::warn!("cladefs: checkpointing branch {}: {err}", writer.branch());
                }
            }
        }
    }

    /// Stops uploading, for `err`: commits waiting for uploads, and forks, fail with it.
    fn fail(&self, writer: &Writer<B>, err: Failure) {
        let mut progress = self.progress.lock().unwrap();
        if !progress.stop {
            log::error!("cladefs: uploading branch {}: {err}", writer.branch());
        }
        progress.failed = Some(err);
        self.changed.notify(usize::MAX);
    }

    /// Opens the store `recovering` recovered, as the bucket holds its branch (see the module's
    /// docs), and returns its writer, none for a point. Its log is checked to follow the branch as
    /// recovery finds it before the head is taken, so that a store plainly stale fences no writer;
    /// then, the head taken, the writer recovers it on from there (TASK-92.6) and the store opens
    /// as the writer found its end, which another writer may have moved on meanwhile.
    async fn start(
        &self,
        (recovering, learned): (Recovering<D>, Option<Learned>),
        options: crate::Options,
        bucket: &Arc<B>,
        holder: u64,
    ) -> Result<Option<Arc<Writer<B>>>, Failure> {
        let branch = recovering.branch();
        let (cache, parts) = (self.options.cache.clone(), self.options.held.clone());
        let placement = recovering.token().unwrap_or(self.options.token);
        let kms = &self.keys.kms;
        let (found, read, cell) = first_reads(&**bucket, branch, learned, kms).await?;
        let found = match found {
            // A branch the bucket acknowledged commits of, which it no longer has: deleted, or
            // another bucket. Making it anew would lose them.
            None if recovering.start().is_some() => {
                return Err(extent_bucket::Error::NoBranch.into());
            }
            // cladefs: made once its cell is held, which gives it its DEK (TASK-411.3).
            None => None,
            Some(record) if record.is_point() || recovering.version().is_some() => {
                let (Recovered { state, layer, .. }, keys) = match recovering.version() {
                    Some(id) => self.version(bucket, branch, record, read.head()?, id).await?,
                    None => self.point(bucket, branch, record, read.head()?).await?,
                };
                let opening =
                    Opening { state, layer, taken: None, cache, keys: keys.clone(), held: parts };
                let store = blocking(|| recovering.open(options, opening))?;
                let _ = self.store.set(Arc::new(store));
                self.hold_ring(&keys);
                let mut progress = self.progress.lock().unwrap();
                progress.uploaded_through(u64::MAX);
                progress.forked_through(u64::MAX);
                return Ok(None);
            }
            Some(record) => {
                // cladefs: recovered under its keys: its cell's, as this placement holds it next,
                // unwrapped once for that taking (TASK-411.3); holding none, a record not given
                // its DEK yet (a fork's child's), read anew once it is, below.
                let before = match record.deks.is_empty() {
                    true => Ring::keyless(),
                    false => {
                        let keys = self.cell_keys(bucket, branch.cell, Some(placement)).await?;
                        keys::ring(&**bucket, &keys, branch, &record, true).await?
                    }
                };
                // From the head read with the record, which the taking goes on from.
                let recovered = extent_bucket::recovery::recover_under_from(
                    &**bucket,
                    branch,
                    record.origin(),
                    read.head()?,
                    &before,
                )
                .await?;
                // Where the writer's chain would go: the log's restamping, not looked at here.
                let chain = Chain::new(branch, 0, recovered.end(), 0);
                let head = recovered.head.clone().unwrap_or_default();
                let found = TakenBranch::of(head, &recovered.state, chain);
                if !recovering.follows(&found) {
                    return Err(extent_bucket::Error::Diverged.into());
                }
                Some((record, recovered, before))
            }
        };
        // cladefs: a branch deleted takes no writer, whatever placement holds its cell.
        if let Some((record, ..)) = &found
            && record.deleted
        {
            return Err(extent_bucket::Error::Deleted.into());
        }
        // cladefs: its cell, held by this placement, whose epoch is the token the branch's head is
        // taken with: a later taking of the cell, elsewhere, fences its writer (TASK-364). A
        // branch whose cell has no object yet, new or a fork's child, makes it; one deleted takes
        // no writer (`Deleted`). The store's branch is its cell's from now on, of its kind, unless
        // the cell has another (`CellHasBranch`, doc-20): all in one compare-and-swap of the cell's
        // object, a read alone when nothing changes. Not
        // a point's, which no writer takes, and any number of hosts open at once. cladefs: a cell
        // made gets a KEK, wrapped by its tenant's ahead: every cell is encrypted (TASK-411.28); one
        // of a branch with DEKs is there already (TASK-411.3).
        let given = recovering.kind().map(|kind| (branch.id, kind));
        let kms = &self.keys.kms;
        let kek = match &found {
            Some((record, ..)) if !record.deks.is_empty() => None,
            _ => Some(keys::new_cell_kek(kms, branch.cell.tenant).await?),
        };
        let making = kek.as_ref().map(|(_, kek)| kek);
        let holding = (placement, placement);
        let remembered = matches!(cell, CellFrom::Remembered(_));
        let holding = cell::hold_from(&**bucket, branch.cell, cell.start(), holding, making, given);
        let (held, object) = match holding.await.map(|(held, object, _)| (held, object)) {
            Ok(held) => held,
            Err(err) => {
                if let Some((secret, kek)) = kek {
                    kms.give_back(branch.cell.tenant, (secret, kek.wrapped));
                }
                return Err(err.into());
            }
        };
        let cell_keys = keys::held(kms, branch.cell, object, kek);
        // cladefs: the host serves a cell of the tenant's: its pool of new cell KEKs, which forks of
        // its stores into new cells take, filled off this path (TASK-411.4).
        kms.refill(branch.cell.tenant);
        let token = held.epoch;
        let taker = Taker { holder, token, remembered: remembered.then_some(held) };
        // Its state, once it holds the head, recovered on from what was found before. cladefs:
        // under its keys, its DEK given it first if it has none: a new branch's, or a fork's
        // child's, whose recovery, read holding no key, it recovers anew under them (TASK-411.3).
        let keyed = |record| self.ring_of(bucket, &cell_keys, branch, record);
        let writer = match found {
            None => {
                failing!("upload::create_branch");
                branch::create(&**bucket, branch, self.holder()).await?;
                let keys = keyed(None).await?;
                Writer::open_keyed(bucket.clone(), branch, taker, keys).await?
            }
            Some((record, recovered, before)) => {
                let keys = keyed(Some(record.clone())).await?;
                match before.is_empty() && !keys.is_empty() {
                    true => Writer::open_keyed(bucket.clone(), branch, taker, keys).await?,
                    false => {
                        let found = (recovered, read);
                        Writer::open_recovered_from(bucket.clone(), record, found, taker, keys)
                            .await?
                    }
                }
            }
        };
        let writer = Arc::new(writer.uploading_within(self.uploads.clone()));
        let keys = writer.keys().clone();
        let (state, layer, taken) = (writer.state(), writer.layer(), Some(writer.taken().clone()));
        let opening = Opening { state, layer, taken, cache, keys: keys.clone(), held: parts };
        let store = blocking(|| recovering.open(options, opening))?;
        let _ = self.store.set(Arc::new(store));
        self.hold_ring(&keys);
        let _ = self.token.set(token);
        self.took(&writer, token);
        Ok(Some(writer))
    }

    /// cladefs: the keys of the store of `branch`, of the cell `cell_keys` are of, its writer's: its
    /// record's DEKs, given it its first if it has none, and its ancestors' (`keys::with_dek`,
    /// `keys::ring`, TASK-411.3). Its record is `record`, if read, or read now.
    async fn ring_of(
        &self,
        bucket: &Arc<B>,
        cell_keys: &CellKeys,
        branch: extent_bucket::layout::Branch,
        record: Option<Record>,
    ) -> Result<Arc<Ring>, Failure> {
        let record = match record {
            Some(record) => record,
            None => {
                branch::record(&**bucket, branch).await?.ok_or(extent_bucket::Error::NoBranch)?
            }
        };
        let record = keys::with_dek(&**bucket, cell_keys, branch, record).await?;
        Ok(keys::ring(&**bucket, cell_keys, branch, &record, true).await?)
    }

    /// `writer` took the branch, with the fencing token `token`: the store names what it reads in
    /// the bucket where the writer's checkpoints move it, as the writer does (TASK-195); the
    /// branch's commits are uploaded, its forks made, and what the store's write-ahead log holds
    /// after them is the uploader's to upload.
    fn took(&self, writer: &Arc<Writer<B>>, token: u64) {
        let following = Arc::downgrade(self.store());
        writer.following(move |moved| {
            let Some(store) = following.upgrade() else { return };
            if let Err(err) = store.moved(moved) {
                log::warn!("cladefs: following the collector's moves: {err:#}");
            }
        });
        *self.writer.lock().unwrap() = Some(writer.clone());
        let taking = Taking { token, epoch: writer.epoch(), taken: SystemTime::now() };
        let mut progress = self.progress.lock().unwrap();
        (progress.next_sequence, progress.taking) = (writer.chain().next_sequence(), Some(taking));
        if let Some(last) = writer.state().last_commit {
            progress.uploaded_through(epoch_of(last));
            progress.forked_through(epoch_of(last));
        }
        drop(progress);
        self.changed.notify(usize::MAX);
    }

    /// Opens the store of `branch` as a point, whose record is `record`: its state, which never
    /// changes, as recovered, with the decoded state it shares (TASK-453). No taking: nothing the
    /// store does goes to the bucket but its forks' records.
    /// cladefs: and its keys, its ancestors' DEKs, under its cell's KEK as read, taking it not
    /// (TASK-411.3).
    /// cladefs: recovered from `head`, what a read of its head found, or what the host knows it
    /// has, none (TASK-449).
    async fn point(
        &self,
        bucket: &Arc<B>,
        branch: Branch,
        record: Record,
        head: Option<extent_bucket::layout::Head>,
    ) -> Result<(Recovered, Arc<Ring>), Failure> {
        if record.deleted {
            return Err(Failure::PointDeleted);
        }
        let Kind::Point(origin) = record.kind else {
            return Err(Failure::msg(format!("branch {branch} is no point")));
        };
        let cell_keys = self.cell_keys(bucket, branch.cell, None).await?;
        let keys = keys::ring(&**bucket, &cell_keys, branch, &record, false).await?;
        let recovering = extent_bucket::recovery::recover_under_from(
            &**bucket,
            branch,
            Some(origin),
            head,
            &keys,
        );
        let recovered = recovering.await?;
        // cladefs: its store's open segment, which nothing writes, encrypted as every store's is
        // (TASK-411.28).
        let keys = keys.for_point(branch);
        let _ = self.point.set(Point { origin, of: Forked::Point(branch) });
        Ok((recovered, keys))
    }

    /// cladefs: opens the store of `branch`, whose record is `record`, as a copy of its version
    /// `id`, which `head`, what a read of its head found, must name (TASK-476): its state at the
    /// version, which never changes, as recovered, with its keys, its branch's, under its cell's
    /// KEK as read, taking it not. No lease, as for a point's: nothing the store does goes to the
    /// bucket but its forks' records. Fails as a point's opening does if the version is named no
    /// more ([`Failure::PointDeleted`]).
    async fn version(
        &self,
        bucket: &Arc<B>,
        branch: Branch,
        record: Record,
        head: Option<extent_bucket::layout::Head>,
        id: u64,
    ) -> Result<(Recovered, Arc<Ring>), Failure> {
        let named = head.as_ref().and_then(|head| head.version(id).copied());
        let Some(version) = named.filter(|_| !record.deleted) else {
            return Err(Failure::PointDeleted);
        };
        let origin = Origin { parent: branch, at: version.at };
        let cell_keys = self.cell_keys(bucket, branch.cell, None).await?;
        let keys = keys::ring(&**bucket, &cell_keys, branch, &record, false).await?;
        let recovering =
            extent_bucket::recovery::recover_version(&**bucket, branch, version.at, &keys);
        let recovered = recovering.await?;
        // cladefs: as a point's (TASK-411.28).
        let keys = keys.for_point(branch);
        let _ = self.point.set(Point { origin, of: Forked::Version(id) });
        Ok((recovered, keys))
    }

    /// Adds the segments of the store's write-ahead log whose commits are durable to the writer, to
    /// be uploaded as they are, after those uploading (`in_flight`), as many as may upload at once
    /// with them: the rest stay in the store, unread, until those are uploaded. If `close`, the
    /// store closes its open segment first if fewer wait: for a commit waiting for its upload, a
    /// fork, the interval up, or the uploader stopping; for no other, as the commits no one waits
    /// for wait in it for those, rather than each go in a segment of its own (TASK-92.15).
    /// cladefs: if not `urgent`, the interval's, the open segment the guest is filling stays open
    /// for its next commit, up to `Options::filling_wait` (TASK-92.41).
    fn gather(
        &self,
        writer: &Arc<Writer<B>>,
        in_flight: &mut InFlight,
        (close, urgent): (bool, bool),
    ) -> Result<Gathered, Failure> {
        failing!("upload::gather");
        let store = self.store();
        let room = IN_FLIGHT.saturating_sub(in_flight.len());
        // What a waiter wants is durable; then closed, if fewer segments wait than may upload, and
        // what the closing closed durable too.
        store.flush()?;
        let mut closed = store.closed(room)?;
        let mut closing = close && closed.len() < room;
        if closing {
            let waited = {
                let progress = self.progress.lock().unwrap();
                let oldest = progress.seen.front().and_then(|(_, seen)| seen.elapsed().ok());
                oldest.is_some_and(|waited| waited >= self.options.filling_wait)
            };
            // cladefs: until its framed file is written, not durable, which its upload waits for no
            // more (TASK-384). Asked again at the next interval, if the guest still hasn't
            // committed.
            match urgent || waited {
                true => drop(store.close_for_upload(false)?),
                false => closing = store.close_for_upload(true)?,
            }
            store.flush()?;
            closed.extend(store.closed(room - closed.len())?);
        }
        let mut segments = Vec::with_capacity(closed.len());
        for closed in closed {
            let (number, tag) = (closed.number, closed.tag());
            let forks: Vec<Branch> =
                closed.footer.contents.forks.iter().map(|forked| forked.branch).collect();
            let own = closed.own.iter().map(|(key, file)| Ok((key.clone(), stored(file)?)));
            let own = own.collect::<Result<_, Failure>>()?;
            let footer = closed.footer.clone();
            let size = closed.file.size().map_err(|err| Failure::local(err, "a segment"))?;
            let closed = Arc::new(closed);
            let body = Body::new(size, move || blocking(|| closed.bytes()));
            let segment = writer.add_segment(Objects { footer, segment: body, own })?;
            // Where each fork it ends is, which a fork failing deletes its child by.
            let origin = Origin { parent: writer.branch(), at: segment };
            self.forks.lock().unwrap().extend(forks.iter().map(|&child| (child, origin)));
            segments.push(Uploading { segment, number, tag, forks: !forks.is_empty() });
        }
        let took = !segments.is_empty();
        in_flight.upload(writer, segments);
        Ok(Gathered { took, closed: closing })
    }

    /// Acknowledges what the writer has uploaded: the commits of the segments it acknowledges are
    /// uploaded, and the store's segments leave it, read in the bucket, into the host's cache if
    /// it has one (TASK-165.8); then the forks they name are made, with a checkpoint at them, which
    /// their children's recovery starts from.
    async fn acknowledged(
        &self,
        writer: &Writer<B>,
        in_flight: &mut InFlight,
    ) -> Result<(), Failure> {
        let through = writer.acknowledge().await?;
        failing!("upload::acknowledged");
        let acked = in_flight.acknowledged(through);
        let Some(epoch) = acked.last().map(Uploading::epoch) else { return Ok(()) };
        blocking(|| {
            for segment in &acked {
                self.store().acknowledged(segment.number)?;
            }
            Ok::<_, Failure>(())
        })?;
        failing!("upload::released");
        let forks = acked.iter().any(|segment| segment.forks);
        {
            let mut progress = self.progress.lock().unwrap();
            progress.uploaded_through(epoch);
            if !forks {
                progress.forked_through(epoch);
            }
        }
        self.changed.notify(usize::MAX);
        if forks {
            // Their children have their records, which the acknowledgement made: the checkpoint
            // makes the forks.
            writer.checkpoint().await?;
            failing!("upload::checkpointed");
            self.progress.lock().unwrap().forked_through(epoch);
            self.changed.notify(usize::MAX);
        }
        Ok(())
    }
}

/// What this host learned of the branch `recovering` recovered, which the opening of its store
/// takes from the bucket's [`Known`](extent_bucket::known::Known) as it begins, whatever comes of
/// it after: so nothing learned for an opening outlives it, a failed one's no later one's
/// (TASK-449's review). Only for a store whose log goes on from nothing the bucket acknowledged:
/// one that has, learned nothing of here, its learning dropped.
fn learned<B: extent_bucket::Bucket, D: Dir>(
    bucket: &B,
    recovering: &Recovering<D>,
) -> Option<Learned> {
    let learned = bucket.known()?.take(recovering.branch());
    learned.filter(|_| recovering.start().is_none())
}

/// What an opening of the store of `branch` reads first, at once (TASK-165.11, TASK-449): its
/// record ([`resumed`]), its head, which the writer's taking goes on from and its recovery starts
/// from, and its cell's object, which its holding goes on from, returning what the holding starts
/// from. Of a branch this host has just made or taken, `learned` ([`learned`]) stands for those
/// reads: its record as it left it, its cell's object, if it held it, and its head, none, if it
/// knows it has none, a clone made or a point. Each is as a read made then: the head's taking is
/// conditional on what it starts from, and so is the cell's holding if it writes; the record is
/// read again once the head is taken, and the cell too if its holding wrote nothing
/// ([`CellFrom::Remembered`]; `Writer::take`).
async fn first_reads<B: extent_bucket::Bucket>(
    bucket: &B,
    branch: Branch,
    learned: Option<Learned>,
    kms: &Arc<keys::CachingKms>,
) -> Result<(Option<Record>, extent_bucket::head::Read, CellFrom), Failure> {
    use extent_bucket::head::{self, Read};
    let key = extent_bucket::layout::cell(branch.cell);
    let cell_read = || extent_bucket::bucket::get(bucket, &key);
    let Some(Learned { record, cell, headless }) = learned else {
        let (found, read, cell) = futures::join!(
            async { resumed(bucket, branch, branch::record(bucket, branch).await?, kms).await },
            head::read(bucket, branch),
            cell_read(),
        );
        return Ok((found?, read, CellFrom::Read(cell)));
    };
    // A point, which no writer takes, holds no cell: its opening reads none.
    let point = record.is_point();
    let none = match headless {
        true => Some(Read::absent()),
        false => point.then(Read::of_point).flatten(),
    };
    let reading = async {
        match none {
            Some(none) => none,
            None => head::read(bucket, branch).await,
        }
    };
    let holding = async {
        match cell {
            Some(cell) => CellFrom::Remembered(cell),
            None if point => CellFrom::Unread,
            None => CellFrom::Read(cell_read().await),
        }
    };
    let (found, read, cell) =
        futures::join!(resumed(bucket, branch, Some(record), kms), reading, holding);
    Ok((found?, read, cell))
}

/// The record of `branch`, `found`, if it has one: a fork of a point its host crashed in the middle
/// of, its record saying so, resumed first ([`branch::resume_point_fork`]: its reference written,
/// the fork made), which fails if the point was deleted. So the store takes no state of a fork
/// not made.
async fn resumed<B: extent_bucket::Bucket>(
    bucket: &B,
    branch: Branch,
    found: Option<Record>,
    kms: &Arc<keys::CachingKms>,
) -> Result<Option<Record>, Failure> {
    if !found.as_ref().is_some_and(|record| !record.deleted && record.forking().is_some()) {
        return Ok(found);
    }
    // cladefs: a version's restore into another cell writes its checkpoint at the version under
    // the version's branch's keys, unwrapped by the host's KMS (TASK-476).
    branch::resume_point_fork(bucket, branch, keys::Parents::Kms(kms, None)).await?;
    Ok(branch::record(bucket, branch).await?)
}

/// The bytes of `file`, an extent's of its own a segment of the write-ahead log names, read only
/// as its upload goes, and checked: a byte the host's disk damaged is not uploaded under the CRC
/// it fails (`read_object`, TASK-153.11).
fn stored<F: File>(file: &Arc<F>) -> Result<Body, Failure> {
    let size = file.size().map_err(|err| Failure::local(err, "a file to upload"))?;
    let file = file.clone();
    Ok(Body::new(size, move || blocking(|| read_object(&*file))))
}

/// What the holding of a store's cell starts from ([`first_reads`]).
enum CellFrom {
    /// What a read made with the opening's first reads found.
    Read(Option<extent_bucket::bucket::Object>),
    /// What this host's fork of the branch left, as it held the cell: a read made earlier. Its
    /// holding, finding it held by this placement already, writes nothing, which no condition
    /// checks; so the writer's taking reads the cell once the head is taken, which another's
    /// holding since refuses (`extent_bucket::Taker::remembered`, TASK-449's review).
    Remembered(extent_bucket::bucket::Object),
    /// Nothing: a point's, which holds no cell; read, if held.
    Unread,
}

impl CellFrom {
    fn start(self) -> branch::Start {
        match self {
            Self::Read(found) => branch::Start::Found(found),
            Self::Remembered(found) => branch::Start::Found(Some(found)),
            Self::Unread => branch::Start::Read,
        }
    }
}

/// cladefs: what a fork made ([`Uploader::forked`]): of a point's, the child's record as it left
/// it, a clone made, and its cell's object, if it held it.
#[derive(Default)]
struct Made {
    record: Option<Record>,
    cell: Option<extent_bucket::bucket::Object>,
}

/// What a gather did ([`Shared::gather`]).
struct Gathered {
    /// Whether it took segments the store closed: the store may have closed more meanwhile.
    took: bool,
    /// Whether it closed the store's open segment, as asked: what was committed before is in the
    /// segments the store closed, taken, or to be taken next.
    closed: bool,
}

/// A segment uploading: its number in the branch, and in the store; the tag of the store's commit
/// it ends with; and whether forks are made at its end.
struct Uploading {
    segment: u64,
    number: u64,
    tag: u64,
    forks: bool,
}

impl Uploading {
    /// The store's epoch of the commit it ends with.
    fn epoch(&self) -> u64 {
        epoch_of(self.tag)
    }
}

/// The segments uploading, in order, and the futures of their uploads: an acknowledgement takes
/// exactly the segments the writer acknowledged. Each is in the store's write-ahead log, durably,
/// before its upload goes (TASK-79.72's rule), so that nothing lands in the writer's name that the
/// log doesn't hold.
#[derive(Default)]
struct InFlight {
    segments: VecDeque<Uploading>,
    uploads: FuturesOrdered<BoxFuture<'static, Result<(), extent_bucket::Error>>>,
}

impl InFlight {
    /// Uploads `segments`, which `writer` took, after those uploading.
    fn upload<B: extent_bucket::Bucket>(
        &mut self,
        writer: &Arc<Writer<B>>,
        segments: Vec<Uploading>,
    ) {
        for uploading in segments {
            let (writer, segment) = (writer.clone(), uploading.segment);
            self.uploads.push_back(async move { writer.upload(segment).await }.boxed());
            self.segments.push_back(uploading);
        }
    }

    /// The next upload done, in order.
    fn next(
        &mut self,
    ) -> impl Future<Output = Option<Result<(), extent_bucket::Error>>> + Unpin + '_ {
        self.uploads.next()
    }

    /// The segments the writer acknowledged, `through` the last, which upload no more.
    fn acknowledged(&mut self, through: Option<u64>) -> Vec<Uploading> {
        let taken =
            |uploading: &&Uploading| through.is_some_and(|through| uploading.segment <= through);
        let count = self.segments.iter().take_while(taken).count();
        self.segments.drain(..count).collect()
    }

    /// How many uploads are in flight.
    fn len(&self) -> usize {
        self.uploads.len()
    }

    fn is_empty(&self) -> bool {
        self.uploads.is_empty()
    }
}

#[cfg(test)]
mod encrypted;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Description, MemDir};
    use extent_bucket::fetch::Limits;
    use storage_device::ExtentId;

    const BRANCH: extent_bucket::layout::Branch = extent_bucket::layout::test_branch(7);

    /// TASK-449's review: a failed making of the bucket runtime, a thread not spawned as under a
    /// limit of the process's threads, fails the call it was made for, not every call after for
    /// the process's life: every fork, take and deletion of a host's until its daemon restarted.
    /// The next call makes it anew. Its own process, as nextest runs each test, whose first call
    /// makes the runtime.
    #[test]
    fn calls_after_a_failed_making_of_the_runtime_make_it_anew() {
        let scenario = fail::FailScenario::setup();
        fail::cfg("upload::bucket_runtime", "return").unwrap();
        let failed = block_on(async { 7 }).err().map(|err| err.to_string());
        fail::remove("upload::bucket_runtime");
        assert_eq!(failed.as_deref(), Some("failpoint upload::bucket_runtime"));
        assert_eq!(block_on(async { 7 }).ok(), Some(7));
        assert_eq!(block_on(async { 8 }).ok(), Some(8));
        scenario.teardown();
    }

    /// TASK-449's review: what a call from no runtime's task spawns, a bucket's connection as it
    /// connects, runs on the bucket runtime, which the host's uploaders and fetches run on: so
    /// the connections such calls leave in the bucket's pool, which the uploads take next, are
    /// served by its threads, not by a runtime of the calls' own beside it, of fewer.
    #[test]
    fn calls_from_no_task_spawn_on_the_bucket_runtime() {
        let spawned = block_on(async {
            tokio::spawn(async { std::thread::current().name().map(str::to_owned) }).await.unwrap()
        });
        assert_eq!(spawned.unwrap().as_deref(), Some("cladefs bucket"));
    }

    /// TASK-92.52: a stall says what it waits for: the bytes past the lag bound, which a stall over
    /// it waits on, and the commits not uploaded and the oldest's age, which a slow upload shows.
    #[test]
    fn a_stall_says_what_it_waits_for() {
        let over = stalled(5 << 20, 4 << 20, 3, Some(Duration::from_millis(250)));
        assert_eq!(
            over,
            "cladefs: commits wait for object storage: 5242880 bytes not uploaded, 1048576 past the \
             lag bound of 4194304; 3 commits not uploaded, the oldest seen 250 ms ago"
        );
        let slow = stalled(1 << 20, 4 << 20, 1, None);
        assert_eq!(
            slow,
            "cladefs: commits wait for object storage: 1048576 bytes not uploaded, 0 past the lag \
             bound of 4194304; 1 commits not uploaded, none seen yet"
        );
    }

    /// TASK-364: a store opened takes its branch's cell, making it for a new branch, whose branch
    /// it is from then on (doc-20); its branch's head is taken with the cell's epoch. A later placement's
    /// taking of the cell, and of the branch's head with the epoch it took it at, fences the
    /// store's writer: its next commit's upload fails, and a placement of a lower token takes the
    /// cell back no more.
    #[test]
    fn stores_take_their_cells_and_are_fenced_with_them() {
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let host = Runtime {
            handle: runtime.handle().clone(),
            uploads: Budget::new(Limits::UNBOUNDED),
            keys: extent_bucket::keys::testing::host(),
        };
        let bucket =
            Arc::new(ObjectBucket::new(Arc::new(object_store::memory::InMemory::new()), ""));
        let dir = MemDir::new();
        let description = Description { block_size: 4096, capacity: 1 << 30, branch: BRANCH };
        let kind = cell::Kind::Fxfs;
        Store::make(&dir, description, Some("memory:///cells"), Some(kind)).unwrap();
        let options = Options { token: 5, ..Options::default() };
        let recovering = Store::recover_unmade(dir).unwrap();
        let (store, uploader) =
            open(recovering, crate::Options::default(), bucket.clone(), (1, options), &host)
                .unwrap();
        store.made().unwrap();
        let object = runtime.block_on(cell::read(&*bucket, BRANCH.cell)).unwrap().unwrap();
        assert_eq!((object.epoch, object.holder, object.token), (1, 5, 5));
        assert_eq!((object.branch, object.kind), (Some(BRANCH.id), Some(cell::Kind::Fxfs)));
        let head = runtime.block_on(extent_bucket::head::get(&*bucket, BRANCH)).unwrap().unwrap();
        assert_eq!((head.epoch, head.token), (1, 1));

        store.write(ExtentId::new(BRANCH.id, 1), 0, &[1; 4096]).unwrap();
        store.commit(None).unwrap();
        uploader.wait_uploaded(store.last_commit().unwrap()).unwrap();
        // Another placement takes the cell, and its writer the branch with the cell's epoch,
        // fencing it (TASK-371).
        let taken = runtime.block_on(async {
            let (held, _) = cell::hold(&*bucket, BRANCH.cell, (6, 6), None, None).await.unwrap();
            drop(
                extent_bucket::Writer::open(bucket.clone(), BRANCH, 0x66, held.epoch)
                    .await
                    .unwrap(),
            );
            held
        });
        assert_eq!(taken.epoch, 2);
        store.write(ExtentId::new(BRANCH.id, 2), 0, &[2; 4096]).unwrap();
        store.commit(None).unwrap();
        let fenced = uploader.wait_uploaded(store.last_commit().unwrap());
        assert!(matches!(fenced, Err(Failure::Bucket(extent_bucket::Error::Fenced))), "{fenced:?}");
        let back = runtime.block_on(cell::hold(&*bucket, BRANCH.cell, (5, 5), None, None));
        assert_eq!(back.map(|(held, _)| held), Err(extent_bucket::Error::Stale));
    }

    /// TASK-79.19: a gather takes no more of the segments the store closed than may upload at once,
    /// with those uploading; the rest it leaves in the store, until those it took are uploaded.
    #[test]
    fn gathers_take_what_may_upload_at_once() {
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let host = Runtime {
            handle: runtime.handle().clone(),
            uploads: Budget::new(Limits::UNBOUNDED),
            keys: extent_bucket::keys::testing::host(),
        };
        let bucket =
            Arc::new(ObjectBucket::new(Arc::new(object_store::memory::InMemory::new()), ""));
        let dir = MemDir::new();
        let description = Description { block_size: 4096, capacity: 1 << 30, branch: BRANCH };
        Store::make(&dir, description, Some("memory:///gathers"), None).unwrap();
        // Its uploader uploads nothing it is not asked to within the hour: the test gathers.
        let options = Options { interval: Duration::from_secs(3600), ..Options::default() };
        let recovering = Store::recover_unmade(dir).unwrap();
        let (store, uploader) =
            open(recovering, crate::Options::default(), bucket.clone(), (1, options), &host)
                .unwrap();
        store.made().unwrap();
        // Each commit closes its segment.
        let closed = 2 * IN_FLIGHT + 1;
        for sequence in 1..=closed as u64 {
            store.write(ExtentId::new(BRANCH.id, sequence), 0, &[sequence as u8; 4096]).unwrap();
            store.close_segment().unwrap();
            store.commit(None).unwrap();
        }
        let shared = &uploader.shared;
        let writer = shared.writer.lock().unwrap().clone().unwrap();
        let mut in_flight = InFlight::default();
        let mut taken = Vec::new();
        while shared.gather(&writer, &mut in_flight, (true, true)).unwrap().took {
            taken.push(in_flight.len());
            runtime.block_on(async {
                while let Some(uploaded) = in_flight.next().await {
                    uploaded.unwrap();
                }
                shared.acknowledged(&writer, &mut in_flight).await.unwrap();
            });
        }
        assert_eq!(taken, [IN_FLIGHT, IN_FLIGHT, 1]);
        assert_eq!(uploader.last_uploaded(), store.last_commit().unwrap());
        drop(uploader);
        let state = runtime.block_on(extent_bucket::recover(&*bucket, BRANCH, None)).unwrap().state;
        let sequences: Vec<u64> = state.extents.keys().map(|extent| extent.sequence).collect();
        assert_eq!(sequences, (1..=closed as u64).collect::<Vec<_>>());
    }
}

/// cladefs: a store committing, sealing and forking, its uploader uploading what it commits, and
/// passes of the collector running over the bucket all along (TASK-291.1), under Shuttle, whose
/// scheduler chooses every interleaving of their threads at each of the store's locks and at each
/// request to the bucket, and replays a failing one from its seed.
/// `cargo nextest run -p extent_store --features shuttle --lib shuttle_tests::`.
#[cfg(all(test, feature = "shuttle"))]
mod shuttle_tests {
    use super::*;
    use crate::{Description, MemDir};
    use extent_bucket::bucket::{ByteRange, Error as BucketError, Meta, Object, PutMode, Version};
    use extent_bucket::collector;
    use extent_bucket::fetch::Limits;
    use extent_bucket::testing::{Direct, SimBucket};
    use shuttle::scheduler::{PctScheduler, RandomScheduler, Scheduler};
    use std::collections::BTreeMap;
    use std::future::Future;
    use std::sync::atomic::Ordering::SeqCst;
    use storage_device::ExtentId;

    const BLOCK: usize = 4096;
    const BRANCH: extent_bucket::layout::Branch = extent_bucket::layout::test_branch(7);
    /// The branch the writer forks.
    const CHILD: extent_bucket::layout::Branch = extent_bucket::layout::test_branch(BRANCH.id + 1);

    /// Small segments, so that commits close several, and extents of two blocks or more in
    /// objects of their own, which upload before the segments naming them (TASK-79.3); flushes of
    /// two blocks at most held in their log records, larger ones sealed (TASK-301).
    const STORE: crate::Options = crate::Options {
        max_extent: 64 << 20,
        segment_size: 3 * BLOCK as u64,
        segment_age: Duration::from_secs(3600),
        large_extent: 2 * BLOCK as u64,
        open_files: 2,
        view_lag: None,
        view_kept_bytes: None,
        held_most: 2 * BLOCK as u64,
    };

    /// The collector's passes compact what is mostly dead, and delete at once.
    const COLLECTING: collector::Options = collector::Options { compact_below: 0.9, grace: 0 };

    /// The schedules each test runs, randomly and by PCT; `SHUTTLE_SOAK` multiplies them.
    fn runs() -> usize {
        let soak: usize = std::env::var("SHUTTLE_SOAK").map_or(1, |soak| soak.parse().unwrap());
        200 * soak
    }

    /// A bucket in memory whose requests each give Shuttle's scheduler a turn first, as a real
    /// bucket's round trips let the other threads go on: so that a pass of the collector, the
    /// uploader and the store interleave at each. Its requests take effect at once, and never fail.
    /// Its gate, once armed, holds up the next listing of the whole bucket, a pass's of its objects.
    struct Turns {
        direct: Direct,
        gate: Gate,
    }

    impl Turns {
        fn new() -> Arc<Self> {
            Arc::new(Self { direct: Direct(SimBucket::new()), gate: Gate::default() })
        }
    }

    /// Holds up a listing of a whole cell, a pass's of its candidates (TASK-395.4), once armed,
    /// until opened.
    #[derive(Default)]
    struct Gate {
        armed: shuttle::sync::atomic::AtomicBool,
        reached: shuttle::sync::atomic::AtomicBool,
        open: shuttle::sync::atomic::AtomicBool,
    }

    impl Gate {
        fn arm(&self) {
            self.armed.store(true, SeqCst);
        }

        /// Waits until a listing held up reaches the gate.
        fn reached(&self) {
            until(|| self.reached.load(SeqCst));
        }

        fn open(&self) {
            self.open.store(true, SeqCst);
        }

        /// A listing of `prefix` goes through the gate.
        fn pass(&self, prefix: &str) {
            let whole = extent_bucket::layout::cell_of_prefix(prefix).is_some();
            if whole && self.armed.swap(false, SeqCst) {
                self.reached.store(true, SeqCst);
                until(|| self.open.load(SeqCst));
            }
        }
    }

    impl extent_bucket::Bucket for Turns {
        fn get(&self, key: &str) -> impl Future<Output = Result<Object, BucketError>> + Send {
            shuttle::thread::yield_now();
            self.direct.get(key)
        }

        fn get_range(
            &self,
            key: &str,
            range: ByteRange,
        ) -> impl Future<Output = Result<Vec<u8>, BucketError>> + Send {
            shuttle::thread::yield_now();
            self.direct.get_range(key, range)
        }

        fn head(&self, key: &str) -> impl Future<Output = Result<Meta, BucketError>> + Send {
            shuttle::thread::yield_now();
            self.direct.head(key)
        }

        fn list(
            &self,
            prefix: &str,
        ) -> impl Future<Output = Result<Vec<Meta>, BucketError>> + Send {
            shuttle::thread::yield_now();
            self.gate.pass(prefix);
            self.direct.list(prefix)
        }

        fn list_delimited(
            &self,
            prefix: &str,
            page: extent_bucket::bucket::Page<'_>,
        ) -> impl Future<Output = Result<extent_bucket::bucket::Listing, BucketError>> + Send
        {
            self.direct.list_delimited(prefix, page)
        }

        fn put(
            &self,
            key: &str,
            bytes: extent_bucket::Bytes,
            mode: PutMode,
        ) -> impl Future<Output = Result<Version, BucketError>> + Send {
            shuttle::thread::yield_now();
            self.direct.put(key, bytes, mode)
        }

        fn delete(&self, key: &str) -> impl Future<Output = Result<(), BucketError>> + Send {
            shuttle::thread::yield_now();
            self.direct.delete(key)
        }

        fn now(&self) -> u64 {
            self.direct.now()
        }
    }

    /// The bytes of the extent at `at`, `blocks` of them.
    fn pattern(at: u64, blocks: usize) -> Vec<u8> {
        (0..blocks * BLOCK).map(|i| (i % 251) as u8 ^ at as u8).collect()
    }

    /// Waits, giving the scheduler turns, for what `receiver` gets: a thread of Shuttle's blocking
    /// on the channel would hold up every other.
    fn received<T>(receiver: &std::sync::mpsc::Receiver<T>) -> T {
        loop {
            match receiver.try_recv() {
                Ok(received) => return received,
                Err(std::sync::mpsc::TryRecvError::Empty) => shuttle::thread::yield_now(),
                Err(err) => panic!("{err}"),
            }
        }
    }

    /// What the branch `branch`, recovered from `bucket`, holds, if not `holds`, extent by extent.
    fn differs(
        bucket: &Arc<Turns>,
        branch: Branch,
        holds: &BTreeMap<ExtentId, Vec<u8>>,
    ) -> Result<(), String> {
        let failed = |err: extent_bucket::Error| format!("branch {branch}: {err}");
        let record = shuttle::future::block_on(branch::record(&**bucket, branch));
        let record = record.map_err(failed)?.ok_or(format!("branch {branch}: no record"))?;
        let recovering = extent_bucket::recover(&**bucket, branch, record.origin());
        let state = shuttle::future::block_on(recovering).map_err(failed)?.state;
        let held: Vec<&ExtentId> = state.extents.keys().collect();
        if held != holds.keys().collect::<Vec<_>>() {
            return Err(format!("branch {branch} holds {held:?}, not {:?}", holds.keys()));
        }
        for (extent, location) in &state.extents {
            let read =
                shuttle::future::block_on(extent_bucket::testing::read(bucket, *extent, location));
            if read.as_ref() != Ok(&holds[extent]) {
                return Err(format!("branch {branch}: {extent:?} reads {read:?}"));
            }
        }
        Ok(())
    }

    /// Waits, giving the scheduler turns, until `done`.
    fn until(done: impl Fn() -> bool) {
        while !done() {
            shuttle::thread::yield_now();
        }
    }

    /// A store of `branch` on a disk of its own, made empty, and its uploader, running on a thread
    /// of its own as `holder`, whose writer has taken the branch.
    struct Session {
        uploader: Arc<Shared<MemDir, Turns>>,
        running: shuttle::thread::JoinHandle<()>,
        store: Arc<Store<MemDir>>,
        branch: Branch,
        /// What the store's extents hold, by their IDs.
        holds: BTreeMap<ExtentId, Vec<u8>>,
        /// The first sequence number of its own extents'.
        next: u64,
    }

    impl Session {
        fn open(bucket: &Arc<Turns>, branch: Branch, holder: u64) -> Self {
            let dir = MemDir::new();
            let description = Description { block_size: BLOCK as u32, capacity: 1 << 30, branch };
            Store::make(&dir, description, None, None).unwrap();
            let recovering = Store::recover_unmade(dir).unwrap();
            // Its interval never runs out: the writer asks for what it commits.
            let never: Clock = Arc::new(|_| futures::future::pending().boxed());
            let options = Options { token: holder, ..Options::default() };
            let uploader = Shared::new(
                options,
                Budget::new(Limits::UNBOUNDED),
                never,
                extent_bucket::keys::testing::host(),
            );
            let (started, start) = std::sync::mpsc::channel();
            let running = {
                let (uploader, bucket) = (uploader.clone(), bucket.clone());
                shuttle::thread::spawn(move || {
                    let recovering = (recovering, None);
                    let running = uploader.run(recovering, STORE, bucket, holder, started);
                    shuttle::future::block_on(running)
                })
            };
            let store = received(&start).unwrap();
            store.made().unwrap();
            let next = uploader.next_sequence();
            Self { uploader, running, store, branch, holds: BTreeMap::new(), next }
        }

        fn extent(&self, at: u64) -> ExtentId {
            ExtentId::new(self.branch.id, self.next + at)
        }

        fn write(&mut self, at: u64, blocks: usize) {
            let bytes = pattern(self.branch.id ^ at, blocks);
            self.store.write(self.extent(at), 0, &bytes).unwrap();
            self.holds.insert(self.extent(at), bytes);
        }

        fn free(&mut self, extent: ExtentId) {
            self.store.free(extent).unwrap();
            self.holds.remove(&extent);
        }

        /// Asks the uploader for what the store committed.
        fn ask(&self) {
            self.uploader.ask(self.store.last_commit().unwrap(), false).unwrap();
        }

        /// The uploader uploads all the store made durable, and stops; and the store closes.
        fn end(self) -> Result<BTreeMap<ExtentId, Vec<u8>>, String> {
            let last = self.store.last_commit().unwrap();
            self.uploader.stop();
            self.running.join().unwrap();
            drop(self.store);
            let (failed, uploaded) = (self.uploader.failed(), self.uploader.last_uploaded());
            if failed != Some(Failure::msg("the uploader stopped")) || uploaded != last {
                return Err(format!("the uploader: {failed:?}, through {uploaded} of {last}"));
            }
            Ok(self.holds)
        }
    }

    /// One schedule: the writer writes, frees, commits and forks, asking for uploads; once the
    /// fork is made, the child's store, on another host, writes, frees what it inherited and
    /// commits, meanwhile; their uploaders upload; passes of the collector run all along. Then the
    /// uploaders upload what is left and stop, a last pass runs alone, and each branch, recovered
    /// from the bucket, holds what its store's last commit left, each extent reading as it was
    /// written. What went wrong, if anything did.
    fn collected_while_committing() -> Result<(), String> {
        let bucket = Turns::new();
        let mut parent = Session::open(&bucket, BRANCH, 1);
        // Passes, one after another, for as long as the writers write and their uploads go.
        let writing = Arc::new(shuttle::sync::atomic::AtomicBool::new(true));
        let collecting = {
            let (bucket, writing) = (bucket.clone(), writing.clone());
            shuttle::thread::spawn(move || {
                for holder in 100.. {
                    let pass = collector::collect(&*bucket, holder, &COLLECTING);
                    shuttle::future::block_on(pass)
                        .map_err(|err| format!("pass {holder}: {err}"))?;
                    if !writing.load(SeqCst) {
                        return Ok::<_, String>(());
                    }
                }
                unreachable!("passes for ever")
            })
        };

        parent.write(0, 1);
        parent.store.commit_ordered(None).unwrap();
        // Asked for that commit with an extent of its own open, the uploader closes the open
        // segment between commits: its footer's next sequence number counts the extent, which the
        // next segment names, its object uploaded first (TASK-79.3).
        parent.write(1, 3);
        parent.ask();
        parent.write(2, 2);
        parent.free(parent.extent(0));
        parent.store.commit(None).unwrap();
        parent.ask();
        parent.write(3, 1);
        parent.store.commit_fork(Child { branch: CHILD, owner: 9, point: false }).unwrap();
        let fork = parent.store.last_commit().unwrap();
        parent.ask();
        until(|| parent.uploader.forked() >= fork);
        // The child, made by the fork, whose record the collector may not have read yet: what its
        // store uploads, it knows nothing of until it reads the record, its segments not those of a
        // branch it recovered.
        let mut child = Session::open(&bucket, CHILD, 2);
        child.holds = parent.holds.clone();
        child.write(0, 1);
        child.free(parent.extent(1));
        child.store.commit_ordered(None).unwrap();
        child.ask();
        parent.free(parent.extent(1));
        parent.write(4, 2);
        parent.store.commit_ordered(None).unwrap();
        parent.ask();
        child.write(1, 3);
        child.store.commit(None).unwrap();
        child.ask();
        parent.free(parent.extent(3));
        parent.store.commit(None).unwrap();
        parent.ask();
        child.free(parent.extent(2));
        child.write(2, 1);
        child.store.commit(None).unwrap();
        child.ask();
        let parent = parent.end()?;
        let child = child.end()?;
        writing.store(false, SeqCst);
        collecting.join().unwrap()?;
        let pass = collector::collect(&*bucket, 200, &COLLECTING);
        shuttle::future::block_on(pass).map_err(|err| format!("the last pass: {err}"))?;
        differs(&bucket, BRANCH, &parent)?;
        differs(&bucket, CHILD, &child)
    }

    /// Runs `schedule` under `scheduler`'s interleavings.
    fn explore(scheduler: impl Scheduler + 'static, schedule: impl Fn() + Send + Sync + 'static) {
        let mut config = shuttle::Config::new();
        config.stack_size = 4 << 20;
        shuttle::Runner::new(scheduler, config).run(schedule);
    }

    /// TASK-291.1: whatever the interleaving, what each store committed and its uploader
    /// acknowledged stays its branch's, the parent's and its fork's child's, the collector running
    /// all along.
    ///
    /// No rule of the collector's off has it lose anything here in thousands of schedules (seven
    /// tried, `candidates_first` among them): once the branch has a checkpoint of its own, each
    /// pass lists the records again after recovering what they name, which leaves a pass's loss
    /// little room between two of its requests. [`listed_while_forking`] holds a pass up there.
    #[test]
    fn collection_keeps_what_a_store_commits_meanwhile() {
        let schedule = || collected_while_committing().unwrap();
        explore(RandomScheduler::new(runs()), schedule);
        explore(PctScheduler::new(3, runs()), schedule);
    }

    /// One schedule: a pass of the collector, held up as it lists the bucket's objects, while the
    /// store's fork to the child is made and the child's store, opened, commits and its uploader
    /// uploads its segment. Then the pass goes on, the uploaders upload what is left and stop, a
    /// last pass runs alone, and each branch, recovered from the bucket, holds what its store's
    /// last commit left. What went wrong, if anything did.
    fn listed_while_forking() -> Result<(), String> {
        let bucket = Turns::new();
        let mut parent = Session::open(&bucket, BRANCH, 1);
        parent.write(0, 1);
        parent.write(1, 3);
        parent.store.commit_ordered(None).unwrap();
        parent.ask();
        let before = parent.store.last_commit().unwrap();
        until(|| parent.uploader.last_uploaded() >= before);
        parent.store.commit_fork(Child { branch: CHILD, owner: 9, point: false }).unwrap();
        let fork = parent.store.last_commit().unwrap();
        bucket.gate.arm();
        let passing = {
            let bucket = bucket.clone();
            shuttle::thread::spawn(move || {
                let pass = collector::collect(&*bucket, 100, &COLLECTING);
                shuttle::future::block_on(pass).map_err(|err| format!("the pass: {err}"))
            })
        };
        bucket.gate.reached();
        parent.ask();
        until(|| parent.uploader.forked() >= fork);
        let mut child = Session::open(&bucket, CHILD, 2);
        child.holds = parent.holds.clone();
        child.write(0, 1);
        child.free(parent.extent(0));
        child.store.commit_ordered(None).unwrap();
        child.ask();
        let committed = child.store.last_commit().unwrap();
        until(|| child.uploader.last_uploaded() >= committed);
        bucket.gate.open();
        passing.join().unwrap()?;
        let parent = parent.end()?;
        let child = child.end()?;
        let pass = collector::collect(&*bucket, 200, &COLLECTING);
        shuttle::future::block_on(pass).map_err(|err| format!("the last pass: {err}"))?;
        differs(&bucket, BRANCH, &parent)?;
        differs(&bucket, CHILD, &child)
    }

    /// A pass listing the bucket's objects before it reads the branches' records keeps what a
    /// child, made after the listing began, uploads before it ends: the pass reads the child's
    /// record, and its recovery names the segment.
    #[test]
    fn a_pass_keeps_what_a_child_made_as_it_lists_uploads() {
        explore(RandomScheduler::new(runs() / 10), || listed_while_forking().unwrap());
    }

    /// The collector listing the bucket's objects after it read what the branches need, not before:
    /// a child whose record no pass read, made meanwhile, has its segment listed, which no branch
    /// the pass recovered names, and deleted.
    #[test]
    fn a_pass_keeping_what_a_child_made_as_it_lists_uploads_needs_candidates_first() {
        let all = extent_bucket::Rules::ALL;
        let off = extent_bucket::Rules { candidates_first: false, ..all };
        explore(RandomScheduler::new(runs() / 10), move || {
            extent_bucket::set_rules(off);
            let listed = listed_while_forking();
            extent_bucket::set_rules(all);
            let extent = "ExtentId { branch: 8, sequence: 1099511627776 }";
            let segment = extent_bucket::layout::segment(CHILD, 0, 1);
            let lost =
                format!("branch {CHILD}: {extent} reads Err(Lost(\"{extent} in {segment}\"))");
            assert_eq!(listed, Err(lost));
        });
    }
}
