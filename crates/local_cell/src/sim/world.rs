// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The simulated world: hosts keeping cells for their guests, each a [`LocalCell`] over a
//! directory of the host's disk in memory, which does its I/O in the order its calls make
//! ([`MemDir::serial`]), and its uploader, `upload`'s, uploading its write-ahead log to the bucket;
//! forks, takeovers, deletions and collectors; driven by a seeded scheduler that also decides when,
//! in what order and how each request to the bucket completes, and when each uploader's interval
//! runs out. After every step it checks every live cell against what its guests did and were
//! told (TASK-88: the segments, their footers and the logs are the cell's, as a host's are).
//!
//! cladefs: replicated cells (doc-21, TASK-680): each host keeps a replica directory, and the
//! network between hosts delays, duplicates, drops and partitions, one way or both, the writers'
//! appends and the takes' fences, reads and drops, as the seed says; each replicator's members are
//! sent what they lack a turn at a time, as the scheduler says ([`Replicator::drive`]); a crash
//! keeps a replica directory, corrupting what it never synced, or loses it, its incarnation with
//! it. Every commit a guest was told is replicated must be in the history of its cell's writer
//! once one serves, as every commit told uploaded must be in the bucket's.
//!
//! cladefs: fast WAL cells (doc-29, board 14433): each host appends its fast WAL cells' commits to
//! its zone's appendable bucket, a region of the one bucket the seed orders every request of, which
//! fails appends before or after they land, refuses past its limit of appends to an object, and
//! goes down at times; takes, on a host of either zone, fence and recover the fast WAL the head
//! names. Every commit a fast WAL told its guest is durable must be in the history of its cell's
//! writer once one serves, its host crashed, its disk lost, before it was uploaded or not.

use crate::fast_wal::{FastWalConfig, FastWalOpener};
use crate::upload::{self, Awaited, Clock, Failure, Shared, Spawn};
use crate::{Description, LocalCell, MemDir, Place, Recovering};
use base32_id::Short;
use bucket_cell::any_bucket::AnyBucket;
use bucket_cell::collector;
use bucket_cell::fetch::{Budget, Limits};
use bucket_cell::layout::{self, Origin, epoch_of, writer_of};
use bucket_cell::testing::{Direct, Fault, SimBucket, key_of};
use bucket_cell::{Error, Recovered, lifecycle, recovery};
use extent_frame::spent::Spent;
use futures::channel::oneshot;
use futures::executor::{LocalPool, LocalSpawner, block_on};
use futures::future::{AbortHandle, BoxFuture, FutureExt as _, abortable};
use futures::task::LocalSpawnExt as _;
use rand::rngs::StdRng;
use rand::{RngExt as _, SeedableRng as _};
use std::cell::RefCell;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use storage_device::ExtentId;

/// How likely each action is, per step.
#[derive(Clone, Copy, Debug)]
pub struct Weights {
    /// A request to the bucket completes.
    pub complete: u32,
    /// A guest writes an extent, which its next commit seals.
    pub write: u32,
    pub commit: u32,
    /// A cell's uploader is asked for its commits, as a commit waiting for its upload asks, or
    /// its interval runs out: either closes the cell's open segment.
    pub close: u32,
    pub checkpoint: u32,
    /// A writer takes a cell that has none.
    pub open: u32,
    /// A writer takes a cell from the one it has.
    pub takeover: u32,
    /// A host crashes, its disk keeping what was durable, or lost, its cells with it.
    pub crash: u32,
    pub delete: u32,
    /// A cell a fork of which lives is deleted: a base whose forks live on (TASK-39.4).
    pub delete_base: u32,
    pub collect: u32,
    /// A collector stops midway.
    pub stop_collector: u32,
    /// What a crash left of a fork of a version, or of a failed one's deletion, is gone on with:
    /// finished, deleted as its host's cleanup deletes it, or forgotten (TASK-79.82).
    pub unfork: u32,
    /// Two hosts restore versions to one child within a round trip, as guests may name any ID:
    /// whichever's record comes first takes it, the other refused (TASK-173).
    pub twins: u32,
    /// cladefs: a cell's writer cuts a version, its cell quiescent (doc-18, TASK-476).
    pub version: u32,
    /// cladefs: a cell's writer drops one of its versions, uploaded or local.
    pub drop_version: u32,
    /// cladefs: a host restores a version a cell's head names, which may be dropped by now: a fork
    /// of it, its child's record alone, as many hosts do at once.
    pub restore_version: u32,
    /// The cell's clock advances.
    pub tick: u32,
    /// cladefs: a cell's writer runs a relocation round, as its uploader does at its checkpoint
    /// rounds, of an allowance of its own (doc-27, board 6214).
    pub relocate: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Profile {
    pub steps: usize,
    pub hosts: usize,
    /// The chances that a request fails before, or after, taking effect.
    pub fail_before: f64,
    pub fail_after: f64,
    /// The chance that a takeover holds the creates of segments of the writer it takes the cell
    /// from, each until an object at its key is deleted ([`SimBucket::hold`], TASK-79.18).
    pub hold: f64,
    /// Whether a host that crashes, its disk kept, starts again at once, taking back the cells
    /// it had, as cladefsd opens its cells at boot: while the creates its writers had in flight
    /// land.
    pub restarts: bool,
    /// cladefs: the chance that a collector's pass is over one cell alone, not a sweep over
    /// every cell (TASK-395.4): none draws nothing of the seed's.
    pub one_cell: f64,
    /// cladefs: the chance that a fork of a version a crash stopped is forgotten, not finished nor
    /// deleted: its host's directory, left unmade, names no child, and deleting it touches nothing
    /// in the bucket (TASK-395.10). None draws nothing of the seed's.
    pub forget: f64,
    /// cladefs: the cloud regions' buckets (TASK-633.10): none, one bucket, as before; else that
    /// many, host H's bucket `1 + H % regions`, each cell in the bucket of the host that made it,
    /// requests routed by their cells' buckets ([`SimBucket::at`]), a region's bucket down for a
    /// stretch at times. None draws nothing of the seed's.
    pub regions: u16,
    /// cladefs: the chance that a cell keeps its unuploaded commits in a fast WAL (doc-29, board
    /// 14433): none draws nothing of the seed's, and gives the hosts no fast buckets.
    pub fast_wal: f64,
    /// cladefs: whether a crash corrupts what a disk kept of the writes its files never synced
    /// ([`MemDir::crash_corrupting`]), at random.
    pub corrupting: bool,
    /// cladefs: whether its end deletes every cell, once checked, and sweeps until the collectors
    /// do nothing more, checking then that nothing is left ([`World::tear_down`]): the copies of
    /// mirrors, kept while their cells' children live, reached in every run (TASK-633.10).
    pub teardown: bool,
    pub weights: Weights,
    /// The most the bucket's clock advances a tick ([`Weights::tick`]).
    pub ticks: u64,
}

impl Weights {
    /// cladefs: each weight as `scale` says, given its name and weight: a swarm's draw of which of
    /// a profile's families a seed runs, and how often (doc-24).
    pub fn scale(&mut self, mut scale: impl FnMut(&'static str, u32) -> u32) {
        macro_rules! scaled {
            ($($field:ident),*) => {
                $(self.$field = scale(stringify!($field), self.$field);)*
            };
        }
        scaled!(
            complete,
            write,
            commit,
            close,
            checkpoint,
            open,
            takeover,
            crash,
            delete,
            delete_base,
            collect,
            stop_collector,
            unfork,
            twins,
            version,
            drop_version,
            restore_version,
            tick,
            relocate
        );
    }
}

/// cladefs: the chance, each step, that a region's bucket goes down, or comes up again.
const REGION_FLIPS: f64 = 0.02;

/// cladefs: the chance that a request to another region's bucket than its asker's waits a turn
/// more, as a request across regions takes longer.
const CROSSING_WAITS: f64 = 0.5;

impl Default for Profile {
    fn default() -> Self {
        Self {
            steps: 400,
            hosts: 3,
            fail_before: 0.05,
            fail_after: 0.05,
            hold: 0.0,
            restarts: false,
            one_cell: 0.5,
            forget: 0.0,
            regions: 0,
            fast_wal: 0.0,
            corrupting: false,
            teardown: false,
            weights: Weights {
                complete: 60,
                write: 6,
                commit: 12,
                close: 6,
                checkpoint: 2,
                open: 3,
                takeover: 1,
                crash: 1,
                delete: 1,
                delete_base: 0,
                collect: 2,
                stop_collector: 1,
                unfork: 0,
                twins: 0,
                version: 1,
                drop_version: 0,
                restore_version: 1,
                tick: 3,
                relocate: 0,
            },
            ticks: 4,
        }
    }
}

/// cladefs: the simulation's cell `id`: a cell of its own, as one made
/// alone, or a fork, is (TASK-364).
fn sim_cell(id: u64) -> bucket_cell::layout::CellId {
    bucket_cell::layout::CellId { tenant: 1, id }
}

/// The block size of the hosts' cells: small, so that a few blocks make an extent, and segments
/// of a few extents close, full, between commits. cladefs: a sector of an open segment, the least
/// it takes (`segments::SECTOR`, TASK-411.8); 512 bytes before.
const BLOCK: u64 = crate::segments::SECTOR;

/// How the hosts keep their cells: in segments of a few extents, those of three blocks
/// or more in objects of their own. No segment closes for its age, which the cell's clock, not
/// the world's, would decide.
fn cell_options() -> crate::Options {
    crate::Options {
        max_extent: 4 * BLOCK,
        segment_size: 6 * BLOCK,
        segment_age: Duration::from_secs(1 << 30),
        large_extent: 3 * BLOCK,
        open_files: 4,
        // Flushes of either kind: records holding their runs, and checking them (TASK-301).
        held_most: 2 * BLOCK,
    }
}

/// The capacity of the hosts' cells: what a commit gives a cell that has none yet.
const CAPACITY: u64 = 1 << 40;

/// The uploaders' interval, which only the world's [`Intervals`] run out.
const INTERVAL: Duration = Duration::from_secs(1);

/// Says what happened, to debug a seed, if `SIM_TRACE` is set.
macro_rules! trace {
    ($($arg:tt)*) => {
        if std::env::var_os("SIM_TRACE").is_some() {
            eprintln!($($arg)*);
        }
    };
}

/// How many PUTs a host's writers have in flight at once ([`Host::uploads`]).
const UPLOADS_AT_ONCE: usize = 2;

type Uploader = Shared<MemDir, SimBucket>;

/// When a session's uploader's interval runs out: only when the world says ([`World::close`]), so
/// that a run's seed alone decides what its uploaders do.
#[derive(Default)]
struct Intervals {
    waiting: Mutex<Vec<oneshot::Sender<()>>>,
}

impl Intervals {
    /// The uploader's clock: each interval it waits out runs out at the next
    /// [`Intervals::run_out`].
    fn clock(self: &Arc<Self>) -> Clock {
        let intervals = self.clone();
        Arc::new(move |_| {
            let (ran_out, running) = oneshot::channel();
            let mut waiting = intervals.waiting.lock().unwrap();
            waiting.retain(|waiting| !waiting.is_canceled());
            waiting.push(ran_out);
            running.map(drop).boxed()
        })
    }

    /// Runs out the intervals waited out now: whether any was.
    fn run_out(&self) -> bool {
        let waiting = std::mem::take(&mut *self.waiting.lock().unwrap());
        let any = waiting.iter().any(|waiting| !waiting.is_canceled());
        for ran_out in waiting {
            let _ = ran_out.send(());
        }
        any
    }
}

/// cladefs: the fast WAL buckets' regions of the world's bucket (doc-29, board 14433): one a zone,
/// zone Z's `FAST_REGION + Z`, past any cloud region's; host H's zone `H % FAST_ZONES`.
const FAST_REGION: u16 = 1000;
const FAST_ZONES: usize = 2;

/// cladefs: how many appends a fast WAL writer makes to a segment before it rolls it, and how many
/// the fast buckets take of an object: room for its `Rolled` record and a taker's `Fence` after
/// them, and not one more, as S3 Express's 10,000 parts leave room past the writers' 5,000.
const FAST_APPENDS: u32 = 3;
const FAST_PARTS: u32 = FAST_APPENDS + 2;

/// cladefs: the URL of zone `zone`'s fast bucket, as heads name it.
fn fast_url(zone: usize) -> String {
    format!("sim-fast://{zone}")
}

/// cladefs: the hosts' opener of the fast buckets heads name (doc-29), every zone's allowed: a
/// region of the world's bucket each.
struct FastBuckets(Arc<SimBucket>);

impl FastBuckets {
    fn zone(url: &str) -> Result<usize, String> {
        let zone = url.strip_prefix("sim-fast://").and_then(|zone| zone.parse().ok());
        zone.filter(|&zone| zone < FAST_ZONES).ok_or_else(|| format!("no fast bucket {url:?}"))
    }

    /// Zone `zone`'s fast bucket, as its hosts ask it.
    fn of(&self, zone: usize) -> Arc<AnyBucket> {
        let region = FAST_REGION + u16::try_from(zone).unwrap();
        Arc::new(AnyBucket::new(self.0.at(region).own()))
    }
}

impl FastWalOpener for FastBuckets {
    fn is_allowed(&self, url: &str) -> Result<(), String> {
        Self::zone(url).map(drop)
    }

