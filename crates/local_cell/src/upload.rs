// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: a cell kept in object storage, and its uploader (backlog doc-7, Uploads;
//! doc-14; TASK-28.5, TASK-165.9), which runs on the host's runtime, its uploads within the host's
//! budget ([`Runtime`]). cladefs's `bucket` keeps the host's configuration, budgets, runtime and
//! errno mapping.
//!
//! **Opening a cell** ([`open`]) is the same whether its cell was here, elsewhere or never
//! anywhere (doc-14, Opening a cell): the cell's writer takes its head, with the epoch of
//! its cell's taking by the cell's placement, or the host's, recovers it and fences its end
//! (`bucket_cell::Writer`); the cell opens as the bucket holds the cell, its write-ahead log
//! put after the cell's end by its commits, what of it landed acknowledged and the rest stamped
//! anew on the writer's chain ([`Recovering::open`]). A log another writer's commits moved past
//! refuses to open (`bucket_cell::Error::Diverged`, `ESTALE`) until `cladefsctl take` drops it;
//! one that plainly diverged before the head is taken refuses before, fencing no writer. A cell
//! is made by its maker, a root, before its cell first opens (`bucket_cell::lifecycle::create`).
//!
//! **A cell at a version** takes no head (doc-20): it holds the version's state, recovered,
//! which nothing writes, and no log; nothing it does goes to the bucket but its forks' records
//! ([`Uploader::forked`], `bucket_cell::lifecycle::fork_version`). Any number of hosts hold one at
//! once.
//!
//! **The uploader** hands the cell's write-ahead log's segments, framed, whose commits are durable
//! ([`LocalCell::closed`]), to the writer (`Writer::add_segment`), each the object it is in the
//! bucket, or stamped anew after a takeover; uploads them, [`IN_FLIGHT`] at once at most, each read
//! only as its upload goes, within the host's budget; and acknowledges them in order once a read of
//! the head shows them the cell's ([`LocalCell::acknowledged`]): their extents are the bucket's
//! from then on, their files the host's cache's. The cell closes its open segment for the uploader
//! when a commit waits for its upload, at a fork, as its interval runs out, and as the uploader
//! stops: dropping it uploads what the cell made durable, its open segment's commits too
//! (TASK-192). Commits waiting to be durable (`COMMIT_UPLOADED`) complete once acknowledged.
//!
//! A fork the cell makes is a fork of the cell once the segment its commit ends is
//! acknowledged, which gives the child its record, and checkpointed ([`Uploader::forked`]): the
//! child's cell is made only then, opening it recovering the child from the bucket. A fork that
//! fails before is deleted in the bucket, as the fork's alone ([`Uploader::unfork`]).
//!
//! A failure of its writer's stops the uploader for good, and the commits waiting for uploads fail
//! with it from then on: its writer fenced, its swap of the cell's head refused as a later writer
//! took it, its cell deleted, or the log diverged, another writer's commits past it. Only opening
//! it anew takes the cell again then. (Until TASK-407, a writer displaced, an earlier
//! writer's segment landed late where its next went, took the cell again, TASK-204: no segment
//! lands where another's goes now, each at a key of its writer's epoch; doc-7, The head.)

#[cfg(test)]
mod encrypted;
#[cfg(all(test, feature = "shuttle"))]
mod shuttle_tests;
#[cfg(test)]
mod tests;

pub use crate::fast_wal::{FastWalConfig, FastWalOpener, FastWalStatus};

use crate::{Dir, DirHold, File, HeldParts, LocalCell, Opening, Recovering, relocations};
use bucket_cell::cache::Cache;
use bucket_cell::fetch::Budget;
use bucket_cell::keys::{self, CellKeys, Ring, RingHold};
use bucket_cell::layout::{CellId, Chain, Child, Head, Origin, epoch_of};
use bucket_cell::mirror::Mirrored;
#[cfg(test)]
use bucket_cell::object_bucket::ObjectBucket;
use bucket_cell::object_bucket::RoutedBucket;
use bucket_cell::wal::TakenCell;
use bucket_cell::{Body, Objects, Recovered, Writer, head, lifecycle};
use event_listener::Event;
use fprint::TypeFingerprint;
use futures::StreamExt as _;
use futures::future::{BoxFuture, FutureExt as _};
use futures::stream::FuturesUnordered;
use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::Poll;
use std::time::{Duration, SystemTime};

/// How many of a cell's segments upload at once. Compressed segments average ~7.5 MiB on build
/// workloads (~5x smaller than the 32 MiB raw `segment_size`), so 32 concurrent PUTs per cell
/// sustain full object-storage line rate across parallel HTTP/1.1 connections while the host's
/// `UPLOAD_BYTES` budget (`Writer::create`) still caps total bytes held in memory before reading
/// any body from disk (TASK-79.19).
pub const IN_FLIGHT: usize = 32;

/// How many segments go between checkpoints, by default ([`Options::checkpoint_every`]).
pub const CHECKPOINT_EVERY: u64 = 64;

/// Minimum uncheckpointed segments uploaded before a clean shutdown writes a checkpoint when no
/// extents were freed since the last checkpoint ([`Options::checkpoint_min_on_close`], hotbed
/// question 535).
pub const CHECKPOINT_MIN_ON_CLOSE: u64 = 8;

/// Default window for coalescing S3/R2 head-swap acknowledgements when no waiter is blocked on
/// durability ([`Options::ack_coalesce`], hotbed question 535).
pub const ACK_COALESCE: Duration = Duration::from_millis(1500);

/// cladefs: the mirror in `bucket` of the version `origin` names, begun in the background on the
/// bucket runtime, its fork to `child` made (doc-23: the first fork of a version into a bucket
/// starts its mirror there; TASK-633.8): nothing unless `child` is of `bucket` and the version of
/// another's cell, nor for one mirrored there already ([`ObjectBucket::mirror_wanted`]), which
/// takes its turn before this returns. What it came to is logged.
fn mirror_wanted(spawn: &Spawn, bucket: &Arc<RoutedBucket>, child: CellId, origin: Origin) {
    let Origin { parent, at, .. } = origin;
    let mirroring = bucket.mirror_wanted(child, origin);
    spawn(async move { logged(mirroring.await, parent, at) }.boxed());
}

/// cladefs: the mirror in `bucket` of the version `cell` was forked from, if of another bucket's
/// cell, begun in the background on `runtime`, the cell's uploader's, as `cell` opens
/// ([`RoutedBucket::mirror_for`]): the host that forked it may have died before it began it. A
/// process that opens cells on a runtime of its own, as a VM's does, makes no other.
fn mirror_for(bucket: &Arc<RoutedBucket>, cell: CellId, runtime: &tokio::runtime::Handle) {
    let mirroring = bucket.clone().mirror_for(cell);
    runtime.spawn(async move {
        match mirroring.await {
            Ok(None) => {}
            Ok(Some((Origin { parent, at, .. }, mirrored))) => {
                logged(Ok(Some(mirrored)), parent, at)
            }
            Err(err) => log::warn!("cladefs: mirroring the version {cell} was forked from: {err}"),
        }
    });
}

/// cladefs: logs what a mirror of `parent`'s version at `at` came to, if one ran.
fn logged(mirrored: Result<Option<Mirrored>, bucket_cell::Error>, parent: CellId, at: u64) {
    match mirrored {
        Ok(None) => {}
        Ok(Some(mirrored)) => log::info!("cladefs: {parent}'s version at {at:x}: {mirrored:?}"),
        Err(err) => log::warn!("cladefs: mirroring {parent}'s version at {at:x}: {err}"),
    }
}

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

/// What a host gives its cells' uploaders: the runtime they run on, sized by the host, not by
/// how many cells it serves, as each waits on object storage mostly, and does its disk work in
/// `block_in_place`, which leaves the runtime's threads to the others (TASK-34); and the budget of
/// the PUTs of objects they upload, each holding its object's bytes while it goes, bounded in
/// bytes, [`UPLOAD_BYTES`], so that a host whose cells are far behind holds no more than these
/// at once, whatever their number (TASK-79.19), and in number.
///
/// cladefs: and the keys it encrypts and reads under (TASK-411.3): its KMS, if it has one, and the
/// keys its fetchers share, which each cell's are held in while it is open.
#[derive(Clone)]
pub struct Runtime {
    pub handle: tokio::runtime::Handle,
    pub uploads: Arc<Budget>,
    pub keys: keys::Host,
}

/// How many cells' uploaders run on `host`, as it changes: what an extent daemon's drain
/// waits to see none of (cladefs TASK-141.4).
pub fn uploaders_on(host: &crate::Host) -> tokio::sync::watch::Receiver<usize> {
    host.uploaders.subscribe()
}

/// How many cells' uploaders run on the process's default host outside `madsim`
/// ([`uploaders_on`]).
#[cfg(not(madsim))]
pub fn uploaders() -> tokio::sync::watch::Receiver<usize> {
    uploaders_on(crate::Host::process())
}

/// Runs `future` to its end on the calling thread in the context of `host`'s bucket runtime
/// ([`bucket_runtime_on`]).
#[allow(clippy::disallowed_methods, reason = "an outermost entry's wait, as above")]
pub fn block_on_host<F: std::future::Future>(
    host: &crate::Host,
    future: F,
) -> Result<F::Output, Failure> {
    crate::blocks!("upload::block_on");
    let runtime = bucket_runtime_on(host)?;
    // Never reached under madsim, whose one thread `blocks!` refuses to block.
    #[cfg(madsim)]
    return Ok(runtime.block_on(future));
    #[cfg(not(madsim))]
    match tokio::runtime::Handle::try_current().map(|current| current.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => {
            Ok(tokio::task::block_in_place(|| runtime.block_on(future)))
        }
        _ => Ok(runtime.block_on(future)),
    }
}

/// Runs `future` to its end on the process's default host's bucket runtime outside `madsim`
/// ([`block_on_host`]).
#[cfg(not(madsim))]
pub fn block_on<F: std::future::Future>(future: F) -> Result<F::Output, Failure> {
    block_on_host(crate::Host::process(), future)
}

/// Under `madsim`, [`block_on`] panics immediately via `crate::blocks!` (tested by
/// `local_cell/tests/madsim.rs`).
#[cfg(madsim)]
pub fn block_on<F: std::future::Future>(future: F) -> Result<F::Output, Failure> {
    drop(future);
    crate::blocks!("upload::block_on");
    unreachable!()
}

/// cladefs: runs `future` to its end as [`block_on_host`] does, panicking if the runtime can't be
/// made.
pub fn wait_on_host<F: std::future::Future>(host: &crate::Host, future: F) -> F::Output {
    block_on_host(host, future).expect("the bucket runtime")
}

/// cladefs: runs `future` to its end on the process's default host's bucket runtime outside
/// `madsim` ([`wait_on_host`]).
#[cfg(not(madsim))]
pub fn wait_on<F: std::future::Future>(future: F) -> F::Output {
    wait_on_host(crate::Host::process(), future)
}

/// Per-host bucket Tokio runtime holder, owned by [`crate::Host`].
pub(crate) struct BucketRuntime(Retried<tokio::runtime::Runtime>);

impl BucketRuntime {
    pub(crate) const fn new() -> Self {
        Self(Retried::new())
    }
}

/// cladefs: `host`'s bucket runtime (TASK-34, TASK-449's review): every cell's uploader and
/// fetches on `host`, so that one runtime serves every connection to `host`'s buckets. Made at
/// its first need; a making that fails, a thread not spawned, is made anew at the next.
pub fn bucket_runtime_on(host: &crate::Host) -> Result<&tokio::runtime::Runtime, Failure> {
    host.bucket_runtime.0.get_or_make(|| {
        failing!("upload::bucket_runtime");
        let threads = std::thread::available_parallelism().map_or(2, |cpus| cpus.get().clamp(2, 8));
        let mut building = tokio::runtime::Builder::new_multi_thread();
        building.worker_threads(threads).thread_name("cladefs bucket");
        // TASK-685.1: madsim's runtime starts no threads of its own.
        #[cfg(not(madsim))]
        building.on_thread_start(crate::shared::prepare_thread);
        building.enable_all().build().map_err(|err| Failure::local(err, "the bucket runtime"))
    })
}

/// cladefs: the process's default host's bucket runtime outside `madsim` ([`bucket_runtime_on`]).
#[cfg(not(madsim))]
pub fn bucket_runtime() -> Result<&'static tokio::runtime::Runtime, Failure> {
    bucket_runtime_on(crate::Host::process())
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

/// Deletes `cell` in `bucket` (`bucket_cell::lifecycle::delete`), as the placement whose fencing
/// token is `token`: its writer stops at its next acknowledgement, no writer takes it again, and
/// the collector reclaims what no other cell inherited from it, as its forks' cells live on
/// (TASK-39.4). Deleting it again does nothing. A later placement's cell, whose token is higher,
/// is not deleted (`bucket_cell::Error::Stale`, TASK-45.1).
///
pub async fn delete_cell<B: bucket_cell::Bucket>(
    bucket: &B,
    cell: CellId,
    token: u64,
) -> Result<(), Failure> {
    Ok(lifecycle::delete(bucket, cell, bucket.random_u64(), token).await?)
}

/// Why uploading stopped, or a cell could not be uploaded to, or opened: the error as it was,
/// typed, which cladefs's `bucket` maps to its errno, in the one place.
#[derive(Clone, Debug, thiserror::Error)]
pub enum Failure {
    /// What object storage's protocol found of the cell, as `bucket_cell` says it: another
    /// writer took it (`Fenced`), it was deleted, a later placement holds it
    /// (`Stale`, TASK-39.7), another writer's commits moved it on from the cell's write-ahead log
    /// (`Diverged`), another cell has a fork's child's ID (`IdTaken`, TASK-79.82), or what it
    /// needs is gone or corrupt.
    #[error(transparent)]
    Bucket(#[from] bucket_cell::Error),
    /// The version a cell is of was deleted: it forks no more, and its forks live on.
    #[error("the version was dropped")]
    VersionDropped,
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
            (Self::VersionDropped, Self::VersionDropped) => true,
            (Self::Local(error), Self::Local(other)) => {
                format!("{error:#}") == format!("{other:#}")
            }
            (Self::Bucket(_) | Self::VersionDropped | Self::Local(_), _) => false,
        }
    }
}

impl Eq for Failure {}

