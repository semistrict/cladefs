// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The simulated world: hosts whose stores keep cells for their guests, each a [`LocalCell`] over a
//! directory of the host's disk in memory, which does its I/O in the order its calls make
//! ([`MemDir::serial`]), and its uploader, `upload`'s, uploading its write-ahead log to the bucket;
//! forks, takeovers, deletions and collectors; driven by a seeded scheduler that also decides when,
//! in what order and how each request to the bucket completes, and when each uploader's interval
//! runs out. After every step it checks every live cell against what its guests did and were
//! told (TASK-88: the segments, their footers and the logs are the store's, as a host's are).

use crate::upload::{self, Clock, Failure, Shared};
use crate::{Description, LocalCell, MemDir, Place, Recovering};
use base32_id::Short;
use bucket_cell::collector::{self, Options};
use bucket_cell::fetch::{Budget, Limits};
use bucket_cell::layout::{self, Origin, epoch_of, writer_of};
use bucket_cell::testing::{Direct, Fault, SimBucket};
use bucket_cell::{Error, Recovered, Writer, lifecycle, recovery};
use extent_frame::spent::Spent;
use futures::channel::oneshot;
use futures::executor::{LocalPool, LocalSpawner, block_on};
use futures::future::{AbortHandle, FutureExt as _, abortable};
use futures::task::LocalSpawnExt as _;
use rand::rngs::StdRng;
use rand::{RngExt as _, SeedableRng as _};
use std::cell::RefCell;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};
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
    /// A store's uploader is asked for its commits, as a commit waiting for its upload asks, or
    /// its interval runs out: either closes the store's open segment.
    pub close: u32,
    pub checkpoint: u32,
    /// A writer takes a cell that has none.
    pub open: u32,
    /// A writer takes a cell from the one it has.
    pub takeover: u32,
    /// A host crashes, its disk keeping what was durable, or lost, its stores with it.
    pub crash: u32,
    pub delete: u32,
    /// A cell a fork of which lives is deleted: a base whose forks live on (TASK-39.4).
    pub delete_base: u32,
    pub collect: u32,
    /// A collector stops midway.
    pub stop_collector: u32,
    /// A placement cut off takes a cell back, with the fencing token it had (TASK-39.7).
    pub reclaim: u32,
    /// A placement cut off deletes a cell, with the fencing token it had (TASK-45.1).
    pub stale_delete: u32,
    /// What a crash left of a fork of a version, or of a failed one's deletion, is gone on with:
    /// finished, deleted as its host's cleanup deletes it, or forgotten (TASK-79.82).
    pub unfork: u32,
    /// Two hosts restore versions to one child within a round trip, as guests may name any ID:
    /// whichever's record comes first takes it, the other refused (TASK-173).
    pub twins: u32,
    /// A writer whose session stopped, a later one taking its cell or its host cut off, which
    /// runs on unaware, checkpoints: late, past where the next writer recovered from (TASK-175).
    pub stale_checkpoint: u32,
    /// cladefs: a cell's writer cuts a version, its store quiescent (doc-18, TASK-476).
    pub version: u32,
    /// cladefs: a cell's writer drops one of its versions, uploaded or local.
    pub drop_version: u32,
    /// cladefs: a host restores a version a cell's head names, which may be dropped by now: a fork
    /// of it, its child's record alone, as many hosts do at once.
    pub restore_version: u32,
    /// The store's clock advances.
    pub tick: u32,
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
    /// it had, as cladefsd opens its stores at boot: while the creates its writers had in flight
    /// land.
    pub restarts: bool,
    /// cladefs: the chance that a collector's pass is over one cell alone, not a sweep over
    /// every cell (TASK-395.4): none draws nothing of the seed's.
    pub one_cell: f64,
    /// cladefs: the chance that a fork of a version a crash stopped is forgotten, not finished nor
    /// deleted: its host's directory, left unmade, names no child, and deleting it touches nothing
    /// in the bucket (TASK-395.10). None draws nothing of the seed's.
    pub forget: f64,
    pub weights: Weights,
    pub options: Options,
}

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
                reclaim: 0,
                stale_delete: 0,
                unfork: 0,
                twins: 0,
                stale_checkpoint: 0,
                version: 1,
                drop_version: 0,
                restore_version: 1,
                tick: 3,
            },
            options: Options { compact_below: 0.5, grace: 4 },
        }
    }
}

/// cladefs: the simulation's cell `id`: a cell of its own, as a store made
/// alone, or a fork, is (TASK-364).
fn sim_cell(id: u64) -> bucket_cell::layout::CellId {
    bucket_cell::layout::CellId { tenant: 1, id }
}

/// The block size of the hosts' stores: small, so that a few blocks make an extent, and segments
/// of a few extents close, full, between commits. cladefs: a sector of an open segment, the least
/// it takes (`segments::SECTOR`, TASK-411.8); 512 bytes before.
const BLOCK: u64 = crate::segments::SECTOR;

/// How the hosts' stores keep their cells: in segments of a few extents, those of three blocks
/// or more in objects of their own. No segment closes for its age, which the store's clock, not
/// the world's, would decide.
fn store_options() -> crate::Options {
    crate::Options {
        max_extent: 4 * BLOCK,
        segment_size: 6 * BLOCK,
        segment_age: Duration::from_secs(1 << 30),
        large_extent: 3 * BLOCK,
        open_files: 4,
        view_lag: None,
        view_kept_bytes: None,
        // Flushes of either kind: records holding their runs, and checking them (TASK-301).
        held_most: 2 * BLOCK,
    }
}

/// The capacity of the hosts' stores: what a commit gives a cell that has none yet.
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