    fn open(&self, url: &str) -> BoxFuture<'static, Result<Arc<AnyBucket>, String>> {
        let opened = Self::zone(url).map(|zone| self.of(zone));
        async move { opened }.boxed()
    }
}
/// cladefs: the most requests one settling completes before the world is found livelocked: far
/// past what a seed's steps leave waiting, so that only a world that never quiets down reaches it.
const SETTLE_MOST: usize = 200_000;

struct CellId {
    origin: Option<Origin>,
    /// The commit of its parent's that the version it was restored from is of, which its first
    /// follows.
    base: Option<u64>,
    /// The commits its guests were told are durable.
    acked: BTreeSet<u64>,
    deleting: bool,
    /// The highest fencing token a writer took it with: its latest placement's.
    taken: u64,
    /// cladefs: the commits its fast WAL told its guests are durable: in the history of its writer
    /// once one serves, or in the bucket.
    told: BTreeSet<u64>,
    /// cladefs: what its guests' syncs wait for, if it keeps its commits in a fast WAL (doc-29):
    /// what its takings ask.
    fast_wal: Option<layout::Fsync>,
}

impl CellId {
    /// A cell forked from `origin`, of its parent's commit `base`.
    fn forked(origin: Option<Origin>, base: Option<u64>) -> Self {
        Self {
            origin,
            base,
            acked: BTreeSet::new(),
            deleting: false,
            taken: 0,
            told: BTreeSet::new(),
            fast_wal: None,
        }
    }
}

/// A commit, as its guest made it.
struct Truth {
    cell: u64,
    parent: Option<u64>,
    /// The extents it sealed, and their bytes.
    extents: Vec<(ExtentId, Vec<u8>)>,
    frees: Vec<ExtentId>,
    root: Option<Vec<u8>>,
    /// The cell's capacity, if it sets it (TASK-165.3).
    capacity: Option<u64>,
}

/// A cell's directory on a host's disk: its write-ahead log, open segment and index, as
/// a crash leaves them.
struct Disk {
    dir: MemDir,
    /// Whether its cell was made whole ([`LocalCell::made`]).
    made: bool,
    /// The history of commits its cell holds ([`World::lineages`]): a new one once its log is
    /// dropped.
    lineage: u64,
}

struct Host {
    /// Each cell's directory (doc-14), by cell.
    disks: HashMap<u64, Disk>,
    sessions: BTreeMap<u64, Session>,
    /// The PUTs its writers upload objects with, as a host's share one budget
    /// (`local_cell::upload`): a few at once, so that uploads wait for each other's to be done,
    /// and are woken as they are (TASK-149.32).
    uploads: Arc<Budget>,
    /// What holds the parts its cells pack as their segments fill, which they share, as a
    /// daemon's do (`crate::held`, TASK-400).
    held: Arc<crate::HeldParts>,
    /// cladefs: its keys, its KMS the tests' and its fetchers' keys its own, which its cells share,
    /// as a daemon's do: every cell encrypted (TASK-411.28).
    keys: bucket_cell::keys::Host,
    /// cladefs: its mirrors under way, by ID ([`World::spawn_mirror`]), which its crash stops.
    mirrors: BTreeMap<u64, AbortHandle>,
    /// cladefs: what its process knows of the mirrors into its bucket, and their turns
    /// (`bucket_cell::mirror::Mirrors`, as a routed `ObjectBucket` keeps them): its crash's to
    /// forget.
    known: bucket_cell::mirror::Mirrors,
    /// cladefs: its zone's fast bucket, of a world with fast WAL cells ([`Profile::fast_wal`]).
    fast_wal: Option<FastWalConfig>,
}

impl Default for Host {
    fn default() -> Self {
        let limits = Limits { requests: UPLOADS_AT_ONCE, bytes: u64::MAX };
        let (disks, sessions) = (HashMap::new(), BTreeMap::new());
        let held = crate::HeldParts::new();
        let keys = bucket_cell::keys::testing::host();
        let (mirrors, known) = (BTreeMap::new(), Default::default());
        let uploads = Budget::new(limits);
        Self { disks, sessions, uploads, held, keys, mirrors, known, fast_wal: None }
    }
}

/// A cell on a host, its uploader, and its guest.
struct Session {
    id: u64,
    /// The fencing token of the placement it is.
    token: u64,
    /// The history of commits its cell holds ([`Disk::lineage`]).
    lineage: u64,
    uploader: Arc<Uploader>,
    intervals: Arc<Intervals>,
    /// What opening the cell gave, once it did.
    started: futures::channel::oneshot::Receiver<Result<Arc<LocalCell<MemDir>>, Failure>>,
    /// The cell, once open.
    cell: Option<Arc<LocalCell<MemDir>>>,
    /// The uploader's task.
    running: AbortHandle,
    guest: Guest,
    /// cladefs: the tasks its uploader runs beside it, its keeper ([`Spawn`]), until the world
    /// spawns them; and once it did, each one's; and its relocation rounds' ([`World::relocate`]).
    kept: Arc<Mutex<Vec<BoxFuture<'static, ()>>>>,
    keepers: Vec<AbortHandle>,
}

#[derive(Default)]
struct Guest {
    head: Option<u64>,
    held: BTreeMap<ExtentId, Vec<u8>>,
    /// What it wrote since its last commit, which the next seals.
    written: BTreeMap<ExtentId, Vec<u8>>,
    next_sequence: u64,
}

enum Event {
    /// A writer's checkpoint failed.
    Failed {
        host: usize,
        cell: u64,
        session: u64,
        error: Error,
    },
    /// cladefs: a writer's relocation round committed, at `epoch` of its session's cell
    /// (doc-27): a commit of the cell's state as of its commit before, its extents elsewhere.
    Relocated {
        lineage: u64,
        cell: u64,
        epoch: u64,
    },
    Deleted {
        cell: u64,
        result: Result<(), Error>,
    },
    /// What deleting a failed fork's child gave ([`lifecycle::delete_fork`]).
    Unforked {
        child: u64,
        result: Result<bool, Error>,
    },
    Collected {
        id: u64,
        result: Result<collector::Report, Error>,
    },
    /// What a host's fork of a version gave.
    Restored {
        child: u64,
        result: Result<(), Error>,
    },
    /// What two hosts' forks of a version to one child gave ([`World::twins`]).
    Twinned {
        child: u64,
        made: [(Restore, Result<(), Error>); 2],
    },
    /// cladefs: what host `host`'s mirror `id` came to ([`World::spawn_mirror`]).
    Mirrored {
        host: usize,
        id: u64,
        result: Result<Option<bucket_cell::mirror::Mirrored>, Error>,
    },
    /// cladefs: what a writer's dropping of a version gave, its session's, on `host`.
    VersionDropped {
        id: u64,
        result: Result<(), Error>,
        host: usize,
        session: u64,
    },
}

/// A failed fork's deletion, until it is done: its child's owner, and the task, unless a crash
/// stopped it, for its host's next start to go on with.
struct Unfork {
    owner: u64,
    running: Option<AbortHandle>,
}

/// A fork of a version, until it is done: of which, naming what, on which host, and the task,
/// unless a crash stopped it, for its host's next start to finish or delete.
struct Restore {
    /// cladefs: the version of the cell `origin` names (TASK-476).
    version: u64,
    origin: Origin,
    /// The commit the fork's child starts from.
    base: Option<u64>,
    host: usize,
    /// Its child's owner.
    owner: u64,
    running: Option<AbortHandle>,
}

/// cladefs: a version a guest cut (TASK-476): of which cell, as of which of its commits, whether
/// its writer drops it, asked, and whether it dropped it, told so; and the epoch of the writer that
/// dropped it, whose head names it no more from then on (TASK-569).
struct Cut {
    cell: u64,
    tag: u64,
    dropping: bool,
    dropped: bool,
    dropped_by: u64,
}

/// What a segment's footer says of the cell's history: its commits' tags.
type Applied = Vec<u64>;

pub struct World {
    rng: StdRng,
    profile: Profile,
    bucket: Arc<SimBucket>,
    pool: LocalPool,
    spawner: LocalSpawner,
    events: Rc<RefCell<Vec<Event>>>,
    hosts: Vec<Host>,
    cells: BTreeMap<u64, CellId>,
    commits: HashMap<u64, Truth>,
    /// The commit, or write, that minted each extent.
    minted: HashMap<ExtentId, u64>,
    /// Each history of commits a cell held, by the cell's epoch of each: what the tags of the
    /// commits its writers upload name ([`World::truth_of`]).
    lineages: HashMap<u64, BTreeMap<u64, u64>>,
    /// The history the commits each writer uploads are of, by its cell and epoch: a writer
    /// uploads one cell's.
    stamps: HashMap<(u64, u64), u64>,
    collectors: BTreeMap<u64, AbortHandle>,
    unforks: BTreeMap<u64, Unfork>,
    restores: BTreeMap<u64, Restore>,
    /// cladefs: the versions the guests cut, by their IDs (TASK-476).
    versions: BTreeMap<u64, Cut>,
    /// What the footer of each segment the history check read says, by its cell, number and
    /// epoch, one writer stamping one footer at a place.
    footers: RefCell<HashMap<(u64, u64, u64), Applied>>,
    next_id: u64,
    /// Whether requests fail.
    faults: bool,
    /// cladefs: the regions whose buckets are down (TASK-633.10).
    down: BTreeSet<u16>,
    /// Whether everything is finishing: then every live cell must open.
    quiescing: bool,
    /// The tasks spawned and not done, aborted or dropped, by number, each with where it was
    /// spawned: one still here once every request is done waits for a wake nothing will give
    /// (TASK-149.32). The uploaders, which wait for commits for as long as their cells are open,
    /// are not among them: that each uploads what it is asked to is checked of it
    /// ([`World::check_uploaded`]).
    tasks: Rc<RefCell<BTreeMap<u64, &'static std::panic::Location<'static>>>>,
    next_task: u64,
    pub violations: Vec<String>,
    /// cladefs: what the hosts' mirrors came to, by outcome, and the copies sweeps alone deleted
    /// ("collected"): what a run reached of mirrors (TASK-633.10).
    pub mirrored: BTreeMap<&'static str, usize>,
}

/// A task's place in [`World::tasks`], which it gives up as it is dropped: done, or aborted.
struct Running {
    tasks: Rc<RefCell<BTreeMap<u64, &'static std::panic::Location<'static>>>>,
    number: u64,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.tasks.borrow_mut().remove(&self.number);
    }
}

/// A sweep of the collector over every cell of `bucket`, as `id`. cladefs: of a region's bucket, by
/// a collector of its region, its own cell's copies of other buckets' cells passed over as such
/// (TASK-633.8).
async fn sweep(bucket: &SimBucket, id: u64) -> Result<collector::Report, Error> {
    match bucket.home() {
        0 => collector::collect(bucket, id).await,
        _ => collector::collect_with_own(bucket, &*bucket.own(), id).await,
    }
}

impl World {
    pub fn new(seed: u64, profile: Profile) -> Self {
        // cladefs: the code's buggify sites, on as the seed says, for this run alone (doc-24).
        bucket_cell::buggify::enable(seed);
        let pool = LocalPool::new();
        let spawner = pool.spawner();
        let mut world = Self {
            rng: StdRng::seed_from_u64(seed),
            profile,
            bucket: match profile.regions {
                0 => SimBucket::new(),
                _ => SimBucket::new().at(1),
            },
            pool,
            spawner,
            events: Rc::default(),
            hosts: (0..profile.hosts).map(|_| Host::default()).collect(),
            cells: BTreeMap::new(),
            commits: HashMap::new(),
            minted: HashMap::new(),
            lineages: HashMap::new(),
            stamps: HashMap::new(),
            collectors: BTreeMap::new(),
            unforks: BTreeMap::new(),
            restores: BTreeMap::new(),
            versions: BTreeMap::new(),
            footers: RefCell::default(),
            next_id: 1,
            faults: true,
            down: BTreeSet::new(),
            quiescing: false,
            tasks: Rc::default(),
            next_task: 0,
            violations: Vec::new(),
            mirrored: BTreeMap::new(),
        };
        if profile.fast_wal > 0.0 {
            world.bucket.set_max_appends(Some(FAST_PARTS));
            let buckets = Arc::new(FastBuckets(world.bucket.clone()));
            for (host, found) in world.hosts.iter_mut().enumerate() {
                let zone = host % FAST_ZONES;
                let mut config =
                    FastWalConfig::new(fast_url(zone), buckets.of(zone), buckets.clone());
                config.max_writer_appends = FAST_APPENDS;
                found.fast_wal = Some(config);
            }
        }
        world.format_regions();
        let (root, owner) = (world.cell_id(1), world.id());
        block_on(lifecycle::create(&Direct(world.bucket.clone()), sim_cell(root), owner)).unwrap();
        let found = world.new_cell(None, None);
        world.cells.insert(root, found);
        world
    }

    /// cladefs: with regions, the first region's bucket formatted, the root, and each other's added
    /// to it in turn, numbered as its region, every one holding the root's registry, as an operator
    /// sets up a bucket a region (TASK-633.3): what a region's collector reads its number from.
    fn format_regions(&self) {
        use bucket_cell::identity::{self, Details};
        if self.profile.regions == 0 {
            return;
        }
        let details =
            |region: u16| Details { url: Some(format!("sim://{region}")), ..Details::default() };
        let (root, root_details) = (Direct(self.bucket.at(1)), details(1));
        block_on(identity::format(&root, &root_details)).unwrap();
        let mut registry = None;
        for region in 2..=self.profile.regions {
            let added = Direct(self.bucket.at(region));
            let adding = identity::add(&root, &root_details, &added, details(region));
            let (identity, added) = block_on(adding).unwrap();
            assert_eq!(identity.number, region, "region {region}'s bucket's number");
            registry = Some(added);
        }
        for region in 1..=self.profile.regions {
            if let Some(registry) = &registry {
                block_on(identity::spread(registry, &Direct(self.bucket.at(region)))).unwrap();
            }
        }
    }

    /// cladefs: a cell forked from `origin`, of its parent's commit `base`.
    fn new_cell(&mut self, origin: Option<Origin>, base: Option<u64>) -> CellId {
        let mut found = CellId::forked(origin, base);
        // cladefs: fast WAL cell, its guests' syncs waiting for it, or for uploads alone.
        if self.profile.fast_wal > 0.0 && self.rng.random_bool(self.profile.fast_wal) {
            let fsync = match self.rng.random_bool(0.5) {
                true => layout::Fsync::FastWal,
                false => layout::Fsync::Ordered,
            };
            found.fast_wal = Some(fsync);
        }
        found
    }

    /// What the run asked of the bucket, in order ([`SimBucket::journal`]).
    pub fn journal(&self) -> Vec<String> {
        self.bucket.journal()
    }

    /// Runs the profile's steps, then lets everything finish and checks that no garbage is left.
    /// The test's outermost entry: its steps await the cells, which [`crate::wait_here`] runs them
    /// for, its pool's tasks run between their awaits ([`World::settle_tasks`]), which
    /// `futures::executor::block_on` would refuse to.
    pub fn run(&mut self) {
        crate::wait_here(async {
            for _ in 0..self.profile.steps {
                self.step().await;
                self.check();
                if !self.violations.is_empty() {
                    return;
                }
            }
            self.quiesce().await;
        })
    }