/// The cell's errors are this host's, but what object storage's protocol found of the cell, as
/// a write-ahead log diverged from it.
impl From<anyhow::Error> for Failure {
    fn from(error: anyhow::Error) -> Self {
        match error.downcast::<bucket_cell::Error>() {
            Ok(error) => Self::Bucket(error),
            Err(error) => Self::Local(Arc::new(error)),
        }
    }
}

/// cladefs: a request the bucket refuses for good, as `bucket_cell` says it (board 5861).
impl From<bucket_cell::bucket::Refused> for Failure {
    fn from(refused: bucket_cell::bucket::Refused) -> Self {
        Self::Bucket(refused.into())
    }
}

/// How an uploader uploads.
#[derive(Clone, Debug)]
pub struct Options {
    /// How long local commits wait at most before they are uploaded.
    pub interval: Duration,
    /// Whether already-closed segments wake the uploader immediately without waiting for
    /// `interval` (closing no open segment). True in production; tests that pause background
    /// uploads set this to false.
    pub upload_closed: bool,
    /// How many commits may wait to be uploaded: the next waits, while object storage is out of
    /// reach, rather than the lag grow without bound (doc-7, decision 3).
    pub max_lag: u64,
    /// How many bytes of the cell's write-ahead log may wait to be acknowledged, likewise: what
    /// the local disk holds until uploaded.
    pub max_lag_bytes: u64,
    /// The host's cache of the bucket's objects, which its cells share: what the bucket
    /// acknowledged of a cell's goes into it (TASK-165.8).
    pub cache: Option<Arc<Cache>>,
    /// cladefs: what holds the parts of segments packed as they fill, which its cells share, if
    /// the host has it ([`crate::Opening::held`], TASK-400): else each its own.
    pub held: Option<Arc<HeldParts>>,
    /// The fencing token of a cell never placed: its host's placement's.
    pub token: u64,
    /// cladefs: how many segments go between checkpoints, which recoveries start from: the segments
    /// a checkpoint covers are no longer read by them, and the writer's relocation rounds run at
    /// checkpoints (doc-27).
    pub checkpoint_every: u64,
    /// cladefs: minimum uncheckpointed segments uploaded before a clean shutdown writes a
    /// checkpoint when no extents were freed since the last checkpoint (hotbed question 535).
    /// Freed extents still trigger a clean-close checkpoint so `cladefs-gc` can reclaim them.
    pub checkpoint_min_on_close: u64,
    /// cladefs: how long to coalesce bucket head-swap acknowledgements after a segment upload
    /// completes when no waiter (`SyncCell`, `COMMIT_UPLOADED`, fork, lag stall, disk pressure,
    /// or clean shutdown) is blocked on durability (hotbed question 535). Zero acknowledges
    /// immediately.
    pub ack_coalesce: Duration,
    /// cladefs: how long the commits in a segment the guest is filling wait past the interval for
    /// its next commit, which seals what it writes in it in place, before the interval closes it
    /// anyway (`LocalCell::close_now_unless_filling`, TASK-92.41): a commit waiting for its upload
    /// closes it at once. 30 s: eight VMs writing 768 MiB each on one disk with their bucket took
    /// 10 s and more each, the interval closing the segments they filled at 10 s, their commits
    /// copying out a segment's worth each.
    pub filling_wait: Duration,
    /// cladefs: what its relocation rounds read through, the host's fetchers' budget and memory, if
    /// it has them, with its [`Options::cache`]: so that their GETs, the tails they keep and the
    /// parts they fetch are the host's, governed and shared (Codex's reviews of fa1912379 and
    /// 10149e8e3). Else each round's own ([`relocations`]).
    pub fetching: Option<Fetching>,
    /// cladefs: whether its checkpoint rounds relocate out of the cell's sparse segments
    /// ([`relocations`], doc-27), the only way they shrink: always, but where a caller runs the
    /// rounds itself, as the simulation and tests that count a round's work do.
    pub relocate: bool,
    /// cladefs: the Fast WAL commit mode a taking of the cell gives it (doc-29): `None` keeps
    /// what the cell's head says, `Some(Some(fsync))` enables Fast WAL with that guest fsync
    /// level, and `Some(None)` disables Fast WAL (flushing any active WAL on take).
    pub fast_wal: Option<Option<bucket_cell::layout::Fsync>>,
    /// cladefs: this host's appendable zonal fast WAL bucket and cross-AZ opener (doc-29).
    pub fast_wal_config: Option<FastWalConfig>,
    /// cladefs: large-write threshold in bytes for fast WAL cells (defaults to 256 KiB; extents at
    /// or above this size upload directly to the cold regional bucket before referencing them
    /// metadata-only in the fast WAL).
    pub fast_wal_large_extent: u64,
}

/// cladefs: the budget of a host's fetchers' GETs and the memory they keep tails and parts in
/// ([`Options::fetching`]).
#[derive(Clone)]
pub struct Fetching {
    pub budgets: bucket_cell::fetch::Budgets,
    pub memory: Arc<bucket_cell::fetch::Memory>,
}

impl std::fmt::Debug for Fetching {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fetching").field("budgets", &self.budgets).finish_non_exhaustive()
    }
}

impl Default for Options {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(1),
            upload_closed: true,
            max_lag: 4096,
            max_lag_bytes: 1 << 30,
            cache: None,
            held: None,
            token: 0,
            checkpoint_every: CHECKPOINT_EVERY,
            checkpoint_min_on_close: CHECKPOINT_MIN_ON_CLOSE,
            ack_coalesce: ACK_COALESCE,
            filling_wait: Duration::from_secs(30),
            relocate: true,
            fetching: None,
            fast_wal: None,
            fast_wal_config: None,
            fast_wal_large_extent: crate::fast_wal::DEFAULT_LARGE_EXTENT,
        }
    }
}

/// Opens the cell `recovering` recovered, keeping its cell in `bucket` (see the module's
/// docs): as `holder` of the cell's head, which must differ from every other taking's, with
/// `cell_options`, its uploader uploading as `options` say, on `runtime`. Returns the cell and
/// its uploader; or why it can't be opened: its log diverged, its cell deleted, a later
/// placement's, or out of reach. cladefs: once its uploader, a task, opened it, holding no thread
/// meanwhile (TASK-685.3); dropped before, the uploader stops, as the opener is gone.
pub async fn open<D: Dir>(
    recovering: Recovering<D>,
    cell_options: crate::Options,
    bucket: Arc<RoutedBucket>,
    (holder, options): (u64, Options),
    runtime: &Runtime,
) -> Result<(Arc<LocalCell<D>>, Uploader<D>), Failure> {
    open_with_head(recovering, cell_options, bucket, (holder, options), runtime, None).await
}

/// Like [`open`], reusing an already-fetched [`head::Read`] from the caller (e.g. a pre-open
/// existence check) so taking a cell costs one head GET instead of two (board 6017).
pub async fn open_with_head<D: Dir>(
    recovering: Recovering<D>,
    cell_options: crate::Options,
    bucket: Arc<RoutedBucket>,
    (holder, options): (u64, Options),
    runtime: &Runtime,
    initial_head: Option<head::Read>,
) -> Result<(Arc<LocalCell<D>>, Uploader<D>), Failure> {
    let sleep: Clock = Arc::new(|interval| tokio::time::sleep(interval).boxed());
    let handle = runtime.handle.clone();
    let spawn: Spawn = Arc::new(move |task| drop(handle.spawn(task)));
    let uploads = runtime.uploads.clone();
    let host = recovering.host().cloned().or_else(|| {
        #[cfg(not(madsim))]
        {
            Some(Arc::clone(crate::Host::process()))
        }
        #[cfg(madsim)]
        {
            None
        }
    });
    let shared =
        Shared::new_on(host.clone(), options, uploads, (sleep, spawn), runtime.keys.clone());
    let (started, start) = futures::channel::oneshot::channel();
    let stopped = Arc::new(Stopped::default());
    let stop = stopped.clone();
    let (uploader, uploads) = (shared.clone(), bucket.clone());
    let cell = recovering.cell();
    if let Some(host) = &host {
        host.uploaders.send_modify(|running| *running += 1);
    }
    // A task, not a thread of its own (TASK-34).
    runtime.handle.spawn(async move {
        let ran = std::panic::AssertUnwindSafe(async {
            // Its conditional writes checked before any is relied on (doc-7, The head).
            match uploads.probed(holder).await {
                Ok(()) => {
                    let (options, head) = (cell_options, initial_head);
                    uploader.run(recovering, options, uploads, holder, started, head).await
                }
                // Its opener gone or not, it says why it stopped to whoever listens.
                Err(err) => drop(started.send(Err(Failure::msg(err)))),
            }
        })
        .catch_unwind()
        .await;
        // cladefs: panicked, it fails, and stops, as it does failing (board 8507): before, what
        // waited for its uploads waited for ever, and its stop, and it counted among the running.
        if let Err(panic) = ran {
            uploader.panicked("its uploader", &*panic);
            uploader.wind_up().await;
        }
        // The cell let go of before the uploader says it stopped: so that dropping this returns
        // with nothing of the uploader's holding the cell open, which the process may open again
        // at once (`Dir::claim` refusing a second opening), as cladefsd does a cell an image it
        // imported made (TASK-165.6).
        drop(uploader);
        if let Some(host) = &host {
            host.uploaders.send_modify(|running| *running -= 1);
        }
        stop.say();
        fail::fail_point!("upload::stopped");
    });
    let opened = match start.await {
        Ok(Ok(open_cell)) => open_cell,
        Ok(Err(err)) => {
            stopped.stopped().await;
            return Err(err);
        }
        Err(_) => return Err(Failure::msg("the uploader stopped")),
    };
    let dir = opened.hold_dir()?;
    mirror_for(&bucket, cell, &runtime.handle);
    let incarnation = node_local::Incarnation::now();
    let uploader = Uploader { shared, bucket, stopped: Some(stopped), incarnation, _dir: dir };
    Ok((opened, uploader))
}

/// cladefs: an ephemeral cell's keys, as its local checkpoint keeps them (doc-25, TASK-698.3): its
/// owner, and what its fork gave it, wrapped, as a fork's head holds them: its cell's KEK under its
/// tenant's, its first DEK and the keys it inherits under its cell's KEK. cladefs: they say the
/// cell directory's version, of their own (doc-28, hotbed review 87).
#[derive(Debug, serde::Serialize, serde::Deserialize, TypeFingerprint)]
struct EphemeralKeys {
    owner: u64,
    kek: Option<keys::Kek>,
    deks: Vec<keys::Dek>,
    inherited: Vec<keys::InheritedDek>,
}

/// cladefs: the corpus's ephemeral keys (doc-28): a KEK, a DEK and a DEK inherited, wrapped bytes
/// of no key's.
#[cfg(test)]
pub(crate) fn golden_keys() -> Vec<u8> {
    let wrapped = |byte: u8| extent_keys::Wrapped(vec![byte; 40]);
    let dek = keys::Dek { generation: 1, kek: 1, wrapped: wrapped(0xd1) };
    let keys = EphemeralKeys {
        owner: 0xfedc_ba98,
        kek: Some(keys::Kek { generation: 1, wrapped: wrapped(0xc1) }),
        deks: vec![dek.clone()],
        inherited: vec![keys::InheritedDek { cell: CellId { tenant: 0x2f, id: 3 }, dek }],
    };
    crate::layout::versioned(&keys)
}

/// cladefs: an ephemeral cell's keys, `bytes`, as this build reads them, as their `{:#?}`.
#[cfg(test)]
pub(crate) fn keys_said(bytes: &[u8]) -> Result<String, anyhow::Error> {
    crate::layout::read_versioned::<EphemeralKeys>(bytes).map(|keys| format!("{keys:#?}"))
}

/// cladefs: the layout of an ephemeral cell's keys' types.
#[cfg(test)]
pub(crate) fn keys_fingerprint() -> String {
    <EphemeralKeys as TypeFingerprint>::fingerprint()
}

/// cladefs: creates the initial local checkpoint, encryption ring, and ring hold for a new
/// standalone (parentless) host-local ephemeral cell of `capacity` bytes (cell-directory v19):
/// wraps a fresh cell KEK under its tenant's KEK via `runtime.keys.kms`, wraps a fresh DEK under
/// it, and registers the ring so `ForgetTenantKeys`/`ShredTenant` revokes it immediately. Writes
/// nothing to `bucket`.
pub async fn create_standalone_ephemeral(
    bucket: &RoutedBucket,
    runtime: &Runtime,
    cell: CellId,
    capacity: u64,
    owner: u64,
) -> Result<(crate::LocalCheckpoint, Arc<Ring>, RingHold), Failure> {
    let (kek_secret, kek) = keys::new_cell_kek(&runtime.keys.kms, cell).await?;
    let dek =
        keys::wrap_dek((&kek_secret, kek.generation), cell, 1, &extent_frame::Secret::random());
    let ephemeral_keys = EphemeralKeys {
        owner,
        kek: Some(kek.clone()),
        deks: vec![dek.clone()],
        inherited: Vec::new(),
    };
    let mut head = Head::of(bucket_cell::record::Made::Root, owner);
    head.epoch = 1;
    head.holder = bucket_cell::Bucket::random_u64(bucket);
    head.keks.push(kek);
    head.deks.push(dek);
    let cell_keys = CellKeys::fresh(cell, &head, runtime.keys.kms.clone());
    let ring = keys::ring(bucket, &cell_keys, cell, &head, true).await?;
    let holding = runtime.keys.ring.hold(ring.clone());
    let checkpoint =
        crate::LocalCheckpoint::root(capacity, crate::layout::versioned(&ephemeral_keys));
    Ok((checkpoint, ring, holding))
}

