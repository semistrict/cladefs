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

use crate::{Dir, DirHold, File, HeldParts, LocalCell, Opening, Recovering, read_object};
use bucket_cell::cache::Cache;
use bucket_cell::fetch::Budget;
use bucket_cell::keys::{self, CellKeys, Ring, RingHold};
use bucket_cell::known::Learned;
use bucket_cell::layout::{CellId, Chain, Child, Head, Origin, epoch_of};
use bucket_cell::mirror::Mirrored;
use bucket_cell::object_bucket::ObjectBucket;
use bucket_cell::wal::TakenCell;
use bucket_cell::{Body, Objects, Recovered, Writer, head, lifecycle};
use event_listener::Event;
use futures::StreamExt as _;
use futures::future::{BoxFuture, Either, FutureExt as _, TryFutureExt as _};
use futures::stream::FuturesOrdered;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::{Duration, SystemTime};

/// How many of a cell's segments upload at once: as many of a segment's size as the host's
/// uploads may hold at once ([`UPLOAD_BYTES`]), so that a cell catching up uploads as many at once
/// as the host's budget lets it, about ten keeping 1 GB/s going with PUTs of 0.35 s, and holds no
/// more of its segments' files open for them (TASK-79.19).
pub const IN_FLIGHT: usize = (UPLOAD_BYTES / SEGMENT) as usize;

/// About the most bytes of a segment: a cell's segments' size
/// (`local_cell::Options::segment_size`).
const SEGMENT: u64 = 32 << 20;

/// How many segments go between checkpoints, by default ([`Options::checkpoint_every`]).
pub const CHECKPOINT_EVERY: u64 = 64;

/// cladefs: the mirror in `bucket` of the version `origin` names, begun in the background on the
/// bucket runtime, its fork to `child` made (doc-23: the first fork of a version into a bucket
/// starts its mirror there; TASK-633.8): nothing unless `child` is of `bucket` and the version of
/// another's cell, nor for one mirrored there already ([`ObjectBucket::mirror_wanted`]), which
/// takes its turn before this returns. What it came to is logged.
fn mirror_wanted(bucket: &Arc<ObjectBucket>, child: CellId, origin: Origin) -> Result<(), Failure> {
    let Origin { parent, at } = origin;
    let mirroring = bucket.mirror_wanted(child, origin);
    bucket_runtime()?.spawn(async move { logged(mirroring.await, parent, at) });
    Ok(())
}