    async fn step(&mut self) {
        self.settle_tasks();
        if self.profile.regions > 0 && self.faults && self.rng.random_bool(REGION_FLIPS) {
            let region = self.rng.random_range(1..=self.profile.regions);
            let down = !self.down.remove(&region);
            if down {
                self.down.insert(region);
            }
            trace!("region {region}'s bucket {}", if down { "down" } else { "up" });
            self.bucket.down(region, down);
        }
        // cladefs: a zone's fast bucket down, or up again (doc-29): its writers' cells degraded to
        // uploading alone.
        if self.profile.fast_wal > 0.0 && self.faults && self.rng.random_bool(REGION_FLIPS) {
            let zone = self.rng.random_range(0..FAST_ZONES);
            let region = FAST_REGION + u16::try_from(zone).unwrap();
            let down = !self.down.remove(&region);
            if down {
                self.down.insert(region);
            }
            trace!("zone {zone}'s fast bucket {}", if down { "down" } else { "up" });
            self.bucket.down(region, down);
        }
        let w = self.profile.weights;
        let choices = [
            w.complete,
            w.commit,
            w.close,
            w.checkpoint,
            w.open,
            w.takeover,
            w.crash,
            w.delete,
            w.delete_base,
            w.collect,
            w.stop_collector,
            w.unfork,
            w.twins,
            w.write,
            w.version,
            w.drop_version,
            w.restore_version,
            w.tick,
            w.relocate,
        ];
        let mut roll = self.rng.random_range(0..choices.iter().sum::<u32>());
        let choice = choices.iter().position(|&weight| {
            let hit = roll < weight;
            roll = roll.saturating_sub(weight);
            hit
        });
        match choice.unwrap() {
            0 => self.complete(),
            1 => self.commit().await,
            2 => self.close(),
            3 => self.checkpoint(),
            4 => self.open(false),
            5 => self.open(true),
            6 => self.crash(),
            7 => self.delete(false),
            8 => self.delete(true),
            9 => self.collect(),
            10 => self.stop_collector(),
            11 => self.unfork(),
            12 => self.twins(),
            13 => self.write().await,
            14 => self.cut_version().await,
            15 => self.drop_version(),
            16 => self.restore_version(),
            17 => {
                let ticks = self.rng.random_range(1..=self.profile.ticks);
                self.bucket.advance(ticks);
            }
            _ => self.relocate(),
        }
        self.settle_tasks();
    }

    /// Runs tasks until they wait for requests, handling what they report and taking in what the
    /// uploaders did.
    fn settle_tasks(&mut self) {
        loop {
            self.pool.run_until_stalled();
            let events = std::mem::take(&mut *self.events.borrow_mut());
            let observed = self.observe();
            if events.is_empty() && !observed {
                return;
            }
            for event in events {
                self.handle(event);
            }
        }
    }

    // Tasks.

    /// Spawns an uploader's `task`, which waits for its cell's commits for as long as it is
    /// open: counted among no [`World::tasks`].
    fn spawn_uploader(&mut self, task: impl Future<Output = ()> + 'static) -> AbortHandle {
        let (task, handle) = abortable(task);
        self.spawner
            .spawn_local(async move {
                let _ = task.await;
            })
            .unwrap();
        handle
    }

    fn handle(&mut self, event: Event) {
        match event {
            Event::Failed { host, cell, session, error } => {
                if let Error::Lost(_) | Error::Corrupt(_) = error {
                    self.violation(format!("cell {cell}'s writer: {error}", cell = Short(cell)));
                }
                self.end(host, cell, session);
            }
            // Into the history its session's cell held then, its session over or not: what it
            // committed, its log keeps (the verifying review of fa1912379).
            Event::Relocated { lineage, cell, epoch } => {
                let history = self.lineages.get_mut(&lineage).unwrap();
                match history.range(..epoch).next_back() {
                    Some((_, &before)) => drop(history.insert(epoch, before)),
                    None => self.violation(format!(
                        "cell {cell} relocated at {epoch}, its guest having committed nothing",
                        cell = Short(cell)
                    )),
                }
            }
            Event::Deleted { cell, result } => match result {
                Ok(()) => {}
                // A later placement took it as it was deleted: it lives on.
                Err(Error::Stale) => self.cells.get_mut(&cell).unwrap().deleting = false,
                Err(error) => {
                    self.violation(format!("deleting cell {cell}: {error}", cell = Short(cell)))
                }
            },
            Event::Unforked { child, result: Ok(true) } => drop(self.unforks.remove(&child)),
            Event::Unforked { child, result } => {
                self.violation(format!(
                    "deleting the failed fork to {child}: {result:?}",
                    child = Short(child)
                ));
            }
            Event::Collected { id, result } => {
                self.collectors.remove(&id);
                if let Err(error) = result {
                    self.violation(format!("collector: {error}"));
                }
            }
            Event::Restored { child, result } => {
                if let Some(restore) = self.restores.remove(&child) {
                    self.restored(child, restore, result);
                }
            }
            // Whichever's record came first took the ID; the other, refused, made nothing.
            Event::Twinned {
                child,
                made:
                    [(restore, result), (_, Err(Error::IdTaken(_)))]
                    | [(_, Err(Error::IdTaken(_))), (restore, result)],
            } => self.restored(child, restore, result),
            Event::Twinned { child, made } => {
                let results = made.map(|(_, result)| result);
                self.violation(format!(
                    "two forks of a version to cell {child} gave {results:?}",
                    child = Short(child)
                ));
            }
            Event::Mirrored { host, id, result } => {
                self.hosts[host].mirrors.remove(&id);
                trace!("host {host}: mirror {id}: {result:?}");
                use bucket_cell::mirror::Mirrored;
                let outcome = match &result {
                    Ok(None) => "not wanted",
                    Ok(Some(Mirrored::Done { .. })) => "done",
                    Ok(Some(Mirrored::Unnamed)) => "unnamed",
                    Ok(Some(Mirrored::GaveUp(_))) => "gave up",
                    Ok(Some(Mirrored::UnderWay)) => "under way",
                    Err(_) => "failed",
                };
                *self.mirrored.entry(outcome).or_default() += 1;
                // Its requests failing, as its bucket's may: begun again by the next fork, or
                // opening, or collector, that wants it.
                if let Err(Error::Lost(_) | Error::Corrupt(_)) = result {
                    self.violation(format!("host {host}'s mirror: {result:?}"));
                }
            }
            Event::VersionDropped { id, result, host, session } => {
                trace!("version {id} dropped: {result:?}", id = Short(id));
                let cut = self.versions.get_mut(&id).unwrap();
                cut.dropping = false;
                let cell = cut.cell;
                // cladefs: told by a session still alive: a crashed host tells nobody, and its
                // writes in flight, an acknowledgement naming the version among them, may land
                // after it died, as doc-18 accepts of a drop and a crash.
                let alive =
                    (self.hosts[host].sessions.get(&cell)).is_some_and(|alive| alive.id == session);
                match result {
                    Ok(()) if alive => cut.dropped = true,
                    Err(Error::Lost(_) | Error::Corrupt(_)) => {
                        self.violation(format!(
                            "dropping version {id}: {result:?}",
                            id = Short(id)
                        ));
                    }
                    // Its writer taken over, or its cell deleted: dropped as its swap of the head
                    // landed or not, which a write that took effect, answered unavailable, then
                    // found changed by the taking over, can't tell its writer (TASK-499): as the
                    // head says, the writer writing it no more. cladefs: and as its host crashed.
                    Ok(()) | Err(_) => {
                        let direct = Direct(self.bucket.clone());
                        let head = block_on(bucket_cell::head::get(&direct, sim_cell(cell)));
                        let named =
                            head.ok().flatten().is_some_and(|head| head.version(id).is_some());
                        let cut = self.versions.get_mut(&id).unwrap();
                        cut.dropped = !named;
                        // Its crashed host's writes in flight may name it yet: no writer
                        // dropped it, told so (TASK-569).
                        if !alive {
                            cut.dropped_by = 0;
                        }
                    }
                }
            }
        }
    }

    /// What `restore`, a fork of a version to `child`, gave.
    fn restored(&mut self, child: u64, restore: Restore, result: Result<(), Error>) {
        trace!(
            "host {}: fork of version {} to {}: {result:?}",
            restore.host,
            Short(restore.version),
            Short(child)
        );
        match result {
            // Told to its VM's manager: a cell from now on, whatever comes of the version.
            // cladefs: its host begins the version's mirror into its bucket, as `Uploader::forked`
            // does (TASK-633.8).
            Ok(()) => {
                let found = self.new_cell(Some(restore.origin), restore.base);
                self.cells.insert(child, found);
                self.spawn_mirror(restore.host, child, Some(restore.origin));
            }
            // The version went, or the fork was given up on: its host deletes it.
            Err(Error::Deleted) => self.fail_restore(child, restore),
            Err(error) => {
                self.violation(format!("forking version {}: {error}", Short(restore.version)))
            }
        }
    }

    // The cells and their uploaders.

    /// Takes in what each session's uploader did since: its cell opened, or refused to; its
    /// uploader stopped; the epoch of its writer, which the tags of the commits it uploads
    /// name; and the commits it uploaded, which their guests are told are durable. Returns
    /// whether a session opened, ended or started anew, which the next run of the tasks may go on
    /// with.
    fn observe(&mut self) -> bool {
        let mut changed = false;
        for host in 0..self.hosts.len() {
            let cells: Vec<u64> = self.hosts[host].sessions.keys().copied().collect();
            for cell in cells {
                // cladefs: what its uploader runs beside it, spawned (TASK-680).
                let kept =
                    std::mem::take(&mut *self.hosts[host].sessions[&cell].kept.lock().unwrap());
                for task in kept {
                    let keeper = self.spawn_uploader(task);
                    self.hosts[host].sessions.get_mut(&cell).unwrap().keepers.push(keeper);
                    changed = true;
                }
                let session = &self.hosts[host].sessions[&cell];
                let id = session.id;
                let session = self.hosts[host].sessions.get_mut(&cell).unwrap();
                if session.cell.is_none() {
                    match session.started.try_recv() {
                        Ok(Some(Ok(open_cell))) => self.opened(host, cell, open_cell),
                        Ok(Some(Err(failure))) => self.refused(host, cell, failure),
                        Ok(None) => continue,
                        Err(futures::channel::oneshot::Canceled) => {
                            unreachable!("an uploader stopped without saying how its cell opened")
                        }
                    }
                    changed = true;
                    continue;
                }
                if let Some(failure) = session.uploader.failed() {
                    trace!(
                        "host {host}: session {id} of {cell}'s uploader stopped: {failure}",
                        cell = Short(cell)
                    );
                    if let Failure::Bucket(Error::Lost(_) | Error::Corrupt(_)) = failure {
                        self.violation(format!(
                            "cell {cell}'s uploader: {failure}",
                            cell = Short(cell)
                        ));
                    }
                    self.end(host, cell, id);
                    changed = true;
                    continue;
                }
                let (lineage, uploader) = (session.lineage, session.uploader.clone());
                if let Some(writer) = uploader.writer() {
                    self.stamps.insert((cell, writer.epoch()), lineage);
                }
                // Its guest is told of what was uploaded, its own commits.
                let uploaded = uploader.last_uploaded();
                let commits = &self.commits;
                let acked: Vec<u64> = (self.lineages[&lineage].range(..=uploaded))
                    .map(|(_, &tag)| tag)
                    .filter(|tag| commits[tag].cell == cell)
                    .collect();
                // cladefs: and of what was appended to its fast WAL, by its sink's word (doc-29,
                // board 14433).
                let open_cell = session.cell.as_ref();
                let told_through =
                    open_cell.and_then(|open_cell| open_cell.fast_wal()).map(|sink| sink.durable());
                let told: Vec<u64> = match told_through {
                    Some(through) => (self.lineages[&lineage].range(..=through))
                        .map(|(_, &tag)| tag)
                        .filter(|tag| commits[tag].cell == cell)
                        .collect(),
                    None => Vec::new(),
                };
                if let Some(found) = self.cells.get_mut(&cell) {
                    found.acked.extend(acked);
                    found.told.extend(told);
                }
            }
        }
        changed
    }

    /// The cell directory of `cell` on `host`, made if its disk has none.
    fn disk(&mut self, host: usize, cell: u64) -> &mut Disk {
        if !self.hosts[host].disks.contains_key(&cell) {
            let dir = MemDir::serial();
            let description =
                Description { block_size: BLOCK as u32, capacity: CAPACITY, cell: sim_cell(cell) };
            LocalCell::make(&dir, description, Some("sim:///"), None).unwrap();
            let lineage = self.id();
            self.lineages.insert(lineage, BTreeMap::new());
            self.hosts[host].disks.insert(cell, Disk { dir, made: false, lineage });
        }
        self.hosts[host].disks.get_mut(&cell).unwrap()
    }

    /// Starts a session of `cell` on `host`: its cell opened, its writer taking the cell, of
    /// a new placement, the latest.
    fn start_session(&mut self, host: usize, cell: u64) {
        let disk = self.disk(host, cell);
        let dir = disk.dir.clone();
        let recovering = match disk.made {
            true => LocalCell::recover(dir),
            false => LocalCell::recover_unmade(dir),
        };
        match recovering {
            Ok(recovering) => self.run_session(host, cell, None, recovering),
            Err(err) => {
                self.violation(format!("recovering cell {cell}: {err:#}", cell = Short(cell)))
            }
        }
    }