/// cladefs: opens the ephemeral cell `recovering` recovered (doc-25, TASK-698.3), with
/// `cell_options`: its keys, as its local checkpoint keeps them, unwrapped through the host's KMS
/// as its fork's head would give them; for a forked ephemeral cell, its parent's state at its
/// version recovered from `bucket`, its parent's, under them; for a standalone (parentless)
/// ephemeral cell, zero bucket I/O and an empty lower layer. No uploader: nothing of it goes to a
/// bucket. Its parent's extents are read through `cache`, the host's, if it has one. Its keys are
/// held among the host's while what it returns with the cell is, so that the host's fetchers read
/// what it inherits, and forgetting its tenant or cell drops them (TASK-698.7).
pub async fn open_ephemeral<D: Dir>(
    recovering: Recovering<D>,
    cell_options: crate::Options,
    bucket: Arc<RoutedBucket>,
    runtime: &Runtime,
    (cache, held): (Option<Arc<Cache>>, Option<Arc<HeldParts>>),
) -> Result<(Arc<LocalCell<D>>, RingHold, Option<Forked>), Failure> {
    let checkpoint = blocking(|| recovering.local_checkpoint())?;
    let (parent, cell) = (checkpoint.parent(), recovering.cell());
    let EphemeralKeys { owner, kek, deks, inherited } =
        crate::layout::read_versioned(checkpoint.keys())
            .map_err(|err| Failure::msg(format!("an ephemeral cell's keys: {err:#}")))?;
    let origin =
        parent.map(|parent| Origin { parent: parent.cell, at: parent.at, epoch: parent.epoch });
    let made = match origin {
        Some(origin) => bucket_cell::record::Made::Clone(origin),
        None => bucket_cell::record::Made::Root,
    };
    let mut head = Head::of(made, owner);
    // cladefs: use a fresh non-zero taking `(epoch, holder)` on every ephemeral reopen so
    // `CachingKms::kek` unwraps afresh instead of reusing a cached `(0, 0)` unwrap from an
    // earlier open (board 5410).
    head.epoch = 1;
    head.holder = bucket_cell::Bucket::random_u64(&*bucket);
    head.keks.extend(kek);
    (head.deks, head.inherited) = (deks, inherited);
    // cladefs: every ephemeral open gets a fresh taking so reopening after a tenant KEK is
    // destroyed unwraps through the KMS instead of reusing a cached `(0, 0)` taking (board 5410,
    // board 2704).
    let cell_keys = CellKeys::fresh(cell, &head, runtime.keys.kms.clone());
    let ring = keys::ring(&*bucket, &cell_keys, cell, &head, true).await?;
    let (parent_state, forked) = match (parent, origin) {
        (Some(parent), Some(origin)) => {
            let recovering_parent =
                bucket_cell::recovery::recover_version(&*bucket, parent.cell, parent.at, &ring);
            let recovered = recovering_parent.await?;
            let state = recovered.layer.unwrap_or_else(|| Arc::new(recovered.state));
            (Some(state), Some(Forked { origin, ring: ring.clone() }))
        }
        _ => (None, None),
    };
    let holding = runtime.keys.ring.hold(ring.clone());
    let opening = crate::EphemeralOpening { parent: parent_state, keys: ring, cache, held };
    let open_cell = blocking(|| recovering.open_ephemeral(cell_options, opening))?;
    Ok((Arc::new(open_cell), holding, forked))
}

/// cladefs: a cell of a fork of a version, as its read evidence needs it (TASK-702.2): the
/// version, by its origin, and the cell's keys, which hold the version's cell's DEKs, inherited.
#[derive(Clone, Debug)]
pub struct Forked {
    pub origin: Origin,
    pub ring: Arc<Ring>,
}

/// cladefs: what says an uploader stopped (TASK-685.3): awaited by [`Uploader::close`], and by its
/// drop, polled once, then waited for on the dropping thread if it must be (board 2946). The drop
/// may come anywhere, a panic unwinding through an executor among them, so it waits parking the
/// thread ([`crate::sync::block_on`]), which nests no executor.
#[derive(Default)]
struct Stopped {
    stopped: std::sync::atomic::AtomicBool,
    said: Event,
}

impl Stopped {
    /// Says the uploader stopped.
    fn say(&self) {
        self.stopped.store(true, std::sync::atomic::Ordering::Release);
        self.said.notify(usize::MAX);
    }

    fn is_stopped(&self) -> bool {
        self.stopped.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Returns once the uploader stopped, holding no thread.
    async fn stopped(&self) {
        while !self.is_stopped() {
            let said = self.said.listen();
            if self.is_stopped() {
                return;
            }
            said.await;
        }
    }
}

/// A cell's uploader (see the module's docs). Dropping it closes the cell's open segment, if its
/// cell is open still, uploads what the cell made durable, then stops.
pub struct Uploader<D: Dir> {
    shared: Arc<Shared<D>>,
    /// The bucket the cell is kept in.
    bucket: Arc<RoutedBucket>,
    /// Says the uploader stopped, once it has: taken by [`Uploader::close`], or its drop, which
    /// wait for it.
    stopped: Option<Arc<Stopped>>,
    /// cladefs: the incarnation of the host it runs on: one gone since, killed, waits for
    /// nothing as it drops (board 14567).
    incarnation: node_local::Incarnation,
    /// The cell directory, held taken for this process until the uploader stopped, last of its
    /// fields to go (TASK-129).
    _dir: DirHold<D>,
}

pub(crate) struct Shared<D: Dir, B: bucket_cell::Bucket = RoutedBucket> {
    /// Its host context, when backed by a host directory (or process default off madsim).
    pub(super) host: Option<Arc<crate::Host>>,
    /// The cell, once opened.
    cell: std::sync::OnceLock<Arc<LocalCell<D>>>,
    /// The cell's writer, once it took the cell, and as it takes it again (TASK-204).
    writer: Mutex<Option<Arc<Writer<B>>>>,
    /// The fencing token its writers take the cell's head with, once one did.
    token: std::sync::OnceLock<u64>,
    /// cladefs: the version the cell is at, if it is one (TASK-476).
    at_version: std::sync::OnceLock<AtVersion>,
    progress: Mutex<Progress>,
    /// Notified as the progress changes: what waits for uploads listens to it, blocking or not
    /// (TASK-43.6).
    changed: Arc<Event>,
    /// Wakes the uploader: a commit waits, or it is to stop.
    wake: tokio::sync::Notify,
    /// Wakes the uploader when a closed segment finishes writing its framed file, without closing
    /// the open segment or resetting the open-segment `interval` timer.
    segment_ready: Arc<tokio::sync::Notify>,
    options: Options,
    /// The host's budget of uploads, which its writer uploads within.
    uploads: Arc<Budget>,
    /// When its interval is up ([`Clock`]).
    clock: Clock,
    /// cladefs: what it runs its keeper on ([`Spawn`]).
    spawn: Spawn,
    /// The holders of its takings after the first, and the owners of what it makes: from the
    /// first taking's holder on ([`Shared::holder`]).
    holders: Mutex<Holders>,
    /// cladefs: the host's keys ([`Runtime::keys`], TASK-411.3).
    keys: keys::Host,
    /// cladefs: the cell's keys, held among the host's for as long as the uploader lives.
    held: Mutex<Option<RingHold>>,
    /// cladefs: the relocation round under way, if one is ([`Shared::relocating`]): what aborts it,
    /// and what says it went, the cell it held with it, which [`Shared::run`] waits for.
    relocation:
        Mutex<Option<(futures::future::AbortHandle, futures::channel::oneshot::Receiver<()>)>>,
    /// cladefs: active Fast WAL sink for a fast-WAL cell (doc-29), set once, as its writer takes the
    /// cell: no lock a panic could poison for its other users (board 9412).
    fast_wal_sink: std::sync::OnceLock<Arc<crate::fast_wal::FastWalSink>>,
    /// cladefs: completion signal for the background Fast WAL writer task, awaited in `wind_up`.
    fast_wal_done: Mutex<Option<futures::channel::oneshot::Receiver<()>>>,
}

/// The holders an uploader's takings of its cell's head after the first take, and the owners of
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

/// cladefs: what an uploader runs a task of its own beside it on, its keeper of a replicated cell's
/// configuration ([`Shared::keep`]): the host's runtime, or, in the simulation, its scheduler,
/// which polls it as a seed says (TASK-680).
pub(crate) type Spawn = Arc<dyn Fn(BoxFuture<'static, ()>) + Send + Sync>;

#[cfg(not(madsim))]
thread_local! {
    // Per thread, outside the simulation: `cladefs::filesystem::EnterFuture` enters a multi-thread
    // runtime handle on non-multi-thread caller threads (`current_thread` or
    // `futures::executor::block_on`) so Fxfs background tasks spawn on a live runtime, where
    // `tokio::task::block_in_place` would panic (board 11691).
    static WITHOUT_BLOCK_IN_PLACE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Runs `work` with `tokio::task::block_in_place` disabled in [`blocking`] on the current OS
/// thread (used when entering a multi-thread `Handle` from a non-multi-thread thread, board 11691).
pub fn without_block_in_place<T>(work: impl FnOnce() -> T) -> T {
    #[cfg(madsim)]
    return work();
    #[cfg(not(madsim))]
    {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                WITHOUT_BLOCK_IN_PLACE.with(|depth| depth.set(depth.get() - 1));
            }
        }
        WITHOUT_BLOCK_IN_PLACE.with(|depth| depth.set(depth.get() + 1));
        let _reset = Reset;
        work()
    }
}

/// Runs `work`, which blocks on the cell's disk, on the uploaders' runtime: on its thread, which
/// hands its other tasks to another meanwhile; or as it is, on a thread no runtime runs, as the
/// simulation's (`tokio::task::block_in_place` runs it so there). cladefs: the daemon's async
/// paths' disk work too, a cell's making, opening and forking (TASK-685.3).
#[allow(clippy::disallowed_methods, reason = "the work waits on the disk alone")]
pub fn blocking<T>(work: impl FnOnce() -> T) -> T {
    // TASK-685.1: madsim's one thread runs it as it is.
    #[cfg(madsim)]
    return host_fs::may_block(work);
    // cladefs: a current-thread runtime's one thread, as tests' and FUSE storage's are, has no
    // other to hand its tasks to (`block_in_place` panics there): it runs it as it is, as a thread
    // no runtime runs does (TASK-685.3).
    #[cfg(not(madsim))]
    if WITHOUT_BLOCK_IN_PLACE.with(|depth| depth.get() == 0)
        && matches!(
            tokio::runtime::Handle::try_current().map(|runtime| runtime.runtime_flavor()),
            Ok(tokio::runtime::RuntimeFlavor::MultiThread)
        )
    {
        tokio::task::block_in_place(|| host_fs::may_block(work))
    } else {
        host_fs::may_block(work)
    }
}

/// cladefs: a version of its cell a cell is at, and the segment its cell's head names
/// it at (TASK-476): its forks fork the version, which its cell's head must name still.
struct AtVersion {
    origin: Origin,
    version: u64,
    /// cladefs: its cell's state at the version, decoded: what an ephemeral cell forked from it
    /// starts from (doc-25, TASK-698.3).
    state: Arc<bucket_cell::State>,
    /// The ring its forks' checkpoints at the version are sealed under, its cell's newest DEK,
    /// which the forks inherit.
    sealing: Arc<Ring>,
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

/// What a wait on the uploader waits for ([`Shared::ask`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Awaited {
    /// The commits up to an epoch uploaded and acknowledged: a commit's durability, which uploading
    /// failing later takes nothing from.
    Uploaded,
    /// The lag within its bounds (`Options::max_lag`, `Options::max_lag_bytes`), for a commit made
    /// since: refused once uploading failed, as no commit made since is its cell's (TASK-550).
    Lag,
}

/// cladefs: what a waiter on the uploader waits for, having found it not there yet: what the
/// waits' blocking and async forms share (TASK-685.3).
enum Waiting {
    /// What [`Shared::reached`] finds, up to an epoch.
    For(Awaited, u64),
    /// The commits up to an epoch appended to the fast WAL, or uploaded to the cold bucket.
    FastWal(Arc<crate::fast_wal::FastWalSink>, u64),
    /// Fast WAL lag backpressure for `commit_epoch` (waits until `lag_epoch` is within lag OR
    /// `sink.head_acked() >= commit_epoch` so cold-head acknowledgement wakes immediately without
    /// waiting on local WAL release, board 12179).
    FastWalLag(Arc<crate::fast_wal::FastWalSink>, u64, u64),
}

#[derive(Default)]
struct Progress {
    /// The last epoch uploaded.
    uploaded: u64,
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
    /// The writer's taking of the cell's head, once it took it.
    taking: Option<Taking>,
    /// The cell's latest commits not uploaded, as the uploader saw them, and when it did: the
    /// first is when the oldest commit waiting was seen first (TASK-50.5).
    seen: VecDeque<(u64, SystemTime)>,
}

/// cladefs: a cell's versions ([`Remote::versions`], TASK-476), by their commits' levels.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Versions {
    /// Those its cell's head names, uploaded, oldest first.
    pub uploaded: Vec<bucket_cell::layout::Version>,
    /// Those its commits cut that the head names not yet, local, by their commits' numbers.
    pub local: Vec<(u64, bucket_cell::layout::NewVersion)>,
}

/// A writer's taking of its cell's head: with the fencing token it took it with, of the epoch,
/// and when.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Taking {
    /// The fencing token it was taken with: the epoch of its cell's taking by this placement
    /// (`bucket_cell::cell::Held::epoch`, TASK-364).
    pub token: u64,
    /// Its epoch: one more than the taking's before.
    pub epoch: u64,
    /// When the writer took it.
    pub taken: SystemTime,
    /// cladefs: its cell's Fast WAL fsync level as the head it took says (doc-29): none unless
    /// the cell uses Fast WAL.
    pub fast_wal: Option<bucket_cell::layout::Fsync>,
}

/// What a cell's uploader has done, and has left to do (TASK-50.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadStatus {
    /// Its writer's taking of its cell's head, once taken.
    pub taking: Option<Taking>,
    /// The epoch of the cell's latest commit uploaded.
    pub uploaded: u64,
    /// The bytes of its write-ahead log not yet acknowledged.
    pub pending_bytes: u64,
    /// When the uploader first saw the oldest commit not yet uploaded, if one is not: within its
    /// interval of when the commit was made.
    pub oldest_pending: Option<SystemTime>,
    /// How many commits waited for the lag to shrink ([`Uploader::stalls`]).
    pub stalls: u64,
    /// cladefs: of a Fast WAL cell, its live fast WAL status (doc-29).
    pub fast_wal: Option<FastWalStatus>,
}

impl Progress {
    /// Notes the cell's commits through `epoch` uploaded.
    fn uploaded_through(&mut self, epoch: u64) {
        self.uploaded = self.uploaded.max(epoch);
        while self.seen.front().is_some_and(|(seen, _)| *seen <= self.uploaded) {
            self.seen.pop_front();
        }
    }
}

impl<D: Dir> Uploader<D> {
    /// The cell.
    fn cell(&self) -> &Arc<LocalCell<D>> {
        self.shared.cell()
    }

    /// Returns once the commits up to `epoch` are uploaded and acknowledged: a future, which holds
    /// no thread while it waits, as for a sync that object storage keeps waiting (TASK-43.6).
    pub fn uploaded(
        &self,
        epoch: u64,
    ) -> impl std::future::Future<Output = Result<(), Failure>> + Send + use<D> {
        self.awaiting(self.waiting_uploaded(epoch))
    }