/// How many of the writers of the sessions stopped the world keeps ([`World::stale`]).
const STALE_WRITERS: usize = 4;

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
    /// The uploader's clock: each interval it waits out runs out at the next [`Intervals::run_out`].
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

    /// Runs out the intervals waited out now.
    fn run_out(&self) {
        for ran_out in std::mem::take(&mut *self.waiting.lock().unwrap()) {
            let _ = ran_out.send(());
        }
    }
}

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
}

impl CellId {
    /// A cell forked from `origin`, of its parent's commit `base`.
    fn forked(origin: Option<Origin>, base: Option<u64>) -> Self {
        Self { origin, base, acked: BTreeSet::new(), deleting: false, taken: 0 }
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

/// A cell's store's directory on a host's disk: its write-ahead log, open segment and index, as
/// a crash leaves them.
struct Disk {
    dir: MemDir,
    /// Whether its store was made whole ([`LocalCell::made`]).
    made: bool,
    /// The history of commits its store holds ([`World::lineages`]): a new one once its log is
    /// dropped.
    lineage: u64,
}

struct Host {
    /// Each cell's store's directory (doc-14), by cell.
    disks: HashMap<u64, Disk>,
    sessions: BTreeMap<u64, Session>,
    /// The PUTs its writers upload objects with, as a host's share one budget
    /// (`local_cell::upload`): a few at once, so that uploads wait for each other's to be done,
    /// and are woken as they are (TASK-149.32).
    uploads: Arc<Budget>,
    /// What holds the parts its stores pack as their segments fill, which they share, as a
    /// daemon's do (`crate::held`, TASK-400).
    held: Arc<crate::HeldParts>,
    /// cladefs: its keys, its KMS the tests' and its fetchers' keys its own, which its stores share,
    /// as a daemon's do: every store encrypted (TASK-411.28).
    keys: bucket_cell::keys::Host,
}

impl Default for Host {
    fn default() -> Self {
        let limits = Limits { requests: UPLOADS_AT_ONCE, bytes: u64::MAX };
        let (disks, sessions) = (HashMap::new(), BTreeMap::new());
        let held = crate::HeldParts::new();
        let keys = bucket_cell::keys::testing::host();
        Self { disks, sessions, uploads: Budget::new(limits), held, keys }
    }
}

/// A cell's store on a host, its uploader, and its guest.
struct Session {
    id: u64,
    /// The fencing token of the placement it is.
    token: u64,
    /// The history of commits its store holds ([`Disk::lineage`]).
    lineage: u64,
    uploader: Arc<Uploader>,
    intervals: Arc<Intervals>,
    /// What opening the store gave, once it did.
    started: mpsc::Receiver<Result<Arc<LocalCell<MemDir>>, Failure>>,
    /// The store, once open.
    store: Option<Arc<LocalCell<MemDir>>>,
    /// The uploader's task.
    running: AbortHandle,
    guest: Guest,
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
    Deleted {
        cell: u64,
        /// Whether a placement cut off deleted it, which must fail.
        stale: bool,
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
    /// cladefs: what a writer's dropping of a version gave.
    VersionDropped {
        id: u64,
        result: Result<(), Error>,
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
/// its writer drops it, asked, and whether it dropped it, told so.
struct Cut {
    cell: u64,
    tag: u64,
    dropping: bool,
    dropped: bool,
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
    /// Each history of commits a store held, by the store's epoch of each: what the tags of the
    /// commits its writers upload name ([`World::truth_of`]).
    lineages: HashMap<u64, BTreeMap<u64, u64>>,
    /// The history the commits each writer uploads are of, by its cell and epoch: a writer
    /// uploads one store's.
    stamps: HashMap<(u64, u64), u64>,
    collectors: BTreeMap<u64, AbortHandle>,
    /// The writers of the sessions stopped, the latest few, which may run on unaware
    /// ([`World::stale_checkpoint`]).
    stale: Vec<Arc<Writer<SimBucket>>>,
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
    /// Whether everything is finishing: then every live cell must open.
    quiescing: bool,
    /// The tasks spawned and not done, aborted or dropped, by number, each with where it was
    /// spawned: one still here once every request is done waits for a wake nothing will give
    /// (TASK-149.32). The uploaders, which wait for commits for as long as their stores are open,
    /// are not among them: that each uploads what it is asked to is checked of it
    /// ([`World::check_uploaded`]).
    tasks: Rc<RefCell<BTreeMap<u64, &'static std::panic::Location<'static>>>>,
    next_task: u64,
    pub violations: Vec<String>,
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

impl World {
    pub fn new(seed: u64, profile: Profile) -> Self {
        let pool = LocalPool::new();
        let spawner = pool.spawner();
        let mut world = Self {
            rng: StdRng::seed_from_u64(seed),
            profile,
            bucket: SimBucket::new(),
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
            stale: Vec::new(),
            unforks: BTreeMap::new(),
            restores: BTreeMap::new(),
            versions: BTreeMap::new(),
            footers: RefCell::default(),
            next_id: 1,
            faults: true,
            quiescing: false,
            tasks: Rc::default(),
            next_task: 0,
            violations: Vec::new(),
        };
        let (root, owner) = (world.id(), world.id());
        block_on(lifecycle::create(&Direct(world.bucket.clone()), sim_cell(root), owner)).unwrap();
        world.cells.insert(root, CellId::forked(None, None));
        world
    }

    /// What the run asked of the bucket, in order ([`SimBucket::journal`]).
    pub fn journal(&self) -> Vec<String> {
        self.bucket.journal()
    }

    /// Runs the profile's steps, then lets everything finish and checks that no garbage is left.
    pub fn run(&mut self) {
        for _ in 0..self.profile.steps {
            self.step();
            self.check();
            if !self.violations.is_empty() {
                return;
            }
        }
        self.quiesce();
    }

    fn step(&mut self) {
        self.settle_tasks();
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
            w.reclaim,
            w.stale_delete,
            w.unfork,
            w.twins,
            w.write,
            w.stale_checkpoint,
            w.version,
            w.drop_version,
            w.restore_version,
            w.tick,
        ];
        let mut roll = self.rng.random_range(0..choices.iter().sum::<u32>());
        let choice = choices.iter().position(|&weight| {
            let hit = roll < weight;
            roll = roll.saturating_sub(weight);
            hit
        });
        match choice.unwrap() {
            0 => self.complete(),
            1 => self.commit(),
            2 => self.close(),
            3 => self.checkpoint(),
            4 => self.open(false),
            5 => self.open(true),
            6 => self.crash(),
            7 => self.delete(false),
            8 => self.delete(true),
            9 => self.collect(),
            10 => self.stop_collector(),
            11 => self.reclaim(),
            12 => self.stale_delete(),
            13 => self.unfork(),
            14 => self.twins(),
            15 => self.write(),
            16 => self.stale_checkpoint(),
            17 => self.cut_version(),
            18 => self.drop_version(),
            19 => self.restore_version(),
            _ => {
                let ticks = self.rng.random_range(1..=self.profile.options.grace);
                self.bucket.advance(ticks);
            }
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

    /// Spawns an uploader's `task`, which waits for its store's commits for as long as it is
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
            Event::Deleted { cell, stale: false, result } => match result {
                Ok(()) => {}
                // A later placement took it as it was deleted: it lives on.
                Err(Error::Stale) => self.cells.get_mut(&cell).unwrap().deleting = false,
                Err(error) => {
                    self.violation(format!("deleting cell {cell}: {error}", cell = Short(cell)))
                }
            },
            Event::Deleted { cell, stale: true, result } => match result {
                Err(Error::Stale) => {}
                // It found the cell's head final, which only its latest placement's delete,
                // begun since, writes: the cell is deleted, by that, as deletes are idempotent.
                Ok(()) if self.cells[&cell].deleting => {}
                Ok(()) => self.violation(format!(
                    "a placement cut off deleted cell {cell}",
                    cell = Short(cell)
                )),
                Err(error) => self
                    .violation(format!("cell {cell}'s stale delete: {error}", cell = Short(cell))),
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
            Event::VersionDropped { id, result } => {
                let cut = self.versions.get_mut(&id).unwrap();
                cut.dropping = false;
                let cell = cut.cell;
                match result {
                    Ok(()) => cut.dropped = true,
                    Err(Error::Lost(_) | Error::Corrupt(_)) => {
                        self.violation(format!(
                            "dropping version {id}: {result:?}",
                            id = Short(id)
                        ));
                    }
                    // Its writer taken over, or its cell deleted: dropped as its swap of the head
                    // landed or not, which a write that took effect, answered unavailable, then
                    // found changed by the taking over, can't tell its writer (TASK-499): as the
                    // head says, the writer writing it no more.
                    Err(_) => {
                        let direct = Direct(self.bucket.clone());
                        let head = block_on(bucket_cell::head::get(&direct, sim_cell(cell)));
                        let named =
                            head.ok().flatten().is_some_and(|head| head.version(id).is_some());
                        self.versions.get_mut(&id).unwrap().dropped = !named;
                    }
                }
            }
        }
    }

    /// What `restore`, a fork of a version to `child`, gave.
    fn restored(&mut self, child: u64, restore: Restore, result: Result<(), Error>) {
        match result {
            // Told to its VM's manager: a cell from now on, whatever comes of the version.
            Ok(()) => {
                let found = CellId::forked(Some(restore.origin), restore.base);
                drop(self.cells.insert(child, found))
            }
            // The version went, or the fork was given up on: its host deletes it.
            Err(Error::Deleted) => self.fail_restore(child, restore),
            Err(error) => {
                self.violation(format!("forking version {}: {error}", Short(restore.version)))
            }
        }
    }

    // The stores and their uploaders.

    /// Takes in what each session's uploader did since: its store opened, or refused to; its
    /// uploader stopped; the epoch of its writer, which the tags of the commits it uploads
    /// name; and the commits it uploaded, which their guests are told are durable. Returns
    /// whether a session opened, ended or started anew, which the next run of the tasks may go on
    /// with.
    fn observe(&mut self) -> bool {
        let mut changed = false;
        for host in 0..self.hosts.len() {
            let cells: Vec<u64> = self.hosts[host].sessions.keys().copied().collect();
            for cell in cells {
                let session = &self.hosts[host].sessions[&cell];
                let id = session.id;
                if session.store.is_none() {
                    match session.started.try_recv() {
                        Ok(Ok(store)) => self.opened(host, cell, store),
                        Ok(Err(failure)) => self.refused(host, cell, failure),
                        Err(mpsc::TryRecvError::Empty) => continue,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            unreachable!("an uploader stopped without saying how its store opened")
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
                if let Some(found) = self.cells.get_mut(&cell) {
                    found.acked.extend(acked);
                }
            }
        }
        changed
    }

    /// The directory of `cell`'s store on `host`, made if its disk has none.
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

    /// Starts a session of `cell` on `host`: its store opened, its writer taking the cell, of
    /// the placement whose fencing token is `token`, or, with none, of a new one, the latest.
    fn start_session(&mut self, host: usize, cell: u64, token: Option<u64>) {
        let disk = self.disk(host, cell);
        let dir = disk.dir.clone();
        let recovering = match disk.made {
            true => LocalCell::recover(dir),
            false => LocalCell::recover_unmade(dir),
        };
        match recovering {
            Ok(recovering) => self.run_session(host, cell, token, recovering),
            Err(err) => self
                .violation(format!("recovering cell {cell}'s store: {err:#}", cell = Short(cell))),
        }
    }

    /// Starts a session of `cell` on `host` that opens the store `recovering` recovered, as
    /// [`World::start_session`] does.
    fn run_session(
        &mut self,
        host: usize,
        cell: u64,
        token: Option<u64>,
        recovering: Recovering<MemDir>,
    ) {
        let (id, holder) = (self.id(), self.id());
        let token = token.unwrap_or(holder);
        let intervals = Arc::<Intervals>::default();
        let options = upload::Options {
            interval: INTERVAL,
            max_lag: u64::MAX,
            max_lag_bytes: u64::MAX,
            cache: None,
            held: Some(self.hosts[host].held.clone()),
            token,
            checkpoint_every: upload::CHECKPOINT_EVERY,
            replication: None,
            members: None,
            // The interval closes a segment filled at once, as the first full one closed between
            // commits, its commits uploading, is what the world's clock can't wait for
            // (TASK-92.41).
            filling_wait: Duration::ZERO,
        };
        let (uploads, keys) = (self.hosts[host].uploads.clone(), self.hosts[host].keys.clone());
        let uploader = Shared::new(options, uploads, intervals.clock(), keys);
        let (started, start) = mpsc::channel();
        let (running, bucket) = (uploader.clone(), self.bucket.clone());
        let running = self.spawn_uploader(async move {
            let recovering = (recovering, None);
            running.run(recovering, store_options(), bucket, holder, started).await;
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
            store: None,
            running,
            guest: Guest::default(),
        };
        self.hosts[host].sessions.insert(cell, session);
    }

    /// `cell`'s store on `host` opened, its writer having taken the cell: its log put after
    /// the cell's end, what it holds that landed acknowledged, the rest stamped anew. Its guest
    /// starts from its last commit, which the store's history holds, or the bucket's.
    fn opened(&mut self, host: usize, cell: u64, store: Arc<LocalCell<MemDir>>) {
        let disk = self.hosts[host].disks.get_mut(&cell).unwrap();
        if !disk.made {
            if let Err(err) = store.made() {
                return self
                    .violation(format!("making cell {cell}'s store: {err:#}", cell = Short(cell)));
            }
            disk.made = true;
        }
        let session = &self.hosts[host].sessions[&cell];
        let (token, lineage, uploader) = (session.token, session.lineage, session.uploader.clone());
        let writer = uploader.writer().expect("a writer took the cell");
        let epoch = match store.last_commit() {
            Ok(epoch) => epoch,
            Err(err) => {
                return self.violation(format!("cell {cell}'s store: {err:#}", cell = Short(cell)));
            }
        };
        // Its last commit: its log's, past the cell's end, or the cell's, its log holding
        // nothing more: the store's history then, if it holds that commit, or the bucket's, the
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
        session.store = Some(store);
    }

    /// `cell`'s store on `host` did not open, for `failure`: its log diverged, another writer's
    /// commits past it, which `cladefsctl take` drops, and it opens again with none; else its guest
    /// goes. cladefs: a log diverged must have: the cell's last commit is neither one of its
    /// commits nor its base. Before, every divergence was taken as one, which hid a log whose
    /// segment ending one commit and going on with the next landed alone, its next commit, durable,
    /// dropped (TASK-345).
    fn refused(&mut self, host: usize, cell: u64, failure: Failure) {
        trace!("host {host}: {cell}'s store refused: {failure}", cell = Short(cell));
        let session = self.hosts[host].sessions.remove(&cell).unwrap();
        let token = session.token;
        self.stop(host, cell, session);
        if let Failure::Bucket(Error::Diverged) = failure {
            let disk = self.hosts[host].disks.get_mut(&cell).unwrap();
            let mut recovering = match LocalCell::recover(disk.dir.clone()) {
                Ok(recovering) => recovering,
                Err(err) => {
                    return self
                        .violation(format!("cell {cell}'s store: {err:#}", cell = Short(cell)));
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
    /// store's history it is of, and the store's epoch of it there. A fork's child names its
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
    /// its host dying stops it, and its store, which closes as it goes, making its last commits
    /// durable, before anything opens its directory again; a crash took what its disk keeps
    /// before ([`World::crash`]).
    fn stop(&mut self, host: usize, cell: u64, session: Session) {
        trace!("host {host}: session {} of {cell} stops", session.id, cell = Short(cell));
        session.running.abort();
        if let Some(writer) = session.uploader.writer() {
            self.stale.push(writer);
            if self.stale.len() > STALE_WRITERS {
                self.stale.remove(0);
            }
        }
        let Session { uploader, store, .. } = session;
        let closing = store.as_ref().map(Arc::downgrade);
        let going = Arc::downgrade(&uploader);
        drop((uploader, store));
        // The uploader's task, aborted, lets go of the store as it is next polled.
        self.pool.run_until_stalled();
        assert!(
            closing.as_ref().is_none_or(|store| store.strong_count() == 0),
            "cell {cell}'s store on host {host} is open still, its session stopped: {} \
             holds of it, {} of its uploader",
            closing.as_ref().map_or(0, std::sync::Weak::strong_count),
            going.strong_count(),
            cell = Short(cell),
        );
    }

    /// `cell`'s store on `host` failed doing what its guest asked, for `err`: which it may only
    /// once its uploader stopped. `store` is the caller's hold of it, let go first, as the
    /// session's end closes the store ([`World::stop`]).
    fn store_failed(
        &mut self,
        host: usize,
        cell: u64,
        store: Arc<LocalCell<MemDir>>,
        err: anyhow::Error,
    ) {
        drop(store);
        let session = &self.hosts[host].sessions[&cell];
        let (id, stopped) = (session.id, session.uploader.failed().is_some());
        if !stopped {
            self.violation(format!(
                "cell {cell}'s store on host {host}: {err:#}",
                cell = Short(cell)
            ));
        }
        self.end(host, cell, id);
    }

    /// Checks that every session's uploader uploads what its store committed, asked to, as
    /// everything settles (TASK-149.32): one that doesn't waits for a wake nothing will give.
    fn check_uploaded(&mut self) {
        trace!("checking uploads");
        let mut asked = Vec::new();
        for host in &self.hosts {
            for (&cell, session) in &host.sessions {
                let Some(store) = &session.store else { continue };
                let epoch = store.last_commit().unwrap();
                let _ = session.uploader.ask(epoch, false);
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

    /// A random session whose store is open, and its writer.
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
                        session.store.is_some()
                            && session.uploader.failed().is_none()
                            && session.uploader.writer().is_some_and(|w| w.stopped().is_none())
                    })
                    .map(move |(cell, _)| (host, *cell))
            })
            .collect();
        (!ready.is_empty()).then(|| ready[self.rng.random_range(0..ready.len())])
    }

    fn write(&mut self) {
        let Some((host, cell)) = self.ready() else { return };
        let seed = self.id();
        self.write_extent(host, cell, seed);
    }

    /// The guest of `cell`'s store on `host` writes an extent of its own, its bytes of `seed`:
    /// whether it could.
    fn write_extent(&mut self, host: usize, cell: u64, seed: u64) -> bool {
        let session = self.hosts[host].sessions.get_mut(&cell).unwrap();
        let extent = ExtentId::new(cell, session.guest.next_sequence);
        session.guest.next_sequence += 1;
        let len = self.rng.random_range(1..=4) * BLOCK as usize;
        let bytes = Self::content(seed, extent, len);
        let store = session.store.clone().unwrap();
        if let Err(err) = store.write(extent, 0, &bytes) {
            self.store_failed(host, cell, store, err);
            return false;
        }
        session.guest.written.insert(extent, bytes);
        if let Some(other) = self.minted.insert(extent, seed) {
            self.violation(format!("{extent:?} minted by {other} and {seed}"));
        }
        true
    }

    /// The store's epoch of the commit `store` just made, which follows `before`'s.
    fn committed(&mut self, cell: u64, store: &LocalCell<MemDir>, before: u64) -> Option<u64> {
        let epoch = store.last_commit().ok()?;
        if epoch <= before {
            self.violation(format!(
                "cell {cell}'s commit after {before} made epoch {epoch}",
                cell = Short(cell)
            ));
            return None;
        }
        Some(epoch)
    }

    /// The guest of `cell`'s store on `host` made the commit `tag`, sealing what it wrote,
    /// freeing `frees`, the store's epoch of it `epoch`.
    fn made(&mut self, host: usize, cell: u64, tag: u64, epoch: u64, mut made: Truth) {
        trace!(
            "host {host}: {cell} commits {tag} at epoch {epoch}, after {:?}",
            made.parent,
            cell = Short(cell)
        );
        // A cell with no capacity yet takes its store's.
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

    fn commit(&mut self) {
        let Some((host, cell)) = self.ready() else { return };
        let tag = self.id();
        for _ in 0..self.rng.random_range(0..3) {
            if !self.write_extent(host, cell, tag) {
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
        if tag.is_multiple_of(5) && !self.resize(host, cell, tag << 20) {
            return;
        }
        let session = &self.hosts[host].sessions[&cell];
        // Durable at once, or later, with a later commit's, or a closing's.
        let durable = self.rng.random_bool(0.5);
        let store = session.store.clone().unwrap();
        let parent = session.guest.head;
        let committing = || -> anyhow::Result<u64> {
            let before = store.last_commit()?;
            for extent in &frees {
                store.free(*extent)?;
            }
            store.commit_ordered(root.as_deref())?;
            if durable {
                store.flush()?;
            }
            Ok(before)
        };
        let before = match committing() {
            Ok(before) => before,
            Err(err) => return self.store_failed(host, cell, store, err),
        };
        let Some(epoch) = self.committed(cell, &store, before) else { return };
        let session = self.hosts[host].sessions.get_mut(&cell).unwrap();
        let extents = std::mem::take(&mut session.guest.written).into_iter().collect();
        let frees = frees.into_iter().collect();
        let made = Truth { cell, parent, extents, frees, root, capacity: None };
        self.made(host, cell, tag, epoch, made);
    }

    /// The guest of `cell`'s store on `host` sets its capacity to `capacity`: a commit of its
    /// own, which seals none of what it wrote. Whether it could.
    fn resize(&mut self, host: usize, cell: u64, capacity: u64) -> bool {
        let tag = self.id();
        let session = &self.hosts[host].sessions[&cell];
        let (store, parent) = (session.store.clone().unwrap(), session.guest.head);
        let resizing = || -> anyhow::Result<u64> {
            let before = store.last_commit()?;
            store.set_capacity(capacity)?;
            Ok(before)
        };
        let before = match resizing() {
            Ok(before) => before,
            Err(err) => {
                self.store_failed(host, cell, store, err);
                return false;
            }
        };
        let Some(epoch) = self.committed(cell, &store, before) else { return false };
        let capacity = Some(capacity);
        let made =
            Truth { cell, parent, extents: Vec::new(), frees: Vec::new(), root: None, capacity };
        self.made(host, cell, tag, epoch, made);
        true
    }

    /// A store's uploader is asked for its commits, as a commit waiting for its upload asks, or its
    /// interval runs out: its open segment closes, and it uploads.
    fn close(&mut self) {
        let Some((host, cell)) = self.ready() else { return };
        let session = &self.hosts[host].sessions[&cell];
        if self.rng.random_bool(0.5) {
            return session.intervals.run_out();
        }
        let (store, uploader) = (session.store.clone().unwrap(), session.uploader.clone());
        let asked =
            store.last_commit().map_err(Failure::from).and_then(|epoch| uploader.ask(epoch, false));
        if let Err(failure) = asked {
            drop(uploader);
            self.store_failed(host, cell, store, anyhow::Error::new(failure));
        }
    }

    /// A writer whose session stopped checkpoints, as one running on unaware of it would: a later
    /// writer's, or none, holding its cell's head.
    fn stale_checkpoint(&mut self) {
        if self.stale.is_empty() {
            return;
        }
        let writer = self.stale[self.rng.random_range(0..self.stale.len())].clone();
        self.spawn(async move {
            let _ = writer.checkpoint().await;
        });
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

    /// A writer takes a cell on a host: one with no writer, or, `takeover`, one with. Only a
    /// cell with a record: as a host opens the stores of the cells it was told of.
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
        // Mostly where its store is.
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
        self.start_session(host, cell, None);
    }

    /// A host crashes: its uploaders stop where they are, what they had in flight landing or not;
    /// its disk keeps what its stores made durable, and some of what they did since, or is lost.
    fn crash(&mut self) {
        let host = self.rng.random_range(0..self.hosts.len());
        trace!("host {host} crashes");
        let sessions = std::mem::take(&mut self.hosts[host].sessions);
        for session in sessions.values() {
            session.running.abort();
        }
        if self.rng.random_bool(0.3) {
            self.hosts[host].disks.clear();
        } else {
            let disks = &mut self.hosts[host].disks;
            let mut keys: Vec<u64> = disks.keys().copied().collect();
            keys.sort_unstable();
            for cell in keys {
                let seed = self.rng.random();
                let disk = disks.get_mut(&cell).unwrap();
                disk.dir = disk.dir.crash(seed);
            }
        }
        let cells: Vec<u64> = sessions.keys().copied().collect();
        // Their stores close on what the crash left behind.
        for (cell, session) in sessions {
            self.stop(host, cell, session);
        }
        if self.profile.restarts {
            for cell in cells {
                let kept = self.hosts[host].disks.contains_key(&cell);
                if kept && !self.cells[&cell].deleting {
                    self.start_session(host, cell, None);
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
    }

    /// `child`, a fork of a version that failed, or a crash left, goes as its host's cleanup deletes
    /// it, or an operator does what no host held: which nothing relies on.
    fn fail_restore(&mut self, child: u64, restore: Restore) {
        let mut found = CellId::forked(Some(restore.origin), restore.base);
        found.deleting = true;
        self.cells.insert(child, found);
        self.unforks.insert(child, Unfork { owner: restore.owner, running: None });
        self.spawn_unfork(child);
    }

    /// Makes the fork `child`, or goes on with one a crash stopped, as opening its handle does.
    fn spawn_restore(&mut self, child: u64) {
        let Restore { version, origin, owner, .. } = self.restores[&child];
        let (bucket, events) = (self.bucket.clone(), self.events.clone());
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

    /// cladefs: a cell's writer cuts a version (doc-18, TASK-476), its store quiescent, its guest
    /// having written nothing since its last commit: a commit of its store's whose segment's
    /// footer names it, which its uploader is asked for, as a snapshot's caller may wait for it.
    fn cut_version(&mut self) {
        let Some((host, cell)) = self.ready() else { return };
        let session = &self.hosts[host].sessions[&cell];
        if !session.guest.written.is_empty() {
            return;
        }
        let (store, uploader, parent) =
            (session.store.clone().unwrap(), session.uploader.clone(), session.guest.head);
        let (id, tag) = (self.id(), self.id());
        let before = match store.last_commit() {
            Ok(before) => before,
            Err(err) => return self.store_failed(host, cell, store, err),
        };
        let cut = layout::NewVersion { id, keep_last: None };
        if let Err(err) = store.commit_version(cut, true) {
            return self.store_failed(host, cell, store, err);
        }
        let Some(epoch) = self.committed(cell, &store, before) else { return };
        let made = Truth {
            cell,
            parent,
            extents: Vec::new(),
            frees: Vec::new(),
            root: None,
            capacity: None,
        };
        self.made(host, cell, tag, epoch, made);
        self.versions.insert(id, Cut { cell, tag, dropping: false, dropped: false });
        trace!(
            "host {host}: {cell} cuts version {id} as commit {tag} at epoch {epoch}",
            cell = Short(cell),
            id = Short(id)
        );
        let _ = uploader.ask(epoch, false);
    }

    /// cladefs: a cell's writer drops one of its versions, uploaded or local, which its store
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
        // Gone from the head as soon as its swap lands, which its writer may be told of later.
        self.versions.get_mut(&id).unwrap().dropping = true;
        let session = &self.hosts[host].sessions[&cell];
        let ids = BTreeSet::from([id]);
        session.store.as_ref().unwrap().forget_versions(&ids);
        let (writer, events) = (session.uploader.writer().unwrap(), self.events.clone());
        self.spawn(async move {
            let result = writer.drop_versions(&ids).await;
            events.borrow_mut().push(Event::VersionDropped { id, result });
        });
    }

    /// cladefs: the versions the heads of the cells with versions name, each by its ID, of which
    /// cell, the segment it is at and the commit it is of: as a host restoring one read it.
    fn named_versions(&self) -> Vec<(u64, u64, u64, u64)> {
        let direct = Direct(self.bucket.clone());
        let cells: BTreeSet<u64> = self.versions.values().map(|cut| cut.cell).collect();
        let mut named = Vec::new();
        for cell in cells {
            let Ok(Some(head)) = block_on(bucket_cell::head::get(&direct, sim_cell(cell))) else {
                continue;
            };
            for version in &head.versions {
                if let Some(cut) = self.versions.get(&version.id) {
                    named.push((version.id, cell, version.at, cut.tag));
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
        let (version, cell, at, tag) = named[self.rng.random_range(0..named.len())];
        let (host, owner) = (self.rng.random_range(0..self.hosts.len()), self.id());
        let origin = Origin { parent: sim_cell(cell), at };
        Some(Restore { version, origin, base: Some(tag), host, owner, running: None })
    }

    /// cladefs: a host restores a version a cell's head names: its child's record, its reference,
    /// the head read again, the fork made (`lifecycle::fork_version`), as many hosts do at once.
    fn restore_version(&mut self) {
        let Some(restore) = self.new_restore() else { return };
        let child = self.id();
        self.restores.insert(child, restore);
        self.spawn_restore(child);
    }

    /// cladefs: two hosts restore one version to one child within a round trip, as guests may name
    /// any ID, each of an owner of its own: whichever's record comes first takes the ID, the other
    /// refused, finding another's (TASK-79.82, TASK-173). Crashes leave them be.
    fn twins(&mut self) {
        let Some(first) = self.new_restore() else { return };
        let (child, owner) = (self.id(), self.id());
        let second = Restore { owner, running: None, ..first };
        let (bucket, events) = (self.bucket.clone(), self.events.clone());
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

    /// A placement cut off takes a cell back, on a host that has no writer of it, with an earlier
    /// fencing token than the cell's latest placement's: which must fail.
    fn reclaim(&mut self) {
        let candidates: Vec<(u64, u64)> = (self.cells.iter())
            .filter(|(_, found)| !found.deleting && found.taken > 1)
            .map(|(&cell, found)| (cell, found.taken))
            .collect();
        if candidates.is_empty() {
            return;
        }
        let (cell, taken) = candidates[self.rng.random_range(0..candidates.len())];
        let hosts: Vec<usize> = (0..self.hosts.len())
            .filter(|&host| !self.hosts[host].sessions.contains_key(&cell))
            .collect();
        if hosts.is_empty() {
            return;
        }
        let host = hosts[self.rng.random_range(0..hosts.len())];
        let token = self.rng.random_range(1..taken);
        self.start_session(host, cell, Some(token));
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
        let (bucket, events) = (self.bucket.clone(), self.events.clone());
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
        self.spawn_delete(cell, token, false);
    }

    /// A placement cut off deletes a cell, with an earlier fencing token than the cell's latest
    /// placement's: which must fail, the cell living on.
    fn stale_delete(&mut self) {
        let candidates: Vec<(u64, u64)> = (self.cells.iter())
            .filter(|(_, found)| !found.deleting && found.taken > 1)
            .map(|(&cell, found)| (cell, found.taken))
            .collect();
        if candidates.is_empty() {
            return;
        }
        let (cell, taken) = candidates[self.rng.random_range(0..candidates.len())];
        let token = self.rng.random_range(1..taken);
        self.spawn_delete(cell, token, true);
    }

    /// Deletes `cell` with the fencing token `token`, a placement cut off's if `stale`.
    fn spawn_delete(&mut self, cell: u64, token: u64, stale: bool) {
        let holder = self.id();
        let (bucket, events) = (self.bucket.clone(), self.events.clone());
        self.spawn(async move {
            let result = lifecycle::delete(&*bucket, sim_cell(cell), holder, token).await;
            events.borrow_mut().push(Event::Deleted { cell, stale, result });
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
        let (bucket, events, options) =
            (self.bucket.clone(), self.events.clone(), self.profile.options);
        let cells: Vec<u64> = self.cells.keys().copied().collect();
        let one_cell = self.profile.one_cell;
        let cell = (!cells.is_empty() && one_cell > 0.0 && self.rng.random_bool(one_cell))
            .then(|| sim_cell(cells[self.rng.random_range(0..cells.len())]));
        let handle = self.spawn(async move {
            let result = match cell {
                Some(cell) => collector::collect_cell(&*bucket, cell, id, &options).await,
                None => collector::collect(&*bucket, id, &options).await,
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
        loop {
            self.settle_tasks();
            if self.bucket.waiting() == 0 {
                return;
            }
            self.bucket.complete(0, Fault::None);
            trace!("{}", self.bucket.journal().last().unwrap());
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
    /// deleted, and the moves it wrote. cladefs: and no more references gone than it retired, whether
    /// or not they were there (TASK-395.4), and deleted marked (TASK-395.13); and the bytes of what
    /// it deleted, and of what it compacted, as they were (TASK-395.5).
    fn check_report(
        &mut self,
        before: &BTreeMap<String, u64>,
        report: Option<Result<collector::Report, Error>>,
    ) {
        let after: BTreeSet<String> = self.bucket.keys().into_iter().collect();
        let moves = |key: &&String| matches!(layout::kind(key), layout::Kind::Moves { .. });
        let reference = |key: &&String| matches!(layout::kind(key), layout::Kind::Reference { .. });
        // The collector's leases, its own, which it reports not.
        let lease =
            |key: &&String| matches!(layout::kind(key), layout::Kind::CollectorLease { .. });
        let gone = || before.keys().filter(|key| !after.contains(*key) && !lease(key));
        let made = || after.iter().filter(|key| !before.contains_key(*key));
        let retired = gone().filter(reference).count();
        let expected = |reported: &collector::Report| collector::Report {
            deleted: gone().filter(|key| !reference(key)).count(),
            compacted: made().filter(moves).count(),
            retired: reported.retired.max(retired),
            deleted_bytes: gone().filter(|key| !reference(key)).map(|key| before[key]).sum(),
            // Each moves of what was there then, the object it is of.
            compacted_bytes: (made().filter(moves))
                .filter_map(|moves| before.get(&layout::moved_from(moves)?))
                .sum(),
            // Which passes skipped their cells, unchanged, the bucket can't tell (TASK-395.6).
            skipped: reported.skipped,
            // Its cells are encrypted under the tests' KMS, which its collectors hold: nothing is
            // left locked (TASK-411.6, TASK-411.28).
            locked: 0,
            uncompacted: 0,
            // cladefs: the cells whose heads went, as only a dead cell's removal deletes them
            // (TASK-455).
            removed: gone()
                .filter(|key| matches!(layout::kind(key), layout::Kind::Head { .. }))
                .count(),
        };
        match report {
            Some(Ok(report)) if report == expected(&report) => {}
            Some(Ok(report)) => {
                let expected = expected(&report);
                self.violation(format!("collector reported {report:?}, having done {expected:?}"))
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
            text += &format!("history {lineage}: commits by the store's epochs {commits:?}\n");
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

    /// Checks that the bucket holds only what live cells need, compacted as the collectors
    /// could (TASK-44.1): [`bucket_cell::testing::uncollected`], its live cells by their records.
    fn check_collected(&mut self) {
        let direct = Direct(self.bucket.clone());
        let compact_below = self.profile.options.compact_below;
        for left in block_on(bucket_cell::testing::uncollected(&direct, compact_below)) {
            self.violation(left);
        }
    }

    fn complete(&mut self) {
        let waiting = self.bucket.waiting();
        if waiting == 0 {
            return;
        }
        let index = self.rng.random_range(0..waiting);
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
                    let Some(object) = block_on(bucket_cell::bucket::get(direct, &key)) else {
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
        let tags = applied.concat().into_iter();
        tags.map(|tag| Ok(self.truth_of(cell.id, Some(tag))?.unwrap())).collect()
    }

    /// Checks that every live cell holds what its commits made, up to one of its own, past
    /// everything its guests were told was durable, and that the current writers, and their
    /// stores, read what their guests hold.
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
            // what holds each extent it names, or the moves that say where that went.
            for key in &recovered.touched {
                if !keys.contains(key) {
                    violations
                        .push(format!("cell {id} needs {key}, which is gone", id = Short(id)));
                }
            }
            for (extent, location) in &state.extents {
                // Its moves, any pass's (TASK-407.1).
                let moves = layout::moves_of(&location.key).unwrap();
                let moved = keys.iter().any(|key| key.starts_with(&moves));
                if !keys.contains(&location.key) && !moved {
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
            // store's segment names the last of the commits it holds.
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
            }
        }
        // The writers no later one fenced read what their guests hold, and their stores.
        for host in &self.hosts {
            for (&id, session) in &host.sessions {
                let (Some(writer), Some(store)) = (session.uploader.writer(), &session.store)
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
                // And its store, where it names them in the bucket (TASK-195).
                for (extent, bytes) in &session.guest.held {
                    let location = match store.place(*extent) {
                        Ok(Some(Place::Bucket(location))) => location,
                        Ok(Some(_)) => continue,
                        Ok(None) => {
                            violations
                                .push(format!("cell {id}'s store lost {extent:?}", id = Short(id)));
                            continue;
                        }
                        Err(err) => {
                            violations.push(format!("cell {id}'s store: {err:#}", id = Short(id)));
                            continue;
                        }
                    };
                    match block_on(bucket_cell::testing::read(&direct, *extent, &location)) {
                        Ok(read) if read == *bytes => {}
                        Ok(_) => violations.push(format!(
                            "cell {id}'s store: {extent:?} reads wrong",
                            id = Short(id)
                        )),
                        Err(error) => {
                            violations.push(format!("cell {id}'s store: {error}", id = Short(id)))
                        }
                    }
                }
            }
        }
        self.violations.extend(violations);
    }

    // The end.

    /// Runs a sweep of the collector over every cell, alone, past the grace period, and checks what
    /// it reports it did ([`World::check_report`]): what it reported, if it did.
    fn sweep_alone(&mut self) -> Option<collector::Report> {
        self.bucket.advance(self.profile.options.grace + 1);
        let id = self.id();
        let (bucket, options) = (self.bucket.clone(), self.profile.options);
        let reported = Rc::new(RefCell::new(None));
        let report = reported.clone();
        let listed = block_on(bucket_cell::Bucket::list(&Direct(self.bucket.clone()), ""));
        let before: BTreeMap<String, u64> =
            listed.unwrap().into_iter().map(|meta| (meta.key, meta.size)).collect();
        self.spawn(async move {
            *report.borrow_mut() = Some(collector::collect(&*bucket, id, &options).await);
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
    fn quiesce(&mut self) {
        trace!("quiescing");
        self.faults = false;
        self.settle();
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
                self.start_session(host, cell, None);
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
                let Some(store) = session.store.clone() else { continue };
                let parent = session.guest.head;
                let tag = self.id();
                let before = store.last_commit().unwrap();
                if let Err(err) = store.commit(None) {
                    self.violation(format!(
                        "cell {cell}'s last store: {err:#}",
                        cell = Short(cell)
                    ));
                    return;
                }
                let Some(epoch) = self.committed(cell, &store, before) else { return };
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
        // cladefs: then sweeps until two in a row delete and compact nothing: a dead cell's pass
        // retires the references its parents' cells keep origins for, which their next passes
        // reclaim, a sweep a generation of a chain (TASK-395.4).
        let mut quiet = 0;
        for _ in 0..16 {
            let Some(report) = self.sweep_alone() else { break };
            quiet = if report.deleted == 0 && report.compacted == 0 { quiet + 1 } else { 0 };
            if quiet == 2 {
                break;
            }
        }
        if quiet < 2 {
            self.violation("16 sweeps of the collector alone, which kept doing something".into());
        }
        self.check_woken();
        self.check();
        self.check_collected();
    }
}