    /// Starts a session of `cell` on `host` that opens the cell `recovering` recovered, as
    /// [`World::start_session`] does.
    fn run_session(
        &mut self,
        host: usize,
        cell: u64,
        token: Option<u64>,
        recovering: Recovering<MemDir>,
    ) {
        let (id, holder) = (self.id(), self.id());
        // cladefs: its placement's, which its cell's description names once it was placed, as a
        // forced one is (TASK-681): what its writer takes the cell with, whatever it is asked.
        let token = recovering.token().or(token).unwrap_or(holder);
        let intervals = Arc::<Intervals>::default();
        let options = upload::Options {
            interval: INTERVAL,
            upload_closed: true,
            max_lag: u64::MAX,
            max_lag_bytes: u64::MAX,
            cache: None,
            held: Some(self.hosts[host].held.clone()),
            token,
            checkpoint_every: upload::CHECKPOINT_EVERY,
            checkpoint_min_on_close: upload::CHECKPOINT_MIN_ON_CLOSE,
            // The simulation's clock advances on demand, as for `filling_wait`.
            ack_coalesce: Duration::ZERO,
            // The interval closes a segment filled at once, as the first full one closed between
            // commits, its commits uploading, is what the world's clock can't wait for
            // (TASK-92.41).
            filling_wait: Duration::ZERO,
            // cladefs: the world's own op runs relocation rounds ([`World::relocate`]), which its
            // model follows: its uploaders run none, whose commits it would not know of.
            relocate: false,
            fetching: None,
            fast_wal: self.cells.get(&cell).and_then(|found| found.fast_wal).map(Some),
            fast_wal_config: self.hosts[host].fast_wal.clone(),
            fast_wal_large_extent: crate::fast_wal::DEFAULT_LARGE_EXTENT,
        };
        let (uploads, keys) = (self.hosts[host].uploads.clone(), self.hosts[host].keys.clone());
        let kept = Arc::<Mutex<Vec<BoxFuture<'static, ()>>>>::default();
        let keeping = kept.clone();
        let spawn: Spawn = Arc::new(move |task| keeping.lock().unwrap().push(task));
        let uploader = Shared::new_on(None, options, uploads, (intervals.clock(), spawn), keys);
        let (started, start) = futures::channel::oneshot::channel();
        let (running, bucket) = (uploader.clone(), self.host_bucket(host));
        let running = self.spawn_uploader(async move {
            running.run(recovering, cell_options(), bucket, holder, started, None).await;
        });
        let lineage = self.hosts[host].disks[&cell].lineage;
        trace!(
            "host {host}: session {id} of {cell} starts, token {token}, history {lineage}",
            cell = Short(cell)
        );
        let session = Session {
            id,
            token,
            lineage,
            uploader,
            intervals,
            started: start,
            cell: None,
            running,
            guest: Guest::default(),
            kept,
            keepers: Vec::new(),
        };
        self.hosts[host].sessions.insert(cell, session);
    }

    /// `cell`'s cell on `host` opened, its writer having taken the cell: its log put after
    /// the cell's end, what it holds that landed acknowledged, the rest stamped anew. Its guest
    /// starts from its last commit, which the cell's history holds, or the bucket's.
    fn opened(&mut self, host: usize, cell: u64, open_cell: Arc<LocalCell<MemDir>>) {
        let disk = self.hosts[host].disks.get_mut(&cell).unwrap();
        if !disk.made {
            if let Err(err) = open_cell.made() {
                return self.violation(format!("making cell {cell}: {err:#}", cell = Short(cell)));
            }
            disk.made = true;
        }
        let session = &self.hosts[host].sessions[&cell];
        let (token, lineage, uploader) = (session.token, session.lineage, session.uploader.clone());
        let writer = uploader.writer().expect("a writer took the cell");
        let epoch = match open_cell.last_commit() {
            Ok(epoch) => epoch,
            Err(err) => {
                return self.violation(format!("cell {cell}: {err:#}", cell = Short(cell)));
            }
        };
        // Its last commit: its log's, past the cell's end, or the cell's, its log holding
        // nothing more: the cell's history then, if it holds that commit, or the bucket's, the
        // commits its history holds past it lost, a crash taking them before they were durable.
        let last = writer.state().last_commit;
        let history = &self.lineages[&lineage];
        let head = match last.map_or(0, epoch_of) == epoch {
            false => history.get(&epoch).copied(),
            true => match self.truth_of(cell, last) {
                Ok(head) => head,
                Err(violation) => return self.violation(violation),
            },
        };
        let lineage = if head.is_some() && history.get(&epoch).copied() != head {
            let lineage = self.id();
            self.lineages.insert(lineage, BTreeMap::new());
            self.hosts[host].disks.get_mut(&cell).unwrap().lineage = lineage;
            lineage
        } else {
            lineage
        };
        self.stamps.insert((cell, writer.epoch()), lineage);
        let history = self.lineages.get_mut(&lineage).unwrap();
        history.retain(|&at, _| at <= epoch);
        if let Some(head) = head {
            history.insert(epoch, head);
        }
        let found = self.cells.get_mut(&cell).unwrap();
        found.taken = found.taken.max(token);
        let (held, _) = self.expected(head);
        let session = self.hosts[host].sessions.get_mut(&cell).unwrap();
        let next_sequence = uploader.next_sequence();
        trace!(
            "host {host}: session {} of {cell} opened by writer {} at epoch {epoch}, the \
             cell's last {last:?}: head {head:?}, history {lineage}",
            session.id,
            writer.epoch(),
            cell = Short(cell)
        );
        session.lineage = lineage;
        session.guest = Guest { head, held, written: BTreeMap::new(), next_sequence };
        session.cell = Some(open_cell);
        // cladefs: its host begins the mirror of the version it was forked from, if any, as
        // `upload::open` does, the host that forked it perhaps dead before it did (TASK-633.8).
        self.spawn_mirror(host, cell, None);
    }

    /// `cell`'s cell on `host` did not open, for `failure`: its log diverged, another writer's
    /// commits past it, which `cladefsctl take` drops, and it opens again with none; else its guest
    /// goes. cladefs: a log diverged must have: the cell's last commit is neither one of its
    /// commits nor its base. Before, every divergence was taken as one, which hid a log whose
    /// segment ending one commit and going on with the next landed alone, its next commit, durable,
    /// dropped (TASK-345).
    fn refused(&mut self, host: usize, cell: u64, failure: Failure) {
        trace!("host {host}: {cell} refused: {failure}", cell = Short(cell));
        let session = self.hosts[host].sessions.remove(&cell).unwrap();
        let token = session.token;
        self.stop(host, cell, session);
        if let Failure::Bucket(Error::Diverged) = failure {
            let disk = self.hosts[host].disks.get_mut(&cell).unwrap();
            // cladefs: one never made whole too, which a take may have restored its members' logs
            // into (TASK-680).
            let recovering = match disk.made {
                true => LocalCell::recover(disk.dir.clone()),
                false => LocalCell::recover_unmade(disk.dir.clone()),
            };
            let mut recovering = match recovering {
                Ok(recovering) => recovering,
                Err(err) => {
                    return self.violation(format!("cell {cell}: {err:#}", cell = Short(cell)));
                }
            };
            let origin = self.cells[&cell].origin;
            let direct = Direct(self.bucket.clone());
            if let Ok(recovered) = block_on(recovery::recover(&direct, sim_cell(cell), origin))
                && recovering.holds_commit(recovered.state.last_commit)
            {
                let last = recovered.state.last_commit;
                return self.violation(format!(
                    "cell {cell}'s log diverged, though the cell's last commit, {last:x?}, \
                     is the log's",
                    cell = Short(cell)
                ));
            }
            if let Err(err) = recovering.drop_wal() {
                return self
                    .violation(format!("dropping cell {cell}'s log: {err:#}", cell = Short(cell)));
            }
            let lineage = self.id();
            self.lineages.insert(lineage, BTreeMap::new());
            self.hosts[host].disks.get_mut(&cell).unwrap().lineage = lineage;
            return self.run_session(host, cell, Some(token), recovering);
        }
        if self.quiescing {
            self.violation(format!("cell {cell} does not open: {failure}", cell = Short(cell)));
        }
    }

    /// The commit the guests made that `tag`, a commit's in a footer or a state of `cell`'s,
    /// names: the tag names the epoch of the writer that uploaded it, which says which
    /// cell's history it is of, and the cell's epoch of it there. A fork's child names its
    /// parent's commits until it has its own.
    fn truth_of(&self, cell: u64, tag: Option<u64>) -> Result<Option<u64>, String> {
        let Some(tag) = tag else { return Ok(None) };
        let mut at = Some(cell);
        while let Some(of) = at {
            if let Some(lineage) = self.stamps.get(&(of, writer_of(tag)))
                && let Some(&commit) = self.lineages[lineage].get(&epoch_of(tag))
            {
                return Ok(Some(commit));
            }
            at = self.cells.get(&of).and_then(|found| found.origin).map(|origin| origin.parent.id);
        }
        Err(format!("cell {cell} names commit {tag:#x}, which no guest made", cell = Short(cell)))
    }

    /// Ends a session: its guest stops.
    fn end(&mut self, host: usize, cell: u64, session: u64) {
        let sessions = &mut self.hosts[host].sessions;
        if sessions.get(&cell).is_some_and(|found| found.id == session) {
            let ended = sessions.remove(&cell).unwrap();
            self.stop(host, cell, ended);
        }
    }

    /// Stops `session`, of `cell` on `host`, taken out of the host's sessions: its uploader, as
    /// its host dying stops it, and its cell, which closes as it goes, making its last commits
    /// durable, before anything opens its directory again; a crash took what its disk keeps
    /// before ([`World::crash`]).
    fn stop(&mut self, host: usize, cell: u64, session: Session) {
        trace!("host {host}: session {} of {cell} stops", session.id, cell = Short(cell));
        session.running.abort();
        for keeper in &session.keepers {
            keeper.abort();
        }
        let Session { uploader, cell: open_cell, kept, .. } = session;
        drop(kept);
        let closing = open_cell.as_ref().map(Arc::downgrade);
        let going = Arc::downgrade(&uploader);
        drop((uploader, open_cell));
        // The uploader's task, aborted, lets go of the cell as it is next polled.
        self.pool.run_until_stalled();
        assert!(
            closing.as_ref().is_none_or(|open_cell| open_cell.strong_count() == 0),
            "cell {cell} on host {host} is open still, its session stopped: {} \
             holds of it, {} of its uploader",
            closing.as_ref().map_or(0, std::sync::Weak::strong_count),
            going.strong_count(),
            cell = Short(cell),
        );
    }

    /// `cell`'s cell on `host` failed doing what its guest asked, for `err`: which it may only
    /// once its uploader stopped. `open_cell` is the caller's hold of it, let go first, as the
    /// session's end closes the cell ([`World::stop`]).
    fn cell_failed(
        &mut self,
        host: usize,
        cell: u64,
        open_cell: Arc<LocalCell<MemDir>>,
        err: anyhow::Error,
    ) {
        drop(open_cell);
        let session = &self.hosts[host].sessions[&cell];
        let (id, stopped) = (session.id, session.uploader.failed().is_some());
        if !stopped {
            self.violation(format!("cell {cell} on host {host}: {err:#}", cell = Short(cell)));
        }
        self.end(host, cell, id);
    }

    /// Checks that every session's uploader uploads what its cell committed, asked to, as
    /// everything settles (TASK-149.32): one that doesn't waits for a wake nothing will give.
    fn check_uploaded(&mut self) {
        trace!("checking uploads");
        let mut asked = Vec::new();
        for host in &self.hosts {
            for (&cell, session) in &host.sessions {
                let Some(open_cell) = &session.cell else { continue };
                let epoch = open_cell.last_commit().unwrap();
                let _ = session.uploader.ask(epoch, Awaited::Uploaded);
                asked.push((cell, Arc::downgrade(&session.uploader), epoch));
            }
        }
        self.settle();
        for (cell, uploader, epoch) in asked {
            // Its session ended meanwhile, which observing its uploader's failure ends.
            let Some(uploader) = uploader.upgrade() else { continue };
            if uploader.failed().is_none() && uploader.last_uploaded() < epoch {
                self.violation(format!(
                    "cell {cell}'s commits through {epoch} wait to be uploaded, everything \
                     settled: uploaded through {}",
                    uploader.last_uploaded(),
                    cell = Short(cell)
                ));
            }
        }
    }

    // Actions.

    /// A random session whose cell is open, and its writer.
    fn ready(&mut self) -> Option<(usize, u64)> {
        let ready: Vec<(usize, u64)> = self
            .hosts
            .iter()
            .enumerate()
            .flat_map(|(host, found)| {
                found
                    .sessions
                    .iter()
                    .filter(|(_, session)| {
                        session.cell.is_some()
                            && session.uploader.failed().is_none()
                            && session.uploader.writer().is_some_and(|w| w.stopped().is_none())
                    })
                    .map(move |(cell, _)| (host, *cell))
            })
            .collect();
        (!ready.is_empty()).then(|| ready[self.rng.random_range(0..ready.len())])
    }

    async fn write(&mut self) {
        let Some((host, cell)) = self.ready() else { return };
        let seed = self.id();
        self.write_extent(host, cell, seed).await;
    }

    /// The guest of `cell`'s cell on `host` writes an extent of its own, its bytes of `seed`:
    /// whether it could.
    async fn write_extent(&mut self, host: usize, cell: u64, seed: u64) -> bool {
        let session = self.hosts[host].sessions.get_mut(&cell).unwrap();
        let extent = ExtentId::new(cell, session.guest.next_sequence);
        session.guest.next_sequence += 1;
        let len = self.rng.random_range(1..=4) * BLOCK as usize;
        let bytes = Self::content(seed, extent, len);
        let open_cell = session.cell.clone().unwrap();
        if let Err(err) = open_cell.write(extent, 0, &bytes).await {
            self.cell_failed(host, cell, open_cell, err);
            return false;
        }
        session.guest.written.insert(extent, bytes);
        if let Some(other) = self.minted.insert(extent, seed) {
            self.violation(format!("{extent:?} minted by {other} and {seed}"));
        }
        true
    }

    /// The cell's epoch of the commit `open_cell` just made, which follows `before`'s.
    fn committed(&mut self, cell: u64, open_cell: &LocalCell<MemDir>, before: u64) -> Option<u64> {
        let epoch = open_cell.last_commit().ok()?;
        if epoch <= before {
            self.violation(format!(
                "cell {cell}'s commit after {before} made epoch {epoch}",
                cell = Short(cell)
            ));
            return None;
        }
        Some(epoch)
    }

    /// The guest of `cell`'s cell on `host` made the commit `tag`, sealing what it wrote,
    /// freeing `frees`, the cell's epoch of it `epoch`.
    fn made(&mut self, host: usize, cell: u64, tag: u64, epoch: u64, mut made: Truth) {
        trace!(
            "host {host}: {cell} commits {tag} at epoch {epoch}, after {:?}",
            made.parent,
            cell = Short(cell)
        );
        // A cell with no capacity yet takes its cell's.
        if made.capacity.is_none() && self.capacity(made.parent).is_none() {
            made.capacity = Some(CAPACITY);
        }
        let session = self.hosts[host].sessions.get_mut(&cell).unwrap();
        self.lineages.get_mut(&session.lineage).unwrap().insert(epoch, tag);
        for (extent, bytes) in &made.extents {
            session.guest.held.insert(*extent, bytes.clone());
        }
        for extent in &made.frees {
            session.guest.held.remove(extent);
        }
        session.guest.head = Some(tag);
        self.commits.insert(tag, made);
    }