    /// What waiting for the commits up to `epoch` uploaded waits for, if they aren't, their
    /// upload asked for.
    fn waiting_uploaded(&self, epoch: u64) -> Result<Option<Waiting>, Failure> {
        let waiting = Waiting::For(Awaited::Uploaded, epoch);
        Ok((!self.shared.ask(epoch, Awaited::Uploaded)?).then_some(waiting))
    }

    /// cladefs: returns once what `waiting` says is there, if anything, holding no thread while it
    /// waits ([`Shared::wait`], TASK-685.3).
    fn awaiting(
        &self,
        waiting: Result<Option<Waiting>, Failure>,
    ) -> impl std::future::Future<Output = Result<(), Failure>> + Send + use<D> {
        let shared = self.shared.clone();
        async move {
            match waiting? {
                Some(waiting) => shared.wait(&waiting).await,
                None => Ok(()),
            }
        }
    }

    /// The epoch of the cell's latest commit uploaded and acknowledged, as the uploader says,
    /// whether or not it stopped.
    pub fn last_uploaded(&self) -> u64 {
        self.shared.last_uploaded()
    }

    /// What the uploader has done, and has left to do.
    pub fn status(&self) -> Result<UploadStatus, Failure> {
        let shared = &self.shared;
        shared.observe()?;
        let pending_bytes = self.cell().wal_bytes()?;
        let fast_wal = shared.fast_wal_sink.get().map(|sink| sink.status());
        let progress = shared.progress();
        Ok(UploadStatus {
            taking: progress.taking,
            uploaded: progress.uploaded,
            pending_bytes,
            oldest_pending: progress.seen.front().map(|(_, seen)| *seen),
            stalls: progress.stalls,
            fast_wal,
        })
    }

    /// cladefs: the Fast WAL fsync level of the cell, as its writer's taking found its head
    /// (doc-29): none unless the cell uses Fast WAL.
    pub fn fast_wal_fsync(&self) -> Option<bucket_cell::layout::Fsync> {
        self.shared.progress().taking.and_then(|taking| taking.fast_wal)
    }

    /// cladefs: waits until local cache lag for `epoch` is within bounds, OR returns immediately
    /// as soon as the cold bucket head itself acknowledges `epoch` (`sink.head_acked() >= epoch`),
    /// even if cold acknowledgement arrives after waiting began (board 12179).
    pub fn fast_wal_lag(
        &self,
        epoch: u64,
    ) -> impl std::future::Future<Output = Result<(), Failure>> + Send + use<D> {
        let waiting = (|| -> Result<Option<Waiting>, Failure> {
            let sink = self.shared.fast_wal_sink.get().cloned();
            let Some(sink) = sink else {
                return self.waiting_lag(epoch);
            };
            if sink.head_acked() >= epoch {
                return Ok(None);
            }
            let lag_epoch = epoch.saturating_sub(self.shared.options.max_lag);
            if self.shared.ask(lag_epoch, Awaited::Lag)? {
                return Ok(None);
            }
            Ok(Some(Waiting::FastWalLag(sink, epoch, lag_epoch)))
        })();
        self.awaiting(waiting)
    }

    /// cladefs: returns once the commits up to `epoch` are durable in the cell's fast WAL or
    /// acknowledged in the cold regional bucket (doc-29).
    pub fn fast_wal(
        &self,
        epoch: u64,
    ) -> impl std::future::Future<Output = Result<(), Failure>> + Send + use<D> {
        let waiting = (|| -> Result<Option<Waiting>, Failure> {
            self.shared.observe()?;
            let sink = self.shared.fast_wal_sink.get().cloned();
            let Some(sink) = sink else {
                return self.waiting_uploaded(epoch);
            };
            let progress = self.shared.progress();
            if sink.durable().max(sink.head_acked()).max(progress.uploaded) >= epoch {
                return Ok(None);
            }
            if let Some(err) = progress.failed.clone().or_else(|| sink.failed()) {
                return Err(err);
            }
            drop(progress);
            if sink.degraded() {
                self.shared.progress().wanted = true;
                self.shared.wake.notify_one();
            }
            Ok(Some(Waiting::FastWal(sink, epoch)))
        })();
        self.awaiting(waiting)
    }

    /// cladefs: returns once the lag is within its bounds, as [`Uploader::lag`] does: a future,
    /// which holds no thread while it waits (TASK-685.3).
    pub fn lag(
        &self,
        epoch: u64,
    ) -> impl std::future::Future<Output = Result<(), Failure>> + Send + use<D> {
        self.awaiting(self.waiting_lag(epoch))
    }

    /// cladefs: what waiting for the lag within its bounds, with `epoch`'s commit, waits for, if it
    /// isn't, a stall counted ([`Uploader::lag`]).
    fn waiting_lag(&self, epoch: u64) -> Result<Option<Waiting>, Failure> {
        let epoch = epoch.saturating_sub(self.shared.options.max_lag);
        let waiting = Waiting::For(Awaited::Lag, epoch);
        Ok((!self.shared.ask(epoch, Awaited::Lag)?).then_some(waiting))
    }

    /// Makes the fork of the version the cell is at to `child` (`lifecycle::fork_version`): its
    /// head, which fails if the version was dropped ([`Failure::VersionDropped`]), and, for a child
    /// of another cell, the checkpoint at the version, sealed under the version's cell's keys
    /// (TASK-476).
    ///
    /// cladefs: the child's head made with the keys `inheriting` gives ([`Uploader::inheriting`]):
    /// a KEK from the host's pool, its DEK and those it inherits, rewrapped under that KEK
    /// (TASK-411.4). Failing, the fork fails, which its host deletes. It says what it left of the
    /// child's head to the child's cell's first opening here ([`Learned`], TASK-449).
    pub async fn forked(&self, child: Child, inheriting: keys::Inheriting) -> Result<(), Failure> {
        failing!("upload::forked");
        let at = self.shared.at_version.get().expect("a fork is of a cell at a version");
        let keys = keys::Parents::Held(&at.sealing);
        let forking = lifecycle::fork_version_keyed(
            &*self.bucket,
            child.cell,
            at.version,
            at.origin,
            (child.owner, Some(&inheriting)),
            keys,
        );
        match forking.await {
            Err(bucket_cell::Error::Deleted) => return Err(Failure::VersionDropped),
            Err(err) => return Err(err.into()),
            Ok(()) => {}
        }
        inheriting.made();
        mirror_wanted(&self.shared.spawn, &self.bucket, child.cell, at.origin);
        Ok(())
    }

    /// cladefs: forks an uploaded version `version` of this writable cell directly in the bucket
    /// without materializing a temporary version directory on disk (board 11701).
    pub async fn fork_version(
        &self,
        version: u64,
        child: Child,
        inheriting: keys::Inheriting,
    ) -> Result<(), Failure> {
        failing!("upload::forked");
        let cell = self.cell().description().cell;
        let writer = self.shared.writer().ok_or_else(|| Failure::msg("no writer: no versions"))?;
        let named = writer
            .versions()
            .await
            .into_iter()
            .find(|v| v.id == version)
            .ok_or(Failure::VersionDropped)?;
        let origin = Origin { parent: cell, at: named.at, epoch: named.epoch };
        let sealing = self.cell().keys().clone();
        let keys = keys::Parents::Held(&sealing);
        let forking = lifecycle::fork_version_keyed(
            &*self.bucket,
            child.cell,
            version,
            origin,
            (child.owner, Some(&inheriting)),
            keys,
        );
        match forking.await {
            Err(bucket_cell::Error::Deleted) => return Err(Failure::VersionDropped),
            Err(err) => return Err(err.into()),
            Ok(()) => {}
        }
        inheriting.made();
        mirror_wanted(&self.shared.spawn, &self.bucket, child.cell, origin);
        Ok(())
    }

    /// cladefs: forks the version the cell is at to `child`, an ephemeral cell kept on the host of
    /// incarnation `incarnation` alone (doc-25, TASK-698.3): its reference written in the bucket
    /// before the cell's head is read, which must name the version still
    /// (`lifecycle::fork_ephemeral`); then what `inheriting` gives it, its keys, kept in its local
    /// checkpoint, which it returns, its state the version's. Fails as [`Uploader::forked`] does.
    pub async fn forked_ephemeral(
        &self,
        child: Child,
        inheriting: keys::Inheriting,
        incarnation: u64,
    ) -> Result<(crate::LocalCheckpoint, Arc<Ring>), Failure> {
        failing!("upload::forked_ephemeral");
        let at = self.shared.at_version.get().expect("a fork is of a cell at a version");
        let forking = lifecycle::fork_ephemeral(
            &*self.bucket,
            child.cell,
            at.version,
            at.origin,
            (incarnation, keys::Parents::Held(&at.sealing)),
        );
        match forking.await {
            Err(bucket_cell::Error::Deleted) => return Err(Failure::VersionDropped),
            Err(err) => return Err(err.into()),
            Ok(()) => {}
        }
        let given = inheriting.spend();
        let (deks, inherited) = given.deks.unwrap_or_default();
        let keys = EphemeralKeys { owner: child.owner, kek: given.kek, deks, inherited };
        inheriting.made();
        let mut head = Head::of(bucket_cell::record::Made::Clone(at.origin), child.owner);
        head.keks.extend(keys.kek.clone());
        (head.deks, head.inherited) = (keys.deks.clone(), keys.inherited.clone());
        let cell_keys = CellKeys::fresh(child.cell, &head, self.shared.keys.kms.clone());
        let ring = keys::ring(&*self.bucket, &cell_keys, child.cell, &head, true).await?;
        let parent = crate::Parent {
            cell: at.origin.parent,
            version: at.version,
            at: at.origin.at,
            epoch: at.origin.epoch,
        };
        let checkpoint =
            crate::LocalCheckpoint::fork(parent, &at.state, crate::layout::versioned(&keys));
        Ok((checkpoint, ring))
    }

    /// cladefs: what a fork of the cell to `child` gives its child, asked for before anything of
    /// the fork is made, its commit or its head, so that a fork refused makes nothing (TASK-411.4):
    /// the cell's keys, which it inherits, and a KEK for the child's cell from the host's pool.
    /// Refused if the host's KMS last heard a tenant of theirs revoked or destroyed, or as the KMS
    /// refuses a KEK (`keys::inheriting`). No KMS call is made with the pool not empty.
    pub async fn inheriting(&self, child: Child) -> Result<keys::Inheriting, Failure> {
        let source = self.cell().keys();
        Ok(keys::inheriting(&self.shared.keys.kms, source, child).await?)
    }

    /// Deletes `child` in object storage, a fork of the version the cell is at that
    /// [`Uploader::forked`] failed to make: its record may land after all, a dead host's, which
    /// would keep what it inherited for ever, no cell holding it. Only a cell the fork made is
    /// deleted, its record naming the fork's owner: one of another's, which took the child's ID, is
    /// left as it is (TASK-79.82). It is deleted as the host's placement (`Options::token`).
    pub async fn unfork(&self, child: Child) -> Result<(), Failure> {
        let (holder, token) = (self.shared.holder(), self.shared.options.token);
        let deleting =
            lifecycle::delete_fork(&*self.bucket, child.cell, child.owner, (holder, token));
        if !deleting.await? {
            log::warn!(
                "cladefs: leaving cell {} in object storage, another's: a fork to its ID failed \
                 here",
                child.cell
            );
        }
        Ok(())
    }

    /// cladefs: the cell's versions (doc-18, TASK-476): those its cell's head names, uploaded,
    /// as its writer last swapped it, oldest first; then those its commits cut that the head names
    /// not yet, local, each by its commit's number, which [`Remote::uploaded`] waits for. None of a
    /// version's or a cell at a version, which has no writer.
    pub async fn versions(&self) -> Versions {
        let uploaded = match self.shared.writer() {
            Some(writer) => writer.versions().await,
            None => Vec::new(),
        };
        let local = self.cell().local_versions(self.last_uploaded());
        let local = (local.into_iter())
            .filter(|(_, cut)| uploaded.iter().all(|version| version.id != cut.id))
            .collect();
        Versions { uploaded, local }
    }

    /// cladefs: drops the versions `ids` names, uploaded or local: the head swapped to name them
    /// no more, and no acknowledgement naming those local (`Writer::drop_versions`, TASK-476).
    /// Fails with no writer, of a version's or a cell at a version.
    pub async fn drop_versions(&self, ids: &BTreeSet<u64>) -> Result<(), Failure> {
        failing!("upload::drop_versions");
        let writer = self.shared.writer().ok_or_else(|| Failure::msg("no writer: no versions"))?;
        self.cell().forget_versions(ids);
        Ok(writer.drop_versions(ids).await?)
    }

    /// cladefs: shares the uploaded version `id`, if `shared`, or no longer: other tenants' cells
    /// may restore it while it is (`Writer::share_version`, TASK-476).
    pub async fn share_version(&self, id: u64, shared: bool) -> Result<(), Failure> {
        let writer = self.shared.writer().ok_or_else(|| Failure::msg("no writer: no versions"))?;
        Ok(writer.share_version(id, shared).await?)
    }

    /// Whether it is a cell at a version: nothing it does goes to object storage but its
    /// forks' records.
    pub fn at_version_origin(&self) -> Option<Origin> {
        self.shared.at_version.get().map(|at| at.origin)
    }

    /// cladefs: of the cell of a fork of a version, once its writer took the cell, the version
    /// and its keys ([`Forked`], TASK-702.2); none of another cell's, or at a version.
    pub fn forked_from(&self) -> Option<Forked> {
        let writer = self.shared.writer()?;
        let origin = writer.taken().head.origin()?;
        Some(Forked { origin, ring: writer.keys().clone() })
    }

    /// Whether its cell is at a version, which nothing writes (TASK-476).
    pub fn is_at_version(&self) -> bool {
        self.shared.at_version.get().is_some()
    }

    /// The lowest sequence number the cell's guest may mint extents' IDs from, which the writer's
    /// epoch gives: no writer of the cell before minted from there.
    pub fn next_sequence(&self) -> u64 {
        self.shared.next_sequence()
    }

    /// The bucket the cell is kept in.
    pub fn bucket(&self) -> &RoutedBucket {
        &self.bucket
    }

    /// What it keeps the cell as.
    pub fn options(&self) -> &Options {
        &self.shared.options
    }

    /// How many commits waited for the lag to shrink.
    pub fn stalls(&self) -> u64 {
        self.shared.progress().stalls
    }