/// cladefs: the mirror in `bucket` of the version `cell` was forked from, if of another bucket's
/// cell, begun in the background on `runtime`, the cell's uploader's, as `cell` opens
/// ([`ObjectBucket::mirror_for`]): the host that forked it may have died before it began it. A
/// process that opens cells on a runtime of its own, as a VM's does, makes no other.
fn mirror_for(bucket: &Arc<ObjectBucket>, cell: CellId, runtime: &tokio::runtime::Handle) {
    let mirroring = bucket.clone().mirror_for(cell);
    runtime.spawn(async move {
        match mirroring.await {
            Ok(None) => {}
            Ok(Some((Origin { parent, at }, mirrored))) => logged(Ok(Some(mirrored)), parent, at),
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

node_local::node_local! {
    /// How many cells' uploaders run in this process ([`open`]): until each, its cell dropped, has
    /// uploaded what its cell made durable, and stopped.
    static UPLOADERS: tokio::sync::watch::Sender<usize> = tokio::sync::watch::channel(0).0;
}

/// How many cells' uploaders run in this process, as it changes: what an extent daemon's drain
/// waits to see none of (cladefs TASK-141.4).
pub fn uploaders() -> tokio::sync::watch::Receiver<usize> {
    UPLOADERS.get().subscribe()
}

/// Runs `future` to its end, on the calling thread, as what is called from no runtime's task does:
/// in the context of the bucket runtime ([`bucket_runtime`]), whose workers run what it spawns,
/// the connections to buckets it opens among them, which live on after it in their buckets' pools.
/// cladefs: on that runtime, not one of its own (TASK-449's review): a runtime of each call's own
/// closed the connections it opened as it returned, each call connecting anew (bucket_cell's
/// `requests_on_runtimes_dropped_after_connect_anew`); one of the calls' own beside the host's
/// served, from its own few threads, the sockets of the connections they left in the pool, which
/// the host's uploads and fetches went on to use.
/// Called on a thread of a multi-threaded runtime, as a sync path of a test's or a FUSE call's
/// is, the thread hands its other tasks to another meanwhile, as [`blocking`] does.
#[allow(clippy::disallowed_methods, reason = "an outermost entry's wait, as above")]
pub fn block_on<F: std::future::Future>(future: F) -> Result<F::Output, Failure> {
    crate::blocks!("upload::block_on");
    let runtime = bucket_runtime()?;
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

/// cladefs: runs `future` to its end as [`block_on`] does, on the calling thread in the bucket
/// runtime's context, which its bucket calls need, its output as it is: what callers outside the
/// daemon block on its async paths with, at their outermost entry, as cladefsctl, FUSE storage and
/// the tests do (TASK-685.3). Panics if the runtime can't be made.
pub fn wait_on<F: std::future::Future>(future: F) -> F::Output {
    block_on(future).expect("the bucket runtime")
}

/// cladefs: the runtime the host's uploaders and fetches run on (TASK-34, TASK-449's review), one
/// for the process: every cell's uploader and fetches, and the calls from no runtime's task
/// ([`block_on`]), so that one runtime serves every connection to the host's buckets. A bucket's
/// connections are tasks of the runtime they opened on (`reqwest`'s, as `object_store` uses it,
/// spawns them there), which stay in the bucket's pool for the next request, whatever its runtime.
/// Sized by the host, not by how many cells it serves, as each cell waits on object storage mostly,
/// and does its disk work in `block_in_place`, which leaves the runtime's threads to the others.
/// Its threads hold their lists of the shared locks they take from their start ([`crate::shared`]).
/// Made at its first need; a making that fails, a thread not spawned, is made anew at the next, not
/// kept failed for the process's life.
pub fn bucket_runtime() -> Result<&'static tokio::runtime::Runtime, Failure> {
    node_local::node_local! {
        static BUCKET_RUNTIME: Retried<tokio::runtime::Runtime> = Retried::new();
    }
    BUCKET_RUNTIME.get().get_or_make(|| {
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
pub async fn delete_cell(bucket: &ObjectBucket, cell: CellId, token: u64) -> Result<(), Failure> {
    Ok(lifecycle::delete(bucket, cell, rand::random(), token).await?)
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

/// cladefs: how long a take waits for the members of its new configuration to hold what the one
/// before held, before it fails (doc-21, TASK-537.3.4).
const SEEDING: Duration = Duration::from_secs(60);

/// cladefs: how often a replicated cell's configuration is looked at ([`Shared::keep`]).
const KEEPING: Duration = Duration::from_millis(250);

/// cladefs: how many of [`KEEPING`]'s ticks a keeper waits, after a replacement it had no members
/// for, before it tries again: from 1 s, doubling, to 30 s. Counted in ticks, not by the host's
/// clock, so that the simulation's scheduler, which says when each tick is, decides it (TASK-680).
const BACKOFF_FIRST: u32 = 4;
const BACKOFF_MOST: u32 = 120;

/// cladefs: how many of a replacement's waits for its members, of 10 ms, between its urgings of
/// the uploader, a replacement to f = 0 waiting for the bucket: one [`KEEPING`]'s worth.
const URGE_EVERY: u32 = 25;

/// How an uploader uploads.
#[derive(Clone, Debug)]
pub struct Options {
    /// How long local commits wait at most before they are uploaded.
    pub interval: Duration,
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
    /// a checkpoint covers are no longer read by them, and the collector may compact them.
    pub checkpoint_every: u64,
    /// cladefs: how long the commits in a segment the guest is filling wait past the interval for
    /// its next commit, which seals what it writes in it in place, before the interval closes it
    /// anyway (`LocalCell::close_now_unless_filling`, TASK-92.41): a commit waiting for its upload
    /// closes it at once. 30 s: eight VMs writing 768 MiB each on one disk with their bucket took
    /// 10 s and more each, the interval closing the segments they filled at 10 s, their commits
    /// copying out a segment's worth each.
    pub filling_wait: Duration,
    /// cladefs: the replication a taking of the cell gives it (doc-21, TASK-537.3.1):
    /// a cell made, or taken, by the control API, as its call asked; none keeps what the cell's
    /// head says.
    pub replication: Option<bucket_cell::layout::Wish>,
    /// cladefs: the hosts a replicated cell's configurations are made of, and how they are reached
    /// (doc-21, TASK-537.3.3): the daemon's peers, if it has them and a replica directory.
    pub members: Option<MembersOf>,
    /// cladefs: how long a member of a replicated cell's configuration is down before a
    /// replacement is picked, and how long the cell is degraded, no replacement to be had, before
    /// it drops to f = 0 (doc-21, Degraded; TASK-537.3.5). 30 s: a replacement sends the whole tail
    /// not uploaded to 2f members, which a peer daemon's restart should not cost, and meanwhile
    /// one member down of f = 1 leaves the cell's commits replicated on the other, and a cell with
    /// no quorum its commits uploaded at once (the user's word, 2026-11-19: 1 s was far too short).
    pub replace_after: Duration,
    /// cladefs: whether this opening's taking of a replicated cell is forced, as a cell placed
    /// forced is (TASK-681, `CreateCell`'s `force`): it goes on with the members of the
    /// configuration the cell's head names that answer, however few.
    pub force: bool,
}

/// cladefs: what a host knows of the hosts its replicated cells' configurations are made of
/// (doc-21, TASK-537.3.3), which the daemon's peer network is. What asks the members is a future,
/// which holds no thread as it waits for them (doc-24 step 10).
pub trait Members: Send + Sync + 'static {
    /// This host as a member: its peer address, and its replica directory's incarnation.
    fn me(&self) -> Result<bucket_cell::layout::Member, String>;
    /// 2f members for configuration `number` of `cell`, this host aside, and those at `avoid`,
    /// each that answered its incarnation.
    fn pick<'a>(
        &'a self,
        cell: CellId,
        number: u64,
        f: u8,
        avoid: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<bucket_cell::layout::Member>, String>>;
    /// A link to `member` of `cell`'s configuration `configuration`.
    fn link(
        &self,
        cell: CellId,
        configuration: &bucket_cell::layout::Configuration,
        member: &bucket_cell::layout::Member,
    ) -> Arc<dyn crate::replicate::MemberLink>;
    /// Fences `cell` at `epoch` on `member` of `configuration`: the ends of the logs it holds of
    /// it, by their numbers, synced, once its promise is (TASK-537.3.4).
    fn fence<'a>(
        &'a self,
        cell: CellId,
        configuration: &'a bucket_cell::layout::Configuration,
        member: &'a bucket_cell::layout::Member,
        epoch: u64,
    ) -> BoxFuture<'a, Result<Vec<(u64, u64)>, String>>;
    /// Log `log` that `member` holds of `configuration`, from its start.
    fn read<'a>(
        &'a self,
        cell: CellId,
        configuration: &'a bucket_cell::layout::Configuration,
        member: &'a bucket_cell::layout::Member,
        log: u64,
    ) -> BoxFuture<'a, Result<Vec<u8>, String>>;
    /// Has `member` drop what it holds of `cell`'s configurations before `configuration`.
    fn drop_before<'a>(
        &'a self,
        cell: CellId,
        configuration: &'a bucket_cell::layout::Configuration,
        member: &'a bucket_cell::layout::Member,
    ) -> BoxFuture<'a, Result<(), String>>;
    /// Whether these members drive `replicator`, a replicator just made of a cell of this host's
    /// that no task serves yet ([`crate::replicate::Replicator::idle`]): a turn at a time, as the
    /// simulation's scheduler does (TASK-680), keeping it to. Else each of its members is served by
    /// a task of its own, as the daemon's are.
    fn drives(&self, _: &Arc<crate::replicate::Replicator>) -> bool {
        false
    }
}

/// cladefs: a host's [`Members`], as [`Options`] holds it.
#[derive(Clone)]
pub struct MembersOf(pub Arc<dyn Members>);

impl std::fmt::Debug for MembersOf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Members")
    }
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
            replication: None,
            members: None,
            replace_after: Duration::from_secs(30),
            force: false,
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
    bucket: Arc<ObjectBucket>,
    (holder, options): (u64, Options),
    runtime: &Runtime,
) -> Result<(Arc<LocalCell<D>>, Uploader<D>), Failure> {
    let sleep: Clock = Arc::new(|interval| tokio::time::sleep(interval).boxed());
    let handle = runtime.handle.clone();
    let spawn: Spawn = Arc::new(move |task| drop(handle.spawn(task)));
    let uploads = runtime.uploads.clone();
    let shared = Shared::new(options, uploads, (sleep, spawn), runtime.keys.clone());
    let (started, start) = futures::channel::oneshot::channel();
    let stopped = Arc::new(Stopped::default());
    let stop = stopped.clone();
    let (uploader, uploads) = (shared.clone(), bucket.clone());
    // What the host learned for this opening, taken as it begins (TASK-449's review).
    let learned = learned(&*bucket, &recovering);
    let cell = recovering.cell();
    UPLOADERS.get().send_modify(|running| *running += 1);
    // A task, not a thread of its own (TASK-34).
    runtime.handle.spawn(async move {
        // Its conditional writes checked before any is relied on (doc-7, The head).
        match uploads.probed(holder).await {
            Ok(()) => {
                let recovering = (recovering, learned);
                uploader.run(recovering, cell_options, uploads, holder, started).await
            }
            // Its opener gone or not, it says why it stopped to whoever listens.
            Err(err) => drop(started.send(Err(Failure::msg(err)))),
        }
        // The cell let go of before the uploader says it stopped: so that dropping this returns
        // with nothing of the uploader's holding the cell open, which the process may open again
        // at once (`Dir::claim` refusing a second opening), as cladefsd does a cell an image it
        // imported made (TASK-165.6).
        drop(uploader);
        UPLOADERS.get().send_modify(|running| *running -= 1);
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
    let uploader = Uploader { shared, bucket, stopped: Some(stopped), _dir: dir };
    Ok((opened, uploader))
}

/// cladefs: an ephemeral cell's keys, as its local checkpoint keeps them (doc-25, TASK-698.3): its
/// owner, and what its fork gave it, wrapped, as a fork's head holds them: its cell's KEK under its
/// tenant's, its first DEK and the keys it inherits under its cell's KEK.
#[derive(serde::Serialize, serde::Deserialize)]
struct EphemeralKeys {
    owner: u64,
    kek: Option<keys::Kek>,
    deks: Vec<keys::Dek>,
    inherited: Vec<keys::InheritedDek>,
}

/// cladefs: opens the ephemeral cell `recovering` recovered (doc-25, TASK-698.3), with
/// `cell_options`: its keys, as its local checkpoint keeps them, unwrapped through the host's KMS
/// as its fork's head would give them; its parent's state at its version recovered from `bucket`,
/// its parent's, under them, which its cell starts from. No uploader: nothing of it goes to a
/// bucket. Its parent's extents are read through `cache`, the host's, if it has one. Its keys are
/// held among the host's while what it returns with the cell is, so that the host's fetchers read
/// what it inherits, and forgetting its tenant or cell drops them (TASK-698.7).
pub async fn open_ephemeral<D: Dir>(
    recovering: Recovering<D>,
    cell_options: crate::Options,
    bucket: Arc<ObjectBucket>,
    runtime: &Runtime,
    (cache, held): (Option<Arc<Cache>>, Option<Arc<HeldParts>>),
) -> Result<(Arc<LocalCell<D>>, RingHold, Forked), Failure> {
    let checkpoint = recovering.local_checkpoint()?;
    let (parent, cell) = (checkpoint.parent(), recovering.cell());
    let EphemeralKeys { owner, kek, deks, inherited } =
        bucket_cell::layout::decode(checkpoint.keys()).map_err(Failure::from)?;
    let origin = Origin { parent: parent.cell, at: parent.at };
    let mut head = Head::of(bucket_cell::record::Made::Clone(origin), owner);
    head.keks.extend(kek);
    (head.deks, head.inherited) = (deks, inherited);
    let cell_keys = CellKeys::of(cell, &head, runtime.keys.kms.clone());
    let ring = keys::ring(&*bucket, &cell_keys, cell, &head, true).await?;
    let recovering_parent =
        bucket_cell::recovery::recover_version(&*bucket, parent.cell, parent.at, &ring);
    let recovered = recovering_parent.await?;
    let state = recovered.layer.unwrap_or_else(|| Arc::new(recovered.state));
    let holding = runtime.keys.ring.hold(ring.clone());
    let forked = Forked { origin, ring: ring.clone() };
    let opening = crate::EphemeralOpening { parent: state, keys: ring, cache, held };
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
    bucket: Arc<ObjectBucket>,
    /// Says the uploader stopped, once it has: taken by [`Uploader::close`], or its drop, which
    /// wait for it.
    stopped: Option<Arc<Stopped>>,
    /// The cell directory, held taken for this process until the uploader stopped, last of its
    /// fields to go (TASK-129).
    _dir: DirHold<D>,
}

pub(crate) struct Shared<D: Dir, B: bucket_cell::Bucket = ObjectBucket> {
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
    /// cladefs: of a replicated cell, what its head asks for and the configuration its taking
    /// activated, which [`Shared::keep`] keeps from then on (TASK-537.3.5).
    keeping: Mutex<Option<(bucket_cell::layout::Wish, bucket_cell::layout::Configuration, bool)>>,
    /// cladefs: said, as its sender goes, once its keeper is done, which holds it, and so its
    /// cell, meanwhile: what [`Shared::run`] waits for before it returns (TASK-701).
    kept: Mutex<Option<futures::channel::oneshot::Receiver<()>>>,
    /// cladefs: the configuration whose members' logs its taking recovered the cell from, if it
    /// did, its own write-ahead log holding less (TASK-537.3.4): what the simulation follows the
    /// history of the commits of (TASK-680).
    #[cfg(test)]
    recovered_from: Mutex<Option<bucket_cell::layout::Configuration>>,
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

/// Runs `work`, which blocks on the cell's disk, on the uploaders' runtime: on its thread, which
/// hands its other tasks to another meanwhile; or as it is, on a thread no runtime runs, as the
/// simulation's (`tokio::task::block_in_place` runs it so there). cladefs: the daemon's async
/// paths' disk work too, a cell's making, opening and forking (TASK-685.3).
#[allow(clippy::disallowed_methods, reason = "the work waits on the disk alone")]
pub fn blocking<T>(work: impl FnOnce() -> T) -> T {
    // TASK-685.1: madsim's one thread runs it as it is.
    #[cfg(madsim)]
    return work();
    // cladefs: a current-thread runtime's one thread, as tests' and FUSE storage's are, has no
    // other to hand its tasks to (`block_in_place` panics there): it runs it as it is, as a thread
    // no runtime runs does (TASK-685.3).
    #[cfg(not(madsim))]
    match tokio::runtime::Handle::try_current().map(|runtime| runtime.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(work),
        _ => work(),
    }
}

/// cladefs: the members of the configuration a take found that it fenced, each with the ends of the
/// logs it holds ([`Shared::fence`]); and, of a forced take (TASK-681), why they are fewer than a
/// take needs, if they are.
struct Fenced {
    members: Vec<(bucket_cell::layout::Member, Vec<(u64, u64)>)>,
    short: Option<String>,
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
    /// The commits up to an epoch told replicated by a replicator, or uploaded.
    Told(Arc<crate::replicate::Replicator>, u64),
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
    /// cladefs: its cell's replication as the head it took says (doc-21, TASK-537.3.1): none for
    /// a cell durable off its host once uploaded alone.
    pub replication: Option<Replicated>,
}

/// cladefs: a replicated cell's replication, as a writer's taking found it: its f, what its
/// guest's fsync waits for, and the number of its active configuration, if it has one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Replicated {
    pub f: u8,
    pub fsync: bucket_cell::layout::Fsync,
    pub configuration: Option<u64>,
    /// cladefs: if the taking was forced and fenced fewer members than a take needs (TASK-681): the
    /// epoch of the last commit the bucket held, past which commits told replicated may be lost.
    pub forced_after: Option<u64>,
}

impl Replicated {
    fn of(replication: &bucket_cell::layout::Replication) -> Self {
        let bucket_cell::layout::Replication { wish, configuration } = replication;
        Self {
            f: wish.f,
            fsync: wish.fsync,
            configuration: configuration.as_ref().map(|configuration| configuration.number),
            forced_after: None,
        }
    }
}

/// What a cell's uploader has done, and has left to do (TASK-50.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    /// cladefs: of a replicated cell, its replication now (TASK-537.3.5).
    pub replication: Option<ReplicationStatus>,
}

/// cladefs: a replicated cell's writer's replication, as it is now (doc-21, Degraded;
/// TASK-537.3.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplicationStatus {
    /// The f of its active configuration: less than its head asks for once it dropped to f = 0.
    pub f: u8,
    /// How many members of its active configuration, the writer aside, take what it sends.
    pub members_up: u32,
    /// Whether fewer than f of them do, or its configuration is of f = 0: its commits waiting for
    /// the bucket, uploaded at once.
    pub degraded: bool,
    /// Whether a replacement of its configuration is under way.
    pub replacing: bool,
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

    /// cladefs: the replication of the cell, as its writer's taking found its head
    /// (doc-21): none for a cell durable off its host once uploaded alone, or not taken yet.
    pub fn replication(&self) -> Option<Replicated> {
        self.shared.progress.lock().unwrap().taking.and_then(|taking| taking.replication)
    }

    /// cladefs: what wakes those waiting on the uploader, for a replicator to call as more is
    /// replicated (TASK-537.3.3).
    pub fn waker(&self) -> impl Fn() + Send + Sync + 'static + use<D> {
        let changed = self.shared.changed.clone();
        move || {
            changed.notify(usize::MAX);
        }
    }

    /// cladefs: returns once the commits up to `epoch` are replicated or uploaded, as
    /// [`Uploader::replicated`] does: a future, which holds no thread while it waits
    /// (TASK-685.3).
    pub fn replicated(
        &self,
        epoch: u64,
    ) -> impl std::future::Future<Output = Result<(), Failure>> + Send + use<D> {
        self.awaiting(self.waiting_replicated(epoch))
    }

    /// cladefs: what waiting for the commits up to `epoch` replicated or uploaded waits for, if
    /// they aren't ([`Uploader::replicated`]).
    fn waiting_replicated(&self, epoch: u64) -> Result<Option<Waiting>, Failure> {
        let shared = &self.shared;
        let Some(replicator) = self.cell().replicator() else {
            return self.waiting_uploaded(epoch);
        };
        // cladefs: and while a replacement is under way, whose members may take long to catch up
        // (TASK-537.3.5); and while the bucket holds not yet what the cell opened with, which no
        // member need hold, nothing told replicated before (`Replicator::told`, TASK-678).
        let gated = shared.last_uploaded() < replicator.base();
        if replicator.degraded() || replicator.replacing_now() || gated {
            if shared.ask(epoch, Awaited::Uploaded)? {
                return Ok(None);
            }
        } else {
            shared.observe()?;
        }
        Ok(Some(Waiting::Told(replicator, epoch)))
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
        let replication = self.cell().replicator().map(|replicator| ReplicationStatus {
            f: u8::try_from(replicator.f()).unwrap_or(u8::MAX),
            members_up: u32::try_from(replicator.members_up()).unwrap_or(u32::MAX),
            degraded: replicator.degraded(),
            replacing: replicator.replacing_now(),
        });
        let progress = shared.progress.lock().unwrap();
        Ok(UploadStatus {
            taking: progress.taking,
            uploaded: progress.uploaded,
            pending_bytes,
            oldest_pending: progress.seen.front().map(|(_, seen)| *seen),
            stalls: progress.stalls,
            replication,
        })
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
        let given = (child.owner, Some(inheriting.given()));
        let forking = lifecycle::fork_version_keyed(
            &*self.bucket,
            child.cell,
            at.version,
            at.origin,
            given,
            keys,
        );
        let made = match forking.await {
            Err(bucket_cell::Error::Deleted) => return Err(Failure::VersionDropped),
            Err(err) => return Err(err.into()),
            Ok(made) => made,
        };
        if let Some(head) = made {
            inheriting.made();
            if let Some(known) = bucket_cell::Bucket::known(&*self.bucket) {
                known.learn(child.cell, Learned { head });
            }
        }
        mirror_wanted(&self.bucket, child.cell, at.origin)?;
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
    ) -> Result<crate::LocalCheckpoint, Failure> {
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
        let given = inheriting.given();
        let (deks, inherited) = given.deks.unwrap_or_default();
        let keys = EphemeralKeys { owner: child.owner, kek: given.kek, deks, inherited };
        inheriting.made();
        let parent =
            crate::Parent { cell: at.origin.parent, version: at.version, at: at.origin.at };
        Ok(crate::LocalCheckpoint::fork(parent, &at.state, bucket_cell::layout::encode(&keys)))
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
    pub fn bucket(&self) -> &ObjectBucket {
        &self.bucket
    }

    /// What it keeps the cell as.
    pub fn options(&self) -> &Options {
        &self.shared.options
    }

    /// How many commits waited for the lag to shrink.
    pub fn stalls(&self) -> u64 {
        self.shared.progress.lock().unwrap().stalls
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
            shared.progress.lock().unwrap().cut = true;
            shared.wake.notify_one();
        }
    }

    /// Has the uploader, stopping, upload all its cell made durable first, as it does unless
    /// cut short ([`Uploader::cut_short`]): for a cell another writer took over, as it was, its
    /// uploader going on, whose going later no writer waits for.
    pub fn drain_on_stop(&self) {
        self.shared.progress.lock().unwrap().cut = false;
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
        // with it (doc-24).
        if node_local::dying() {
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
        self.progress.lock().unwrap().wanted = true;
        self.wake.notify_one();
    }
}

impl<D: Dir, B: bucket_cell::Bucket> Shared<D, B> {
    /// An uploader that uploads as `options` say, within `uploads`, its interval running out on
    /// `clock`: started by [`Shared::run`]. cladefs: encrypting and reading under `keys`, the
    /// host's (TASK-411.3); its keeper run on `spawn`.
    pub(crate) fn new(
        options: Options,
        uploads: Arc<Budget>,
        (clock, spawn): (Clock, Spawn),
        keys: keys::Host,
    ) -> Arc<Self> {
        Arc::new(Self {
            cell: std::sync::OnceLock::new(),
            writer: Default::default(),
            token: Default::default(),
            at_version: Default::default(),
            progress: Mutex::new(Progress::default()),
            changed: Arc::new(Event::new()),
            wake: tokio::sync::Notify::new(),
            options,
            uploads,
            clock,
            spawn,
            holders: Mutex::default(),
            keys,
            held: Mutex::default(),
            keeping: Mutex::default(),
            kept: Mutex::default(),
            #[cfg(test)]
            recovered_from: Mutex::default(),
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

    /// The epoch of the cell's latest commit uploaded and acknowledged.
    pub(crate) fn last_uploaded(&self) -> u64 {
        self.progress.lock().unwrap().uploaded
    }

    /// The lowest sequence number the cell's guest may mint extents' IDs from.
    pub(crate) fn next_sequence(&self) -> u64 {
        self.progress.lock().unwrap().next_sequence
    }

    /// Why uploading stopped, if it did.
    pub(crate) fn failed(&self) -> Option<Failure> {
        self.progress.lock().unwrap().failed.clone()
    }

    /// The cell's writer, once one took it.
    pub(crate) fn writer(&self) -> Option<Arc<Writer<B>>> {
        self.writer.lock().unwrap().clone()
    }

    /// Has the uploader stop: once it uploaded what its cell made durable, its open segment's
    /// commits too, unless cut short ([`Uploader::cut_short`]).
    pub(crate) fn stop(&self) {
        self.progress.lock().unwrap().stop = true;
        self.changed.notify(usize::MAX);
        self.wake.notify_one();
    }

    /// Waits until the uploader is woken, or `interval` runs out first: whether it was woken.
    async fn woken(&self, interval: Duration) -> bool {
        // cladefs: in a simulation, the interval run out at once, uploading what it holds now, of
        // any size (doc-24).
        if bucket_cell::buggify!() {
            return false;
        }
        let notified = std::pin::pin!(self.wake.notified());
        let ran_out = (self.clock)(interval);
        matches!(futures::future::select(notified, ran_out).await, Either::Left(_))
    }

    fn cell(&self) -> &Arc<LocalCell<D>> {
        self.cell.get().expect("the uploader opened its cell")
    }

    /// Notes the cell's latest commit, if it isn't uploaded, as seen now.
    fn observe(&self) -> Result<(), Failure> {
        let epoch = self.cell().last_commit()?;
        let mut progress = self.progress.lock().unwrap();
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
        let mut progress = self.progress.lock().unwrap();
        if self.reached(&progress, epoch, awaited).transpose()?.is_some() {
            return Ok(true);
        }
        progress.wanted = true;
        if awaited == Awaited::Lag {
            progress.stalls += 1;
            let pending = self.cell().wal_bytes()?;
            let commits = epoch.saturating_sub(progress.uploaded);
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
        Ok(uploaded >= epoch && self.cell().wal_bytes()? <= max_bytes)
    }

    /// cladefs: what `waiting` finds in `progress`: done, or failed, or, if nothing yet, `None`
    /// ([`Shared::reached`]; a replicator's commits told replicated).
    fn found(&self, progress: &Progress, waiting: &Waiting) -> Option<Result<(), Failure>> {
        match waiting {
            Waiting::For(awaited, epoch) => self.reached(progress, *epoch, *awaited),
            Waiting::Told(replicator, epoch) if replicator.told(progress.uploaded) >= *epoch => {
                Some(Ok(()))
            }
            Waiting::Told(..) => progress.failed.clone().map(Err),
        }
    }

    /// cladefs: returns once the progress has what `waiting` waits for, done or failed, holding no
    /// thread while it waits (TASK-685.3).
    async fn wait(&self, waiting: &Waiting) -> Result<(), Failure> {
        loop {
            let changed = {
                let progress = self.progress.lock().unwrap();
                if let Some(found) = self.found(&progress, waiting) {
                    return found;
                }
                self.changed.listen()
            };
            changed.await;
        }
    }

    /// Runs the uploader of the cell `recovering` recovered, opening it first, with what the host
    /// learned of its cell for this opening, if anything ([`learned`]): returns once it stopped,
    /// its keeper too, if it has one, so that nothing of it holds the cell open (TASK-701).
    pub(crate) async fn run(
        self: &Arc<Self>,
        recovering: (Recovering<D>, Option<Learned>),
        options: crate::Options,
        bucket: Arc<B>,
        holder: u64,
        started: futures::channel::oneshot::Sender<Result<Arc<LocalCell<D>>, Failure>>,
    ) {
        self.upload(recovering, options, bucket, holder, started).await;
        let kept = self.kept.lock().unwrap().take();
        if let Some(kept) = kept {
            // Said as the keeper's task ends, or goes, its sender with it.
            let _ = kept.await;
        }
    }

    /// Runs the uploader, as [`Shared::run`] does, but for waiting for its keeper.
    async fn upload(
        self: &Arc<Self>,
        recovering: (Recovering<D>, Option<Learned>),
        options: crate::Options,
        bucket: Arc<B>,
        holder: u64,
        started: futures::channel::oneshot::Sender<Result<Arc<LocalCell<D>>, Failure>>,
    ) {
        let interval = self.options.interval;
        self.holders.lock().unwrap().0 = holder;
        let writer = match self.start(recovering, options, &bucket, holder).await {
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
            while !self.progress.lock().unwrap().stop {
                self.wake.notified().await;
            }
            return;
        };
        // cladefs: a replicated cell's configuration kept whole beside (TASK-537.3.5).
        let keeping = self.keeping.lock().unwrap().take();
        if let Some((wish, configuration, stale)) = keeping {
            let (done, kept) = futures::channel::oneshot::channel::<()>();
            *self.kept.lock().unwrap() = Some(kept);
            let keeper = self.clone().keep(writer.clone(), wish, (configuration, stale));
            (self.spawn)(
                async move {
                    keeper.await;
                    // Said once the keeper, and what it held, went.
                    drop(done);
                }
                .boxed(),
            );
        }
        // The segments uploaded, and as many as were when the last checkpoint was begun.
        let (mut segments, mut checkpointed) = (0, 0);
        let mut in_flight = InFlight::default();
        // cladefs: the work with the cell's head under way, an acknowledgement or a checkpoint, one
        // at a time, beside the uploads, which go on meanwhile as it waits for the bucket's object
        // store to take the head's next write: an acknowledgement then names all they uploaded
        // (`Writer::acknowledge`, TASK-690). And whether an upload since it began wants an
        // acknowledgement.
        let mut head_work: Option<BoxFuture<'static, Result<HeadWork, bucket_cell::Error>>> = None;
        let mut ack_wanted = false;
        // Whether the next gather closes the cell's open segment: for a commit waiting for its
        // upload, or a fork, or as the interval is up, or the uploader stops; and whether the last
        // took segments, after which the cell may hold more closed, which the next takes,
        // closing none for them (TASK-92.15).
        // cladefs: and whether a commit waits for it, or a fork, or the uploader stops: closing
        // whatever it holds, not only once the guest is filling it no more (TASK-92.41).
        let (mut close, mut more, mut urgent) = (false, false, false);
        loop {
            if let Err(err) = self.observe() {
                return self.fail(&writer, err);
            }
            // cladefs: acknowledgements the cell's disk refused before, tried again.
            if !in_flight.acknowledging.is_empty()
                && let Err(err) = self.acknowledge_pending(&mut in_flight).await
            {
                return self.fail(&writer, err);
            }
            let (stop, cut) = {
                let mut progress = self.progress.lock().unwrap();
                urgent |= std::mem::take(&mut progress.wanted);
                close |= urgent;
                (progress.stop, progress.cut)
            };
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
                close &= !gathered.closed;
                urgent &= !gathered.closed;
                more = gathered.took;
                if more {
                    continue;
                }
                if stop && in_flight.is_empty() && head_work.is_none() && !ack_wanted {
                    // A checkpoint on clean shutdown (doc-7, Checkpoints).
                    if let Err(err) = writer.checkpoint().await {
                        log::warn!("cladefs: checkpointing cell {}: {err}", writer.cell());
                    }
                    return self.fail(&writer, Failure::msg("the uploader stopped"));
                }
            }
            if ack_wanted && head_work.is_none() {
                ack_wanted = false;
                let writer = writer.clone();
                let acknowledging = async move { writer.acknowledge().await };
                head_work = Some(acknowledging.map_ok(HeadWork::Acknowledged).boxed());
            }
            let mut woken = std::pin::pin!(self.woken(interval));
            let happened = futures::future::poll_fn(|cx| {
                if let Some(work) = head_work.as_mut()
                    && let Poll::Ready(done) = work.poll_unpin(cx)
                {
                    return Poll::Ready(Happened::Headed(done));
                }
                if let Poll::Ready(Some(uploaded)) = in_flight.next().poll_unpin(cx) {
                    return Poll::Ready(Happened::Uploaded(uploaded));
                }
                woken.as_mut().poll(cx).map(Happened::Woken)
            })
            .await;
            let done = match happened {
                Happened::Woken(woken) => {
                    close |= !woken;
                    Ok(())
                }
                Happened::Uploaded(uploaded) => uploaded.map_err(Failure::from).map(|()| {
                    segments += 1;
                    ack_wanted = true;
                }),
                Happened::Headed(done) => {
                    head_work = None;
                    match done {
                        Ok(HeadWork::Acknowledged(through)) => {
                            let acked = self.acknowledged(through, &mut in_flight);
                            // A checkpoint every so many segments, after the commits it covers
                            // returned.
                            if segments - checkpointed >= self.options.checkpoint_every {
                                checkpointed = segments;
                                head_work = Some(Self::checkpointing(writer.clone()));
                            }
                            acked.await
                        }
                        Ok(HeadWork::Checkpointed) => Ok(()),
                        Err(err) => Err(err.into()),
                    }
                }
            };
            if let Err(err) = done {
                return self.fail(&writer, err);
            }
        }
    }

    /// cladefs: writes a checkpoint as of what `writer` acknowledged, as the run's work with the
    /// cell's head: a checkpoint failing is said, and fails nothing.
    fn checkpointing(
        writer: Arc<Writer<B>>,
    ) -> BoxFuture<'static, Result<HeadWork, bucket_cell::Error>> {
        async move {
            // A test's: the checkpoint landing late, after the commits it covers returned, as on
            // a loaded host, which a collector's pass then runs beside (TASK-339).
            fail::fail_point!("upload::checkpointing");
            if let Err(err) = writer.checkpoint().await {
                log::warn!("cladefs: checkpointing cell {}: {err}", writer.cell());
            }
            Ok(HeadWork::Checkpointed)
        }
        .boxed()
    }

    /// Stops uploading, for `err`: commits waiting for uploads, and forks, fail with it.
    fn fail(&self, writer: &Writer<B>, err: Failure) {
        let mut progress = self.progress.lock().unwrap();
        if !progress.stop {
            log::error!("cladefs: uploading cell {}: {err}", writer.cell());
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
        &self,
        (recovering, learned): (Recovering<D>, Option<Learned>),
        options: crate::Options,
        bucket: &Arc<B>,
        holder: u64,
    ) -> Result<Option<Arc<Writer<B>>>, Failure> {
        let cell = recovering.cell();
        let (cache, parts) = (self.options.cache.clone(), self.options.held.clone());
        let placement = recovering.token().unwrap_or(self.options.token);
        let kms = &self.keys.kms;
        let read = first_read(&**bucket, cell, learned, kms).await?;
        // A cell with no head is none: its maker makes it before its cell first opens.
        let Some(found) = read.head(cell)? else {
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
            let _ = self.cell.set(Arc::new(open_cell));
            self.hold_ring(&keys);
            let mut progress = self.progress.lock().unwrap();
            progress.uploaded_through(u64::MAX);
            return Ok(None);
        }
        // cladefs: a cell deleted takes no writer.
        if found.deleted() {
            return Err(bucket_cell::Error::Deleted.into());
        }
        // cladefs: recovered before the taking under its keys, as the taking will hold them,
        // unwrapped once for it (TASK-411.3); a head with none yet, a fork's child's or a new
        // cell's, which its taking gives them, recovered once it is taken.
        let before = match found.deks.is_empty() {
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
                let chain = Chain::new(cell, 0, recovered.end(), 0);
                let head = recovered.head.clone().unwrap_or_default();
                if !recovering.follows(&TakenCell::of(head, &recovered.state, chain)) {
                    return Err(bucket_cell::Error::Diverged.into());
                }
                Some(recovered)
            }
            true => None,
        };
        // cladefs: its head taken by this placement, in one conditional write that gives the cell
        // what it lacks (TASK-515): a new cell's KEK from the host's pool, wrapped by its tenant's
        // ahead (every cell is encrypted, TASK-411.28), its DEK, and the kind of the cell, unless
        // it is of another (`OtherKind`, doc-20). Not a version's, which no writer
        // takes, and any number of hosts open at once. A cell with no head is no cell: its maker
        // makes it, before its cell opens (`lifecycle::create`), and no taking does.
        let taking = head::Taking {
            holder,
            token: placement,
            kind: recovering.kind(),
            keys: None,
            replication: self.options.replication,
        };
        let (held, cell_keys) = keys::taking(&**bucket, kms, cell, read, taking).await?;
        // cladefs: a replicated cell whose head names a configuration of f >= 1: its members
        // fenced at this taking's epoch, once it is taken, before anything of theirs is read
        // (doc-21, TASK-537.3.4): no writer of an earlier epoch has a commit replicated after.
        let named = (held.head.replication.as_ref())
            .and_then(|replication| replication.configuration.clone());
        // cladefs: a replicated cell's, whose writer's copies its cell notes (TASK-683).
        let replicated = held.head.replication.is_some() || self.options.replication.is_some();
        let old = named.clone().filter(|configuration| configuration.f >= 1);
        // cladefs: or, its placement's takings forced, those of them that answer, however few
        // (TASK-681).
        // Its own copy counted if it is its writer's, its log holding records, or, holding none,
        // its description naming the configuration's writer's epoch (TASK-683).
        let forced = recovering.forced() || self.options.force;
        let fenced = match &old {
            Some(old) => {
                let own = recovering.holds_own_copy(old.epoch);
                Some(self.fence(cell, old, held.head.epoch, own, forced).await?)
            }
            None => None,
        };
        // cladefs: the host serves a cell of the tenant's: its pool of new cell KEKs, which forks
        // of its cells into new cells take, filled off this path (TASK-411.4).
        kms.refill(cell);
        let keys = keys::ring(&**bucket, &cell_keys, cell, &held.head, true).await?;
        let writer = Writer::holding(bucket.clone(), cell, held, before, keys).await?;
        let writer = Arc::new(writer.uploading_within(self.uploads.clone()));
        let keys = writer.keys().clone();
        // cladefs: what the fenced members hold, the longest copy of each log, after the head's
        // last commit: the cell's write-ahead log, if it holds none, and what the new
        // configuration's members are given before it is active (TASK-537.3.4).
        let (seed, recovering, short, restored) = match (&old, fenced) {
            (Some(old), Some(Fenced { members: fenced, short })) => {
                let logs = self.read_longest(cell, old, &fenced, &keys).await?;
                let last = writer.taken().last_commit;
                let seed = match crate::replicate::trimmed(cell, &logs, last, Some(&keys)) {
                    Ok(seed) => seed,
                    // Forced, what the members that answered hold, going not on from the head, is
                    // given up as what the others held is (TASK-681).
                    Err(err) if short.is_some() => {
                        log::warn!("cladefs: {cell:?} taken by force: its members' logs: {err:#}");
                        Vec::new()
                    }
                    Err(err) => return Err(err.into()),
                };
                // Its own write-ahead log, if it holds one, recovered unless the members' go past
                // it: a later writer's, or this writer's beyond its own sync (TASK-537.3.4's
                // review).
                let own = recovering.last_commit_held();
                let members = crate::replicate::last_commit(cell, &seed);
                let ahead = crate::replicate::members_ahead(own, members);
                // Its own kept, the members' logs are given the new configuration only if they are
                // of its history: else an earlier writer's, past what a later one, this cell's
                // own, went on from, none of it told replicated (TASK-678's review: a retake whose
                // take before deferred, the head naming the configuration before still).
                let seed = match ahead || members.is_none() || recovering.holds_commits(members) {
                    true => seed,
                    false => {
                        log::info!(
                            "cladefs: {cell:?}: its members' logs, through {members:?}, are an \
                             earlier writer's than its own, through {own:?}: given up"
                        );
                        Vec::new()
                    }
                };
                let recovering = match ahead {
                    false => recovering,
                    true => blocking(|| -> Result<_, Failure> {
                        #[cfg(test)]
                        {
                            *self.recovered_from.lock().unwrap() = Some(old.clone());
                        }
                        let mut recovering = recovering;
                        if recovering.has_wal() {
                            log::warn!(
                                "cladefs: {cell:?}: its members hold past its own log ({own:?} < \
                                 {members:?}): recovered from them"
                            );
                            recovering.drop_wal()?;
                        }
                        Ok(recovering.restored(&seed, last)?)
                    })?,
                };
                (seed, recovering, short, ahead)
            }
            _ => (Vec::new(), recovering, None, false),
        };
        // cladefs: taken by force from too few members: what may be lost noted in the cell's
        // description, which its status says from now on, while this placement holds it; or, of an
        // earlier taking of this forced placement, what that one noted (TASK-681).
        let mut recovering = recovering;
        // cladefs: the copies it is noted, before a configuration of its writer's is named or
        // replicates (TASK-683): that writer's; and, its log restored from members of the
        // configuration named, the longest copies of every log a quorum fenced, that one's, all
        // of it but taken by force. Those it was before of the one named or later kept, unless its
        // log was dropped, which let them go.
        if replicated {
            let mut copies = vec![writer.epoch()];
            if let (true, None, Some(old)) = (restored, &short, &old) {
                copies.push(old.epoch);
            }
            let named = named.as_ref().map(|configuration| configuration.epoch);
            blocking(|| recovering.note_writer_copies(&copies, named))?;
        }
        let forced_after = match &short {
            Some(_) => {
                let kept = writer.taken().last_commit.map_or(0, epoch_of);
                blocking(|| recovering.note_forced_after(placement, kept))?;
                Some(kept)
            }
            None => recovering.forced_after(),
        };
        let (state, layer, taken) = (writer.state(), writer.layer(), Some(writer.taken().clone()));
        let opening = Opening { state, layer, taken, cache, keys: keys.clone(), held: parts };
        let open_cell = blocking(|| recovering.open(options, opening))?;
        let _ = self.cell.set(Arc::new(open_cell));
        self.hold_ring(&keys);
        let _ = self.token.set(placement);
        self.took(&writer, placement);
        // cladefs: taken by force from too few members: what may be lost said, and kept in its
        // status (TASK-681).
        // Kept for sure: what the bucket holds. Past it, what the members that answered held, and
        // its own write-ahead log, which may be an earlier writer's, past what a later one told
        // replicated on the members that answered not (TASK-680's simulation, seed 129).
        if let (Some(short), Some(kept)) = (&short, forced_after) {
            log::warn!(
                "cladefs: {cell:?} taken by force: {short}: the bucket holds its commits through \
                 {kept}; commits told replicated past it that only the members that answered not \
                 held may be lost"
            );
        }
        if let Some(kept) = forced_after {
            self.forced_after(kept);
        }
        // cladefs: a replicated cell's commits replicated from now on (doc-21, TASK-537.3.3).
        // cladefs: and, of a cell whose head names a configuration, replicated at its f at least,
        // whatever this taking asked: its head goes on naming a live configuration, whose
        // members hold what the one before held, until its writer drops it to f = 0 once the
        // bucket holds all it told (TASK-537.3.5; TASK-537.3.4's review).
        if let Some(mut replication) = writer.taken().head.replication.clone() {
            let wish = replication.wish;
            let before = old.as_ref().map_or(0, |old| old.f);
            replication.wish.f = replication.wish.f.max(before);
            if replication.wish.f >= 1 {
                let (configuration, stale) =
                    self.replicate(&writer, replication, old, seed).await?;
                *self.keeping.lock().unwrap() = Some((wish, configuration, stale));
            }
        }
        Ok(Some(writer))
    }

    /// cladefs: `writer`, having taken a replicated cell asking for `replication`, has its commits
    /// replicated from now on (doc-21, TASK-537.3.3, TASK-537.3.4): a configuration of this host
    /// and 2f members picked from its peers, given `seed`, what the configuration `old` before held
    /// after the head's last commit, and activated by a swap of the head once f of them hold it, so
    /// that what `old` held replicated stays so; then the cell's replicator started, of the
    /// commits after those it opened with, on no member of the new configuration but the seed's.
    /// The configuration before's members drop what they held of it, if they answer.
    ///
    /// A cell that opened holding more than `old`'s members gave, its own write-ahead log's
    /// commits on its own host (a retake on the writer's disk, the member holding the rest out of
    /// reach), may hold commits told replicated that no member of the new configuration is given:
    /// it is activated only by the acknowledgement that names them all, in the same swap
    /// (`Writer::configure_past`), the head naming `old`, fenced, until then (TASK-678; the spec's
    /// installOwn, its mutation InstallMembersOnly). Returns the configuration, and whether the
    /// head names one before it still ([`Shared::keep`]'s `stale`).
    async fn replicate(
        &self,
        writer: &Arc<Writer<B>>,
        replication: bucket_cell::layout::Replication,
        old: Option<bucket_cell::layout::Configuration>,
        seed: Vec<(u64, Vec<u8>)>,
    ) -> Result<(bucket_cell::layout::Configuration, bool), Failure> {
        use bucket_cell::layout::Configuration;
        let bucket_cell::layout::Replication { wish, configuration } = replication;
        let members = self.members()?;
        let open_cell = self.cell().clone();
        let cell = open_cell.description().cell;
        let before = configuration.clone();
        let number = configuration.map_or(1, |configuration| configuration.number + 1);
        let me = members.me().map_err(Failure::msg)?;
        let picked = (me, members.pick(cell, number, wish.f, &[]).await);
        let base = open_cell.last_commit()?;
        let epoch = writer.epoch();
        // The last commit the head names, which every configuration's members go on from.
        let named = writer.taken().last_commit.map(epoch_of);
        let (me, others) = match picked {
            (me, Ok(others)) => (me, others),
            // cladefs: no members to be had: the cell serves degraded, at f = 0, its commits
            // waiting for the bucket, until its keeper has members (Codex's review of
            // TASK-537.3.5): a take of a cell its writer dropped to f = 0 needs no peers. And a
            // take that recovered what the configuration before held, which it can give no
            // configuration of its own, a host of three lost (TASK-537.3.6): the head goes on
            // naming that configuration, fenced, its members holding what was told, until the
            // acknowledgement that names all the cell recovered names its own, of f = 0, in the
            // same swap (`Writer::configure_past`; the spec's aloneSwitch).
            (me, Err(err)) => {
                log::warn!(
                    "cladefs: {cell:?} degraded: no configuration of f {} to be had, its \
                     commits waiting for the bucket: {err}",
                    wish.f
                );
                let spool = blocking(|| open_cell.spool())?;
                let replicator = self.replicator(&members, 0, base, spool, Vec::new());
                open_cell.replicate_to(replicator).await;
                return Ok(match before {
                    Some(before) if before.f == 0 => (before, false),
                    // The writer's own, of f = 0, which the head names once it names all the
                    // cell recovered: at once if the bucket holds it all.
                    Some(before) => {
                        let number = before.number + 1;
                        let alone = Configuration { number, epoch, f: 0, members: vec![me] };
                        match named.is_some_and(|named| named >= base) {
                            true => writer.configure(alone.clone()).await?,
                            false => writer.configure_past(base, alone.clone()),
                        }
                        (alone, true)
                    }
                    // A cell the head names no configuration of: its keeper's replacement the
                    // first.
                    None => (Configuration { number: 0, epoch, f: 0, members: vec![me] }, false),
                });
            }
        };
        let mut all = vec![me];
        all.extend(others.iter().cloned());
        let configuration = Configuration { number, epoch, f: wish.f, members: all };
        let links =
            others.iter().map(|member| members.link(cell, &configuration, member)).collect();
        let spool = blocking(|| open_cell.spool())?;
        let replicator = self.replicator(&members, usize::from(wish.f), base, spool, links);
        // What `old`'s members held, which they give the new configuration's: through the seed's
        // last commit, or the head's. The cell holding more, its own log's, of `old`'s writer's
        // (TASK-678): the head names the new configuration once it names those too.
        let given = crate::replicate::last_commit(cell, &seed).map(epoch_of).max(named);
        if bucket_cell::rules().installs_own && old.is_some() && given.unwrap_or(0) < base {
            log::info!(
                "cladefs: {cell:?} replicated in configuration {number} with {:?} once the bucket \
                 holds its commits through {base}, which its own log held past what the members \
                 of configuration {} gave ({given:?}): until then the head names that one",
                configuration.members.iter().map(|member| &member.address).collect::<Vec<_>>(),
                old.as_ref().map_or(0, |old| old.number),
            );
            writer.configure_past(base, configuration.clone());
            open_cell.replicate_to(replicator).await;
            return Ok((configuration, true));
        }
        if !seed.is_empty() {
            let appends = seed.into_iter().map(|(log, bytes)| (log, 0, bytes)).collect();
            let flushed = crate::replicate::Flushed { epoch: base, appends };
            replicator.flushed(flushed).map_err(|err| Failure::local(err, "spooling the seed"))?;
            let started = std::time::Instant::now();
            while replicator.replicated() < base {
                if replicator.fenced().is_some() || started.elapsed() > SEEDING {
                    return Err(Failure::msg(format!(
                        "the new configuration's members took not what the one before held \
                         within {SEEDING:?}: {:?}",
                        replicator.down()
                    )));
                }
                (self.clock)(Duration::from_millis(10)).await;
            }
        }
        writer.configure(configuration.clone()).await?;
        open_cell.replicate_to(replicator).await;
        self.configured(number);
        log::info!(
            "cladefs: {cell:?} replicated in configuration {number} with {:?}",
            configuration.members.iter().map(|member| &member.address).collect::<Vec<_>>()
        );
        if let Some(old) = old {
            self.drop_before(cell, &old, &configuration).await;
        }
        Ok((configuration, false))
    }

    /// cladefs: a replicator of a configuration of `f`, its members reached by `links`, started
    /// after the commit of epoch `base`, spooling in `spool` ([`crate::replicate::Replicator`]):
    /// its members served by threads of their own, unless `members` drive it.
    fn replicator(
        &self,
        members: &Arc<dyn Members>,
        f: usize,
        base: u64,
        spool: Arc<dyn crate::replicate::Spool>,
        links: Vec<Arc<dyn crate::replicate::MemberLink>>,
    ) -> Arc<crate::replicate::Replicator> {
        let changed = self.changed.clone();
        let waker = move || {
            changed.notify(usize::MAX);
        };
        let replicator = crate::replicate::Replicator::idle(f, base, spool, links, waker);
        if !members.drives(&replicator) {
            replicator.run_tasks();
        }
        replicator
    }

    /// cladefs: has the members of `old`, but this host, drop what they hold of `cell`'s
    /// configurations before `configuration`, best effort: a member that does not answer sweeps
    /// them later (doc-21, Sweeping stale replica logs).
    async fn drop_before(
        &self,
        cell: CellId,
        old: &bucket_cell::layout::Configuration,
        configuration: &bucket_cell::layout::Configuration,
    ) {
        let Ok(members) = self.members() else { return };
        for member in old.members.iter().skip(1) {
            if let Err(err) = members.drop_before(cell, configuration, member).await {
                log::info!("cladefs: {} keeps {cell:?}'s logs for now: {err}", member.address);
            }
        }
    }

    /// cladefs: keeps the configuration of a replicated cell, `configuration` as its taking
    /// activated, what its head asks for, `wish`, until the uploader stops, or its writer is fenced
    /// (doc-21, Replacing a member and Degraded; TASK-537.3.5):
    /// - a member refusing the writer, or down for [`Options::replace_after`], is replaced, by a
    ///   configuration of `wish`'s f, picked from the host's peers, the refusing aside;
    /// - its commits uploaded at once, rather than at the interval, while degraded: fewer than f
    ///   members take what it sends, their waiters left to the bucket;
    /// - degraded, and no replacement to be had, it drops to f = 0, so that a take of the cell
    ///   needs no member of a configuration lost, and back up as members come back;
    /// - a configuration of another f than `wish`'s, as a taking asking for f = 0 of a cell
    ///   replicated keeps until the bucket holds what it told, is replaced by one of `wish`'s;
    /// - `stale`, the head naming the configuration of the writer before, which the taking could
    ///   give no configuration of its own (TASK-537.3.6): nothing replaced until the
    ///   acknowledgement naming what the taking recovered names the writer's own
    ///   (`Writer::configure_past`).
    async fn keep(
        self: Arc<Self>,
        writer: Arc<Writer<B>>,
        wish: bucket_cell::layout::Wish,
        (mut configuration, mut stale): (bucket_cell::layout::Configuration, bool),
    ) {
        let cell = writer.cell();
        let mut degraded = None::<std::time::Instant>;
        // Ticks to wait before trying a replacement again, and those left to wait.
        let (mut backoff, mut skip) = (0u32, 0u32);
        // Each attempt's configuration a number of its own, one given up never reused (the
        // review's: a member of both would hold what it never dropped of the first under the
        // second's key); and the members that refused one, not picked again at once.
        let mut number = configuration.number + 1;
        let mut avoid = Vec::<String>::new();
        loop {
            // Its tick, or the uploader stopping, which it holds up no longer (TASK-701).
            let stopping = std::pin::pin!(self.until_stopping());
            futures::future::select((self.clock)(KEEPING), stopping).await;
            if self.stopping() {
                return;
            }
            let Some(replicator) = self.cell().replicator() else { return };
            if replicator.fenced().is_some() {
                return;
            }
            skip = skip.saturating_sub(1);
            if replicator.degraded() {
                if degraded.is_none() {
                    log::warn!(
                        "cladefs: {cell:?} degraded: {} of the {} members of configuration {} \
                         up, its commits waiting for the bucket: {:?}",
                        replicator.members_up(),
                        2 * usize::from(configuration.f),
                        configuration.number,
                        replicator.down()
                    );
                }
                degraded.get_or_insert_with(std::time::Instant::now);
                self.eager();
            } else if degraded.take().is_some() {
                log::info!("cladefs: {cell:?} degraded no more");
            }
            // Alone, its own configuration is named once the bucket holds what it recovered: no
            // replacement before (the spec's aloneSwitch).
            if stale {
                match writer.switching() {
                    // What it waits for uploaded at once (TASK-678).
                    true => {
                        self.eager();
                        continue;
                    }
                    false => stale = false,
                }
                // What StatCell says of it, the head naming it now (Codex's review).
                self.configured(configuration.number);
                if configuration.f == 0 && wish.f == 0 {
                    self.cell().unreplicate().await;
                    log::info!("cladefs: {cell:?} replicated no more, as its head asks");
                    return;
                }
            }
            let replace_after = self.options.replace_after;
            let replace = configuration.f != wish.f || replicator.lost(replace_after);
            if !replace || skip > 0 {
                continue;
            }
            let refusing = replicator.refusing().into_iter();
            avoid.extend(
                refusing.filter_map(|m| Some(configuration.members.get(m + 1)?.address.clone())),
            );
            number = number.max(configuration.number + 1);
            let replacing = (&configuration, &mut number, wish.f);
            let mut new = self.replace(&writer, &replicator, replacing, &mut avoid).await;
            // Degraded, and no member to be had: f = 0, once the bucket holds what it told.
            let long = degraded.is_some_and(|since| since.elapsed() >= replace_after);
            if matches!(new, Ok(None)) && wish.f >= 1 && configuration.f >= 1 && long {
                let replacing = (&configuration, &mut number, 0);
                new = self.replace(&writer, &replicator, replacing, &mut avoid).await;
            }
            match new {
                Ok(Some(new)) => {
                    configuration = new;
                    backoff = 0;
                    avoid.clear();
                    if configuration.f == 0 && wish.f == 0 {
                        self.cell().unreplicate().await;
                        log::info!("cladefs: {cell:?} replicated no more, as its head asks");
                        return;
                    }
                }
                Ok(None) => {
                    backoff = (backoff * 2).clamp(BACKOFF_FIRST, BACKOFF_MOST);
                    skip = backoff;
                }
                // Its writer stopped, the uploader with it.
                Err(err) => {
                    log::error!("cladefs: {cell:?}: replacing its configuration: {err}");
                    return;
                }
            }
        }
    }

    /// cladefs: the head names the writer's configuration `number`, as its status says from now.
    fn configured(&self, number: u64) {
        if let Some(Taking { replication: Some(replicated), .. }) =
            self.progress.lock().unwrap().taking.as_mut()
        {
            replicated.configuration = Some(number);
        }
    }

    /// cladefs: the writer's taking was forced, fewer members fenced than a take needs, and
    /// the bucket holding its commits through epoch `kept`, as its status says from now (TASK-681).
    fn forced_after(&self, kept: u64) {
        if let Some(Taking { replication: Some(replicated), .. }) =
            self.progress.lock().unwrap().taking.as_mut()
        {
            replicated.forced_after = Some(kept);
        }
    }

    /// cladefs: the configuration whose members' logs its taking recovered the cell from, if it
    /// did ([`Shared::recovered_from`]'s field).
    #[cfg(test)]
    pub(crate) fn recovered_from(&self) -> Option<bucket_cell::layout::Configuration> {
        self.recovered_from.lock().unwrap().clone()
    }

    /// cladefs: whether the uploader stops, or stopped.
    fn stopping(&self) -> bool {
        let progress = self.progress.lock().unwrap();
        progress.stop || progress.failed.is_some()
    }

    /// cladefs: returns once the uploader is stopping ([`Shared::stopping`]), holding no thread.
    async fn until_stopping(&self) {
        loop {
            let changed = self.changed.listen();
            if self.stopping() {
                return;
            }
            changed.await;
        }
    }

    /// cladefs: has the uploader close and upload what the cell holds at once, if it holds
    /// commits not uploaded: as a replicated cell's commits wait for the bucket (doc-21, Degraded).
    fn eager(&self) {
        let Ok(last) = self.cell().last_commit() else { return };
        if last > self.last_uploaded() {
            bucket_cell::host_disk::Urge::urge(self);
        }
    }

    /// cladefs: replaces `writer`'s configuration `from` with `number`, of `f`, `number` moved past
    /// it once its members are sent anything (doc-21, Replacing a member; TASK-537.3.5): this host
    /// and 2f members picked from its peers, those at `avoid` aside, made at its epoch; given every
    /// log the replicator spools, and every flush from now, its commits counted on both meanwhile
    /// (the joint rule); swapped into the head once f of its members hold all the guest was told
    /// replicated, or, for f = 0, once the bucket does; then counted alone. The new configuration,
    /// or none if none was to be had, or it took not what it needed within [`SEEDING`], given up,
    /// those of its members that refused the writer added to `avoid`. Fails if the head's swap
    /// does: the writer stops, the replacement's joint rule kept, as the head may name either.
    async fn replace(
        &self,
        writer: &Arc<Writer<B>>,
        replicator: &Arc<crate::replicate::Replicator>,
        (from, next, f): (&bucket_cell::layout::Configuration, &mut u64, u8),
        avoid: &mut Vec<String>,
    ) -> Result<Option<bucket_cell::layout::Configuration>, Failure> {
        let members = self.members()?;
        let cell = writer.cell();
        let number = *next;
        let picked = async {
            let me = members.me()?;
            let others = match f {
                0 => Vec::new(),
                f => members.pick(cell, number, f, &avoid[..]).await?,
            };
            Ok::<_, String>((me, others))
        };
        let picked = picked.await;
        let (me, others) = match picked {
            Ok(picked) => picked,
            Err(err) => {
                log::warn!(
                    "cladefs: {cell:?}: no configuration {number} of f {f} to be had: {err}"
                );
                return Ok(None);
            }
        };
        let mut all = vec![me];
        all.extend(others.iter().cloned());
        let configuration =
            bucket_cell::layout::Configuration { number, epoch: writer.epoch(), f, members: all };
        let links = others.iter().map(|member| members.link(cell, &configuration, member));
        replicator.replacing(usize::from(f), links.collect());
        *next = number + 1;
        let started = std::time::Instant::now();
        let mut waits = 0u32;
        while !replicator.installed(self.last_uploaded()) {
            if self.stopping() || replicator.fenced().is_some() || started.elapsed() > SEEDING {
                let refusing = replicator.abandon().into_iter();
                avoid.extend(refusing.filter_map(|m| Some(others.get(m)?.address.clone())));
                log::warn!(
                    "cladefs: {cell:?}: configuration {number} of f {f} given up: its members \
                     took not what the guest was told replicated within {SEEDING:?}: {:?}",
                    replicator.down()
                );
                return Ok(None);
            }
            // Of f = 0, what was told replicated uploaded at once.
            // cladefs: in a simulation, at any wait (doc-24).
            if f == 0 && (waits.is_multiple_of(URGE_EVERY) || bucket_cell::buggify!()) {
                self.eager();
            }
            waits += 1;
            (self.clock)(Duration::from_millis(10)).await;
        }
        writer.configure(configuration.clone()).await?;
        replicator.swapped();
        self.configured(number);
        log::info!(
            "cladefs: {cell:?} replicated in configuration {number}, of f {f}, with {:?}, \
             replacing {}",
            configuration.members.iter().map(|member| &member.address).collect::<Vec<_>>(),
            from.number
        );
        self.drop_before(cell, from, &configuration).await;
        Ok(Some(configuration))
    }

    /// cladefs: the host's members, which a replicated cell needs.
    fn members(&self) -> Result<Arc<dyn Members>, Failure> {
        let members = self.options.members.clone().ok_or_else(|| {
            Failure::msg(
                "a replicated cell needs the daemon's peers and replica directory \
                 (CLADEFSD_PEER_ADDRESS, CLADEFSD_PEERS, CLADEFSD_REPLICAS)",
            )
        })?;
        Ok(members.0)
    }

    /// cladefs: fences `cell` at `epoch` on the members of `old`, the configuration its head named
    /// as it was taken (doc-21, TASK-537.3.4): f+1 members other than its writer must answer, so
    /// that no ack quorum of `old` is left unfenced, the writer's own copy, which nothing fences,
    /// counting only if this host is the writer and holds it (`own`: its cell noted as the
    /// writer's copy of `old`, TASK-683). Each
    /// that answered, with the ends of the logs it holds. `forced` (TASK-681), fewer may: why they
    /// are too few, then.
    async fn fence(
        &self,
        cell: CellId,
        old: &bucket_cell::layout::Configuration,
        epoch: u64,
        own: bool,
        forced: bool,
    ) -> Result<Fenced, Failure> {
        let members = self.members()?;
        let me = members.me().map_err(Failure::msg)?;
        let mine = own && old.members.first() == Some(&me);
        let needed = usize::from(old.f) + 1 - usize::from(mine);
        let mut fenced = Vec::new();
        let mut refused = Vec::new();
        for member in old.members.iter().skip(1) {
            match members.fence(cell, old, member, epoch).await {
                Ok(ends) => fenced.push((member.clone(), ends)),
                Err(err) => refused.push(format!("{}: {err}", member.address)),
            }
        }
        if fenced.len() < needed {
            let short = format!(
                "{} of the {needed} members of configuration {} a take must fence answered: {}",
                fenced.len(),
                old.number,
                refused.join("; ")
            );
            return match forced {
                true => Ok(Fenced { members: fenced, short: Some(short) }),
                false => Err(Failure::msg(short)),
            };
        }
        Ok(Fenced { members: fenced, short: None })
    }

    /// cladefs: of each log the members `fenced` hold of `old`, the longest copy, read from the
    /// member holding it: every commit a guest was told replicated is on one of them, and each
    /// member's logs are prefixes of the writer's (doc-21, TASK-537.3.4). Of a segment's records,
    /// the longest copy `keys`, the cell's, find authentic, cut to what of it is whole: what a
    /// member answers is unauthenticated, and a longer copy garbled, or forged, would otherwise
    /// hide an honest one (TASK-684). Only a copy found not authentic is passed over: one that
    /// can't be read, or reads shorter than its member's fence said, fails the take, as it may be
    /// the only one holding commits told replicated; and so does a log none of whose copies is
    /// authentic, as a log missing would look like a writer's base, given up
    /// (`replicate::trimmed`).
    async fn read_longest(
        &self,
        cell: CellId,
        old: &bucket_cell::layout::Configuration,
        fenced: &[(bucket_cell::layout::Member, Vec<(u64, u64)>)],
        keys: &Arc<Ring>,
    ) -> Result<Vec<(u64, Vec<u8>)>, Failure> {
        // Each log's copies, by their members' fences' ends, the longest first.
        let mut copies: BTreeMap<u64, Vec<(u64, bucket_cell::layout::Member)>> = BTreeMap::new();
        for (member, ends) in fenced {
            for &(log, end) in ends {
                copies.entry(log).or_default().push((end, member.clone()));
            }
        }
        for held in copies.values_mut() {
            held.sort_by_key(|copy| std::cmp::Reverse(copy.0));
        }
        let members = self.members()?;
        let mut logs = Vec::with_capacity(copies.len());
        {
            for (log, held) in copies {
                let segment = crate::layout::SegmentId { cell: cell.id, number: log >> 1 };
                let (mut best, mut refused) = (None::<Vec<u8>>, Vec::new());
                for (end, member) in held {
                    if best.as_ref().is_some_and(|best| best.len() as u64 >= end) {
                        break;
                    }
                    let bytes = members.read(cell, old, &member, log).await;
                    let mut bytes = bytes.map_err(Failure::msg)?;
                    if (bytes.len() as u64) < end {
                        return Err(Failure::msg(format!(
                            "{} gave {} bytes of log {log}, fenced at {end}",
                            member.address,
                            bytes.len()
                        )));
                    }
                    // A segment's records, as far as they are whole and authentic; its own
                    // extents' files, whose records name them, as they are.
                    if log & 1 == 0 {
                        match crate::wal::replica_whole(&bytes, segment, keys) {
                            Ok(whole) => bytes.truncate(whole),
                            Err(err) => {
                                log::warn!(
                                    "cladefs: {}'s log {log} of {cell:?}: {err:#}: passed over",
                                    member.address
                                );
                                refused.push(format!("{}: {err:#}", member.address));
                                continue;
                            }
                        }
                    }
                    if best.as_ref().is_none_or(|best| bytes.len() > best.len()) {
                        best = Some(bytes);
                    }
                }
                match best {
                    Some(bytes) => logs.push((log, bytes)),
                    None => {
                        return Err(Failure::msg(format!(
                            "no member's copy of log {log} of {cell:?} is authentic: {}",
                            refused.join("; ")
                        )));
                    }
                }
            }
        }
        Ok(logs)
    }

    /// `writer` took the cell, with the fencing token `token`: the cell names what it reads in
    /// the bucket where the writer's checkpoints move it, as the writer does (TASK-195); the
    /// cell's commits are uploaded, its forks made, and what the cell's write-ahead log holds
    /// after them is the uploader's to upload.
    fn took(&self, writer: &Arc<Writer<B>>, token: u64) {
        let following = Arc::downgrade(self.cell());
        writer.following(move |moved| {
            let Some(cell) = following.upgrade() else { return };
            if let Err(err) = cell.moved(moved) {
                log::warn!("cladefs: following the collector's moves: {err:#}");
            }
        });
        *self.writer.lock().unwrap() = Some(writer.clone());
        let replication = writer.taken().head.replication.as_ref().map(Replicated::of);
        let taking = Taking { token, epoch: writer.epoch(), taken: SystemTime::now(), replication };
        let mut progress = self.progress.lock().unwrap();
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
        let origin = Origin { parent: cell, at: version.at };
        let cell_keys = CellKeys::of(cell, &head, self.keys.kms.clone());
        let keys = keys::ring(&**bucket, &cell_keys, cell, &head, true).await?;
        let recovering = bucket_cell::recovery::recover_version(&**bucket, cell, version.at, &keys);
        let recovered = recovering.await?;
        // cladefs: its cell's open segment, which nothing writes, encrypted as every cell's is
        // (TASK-411.28); its forks' checkpoints sealed under its cell's.
        let sealing = keys.clone();
        let keys = keys.for_version(cell);
        let state = (recovered.layer.clone()).unwrap_or_else(|| Arc::new(recovered.state.clone()));
        let _ = self.at_version.set(AtVersion { origin, version: id, sealing, state });
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
        // what the closing closed durable too.
        cell.flush().await?;
        let mut closed = cell.closed(room).await?;
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
                true => drop(cell.close_for_upload(false).await?),
                false => closing = cell.close_for_upload(true).await?,
            }
            cell.flush().await?;
            closed.extend(cell.closed(room - closed.len()).await?);
        }
        let mut segments = Vec::with_capacity(closed.len());
        for closed in closed {
            let (number, tag) = (closed.number, closed.tag());
            // cladefs: what reads its files, on a blocking thread, as `blocking` gives (board
            // 2944).
            let own = blocking(|| {
                let own = closed.own.iter().map(|(key, file)| Ok((key.clone(), stored(file)?)));
                own.collect::<Result<_, Failure>>()
            })?;
            let footer = closed.footer.clone();
            let size = closed.file.size().map_err(|err| Failure::local(err, "a segment"))?;
            let closed = Arc::new(closed);
            let body = Body::new(size, move || blocking(|| closed.bytes()));
            let segment = writer.add_segment(Objects { footer, segment: body, own })?;
            segments.push(Uploading { segment, number, tag });
        }
        let took = !segments.is_empty();
        in_flight.upload(writer, segments);
        Ok(Gathered { took, closed: closing })
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
        let mut epoch = None;
        let stored = async {
            while let Some(segment) = in_flight.acknowledging.front() {
                self.cell().acknowledged(segment.number).await?;
                epoch = Some(segment.epoch());
                in_flight.acknowledging.pop_front();
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        match stored {
            Ok(()) => in_flight.acknowledging_failed = false,
            Err(err) if full(&err) => {
                if !std::mem::replace(&mut in_flight.acknowledging_failed, true) {
                    log::warn!(
                        "cladefs: cell {} records no acknowledgement for now, tried again \
                         each pass: {err:#}",
                        self.cell().description().cell
                    );
                }
            }
            Err(err) => return Err(err.into()),
        }
        let Some(epoch) = epoch else { return Ok(()) };
        failing!("upload::released");
        self.progress.lock().unwrap().uploaded_through(epoch);
        self.changed.notify(usize::MAX);
        Ok(())
    }
}

/// What this host learned of the cell `recovering` recovered, which the opening of its cell
/// takes from the bucket's [`Known`](bucket_cell::known::Known) as it begins, whatever comes of
/// it after: so nothing learned for an opening outlives it, a failed one's no later one's
/// (TASK-449's review). Only for a cell whose log goes on from nothing the bucket acknowledged:
/// one that has, learned nothing of here, its learning dropped.
fn learned<B: bucket_cell::Bucket, D: Dir>(
    bucket: &B,
    recovering: &Recovering<D>,
) -> Option<Learned> {
    let learned = bucket.known()?.take(recovering.cell());
    learned.filter(|_| recovering.start().is_none())
}

/// What an opening of the cell of `cell` reads first (TASK-165.11, TASK-449): its head, which
/// the taking goes on from and its recovery starts from; of a cell this host has just made,
/// `learned` ([`learned`]) stands for that read: its head as its fork left it, as a read made then,
/// the taking conditional on it. A fork of a version its host crashed in the middle of, its head
/// saying so, is resumed first ([`lifecycle::resume_version_fork`]: its reference written, the
/// fork made), which fails if the version was dropped: so the cell takes no state of a fork not
/// made.
async fn first_read<B: bucket_cell::Bucket>(
    bucket: &B,
    cell: CellId,
    learned: Option<Learned>,
    kms: &Arc<keys::CachingKms>,
) -> Result<head::Read, Failure> {
    let read = match learned {
        Some(Learned { head }) => head::Read::found(head),
        None => head::read(bucket, cell).await,
    };
    if !read.head(cell)?.is_some_and(|head| !head.deleted() && head.forking().is_some()) {
        return Ok(read);
    }
    // cladefs: a version's restore into another cell writes its checkpoint at the version under
    // the version's cell's keys, unwrapped by the host's KMS (TASK-476).
    let start = lifecycle::Start::Found(read.slots());
    match lifecycle::resume_version_fork(bucket, cell, start, keys::Parents::Kms(kms, None)).await?
    {
        Some(left) => Ok(head::Read::found(left)),
        None => Ok(head::read(bucket, cell).await),
    }
}

/// The bytes of `file`, an extent's of its own a segment of the write-ahead log names, read only
/// as its upload goes, and checked: a byte the host's disk damaged is not uploaded under the CRC
/// it fails (`read_object`, TASK-153.11).
fn stored<F: File>(file: &Arc<F>) -> Result<Body, Failure> {
    let size = file.size().map_err(|err| Failure::local(err, "a file to upload"))?;
    let file = file.clone();
    Ok(Body::new(size, move || blocking(|| read_object(&*file))))
}

/// What a gather did ([`Shared::gather`]).
struct Gathered {
    /// Whether it took segments the cell closed: the cell may have closed more meanwhile.
    took: bool,
    /// Whether it closed the cell's open segment, as asked: what was committed before is in the
    /// segments the cell closed, taken, or to be taken next.
    closed: bool,
}

/// A segment uploading: its number in the cell, and in the open cell; and the tag of the open
/// cell's commit it ends with.
struct Uploading {
    segment: u64,
    number: u64,
    tag: u64,
}

impl Uploading {
    /// The cell's epoch of the commit it ends with.
    fn epoch(&self) -> u64 {
        epoch_of(self.tag)
    }
}

/// The segments uploading, in order, and the futures of their uploads: an acknowledgement takes
/// exactly the segments the writer acknowledged. Each is in the cell's write-ahead log, durably,
/// before its upload goes (TASK-79.72's rule), so that nothing lands in the writer's name that the
/// log doesn't hold.
/// cladefs: the uploader's run's work with its cell's head, done.
enum HeadWork {
    /// An acknowledgement, to the segment it returns.
    Acknowledged(Option<u64>),
    Checkpointed,
}

/// cladefs: what the uploader's run waits on that happened first.
enum Happened {
    /// The next upload in order is done.
    Uploaded(Result<(), bucket_cell::Error>),
    /// The work with the cell's head under way is done.
    Headed(Result<HeadWork, bucket_cell::Error>),
    /// It was woken, or its interval ran out: whether it was woken.
    Woken(bool),
}

#[derive(Default)]
struct InFlight {
    segments: VecDeque<Uploading>,
    uploads: FuturesOrdered<BoxFuture<'static, Result<(), bucket_cell::Error>>>,
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
            self.uploads.push_back(async move { writer.upload(segment).await }.boxed());
            self.segments.push_back(uploading);
        }
    }

    /// The next upload done, in order.
    fn next(
        &mut self,
    ) -> impl Future<Output = Option<Result<(), bucket_cell::Error>>> + Unpin + '_ {
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

/// cladefs: whether `err` is a full disk's: ENOSPC, or a quota's EDQUOT, which room made later
/// lets pass (board 5359).
fn full(err: &anyhow::Error) -> bool {
    use std::io::ErrorKind::{QuotaExceeded, StorageFull};
    let mut io = err.chain().filter_map(|cause| cause.downcast_ref::<std::io::Error>());
    io.any(|io| matches!(io.kind(), StorageFull | QuotaExceeded))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Description, MemDir};
    use bucket_cell::fetch::Limits;
    use storage_device::ExtentId;

    const CELL: bucket_cell::layout::CellId = bucket_cell::layout::test_cell(7);

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

    /// TASK-364: an opening takes its cell, making it for a new cell, whose cell it is from then on
    /// (doc-20); its cell's head is taken with the cell's epoch. A later placement's taking of the
    /// cell, and of the cell's head with the epoch it took it at, fences the cell's writer: its
    /// next commit's upload fails, and a placement of a lower token takes the cell back no more.
    #[tokio::test(flavor = "multi_thread")]
    async fn open_cells_take_their_cells_and_are_fenced_with_them() {
        let host = Runtime {
            handle: tokio::runtime::Handle::current(),
            uploads: Budget::new(Limits::UNBOUNDED),
            keys: bucket_cell::keys::testing::host(),
        };
        let bucket =
            Arc::new(ObjectBucket::new(Arc::new(object_store::memory::InMemory::new()), ""));
        let dir = MemDir::new();
        let description = Description { block_size: 4096, capacity: 1 << 30, cell: CELL };
        let kind = bucket_cell::cell::Kind::Fxfs;
        LocalCell::make(&dir, description, Some("memory:///cells"), Some(kind)).unwrap();
        lifecycle::create(&*bucket, CELL, 1).await.unwrap();
        let options = Options { token: 5, ..Options::default() };
        let recovering = LocalCell::recover_unmade(dir).unwrap();
        let (cell, uploader) =
            open(recovering, crate::Options::default(), bucket.clone(), (1, options), &host)
                .await
                .unwrap();
        cell.made().unwrap();
        let head = bucket_cell::head::get(&*bucket, CELL).await.unwrap().unwrap();
        assert_eq!((head.epoch, head.holder, head.token), (1, 1, 5));
        assert_eq!(head.kind, Some(bucket_cell::cell::Kind::Fxfs));

        cell.write(ExtentId::new(CELL.id, 1), 0, &[1; 4096]).await.unwrap();
        cell.commit(None).await.unwrap();
        uploader.uploaded(cell.last_commit().unwrap()).await.unwrap();
        // Another placement takes the cell, fencing it (TASK-371).
        let taken = bucket_cell::testing::take(&*bucket, CELL, 0x66, 6).await;
        assert_eq!(taken.map(|held| held.head.epoch), Ok(2));
        cell.write(ExtentId::new(CELL.id, 2), 0, &[2; 4096]).await.unwrap();
        cell.commit(None).await.unwrap();
        let fenced = uploader.uploaded(cell.last_commit().unwrap()).await;
        assert!(matches!(fenced, Err(Failure::Bucket(bucket_cell::Error::Fenced))), "{fenced:?}");
        let back = bucket_cell::testing::take(&*bucket, CELL, 0x55, 5).await;
        assert_eq!(back.map(|held| held.head), Err(bucket_cell::Error::Stale));
    }

    /// TASK-539.9: a cell's disk under pressure urges its uploader, which closes the open segment
    /// its guest is filling and uploads it at once, rather than wait out its interval, and
    /// `filling_wait` past it, so that what it held leaves the disk (doc-22).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_disk_under_pressure_has_its_cells_uploaded_at_once() {
        let host = Runtime {
            handle: tokio::runtime::Handle::current(),
            uploads: Budget::new(Limits::UNBOUNDED),
            keys: bucket_cell::keys::testing::host(),
        };
        let bucket =
            Arc::new(ObjectBucket::new(Arc::new(object_store::memory::InMemory::new()), ""));
        let dir = MemDir::new();
        let disk = dir.disk().disk().clone();
        let description = Description { block_size: 4096, capacity: 1 << 30, cell: CELL };
        LocalCell::make(&dir, description, Some("memory:///urged"), None).unwrap();
        lifecycle::create(&*bucket, CELL, 1).await.unwrap();
        // Its uploader uploads nothing it is not asked to within the hour.
        let hour = Duration::from_secs(3600);
        let options = Options { interval: hour, filling_wait: hour, ..Options::default() };
        let recovering = LocalCell::recover_unmade(dir).unwrap();
        let (cell, uploader) =
            open(recovering, crate::Options::default(), bucket.clone(), (1, options), &host)
                .await
                .unwrap();
        cell.made().unwrap();
        cell.write(ExtentId::new(CELL.id, 1), 0, &[1; 4096]).await.unwrap();
        cell.commit(None).await.unwrap();
        let epoch = cell.last_commit().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(uploader.last_uploaded() < epoch, "uploaded unasked");
        disk.urge();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while uploader.last_uploaded() < epoch {
            assert!(std::time::Instant::now() < deadline, "not uploaded as its disk urged it");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(cell.closed(usize::MAX).await.unwrap().len(), 0, "its segments left unuploaded");
    }

    /// TASK-539.12: the cell's disk refusing the record of an acknowledgement, as a full one does,
    /// fails neither the cell nor its uploader: the record before it whole, the cell writes and
    /// commits on, its segments wait in it, and the uploader records them once it can, which lets
    /// them go. Before, the cell failed for good.
    #[tokio::test(flavor = "multi_thread")]
    async fn acknowledgements_a_full_disk_refuses_wait_for_room() {
        let scenario = fail::FailScenario::setup();
        let host = Runtime {
            handle: tokio::runtime::Handle::current(),
            uploads: Budget::new(Limits::UNBOUNDED),
            keys: bucket_cell::keys::testing::host(),
        };
        let bucket =
            Arc::new(ObjectBucket::new(Arc::new(object_store::memory::InMemory::new()), ""));
        let dir = MemDir::new();
        let description = Description { block_size: 4096, capacity: 1 << 30, cell: CELL };
        LocalCell::make(&dir, description, Some("memory:///full"), None).unwrap();
        lifecycle::create(&*bucket, CELL, 1).await.unwrap();
        let options = Options { interval: Duration::from_millis(20), ..Options::default() };
        let recovering = LocalCell::recover_unmade(dir).unwrap();
        let (cell, uploader) =
            open(recovering, crate::Options::default(), bucket.clone(), (1, options), &host)
                .await
                .unwrap();
        cell.made().unwrap();
        fail::cfg("wal::start", "return").unwrap();
        cell.write(ExtentId::new(CELL.id, 1), 0, &[1; 4096]).await.unwrap();
        cell.commit(None).await.unwrap();
        let epoch = cell.last_commit().unwrap();
        // Its interval's passes, 25 of them, each trying again.
        std::thread::sleep(Duration::from_millis(500));
        assert!(uploader.last_uploaded() < epoch, "counted uploaded, unrecorded");
        cell.write(ExtentId::new(CELL.id, 2), 0, &[2; 4096]).await.unwrap();
        cell.commit(None).await.unwrap();
        fail::remove("wal::start");
        uploader.uploaded(cell.last_commit().unwrap()).await.unwrap();
        assert_eq!(cell.closed(usize::MAX).await.unwrap().len(), 0, "its segments left in it");
        scenario.teardown();
    }

    /// board 5359: the cell's disk failing the record of an acknowledgement otherwise than a full
    /// disk does (EIO) fails the uploader with it, and a wait on the upload returns that failure:
    /// before, the record was tried again for ever, as a full disk's is, and the wait never ended.
    #[tokio::test(flavor = "multi_thread")]
    async fn acknowledgements_a_failing_disk_refuses_fail_the_uploader() {
        let scenario = fail::FailScenario::setup();
        let host = Runtime {
            handle: tokio::runtime::Handle::current(),
            uploads: Budget::new(Limits::UNBOUNDED),
            keys: bucket_cell::keys::testing::host(),
        };
        let bucket =
            Arc::new(ObjectBucket::new(Arc::new(object_store::memory::InMemory::new()), ""));
        let dir = MemDir::new();
        let description = Description { block_size: 4096, capacity: 1 << 30, cell: CELL };
        LocalCell::make(&dir, description, Some("memory:///failing"), None).unwrap();
        lifecycle::create(&*bucket, CELL, 1).await.unwrap();
        let options = Options { interval: Duration::from_millis(20), ..Options::default() };
        let recovering = LocalCell::recover_unmade(dir).unwrap();
        let (cell, uploader) =
            open(recovering, crate::Options::default(), bucket.clone(), (1, options), &host)
                .await
                .unwrap();
        cell.made().unwrap();
        fail::cfg("wal::start", "return(eio)").unwrap();
        cell.write(ExtentId::new(CELL.id, 1), 0, &[1; 4096]).await.unwrap();
        cell.commit(None).await.unwrap();
        let waited = uploader.uploaded(cell.last_commit().unwrap()).await;
        let failure = format!("{:#}", waited.unwrap_err());
        assert!(failure.contains("writing the WAL's start"), "{failure}");
        assert_eq!(uploader.failed().map(|failed| format!("{failed:#}")), Some(failure));
        fail::remove("wal::start");
        scenario.teardown();
    }

    /// TASK-690: a cell's uploader swaps its cell's head no faster than a bucket of GCS's object
    /// store takes it, however fast the cell commits: a guest waiting for each commit's upload,
    /// then one closing segment after segment. Each swap names all uploaded since the last, the
    /// uploads going on as it waits; the cell refuses no write of the head. TASK-691: each slot of
    /// the head a second apart, the head many times a second.
    #[tokio::test(flavor = "multi_thread")]
    async fn heads_are_swapped_as_fast_as_gcs_takes_them() {
        use bucket_cell::faults::{Fault, Faults, Faulty};
        use bucket_cell::object_bucket::Request;
        let host = Runtime {
            handle: tokio::runtime::Handle::current(),
            uploads: Budget::new(Limits::UNBOUNDED),
            keys: bucket_cell::keys::testing::host(),
        };
        let second = Duration::from_secs(1);
        let faults = Faults::new(Duration::ZERO, None);
        faults.set(Fault { rewrite_every: second, ..Fault::default() });
        let gcs = Faulty::new(Arc::new(object_store::memory::InMemory::new()), faults);
        let bucket = Arc::new(ObjectBucket::new(Arc::new(gcs), "").rewriting_every(second));
        let dir = MemDir::new();
        let description = Description { block_size: 4096, capacity: 1 << 30, cell: CELL };
        LocalCell::make(&dir, description, Some("memory:///gcs"), None).unwrap();
        lifecycle::create(&*bucket, CELL, 1).await.unwrap();
        let recovering = LocalCell::recover_unmade(dir).unwrap();
        let (cell, uploader) = open(
            recovering,
            crate::Options::default(),
            bucket.clone(),
            (1, Options::default()),
            &host,
        )
        .await
        .unwrap();
        cell.made().unwrap();
        // Its updates from now on: the head's swaps, the bucket's probe and the taking done.
        let updates = || bucket.stats().requests(Request::Update);
        let opened = updates();
        let started = std::time::Instant::now();
        let mut sequence = 0;
        let mut write = async |close: bool| {
            sequence += 1;
            cell.write(ExtentId::new(CELL.id, sequence), 0, &[sequence as u8; 4096]).await.unwrap();
            if close {
                cell.close_segment().unwrap();
            }
            cell.commit(None).await.unwrap();
            sequence
        };
        let mut waits = Vec::new();
        while started.elapsed() < 3 * second {
            write(false).await;
            let waiting = std::time::Instant::now();
            uploader.uploaded(cell.last_commit().unwrap()).await.unwrap();
            waits.push(waiting.elapsed());
        }
        for _ in 0..3 * IN_FLIGHT {
            write(true).await;
        }
        let last = write(false).await;
        uploader.uploaded(cell.last_commit().unwrap()).await.unwrap();
        let elapsed = started.elapsed();
        let stats = bucket.stats();
        assert_eq!(stats.failures(Request::Update), 0, "writes of the head refused");
        // Each slot's a second after its last, of HEAD_SLOTS; more than one a second in all.
        let swaps = updates() - opened;
        let most = bucket_cell::identity::DEFAULT_HEAD_SLOTS * (elapsed.as_secs() + 1);
        assert!(swaps <= most, "{swaps} swaps of the head in {elapsed:?}");
        assert!(swaps > 2 * (elapsed.as_secs() + 1), "{swaps} swaps of the head in {elapsed:?}");
        // A commit waits for its swap's turn: a part of a second, as long as the guest syncs no
        // faster than HEAD_SLOTS a second; up to a second for one past that, as this guest is.
        let longest = waits.iter().max().unwrap();
        assert!(*longest < second + second / 2, "a commit waited {longest:?}: {waits:?}");
        drop(uploader);
        let state = bucket_cell::recover(&*bucket, CELL, None).await.unwrap().state;
        let sequences: Vec<u64> = state.extents.keys().map(|extent| extent.sequence).collect();
        assert_eq!(sequences, (1..=last).collect::<Vec<_>>());
    }

    /// TASK-79.19: a gather takes no more of the segments the cell closed than may upload at once,
    /// with those uploading; the rest it leaves in the cell, until those it took are uploaded.
    #[tokio::test(flavor = "multi_thread")]
    async fn gathers_take_what_may_upload_at_once() {
        let host = Runtime {
            handle: tokio::runtime::Handle::current(),
            uploads: Budget::new(Limits::UNBOUNDED),
            keys: bucket_cell::keys::testing::host(),
        };
        let bucket =
            Arc::new(ObjectBucket::new(Arc::new(object_store::memory::InMemory::new()), ""));
        let dir = MemDir::new();
        let description = Description { block_size: 4096, capacity: 1 << 30, cell: CELL };
        LocalCell::make(&dir, description, Some("memory:///gathers"), None).unwrap();
        lifecycle::create(&*bucket, CELL, 1).await.unwrap();
        // Its uploader uploads nothing it is not asked to within the hour: the test gathers.
        let options = Options { interval: Duration::from_secs(3600), ..Options::default() };
        let recovering = LocalCell::recover_unmade(dir).unwrap();
        let (cell, uploader) =
            open(recovering, crate::Options::default(), bucket.clone(), (1, options), &host)
                .await
                .unwrap();
        cell.made().unwrap();
        // Each commit closes its segment.
        let closed = 2 * IN_FLIGHT + 1;
        for sequence in 1..=closed as u64 {
            cell.write(ExtentId::new(CELL.id, sequence), 0, &[sequence as u8; 4096]).await.unwrap();
            cell.close_segment().unwrap();
            cell.commit(None).await.unwrap();
        }
        let shared = &uploader.shared;
        let writer = shared.writer.lock().unwrap().clone().unwrap();
        let mut in_flight = InFlight::default();
        let mut taken = Vec::new();
        while shared.gather(&writer, &mut in_flight, (true, true)).await.unwrap().took {
            taken.push(in_flight.len());
            (async {
                while let Some(uploaded) = in_flight.next().await {
                    uploaded.unwrap();
                }
                let through = writer.acknowledge().await.unwrap();
                shared.acknowledged(through, &mut in_flight).await.unwrap();
            })
            .await;
        }
        assert_eq!(taken, [IN_FLIGHT, IN_FLIGHT, 1]);
        assert_eq!(uploader.last_uploaded(), cell.last_commit().unwrap());
        drop(uploader);
        let state = bucket_cell::recover(&*bucket, CELL, None).await.unwrap().state;
        let sequences: Vec<u64> = state.extents.keys().map(|extent| extent.sequence).collect();
        assert_eq!(sequences, (1..=closed as u64).collect::<Vec<_>>());
    }
}

/// cladefs: a cell committing, sealing and forking, its uploader uploading what it commits, and
/// passes of the collector running over the bucket all along (TASK-291.1), under Shuttle, whose
/// scheduler chooses every interleaving of their threads at each of the cell's locks and at each
/// request to the bucket, and replays a failing one from its seed.
/// `cargo nextest run -p local_cell --features shuttle --lib shuttle_tests::`.
#[cfg(all(test, feature = "shuttle"))]
mod shuttle_tests {
    use super::*;
    use crate::{Description, MemDir};
    use bucket_cell::bucket::{ByteRange, Error as BucketError, Meta, Object, PutMode, Version};
    use bucket_cell::collector;
    use bucket_cell::fetch::Limits;
    use bucket_cell::testing::{Direct, SimBucket};
    use shuttle::scheduler::{PctScheduler, RandomScheduler, Scheduler};
    use std::collections::BTreeMap;
    use std::future::Future;
    use std::sync::atomic::Ordering::SeqCst;
    use storage_device::ExtentId;

    const BLOCK: usize = 4096;
    const CELL: bucket_cell::layout::CellId = bucket_cell::layout::test_cell(7);
    /// The cell the writer forks.

    /// Small segments, so that commits close several, and extents of two blocks or more in
    /// objects of their own, which upload before the segments naming them (TASK-79.3); flushes of
    /// two blocks at most held in their log records, larger ones sealed (TASK-301).
    const OPTIONS: crate::Options = crate::Options {
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
    /// uploader and the cell interleave at each. Its requests take effect at once, and never fail.
    /// Its gate, once armed, holds up the next listing of the whole bucket, a pass's of its
    /// objects.
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
            let whole = bucket_cell::layout::cell_of_prefix(prefix).is_some();
            if whole && self.armed.swap(false, SeqCst) {
                self.reached.store(true, SeqCst);
                until(|| self.open.load(SeqCst));
            }
        }
    }

    impl bucket_cell::Bucket for Turns {
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
            page: bucket_cell::bucket::Page<'_>,
        ) -> impl Future<Output = Result<bucket_cell::bucket::Listing, BucketError>> + Send
        {
            self.direct.list_delimited(prefix, page)
        }

        fn put(
            &self,
            key: &str,
            bytes: bucket_cell::Bytes,
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
    fn received<T>(receiver: &mut futures::channel::oneshot::Receiver<T>) -> T {
        loop {
            match receiver.try_recv() {
                Ok(Some(received)) => return received,
                Ok(None) => shuttle::thread::yield_now(),
                Err(err) => panic!("{err}"),
            }
        }
    }

    /// What the cell `cell`, recovered from `bucket`, holds, if not `holds`, extent by extent.
    fn differs(
        bucket: &Arc<Turns>,
        cell: CellId,
        holds: &BTreeMap<ExtentId, Vec<u8>>,
    ) -> Result<(), String> {
        let failed = |err: bucket_cell::Error| format!("cell {cell}: {err}");
        let record = shuttle::future::block_on(bucket_cell::head::get(&**bucket, cell));
        let record = record.map_err(failed)?.ok_or(format!("cell {cell}: no head"))?;
        let recovering = bucket_cell::recover(&**bucket, cell, record.origin());
        let state = shuttle::future::block_on(recovering).map_err(failed)?.state;
        let held: Vec<&ExtentId> = state.extents.keys().collect();
        if held != holds.keys().collect::<Vec<_>>() {
            return Err(format!("cell {cell} holds {held:?}, not {:?}", holds.keys()));
        }
        for (extent, location) in &state.extents {
            let read =
                shuttle::future::block_on(bucket_cell::testing::read(bucket, *extent, location));
            if read.as_ref() != Ok(&holds[extent]) {
                return Err(format!("cell {cell}: {extent:?} reads {read:?}"));
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

    /// A cell of `cell` on a disk of its own, made empty, and its uploader, running on a thread
    /// of its own as `holder`, whose writer has taken the cell.
    struct Session {
        uploader: Arc<Shared<MemDir, Turns>>,
        running: shuttle::thread::JoinHandle<()>,
        open_cell: Arc<LocalCell<MemDir>>,
        cell: CellId,
        /// What the cell's extents hold, by their IDs.
        holds: BTreeMap<ExtentId, Vec<u8>>,
        /// The first sequence number of its own extents'.
        next: u64,
    }

    impl Session {
        fn open(bucket: &Arc<Turns>, cell: CellId, holder: u64) -> Self {
            let dir = MemDir::new();
            let description = Description { block_size: BLOCK as u32, capacity: 1 << 30, cell };
            LocalCell::make(&dir, description, None, None).unwrap();
            shuttle::future::block_on(lifecycle::create(&**bucket, cell, holder)).unwrap();
            let recovering = LocalCell::recover_unmade(dir).unwrap();
            // Its interval never runs out: the writer asks for what it commits.
            let never: Clock = Arc::new(|_| futures::future::pending().boxed());
            let spawn: Spawn = Arc::new(|task| drop(shuttle::future::spawn(task)));
            let options = Options { token: holder, ..Options::default() };
            let uploader = Shared::new(
                options,
                Budget::new(Limits::UNBOUNDED),
                (never, spawn),
                bucket_cell::keys::testing::host(),
            );
            let (started, mut start) = futures::channel::oneshot::channel();
            let running = {
                let (uploader, bucket) = (uploader.clone(), bucket.clone());
                shuttle::thread::spawn(move || {
                    let recovering = (recovering, None);
                    let running = uploader.run(recovering, OPTIONS, bucket, holder, started);
                    shuttle::future::block_on(running)
                })
            };
            let open_cell = received(&mut start).unwrap();
            open_cell.made().unwrap();
            let next = uploader.next_sequence();
            Self { uploader, running, open_cell, cell, holds: BTreeMap::new(), next }
        }

        fn extent(&self, at: u64) -> ExtentId {
            ExtentId::new(self.cell.id, self.next + at)
        }

        fn write(&mut self, at: u64, blocks: usize) {
            let bytes = pattern(self.cell.id ^ at, blocks);
            shuttle::future::block_on(self.open_cell.write(self.extent(at), 0, &bytes)).unwrap();
            self.holds.insert(self.extent(at), bytes);
        }

        fn free(&mut self, extent: ExtentId) {
            self.open_cell.free(extent).unwrap();
            self.holds.remove(&extent);
        }

        /// Asks the uploader for what the cell committed.
        fn ask(&self) {
            self.uploader.ask(self.open_cell.last_commit().unwrap(), Awaited::Uploaded).unwrap();
        }

        /// The uploader uploads all the cell made durable, and stops; and the cell closes.
        fn end(self) -> Result<BTreeMap<ExtentId, Vec<u8>>, String> {
            let last = self.open_cell.last_commit().unwrap();
            self.uploader.stop();
            self.running.join().unwrap();
            drop(self.open_cell);
            let (failed, uploaded) = (self.uploader.failed(), self.uploader.last_uploaded());
            if failed != Some(Failure::msg("the uploader stopped")) || uploaded != last {
                return Err(format!("the uploader: {failed:?}, through {uploaded} of {last}"));
            }
            Ok(self.holds)
        }
    }

    /// One schedule: the writer writes, frees and commits, asking for uploads; its uploader
    /// uploads; passes of the collector run all along. Then the uploader uploads what is left and
    /// stops, a last pass runs alone, and the cell, recovered from the bucket, holds what its
    /// cell's last commit left, each extent reading as it was written. What went wrong, if anything
    /// did.
    fn collected_while_committing() -> Result<(), String> {
        let bucket = Turns::new();
        let mut parent = Session::open(&bucket, CELL, 1);
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
        shuttle::future::block_on(parent.open_cell.commit_ordered(None)).unwrap();
        // Asked for that commit with an extent of its own open, the uploader closes the open
        // segment between commits: its footer's next sequence number counts the extent, which the
        // next segment names, its object uploaded first (TASK-79.3).
        parent.write(1, 3);
        parent.ask();
        parent.write(2, 2);
        parent.free(parent.extent(0));
        shuttle::future::block_on(parent.open_cell.commit(None)).unwrap();
        parent.ask();
        parent.write(3, 1);
        shuttle::future::block_on(parent.open_cell.commit(None)).unwrap();
        parent.ask();
        parent.free(parent.extent(1));
        parent.write(4, 2);
        shuttle::future::block_on(parent.open_cell.commit_ordered(None)).unwrap();
        parent.ask();
        parent.free(parent.extent(3));
        shuttle::future::block_on(parent.open_cell.commit(None)).unwrap();
        parent.ask();
        let parent = parent.end()?;
        writing.store(false, SeqCst);
        collecting.join().unwrap()?;
        let pass = collector::collect(&*bucket, 200, &COLLECTING);
        shuttle::future::block_on(pass).map_err(|err| format!("the last pass: {err}"))?;
        differs(&bucket, CELL, &parent)
    }

    /// Runs `schedule` under `scheduler`'s interleavings.
    fn explore(scheduler: impl Scheduler + 'static, schedule: impl Fn() + Send + Sync + 'static) {
        let mut config = shuttle::Config::new();
        config.stack_size = 4 << 20;
        shuttle::Runner::new(scheduler, config).run(schedule);
    }

    /// TASK-291.1: whatever the interleaving, what the cell committed and its uploader
    /// acknowledged stays its cell's, the collector running all along. [`listed_while_committing`]
    /// holds a pass up as it lists.
    #[test]
    fn collection_keeps_what_a_cell_commits_meanwhile() {
        let schedule = || collected_while_committing().unwrap();
        explore(RandomScheduler::new(runs()), schedule);
        explore(PctScheduler::new(3, runs()), schedule);
    }

    /// One schedule: a pass of the collector, held up as it lists the cell's objects, while the
    /// cell commits and its uploader uploads its segment and acknowledges it. Then the pass goes
    /// on, the uploader uploads what is left and stops, a last pass runs alone, and the cell,
    /// recovered from the bucket, holds what its cell's last commit left. What went wrong, if
    /// anything did.
    fn listed_while_committing() -> Result<(), String> {
        let bucket = Turns::new();
        let mut parent = Session::open(&bucket, CELL, 1);
        parent.write(0, 1);
        parent.write(1, 3);
        shuttle::future::block_on(parent.open_cell.commit_ordered(None)).unwrap();
        parent.ask();
        let before = parent.open_cell.last_commit().unwrap();
        until(|| parent.uploader.last_uploaded() >= before);
        bucket.gate.arm();
        let passing = {
            let bucket = bucket.clone();
            shuttle::thread::spawn(move || {
                let pass = collector::collect(&*bucket, 100, &COLLECTING);
                shuttle::future::block_on(pass).map_err(|err| format!("the pass: {err}"))
            })
        };
        bucket.gate.reached();
        parent.write(2, 1);
        parent.free(parent.extent(0));
        shuttle::future::block_on(parent.open_cell.commit_ordered(None)).unwrap();
        parent.ask();
        let committed = parent.open_cell.last_commit().unwrap();
        until(|| parent.uploader.last_uploaded() >= committed);
        bucket.gate.open();
        passing.join().unwrap()?;
        let parent = parent.end()?;
        let pass = collector::collect(&*bucket, 200, &COLLECTING);
        shuttle::future::block_on(pass).map_err(|err| format!("the last pass: {err}"))?;
        differs(&bucket, CELL, &parent)
    }

    /// A pass listing the cell's objects before it reads its head keeps what the writer uploads
    /// and acknowledges after the listing began: none of it is a candidate.
    #[test]
    fn a_pass_keeps_what_a_writer_uploads_as_it_lists() {
        explore(RandomScheduler::new(runs() / 10), || listed_while_committing().unwrap());
    }
}