    async fn commit(&mut self) {
        let Some((host, cell)) = self.ready() else { return };
        let tag = self.id();
        for _ in 0..self.rng.random_range(0..3) {
            if !self.write_extent(host, cell, tag).await {
                return;
            }
        }
        let session = &self.hosts[host].sessions[&cell];
        let held: Vec<ExtentId> =
            session.guest.held.keys().chain(session.guest.written.keys()).copied().collect();
        let mut frees = BTreeSet::new();
        for _ in 0..self.rng.random_range(0..3) {
            if !held.is_empty() {
                frees.insert(held[self.rng.random_range(0..held.len())]);
            }
        }
        let root = self.rng.random_bool(0.2).then(|| tag.to_le_bytes().to_vec());
        // Every fifth by its tag resizes the cell first, a commit of its own, which seals none
        // of what the guest wrote: by the tag, not the generator (TASK-165.3).
        if tag.is_multiple_of(5) && !self.resize(host, cell, tag << 20).await {
            return;
        }
        let session = &self.hosts[host].sessions[&cell];
        // Durable at once, or later, with a later commit's, or a closing's.
        let durable = self.rng.random_bool(0.5);
        let open_cell = session.cell.clone().unwrap();
        let parent = session.guest.head;
        let committing = async || -> anyhow::Result<u64> {
            let before = open_cell.last_commit()?;
            for extent in &frees {
                open_cell.free(*extent)?;
            }
            open_cell.commit_ordered(root.as_deref()).await?;
            if durable {
                open_cell.flush().await?;
            }
            Ok(before)
        };
        let before = match committing().await {
            Ok(before) => before,
            Err(err) => return self.cell_failed(host, cell, open_cell, err),
        };
        let Some(epoch) = self.committed(cell, &open_cell, before) else { return };
        let session = self.hosts[host].sessions.get_mut(&cell).unwrap();
        let extents = std::mem::take(&mut session.guest.written).into_iter().collect();
        let frees = frees.into_iter().collect();
        let made = Truth { cell, parent, extents, frees, root, capacity: None };
        self.made(host, cell, tag, epoch, made);
    }

    /// The guest of `cell`'s cell on `host` sets its capacity to `capacity`: a commit of its
    /// own, which seals none of what it wrote. Whether it could.
    async fn resize(&mut self, host: usize, cell: u64, capacity: u64) -> bool {
        let tag = self.id();
        let session = &self.hosts[host].sessions[&cell];
        let (open_cell, parent) = (session.cell.clone().unwrap(), session.guest.head);
        let resizing = async || -> anyhow::Result<u64> {
            let before = open_cell.last_commit()?;
            open_cell.set_capacity(capacity).await?;
            Ok(before)
        };
        let before = match resizing().await {
            Ok(before) => before,
            Err(err) => {
                self.cell_failed(host, cell, open_cell, err);
                return false;
            }
        };
        let Some(epoch) = self.committed(cell, &open_cell, before) else { return false };
        let capacity = Some(capacity);
        let made =
            Truth { cell, parent, extents: Vec::new(), frees: Vec::new(), root: None, capacity };
        self.made(host, cell, tag, epoch, made);
        true
    }

    /// A cell's uploader is asked for its commits, as a commit waiting for its upload asks, or its
    /// interval runs out: its open segment closes, and it uploads.
    fn close(&mut self) {
        let Some((host, cell)) = self.ready() else { return };
        let session = &self.hosts[host].sessions[&cell];
        if self.rng.random_bool(0.5) {
            session.intervals.run_out();
            return;
        }
        let (open_cell, uploader) = (session.cell.clone().unwrap(), session.uploader.clone());
        let asked = open_cell
            .last_commit()
            .map_err(Failure::from)
            .and_then(|epoch| uploader.ask(epoch, Awaited::Uploaded));
        if let Err(failure) = asked {
            drop(uploader);
            self.cell_failed(host, cell, open_cell, anyhow::Error::new(failure));
        }
    }

    fn checkpoint(&mut self) {
        let Some((host, cell)) = self.ready() else { return };
        let session = &self.hosts[host].sessions[&cell];
        let (id, writer) = (session.id, session.uploader.writer().unwrap());
        let events = self.events.clone();
        self.spawn(async move {
            if let Err(error) = writer.checkpoint().await {
                events.borrow_mut().push(Event::Failed { host, cell, session: id, error });
            }
        });
    }

    /// cladefs: a cell's writer relocates out of its sparse segments, as its uploader's rounds do
    /// (`relocations::round`), of an allowance of a few blocks to many segments: what a round of
    /// its own would copy at once, its cell's frees and commits going on beside it (doc-27).
    fn relocate(&mut self) {
        let Some((host, cell)) = self.ready() else { return };
        let session = &self.hosts[host].sessions[&cell];
        let (open_cell, writer) =
            (session.cell.clone().unwrap(), session.uploader.writer().unwrap());
        let (bucket, uploads) = (self.host_bucket(host), self.hosts[host].uploads.clone());
        let allowance = self.rng.random_range(1..=64) << 14;
        let (lineage, events) = (session.lineage, self.events.clone());
        let round = self.spawn(async move {
            let pins = crate::relocations::Pins::default();
            // A round failing fails nothing (doc-27): it says so in the trace alone.
            let budgets =
                crate::relocations::Budgets { uploads: &uploads, fetching: None, cache: None };
            let places = (&bucket, budgets);
            let ended =
                crate::relocations::round(&open_cell, &writer, places, &pins, allowance).await;
            trace!("cell {cell} relocated {ended:?}", cell = Short(cell));
            if let Some(epoch) = crate::relocations::relocated(&ended).commit {
                events.borrow_mut().push(Event::Relocated { lineage, cell, epoch });
            }
        });
        // Its uploader's, as its own rounds are, which its stopping waits for: ended with the
        // session, holding its cell no more.
        self.hosts[host].sessions.get_mut(&cell).unwrap().keepers.push(round);
    }

    /// A writer takes a cell on a host: one with no writer, or, `takeover`, one with. Only a
    /// cell with a record: as a host opens the cells it was told of.
    fn open(&mut self, takeover: bool) {
        let direct = Direct(self.bucket.clone());
        let candidates: Vec<u64> = self
            .cells
            .iter()
            .filter(|(_, found)| !found.deleting)
            .filter(|(cell, _)| {
                let open = self.hosts.iter().any(|host| host.sessions.contains_key(cell));
                open == takeover
            })
            .filter(|&(&cell, _)| {
                matches!(block_on(bucket_cell::head::get(&direct, sim_cell(cell))), Ok(Some(_)))
            })
            .map(|(&cell, _)| cell)
            .collect();
        if candidates.is_empty() {
            return;
        }
        let cell = candidates[self.rng.random_range(0..candidates.len())];
        let elsewhere: Vec<usize> = (0..self.hosts.len())
            .filter(|&host| !self.hosts[host].sessions.contains_key(&cell))
            .collect();
        if elsewhere.is_empty() {
            return;
        }
        // Mostly where its cell is.
        let with_disk: Vec<usize> = elsewhere
            .iter()
            .copied()
            .filter(|&host| self.hosts[host].disks.contains_key(&cell))
            .collect();
        let pool =
            if !with_disk.is_empty() && self.rng.random_bool(0.7) { with_disk } else { elsewhere };
        let host = pool[self.rng.random_range(0..pool.len())];
        if takeover && self.profile.hold > 0.0 && self.rng.random_bool(self.profile.hold) {
            let writers = self.hosts.iter().filter_map(|host| {
                host.sessions.get(&cell)?.uploader.writer().map(|writer| writer.epoch())
            });
            if let Some(epoch) = writers.max() {
                self.bucket.hold(sim_cell(cell), epoch);
            }
        }
        self.start_session(host, cell);
    }

    /// A host crashes: its uploaders stop where they are, what they had in flight landing or not;
    /// its disk keeps what its cells made durable, and some of what they did since, or is lost.
    fn crash(&mut self) {
        let host = self.rng.random_range(0..self.hosts.len());
        trace!("host {host} crashes");
        let sessions = std::mem::take(&mut self.hosts[host].sessions);
        for session in sessions.values() {
            session.running.abort();
            for keeper in &session.keepers {
                keeper.abort();
            }
        }
        let lost = self.rng.random_bool(0.3);
        let corrupting = self.profile.corrupting;
        if lost {
            self.hosts[host].disks.clear();
        } else {
            let disks = &mut self.hosts[host].disks;
            let mut keys: Vec<u64> = disks.keys().copied().collect();
            keys.sort_unstable();
            for cell in keys {
                let seed = self.rng.random();
                let disk = disks.get_mut(&cell).unwrap();
                disk.dir = match corrupting {
                    true => disk.dir.crash_corrupting(seed),
                    false => disk.dir.crash(seed),
                };
            }
        }
        let cells: Vec<u64> = sessions.keys().copied().collect();
        // Their cells close on what the crash left behind.
        for (cell, session) in sessions {
            self.stop(host, cell, session);
        }
        if self.profile.restarts {
            for cell in cells {
                let kept = self.hosts[host].disks.contains_key(&cell);
                if kept && !self.cells[&cell].deleting {
                    self.start_session(host, cell);
                }
            }
        }
        // Whichever host's: what they had in flight may land still.
        for unfork in self.unforks.values_mut() {
            if let Some(running) = unfork.running.take() {
                running.abort();
            }
        }
        for restore in self.restores.values_mut().filter(|restore| restore.host == host) {
            if let Some(running) = restore.running.take() {
                running.abort();
            }
        }
        // cladefs: its mirrors, their pins left to go stale, and what its process knew of them
        // (TASK-633.8).
        for (_, running) in std::mem::take(&mut self.hosts[host].mirrors) {
            running.abort();
        }
        self.hosts[host].known = Default::default();
    }

    /// `child`, a fork of a version that failed, or a crash left, goes as its host's cleanup
    /// deletes it, or an operator does what no host held: which nothing relies on.
    fn fail_restore(&mut self, child: u64, restore: Restore) {
        let mut found = CellId::forked(Some(restore.origin), restore.base);
        found.deleting = true;
        self.cells.insert(child, found);
        self.unforks.insert(child, Unfork { owner: restore.owner, running: None });
        self.spawn_unfork(child);
    }

    /// Makes the fork `child`, or goes on with one a crash stopped, as opening its handle does.
    fn spawn_restore(&mut self, child: u64) {
        let Restore { version, origin, owner, host, .. } = self.restores[&child];
        trace!("host {host}: forks version {} to {}", Short(version), Short(child));
        let (bucket, events) = (self.host_bucket(host), self.events.clone());
        let running = self.spawn(async move {
            let child = sim_cell(child);
            let kms = bucket_cell::keys::testing::host_kms();
            let keys = bucket_cell::keys::Parents::Kms(&kms, None);
            let result =
                lifecycle::fork_version(&*bucket, child, version, origin, owner, keys).await;
            let child = child.id;
            events.borrow_mut().push(Event::Restored { child, result });
        });
        self.restores.get_mut(&child).unwrap().running = Some(running);
    }

    /// cladefs: host `host`'s mirror into its bucket of the version `child` was forked from:
    /// `origin`, or as `child`'s head names it (`bucket_cell::mirror::wanted`, TASK-633.8), as the
    /// child's fork on the host, or its cell's opening there, begins it; stopped by the host's
    /// crash. Nothing without regions.
    fn spawn_mirror(&mut self, host: usize, child: u64, origin: Option<Origin>) {
        if self.profile.regions == 0 {
            return;
        }
        let (id, number) = (self.id(), self.region_of(host));
        trace!(
            "host {host}: mirror {id} of the version {child} was forked from, {origin:?}",
            child = Short(child)
        );
        let (bucket, events) = (self.host_bucket(host), self.events.clone());
        // Its turn taken as it is asked for, a fork's, as `ObjectBucket::mirror_wanted` takes it;
        // an opening's once it read the child's head, as `ObjectBucket::mirror_for` does.
        let known = self.hosts[host].known.clone();
        let taken = origin.map(|origin| (origin, known.turn(origin)));
        let running = self.spawn(async move {
            let child = sim_cell(child);
            let result = async {
                let (origin, turn) = match taken {
                    Some(taken) => taken,
                    None => {
                        let head = bucket_cell::head::get(&*bucket, child).await?;
                        let Some(origin) = head.and_then(|head| head.origin()) else {
                            return Ok(None);
                        };
                        (origin, known.turn(origin))
                    }
                };
                let Some(turn) = turn else { return Ok(None) };
                turn.mirror(&*bucket, &*bucket.own(), number, child, origin).await
            };
            let result = result.await;
            events.borrow_mut().push(Event::Mirrored { host, id, result });
        });
        self.hosts[host].mirrors.insert(id, running);
    }

    /// cladefs: a cell's writer cuts a version (doc-18, TASK-476), its cell quiescent, its guest
    /// having written nothing since its last commit: a commit of its cell's whose segment's
    /// footer names it, which its uploader is asked for, as a snapshot's caller may wait for it.
    async fn cut_version(&mut self) {
        let Some((host, cell)) = self.ready() else { return };
        let session = &self.hosts[host].sessions[&cell];
        if !session.guest.written.is_empty() {
            return;
        }
        let (open_cell, uploader, parent) =
            (session.cell.clone().unwrap(), session.uploader.clone(), session.guest.head);
        let (id, tag) = (self.id(), self.id());
        let before = match open_cell.last_commit() {
            Ok(before) => before,
            Err(err) => return self.cell_failed(host, cell, open_cell, err),
        };
        let cut = layout::NewVersion { id, keep_last: None };
        if let Err(err) = open_cell.commit_version(cut, true).await {
            return self.cell_failed(host, cell, open_cell, err);
        }
        let Some(epoch) = self.committed(cell, &open_cell, before) else { return };
        let made = Truth {
            cell,
            parent,
            extents: Vec::new(),
            frees: Vec::new(),
            root: None,
            capacity: None,
        };
        self.made(host, cell, tag, epoch, made);
        let cut = Cut { cell, tag, dropping: false, dropped: false, dropped_by: 0 };
        self.versions.insert(id, cut);
        trace!(
            "host {host}: {cell} cuts version {id} as commit {tag} at epoch {epoch}",
            cell = Short(cell),
            id = Short(id)
        );
        let _ = uploader.ask(epoch, Awaited::Uploaded);
    }