    /// Why uploading stopped, if it did: the open cell's commits are the cell's no more.
    pub fn failed(&self) -> Option<Failure> {
        self.shared.failed()
    }

    /// What has the uploader, once it stops, or as it is stopping, stop at once (TASK-216): as a
    /// writer opening the cell again here waits for it, which the cell's write-ahead log keeps
    /// what it hadn't uploaded for, and whose opening uploads it. Stopped so, its uploads in flight
    /// are given up on, as a crashed host's are, landing or not, and its writer writes no
    /// checkpoint. Its uploader gone, it does nothing. Stopping as dropping it does otherwise, it
    /// uploads all its cell made durable first, which takes as long as the bucket takes.
    pub fn cut_short(&self) -> impl Fn() + Send + Sync + 'static {
        let shared = Arc::downgrade(&self.shared);
        move || {
            let Some(shared) = shared.upgrade() else { return };
            shared.progress().cut = true;
            shared.wake.notify_one();
        }
    }
}

impl<D: Dir> Uploader<D> {
    /// cladefs: stops it, as dropping it does, and returns once it stopped: its cell's open segment
    /// closed, if its cell is open still, what the cell made durable uploaded, and nothing of it
    /// holding the cell open, which may be opened again at once (TASK-685.3). Holds no thread
    /// while it waits, as the daemon closes its cells: dropping it, unclosed, blocks, unless its
    /// task stopped already.
    pub async fn close(mut self) {
        self.shared.stop();
        if let Some(stopped) = self.stopped.take() {
            stopped.stopped().await;
        }
    }

    /// cladefs: stops it, as [`Uploader::close`] does, and returns once it stopped, for an owner
    /// that holds it shared: dropped after, it waits for nothing (board 5614).
    pub async fn stop(&self) {
        self.shared.stop();
        if let Some(stopped) = &self.stopped {
            stopped.stopped().await;
        }
    }
}

impl<D: Dir> Drop for Uploader<D> {
    fn drop(&mut self) {
        self.shared.stop();
        // cladefs: its host dying, killed, as a simulated host is, nothing waits: its task went
        // with it (doc-24). Or killed since, its drop coming after the host started again
        // (`node_local::Incarnation`, board 14567).
        if self.incarnation.is_gone() {
            return;
        }
        let Some(stopped) = self.stopped.take() else { return };
        // cladefs: dropped unclosed, its stop waited for here: at once, if its task stopped
        // already; or blocking the dropping thread, an outermost entry's, while its task stops
        // (board 2946).
        let mut stopping = std::pin::pin!(stopped.stopped());
        let waker = std::task::Waker::noop();
        if let Poll::Pending = stopping.as_mut().poll(&mut std::task::Context::from_waker(waker)) {
            crate::blocks!(
                "an uploader dropped unclosed, waiting for it to stop (Uploader::close)"
            );
            crate::sync::block_on(stopping);
        }
    }
}

/// cladefs: its disk under pressure urges it: it closes its cell's open segment at its next
/// gather, whatever its guest is doing, and uploads it, so that what it held leaves the disk.
impl<D: Dir, B: bucket_cell::Bucket> bucket_cell::host_disk::Urge for Shared<D, B> {
    fn urge(&self) {
        self.progress().wanted = true;
        self.wake.notify_one();
    }
}

impl<D: Dir, B: bucket_cell::Bucket> Shared<D, B> {
    /// An uploader that uploads as `options` say, within `uploads`, its interval running out on
    /// `clock`: started by [`Shared::run`]. cladefs: encrypting and reading under `keys`, the
    /// host's (TASK-411.3); its keeper run on `spawn`.
    pub(crate) fn new_on(
        host: Option<Arc<crate::Host>>,
        options: Options,
        uploads: Arc<Budget>,
        (clock, spawn): (Clock, Spawn),
        keys: keys::Host,
    ) -> Arc<Self> {
        Arc::new(Self {
            host,
            cell: std::sync::OnceLock::new(),
            writer: Default::default(),
            token: Default::default(),
            at_version: Default::default(),
            progress: Mutex::new(Progress::default()),
            changed: Arc::new(Event::new()),
            wake: tokio::sync::Notify::new(),
            segment_ready: Arc::new(tokio::sync::Notify::new()),
            options,
            uploads,
            clock,
            spawn,
            holders: Mutex::default(),
            keys,
            held: Mutex::default(),
            relocation: Mutex::default(),
            fast_wal_sink: std::sync::OnceLock::new(),
            fast_wal_done: Mutex::default(),
        })
    }

    /// cladefs: holds `ring`, the cell's keys, among the host's, which its fetchers read under,
    /// from now on.
    fn hold_ring(&self, ring: &Arc<Ring>) {
        *self.held.lock().unwrap() = Some(self.keys.ring.hold(ring.clone()));
    }

    /// A holder, or an owner, of the uploader's own, after its first taking's ([`Holders`]).
    fn holder(&self) -> u64 {
        self.holders.lock().unwrap().next()
    }

    /// cladefs: its progress, locked, as every user of it locks it: a panic holding it, which
    /// left it as the panic found it, half changed perhaps, fails the uploader, unless it failed
    /// already, naming that, as [`Shared::panicked`] does; and its poison is cleared, so that what
    /// takes it next, a commit's wait, a close, a drop, finds the failure, rather than panic in
    /// turn (board 9412): `Uploader::drop`, unwinding already, aborted the process so.
    fn progress(&self) -> MutexGuard<'_, Progress> {
        self.progress_or(|| {
            let failure = Failure::msg("a panic left its progress as it held it");
            log::error!("cladefs: uploading a cell: {failure}");
            failure
        })
    }

    /// cladefs: its progress, locked, as [`Shared::progress`] says, `failure` its failure if a
    /// panic holding it left it poisoned: the waiters, and the uploader, woken to find it.
    fn progress_or(&self, failure: impl FnOnce() -> Failure) -> MutexGuard<'_, Progress> {
        self.progress.lock().unwrap_or_else(|poisoned| {
            self.progress.clear_poison();
            let mut progress = poisoned.into_inner();
            progress.failed.get_or_insert_with(failure);
            self.changed.notify(usize::MAX);
            self.wake.notify_one();
            progress
        })
    }

    /// The epoch of the cell's latest commit uploaded and acknowledged.
    pub(crate) fn last_uploaded(&self) -> u64 {
        self.progress().uploaded
    }

    /// The lowest sequence number the cell's guest may mint extents' IDs from.
    pub(crate) fn next_sequence(&self) -> u64 {
        self.progress().next_sequence
    }

    /// Why uploading stopped, if it did.
    pub(crate) fn failed(&self) -> Option<Failure> {
        let failed = self.progress().failed.clone();
        failed.or_else(|| self.fast_wal_sink.get().and_then(|sink| sink.failed()))
    }

    /// The cell's writer, once one took it.
    pub(crate) fn writer(&self) -> Option<Arc<Writer<B>>> {
        self.writer.lock().unwrap().clone()
    }

    /// Has the uploader stop: once it uploaded what its cell made durable, its open segment's
    /// commits too, unless cut short ([`Uploader::cut_short`]).
    pub(crate) fn stop(&self) {
        self.progress().stop = true;
        if let Some(sink) = self.fast_wal_sink.get() {
            sink.stop();
        }
        self.changed.notify(usize::MAX);
        self.wake.notify_one();
    }

    /// A sleep for the open-segment close interval, or immediate in a buggified simulation turn.
    fn interval_sleep(&self, interval: Duration) -> BoxFuture<'static, ()> {
        // cladefs: in a simulation, the interval runs out at once, uploading what it holds now,
        // of any size (doc-24).
        if bucket_cell::buggify!() {
            return futures::future::ready(()).boxed();
        }
        (self.clock)(interval)
    }

    fn cell(&self) -> &Arc<LocalCell<D>> {
        self.cell.get().expect("the uploader opened its cell")
    }

    /// Notes the cell's latest commit, if it isn't uploaded, as seen now.
    fn observe(&self) -> Result<(), Failure> {
        let epoch = self.cell().last_commit()?;
        let mut progress = self.progress();
        let newer = progress.seen.back().is_none_or(|(seen, _)| *seen < epoch);
        if epoch > progress.uploaded && newer {
            progress.seen.push_back((epoch, SystemTime::now()));
        }
        Ok(())
    }

    /// Whether what `awaited` waits for, up to `epoch`, is there ([`Shared::reached`]): if not,
    /// asks the uploader for it, counting a stall if it is the lag.
    pub(crate) fn ask(&self, epoch: u64, awaited: Awaited) -> Result<bool, Failure> {
        self.observe()?;
        let mut progress = self.progress();
        if self.reached(&progress, epoch, awaited).transpose()?.is_some() {
            return Ok(true);
        }
        progress.wanted = true;
        if awaited == Awaited::Lag {
            progress.stalls += 1;
            let pending = self.cell().wal_bytes()?;
            let commits = epoch.saturating_sub(progress.uploaded);
            // A clock gone back: no age to say.
            let oldest = progress.seen.front().and_then(|(_, seen)| seen.elapsed().ok());
            let max = self.options.max_lag_bytes;
            log::warn!("{}", stalled(pending, max, commits, oldest));
        }
        drop(progress);
        self.wake.notify_one();
        Ok(false)
    }

    /// What waiting for `awaited` up to `epoch` finds in `progress`: done, or failed, or, if
    /// nothing yet, `None`. The commits up to `epoch` uploaded are done, whether or not uploading
    /// failed since; the lag, which a commit made since waits for, is never done once it failed,
    /// that commit its cell's no more (TASK-550).
    fn reached(
        &self,
        progress: &Progress,
        epoch: u64,
        awaited: Awaited,
    ) -> Option<Result<(), Failure>> {
        let failed = || progress.failed.clone().map(Err);
        match awaited {
            Awaited::Lag => failed().or_else(|| match self.within_lag(progress.uploaded, epoch) {
                Ok(true) => Some(Ok(())),
                Ok(false) => None,
                Err(err) => Some(Err(err)),
            }),
            Awaited::Uploaded if progress.uploaded >= epoch => Some(Ok(())),
            Awaited::Uploaded => failed(),
        }
    }

    /// Whether, `uploaded` up to, the commits up to `epoch` are, and the bytes of the write-ahead
    /// log within `Options::max_lag_bytes`: bytes only bound how far commits go ahead, not when one
    /// is durable.
    fn within_lag(&self, uploaded: u64, epoch: u64) -> Result<bool, Failure> {
        let max_bytes = self.options.max_lag_bytes;
        let effective_uploaded =
            self.fast_wal_sink.get().map_or(uploaded, |sink| uploaded.max(sink.head_acked()));
        Ok(effective_uploaded >= epoch && self.cell().wal_bytes()? <= max_bytes)
    }

    /// cladefs: what `waiting` finds in `progress`: done, or failed, or, if nothing yet, `None`
    /// ([`Shared::reached`]; a replicator's commits told replicated).
    fn found(&self, progress: &Progress, waiting: &Waiting) -> Option<Result<(), Failure>> {
        match waiting {
            Waiting::For(awaited, epoch) => self.reached(progress, *epoch, *awaited),
            Waiting::FastWal(sink, epoch)
                if sink.durable().max(sink.head_acked()).max(progress.uploaded) >= *epoch =>
            {
                Some(Ok(()))
            }
            Waiting::FastWal(sink, _) => progress.failed.clone().or_else(|| sink.failed()).map(Err),
            Waiting::FastWalLag(sink, commit_epoch, _) if sink.head_acked() >= *commit_epoch => {
                Some(Ok(()))
            }
            Waiting::FastWalLag(_, _, lag_epoch) => {
                self.reached(progress, *lag_epoch, Awaited::Lag)
            }
        }
    }

    /// cladefs: returns once the progress has what `waiting` waits for, done or failed, holding no
    /// thread while it waits (TASK-685.3). Listener is registered before checking state so
    /// lock-free fast-WAL / replicator durability updates can never lose a wakeup.
    async fn wait(&self, waiting: &Waiting) -> Result<(), Failure> {
        loop {
            let changed = self.changed.listen();
            {
                let progress = self.progress();
                if let Some(found) = self.found(&progress, waiting) {
                    return found;
                }
            }
            changed.await;
        }
    }

    /// Runs the uploader of the cell `recovering` recovered, opening it first: returns once it
    /// stopped, its keeper too, if it has one, so that nothing of it holds the cell open
    /// (TASK-701).
    pub(crate) async fn run(
        self: &Arc<Self>,
        recovering: Recovering<D>,
        options: crate::Options,
        bucket: Arc<B>,
        holder: u64,
        started: futures::channel::oneshot::Sender<Result<Arc<LocalCell<D>>, Failure>>,
        initial_head: Option<head::Read>,
    ) {
        self.upload(recovering, options, bucket, holder, started, initial_head).await;
        self.wind_up().await;
    }

    /// cladefs: what an uploader waits for as it stops, however it stopped, returning or panicking
    /// (board 8507): a relocation round under way ends with it, its cell let go (TASK-685.3); and
    /// its keeper (TASK-701). Its locks taken as a panic holding one left them, poisoned or not:
    /// what it takes of them is all it changes.
    async fn wind_up(&self) {
        if let Some(sink) = self.fast_wal_sink.get() {
            sink.stop();
        }
        let fast_wal_done =
            self.fast_wal_done.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        if let Some(done) = fast_wal_done {
            // Said as the fast WAL writer task exits, or goes with its sender.
            let _ = done.await;
        }
        let relocation = self.relocation.lock();
        let round = relocation.unwrap_or_else(std::sync::PoisonError::into_inner).take();
        if let Some((abort, gone)) = round {
            abort.abort();
            // Sent or dropped as the round's task ends: either is what is waited for.
            let _ = gone.await;
        }
    }

    /// cladefs: stops uploading, `what` having panicked, its task or its keeper's, with `panic`
    /// (board 8507): commits waiting for uploads, and forks, fail, naming it, as at
    /// [`Shared::fail`], unless it failed already; and the uploader stops, woken (its keeper's
    /// panic) or not (its own). Its progress taken as a panic holding it left it, poisoned or not
    /// ([`Shared::progress_or`]): its failure is all it changes of it.
    fn panicked(&self, what: &str, panic: &(dyn std::any::Any + Send)) {
        let message = (panic.downcast_ref::<&str>().copied())
            .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("of no message");
        let failure = format!("{what} panicked: {message}");
        let writer = self.writer.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        match writer {
            Some(writer) => log::error!("cladefs: uploading cell {}: {failure}", writer.cell()),
            None => log::error!("cladefs: opening a cell to upload it: {failure}"),
        }
        let failure = Failure::msg(failure);
        let mut progress = self.progress_or(|| failure.clone());
        progress.failed.get_or_insert(failure);
        drop(progress);
        self.changed.notify(usize::MAX);
        self.wake.notify_one();
    }

    /// Runs the uploader, as [`Shared::run`] does, but for waiting for its keeper.
    async fn upload(
        self: &Arc<Self>,
        recovering: Recovering<D>,
        options: crate::Options,
        bucket: Arc<B>,
        holder: u64,
        started: futures::channel::oneshot::Sender<Result<Arc<LocalCell<D>>, Failure>>,
        initial_head: Option<head::Read>,
    ) {
        let interval = self.options.interval;
        self.holders.lock().unwrap().0 = holder;
        let writer = match self.start(recovering, options, &bucket, holder, initial_head).await {
            Ok(writer) => {
                // cladefs: urged by its disk as it comes under pressure (doc-22, TASK-539.9).
                if let Some(disk) = crate::dir::Dir::disk(self.cell().dir()) {
                    let urged: std::sync::Weak<dyn bucket_cell::host_disk::Urge> =
                        Arc::downgrade(self) as _;
                    disk.on_pressure(urged);
                }
                // cladefs: its opener gone, as an opening's future dropped, nothing waits for the
                // cell: it stops, as it would were it dropped (TASK-685.3).
                if started.send(Ok(self.cell().clone())).is_err() {
                    return;
                }
                writer
            }
            Err(err) => return drop(started.send(Err(err))),
        };
        // A version's cell has nothing to upload.
        let Some(writer) = writer else {
            while !self.progress().stop {
                self.wake.notified().await;
            }
            return;
        };
        // The segments uploaded, and as many as were when the last checkpoint was begun; and the
        // newest segment taken that freed extents, which a clean close checkpoints unless the
        // head's checkpoint covers it (hotbed question 535): by its number, not a flag a periodic
        // checkpoint clears, as one begun before that segment was acknowledged covers it not
        // (board 13938).
        let (mut segments, mut checkpointed) = (0u64, 0u64);
        let mut freed: Option<u64> = None;
        let mut in_flight = InFlight::default();
        // cladefs: head acknowledgements (`Writer::acknowledge`) and periodic checkpoint uploads
        // (`Writer::checkpoint`) run in separate slots beside segment uploads: checkpoint PUTs
        // hold no head lock while uploading and piggyback on the next acknowledgement swap
        // (`Writer::write_checkpoint`), so multi-second checkpoint uploads never stall head swaps
        // or WAL release (TASK-690).
        let mut ack_work: Option<BoxFuture<'static, Result<Option<u64>, bucket_cell::Error>>> =
            None;
        let mut checkpoint_work: Option<BoxFuture<'static, ()>> = None;
        let mut ack_wanted = false;
        let mut ack_wait: Option<BoxFuture<'static, ()>> = None;
        let mut ack_urgent = false;
        // Persistent open-segment close interval timer: survives upload/ack completions so steady
        // upload traffic never starves or resets the open-segment close cadence.
        let mut interval_wait: Option<BoxFuture<'static, ()>> = None;
        // cladefs: the relocation round under way, if one is, with what it was allowed; what the
        // next may copy; and the keys the states the cell keeps read (doc-27, board 6214).
        let mut relocating: Option<(u64, Round)> = None;
        let mut relocate_wanted = false;
        let mut credit = relocations::Credit::default();
        let pins = Arc::new(relocations::Pins::default());
        // Whether the next gather closes the cell's open segment: for a commit waiting for its
        // upload, or a fork, or as the interval is up, or the uploader stops; and whether the cell
        // may hold closed segments ready to upload without closing the open segment (TASK-92.15).
        // cladefs: and whether a commit waits for it, or a fork, or the uploader stops: closing
        // whatever it holds, not only once the guest is filling it no more (TASK-92.41).
        let (mut close, mut more, mut urgent) = (false, self.options.upload_closed, false);
        loop {
            // cladefs: a test's uploader panicking (board 8507).
            fail::fail_point!("upload::turn");
            if let Err(err) = self.observe() {
                return self.fail(&writer, err);
            }
            // cladefs: acknowledgements the cell's disk refused before, tried again.
            if !in_flight.acknowledging.is_empty()
                && let Err(err) = self.acknowledge_pending(&mut in_flight).await
            {
                return self.fail(&writer, err);
            }
            let (stop, cut, failed) = {
                let mut progress = self.progress();
                let wanted = std::mem::take(&mut progress.wanted);
                urgent |= wanted;
                ack_urgent |= wanted;
                close |= urgent;
                (progress.stop, progress.cut, progress.failed.is_some())
            };
            // cladefs: failed beside it, its keeper having panicked (board 8507): it stops, as cut
            // short, what it has in flight left to the cell's log.
            if failed {
                return;
            }
            // cladefs: stopping relocates nothing more, which keeps a drain short (doc-27): what a
            // round under way committed, the write-ahead log keeps.
            if stop {
                self.abort_round();
            }
            // Cut short (TASK-216): what it has in flight, and what it would have taken, the
            // cell's log keeps, for the cell's next opening.
            if stop && cut {
                return self.fail(&writer, Failure::msg("the uploader stopped, cut short"));
            }
            if (close || more || stop) && in_flight.len() < IN_FLIGHT {
                let closing = (close || stop, urgent || stop);
                let gathered = self.gather(&writer, &mut in_flight, closing).await;
                let gathered = match gathered {
                    Ok(gathered) => gathered,
                    Err(err) => return self.fail(&writer, err),
                };
                // Done with once closed; the segments the cell had closed taking the room, it
                // closes the next time.
                if gathered.closed {
                    interval_wait = None;
                }
                close &= !gathered.closed;
                urgent &= !gathered.closed;
                more = gathered.took;
                freed = freed.max(gathered.freed);
                credit.uploaded(gathered.data);
                if more {
                    continue;
                }
                if stop
                    && in_flight.is_empty()
                    && ack_work.is_none()
                    && checkpoint_work.is_none()
                    && !ack_wanted
                    && relocating.is_none()
                {
                    // A checkpoint on clean shutdown (doc-7, Checkpoints), skipped below
                    // `checkpoint_min_on_close` once the cell already has a checkpoint and the
                    // head's checkpoint covers every segment that freed extents and every named
                    // version (hotbed question 535, board 11995, board 13938, hotbed question
                    // 584). Checking each by its segment against `head.checkpoint` covers those
                    // in flight during a periodic checkpoint, or past a failed one; versions,
                    // named in the head, also those persisted across an unclean stop.
                    let uncheckpointed = segments.saturating_sub(checkpointed);
                    let head = writer.head().await;
                    let checkpoint_at = head.checkpoint.map(|(at, _)| at);
                    let uncovered = |segment: u64| checkpoint_at.is_none_or(|at| segment > at);
                    let no_checkpoint_yet = checkpoint_at.is_none();
                    let uncheckpointed_free = freed.is_some_and(uncovered);
                    let uncheckpointed_version =
                        head.versions.iter().any(|version| uncovered(version.at));
                    if (uncheckpointed >= self.options.checkpoint_min_on_close
                        || (uncheckpointed > 0 && no_checkpoint_yet)
                        || uncheckpointed_free
                        || uncheckpointed_version)
                        && let Err(err) = writer.checkpoint().await
                    {
                        log::warn!("cladefs: checkpointing cell {}: {err}", writer.cell());
                    }
                    return self.fail(&writer, Failure::msg("the uploader stopped"));
                }
            }
            let ack_due = ack_urgent
                || urgent
                || stop
                || self.options.ack_coalesce.is_zero()
                || ack_wait.is_none();
            if ack_wanted && ack_work.is_none() && ack_due {
                ack_wanted = false;
                ack_wait = None;
                if in_flight.is_empty() {
                    ack_urgent = false;
                }
                let writer = writer.clone();
                ack_work = Some(async move { writer.acknowledge().await }.boxed());
            }
            // cladefs: a relocation round after a checkpoint, once any earlier round is done, the
            // uploader isn't stopping and the cell's disk has room (doc-27, board 10918).
            if relocate_wanted && self.options.relocate && relocating.is_none() && !stop {
                relocate_wanted = false;
                let pressed = crate::dir::Dir::disk(self.cell().dir())
                    .is_some_and(|disk| disk.under_pressure());
                if !pressed {
                    let allowance = credit.take();
                    let round =
                        self.relocating(writer.clone(), (bucket.clone(), pins.clone()), allowance);
                    relocating = Some((allowance, round));
                }
            }
            if interval_wait.is_none() && !close && !stop {
                interval_wait = Some(self.interval_sleep(interval));
            }
            let watch_closed = self.options.upload_closed && !more;
            let mut wake = std::pin::pin!(self.wake.notified());
            let mut segment_ready = std::pin::pin!(self.segment_ready.notified());
            let happened = futures::future::poll_fn(|cx| {
                if let Some(work) = ack_work.as_mut()
                    && let Poll::Ready(done) = work.poll_unpin(cx)
                {
                    return Poll::Ready(Happened::Acknowledged(done));
                }
                if let Some(work) = checkpoint_work.as_mut()
                    && let Poll::Ready(()) = work.poll_unpin(cx)
                {
                    return Poll::Ready(Happened::Checkpointed);
                }
                if let Poll::Ready(Some(uploaded)) = in_flight.poll_next(cx) {
                    return Poll::Ready(Happened::Uploaded(uploaded));
                }
                if let Some(wait) = ack_wait.as_mut()
                    && let Poll::Ready(()) = wait.poll_unpin(cx)
                {
                    return Poll::Ready(Happened::AckDue);
                }
                if let Some((allowance, round)) = relocating.as_mut()
                    && let Poll::Ready(ended) = round.poll_unpin(cx)
                {
                    let ended = ended.unwrap_or_else(|_| Err(anyhow::anyhow!("its task dropped")));
                    return Poll::Ready(Happened::Relocated(*allowance, ended));
                }
                if watch_closed && let Poll::Ready(()) = segment_ready.as_mut().poll(cx) {
                    return Poll::Ready(Happened::SegmentReady);
                }
                if let Poll::Ready(()) = wake.as_mut().poll(cx) {
                    return Poll::Ready(Happened::Woken);
                }
                if let Some(wait) = interval_wait.as_mut()
                    && let Poll::Ready(()) = wait.poll_unpin(cx)
                {
                    return Poll::Ready(Happened::Interval);
                }
                Poll::Pending
            })
            .await;
            let done = match happened {
                Happened::Woken => Ok(()),
                Happened::SegmentReady => {
                    more = true;
                    Ok(())
                }
                Happened::Interval => {
                    interval_wait = None;
                    close = true;
                    Ok(())
                }
                Happened::AckDue => {
                    ack_wait = None;
                    Ok(())
                }
                Happened::Uploaded(uploaded) => uploaded.map_err(Failure::from).map(|()| {
                    segments += 1;
                    if in_flight.ready_to_ack() {
                        ack_wanted = true;
                        if ack_wait.is_none()
                            && !self.options.ack_coalesce.is_zero()
                            && !ack_urgent
                            && !urgent
                            && !stop
                        {
                            ack_wait = Some((self.clock)(self.options.ack_coalesce));
                        }
                    }
                }),
                Happened::Acknowledged(done) => {
                    ack_work = None;
                    match done {
                        Ok(through) => {
                            let acked = self.acknowledged(through, &mut in_flight).await;
                            // A checkpoint every so many segments, after the commits it covers
                            // returned, running concurrently with subsequent acknowledgements.
                            // cladefs: BUGGIFY (doc-24, board 14567): one after any segment, as a
                            // knob drawn low would have it, so that what versions alone keep, their
                            // segments before the cell's checkpoint, is the collector's once they
                            // are dropped (`forks_check_versions`).
                            let due = match bucket_cell::buggify!(0.3) {
                                true => segments > checkpointed,
                                false => segments - checkpointed >= self.options.checkpoint_every,
                            };
                            if due && checkpoint_work.is_none() && !stop {
                                checkpointed = segments;
                                checkpoint_work = Some(Self::checkpointing(writer.clone()));
                            }
                            if in_flight.ready_to_ack() {
                                ack_wanted = true;
                                if ack_wait.is_none()
                                    && !self.options.ack_coalesce.is_zero()
                                    && !ack_urgent
                                    && !urgent
                                    && !stop
                                {
                                    ack_wait = Some((self.clock)(self.options.ack_coalesce));
                                }
                            }
                            acked
                        }
                        Err(err) => Err(err.into()),
                    }
                }
                Happened::Checkpointed => {
                    checkpoint_work = None;
                    // cladefs: queue a relocation round for after this checkpoint (started at the
                    // top of the loop once any earlier round has ended, doc-27, board 10918).
                    relocate_wanted = true;
                    Ok(())
                }
                Happened::Relocated(allowance, ended) => {
                    relocating = None;
                    self.relocation.lock().unwrap().take();
                    if let Err(err) = &ended {
                        log::warn!("cladefs: relocating cell {}: {err:#}", writer.cell());
                    }
                    // What it committed counts, failing after or not.
                    credit.spent(allowance, relocations::relocated(&ended).bytes);
                    Ok(())
                }
            };
            if let Err(err) = done {
                return self.fail(&writer, err);
            }
        }
    }

    /// cladefs: writes a checkpoint as of what `writer` acknowledged, concurrently with segment
    /// uploads and head acknowledgements: a checkpoint failing is logged, and fails nothing.
    fn checkpointing(writer: Arc<Writer<B>>) -> BoxFuture<'static, ()> {
        async move {
            // A test's: the checkpoint landing late, after the commits it covers returned, as on
            // a loaded host, which a collector's pass then runs beside (TASK-339).
            fail::fail_point!("upload::checkpointing");
            if let Err(err) = writer.checkpoint().await {
                log::warn!("cladefs: checkpointing cell {}: {err}", writer.cell());
            }
        }
        .boxed()
    }

    /// cladefs: a relocation round over its cell, `writer`'s, of `allowance` bytes at most
    /// ([`relocations::round`]), within the host's uploads' budget: how it ended. A task of its
    /// own, as its spawn runs its keeper, not polled by the run: it waits for the cell's commits,
    /// which may wait for what the run does meanwhile, a commit's upload for its gather (Codex's
    /// review of d33a6c360). A failure fails nothing but the round.
    fn relocating(
        &self,
        writer: Arc<Writer<B>>,
        (bucket, pins): (Arc<B>, Arc<relocations::Pins>),
        allowance: u64,
    ) -> Round {
        let (cell, uploads) = (self.cell().clone(), self.uploads.clone());
        let (fetching, cache) = (self.options.fetching.clone(), self.options.cache.clone());
        let (ended, round) = futures::channel::oneshot::channel();
        let (abort, registration) = futures::future::AbortHandle::new_pair();
        let (done, gone) = futures::channel::oneshot::channel::<()>();
        *self.relocation.lock().unwrap() = Some((abort, gone));
        let ending = async move {
            fail::fail_point!("upload::relocating", |_| Err(anyhow::anyhow!("failpoint")));
            let budgets = relocations::Budgets { uploads: &uploads, fetching, cache };
            relocations::round(&cell, &writer, (&bucket, budgets), &pins, allowance).await
        };
        let ending = futures::future::Abortable::new(ending, registration);
        (self.spawn)(
            async move {
                let stopped = |_| Err(anyhow::anyhow!("the uploader stopped"));
                // Aborted is the uploader stopping, sent as the round's failure; the send fails
                // only once no one waits for the round.
                drop(ended.send(ending.await.unwrap_or_else(stopped)));
                // Said once the round, and the cell it held, went.
                drop(done);
            }
            .boxed(),
        );
        round
    }

    /// cladefs: aborts the relocation round under way, if one is.
    fn abort_round(&self) {
        if let Some((abort, _)) = &*self.relocation.lock().unwrap() {
            abort.abort();
        }
    }

    /// Stops uploading, for `err`: commits waiting for uploads, and forks, fail with it.
    fn fail(&self, writer: &Writer<B>, err: Failure) {
        if let Some(sink) = self.fast_wal_sink.get() {
            sink.stop();
        }
        let mut progress = self.progress();
        if !progress.stop {
            log::error!("cladefs: uploading cell {}: {err}", writer.cell());
        }
        // What its cell needs gone or corrupt: no fault but a rule of the protocol broken.
        if let Failure::Bucket(bucket_cell::Error::Lost(_) | bucket_cell::Error::Corrupt(_)) = &err
        {
            bucket_cell::buggify::violated(|| format!("uploading cell {}: {err}", writer.cell()));
        }
        progress.failed = Some(err);
        self.changed.notify(usize::MAX);
    }

    /// Opens the cell `recovering` recovered, as the bucket holds its cell (see the module's
    /// docs), and returns its writer, none for a version. Its log is checked to follow the cell as
    /// recovery finds it before the head is taken, so that a cell plainly stale fences no writer;
    /// then, the head taken, the writer recovers it on from there (TASK-92.6) and the cell opens
    /// as the writer found its end, which another writer may have moved on meanwhile.
    async fn start(
        self: &Arc<Self>,
        recovering: Recovering<D>,
        mut options: crate::Options,
        bucket: &Arc<B>,
        holder: u64,
        initial_head: Option<head::Read>,
    ) -> Result<Option<Arc<Writer<B>>>, Failure> {
        let cell = recovering.cell();
        let (cache, parts) = (self.options.cache.clone(), self.options.held.clone());
        let placement = recovering.token().unwrap_or(self.options.token);
        let kms = &self.keys.kms;
        let read = first_read(&**bucket, cell, initial_head).await?;
        // A cell with no head is none: its maker makes it before its cell first opens.
        let Some(found) = read.head()? else {
            return Err(match recovering.version() {
                Some(_) => Failure::VersionDropped,
                None => bucket_cell::Error::NoCell.into(),
            });
        };
        if let Some(id) = recovering.version() {
            let (Recovered { state, layer, .. }, keys) =
                self.version(bucket, cell, found, id).await?;
            let opening =
                Opening { state, layer, taken: None, cache, keys: keys.clone(), held: parts };
            let open_cell = blocking(|| recovering.open(options, opening))?;
            assert!(self.cell.set(Arc::new(open_cell)).is_ok(), "an uploader opens its cell once");
            self.hold_ring(&keys);
            let mut progress = self.progress();
            progress.uploaded_through(u64::MAX);
            return Ok(None);
        }
        // cladefs: a cell deleted takes no writer.
        if found.deleted() {
            return Err(bucket_cell::Error::Deleted.into());
        }
        // cladefs: fail hard before taking the head if this cell will run in Fast WAL mode or has
        // an active Fast WAL to fence and recover, and this host cannot satisfy it (doc-29):
        // - no configured Fast WAL bucket,
        // - persisted cross-AZ active bucket URL not in the host's allowlist, or
        // - enabling Fast WAL on a cell with unuploaded local WAL commits (must sync first so no
        //   earlier local-only commit can be orphaned behind a newly activated Fast WAL).
        let want_fast_wal_pre = match self.options.fast_wal {
            Some(Some(_)) => true,
            Some(None) => false,
            None => found.fast_wal.is_some(),
        };
        let old_active_pre = found.fast_wal.as_ref().and_then(|fw| fw.active.as_ref());
        if want_fast_wal_pre || old_active_pre.is_some() {
            let cfg = self.options.fast_wal_config.as_ref().ok_or_else(|| {
                Failure::msg(
                    "cell requires fast WAL, but CLADEFSD_FAST_WAL_BUCKET is not configured",
                )
            })?;
            if let Some(active) = old_active_pre
                && cfg.bucket_url != active.bucket
            {
                cfg.opener.is_allowed(&active.bucket).map_err(Failure::msg)?;
            }
        }
        if want_fast_wal_pre && old_active_pre.is_none() && recovering.has_wal() {
            return Err(Failure::msg(
                "cell has unuploaded local commits; sync to the cold bucket before enabling fast \
                 WAL",
            ));
        }
        // cladefs: recovered before the taking under its keys, as the taking will hold them,
        // unwrapped once for it (TASK-411.3); a head with none yet, a fork's child's or a new
        // cell's, which its taking gives them, recovered once it is taken.
        let (before, _pre_ring) = match found.deks.is_empty() {
            false => {
                let keys = CellKeys::to_take(cell, &found, kms.clone(), holder);
                let ring = keys::ring(&**bucket, &keys, cell, &found, true).await?;
                let recovered = bucket_cell::recovery::recover_under_from(
                    &**bucket,
                    cell,
                    found.origin(),
                    Some(found.clone()),
                    &ring,
                )
                .await?;
                // Where the writer's chain would go: the log's restamping, not looked at here.
                // Cells with an active fast WAL treat local disk WAL as cache-only and drop it
                // after fencing & recovering the fast WAL.
                if old_active_pre.is_none() {
                    let chain = Chain::new(cell, 0, recovered.end(), 0);
                    let head = recovered.head.clone().unwrap_or_default();
                    if !recovering.follows(&TakenCell::of(head, &recovered.state, chain)) {
                        return Err(bucket_cell::Error::Diverged.into());
                    }
                }
                (Some(recovered), Some(ring))
            }
            true => (None, None),
        };
        // cladefs: its head taken by this placement, in one conditional write that gives the cell
        // what it lacks (TASK-515): a new cell's KEK from the host's pool, wrapped by its tenant's
        // ahead (every cell is encrypted, TASK-411.28), its DEK, and the kind of the cell, unless
        // it is of another (`OtherKind`, doc-20). Not a version's, which no writer
        // takes, and any number of hosts open at once. A cell with no head is no cell: its maker
        // makes it, before its cell opens (`lifecycle::create`), and no taking does.
        let fast_wal_allowed: Option<Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>> = {
            let cfg = self.options.fast_wal_config.clone();
            Some(Arc::new(move |url: &str| match &cfg {
                Some(cfg) if cfg.bucket_url == url => Ok(()),
                Some(cfg) => cfg.opener.is_allowed(url),
                None => Err(
                    "cell has an active fast WAL, but CLADEFSD_FAST_WAL_BUCKET is not configured"
                        .into(),
                ),
            }))
        };
        let taking = head::Taking {
            holder,
            token: placement,
            kind: recovering.kind(),
            keys: None,
            fast_wal: self.options.fast_wal,
            fast_wal_allowed,
        };
        let (held, cell_keys) = keys::taking(&**bucket, kms, cell, read, taking).await?;
        // cladefs: the host serves a cell of the tenant's: its pool of new cell KEKs, which forks
        // of its cells into new cells take, filled off this path (TASK-411.4).
        kms.refill(cell);
        let keys = keys::ring(&**bucket, &cell_keys, cell, &held.head, true).await?;
        let writer = Writer::holding(bucket.clone(), cell, held, before, keys).await?;
        let writer = Arc::new(writer.uploading_within(self.uploads.clone()));
        let keys = writer.keys().clone();
        let mut recovering = recovering;

        // cladefs: Fast WAL takeover fencing, recovery, and activation (doc-29).
        let taken_head = writer.head().await;
        let old_active = taken_head.fast_wal.as_ref().and_then(|fw| fw.active.clone());
        let want_fast_wal = match self.options.fast_wal {
            Some(Some(fsync)) => Some(fsync),
            Some(None) => None,
            None => taken_head.fast_wal.as_ref().map(|fw| fw.fsync),
        };
        let target_fast_wal = match want_fast_wal {
            Some(fsync) => {
                let cfg = self.options.fast_wal_config.as_ref().ok_or_else(|| {
                    Failure::msg(
                        "cell requires fast WAL, but CLADEFSD_FAST_WAL_BUCKET is not configured",
                    )
                })?;
                Some(bucket_cell::layout::FastWal {
                    fsync,
                    active: Some(bucket_cell::layout::ActiveWal {
                        epoch: writer.epoch(),
                        bucket: cfg.bucket_url.clone(),
                    }),
                })
            }
            None => None,
        };
        if let Some(active) = old_active.as_ref() {
            let cfg = self.options.fast_wal_config.as_ref().ok_or_else(|| {
                Failure::msg(
                    "cell has an active fast WAL, but CLADEFSD_FAST_WAL_BUCKET is not configured",
                )
            })?;
            let old_bucket = if cfg.bucket_url == active.bucket {
                cfg.bucket.clone()
            } else {
                (cfg.opener.open(&active.bucket)).await.map_err(Failure::msg)?
            };
            crate::fast_wal::fence_and_recover(
                &writer,
                bucket,
                &old_bucket,
                active,
                target_fast_wal.clone(),
            )
            .await?;
            let sweep_bucket = old_bucket.clone();
            let sweep_keys = writer.keys().clone();
            let head_epoch = writer.epoch();
            let active_wal_epoch =
                target_fast_wal.as_ref().and_then(|fw| fw.active.as_ref()).map(|a| a.epoch);
            (self.spawn)(
                async move {
                    if let Err(err) = crate::fast_wal::sweep_superseded(
                        &sweep_bucket,
                        cell,
                        head_epoch,
                        active_wal_epoch,
                        &sweep_keys,
                    )
                    .await
                    {
                        log::warn!("cladefs: sweeping superseded fast WAL for {cell}: {err}");
                    }
                }
                .boxed(),
            );
        } else if taken_head.fast_wal != target_fast_wal {
            if target_fast_wal.is_some() && recovering.has_wal() {
                return Err(Failure::msg(
                    "cell has unuploaded local commits; sync to the cold bucket before enabling \
                     fast WAL",
                ));
            }
            writer.activate_fast_wal(target_fast_wal.clone()).await?;
        }
        // Only drop local WAL when the cell already had an active fast WAL (local disk was
        // cache-only); never drop local or peer-recovered WAL when newly enabling Fast WAL.
        if old_active.is_some() {
            blocking(|| recovering.drop_wal())?;
        }
        if target_fast_wal.is_some() {
            options.large_extent = options.large_extent.min(self.options.fast_wal_large_extent);
        }

        let head_now = writer.head().await;
        let state = writer.state();
        let layer = writer.layer();
        let taken = Some(TakenCell::of(head_now.clone(), &state, writer.chain()));
        let opening = Opening { state, layer, taken, cache, keys: keys.clone(), held: parts };
        let open_cell = blocking(|| recovering.open(options, opening))?;
        assert!(self.cell.set(Arc::new(open_cell)).is_ok(), "an uploader opens its cell once");
        if self.options.upload_closed {
            self.cell().wake_on_segment_written(self.segment_ready.clone());
        }
        self.hold_ring(&keys);
        self.token.set(placement).expect("an uploader's writers take one placement's token");
        self.took(&writer, placement, &head_now);
        if target_fast_wal.is_some() {
            let cfg =
                self.options.fast_wal_config.as_ref().expect("fast WAL config checked before take");
            let initial_epoch = writer.state().last_commit.map_or(0, epoch_of);
            let shared_weak = Arc::downgrade(self);
            let urge_cold: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                if let Some(shared) = shared_weak.upgrade() {
                    shared.progress().wanted = true;
                    shared.wake.notify_one();
                }
            });
            let hot_budget = self.host.as_ref().map(|h| Arc::clone(&h.hot_budget));
            let sink = crate::fast_wal::FastWalSink::new(
                cfg.bucket_url.clone(),
                initial_epoch,
                self.changed.clone(),
                urge_cold,
                hot_budget,
            );
            self.cell().fast_wal_to(sink.clone());
            assert!(
                self.fast_wal_sink.set(sink.clone()).is_ok(),
                "an uploader opens its cell once"
            );
            let fw_writer = crate::fast_wal::FastWalWriter::new(
                writer.clone(),
                cfg.bucket.clone(),
                sink,
                cfg.max_writer_appends,
                cfg.max_segment_bytes,
                self.clock.clone(),
            );
            let (done_tx, done_rx) = futures::channel::oneshot::channel();
            *self.fast_wal_done.lock().unwrap() = Some(done_rx);
            (self.spawn)(
                async move {
                    fw_writer.run().await;
                    drop(done_tx);
                }
                .boxed(),
            );
        }
        Ok(Some(writer))
    }

    /// `writer` took the cell, with the fencing token `token`: the cell's commits are uploaded,
    /// its forks made, and what the cell's write-ahead log holds after them is the uploader's to
    /// upload.
    fn took(&self, writer: &Arc<Writer<B>>, token: u64, head: &Head) {
        *self.writer.lock().unwrap() = Some(writer.clone());
        let fast_wal = head.fast_wal.as_ref().map(|fw| fw.fsync);
        let taking = Taking { token, epoch: writer.epoch(), taken: SystemTime::now(), fast_wal };
        let mut progress = self.progress();
        (progress.next_sequence, progress.taking) = (writer.chain().next_sequence(), Some(taking));
        if let Some(last) = writer.state().last_commit {
            progress.uploaded_through(epoch_of(last));
        }
        drop(progress);
        self.changed.notify(usize::MAX);
    }

    /// cladefs: opens `cell`, whose head is `head`, as a copy of its version `id`,
    /// which the head must name (TASK-476): its state at the version, which never changes, as
    /// recovered, with its keys, under its KEK as read, taking it not. No taking: nothing the cell
    /// does goes to the bucket but its forks' heads. Fails if the version is named no more
    /// ([`Failure::VersionDropped`]).
    async fn version(
        &self,
        bucket: &Arc<B>,
        cell: CellId,
        head: Head,
        id: u64,
    ) -> Result<(Recovered, Arc<Ring>), Failure> {
        let Some(version) = head.version(id).copied() else {
            return Err(Failure::VersionDropped);
        };
        let origin = Origin { parent: cell, at: version.at, epoch: version.epoch };
        let cell_keys = CellKeys::of(cell, &head, self.keys.kms.clone());
        let keys = keys::ring(&**bucket, &cell_keys, cell, &head, true).await?;
        let recovering = bucket_cell::recovery::recover_version(&**bucket, cell, version.at, &keys);
        let recovered = recovering.await?;
        // cladefs: its cell's open segment, which nothing writes, encrypted as every cell's is
        // (TASK-411.28); its forks' checkpoints sealed under its cell's.
        let sealing = keys.clone();
        let keys = keys.for_version(cell);
        let state = (recovered.layer.clone()).unwrap_or_else(|| Arc::new(recovered.state.clone()));
        let at_version = AtVersion { origin, version: id, sealing, state };
        assert!(self.at_version.set(at_version).is_ok(), "an uploader opens one version");
        Ok((recovered, keys))
    }

    /// Adds the segments of the cell's write-ahead log whose commits are durable to the writer, to
    /// be uploaded as they are, after those uploading (`in_flight`), as many as may upload at once
    /// with them: the rest stay in the cell, unread, until those are uploaded. If `close`, the
    /// cell closes its open segment first if fewer wait: for a commit waiting for its upload, a
    /// fork, the interval up, or the uploader stopping; for no other, as the commits no one waits
    /// for wait in it for those, rather than each go in a segment of its own (TASK-92.15).
    /// cladefs: if not `urgent`, the interval's, the open segment the guest is filling stays open
    /// for its next commit, up to `Options::filling_wait` (TASK-92.41).
    async fn gather(
        &self,
        writer: &Arc<Writer<B>>,
        in_flight: &mut InFlight,
        (close, urgent): (bool, bool),
    ) -> Result<Gathered, Failure> {
        failing!("upload::gather");
        let cell = self.cell();
        let room = IN_FLIGHT.saturating_sub(in_flight.len());
        // What a waiter wants is durable; then closed, if fewer segments wait than may upload, and
        // what the closing closed durable too. Skip when nothing is pending so the uploader's
        // startup probe (`upload_closed = true`) does not immediately settle a freshly restored
        // fork's deferred durability (board 7420).
        if cell.flush_pending() {
            cell.flush().await?;
        }
        let mut closed = if close || !self.options.upload_closed {
            cell.closed(room).await?
        } else {
            cell.closed_ready(room).await?
        };
        let mut closing = close && closed.len() < room;
        if closing {
            let waited = {
                let progress = self.progress();
                // A clock gone back: nothing waited yet.
                let oldest = progress.seen.front().and_then(|(_, seen)| seen.elapsed().ok());
                oldest.is_some_and(|waited| waited >= self.options.filling_wait)
            };
            // cladefs: until its framed file is written, not durable, which its upload waits for no
            // more (TASK-384). Asked again at the next interval, if the guest still hasn't
            // committed.
            match urgent || waited {
                true => drop(cell.close_for_upload(false).await?),
                false => closing = cell.close_for_upload(true).await?,
            }
            cell.flush().await?;
            closed.extend(cell.closed(room - closed.len()).await?);
        }
        let mut segments = Vec::with_capacity(closed.len());
        let data = closed.iter().map(|closed| closed.data_len).sum();
        let mut freed = None;
        for closed in closed {
            let (number, tag) = (closed.number, closed.tag());
            let continues = closed.footer.contents.continued.is_some();
            let frees = !closed.footer.contents.frees.is_empty();
            // cladefs: what reads its files, on a blocking thread, as `blocking` gives (board
            // 2944).
            let (own, size) = blocking(|| -> Result<_, Failure> {
                let own = closed
                    .own
                    .iter()
                    .map(|own| {
                        let reader = own.clone();
                        let body = Body::new(own.size, move || blocking(|| reader.bytes()));
                        Ok((own.key.clone(), body))
                    })
                    .collect::<Result<_, Failure>>()?;
                let size = closed.file.size().map_err(|err| Failure::local(err, "a segment"))?;
                Ok((own, size))
            })?;
            let (footer, data_len) = (closed.footer.clone(), closed.data_len);
            let closed = Arc::new(closed);
            let body = Body::new(size, move || blocking(|| closed.bytes()));
            let segment = writer.add_segment(Objects { footer, segment: body, own, data_len })?;
            if frees {
                freed = Some(segment);
            }
            segments.push(Uploading { segment, number, tag, continues });
        }
        let took = !segments.is_empty();
        in_flight.upload(writer, segments);
        Ok(Gathered { took, data, closed: closing, freed })
    }

    /// Acknowledges what the writer has uploaded: the commits of the segments it acknowledges are
    /// uploaded, and the cell's segments leave it, read in the bucket, into the host's cache if
    /// it has one (TASK-165.8); then the forks they name are made, with a checkpoint at them, which
    /// their children's recovery starts from.
    async fn acknowledged(
        &self,
        through: Option<u64>,
        in_flight: &mut InFlight,
    ) -> Result<(), Failure> {
        failing!("upload::acknowledged");
        let acked = in_flight.acknowledged(through);
        // Notify Fast WAL immediately once the cold head CAS succeeds, without blocking degraded
        // fallback or oversized-batch spill waiters on local WAL start-file sync.
        if let Some(last_epoch) = acked.last().map(|segment| segment.epoch())
            && let Some(sink) = self.fast_wal_sink.get()
        {
            sink.head_acknowledged(last_epoch);
        }
        in_flight.acknowledging.extend(acked);
        self.acknowledge_pending(in_flight).await
    }

    /// cladefs: has the cell record the segments the writer acknowledged, in order, which leave it
    /// then, and counts their commits uploaded: as many as its disk lets it, the rest kept for the
    /// next pass, as a full disk fails the record of an acknowledgement, which leaves the one
    /// before it whole (`wal::StartFile`), and the cell goes on; any other failure is the
    /// uploader's (TASK-539.12). Before, the cell failed for it. A disk failing otherwise (EIO) is
    /// no full one: tried again for ever, it left every wait on an upload hanging (board 5359).
    async fn acknowledge_pending(&self, in_flight: &mut InFlight) -> Result<(), Failure> {
        if in_flight.acknowledging.is_empty() {
            return Ok(());
        }
        let numbers: Vec<u64> =
            in_flight.acknowledging.iter().map(|segment| segment.number).collect();
        let epoch = match self.cell().acknowledged_batch(&numbers).await {
            Ok(()) => {
                in_flight.acknowledging_failed = false;
                let epoch = in_flight.acknowledging.back().map(Uploading::epoch);
                in_flight.acknowledging.clear();
                epoch
            }
            Err(err) if full(&err) => {
                if !std::mem::replace(&mut in_flight.acknowledging_failed, true) {
                    log::warn!(
                        "cladefs: cell {} records no acknowledgement for now, tried again \
                         each pass: {err:#}",
                        self.cell().description().cell
                    );
                }
                None
            }
            Err(err) => return Err(err.into()),
        };
        let Some(epoch) = epoch else { return Ok(()) };
        failing!("upload::released");
        if let Some(sink) = self.fast_wal_sink.get() {
            sink.head_acknowledged(epoch);
        }
        self.progress().uploaded_through(epoch);
        self.changed.notify(usize::MAX);
        Ok(())
    }
}