    /// cladefs: a cell's writer drops one of its versions, uploaded or local, which its cell
    /// forgets: told dropped once its swap of the head is done.
    fn drop_version(&mut self) {
        let Some((host, cell)) = self.ready() else { return };
        let ids: Vec<u64> = (self.versions.iter())
            .filter(|(_, cut)| cut.cell == cell && !cut.dropped && !cut.dropping)
            .map(|(&id, _)| id)
            .collect();
        if ids.is_empty() {
            return;
        }
        let id = ids[self.rng.random_range(0..ids.len())];
        let session = &self.hosts[host].sessions[&cell];
        let (writer, events) = (session.uploader.writer().unwrap(), self.events.clone());
        let session_id = session.id;
        // Gone from the head as soon as its swap lands, which its writer may be told of later.
        let cut = self.versions.get_mut(&id).unwrap();
        (cut.dropping, cut.dropped_by) = (true, writer.epoch());
        trace!(
            "host {host}: {cell} drops version {id} at writer epoch {epoch}",
            cell = Short(cell),
            id = Short(id),
            epoch = writer.epoch()
        );
        let ids = BTreeSet::from([id]);
        session.cell.as_ref().unwrap().forget_versions(&ids);
        self.spawn(async move {
            let result = writer.drop_versions(&ids).await;
            let event = Event::VersionDropped { id, result, host, session: session_id };
            events.borrow_mut().push(event);
        });
    }

    /// cladefs: the versions the heads of the cells with versions name, each by its ID, of which
    /// cell, the segment it is at and the commit it is of: as a host restoring one read it.
    fn named_versions(&self) -> Vec<(u64, u64, u64, u64, u64)> {
        let direct = Direct(self.bucket.clone());
        let cells: BTreeSet<u64> = self.versions.values().map(|cut| cut.cell).collect();
        let mut named = Vec::new();
        for cell in cells {
            let Ok(Some(head)) = block_on(bucket_cell::head::get(&direct, sim_cell(cell))) else {
                continue;
            };
            for version in &head.versions {
                if let Some(cut) = self.versions.get(&version.id) {
                    named.push((version.id, cell, version.at, version.epoch, cut.tag));
                }
            }
        }
        named
    }

    /// cladefs: a restore of a version a cell's head names, on a random host, of an owner of its
    /// own, not yet begun; none if no head names one.
    fn new_restore(&mut self) -> Option<Restore> {
        let named = self.named_versions();
        if named.is_empty() {
            return None;
        }
        let (version, cell, at, epoch, tag) = named[self.rng.random_range(0..named.len())];
        let (host, owner) = (self.rng.random_range(0..self.hosts.len()), self.id());
        let origin = Origin { parent: sim_cell(cell), at, epoch };
        Some(Restore { version, origin, base: Some(tag), host, owner, running: None })
    }

    /// cladefs: a host restores a version a cell's head names: its child's record, its reference,
    /// the head read again, the fork made (`lifecycle::fork_version`), as many hosts do at once.
    fn restore_version(&mut self) {
        let Some(restore) = self.new_restore() else { return };
        let child = self.cell_id(self.region_of(restore.host));
        self.restores.insert(child, restore);
        self.spawn_restore(child);
    }

    /// cladefs: two hosts restore one version to one child within a round trip, as guests may name
    /// any ID, each of an owner of its own: whichever's record comes first takes the ID, the other
    /// refused, finding another's (TASK-79.82, TASK-173). Crashes leave them be.
    fn twins(&mut self) {
        let Some(first) = self.new_restore() else { return };
        let (child, owner) = (self.cell_id(self.region_of(first.host)), self.id());
        let second = Restore { owner, running: None, ..first };
        let (bucket, events) = (self.host_bucket(first.host), self.events.clone());
        self.spawn(async move {
            let kms = bucket_cell::keys::testing::host_kms();
            let fork = |restore: &Restore| {
                let keys = bucket_cell::keys::Parents::Kms(&kms, None);
                let Restore { version, origin, owner, .. } = *restore;
                lifecycle::fork_version(&*bucket, sim_cell(child), version, origin, owner, keys)
            };
            let made = futures::join!(fork(&first), fork(&second));
            let made = [(first, made.0), (second, made.1)];
            events.borrow_mut().push(Event::Twinned { child, made });
        });
    }

    /// Goes on with what a crash left, as its host's next start does: a failed fork's deletion
    /// ([`lifecycle::delete_fork`]), or a fork of a version, finished, deleted or, as often as
    /// [`Profile::forget`] says, forgotten.
    fn unfork(&mut self) {
        let stopped = self.unforks.iter().find(|(_, unfork)| unfork.running.is_none());
        if let Some((&child, _)) = stopped {
            return self.spawn_unfork(child);
        }
        let stopped = self.restores.iter().find(|(_, restore)| restore.running.is_none());
        let Some((&child, _)) = stopped else { return };
        let forget = self.profile.forget;
        if forget > 0.0 && self.rng.random_bool(forget) {
            self.restores.remove(&child);
            return;
        }
        match self.rng.random_bool(0.5) {
            true => self.spawn_restore(child),
            false => self.give_up_restore(child),
        }
    }

    /// Deletes what a crash left of the fork of a version to `child`.
    fn give_up_restore(&mut self, child: u64) {
        let restore = self.restores.remove(&child).unwrap();
        self.fail_restore(child, restore);
    }

    /// Deletes `child`, a failed fork's, as its host does.
    fn spawn_unfork(&mut self, child: u64) {
        let (owner, holder) = (self.unforks[&child].owner, self.id());
        let (bucket, events) = (self.cell_bucket(child), self.events.clone());
        let running = self.spawn(async move {
            let deleting = lifecycle::delete_fork(&*bucket, sim_cell(child), owner, (holder, 0));
            let result = deleting.await;
            events.borrow_mut().push(Event::Unforked { child, result });
        });
        self.unforks.get_mut(&child).unwrap().running = Some(running);
    }

    /// Deletes a cell, if `bases`, one a fork of which lives.
    fn delete(&mut self, bases: bool) {
        let live = |found: &CellId| !found.deleting;
        // Keep a cell or two.
        if self.cells.values().filter(|found| live(found)).count() < 3 {
            return;
        }
        let candidates: Vec<u64> = self
            .cells
            .iter()
            .filter(|(_, found)| live(found))
            .filter(|&(&cell, _)| {
                !bases
                    || self.cells.values().any(|child| {
                        live(child)
                            && child.origin.is_some_and(|origin| origin.parent == sim_cell(cell))
                    })
            })
            .map(|(&cell, _)| cell)
            .collect();
        if candidates.is_empty() {
            return;
        }
        let cell = candidates[self.rng.random_range(0..candidates.len())];
        let found = self.cells.get_mut(&cell).unwrap();
        found.deleting = true;
        // Its latest placement's, as far as anything here knows: one taking it meanwhile is later.
        let token = found.taken;
        self.spawn_delete(cell, token);
    }

    /// Deletes `cell` with the fencing token `token`.
    fn spawn_delete(&mut self, cell: u64, token: u64) {
        let holder = self.id();
        let (bucket, events) = (self.cell_bucket(cell), self.events.clone());
        self.spawn(async move {
            let result = lifecycle::delete(&*bucket, sim_cell(cell), holder, token).await;
            events.borrow_mut().push(Event::Deleted { cell, result });
        });
    }

    /// A sweep of the collector over every cell, or, as often as [`Profile::one_cell`] says, a pass
    /// over one cell alone (TASK-395.4): passes over one cell and another overlap, as over
    /// one cell.
    fn collect(&mut self) {
        if self.collectors.len() >= 2 {
            return;
        }
        let id = self.id();
        let (bucket, events) = (self.bucket.clone(), self.events.clone());
        // cladefs: a region's collector, of its own bucket (TASK-633.10).
        let bucket = match self.profile.regions {
            0 => bucket,
            regions => bucket.at(self.rng.random_range(1..=regions)),
        };
        let cells: Vec<u64> = self.cells.keys().copied().collect();
        let one_cell = self.profile.one_cell;
        let cell = (!cells.is_empty() && one_cell > 0.0 && self.rng.random_bool(one_cell))
            .then(|| sim_cell(cells[self.rng.random_range(0..cells.len())]));
        let handle = self.spawn(async move {
            let result = match cell {
                // cladefs: a region's collector passes over copies of other buckets' cells as
                // copies (`collect_copy`), never as cells of its own (board 10446).
                Some(cell)
                    if bucket.home() != 0
                        && layout::routed_to(&cell.prefix())
                            .is_some_and(|of| of != bucket.home()) =>
                {
                    collector::Collector::new()
                        .collect_copy(&*bucket, &*bucket.own(), bucket.home(), cell)
                        .await
                }
                Some(cell) => collector::collect_cell(&*bucket, cell, id).await,
                None => sweep(&bucket, id).await,
            };
            events.borrow_mut().push(Event::Collected { id, result });
        });
        self.collectors.insert(id, handle);
    }

    fn stop_collector(&mut self) {
        if let Some((&id, _)) = self.collectors.iter().next() {
            self.collectors.remove(&id).unwrap().abort();
        }
    }

    /// The commits of `cell` up to `head`, oldest first.
    fn own_path(&self, cell: u64, head: Option<u64>) -> Vec<u64> {
        let mut path = Vec::new();
        let mut at = head;
        while let Some(tag) = at.filter(|tag| self.commits[tag].cell == cell) {
            path.push(tag);
            at = self.commits[&tag].parent;
        }
        path.reverse();
        path
    }

    /// The extents and root the commits up to `head` leave.
    fn expected(&self, head: Option<u64>) -> (BTreeMap<ExtentId, Vec<u8>>, Option<Vec<u8>>) {
        let mut path = Vec::new();
        let mut at = head;
        while let Some(tag) = at {
            path.push(tag);
            at = self.commits[&tag].parent;
        }
        let mut extents = BTreeMap::new();
        let mut root = None;
        for tag in path.into_iter().rev() {
            let truth = &self.commits[&tag];
            for (extent, bytes) in &truth.extents {
                extents.insert(*extent, bytes.clone());
            }
            for extent in &truth.frees {
                extents.remove(extent);
            }
            if truth.root.is_some() {
                root = truth.root.clone();
            }
        }
        (extents, root)
    }

    /// The capacity the commits up to `head` leave: the last one set, a fork's parent's before it.
    fn capacity(&self, head: Option<u64>) -> Option<u64> {
        let mut at = head;
        while let Some(tag) = at {
            let truth = &self.commits[&tag];
            if truth.capacity.is_some() {
                return truth.capacity;
            }
            at = truth.parent;
        }
        None
    }

    /// The sequence numbers of `cell`'s own extents that the commits up to `head` freed once a
    /// commit before held them, which it writes no more (TASK-79.6, TASK-165.3): not those a commit
    /// sealed and freed itself, which no commit held.
    fn spent(&self, cell: u64, head: Option<u64>) -> Spent {
        let mut spent = Spent::default();
        let mut at = head;
        while let Some(tag) = at {
            let truth = &self.commits[&tag];
            let sealed =
                |extent: &ExtentId| truth.extents.iter().any(|(sealed, ..)| sealed == extent);
            for extent in truth.frees.iter().filter(|extent| extent.cell == cell) {
                if !sealed(extent) {
                    spent.insert(extent.sequence);
                }
            }
            at = truth.parent;
        }
        spent
    }

    /// Whether `ancestor` is `head` or before it.
    fn precedes(&self, ancestor: u64, head: Option<u64>) -> bool {
        let mut at = head;
        while let Some(tag) = at {
            if tag == ancestor {
                return true;
            }
            at = self.commits[&tag].parent;
        }
        false
    }

    /// Requests complete, without failing, until none is left, those held too.
    fn settle(&mut self) {
        self.bucket.release();
        let mut completed = 0;
        loop {
            self.settle_tasks();
            if self.bucket.waiting() == 0 {
                return;
            }
            self.bucket.complete(0, Fault::None);
            trace!("{}", self.bucket.journal().last().unwrap());
            // cladefs: within a bound, as FoundationDB's runs must quiet down once their faults
            // stop: a world whose tasks ask for requests for ever, no fault answering them,
            // livelocks, which hung the run before (doc-24).
            completed += 1;
            if completed > SETTLE_MOST {
                let waiting = self.bucket.waiting();
                self.violation(format!(
                    "the world doesn't settle: {SETTLE_MOST} requests completed, every one \
                     answered, and {waiting} more waiting: a livelock"
                ));
                return;
            }
        }
    }

    /// The live cells, by their records.
    fn live(&self) -> Vec<u64> {
        let direct = Direct(self.bucket.clone());
        self.cells
            .keys()
            .copied()
            .filter(|&id| {
                let head = block_on(bucket_cell::head::get(&direct, sim_cell(id)));
                matches!(head, Ok(Some(head)) if !head.deleted())
            })
            .collect()
    }

    /// Checks that every task is done once everything settled, every request done and nothing
    /// failing (TASK-149.32): an upload, an acknowledgement, a checkpoint, a fork, a deletion or a
    /// collection still waiting then waits for a wake nothing will give. Its run would only end,
    /// its check reading nothing of what it waited to do.
    fn check_woken(&mut self) {
        self.settle();
        let waiting: Vec<String> = (self.tasks.borrow().values())
            .map(|spawned| format!("{}:{}", spawned.file(), spawned.line()))
            .collect();
        if !waiting.is_empty() {
            self.violation(format!("tasks still wait, everything settled: spawned at {waiting:?}"));
        }
    }