/// What an opening of the cell of `cell` reads first (TASK-165.11, board 6020): its head, which
/// the taking goes on from and its recovery starts from. A fork of a version whose restore never
/// finished (still `Forking`, not deleted) is refused as no cell ([`bucket_cell::Error::NoCell`]):
/// its caller retries with a fresh child ID instead of resuming in place.
async fn first_read<B: bucket_cell::Bucket>(
    bucket: &B,
    cell: CellId,
    initial_head: Option<head::Read>,
) -> Result<head::Read, Failure> {
    let read = match initial_head {
        Some(read) => read,
        None => head::read(bucket, cell).await?,
    };
    if read.head()?.is_some_and(|head| !head.deleted() && head.forking().is_some()) {
        return Err(bucket_cell::Error::NoCell.into());
    }
    Ok(read)
}

/// What a gather did ([`Shared::gather`]).
struct Gathered {
    /// Whether it took segments the cell closed: the cell may have closed more meanwhile.
    took: bool,
    /// cladefs: the bytes of data of the segments it took ([`relocations::Credit`]).
    data: u64,
    /// Whether it closed the cell's open segment, as asked: what was committed before is in the
    /// segments the cell closed, taken, or to be taken next.
    closed: bool,
    /// cladefs: the newest segment taken that freed extents, by its number in the cell, which
    /// `cladefs-gc` reclaims once a checkpoint covers it (hotbed question 535, board 13938).
    freed: Option<u64>,
}