    /// Checks that a sweep of the collector that ran alone, the bucket holding `before` as it
    /// started, the objects' sizes by their keys, reports what it did (TASK-44.1): the objects it
    /// deleted. cladefs: and no more references gone than it retired, whether or not they were
    /// there (TASK-395.4), and deleted marked (TASK-395.13); and the bytes of what it deleted, as
    /// they were (TASK-395.5).
    fn check_report(
        &mut self,
        before: &BTreeMap<String, u64>,
        report: Option<Result<collector::Report, Error>>,
    ) {
        // cladefs: by where each is kept ([`SimBucket::stored`]): a copy in one region's bucket and
        // its original in another's apart (TASK-633.8).
        let after: BTreeSet<String> = self.bucket.stored().into_iter().collect();
        fn kind(stored: &str) -> layout::Kind {
            layout::kind(key_of(stored))
        }
        let reference = |key: &&String| matches!(kind(key), layout::Kind::Reference { .. });
        // The collector's leases, its own, which it reports not. cladefs: and the pins and holds of
        // mirrors, theirs, which a pass over a copy deletes, or a mirror it begins again lets go,
        // and the marks of references retired in a copy (TASK-633.8), unreported:
        // [`bucket_cell::testing::uncollected_copies`] checks them.
        let copy = |stored: &str| {
            let region = stored.strip_prefix('@').and_then(|rest| rest.split_once('/'));
            let region = region.and_then(|(region, _)| region.parse::<u16>().ok());
            layout::routed_to(key_of(stored)).is_some_and(|of| Some(of) != region)
        };
        let lease = |key: &&String| match kind(key) {
            layout::Kind::CollectorLease { .. } | layout::Kind::Pin { .. } => true,
            layout::Kind::Hold { .. } => true,
            layout::Kind::Retired { .. } => copy(key),
            _ => false,
        };
        let gone = || before.keys().filter(|key| !after.contains(*key) && !lease(key));
        let retired = gone().filter(reference).count();
        let markers = gone().filter(|key| matches!(kind(key), layout::Kind::Mirror { .. }));
        *self.mirrored.entry("collected").or_default() += markers.count();
        let expected = |reported: &collector::Report| collector::Report {
            deleted: gone().filter(|key| !reference(key)).count(),
            retired: reported.retired.max(retired),
            deleted_bytes: gone().filter(|key| !reference(key)).map(|key| before[key]).sum(),
            // Which passes skipped their cells, unchanged, the bucket can't tell (TASK-395.6).
            skipped: reported.skipped,
            // cladefs: the cells whose heads went, as only a dead cell's removal deletes them
            // (TASK-455).
            removed: gone()
                .filter_map(|key| match kind(key) {
                    layout::Kind::Head { cell, .. } => Some(cell),
                    _ => None,
                })
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
        };
        match report {
            Some(Ok(report)) if report == expected(&report) => {}
            Some(Ok(report)) => {
                let expected = expected(&report);
                let gone: Vec<&String> = gone().collect();
                self.violation(format!(
                    "collector reported {report:?}, having done {expected:?}: gone {gone:#?}"
                ))
            }
            Some(Err(error)) => self.violation(format!("collector: {error}")),
            None => self.violation("a collector alone did not finish".to_owned()),
        }
    }