/// A segment uploading: its number in the cell, and in the open cell; the tag of the open cell's
/// commit it ends with; and whether a commit continues past it into the next segment.
struct Uploading {
    segment: u64,
    number: u64,
    tag: u64,
    continues: bool,
}

impl Uploading {
    /// The cell's epoch of the commit it ends with.
    fn epoch(&self) -> u64 {
        epoch_of(self.tag)
    }
}

/// cladefs: a relocation round under way, its task's, as it ends ([`Shared::relocating`]).
type Round = futures::channel::oneshot::Receiver<anyhow::Result<crate::Relocated>>;

/// cladefs: what the uploader's run waits on that happened first.
enum Happened {
    /// An in-flight upload finished (in any order, avoiding head-of-line blocking).
    Uploaded(Result<(), bucket_cell::Error>),
    /// cladefs: the relocation round under way, given what it was allowed, is done: how it ended.
    Relocated(u64, anyhow::Result<crate::Relocated>),
    /// The head acknowledgement under way finished, through the segment it returns.
    Acknowledged(Result<Option<u64>, bucket_cell::Error>),
    /// The background checkpoint upload finished.
    Checkpointed,
    /// The head-swap coalescing window elapsed (hotbed question 535).
    AckDue,
    /// A closed segment finished framing and is ready to upload without closing the open segment.
    SegmentReady,
    /// The uploader was explicitly woken (`ask`, `stop`, `urge`, etc.).
    Woken,
    /// The open-segment close interval elapsed.
    Interval,
}

/// The segments uploading, in order, and the unordered futures of their uploads: completions
/// refill slots immediately without head-of-line blocking, while acknowledgements still advance
/// strictly through the longest contiguous uploaded prefix ending on a complete commit.
#[derive(Default)]
struct InFlight {
    segments: VecDeque<Uploading>,
    uploads: FuturesUnordered<BoxFuture<'static, Result<u64, bucket_cell::Error>>>,
    /// Segment numbers (in `Writer`'s numbering) whose PUTs finished and await head swap.
    uploaded: BTreeSet<u64>,
    /// cladefs: the segments the writer acknowledged that the cell hasn't yet, its last try at
    /// recording it failing on its disk, as a full one fails it: tried again at each pass
    /// ([`Shared::acknowledge_pending`], TASK-539.12).
    acknowledging: VecDeque<Uploading>,
    /// cladefs: whether the cell's last try failed so: said once until one succeeds.
    acknowledging_failed: bool,
}

impl InFlight {
    /// Uploads `segments`, which `writer` took, after those uploading.
    fn upload<B: bucket_cell::Bucket>(
        &mut self,
        writer: &Arc<Writer<B>>,
        segments: Vec<Uploading>,
    ) {
        for uploading in segments {
            let (writer, segment) = (writer.clone(), uploading.segment);
            self.uploads
                .push(async move { writer.upload(segment).await.map(|()| segment) }.boxed());
            self.segments.push_back(uploading);
        }
    }

    /// Polls for any finished upload, recording its segment number on completion.
    fn poll_next(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Result<(), bucket_cell::Error>>> {
        match self.uploads.poll_next_unpin(cx) {
            Poll::Ready(Some(Ok(segment))) => {
                self.uploaded.insert(segment);
                Poll::Ready(Some(Ok(())))
            }
            Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(err))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    /// Awaits the next finished upload (used by tests that drive `InFlight` directly).
    #[cfg(test)]
    async fn next(&mut self) -> Option<Result<(), bucket_cell::Error>> {
        futures::future::poll_fn(|cx| self.poll_next(cx)).await
    }

    /// Whether a contiguous prefix of `segments` has finished uploading and ends on at least one
    /// complete (non-continued) commit, matching `Writer::naming`.
    fn ready_to_ack(&self) -> bool {
        self.segments
            .iter()
            .take_while(|segment| self.uploaded.contains(&segment.segment))
            .any(|segment| !segment.continues)
    }

    /// The segments the writer acknowledged, `through` the last, which upload no more.
    fn acknowledged(&mut self, through: Option<u64>) -> Vec<Uploading> {
        let taken =
            |uploading: &&Uploading| through.is_some_and(|through| uploading.segment <= through);
        let count = self.segments.iter().take_while(taken).count();
        let acked: Vec<Uploading> = self.segments.drain(..count).collect();
        for segment in &acked {
            self.uploaded.remove(&segment.segment);
        }
        acked
    }

    /// How many uploads are in flight.
    fn len(&self) -> usize {
        self.uploads.len()
    }

    fn is_empty(&self) -> bool {
        self.uploads.is_empty()
    }
}

/// cladefs: whether `err` is a full disk's: ENOSPC, or a quota's EDQUOT, which room made later
/// lets pass (board 5359).
fn full(err: &anyhow::Error) -> bool {
    use std::io::ErrorKind::{QuotaExceeded, StorageFull};
    let mut io = err.chain().filter_map(|cause| cause.downcast_ref::<std::io::Error>());
    io.any(|io| matches!(io.kind(), StorageFull | QuotaExceeded))
}