    /// What the bucket holds and what the live cells name, for a failure's message.
    pub fn describe(&self) -> String {
        let direct = Direct(self.bucket.clone());
        let mut text = String::new();
        for cell in self.live() {
            let origin = self.cells[&cell].origin;
            match block_on(recovery::recover(&direct, sim_cell(cell), origin)) {
                Ok(recovered) => {
                    let keys: BTreeSet<_> =
                        recovered.state.extents.values().map(|location| &location.key).collect();
                    text += &format!(
                        "cell {cell} from {origin:?}: checkpoint {:?}, end {}, epoch {}, names {keys:?}\n",
                        recovered.checkpoint(),
                        recovered.end(),
                        recovered.epoch(),
                        cell = Short(cell)
                    );
                }
                Err(error) => text += &format!("cell {cell}: {error}\n", cell = Short(cell)),
            }
        }
        for key in self.bucket.keys() {
            text += &format!("  {key}\n");
            if let layout::Kind::Segment { .. } = layout::kind(&key)
                && let Ok(object) = block_on(bucket_cell::bucket::Bucket::get(&direct, &key))
                && let Ok(footer) = layout::Segment::footer(&object.bytes)
            {
                let short = |extents: &mut dyn Iterator<Item = ExtentId>| {
                    extents
                        .map(|extent| Short(extent.sequence).to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                };
                text += &format!(
                    "    epoch {} commits {:?} extents [{}] own [{}] frees [{}]\n",
                    footer.epoch,
                    footer.contents.commits,
                    short(&mut footer.contents.extents.iter().map(|e| e.0)),
                    short(&mut footer.own().map(|e| e.0)),
                    short(&mut footer.contents.frees.iter().copied()),
                );
            }
        }
        let mut stamps: Vec<_> = self.stamps.iter().collect();
        stamps.sort();
        for ((cell, writer), lineage) in stamps {
            text += &format!(
                "writer {writer} of {cell} uploads history {lineage}\n",
                cell = Short(*cell)
            );
        }
        let mut lineages: Vec<_> = self.lineages.iter().collect();
        lineages.sort();
        for (lineage, commits) in lineages {
            text += &format!("history {lineage}: commits by the cell's epochs {commits:?}\n");
        }
        for (tag, truth) in &self.commits {
            let short = |extents: &mut dyn Iterator<Item = ExtentId>| {
                extents
                    .map(|extent| Short(extent.sequence).to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            };
            text += &format!(
                "commit {tag} of {} after {:?}: extents [{}] frees [{}]\n",
                Short(truth.cell),
                truth.parent,
                short(&mut truth.extents.iter().map(|e| e.0)),
                short(&mut truth.frees.iter().copied()),
            );
        }
        text
    }

    /// Checks that the bucket holds only what live cells need (TASK-44.1):
    /// [`bucket_cell::testing::uncollected`], its live cells by their records.
    fn check_collected(&mut self) {
        let direct = Direct(self.bucket.clone());
        for left in block_on(bucket_cell::testing::uncollected(&direct)) {
            self.violation(left);
        }
        // cladefs: and what each region's bucket keeps of other buckets' cells (TASK-633.8).
        for region in (1..=self.profile.regions).collect::<Vec<_>>() {
            let (routed, own) =
                (Direct(self.bucket.at(region)), Direct(self.bucket.at(region).own()));
            for left in block_on(bucket_cell::testing::uncollected_copies(&routed, &own, region)) {
                self.violation(left);
            }
        }
    }

    fn complete(&mut self) {
        let waiting = self.bucket.waiting();
        if waiting == 0 {
            return;
        }
        let index = self.rng.random_range(0..waiting);
        // cladefs: a request to another region's bucket takes longer than one to its asker's own:
        // passed over, at times, its turn going by (TASK-633.10).
        if self.profile.regions > 0
            && self.bucket.crosses(index)
            && self.rng.random_bool(CROSSING_WAITS)
        {
            return;
        }
        let roll: f64 = self.rng.random();
        let fault = if !self.faults {
            Fault::None
        } else if roll < self.profile.fail_before {
            Fault::Before
        } else if roll < self.profile.fail_before + self.profile.fail_after {
            Fault::After
        } else {
            Fault::None
        };
        self.bucket.complete(index, fault);
        trace!("{}", self.bucket.journal().last().unwrap());
    }

    /// Spawns `task`, counted among the [`World::tasks`] until it is done or aborted, by where
    /// it was spawned.
    #[track_caller]
    fn spawn(&mut self, task: impl Future<Output = ()> + 'static) -> AbortHandle {
        let (task, handle) = abortable(task);
        self.next_task += 1;
        let number = self.next_task;
        self.tasks.borrow_mut().insert(number, std::panic::Location::caller());
        let running = Running { tasks: self.tasks.clone(), number };
        self.spawner
            .spawn_local(async move {
                let _running = running;
                let _ = task.await;
            })
            .unwrap();
        handle
    }

    /// The bytes of `extent`, `len` of them, as the write `tag` writes them: a pattern of both,
    /// which compresses, as a guest's data mostly does.
    fn content(tag: u64, extent: ExtentId, len: usize) -> Vec<u8> {
        let seed =
            tag.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ extent.sequence.rotate_left(17) ^ extent.cell;
        (0..len).map(|i| (seed.rotate_left(i as u32 % 64) >> 3) as u8 ^ i as u8).collect()
    }

    /// cladefs: a new cell's ID, in the bucket of region `region` if the world has regions
    /// ([`Profile::regions`]).
    fn cell_id(&mut self, region: u16) -> u64 {
        let id = self.id();
        match self.profile.regions {
            0 => id,
            _ => u64::from(region) << 48 | id,
        }
    }

    /// cladefs: the bucket as host `host` asks it, of its region's ([`Profile::regions`]).
    fn host_bucket(&self, host: usize) -> Arc<SimBucket> {
        match self.profile.regions {
            0 => self.bucket.clone(),
            _ => self.bucket.at(self.region_of(host)),
        }
    }

    /// cladefs: the bucket as a host of the region of `cell`'s bucket asks it.
    fn cell_bucket(&self, cell: u64) -> Arc<SimBucket> {
        match self.profile.regions {
            0 => self.bucket.clone(),
            _ => self.bucket.at(bucket_cell::objectid::bucket_of(cell).max(1)),
        }
    }

    /// cladefs: the region of host `host`'s bucket ([`Profile::regions`]).
    fn region_of(&self, host: usize) -> u16 {
        match self.profile.regions {
            0 => 0,
            regions => 1 + (host % usize::from(regions)) as u16,
        }
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn violation(&mut self, violation: String) {
        self.violations.push(violation);
    }

    // The truth.

    /// The commits the segments of the cell `recovered` is since its newest checkpoint make, in
    /// the order recovering it applied them: each segment its head names, by its number and epoch.
    fn applied(&self, direct: &Direct, recovered: &Recovered) -> Result<Vec<u64>, String> {
        let cell = recovered.state.cell;
        let mut footers = self.footers.borrow_mut();
        let mut applied = Vec::new();
        let from = recovered.checkpoint().map_or(0, |(segment, _)| segment + 1);
        for (segment, epoch) in recovered.head.iter().flat_map(|head| head.segments(from)) {
            let commits = match footers.entry((cell.id, segment, epoch)) {
                Entry::Occupied(known) => known.into_mut(),
                Entry::Vacant(unknown) => {
                    let key = layout::segment(cell, segment, epoch);
                    let got = block_on(bucket_cell::bucket::get(direct, &key));
                    let Some(object) = got.map_err(|err| err.to_string())? else {
                        return Err(format!(
                            "cell {}: segment {segment} gone",
                            Short(cell.id),
                            segment = Short(segment)
                        ));
                    };
                    let footer = layout::Segment::footer(&object.bytes)
                        .map_err(|error| format!("{key}: {error}"))?;
                    unknown.insert(footer.contents.commits)
                }
            };
            applied.push(commits.clone());
        }
        // cladefs: a relocation's commit holds the state of the commit before it, under a tag of
        // its own (doc-27, [`Event::Relocated`]): one commit of the guest's, applied once.
        let mut truths: Vec<(u64, u64)> = Vec::new();
        for tag in applied.concat() {
            let truth = self.truth_of(cell.id, Some(tag))?.unwrap();
            match truths.last() {
                Some(&(before, held)) if held == truth && before != tag => {}
                _ => truths.push((tag, truth)),
            }
        }
        Ok(truths.into_iter().map(|(_, truth)| truth).collect())
    }

    /// Checks that every live cell holds what its commits made, up to one of its own, past
    /// everything its guests were told was durable, and that the current writers, and their
    /// cells, read what their guests hold.
    pub fn check(&mut self) {
        let direct = Arc::new(Direct(self.bucket.clone()));
        let mut violations = Vec::new();
        let keys: BTreeSet<String> = self.bucket.keys().into_iter().collect();
        for (&id, cell) in &self.cells {
            // A failed fork's, being deleted: no guest knows of it.
            if self.unforks.contains_key(&id) {
                continue;
            }
            let record = match block_on(bucket_cell::head::get(&*direct, sim_cell(id))) {
                Ok(Some(record)) if !record.deleted() => record,
                Ok(Some(_)) if !cell.deleting => {
                    violations.push(format!(
                        "cell {id} is deleted, though no delete of it held",
                        id = Short(id)
                    ));
                    continue;
                }
                Ok(_) => continue,
                Err(error) => {
                    violations.push(format!("cell {id}'s record: {error}", id = Short(id)));
                    continue;
                }
            };
            if record.origin() != cell.origin {
                violations.push(format!(
                    "cell {id}'s record says {:?}",
                    record.origin(),
                    id = Short(id)
                ));
            }
            if record.forking().is_some() {
                violations
                    .push(format!("cell {id}, told made, is being made still", id = Short(id)));
            }
            // No placement took it back from a later one: no taking by an earlier placement
            // lowers its head's token (TASK-39.7, TASK-515).
            if record.token < cell.taken {
                violations.push(format!(
                    "cell {id}'s head is an earlier placement's: token {} after {}",
                    record.token,
                    cell.taken,
                    id = Short(id)
                ));
            }
            let recovered =
                match block_on(recovery::recover(&*direct, sim_cell(id), record.origin())) {
                    Ok(recovered) => recovered,
                    Err(error) => {
                        violations.push(format!("cell {id}: {error}", id = Short(id)));
                        continue;
                    }
                };
            let state = &recovered.state;
            // Every acknowledged segment's objects exist: what recovering the cell read, and
            // what holds each extent it names.
            for key in &recovered.touched {
                if !keys.contains(key) {
                    violations
                        .push(format!("cell {id} needs {key}, which is gone", id = Short(id)));
                }
            }
            for (extent, location) in &state.extents {
                if !keys.contains(&location.key) {
                    violations.push(format!(
                        "{extent:?} of {id}: {} is gone",
                        location.key,
                        id = Short(id)
                    ));
                }
            }
            let head = match self.truth_of(id, state.last_commit) {
                Ok(head) => head,
                Err(violation) => {
                    violations.push(violation);
                    continue;
                }
            };
            let own = self.own_path(id, head);
            let start = own.first().map_or(head, |first| self.commits[first].parent);
            if start != cell.base {
                violations.push(format!(
                    "cell {id} holds commit {head:?}, not of its history",
                    id = Short(id)
                ));
                continue;
            }
            // Its segments since its newest checkpoint apply each commit after the one before, of
            // its history: each once, in order, none another writer's history put after it. A
            // cell's segment names the last of the commits it holds.
            let mut before = recovered.checkpoint().is_none().then_some(cell.base);
            let applied = match self.applied(&direct, &recovered) {
                Ok(applied) => applied,
                Err(violation) => {
                    violations.push(violation);
                    continue;
                }
            };
            for commit in applied {
                let follows = |before: Option<u64>| {
                    let parent = self.commits[&commit].parent;
                    before.is_none_or(|before| self.precedes(before, parent))
                };
                if let Some(before) = before.filter(|&before| !follows(before)) {
                    violations.push(format!(
                        "cell {id} applies commit {commit} after {before:?}, not of its history",
                        id = Short(id)
                    ));
                    break;
                }
                before = Some(Some(commit));
            }
            for &acked in &cell.acked {
                if !self.precedes(acked, head) {
                    violations.push(format!(
                        "cell {id} lost commit {acked}, acknowledged",
                        id = Short(id)
                    ));
                }
            }
            // cladefs: every commit its fast WAL told durable is in the history of the cell's
            // writer that serves, the head's (doc-29): FastWal.tla's DurableSurvives.
            if !cell.told.is_empty() {
                let serving =
                    self.hosts.iter().filter_map(|host| host.sessions.get(&id)).find(|session| {
                        session.cell.is_some()
                            && session.uploader.failed().is_none()
                            && session.uploader.writer().is_some_and(|writer| {
                                writer.stopped().is_none() && writer.epoch() == record.epoch
                            })
                    });
                if let Some(serving) = serving {
                    for &told in &cell.told {
                        if !self.precedes(told, serving.guest.head) {
                            violations.push(format!(
                                "cell {id} lost commit {told}, told durable off its host: its \
                                 writer of epoch {} holds {:?}",
                                record.epoch,
                                serving.guest.head,
                                id = Short(id)
                            ));
                        }
                    }
                }
            }
            let (extents, root) = self.expected(head);
            if state.root != root {
                violations.push(format!(
                    "cell {id}'s root is {:?}, not {root:?}",
                    state.root,
                    id = Short(id)
                ));
            }
            let spent = self.spent(id, head);
            if state.spent != spent {
                violations.push(format!(
                    "cell {id} spent {:?}, not {spent:?}",
                    state.spent,
                    id = Short(id)
                ));
            }
            let capacity = self.capacity(head);
            if state.capacity != capacity {
                let found = state.capacity;
                violations.push(format!(
                    "cell {id}'s capacity is {found:?}, not {capacity:?}",
                    id = Short(id)
                ));
            }
            let held: Vec<_> = state.extents.keys().collect();
            let made: Vec<_> = extents.keys().collect();
            if held != made {
                violations.push(format!(
                    "cell {id} holds {held:?}, its commits made {made:?}",
                    id = Short(id)
                ));
                continue;
            }
            for (extent, location) in &state.extents {
                match block_on(bucket_cell::testing::read(&direct, *extent, location)) {
                    Ok(bytes) if bytes == extents[extent] => {}
                    Ok(_) => violations
                        .push(format!("cell {id}: {extent:?} reads wrong", id = Short(id))),
                    Err(error) => violations.push(format!("cell {id}: {error}", id = Short(id))),
                }
            }
        }
        // cladefs: every version a live cell's head names recovers, holding what its guest's
        // commits held as of its cut, read back; and every version acknowledged, its cell live,
        // not dropped, its head names (TASK-476).
        for (&id, cell) in &self.cells {
            if cell.deleting || self.unforks.contains_key(&id) {
                continue;
            }
            let Ok(Some(head)) = block_on(bucket_cell::head::get(&*direct, sim_cell(id))) else {
                continue;
            };
            if head.epoch == layout::Head::FINAL {
                continue;
            }
            for version in &head.versions {
                let Some(cut) = self.versions.get(&version.id).filter(|cut| cut.cell == id) else {
                    violations.push(format!(
                        "cell {id} names version {}, cut by none",
                        Short(version.id),
                        id = Short(id)
                    ));
                    continue;
                };
                let ring = bucket_cell::keys::Ring::keyless();
                let recovering =
                    recovery::recover_version(&*direct, sim_cell(id), version.at, &ring);
                let recovered = match block_on(recovering) {
                    Ok(recovered) => recovered,
                    Err(error) => {
                        violations.push(format!(
                            "version {} of {id}: {error}",
                            Short(version.id),
                            id = Short(id)
                        ));
                        continue;
                    }
                };
                for key in &recovered.touched {
                    if !keys.contains(key) {
                        violations.push(format!(
                            "version {} of {id} needs {key}, gone",
                            Short(version.id),
                            id = Short(id)
                        ));
                    }
                }
                let (extents, _) = self.expected(Some(cut.tag));
                let held: Vec<_> = recovered.state.extents.keys().collect();
                if held != extents.keys().collect::<Vec<_>>() {
                    violations.push(format!(
                        "version {} of {id} holds {held:?}",
                        Short(version.id),
                        id = Short(id)
                    ));
                    continue;
                }
                for (extent, location) in &recovered.state.extents {
                    match block_on(bucket_cell::testing::read(&direct, *extent, location)) {
                        Ok(bytes) if bytes == extents[extent] => {}
                        Ok(_) => violations
                            .push(format!("version {}: {extent:?} reads wrong", Short(version.id))),
                        Err(error) => {
                            violations.push(format!("version {}: {error}", Short(version.id)))
                        }
                    }
                }
            }
            for (&version, cut) in &self.versions {
                if cut.cell == id
                    && !cut.dropped
                    && !cut.dropping
                    && cell.acked.contains(&cut.tag)
                    && head.version(version).is_none()
                {
                    violations.push(format!(
                        "cell {id} names version {version} no more, its commit {} told \
                         durable: its head {head:?}",
                        cut.tag,
                        id = Short(id),
                        version = Short(version)
                    ));
                }
                // cladefs: nor one its writer dropped, told so, which no acknowledgement of that
                // writer's names after (TASK-569): a later writer may, from a log that cut it.
                if cut.cell == id
                    && cut.dropped
                    && head.epoch == cut.dropped_by
                    && head.version(version).is_some()
                {
                    violations.push(format!(
                        "cell {id} names version {version}, which its writer of epoch {} \
                         dropped: its head {head:?}",
                        cut.dropped_by,
                        id = Short(id),
                        version = Short(version)
                    ));
                }
            }
        }
        // The writers no later one fenced read what their guests hold, and their cells.
        for host in &self.hosts {
            for (&id, session) in &host.sessions {
                let (Some(writer), Some(open_cell)) = (session.uploader.writer(), &session.cell)
                else {
                    continue;
                };
                let head = block_on(bucket_cell::head::get(&*direct, sim_cell(id)));
                let epoch = head.map(|head| head.map_or(0, |head| head.epoch));
                if writer.stopped().is_some() || epoch != Ok(writer.epoch()) {
                    continue;
                }
                if session.uploader.failed().is_some() {
                    continue;
                }
                for (extent, location) in &writer.state().extents {
                    // Not one its guest freed: the writer's state lags what landed, unacknowledged
                    // yet, whose free a collector may already have reclaimed (seed 4116 of
                    // `everything` at 25x, TASK-149.32).
                    let Some(bytes) = session.guest.held.get(extent) else { continue };
                    match block_on(bucket_cell::testing::read(&direct, *extent, location)) {
                        Ok(read) if read == *bytes => {}
                        Ok(_) => violations.push(format!(
                            "cell {id}'s writer: {extent:?} reads wrong",
                            id = Short(id)
                        )),
                        Err(error) => {
                            violations.push(format!("cell {id}'s writer: {error}", id = Short(id)))
                        }
                    }
                }
                // And its cell, where it names them in the bucket (TASK-195).
                for (extent, bytes) in &session.guest.held {
                    let location = match open_cell.place(*extent) {
                        Ok(Some(Place::Bucket(location))) => location,
                        Ok(Some(_)) => continue,
                        Ok(None) => {
                            violations.push(format!("cell {id} lost {extent:?}", id = Short(id)));
                            continue;
                        }
                        Err(err) => {
                            violations.push(format!("cell {id}: {err:#}", id = Short(id)));
                            continue;
                        }
                    };
                    match block_on(bucket_cell::testing::read(&direct, *extent, &location)) {
                        Ok(read) if read == *bytes => {}
                        Ok(_) => violations
                            .push(format!("cell {id}: {extent:?} reads wrong", id = Short(id))),
                        Err(error) => {
                            violations.push(format!("cell {id}: {error}", id = Short(id)))
                        }
                    }
                }
            }
        }
        self.violations.extend(violations);
    }

    // The end.

    /// Runs a sweep of the collector over every cell, alone, and checks what
    /// it reports it did ([`World::check_report`]): what it reported, if it did.
    /// cladefs: with regions, a sweep of each region's bucket in turn, by a collector of its own,
    /// as each region runs its own (TASK-633.10): what they deleted, all told.
    fn sweep_alone(&mut self) -> Option<collector::Report> {
        let mut all = collector::Report::default();
        for bucket in self.region_buckets() {
            let report = self.sweep_alone_in(bucket)?;
            all.deleted += report.deleted;
        }
        Some(all)
    }

    /// cladefs: the bucket of each region, as a host of it asks: the one bucket without regions.
    fn region_buckets(&self) -> Vec<Arc<SimBucket>> {
        match self.profile.regions {
            0 => vec![self.bucket.clone()],
            regions => (1..=regions).map(|region| self.bucket.at(region)).collect(),
        }
    }

    /// A sweep of `bucket` alone ([`World::sweep_alone`]), checked against what every region's
    /// bucket holds before and after.
    fn sweep_alone_in(&mut self, bucket: Arc<SimBucket>) -> Option<collector::Report> {
        self.bucket.advance(self.profile.ticks + 1);
        let id = self.id();
        let reported = Rc::new(RefCell::new(None));
        let report = reported.clone();
        // Every region's objects, by where each is kept.
        let before = self.bucket.sizes();
        self.spawn(async move {
            *report.borrow_mut() = Some(sweep(&bucket, id).await);
        });
        self.settle();
        let reported = reported.take();
        let report = reported.clone().and_then(Result::ok);
        self.check_report(&before, reported);
        report
    }

    /// Stops the guests, lets every request and task finish, takes every live cell over with a
    /// new writer, then has each commit, upload and checkpoint while collectors run, a few times
    /// over.
    async fn quiesce(&mut self) {
        trace!("quiescing");
        self.faults = false;
        for region in std::mem::take(&mut self.down) {
            self.bucket.down(region, false);
        }
        self.settle();
        // cladefs: long enough that the pins of mirrors a crash stopped are stale, which the next
        // opening of a child, or collector, takes over (TASK-633.8).
        if self.profile.regions > 0 {
            self.bucket.advance(bucket_cell::mirror::PIN_STALE + 1);
        }
        // Forks of versions a crash stopped, or a lost disk left with no host: deleted.
        let stopped = self.restores.keys().copied().collect::<Vec<_>>();
        for child in stopped {
            self.give_up_restore(child);
        }
        // Those a crash stopped, their hosts up again.
        let stopped = self.unforks.iter().filter(|(_, unfork)| unfork.running.is_none());
        for child in stopped.map(|(&child, _)| child).collect::<Vec<_>>() {
            self.spawn_unfork(child);
        }
        self.settle();
        // Twice, so that extents a writer uploaded again, which it alone can tell are its, belong
        // to an earlier writer.
        for _ in 0..2 {
            for host in 0..self.hosts.len() {
                for (cell, session) in std::mem::take(&mut self.hosts[host].sessions) {
                    self.stop(host, cell, session);
                }
            }
            self.settle();
            self.quiescing = true;
            for cell in self.live() {
                let held = self.hosts.iter().any(|host| host.sessions.contains_key(&cell));
                if held {
                    continue;
                }
                let host =
                    self.hosts.iter().position(|host| host.disks.contains_key(&cell)).unwrap_or(0);
                self.start_session(host, cell);
            }
            self.settle();
            self.check_uploaded();
        }
        for _ in 0..4 {
            let sessions: Vec<(usize, u64)> = self
                .hosts
                .iter()
                .enumerate()
                .flat_map(|(host, found)| found.sessions.keys().map(move |&cell| (host, cell)))
                .collect();
            for &(host, cell) in &sessions {
                let Some(session) = self.hosts[host].sessions.get(&cell) else { continue };
                let Some(open_cell) = session.cell.clone() else { continue };
                let parent = session.guest.head;
                let tag = self.id();
                let before = open_cell.last_commit().unwrap();
                if let Err(err) = open_cell.commit(None).await {
                    self.violation(format!(
                        "cell {cell}'s last commit: {err:#}",
                        cell = Short(cell)
                    ));
                    return;
                }
                let Some(epoch) = self.committed(cell, &open_cell, before) else { return };
                let session = self.hosts[host].sessions.get_mut(&cell).unwrap();
                let extents = std::mem::take(&mut session.guest.written).into_iter().collect();
                let made =
                    Truth { cell, parent, extents, frees: Vec::new(), root: None, capacity: None };
                self.made(host, cell, tag, epoch, made);
            }
            self.check_uploaded();
            for &(host, cell) in &sessions {
                let Some(session) = self.hosts[host].sessions.get(&cell) else { continue };
                if let Some(writer) = session.uploader.writer() {
                    self.spawn(async move {
                        let _ = writer.checkpoint().await;
                    });
                }
            }
            self.settle();
            self.sweep_alone();
        }
        self.sweep_until_quiet();
        self.check_woken();
        self.check();
        self.check_collected();
        if self.profile.teardown {
            self.tear_down();
        }
    }

    /// Sweeps until two in a row delete nothing: a dead cell's pass retires the
    /// references its parents' cells keep origins for, which their next passes reclaim, a sweep a
    /// generation of a chain (TASK-395.4).
    fn sweep_until_quiet(&mut self) {
        let mut quiet = 0;
        for _ in 0..16 {
            let Some(report) = self.sweep_alone() else { break };
            quiet = if report.deleted == 0 { quiet + 1 } else { 0 };
            if quiet == 2 {
                break;
            }
        }
        if quiet < 2 {
            self.violation("16 sweeps of the collector alone, which kept doing something".into());
        }
    }

    /// cladefs: the end of the world, once checked ([`Profile::teardown`]): every session stopped,
    /// every live cell deleted by its latest placement, then sweeps alone until they do nothing
    /// more, which leave nothing a live cell needs, there being none: every copy of a mirror gone,
    /// with its cell and its children (TASK-633.10).
    fn tear_down(&mut self) {
        trace!("tearing down");
        for host in 0..self.hosts.len() {
            for (cell, session) in std::mem::take(&mut self.hosts[host].sessions) {
                self.stop(host, cell, session);
            }
        }
        self.settle();
        for cell in self.live() {
            let found = self.cells.get_mut(&cell).unwrap();
            found.deleting = true;
            let token = found.taken;
            self.spawn_delete(cell, token);
        }
        self.settle();
        let left = self.live();
        if !left.is_empty() {
            self.violation(format!("cells {left:?} live on, deleted"));
        }
        self.sweep_until_quiet();
        self.check_woken();
        self.check_collected();
    }
}
